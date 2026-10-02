//! polltest — regression coverage for TODO.md Phase 9 (poll/select/epoll):
//! real per-fd readiness (not the "always ready" stub this phase replaced),
//! real epoll_wait blocking-until-ready-or-timeout, and the `epoll_event`
//! ABI mismatch between the kernel and relibc's userspace poll()/select()
//! emulation (both of which are implemented atop epoll — see
//! userland/relibc/src/header/poll/mod.rs and sys_select/mod.rs).
//!
//! Initializes via relibc_start_v1 (same as pthreadtest/timertest/sigtest)
//! so TLS is set up — errno and the real poll()/select()/epoll_*() Pal
//! calls all need it.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `poll_main` returns the number of failures as the exit code.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_short = i16;
type c_uint = u32;
type c_ulonglong = u64;
type ssize_t = isize;
type size_t = usize;
type pid_t = i32;

const AF_UNIX: c_int = 1;
const SOCK_STREAM: c_int = 1;
const O_NONBLOCK: c_int = 0o4000;

const POLLIN: c_short = 0x001;
const POLLHUP: c_short = 0x010;

const EPOLLIN:  c_uint = 0x001;
const EPOLLOUT: c_uint = 0x004;
const EPOLLHUP: c_uint = 0x010;
const EPOLL_CTL_ADD: c_int = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct pollfd {
    pub fd: c_int,
    pub events: c_short,
    pub revents: c_short,
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

/// A 1024-bit (FD_SETSIZE) fd_set. Binary-compatible with relibc's
/// `cbitset`-backed `fd_set`: both are just a flat little-endian bitmap, so
/// a raw byte array manipulated with `fd/8`/`fd%8` indexing matches
/// regardless of the internal Rust wrapper type on either side.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct fd_set {
    pub bits: [u8; 128],
}
impl fd_set {
    fn zeroed() -> Self { Self { bits: [0u8; 128] } }
    fn set(&mut self, fd: usize) { self.bits[fd / 8] |= 1 << (fd % 8); }
    fn is_set(&self, fd: usize) -> bool { self.bits[fd / 8] & (1 << (fd % 8)) != 0 }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timeval { pub tv_sec: i64, pub tv_usec: i64 }

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec { pub tv_sec: i64, pub tv_nsec: i64 }
const CLOCK_MONOTONIC: c_int = 1;

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

    pub fn pipe(fildes: *mut c_int) -> c_int;
    pub fn pipe2(fildes: *mut c_int, flags: c_int) -> c_int;
    pub fn dup(fildes: c_int) -> c_int;
    pub fn dup2(fildes: c_int, fildes2: c_int) -> c_int;

    pub fn socketpair(domain: c_int, kind: c_int, protocol: c_int, sv: *mut c_int) -> c_int;
    pub fn send(socket: c_int, buf: *const c_void, len: size_t, flags: c_int) -> ssize_t;
    pub fn recv(socket: c_int, buf: *mut c_void, len: size_t, flags: c_int) -> ssize_t;

    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn select(
        nfds: c_int, readfds: *mut fd_set, writefds: *mut fd_set,
        exceptfds: *mut fd_set, timeout: *mut timeval,
    ) -> c_int;

    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;
    pub fn clock_gettime(clk: c_int, tp: *mut timespec) -> c_int;

    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn _exit(status: c_int) -> !;
    pub fn fcntl(fildes: c_int, cmd: c_int, ...) -> c_int;
    pub fn syscall(sysno: i64, ...) -> i64;
    pub fn open(path: *const u8, flags: c_int, ...) -> c_int;
    pub fn unlink(path: *const u8) -> c_int;
    pub fn sendmsg(socket: c_int, msg: *const msghdr, flags: c_int) -> ssize_t;
    pub fn recvmsg(socket: c_int, msg: *mut msghdr, flags: c_int) -> ssize_t;
}

#[repr(C)]
pub struct iovec { pub iov_base: *mut c_void, pub iov_len: size_t }

#[repr(C)]
pub struct msghdr {
    pub msg_name: *mut c_void,
    pub msg_namelen: u32,
    pub msg_iov: *mut iovec,
    pub msg_iovlen: size_t,
    pub msg_control: *mut c_void,
    pub msg_controllen: size_t,
    pub msg_flags: c_int,
}

#[repr(C)]
pub struct cmsghdr { pub cmsg_len: size_t, pub cmsg_level: c_int, pub cmsg_type: c_int }

// ── Assembly entry point (identical to timertest's) ──────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset poll_main",
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
    "   adrp x1, poll_main",
    "   add x1, x1, :lo12:poll_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn poll_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    if !test_pipe_epoll_no_false_positive() { failures += 1; }
    if !test_pipe_epoll_pollout_reflects_ring_full() { failures += 1; }
    if !test_poll_and_select_match_real_pipe_readiness() { failures += 1; }
    if !test_socketpair_epoll_readiness_and_real_recv() { failures += 1; }
    if !test_epoll_wait_times_out_then_sees_write() { failures += 1; }
    if !test_pipe_hup_reflects_writer_refcount() { failures += 1; }
    if !test_poll_timeout_wake_latency() { failures += 1; }
    if !test_epoll_close_drops_registration() { failures += 1; }
    if !test_epoll_fork_child_keeps_registration() { failures += 1; }
    if !test_epoll_dup_keeps_registration() { failures += 1; }
    if !test_epoll_scm_rights_keeps_registration() { failures += 1; }
    if !test_close_range_keeps_forked_registration() { failures += 1; }
    if !test_close_range_cloexec_and_all_kinds() { failures += 1; }
    if !test_epoll_ctl_errno() { failures += 1; }
    if !test_epoll_fork_shared_instance() { failures += 1; }
    if !test_epoll_fork_child_close_keeps_parent() { failures += 1; }
    if !test_epoll_fork_cloexec_per_table() { failures += 1; }

    puts(b"--- polltest done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── Helpers ────────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr(), 7);
    } else {
        write(1, b": FAIL\n".as_ptr(), 7);
    }
    passed
}

unsafe fn new_pipe() -> (c_int, c_int) {
    let mut fds = [0i32; 2];
    assert_eq!(pipe(fds.as_mut_ptr()), 0);
    (fds[0], fds[1]) // (read_end, write_end)
}

// ── 1. epoll on a pipe: no false-positive readiness while empty ─────────────
//
// The pre-fix `probe_fd_events` unconditionally reported EPOLLIN/EPOLLOUT
// for anything requested, regardless of real fd state. This is the direct
// regression test: an epoll_wait(timeout=0) on an empty pipe's read end
// must report zero events, not a false EPOLLIN.

unsafe fn test_pipe_epoll_no_false_positive() -> bool {
    let name = b"pipe_epoll_no_false_positive\0";
    let (rfd, wfd) = new_pipe();

    let ep = epoll_create1(0);
    if ep < 0 { return report(name, false); }
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: rfd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev) != 0 { return report(name, false); }

    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n_empty = epoll_wait(ep, out.as_mut_ptr(), 4, 0);

    write(wfd, b"hello".as_ptr(), 5);
    let n_ready = epoll_wait(ep, out.as_mut_ptr(), 4, 200);
    let got_in = n_ready == 1 && (out[0].events & EPOLLIN) != 0;

    let mut buf = [0u8; 5];
    let n_read = read(rfd, buf.as_mut_ptr(), 5);
    let data_ok = n_read == 5 && &buf == b"hello";

    close(wfd);
    let n_eof = epoll_wait(ep, out.as_mut_ptr(), 4, 200);
    let eof_ok = n_eof == 1 && (out[0].events & (EPOLLIN | EPOLLHUP)) != 0;

    close(rfd);
    close(ep);

    report(name, n_empty == 0 && got_in && data_ok && eof_ok)
}

// ── 2. epoll POLLOUT reflects real ring occupancy, not "always writable" ────
//
// Fills the pipe's ring completely via short writes until EAGAIN, then
// checks epoll reports NOT writable — the pre-fix code always reported
// EPOLLOUT regardless of ring state. Also checks the PIPE_BUF threshold:
// draining a few bytes must not flip POLLOUT back on; only freeing a full
// PIPE_BUF (4096) does. See the PIPE_BUF comment below for the Linux
// ground-truth measurement this mirrors.

unsafe fn test_pipe_epoll_pollout_reflects_ring_full() -> bool {
    let name = b"pipe_epoll_pollout_reflects_ring_full\0";
    // Non-blocking both ends: the kernel's pipe ring (PIPE_RING_SIZE) is larger
    // than any single write below, so "full" is reached by writing until the
    // write end reports EAGAIN, not by a hardcoded byte count. Filling to a
    // fixed size silently stopped filling the ring the moment PIPE_RING_SIZE
    // grew past that number, which is what made this test fail. O_NONBLOCK keeps
    // the last (would-block) write from hanging once the ring is full.
    let mut fds = [0i32; 2];
    if pipe2(fds.as_mut_ptr(), O_NONBLOCK) != 0 { return report(name, false); }
    let (rfd, wfd) = (fds[0], fds[1]);

    let big = [0u8; 8192];
    let mut total = 0usize;
    loop {
        let n = write(wfd, big.as_ptr(), 8192);
        if n <= 0 { break; } // EAGAIN: ring is now full
        total += n as usize;
        if total > (1 << 20) { break; } // safety valve; a real ring is far smaller
    }
    let filled = total > 0;

    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLOUT, data: epoll_data { fd: wfd } };
    epoll_ctl(ep, EPOLL_CTL_ADD, wfd, &mut ev);

    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n_full = epoll_wait(ep, out.as_mut_ptr(), 4, 0);

    // POSIX PIPE_BUF: writes of at most this many bytes are atomic, and it's
    // also Linux's POLLOUT threshold — the write end only becomes writable
    // once at least PIPE_BUF (4096) bytes are free, not merely >0. Verified
    // against real Linux (5.x, glibc) on x86_64: filling a pipe to EAGAIN and
    // draining 256 bytes leaves poll()/epoll_wait() reporting NOT writable;
    // draining up to exactly 4095 bytes free still reports NOT writable;
    // crossing to 4096 bytes free is the first point POLLOUT/EPOLLOUT appears.
    // A small drain must therefore NOT reappear as writable — only a drain
    // that frees a full PIPE_BUF does.
    const PIPE_BUF: usize = 4096;

    // Small drain (well under PIPE_BUF): must stay NOT writable.
    let mut small_drain = [0u8; 256];
    let n_small_drained = read(rfd, small_drain.as_mut_ptr(), 256);
    let n_after_small_drain = epoll_wait(ep, out.as_mut_ptr(), 4, 50);
    let still_not_writable = n_after_small_drain == 0;

    // Drain the rest of a full PIPE_BUF worth of free space; writability
    // must now reappear.
    let mut rest_drain = [0u8; PIPE_BUF];
    let n_rest_drained = read(rfd, rest_drain.as_mut_ptr(), PIPE_BUF - 256);
    let n_after_full_drain = epoll_wait(ep, out.as_mut_ptr(), 4, 200);
    let writable_again = n_after_full_drain == 1 && (out[0].events & EPOLLOUT) != 0;

    close(rfd);
    close(wfd);
    close(ep);

    report(name, filled && n_full == 0 && n_small_drained > 0 && still_not_writable
        && n_rest_drained > 0 && writable_again)
}

// ── 3. Real poll()/select() (layered on epoll in relibc) see real readiness ─

unsafe fn test_poll_and_select_match_real_pipe_readiness() -> bool {
    let name = b"poll_and_select_match_real_pipe_readiness\0";
    let (rfd, wfd) = new_pipe();

    let mut pfd = pollfd { fd: rfd, events: POLLIN, revents: 0 };
    let n_empty = poll(&mut pfd, 1, 50);
    let empty_had_no_pollin = pfd.revents & POLLIN == 0;

    write(wfd, b"abc".as_ptr(), 3);
    pfd.revents = 0;
    let n_ready = poll(&mut pfd, 1, 200);
    let poll_saw_data = n_ready == 1 && (pfd.revents & POLLIN) != 0;

    let mut buf = [0u8; 3];
    read(rfd, buf.as_mut_ptr(), 3);

    // select(): empty again — must time out with nothing set.
    let mut rset = fd_set::zeroed();
    rset.set(rfd as usize);
    let mut tv = timeval { tv_sec: 0, tv_usec: 50_000 };
    let n_sel_empty = select(rfd + 1, &mut rset, core::ptr::null_mut(), core::ptr::null_mut(), &mut tv);
    let sel_empty_ok = n_sel_empty == 0 && !rset.is_set(rfd as usize);

    write(wfd, b"xyz".as_ptr(), 3);
    let mut rset2 = fd_set::zeroed();
    rset2.set(rfd as usize);
    let mut tv2 = timeval { tv_sec: 0, tv_usec: 200_000 };
    let n_sel_ready = select(rfd + 1, &mut rset2, core::ptr::null_mut(), core::ptr::null_mut(), &mut tv2);
    let sel_ready_ok = n_sel_ready == 1 && rset2.is_set(rfd as usize);

    close(rfd);
    close(wfd);

    report(name, n_empty == 0 && empty_had_no_pollin && poll_saw_data
        && sel_empty_ok && sel_ready_ok)
}

// ── 4. socketpair: epoll readiness matches real recv() data, not fake EOF ───
//
// The bug class this guards against: if epoll falsely reports EPOLLIN on an
// empty-but-open socket, a non-blocking recv() returns 0 — indistinguishable
// from a real EOF/close to the caller. This checks epoll only ever reports
// EPOLLIN once real data (or a real close) is present.

unsafe fn test_socketpair_epoll_readiness_and_real_recv() -> bool {
    let name = b"socketpair_epoll_readiness_and_real_recv\0";
    let mut sv = [0i32; 2];
    if socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let (a, b) = (sv[0], sv[1]);

    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: a } };
    epoll_ctl(ep, EPOLL_CTL_ADD, a, &mut ev);

    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n_empty = epoll_wait(ep, out.as_mut_ptr(), 4, 0);

    send(b, b"ping".as_ptr() as *const c_void, 4, 0);
    let n_ready = epoll_wait(ep, out.as_mut_ptr(), 4, 200);
    let saw_in = n_ready == 1 && (out[0].events & EPOLLIN) != 0;

    let mut buf = [0u8; 4];
    let n_recv = recv(a, buf.as_mut_ptr() as *mut c_void, 4, 0);
    let real_data = n_recv == 4 && &buf == b"ping"; // not the false-EOF (0) a fake-ready bug would produce

    close(b);
    let n_hup = epoll_wait(ep, out.as_mut_ptr(), 4, 200);
    let saw_hup = n_hup == 1 && (out[0].events & (EPOLLIN | EPOLLHUP)) != 0;
    let n_eof = recv(a, buf.as_mut_ptr() as *mut c_void, 4, 0);

    close(a);
    close(ep);

    report(name, n_empty == 0 && saw_in && real_data && saw_hup && n_eof == 0)
}

// ── epoll registrations die with the file (lane termsegv, 2026-10-01) ─────
//
// Linux removes an epoll item when its file is closed: a closed fd never
// reports anything again (there is no EPOLLNVAL), and a new file that lands on
// the same fd number is not registered. This kernel kept the interest keyed by
// number and reported POLLNVAL for the closed fd on every wait — which woke a
// cosmic-term worker thread parked on the Wayland socket its main thread had
// just disconnected, and that thread then crashed in libwayland. Covers close,
// dup2 onto a registered fd, and number reuse, for a socket and a pipe.

unsafe fn test_epoll_close_drops_registration() -> bool {
    let name = b"epoll_close_drops_registration\0";
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let ep = epoll_create1(0);

    // 1. close() of a registered socket: nothing to report afterwards.
    let mut sv = [0i32; 2];
    if socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: sv[0] } };
    epoll_ctl(ep, EPOLL_CTL_ADD, sv[0], &mut ev);
    close(sv[0]);
    let n_closed_sock = epoll_wait(ep, out.as_mut_ptr(), 4, 30);
    close(sv[1]);

    // 2. close() of a registered pipe end, then a new pipe reusing the number
    //    with data in it: still nothing (the new file was never added).
    let (r1, w1) = new_pipe();
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: r1 } };
    epoll_ctl(ep, EPOLL_CTL_ADD, r1, &mut ev);
    close(r1);
    let n_closed_pipe = epoll_wait(ep, out.as_mut_ptr(), 4, 30);
    let (r2, w2) = new_pipe();
    let moved = if r2 != r1 { dup2(r2, r1) == r1 } else { true };
    write(w2, b"x".as_ptr(), 1);
    let n_reused = epoll_wait(ep, out.as_mut_ptr(), 4, 30);

    // 3. dup2() onto a registered fd closes the file that was there: the
    //    registration goes with it even though the number stays open.
    let (r3, w3) = new_pipe();
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: r3 } };
    epoll_ctl(ep, EPOLL_CTL_ADD, r3, &mut ev);
    let replaced = dup2(r1, r3) == r3; // r1 now names the readable pipe 2
    let n_dup2 = epoll_wait(ep, out.as_mut_ptr(), 4, 30);

    // 4. Sanity: the same instance still reports a live registration.
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: r1 } };
    epoll_ctl(ep, EPOLL_CTL_ADD, r1, &mut ev);
    let n_live = epoll_wait(ep, out.as_mut_ptr(), 4, 30);

    for fd in [w1, r1, w2, r3, w3, ep] { close(fd); }
    if r2 != r1 { close(r2); }
    let ok = n_closed_sock == 0 && n_closed_pipe == 0 && moved && n_reused == 0
        && replaced && n_dup2 == 0 && n_live == 1;
    if !ok {
        puts(b"  (expected 0 0 0 0 1 after close/close/reuse/dup2/live)\0".as_ptr());
        let mut b = [b' '; 16];
        for (i, v) in [n_closed_sock, n_closed_pipe, n_reused, n_dup2, n_live].iter().enumerate() {
            b[i * 2] = b'0' + (*v).clamp(0, 9) as u8;
        }
        b[15] = 0;
        puts(b.as_ptr());
    }
    report(name, ok)
}

// ── Epoll items live as long as the open file description ──────────────────
//
// Linux keys an epoll item by (open file description, fd number) and removes
// it only when the description's LAST reference is gone — a dup, a forked
// child's copy or an SCM_RIGHTS-received copy keeps it reporting after the
// registered fd itself is closed. Every expectation below was first checked
// with the same program in C on Linux 7.2 (lane epollofd, 2026-10-01).

const EPOLL_CTL_DEL: c_int = 2;
const F_GETFD: c_int = 1;
const FD_CLOEXEC: c_int = 1;
const SYS_CLOSE_RANGE: i64 = 436;
const CLOSE_RANGE_UNSHARE: u32 = 1 << 1;
const CLOSE_RANGE_CLOEXEC: u32 = 1 << 2;

unsafe fn ep_add(ep: c_int, fd: c_int, data: u64) -> c_int {
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { u64: data } };
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &mut ev)
}

/// epoll_wait(30 ms) → (n, the data words of the events).
unsafe fn ep_wait(ep: c_int) -> (c_int, [u64; 4]) {
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, 30);
    let mut d = [0u64; 4];
    for i in 0..(n.max(0) as usize).min(4) { d[i] = out[i].data.u64; }
    (n, d)
}

unsafe fn print_nums(label: &[u8], v: &[i64]) {
    write(1, label.as_ptr(), label.len());
    for x in v {
        write(1, b" ".as_ptr(), 1);
        if *x < 0 { write(1, b"-".as_ptr(), 1); }
        print_num(x.unsigned_abs());
    }
    write(1, b"\n".as_ptr(), 1);
}

/// A child that holds every inherited fd until a byte arrives on `go_r`.
unsafe fn fork_holder(go_r: c_int, go_w: c_int) -> pid_t {
    let c = fork();
    if c == 0 {
        close(go_w);
        let mut b = 0u8;
        read(go_r, &mut b, 1);
        _exit(0);
    }
    close(go_r);
    c
}

unsafe fn test_epoll_fork_child_keeps_registration() -> bool {
    let name = b"epoll_fork_child_keeps_registration\0";
    let (r, w) = new_pipe();
    let (go_r, go_w) = new_pipe();
    let ep = epoll_create1(0);
    ep_add(ep, r, 0x77);
    let child = fork_holder(go_r, go_w);
    close(r);                                    // the child still holds the description
    write(w, b"x".as_ptr(), 1);
    let (n1, d1) = ep_wait(ep);
    // A new readable pipe on the same number, registered too: a second item.
    let (q_r, q_w) = new_pipe();
    if q_r != r { dup2(q_r, r); close(q_r); }
    let add = ep_add(ep, r, 0x88);
    write(q_w, b"y".as_ptr(), 1);
    let (n2, d2) = ep_wait(ep);
    let both = d2[..2].contains(&0x77) && d2[..2].contains(&0x88);
    // DEL by number resolves to the file the number names now: the new item.
    let del = epoll_ctl(ep, EPOLL_CTL_DEL, r, core::ptr::null_mut());
    let (n3, d3) = ep_wait(ep);
    write(go_w, b"g".as_ptr(), 1);               // last reference to the old pipe goes
    waitpid(child, core::ptr::null_mut(), 0);
    let (n4, _) = ep_wait(ep);
    print_nums(b"  fork: n1 n2 add del n3 n4 =", &[n1 as i64, n2 as i64, add as i64, del as i64, n3 as i64, n4 as i64]);
    for fd in [r, q_w, w, go_w, ep] { close(fd); }
    report(name, n1 == 1 && d1[0] == 0x77 && add == 0 && n2 == 2 && both
        && del == 0 && n3 == 1 && d3[0] == 0x77 && n4 == 0)
}

unsafe fn test_epoll_dup_keeps_registration() -> bool {
    let name = b"epoll_dup_keeps_registration\0";
    let (r, w) = new_pipe();
    let ep = epoll_create1(0);
    ep_add(ep, r, 0x55);
    let d = dup(r);
    close(r);
    write(w, b"x".as_ptr(), 1);
    let (n1, d1) = ep_wait(ep);
    close(d);
    let (n2, _) = ep_wait(ep);
    print_nums(b"  dup: n1 n2 =", &[n1 as i64, n2 as i64]);
    close(w); close(ep);
    report(name, d >= 0 && n1 == 1 && d1[0] == 0x55 && n2 == 0)
}

unsafe fn send_fd(sock: c_int, fd: c_int) -> ssize_t {
    let mut c = b'f';
    let mut iov = iovec { iov_base: &mut c as *mut u8 as *mut c_void, iov_len: 1 };
    let mut buf = [0u64; 4];
    let cm = buf.as_mut_ptr() as *mut cmsghdr;
    (*cm).cmsg_len = core::mem::size_of::<cmsghdr>() + 4;
    (*cm).cmsg_level = 1; // SOL_SOCKET
    (*cm).cmsg_type = 1;  // SCM_RIGHTS
    core::ptr::write_unaligned((cm as *mut u8).add(core::mem::size_of::<cmsghdr>()) as *mut c_int, fd);
    let m = msghdr { msg_name: core::ptr::null_mut(), msg_namelen: 0, msg_iov: &mut iov, msg_iovlen: 1,
        msg_control: buf.as_mut_ptr() as *mut c_void, msg_controllen: core::mem::size_of::<cmsghdr>() + 8,
        msg_flags: 0 };
    sendmsg(sock, &m, 0)
}

unsafe fn recv_fd(sock: c_int) -> c_int {
    let mut c = 0u8;
    let mut iov = iovec { iov_base: &mut c as *mut u8 as *mut c_void, iov_len: 1 };
    let mut buf = [0u64; 4];
    let mut m = msghdr { msg_name: core::ptr::null_mut(), msg_namelen: 0, msg_iov: &mut iov, msg_iovlen: 1,
        msg_control: buf.as_mut_ptr() as *mut c_void, msg_controllen: 32, msg_flags: 0 };
    if recvmsg(sock, &mut m, 0) != 1 || m.msg_controllen < core::mem::size_of::<cmsghdr>() + 4 { return -1; }
    let cm = buf.as_ptr() as *const cmsghdr;
    if (*cm).cmsg_type != 1 { return -1; }
    core::ptr::read_unaligned((cm as *const u8).add(core::mem::size_of::<cmsghdr>()) as *const c_int)
}

// Received over SCM_RIGHTS, the description keeps the sender's item alive —
// and while the fd is still queued in the socket (no fd names it), Linux
// reports it too (lane epollerr: probed through the queued descriptor).
unsafe fn test_epoll_scm_rights_keeps_registration() -> bool {
    let name = b"epoll_scm_rights_keeps_registration\0";
    let mut sv = [0i32; 2];
    if socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let (r, w) = new_pipe();
    let ep = epoll_create1(0);
    ep_add(ep, r, 0x66);
    let sent = send_fd(sv[0], r);
    close(r);
    write(w, b"x".as_ptr(), 1);
    let (n0, d0) = ep_wait(ep);                  // in flight: no fd names it
    let r2 = recv_fd(sv[1]);
    let (n1, d1) = ep_wait(ep);
    close(r2);
    let (n2, _) = ep_wait(ep);
    print_nums(b"  scm: sent inflight r2 n1 n2 =", &[sent as i64, n0 as i64, r2 as i64, n1 as i64, n2 as i64]);
    for fd in [w, sv[0], sv[1], ep] { close(fd); }
    report(name, sent == 1 && n0 == 1 && d0[0] == 0x66 && r2 >= 0 && n1 == 1 && d1[0] == 0x66 && n2 == 0)
}

unsafe fn test_close_range_keeps_forked_registration() -> bool {
    let name = b"close_range_keeps_forked_registration\0";
    let (r, w) = new_pipe();
    let (go_r, go_w) = new_pipe();
    let ep = epoll_create1(0);
    ep_add(ep, r, 0x99);
    let child = fork_holder(go_r, go_w);
    let rc = syscall(SYS_CLOSE_RANGE, r as i64, r as i64, 0i64);
    write(w, b"x".as_ptr(), 1);
    let (n1, d1) = ep_wait(ep);
    write(go_w, b"g".as_ptr(), 1);
    waitpid(child, core::ptr::null_mut(), 0);
    let (n2, _) = ep_wait(ep);
    print_nums(b"  close_range_epoll: rc n1 n2 =", &[rc, n1 as i64, n2 as i64]);
    for fd in [w, go_w, ep] { close(fd); }
    report(name, rc == 0 && n1 == 1 && d1[0] == 0x99 && n2 == 0)
}

// close_range covers every fd kind (sockets and epoll fds sit in their own
// number ranges here and used to be skipped), CLOSE_RANGE_CLOEXEC marks
// instead of closing, CLOSE_RANGE_UNSHARE is accepted, and bad arguments are
// EINVAL.
unsafe fn test_close_range_cloexec_and_all_kinds() -> bool {
    let name = b"close_range_cloexec_and_all_kinds\0";
    let (r, w) = new_pipe();
    let mut sv = [0i32; 2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr());
    let ep = epoll_create1(0);
    let all = [r, w, sv[0], sv[1], ep];
    let lo = *all.iter().min().unwrap() as i64;
    let hi = *all.iter().max().unwrap() as i64;
    let rc = syscall(SYS_CLOSE_RANGE, lo, hi, CLOSE_RANGE_CLOEXEC as i64);
    let marked = all.iter().filter(|&&fd| fcntl(fd, F_GETFD) == FD_CLOEXEC).count();
    let rc2 = syscall(SYS_CLOSE_RANGE, lo, hi, CLOSE_RANGE_UNSHARE as i64);
    let closed = all.iter().filter(|&&fd| fcntl(fd, F_GETFD) == -1).count();
    let bad_order = syscall(SYS_CLOSE_RANGE, 5i64, 4i64, 0i64);
    let bad_flag = syscall(SYS_CLOSE_RANGE, 3i64, 4i64, 1i64 << 7);
    print_nums(b"  close_range: rc marked rc2 closed bad_order bad_flag =",
        &[rc, marked as i64, rc2, closed as i64, bad_order, bad_flag]);
    // relibc's syscall() returns the raw -errno: EINVAL is -22.
    report(name, rc == 0 && marked == 5 && rc2 == 0 && closed == 5 && bad_order == -22 && bad_flag == -22)
}

// ── epoll_ctl's errors, and epoll fds shared across fork ────────────────────
//
// Every expectation below was first checked with the same program in C on
// Linux 7.2 (lane epollerr, 2026-10-02): ADD of an existing (fd, file) is
// EEXIST, MOD/DEL of a missing one ENOENT, ADD of the instance itself or
// through a non-epoll fd EINVAL, ADD of a closed fd EBADF, ADD of a regular
// file or directory EPERM. An epoll fd is an open file description: a fork
// child's copy names the same instance (one interest list), each copy has its
// own FD_CLOEXEC, and the instance lives until the last copy is closed.

#[cfg(target_arch = "x86_64")]
const SYS_EPOLL_CTL: i64 = 233;
#[cfg(not(target_arch = "x86_64"))]
const SYS_EPOLL_CTL: i64 = 21;
const EPOLL_CTL_MOD: c_int = 3;
const F_SETFD: c_int = 2;

/// Raw epoll_ctl: 0 or -errno.
unsafe fn ctl(ep: c_int, op: c_int, fd: c_int, data: u64) -> i64 {
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { u64: data } };
    syscall(SYS_EPOLL_CTL, ep as i64, op as i64, fd as i64, &mut ev as *mut epoll_event as i64)
}

unsafe fn ep_wait_ms(ep: c_int, ms: c_int) -> (c_int, [u64; 4]) {
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, ms);
    let mut d = [0u64; 4];
    for i in 0..(n.max(0) as usize).min(4) { d[i] = out[i].data.u64; }
    (n, d)
}

unsafe fn test_epoll_ctl_errno() -> bool {
    let name = b"epoll_ctl_errno\0";
    let (r, w) = new_pipe();
    let ep = epoll_create1(0);
    let a1 = ctl(ep, EPOLL_CTL_ADD, r, 1);
    let a2 = ctl(ep, EPOLL_CTL_ADD, r, 2);
    let m1 = ctl(ep, EPOLL_CTL_MOD, w, 3);
    let d1 = ctl(ep, EPOLL_CTL_DEL, w, 0);
    let slf = ctl(ep, EPOLL_CTL_ADD, ep, 4);
    let f = open(b"/tmp/epollerr-reg\0".as_ptr(), 0o102 /* O_CREAT|O_RDWR */, 0o600);
    let reg = ctl(ep, EPOLL_CTL_ADD, f, 5);
    let dir = open(b"/\0".as_ptr(), 0);
    let rdir = ctl(ep, EPOLL_CTL_ADD, dir, 6);
    let bad = ctl(ep, EPOLL_CTL_ADD, 999, 7);
    let notep = ctl(w, EPOLL_CTL_ADD, r, 8);
    let badop = ctl(ep, 77, r, 9);
    let d2 = ctl(ep, EPOLL_CTL_DEL, r, 0);
    let d3 = ctl(ep, EPOLL_CTL_DEL, r, 0);
    let m2 = ctl(ep, EPOLL_CTL_MOD, r, 0);
    let a3 = ctl(ep, EPOLL_CTL_ADD, r, 10);
    let null = open(b"/dev/null\0".as_ptr(), 2);
    let rnull = ctl(ep, EPOLL_CTL_ADD, null, 11);
    print_nums(b"  errno: add add2 modnone delnone self reg dir badfd notep badop del del2 mod readd devnull =",
        &[a1, a2, m1, d1, slf, reg, rdir, bad, notep, badop, d2, d3, m2, a3, rnull]);
    for fd in [f, dir, null, ep, r, w] { close(fd); }
    unlink(b"/tmp/epollerr-reg\0".as_ptr());
    // Linux: 0 -17 -2 -2 -22 -1 -1 -9 -22 -22 0 -2 -2 0 -1
    report(name, a1 == 0 && a2 == -17 && m1 == -2 && d1 == -2 && slf == -22
        && reg == -1 && rdir == -1 && bad == -9 && notep == -22 && badop == -22
        && d2 == 0 && d3 == -2 && m2 == -2 && a3 == 0 && rnull == -1)
}

unsafe fn test_epoll_fork_shared_instance() -> bool {
    let name = b"epoll_fork_shared_instance\0";
    let (p_r, p_w) = new_pipe();
    let (q_r, q_w) = new_pipe();
    let (go_r, go_w) = new_pipe();
    let (back_r, back_w) = new_pipe();
    let ep = epoll_create1(0);
    ctl(ep, EPOLL_CTL_ADD, p_r, 0x11);
    let c = fork();
    if c == 0 {
        let mut b = 0u8;
        let mut r: i32 = 0;
        read(go_r, &mut b, 1);
        // 1: the parent's registration and readiness, through the child's copy
        let (n, d) = ep_wait_ms(ep, 1000);
        if n == 1 && d[0] == 0x11 { r |= 1; }
        // 2: an ADD from the child lands in the shared interest list
        if ctl(ep, EPOLL_CTL_ADD, q_r, 0x22) == 0 { r |= 2; }
        // 3: the same (file, fd) from the child: EEXIST
        if ctl(ep, EPOLL_CTL_ADD, p_r, 0x33) == -17 { r |= 4; }
        // 4: a child-only readable pipe
        let (c_r, c_w) = new_pipe();
        write(c_w, b"z".as_ptr(), 1);
        if ctl(ep, EPOLL_CTL_ADD, c_r, 0x44) == 0 { r |= 8; }
        // 5: the child's copy of p_r goes; the parent's item stays
        close(p_r);
        write(back_w, &r as *const i32 as *const u8, 4);
        read(go_r, &mut b, 1);                   // the parent closed its epoll fd
        let (n, d) = ep_wait_ms(ep, 1000);
        let r2: i32 = (n >= 1 && d[..(n as usize).min(4)].contains(&0x22)) as i32;
        write(back_w, &r2 as *const i32 as *const u8, 4);
        _exit(0);
    }
    write(p_w, b"x".as_ptr(), 1);
    write(go_w, b"g".as_ptr(), 1);
    let mut r: i32 = -1;
    read(back_r, &mut r as *mut i32 as *mut u8, 4);
    let (n, d) = ep_wait_ms(ep, 100);
    let has = |v: u64| d[..(n.max(0) as usize).min(4)].contains(&v);
    let (s11, s22, s44) = (has(0x11), has(0x22), has(0x44));
    write(q_w, b"q".as_ptr(), 1);
    let (n, d) = ep_wait_ms(ep, 100);
    let t22 = d[..(n.max(0) as usize).min(4)].contains(&0x22);
    let pdel = ctl(ep, EPOLL_CTL_DEL, q_r, 0);
    let padd = ctl(ep, EPOLL_CTL_ADD, q_r, 0x22);
    close(ep);                                   // the child's copy keeps the instance
    write(go_w, b"g".as_ptr(), 1);
    let mut r2: i32 = -1;
    read(back_r, &mut r2 as *mut i32 as *mut u8, 4);
    waitpid(c, core::ptr::null_mut(), 0);
    print_nums(b"  fork_shared: child p q cp q_after_write pdel padd child_after_parent_close =",
        &[r as i64, s11 as i64, s22 as i64, s44 as i64, t22 as i64, pdel, padd, r2 as i64]);
    for fd in [p_r, p_w, q_r, q_w, go_r, go_w, back_r, back_w] { close(fd); }
    // Linux: 15 1 0 1 1 0 0 1
    report(name, r == 15 && s11 && !s22 && s44 && t22 && pdel == 0 && padd == 0 && r2 == 1)
}

unsafe fn test_epoll_fork_child_close_keeps_parent() -> bool {
    let name = b"epoll_fork_child_close_keeps_parent\0";
    let (r, w) = new_pipe();
    let ep = epoll_create1(0);
    ctl(ep, EPOLL_CTL_ADD, r, 0x55);
    let c = fork();
    if c == 0 { close(ep); _exit(0); }
    waitpid(c, core::ptr::null_mut(), 0);
    write(w, b"x".as_ptr(), 1);
    let (n, d) = ep_wait_ms(ep, 100);
    let a = ctl(ep, EPOLL_CTL_ADD, r, 0x56);
    print_nums(b"  fork_lastref: n d readd =", &[n as i64, d[0] as i64, a]);
    for fd in [r, w, ep] { close(fd); }
    // Linux: 1 85 -17
    report(name, n == 1 && d[0] == 0x55 && a == -17)
}

unsafe fn test_epoll_fork_cloexec_per_table() -> bool {
    let name = b"epoll_fork_cloexec_per_table\0";
    let ep = epoll_create1(0x80000); // EPOLL_CLOEXEC
    let c = fork();
    if c == 0 { fcntl(ep, F_SETFD, 0); _exit(if fcntl(ep, F_GETFD) == 0 { 0 } else { 1 }); }
    let mut st = 0;
    waitpid(c, &mut st, 0);
    let pf = fcntl(ep, F_GETFD);
    let rc = (st >> 8) & 0xff;
    print_nums(b"  fork_cloexec: child_rc parent =", &[rc as i64, pf as i64]);
    close(ep);
    // Linux: 0 1
    report(name, rc == 0 && pf == FD_CLOEXEC)
}

// ── 5. epoll_wait honours its timeout: returns 0 when empty, then sees data ──
//
// Distinguishes a real blocking-with-timeout from the pre-fix code, which
// ignored the timeout argument entirely. On an empty pipe a bounded wait must
// actually elapse and return 0 (no false readiness, no hang); once a real
// write lands the very next wait must report EPOLLIN.

unsafe fn test_epoll_wait_times_out_then_sees_write() -> bool {
    let name = b"epoll_wait_times_out_then_sees_write\0";
    let (rfd, wfd) = new_pipe();

    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: rfd } };
    epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev);

    let mut out: [epoll_event; 4] = core::mem::zeroed();
    // Empty pipe, non-zero timeout: must return 0 after the wait, not readiness.
    let n_timeout = epoll_wait(ep, out.as_mut_ptr(), 4, 30);

    // After a real write, the next wait must observe EPOLLIN.
    write(wfd, b"z".as_ptr(), 1);
    let n_ready = epoll_wait(ep, out.as_mut_ptr(), 4, 2000);
    let saw_write = n_ready == 1 && (out[0].events & EPOLLIN) != 0;

    close(rfd);
    close(wfd);
    close(ep);

    report(name, n_timeout == 0 && saw_write)
}

// ── 6. Pipe hang-up reflects the WRITER refcount, not a single boolean ───────
//
// A read end must report POLLHUP only once EVERY write-end fd is gone. dup()
// gives a second fd on the write end; closing just one must NOT raise HUP.
// Before the fix the pipe tracked open-ness as a bool, so the first close()
// falsely signalled EOF/HUP — which is exactly what made poll/select/epoll
// misbehave for pipes shared across dup() and inherited across fork().

unsafe fn test_pipe_hup_reflects_writer_refcount() -> bool {
    let name = b"pipe_hup_reflects_writer_refcount\0";
    let (rfd, wfd) = new_pipe();
    let wfd2 = dup(wfd); // two fds now hold the write end
    if wfd2 < 0 { return report(name, false); }

    // Close one writer: the other keeps the pipe writable → no hangup yet.
    close(wfd);
    let mut pfd = pollfd { fd: rfd, events: POLLIN, revents: 0 };
    poll(&mut pfd, 1, 0);
    let no_hup_while_writer_open = pfd.revents & POLLHUP == 0;

    // Close the last writer: now the read end must see the hangup.
    close(wfd2);
    pfd.revents = 0;
    poll(&mut pfd, 1, 0);
    let hup_after_last_writer = pfd.revents & POLLHUP != 0;

    close(rfd);
    report(name, no_hup_while_writer_open && hup_after_last_writer)
}

// ── 7. Timed-poll wake latency: how late does a 10 ms poll() come back? ─────
//
// 1000 × poll(one idle pipe, 10 ms), each timed with CLOCK_MONOTONIC. The
// overshoot (elapsed − 10 ms) is what the kernel's poll-deadline tick adds on
// top of the timeout. The tick services timed waiters under a
// `RUN_QUEUE.try_lock()`; when that failed, the wake slipped by one tick per
// failure, so a 10 ms poll routinely came back at 20–30 ms with the desktop
// idle (the tick-aligned idle loops of the other CPUs held the lock at the
// very instant the tick tried it). The bound is two ticks of slack past the
// one-tick granularity of a tick-based deadline; p50/p99/max are printed so a
// regression shows as numbers, not just a FAIL.

unsafe fn print_num(v: u64) {
    let mut buf = [0u8; 20]; let mut i = 0; let mut x = v;
    if x == 0 { write(1, b"0".as_ptr(), 1); return; }
    while x > 0 { buf[i] = b'0' + (x % 10) as u8; x /= 10; i += 1; }
    while i > 0 { i -= 1; write(1, buf.as_ptr().add(i), 1); }
}

unsafe fn now_ns() -> u64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

unsafe fn test_poll_timeout_wake_latency() -> bool {
    let name = b"poll_timeout_wake_latency\0";
    const N: usize = 1000;
    const TIMEOUT_MS: u64 = 10;
    let (rfd, wfd) = new_pipe();
    static mut OVER_US: [u32; N] = [0; N];
    let mut early = 0usize;
    for i in 0..N {
        let mut pfd = pollfd { fd: rfd, events: POLLIN, revents: 0 };
        let t0 = now_ns();
        let r = poll(&mut pfd, 1, TIMEOUT_MS as c_int);
        let t1 = now_ns();
        if r != 0 { early += 1; } // an idle pipe never becomes readable
        let el = t1.saturating_sub(t0);
        let over = el.saturating_sub(TIMEOUT_MS * 1_000_000) / 1000;
        OVER_US[i] = over.min(u32::MAX as u64) as u32;
        // Deadlines are absolute and never early (POSIX "at least"): any
        // return before the full timeout has elapsed is a failure.
        if el < TIMEOUT_MS * 1_000_000 { early += 1; }
    }
    close(rfd); close(wfd);
    // Insertion sort: N is small and this is a no_std binary.
    for i in 1..N {
        let v = OVER_US[i]; let mut j = i;
        while j > 0 && OVER_US[j - 1] > v { OVER_US[j] = OVER_US[j - 1]; j -= 1; }
        OVER_US[j] = v;
    }
    let p50 = OVER_US[N / 2] as u64;
    let p90 = OVER_US[N * 9 / 10] as u64;
    let p99 = OVER_US[N * 99 / 100] as u64;
    let max = OVER_US[N - 1] as u64;
    write(1, b"poll 10ms x1000 overshoot us: p50=".as_ptr(), 34); print_num(p50);
    write(1, b" p90=".as_ptr(), 5); print_num(p90);
    write(1, b" p99=".as_ptr(), 5); print_num(p99);
    write(1, b" max=".as_ptr(), 5); print_num(max);
    write(1, b" early=".as_ptr(), 7); print_num(early as u64);
    write(1, b"\n".as_ptr(), 1);
    // Measured 2026-09-18 with the greeter desktop up: before the fix
    // p50=10 p90=9494 p99=21002 max=40445 (aarch64/HVF); after, p90 ≤ ~200
    // and p99 ≤ ~2.5 ms on HVF, p99 ≤ ~7.5 ms on x86_64/TCG. A tick-slip
    // regression puts p90 back at a full tick (10 ms) — the p90 bound is the
    // sharp one; the p99 bound is one tick of slack for the odd host-preempted
    // holder. 2026-09-24 (lane/polltimer): with never-early absolute deadlines
    // (lane/timespec) the wake was tick-granular and p90 sat at ~10.1 ms; a
    // one-shot timer armed to each deadline brings it to p50 ≈ 0.3 ms / p90
    // ≈ 1.3–1.8 ms on aarch64/HVF and p90 ≈ 0.26 ms on x86_64/TCG (steady state).
    report(name, early == 0 && p90 <= 5_000 && p99 <= 20_000)
}
