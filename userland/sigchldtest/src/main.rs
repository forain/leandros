//! sigchldtest — regression coverage for SIGCHLD delivery and its
//! EINTR-on-blocking-syscall contract: a blocking read()/nanosleep() in the
//! parent must be interrupted when a child exits while SIGCHLD has a real
//! handler installed (not SIG_IGN/SIG_DFL, which never generate a
//! deliverable signal for the syscall to be interrupted by).
//!
//! Modeled on sigtest: identical relibc_start_v1 entry, identical
//! `sigaction` struct/extern shape installed through relibc's POSIX
//! `sigaction()` wrapper (not a raw rt_sigaction syscall) — relibc's
//! `Sys::sigaction` injects the real SA_RESTORER trampoline pointer for us;
//! sigtest already regression-tests that plumbing directly
//! (`sigaction_struct_roundtrip`), so this suite reuses it rather than
//! re-deriving a trampoline by hand.
//!
//! Initializes via relibc_start_v1 (same as pthreadtest/timertest/sigtest)
//! so TLS is set up properly — errno and the sigaction SA_RESTORER
//! trampoline both need it.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); a final "SIGCHLDTEST: PASS" line, or one
//! "SIGCHLDTEST: FAIL <case>" line per failing case (a/b), summarizes the
//! run. `sigchld_main` returns the number of failures as the exit code.
//!
//! Runtime note: SIGCHLD delivery on child exit is not yet implemented in
//! the LeandrOS kernel as of this writing — this suite is expected to fail
//! at runtime until that lands. It exists to lock in the intended
//! Linux-compatible contract ahead of that kernel work.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicI32, Ordering};

type c_int = i32;
type c_long = i64;
type time_t = i64;
type pid_t = c_int;
type size_t = usize;

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

const SIGCHLD: c_int = 17;
const EINTR: c_int = 4;
const CLOCK_MONOTONIC: c_int = 1;

fn zeroed_sigaction(handler: Option<extern "C" fn(c_int)>) -> sigaction {
    sigaction { sa_handler: handler, sa_flags: 0, sa_restorer: None, sa_mask: 0 }
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn read(fd: i32, buf: *mut c_void, count: size_t) -> isize;
    pub fn exit(status: i32) -> !;
    pub fn __errno_location() -> *mut c_int;

    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn sigaction(sig: c_int, act: *const sigaction, oact: *mut sigaction) -> c_int;

    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn clock_gettime(clk: c_int, tp: *mut timespec) -> c_int;
}

// ── Assembly entry point (identical to sigtest's) ───────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset sigchld_main",
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
    "   adrp x1, sigchld_main",
    "   add x1, x1, :lo12:sigchld_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn sigchld_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;
    let mut failed_cases: [bool; 2] = [false; 2];

    if !test_sigchld_interrupts_read() { failures += 1; failed_cases[0] = true; }
    if !test_sigchld_interrupts_nanosleep() { failures += 1; failed_cases[1] = true; }

    puts(b"--- sigchldtest done ---\n\0".as_ptr());

    if failures == 0 {
        puts(b"SIGCHLDTEST: PASS\0".as_ptr());
    } else {
        let labels: [u8; 2] = [b'a', b'b'];
        for (i, &failed) in failed_cases.iter().enumerate() {
            if failed {
                let prefix = b"SIGCHLDTEST: FAIL ";
                write(1, prefix.as_ptr(), prefix.len());
                write(1, &labels[i] as *const u8, 1);
                write(1, b"\n".as_ptr(), 1);
            }
        }
    }

    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr(), 7);
    } else {
        write(1, b": FAIL\n".as_ptr(), 7);
    }
    passed
}

fn sleep_ms(ms: i64) {
    let req = timespec { tv_sec: ms / 1000, tv_nsec: (ms % 1000) * 1_000_000 };
    unsafe { nanosleep(&req, core::ptr::null_mut()); }
}

unsafe fn now_ms() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

// ── SIGCHLD handler ───────────────────────────────────────────────────────

static SIGCHLD_COUNT: AtomicI32 = AtomicI32::new(0);

extern "C" fn sigchld_handler(_sig: c_int) {
    SIGCHLD_COUNT.fetch_add(1, Ordering::SeqCst);
}

unsafe fn install_sigchld_handler() -> bool {
    let act = zeroed_sigaction(Some(sigchld_handler));
    sigaction(SIGCHLD, &act, core::ptr::null_mut()) == 0
}

// ── a. SIGCHLD interrupts a blocking read() with EINTR ───────────────────────
//
// The child sleeps 300ms then exits; the parent is meanwhile blocked in
// read(0, ..., 1) with no data available. If SIGCHLD delivery is wired up
// correctly, the child's exit interrupts the read syscall, the handler runs
// (bumping SIGCHLD_COUNT), and read() returns -1/EINTR instead of hanging
// forever. (Depends on fd 0 being open and actually blocking when empty —
// true of the serial console this suite runs against.)
unsafe fn test_sigchld_interrupts_read() -> bool {
    let name = b"sigchld_interrupts_read\0";
    SIGCHLD_COUNT.store(0, Ordering::SeqCst);

    if !install_sigchld_handler() { return report(name, false); }

    let r = fork();
    if r == 0 {
        sleep_ms(300);
        exit(0);
    }
    if r < 0 { return report(name, false); }

    let mut buf = [0u8; 1];
    let rc = read(0, buf.as_mut_ptr() as *mut c_void, 1);
    let interrupted = rc == -1 && *__errno_location() == EINTR;
    let handler_ran = SIGCHLD_COUNT.load(Ordering::SeqCst) >= 1;
    let _ = buf;

    let mut status: c_int = 0;
    waitpid(r, &mut status, 0);

    report(name, interrupted && handler_ran)
}

// ── b. SIGCHLD interrupts a blocking nanosleep() with EINTR ─────────────────
//
// Same setup, but the parent blocks in nanosleep(2s) instead of read(). Some
// libc/kernel combinations swallow EINTR internally and just return early
// once woken rather than surfacing -1/EINTR to the caller; mirroring
// sigtest's level of rigor (it checks the documented contract held, not
// every legal implementation choice), this test accepts either outcome as
// long as the wakeup was actually caused by the signal: either nanosleep
// reports -1/EINTR, or it returns having elapsed well under the requested
// 2000ms with the handler flag set.
unsafe fn test_sigchld_interrupts_nanosleep() -> bool {
    let name = b"sigchld_interrupts_nanosleep\0";
    SIGCHLD_COUNT.store(0, Ordering::SeqCst);

    if !install_sigchld_handler() { return report(name, false); }

    let r = fork();
    if r == 0 {
        sleep_ms(300);
        exit(0);
    }
    if r < 0 { return report(name, false); }

    let start = now_ms();
    let req = timespec { tv_sec: 2, tv_nsec: 0 };
    let mut rem = timespec { tv_sec: 0, tv_nsec: 0 };
    let rc = nanosleep(&req, &mut rem);
    let elapsed = now_ms() - start;

    let handler_ran = SIGCHLD_COUNT.load(Ordering::SeqCst) >= 1;
    let interrupted_explicitly = rc == -1 && *__errno_location() == EINTR;
    let woke_early = elapsed < 1000; // well under the requested 2000ms
    let ok = handler_ran && (interrupted_explicitly || woke_early);

    let mut status: c_int = 0;
    waitpid(r, &mut status, 0);

    report(name, ok)
}
