//! Module runtime (kmod ABI v1). A module crate is a `staticlib` that depends on this crate and
//! invokes `kmod_rt::module!{ .. }` once.
#![no_std]

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU16, Ordering};

pub use kmod_abi::{self, KmodCtx, KmodDevId, KmodId, BUS_PCI, BUS_USB};
use kmod_abi::sys;

static OWNER: AtomicU16 = AtomicU16::new(0);

/// The id the loader assigned to this module (0 before `kmod_init` ran).
pub fn owner() -> KmodId {
    KmodId(OWNER.load(Ordering::Relaxed))
}

/// PCI id: vendor, device (0xffff = any).
pub const fn pci(vendor: u16, device: u16) -> KmodDevId {
    KmodDevId { bus: BUS_PCI, a: vendor, b: device, c: 0, class: 0, class_mask: 0 }
}

/// USB id: vid, pid (0xffff = any), class = class<<16|sub<<8|proto under class_mask.
pub const fn usb(vid: u16, pid: u16, class: u32, class_mask: u32) -> KmodDevId {
    KmodDevId { bus: BUS_USB, a: vid, b: pid, c: 0, class, class_mask }
}

/// The 128-byte header placed in `.kmod.info`; everything but magic/abi/size/machine/name is
/// patched after the link by scripts/kmod/kmodpost.py.
#[repr(C, align(8))]
pub struct RawHeader(pub [u8; 128]);

pub const fn header(name: &str) -> RawHeader {
    let mut h = [0u8; 128];
    let magic = kmod_abi::KMOD_MAGIC;
    let mut i = 0;
    while i < 8 {
        h[i] = magic[i];
        i += 1;
    }
    let abi = kmod_abi::KMOD_ABI_VERSION.to_le_bytes();
    let mut i = 0;
    while i < 4 {
        h[8 + i] = abi[i];
        i += 1;
    }
    h[12] = 128;
    #[cfg(target_arch = "x86_64")]
    let m = kmod_abi::EM_X86_64.to_le_bytes();
    #[cfg(target_arch = "aarch64")]
    let m = kmod_abi::EM_AARCH64.to_le_bytes();
    let mut i = 0;
    while i < 4 {
        h[16 + i] = m[i];
        i += 1;
    }
    let n = name.as_bytes();
    assert!(n.len() < 32, "module name too long");
    let mut i = 0;
    while i < n.len() {
        h[96 + i] = n[i];
        i += 1;
    }
    RawHeader(h)
}

#[doc(hidden)]
pub fn __enter(ctx: *const KmodCtx, init: fn() -> i32) -> i32 {
    const EINVAL: i32 = -22;
    if ctx.is_null() {
        return EINVAL;
    }
    let c = unsafe { &*ctx };
    if c.ctx_size as usize != core::mem::size_of::<KmodCtx>() || c.abi_version != kmod_abi::KMOD_ABI_VERSION {
        return EINVAL;
    }
    OWNER.store(c.owner.0, Ordering::Relaxed);
    // This copy of `spin` has its own (unset) hook; forward it to the kernel's accounting.
    spin::held::set_hook(|acq| unsafe { sys::kapi_spin_held(acq) });
    init()
}

#[macro_export]
macro_rules! __kmod_unit {
    ($e:expr) => {
        ()
    };
}

/// `module!{ name: "x", init: crate::init, ids: [kmod_rt::pci(0x1af4, 0x1044)] }`
#[macro_export]
macro_rules! module {
    (name: $name:literal, init: $init:path $(, ids: [$($id:expr),* $(,)?])? $(,)?) => {
        #[used]
        #[link_section = ".kmod.info"]
        static __KMOD_HDR: $crate::RawHeader = $crate::header($name);
        $(
        #[used]
        #[link_section = ".kmod.ids"]
        static __KMOD_IDS: [$crate::KmodDevId; [$($crate::__kmod_unit!($id)),*].len()] = [$($id),*];
        )?
        #[no_mangle]
        pub unsafe extern "C" fn kmod_init(ctx: *const $crate::KmodCtx) -> i32 {
            $crate::__enter(ctx, $init)
        }
    };
}

// --- logging -------------------------------------------------------------------------------

pub struct Buf {
    b: [u8; 256],
    n: usize,
}
impl Buf {
    pub const fn new() -> Buf {
        Buf { b: [0; 256], n: 0 }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.b[..self.n]
    }
}
impl Write for Buf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.b.len() - self.n;
        let k = s.len().min(room); // truncate (byte-wise) rather than fail
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

#[doc(hidden)]
pub fn __log(level: u32, args: fmt::Arguments) {
    let mut b = Buf::new();
    let _ = b.write_fmt(args);
    unsafe { sys::kapi_log(level, b.as_bytes().as_ptr(), b.as_bytes().len()) };
}

#[macro_export]
macro_rules! kinfo { ($($a:tt)*) => { $crate::__log(6, format_args!($($a)*)) } }
#[macro_export]
macro_rules! kwarn { ($($a:tt)*) => { $crate::__log(4, format_args!($($a)*)) } }
#[macro_export]
macro_rules! kerr { ($($a:tt)*) => { $crate::__log(3, format_args!($($a)*)) } }

// --- allocator and panic -------------------------------------------------------------------

struct KernelHeap;
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        sys::kapi_alloc(l.size(), l.align())
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        sys::kapi_dealloc(p, l.size(), l.align())
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        sys::kapi_realloc(p, l.size(), l.align(), new)
    }
}

#[global_allocator]
static HEAP: KernelHeap = KernelHeap;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let mut b = Buf::new();
    let _ = write!(b, "{}", info);
    unsafe { sys::kapi_panic(owner(), b.as_bytes().as_ptr(), b.as_bytes().len()) }
}
