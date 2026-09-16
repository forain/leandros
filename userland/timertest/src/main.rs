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
const CLOCK_MONOTONIC: clockid_t = 1;
const TICK_NS: i64 = 10_000_000; // one 100 Hz scheduler tick
const ITIMER_REAL: c_int = 0;
const EAGAIN: c_int = 11;
const MAX_TIMERS: usize = 8;

// timerfd_create/timerfd_settime have no relibc C wrapper (same as
// idletest/wakepolltest) — issued via the raw `syscall` vararg thunk.
#[cfg(target_arch = "x86_64")]
mod nr {
    pub const TIMERFD_CREATE: i64 = 283;
    pub const TIMERFD_SETTIME: i64 = 286;
    pub const TIMERFD_GETTIME: i64 = 287;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const TIMERFD_CREATE: i64 = 85;
    pub const TIMERFD_SETTIME: i64 = 86;
    pub const TIMERFD_GETTIME: i64 = 87;
}

const TFD_TIMER_ABSTIME: i64 = 1;
const TFD_TIMER_CANCEL_ON_SET: i64 = 2;
const POLLIN: i16 = 0x0001;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct pollfd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn read(fd: c_int, buf: *mut u8, count: usize) -> isize;
    pub fn close(fd: c_int) -> c_int;
    pub fn exit(status: i32) -> !;
    pub fn __errno_location() -> *mut c_int;
    pub fn syscall(sysno: c_long, ...) -> c_long;

    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn clock_gettime(clockid: clockid_t, tp: *mut timespec) -> c_int;
    pub fn clock_getres(clockid: clockid_t, res: *mut timespec) -> c_int;
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
    if !test_clock_monotonic_subtick() { failures += 1; }
    if !test_timerfd_subtick_interval() { failures += 1; }
    if !test_timerfd_abstime_future() { failures += 1; }
    if !test_timerfd_abstime_past() { failures += 1; }
    if !test_timerfd_relative_unchanged() { failures += 1; }

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

// ── 6. CLOCK_MONOTONIC advances *inside* a scheduler tick ───────────────────
//
// sys_clock_gettime used to derive the answer from the 100 Hz tick counter
// alone, so CLOCK_MONOTONIC moved in 10 ms steps and clock_getres claimed
// 10 ms.  Nothing above catches that: every check here is a *duration* test
// with milliseconds of slack, and a 10 ms-granular clock satisfies all of
// them.  Userspace nevertheless acts on the difference between two closely
// spaced readings — Mesa's venus ring suppresses its "wake the idle
// renderer" notify whenever this clock says under 1 ms has elapsed since the
// last one, and a clock that cannot resolve 1 ms withholds that notify for a
// whole tick, ten times the renderer's idle timeout.  So assert the property
// directly rather than a duration: sample in a tight loop and require the
// clock to land strictly between tick boundaries and to advance by less than
// a tick, while never stepping backwards.

unsafe fn test_clock_monotonic_subtick() -> bool {
    let name = b"clock_monotonic_subtick\0";

    // Resolution must not claim the whole tick, and must be a real duration.
    let mut res = core::mem::zeroed::<timespec>();
    if clock_getres(CLOCK_MONOTONIC, &mut res) != 0 { return report(name, false); }
    let res_ns = res.tv_sec * 1_000_000_000 + res.tv_nsec;
    let res_ok = res_ns > 0 && res_ns < TICK_NS;

    let mut prev: i64 = -1;
    let mut monotonic = true;      // never steps backwards
    let mut off_boundary = false;  // some reading is not a whole tick
    let mut subtick_step = false;  // some advance is smaller than a tick
    let mut min_step: i64 = i64::MAX;
    let mut first: i64 = 0;
    let mut last: i64 = 0;

    // ~4000 back-to-back readings; even under TCG this spans several ticks,
    // so a tick-granular clock would still show its steps here.
    for i in 0..4000 {
        let mut ts = core::mem::zeroed::<timespec>();
        if clock_gettime(CLOCK_MONOTONIC, &mut ts) != 0 { return report(name, false); }
        let ns = ts.tv_sec * 1_000_000_000 + ts.tv_nsec;
        if i == 0 { first = ns; }
        last = ns;
        if ns % TICK_NS != 0 { off_boundary = true; }
        if prev >= 0 {
            let d = ns - prev;
            if d < 0 { monotonic = false; }
            if d > 0 && d < TICK_NS {
                subtick_step = true;
                if d < min_step { min_step = d; }
            }
        }
        prev = ns;
    }

    // The loop must actually have taken time; otherwise "no backward step"
    // is vacuous.
    let advanced = last > first;

    // Sanity-check the scale as well as the granularity: the interpolated
    // fraction is clamped below one tick, so it can never make the clock
    // drift, but a wildly wrong anchor would show up as an elapsed time that
    // does not resemble the sleep.
    let mut a = core::mem::zeroed::<timespec>();
    let mut b = core::mem::zeroed::<timespec>();
    clock_gettime(CLOCK_MONOTONIC, &mut a);
    sleep_ms(200);
    clock_gettime(CLOCK_MONOTONIC, &mut b);
    let slept = (b.tv_sec * 1_000_000_000 + b.tv_nsec) - (a.tv_sec * 1_000_000_000 + a.tv_nsec);
    let sleep_plausible = slept >= 150_000_000 && slept <= 2_000_000_000;

    print_kv(b"  clock_getres_ns=\0", res_ns as u64);
    print_kv(b"  sleep200ms_measured_ns=\0", slept.max(0) as u64);
    print_kv(b"  min_subtick_step_ns=\0", if min_step == i64::MAX { 0 } else { min_step as u64 });
    print_kv(b"  loop_span_ns=\0", (last - first).max(0) as u64);

    report(name, res_ok && monotonic && off_boundary && subtick_step && advanced && sleep_plausible)
}

// ── 7. timerfd sub-tick periodic interval must not decay to one-shot ────────
//
// handle_timerfd_settime converts it_interval from nanoseconds to 100 Hz
// scheduler ticks by dividing by NS_PER_TICK (10ms). A 5ms interval used to
// truncate to 0 ticks, and 0 ticks means one-shot to the rest of the timerfd
// machinery (timerfd_poll_expirations, fold_expired_timerfds), so a timerfd
// armed with a sub-tick period fired once and never rearmed. Regression for
// the fix: it_interval floors at 1 tick whenever a nonzero interval_ns was
// requested.

unsafe fn test_timerfd_subtick_interval() -> bool {
    let name = b"timerfd_subtick_interval\0";

    let tfd = syscall(nr::TIMERFD_CREATE, CLOCK_MONOTONIC as c_long, 0i64) as c_int;
    if tfd < 0 { return report(name, false); }

    // 5ms value and interval — both shorter than the 10ms scheduler tick.
    let its = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 5_000_000 },
        it_value:    timespec { tv_sec: 0, tv_nsec: 5_000_000 },
    };
    if syscall(nr::TIMERFD_SETTIME, tfd as c_long, 0i64,
        &its as *const itimerspec as c_long, 0i64) != 0 {
        close(tfd);
        return report(name, false);
    }

    // ~100ms at a 5ms period should yield well over one expiration if the
    // timer keeps rearming; a decayed one-shot would report exactly 1.
    sleep_ms(100);

    let mut count: u64 = 0;
    let n = read(tfd, &mut count as *mut u64 as *mut u8, 8);

    close(tfd);
    report(name, n == 8 && count >= 2)
}

// ── 8–10. timerfd TFD_TIMER_ABSTIME ─────────────────────────────────────────
//
// sys_timerfd_settime used to drop its flags argument, so an absolute
// it_value (a CLOCK_MONOTONIC reading) was armed as a *relative* interval and
// fired at `now + now + delta` — roughly "uptime from now", growing with
// uptime. The same landmine class as clock_nanosleep's TIMER_ABSTIME and
// FUTEX_WAIT_BITSET before their fixes. These cases compute every expected
// value from clock_gettime at run time so they hold at any uptime: run them
// a few seconds after boot at least, when the bug's error would be seconds,
// not the tens of milliseconds the bounds tolerate.

unsafe fn now_ns() -> i64 {
    let mut ts = core::mem::zeroed::<timespec>();
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

fn ts_from_ns(ns: i64) -> timespec {
    timespec { tv_sec: ns / 1_000_000_000, tv_nsec: ns % 1_000_000_000 }
}

fn ts_to_ns(ts: &timespec) -> i64 { ts.tv_sec * 1_000_000_000 + ts.tv_nsec }

unsafe fn tfd_settime(tfd: c_int, flags: i64, its: *const itimerspec, old: *mut itimerspec) -> c_long {
    syscall(nr::TIMERFD_SETTIME, tfd as c_long, flags, its as c_long, old as c_long)
}

unsafe fn tfd_gettime(tfd: c_int) -> Option<itimerspec> {
    let mut cur = core::mem::zeroed::<itimerspec>();
    if syscall(nr::TIMERFD_GETTIME, tfd as c_long, &mut cur as *mut itimerspec as c_long) != 0 {
        return None;
    }
    Some(cur)
}

/// Wait up to `timeout_ms` for the timerfd to become readable, then read it.
/// Returns (expiration count, elapsed ns since `t0`), or None on failure.
unsafe fn tfd_wait_read(tfd: c_int, t0: i64, timeout_ms: c_int) -> Option<(u64, i64)> {
    let mut pfd = pollfd { fd: tfd, events: POLLIN, revents: 0 };
    let pr = poll(&mut pfd, 1, timeout_ms);
    let elapsed = now_ns() - t0;
    if pr != 1 || pfd.revents & POLLIN == 0 { return None; }
    let mut count: u64 = 0;
    if read(tfd, &mut count as *mut u64 as *mut u8, 8) != 8 { return None; }
    Some((count, elapsed))
}

// 8. An absolute deadline 300 ms ahead fires in [300, 400) ms, and gettime
//    reports the remaining time (not the absolute timestamp) while armed.
unsafe fn test_timerfd_abstime_future() -> bool {
    let name = b"timerfd_abstime_future\0";

    let tfd = syscall(nr::TIMERFD_CREATE, CLOCK_MONOTONIC as c_long, 0i64) as c_int;
    if tfd < 0 { return report(name, false); }

    let t0 = now_ns();
    let its = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 0 },
        it_value:    ts_from_ns(t0 + 300_000_000),
    };
    // CANCEL_ON_SET rides along: it must be accepted (ignored), not EINVAL.
    if tfd_settime(tfd, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, &its, core::ptr::null_mut()) != 0 {
        close(tfd);
        return report(name, false);
    }

    // Remaining time must be relative and within the deadline: an
    // implementation that stored the absolute value as an interval reports
    // ~uptime here.
    let armed = match tfd_gettime(tfd) { Some(c) => c, None => { close(tfd); return report(name, false); } };
    let remaining = ts_to_ns(&armed.it_value);
    let remaining_ok = remaining > 0 && remaining <= 300_000_000 + 2 * TICK_NS
        && ts_to_ns(&armed.it_interval) == 0;

    let fired = tfd_wait_read(tfd, t0, 2000);
    let after = tfd_gettime(tfd);
    close(tfd);

    let (count, elapsed) = match fired { Some(v) => v, None => (0, -1) };
    let fired_ok = count == 1 && elapsed >= 300_000_000 && elapsed < 400_000_000;
    // One-shot: disarmed after the expiry was read.
    let disarmed_ok = match after { Some(c) => ts_to_ns(&c.it_value) == 0, None => false };

    print_kv(b"  abstime_future_remaining_ns=\0", remaining.max(0) as u64);
    print_kv(b"  abstime_future_elapsed_ns=\0", elapsed.max(0) as u64);
    print_kv(b"  abstime_future_count=\0", count);
    report(name, remaining_ok && fired_ok && disarmed_ok)
}

// 9. An absolute deadline already in the past fires immediately, exactly
//    once, and leaves the one-shot disarmed.
unsafe fn test_timerfd_abstime_past() -> bool {
    let name = b"timerfd_abstime_past\0";

    let tfd = syscall(nr::TIMERFD_CREATE, CLOCK_MONOTONIC as c_long, 0i64) as c_int;
    if tfd < 0 { return report(name, false); }

    let t0 = now_ns();
    // Half of uptime ago, floored at 1 ns (0 would mean "disarm").
    let past = (t0 / 2).max(1);
    let its = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 0 },
        it_value:    ts_from_ns(past),
    };
    if tfd_settime(tfd, TFD_TIMER_ABSTIME, &its, core::ptr::null_mut()) != 0 {
        close(tfd);
        return report(name, false);
    }

    let fired = tfd_wait_read(tfd, t0, 2000);
    let after = tfd_gettime(tfd);
    close(tfd);

    let (count, elapsed) = match fired { Some(v) => v, None => (0, -1) };
    // "Immediately": well under the 300 ms a live deadline would take, with
    // room for a couple of scheduler ticks of wake latency.
    let fired_ok = count == 1 && elapsed >= 0 && elapsed < 100_000_000;
    let disarmed_ok = match after { Some(c) => ts_to_ns(&c.it_value) == 0, None => false };

    print_kv(b"  abstime_past_elapsed_ns=\0", elapsed.max(0) as u64);
    print_kv(b"  abstime_past_count=\0", count);
    report(name, fired_ok && disarmed_ok)
}

// 10. A relative 300 ms one-shot still fires in [300, 400) ms, and the
//     old_value out-parameter reports the setting being replaced.
unsafe fn test_timerfd_relative_unchanged() -> bool {
    let name = b"timerfd_relative_unchanged\0";

    let tfd = syscall(nr::TIMERFD_CREATE, CLOCK_MONOTONIC as c_long, 0i64) as c_int;
    if tfd < 0 { return report(name, false); }

    // Arm a far-off timer first so the real arming below has something to
    // report back through old_value.
    let far = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 0 },
        it_value:    timespec { tv_sec: 10, tv_nsec: 0 },
    };
    if tfd_settime(tfd, 0, &far, core::ptr::null_mut()) != 0 { close(tfd); return report(name, false); }

    let t0 = now_ns();
    let its = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 0 },
        it_value:    timespec { tv_sec: 0, tv_nsec: 300_000_000 },
    };
    let mut old = core::mem::zeroed::<itimerspec>();
    if tfd_settime(tfd, 0, &its, &mut old) != 0 { close(tfd); return report(name, false); }
    let old_ns = ts_to_ns(&old.it_value);
    let old_ok = old_ns > 9_000_000_000 && old_ns <= 10_000_000_000 + 2 * TICK_NS;

    let armed = match tfd_gettime(tfd) { Some(c) => c, None => { close(tfd); return report(name, false); } };
    let remaining = ts_to_ns(&armed.it_value);
    let remaining_ok = remaining > 0 && remaining <= 300_000_000 + 2 * TICK_NS;

    let fired = tfd_wait_read(tfd, t0, 2000);
    close(tfd);

    let (count, elapsed) = match fired { Some(v) => v, None => (0, -1) };
    let fired_ok = count == 1 && elapsed >= 300_000_000 && elapsed < 400_000_000;

    print_kv(b"  relative_old_value_ns=\0", old_ns.max(0) as u64);
    print_kv(b"  relative_remaining_ns=\0", remaining.max(0) as u64);
    print_kv(b"  relative_elapsed_ns=\0", elapsed.max(0) as u64);
    report(name, old_ok && remaining_ok && fired_ok)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Print `label` followed by `v` in decimal and a newline — no libc formatting
/// available in this no_std binary.
unsafe fn print_kv(label: &[u8], v: u64) {
    write(1, label.as_ptr(), label.len() - 1);
    let mut buf = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    loop {
        buf[n] = b'0' + (x % 10) as u8;
        n += 1;
        x /= 10;
        if x == 0 { break; }
    }
    let mut out = [0u8; 20];
    for i in 0..n { out[i] = buf[n - 1 - i]; }
    write(1, out.as_ptr(), n);
    write(1, b"\n".as_ptr(), 1);
}

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr(), 7);
    } else {
        write(1, b": FAIL\n".as_ptr(), 7);
    }
    passed
}
