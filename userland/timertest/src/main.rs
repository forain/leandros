//! timertest — standalone regression coverage for TODO.md Phase 8 (POSIX
//! Timers): timer_create/settime/gettime/delete/getoverrun, setitimer/
//! getitimer, alarm(), and real end-to-end SIGALRM delivery.
//!
//! Initializes via relibc_start_v1 (same as pthreadtest) so TLS is set up
//! properly — errno and the sigaction SA_RESTORER trampoline both need it.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `timer_main` returns the number of failures as the exit code.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicI32, Ordering};

type c_int = i32;
type c_long = i64;
type time_t = i64;
type clockid_t = c_int;
type timer_t = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    pub tv_sec: time_t,
    pub tv_nsec: c_long,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct itimerspec {
    pub it_interval: timespec,
    pub it_value: timespec,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timeval {
    pub tv_sec: time_t,
    pub tv_usec: time_t,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct itimerval {
    pub it_interval: timeval,
    pub it_value: timeval,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union sigval {
    pub sival_int: c_int,
    pub sival_ptr: *mut c_void,
}

/// Matches relibc's `#[cfg(any(target_os = "linux", target_os = "leandros"))]`
/// layout in header/signal/mod.rs exactly (64 bytes on 64-bit).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct sigevent {
    pub sigev_value: sigval,
    pub sigev_signo: c_int,
    pub sigev_notify: c_int,
    pub sigev_notify_thread_id: c_int,
    pub __unused1: [c_int; 11],
}

pub type sigset_t = u64;

#[repr(C)]
pub struct sigaction {
    pub sa_handler: Option<extern "C" fn(c_int)>,
    pub sa_flags: c_int,
    pub sa_restorer: Option<unsafe extern "C" fn()>,
    pub sa_mask: sigset_t,
}

const SIGEV_SIGNAL: c_int = 0;
const SIGALRM: c_int = 14;
const CLOCK_REALTIME: clockid_t = 0;
const ITIMER_REAL: c_int = 0;
const EAGAIN: c_int = 11;
const MAX_TIMERS: usize = 8;

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn exit(status: i32) -> !;
    pub fn __errno_location() -> *mut c_int;

    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn sigaction(sig: c_int, act: *const sigaction, oact: *mut sigaction) -> c_int;

    pub fn timer_create(clockid: clockid_t, evp: *mut sigevent, timerid: *mut timer_t) -> c_int;
    pub fn timer_settime(
        timerid: timer_t,
        flags: c_int,
        value: *const itimerspec,
        ovalue: *mut itimerspec,
    ) -> c_int;
    pub fn timer_gettime(timerid: timer_t, value: *mut itimerspec) -> c_int;
    pub fn timer_getoverrun(timerid: timer_t) -> c_int;
    pub fn timer_delete(timerid: timer_t) -> c_int;

    pub fn setitimer(which: c_int, value: *const itimerval, ovalue: *mut itimerval) -> c_int;
    pub fn getitimer(which: c_int, value: *mut itimerval) -> c_int;
    pub fn alarm(seconds: u32) -> u32;
}

// ── Assembly entry point (identical to pthreadtest's) ───────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset timer_main",
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
    "   adrp x1, timer_main",
    "   add x1, x1, :lo12:timer_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn timer_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    if !test_timer_create_delete_slot_zero() { failures += 1; }
    if !test_timer_oneshot_signal_delivery() { failures += 1; }
    if !test_timer_periodic_overrun() { failures += 1; }
    if !test_timer_max_and_eagain() { failures += 1; }
    if !test_alarm_and_setitimer_no_leak() { failures += 1; }

    puts(b"--- timertest done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── Shared SIGALRM handler ───────────────────────────────────────────────────

static SIGALRM_COUNT: AtomicI32 = AtomicI32::new(0);

extern "C" fn sigalrm_handler(_sig: c_int) {
    SIGALRM_COUNT.fetch_add(1, Ordering::SeqCst);
}

unsafe fn install_sigalrm_handler() -> bool {
    let act = sigaction {
        sa_handler: Some(sigalrm_handler),
        sa_flags: 0,
        sa_restorer: None,
        sa_mask: 0,
    };
    sigaction(SIGALRM, &act, core::ptr::null_mut()) == 0
}

fn zeroed_sigevent(signo: c_int) -> sigevent {
    sigevent {
        sigev_value: sigval { sival_int: 0 },
        sigev_signo: signo,
        sigev_notify: SIGEV_SIGNAL,
        sigev_notify_thread_id: 0,
        __unused1: [0; 11],
    }
}

fn sleep_ms(ms: i64) {
    let req = timespec { tv_sec: ms / 1000, tv_nsec: (ms % 1000) * 1_000_000 };
    unsafe { nanosleep(&req, core::ptr::null_mut()); }
}

// ── 1. Slot-0 round trip ─────────────────────────────────────────────────────
//
// The very first timer any process creates lands in table slot 0.  relibc's
// timer_settime/gettime/delete/getoverrun all reject a NULL `timer_t` as
// EFAULT, and slot 0 cast to a raw pointer *is* NULL unless the server
// offsets its handles by one — this is a regression test for that fix.

unsafe fn test_timer_create_delete_slot_zero() -> bool {
    let name = b"timer_create_delete_slot_zero\0";

    let mut evp = zeroed_sigevent(SIGALRM);
    let mut tid: timer_t = core::ptr::null_mut();
    if timer_create(CLOCK_REALTIME, &mut evp, &mut tid) != 0 { return report(name, false); }
    if tid.is_null() { return report(name, false); } // would collide with EFAULT checks

    let far_future = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 0 },
        it_value: timespec { tv_sec: 10, tv_nsec: 0 },
    };
    if timer_settime(tid, 0, &far_future, core::ptr::null_mut()) != 0 {
        timer_delete(tid);
        return report(name, false);
    }

    let mut cur = core::mem::zeroed::<itimerspec>();
    if timer_gettime(tid, &mut cur) != 0 || cur.it_value.tv_sec == 0 {
        timer_delete(tid);
        return report(name, false);
    }

    let ok = timer_delete(tid) == 0;
    report(name, ok)
}

// ── 2. Real one-shot SIGALRM delivery ────────────────────────────────────────

unsafe fn test_timer_oneshot_signal_delivery() -> bool {
    let name = b"timer_oneshot_signal_delivery\0";
    SIGALRM_COUNT.store(0, Ordering::SeqCst);
    if !install_sigalrm_handler() { return report(name, false); }

    let mut evp = zeroed_sigevent(SIGALRM);
    let mut tid: timer_t = core::ptr::null_mut();
    if timer_create(CLOCK_REALTIME, &mut evp, &mut tid) != 0 { return report(name, false); }

    let spec = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 0 },
        it_value: timespec { tv_sec: 0, tv_nsec: 200_000_000 }, // 200ms, one-shot
    };
    if timer_settime(tid, 0, &spec, core::ptr::null_mut()) != 0 {
        timer_delete(tid);
        return report(name, false);
    }

    // Each syscall return re-checks timer expiry, so a series of short
    // sleeps guarantees delivery gets noticed rather than a single sleep
    // racing the deadline. Bounded at ~2s total.
    let mut fired = false;
    for _ in 0..200 {
        sleep_ms(10);
        if SIGALRM_COUNT.load(Ordering::SeqCst) > 0 { fired = true; break; }
    }

    timer_delete(tid);
    report(name, fired && SIGALRM_COUNT.load(Ordering::SeqCst) == 1)
}

// ── 3. Periodic timer: missed-interval catch-up + overrun accounting ────────

unsafe fn test_timer_periodic_overrun() -> bool {
    let name = b"timer_periodic_overrun\0";
    SIGALRM_COUNT.store(0, Ordering::SeqCst);
    if !install_sigalrm_handler() { return report(name, false); }

    let mut evp = zeroed_sigevent(SIGALRM);
    let mut tid: timer_t = core::ptr::null_mut();
    if timer_create(CLOCK_REALTIME, &mut evp, &mut tid) != 0 { return report(name, false); }

    // 20ms period. A single 300ms sleep lets ~14 periods elapse without any
    // syscall in between to notice them individually, so check_timers()
    // must catch the deadline up in one step and fold the extra
    // expirations into the overrun counter rather than losing them.
    let spec = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 20_000_000 },
        it_value:    timespec { tv_sec: 0, tv_nsec: 20_000_000 },
    };
    if timer_settime(tid, 0, &spec, core::ptr::null_mut()) != 0 {
        timer_delete(tid);
        return report(name, false);
    }

    sleep_ms(300);

    let fired_once = SIGALRM_COUNT.load(Ordering::SeqCst) >= 1;
    let overrun = timer_getoverrun(tid);
    // Querying again immediately should report 0 — overrun resets on read.
    let overrun_after_read = timer_getoverrun(tid);

    timer_delete(tid);
    report(name, fired_once && overrun > 0 && overrun_after_read == 0)
}

// ── 4. MAX_TIMERS exhaustion / EAGAIN boundary ───────────────────────────────

unsafe fn test_timer_max_and_eagain() -> bool {
    let name = b"timer_max_and_eagain\0";
    let mut ids = [core::ptr::null_mut::<c_void>(); MAX_TIMERS];
    let mut created = 0usize;

    for slot in ids.iter_mut() {
        let mut evp = zeroed_sigevent(SIGALRM);
        let mut tid: timer_t = core::ptr::null_mut();
        if timer_create(CLOCK_REALTIME, &mut evp, &mut tid) != 0 { break; }
        *slot = tid;
        created += 1;
    }

    let all_slots_used = created == MAX_TIMERS;

    // One more should fail with EAGAIN (table full), not silently succeed.
    let mut extra_evp = zeroed_sigevent(SIGALRM);
    let mut extra_tid: timer_t = core::ptr::null_mut();
    let extra_rc = timer_create(CLOCK_REALTIME, &mut extra_evp, &mut extra_tid);
    let rejected_with_eagain = extra_rc != 0 && *__errno_location() == EAGAIN;

    for &tid in ids.iter().take(created) {
        timer_delete(tid);
    }

    report(name, all_slots_used && rejected_with_eagain)
}

// ── 5. alarm()/setitimer() share one reserved slot, no leak ─────────────────
//
// alarm() and setitimer(ITIMER_REAL) both rearm one implicit per-process
// timer distinct from timer_create()'d ones. A prior bug re-created that
// timer from scratch on every call instead of reusing it, leaking a table
// slot each time until timer_create() started failing with EAGAIN even
// with none of the caller's own timers outstanding.

unsafe fn test_alarm_and_setitimer_no_leak() -> bool {
    let name = b"alarm_and_setitimer_no_leak\0";

    let first = alarm(2);
    let second = alarm(2); // rearm — must report the ~2s left on the first call
    let rearm_reported_remaining = first == 0 && second > 0;

    let disarm = itimerval {
        it_interval: timeval { tv_sec: 0, tv_usec: 0 },
        it_value:    timeval { tv_sec: 0, tv_usec: 0 },
    };
    let mut old = core::mem::zeroed::<itimerval>();
    let setitimer_ok = setitimer(ITIMER_REAL, &disarm, &mut old) == 0;
    // The alarm() calls above should be reflected as the "old" value here —
    // same underlying slot, not a second independent timer.
    let shared_slot = old.it_value.tv_sec > 0 || old.it_value.tv_usec > 0;

    let mut confirm = core::mem::zeroed::<itimerval>();
    let getitimer_ok = getitimer(ITIMER_REAL, &mut confirm) == 0
        && confirm.it_value.tv_sec == 0 && confirm.it_value.tv_usec == 0;

    // With slot 0 permanently claimed by alarm()/setitimer(), exactly
    // MAX_TIMERS - 1 slots must remain free for ordinary timer_create().
    let mut ids = [core::ptr::null_mut::<c_void>(); MAX_TIMERS];
    let mut created = 0usize;
    for slot in ids.iter_mut() {
        let mut evp = zeroed_sigevent(SIGALRM);
        let mut tid: timer_t = core::ptr::null_mut();
        if timer_create(CLOCK_REALTIME, &mut evp, &mut tid) != 0 { break; }
        *slot = tid;
        created += 1;
    }
    for &tid in ids.iter().take(created) {
        timer_delete(tid);
    }
    let no_leak = created == MAX_TIMERS - 1;

    report(name, rearm_reported_remaining && setitimer_ok && shared_slot && getitimer_ok && no_leak)
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
