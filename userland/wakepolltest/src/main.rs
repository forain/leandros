//! wakepolltest — regression coverage for the epoll/poll WAKE path: an edge
//! that arrives while a task is already parked in a blocking epoll_wait must
//! wake it. epolltest only ever writes BEFORE epoll_wait (it exercises the
//! probe, not the wake); this test parks first, then delivers the edge from a
//! second thread (or a timer), which is the exact shape busd's tokio reactor
//! needs (park in epoll_wait, get woken by the waker-eventfd written from the
//! task that just became runnable). See the M7 W1 kernel poll/wake hole.
//!
//! Method (bounded, no hang risk): every observation uses a LONG FINITE
//! timeout as its window and measures the ELAPSED time with CLOCK_MONOTONIC.
//! The kernel's edge-wake path (wake_poll -> unblock_port) is identical for
//! finite and infinite waits, so a lost edge shows up unambiguously as "woke
//! only when the finite timeout expired" (elapsed ~= WINDOW) rather than "woke
//! promptly on the edge" (elapsed ~= stimulus delay). PASS if the wait
//! returned events at ~STIMULUS_MS; FAIL if it slept the whole WINDOW.
//!
//! Initializes via relibc_start_v1 (TLS) exactly like epolltest/pthreadtest.
//! Each check prints "<name>: PASS" / "<name>: FAIL (...)"; wake_main returns
//! the failure count and prints a final SUMMARY line.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_long = i64;
type c_uint = u32;
type size_t = usize;
type ssize_t = isize;
type time_t = i64;
type clockid_t = c_int;

const EPOLLIN: c_uint = 0x001;
const EPOLLET: c_uint = 1 << 31;
const EPOLL_CTL_ADD: c_int = 1;
const CLOCK_MONOTONIC: clockid_t = 1;

// Stimulus fires ~1s after the parked wait begins; the observation window is
// 6s. A prompt edge-wake returns near 1000ms; a lost edge sleeps to ~6000ms.
const STIMULUS_MS: i64 = 1000;
const WINDOW_MS: c_int = 6000;
// Classify: a wait that returned an event before this bound was woken by the
// edge; at/after it, only the timeout (or nothing) woke it.
const PROMPT_MS: i64 = 3500;

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const EVENTFD2: i64 = 290;
    pub const TIMERFD_CREATE: i64 = 283;
    pub const TIMERFD_SETTIME: i64 = 286;
    pub const SOCKETPAIR: i64 = 53;
    pub const FUTEX: i64 = 202;
    pub const SOCKET: i64 = 41;
    pub const BIND: i64 = 49;
    pub const LISTEN: i64 = 50;
    pub const CONNECT: i64 = 42;
    pub const ACCEPT: i64 = 43;
    pub const ACCEPT4: i64 = 288;
    pub const RECVFROM: i64 = 45;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const EVENTFD2: i64 = 19;
    pub const TIMERFD_CREATE: i64 = 85;
    pub const TIMERFD_SETTIME: i64 = 86;
    pub const SOCKETPAIR: i64 = 199;
    pub const FUTEX: i64 = 98;
    pub const SOCKET: i64 = 198;
    pub const BIND: i64 = 200;
    pub const LISTEN: i64 = 201;
    pub const CONNECT: i64 = 203;
    pub const ACCEPT: i64 = 202;
    pub const ACCEPT4: i64 = 242;
    pub const RECVFROM: i64 = 207;
}

pub type pthread_t = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub union epoll_data {
    pub ptr: *mut c_void,
    pub fd: c_int,
    pub u32: c_uint,
    pub u64: u64,
}

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

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const u8, count: size_t) -> ssize_t;
    pub fn read(fd: c_int, buf: *mut u8, count: size_t) -> ssize_t;
    pub fn close(fd: c_int) -> c_int;
    pub fn exit(status: c_int) -> !;
    pub fn usleep(usec: c_uint) -> c_int;

    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;

    pub fn clock_gettime(clockid: clockid_t, tp: *mut timespec) -> c_int;

    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;

    pub fn syscall(sysno: c_long, ...) -> c_long;
}

// ── entry (identical shim to epolltest) ────────────────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset wake_main",
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
    "   adrp x1, wake_main",
    "   add x1, x1, :lo12:wake_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── helpers ────────────────────────────────────────────────────────────────

unsafe fn now_ms() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

fn report(name: &[u8], ok: bool, elapsed: i64) -> bool {
    unsafe {
        let mut line = [0u8; 96];
        let mut p = 0usize;
        for &b in name { line[p] = b; p += 1; }
        let tag: &[u8] = if ok { b": PASS (" } else { b": FAIL (" };
        for &b in tag { line[p] = b; p += 1; }
        // elapsed ms
        let mut e = elapsed;
        if e < 0 { e = 0; }
        let mut digits = [0u8; 12];
        let mut d = 0usize;
        if e == 0 { digits[d] = b'0'; d += 1; }
        while e > 0 { digits[d] = b'0' + (e % 10) as u8; e /= 10; d += 1; }
        while d > 0 { d -= 1; line[p] = digits[d]; p += 1; }
        for &b in b"ms)\n\0" { line[p] = b; p += 1; }
        puts(line.as_ptr());
    }
    ok
}

// Detailed report for the discriminating xthread tests: appends n (epoll_wait
// return) and the writer's actual write time. FAIL classification:
//   wrote>=0 && el~WINDOW && n>=1  -> writer wrote on time, reader edge-wake LOST
//   wrote<0  || wrote~WINDOW       -> writer's sleep stranded (deadline path)
fn report_x(name: &[u8], ok: bool, elapsed: i64, n: i32, wrote: i64) -> bool {
    unsafe {
        let mut line = [0u8; 128];
        let mut p = 0usize;
        macro_rules! put { ($s:expr) => { for &b in $s { line[p] = b; p += 1; } } }
        macro_rules! num { ($v:expr) => {{
            let mut e: i64 = $v; let neg = e < 0; if neg { e = -e; }
            if neg { line[p] = b'-'; p += 1; }
            let mut d = [0u8; 12]; let mut k = 0;
            if e == 0 { d[k] = b'0'; k += 1; }
            while e > 0 { d[k] = b'0' + (e % 10) as u8; e /= 10; k += 1; }
            while k > 0 { k -= 1; line[p] = d[k]; p += 1; }
        }} }
        put!(name);
        put!(if ok { b": PASS " } else { b": FAIL " });
        put!(b"el="); num!(elapsed); put!(b" n="); num!(n as i64);
        put!(b" wrote="); num!(wrote);
        put!(b"\n\0");
        puts(line.as_ptr());
    }
    ok
}

// Shared stimulus fds passed to writer threads via a small static (single test
// runs at a time; the harness is sequential).
static mut STIM_FD: c_int = -1;

// When true, writer threads BUSY-WAIT (clock_gettime spin) instead of usleep,
// so the stimulus does NOT depend on the nanosleep/deadline-tick path. Toggled
// by wake_main to discriminate "writer's sleep is stranded" from "reader's edge
// wake is lost".
static mut BUSY_WRITER: bool = false;
// Wall-clock ms at which the writer actually performed the stimulus write.
static mut WROTE_AT: i64 = -1;

unsafe fn writer_delay() {
    if BUSY_WRITER {
        let t0 = now_ms();
        while now_ms() - t0 < STIMULUS_MS { core::hint::spin_loop(); }
    } else {
        usleep((STIMULUS_MS as c_uint) * 1000);
    }
}

extern "C" fn writer_eventfd(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        writer_delay();
        let one: u64 = 1;
        write(STIM_FD, &one as *const u64 as *const u8, 8);
        WROTE_AT = now_ms();
    }
    core::ptr::null_mut()
}

extern "C" fn writer_bytes(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        writer_delay();
        let b = b"x";
        write(STIM_FD, b.as_ptr(), 1);
        WROTE_AT = now_ms();
    }
    core::ptr::null_mut()
}

// ── control: probe path (edge already present before the wait) — proves the
//    harness/measurement works and matches epolltest's passing behavior ──────

unsafe fn test_probe_eventfd() -> bool {
    let name = b"probe_eventfd_ready";
    let efd = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    if efd < 0 { return report(name, false, 0); }
    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: efd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, efd, &mut ev) != 0 { return report(name, false, 0); }
    let one: u64 = 1;
    write(efd, &one as *const u64 as *const u8, 8);
    let t0 = now_ms();
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, WINDOW_MS);
    let el = now_ms() - t0;
    close(efd); close(ep);
    report_x(name, n >= 1 && el < PROMPT_MS, el, n, WROTE_AT)
}

// ── 1. cross-thread eventfd wake (level) — the busd waker-eventfd shape ─────

unsafe fn test_xthread_eventfd_level() -> bool {
    let name = b"xthread_eventfd_level";
    let efd = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    if efd < 0 { return report(name, false, 0); }
    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: efd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, efd, &mut ev) != 0 { return report(name, false, 0); }

    STIM_FD = efd;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), writer_eventfd, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    let t0 = now_ms();
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, WINDOW_MS);
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    close(efd); close(ep);
    report_x(name, n >= 1 && el < PROMPT_MS, el, n, WROTE_AT)
}

// ── 2. cross-thread eventfd wake (EPOLLET) ─────────────────────────────────

unsafe fn test_xthread_eventfd_et() -> bool {
    let name = b"xthread_eventfd_et";
    let efd = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    if efd < 0 { return report(name, false, 0); }
    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN | EPOLLET, data: epoll_data { fd: efd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, efd, &mut ev) != 0 { return report(name, false, 0); }

    STIM_FD = efd;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), writer_eventfd, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    let t0 = now_ms();
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, WINDOW_MS);
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    close(efd); close(ep);
    report_x(name, n >= 1 && el < PROMPT_MS, el, n, WROTE_AT)
}

// ── 2b. cross-thread FUTEX_WAKE of a TIMED futex waiter ────────────────────
//    The exact shape that stranded busd (M7b): a thread parks in
//    futex(FUTEX_WAIT) WITH a timeout; another thread issues FUTEX_WAKE WITHOUT
//    changing the futex word. On Linux FUTEX_WAKE wakes timed and untimed
//    waiters identically, so the waiter returns promptly. A kernel whose timed
//    futex_wait yield-loop-polls the word without registering loses the wake
//    and the waiter sleeps its full timeout (and burns a CPU meanwhile). We use
//    the same finite-window/elapsed method as the epoll wake tests.

static mut FUTEX_WORD: u32 = 0;

extern "C" fn writer_futex_wake(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        writer_delay();
        // Pure wake: do NOT change FUTEX_WORD, so only the FUTEX_WAKE (not a
        // value-change the waiter could poll) can release the waiter.
        syscall(nr::FUTEX, core::ptr::addr_of!(FUTEX_WORD) as c_long, 1i64, 1i64, 0i64, 0i64, 0i64);
        WROTE_AT = now_ms();
    }
    core::ptr::null_mut()
}

unsafe fn test_xthread_futex_timed_wake() -> bool {
    let name = b"xthread_futex_timed_wake";
    FUTEX_WORD = 0;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), writer_futex_wake, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    // WINDOW_MS timeout, absolute-free relative timespec (as this kernel treats it).
    let ts = timespec { tv_sec: (WINDOW_MS as i64) / 1000, tv_nsec: 0 };
    let t0 = now_ms();
    let r = syscall(
        nr::FUTEX,
        core::ptr::addr_of!(FUTEX_WORD) as c_long,
        0i64,                       // FUTEX_WAIT
        0i64,                       // expected value (matches FUTEX_WORD)
        &ts as *const timespec as c_long,
        0i64, 0i64,
    );
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    // Woken by the cross-thread FUTEX_WAKE ⇒ returns 0 well before the window.
    // A lost wake sleeps to ~WINDOW_MS (returns -ETIMEDOUT = -110).
    report_x(name, el < PROMPT_MS, el, r as i32, WROTE_AT)
}

// ── 2c. FUTEX_WAIT_BITSET takes an ABSOLUTE deadline ───────────────────────
//    Rust std's `futex_wait` (under every Condvar::wait_timeout /
//    thread::park_timeout / mpsc::recv_timeout) and relibc's Pal::futex_wait
//    both issue FUTEX_WAIT_BITSET with `now + timeout` on CLOCK_MONOTONIC, as
//    Linux defines it. A kernel that reads that timespec as a RELATIVE interval
//    sleeps for "uptime + timeout": invisible in the first seconds after boot,
//    a multi-minute stall once the machine has been up a few minutes — which is
//    exactly how the session compositor started by greetd after a greeter
//    login sat in zbus's D-Bus handshake with its reply unread. Nobody wakes
//    the word here, so the only way out is the deadline: it must fire at the
//    deadline, not at deadline-plus-uptime.

unsafe fn test_futex_bitset_abs_deadline() -> bool {
    let name = b"futex_bitset_abs_deadline";
    FUTEX_WORD = 0;
    let mut now = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut now);
    // Absolute deadline 200 ms from now.
    let mut dl = timespec { tv_sec: now.tv_sec, tv_nsec: now.tv_nsec + 200_000_000 };
    if dl.tv_nsec >= 1_000_000_000 { dl.tv_sec += 1; dl.tv_nsec -= 1_000_000_000; }
    let t0 = now_ms();
    let r = syscall(
        nr::FUTEX,
        core::ptr::addr_of!(FUTEX_WORD) as c_long,
        9i64 | 128,                 // FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG
        0i64,                       // expected value (matches FUTEX_WORD)
        &dl as *const timespec as c_long,
        0i64,
        0xffff_ffffi64,             // FUTEX_BITSET_MATCH_ANY
    );
    let el = now_ms() - t0;
    // -ETIMEDOUT (-110) at ~200 ms. Relative misreading sleeps ~uptime (tens of
    // seconds by the time this test runs), so the bound is generous and still
    // discriminating.
    let ok = r == -110 && el >= 150 && el < 2000;
    if !report_x(name, ok, el, r as i32, -1) { return false; }

    // A deadline already in the past times out at once (Linux: ETIMEDOUT, the
    // value check still wins with EAGAIN when the word has moved).
    let name2 = b"futex_bitset_past_deadline";
    let past = timespec { tv_sec: 0, tv_nsec: 1 };
    let t0 = now_ms();
    let r = syscall(
        nr::FUTEX,
        core::ptr::addr_of!(FUTEX_WORD) as c_long,
        9i64 | 128,
        0i64,
        &past as *const timespec as c_long,
        0i64,
        0xffff_ffffi64,
    );
    let el = now_ms() - t0;
    report_x(name2, r == -110 && el < 500, el, r as i32, -1)
}

// ── 3. cross-thread pipe wake (level) ──────────────────────────────────────

unsafe fn test_xthread_pipe_level() -> bool {
    let name = b"xthread_pipe_level";
    let mut fds = [0i32; 2];
    // pipe2 via raw is avoided; use socketpair-free pipe through relibc? relibc
    // pipe2 is a C fn but not imported — use the raw pipe2 syscall number is
    // arch-specific; instead reuse eventfd-style path with a UNIX socketpair in
    // test 4. For the pipe, use the standard pipe2 relibc symbol.
    extern "C" { fn pipe2(fildes: *mut c_int, flags: c_int) -> c_int; }
    if pipe2(fds.as_mut_ptr(), 0) != 0 { return report(name, false, 0); }
    let (rfd, wfd) = (fds[0], fds[1]);
    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: rfd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev) != 0 { return report(name, false, 0); }

    STIM_FD = wfd;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), writer_bytes, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    let t0 = now_ms();
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, WINDOW_MS);
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    close(rfd); close(wfd); close(ep);
    report_x(name, n >= 1 && el < PROMPT_MS, el, n, WROTE_AT)
}

// ── 4. cross-thread AF_UNIX socketpair wake — the exact busd client shape ───

unsafe fn test_xthread_unix_level() -> bool {
    let name = b"xthread_unix_level";
    const AF_UNIX: c_long = 1;
    const SOCK_STREAM: c_long = 1;
    let mut sv = [0i32; 2];
    let r = syscall(nr::SOCKETPAIR, AF_UNIX, SOCK_STREAM, 0i64, sv.as_mut_ptr() as c_long);
    if r != 0 { return report(name, false, 0); }
    let (a, b) = (sv[0], sv[1]);
    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: a } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, a, &mut ev) != 0 { return report(name, false, 0); }

    STIM_FD = b;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), writer_bytes, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    let t0 = now_ms();
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, WINDOW_MS);
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    close(a); close(b); close(ep);
    report_x(name, n >= 1 && el < PROMPT_MS, el, n, WROTE_AT)
}

// ── 5. timerfd deadline wakes a parked epoll_wait (the deadline-tick path) ──

unsafe fn test_timerfd_deadline() -> bool {
    let name = b"timerfd_deadline";
    let tfd = syscall(nr::TIMERFD_CREATE, CLOCK_MONOTONIC as c_long, 0i64) as c_int;
    if tfd < 0 { return report(name, false, 0); }
    // one-shot at STIMULUS_MS
    let its = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 0 },
        it_value: timespec { tv_sec: 1, tv_nsec: 0 },
    };
    if syscall(nr::TIMERFD_SETTIME, tfd as c_long, 0i64,
        &its as *const itimerspec as c_long, 0i64) != 0 {
        return report(name, false, 0);
    }
    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: tfd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &mut ev) != 0 { return report(name, false, 0); }
    let t0 = now_ms();
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, WINDOW_MS);
    let el = now_ms() - t0;
    close(tfd); close(ep);
    // Fired near STIMULUS_MS (not immediately, not at the window timeout).
    report(name, n >= 1 && el < PROMPT_MS && el > 500, el)
}

// ── 6. periodic timerfd cadence — tokio::time::interval analog (must not
//    busy-spin nor stall: 4 ticks at 250ms => ~1000ms total) ────────────────

unsafe fn test_timerfd_periodic() -> bool {
    let name = b"timerfd_periodic";
    let tfd = syscall(nr::TIMERFD_CREATE, CLOCK_MONOTONIC as c_long, 0i64) as c_int;
    if tfd < 0 { return report(name, false, 0); }
    let its = itimerspec {
        it_interval: timespec { tv_sec: 0, tv_nsec: 250_000_000 },
        it_value: timespec { tv_sec: 0, tv_nsec: 250_000_000 },
    };
    if syscall(nr::TIMERFD_SETTIME, tfd as c_long, 0i64,
        &its as *const itimerspec as c_long, 0i64) != 0 {
        return report(name, false, 0);
    }
    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: tfd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &mut ev) != 0 { return report(name, false, 0); }
    let t0 = now_ms();
    let mut fired = 0;
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    while fired < 4 {
        let n = epoll_wait(ep, out.as_mut_ptr(), 4, WINDOW_MS);
        if n < 1 { break; }
        let mut ticks: u64 = 0;
        read(tfd, &mut ticks as *mut u64 as *mut u8, 8);
        fired += 1;
    }
    let el = now_ms() - t0;
    close(tfd); close(ep);
    // 4 periods of 250ms = ~1000ms. Busy-spin would blow way past the window
    // per-iteration only if it never fires; a stall would hit the window.
    // Accept a generous cadence band [700ms, 3000ms].
    report(name, fired == 4 && el > 700 && el < 3000, el)
}

// ── accept4(SOCK_NONBLOCK) flag plumbing — the W1 wedge regression ──────────
// The kernel aliased ACCEPT|ACCEPT4 and forwarded no flags, so every accepted
// socket was BLOCKING regardless of SOCK_NONBLOCK. tokio/zbus (busd) accept4s
// with SOCK_NONBLOCK and then recv()s on the still-empty ring expecting EAGAIN;
// a blocking accepted fd makes that recv hang forever — the W1 freeze. These
// two tests assert (A) accept4(SOCK_NONBLOCK)'s recv returns EAGAIN PROMPTLY,
// and (B) plain accept (no flags) stays BLOCKING (recv waits for the peer's
// byte, does not wrongly EAGAIN) — i.e. the flag is honored when asked and the
// default is unchanged. Abstract AF_UNIX addresses keep it self-contained.

const AF_UNIX_L:   c_long = 1;
const SOCK_STREAM_L: c_long = 1;
const SOCK_NONBLOCK_F: c_long = 0x800;
const EAGAIN: isize = -11;

// Build an abstract sockaddr_un (leading NUL) into `buf`; returns addrlen.
unsafe fn make_abstract(buf: &mut [u8; 32], tag: &[u8]) -> c_long {
    buf[0] = AF_UNIX_L as u8; buf[1] = 0; // sa_family = AF_UNIX (LE)
    buf[2] = 0;                            // abstract: sun_path[0] = NUL
    for (i, &b) in tag.iter().enumerate() { buf[3 + i] = b; }
    (3 + tag.len()) as c_long
}

unsafe fn bind_listen_abstract(tag: &[u8]) -> c_int {
    let sfd = syscall(nr::SOCKET, AF_UNIX_L, SOCK_STREAM_L, 0i64) as c_int;
    if sfd < 0 { return -1; }
    let mut sa = [0u8; 32];
    let alen = make_abstract(&mut sa, tag);
    if syscall(nr::BIND, sfd as c_long, sa.as_ptr() as c_long, alen) != 0 { close(sfd); return -1; }
    if syscall(nr::LISTEN, sfd as c_long, 4i64) != 0 { close(sfd); return -1; }
    sfd
}

unsafe fn connect_abstract(tag: &[u8]) -> c_int {
    let cfd = syscall(nr::SOCKET, AF_UNIX_L, SOCK_STREAM_L, 0i64) as c_int;
    if cfd < 0 { return -1; }
    let mut sa = [0u8; 32];
    let alen = make_abstract(&mut sa, tag);
    if syscall(nr::CONNECT, cfd as c_long, sa.as_ptr() as c_long, alen) != 0 { close(cfd); return -1; }
    cfd
}

// accept() on LeandrOS is non-blocking at the syscall layer (returns EAGAIN
// when no connect is pending); the listener's "blocking" semantics are a
// userspace retry. Retry accept for up to ~1.5s so the connector thread has
// landed its connect, mirroring how relibc/tokio drive accept.
unsafe fn accept_retry(sfd: c_int, flags: c_long) -> c_int {
    let nr_accept = if flags == 0 { nr::ACCEPT } else { nr::ACCEPT4 };
    let mut tries = 0;
    while tries < 300 {
        let r = if flags == 0 {
            syscall(nr_accept, sfd as c_long, 0i64, 0i64)
        } else {
            syscall(nr_accept, sfd as c_long, 0i64, 0i64, flags)
        };
        if r >= 0 { return r as c_int; }
        usleep(5000);
        tries += 1;
    }
    -1
}

// Connector for test A: connect, keep the connection open (ring EMPTY — sends
// nothing) for a bounded window so main can accept+recv, then close. Bounded
// so pthread_join can never hang.
extern "C" fn connector_a(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let cfd = connect_abstract(b"wpt-accept4-a");
        if cfd >= 0 { usleep(2_000_000); close(cfd); }
    }
    core::ptr::null_mut()
}

// Connector for test B: connect, wait, THEN send one byte — so a correctly
// BLOCKING accepted socket unblocks with data (~STIMULUS), while a wrongly
// nonblocking one returns EAGAIN immediately.
extern "C" fn connector_b(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let cfd = connect_abstract(b"wpt-accept4-b");
        if cfd >= 0 {
            usleep((STIMULUS_MS as c_uint) * 1000);
            let byte = [0x5au8; 1];
            write(cfd, byte.as_ptr(), 1);
            close(cfd);
        }
    }
    core::ptr::null_mut()
}

// A: accept4(SOCK_NONBLOCK) then recv on the empty ring MUST return EAGAIN
// promptly (bounded), not hang.
unsafe fn test_accept4_nonblock_eagain() -> bool {
    let name = b"accept4_nonblock_eagain";
    let sfd = bind_listen_abstract(b"wpt-accept4-a");
    if sfd < 0 { return report(name, false, 0); }
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), connector_a, core::ptr::null_mut()) != 0 {
        close(sfd); return report(name, false, 0);
    }
    // accept4 (retried) returns the accepted fd carrying SOCK_NONBLOCK.
    let afd = accept_retry(sfd, SOCK_NONBLOCK_F);
    if afd < 0 { close(sfd); pthread_join(th, core::ptr::null_mut()); return report(name, false, 0); }
    // Empty ring + nonblock ⇒ EAGAIN, measured; a blocking fd would sleep to
    // the WINDOW (or forever) — the exact W1 hang.
    let t0 = now_ms();
    let mut buf = [0u8; 8];
    let r = syscall(nr::RECVFROM, afd as c_long, buf.as_mut_ptr() as c_long, 8i64, 0i64, 0i64, 0i64) as isize;
    let el = now_ms() - t0;
    close(afd); close(sfd);
    pthread_join(th, core::ptr::null_mut());
    report_x(name, r == EAGAIN && el < PROMPT_MS, el, r as i32, r as i64)
}

// B: plain accept (no flags) MUST stay blocking — recv waits for the peer's
// delayed byte rather than wrongly returning EAGAIN.
unsafe fn test_accept_noflags_blocking() -> bool {
    let name = b"accept_noflags_blocking";
    let sfd = bind_listen_abstract(b"wpt-accept4-b");
    if sfd < 0 { return report(name, false, 0); }
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), connector_b, core::ptr::null_mut()) != 0 {
        close(sfd); return report(name, false, 0);
    }
    let afd = accept_retry(sfd, 0);
    if afd < 0 { close(sfd); pthread_join(th, core::ptr::null_mut()); return report(name, false, 0); }
    // Blocking recv: should sleep until the connector sends at ~STIMULUS_MS,
    // then return 1. A leaked-nonblock fd returns EAGAIN at ~0ms → fail.
    let t0 = now_ms();
    let mut buf = [0u8; 8];
    let r = syscall(nr::RECVFROM, afd as c_long, buf.as_mut_ptr() as c_long, 8i64, 0i64, 0i64, 0i64) as isize;
    let el = now_ms() - t0;
    close(afd); close(sfd);
    pthread_join(th, core::ptr::null_mut());
    // Received exactly the byte, AFTER a real block (not an instant EAGAIN),
    // within a bounded window.
    report_x(name, r == 1 && el > 300 && el < WINDOW_MS as i64, el, r as i32, r as i64)
}

// ── 7. SAME-THREAD self-wake RE-ARM (the busd/mio reactor shape) ────────────
// Every test above is CROSS-thread with a FRESH eventfd written ONCE. busd's
// tokio runtime is single-threaded: its mio reactor writes its OWN waker
// eventfd (registered EPOLLET) then the SAME thread parks in epoll_wait, and
// that waker eventfd is RE-USED (written, reported, drained, written again...).
// The untested gap is the EPOLLET re-arm edge after a drain, same thread,
// issued BEFORE the park. On Linux each write after a drain is a fresh 0->1
// edge that fires; if LeandrOS loses the 2nd..Nth edge, a single-threaded
// reactor that self-notifies then parks is never woken — the W1 freeze shape.
//
// Method: do N rounds of {write efd (same thread); epoll_wait(WINDOW); drain}.
// Round 1 is the initial ADD edge (known-good, M7a). Rounds 2..N are the RE-ARM
// edges. Every round must return promptly; a lost re-arm edge sleeps to WINDOW.
// No second thread and no stimulus delay: the edge is present BEFORE the wait,
// so a prompt return is ~0ms and a lost edge is ~WINDOW — unambiguous.

const REARM_ROUNDS: usize = 8;

unsafe fn test_self_eventfd_rearm(name: &[u8], et: bool) -> bool {
    let efd = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    if efd < 0 { return report(name, false, 0); }
    let ep = epoll_create1(0);
    let flags = if et { EPOLLIN | EPOLLET } else { EPOLLIN };
    let mut ev = epoll_event { events: flags, data: epoll_data { fd: efd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, efd, &mut ev) != 0 { return report(name, false, 0); }

    let mut worst: i64 = 0;
    let mut lost = 0usize;
    let mut first_lost_round: i64 = -1;
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    for r in 0..REARM_ROUNDS {
        // Same-thread write BEFORE the park (the reactor self-notify).
        let one: u64 = 1;
        write(efd, &one as *const u64 as *const u8, 8);
        let t0 = now_ms();
        let n = epoll_wait(ep, out.as_mut_ptr(), 4, WINDOW_MS);
        let el = now_ms() - t0;
        if el > worst { worst = el; }
        if !(n >= 1 && el < PROMPT_MS) {
            lost += 1;
            if first_lost_round < 0 { first_lost_round = r as i64; }
        }
        // Drain to 0 so the next write is a genuine 0->1 re-arm edge.
        let mut v: u64 = 0;
        read(efd, &mut v as *mut u64 as *mut u8, 8);
    }
    close(efd); close(ep);
    // Report worst elapsed; annotate first lost round in the n= field.
    report_x(name, lost == 0, worst, first_lost_round as i32, lost as i64)
}

// ── stress discriminator: N cross-thread eventfd wakes, sleep vs busy writer ─
// Small window so a stranded wake is cheap. Returns fails (woke only at window).
const S_WINDOW_MS: c_int = 1500;
const S_STIM_MS: i64 = 250;
const S_PROMPT_MS: i64 = 900;

unsafe fn stress_eventfd_once() -> i64 {
    let efd = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: efd } };
    epoll_ctl(ep, EPOLL_CTL_ADD, efd, &mut ev);
    STIM_FD = efd;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), stress_writer, core::ptr::null_mut());
    let t0 = now_ms();
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, S_WINDOW_MS);
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    close(efd); close(ep);
    if n >= 1 && el < S_PROMPT_MS { -1 } else { el } // -1 = pass; else elapsed at fail
}

extern "C" fn stress_writer(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        if BUSY_WRITER {
            let t0 = now_ms();
            while now_ms() - t0 < S_STIM_MS { core::hint::spin_loop(); }
        } else {
            usleep((S_STIM_MS as c_uint) * 1000);
        }
        let one: u64 = 1;
        write(STIM_FD, &one as *const u64 as *const u8, 8);
        WROTE_AT = now_ms();
    }
    core::ptr::null_mut()
}

unsafe fn run_stress() -> i32 {
    const ITERS: usize = 15;
    let mut total_fails = 0i32;
    // BUSY first (fast, pure edge path), then SLEEP (deadline path — the M7
    // NEXT_POLL_DEADLINE clobber stranded these; both must be clean).
    for &(busy, label) in &[(true, b"BUSY \0".as_ptr()), (false, b"SLEEP\0".as_ptr())] {
        BUSY_WRITER = busy;
        let mut fails = 0i32;
        let mut worst = 0i64;
        let mut pat = [0u8; ITERS + 1];
        for i in 0..ITERS {
            let r = stress_eventfd_once();
            if r >= 0 { fails += 1; if r > worst { worst = r; } pat[i] = b'X'; }
            else { pat[i] = b'.'; }
        }
        total_fails += fails;
        BUSY_WRITER = false;
        // "STRESS <label> [......X.....] fails=<f>/<ITERS> worst=<worst>ms"
        let mut line = [0u8; 96]; let mut p = 0usize;
        macro_rules! put { ($s:expr) => { for &b in $s { line[p]=b; p+=1; } } }
        macro_rules! num { ($v:expr) => {{ let mut e:i64=$v; let mut d=[0u8;12]; let mut k=0;
            if e==0 { d[k]=b'0'; k+=1; } while e>0 { d[k]=b'0'+(e%10)as u8; e/=10; k+=1; }
            while k>0 { k-=1; line[p]=d[k]; p+=1; } }} }
        put!(b"STRESS "); { let mut i=0; while label.add(i).read()!=0 { line[p]=label.add(i).read(); p+=1; i+=1; } }
        put!(b" ["); for i in 0..ITERS { line[p]=pat[i]; p+=1; } put!(b"] fails=");
        num!(fails as i64); put!(b"/"); num!(ITERS as i64);
        put!(b" worst="); num!(worst); put!(b"ms\n\0");
        puts(line.as_ptr());
    }
    total_fails
}

// ── pipe blocking-writer coverage (idlecpu lane, 2026-09-16) ───────────────
//
// A full pipe's writer used to yield-spin inside the kernel (sys_write's
// EAGAIN retry) for as long as the reader was slow — one host core pinned per
// pipeline. It now parks on the pipe's wake tag like a poll(POLLOUT) waiter.
// These cases pin down the contract: the writer's CPU time is a small fraction
// of its wall time; POLLOUT on a full pipe stays clear until a drain frees
// PIPE_BUF; writes <= PIPE_BUF are atomic; and a close(2) of either end wakes
// the parked peer (EOF / EPIPE) promptly — a live process closing its end,
// not an exit, which the SIGCHLD broadcast used to paper over.

const O_NONBLOCK: c_int = 0o4000;
const POLLOUT: i16 = 0x004;
const CLOCK_THREAD_CPUTIME_ID: clockid_t = 3;
const F_GETPIPE_SZ: c_int = 1032;
const SIGPIPE: c_int = 13;
const SIG_IGN: usize = 1;
const PIPE_BUF: usize = 4096;

#[repr(C)]
struct pollfd { fd: c_int, events: i16, revents: i16 }

extern "C" {
    fn pipe2(fildes: *mut c_int, flags: c_int) -> c_int;
    fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int;
    fn signal(sig: c_int, handler: usize) -> usize;
}

unsafe fn thread_cpu_ms() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

const XFER_BYTES: usize = 16 * 1024 * 1024;
static mut XFER_BUF: [u8; 65536] = [0x5a; 65536];
static mut READ_TOTAL: usize = 0;

/// Slow reader: 4 KiB bites, a 10 ms nap every 128 KiB, so the 16 KiB ring
/// is full for most of the transfer and the writer must wait ~128 times.
extern "C" fn slow_reader(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let mut buf = [0u8; 4096];
        let mut total = 0usize;
        let mut reads = 0usize;
        loop {
            let n = read(STIM_FD, buf.as_mut_ptr(), buf.len());
            if n <= 0 { break; }
            total += n as usize;
            reads += 1;
            if reads % 32 == 0 { usleep(10_000); }
        }
        READ_TOTAL = total;
    }
    core::ptr::null_mut()
}

/// Fast writer of XFER_BYTES into a pipe drained by `slow_reader`: PASS when
/// everything arrived and the writer's thread CPU time is under a quarter of
/// its wall time (a parked writer spends ~0; the old spin spent ~all of it).
unsafe fn test_pipe_writer_blocks() -> bool {
    let name = b"pipe_writer_blocks";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), 0) != 0 { return report(name, false, 0); }
    let (rfd, wfd) = (fds[0], fds[1]);
    STIM_FD = rfd;
    READ_TOTAL = 0;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), slow_reader, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    let t0 = now_ms();
    let c0 = thread_cpu_ms();
    let mut sent = 0usize;
    while sent < XFER_BYTES {
        let want = (XFER_BYTES - sent).min(XFER_BUF.len());
        let n = write(wfd, XFER_BUF.as_ptr(), want);
        if n <= 0 { break; }
        sent += n as usize;
    }
    let wall = now_ms() - t0;
    let cpu = thread_cpu_ms() - c0;
    close(wfd); // EOF for the reader
    pthread_join(th, core::ptr::null_mut());
    close(rfd);
    let ok = sent == XFER_BYTES && READ_TOTAL == XFER_BYTES && wall > 0 && cpu * 4 < wall;
    // report_x's n/wrote columns carry cpu ms and bytes received (KiB).
    report_x(name, ok, wall, cpu as i32, (READ_TOTAL / 1024) as i64)
}

/// Fill a non-blocking pipe to EAGAIN; returns the byte count that fit.
unsafe fn fill_pipe(wfd: c_int) -> usize {
    let mut filled = 0usize;
    loop {
        let n = write(wfd, XFER_BUF.as_ptr(), PIPE_BUF);
        if n <= 0 { break; }
        filled += n as usize;
        if filled > 1 << 24 { break; } // never full: give up (the test fails below)
    }
    filled
}

extern "C" fn drain_one_chunk(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        writer_delay();
        let mut buf = [0u8; PIPE_BUF];
        read(STIM_FD, buf.as_mut_ptr(), buf.len());
        WROTE_AT = now_ms();
    }
    core::ptr::null_mut()
}

/// poll(POLLOUT) on a full pipe: not writable now (0 with a zero timeout),
/// then woken promptly when the reader frees PIPE_BUF bytes.
unsafe fn test_pipe_pollout_full() -> bool {
    let name = b"pipe_pollout_full";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false, 0); }
    let (rfd, wfd) = (fds[0], fds[1]);
    let filled = fill_pipe(wfd);
    let mut pfd = pollfd { fd: wfd, events: POLLOUT, revents: 0 };
    let now0 = poll(&mut pfd, 1, 0);
    STIM_FD = rfd;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), drain_one_chunk, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    let t0 = now_ms();
    pfd.revents = 0;
    let n = poll(&mut pfd, 1, WINDOW_MS);
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    close(rfd); close(wfd);
    let ok = filled > 0 && now0 == 0 && n == 1 && pfd.revents & POLLOUT != 0 && el < PROMPT_MS;
    report_x(name, ok, el, n, WROTE_AT)
}

/// PIPE_BUF atomicity on a non-blocking pipe with 100 bytes free: a 4096-byte
/// write is refused whole (EAGAIN), an 8192-byte one is written partially
/// (100), and then 50 bytes find no room.
unsafe fn test_pipe_buf_atomic() -> bool {
    let name = b"pipe_buf_atomic";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false, 0); }
    let (rfd, wfd) = (fds[0], fds[1]);
    let filled = fill_pipe(wfd);
    let mut buf = [0u8; 100];
    let got = read(rfd, buf.as_mut_ptr(), buf.len());
    let w1 = write(wfd, XFER_BUF.as_ptr(), 4096);
    let w2 = write(wfd, XFER_BUF.as_ptr(), 8192);
    let w3 = write(wfd, XFER_BUF.as_ptr(), 50);
    close(rfd); close(wfd);
    let ok = filled > 0 && got == 100 && w1 < 0 && w2 == 100 && w3 < 0;
    report_x(name, ok, filled as i64, w1 as i32, w2 as i64)
}

extern "C" fn close_stim_fd(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        writer_delay();
        close(STIM_FD);
        WROTE_AT = now_ms();
    }
    core::ptr::null_mut()
}

/// A blocking read() parked on an empty pipe returns 0 promptly when the
/// (only) write end is closed by another thread.
unsafe fn test_pipe_close_wakes_reader() -> bool {
    let name = b"pipe_close_wakes_reader";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), 0) != 0 { return report(name, false, 0); }
    let (rfd, wfd) = (fds[0], fds[1]);
    STIM_FD = wfd;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), close_stim_fd, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    let t0 = now_ms();
    let mut b = [0u8; 16];
    let n = read(rfd, b.as_mut_ptr(), b.len());
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    close(rfd);
    report_x(name, n == 0 && el < PROMPT_MS, el, n as i32, WROTE_AT)
}

/// A blocking write() parked on a full pipe fails with EPIPE promptly when the
/// read end is closed by another thread (SIGPIPE ignored for the check).
unsafe fn test_pipe_close_wakes_writer() -> bool {
    let name = b"pipe_close_wakes_writer";
    signal(SIGPIPE, SIG_IGN);
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), 0) != 0 { return report(name, false, 0); }
    let (rfd, wfd) = (fds[0], fds[1]);
    // Fill exactly to capacity with blocking writes (each fits, none waits).
    let cap = fcntl(wfd, F_GETPIPE_SZ, 0);
    if cap <= 0 { return report(name, false, 0); }
    let mut filled = 0usize;
    while filled < cap as usize {
        let want = (cap as usize - filled).min(PIPE_BUF);
        let n = write(wfd, XFER_BUF.as_ptr(), want);
        if n <= 0 { return report(name, false, filled as i64); }
        filled += n as usize;
    }
    STIM_FD = rfd;
    WROTE_AT = -1;
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), close_stim_fd, core::ptr::null_mut()) != 0 {
        return report(name, false, 0);
    }
    let t0 = now_ms();
    let n = write(wfd, XFER_BUF.as_ptr(), 1); // full: parks until the reader closes
    let el = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    close(wfd);
    report_x(name, n < 0 && el < PROMPT_MS, el, n as i32, WROTE_AT)
}

#[no_mangle]
pub unsafe extern "C" fn wake_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    puts(b"--- wakepolltest start ---\n\0".as_ptr());

    // Core coverage: cross-thread eventfd wake stressed in BOTH writer modes.
    // BUSY = pure edge path; SLEEP = the nanosleep/deadline path that the M7
    // NEXT_POLL_DEADLINE clobber stranded. Both batches must be 0 fails.
    let mut failures = run_stress();
    puts(b"--- stress done ---\n\0".as_ptr());

    // Single-shot fd-type coverage (sleep-writer = deadline path), so pipe /
    // AF_UNIX / timerfd edges arriving while parked are exercised too.
    BUSY_WRITER = false;
    if !test_probe_eventfd() { failures += 1; }
    if !test_xthread_futex_timed_wake() { failures += 1; }
    if !test_futex_bitset_abs_deadline() { failures += 1; }
    if !test_xthread_pipe_level() { failures += 1; }
    if !test_xthread_unix_level() { failures += 1; }
    if !test_timerfd_deadline() { failures += 1; }
    if !test_timerfd_periodic() { failures += 1; }
    // Same-thread self-wake re-arm (busd/mio reactor shape). ET is what mio uses.
    if !test_self_eventfd_rearm(b"self_eventfd_et_rearm", true) { failures += 1; }
    if !test_self_eventfd_rearm(b"self_eventfd_level_rearm", false) { failures += 1; }
    // accept4(SOCK_NONBLOCK) flag plumbing — the W1 wedge regression guard.
    if !test_accept4_nonblock_eagain() { failures += 1; }
    if !test_accept_noflags_blocking() { failures += 1; }
    // Blocking pipe writer/reader park (not spin), PIPE_BUF, close wakes.
    if !test_pipe_writer_blocks() { failures += 1; }
    if !test_pipe_pollout_full() { failures += 1; }
    if !test_pipe_buf_atomic() { failures += 1; }
    if !test_pipe_close_wakes_reader() { failures += 1; }
    if !test_pipe_close_wakes_writer() { failures += 1; }

    // SUMMARY pass=<P> fail=<F>  (P is informational; the exit code is the
    // failure count — 0 means every wake path is clean.)
    let total: i32 = 30 + 8 + 2 + 5;
    let mut line = [0u8; 48];
    let mut p = 0;
    macro_rules! put { ($s:expr) => { for &b in $s { line[p] = b; p += 1; } } }
    macro_rules! num { ($v:expr) => {{
        let mut e: i32 = $v; let mut d = [0u8; 4]; let mut k = 0;
        if e == 0 { d[k] = b'0'; k += 1; }
        while e > 0 { d[k] = b'0' + (e % 10) as u8; e /= 10; k += 1; }
        while k > 0 { k -= 1; line[p] = d[k]; p += 1; }
    }} }
    put!(b"SUMMARY pass="); num!(total - failures);
    put!(b" fail="); num!(failures);
    line[p] = b'\n'; p += 1; line[p] = 0;
    puts(line.as_ptr());
    puts(b"--- wakepolltest done ---\n\0".as_ptr());
    failures
}
