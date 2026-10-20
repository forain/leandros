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

mod cost;
mod abi;

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
    pub const TIMERFD_CREATE: i64 = 283;
    pub const TIMERFD_SETTIME: i64 = 286;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const SIGNALFD4: i64 = 74;
    pub const INOTIFY_INIT1: i64 = 26;
    pub const INOTIFY_ADD_WATCH: i64 = 27;
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

pub type pthread_t = *mut c_void;

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
    pub fn socketpair(domain: c_int, ty: c_int, protocol: c_int, sv: *mut c_int) -> c_int;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;

    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;

    pub fn sigprocmask(how: c_int, set: *const sigset_t, oset: *mut sigset_t) -> c_int;
    pub fn raise(sig: c_int) -> c_int;

    pub fn clock_gettime(clockid: clockid_t, tp: *mut timespec) -> c_int;
    pub fn readlink(path: *const u8, buf: *mut u8, bufsize: size_t) -> ssize_t;
    pub fn getpid() -> c_int;
    pub fn kill(pid: c_int, sig: c_int) -> c_int;

    pub fn usleep(usec: c_uint) -> c_int;
    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;

    // eventfd2/signalfd4/inotify_init1/inotify_add_watch have no relibc C
    // wrapper — go straight through the raw syscall entry point.
    pub fn syscall(sysno: c_long, ...) -> c_long;

    pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    pub fn bind(fd: c_int, addr: *const sockaddr_in, len: u32) -> c_int;
    pub fn listen(fd: c_int, backlog: c_int) -> c_int;
    pub fn connect(fd: c_int, addr: *const sockaddr_in, len: u32) -> c_int;
    pub fn accept(fd: c_int, addr: *mut sockaddr_in, len: *mut u32) -> c_int;
    pub fn getsockname(fd: c_int, addr: *mut sockaddr_in, len: *mut u32) -> c_int;
    #[link_name = "bind"]
    pub fn bind_un(fd: c_int, addr: *const sockaddr_un, len: u32) -> c_int;
    #[link_name = "connect"]
    pub fn connect_un(fd: c_int, addr: *const sockaddr_un, len: u32) -> c_int;
    pub fn send(fd: c_int, buf: *const u8, len: usize, flags: c_int) -> isize;
    pub fn recv(fd: c_int, buf: *mut u8, len: usize, flags: c_int) -> isize;
    pub fn __errno_location() -> *mut c_int;
}

#[repr(C)]
pub struct sockaddr_un {
    pub sun_family: u16,
    pub sun_path: [u8; 108],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct sockaddr_in {
    pub sin_family: u16,
    pub sin_port: u16,
    pub sin_addr: u32,
    pub sin_zero: [u8; 8],
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
pub unsafe extern "C" fn epoll_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    if argc >= 2 {
        let arg = *argv.add(1);
        if !arg.is_null() && *arg == b'b' { return bench(); }
        // `abi` / `pollbench` (lane epollabi, see abi.rs).
        if !arg.is_null() && *arg == b'a' { return abi::abi_main(); }
        if !arg.is_null() && *arg == b'p' { return abi::pollbench(); }
        // `cost` / `costbench` (lane epollcost, see cost.rs).
        if !arg.is_null() && *arg == b'c' {
            return if *arg.add(4) == b'b' { cost::costbench() } else { cost::cost_main() };
        }
    }
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
    if !test_epoll_ctl_add_wakes_parked_waiter() { failures += 1; }
    if !test_tcp_et_no_refire() { failures += 1; }
    if !test_unrelated_unix_traffic_no_wake() { failures += 1; }
    if !test_timerfd_set_not_woken_by_unix_traffic() { failures += 1; }
    if !test_own_timerfd_among_many_wakes() { failures += 1; }
    if !test_lt_stays_reported() { failures += 1; }
    if !test_et_once_across_calls() { failures += 1; }
    if !test_userspace_window() { failures += 1; }
    if !test_keyed_and_unkeyed_burst() { failures += 1; }
    if !test_wake_ring_overflow() { failures += 1; }
    if !test_nested_epoll_warm() { failures += 1; }
    if !test_ctl_del_while_parked() { failures += 1; }
    if !test_oneshot_rearm_warm() { failures += 1; }
    if !test_foreign_add_does_not_widen() { failures += 1; }
    if !test_eventfd_semaphore() { failures += 1; }
    if !test_unix_listener_close_resets_pending() { failures += 1; }
    if !test_nested_inner_add_while_away() { failures += 1; }
    if !test_alternating_instances_cached() { failures += 1; }

    write_summary(27, failures);
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

// ── 11. epoll_ctl(ADD) must wake a sibling already parked in epoll_wait ───────
//
// The targeted-wake scheme records a `poll_mask` (the OR of the parked
// interests' tags) when a thread parks in epoll_wait, and a producer wakes only
// pollers whose mask intersects the changed object's tag. That opens exactly
// one lost-wake hole: a CLONE_THREAD sibling EPOLL_CTL_ADDs a NEW fd to a
// shared epoll instance while this thread is already parked with a mask
// computed BEFORE that fd existed. A later targeted wake for the new fd (here
// the eventfd write, tagged EVENTFD) would not intersect the stale mask (the
// dummy pipe's PIPE tag) and would be lost. The kernel closes it by making
// sys_epoll_ctl issue an untagged BROADCAST wake on any successful mutation;
// this test is the direct check of that.

#[repr(C)]
struct AddRaceArgs { ep: c_int, efd: c_int }

extern "C" fn add_race_sibling(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &*(arg as *const AddRaceArgs);
        usleep(80_000); // let the main thread reach epoll_wait and park
        let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: a.efd } };
        epoll_ctl(a.ep, EPOLL_CTL_ADD, a.efd, &mut ev); // the add whose broadcast we test
        usleep(5_000);
        let one: u64 = 1;
        write(a.efd, &one as *const u64 as *const u8, 8); // EVENTFD-tagged edge
    }
    core::ptr::null_mut()
}

unsafe fn test_epoll_ctl_add_wakes_parked_waiter() -> bool {
    let name = b"epoll_ctl_add_wakes_parked_waiter\0";
    // A dummy pipe, never written, keeps the set non-empty and gives the parked
    // waiter a NARROW mask (the pipe's tag) that excludes the eventfd added
    // later — the exact stale mask the broadcast must defeat.
    let mut pfds = [0 as c_int; 2];
    if pipe2(pfds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let (prd, pwr) = (pfds[0], pfds[1]);
    let ep = epoll_create1(0);
    if ep < 0 { close(prd); close(pwr); return report(name, false); }
    let mut pev = epoll_event { events: EPOLLIN, data: epoll_data { fd: prd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, prd, &mut pev) != 0 {
        close(ep); close(prd); close(pwr); return report(name, false);
    }
    let efd = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    if efd < 0 { close(ep); close(prd); close(pwr); return report(name, false); }

    let mut args = AddRaceArgs { ep, efd };
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(),
                      add_race_sibling, &mut args as *mut _ as *mut c_void) != 0 {
        close(efd); close(ep); close(prd); close(pwr); return report(name, false);
    }

    let mut start: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut start);
    // Finite (not -1) so a REGRESSED kernel fails slow at the deadline instead
    // of hanging the whole suite; the parked code path is identical either way.
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, 4000);
    let mut end: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut end);
    let elapsed_ms = (end.tv_sec - start.tv_sec) * 1000
        + (end.tv_nsec - start.tv_nsec) / 1_000_000;

    pthread_join(th, core::ptr::null_mut());

    let mut saw_efd = false;
    for i in 0..(n.max(0) as usize).min(4) {
        if out[i].data.fd == efd { saw_efd = true; }
    }
    close(efd); close(ep); close(prd); close(pwr);
    // PROMPT wake is the assertion: a kernel that lost the add-race wake only
    // returns at the 4000ms deadline (>1000ms).
    report(name, n >= 1 && saw_efd && elapsed_ms < 1000)
}

// ── 12. EPOLLET on a connected TCP socket fires once per edge ─────────────────
//
// tokio registers every TcpStream EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET. inet
// sockets used to report no edge-seq, so the kernel treated them
// level-triggered even under EPOLLET: an idle, permanently writable
// connection came back from every epoll_wait, and sshd's reactor spun at
// 100% of a CPU for as long as an SSH session was open (lane sshspin,
// 2026-10-17). Linux: the writable edge is reported once; new data is a new
// edge even while the socket is still readable from an older one.

unsafe fn tcp_et_wait(ep: c_int, timeout: c_int) -> (c_int, c_uint) {
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, timeout);
    let ev = if n > 0 { out[0].events } else { 0 };
    (n, ev)
}

unsafe fn test_tcp_et_no_refire() -> bool {
    const AF_INET: c_int = 2;
    const SOCK_STREAM: c_int = 1;
    const EPOLLOUT: c_uint = 0x004;
    let name = b"tcp_et_no_refire\0";
    let lo = sockaddr_in { sin_family: AF_INET as u16, sin_port: 0,
                           sin_addr: u32::from_ne_bytes([127, 0, 0, 1]), sin_zero: [0; 8] };
    let ls = socket(AF_INET, SOCK_STREAM, 0);
    if ls < 0 { return report(name, false); }
    let mut addr = lo;
    let mut alen = core::mem::size_of::<sockaddr_in>() as u32;
    if bind(ls, &lo, alen) != 0 || listen(ls, 4) != 0 || getsockname(ls, &mut addr, &mut alen) != 0 {
        close(ls); return report(name, false);
    }
    let cs = socket(AF_INET, SOCK_STREAM, 0);
    if cs < 0 || connect(cs, &addr, core::mem::size_of::<sockaddr_in>() as u32) != 0 {
        close(ls); return report(name, false);
    }
    let ss = accept(ls, core::ptr::null_mut(), core::ptr::null_mut());
    if ss < 0 { close(cs); close(ls); return report(name, false); }

    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN | EPOLLOUT | EPOLLET, data: epoll_data { fd: cs } };
    if ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, cs, &mut ev) != 0 {
        close(ss); close(cs); close(ls); return report(name, false);
    }
    // The initial edge: writable.
    let (n1, ev1) = tcp_et_wait(ep, 1000);
    // Idle: nothing may come back. A level-reported socket returns from every
    // one of these at once; allow a stray edge (a late ACK of the handshake).
    let mut refires = 0;
    for _ in 0..20 {
        if tcp_et_wait(ep, 10).0 > 0 { refires += 1; }
    }
    // Data is an edge; more data while still unread is another one.
    write(ss, b"x".as_ptr(), 1);
    let (n3, ev3) = tcp_et_wait(ep, 2000);
    write(ss, b"y".as_ptr(), 1);
    let (n4, ev4) = tcp_et_wait(ep, 2000);
    let mut after = 0;
    for _ in 0..10 {
        if tcp_et_wait(ep, 10).0 > 0 { after += 1; }
    }
    let mut buf = [0u8; 8];
    let got = read(cs, buf.as_mut_ptr(), buf.len());

    close(ep); close(ss); close(cs); close(ls);
    write(1, b"  tcp_et: n1=".as_ptr(), 13); write_i64(1, n1 as i64);
    write(1, b" refires=".as_ptr(), 9); write_i64(1, refires);
    write(1, b" n3=".as_ptr(), 4); write_i64(1, n3 as i64);
    write(1, b" n4=".as_ptr(), 4); write_i64(1, n4 as i64);
    write(1, b" after=".as_ptr(), 7); write_i64(1, after);
    write(1, b" read=".as_ptr(), 6); write_i64(1, got as i64);
    write(1, b"\n".as_ptr(), 1);
    report(name, n1 == 1 && ev1 & EPOLLOUT != 0 && refires <= 1
        && n3 == 1 && ev3 & EPOLLIN != 0 && n4 == 1 && ev4 & EPOLLIN != 0
        && after <= 1 && got == 2)
}

// ── 13. unrelated AF_UNIX traffic must not wake a big parked interest set ────
//
// A tokio reactor watching many AF_UNIX sockets (busd, ~40 D-Bus clients)
// parks with three bloom bits per socket; a 64-bit mask is then nearly full and
// every send/recv anywhere in the system used to wake it (YouTube in Firefox
// made the session's D-Bus broker, panel and session manager burn >1 CPU this
// way on the laptop). With exact keys (sched::PollSet) a keyed AF_UNIX wake
// reaches only pollers that named that connection end.
//
// A sibling thread parks in epoll_wait on 48 socketpair ends (EPOLLIN, tokio's
// EPOLLET too) plus a quit eventfd; the main thread then pushes 300 bytes one
// at a time round-robin through 32 private, unwatched socketpairs (many
// distinct objects: one fixed pair either collides with the waiter's bloom or
// it does not, which made a single-pair version pass by luck on the old
// kernel). The verdict counts the waiter's OWN epoll parks over the traffic
// (the kernel's per-thread-group park row in /proc/sysstat, lane nowake):
// parked once and never woken is 1, and fewer than 1/8 of the wake calls may
// reach it. It used to be the system-wide /proc/wakestat count of
// narrow-mask tasks woken, with the waiter minus a control pass without it;
// in a busy live session the rest of the system's wakes differ between the
// two passes by more than the threshold, so that verdict flaked (11 %
// measured against 1/8) without any stray wake of the waiter. The wakestat
// difference is still printed.

const WATCHED_PAIRS: usize = 48;
const PINGS: usize = 300;
const TRAFFIC_PAIRS: usize = 32;

#[repr(C)]
struct ParkArgs { ep: c_int, quit: c_int, returns: i64 }

extern "C" fn park_on_many(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut ParkArgs);
        let mut out: [epoll_event; 8] = core::mem::zeroed();
        for _ in 0..64 {
            let n = epoll_wait(a.ep, out.as_mut_ptr(), 8, 30_000);
            a.returns += 1;
            let mut quit = false;
            for i in 0..(n.max(0) as usize).min(8) {
                if out[i].data.fd == a.quit { quit = true; }
            }
            if quit || n <= 0 { break; }
        }
    }
    core::ptr::null_mut()
}

/// Sum the `calls` column and `woken - allmask` (tasks woken that had a
/// narrow mask) of /proc/wakestat over the net server's wake sites.
unsafe fn wakestat_totals() -> Option<(i64, i64)> {
    let fd = open(b"/proc/wakestat\0".as_ptr(), 0);
    if fd < 0 { return None; }
    let mut buf = [0u8; 8192];
    let mut len = 0usize;
    loop {
        if len >= buf.len() { break; }
        let n = read(fd, buf.as_mut_ptr().add(len), buf.len() - len);
        if n <= 0 { break; }
        len += n as usize;
    }
    close(fd);
    let (mut calls, mut woken) = (0i64, 0i64);
    for line in buf[..len].split(|&c| c == b'\n') {
        // Only the AF_UNIX wake sites (the traffic's own): timer, pipe and
        // eventfd wakes of the rest of the boot are noise here.
        if !line.starts_with(b"servers/net/") { continue; }
        // site calls tagged woken allmask
        let mut col = 0;
        for f in line.split(|&c| c == b' ').filter(|f| !f.is_empty()) {
            let mut v = 0i64;
            for &c in f { if c.is_ascii_digit() { v = v * 10 + (c - b'0') as i64; } }
            if col == 1 { calls += v; }
            if col == 3 { woken += v; }
            if col == 4 { woken -= v; }
            col += 1;
        }
    }
    Some((calls, woken))
}

unsafe fn test_unrelated_unix_traffic_no_wake() -> bool {
    let name = b"unrelated_unix_traffic_no_wake\0";
    const AF_UNIX: c_int = 1;
    const SOCK_STREAM: c_int = 1;
    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }
    let mut watched = [[-1 as c_int; 2]; WATCHED_PAIRS];
    let mut ok = true;
    for p in watched.iter_mut() {
        if socketpair(AF_UNIX, SOCK_STREAM, 0, p.as_mut_ptr()) != 0 { ok = false; break; }
        let mut ev = epoll_event { events: EPOLLIN | EPOLLET, data: epoll_data { fd: p[0] } };
        if epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &mut ev) != 0 { ok = false; break; }
    }
    let quit = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    let mut qev = epoll_event { events: EPOLLIN, data: epoll_data { fd: quit } };
    if quit < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, quit, &mut qev) != 0 { ok = false; }
    let mut traffic = [[-1 as c_int; 2]; TRAFFIC_PAIRS];
    for t in traffic.iter_mut() {
        if socketpair(AF_UNIX, SOCK_STREAM, 0, t.as_mut_ptr()) != 0 { ok = false; }
    }

    let r = if ok { traffic_vs_waiter(ep, quit) } else { None };
    for p in watched.iter() { if p[0] >= 0 { close(p[0]); close(p[1]); } }
    if quit >= 0 { close(quit); }
    close(ep);

    let Some((calls, woken, returns, parks)) = r else { return report(name, false) };
    write(1, b"unrelated_unix_traffic: wake_calls=".as_ptr(), 35);
    write_i64(1, calls);
    write(1, b" system_woken_delta=".as_ptr(), 20);
    write_i64(1, woken);
    write(1, b" waiter_returns=".as_ptr(), 16);
    write_i64(1, returns);
    write(1, b" waiter_parks=".as_ptr(), 14);
    write_i64(1, parks);
    write(1, b"\n".as_ptr(), 1);
    // The traffic makes 2 wake calls per ping (send: RD, recv: WR). A
    // bloom-only kernel wakes the waiter on most of them (hundreds of
    // parks); the waiter may be woken by fewer than 1/8 of them (the old
    // threshold, now on its own parks), which leaves room for a live
    // session's broadcast wakes (the net daemon's safety net). It must have
    // returned exactly once (the quit edge).
    report(name, calls >= PINGS as i64 && parks >= 1 && (parks - 1) * 8 < 2 * PINGS as i64
                 && returns == 1)
}

/// The traffic half of tests 13 and 14: AF_UNIX pings through private pairs,
/// once with no waiter (control) and once with a thread parked in
/// `epoll_wait(ep)`, which `quit` (an eventfd in `ep`) then releases.
/// `(wake calls, narrow-mask tasks woken beyond the control, waiter returns,
/// waiter parks)`.
///
/// The verdict is on the last: the epoll parks of THIS thread group while
/// the waiter sits in its one epoll_wait (the main thread makes no epoll
/// call meanwhile). Parked once, never woken = 1; every wake that reaches it
/// and does not concern it is one more park. The system-wide wakestat
/// difference is printed for context only: in a live desktop session the
/// other tasks' own wakes differ between the two passes by more than the
/// threshold (11 % against 1/8 measured), which made the old verdict flaky.
unsafe fn traffic_vs_waiter(ep: c_int, quit: c_int) -> Option<(i64, i64, i64, i64)> {
    const AF_UNIX: c_int = 1;
    const SOCK_STREAM: c_int = 1;
    let mut ok = true;
    let mut traffic = [[-1 as c_int; 2]; TRAFFIC_PAIRS];
    for t in traffic.iter_mut() {
        if socketpair(AF_UNIX, SOCK_STREAM, 0, t.as_mut_ptr()) != 0 { ok = false; }
    }
    let mut args = ParkArgs { ep, quit, returns: 0 };
    let mut th: pthread_t = core::ptr::null_mut();
    let mut result = None;
    let pings = |traffic: &[[c_int; 2]; TRAFFIC_PAIRS]| -> Option<(i64, i64)> {
        let before = wakestat_totals();
        let mut b = [0u8; 1];
        for i in 0..PINGS {
            let t = traffic[i % TRAFFIC_PAIRS];
            write(t[0], b"x".as_ptr(), 1);
            read(t[1], b.as_mut_ptr(), 1);
            // Spaced, so a waiter woken by one ping has re-parked before the
            // next: back to back, a woken waiter is still Ready for most of
            // them and the count measured scheduling, not the wake filter.
            usleep(2_000);
        }
        let after = wakestat_totals();
        match (before, after) { (Some(b0), Some(b1)) => Some((b1.0 - b0.0, b1.1 - b0.1)), _ => None }
    };
    // Control pass with no waiter: what the same traffic wakes in the rest of
    // the system (other parked tasks whose masks share bits). The waiter's
    // share is the difference.
    let control = if ok { pings(&traffic) } else { None };
    if ok && pthread_create(&mut th, core::ptr::null(), park_on_many,
                            &mut args as *mut _ as *mut c_void) == 0 {
        usleep(150_000); // let the waiter park
        let p0 = epoll_own_parks();
        let with = pings(&traffic);
        let p1 = epoll_own_parks();
        let one: u64 = 1;
        write(quit, &one as *const u64 as *const u8, 8);
        pthread_join(th, core::ptr::null_mut());
        // p1 - p0 counts the re-parks during the traffic; the first park
        // happened before p0.
        if let (Some(c), Some(w)) = (control, with) {
            result = Some((w.0, w.1 - c.1, args.returns, 1 + p1 - p0));
        }
    }
    for t in traffic.iter() { if t[0] >= 0 { close(t[0]); close(t[1]); } }
    result
}

// ── 14. a set of timerfds (and its epoll's own tag) refines keyed wakes ────────
//
// Lane deskbusy2 (2026-10-17): a typical iced/tokio event-loop thread watches an
// eventfd (keyed) plus timerfds, an inotify fd, a signalfd — objects whose
// probes answered with bloom bits but no key — and its epoll's own tag. Every
// such bit "poisoned" the keyed refinement, so with ~600 such threads on the
// laptop desktop each AF_UNIX send during video still woke ~5 of them
// (~17 000 unrefined wakes/s; cosmic-panel, -workspaces, -files-applet...
// spent 10-60 % of a CPU re-probing in the kernel). Timerfds now carry exact
// keys and the others are "foreign" (never targeted by a keyed wake), so a
// waiter parked on 40 disarmed timerfds + an inotify fd + a quit eventfd must
// not be woken by unrelated AF_UNIX traffic (same measure as test 13).

const TIMERFDS: usize = 40;

unsafe fn test_timerfd_set_not_woken_by_unix_traffic() -> bool {
    let name = b"timerfd_set_not_woken_by_unix_traffic\0";
    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }
    let mut ok = true;
    let mut tfds = [-1 as c_int; TIMERFDS];
    for t in tfds.iter_mut() {
        *t = syscall(nr::TIMERFD_CREATE, 1i64 /* CLOCK_MONOTONIC */, 0i64) as c_int;
        let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: *t } };
        if *t < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, *t, &mut ev) != 0 { ok = false; break; }
    }
    let ino = syscall(nr::INOTIFY_INIT1, 0i64) as c_int;
    let mut iev = epoll_event { events: EPOLLIN, data: epoll_data { fd: ino } };
    if ino < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, ino, &mut iev) != 0 { ok = false; }
    let quit = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    let mut qev = epoll_event { events: EPOLLIN, data: epoll_data { fd: quit } };
    if quit < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, quit, &mut qev) != 0 { ok = false; }
    let r = if ok { traffic_vs_waiter(ep, quit) } else { None };
    for t in tfds.iter() { if *t >= 0 { close(*t); } }
    if ino >= 0 { close(ino); }
    if quit >= 0 { close(quit); }
    close(ep);
    let Some((calls, woken, returns, parks)) = r else { return report(name, false) };
    write(1, b"timerfd_set_traffic: wake_calls=".as_ptr(), 32);
    write_i64(1, calls);
    write(1, b" system_woken_delta=".as_ptr(), 20);
    write_i64(1, woken);
    write(1, b" waiter_returns=".as_ptr(), 16);
    write_i64(1, returns);
    write(1, b" waiter_parks=".as_ptr(), 14);
    write_i64(1, parks);
    write(1, b"\n".as_ptr(), 1);
    // Same verdict as test 13: the waiter's own parks.
    report(name, calls >= PINGS as i64 && parks >= 1 && (parks - 1) * 8 < 2 * PINGS as i64
                 && returns == 1)
}

// ── 15. a waiter's own timerfd still wakes it among many keyed ones ───────────
//
// The other half of test 14: the deadline tick's timerfd expiry wake is keyed
// now, so a poller whose timerfd bits are all refined is woken only by its own
// timerfds' keys. 40 disarmed timerfds plus one armed for 50 ms: epoll_wait
// must return that one, on time (and a second time after re-arming).

#[repr(C)]
struct Itimerspec { interval_s: i64, interval_ns: i64, value_s: i64, value_ns: i64 }

unsafe fn test_own_timerfd_among_many_wakes() -> bool {
    let name = b"own_timerfd_among_many_wakes\0";
    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }
    let mut ok = true;
    let mut tfds = [-1 as c_int; TIMERFDS + 1];
    for t in tfds.iter_mut() {
        *t = syscall(nr::TIMERFD_CREATE, 1i64, 0i64) as c_int;
        let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: *t } };
        if *t < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, *t, &mut ev) != 0 { ok = false; break; }
    }
    let armed = tfds[TIMERFDS / 2];
    let mut hits = 0;
    let mut slow = 0i64;
    for _ in 0..2 {
        if !ok { break; }
        let its = Itimerspec { interval_s: 0, interval_ns: 0, value_s: 0, value_ns: 50_000_000 };
        if syscall(nr::TIMERFD_SETTIME, armed as i64, 0i64, &its as *const _ as i64, 0i64) != 0 { ok = false; break; }
        let t0 = now_ms();
        let mut out: [epoll_event; 4] = core::mem::zeroed();
        let n = epoll_wait(ep, out.as_mut_ptr(), 4, 3000);
        let dt = now_ms() - t0;
        if dt > slow { slow = dt; }
        if n == 1 && out[0].data.fd == armed { hits += 1; }
        let mut b = [0u8; 8];
        read(armed, b.as_mut_ptr(), 8);
    }
    for t in tfds.iter() { if *t >= 0 { close(*t); } }
    close(ep);
    write(1, b"own_timerfd: hits=".as_ptr(), 18);
    write_i64(1, hits);
    write(1, b" max_ms=".as_ptr(), 8);
    write_i64(1, slow);
    write(1, b"\n".as_ptr(), 1);
    report(name, ok && hits == 2 && slow < 1000)
}

// ── 16-23. the epoll ready list (lane epollready) ─────────────────────────────
//
// epoll_wait no longer re-probes every registration after every wake: the
// calling thread keeps a snapshot of its registrations across calls and the
// kernel records which wakes reached it (in user space too), so a call
// re-probes only the registrations those wakes can concern, plus the ones that
// were ready last time (the ready list) and new or MODified ones. These cases
// pin the semantics that must not change.

unsafe fn efd_new() -> c_int { syscall(nr::EVENTFD2, 0i64, 0i64) as c_int }
unsafe fn efd_bump(fd: c_int) { let one: u64 = 1; write(fd, &one as *const u64 as *const u8, 8); }
unsafe fn efd_drain(fd: c_int) { let mut b = [0u8; 8]; read(fd, b.as_mut_ptr(), 8); }
unsafe fn ep_add(ep: c_int, fd: c_int, events: c_uint) -> bool {
    let mut ev = epoll_event { events, data: epoll_data { fd } };
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &mut ev) == 0
}
/// The fds `epoll_wait(ep, .., timeout)` reported, as a bitmask over `fds`.
unsafe fn ep_seen(ep: c_int, timeout: c_int, fds: &[c_int]) -> (i32, u64) {
    let mut out: [epoll_event; 64] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 64, timeout);
    let mut m = 0u64;
    for i in 0..(n.max(0) as usize).min(64) {
        let fd = out[i].data.fd;
        if let Some(j) = fds.iter().position(|&f| f == fd) { m |= 1 << j; }
    }
    (n, m)
}

// 16. A level-triggered fd that stays ready is reported by every call, also
// when other registrations come and go around it.
unsafe fn test_lt_stays_reported() -> bool {
    let name = b"lt_stays_reported\0";
    let mut p = [0i32; 2];
    if pipe2(p.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let b = efd_new();
    let ep = epoll_create1(0);
    if ep < 0 || b < 0 || !ep_add(ep, p[0], EPOLLIN) || !ep_add(ep, b, EPOLLIN) { return report(name, false); }
    let fds = [p[0], b];
    write(p[1], b"x".as_ptr(), 1);
    let r1 = ep_seen(ep, 100, &fds);   // A
    efd_bump(b);
    let r2 = ep_seen(ep, 100, &fds);   // A (still) and B
    efd_drain(b);
    let r3 = ep_seen(ep, 100, &fds);   // A only
    let r4 = ep_seen(ep, 0, &fds);     // A again, zero timeout
    let mut buf = [0u8; 4];
    read(p[0], buf.as_mut_ptr(), 4);
    let r5 = ep_seen(ep, 30, &fds);    // nothing
    close(ep); close(p[0]); close(p[1]); close(b);
    write(1, b"  lt_stays: masks=".as_ptr(), 18);
    for r in [r1, r2, r3, r4, r5] { write_i64(1, r.1 as i64); write(1, b" ".as_ptr(), 1); }
    write(1, b"\n".as_ptr(), 1);
    report(name, r1 == (1, 1) && r2 == (2, 3) && r3 == (1, 1) && r4 == (1, 1) && r5 == (0, 0))
}

// 17. EPOLLET over many calls: one report per edge, none while idle, and an
// edge that happens while the thread is in user space is reported by the
// next call.
unsafe fn test_et_once_across_calls() -> bool {
    let name = b"et_once_across_calls\0";
    let mut p = [0i32; 2];
    if pipe2(p.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let other = efd_new();
    let ep = epoll_create1(0);
    if ep < 0 || other < 0 || !ep_add(ep, p[0], EPOLLIN | EPOLLET) || !ep_add(ep, other, EPOLLIN) {
        return report(name, false);
    }
    let fds = [p[0], other];
    write(p[1], b"a".as_ptr(), 1);
    let r1 = ep_seen(ep, 100, &fds);
    let r2 = ep_seen(ep, 20, &fds);
    let r3 = ep_seen(ep, 20, &fds);
    write(p[1], b"b".as_ptr(), 1);     // a new edge, level unchanged
    let r4 = ep_seen(ep, 100, &fds);
    let r5 = ep_seen(ep, 20, &fds);
    close(ep); close(p[0]); close(p[1]); close(other);
    report(name, r1 == (1, 1) && r2.0 == 0 && r3.0 == 0 && r4 == (1, 1) && r5.0 == 0)
}

// 18. Readiness that appears while the caller runs in user space between two
// calls (no thread is parked, so it is only a recorded wake): reported by the
// next zero-timeout call, as are registrations added in between.
unsafe fn test_userspace_window() -> bool {
    let name = b"userspace_window\0";
    let ep = epoll_create1(0);
    let mut e = [-1 as c_int; 8];
    let mut ok = ep >= 0;
    for f in e.iter_mut().take(6) { *f = efd_new(); ok &= *f >= 0 && ep_add(ep, *f, EPOLLIN); }
    if !ok { return report(name, false); }
    efd_bump(e[0]);
    let r1 = ep_seen(ep, 100, &e);      // e0
    efd_drain(e[0]);
    efd_bump(e[3]);                      // while "in user space"
    let r2 = ep_seen(ep, 0, &e);        // e3
    efd_drain(e[3]);
    e[6] = efd_new();
    efd_bump(e[6]);
    ok &= ep_add(ep, e[6], EPOLLIN);     // added already readable
    let r3 = ep_seen(ep, 0, &e);        // e6
    efd_drain(e[6]);
    let r4 = ep_seen(ep, 20, &e);       // nothing
    close(ep);
    for f in e.iter() { if *f >= 0 { close(*f); } }
    report(name, ok && r1 == (1, 1) && r2 == (1, 1 << 3) && r3 == (1, 1 << 6) && r4 == (0, 0))
}

// 19. One burst makes a keyed object (eventfd, pipe), a timerfd (keyed tick
// wake) and a signalfd (an unkeyed, bit-only wake) ready together while the
// waiter is parked: every one of them is reported.

const SIGUSR2: c_int = 12;

#[repr(C)]
struct MixArgs { ep: c_int, fds: [c_int; 4], seen: u64, done: c_int }

extern "C" fn mix_waiter(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut MixArgs);
        let mut out: [epoll_event; 8] = core::mem::zeroed();
        let t0 = now_ms();
        while a.seen != 0xf && now_ms() - t0 < 3000 {
            let n = epoll_wait(a.ep, out.as_mut_ptr(), 8, 200);
            for i in 0..(n.max(0) as usize).min(8) {
                let fd = out[i].data.fd;
                if let Some(j) = a.fds.iter().position(|&f| f == fd) {
                    a.seen |= 1 << j;
                    let mut b = [0u8; 128];
                    read(fd, b.as_mut_ptr(), if j == 3 { 128 } else { 8 });
                }
            }
        }
        a.done = 1;
    }
    core::ptr::null_mut()
}

unsafe fn test_keyed_and_unkeyed_burst() -> bool {
    let name = b"keyed_and_unkeyed_burst\0";
    let mask: sigset_t = 1u64 << (SIGUSR2 - 1);
    if sigprocmask(SIG_BLOCK, &mask, core::ptr::null_mut()) != 0 { return report(name, false); }
    let sfd = syscall(nr::SIGNALFD4, -1i64, &mask as *const sigset_t as *const c_void, 8i64, 0i64) as c_int;
    let efd = efd_new();
    let mut p = [0i32; 2];
    let pok = pipe2(p.as_mut_ptr(), O_NONBLOCK) == 0;
    let tfd = syscall(nr::TIMERFD_CREATE, 1i64, 0i64) as c_int;
    let ep = epoll_create1(0);
    if sfd < 0 || efd < 0 || !pok || tfd < 0 || ep < 0 { return report(name, false); }
    let mut ok = true;
    for f in [efd, p[0], tfd, sfd] { ok &= ep_add(ep, f, EPOLLIN); }
    // 30 idle registrations around them, as a real event loop has.
    let mut idle = [-1 as c_int; 30];
    for f in idle.iter_mut() { *f = efd_new(); ok &= *f >= 0 && ep_add(ep, *f, EPOLLIN); }
    let mut a = MixArgs { ep, fds: [efd, p[0], tfd, sfd], seen: 0, done: 0 };
    let mut th: pthread_t = core::ptr::null_mut();
    if !ok || pthread_create(&mut th, core::ptr::null(), mix_waiter, &mut a as *mut _ as *mut c_void) != 0 {
        return report(name, false);
    }
    usleep(100_000); // parked
    efd_bump(efd);
    kill(getpid(), SIGUSR2);
    write(p[1], b"x".as_ptr(), 1);
    let its = Itimerspec { interval_s: 0, interval_ns: 0, value_s: 0, value_ns: 10_000_000 };
    syscall(nr::TIMERFD_SETTIME, tfd as i64, 0i64, &its as *const _ as i64, 0i64);
    pthread_join(th, core::ptr::null_mut());
    for f in [efd, p[0], p[1], tfd, sfd, ep] { close(f); }
    for f in idle.iter() { if *f >= 0 { close(*f); } }
    write(1, b"  burst: seen=".as_ptr(), 14); write_i64(1, a.seen as i64); write(1, b"\n".as_ptr(), 1);
    report(name, a.seen == 0xf)
}

// 20. More wakes than the per-thread wake ring holds (8 keys): 40 eventfds
// become ready while the waiter is busy in user space, then while it is
// parked. Every one is reported (the overflow falls back to bits).
const OVF: usize = 40;

#[repr(C)]
struct OvfArgs { ep: c_int, fds: [c_int; OVF], first: u64, second: u64 }

extern "C" fn ovf_waiter(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut OvfArgs);
        // Round 1: busy in user space while all 40 fire, then one zero-
        // timeout call must report them all.
        let (_, m0) = ep_seen(a.ep, 2000, &a.fds[..1]); // fd 0: the go signal
        if m0 == 1 { efd_drain(a.fds[0]); }
        usleep(200_000);
        let (_, m) = ep_seen(a.ep, 0, &a.fds);
        a.first = m;
        for j in 0..OVF { if m & (1 << j) != 0 { efd_drain(a.fds[j]); } }
        // Round 2: parked while all 40 fire in one burst.
        let t0 = now_ms();
        while a.second != (1u64 << OVF) - 1 && now_ms() - t0 < 3000 {
            let (_, m) = ep_seen(a.ep, 500, &a.fds);
            for j in 0..OVF { if m & (1 << j) != 0 { efd_drain(a.fds[j]); } }
            a.second |= m;
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_wake_ring_overflow() -> bool {
    let name = b"wake_ring_overflow\0";
    let ep = epoll_create1(0);
    let mut a = OvfArgs { ep, fds: [-1; OVF], first: 0, second: 0 };
    let mut ok = ep >= 0;
    for f in a.fds.iter_mut() { *f = efd_new(); ok &= *f >= 0 && ep_add(ep, *f, EPOLLIN); }
    let mut th: pthread_t = core::ptr::null_mut();
    if !ok || pthread_create(&mut th, core::ptr::null(), ovf_waiter, &mut a as *mut _ as *mut c_void) != 0 {
        return report(name, false);
    }
    usleep(50_000);
    efd_bump(a.fds[0]);
    usleep(50_000);   // the waiter is in its 200 ms sleep now
    for j in 1..OVF { efd_bump(a.fds[j]); }
    efd_bump(a.fds[0]);
    usleep(400_000);  // round 2: the waiter parks
    for j in 0..OVF { efd_bump(a.fds[j]); }
    pthread_join(th, core::ptr::null_mut());
    for f in a.fds.iter() { if *f >= 0 { close(*f); } }
    close(ep);
    let all = (1u64 << OVF) - 1;
    write(1, b"  overflow: first=".as_ptr(), 18); write_i64(1, a.first as i64);
    write(1, b" second=".as_ptr(), 8); write_i64(1, a.second as i64); write(1, b"\n".as_ptr(), 1);
    report(name, a.first == all && a.second == all)
}

// 21. Nested epoll with a warm snapshot: the inner set's readiness reaches a
// waiter parked on the outer set, and the inner fd is then reported by the
// inner set.
#[repr(C)]
struct NestArgs { outer: c_int, inner: c_int, q: c_int, pr: c_int, got_inner: c_int, got_pipe: c_int }

extern "C" fn nest_waiter(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut NestArgs);
        // Warm the snapshot: q is already readable.
        let (_, m) = ep_seen(a.outer, 1000, &[a.q]);
        if m == 1 { efd_drain(a.q); }
        let (_, m2) = ep_seen(a.outer, 3000, &[a.inner]);
        a.got_inner = (m2 == 1) as c_int;
        let (_, m3) = ep_seen(a.inner, 0, &[a.pr]);
        a.got_pipe = (m3 == 1) as c_int;
    }
    core::ptr::null_mut()
}

unsafe fn test_nested_epoll_warm() -> bool {
    let name = b"nested_epoll_warm\0";
    let mut p = [0i32; 2];
    if pipe2(p.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let inner = epoll_create1(0);
    let outer = epoll_create1(0);
    let q = efd_new();
    if inner < 0 || outer < 0 || q < 0 || !ep_add(inner, p[0], EPOLLIN)
        || !ep_add(outer, inner, EPOLLIN) || !ep_add(outer, q, EPOLLIN) {
        return report(name, false);
    }
    efd_bump(q);
    let mut a = NestArgs { outer, inner, q, pr: p[0], got_inner: 0, got_pipe: 0 };
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), nest_waiter, &mut a as *mut _ as *mut c_void) != 0 {
        return report(name, false);
    }
    usleep(150_000);
    write(p[1], b"x".as_ptr(), 1);
    pthread_join(th, core::ptr::null_mut());
    for f in [p[0], p[1], inner, outer, q] { close(f); }
    report(name, a.got_inner == 1 && a.got_pipe == 1)
}

// 22. EPOLL_CTL_DEL by another thread while the waiter is parked: the removed
// fd is never reported, the remaining one is.
#[repr(C)]
struct DelArgs { ep: c_int, a: c_int, b: c_int, r1: (i32, u64), r2: (i32, u64) }

extern "C" fn del_waiter(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let d = &mut *(arg as *mut DelArgs);
        let fds = [d.a, d.b];
        d.r1 = ep_seen(d.ep, 3000, &fds);
        efd_drain(d.b);
        d.r2 = ep_seen(d.ep, 50, &fds);
    }
    core::ptr::null_mut()
}

unsafe fn test_ctl_del_while_parked() -> bool {
    let name = b"ctl_del_while_parked\0";
    let ep = epoll_create1(0);
    let a = efd_new();
    let b = efd_new();
    let warm = efd_new();
    if ep < 0 || a < 0 || b < 0 || warm < 0 || !ep_add(ep, a, EPOLLIN) || !ep_add(ep, b, EPOLLIN)
        || !ep_add(ep, warm, EPOLLIN) {
        return report(name, false);
    }
    let mut d = DelArgs { ep, a, b, r1: (0, 0), r2: (0, 0) };
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), del_waiter, &mut d as *mut _ as *mut c_void) != 0 {
        return report(name, false);
    }
    usleep(100_000);
    let mut ev = epoll_event { events: 0, data: epoll_data { fd: a } };
    let del = epoll_ctl(ep, 2 /* EPOLL_CTL_DEL */, a, &mut ev);
    efd_bump(a);
    usleep(100_000);
    efd_bump(b);
    pthread_join(th, core::ptr::null_mut());
    for f in [ep, a, b, warm] { close(f); }
    report(name, del == 0 && d.r1 == (1, 2) && d.r2 == (0, 0))
}

// 23. EPOLLONESHOT re-armed by MOD on a warm snapshot (calloop's pattern: it
// re-arms after every event): a re-armed, still-ready fd fires on the next
// call; a disarmed one never does.
unsafe fn test_oneshot_rearm_warm() -> bool {
    let name = b"oneshot_rearm_warm\0";
    let ep = epoll_create1(0);
    let e = efd_new();
    let other = efd_new();
    if ep < 0 || e < 0 || other < 0 || !ep_add(ep, e, EPOLLIN | EPOLLONESHOT) || !ep_add(ep, other, EPOLLIN) {
        return report(name, false);
    }
    let fds = [e, other];
    let rearm = || { let mut ev = epoll_event { events: EPOLLIN | EPOLLONESHOT, data: epoll_data { fd: e } };
                     epoll_ctl(ep, EPOLL_CTL_MOD, e, &mut ev) == 0 };
    efd_bump(e);
    let r1 = ep_seen(ep, 100, &fds);   // fires, disarms
    let r2 = ep_seen(ep, 20, &fds);    // disarmed although readable
    let m1 = rearm();
    let r3 = ep_seen(ep, 0, &fds);     // re-armed and still readable
    efd_drain(e);
    let m2 = rearm();
    let r4 = ep_seen(ep, 20, &fds);    // armed, not readable
    efd_bump(e);
    let r5 = ep_seen(ep, 100, &fds);   // fires
    close(ep); close(e); close(other);
    report(name, m1 && m2 && r1 == (1, 1) && r2.0 == 0 && r3 == (1, 1) && r4.0 == 0 && r5 == (1, 1))
}

// 24. An epoll_ctl ADD on OTHER instances must not widen this thread's
// ready-list filter for good. ADDing a not-ready fd widens the masks of the
// waiters whose mask holds the instance's self-tag bit (a bloom bit other
// instances share); a listener's filter is not rebuilt per park, so a
// widening that stuck to it (with a broadcast-tagged fd: a TCP listener)
// made it a broadcast waiter woken by every wake in the system. Measured by
// this thread group's epoll kernel time while unrelated AF_UNIX traffic
// flows: about one quiet call's worth, not a pass per push. (The system-wide
// park count is printed too, but broadcast waiters like sshd's park per push
// whatever this thread does.)
unsafe fn epoll_parks() -> i64 {
    let fd = open(b"/proc/wakestat\0".as_ptr(), 0);
    if fd < 0 { return -1; }
    static mut WBUF: [u8; 65536] = [0; 65536];
    let buf = &mut *core::ptr::addr_of_mut!(WBUF);
    let mut len = 0usize;
    while len < buf.len() {
        let n = read(fd, buf.as_mut_ptr().add(len), buf.len() - len);
        if n <= 0 { break; }
        len += n as usize;
    }
    close(fd);
    for line in buf[..len].split(|&c| c == b'\n') {
        // # epoll calls N miss N probes N unknown N parks N
        if !line.starts_with(b"# epoll calls") { continue; }
        let mut last = -1i64;
        for f in line.split(|&c| c == b' ') {
            if !f.is_empty() && f.iter().all(|c| c.is_ascii_digit()) {
                last = f.iter().fold(0i64, |x, &c| x * 10 + (c - b'0') as i64);
            }
        }
        return last;
    }
    -1
}

// 25. EFD_SEMAPHORE: a read returns 1 and subtracts 1 (POLLIN stays up while
// the counter is > 0); without the flag a read returns the whole counter.
unsafe fn test_eventfd_semaphore() -> bool {
    let name = b"eventfd_semaphore\0";
    const EFD_SEMAPHORE: i64 = 1;
    const EFD_NONBLOCK: i64 = 0o4000;
    let sem = syscall(nr::EVENTFD2, 3i64, EFD_SEMAPHORE | EFD_NONBLOCK) as c_int;
    let plain = syscall(nr::EVENTFD2, 3i64, EFD_NONBLOCK) as c_int;
    let ep = epoll_create1(0);
    if sem < 0 || plain < 0 || ep < 0 || !ep_add(ep, sem, EPOLLIN) { return report(name, false); }
    let rd = |fd: c_int| -> i64 {
        let mut v: u64 = 0;
        if read(fd, &mut v as *mut u64 as *mut u8, 8) == 8 { v as i64 } else { -1 }
    };
    let mut ok = true;
    for _ in 0..3 {
        ok &= ep_seen(ep, 0, &[sem]).1 == 1;
        ok &= rd(sem) == 1;
    }
    ok &= ep_seen(ep, 0, &[sem]).1 == 0;
    ok &= rd(sem) == -1;
    let two: u64 = 2;
    write(sem, &two as *const u64 as *const u8, 8);
    ok &= rd(sem) == 1;
    ok &= rd(sem) == 1;
    ok &= rd(sem) == -1;
    ok &= rd(plain) == 3;
    ok &= rd(plain) == -1;
    close(sem); close(plain); close(ep);
    report(name, ok)
}

unsafe fn test_foreign_add_does_not_widen() -> bool {
    let name = b"foreign_add_does_not_widen\0";
    const AF_UNIX: c_int = 1;
    const SOCK_STREAM: c_int = 1;
    const AF_INET: c_int = 2;
    let ep = epoll_create1(0);
    let quit = efd_new();
    if ep < 0 || quit < 0 { return report(name, false); }
    let mut a = BenchArgs { pairs: [[-1; 2]; BENCH_PAIRS], ack: -1, rounds: 400, traffic: [[-1; 2]; 8], quit };
    for p in a.pairs.iter_mut() {
        if socketpair(AF_UNIX, SOCK_STREAM, 0, p.as_mut_ptr()) != 0 || !ep_add(ep, p[0], EPOLLIN) {
            return report(name, false);
        }
    }
    if !ep_add(ep, quit, EPOLLIN) { return report(name, false); }
    for t in a.traffic.iter_mut() {
        if socketpair(AF_UNIX, SOCK_STREAM, 0, t.as_mut_ptr()) != 0 { return report(name, false); }
    }
    // Warm: this thread now listens with its real (narrow) filter.
    let mut out: [epoll_event; 8] = core::mem::zeroed();
    epoll_wait(ep, out.as_mut_ptr(), 8, 0);
    // Foreign ADDs of a not-ready broadcast-tagged fd on 32 live instances
    // (32 self-tag bits) while this thread is in user space.
    let lo = sockaddr_in { sin_family: AF_INET as u16, sin_port: 0,
                           sin_addr: u32::from_ne_bytes([127, 0, 0, 1]), sin_zero: [0; 8] };
    let ls = socket(AF_INET, SOCK_STREAM, 0);
    let listening = ls >= 0 && bind(ls, &lo, core::mem::size_of::<sockaddr_in>() as u32) == 0 && listen(ls, 4) == 0;
    let mut others = [-1 as c_int; 32];
    for o in others.iter_mut() {
        *o = epoll_create1(0);
        if listening && *o >= 0 { ep_add(*o, ls, EPOLLIN); }
    }
    for &o in others.iter() { if o >= 0 { close(o); } }
    // The rest of the system's parks over a quiet 400 ms wait (baseline).
    // This thread group's epoll kernel time over a quiet 400 ms wait
    // (baseline: one cached pass, then the timeout), then over the
    // unrelated traffic. A filter the ADDs widened to broadcast re-probes on
    // every one of the ~800 wakes (a RD and a WR wake per push).
    let (_, q0) = epoll_sysstat();
    let quiet = epoll_wait(ep, out.as_mut_ptr(), 8, 400);
    let (_, q1) = epoll_sysstat();
    let p0 = epoll_parks();
    let (_, n0) = epoll_sysstat();
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), bench_noise, &mut a as *mut _ as *mut c_void);
    let mut quit_seen = false;
    let mut other = 0;
    while !quit_seen {
        let n = epoll_wait(ep, out.as_mut_ptr(), 8, 10_000);
        if n <= 0 { break; }
        for i in 0..(n as usize).min(8) {
            if out[i].data.fd == quit { quit_seen = true; } else { other += 1; }
        }
    }
    pthread_join(th, core::ptr::null_mut());
    let (_, n1) = epoll_sysstat();
    let parks = epoll_parks() - p0; // system-wide: sshd-like broadcast waiters too
    let (quiet_ns, busy_ns) = (q1 - q0, n1 - n0);
    write(1, b"  foreign_add_does_not_widen: pushes=400 kernel_ns=".as_ptr(), 51); write_i64(1, busy_ns);
    write(1, b" quiet_kernel_ns=".as_ptr(), 17); write_i64(1, quiet_ns);
    write(1, b" system_parks=".as_ptr(), 14); write_i64(1, parks);
    write(1, b" tcp_listener=".as_ptr(), 14); write_i64(1, listening as i64);
    write(1, b"\n".as_ptr(), 1);
    for p in a.pairs.iter().chain(a.traffic.iter()) { close(p[0]); close(p[1]); }
    if ls >= 0 { close(ls); }
    close(ep); close(quit);
    // Unwidened: one wake (quit), about the quiet call's cost. Widened: ~800
    // passes (>= 2 ms on HVF, >= 20 ms on TCG). The floor absorbs one slow
    // pass under a loaded host.
    report(name, quiet == 0 && quit_seen && other == 0 && busy_ns >= 0
                 && busy_ns < (10 * quiet_ns).max(400_000))
}

// ── 25. closing an AF_UNIX listener resets its pending connectors ──────────
//
// Lane nowake (2026-10-18). A connect() to an AF_UNIX listener returns at once
// and waits on the listener's queue for its accept(). If the listener is
// closed first, Linux (`unix_release_sock` on the queued embryos) shuts the
// connector down with ECONNRESET: it polls POLLHUP|POLLERR (and readable), its
// first recv answers ECONNRESET and later ones EOF, and a send answers EPIPE.
// LeandrOS left it pending for ever: no error, and no wake for a waiter in
// epoll_wait (invariant 16). A sibling thread waits on the connector in
// epoll_wait (EPOLLIN; ERR/HUP are implicit); the main thread closes the
// listener 100 ms later. The waiter must return within a second with
// EPOLLHUP|EPOLLERR.

#[repr(C)]
struct LgoneArgs { ep: c_int, n: c_int, events: u32, ms: i64 }

extern "C" fn lgone_waiter(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut LgoneArgs);
        let mut out: [epoll_event; 4] = core::mem::zeroed();
        let t0 = now_ms();
        a.n = epoll_wait(a.ep, out.as_mut_ptr(), 4, 3000);
        a.ms = now_ms() - t0;
        if a.n > 0 { a.events = out[0].events; }
    }
    core::ptr::null_mut()
}

unsafe fn test_unix_listener_close_resets_pending() -> bool {
    let name = b"unix_listener_close_resets_pending\0";
    const AF_UNIX: c_int = 1;
    const SOCK_STREAM: c_int = 1;
    const EPOLLERR: u32 = 0x008;
    const EPOLLHUP: u32 = 0x010;
    const MSG_NOSIGNAL: c_int = 0x4000;
    const ECONNRESET: c_int = 104;
    const EPIPE: c_int = 32;
    // Abstract address, unique per run.
    let mut addr = sockaddr_un { sun_family: AF_UNIX as u16, sun_path: [0; 108] };
    let tag = b"epolltest-lgone-";
    addr.sun_path[1..1 + tag.len()].copy_from_slice(tag);
    let mut pid = getpid() as u32;
    let mut len = 1 + tag.len();
    while pid > 0 { addr.sun_path[len] = b'0' + (pid % 10) as u8; pid /= 10; len += 1; }
    let alen = (2 + len) as u32;
    let ls = socket(AF_UNIX, SOCK_STREAM, 0);
    if ls < 0 || bind_un(ls, &addr, alen) != 0 || listen(ls, 4) != 0 { return report(name, false); }
    let c = socket(AF_UNIX, SOCK_STREAM, 0);
    if c < 0 || connect_un(c, &addr, alen) != 0 { return report(name, false); }
    // Bytes written before the accept are buffered (and die with the embryo).
    let early = send(c, b"hi".as_ptr(), 2, MSG_NOSIGNAL) == 2;
    let ep = epoll_create1(0);
    if ep < 0 || !ep_add(ep, c, EPOLLIN) { return report(name, false); }
    // Pending, listener alive: nothing to report yet.
    let (quiet_n, _) = ep_seen(ep, 0, &[c]);
    let mut a = LgoneArgs { ep, n: -1, events: 0, ms: -1 };
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), lgone_waiter, &mut a as *mut _ as *mut c_void) != 0 {
        return report(name, false);
    }
    usleep(100_000); // let the waiter park
    close(ls);
    pthread_join(th, core::ptr::null_mut());
    let mut b = [0u8; 4];
    let r1 = recv(c, b.as_mut_ptr(), 4, 0);
    let e1 = if r1 < 0 { *__errno_location() } else { 0 };
    let r2 = recv(c, b.as_mut_ptr(), 4, 0);
    let w = send(c, b"x".as_ptr(), 1, MSG_NOSIGNAL);
    let ew = if w < 0 { *__errno_location() } else { 0 };
    close(c);
    close(ep);
    write(1, b"  unix_listener_gone: n=".as_ptr(), 24); write_i64(1, a.n as i64);
    write(1, b" events=".as_ptr(), 8); write_i64(1, a.events as i64);
    write(1, b" ms=".as_ptr(), 4); write_i64(1, a.ms);
    write(1, b" recv1=".as_ptr(), 7); write_i64(1, if r1 < 0 { -(e1 as i64) } else { r1 as i64 });
    write(1, b" recv2=".as_ptr(), 7); write_i64(1, r2 as i64);
    write(1, b" send=".as_ptr(), 6); write_i64(1, if w < 0 { -(ew as i64) } else { w as i64 });
    write(1, b"\n".as_ptr(), 1);
    report(name, early && quiet_n == 0 && a.n == 1
                 && a.events & (EPOLLHUP | EPOLLERR) == (EPOLLHUP | EPOLLERR)
                 && a.ms >= 0 && a.ms < 1000
                 && r1 == -1 && e1 == ECONNRESET && r2 == 0 && w == -1 && ew == EPIPE)
}

// ── 26. a nested instance's new registration is woken (lane nowake) ─────────
//
// An outer instance E1 watches an inner instance E2. A nested registration's
// answer is keyless (E2's bits), so after a thread's warm wait on E1, an ADD
// to E2 of a not-ready keyed fd whose bloom bit E2 already holds left the
// answer — and so the thread's listener filter — unchanged: the filter, built
// with the inner keys of the time, then dropped the new fd's keyed wake and
// the waiter slept through it. E2 holds 64 socketpair ends (128 keys), so a
// new pipe's bit collides with ~87 % odds; 4 trials, each a new pipe added
// while away and written 100 ms into a 3 s wait on E1, which must return
// within a second every time.

#[repr(C)]
struct LateWrite { fd: c_int }

extern "C" fn late_write(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &*(arg as *const LateWrite);
        usleep(100_000);
        write(a.fd, b"x".as_ptr(), 1);
        // Linger past the verdict: a thread exit wakes broadcast, which
        // would rescue a lost keyed wake and hide the bug.
        usleep(1_200_000);
    }
    core::ptr::null_mut()
}

unsafe fn test_nested_inner_add_while_away() -> bool {
    let name = b"nested_inner_add_while_away\0";
    const AF_UNIX: c_int = 1;
    const SOCK_STREAM: c_int = 1;
    const TRIALS: usize = 4;
    let e1 = epoll_create1(0);
    let e2 = epoll_create1(0);
    if e1 < 0 || e2 < 0 { return report(name, false); }
    let mut pairs = [[-1 as c_int; 2]; 64];
    for p in pairs.iter_mut() {
        if socketpair(AF_UNIX, SOCK_STREAM, 0, p.as_mut_ptr()) != 0 || !ep_add(e2, p[0], EPOLLIN) {
            return report(name, false);
        }
    }
    if !ep_add(e1, e2, EPOLLIN) { return report(name, false); }
    // Warm: this thread's snapshot of E1 and its listener filter now exist.
    let (warm, _) = ep_seen(e1, 0, &[e2]);
    let mut pipes = [[-1 as c_int; 2]; TRIALS];
    let (mut hits, mut slow) = (0, 0i64);
    for pp in pipes.iter_mut() {
        // Away from epoll_wait: a not-ready pipe joins the inner instance.
        if pipe2(pp.as_mut_ptr(), 0) != 0 || !ep_add(e2, pp[0], EPOLLIN) { break; }
        let a = LateWrite { fd: pp[1] };
        let mut th: pthread_t = core::ptr::null_mut();
        if pthread_create(&mut th, core::ptr::null(), late_write, &a as *const _ as *mut c_void) != 0 { break; }
        let t0 = now_ms();
        let (n, seen) = ep_seen(e1, 3000, &[e2]);
        let ms = now_ms() - t0;
        pthread_join(th, core::ptr::null_mut());
        if n == 1 && seen == 1 { hits += 1; }
        if ms > slow { slow = ms; }
        let mut b = [0u8; 1];
        read(pp[0], b.as_mut_ptr(), 1); // E2 quiet again for the next trial
    }
    for p in pairs.iter().chain(pipes.iter()) { if p[0] >= 0 { close(p[0]); close(p[1]); } }
    close(e2); close(e1);
    write(1, b"  nested_inner_add: hits=".as_ptr(), 25); write_i64(1, hits);
    write(1, b" max_ms=".as_ptr(), 8); write_i64(1, slow);
    write(1, b"\n".as_ptr(), 1);
    report(name, warm == 0 && hits == TRIALS as i64 && slow < 1000)
}

// ── 27. a thread alternating two instances keeps both snapshots ─────────────
//
// Lane nowake. cosmic-comp's main thread waits on calloop's instance, then on
// wayland-server's (a zero-timeout wait in `dispatch_clients`); with one
// snapshot per thread every switch missed the ready-list cache (a full probe
// pass). The kernel now keeps the other instance's snapshot. 40 rounds over
// two instances of 16 pipes each: an event on B is seen by B's wait, an event
// on A that arrives while the thread is in B's wait is NOT reported by B and
// IS reported by the next wait on A (the kept snapshot's carry), and the
// thread group's "other instance" misses (/proc/sysstat row 65533) stay a
// handful instead of ~80.

const NR_EPOLL_MISS_INST: i64 = 0xfffd;

unsafe fn test_alternating_instances_cached() -> bool {
    let name = b"alternating_instances_cached\0";
    const N: usize = 16;
    const ROUNDS: usize = 40;
    let ea = epoll_create1(0);
    let eb = epoll_create1(0);
    if ea < 0 || eb < 0 { return report(name, false); }
    let mut pa = [[-1 as c_int; 2]; N];
    let mut pb = [[-1 as c_int; 2]; N];
    for i in 0..N {
        if pipe2(pa[i].as_mut_ptr(), O_NONBLOCK) != 0 || pipe2(pb[i].as_mut_ptr(), O_NONBLOCK) != 0
            || !ep_add(ea, pa[i][0], EPOLLIN) || !ep_add(eb, pb[i][0], EPOLLIN) {
            return report(name, false);
        }
    }
    let rd: [c_int; N] = core::array::from_fn(|i| pa[i][0]);
    let rb: [c_int; N] = core::array::from_fn(|i| pb[i][0]);
    // Warm both.
    ep_seen(ea, 0, &rd);
    ep_seen(eb, 0, &rb);
    let m0 = sysstat_own(&[NR_EPOLL_MISS_INST]).0;
    let mut ok_rounds = 0;
    let mut b = [0u8; 8];
    for r in 0..ROUNDS {
        let k = r % N;
        efd_bump_pipe(pb[k][1]);
        let (nb, sb) = ep_seen(eb, 0, &rb);
        read(pb[k][0], b.as_mut_ptr(), 8);
        efd_bump_pipe(pa[k][1]);
        let (nb2, _) = ep_seen(eb, 0, &rb);          // A's event is not B's
        let (na, sa) = ep_seen(ea, 1000, &rd);       // ...but A's next wait has it
        read(pa[k][0], b.as_mut_ptr(), 8);
        if nb == 1 && sb == 1 << k && nb2 == 0 && na == 1 && sa == 1 << k { ok_rounds += 1; }
    }
    let misses = sysstat_own(&[NR_EPOLL_MISS_INST]).0 - m0;
    for i in 0..N { close(pa[i][0]); close(pa[i][1]); close(pb[i][0]); close(pb[i][1]); }
    close(ea); close(eb);
    write(1, b"  alternating: ok_rounds=".as_ptr(), 25); write_i64(1, ok_rounds);
    write(1, b" inst_misses=".as_ptr(), 13); write_i64(1, misses);
    write(1, b"\n".as_ptr(), 1);
    report(name, ok_rounds == ROUNDS as i64 && misses <= 4)
}

unsafe fn efd_bump_pipe(fd: c_int) { write(fd, b"x".as_ptr(), 1); }

// ── bench (`epolltest bench`, not part of the suite) ─────────────────────────
//
// Kernel time per epoll_wait from /proc/sysstat (this thread group, the epoll
// wait syscalls only):
//  - `roundtrip`: a thread waits on 41 registrations (40 socketpair ends + an
//    eventfd); a helper makes one of them readable at a time and waits for the
//    reply. One wake and one event per call: cosmic-comp's pattern.
//  - `spurious`: the same set plus /dev/tty (a console proxy, no per-object
//    tag: the thread parks broadcast like sshd) while a helper pushes bytes
//    through 8 unwatched socketpairs: every push wakes it, none is for it.

#[cfg(target_arch = "x86_64")]
const EPOLL_NRS: [i64; 3] = [232, 281, 441];
#[cfg(target_arch = "aarch64")]
const EPOLL_NRS: [i64; 2] = [22, 441];

/// (calls, kernel ns) of this thread group's epoll waits so far.
unsafe fn epoll_sysstat() -> (i64, i64) { sysstat_own(&EPOLL_NRS) }

/// Pseudo syscall number of the kernel's epoll park census row
/// (`sched::sysacct::NR_EPOLL_PARK`): its `calls` column counts every real
/// sleep this thread group's epoll waits took.
const NR_EPOLL_PARK: i64 = 0xffff;

/// How many times this thread group's epoll waits have parked so far.
unsafe fn epoll_own_parks() -> i64 { sysstat_own(&[NR_EPOLL_PARK]).0 }

/// (calls, kernel ns) summed over this thread group's /proc/sysstat rows
/// whose syscall number is in `nrs`.
unsafe fn sysstat_own(nrs: &[i64]) -> (i64, i64) {
    let fd = open(b"/proc/sysstat\0".as_ptr(), 0);
    if fd < 0 { return (0, 0); }
    static mut BUF: [u8; 262144] = [0; 262144];
    let buf = &mut *core::ptr::addr_of_mut!(BUF);
    let mut len = 0usize;
    while len < buf.len() {
        let n = read(fd, buf.as_mut_ptr().add(len), buf.len() - len);
        if n <= 0 { break; }
        len += n as usize;
    }
    close(fd);
    let me = getpid() as i64;
    let (mut calls, mut ns) = (0i64, 0i64);
    for line in buf[..len].split(|&c| c == b'\n') {
        if !line.starts_with(b"s ") { continue; }
        let mut v = [0i64; 6];
        for (i, f) in line[2..].split(|&c| c == b' ').filter(|f| !f.is_empty()).take(5).enumerate() {
            let mut x = 0i64;
            for &c in f { if c.is_ascii_digit() { x = x * 10 + (c - b'0') as i64; } }
            v[i] = x;
        }
        // s tgid nr calls ns zero
        if v[0] == me && nrs.contains(&v[1]) {
            calls += v[2];
            ns += v[3];
        }
    }
    (calls, ns)
}

const BENCH_PAIRS: usize = 40;

#[repr(C)]
struct BenchArgs { pairs: [[c_int; 2]; BENCH_PAIRS], ack: c_int, rounds: usize, traffic: [[c_int; 2]; 8], quit: c_int }

extern "C" fn bench_pinger(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut BenchArgs);
        for i in 0..a.rounds {
            write(a.pairs[i % BENCH_PAIRS][1], b"x".as_ptr(), 1);
            efd_drain(a.ack); // blocking: the waiter's reply
        }
    }
    core::ptr::null_mut()
}

extern "C" fn bench_noise(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut BenchArgs);
        let mut b = [0u8; 1];
        for i in 0..a.rounds {
            let t = a.traffic[i % 8];
            write(t[0], b"x".as_ptr(), 1);
            read(t[1], b.as_mut_ptr(), 1);
            usleep(1_000);
        }
        efd_bump(a.quit);
    }
    core::ptr::null_mut()
}

unsafe fn bench() -> i32 {
    const AF_UNIX: c_int = 1;
    const SOCK_STREAM: c_int = 1;
    let ep = epoll_create1(0);
    let ack = efd_new();
    let quit = efd_new();
    let mut a = BenchArgs { pairs: [[-1; 2]; BENCH_PAIRS], ack, rounds: 2000, traffic: [[-1; 2]; 8], quit };
    for p in a.pairs.iter_mut() {
        if socketpair(AF_UNIX, SOCK_STREAM, 0, p.as_mut_ptr()) != 0 || !ep_add(ep, p[0], EPOLLIN) { return 1; }
    }
    if !ep_add(ep, quit, EPOLLIN) { return 1; }
    for t in a.traffic.iter_mut() { if socketpair(AF_UNIX, SOCK_STREAM, 0, t.as_mut_ptr()) != 0 { return 1; } }

    // roundtrip
    let mut th: pthread_t = core::ptr::null_mut();
    let (c0, n0) = epoll_sysstat();
    let t0 = now_ms();
    pthread_create(&mut th, core::ptr::null(), bench_pinger, &mut a as *mut _ as *mut c_void);
    let mut out: [epoll_event; 8] = core::mem::zeroed();
    let mut done = 0usize;
    let mut b = [0u8; 16];
    while done < a.rounds {
        let n = epoll_wait(ep, out.as_mut_ptr(), 8, 5000);
        if n <= 0 { break; }
        for i in 0..(n as usize).min(8) {
            read(out[i].data.fd, b.as_mut_ptr(), 16);
            done += 1;
            efd_bump(ack);
        }
    }
    pthread_join(th, core::ptr::null_mut());
    let ms = now_ms() - t0;
    let (c1, n1) = epoll_sysstat();
    let calls = c1 - c0;
    write(1, b"bench roundtrip: rounds=".as_ptr(), 24); write_i64(1, done as i64);
    write(1, b" calls=".as_ptr(), 7); write_i64(1, calls);
    write(1, b" kernel_ns_per_call=".as_ptr(), 20); write_i64(1, if calls > 0 { (n1 - n0) / calls } else { -1 });
    write(1, b" wall_us_per_round=".as_ptr(), 19); write_i64(1, ms * 1000 / done.max(1) as i64);
    write(1, b"\n".as_ptr(), 1);

    // spurious
    let tty = open(b"/dev/tty\0".as_ptr(), 2);
    let watched_tty = tty >= 0 && ep_add(ep, tty, EPOLLIN);
    a.rounds = 1000;
    let (c0, n0) = epoll_sysstat();
    pthread_create(&mut th, core::ptr::null(), bench_noise, &mut a as *mut _ as *mut c_void);
    let mut quit_seen = false;
    let mut other = 0i64;
    while !quit_seen {
        let n = epoll_wait(ep, out.as_mut_ptr(), 8, 10_000);
        if n <= 0 { break; }
        for i in 0..(n as usize).min(8) {
            if out[i].data.fd == quit { quit_seen = true; } else { other += 1; }
        }
        if other > 50 { break; } // the tty reports input: not this bench
    }
    pthread_join(th, core::ptr::null_mut());
    let (c1, n1) = epoll_sysstat();
    write(1, b"bench spurious: tty=".as_ptr(), 20); write_i64(1, watched_tty as i64);
    write(1, b" pushes=".as_ptr(), 8); write_i64(1, a.rounds as i64);
    write(1, b" calls=".as_ptr(), 7); write_i64(1, c1 - c0);
    write(1, b" other_events=".as_ptr(), 14); write_i64(1, other);
    write(1, b" kernel_ns_per_push=".as_ptr(), 20); write_i64(1, (n1 - n0) / a.rounds as i64);
    write(1, b"\n".as_ptr(), 1);
    0
}

unsafe fn now_ms() -> i64 {
    let mut t: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut t);
    (t.tv_sec as i64) * 1000 + (t.tv_nsec as i64) / 1_000_000
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
