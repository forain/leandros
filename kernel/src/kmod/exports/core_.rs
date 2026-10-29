//! kapi exports: core, logging, memory, kthreads
use super::*;

#[no_mangle]
pub extern "C" fn kapi_abi_version() -> u32 { KMOD_ABI_VERSION }

#[no_mangle]
pub extern "C" fn kapi_log(level: u32, ptr: *const u8, len: usize) {
    let m = unsafe { bytes(ptr, len.min(512)) };
    let o = crate::kmod::load::CUR_OWNER.load(Ordering::SeqCst);
    let nm = if o != 0 { crate::kmod::registry::name_of(KmodId(o)) } else { [0; 32] };
    let nm = crate::kmod::name_str(&nm);
    let sep = if nm.is_empty() { "" } else { " " };
    log(&alloc::format!("[kmod{}{}] <{}> {}\n", sep, nm, level, alloc::string::String::from_utf8_lossy(m)));
}

#[no_mangle]
pub extern "C" fn kapi_panic(owner: KmodId, ptr: *const u8, len: usize) -> ! {
    let m = unsafe { bytes(ptr, len.min(512)) };
    let nm = crate::kmod::registry::name_of(owner);
    panic!("kmod {} (id {}) panic: {}", crate::kmod::name_str(&nm), owner.0, alloc::string::String::from_utf8_lossy(m));
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
            crate::kmod::registry::note_registration(owner, RegKind::KTHREAD, pid as u64);
            pid as i64
        }
        None => -12,
    }
}

pub(super) static PING_COUNT: AtomicU32 = AtomicU32::new(0);
pub(super) static PING_OWNER: AtomicU16 = AtomicU16::new(0);
pub(super) static PING_CTXSZ: AtomicU32 = AtomicU32::new(0);
pub(super) static PING_ABI: AtomicU32 = AtomicU32::new(0);
pub(super) static PING_CMDLEN: AtomicUsize = AtomicUsize::new(0);

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

