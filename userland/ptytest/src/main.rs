//! ptytest — coverage for the `/dev/ptmx` + `/dev/pts/N` pair: the pool and
//! termios line discipline in `servers/tty/src/pty.rs`, the `VnodeKind::Pty`
//! vnode in `servers/vfs`, and the ioctl routing in `kernel/src/syscall.rs`.
//!
//! The suite comes in two halves, and they are deliberately different in kind.
//!
//! # Half one (1-6): the rings, single process
//!
//! Both ends are opened `O_NONBLOCK` on purpose. Everything a pty does is
//! synchronous — the write and the read that observes it are two syscalls into
//! the same in-kernel pool — so `EAGAIN` is a *result* here, not a race: it is
//! the only way to assert the negative half of canonical mode ("a partial line
//! is NOT readable"), which is the property the whole discipline exists for.
//! It also means no subtest can hang the boot if the discipline is wrong.
//!
//! # Half two (7-15): a real child on the slave
//!
//! Rings and ioctls working is not the same thing as a pty *being a terminal*.
//! Everything in half two forks a child that performs the exact sequence a
//! terminal emulator performs — `setsid`, `open("/dev/pts/N")`,
//! `ioctl(TIOCSCTTY)`, `dup2` onto 0/1/2 — because that sequence is what turns
//! the pair into a controlling terminal, and job-control signals have nowhere
//! to go until it has run. Half one could not have caught a single one of the
//! bugs half two found: with no child there is no session, so `pgrp` stayed 0
//! and every signal path was a no-op that returned success.
//!
//! Subtest 15 is the whole point of the file: it runs `brush`, the real shell,
//! on a slave and drives it from the master the way `cosmic-term` will. The
//! parent acts as the terminal emulator, which includes answering the ESC[6n
//! cursor-position query — on a pty nothing else can, and a line editor that
//! never gets a reply hangs until its timeout and then gives up on interactive
//! mode.
//!
//! Initializes via relibc_start_v1 (same as polltest/timertest) so TLS and
//! errno are set up.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL"; the final line is
//! `ptytest: <passed>/<total>` and the exit code is the failure count.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

type c_int = i32;
type c_ulong = u64;
type pid_t = i32;
type ssize_t = isize;
type size_t = usize;

const O_RDWR: c_int = 0o2;
const O_NOCTTY: c_int = 0o400;
const O_NONBLOCK: c_int = 0o4000;

const TCGETS: c_ulong = 0x5401;
const TCSETS: c_ulong = 0x5402;
const TCFLSH: c_ulong = 0x540B;
const TIOCSCTTY: c_ulong = 0x540E;
const TIOCGPGRP: c_ulong = 0x540F;
const TIOCGWINSZ: c_ulong = 0x5413;
const TIOCSWINSZ: c_ulong = 0x5414;
const TIOCGSID: c_ulong = 0x5429;
const TIOCGPTN: c_ulong = 0x8004_5430;
const TIOCSPTLCK: c_ulong = 0x4004_5431;
/// Not an `_IO*` encoding: `TIOCGPTPEER` is a bare number, takes open flags by
/// value and returns a file descriptor.
const TIOCGPTPEER: c_ulong = 0x5441;

const SIGHUP: c_int = 1;
const SIGINT: c_int = 2;
const SIGWINCH: c_int = 28;

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 0x01;
const MAP_ANONYMOUS: c_int = 0x20;

/// A signal that reaches its default action terminates through
/// `sched::exit_group_signal`, so `waitpid` reports the POSIX `WIFSIGNALED`
/// encoding: the signal number in the low 7 bits, exit-status byte zero.
///
/// Pinned to exactly that. This used to also accept a normal exit with code
/// `128 + signo` — what the kernel produced while it tracked no terminating
/// signal — and a test that accepts both encodings cannot fail if the kernel
/// regresses to the old one.
fn killed_by(status: c_int, signo: c_int) -> bool {
    wifsignaled(status) && wtermsig(status) == signo
}

fn wifsignaled(status: c_int) -> bool { ((status & 0x7f) + 1) >> 1 > 0 }
fn wtermsig(status: c_int) -> c_int { status & 0x7f }

/// TCFLSH argument: discard both queues. Passed by *value*, not by pointer.
const TCIOFLUSH: usize = 2;

// termios bits, Linux values.
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
const OPOST: u32 = 0x0001;
const ONLCR: u32 = 0x0004;
const VEOF: usize = 4;

/// Linux `struct termios`: the kernel reads/writes the first 36 bytes
/// (4 flag words, `c_line`, then 19 `c_cc` slots). The tail is padding so a
/// stray TCGETS2 (44 bytes) could never scribble past the end.
#[repr(C)]
#[derive(Clone, Copy)]
struct termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 19],
    _pad: [u8; 24],
}

impl termios {
    fn blank() -> Self {
        termios {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0u8; 19],
            _pad: [0u8; 24],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
struct winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
pub struct timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t;
    pub fn close(fd: c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, out: *mut c_void) -> c_int;
    pub fn exit(status: i32) -> !;
    pub fn __errno_location() -> *mut c_int;

    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn _exit(status: c_int) -> !;
    pub fn getpid() -> pid_t;
    pub fn setsid() -> pid_t;
    pub fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    pub fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> c_int;
    pub fn isatty(fd: c_int) -> c_int;
    pub fn ttyname(fd: c_int) -> *mut u8;
    pub fn signal(signum: c_int, handler: usize) -> usize;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int,
                fd: c_int, offset: i64) -> *mut c_void;
}

// ── Assembly entry point (identical to polltest's) ───────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset pty_main",
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
    "   adrp x1, pty_main",
    "   add x1, x1, :lo12:pty_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── Output helpers ───────────────────────────────────────────────────────────

unsafe fn out(s: &[u8]) {
    write(1, s.as_ptr() as *const c_void, s.len());
}

unsafe fn out_num(mut v: usize) {
    let mut d = [0u8; 20];
    let mut n = 0;
    if v == 0 {
        d[0] = b'0';
        n = 1;
    } else {
        while v > 0 {
            d[n] = b'0' + (v % 10) as u8;
            n += 1;
            v /= 10;
        }
        d[..n].reverse();
    }
    out(&d[..n]);
}

unsafe fn report(name: &[u8], passed: bool) -> bool {
    out(name);
    out(if passed { b": PASS\n" } else { b": FAIL\n" });
    passed
}

// ── PTY helpers ──────────────────────────────────────────────────────────────

/// Set the whole pair's termios from `t` (TCSETS on either end reaches the one
/// shared record — a pty has a single line discipline, not one per end).
unsafe fn set_termios(fd: c_int, t: &termios) -> bool {
    ioctl(fd, TCSETS, t as *const termios as *mut c_void) == 0
}

/// Drop whatever earlier subtests left in either queue, so each one starts from
/// a known-empty pair rather than inheriting the previous one's echo.
unsafe fn flush_both(fd: c_int) {
    ioctl(fd, TCFLSH, TCIOFLUSH as *mut c_void);
}

/// Read once; returns the byte count, or a negative value for EAGAIN/error.
unsafe fn try_read(fd: c_int, buf: &mut [u8]) -> ssize_t {
    read(fd, buf.as_mut_ptr() as *mut c_void, buf.len())
}

unsafe fn write_all(fd: c_int, data: &[u8]) -> bool {
    write(fd, data.as_ptr() as *const c_void, data.len()) == data.len() as ssize_t
}

// ── 1. ptmx allocation, TIOCGPTN, unlockpt, slave open ───────────────────────
//
// The whole openpty(3) opening sequence, in order. TIOCSPTLCK matters as more
// than a formality: a fresh pair is locked, so the slave open below is also a
// check that clearing the lock is what unblocked it.

/// `posix_openpt` + `TIOCGPTN` + `unlockpt`, i.e. everything a terminal
/// emulator does before it forks. Returns the master fd and the pty number.
unsafe fn open_master(nonblock: bool) -> Option<(c_int, usize)> {
    let flags = O_RDWR | O_NOCTTY | if nonblock { O_NONBLOCK } else { 0 };
    let m = open(b"/dev/ptmx\0".as_ptr(), flags);
    if m < 0 {
        return None;
    }
    let mut n: u32 = 0xFFFF_FFFF;
    if ioctl(m, TIOCGPTN, &mut n as *mut u32 as *mut c_void) != 0 || n > 99 {
        close(m);
        return None;
    }
    let mut unlock: i32 = 0;
    if ioctl(m, TIOCSPTLCK, &mut unlock as *mut i32 as *mut c_void) != 0 {
        close(m);
        return None;
    }
    Some((m, n as usize))
}

/// `ptsname(3)` without the static buffer: "/dev/pts/" + at most two digits.
fn pts_path(n: usize, out: &mut [u8; 16]) {
    out[..9].copy_from_slice(b"/dev/pts/");
    let mut p = 9;
    if n >= 10 {
        out[p] = b'0' + (n / 10) as u8;
        p += 1;
    }
    out[p] = b'0' + (n % 10) as u8;
    out[p + 1] = 0;
}

unsafe fn open_pair() -> Option<(c_int, c_int, usize)> {
    let (m, n) = open_master(true)?;
    let mut path = [0u8; 16];
    pts_path(n, &mut path);
    let s = open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK);
    if s < 0 {
        close(m);
        return None;
    }
    Some((m, s, n))
}

// ── 2. raw mode both directions, incl. ONLCR on the slave's output ───────────

unsafe fn test_raw_roundtrip(m: c_int, s: c_int) -> bool {
    let mut t = termios::blank();
    t.c_oflag = OPOST | ONLCR; // the slave's "\n" must reach the master as "\r\n"
    if !set_termios(m, &t) {
        return report(b"raw_roundtrip", false);
    }
    flush_both(m);

    // master → slave: raw mode delivers the bytes verbatim, immediately.
    if !write_all(m, b"hi") {
        return report(b"raw_roundtrip", false);
    }
    let mut buf = [0u8; 16];
    let n = try_read(s, &mut buf);
    if n != 2 || &buf[..2] != b"hi" {
        return report(b"raw_roundtrip", false);
    }

    // slave → master: OPOST|ONLCR turns the one "\n" into "\r\n", which is the
    // difference between a terminal emulator drawing lines and drawing a
    // staircase.
    if !write_all(s, b"a\n") {
        return report(b"raw_roundtrip", false);
    }
    let n = try_read(m, &mut buf);
    report(b"raw_roundtrip", n == 3 && &buf[..3] == b"a\r\n")
}

// ── 3. canonical mode: a line is invisible until its terminator ──────────────

unsafe fn test_canonical_line(m: c_int, s: c_int) -> bool {
    let mut t = termios::blank();
    t.c_lflag = ICANON; // no ECHO: this subtest is about the line, not the echo
    t.c_cc[VEOF] = 4;
    if !set_termios(m, &t) {
        return report(b"canonical_line", false);
    }
    flush_both(m);

    if !write_all(m, b"abc") {
        return report(b"canonical_line", false);
    }
    // The negative half, and the reason for O_NONBLOCK: "abc" is buffered in
    // the canonical accumulator, so it is not readable. A pty that hands over a
    // half-typed line breaks every line editor on it.
    let mut buf = [0u8; 16];
    if try_read(s, &mut buf) >= 0 {
        return report(b"canonical_line", false);
    }

    if !write_all(m, b"\n") {
        return report(b"canonical_line", false);
    }
    let n = try_read(s, &mut buf);
    // Exactly the line, terminator included — not more, even though nothing
    // else is queued, and not less.
    report(b"canonical_line", n == 4 && &buf[..4] == b"abc\n")
}

// ── 4. echo ──────────────────────────────────────────────────────────────────

unsafe fn test_echo(m: c_int, s: c_int) -> bool {
    let mut t = termios::blank();
    t.c_lflag = ECHO; // raw + echo: what a shell's own line editor asks for
    if !set_termios(m, &t) {
        return report(b"echo", false);
    }
    flush_both(m);

    if !write_all(m, b"x") {
        return report(b"echo", false);
    }
    // The echo comes back on the master without the slave doing anything at
    // all — it is generated by the input discipline, not by the program.
    let mut buf = [0u8; 16];
    let n = try_read(m, &mut buf);
    let echoed = n == 1 && buf[0] == b'x';

    // …and the byte still reached the slave.
    let n = try_read(s, &mut buf);
    report(b"echo", echoed && n == 1 && buf[0] == b'x')
}

// ── 5. window size round-trip ────────────────────────────────────────────────

unsafe fn test_winsize(m: c_int, s: c_int) -> bool {
    let want = winsize { ws_row: 40, ws_col: 100, ws_xpixel: 800, ws_ypixel: 640 };
    if ioctl(m, TIOCSWINSZ, &want as *const winsize as *mut c_void) != 0 {
        return report(b"winsize", false);
    }
    let mut got = winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    if ioctl(m, TIOCGWINSZ, &mut got as *mut winsize as *mut c_void) != 0 {
        return report(b"winsize", false);
    }
    if got != want {
        return report(b"winsize", false);
    }
    // The slave must see the same geometry: TIOCGWINSZ on the *slave* is what
    // every full-screen program actually calls.
    let mut from_slave = winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    if ioctl(s, TIOCGWINSZ, &mut from_slave as *mut winsize as *mut c_void) != 0 {
        return report(b"winsize", false);
    }
    report(b"winsize", from_slave == want)
}

// ── 6. hangup: the last master close is EOF for the slave ────────────────────

unsafe fn test_master_close_is_eof(m: c_int, s: c_int) -> bool {
    flush_both(m);
    if close(m) != 0 {
        return report(b"master_close_eof", false);
    }
    // 0, not EAGAIN: a shell on the slave exits on EOF. Reporting EAGAIN here
    // instead would leave it spinning on a terminal that will never speak
    // again.
    let mut buf = [0u8; 16];
    report(b"master_close_eof", try_read(s, &mut buf) == 0)
}

// ═════════════════════════════════════════════════════════════════════════════
// Half two: a real child on the slave
// ═════════════════════════════════════════════════════════════════════════════

/// Child→parent result slots, in a `MAP_SHARED` page. A pipe would need a
/// second fd pair per subtest and could itself block; the child's exit is the
/// synchronisation point, so a shared word read after `waitpid` needs no
/// ordering beyond that.
static mut SHARED: *mut u32 = core::ptr::null_mut();

unsafe fn shared_init() -> bool {
    let p = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE,
                 MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if p.is_null() || p as isize == -1 {
        return false;
    }
    SHARED = p as *mut u32;
    for i in 0..16 {
        core::ptr::write_volatile(SHARED.add(i), 0);
    }
    true
}

unsafe fn shared_set(slot: usize, v: u32) {
    core::ptr::write_volatile(SHARED.add(slot), v);
}

unsafe fn shared_get(slot: usize) -> u32 {
    core::ptr::read_volatile(SHARED.add(slot))
}

unsafe fn msleep(ms: i64) {
    let ts = timespec { tv_sec: ms / 1000, tv_nsec: (ms % 1000) * 1_000_000 };
    nanosleep(&ts, core::ptr::null_mut());
}

/// `login_tty(3)`, open-coded: the exact four steps between `fork` and
/// `execve` in every terminal emulator on earth. Returns the slave fd, or -1
/// naming nothing in particular — the child has no console to complain on, so
/// failures are reported through the shared page by the caller.
///
/// The order is not negotiable. `setsid` first, because `TIOCSCTTY` attaches
/// the terminal to the caller's *session* and a child that is still in its
/// parent's session would attach it to the parent's; and `O_NOCTTY` is absent
/// here for the same reason it is present everywhere else in this file — this
/// is the one open that is supposed to be about the controlling terminal.
unsafe fn child_take_ctty(n: usize) -> c_int {
    setsid();
    let mut path = [0u8; 16];
    pts_path(n, &mut path);
    let fd = open(path.as_ptr(), O_RDWR);
    if fd < 0 {
        return -1;
    }
    if ioctl(fd, TIOCSCTTY, core::ptr::null_mut()) != 0 {
        close(fd);
        return -1;
    }
    fd
}

/// `child_take_ctty` plus the `dup2` onto 0/1/2 that makes the pty the child's
/// stdio.
unsafe fn child_login_tty(n: usize) -> bool {
    let fd = child_take_ctty(n);
    if fd < 0 {
        return false;
    }
    dup2(fd, 0);
    dup2(fd, 1);
    dup2(fd, 2);
    if fd > 2 {
        close(fd);
    }
    true
}

/// Wait up to `ms` for `needle` to appear in what the master has produced.
/// Returns the total byte count accumulated in `acc`, or -1 on timeout.
///
/// Every wait in half two is bounded. A pty bug that loses a wakeup would
/// otherwise hang the boot rather than fail a subtest, and a hung boot tells
/// you nothing about which of twelve things broke.
unsafe fn expect(m: c_int, needle: &[u8], acc: &mut [u8], len: &mut usize, ms: i64) -> bool {
    let mut waited = 0i64;
    loop {
        let mut chunk = [0u8; 256];
        let n = read(m, chunk.as_mut_ptr() as *mut c_void, chunk.len());
        if n > 0 {
            for i in 0..n as usize {
                if *len < acc.len() {
                    acc[*len] = chunk[i];
                    *len += 1;
                }
            }
            if find(&acc[..*len], needle).is_some() {
                return true;
            }
            continue;
        }
        if find(&acc[..*len], needle).is_some() {
            return true;
        }
        if waited >= ms {
            return false;
        }
        msleep(10);
        waited += 10;
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn count_of(hay: &[u8], needle: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            n += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    n
}

// ── 7. setsid + TIOCSCTTY actually associates a session ──────────────────────
//
// The pool's `pgrp`/`sid` fields existed before this subtest and were never
// non-zero in a test run, which made every signal path below vacuously "pass".

unsafe fn test_ctty(m: c_int, n: usize) -> bool {
    let child = fork();
    if child == 0 {
        close(m);
        let fd = child_take_ctty(n);
        if fd < 0 {
            _exit(1);
        }
        let me = getpid() as u32;
        let mut fg: u32 = 0;
        let mut sid: u32 = 0;
        let got_fg = ioctl(fd, TIOCGPGRP, &mut fg as *mut u32 as *mut c_void) == 0;
        let got_sid = ioctl(fd, TIOCGSID, &mut sid as *mut u32 as *mut c_void) == 0;
        // isatty() IS TCGETS, and ttyname() additionally readlinks
        // /proc/self/fd/N and stats the result — the two together are how every
        // program decides "I am on a terminal, and it is that one".
        let tty = ttyname(fd);
        let named = !tty.is_null() && *tty.add(5) == b'p' && *tty.add(6) == b't';
        // isatty() reduces to exactly this call, so probe it directly too:
        // a TCGETS that fails is the whole of "this is not a terminal".
        let mut t = termios::blank();
        let tcgets = ioctl(fd, TCGETS, &mut t as *mut termios as *mut c_void) == 0;
        shared_set(0, (got_fg && got_sid && fg == me && sid == me
                       && tcgets && isatty(fd) == 1 && named) as u32);
        _exit(0);
    }
    let mut status: c_int = 0;
    waitpid(child, &mut status, 0);
    // The master's view has to agree: this is the query a terminal emulator
    // makes to find out which job it is talking to.
    let mut fg: u32 = 0;
    let master_fg = ioctl(m, TIOCGPGRP, &mut fg as *mut u32 as *mut c_void) == 0
        && fg == child as u32;
    report(b"ctty_setsid", shared_get(0) == 1 && master_fg)
}

// ── 8. the child's stdio really is the pty ───────────────────────────────────

unsafe fn test_child_stdio(m: c_int, n: usize) -> bool {
    let child = fork();
    if child == 0 {
        close(m);
        if !child_login_tty(n) {
            _exit(1);
        }
        write(1, b"hi\n".as_ptr() as *const c_void, 3);
        let mut buf = [0u8; 32];
        // A *blocking* canonical read on fd 0 — the shape of every read a shell
        // ever does, and the one thing half one could not exercise.
        let r = read(0, buf.as_mut_ptr() as *mut c_void, buf.len());
        shared_set(1, (r == 3 && &buf[..3] == b"go\n") as u32);
        _exit(0);
    }
    let mut acc = [0u8; 256];
    let mut len = 0usize;
    // ONLCR is on in the default termios, so the child's "\n" must arrive as
    // "\r\n" — the master side sees post-processed output, not raw bytes.
    let saw = expect(m, b"hi\r\n", &mut acc, &mut len, 5000);
    write_all(m, b"go\n");
    let mut status: c_int = 0;
    waitpid(child, &mut status, 0);
    report(b"child_stdio", saw && shared_get(1) == 1)
}

// ── 9. TIOCSWINSZ delivers SIGWINCH to the foreground group ──────────────────

static WINCH: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_winch(_sig: c_int) {
    WINCH.store(1, Ordering::SeqCst);
}

unsafe fn test_sigwinch(m: c_int, n: usize) -> bool {
    let child = fork();
    if child == 0 {
        close(m);
        if !child_login_tty(n) {
            _exit(1);
        }
        signal(SIGWINCH, on_winch as *const () as usize);
        write(1, b"R\n".as_ptr() as *const c_void, 2);
        let mut waited = 0;
        while WINCH.load(Ordering::SeqCst) == 0 && waited < 500 {
            msleep(10);
            waited += 1;
        }
        // The geometry the signal is *about* must already be readable when the
        // handler runs; a SIGWINCH that arrives before TIOCSWINSZ has landed
        // would make every full-screen program redraw at the old size.
        let mut ws = winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
        ioctl(0, TIOCGWINSZ, &mut ws as *mut winsize as *mut c_void);
        shared_set(2, (WINCH.load(Ordering::SeqCst) == 1
                       && ws.ws_row == 50 && ws.ws_col == 132) as u32);
        _exit(0);
    }
    let mut acc = [0u8; 128];
    let mut len = 0usize;
    let ready = expect(m, b"R\r\n", &mut acc, &mut len, 5000);
    let want = winsize { ws_row: 50, ws_col: 132, ws_xpixel: 0, ws_ypixel: 0 };
    ioctl(m, TIOCSWINSZ, &want as *const winsize as *mut c_void);
    let mut status: c_int = 0;
    waitpid(child, &mut status, 0);
    report(b"sigwinch", ready && shared_get(2) == 1)
}

// ── 10. ^C signals the foreground job and nothing else ───────────────────────

static PARENT_INT: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_int(_sig: c_int) {
    PARENT_INT.store(1, Ordering::SeqCst);
}

unsafe fn test_sigint(m: c_int, n: usize) -> bool {
    signal(SIGINT, on_int as *const () as usize);
    PARENT_INT.store(0, Ordering::SeqCst);
    let child = fork();
    if child == 0 {
        close(m);
        if !child_login_tty(n) {
            _exit(1);
        }
        write(1, b"R\n".as_ptr() as *const c_void, 2);
        // Nothing but the signal may end this child, so it must not read: a
        // read that returns EOF for an unrelated reason would look like a pass.
        for _ in 0..500 {
            msleep(10);
        }
        _exit(9);
    }
    let mut acc = [0u8; 128];
    let mut len = 0usize;
    let ready = expect(m, b"R\r\n", &mut acc, &mut len, 5000);
    write_all(m, b"\x03");
    let mut status: c_int = 0;
    waitpid(child, &mut status, 0);
    // The negative half: this process is in a different process group, and a
    // ^C that reached it would mean the signal went to whoever wrote to the
    // master rather than to the terminal's foreground job.
    report(b"sigint_fg_only",
           ready && killed_by(status, SIGINT) && PARENT_INT.load(Ordering::SeqCst) == 0)
}

// ── 11. the last master close hangs the session up ───────────────────────────

unsafe fn test_sighup(m: c_int, n: usize) -> bool {
    let child = fork();
    if child == 0 {
        // The inherited master fd MUST go: while any copy of it is open there
        // is, by definition, no hangup — which is exactly the bug a terminal
        // emulator that forgets this line ships.
        close(m);
        if !child_login_tty(n) {
            _exit(1);
        }
        write(1, b"R\n".as_ptr() as *const c_void, 2);
        for _ in 0..500 {
            msleep(10);
        }
        _exit(9);
    }
    let mut acc = [0u8; 128];
    let mut len = 0usize;
    let ready = expect(m, b"R\r\n", &mut acc, &mut len, 5000);
    close(m);
    let mut status: c_int = 0;
    waitpid(child, &mut status, 0);
    report(b"sighup_on_close", ready && killed_by(status, SIGHUP))
}

// ── 12. /dev/tty is the *controlling* terminal, not the machine console ──────
//
// The one path that cannot be reached without a session. `/dev/tty` resolved
// to the console proxy unconditionally, so a program under a terminal emulator
// that opened it — sudo, less, ssh, and crossterm whenever stdin is not usable
// — did not fail, it succeeded at the wrong terminal: reading the machine's
// keyboard and painting on the framebuffer while the emulator holding the
// master saw nothing.

unsafe fn test_devtty(m: c_int, n: usize) -> bool {
    let child = fork();
    if child == 0 {
        close(m);
        if !child_login_tty(n) {
            _exit(1);
        }
        let t = open(b"/dev/tty\0".as_ptr(), O_RDWR);
        if t < 0 {
            _exit(1);
        }
        // The assertion is not that this open succeeds — it always did — but
        // that what comes out of it arrives on the master.
        write(t, b"T\n".as_ptr() as *const c_void, 2);
        shared_set(3, (isatty(t) == 1) as u32);
        close(t);
        _exit(0);
    }
    let mut acc = [0u8; 128];
    let mut len = 0usize;
    let saw = expect(m, b"T\r\n", &mut acc, &mut len, 5000);
    let mut status: c_int = 0;
    waitpid(child, &mut status, 0);
    report(b"devtty_is_ctty", saw && shared_get(3) == 1)
}

// ── 13. the master's end-of-child is EIO, not EOF ────────────────────────────
//
// The distinction is not cosmetic. alacritty_terminal polls the master
// level-triggered and swallows EIO deliberately, waiting for SIGCHLD; a 0
// leaves the fd permanently readable-and-empty and it spins at 100% CPU. The
// second half of the check is the part that makes the first half safe: a
// *never-opened* slave must still be EAGAIN, or every pty would report its
// child dead in the window between fork and the child's open.

unsafe fn test_master_eio() -> bool {
    let (m, n) = match open_master(true) {
        Some(x) => x,
        None => return report(b"master_eof_is_eio", false),
    };
    let mut buf = [0u8; 8];
    // Before any slave has ever opened: not an error, just nothing yet.
    let before = try_read(m, &mut buf) == -1 && *__errno_location() == 11; // EAGAIN

    let mut path = [0u8; 16];
    pts_path(n, &mut path);
    let s = open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK);
    let opened = s >= 0;
    close(s);

    let after = try_read(m, &mut buf) == -1 && *__errno_location() == 5; // EIO
    close(m);
    report(b"master_eof_is_eio", before && opened && after)
}

// ── 14. TIOCGPTPEER ──────────────────────────────────────────────────────────
//
// rustix (and therefore cosmic-term, which bypasses libc entirely) opens every
// slave with this and only falls back to TIOCGPTN + open("/dev/pts/N") on
// ENOSYS or EPERM. The generic ENOTTY an unknown ioctl gets is neither, so
// before this existed the first thing a terminal tab did was fail.

unsafe fn test_gptpeer() -> bool {
    let (m, _n) = match open_master(true) {
        Some(x) => x,
        None => return report(b"tiocgptpeer", false),
    };
    // `arg` is open flags by value, not a pointer, and the result is a fd.
    let s = ioctl(m, TIOCGPTPEER, (O_RDWR | O_NOCTTY | O_NONBLOCK) as *mut c_void);
    if s < 3 {
        close(m);
        return report(b"tiocgptpeer", false);
    }
    // It has to be a working slave, not just a number: same pair, live rings.
    let mut t = termios::blank();
    t.c_oflag = OPOST | ONLCR;
    set_termios(m, &t);
    flush_both(m);
    let wrote = write_all(m, b"z");
    let mut buf = [0u8; 8];
    let got = try_read(s, &mut buf);
    // And it must be a terminal in its own right — isatty() on it is what
    // every program the shell launches will ask.
    let tty = isatty(s) == 1;
    close(s);
    close(m);
    report(b"tiocgptpeer", wrote && got == 1 && buf[0] == b'z' && tty)
}

// ── 15. brush, the real shell, driven from the master ────────────────────────
//
// This is the subtest the other eleven exist to support. It is also the only
// one that plays terminal emulator: `brush` runs reedline over crossterm,
// which opens `/dev/tty` for a raw-mode handle and writes a `CSI 6 n`
// cursor-position query to it. On the console the kernel answers that query
// itself from the framebuffer cursor; on a pty it cannot and must not — the
// program on the other end of the master is the terminal, and answering is its
// job. Nothing replies, reedline waits out its timeout and falls back, and the
// shell that comes up is not the one the user asked for.

const ACC_CAP: usize = 24576;
static mut ACC: [u8; ACC_CAP] = [0; ACC_CAP];

/// The accumulated master output as a slice. Single-threaded and the only
/// forked child here `execve`s immediately, so there is no second writer.
unsafe fn acc() -> &'static mut [u8] {
    core::slice::from_raw_parts_mut((&raw mut ACC) as *mut u8, ACC_CAP)
}

/// Read whatever the master has, append it to `ACC`, and answer any DSR in it.
/// Returns bytes read this pass (0 for "nothing yet", <0 for EOF/error).
unsafe fn pump(m: c_int, len: &mut usize, scanned: &mut usize) -> ssize_t {
    let mut chunk = [0u8; 512];
    let n = read(m, chunk.as_mut_ptr() as *mut c_void, chunk.len());
    if n > 0 {
        let acc = acc();
        for i in 0..n as usize {
            if *len < ACC_CAP {
                acc[*len] = chunk[i];
                *len += 1;
            }
        }
        // Scan from where the last pass stopped, minus the length of the
        // longest sequence we answer, so a query split across two reads is
        // still seen exactly once.
        let from = scanned.saturating_sub(3);
        let mut i = from;
        while i + 4 <= *len {
            if &acc[i..i + 4] == b"\x1b[6n" {
                // Row 1, column 1: the geometry does not matter to this test,
                // only that a reply arrives at all.
                write_all(m, b"\x1b[1;1R");
                i += 4;
            } else {
                i += 1;
            }
        }
        *scanned = *len;
    }
    n
}

unsafe fn pump_for(m: c_int, len: &mut usize, scanned: &mut usize, ms: i64) {
    let mut waited = 0i64;
    while waited < ms {
        if pump(m, len, scanned) <= 0 {
            msleep(10);
            waited += 10;
        }
    }
}

/// Pump until `needle` appears in `ACC` or `ms` elapses.
unsafe fn pump_until(m: c_int, len: &mut usize, scanned: &mut usize,
                     needle: &[u8], ms: i64) -> bool {
    let mut waited = 0i64;
    loop {
        if find(&acc()[..*len], needle).is_some() {
            return true;
        }
        if pump(m, len, scanned) <= 0 {
            if waited >= ms {
                return false;
            }
            msleep(10);
            waited += 10;
        }
    }
}

/// Pump until the shell stops talking for `idle_ms`. Waiting for a specific
/// prompt string would bake this test into whatever `PS1` brush happens to
/// default to; waiting for the output to stop is what a human does, and it
/// also absorbs however many rounds of terminal probing the line editor needs.
/// False means nothing ever arrived — the child never got as far as printing.
unsafe fn pump_until_idle(m: c_int, len: &mut usize, scanned: &mut usize,
                          idle_ms: i64, max_ms: i64) -> bool {
    let mut waited = 0i64;
    let mut idle = 0i64;
    while waited < max_ms {
        if pump(m, len, scanned) > 0 {
            idle = 0;
        } else {
            if *len > 0 && idle >= idle_ms {
                return true;
            }
            msleep(10);
            idle += 10;
        }
        waited += 10;
    }
    *len > 0
}

/// Print a byte range with the control characters spelled out, so the report
/// shows a transcript rather than a screenful of escape sequences.
unsafe fn dump(label: &[u8], bytes: &[u8]) {
    out(label);
    for &b in bytes {
        match b {
            0x1b => out(b"\\e"),
            b'\r' => out(b"\\r"),
            b'\n' => out(b"\\n"),
            0x20..=0x7e => out(&[b]),
            _ => out(b"."),
        }
    }
    out(b"\n");
}

unsafe fn test_brush(m: c_int, n: usize) -> bool {
    let child = fork();
    if child == 0 {
        close(m);
        if !child_login_tty(n) {
            _exit(1);
        }
        static PATH_ENV: &[u8] = b"PATH=/bin\0";
        static TERM_ENV: &[u8] = b"TERM=xterm-256color\0";
        static HOME_ENV: &[u8] = b"HOME=/root\0";
        static PS1_ENV: &[u8] = b"PS1=PTYSH> \0";
        let envp: [*const u8; 5] = [
            PATH_ENV.as_ptr(), TERM_ENV.as_ptr(), HOME_ENV.as_ptr(), PS1_ENV.as_ptr(),
            core::ptr::null(),
        ];
        static ARG0: &[u8] = b"brush\0";
        static ARG1: &[u8] = b"-i\0";
        let argv: [*const u8; 3] = [ARG0.as_ptr(), ARG1.as_ptr(), core::ptr::null()];
        execve(b"/bin/brush\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
        _exit(127);
    }

    let mut len = 0usize;
    let mut scanned = 0usize;
    let started = pump_until_idle(m, &mut len, &mut scanned, 700, 25000);
    let prompt_end = len;

    // Enter is CR on a terminal, not LF: that is what the keyboard sends, and
    // a shell in raw mode has ICRNL off, so sending LF here would test a
    // sequence no terminal ever produces.
    write_all(m, b"echo hello\r");
    let echoed = pump_until(m, &mut len, &mut scanned, b"hello\r\n", 15000);
    pump_for(m, &mut len, &mut scanned, 500);
    let hello_end = len;

    // The unambiguous assertion. "hello" also appears in the *echo* of the
    // typed line, so it cannot distinguish a shell that ran the command from a
    // line editor that merely painted it; "42" appears only if the shell
    // evaluated the arithmetic.
    write_all(m, b"echo $((21*2))\r");
    let ran = pump_until(m, &mut len, &mut scanned, b"42", 15000);
    pump_for(m, &mut len, &mut scanned, 500);

    write_all(m, b"exit\r");
    let mut status: c_int = 0;
    let mut waited = 0;
    while waitpid(child, &mut status, 1) == 0 && waited < 500 {
        pump(m, &mut len, &mut scanned);
        msleep(10);
        waited += 1;
    }

    let acc = acc();
    dump(b"  brush prompt: ", &acc[..prompt_end.min(400)]);
    dump(b"  echo hello:   ", &acc[prompt_end..hello_end.min(prompt_end + 400)]);
    dump(b"  echo 21*2:    ", &acc[hello_end..len.min(hello_end + 400)]);

    // `hello` twice — once echoed by the line editor as it was typed, once
    // printed by the command — is what proves the shell was interactive rather
    // than reading a script from a pipe.
    let interactive = count_of(&acc[..len], b"hello") >= 2;
    report(b"brush_on_pty", started && echoed && ran && interactive)
}

// ── Driver ───────────────────────────────────────────────────────────────────

/// Run one half-two subtest on a pair of its own.
///
/// Each fork test gets a fresh pair rather than sharing one, because the thing
/// they are testing *is* the pair's lifetime: a leftover slave reference from
/// the previous subtest would suppress the very hangup the next one asserts.
unsafe fn with_pair(name: &[u8], f: unsafe fn(c_int, usize) -> bool) -> bool {
    match open_master(true) {
        Some((m, n)) => {
            let ok = f(m, n);
            // test_sighup closes the master itself; a second close is a
            // harmless EBADF, and losing it would leak the pair.
            close(m);
            ok
        }
        None => report(name, false),
    }
}

#[no_mangle]
pub unsafe extern "C" fn pty_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    const TOTAL: usize = 15;
    let mut passed = 0usize;

    let (m, s) = match open_pair() {
        Some((m, s, _n)) => {
            report(b"open_ptmx_and_slave", true);
            passed += 1;
            (m, s)
        }
        None => {
            report(b"open_ptmx_and_slave", false);
            // Nothing downstream can run without a pair; report the rest as
            // failures rather than skipping them, so the summary line still
            // has a fixed denominator.
            out(b"ptytest: 0/");
            out_num(TOTAL);
            out(b"\n");
            return TOTAL as i32;
        }
    };

    // Ordering is deliberate: everything that needs a live master runs before
    // test 6, which closes it.
    if test_raw_roundtrip(m, s) { passed += 1; }
    if test_canonical_line(m, s) { passed += 1; }
    if test_echo(m, s) { passed += 1; }
    if test_winsize(m, s) { passed += 1; }
    if test_master_close_is_eof(m, s) { passed += 1; }

    // Released before half two so the pool is not one pair short for the rest
    // of the run — the pair only frees when *both* ends are gone.
    close(s);

    if !shared_init() {
        report(b"shared_page", false);
        out(b"ptytest: ");
        out_num(passed);
        out(b"/");
        out_num(TOTAL);
        out(b"\n");
        return (TOTAL - passed) as i32;
    }

    if with_pair(b"ctty_setsid", test_ctty) { passed += 1; }
    if with_pair(b"child_stdio", test_child_stdio) { passed += 1; }
    if with_pair(b"sigwinch", test_sigwinch) { passed += 1; }
    if with_pair(b"sigint_fg_only", test_sigint) { passed += 1; }
    if with_pair(b"sighup_on_close", test_sighup) { passed += 1; }
    if with_pair(b"devtty_is_ctty", test_devtty) { passed += 1; }
    if test_master_eio() { passed += 1; }
    if test_gptpeer() { passed += 1; }
    if with_pair(b"brush_on_pty", test_brush) { passed += 1; }

    out(b"ptytest: ");
    out_num(passed);
    out(b"/");
    out_num(TOTAL);
    out(b"\n");
    (TOTAL - passed) as i32
}
