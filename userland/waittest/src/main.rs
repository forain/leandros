//! waittest — regression coverage for wait4/waitpid semantics: WNOHANG
//! polling, blocking wait, ECHILD on no children, and negative-pid
//! (process-group) reaping.
//!
//! Modeled on forktest (fork()/waitpid() through real relibc, plain exit
//! codes as the parent/child synchronization signal) and exercises the
//! wait4-specific surface forktest doesn't: WNOHANG polling loops, blocking
//! wait4(-1, ...), the ECHILD boundary, and wait4(-pgid, ...) after
//! setpgid().
//!
//! relibc does not export a standalone wait4(); its waitpid(pid, stat_loc,
//! options) issues the WAIT4 syscall directly (see relibc's
//! header/sys_wait/mod.rs and platform/linux/mod.rs), including support for
//! pid < -1 meaning "any child whose process group equals |pid|" — exactly
//! wait4's negative-pid contract, and pid == -1 meaning "any child" — so
//! every "wait4(...)" call this suite's spec calls for is made through
//! relibc's waitpid(), which issues the identical syscall under a POSIX
//! name (no rusage out-param, which nothing here needs).
//!
//! Initializes via relibc_start_v1 (same as pthreadtest/timertest/sigtest/
//! polltest/forktest/racetest) so TLS is set up — errno and the allocator
//! both need it.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); a final "WAITTEST: PASS" line, or one "WAITTEST: FAIL <case>"
//! line per failing case (a/b/c/d), summarizes the run. `wait_main` returns
//! the number of failures as the exit code.
//!
//! Runtime note: wait4's WNOHANG/blocking/ECHILD/process-group semantics
//! are not yet implemented in the LeandrOS kernel as of this writing — this
//! suite is expected to fail at runtime until that lands. It exists to lock
//! in the intended Linux-compatible contract ahead of that kernel work.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_long = i64;
type time_t = i64;
type pid_t = i32;
type size_t = usize;

const WNOHANG: c_int = 1;
const ECHILD: c_int = 10;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    pub tv_sec: time_t,
    pub tv_nsec: c_long,
}

// WEXITSTATUS / WIFEXITED on the musl/Linux wait-status encoding: a normal
// exit is `(code & 0xff) << 8`, and the low 7 bits are a terminating signal
// (0 ⇒ exited normally). Copied from forktest.
fn wifexited(status: c_int) -> bool { (status & 0x7f) == 0 }
fn wexitstatus(status: c_int) -> c_int { (status >> 8) & 0xff }

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn _exit(status: c_int) -> !;
    pub fn __errno_location() -> *mut c_int;

    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn setpgid(pid: pid_t, pgid: pid_t) -> c_int;

    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
}

// ── Assembly entry point (identical to forktest's) ───────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset wait_main",
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
    "   adrp x1, wait_main",
    "   add x1, x1, :lo12:wait_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn wait_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;
    let mut failed_cases: [bool; 4] = [false; 4];

    if !test_wnohang_poll_until_exit() { failures += 1; failed_cases[0] = true; }
    if !test_blocking_wait_for_exit() { failures += 1; failed_cases[1] = true; }
    if !test_echild_no_children() { failures += 1; failed_cases[2] = true; }
    if !test_wait_on_process_group() { failures += 1; failed_cases[3] = true; }

    puts(b"--- waittest done ---\n\0".as_ptr());

    if failures == 0 {
        puts(b"WAITTEST: PASS\0".as_ptr());
    } else {
        let labels: [u8; 4] = [b'a', b'b', b'c', b'd'];
        for (i, &failed) in failed_cases.iter().enumerate() {
            if failed {
                let prefix = b"WAITTEST: FAIL ";
                write(1, prefix.as_ptr() as *const c_void, prefix.len());
                write(1, &labels[i] as *const u8 as *const c_void, 1);
                write(1, b"\n".as_ptr() as *const c_void, 1);
            }
        }
    }

    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { _exit(134); }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr() as *const c_void, name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr() as *const c_void, 7);
    } else {
        write(1, b": FAIL\n".as_ptr() as *const c_void, 7);
    }
    passed
}

fn sleep_ms(ms: i64) {
    let req = timespec { tv_sec: ms / 1000, tv_nsec: (ms % 1000) * 1_000_000 };
    unsafe { nanosleep(&req, core::ptr::null_mut()); }
}

// ── a. WNOHANG poll loop until the child's exit is observed ─────────────────
//
// Before the child exits, waitpid(-1, &status, WNOHANG) may legitimately
// return 0 (no state change yet) rather than the child pid; the parent must
// keep polling with a short nanosleep between tries until it does. Bounded
// at 200 tries * 20ms = 4s so a kernel regression (WNOHANG never observing
// the exit) fails the test instead of hanging forever.
unsafe fn test_wnohang_poll_until_exit() -> bool {
    let name = b"wnohang_poll_until_exit\0";

    let r = fork();
    if r == 0 {
        _exit(42);
    }
    if r < 0 { return report(name, false); }

    let mut status: c_int = 0;
    let mut reaped: pid_t = 0;
    for _ in 0..200 {
        let w = waitpid(-1, &mut status, WNOHANG);
        if w == r {
            reaped = w;
            break;
        }
        if w < 0 { return report(name, false); }
        sleep_ms(20);
    }

    let ok = reaped == r && wifexited(status) && wexitstatus(status) == 42;
    report(name, ok)
}

// ── b. Blocking waitpid(-1, &status, 0) for a delayed exit ──────────────────

unsafe fn test_blocking_wait_for_exit() -> bool {
    let name = b"blocking_wait_for_exit\0";

    let r = fork();
    if r == 0 {
        sleep_ms(200);
        _exit(7);
    }
    if r < 0 { return report(name, false); }

    let mut status: c_int = 0;
    let waited = waitpid(-1, &mut status, 0);

    let ok = waited == r && wifexited(status) && wexitstatus(status) == 7;
    report(name, ok)
}

// ── c. ECHILD when the process has no children at all ───────────────────────
//
// By this point in the run both prior children (a, b) have already been
// reaped by their own tests, so this call has nothing to wait on.
unsafe fn test_echild_no_children() -> bool {
    let name = b"echild_no_children\0";

    let mut status: c_int = 0;
    let waited = waitpid(-1, &mut status, 0);
    let ok = waited == -1 && *__errno_location() == ECHILD;
    report(name, ok)
}

// ── d. waitpid(-pgid, ...) reaps a child that made itself a group leader ────

unsafe fn test_wait_on_process_group() -> bool {
    let name = b"wait_on_process_group\0";

    let r = fork();
    if r == 0 {
        setpgid(0, 0);
        _exit(0);
    }
    if r < 0 { return report(name, false); }

    let mut status: c_int = 0;
    let waited = waitpid(-r, &mut status, 0);

    let ok = waited == r && wifexited(status);
    report(name, ok)
}
