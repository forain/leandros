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
    log(&alloc::format!("[kmod] <{}> {}\n", level, alloc::string::String::from_utf8_lossy(m)));
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
const _: () = {
    assert!(mm::kmodwin::BASE as u64 == kmod_abi::layout::BASE);
    assert!(mm::kmodwin::SIZE as u64 == kmod_abi::layout::SIZE);
};

macro_rules! sym { ($n:literal, $f:ident) => { KSym { name: concat!($n, "\0").as_ptr(), addr: $f as *const () } }; }

pub const NEXPORTS: usize = 14;

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
