//! sigtest2 — regression coverage for three POSIX signal contracts the kernel
//! used to get wrong:
//!
//!  1. `fork()` and `pthread_create()` hand the new task the creator's signal
//!     mask (`clone.rs` used to zero it, so the "block in main, then spawn
//!     workers" signalfd pattern silently fell apart).
//!  2. A user-mode CPU fault reaches an installed SIGSEGV handler carrying a
//!     `siginfo_t` (`si_code` SEGV_MAPERR/SEGV_ACCERR, `si_addr`), and the
//!     handler can `siglongjmp` out. Without a handler — or with SIGSEGV
//!     blocked, or on a fault inside the handler itself — the process dies
//!     with `WIFSIGNALED(SIGSEGV)`.
//!  3. SIGSTOP stops the process (it used to terminate it): `waitpid(WUNTRACED)`
//!     reports `WIFSTOPPED`, the child really makes no progress, SIGCONT
//!     resumes it and `waitpid(WCONTINUED)` reports `WIFCONTINUED`, and
//!     SIGKILL ends it — stopped or running — with `WIFSIGNALED(SIGKILL)`.
//!  4. `sigsuspend()` and `sigtimedwait()` park the caller (they used to
//!     yield-spin in the kernel): the wait's CPU time is a small fraction of
//!     its wall time, the wake is prompt, and the timeout path is honoured.
//!
//! Same shape as sigtest: relibc_start_v1 entry, relibc's POSIX wrappers, one
//! "<name>: PASS"/"<name>: FAIL" line per check and a final "SIGTEST2: PASS"
//! or "SIGTEST2: FAIL <n>" summary; the exit code is the failure count.
//!
//! On x86_64 stdout goes to the framebuffer console, not serial — read the
//! result off a screenshot.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicI32, AtomicUsize, AtomicU64, Ordering};

type c_int = i32;
type c_long = i64;
type time_t = i64;
type off_t = i64;
type pid_t = c_int;
type size_t = usize;

pub type sigset_t = u64;
pub type pthread_t = *mut c_void;

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

/// The leading part of LP64 Linux's `siginfo_t` as a fault signal fills it:
/// three ints, padding to 8, then `_sifields._sigfault.si_addr` — the first
/// word of the union, where `_kill` puts `si_pid`/`si_uid`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct siginfo_t {
    pub si_signo: c_int,
    pub si_errno: c_int,
    pub si_code:  c_int,
    pub _pad0:    c_int,
    pub si_addr:  usize,
}

const SIGKILL:  c_int = 9;
const SIGUSR1:  c_int = 10;
const SIGSEGV:  c_int = 11;
const SIGUSR2:  c_int = 12;
const SIGCONT:  c_int = 18;
const SIGSTOP:  c_int = 19;

const SIG_BLOCK:   c_int = 0;
const SIG_UNBLOCK: c_int = 1;

const SA_SIGINFO: c_int = 0x0000_0004;

const SEGV_MAPERR: c_int = 1;
const SEGV_ACCERR: c_int = 2;

const WNOHANG:    c_int = 1;
const WUNTRACED:  c_int = 2;
const WCONTINUED: c_int = 8;

const PROT_READ:  c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_PRIVATE:   c_int = 0x02;
const MAP_ANONYMOUS: c_int = 0x20;

const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0o4000;
const EAGAIN: c_int = 11;

/// A user address nothing maps: far above any static image, heap or mmap
/// region and far below the stack, inside the canonical lower half.
const UNMAPPED: usize = 0x0000_4000_0000_0000;

// <sys/wait.h> status decoding (musl/relibc encoding).
fn wifexited(s: c_int) -> bool    { s & 0x7f == 0 }
fn wexitstatus(s: c_int) -> c_int { (s >> 8) & 0xff }
fn wifsignaled(s: c_int) -> bool  { s & 0x7f != 0 && s & 0x7f != 0x7f }
fn wtermsig(s: c_int) -> c_int    { s & 0x7f }
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
    pub fn close(fd: i32) -> i32;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    pub fn exit(status: i32) -> !;
    pub fn _exit(status: i32) -> !;
    pub fn __errno_location() -> *mut c_int;

    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn sigaction(sig: c_int, act: *const sigaction, oact: *mut sigaction) -> c_int;
    pub fn sigprocmask(how: c_int, set: *const sigset_t, oset: *mut sigset_t) -> c_int;
    pub fn pthread_sigmask(how: c_int, set: *const sigset_t, oset: *mut sigset_t) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn clock_gettime(clk: c_int, tp: *mut timespec) -> c_int;
    pub fn getpid() -> pid_t;
    pub fn sigsuspend(mask: *const sigset_t) -> c_int;
    pub fn sigtimedwait(set: *const sigset_t, info: *mut c_void, timeout: *const timespec) -> c_int;

    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int, fd: c_int, off: off_t) -> *mut c_void;
    pub fn mprotect(addr: *mut c_void, len: size_t, prot: c_int) -> c_int;
    pub fn munmap(addr: *mut c_void, len: size_t) -> c_int;

    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;

    // musl-layout sigjmp_buf: __jmp_buf (8 or 22 longs) + __fl + 128-byte
    // __ss. 64 words covers both architectures with room to spare.
    pub fn sigsetjmp(env: *mut u64, savemask: c_int) -> c_int;
    pub fn siglongjmp(env: *mut u64, val: c_int) -> !;
}

// ── Assembly entry point (identical to sigtest's) ───────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset sig2_main",
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
    "   adrp x1, sig2_main",
    "   add x1, x1, :lo12:sig2_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn sig2_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    // 1. mask inheritance
    if !test_fork_inherits_mask() { failures += 1; }
    if !test_pthread_inherits_mask() { failures += 1; }
    // 2. faults reach handlers
    if !test_segv_handler_siginfo_and_siglongjmp() { failures += 1; }
    if !test_segv_accerr_on_readonly_page() { failures += 1; }
    if !test_segv_default_kills() { failures += 1; }
    if !test_segv_blocked_kills() { failures += 1; }
    if !test_segv_inside_handler_kills() { failures += 1; }
    // 3. job control
    if !test_stop_continue_kill() { failures += 1; }
    if !test_kill_while_stopped() { failures += 1; }
    // 4. blocking signal waits park
    if !test_sigsuspend_parks() { failures += 1; }
    if !test_sigtimedwait_parks() { failures += 1; }
    if !test_sigtimedwait_timeout() { failures += 1; }

    puts(b"--- sigtest2 done ---\0".as_ptr());
    if failures == 0 {
        puts(b"SIGTEST2: PASS\0".as_ptr());
    } else {
        let mut line = *b"SIGTEST2: FAIL 0\0";
        line[15] = b'0' + (failures as u8 % 10);
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

/// Failure with a step number, so a FAIL line says *where* the check broke.
unsafe fn fail_at(name: &[u8], step: u8) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    let mut tail = *b": FAIL at step 00\0";
    tail[15] = b'0' + step / 10;
    tail[16] = b'0' + step % 10;
    puts(tail.as_ptr());
    false
}

/// Sleep ~10 ms (one kernel tick; nanosleep rounds up to whole ticks).
unsafe fn nap() {
    let ts = timespec { tv_sec: 0, tv_nsec: 10_000_000 };
    nanosleep(&ts, core::ptr::null_mut());
}

/// Poll `f` up to ~2 s; false on timeout so a missed event fails the check
/// instead of hanging the suite.
unsafe fn wait_until(f: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if f() { return true; }
        nap();
    }
    f()
}

/// Reap `child` with a bounded poll; returns the wait status or None.
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

unsafe fn errno() -> c_int { *__errno_location() }

fn sig_dfl() -> Option<extern "C" fn(c_int)> { None }

fn zeroed_sigaction(handler: Option<extern "C" fn(c_int)>) -> sigaction {
    sigaction { sa_handler: handler, sa_flags: 0, sa_restorer: None, sa_mask: 0 }
}

/// Install a three-argument `SA_SIGINFO` handler (see sigtest for why the
/// transmute is harmless: both architectures pass the three arguments in the
/// first three argument registers regardless of the prototype).
unsafe fn install_siginfo(
    sig: c_int,
    h: extern "C" fn(c_int, *const siginfo_t, *mut c_void),
) -> bool {
    let mut act = zeroed_sigaction(Some(
        core::mem::transmute::<
            extern "C" fn(c_int, *const siginfo_t, *mut c_void),
            extern "C" fn(c_int),
        >(h),
    ));
    act.sa_flags = SA_SIGINFO;
    sigaction(sig, &act, core::ptr::null_mut()) == 0
}

unsafe fn set_nonblock(fd: c_int) -> bool {
    let fl = fcntl(fd, F_GETFL);
    fl >= 0 && fcntl(fd, F_SETFL, fl | O_NONBLOCK) >= 0
}

/// Read and discard everything currently in a non-blocking pipe; returns the
/// number of bytes drained.
unsafe fn drain(fd: c_int) -> usize {
    let mut buf = [0u8; 256];
    let mut total = 0;
    loop {
        let n = read(fd, buf.as_mut_ptr(), buf.len());
        if n <= 0 { return total; }
        total += n as usize;
        if (n as usize) < buf.len() { return total; }
    }
}

// ── 1a. fork() inherits the calling thread's signal mask ───────────────────

unsafe fn test_fork_inherits_mask() -> bool {
    let name = b"fork_inherits_mask\0";
    let usr1: sigset_t = 1u64 << (SIGUSR1 - 1);
    if sigprocmask(SIG_BLOCK, &usr1, core::ptr::null_mut()) != 0 { return fail_at(name, 1); }

    let child = fork();
    if child < 0 { sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut()); return fail_at(name, 2); }
    if child == 0 {
        let mut cur: sigset_t = 0;
        if sigprocmask(SIG_BLOCK, core::ptr::null(), &mut cur) != 0 { _exit(3); }
        // SIGUSR1 must be blocked (inherited); SIGUSR2 must not be (control:
        // the child got *our* mask, not a full or garbage one).
        let has_usr1 = cur & (1u64 << (SIGUSR1 - 1)) != 0;
        let has_usr2 = cur & (1u64 << (SIGUSR2 - 1)) != 0;
        _exit(if has_usr1 && !has_usr2 { 0 } else if !has_usr1 { 1 } else { 2 });
    }
    sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());

    let st = match reap(child) { Some(s) => s, None => return fail_at(name, 4) };
    if !wifexited(st) { return fail_at(name, 5); }
    match wexitstatus(st) {
        0 => report(name, true),
        1 => fail_at(name, 6), // child came up with SIGUSR1 unblocked
        2 => fail_at(name, 7), // child came up with SIGUSR2 blocked too
        _ => fail_at(name, 8),
    }
}

// ── 1b. pthread_create() inherits the creating thread's signal mask ────────

static THREAD_MASK: AtomicU64 = AtomicU64::new(u64::MAX);

extern "C" fn mask_reader(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let mut cur: sigset_t = 0;
        if pthread_sigmask(SIG_BLOCK, core::ptr::null(), &mut cur) == 0 {
            THREAD_MASK.store(cur, Ordering::SeqCst);
        } else {
            THREAD_MASK.store(u64::MAX - 1, Ordering::SeqCst);
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_pthread_inherits_mask() -> bool {
    let name = b"pthread_inherits_mask\0";
    let usr1: sigset_t = 1u64 << (SIGUSR1 - 1);
    THREAD_MASK.store(u64::MAX, Ordering::SeqCst);
    if pthread_sigmask(SIG_BLOCK, &usr1, core::ptr::null_mut()) != 0 { return fail_at(name, 1); }

    let mut th: pthread_t = core::ptr::null_mut();
    let rc = pthread_create(&mut th, core::ptr::null(), mask_reader, core::ptr::null_mut());
    if rc != 0 {
        pthread_sigmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());
        return fail_at(name, 2);
    }
    pthread_join(th, core::ptr::null_mut());
    pthread_sigmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());

    let seen = THREAD_MASK.load(Ordering::SeqCst);
    if seen == u64::MAX { return fail_at(name, 3); }     // thread never ran
    if seen == u64::MAX - 1 { return fail_at(name, 4); } // pthread_sigmask failed
    if seen & usr1 == 0 { return fail_at(name, 5); }     // mask not inherited
    if seen & (1u64 << (SIGUSR2 - 1)) != 0 { return fail_at(name, 6); }
    report(name, true)
}

// ── 2a. SIGSEGV handler sees si_addr/si_code and siglongjmps out ───────────

static mut SEGV_ENV: [u64; 64] = [0; 64];
static SEGV_N:     AtomicI32   = AtomicI32::new(0);
static SEGV_SIGNO: AtomicI32   = AtomicI32::new(0);
static SEGV_CODE:  AtomicI32   = AtomicI32::new(0);
static SEGV_ADDR:  AtomicUsize = AtomicUsize::new(0);

fn segv_env() -> *mut u64 { core::ptr::addr_of_mut!(SEGV_ENV) as *mut u64 }

extern "C" fn segv_recover(sig: c_int, info: *const siginfo_t, _uc: *mut c_void) {
    unsafe {
        let i = &*info;
        SEGV_SIGNO.store(sig, Ordering::SeqCst);
        SEGV_CODE.store(i.si_code, Ordering::SeqCst);
        SEGV_ADDR.store(i.si_addr, Ordering::SeqCst);
        SEGV_N.fetch_add(1, Ordering::SeqCst);
        // Never returns to the faulting store: siglongjmp restores the mask
        // sigsetjmp(…, 1) saved, which un-blocks SIGSEGV again — so a second
        // fault must reach this handler a second time.
        siglongjmp(segv_env(), 1);
    }
}

/// One deliberate fault at `addr`, recovered through the jmp_buf. Returns
/// false if the store *succeeded* (no fault at all).
#[inline(never)]
unsafe fn fault_and_recover(addr: usize) -> bool {
    if sigsetjmp(segv_env(), 1) == 0 {
        core::ptr::write_volatile(addr as *mut u8, 0x5a);
        return false;
    }
    true
}

unsafe fn test_segv_handler_siginfo_and_siglongjmp() -> bool {
    let name = b"segv_handler_siginfo_siglongjmp\0";
    SEGV_N.store(0, Ordering::SeqCst);
    if !install_siginfo(SIGSEGV, segv_recover) { return fail_at(name, 1); }

    // First fault: unmapped address → SEGV_MAPERR with the exact address.
    if !fault_and_recover(UNMAPPED) { return fail_at(name, 2); }
    if SEGV_N.load(Ordering::SeqCst) != 1 { return fail_at(name, 3); }
    if SEGV_SIGNO.load(Ordering::SeqCst) != SIGSEGV { return fail_at(name, 4); }
    if SEGV_CODE.load(Ordering::SeqCst) != SEGV_MAPERR { return fail_at(name, 5); }
    if SEGV_ADDR.load(Ordering::SeqCst) != UNMAPPED { return fail_at(name, 6); }

    // The handler ran with SIGSEGV blocked; siglongjmp must have restored the
    // pre-handler mask, or the second fault below would be a forced kill.
    let mut cur: sigset_t = 0;
    sigprocmask(SIG_BLOCK, core::ptr::null(), &mut cur);
    if cur & (1u64 << (SIGSEGV - 1)) != 0 { return fail_at(name, 7); }

    // Second fault at a different address: handler re-armed, new si_addr.
    let addr2 = UNMAPPED + 0x1000 * 7 + 0x10;
    if !fault_and_recover(addr2) { return fail_at(name, 8); }
    if SEGV_N.load(Ordering::SeqCst) != 2 { return fail_at(name, 9); }
    if SEGV_ADDR.load(Ordering::SeqCst) != addr2 { return fail_at(name, 10); }

    report(name, true)
}

// ── 2b. A write to a PROT_READ page is SEGV_ACCERR ─────────────────────────

unsafe fn test_segv_accerr_on_readonly_page() -> bool {
    let name = b"segv_accerr_readonly_page\0";
    if !install_siginfo(SIGSEGV, segv_recover) { return fail_at(name, 1); }
    let len = 4096usize;
    let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 || p.is_null() { return fail_at(name, 2); }
    // Make the page present and writable first, then revoke write: the fault
    // must then be a permission fault on a present page, not a lazy-mapping
    // miss.
    core::ptr::write_volatile(p as *mut u8, 1);
    if mprotect(p, len, PROT_READ) != 0 { munmap(p, len); return fail_at(name, 3); }
    if core::ptr::read_volatile(p as *const u8) != 1 { munmap(p, len); return fail_at(name, 4); }

    SEGV_N.store(0, Ordering::SeqCst);
    let target = p as usize + 0x40;
    if !fault_and_recover(target) { munmap(p, len); return fail_at(name, 5); }
    munmap(p, len);
    if SEGV_N.load(Ordering::SeqCst) != 1 { return fail_at(name, 6); }
    if SEGV_CODE.load(Ordering::SeqCst) != SEGV_ACCERR { return fail_at(name, 7); }
    if SEGV_ADDR.load(Ordering::SeqCst) != target { return fail_at(name, 8); }
    report(name, true)
}

// ── 2c. No handler: the process dies with SIGSEGV ──────────────────────────

unsafe fn expect_child_killed_by(name: &[u8], sig: c_int, child_body: unsafe fn() -> !) -> bool {
    let child = fork();
    if child < 0 { return fail_at(name, 20); }
    if child == 0 { child_body(); }
    let st = match reap(child) { Some(s) => s, None => return fail_at(name, 21) };
    if wifexited(st) {
        // 7 = "the store succeeded", anything else = an unexpected path.
        return fail_at(name, if wexitstatus(st) == 7 { 22 } else { 23 });
    }
    if !wifsignaled(st) { return fail_at(name, 24); }
    if wtermsig(st) != sig { return fail_at(name, 25); }
    report(name, true)
}

unsafe fn segv_child_default() -> ! {
    // Dispositions are inherited across fork now, so drop the handler the
    // suite installed before faulting.
    let act = zeroed_sigaction(sig_dfl());
    sigaction(SIGSEGV, &act, core::ptr::null_mut());
    core::ptr::write_volatile(UNMAPPED as *mut u8, 1);
    _exit(7);
}

unsafe fn test_segv_default_kills() -> bool {
    expect_child_killed_by(b"segv_default_kills\0", SIGSEGV, segv_child_default)
}

// ── 2d. Handler installed but SIGSEGV blocked: forced kill, not deferral ───

unsafe fn segv_child_blocked() -> ! {
    install_siginfo(SIGSEGV, segv_recover);
    let segv: sigset_t = 1u64 << (SIGSEGV - 1);
    sigprocmask(SIG_BLOCK, &segv, core::ptr::null_mut());
    core::ptr::write_volatile(UNMAPPED as *mut u8, 1);
    _exit(7);
}

unsafe fn test_segv_blocked_kills() -> bool {
    expect_child_killed_by(b"segv_blocked_kills\0", SIGSEGV, segv_child_blocked)
}

// ── 2e. A fault inside the SIGSEGV handler is fatal (no recursion) ─────────

extern "C" fn segv_refault(_sig: c_int, _info: *const siginfo_t, _uc: *mut c_void) {
    unsafe {
        // SIGSEGV is masked while this runs (no SA_NODEFER), so this second
        // fault must kill the process rather than re-enter here forever.
        core::ptr::write_volatile((UNMAPPED + 0x2000) as *mut u8, 2);
        _exit(8);
    }
}

unsafe fn segv_child_refault() -> ! {
    install_siginfo(SIGSEGV, segv_refault);
    core::ptr::write_volatile(UNMAPPED as *mut u8, 1);
    _exit(7);
}

unsafe fn test_segv_inside_handler_kills() -> bool {
    expect_child_killed_by(b"segv_inside_handler_kills\0", SIGSEGV, segv_child_refault)
}

// ── 3a. SIGSTOP stops, SIGCONT continues, SIGKILL kills ────────────────────

/// Child body for the job-control tests: announce liveness on the pipe, then
/// keep writing one byte per tick forever. While stopped, the pipe must stay
/// silent — that is what proves the stop is real and not just a status code.
unsafe fn ticking_child(wfd: c_int) -> ! {
    let b = [b'.'];
    loop {
        write(wfd, b.as_ptr(), 1);
        nap();
    }
}

/// Wait until `rfd` (non-blocking) yields at least one byte, ~2 s bound.
unsafe fn pipe_becomes_readable(rfd: c_int) -> bool {
    wait_until(|| {
        let mut b = [0u8; 1];
        let n = read(rfd, b.as_mut_ptr(), 1);
        n == 1
    })
}

/// True if `rfd` stays silent for ~300 ms.
unsafe fn pipe_stays_silent(rfd: c_int) -> bool {
    for _ in 0..30 {
        nap();
        let mut b = [0u8; 1];
        let n = read(rfd, b.as_mut_ptr(), 1);
        if n == 1 { return false; }
        if n < 0 && errno() != EAGAIN { return false; }
    }
    true
}

unsafe fn test_stop_continue_kill() -> bool {
    let name = b"stop_continue_kill\0";
    let mut fds: [c_int; 2] = [0; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return fail_at(name, 1); }
    let (rfd, wfd) = (fds[0], fds[1]);
    if !set_nonblock(rfd) { return fail_at(name, 2); }

    let child = fork();
    if child < 0 { return fail_at(name, 3); }
    if child == 0 { close(rfd); ticking_child(wfd); }
    close(wfd);

    // The child is running (it has written at least once).
    if !pipe_becomes_readable(rfd) { kill(child, SIGKILL); reap(child); return fail_at(name, 4); }

    // SIGSTOP → WIFSTOPPED with WSTOPSIG == SIGSTOP.
    if kill(child, SIGSTOP) != 0 { kill(child, SIGKILL); reap(child); return fail_at(name, 5); }
    let mut st: c_int = 0;
    let r = waitpid(child, &mut st, WUNTRACED);
    if r != child { kill(child, SIGKILL); reap(child); return fail_at(name, 6); }
    if !wifstopped(st) { kill(child, SIGKILL); reap(child); return fail_at(name, 7); }
    if wstopsig(st) != SIGSTOP { kill(child, SIGKILL); reap(child); return fail_at(name, 8); }

    // The same stop is reported once: a brush-style WUNTRACED|WNOHANG poll
    // must now say "no state change".
    if waitpid(child, &mut st, WUNTRACED | WNOHANG) != 0 { kill(child, SIGKILL); reap(child); return fail_at(name, 9); }

    // Really stopped: drain what it wrote before stopping, then expect silence.
    drain(rfd);
    if !pipe_stays_silent(rfd) { kill(child, SIGKILL); reap(child); return fail_at(name, 10); }

    // SIGCONT → WIFCONTINUED, and the child ticks again.
    if kill(child, SIGCONT) != 0 { kill(child, SIGKILL); reap(child); return fail_at(name, 11); }
    let r = waitpid(child, &mut st, WCONTINUED);
    if r != child { kill(child, SIGKILL); reap(child); return fail_at(name, 12); }
    if !wifcontinued(st) { kill(child, SIGKILL); reap(child); return fail_at(name, 13); }
    if !pipe_becomes_readable(rfd) { kill(child, SIGKILL); reap(child); return fail_at(name, 14); }
    // ... and the continue, too, is reported only once.
    if waitpid(child, &mut st, WCONTINUED | WNOHANG) != 0 { kill(child, SIGKILL); reap(child); return fail_at(name, 15); }

    // SIGKILL → WIFSIGNALED(SIGKILL).
    if kill(child, SIGKILL) != 0 { return fail_at(name, 16); }
    let st = match reap(child) { Some(s) => s, None => return fail_at(name, 17) };
    close(rfd);
    if !wifsignaled(st) || wtermsig(st) != SIGKILL { return fail_at(name, 18); }
    report(name, true)
}

// ── 3b. SIGKILL ends a *stopped* process without a SIGCONT first ───────────

unsafe fn test_kill_while_stopped() -> bool {
    let name = b"kill_while_stopped\0";
    let mut fds: [c_int; 2] = [0; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return fail_at(name, 1); }
    let (rfd, wfd) = (fds[0], fds[1]);
    if !set_nonblock(rfd) { return fail_at(name, 2); }

    let child = fork();
    if child < 0 { return fail_at(name, 3); }
    if child == 0 { close(rfd); ticking_child(wfd); }
    close(wfd);
    if !pipe_becomes_readable(rfd) { kill(child, SIGKILL); reap(child); return fail_at(name, 4); }

    if kill(child, SIGSTOP) != 0 { kill(child, SIGKILL); reap(child); return fail_at(name, 5); }
    let mut st: c_int = 0;
    if waitpid(child, &mut st, WUNTRACED) != child || !wifstopped(st) {
        kill(child, SIGKILL); reap(child); return fail_at(name, 6);
    }
    drain(rfd);
    if !pipe_stays_silent(rfd) { kill(child, SIGKILL); reap(child); return fail_at(name, 7); }

    if kill(child, SIGKILL) != 0 { return fail_at(name, 8); }
    let st = match reap(child) { Some(s) => s, None => return fail_at(name, 9) };
    close(rfd);
    if !wifsignaled(st) || wtermsig(st) != SIGKILL { return fail_at(name, 10); }
    report(name, true)
}

// ── 4. Blocking signal waits park instead of spinning ─────────────────────
//
// `sigsuspend` and `sigtimedwait` used to yield-spin in the kernel until a
// signal arrived, pinning a CPU for the whole wait. They now park on the poll
// wait-channel, so the waiter's CPU time (CLOCK_THREAD_CPUTIME_ID, which is
// real accounting) must be a small fraction of its wall time, and the wake
// must still be prompt: a child delivers SIGUSR1 after ~300 ms and the wait
// returns inside [300, 400) ms. The timeout path is checked the same way.

const CLOCK_MONOTONIC: c_int = 1;
const CLOCK_THREAD_CPUTIME_ID: c_int = 3;
const EINTR: c_int = 4;
const SENDER_DELAY_MS: i64 = 300;

static USR1_SEEN: AtomicUsize = AtomicUsize::new(0);

extern "C" fn usr1_count(_sig: c_int) {
    USR1_SEEN.fetch_add(1, Ordering::SeqCst);
}

unsafe fn clock_ms(clk: c_int) -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(clk, &mut ts);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

/// Fork a child that sends `sig` to the caller after `delay_ms`; its pid.
unsafe fn delayed_sender(target: pid_t, sig: c_int, delay_ms: i64) -> pid_t {
    let child = fork();
    if child == 0 {
        let ts = timespec { tv_sec: delay_ms / 1000, tv_nsec: (delay_ms % 1000) * 1_000_000 };
        nanosleep(&ts, core::ptr::null_mut());
        kill(target, sig);
        _exit(0);
    }
    child
}

/// The parked-wait contract: woke within the window, and spent well under a
/// quarter of that wall time on a CPU (a spin spends nearly all of it).
fn parked_ok(wall_ms: i64, cpu_ms: i64, lo: i64, hi: i64) -> bool {
    wall_ms >= lo && wall_ms < hi && cpu_ms * 4 < wall_ms
}

unsafe fn report_timing(name: &[u8], wall_ms: i64, cpu_ms: i64) {
    // "<name>: wall=NNNms cpu=NNNms"
    write(1, name.as_ptr(), name.len() - 1);
    let mut line = *b": wall=0000ms cpu=0000ms\0";
    let mut put = |off: usize, v: i64| {
        let v = v.clamp(0, 9999) as u32;
        line[off]     = b'0' + (v / 1000 % 10) as u8;
        line[off + 1] = b'0' + (v / 100 % 10) as u8;
        line[off + 2] = b'0' + (v / 10 % 10) as u8;
        line[off + 3] = b'0' + (v % 10) as u8;
    };
    put(7, wall_ms);
    put(18, cpu_ms);
    puts(line.as_ptr());
}

unsafe fn test_sigsuspend_parks() -> bool {
    let name = b"sigsuspend_parks\0";
    let usr1: sigset_t = 1u64 << (SIGUSR1 - 1);
    let act = zeroed_sigaction(Some(usr1_count));
    if sigaction(SIGUSR1, &act, core::ptr::null_mut()) != 0 { return fail_at(name, 1); }
    if sigprocmask(SIG_BLOCK, &usr1, core::ptr::null_mut()) != 0 { return fail_at(name, 2); }
    USR1_SEEN.store(0, Ordering::SeqCst);

    let child = delayed_sender(getpid(), SIGUSR1, SENDER_DELAY_MS);
    if child < 0 { return fail_at(name, 3); }

    let none: sigset_t = 0;
    let wall0 = clock_ms(CLOCK_MONOTONIC);
    let cpu0 = clock_ms(CLOCK_THREAD_CPUTIME_ID);
    let r = sigsuspend(&none);
    let e = errno();
    let wall = clock_ms(CLOCK_MONOTONIC) - wall0;
    let cpu = clock_ms(CLOCK_THREAD_CPUTIME_ID) - cpu0;
    report_timing(name, wall, cpu);

    let reaped = reap(child).is_some();
    // sigsuspend always returns -1/EINTR, the handler ran exactly once, and
    // the caller's mask (SIGUSR1 blocked) is back in place.
    if r != -1 || e != EINTR { return fail_at(name, 4); }
    if USR1_SEEN.load(Ordering::SeqCst) != 1 { return fail_at(name, 5); }
    let mut cur: sigset_t = 0;
    if sigprocmask(SIG_BLOCK, core::ptr::null(), &mut cur) != 0 || cur & usr1 == 0 { return fail_at(name, 6); }
    if !reaped { return fail_at(name, 7); }
    if !parked_ok(wall, cpu, SENDER_DELAY_MS, SENDER_DELAY_MS + 100) { return fail_at(name, 8); }
    sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());
    report(name, true)
}

unsafe fn test_sigtimedwait_parks() -> bool {
    let name = b"sigtimedwait_parks\0";
    let usr1: sigset_t = 1u64 << (SIGUSR1 - 1);
    if sigprocmask(SIG_BLOCK, &usr1, core::ptr::null_mut()) != 0 { return fail_at(name, 1); }

    let child = delayed_sender(getpid(), SIGUSR1, SENDER_DELAY_MS);
    if child < 0 { return fail_at(name, 2); }

    // A full 128-byte siginfo_t: si_signo at +0, si_pid at +16.
    let mut info = [0u64; 16];
    let timeout = timespec { tv_sec: 2, tv_nsec: 0 };
    let wall0 = clock_ms(CLOCK_MONOTONIC);
    let cpu0 = clock_ms(CLOCK_THREAD_CPUTIME_ID);
    let r = sigtimedwait(&usr1, info.as_mut_ptr() as *mut c_void, &timeout);
    let wall = clock_ms(CLOCK_MONOTONIC) - wall0;
    let cpu = clock_ms(CLOCK_THREAD_CPUTIME_ID) - cpu0;
    report_timing(name, wall, cpu);

    let reaped = reap(child).is_some();
    if r != SIGUSR1 { return fail_at(name, 3); }
    let p = info.as_ptr() as *const u8;
    let si_signo = core::ptr::read_unaligned(p as *const i32);
    let si_pid = core::ptr::read_unaligned(p.add(16) as *const i32);
    if si_signo != SIGUSR1 { return fail_at(name, 4); }
    if si_pid != child { return fail_at(name, 5); }
    if !reaped { return fail_at(name, 6); }
    if !parked_ok(wall, cpu, SENDER_DELAY_MS, SENDER_DELAY_MS + 100) { return fail_at(name, 7); }
    sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());
    report(name, true)
}

unsafe fn test_sigtimedwait_timeout() -> bool {
    let name = b"sigtimedwait_timeout\0";
    let usr1: sigset_t = 1u64 << (SIGUSR1 - 1);
    if sigprocmask(SIG_BLOCK, &usr1, core::ptr::null_mut()) != 0 { return fail_at(name, 1); }

    // Nobody sends: the wait must end at the deadline with EAGAIN, having
    // parked (not re-probed at full speed) for the whole 200 ms.
    let timeout = timespec { tv_sec: 0, tv_nsec: 200_000_000 };
    let wall0 = clock_ms(CLOCK_MONOTONIC);
    let cpu0 = clock_ms(CLOCK_THREAD_CPUTIME_ID);
    let r = sigtimedwait(&usr1, core::ptr::null_mut(), &timeout);
    let e = errno();
    let wall = clock_ms(CLOCK_MONOTONIC) - wall0;
    let cpu = clock_ms(CLOCK_THREAD_CPUTIME_ID) - cpu0;
    report_timing(name, wall, cpu);

    if r != -1 || e != EAGAIN { return fail_at(name, 2); }
    if !parked_ok(wall, cpu, 200, 300) { return fail_at(name, 3); }
    sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());
    report(name, true)
}
