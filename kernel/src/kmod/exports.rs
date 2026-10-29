//! Core kapi v1: the symbols a module may import. Each is `#[no_mangle] extern "C"`
//! and listed in `.ksymtab` (kept by the linker scripts, bounded by
//! `__ksymtab_start/_end`). Subsystem exports are appended by their lanes.

use core::alloc::Layout;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicUsize, Ordering};
use kmod_abi::{sys, KSym, KmodCtx, KmodId, RegKind, KMOD_ABI_VERSION};

fn log(s: &str) { drivers::pci::serial_debug(s); }

unsafe fn bytes<'a>(p: *const u8, n: usize) -> &'a [u8] {
    if p.is_null() || n == 0 { &[] } else { core::slice::from_raw_parts(p, n) }
}

#[no_mangle]
pub extern "C" fn kapi_abi_version() -> u32 { KMOD_ABI_VERSION }

#[no_mangle]
pub extern "C" fn kapi_log(level: u32, ptr: *const u8, len: usize) {
    let m = unsafe { bytes(ptr, len.min(512)) };
    let o = super::load::CUR_OWNER.load(Ordering::SeqCst);
    let nm = if o != 0 { super::registry::name_of(KmodId(o)) } else { [0; 32] };
    let nm = super::name_str(&nm);
    let sep = if nm.is_empty() { "" } else { " " };
    log(&alloc::format!("[kmod{}{}] <{}> {}\n", sep, nm, level, alloc::string::String::from_utf8_lossy(m)));
}

#[no_mangle]
pub extern "C" fn kapi_panic(owner: KmodId, ptr: *const u8, len: usize) -> ! {
    let m = unsafe { bytes(ptr, len.min(512)) };
    let nm = super::registry::name_of(owner);
    panic!("kmod {} (id {}) panic: {}", super::name_str(&nm), owner.0, alloc::string::String::from_utf8_lossy(m));
}

#[no_mangle]
pub extern "C" fn kapi_alloc(size: usize, align: usize) -> *mut u8 {
    match Layout::from_size_align(size, align) {
        Ok(l) if size != 0 => unsafe { alloc::alloc::alloc(l) },
        _ => core::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn kapi_dealloc(ptr: *mut u8, size: usize, align: usize) {
    if let Ok(l) = Layout::from_size_align(size, align) {
        if !ptr.is_null() && size != 0 { unsafe { alloc::alloc::dealloc(ptr, l) } }
    }
}

#[no_mangle]
pub extern "C" fn kapi_realloc(ptr: *mut u8, size: usize, align: usize, new_size: usize) -> *mut u8 {
    match Layout::from_size_align(size, align) {
        Ok(l) if !ptr.is_null() && size != 0 && new_size != 0 => unsafe { alloc::alloc::realloc(ptr, l, new_size) },
        _ => core::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn kapi_cmdline(len_out: *mut usize) -> *const u8 {
    let c = crate::boot_cmdline().unwrap_or("");
    if !len_out.is_null() { unsafe { len_out.write(c.len()); } }
    c.as_ptr()
}

#[no_mangle]
pub extern "C" fn kapi_monotonic_ns() -> u64 { sched::monotonic_ns() }
#[no_mangle]
pub extern "C" fn kapi_msleep(ms: u64) { sched::kthread::msleep(ms) }
#[no_mangle]
pub extern "C" fn kapi_udelay(us: u64) { sched::kthread::udelay(us) }
#[no_mangle]
pub extern "C" fn kapi_spin_held(acquired: bool) { sched::spinheld::account(acquired) }
#[no_mangle]
pub extern "C" fn kapi_spin_held_count() -> u32 { sched::spinheld::held() }

#[no_mangle]
pub extern "C" fn kapi_kthread_spawn(name: *const u8, name_len: usize, f: extern "C" fn(*mut u8),
                                     arg: *mut u8, owner: KmodId) -> i64 {
    let nm = core::str::from_utf8(unsafe { bytes(name, name_len.min(24)) }).unwrap_or("kmod");
    let a = arg as usize;
    match sched::kthread::spawn(nm, move || f(a as *mut u8)) {
        Some(t) => {
            let pid = t.pid();
            super::registry::note_registration(owner, RegKind::KTHREAD, pid as u64);
            pid as i64
        }
        None => -12,
    }
}

static PING_COUNT: AtomicU32 = AtomicU32::new(0);
static PING_OWNER: AtomicU16 = AtomicU16::new(0);
static PING_CTXSZ: AtomicU32 = AtomicU32::new(0);
static PING_ABI: AtomicU32 = AtomicU32::new(0);
static PING_CMDLEN: AtomicUsize = AtomicUsize::new(0);

/// Test aid: counts calls, remembers the last ctx, returns 42 + owner.
#[no_mangle]
pub extern "C" fn kapi_ktest_ping(ctx: *const KmodCtx) -> i32 {
    PING_COUNT.fetch_add(1, Ordering::SeqCst);
    if ctx.is_null() { return -22; }
    let c = unsafe { &*ctx };
    PING_OWNER.store(c.owner.0, Ordering::SeqCst);
    PING_CTXSZ.store(c.ctx_size, Ordering::SeqCst);
    PING_ABI.store(c.abi_version, Ordering::SeqCst);
    PING_CMDLEN.store(c.cmdline_len, Ordering::SeqCst);
    42 + c.owner.0 as i32
}

/// (calls, last owner, last ctx_size, last abi_version, last cmdline_len)
pub fn ping_state() -> (u32, u16, u32, u32, usize) {
    (PING_COUNT.load(Ordering::SeqCst), PING_OWNER.load(Ordering::SeqCst), PING_CTXSZ.load(Ordering::SeqCst),
     PING_ABI.load(Ordering::SeqCst), PING_CMDLEN.load(Ordering::SeqCst))
}

// ── PCI / memory / entropy exports (L5a) ────────────────────────────────────

use kmod_abi::{KPciDev, KMAP_MMIO, KMAP_WC, KMAP_WRITABLE};

fn unbdf(bdf: u32) -> (u8, u8, u8) { ((bdf >> 16) as u8, (bdf >> 8) as u8, bdf as u8) }

/// Index-th PCI function matching vendor/device (0xffff = any device). 0 ok, -19 none, -22 bad out.
#[no_mangle]
pub extern "C" fn kapi_pci_find(vendor: u16, device: u16, index: u32, out: *mut KPciDev) -> i32 {
    if out.is_null() { return -22; }
    let hit = drivers::pci::scan().into_iter()
        .filter(|d| d.vendor_id == vendor && (device == 0xffff || d.device_id == device))
        .nth(index as usize);
    match hit {
        Some(d) => {
            unsafe {
                out.write(KPciDev {
                    bdf: (d.bus as u32) << 16 | (d.dev as u32) << 8 | d.func as u32,
                    vendor: d.vendor_id,
                    device: d.device_id,
                    class: (d.class as u32) << 16 | (d.subclass as u32) << 8 | d.prog_if as u32,
                    _pad: 0,
                    bars: d.bars,
                });
            }
            0
        }
        None => -19,
    }
}

/// Config read of `width` (1, 2 or 4) bytes at `off` (< 256). Bad arguments read as all-ones.
#[no_mangle]
pub extern "C" fn kapi_pci_cfg_read(bdf: u32, off: u16, width: u8) -> u32 {
    let (b, d, f) = unbdf(bdf);
    if off > 0xff { return u32::MAX; }
    let o = off as u8;
    unsafe {
        match width {
            1 => drivers::pci::pci_read_config_8(b, d, f, o) as u32,
            2 => drivers::pci::pci_read_config_16(b, d, f, o) as u32,
            4 => drivers::pci::pci_read_config_32(b, d, f, o),
            _ => u32::MAX,
        }
    }
}

#[no_mangle]
pub extern "C" fn kapi_pci_cfg_write(bdf: u32, off: u16, width: u8, val: u32) {
    let (b, d, f) = unbdf(bdf);
    if off > 0xff { return; }
    let o = off as u8;
    unsafe {
        match width {
            1 => drivers::pci::pci_write_config_8(b, d, f, o, val as u8),
            2 => drivers::pci::pci_write_config_16(b, d, f, o, val as u16),
            4 => drivers::pci::pci_write_config_32(b, d, f, o, val),
            _ => {}
        }
    }
}

/// Map device memory into the kernel. Only KMAP_* are honoured: the mapping is always
/// kernel-only and non-executable. Returns 0 on failure (the caller falls back itself).
#[no_mangle]
pub extern "C" fn kapi_map_device(phys: u64, len: usize, flags: u32) -> usize {
    use mm::paging::{map_kernel_device, PageFlags};
    if len == 0 { return 0; }
    let mut f = PageFlags::PRESENT;
    if flags & KMAP_WRITABLE != 0 { f |= PageFlags::WRITABLE; }
    if flags & KMAP_MMIO != 0 { f |= PageFlags::MMIO; }
    if flags & KMAP_WC != 0 { f |= PageFlags::WRITECOMBINE; }
    unsafe { map_kernel_device(phys as usize, len, f).unwrap_or(0) }
}

#[no_mangle]
pub extern "C" fn kapi_phys_to_virt(phys: u64) -> usize { mm::phys_to_virt(phys as usize) }

/// 2^order pages. `below` is an exclusive physical ceiling (0 = any); the block ends at or below it.
#[no_mangle]
pub extern "C" fn kapi_page_alloc(order: u32, below: u64) -> u64 {
    if order > 10 { return 0; }
    let r = if below == 0 { mm::buddy::alloc(order as usize) }
            else { mm::buddy::alloc_below(order as usize, below as usize) };
    r.map_or(0, |p| p as u64)
}

#[no_mangle]
pub extern "C" fn kapi_page_free(phys: u64, order: u32) {
    if phys != 0 && order <= 10 { mm::buddy::free(phys as usize, order as usize); }
}

const HW_SLOTS: usize = 4;
static HW_READ: [AtomicUsize; HW_SLOTS] = [const { AtomicUsize::new(0) }; HW_SLOTS];
static HW_USED: AtomicUsize = AtomicUsize::new(0);

fn hw_tramp<const N: usize>(out: &mut [u8]) -> usize {
    let p = HW_READ[N].load(Ordering::Acquire);
    if p == 0 { return 0; }
    let f: extern "C" fn(*mut u8, usize) -> usize = unsafe { core::mem::transmute(p) };
    f(out.as_mut_ptr(), out.len()).min(out.len())
}

/// Register a module entropy source and reseed the CSPRNG at once so it contributes
/// immediately. 0 ok, -12 no free slot (MAX 4), -22 bad name.
#[no_mangle]
pub extern "C" fn kapi_hwrng_register(owner: KmodId, name: *const u8, len: usize,
                                      read: extern "C" fn(*mut u8, usize) -> usize) -> i32 {
    let nm = match core::str::from_utf8(unsafe { bytes(name, len.min(32)) }) {
        Ok(s) if !s.is_empty() => s,
        _ => return -22,
    };
    let i = HW_USED.fetch_add(1, Ordering::SeqCst);
    if i >= HW_SLOTS { HW_USED.store(HW_SLOTS, Ordering::SeqCst); return -12; }
    // Modules are permanent, so the name bytes live as long as the kernel.
    let nm: &'static str = unsafe { core::mem::transmute::<&str, &'static str>(nm) };
    HW_READ[i].store(read as usize, Ordering::Release);
    let tramp: sched::random::SourceFn = match i {
        0 => hw_tramp::<0>, 1 => hw_tramp::<1>, 2 => hw_tramp::<2>, _ => hw_tramp::<3>,
    };
    sched::random::register_source(nm, tramp);
    super::registry::note_registration(owner, RegKind::HWRNG, i as u64);
    sched::random::reseed_now();
    0
}

// Compile-time signature check against kmod_abi::sys.
const _: sys::AbiVersionFn = kapi_abi_version;
const _: sys::LogFn = kapi_log;
const _: sys::PanicFn = kapi_panic;
const _: sys::AllocFn = kapi_alloc;
const _: sys::DeallocFn = kapi_dealloc;
const _: sys::ReallocFn = kapi_realloc;
const _: sys::CmdlineFn = kapi_cmdline;
const _: sys::MonotonicFn = kapi_monotonic_ns;
const _: sys::SleepFn = kapi_msleep;
const _: sys::SleepFn = kapi_udelay;
const _: sys::SpinHeldFn = kapi_spin_held;
const _: sys::SpinHeldCountFn = kapi_spin_held_count;
const _: sys::KthreadSpawnFn = kapi_kthread_spawn;
const _: sys::KtestPingFn = kapi_ktest_ping;
const _: sys::PciFindFn = kapi_pci_find;
const _: sys::PciCfgReadFn = kapi_pci_cfg_read;
const _: sys::PciCfgWriteFn = kapi_pci_cfg_write;
const _: sys::MapDeviceFn = kapi_map_device;
const _: sys::PhysToVirtFn = kapi_phys_to_virt;
const _: sys::PageAllocFn = kapi_page_alloc;
const _: sys::PageFreeFn = kapi_page_free;
const _: sys::HwrngRegisterFn = kapi_hwrng_register;
const _: () = {
    assert!(mm::kmodwin::BASE as u64 == kmod_abi::layout::BASE);
    assert!(mm::kmodwin::SIZE as u64 == kmod_abi::layout::SIZE);
};

macro_rules! sym { ($n:literal, $f:ident) => { KSym { name: concat!($n, "\0").as_ptr(), addr: $f as *const () } }; }

pub const NEXPORTS: usize = 22;

#[used]
#[link_section = ".ksymtab"]
static KSYMTAB: [KSym; NEXPORTS] = [
    sym!("kapi_abi_version", kapi_abi_version),
    sym!("kapi_log", kapi_log),
    sym!("kapi_panic", kapi_panic),
    sym!("kapi_alloc", kapi_alloc),
    sym!("kapi_dealloc", kapi_dealloc),
    sym!("kapi_realloc", kapi_realloc),
    sym!("kapi_cmdline", kapi_cmdline),
    sym!("kapi_monotonic_ns", kapi_monotonic_ns),
    sym!("kapi_msleep", kapi_msleep),
    sym!("kapi_udelay", kapi_udelay),
    sym!("kapi_spin_held", kapi_spin_held),
    sym!("kapi_spin_held_count", kapi_spin_held_count),
    sym!("kapi_kthread_spawn", kapi_kthread_spawn),
    sym!("kapi_ktest_ping", kapi_ktest_ping),
    sym!("kapi_pci_find", kapi_pci_find),
    sym!("kapi_pci_cfg_read", kapi_pci_cfg_read),
    sym!("kapi_pci_cfg_write", kapi_pci_cfg_write),
    sym!("kapi_map_device", kapi_map_device),
    sym!("kapi_phys_to_virt", kapi_phys_to_virt),
    sym!("kapi_page_alloc", kapi_page_alloc),
    sym!("kapi_page_free", kapi_page_free),
    sym!("kapi_hwrng_register", kapi_hwrng_register),
];

extern "C" {
    static __ksymtab_start: KSym;
    static __ksymtab_end: KSym;
}

/// The linked export table, as the linker script delimits it.
pub fn ksymtab() -> &'static [KSym] {
    unsafe {
        let s = core::ptr::addr_of!(__ksymtab_start);
        let e = core::ptr::addr_of!(__ksymtab_end);
        core::slice::from_raw_parts(s, (e as usize - s as usize) / core::mem::size_of::<KSym>())
    }
}

/// Address of an export, found by walking the table.
pub fn lookup(name: &str) -> Option<usize> {
    for k in ksymtab() {
        let mut n = 0;
        while unsafe { *k.name.add(n) } != 0 { n += 1; }
        if unsafe { core::slice::from_raw_parts(k.name, n) } == name.as_bytes() { return Some(k.addr as usize); }
    }
    None
}
