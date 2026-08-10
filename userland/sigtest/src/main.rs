//! sigtest — standalone regression coverage for LeandrOS signal handling
//! (TODO.md Phase 2), including a direct regression test for the
//! `SigAction` field-order bug found and fixed while building the Phase 8
//! `timertest` suite: `sched::task::SigAction`'s `mask`/`restorer` fields
//! were swapped relative to the real POSIX `struct sigaction` layout, so
//! `sa_restorer` (a function pointer) landed in the kernel's `mask` slot
//! and `sa_mask` (0) landed in `restorer` — any handler that actually ran
//! crashed the process on return through a NULL trampoline.
//!
//! Initializes via relibc_start_v1 (same as pthreadtest/timertest) so TLS
//! is set up properly — errno and the sigaction SA_RESTORER trampoline
//! both need it.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `sig_main` returns the number of failures as the exit code.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

type c_int = i32;
type pid_t = c_int;

pub type sigset_t = u64;

#[repr(C)]
pub struct sigaction {
    pub sa_handler: Option<extern "C" fn(c_int)>,
    pub sa_flags: c_int,
    pub sa_restorer: Option<unsafe extern "C" fn()>,
    pub sa_mask: sigset_t,
}

const SIGKILL: c_int = 9;
const SIGUSR1: c_int = 10;
const SIGUSR2: c_int = 12;
const SIGCHLD: c_int = 17;

const SIG_BLOCK: c_int = 0;
const SIG_UNBLOCK: c_int = 1;

const SA_RESTORER: c_int = 0x0400_0000;
const SA_SIGINFO:  c_int = 0x0000_0004;

const WNOHANG: c_int = 1;

// siginfo_t.si_code values — see `sched/src/task.rs`.
const SI_USER:     c_int = 0;
const SI_TKILL:    c_int = -6;
const CLD_EXITED:  c_int = 1;
const CLD_KILLED:  c_int = 2;

/// The leading, architecture-independent part of LP64 Linux's `siginfo_t`:
/// three ints, four bytes of padding that align the `_sifields` union to 8,
/// then the `_kill`/`_sigchld` members. x86-64 and AArch64 share it, so one
/// declaration covers both. The trailing bytes of the 128-byte struct are not
/// modelled because nothing here reads them.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct siginfo_t {
    pub si_signo:  c_int,
    pub si_errno:  c_int,
    pub si_code:   c_int,
    pub _pad0:     c_int,
    pub si_pid:    pid_t,
    pub si_uid:    u32,
    pub si_status: c_int,
}

/// `struct signalfd_siginfo` field offsets (Linux `<sys/signalfd.h>`). The
/// record is 128 bytes; only the fields the kernel fills are named.
mod ssi {
    pub const SIGNO:  usize = 0;
    pub const CODE:   usize = 8;
    pub const PID:    usize = 12;
    pub const UID:    usize = 16;
    pub const STATUS: usize = 40;
}

#[cfg(target_arch = "x86_64")]
mod nr { pub const SIGNALFD4: i64 = 289; }
#[cfg(target_arch = "aarch64")]
mod nr { pub const SIGNALFD4: i64 = 74; }

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
    pub fn _exit(status: i32) -> !;

    pub fn getpid() -> pid_t;
    pub fn getuid() -> u32;
    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn raise(sig: c_int) -> c_int;
    pub fn sigaction(sig: c_int, act: *const sigaction, oact: *mut sigaction) -> c_int;
    pub fn sigprocmask(how: c_int, set: *const sigset_t, oset: *mut sigset_t) -> c_int;
    pub fn sigpending(set: *mut sigset_t) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;

    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;

    // signalfd4 has no relibc C wrapper — go straight through the raw syscall
    // entry point, exactly as epolltest does.
    pub fn syscall(sysno: c_long, ...) -> c_long;
}

type c_long = i64;
type time_t = i64;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    pub tv_sec:  time_t,
    pub tv_nsec: c_long,
}

/// Sleep ~10 ms. Every handshake in the siginfo tests below is a bounded poll
/// on an atomic the handler sets, rather than a bare sleep-and-hope.
unsafe fn nap() {
    let ts = timespec { tv_sec: 0, tv_nsec: 10_000_000 };
    nanosleep(&ts, core::ptr::null_mut());
}

/// Poll `f` up to ~2 s. Returns false on timeout, so a missed signal fails the
/// check instead of hanging the suite.
unsafe fn wait_until(f: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if f() { return true; }
        nap();
    }
    f()
}

/// Reap `child` without blocking. A plain `waitpid(..., 0)` is legitimately
/// interruptible here — these tests all have a live SIGCHLD handler, which is
/// precisely the condition that makes wait4 return EINTR — so poll instead of
/// leaving the outcome to whether the signal beat the syscall.
unsafe fn reap(child: pid_t) {
    for _ in 0..200 {
        if waitpid(child, core::ptr::null_mut(), WNOHANG) == child { return; }
        nap();
    }
}

/// relibc's own `SIG_IGN` sentinel (`header/signal/mod.rs`) is `pub(crate)`
/// and not exported, but its value (1, matching the kernel's
/// `sched::task::SigAction.handler == 1` convention) is part of the public
/// POSIX ABI, so hardcoding it here is legitimate rather than guessing.
fn sig_ign() -> Option<extern "C" fn(c_int)> {
    unsafe { core::mem::transmute::<usize, Option<extern "C" fn(c_int)>>(1) }
}

fn zeroed_sigaction(handler: Option<extern "C" fn(c_int)>) -> sigaction {
    sigaction { sa_handler: handler, sa_flags: 0, sa_restorer: None, sa_mask: 0 }
}

// ── Assembly entry point (identical to pthreadtest's/timertest's) ──────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset sig_main",
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
    "   adrp x1, sig_main",
    "   add x1, x1, :lo12:sig_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn sig_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    if !test_sigaction_struct_roundtrip() { failures += 1; }
    if !test_signal_delivery_and_return() { failures += 1; }
    if !test_two_signals_distinct_handlers() { failures += 1; }
    if !test_sigprocmask_blocks_and_defers() { failures += 1; }
    if !test_sig_ign_default_disposition() { failures += 1; }
    if !test_raise_delivers_signal() { failures += 1; }
    if !test_siginfo_origin_kill_vs_raise() { failures += 1; }
    if !test_sigchld_siginfo_exited() { failures += 1; }
    if !test_sigchld_siginfo_killed() { failures += 1; }
    if !test_signalfd_agrees_with_handler() { failures += 1; }
    if !test_shared_handoff_keeps_payloads_apart() { failures += 1; }

    puts(b"--- sigtest done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── 1. SigAction struct field-order regression ──────────────────────────────
//
// Reads a disposition back via `sigaction(sig, NULL, &old)` and checks that
// `sa_mask` comes back as the exact bitmask we set, not some other field's
// bytes. If `mask`/`restorer` were still swapped in the kernel, `old.sa_mask`
// would read back as relibc's `__restore_rt` trampoline address instead —
// a large, non-power-of-two pointer value, trivially distinguishable from
// our deliberately small, known mask.

extern "C" fn roundtrip_handler(_sig: c_int) {}

unsafe fn test_sigaction_struct_roundtrip() -> bool {
    let name = b"sigaction_struct_roundtrip\0";

    let distinctive_mask: u64 = 1u64 << (SIGUSR2 - 1); // block SIGUSR2 during the handler
    let mut act = zeroed_sigaction(Some(roundtrip_handler));
    act.sa_mask = distinctive_mask;
    if sigaction(SIGUSR1, &act, core::ptr::null_mut()) != 0 { return report(name, false); }

    let mut old = core::mem::zeroed::<sigaction>();
    if sigaction(SIGUSR1, core::ptr::null(), &mut old) != 0 { return report(name, false); }

    // Compare raw addresses rather than `==` on the fn-pointer Option — the
    // compiler warns that fn-pointer equality isn't guaranteed meaningful
    // in general (identical-code-folding etc.), and what this test actually
    // cares about is the literal bytes that round-tripped through the
    // kernel, which is exactly what an address comparison checks.
    let handler_ok = old.sa_handler.map(|f| f as *const () as usize)
        == Some(roundtrip_handler as *const () as usize);
    let mask_ok = old.sa_mask == distinctive_mask;
    // relibc's Sys::sigaction always injects SA_RESTORER + a real trampoline
    // pointer — confirm it landed in `sa_restorer`, not silently in `sa_mask`.
    let restorer_ok = (old.sa_flags & SA_RESTORER) != 0 && old.sa_restorer.is_some();

    report(name, handler_ok && mask_ok && restorer_ok)
}

// ── 2. Real end-to-end delivery + return through the sigreturn trampoline ──
//
// This is exactly the path that used to crash (EL0 fault, ELR=0): a
// corrupted `sa_restorer` meant execution never came back here after the
// handler ran.

static DELIVERY_COUNT: AtomicI32 = AtomicI32::new(0);
static RESUMED_AFTER_KILL: AtomicI32 = AtomicI32::new(0);

extern "C" fn delivery_handler(_sig: c_int) {
    DELIVERY_COUNT.fetch_add(1, Ordering::SeqCst);
}

unsafe fn test_signal_delivery_and_return() -> bool {
    let name = b"signal_delivery_and_return\0";
    DELIVERY_COUNT.store(0, Ordering::SeqCst);
    RESUMED_AFTER_KILL.store(0, Ordering::SeqCst);

    let act = zeroed_sigaction(Some(delivery_handler));
    if sigaction(SIGUSR1, &act, core::ptr::null_mut()) != 0 { return report(name, false); }

    let pid = getpid();
    if kill(pid, SIGUSR1) != 0 { return report(name, false); }
    // Reaching here at all (rather than a fault) proves the sigreturn
    // trampoline round-tripped correctly.
    RESUMED_AFTER_KILL.store(1, Ordering::SeqCst);

    report(name, DELIVERY_COUNT.load(Ordering::SeqCst) == 1
        && RESUMED_AFTER_KILL.load(Ordering::SeqCst) == 1)
}

// ── 3. Two signals, two handlers, no cross-wiring ───────────────────────────

static COUNT_USR1: AtomicI32 = AtomicI32::new(0);
static COUNT_USR2: AtomicI32 = AtomicI32::new(0);

extern "C" fn handler_usr1(_sig: c_int) { COUNT_USR1.fetch_add(1, Ordering::SeqCst); }
extern "C" fn handler_usr2(_sig: c_int) { COUNT_USR2.fetch_add(1, Ordering::SeqCst); }

unsafe fn test_two_signals_distinct_handlers() -> bool {
    let name = b"two_signals_distinct_handlers\0";
    COUNT_USR1.store(0, Ordering::SeqCst);
    COUNT_USR2.store(0, Ordering::SeqCst);

    let act1 = zeroed_sigaction(Some(handler_usr1));
    let act2 = zeroed_sigaction(Some(handler_usr2));
    if sigaction(SIGUSR1, &act1, core::ptr::null_mut()) != 0 { return report(name, false); }
    if sigaction(SIGUSR2, &act2, core::ptr::null_mut()) != 0 { return report(name, false); }

    let pid = getpid();
    kill(pid, SIGUSR1);
    kill(pid, SIGUSR2);

    report(name, COUNT_USR1.load(Ordering::SeqCst) == 1 && COUNT_USR2.load(Ordering::SeqCst) == 1)
}

// ── 4. sigprocmask defers delivery; sigpending reports it; unblock delivers ─

static COUNT_BLOCKED: AtomicU32 = AtomicU32::new(0);

extern "C" fn blocked_handler(_sig: c_int) { COUNT_BLOCKED.fetch_add(1, Ordering::SeqCst); }

unsafe fn test_sigprocmask_blocks_and_defers() -> bool {
    let name = b"sigprocmask_blocks_and_defers\0";
    COUNT_BLOCKED.store(0, Ordering::SeqCst);

    let act = zeroed_sigaction(Some(blocked_handler));
    if sigaction(SIGUSR1, &act, core::ptr::null_mut()) != 0 { return report(name, false); }

    let mask: sigset_t = 1u64 << (SIGUSR1 - 1);
    if sigprocmask(SIG_BLOCK, &mask, core::ptr::null_mut()) != 0 { return report(name, false); }

    let pid = getpid();
    kill(pid, SIGUSR1);
    // Blocked: the handler must not have run yet.
    let deferred = COUNT_BLOCKED.load(Ordering::SeqCst) == 0;

    let mut pending: sigset_t = 0;
    if sigpending(&mut pending) != 0 { return report(name, false); }
    let reported_pending = (pending & mask) != 0;

    if sigprocmask(SIG_UNBLOCK, &mask, core::ptr::null_mut()) != 0 { return report(name, false); }
    // Unblocking a pending signal delivers it synchronously on this
    // syscall's own return path, before control comes back here.
    let delivered_on_unblock = COUNT_BLOCKED.load(Ordering::SeqCst) == 1;

    report(name, deferred && reported_pending && delivered_on_unblock)
}

// ── 5. SIG_IGN: no handler call, no default-terminate ───────────────────────

unsafe fn test_sig_ign_default_disposition() -> bool {
    let name = b"sig_ign_default_disposition\0";

    let act = zeroed_sigaction(sig_ign());
    if sigaction(SIGUSR2, &act, core::ptr::null_mut()) != 0 { return report(name, false); }

    let pid = getpid();
    kill(pid, SIGUSR2);
    // SIGUSR2's SIG_DFL action is terminate (not in the kernel's
    // default-ignore set) -- reaching here at all proves SIG_IGN, not
    // SIG_DFL, was actually honored.
    report(name, true)
}

// ── 6. raise() regression: TKILL had no kernel dispatch arm ─────────────────
//
// raise() resolves to Sys::raise(), which calls GETTID then issues a raw
// TKILL syscall (nr 130 on AArch64, 200 on x86-64) against its own tid.
// The kernel's dispatch table had every other thread-signal syscall
// (KILL, TGKILL) wired up but no TKILL arm at all, so every call fell
// through to the default `_ => -38` (ENOSYS) case: raise() always failed,
// even though kill(getpid(), sig) — exercised by the tests above — worked
// fine. This test calls raise() directly rather than kill(), so it fails
// (return != 0) if the TKILL arm regresses.

static COUNT_RAISED: AtomicI32 = AtomicI32::new(0);

extern "C" fn raise_handler(_sig: c_int) { COUNT_RAISED.fetch_add(1, Ordering::SeqCst); }

unsafe fn test_raise_delivers_signal() -> bool {
    let name = b"raise_delivers_signal\0";
    COUNT_RAISED.store(0, Ordering::SeqCst);

    let act = zeroed_sigaction(Some(raise_handler));
    if sigaction(SIGUSR1, &act, core::ptr::null_mut()) != 0 { return report(name, false); }

    let raise_status = raise(SIGUSR1);

    report(name, raise_status == 0 && COUNT_RAISED.load(Ordering::SeqCst) == 1)
}

// ── 7-11. Per-signal siginfo ────────────────────────────────────────────────
//
// Delivered siginfo used to carry `si_signo` and nothing else, so every
// handler read `si_code == 0` — which is `SI_USER`, a real and specific
// answer ("someone called kill()"), not a blank. A SIGCHLD handler could not
// tell `CLD_EXITED` from `CLD_KILLED`, and `signalfd` reported the same
// nothing. These five checks pin the payload down at both ends: the handler
// and the signalfd, for the same event.

/// Install a three-argument `SA_SIGINFO` handler.
///
/// Both architectures pass `(signo, &siginfo, &ucontext)` in the first three
/// argument registers regardless of `SA_SIGINFO`, but the flag is what a real
/// program sets, so set it — the transmute is only needed because this file
/// declares `sa_handler` with the one-argument POSIX prototype.
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

/// One recorded delivery. Written by a handler, read by the test body.
struct Recorder {
    n:      AtomicI32,
    code:   AtomicI32,
    pid:    AtomicI32,
    uid:    AtomicU32,
    status: AtomicI32,
}

impl Recorder {
    const fn new() -> Recorder {
        Recorder {
            n:      AtomicI32::new(0),
            code:   AtomicI32::new(0),
            pid:    AtomicI32::new(0),
            uid:    AtomicU32::new(0),
            status: AtomicI32::new(0),
        }
    }
    fn reset(&self) {
        self.n.store(0, Ordering::SeqCst);
        self.code.store(0, Ordering::SeqCst);
        self.pid.store(0, Ordering::SeqCst);
        self.uid.store(0, Ordering::SeqCst);
        self.status.store(0, Ordering::SeqCst);
    }
    unsafe fn record(&self, info: *const siginfo_t) {
        let i = &*info;
        self.code.store(i.si_code, Ordering::SeqCst);
        self.pid.store(i.si_pid, Ordering::SeqCst);
        self.uid.store(i.si_uid, Ordering::SeqCst);
        self.status.store(i.si_status, Ordering::SeqCst);
        self.n.fetch_add(1, Ordering::SeqCst);
    }
    fn fired(&self) -> bool { self.n.load(Ordering::SeqCst) > 0 }
}

// ── 7. si_code distinguishes kill(2) from raise() ───────────────────────────
//
// `SI_USER` vs `SI_TKILL` is the distinction that makes `si_code == 0`
// ambiguous in the first place: 0 is a real value meaning "a process sent
// this with kill()", so a kernel that fills nothing is not silent, it is
// asserting something. raise() must not look like an external kill.

static ORIGIN: Recorder = Recorder::new();

extern "C" fn origin_handler(_sig: c_int, info: *const siginfo_t, _uc: *mut c_void) {
    unsafe { ORIGIN.record(info); }
}

unsafe fn test_siginfo_origin_kill_vs_raise() -> bool {
    let name = b"siginfo_origin_kill_vs_raise\0";
    if !install_siginfo(SIGUSR1, origin_handler) { return report(name, false); }
    let me = getpid();

    ORIGIN.reset();
    if kill(me, SIGUSR1) != 0 { return report(name, false); }
    if !wait_until(|| ORIGIN.fired()) { return report(name, false); }
    let kill_ok = ORIGIN.code.load(Ordering::SeqCst) == SI_USER
        && ORIGIN.pid.load(Ordering::SeqCst) == me
        && ORIGIN.uid.load(Ordering::SeqCst) == getuid();

    ORIGIN.reset();
    if raise(SIGUSR1) != 0 { return report(name, false); }
    if !wait_until(|| ORIGIN.fired()) { return report(name, false); }
    let raise_ok = ORIGIN.code.load(Ordering::SeqCst) == SI_TKILL
        && ORIGIN.pid.load(Ordering::SeqCst) == me;

    report(name, kill_ok && raise_ok)
}

// ── 8/9. SIGCHLD carries how the child died ─────────────────────────────────

static CHILD: Recorder = Recorder::new();

extern "C" fn child_handler(_sig: c_int, info: *const siginfo_t, _uc: *mut c_void) {
    unsafe { CHILD.record(info); }
}

unsafe fn test_sigchld_siginfo_exited() -> bool {
    let name = b"sigchld_siginfo_exited\0";
    if !install_siginfo(SIGCHLD, child_handler) { return report(name, false); }
    CHILD.reset();

    let child = fork();
    if child < 0 { return report(name, false); }
    if child == 0 { _exit(42); }

    let fired = wait_until(|| CHILD.fired());
    reap(child);
    if !fired { return report(name, false); }

    report(name,
        CHILD.code.load(Ordering::SeqCst)   == CLD_EXITED
     && CHILD.pid.load(Ordering::SeqCst)    == child
     && CHILD.status.load(Ordering::SeqCst) == 42
     && CHILD.uid.load(Ordering::SeqCst)    == getuid())
}

unsafe fn test_sigchld_siginfo_killed() -> bool {
    let name = b"sigchld_siginfo_killed\0";
    if !install_siginfo(SIGCHLD, child_handler) { return report(name, false); }
    CHILD.reset();

    let child = fork();
    if child < 0 { return report(name, false); }
    if child == 0 { loop { nap(); } }

    // Give the child time to reach its sleep loop; killing a task that has not
    // been scheduled yet is legal but makes the test depend on that.
    nap();
    if kill(child, SIGKILL) != 0 { return report(name, false); }

    let fired = wait_until(|| CHILD.fired());
    reap(child);
    if !fired { return report(name, false); }

    // si_status is the *signal*, not `128 + signal`: that shell convention is
    // what the exit code carries, and conflating the two is exactly how
    // WIFEXITED once read true for a killed process.
    report(name,
        CHILD.code.load(Ordering::SeqCst)   == CLD_KILLED
     && CHILD.pid.load(Ordering::SeqCst)    == child
     && CHILD.status.load(Ordering::SeqCst) == SIGKILL)
}

// ── 10. signalfd reports the same payload the handler would have seen ───────

unsafe fn test_signalfd_agrees_with_handler() -> bool {
    let name = b"signalfd_agrees_with_handler\0";

    let mask: sigset_t = 1u64 << (SIGCHLD - 1);
    if sigprocmask(SIG_BLOCK, &mask, core::ptr::null_mut()) != 0 { return report(name, false); }

    let sfd = syscall(nr::SIGNALFD4, -1i64, &mask as *const sigset_t as *const c_void, 8i64, 0i64) as i32;
    if sfd < 0 {
        sigprocmask(SIG_UNBLOCK, &mask, core::ptr::null_mut());
        return report(name, false);
    }

    let child = fork();
    if child < 0 {
        close(sfd);
        sigprocmask(SIG_UNBLOCK, &mask, core::ptr::null_mut());
        return report(name, false);
    }
    if child == 0 { _exit(33); }

    let mut buf = [0u8; 128];
    let mut n = 0isize;
    for _ in 0..200 {
        n = read(sfd, buf.as_mut_ptr(), 128);
        if n == 128 { break; }
        nap();
    }

    let g32 = |o: usize| i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let ok = n == 128
        && g32(ssi::SIGNO)  == SIGCHLD
        && g32(ssi::CODE)   == CLD_EXITED
        && g32(ssi::PID)    == child
        && g32(ssi::UID)    == getuid() as i32
        && g32(ssi::STATUS) == 33;

    reap(child);
    close(sfd);
    sigprocmask(SIG_UNBLOCK, &mask, core::ptr::null_mut());
    report(name, ok)
}

// ── 11. The shared_signal_pending hand-off must not swap payloads ───────────
//
// A process-directed signal that every thread currently blocks is parked on
// the thread-group leader's `shared_signal_pending`, and claimed later by
// whichever thread unblocks it first. The payload has to make that trip with
// its own bit and no other. If the claim copied the leader's whole
// `signal_info` array instead of the claimed slots, the claiming thread's
// *other* pending signals would silently inherit the leader's payloads — a
// SIGUSR2 delivered carrying a SIGCHLD's `si_code`, which is strictly worse
// than the zeros this all replaced and reads like a userspace bug.
//
// The setup makes that failure observable rather than merely possible:
//
//   1. the leader takes a SIGUSR2 by kill(), leaving SI_USER in *its* slot 12;
//   2. both threads block SIGUSR2 and SIGCHLD;
//   3. the worker raise()s SIGUSR2 — SI_TKILL, in the *worker's* slot 12;
//   4. the leader kill()s the process with SIGCHLD, which nobody can take, so
//      it parks on the leader with SI_USER in slot 17;
//   5. the worker unblocks SIGCHLD alone, claiming exactly one bit;
//   6. the worker unblocks SIGUSR2 and reads its si_code.
//
// Correct: SI_TKILL, the value the worker stored in step 3. A whole-array
// copy in step 5: SI_USER, the leader's step-1 residue. The two differ.

static HANDOFF_CHLD: Recorder = Recorder::new();
static HANDOFF_USR:  Recorder = Recorder::new();
static WORKER_ARMED: AtomicI32 = AtomicI32::new(0);
static CHLD_PARKED:  AtomicI32 = AtomicI32::new(0);

extern "C" fn handoff_chld_handler(_sig: c_int, info: *const siginfo_t, _uc: *mut c_void) {
    unsafe { HANDOFF_CHLD.record(info); }
}
extern "C" fn handoff_usr_handler(_sig: c_int, info: *const siginfo_t, _uc: *mut c_void) {
    unsafe { HANDOFF_USR.record(info); }
}

extern "C" fn handoff_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        // Block explicitly rather than relying on inheritance. POSIX says a
        // new thread starts with the creating thread's signal mask, but this
        // kernel's `clone_thread` does not copy `signal_mask` at all, so a
        // worker here starts with everything unblocked. Leaning on inheritance
        // would silently defeat the whole check: with SIGCHLD unblocked in
        // this thread, `deliver_signal_process` would hand it straight over
        // and the parked-then-claimed path under test would never run.
        let both: sigset_t = (1u64 << (SIGCHLD - 1)) | (1u64 << (SIGUSR2 - 1));
        sigprocmask(SIG_BLOCK, &both, core::ptr::null_mut());

        // Step 3: a thread-directed SIGUSR2, blocked, so it stays pending on
        // this thread with SI_TKILL in this thread's slot.
        raise(SIGUSR2);
        WORKER_ARMED.store(1, Ordering::SeqCst);

        if !wait_until(|| CHLD_PARKED.load(Ordering::SeqCst) == 1) {
            return core::ptr::null_mut();
        }

        // Step 5: claim SIGCHLD and nothing else.
        let chld: sigset_t = 1u64 << (SIGCHLD - 1);
        sigprocmask(SIG_UNBLOCK, &chld, core::ptr::null_mut());
        wait_until(|| HANDOFF_CHLD.fired());

        // Step 6: now let the worker's own SIGUSR2 through.
        let usr: sigset_t = 1u64 << (SIGUSR2 - 1);
        sigprocmask(SIG_UNBLOCK, &usr, core::ptr::null_mut());
        wait_until(|| HANDOFF_USR.fired());
    }
    core::ptr::null_mut()
}

unsafe fn test_shared_handoff_keeps_payloads_apart() -> bool {
    let name = b"shared_handoff_keeps_payloads_apart\0";
    let me = getpid();

    if !install_siginfo(SIGCHLD, handoff_chld_handler) { return fail_at(name, 1); }
    if !install_siginfo(SIGUSR2, handoff_usr_handler)  { return fail_at(name, 2); }
    HANDOFF_CHLD.reset();
    HANDOFF_USR.reset();
    WORKER_ARMED.store(0, Ordering::SeqCst);
    CHLD_PARKED.store(0, Ordering::SeqCst);

    // Step 1: leave SI_USER in the *leader's* SIGUSR2 slot. This is the value
    // a whole-array copy would smuggle onto the worker.
    if kill(me, SIGUSR2) != 0 { return fail_at(name, 3); }
    if !wait_until(|| HANDOFF_USR.fired()) {
        let mut cur: sigset_t = 0;
        sigprocmask(SIG_BLOCK, core::ptr::null(), &mut cur);
        let mut pend: sigset_t = 0;
        sigpending(&mut pend);
        let mut disp = core::mem::zeroed::<sigaction>();
        sigaction(SIGUSR2, core::ptr::null(), &mut disp);
        write(1, b"handoff: mask=".as_ptr(), 14);
        put_i32(cur as i32);
        write(1, b" pend=".as_ptr(), 6);
        put_i32(pend as i32);
        write(1, b" disp=".as_ptr(), 6);
        put_i32(disp.sa_handler.map(|f| f as *const () as usize).unwrap_or(0) as i32);
        write(1, b" flags=".as_ptr(), 7);
        put_i32(disp.sa_flags);
        write(1, b"\n".as_ptr(), 1);
        return fail_at(name, 4);
    }
    let leader_saw_si_user = HANDOFF_USR.code.load(Ordering::SeqCst) == SI_USER;
    HANDOFF_USR.reset();

    // Step 2: block both, in the leader; the worker inherits this mask.
    let both: sigset_t = (1u64 << (SIGCHLD - 1)) | (1u64 << (SIGUSR2 - 1));
    if sigprocmask(SIG_BLOCK, &both, core::ptr::null_mut()) != 0 { return fail_at(name, 5); }

    let mut worker: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut worker, core::ptr::null(), handoff_worker, core::ptr::null_mut()) != 0 {
        sigprocmask(SIG_UNBLOCK, &both, core::ptr::null_mut());
        return fail_at(name, 6);
    }

    let armed = wait_until(|| WORKER_ARMED.load(Ordering::SeqCst) == 1);

    // Step 4: nobody can take this, so it parks on the leader.
    if armed { kill(me, SIGCHLD); }
    // Positive evidence that it really parked, rather than being handed to a
    // thread that turned out to have it unblocked: this is the *leader*
    // asking, and the leader has SIGCHLD masked, so its own `signal_pending`
    // cannot hold it. sigpending(2) here reports thread-pending OR the
    // leader's `shared_signal_pending` — so a SIGCHLD visible from this thread
    // can only be the parked one. Without this the check would still pass on a
    // kernel that never parked anything, proving nothing about the hand-off.
    let mut pend: sigset_t = 0;
    sigpending(&mut pend);
    let parked = pend & (1u64 << (SIGCHLD - 1)) != 0;
    CHLD_PARKED.store(1, Ordering::SeqCst);

    pthread_join(worker, core::ptr::null_mut());
    sigprocmask(SIG_UNBLOCK, &both, core::ptr::null_mut());

    // Printed unconditionally: when this check fails, *which* of the six
    // observations went wrong is the entire diagnosis, and a bare FAIL says
    // nothing about whether the payload crossed or the hand-off never ran.
    write(1, b"handoff: armed=".as_ptr(), 15);
    put_i32(armed as i32);
    write(1, b" parked=".as_ptr(), 8);
    put_i32(parked as i32);
    write(1, b" leader_si_user=".as_ptr(), 16);
    put_i32(leader_saw_si_user as i32);
    write(1, b" chld_n=".as_ptr(), 8);
    put_i32(HANDOFF_CHLD.n.load(Ordering::SeqCst));
    write(1, b" chld_code=".as_ptr(), 11);
    put_i32(HANDOFF_CHLD.code.load(Ordering::SeqCst));
    write(1, b" chld_pid=".as_ptr(), 10);
    put_i32(HANDOFF_CHLD.pid.load(Ordering::SeqCst));
    write(1, b" usr_n=".as_ptr(), 7);
    put_i32(HANDOFF_USR.n.load(Ordering::SeqCst));
    write(1, b" usr_code=".as_ptr(), 10);
    put_i32(HANDOFF_USR.code.load(Ordering::SeqCst));
    write(1, b" usr_pid=".as_ptr(), 9);
    put_i32(HANDOFF_USR.pid.load(Ordering::SeqCst));
    write(1, b" me=".as_ptr(), 4);
    put_i32(me);
    write(1, b"\n".as_ptr(), 1);

    let chld_ok = HANDOFF_CHLD.fired()
        && HANDOFF_CHLD.code.load(Ordering::SeqCst) == SI_USER
        && HANDOFF_CHLD.pid.load(Ordering::SeqCst)  == me;
    // The discriminating assertion.
    let usr_ok = HANDOFF_USR.fired()
        && HANDOFF_USR.code.load(Ordering::SeqCst) == SI_TKILL
        && HANDOFF_USR.pid.load(Ordering::SeqCst)  == me;

    report(name, armed && parked && leader_saw_si_user && chld_ok && usr_ok)
}

// ── Helper ──────────────────────────────────────────────────────────────────

/// Report a failure together with *where* it happened. The hand-off check has
/// eight ways to bail before it reaches its assertions, and "FAIL" alone does
/// not distinguish "the payload crossed" from "the setup never ran".
unsafe fn fail_at(name: &[u8], step: c_int) -> bool {
    write(1, b"handoff: bailed at step ".as_ptr(), 24);
    put_i32(step);
    write(1, b"\n".as_ptr(), 1);
    report(name, false)
}

/// Minimal signed-decimal writer — there is no printf in this suite.
unsafe fn put_i32(v: i32) {
    let mut buf = [0u8; 12];
    let mut n = 0;
    let neg = v < 0;
    let mut u = if neg { (v as i64).unsigned_abs() } else { v as u64 };
    if u == 0 { buf[n] = b'0'; n += 1; }
    while u > 0 { buf[n] = b'0' + (u % 10) as u8; n += 1; u /= 10; }
    if neg { buf[n] = b'-'; n += 1; }
    let mut out = [0u8; 12];
    for i in 0..n { out[i] = buf[n - 1 - i]; }
    write(1, out.as_ptr(), n);
}

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr(), 7);
    } else {
        write(1, b": FAIL\n".as_ptr(), 7);
    }
    passed
}
