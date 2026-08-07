//! epolltest — regression coverage for epoll trigger semantics (level vs.
//! edge, timeout accuracy, EPOLLONESHOT), plus three adjacent fd-flavor
//! checks that ride the same epoll machinery: signalfd, inotify, and
//! /proc/self/exe. Companion to polltest (basic readiness correctness)
//! and idletest (epoll_wait really blocks) — this one is about the finer
//! trigger-mode contract.
//!
//! signalfd4/inotify_init1/inotify_add_watch have no C wrapper in relibc
//! (only the raw syscall numbers exist there — see
//! userland/relibc/src/header/sys_syscall/{x86_64,aarch64}.rs), so those
//! go through the raw `syscall()` entry point, same as eventfd2 below;
//! everything else (epoll_create1/epoll_ctl/epoll_wait, pipe2,
//! sigprocmask, raise, clock_gettime, readlink) is a real relibc C
//! function, exercised the same way polltest/sigtest do.
//!
//! Initializes via relibc_start_v1 (same as polltest/timertest/sigtest)
//! so TLS is set up — errno, the sigaction SA_RESTORER trampoline, and
//! the real epoll_*() Pal calls all need it.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `epoll_main` returns the number of failures as the exit
//! code, and a final `SUMMARY pass=<P> fail=<F>` line is printed last.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_long = i64;
type c_uint = u32;
type c_ulonglong = u64;
type ssize_t = isize;
type size_t = usize;
type time_t = i64;
type clockid_t = c_int;

pub type sigset_t = u64;

const O_NONBLOCK: c_int = 0o4000;

const EPOLLIN: c_uint = 0x001;
const EPOLLET: c_uint = 1 << 31;
const EPOLLONESHOT: c_uint = 0x4000_0000;
const EPOLL_CTL_ADD: c_int = 1;
const EPOLL_CTL_MOD: c_int = 3;

const CLOCK_MONOTONIC: clockid_t = 1;

const SIG_BLOCK: c_int = 0;
const SIGUSR1: c_int = 10;

// Raw syscall numbers for the functions relibc doesn't wrap with a C entry
// point. Values from userland/relibc/src/header/sys_syscall/{x86_64,
// aarch64}.rs (also matches kernel/src/syscall.rs's dispatch table).
#[cfg(target_arch = "x86_64")]
mod nr {
    pub const SIGNALFD4: i64 = 289;
    pub const INOTIFY_INIT1: i64 = 294;
    pub const INOTIFY_ADD_WATCH: i64 = 254;
    pub const EVENTFD2: i64 = 290;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const SIGNALFD4: i64 = 74;
    pub const INOTIFY_INIT1: i64 = 26;
    pub const INOTIFY_ADD_WATCH: i64 = 27;
    pub const EVENTFD2: i64 = 19;
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

    pub fn pipe2(fildes: *mut c_int, flags: c_int) -> c_int;

    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;

    pub fn sigprocmask(how: c_int, set: *const sigset_t, oset: *mut sigset_t) -> c_int;
    pub fn raise(sig: c_int) -> c_int;

    pub fn clock_gettime(clockid: clockid_t, tp: *mut timespec) -> c_int;
    pub fn readlink(path: *const u8, buf: *mut u8, bufsize: size_t) -> ssize_t;
    pub fn getpid() -> c_int;

    // eventfd2/signalfd4/inotify_init1/inotify_add_watch have no relibc C
    // wrapper — go straight through the raw syscall entry point.
    pub fn syscall(sysno: c_long, ...) -> c_long;
}

// ── Assembly entry point (identical to polltest's/sigtest's) ───────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset epoll_main",
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
    "   adrp x1, epoll_main",
    "   add x1, x1, :lo12:epoll_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn epoll_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    if !test_et_fires_once() { failures += 1; }
    if !test_level_refires() { failures += 1; }
    if !test_et_refires_after_edge() { failures += 1; }
    if !test_timeout_accuracy() { failures += 1; }
    if !test_oneshot() { failures += 1; }
    if !test_signalfd_signo() { failures += 1; }
    if !test_inotify_never_fires() { failures += 1; }
    if !test_proc_self_exe() { failures += 1; }
    if !test_proc_pid_exe() { failures += 1; }
    if !test_nested_epoll() { failures += 1; }

    write_summary(10, failures);
    puts(b"--- epolltest done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── 1. EPOLLET: one edge in, exactly one event out, no re-fire on drain-less
//    re-poll ──────────────────────────────────────────────────────────────

unsafe fn test_et_fires_once() -> bool {
    let name = b"et_fires_once\0";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let (rfd, wfd) = (fds[0], fds[1]);

    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }
    let mut ev = epoll_event { events: EPOLLIN | EPOLLET, data: epoll_data { fd: rfd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev) != 0 { return report(name, false); }

    write(wfd, b"x".as_ptr(), 1);

    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n1 = epoll_wait(ep, out.as_mut_ptr(), 4, 100);
    // No drain in between: the edge that fired n1 must not fire again.
    let n2 = epoll_wait(ep, out.as_mut_ptr(), 4, 100);

    close(rfd);
    close(wfd);
    close(ep);

    report(name, n1 == 1 && n2 == 0)
}

// ── 2. Level-triggered (no EPOLLET): re-fires on re-poll while still
//    readable, even without a new write ─────────────────────────────────────

unsafe fn test_level_refires() -> bool {
    let name = b"level_refires\0";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let (rfd, wfd) = (fds[0], fds[1]);

    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: rfd } }; // no EPOLLET
    if epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev) != 0 { return report(name, false); }

    write(wfd, b"x".as_ptr(), 1);

    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n1 = epoll_wait(ep, out.as_mut_ptr(), 4, 100);
    // No drain: data is still sitting there, so a level-triggered fd must
    // report ready again.
    let n2 = epoll_wait(ep, out.as_mut_ptr(), 4, 100);

    close(rfd);
    close(wfd);
    close(ep);

    report(name, n1 >= 1 && n2 >= 1)
}

// ── 3. EPOLLET: a second, independent write is a new edge and must re-fire ──

unsafe fn test_et_refires_after_edge() -> bool {
    let name = b"et_refires_after_edge\0";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let (rfd, wfd) = (fds[0], fds[1]);

    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }
    let mut ev = epoll_event { events: EPOLLIN | EPOLLET, data: epoll_data { fd: rfd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev) != 0 { return report(name, false); }

    write(wfd, b"a".as_ptr(), 1);
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n1 = epoll_wait(ep, out.as_mut_ptr(), 4, 100);

    // A second write (no drain) is a new edge — the seq that backs EPOLLET
    // emulation advances on every write, not just empty-to-nonempty.
    write(wfd, b"b".as_ptr(), 1);
    let n2 = epoll_wait(ep, out.as_mut_ptr(), 4, 100);

    close(rfd);
    close(wfd);
    close(ep);

    report(name, n1 == 1 && n2 == 1)
}

// ── 4. epoll_wait's timeout is honored to within a generous tolerance ──────

unsafe fn test_timeout_accuracy() -> bool {
    let name = b"timeout_accuracy\0";
    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }

    let efd = syscall(nr::EVENTFD2, 0i64, 0i64) as i32;
    if efd < 0 { close(ep); return report(name, false); }
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: efd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, efd, &mut ev) != 0 {
        close(efd); close(ep); return report(name, false);
    }

    let mut start: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut start);

    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, 200);

    let mut end: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut end);

    let elapsed_ms = (end.tv_sec - start.tv_sec) * 1000
        + (end.tv_nsec - start.tv_nsec) / 1_000_000;

    close(efd);
    close(ep);

    // Generous window: tick granularity (~10ms) on the low end, a lot of
    // slack on the high end so scheduling jitter can't cause a spurious
    // FAIL.
    report(name, n == 0 && elapsed_ms >= 150 && elapsed_ms <= 600)
}

// ── 5. EPOLLONESHOT: fires once, disarms, then EPOLL_CTL_MOD re-arms it ────

unsafe fn test_oneshot() -> bool {
    let name = b"oneshot\0";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let (rfd, wfd) = (fds[0], fds[1]);

    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }
    let mut ev = epoll_event { events: EPOLLIN | EPOLLONESHOT, data: epoll_data { fd: rfd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev) != 0 { return report(name, false); }

    write(wfd, b"a".as_ptr(), 1);
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n_fire = epoll_wait(ep, out.as_mut_ptr(), 4, 100);
    let fired = n_fire == 1;

    // Drain the first byte, then write a fresh one: a still-armed interest
    // would see this as new readable data, but EPOLLONESHOT must have
    // disarmed it after the first fire above.
    let mut drain = [0u8; 1];
    read(rfd, drain.as_mut_ptr(), 1);
    write(wfd, b"b".as_ptr(), 1);
    let n_disarmed = epoll_wait(ep, out.as_mut_ptr(), 4, 100);
    let disarmed = n_disarmed == 0;

    // Re-arm via EPOLL_CTL_MOD: the unread "b" byte must now be reported.
    let mut ev2 = epoll_event { events: EPOLLIN | EPOLLONESHOT, data: epoll_data { fd: rfd } };
    epoll_ctl(ep, EPOLL_CTL_MOD, rfd, &mut ev2);
    let n_rearm = epoll_wait(ep, out.as_mut_ptr(), 4, 100);
    let rearmed = n_rearm >= 1;

    close(rfd);
    close(wfd);
    close(ep);

    report(name, fired && disarmed && rearmed)
}

// ── 6. signalfd: blocked SIGUSR1 shows up as a read()able signalfd_siginfo ──

unsafe fn test_signalfd_signo() -> bool {
    let name = b"signalfd_signo\0";

    let mask: sigset_t = 1u64 << (SIGUSR1 - 1);
    if sigprocmask(SIG_BLOCK, &mask, core::ptr::null_mut()) != 0 { return report(name, false); }

    let sfd = syscall(
        nr::SIGNALFD4,
        -1i64,
        &mask as *const sigset_t as *const c_void,
        8i64,
        0i64,
    ) as i32;
    if sfd < 0 { return report(name, false); }

    if raise(SIGUSR1) != 0 { close(sfd); return report(name, false); }

    let mut buf = [0u8; 128];
    let n = read(sfd, buf.as_mut_ptr(), 128);
    // struct signalfd_siginfo's ssi_signo is the first u32 (offset 0).
    let ssi_signo = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);

    close(sfd);
    report(name, n == 128 && ssi_signo == SIGUSR1 as u32)
}

// ── 7. inotify: valid fd/watch, but never fires (no filesystem events here) ─

unsafe fn test_inotify_never_fires() -> bool {
    let name = b"inotify_never_fires\0";

    let fd = syscall(nr::INOTIFY_INIT1, 0i64) as i32;
    let fd_ok = fd >= 0;

    let mut wd_ok = false;
    let mut n = 0i32;
    if fd_ok {
        let path = b"/tmp\0";
        let wd = syscall(
            nr::INOTIFY_ADD_WATCH,
            fd as i64,
            path.as_ptr() as *const c_void,
            0xFFFi64,
        ) as i32;
        wd_ok = wd >= 0;

        let ep = epoll_create1(0);
        if ep >= 0 {
            let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd } };
            epoll_ctl(ep, EPOLL_CTL_ADD, fd, &mut ev);
            let mut out: [epoll_event; 4] = core::mem::zeroed();
            n = epoll_wait(ep, out.as_mut_ptr(), 4, 150);
            close(ep);
        }
        close(fd);
    }

    report(name, fd_ok && wd_ok && n == 0)
}

// ── 8. /proc/self/exe resolves to this binary's own exec path, not /bin/init ─

unsafe fn test_proc_self_exe() -> bool {
    let name = b"proc_self_exe\0";

    let mut buf = [0u8; 256];
    let n = readlink(b"/proc/self/exe\0".as_ptr(), buf.as_mut_ptr(), 255);

    let ok = n > 0 && {
        let got = &buf[0..n as usize];
        ends_with(got, b"epolltest") && got != &b"/bin/init"[..]
    };

    write(1, b"proc_self_exe: got ".as_ptr(), 19);
    if n > 0 { write(1, buf.as_ptr(), n as usize); }
    write(1, b"\n".as_ptr(), 1);

    report(name, ok)
}

// ── 9. /proc/<pid>/exe (numeric, not the "self" alias) resolves the same way ─
//
// The kernel keys the executable-path side table by tgid, and "self" was the
// only spelling that ever exercised the lookup — a caller naming itself by
// its own numeric pid (as e.g. a `/proc/<pid>/exe` symlink reader outside the
// process would) took a completely different, unimplemented code path.
// Building the path from a real getpid() and comparing it against the
// "self" answer proves the numeric form is wired to the same table, not
// just present.

unsafe fn test_proc_pid_exe() -> bool {
    let name = b"proc_pid_exe\0";

    let pid = getpid();
    let mut path = [0u8; 32];
    let mut i = 0usize;
    for &b in b"/proc/" { path[i] = b; i += 1; }
    // Format pid in decimal (pid is always positive).
    let mut digits = [0u8; 10];
    let mut nd = 0usize;
    let mut v = pid as u32;
    if v == 0 { digits[0] = b'0'; nd = 1; }
    while v > 0 { digits[nd] = b'0' + (v % 10) as u8; v /= 10; nd += 1; }
    while nd > 0 { nd -= 1; path[i] = digits[nd]; i += 1; }
    for &b in b"/exe\0" { path[i] = b; i += 1; }

    let mut buf = [0u8; 256];
    let n = readlink(path.as_ptr(), buf.as_mut_ptr(), 255);

    let ok = n > 0 && {
        let got = &buf[0..n as usize];
        ends_with(got, b"epolltest") && got != &b"/bin/init"[..]
    };

    write(1, b"proc_pid_exe: path=".as_ptr(), 20);
    write(1, path.as_ptr(), i - 1); // exclude the trailing NUL
    write(1, b" got ".as_ptr(), 5);
    if n > 0 { write(1, buf.as_ptr(), n as usize); } else { write_i64(1, n as i64); }
    write(1, b"\n".as_ptr(), 1);

    report(name, ok)
}

// ── 10. A nested epoll fd is readable only when its OWN interest list has a
//    ready event ────────────────────────────────────────────────────────────
//
// An epoll fd registered inside another epoll is how every calloop/libevent
// -style event loop composes reactors: wayland-backend's server `poll_fd()`
// IS an epoll fd, and calloop puts it in its own epoll. If the outer
// epoll_wait reports the inner one ready unconditionally, the loop returns
// instantly forever with nothing to dispatch — a pure busy-spin that no
// correctness test notices (the COSMIC panel spun at ~4600 dispatches/s).
//
// Three phases, because only the middle one passes by accident:
//   idle  → outer must BLOCK for the full timeout and report nothing;
//   armed → a write to the inner pipe must surface on the outer wait;
//   drained → readiness must go away again.

unsafe fn test_nested_epoll() -> bool {
    let name = b"nested_epoll\0";
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let (rfd, wfd) = (fds[0], fds[1]);

    let inner = epoll_create1(0);
    let outer = epoll_create1(0);
    if inner < 0 || outer < 0 { return report(name, false); }

    let mut ev_in = epoll_event { events: EPOLLIN, data: epoll_data { fd: rfd } };
    let mut ev_out = epoll_event { events: EPOLLIN, data: epoll_data { fd: inner } };
    if epoll_ctl(inner, EPOLL_CTL_ADD, rfd, &mut ev_in) != 0
        || epoll_ctl(outer, EPOLL_CTL_ADD, inner, &mut ev_out) != 0
    {
        close(rfd); close(wfd); close(inner); close(outer);
        return report(name, false);
    }

    let mut out: [epoll_event; 4] = core::mem::zeroed();

    // Idle: nothing written yet, so the inner set has no ready event and the
    // outer wait must sit out its whole timeout. The elapsed check is the
    // point of the test — an `n == 0` that returned instantly would still be
    // a spin.
    let mut start: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut start);
    let n_idle = epoll_wait(outer, out.as_mut_ptr(), 4, 200);
    let mut end: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut end);
    let idle_ms = (end.tv_sec - start.tv_sec) * 1000
        + (end.tv_nsec - start.tv_nsec) / 1_000_000;
    let idle_ok = n_idle == 0 && idle_ms >= 150;

    // Armed: a byte on the pipe makes the inner set ready, which must make
    // the inner epoll fd itself readable to the outer one.
    write(wfd, b"x".as_ptr(), 1);
    let n_armed = epoll_wait(outer, out.as_mut_ptr(), 4, 200);
    let armed_ok = n_armed == 1
        && out[0].events & EPOLLIN != 0
        && { let d = out[0].data; d.fd == inner };

    // Drained: readiness is recomputed from the inner list, not latched.
    let mut drain = [0u8; 1];
    read(rfd, drain.as_mut_ptr(), 1);
    let n_drained = epoll_wait(outer, out.as_mut_ptr(), 4, 100);
    let drained_ok = n_drained == 0;

    close(rfd); close(wfd); close(inner); close(outer);

    write(1, b"nested_epoll: idle_n=".as_ptr(), 21);
    write_i64(1, n_idle as i64);
    write(1, b" idle_ms=".as_ptr(), 9);
    write_i64(1, idle_ms);
    write(1, b" armed_n=".as_ptr(), 9);
    write_i64(1, n_armed as i64);
    write(1, b" drained_n=".as_ptr(), 11);
    write_i64(1, n_drained as i64);
    write(1, b"\n".as_ptr(), 1);

    report(name, idle_ok && armed_ok && drained_ok)
}

fn ends_with(haystack: &[u8], suffix: &[u8]) -> bool {
    if haystack.len() < suffix.len() { return false; }
    &haystack[haystack.len() - suffix.len()..] == suffix
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

unsafe fn write_summary(total: i32, failures: i32) {
    let pass = total - failures;
    write(1, b"SUMMARY pass=".as_ptr(), 13);
    write_i64(1, pass as i64);
    write(1, b" fail=".as_ptr(), 6);
    write_i64(1, failures as i64);
    write(1, b"\n".as_ptr(), 1);
}
