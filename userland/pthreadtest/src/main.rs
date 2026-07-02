//! pthreadtest — standalone regression coverage for TODO.md Phase 4 (Thread Management):
//! pthread_create/join, mutex contention, condvar wait/signal, thread-specific data
//! (TSD) destructors, and cleanup stack execution.
//!
//! Initializes via relibc_start_v1 to set up TLS (tcb / %fs_base / tpidr_el0) properly.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `pthread_main` returns the number of failures as the exit code.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

pub type pthread_t = *mut c_void;
pub type pthread_key_t = u64;

#[repr(C)]
pub union pthread_mutex_t {
    __relibc_internal_size: [u8; 12],
    __relibc_internal_align: i32,
}

#[repr(C)]
pub union pthread_cond_t {
    __relibc_internal_size: [u8; 8],
    __relibc_internal_align: i32,
}

#[repr(C)]
pub struct CleanupLinkedListEntry {
    routine: extern "C" fn(*mut c_void),
    arg: *mut c_void,
    prev: *const c_void,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn exit(status: i32) -> !;

    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> i32;

    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> i32;

    pub fn pthread_mutex_init(mutex: *mut pthread_mutex_t, attr: *const c_void) -> i32;
    pub fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> i32;
    pub fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> i32;
    pub fn pthread_mutex_destroy(mutex: *mut pthread_mutex_t) -> i32;

    pub fn pthread_cond_init(cond: *mut pthread_cond_t, attr: *const c_void) -> i32;
    pub fn pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> i32;
    pub fn pthread_cond_signal(cond: *mut pthread_cond_t) -> i32;
    pub fn pthread_cond_destroy(cond: *mut pthread_cond_t) -> i32;

    pub fn pthread_key_create(
        key: *mut pthread_key_t,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> i32;
    pub fn pthread_setspecific(key: pthread_key_t, value: *const c_void) -> i32;
    pub fn pthread_getspecific(key: pthread_key_t) -> *mut c_void;

    pub fn __relibc_internal_pthread_cleanup_push(new_entry: *mut c_void);
    pub fn __relibc_internal_pthread_cleanup_pop(execute: i32);
    pub fn pthread_exit(retval: *mut c_void) -> !;
}

macro_rules! pthread_cleanup_push {
    ($entry:ident, $routine:expr, $arg:expr) => {
        let mut $entry = CleanupLinkedListEntry {
            routine: $routine,
            arg: $arg,
            prev: core::ptr::null(),
        };
        __relibc_internal_pthread_cleanup_push(core::ptr::from_mut(&mut $entry).cast());
    };
}

macro_rules! pthread_cleanup_pop {
    ($execute:expr) => {
        __relibc_internal_pthread_cleanup_pop($execute);
    };
}

// ── Assembly Entry point ─────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset pthread_main",
    "   and rsp, -16",
    "   call relibc_start_v1",
    "   ud2"
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   mov x29, #0",
    "   mov x30, #0",
    "   mov x0, sp",
    "   adrp x1, pthread_main",
    "   add x1, x1, :lo12:pthread_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn pthread_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    if !test_pthread_create_join() { failures += 1; }
    if !test_pthread_mutex() { failures += 1; }
    if !test_pthread_condvar() { failures += 1; }
    if !test_pthread_tsd() { failures += 1; }
    if !test_pthread_cleanup() { failures += 1; }

    puts(b"--- pthreadtest done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── 1. Create and Join ───────────────────────────────────────────────────────

extern "C" fn create_join_shim(arg: *mut c_void) -> *mut c_void {
    arg
}

unsafe fn test_pthread_create_join() -> bool {
    let name = b"pthread_create_join\0";
    let mut thread: pthread_t = core::ptr::null_mut();
    let magic = 0x12345678 as *mut c_void;

    let r = pthread_create(&mut thread, core::ptr::null(), create_join_shim, magic);
    if r != 0 { return report(name, false); }

    let mut retval: *mut c_void = core::ptr::null_mut();
    let r2 = pthread_join(thread, &mut retval);
    if r2 != 0 { return report(name, false); }

    report(name, retval == magic)
}

// ── 2. Mutex Contention ─────────────────────────────────────────────────────

static mut MUTEX_SHARED_COUNTER: i32 = 0;
static mut MUTEX_LOCK: pthread_mutex_t = pthread_mutex_t { __relibc_internal_align: 0 };

extern "C" fn mutex_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        for _ in 0..2000 {
            pthread_mutex_lock(&raw mut MUTEX_LOCK);
            MUTEX_SHARED_COUNTER += 1;
            pthread_mutex_unlock(&raw mut MUTEX_LOCK);
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_pthread_mutex() -> bool {
    let name = b"pthread_mutex\0";
    MUTEX_SHARED_COUNTER = 0;

    let r = pthread_mutex_init(&raw mut MUTEX_LOCK, core::ptr::null());
    if r != 0 { return report(name, false); }

    let mut t1: pthread_t = core::ptr::null_mut();
    let mut t2: pthread_t = core::ptr::null_mut();

    let r1 = pthread_create(&mut t1, core::ptr::null(), mutex_worker, core::ptr::null_mut());
    let r2 = pthread_create(&mut t2, core::ptr::null(), mutex_worker, core::ptr::null_mut());

    if r1 != 0 || r2 != 0 {
        pthread_mutex_destroy(&raw mut MUTEX_LOCK);
        return report(name, false);
    }

    pthread_join(t1, core::ptr::null_mut());
    pthread_join(t2, core::ptr::null_mut());

    let final_val = MUTEX_SHARED_COUNTER;
    pthread_mutex_destroy(&raw mut MUTEX_LOCK);

    report(name, final_val == 4000)
}

// ── 3. Condvar Wait/Signal ──────────────────────────────────────────────────

static mut COND_MUTEX: pthread_mutex_t = pthread_mutex_t { __relibc_internal_align: 0 };
static mut COND_VAR: pthread_cond_t = pthread_cond_t { __relibc_internal_align: 0 };
static mut COND_READY: i32 = 0;

extern "C" fn cond_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        pthread_mutex_lock(&raw mut COND_MUTEX);
        COND_READY = 1;
        pthread_cond_signal(&raw mut COND_VAR);
        pthread_mutex_unlock(&raw mut COND_MUTEX);
    }
    core::ptr::null_mut()
}

unsafe fn test_pthread_condvar() -> bool {
    let name = b"pthread_condvar\0";
    COND_READY = 0;

    pthread_mutex_init(&raw mut COND_MUTEX, core::ptr::null());
    pthread_cond_init(&raw mut COND_VAR, core::ptr::null());

    let mut t: pthread_t = core::ptr::null_mut();
    let r = pthread_create(&mut t, core::ptr::null(), cond_worker, core::ptr::null_mut());
    if r != 0 {
        pthread_mutex_destroy(&raw mut COND_MUTEX);
        pthread_cond_destroy(&raw mut COND_VAR);
        return report(name, false);
    }

    pthread_mutex_lock(&raw mut COND_MUTEX);
    while COND_READY == 0 {
        pthread_cond_wait(&raw mut COND_VAR, &raw mut COND_MUTEX);
    }
    pthread_mutex_unlock(&raw mut COND_MUTEX);

    pthread_join(t, core::ptr::null_mut());

    pthread_mutex_destroy(&raw mut COND_MUTEX);
    pthread_cond_destroy(&raw mut COND_VAR);

    report(name, true)
}

// ── 4. Thread-Specific Data (TSD) ──────────────────────────────────────────

static mut TSD_DESTRUCTOR_RUNS: i32 = 0;

unsafe extern "C" fn tsd_destructor(arg: *mut c_void) {
    if arg == 0xBAADF00D as *mut c_void {
        TSD_DESTRUCTOR_RUNS += 1;
    }
}

unsafe fn test_pthread_tsd() -> bool {
    let name = b"pthread_tsd\0";
    TSD_DESTRUCTOR_RUNS = 0;

    let mut key: pthread_key_t = 0;
    let r = pthread_key_create(&mut key, Some(tsd_destructor));
    if r != 0 { return report(name, false); }

    extern "C" fn tsd_worker(arg: *mut c_void) -> *mut c_void {
        let k = unsafe { *(arg as *mut pthread_key_t) };
        unsafe {
            pthread_setspecific(k, 0xBAADF00D as *mut c_void);
        }
        core::ptr::null_mut()
    }

    let mut t: pthread_t = core::ptr::null_mut();
    let r2 = pthread_create(&mut t, core::ptr::null(), tsd_worker, &mut key as *mut _ as *mut c_void);
    if r2 != 0 { return report(name, false); }

    pthread_join(t, core::ptr::null_mut());

    report(name, TSD_DESTRUCTOR_RUNS == 1)
}

// ── 5. Cleanup Handlers ─────────────────────────────────────────────────────

static mut CLEANUP_RUNS: i32 = 0;

extern "C" fn cleanup_routine(arg: *mut c_void) {
    let val = arg as usize;
    unsafe {
        CLEANUP_RUNS += val as i32;
    }
}

extern "C" fn cleanup_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        pthread_cleanup_push!(entry, cleanup_routine, 10 as *mut c_void);
        pthread_cleanup_pop!(1);

        pthread_cleanup_push!(entry2, cleanup_routine, 100 as *mut c_void);
        pthread_exit(core::ptr::null_mut());
    }
}

unsafe fn test_pthread_cleanup() -> bool {
    let name = b"pthread_cleanup\0";
    CLEANUP_RUNS = 0;

    let mut t: pthread_t = core::ptr::null_mut();
    let r = pthread_create(&mut t, core::ptr::null(), cleanup_worker, core::ptr::null_mut());
    if r != 0 { return report(name, false); }

    pthread_join(t, core::ptr::null_mut());

    report(name, CLEANUP_RUNS == 110)
}

// ── Helper ──────────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr(), 7);
    } else {
        write(1, b": FAIL\n".as_ptr(), 7);
    }
    passed
}
