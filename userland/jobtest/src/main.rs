//! jobtest — terminal job control on a pty pair, end to end:
//!
//!  1. `^Z` (VSUSP) typed at the master stops the terminal's **foreground
//!     process group** with SIGTSTP — `waitpid(WUNTRACED)` reports it,
//!     `/proc/<pid>/stat` reads `T`, and SIGCONT resumes it
//!     (`waitpid(WCONTINUED)`). The job took the terminal itself with
//!     `tcsetpgrp` while still in the background, the way every shell's child
//!     does, which only works because it ignores SIGTTOU.
//!  2. A background-group `read()` of the slave stops the reader with SIGTTIN;
//!     once handed the terminal and continued, the *same* read completes.
//!  3. A background reader that ignores SIGTTIN gets EIO instead.
//!  4. `tcsetpgrp` from a background group with SIGTTOU at its default stops
//!     the caller with SIGTTOU.
//!  5. A background `write()` stops the writer with SIGTTOU only when the
//!     terminal has TOSTOP; without it the write goes through.
//!  6. The orphaned-process-group rule: a stopped job whose last anchor
//!     exits gets SIGHUP + SIGCONT.
//!  7. The same rule for the exiting process's CHILD's group: a stopped child
//!     in its own group, anchored only by its parent, gets SIGHUP + SIGCONT
//!     when that parent exits (the half of the rule that runs after the
//!     child has been reparented to init).
//!
//! The test process forks once; the child becomes a session leader with the
//! slave as its controlling terminal and runs every case as that terminal's
//! "shell" (SIGTTOU ignored, like a real one). Its stdout is still the
//! caller's console, so results print where the test was started.
//!
//! Same shape as sigtest2: relibc_start_v1 entry, one "<name>: PASS"/"<name>:
//! FAIL at step N" line per check, a final "JOBTEST: PASS"/"JOBTEST: FAIL <n>"
//! summary, exit code = failure count.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicI32, Ordering};

type c_int = i32;
type c_long = i64;
type c_ulong = u64;
type time_t = i64;
type pid_t = c_int;

pub type sigset_t = u64;

#[repr(C)]
pub struct sigaction {
    pub sa_handler: Option<extern "C" fn(c_int)>,
    pub sa_flags: c_int,
    pub sa_restorer: Option<unsafe extern "C" fn()>,
    pub sa_mask: sigset_t,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    pub tv_sec: time_t,
    pub tv_nsec: c_long,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 19],
}

const SIGHUP:  c_int = 1;
const SIGKILL: c_int = 9;
const SIGCONT: c_int = 18;
const SIGSTOP: c_int = 19;
const SIGTSTP: c_int = 20;
const SIGTTIN: c_int = 21;
const SIGTTOU: c_int = 22;

const SIG_IGN: usize = 1;

const WNOHANG:    c_int = 1;
const WUNTRACED:  c_int = 2;
const WCONTINUED: c_int = 8;

const O_RDONLY: c_int = 0;
const O_RDWR:   c_int = 0o2;
const O_NOCTTY: c_int = 0o400;

const EIO: c_int = 5;

const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0o4000;

const TCGETS:     c_ulong = 0x5401;
const TCSETS:     c_ulong = 0x5402;
const TIOCSCTTY:  c_ulong = 0x540E;
const TIOCGPGRP:  c_ulong = 0x540F;
const TIOCSPGRP:  c_ulong = 0x5410;
const TIOCGPTN:   c_ulong = 0x8004_5430;
const TIOCSPTLCK: c_ulong = 0x4004_5431;

const TOSTOP: u32 = 0x0100;
const VSUSP: u8 = 0x1A;

fn wifexited(s: c_int) -> bool    { s & 0x7f == 0 }
fn wexitstatus(s: c_int) -> c_int { (s >> 8) & 0xff }
fn wifstopped(s: c_int) -> bool   { s & 0xff == 0x7f }
fn wstopsig(s: c_int) -> c_int    { (s >> 8) & 0xff }
fn wifcontinued(s: c_int) -> bool { s == 0xffff }

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: i32) -> i32;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, arg: *mut c_void) -> c_int;
    pub fn exit(status: i32) -> !;
    pub fn _exit(status: i32) -> !;
    pub fn __errno_location() -> *mut c_int;

    pub fn fork() -> pid_t;
    pub fn getpid() -> pid_t;
    pub fn setsid() -> pid_t;
    pub fn setpgid(pid: pid_t, pgid: pid_t) -> c_int;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn signal(signum: c_int, handler: usize) -> usize;
    pub fn sigaction(sig: c_int, act: *const sigaction, oact: *mut sigaction) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
}

// ── Assembly entry point (identical to sigtest2's) ──────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset job_main",
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
    "   adrp x1, job_main",
    "   add x1, x1, :lo12:job_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn job_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    // The pair is opened before the fork so both the session child (which
    // types at the master) and its jobs (which read the slave) inherit it.
    let (master, slave) = match open_pair() {
        Some(p) => p,
        None => {
            puts(b"jobtest: cannot open a pty pair\0".as_ptr());
            puts(b"JOBTEST: FAIL 1\0".as_ptr());
            return 1;
        }
    };

    let leader = fork();
    if leader < 0 {
        puts(b"JOBTEST: FAIL 1\0".as_ptr());
        return 1;
    }
    if leader == 0 {
        let failures = session_main(master, slave);
        _exit(failures);
    }
    close(master);
    close(slave);
    let mut st: c_int = 0;
    if waitpid(leader, &mut st, 0) != leader || !wifexited(st) {
        puts(b"jobtest: session leader died\0".as_ptr());
        puts(b"JOBTEST: FAIL 1\0".as_ptr());
        return 1;
    }
    wexitstatus(st)
}

/// The session leader: `setsid`, claim the slave, ignore SIGTTOU as a shell
/// does, run the cases.
unsafe fn session_main(master: c_int, slave: c_int) -> i32 {
    let mut failures = 0;
    if setsid() < 0 {
        puts(b"jobtest: setsid failed\0".as_ptr());
        return 1;
    }
    if ioctl(slave, TIOCSCTTY, core::ptr::null_mut()) != 0 {
        puts(b"jobtest: TIOCSCTTY failed\0".as_ptr());
        return 1;
    }
    signal(SIGTTOU, SIG_IGN);
    let me = getpid();

    if !test_tstp_stops_foreground(master, slave, me) { failures += 1; }
    if !test_background_read_sigttin(master, slave, me) { failures += 1; }
    if !test_background_read_ignored_eio(slave, me) { failures += 1; }
    if !test_tcsetpgrp_from_background_sigttou(slave, me) { failures += 1; }
    if !test_background_write_tostop(slave, me) { failures += 1; }
    if !test_orphaned_pgrp_gets_sighup(slave, me) { failures += 1; }
    if !test_orphaned_child_pgrp_gets_sighup() { failures += 1; }

    puts(b"--- jobtest done ---\0".as_ptr());
    if failures == 0 {
        puts(b"JOBTEST: PASS\0".as_ptr());
    } else {
        let mut line = *b"JOBTEST: FAIL 0\0";
        line[14] = b'0' + (failures as u8 % 10);
        puts(line.as_ptr());
    }
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── Helpers ────────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], ok: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if ok { puts(b": PASS\0".as_ptr()); } else { puts(b": FAIL\0".as_ptr()); }
    ok
}

unsafe fn fail_at(name: &[u8], step: u8) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    let mut tail = *b": FAIL at step 00\0";
    tail[15] = b'0' + step / 10;
    tail[16] = b'0' + step % 10;
    puts(tail.as_ptr());
    false
}

unsafe fn errno() -> c_int { *__errno_location() }

/// ~10 ms (one tick).
unsafe fn nap() {
    let ts = timespec { tv_sec: 0, tv_nsec: 10_000_000 };
    nanosleep(&ts, core::ptr::null_mut());
}

/// Poll `f` for up to ~3 s.
unsafe fn wait_until(f: impl Fn() -> bool) -> bool {
    for _ in 0..300 {
        if f() { return true; }
        nap();
    }
    f()
}

/// `waitpid(child, WUNTRACED|WNOHANG)` polled for up to ~3 s until it reports
/// a stop; returns the stop signal, or None.
unsafe fn wait_stopped(child: pid_t) -> Option<c_int> {
    let mut st: c_int = 0;
    for _ in 0..300 {
        let r = waitpid(child, &mut st, WUNTRACED | WNOHANG);
        if r == child {
            return if wifstopped(st) { Some(wstopsig(st)) } else { None };
        }
        if r < 0 { return None; }
        nap();
    }
    None
}

/// Reap an exited/killed child with a bounded poll.
unsafe fn reap(child: pid_t) -> Option<c_int> {
    let mut st: c_int = 0;
    for _ in 0..300 {
        let r = waitpid(child, &mut st, WNOHANG);
        if r == child { return Some(st); }
        if r < 0 { return None; }
        nap();
    }
    None
}

unsafe fn kill_reap(child: pid_t) {
    kill(child, SIGKILL);
    reap(child);
}

unsafe fn tcgetpgrp(fd: c_int) -> pid_t {
    let mut pg: u32 = 0;
    if ioctl(fd, TIOCGPGRP, &mut pg as *mut u32 as *mut c_void) != 0 { return -1; }
    pg as pid_t
}

unsafe fn tcsetpgrp(fd: c_int, pgid: pid_t) -> c_int {
    let mut pg: u32 = pgid as u32;
    ioctl(fd, TIOCSPGRP, &mut pg as *mut u32 as *mut c_void)
}

/// `posix_openpt` + `TIOCGPTN` + `unlockpt` + `open(ptsname)`, blocking, no
/// controlling-terminal side effects.
unsafe fn open_pair() -> Option<(c_int, c_int)> {
    let m = open(b"/dev/ptmx\0".as_ptr(), O_RDWR | O_NOCTTY);
    if m < 0 { return None; }
    let mut n: u32 = 0;
    if ioctl(m, TIOCGPTN, &mut n as *mut u32 as *mut c_void) != 0 || n > 99 {
        close(m);
        return None;
    }
    let mut unlock: i32 = 0;
    if ioctl(m, TIOCSPTLCK, &mut unlock as *mut i32 as *mut c_void) != 0 {
        close(m);
        return None;
    }
    let mut path = [0u8; 16];
    path[..9].copy_from_slice(b"/dev/pts/");
    let mut i = 9;
    if n >= 10 { path[i] = b'0' + (n / 10) as u8; i += 1; }
    path[i] = b'0' + (n % 10) as u8;
    let s = open(path.as_ptr(), O_RDWR | O_NOCTTY);
    if s < 0 {
        close(m);
        return None;
    }
    Some((m, s))
}

/// The state letter of `/proc/<pid>/stat` (third field), or 0.
unsafe fn proc_state(pid: pid_t) -> u8 {
    let mut path = *b"/proc/0000000000/stat\0";
    // Right-align the pid into the ten digit cells, then shift left.
    let mut digits = [0u8; 10];
    let mut n = pid as u32;
    let mut d = 0usize;
    if n == 0 { digits[0] = b'0'; d = 1; }
    while n > 0 { digits[d] = b'0' + (n % 10) as u8; d += 1; n /= 10; }
    let mut p = 6usize;
    for k in (0..d).rev() { path[p] = digits[k]; p += 1; }
    path[p] = b'/'; path[p + 1] = b's'; path[p + 2] = b't'; path[p + 3] = b'a';
    path[p + 4] = b't'; path[p + 5] = 0;
    let fd = open(path.as_ptr(), O_RDONLY);
    if fd < 0 { return 0; }
    let mut buf = [0u8; 128];
    let got = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    if got <= 0 { return 0; }
    // "<pid> (<comm>) <state> ..."
    let s = &buf[..got as usize];
    let close_paren = match s.iter().position(|&b| b == b')') { Some(i) => i, None => return 0 };
    if close_paren + 2 >= s.len() { return 0; }
    s[close_paren + 2]
}

unsafe fn set_nonblock(fd: c_int) -> bool {
    let fl = fcntl(fd, F_GETFL, 0);
    fl >= 0 && fcntl(fd, F_SETFL, fl | O_NONBLOCK) == 0
}

/// Read one byte from a nonblocking `fd` within ~3 s.
unsafe fn read_byte_timeout(fd: c_int) -> Option<u8> {
    for _ in 0..300 {
        let mut b = 0u8;
        let r = read(fd, &mut b, 1);
        if r == 1 { return Some(b); }
        if r == 0 { return None; } // writer gone
        nap();
    }
    None
}

/// Make a fresh process group for the calling child.
unsafe fn own_pgrp() -> bool { setpgid(0, 0) == 0 }

/// Loop reading the slave until a byte arrives, then exit 0; EIO exits with
/// EIO; anything else with 99. EINTR (a stop signal that interrupted the
/// read, since this kernel does not restart syscalls) is retried.
unsafe fn read_slave_then_exit(slave: c_int, notify: c_int) -> ! {
    if notify >= 0 { write(notify, b"r".as_ptr(), 1); }
    loop {
        let mut b = 0u8;
        let r = read(slave, &mut b, 1);
        if r == 1 { _exit(0); }
        if r == 0 { _exit(98); }
        let e = errno();
        if e == EIO { _exit(EIO); }
        if e != 4 /* EINTR */ && e != 11 /* EAGAIN */ { _exit(99); }
    }
}

// ── 1. ^Z stops the foreground group with SIGTSTP; SIGCONT resumes ─────────

unsafe fn test_tstp_stops_foreground(master: c_int, slave: c_int, me: pid_t) -> bool {
    let name = b"tstp_stops_foreground\0";
    let mut fds: [c_int; 2] = [0; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return fail_at(name, 1); }
    let (rfd, wfd) = (fds[0], fds[1]);
    if !set_nonblock(rfd) { return fail_at(name, 1); }

    let child = fork();
    if child < 0 { return fail_at(name, 2); }
    if child == 0 {
        close(rfd);
        // A shell's job: new group, then take the terminal from the
        // background — allowed only because SIGTTOU is ignored (inherited).
        if !own_pgrp() { _exit(90); }
        if tcsetpgrp(slave, getpid()) != 0 { _exit(91); }
        read_slave_then_exit(slave, wfd);
    }
    close(wfd);

    // The child is in the foreground and about to read.
    if read_byte_timeout(rfd) != Some(b'r') { kill_reap(child); close(rfd); return fail_at(name, 3); }
    close(rfd);
    if !wait_until(|| tcgetpgrp(slave) == child) { kill_reap(child); return fail_at(name, 4); }

    // ^Z at the master: the line discipline signals the foreground group.
    let z = [VSUSP];
    if write(master, z.as_ptr(), 1) != 1 { kill_reap(child); return fail_at(name, 5); }
    match wait_stopped(child) {
        Some(sig) if sig == SIGTSTP => {}
        Some(_) => { kill_reap(child); return fail_at(name, 6); }
        None => { kill_reap(child); return fail_at(name, 7); }
    }
    // /proc/<pid>/stat reports the stop.
    if proc_state(child) != b'T' { kill_reap(child); return fail_at(name, 8); }
    // The shell takes the terminal back while the job is stopped (allowed:
    // SIGTTOU ignored), then `fg`: terminal to the job first, SIGCONT second
    // — the other order would have the resumed read take SIGTTIN.
    if tcsetpgrp(slave, me) != 0 { kill_reap(child); return fail_at(name, 9); }
    if tcgetpgrp(slave) != me { kill_reap(child); return fail_at(name, 10); }
    if tcsetpgrp(slave, child) != 0 { kill_reap(child); return fail_at(name, 11); }
    if kill(child, SIGCONT) != 0 { kill_reap(child); return fail_at(name, 12); }
    let mut st: c_int = 0;
    if waitpid(child, &mut st, WCONTINUED) != child || !wifcontinued(st) {
        kill_reap(child); return fail_at(name, 13);
    }
    if !wait_until(|| proc_state(child) != b'T') { kill_reap(child); return fail_at(name, 14); }
    // The job is reading again; a line completes it.
    if write(master, b"x\n".as_ptr(), 2) != 2 { kill_reap(child); return fail_at(name, 15); }
    let st = match reap(child) { Some(s) => s, None => { kill_reap(child); return fail_at(name, 16); } };
    if tcsetpgrp(slave, me) != 0 { return fail_at(name, 17); }
    if !wifexited(st) || wexitstatus(st) != 0 { return fail_at(name, 18); }
    report(name, true)
}

// ── 2. a background read stops the reader with SIGTTIN, then completes ─────

unsafe fn test_background_read_sigttin(master: c_int, slave: c_int, me: pid_t) -> bool {
    let name = b"background_read_sigttin\0";
    if tcgetpgrp(slave) != me { return fail_at(name, 1); }

    let child = fork();
    if child < 0 { return fail_at(name, 2); }
    if child == 0 {
        if !own_pgrp() { _exit(90); }
        read_slave_then_exit(slave, -1);
    }
    match wait_stopped(child) {
        Some(sig) if sig == SIGTTIN => {}
        Some(_) => { kill_reap(child); return fail_at(name, 3); }
        None => { kill_reap(child); return fail_at(name, 4); }
    }
    if proc_state(child) != b'T' { kill_reap(child); return fail_at(name, 5); }
    // Nothing typed yet: the stopped reader must not have consumed anything.
    // Hand it the terminal, continue it, then type — the read completes.
    if tcsetpgrp(slave, child) != 0 { kill_reap(child); return fail_at(name, 6); }
    if kill(child, SIGCONT) != 0 { kill_reap(child); return fail_at(name, 7); }
    let mut st: c_int = 0;
    if waitpid(child, &mut st, WCONTINUED) != child || !wifcontinued(st) {
        kill_reap(child); return fail_at(name, 8);
    }
    // Still alive and still waiting for input.
    if waitpid(child, &mut st, WNOHANG | WUNTRACED) != 0 { kill_reap(child); return fail_at(name, 9); }
    if write(master, b"y\n".as_ptr(), 2) != 2 { kill_reap(child); return fail_at(name, 10); }
    let st = match reap(child) { Some(s) => s, None => { kill_reap(child); return fail_at(name, 11); } };
    if tcsetpgrp(slave, me) != 0 { return fail_at(name, 12); }
    if !wifexited(st) || wexitstatus(st) != 0 { return fail_at(name, 13); }
    report(name, true)
}

// ── 3. a background reader ignoring SIGTTIN gets EIO ───────────────────────

unsafe fn test_background_read_ignored_eio(slave: c_int, me: pid_t) -> bool {
    let name = b"background_read_ignored_eio\0";
    if tcgetpgrp(slave) != me { return fail_at(name, 1); }
    let child = fork();
    if child < 0 { return fail_at(name, 2); }
    if child == 0 {
        if !own_pgrp() { _exit(90); }
        signal(SIGTTIN, SIG_IGN);
        read_slave_then_exit(slave, -1);
    }
    let st = match reap(child) { Some(s) => s, None => { kill_reap(child); return fail_at(name, 3); } };
    if !wifexited(st) { return fail_at(name, 4); }
    if wexitstatus(st) != EIO { return fail_at(name, 5); }
    report(name, true)
}

// ── 4. tcsetpgrp from the background stops the caller with SIGTTOU ─────────

unsafe fn test_tcsetpgrp_from_background_sigttou(slave: c_int, me: pid_t) -> bool {
    let name = b"tcsetpgrp_background_sigttou\0";
    if tcgetpgrp(slave) != me { return fail_at(name, 1); }
    let child = fork();
    if child < 0 { return fail_at(name, 2); }
    if child == 0 {
        if !own_pgrp() { _exit(90); }
        // Back to the default action: the inherited SIG_IGN is what lets a
        // shell's child do this without stopping.
        signal(SIGTTOU, 0 /* SIG_DFL */);
        // Stopped inside this call; once continued in the foreground it
        // succeeds and the child exits 0.
        let r = tcsetpgrp(slave, getpid());
        _exit(if r == 0 { 0 } else { 92 });
    }
    match wait_stopped(child) {
        Some(sig) if sig == SIGTTOU => {}
        Some(_) => { kill_reap(child); return fail_at(name, 3); }
        None => { kill_reap(child); return fail_at(name, 4); }
    }
    // The terminal did not change hands while the caller was stopped.
    if tcgetpgrp(slave) != me { kill_reap(child); return fail_at(name, 5); }
    // Hand over and continue: the interrupted tcsetpgrp now completes.
    if tcsetpgrp(slave, child) != 0 { kill_reap(child); return fail_at(name, 6); }
    if kill(child, SIGCONT) != 0 { kill_reap(child); return fail_at(name, 7); }
    let st = match reap(child) { Some(s) => s, None => { kill_reap(child); return fail_at(name, 8); } };
    if tcsetpgrp(slave, me) != 0 { return fail_at(name, 9); }
    if !wifexited(st) || wexitstatus(st) != 0 { return fail_at(name, 10); }
    report(name, true)
}

// ── 5. background write: SIGTTOU with TOSTOP, allowed without ──────────────

unsafe fn test_background_write_tostop(slave: c_int, me: pid_t) -> bool {
    let name = b"background_write_tostop\0";
    if tcgetpgrp(slave) != me { return fail_at(name, 1); }
    let mut t = termios { c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0, c_line: 0, c_cc: [0; 19] };
    if ioctl(slave, TCGETS, &mut t as *mut termios as *mut c_void) != 0 { return fail_at(name, 2); }
    let saved = t;

    // Without TOSTOP a background write is allowed.
    t.c_lflag &= !TOSTOP;
    if ioctl(slave, TCSETS, &mut t as *mut termios as *mut c_void) != 0 { return fail_at(name, 3); }
    let child = fork();
    if child < 0 { return fail_at(name, 4); }
    if child == 0 {
        if !own_pgrp() { _exit(90); }
        signal(SIGTTOU, 0 /* SIG_DFL */);
        let r = write(slave, b"bg\n".as_ptr(), 3);
        _exit(if r == 3 { 0 } else { 93 });
    }
    let st = match reap(child) { Some(s) => s, None => { kill_reap(child); return fail_at(name, 5); } };
    if !wifexited(st) || wexitstatus(st) != 0 { return fail_at(name, 6); }

    // With TOSTOP the same write stops the writer.
    t.c_lflag |= TOSTOP;
    if ioctl(slave, TCSETS, &mut t as *mut termios as *mut c_void) != 0 { return fail_at(name, 7); }
    let child = fork();
    if child < 0 { return fail_at(name, 8); }
    if child == 0 {
        if !own_pgrp() { _exit(90); }
        signal(SIGTTOU, 0 /* SIG_DFL */);
        let r = write(slave, b"bg\n".as_ptr(), 3);
        _exit(if r == 3 { 0 } else { 93 });
    }
    let ok = match wait_stopped(child) {
        Some(sig) if sig == SIGTTOU => true,
        _ => false,
    };
    kill_reap(child);
    let mut restore = saved;
    ioctl(slave, TCSETS, &mut restore as *mut termios as *mut c_void);
    if !ok { return fail_at(name, 9); }
    report(name, true)
}

// ── 6. an orphaned group with a stopped member gets SIGHUP + SIGCONT ───────

static HUP_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_hup(_sig: c_int) {
    unsafe {
        let fd = HUP_PIPE.load(Ordering::SeqCst);
        if fd >= 0 { write(fd, b"H".as_ptr(), 1); }
        _exit(0);
    }
}

unsafe fn test_orphaned_pgrp_gets_sighup(_slave: c_int, _me: pid_t) -> bool {
    let name = b"orphaned_pgrp_sighup\0";
    let mut fds: [c_int; 2] = [0; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return fail_at(name, 1); }
    let (rfd, wfd) = (fds[0], fds[1]);
    if !set_nonblock(rfd) { return fail_at(name, 2); }

    // P: a job in its own group; G: P's child in the same group, stopped.
    // When P exits, G's group has no member whose parent is outside the
    // group but inside the session — it is orphaned — and G is stopped, so
    // the kernel must send SIGHUP then SIGCONT. G's handler reports and exits.
    let p = fork();
    if p < 0 { return fail_at(name, 3); }
    if p == 0 {
        close(rfd);
        if !own_pgrp() { _exit(90); }
        // G tells P through a second pipe once its SIGHUP handler is in
        // place; only then may P stop it.
        let mut gfds: [c_int; 2] = [0; 2];
        if pipe(gfds.as_mut_ptr()) != 0 { _exit(94); }
        let g = fork();
        if g < 0 { _exit(94); }
        if g == 0 {
            close(gfds[0]);
            HUP_PIPE.store(wfd, Ordering::SeqCst);
            let act = sigaction { sa_handler: Some(on_hup), sa_flags: 0, sa_restorer: None, sa_mask: 0 };
            sigaction(SIGHUP, &act, core::ptr::null_mut());
            write(gfds[1], b"g".as_ptr(), 1);
            loop { nap(); }
        }
        close(gfds[1]);
        let mut b = 0u8;
        if read(gfds[0], &mut b, 1) != 1 || b != b'g' { kill(g, SIGKILL); _exit(95); }
        kill(g, SIGSTOP);
        let mut st: c_int = 0;
        if waitpid(g, &mut st, WUNTRACED) != g || !wifstopped(st) { kill(g, SIGKILL); _exit(96); }
        // Leave: G's only anchor outside its group goes away.
        _exit(0);
    }
    close(wfd);
    let st = match reap(p) { Some(s) => s, None => { kill_reap(p); close(rfd); return fail_at(name, 4); } };
    if !wifexited(st) || wexitstatus(st) != 0 { close(rfd); return fail_at(name, 5); }
    // G, orphaned and stopped, got SIGHUP (and the SIGCONT needed to run it).
    let got = read_byte_timeout(rfd);
    close(rfd);
    if got != Some(b'H') { return fail_at(name, 6); }
    report(name, true)
}

// ── 7. a stopped child in its own group is orphaned by its parent's exit ───

unsafe fn test_orphaned_child_pgrp_gets_sighup() -> bool {
    let name = b"orphaned_child_pgrp_sighup\0";
    let mut fds: [c_int; 2] = [0; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return fail_at(name, 1); }
    let (rfd, wfd) = (fds[0], fds[1]);
    if !set_nonblock(rfd) { return fail_at(name, 2); }
    // P tells the test C's pid, so a failure can still clean C up.
    let mut pfds: [c_int; 2] = [0; 2];
    if pipe(pfds.as_mut_ptr()) != 0 { return fail_at(name, 2); }

    // P: its own group, in this session. C: P's child in ANOTHER group of the
    // same session, stopped. P is C's group's only anchor; when P exits, C's
    // group is orphaned with a stopped member, so C must get SIGHUP + SIGCONT.
    // Unlike case 6 the orphaned group is not the exiting process's own.
    let p = fork();
    if p < 0 { return fail_at(name, 3); }
    if p == 0 {
        close(rfd);
        close(pfds[0]);
        if !own_pgrp() { _exit(90); }
        let mut gfds: [c_int; 2] = [0; 2];
        if pipe(gfds.as_mut_ptr()) != 0 { _exit(94); }
        let c = fork();
        if c < 0 { _exit(94); }
        if c == 0 {
            close(gfds[0]);
            if !own_pgrp() { _exit(90); }
            HUP_PIPE.store(wfd, Ordering::SeqCst);
            let act = sigaction { sa_handler: Some(on_hup), sa_flags: 0, sa_restorer: None, sa_mask: 0 };
            sigaction(SIGHUP, &act, core::ptr::null_mut());
            write(gfds[1], b"c".as_ptr(), 1);
            loop { nap(); }
        }
        close(gfds[1]);
        close(wfd);
        let cb = (c as u32).to_le_bytes();
        write(pfds[1], cb.as_ptr(), 4);
        let mut b = 0u8;
        if read(gfds[0], &mut b, 1) != 1 || b != b'c' { kill(c, SIGKILL); _exit(95); }
        kill(c, SIGSTOP);
        let mut st: c_int = 0;
        if waitpid(c, &mut st, WUNTRACED) != c || !wifstopped(st) { kill(c, SIGKILL); _exit(96); }
        _exit(0);
    }
    close(wfd);
    close(pfds[1]);
    let mut cb = [0u8; 4];
    let c = if read(pfds[0], cb.as_mut_ptr(), 4) == 4 { i32::from_le_bytes(cb) } else { -1 };
    close(pfds[0]);
    let st = match reap(p) { Some(s) => s, None => { kill_reap(p); close(rfd); if c > 0 { kill(c, SIGKILL); } return fail_at(name, 4); } };
    if !wifexited(st) || wexitstatus(st) != 0 { close(rfd); if c > 0 { kill(c, SIGKILL); } return fail_at(name, 5); }
    let got = read_byte_timeout(rfd);
    close(rfd);
    if got != Some(b'H') {
        // Not HUP'd: it would stay stopped under init for ever. Clean up.
        if c > 0 { kill(c, SIGKILL); }
        return fail_at(name, 6);
    }
    report(name, true)
}
