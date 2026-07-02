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

const SIGUSR1: c_int = 10;
const SIGUSR2: c_int = 12;

const SIG_BLOCK: c_int = 0;
const SIG_UNBLOCK: c_int = 1;

const SA_RESTORER: c_int = 0x0400_0000;

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn exit(status: i32) -> !;

    pub fn getpid() -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn sigaction(sig: c_int, act: *const sigaction, oact: *mut sigaction) -> c_int;
    pub fn sigprocmask(how: c_int, set: *const sigset_t, oset: *mut sigset_t) -> c_int;
    pub fn sigpending(set: *mut sigset_t) -> c_int;
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

// ── Helper ──────────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr(), 7);
    } else {
        write(1, b": FAIL\n".as_ptr(), 7);
    }
    passed
}
