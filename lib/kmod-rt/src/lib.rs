//! Module runtime (kmod ABI v1). A module crate is a `staticlib` that depends on this crate and
//! invokes `kmod_rt::module!{ .. }` once.
#![no_std]

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU16, Ordering};

pub use kmod_abi::{self, KmodCtx, KmodDevId, KmodId, BUS_OF, BUS_PCI, BUS_SDIO, BUS_USB};
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

/// SDIO function-1 CIS manfid row (`did` 0xffff = any).
pub const fn sdio(vid: u16, did: u16) -> KmodDevId {
    KmodDevId { bus: BUS_SDIO, a: vid, b: did, c: 0, class: 0, class_mask: 0 }
}

/// Total byte length of the NUL-separated `.kmod.ofstr` blob for these compatibles.
pub const fn __of_total(s: &[&str]) -> usize {
    let mut t = 0; let mut i = 0;
    while i < s.len() { t += s[i].len() + 1; i += 1; }
    t
}

/// The `.kmod.ofstr` blob: each compatible followed by a NUL.
pub const fn __of_blob<const N: usize>(s: &[&str]) -> [u8; N] {
    let mut out = [0u8; N]; let mut o = 0; let mut i = 0;
    while i < s.len() {
        let b = s[i].as_bytes(); let mut j = 0;
        while j < b.len() { out[o] = b[j]; o += 1; j += 1; }
        out[o] = 0; o += 1; i += 1;
    }
    out
}

/// OF rows: `a`/`b` = low/high 16 bits of the string's offset in `.kmod.ofstr`, `c` = length,
/// `class` = FNV-1a 32 of the string (kmodpost checks both against the blob).
pub const fn __of_ids<const N: usize>(s: &[&str]) -> [KmodDevId; N] {
    let mut out = [KmodDevId { bus: BUS_OF, a: 0, b: 0, c: 0, class: 0, class_mask: 0 }; N];
    let mut off = 0usize; let mut i = 0;
    while i < s.len() {
        out[i] = KmodDevId { bus: BUS_OF, a: (off & 0xffff) as u16, b: (off >> 16) as u16, c: s[i].len() as u16,
            class: kmod_abi::alias::fnv1a32(s[i].as_bytes()), class_mask: 0 };
        off += s[i].len() + 1; i += 1;
    }
    out
}

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
    kmod_kapi::set_owner(c.owner);
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
    (name: $name:literal, init: $init:path, of: [$($s:literal),+ $(,)?] $(,)?) => {
        #[used]
        #[link_section = ".kmod.info"]
        static __KMOD_HDR: $crate::RawHeader = $crate::header($name);
        #[used]
        #[link_section = ".kmod.ofstr"]
        static __KMOD_OFSTR: [u8; $crate::__of_total(&[$($s),+])] = $crate::__of_blob::<{ $crate::__of_total(&[$($s),+]) }>(&[$($s),+]);
        #[used]
        #[link_section = ".kmod.ids"]
        static __KMOD_IDS: [$crate::KmodDevId; [$($crate::__kmod_unit!($s)),+].len()] =
            $crate::__of_ids::<{ [$($crate::__kmod_unit!($s)),+].len() }>(&[$($s),+]);
        #[no_mangle]
        pub unsafe extern "C" fn kmod_init(ctx: *const $crate::KmodCtx) -> i32 {
            $crate::__enter(ctx, $init)
        }
    };
    (name: $name:literal, init: $init:path, sdio: [$(($v:expr, $d:expr)),+ $(,)?] $(,)?) => {
        $crate::module! { name: $name, init: $init, ids: [$($crate::sdio($v, $d)),+] }
    };
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

/// PCI class id: matches every function whose class<<16|subclass<<8|prog_if satisfies
/// `(value & mask) == (class & mask)` (modules.alias `pci * * <class> <mask> <name>`).
pub const fn pci_class(class: u32, mask: u32) -> KmodDevId {
    KmodDevId { bus: BUS_PCI, a: 0xffff, b: 0xffff, c: 0, class, class_mask: mask }
}
