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
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const EVENTFD2: i64 = 19;
    pub const TIMERFD_CREATE: i64 = 85;
    pub const TIMERFD_SETTIME: i64 = 86;
    pub const SOCKETPAIR: i64 = 199;
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
    if !test_xthread_pipe_level() { failures += 1; }
    if !test_xthread_unix_level() { failures += 1; }
    if !test_timerfd_deadline() { failures += 1; }
    if !test_timerfd_periodic() { failures += 1; }

    // SUMMARY pass=<P> fail=<F>  (P is informational; the exit code is the
    // failure count — 0 means every wake path is clean.)
    let total: i32 = 30 + 5;
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
