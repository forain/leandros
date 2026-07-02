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
    pub fn dup(fildes: c_int) -> c_int;

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
}

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
// Fills the pipe's 4096-byte ring completely via one short write, then
// checks epoll reports NOT writable — the pre-fix code always reported
// EPOLLOUT regardless of ring state.

unsafe fn test_pipe_epoll_pollout_reflects_ring_full() -> bool {
    let name = b"pipe_epoll_pollout_reflects_ring_full\0";
    let (rfd, wfd) = new_pipe();

    let big = [0u8; 8192];
    let mut total = 0usize;
    loop {
        let n = write(wfd, big.as_ptr(), 8192);
        if n <= 0 { break; }
        total += n as usize;
        if total >= 4096 { break; } // ring is 4096 bytes; this must be enough
    }
    let filled = total > 0 && total <= 4096;

    let ep = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLOUT, data: epoll_data { fd: wfd } };
    epoll_ctl(ep, EPOLL_CTL_ADD, wfd, &mut ev);

    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let n_full = epoll_wait(ep, out.as_mut_ptr(), 4, 0);

    // Drain some space, then confirm writability reappears.
    let mut drain = [0u8; 256];
    let n_drained = read(rfd, drain.as_mut_ptr(), 256);
    let n_after_drain = epoll_wait(ep, out.as_mut_ptr(), 4, 200);
    let writable_again = n_after_drain == 1 && (out[0].events & EPOLLOUT) != 0;

    close(rfd);
    close(wfd);
    close(ep);

    report(name, filled && n_full == 0 && n_drained > 0 && writable_again)
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
