//! idletest — proves that `epoll_wait` actually BLOCKS the calling thread
//! (near-zero CPU consumed) instead of busy-spinning while waiting on fds
//! that never become ready. Companion to polltest (which checks epoll's
//! *readiness reporting* is correct) — this checks epoll's *waiting* is
//! real, via a getrusage() CPU-time delta over a ~3s idle window bounded
//! by a timerfd.
//!
//! Registers 4 eventfds that are never written to (mimicking idle
//! never-fires wakers, e.g. tokio/mio reactor registrations that just sit
//! there) plus a one-shot 3s CLOCK_MONOTONIC timerfd, then blocks in
//! epoll_wait(timeout=-1) until the timerfd (and only the timerfd) wakes
//! it. If epoll_wait were spinning instead of really blocking, the
//! getrusage() utime+stime delta across the wait would be close to the
//! full 3s of wall time instead of a small fraction of it.
//!
//! eventfd2/timerfd_create/timerfd_settime have no C wrapper in relibc
//! (only the raw syscall numbers exist there — see
//! userland/relibc/src/header/sys_syscall/{x86_64,aarch64}.rs), so those
//! three go through the raw `syscall()` entry point; everything else
//! (epoll_create1/epoll_ctl/epoll_wait, getrusage, read/write/close) is a
//! real relibc C function, exercised the same way polltest/timertest do.
//!
//! Initializes via relibc_start_v1 (same as polltest/timertest/sigtest)
//! so TLS is set up — errno and the real epoll_*()/getrusage() Pal calls
//! both need it.
//!
//! Prints "IDLE_START", then "<name>: PASS"/"FAIL" per check, then a
//! final `SUMMARY pass=<P> fail=<F>` line; `idle_main` returns the number
//! of failures as the exit code.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_long = i64;
type c_uint = u32;
type c_ulonglong = u64;
type time_t = i64;
type suseconds_t = i64;
type clockid_t = c_int;

const EPOLLIN: c_uint = 0x001;
const EPOLL_CTL_ADD: c_int = 1;

const CLOCK_MONOTONIC: clockid_t = 1;
const RUSAGE_SELF: c_int = 0;

// Raw syscall numbers for the three functions relibc doesn't wrap with a C
// entry point. Values from userland/relibc/src/header/sys_syscall/{x86_64,
// aarch64}.rs (also matches kernel/src/syscall.rs's dispatch table).
#[cfg(target_arch = "x86_64")]
mod nr {
    pub const EVENTFD2: i64 = 290;
    pub const TIMERFD_CREATE: i64 = 283;
    pub const TIMERFD_SETTIME: i64 = 286;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const EVENTFD2: i64 = 19;
    pub const TIMERFD_CREATE: i64 = 85;
    pub const TIMERFD_SETTIME: i64 = 86;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union epoll_data {
    pub ptr: *mut c_void,
    pub fd: c_int,
    pub u32: c_uint,
    pub u64: c_ulonglong,
}

/// Must match `userland/relibc/src/header/sys_epoll/mod.rs`'s `epoll_event`
/// exactly (see that struct's doc comment): packed to 12 bytes (data at
/// offset 4) on x86_64 only; natural 16-byte layout everywhere else.
#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct epoll_event {
    pub events: c_uint,
    pub data: epoll_data,
}
#[cfg(not(target_arch = "x86_64"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct epoll_event {
    pub events: c_uint,
    pub data: epoll_data,
}

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
    pub tv_usec: suseconds_t,
}

/// Matches `userland/relibc/src/header/sys_resource/mod.rs`'s `rusage`
/// exactly: `ru_utime` at offset 0, `ru_stime` at offset 16 (each a
/// 16-byte timeval), followed by the `c_long` accounting fields we don't
/// otherwise touch here.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rusage {
    pub ru_utime: timeval,
    pub ru_stime: timeval,
    pub ru_maxrss: c_long,
    pub ru_ixrss: c_long,
    pub ru_idrss: c_long,
    pub ru_isrss: c_long,
    pub ru_minflt: c_long,
    pub ru_majflt: c_long,
    pub ru_nswap: c_long,
    pub ru_inblock: c_long,
    pub ru_oublock: c_long,
    pub ru_msgsnd: c_long,
    pub ru_msgrcv: c_long,
    pub ru_nsignals: c_long,
    pub ru_nvcsw: c_long,
    pub ru_nivcsw: c_long,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    pub fn close(fd: i32) -> i32;
    pub fn exit(status: i32) -> !;

    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;

    pub fn getrusage(who: c_int, r_usage: *mut rusage) -> c_int;

    // eventfd2/timerfd_create/timerfd_settime have no relibc C wrapper —
    // go straight through the raw syscall entry point (nr module above).
    pub fn syscall(sysno: c_long, ...) -> c_long;
}

// ── Assembly entry point (identical to polltest's/timertest's) ─────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset idle_main",
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
    "   adrp x1, idle_main",
    "   add x1, x1, :lo12:idle_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

/// PASS threshold: at most this many microseconds of utime+stime may be
/// consumed while blocked across the ~3s wall-clock idle window. A busy
/// spin would burn essentially all 3s of CPU; a real block burns close to
/// nothing (scheduler bookkeeping, the eventual timerfd wakeup itself).
const IDLE_CPU_THRESHOLD_US: i64 = 150_000;

/// Guards against a spin bug turning "wait for the timer" into an infinite
/// loop: if epoll_wait keeps returning without ever showing the timerfd
/// ready, give up after this many iterations rather than hanging forever.
const MAX_ITERATIONS: u32 = 100;

#[no_mangle]
pub unsafe extern "C" fn idle_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    let (idle_cpu_ok, timer_wake_ok) = run_idle_scenario();

    if !report(b"idle_cpu\0", idle_cpu_ok) { failures += 1; }
    if !report(b"timer_wake\0", timer_wake_ok) { failures += 1; }

    write_summary(failures);
    puts(b"--- idletest done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── Scenario ─────────────────────────────────────────────────────────────

unsafe fn run_idle_scenario() -> (bool, bool) {
    let ep = epoll_create1(0);
    if ep < 0 { return (false, false); }

    // 4 quiet eventfds: registered, never written to — like idle reactor
    // wakers sitting in an epoll set with nothing to say.
    let mut ev_fds = [0i32; 4];
    for slot in ev_fds.iter_mut() {
        let fd = syscall(nr::EVENTFD2, 0i64, 0i64) as i32;
        if fd < 0 { return (false, false); }
        *slot = fd;
        let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd } };
        epoll_ctl(ep, EPOLL_CTL_ADD, fd, &mut ev);
    }

    // One-shot 3s CLOCK_MONOTONIC timerfd — the only fd that will ever
    // become ready.
    let tfd = syscall(nr::TIMERFD_CREATE, CLOCK_MONOTONIC as i64, 0i64) as i32;
    if tfd < 0 { return (false, false); }
    let its = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 0 },
        it_value: timespec { tv_sec: 3, tv_nsec: 0 },
    };
    let settime_rc = syscall(
        nr::TIMERFD_SETTIME,
        tfd as i64,
        0i64,
        &its as *const itimerspec as *const c_void,
        core::ptr::null::<c_void>(),
    );
    if settime_rc != 0 {
        return (false, false);
    }
    let mut tfd_ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: tfd } };
    epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &mut tfd_ev);

    puts(b"IDLE_START\0".as_ptr());

    let mut before: rusage = core::mem::zeroed();
    getrusage(RUSAGE_SELF, &mut before);
    let before_us = rusage_total_us(&before);

    let mut events: [epoll_event; 8] = core::mem::zeroed();
    let mut timer_woke = false;
    let mut iterations: u32 = 0;
    while iterations < MAX_ITERATIONS {
        iterations += 1;
        let n = epoll_wait(ep, events.as_mut_ptr(), 8, -1);
        if n <= 0 { continue; }
        for i in 0..(n as usize) {
            let fd = events[i].data.fd;
            if fd == tfd {
                let mut buf = [0u8; 8];
                read(tfd, buf.as_mut_ptr(), 8);
                timer_woke = true;
            }
        }
        if timer_woke { break; }
    }

    let mut after: rusage = core::mem::zeroed();
    getrusage(RUSAGE_SELF, &mut after);
    let after_us = rusage_total_us(&after);

    let delta_us = after_us - before_us;
    write(1, b"IDLE_CPU_US ".as_ptr(), 12);
    write_i64(1, delta_us);
    write(1, b"\n".as_ptr(), 1);

    for fd in ev_fds { close(fd); }
    close(tfd);
    close(ep);

    let idle_cpu_ok = delta_us < IDLE_CPU_THRESHOLD_US;
    (idle_cpu_ok, timer_woke)
}

fn rusage_total_us(ru: &rusage) -> i64 {
    ru.ru_utime.tv_sec * 1_000_000 + ru.ru_utime.tv_usec
        + ru.ru_stime.tv_sec * 1_000_000 + ru.ru_stime.tv_usec
}

// ── Helpers ──────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr(), 7);
    } else {
        write(1, b": FAIL\n".as_ptr(), 7);
    }
    passed
}

unsafe fn write_i64(fd: i32, v: i64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let neg = v < 0;
    // i64::MIN can't be negated safely; not a realistic delta here, but
    // work in u64 magnitude to stay correct regardless.
    let mut mag: u64 = if neg { (v as i128).unsigned_abs() as u64 } else { v as u64 };
    if mag == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while mag > 0 {
            i -= 1;
            buf[i] = b'0' + (mag % 10) as u8;
            mag /= 10;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    write(fd, buf[i..].as_ptr(), buf.len() - i);
}

unsafe fn write_summary(failures: i32) {
    let pass = 2 - failures;
    write(1, b"SUMMARY pass=".as_ptr(), 13);
    write_i64(1, pass as i64);
    write(1, b" fail=".as_ptr(), 6);
    write_i64(1, failures as i64);
    write(1, b"\n".as_ptr(), 1);
}
