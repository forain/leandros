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

const SIGALRM: c_int = 14;
const SIGKILL: c_int = 9;
const SIGUSR1: c_int = 10;
const SIGUSR2: c_int = 12;
const SIGCHLD: c_int = 17;
const SIGCONT: c_int = 18;
const SIGSTOP: c_int = 19;

const SIG_BLOCK: c_int = 0;
const SIG_UNBLOCK: c_int = 1;

const SA_RESTORER: c_int = 0x0400_0000;
const SA_SIGINFO:  c_int = 0x0000_0004;
const SA_RESTART:  c_int = 0x1000_0000;

const CLOCK_MONOTONIC: c_int = 1;

const WNOHANG: c_int = 1;
const EINTR:     c_int = 4;
const ETIMEDOUT: c_int = 110;

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
mod nr { pub const SIGNALFD4: i64 = 289; pub const FUTEX: i64 = 202; pub const GETTID: i64 = 186; pub const TGKILL: i64 = 234;
         pub const NANOSLEEP: i64 = 35; pub const PPOLL: i64 = 271; pub const PSELECT6: i64 = 270; }
#[cfg(target_arch = "aarch64")]
mod nr { pub const SIGNALFD4: i64 = 74; pub const FUTEX: i64 = 98; pub const GETTID: i64 = 178; pub const TGKILL: i64 = 131;
         pub const NANOSLEEP: i64 = 101; pub const PPOLL: i64 = 73; pub const PSELECT6: i64 = 72; }

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
    pub fn pipe(fds: *mut i32) -> i32;
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
    pub fn clock_gettime(clockid: c_int, tp: *mut timespec) -> c_int;

    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;
    pub fn sigaltstack(ss: *const stack_t, old: *mut stack_t) -> c_int;

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

unsafe fn now_ns() -> i64 {
    let mut ts = core::mem::zeroed::<timespec>();
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
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
pub unsafe extern "C" fn sig_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    // `sigtest futex N`: only the futex restart checks, N stress iterations.
    // `sigtest futexab N`: the SIGCHLD A/B diagnostic (see futex_sigchld_ab).
    if argc >= 2 {
        let arg = |i: isize| -> &[u8] {
            let p = *argv.offset(i);
            let mut n = 0;
            while *p.add(n) != 0 { n += 1; }
            core::slice::from_raw_parts(p, n)
        };
        let iters = if argc >= 3 {
            arg(2).iter().fold(0i32, |a, &c| if c.is_ascii_digit() { a * 10 + (c - b'0') as i32 } else { a })
        } else { 20 };
        match arg(1) {
            b"futex" => {
                if !test_futex_wait_signal_restart() { failures += 1; }
                if !test_futex_wait_restart_stress(iters) { failures += 1; }
                if !test_futex_wake_beats_restart(iters) { failures += 1; }
                if !test_futex_ignored_signal_keeps_waiting() { failures += 1; }
                if !test_futex_timed_signal_stress(iters) { failures += 1; }
                if !test_futex_wait_bitset_signal() { failures += 1; }
                if !test_timed_wait_stop_resumes_remainder() { failures += 1; }
                puts(b"--- sigtest futex done ---\n\0".as_ptr());
                return failures;
            }
            b"stoprem" => {
                if !test_timed_wait_stop_resumes_remainder() { failures += 1; }
                puts(b"--- sigtest stoprem done ---\n\0".as_ptr());
                return failures;
            }
            b"pollmask" => {
                if !test_ppoll_pselect_sigmask() { failures += 1; }
                if !test_futex_wait_bitset_signal() { failures += 1; }
                if !test_timed_wait_stop_resumes_remainder() { failures += 1; }
                puts(b"--- sigtest pollmask done ---\n\0".as_ptr());
                return failures;
            }
            b"futexab" => {
                futex_sigchld_ab(iters);
                puts(b"--- sigtest futexab done ---\n\0".as_ptr());
                return 0;
            }
            _ => {}
        }
    }

    if !test_sigaction_struct_roundtrip() { failures += 1; }
    if !test_signal_delivery_and_return() { failures += 1; }
    if !test_handler_preserves_vector_regs() { failures += 1; }
    if !test_two_signals_distinct_handlers() { failures += 1; }
    if !test_sigprocmask_blocks_and_defers() { failures += 1; }
    if !test_sig_ign_default_disposition() { failures += 1; }
    if !test_raise_delivers_signal() { failures += 1; }
    if !test_siginfo_origin_kill_vs_raise() { failures += 1; }
    if !test_sigchld_siginfo_exited() { failures += 1; }
    if !test_sigchld_siginfo_killed() { failures += 1; }
    if !test_signalfd_agrees_with_handler() { failures += 1; }
    if !test_shared_handoff_keeps_payloads_apart() { failures += 1; }
    if !test_futex_wait_signal_restart() { failures += 1; }
    if !test_futex_wait_restart_stress(20) { failures += 1; }
    if !test_futex_wake_beats_restart(20) { failures += 1; }
    if !test_futex_ignored_signal_keeps_waiting() { failures += 1; }
    if !test_futex_timed_signal_stress(5) { failures += 1; }
    if !test_futex_wait_bitset_signal() { failures += 1; }
    if !test_timed_wait_stop_resumes_remainder() { failures += 1; }
    if !test_ppoll_pselect_sigmask() { failures += 1; }
    if !test_stack_overflow_sigsegv_on_altstack() { failures += 1; }

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

// ── 2b. A handler must not leak into the interrupted code's vector registers
//
// The signal frame has to carry the FP/SIMD state (aarch64 fpsimd_context,
// x86-64 FXSAVE area) and rt_sigreturn has to put it back: handlers are
// ordinary compiled code and use q/xmm registers freely. The kill(2) below is
// issued from inside one asm block with a known pattern live in the vector
// registers, so the signal is delivered on that syscall's return, the handler
// overwrites every vector register, and the block reads them back after
// sigreturn. Before the fix they came back holding the handler's pattern.

static VREG_HANDLER_RAN: AtomicI32 = AtomicI32::new(0);

extern "C" fn vreg_clobber_handler(_sig: c_int) {
    VREG_HANDLER_RAN.fetch_add(1, Ordering::SeqCst);
    unsafe {
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!(
            "movi v0.16b, #0xa5", "movi v1.16b, #0xa5", "movi v2.16b, #0xa5", "movi v3.16b, #0xa5",
            "movi v4.16b, #0xa5", "movi v5.16b, #0xa5", "movi v6.16b, #0xa5", "movi v7.16b, #0xa5",
            "movi v8.16b, #0xa5", "movi v9.16b, #0xa5", "movi v10.16b, #0xa5", "movi v11.16b, #0xa5",
            "movi v12.16b, #0xa5", "movi v13.16b, #0xa5", "movi v14.16b, #0xa5", "movi v15.16b, #0xa5",
            "movi v16.16b, #0xa5", "movi v17.16b, #0xa5", "movi v18.16b, #0xa5", "movi v19.16b, #0xa5",
            "movi v20.16b, #0xa5", "movi v21.16b, #0xa5", "movi v22.16b, #0xa5", "movi v23.16b, #0xa5",
            "movi v24.16b, #0xa5", "movi v25.16b, #0xa5", "movi v26.16b, #0xa5", "movi v27.16b, #0xa5",
            "movi v28.16b, #0xa5", "movi v29.16b, #0xa5", "movi v30.16b, #0xa5", "movi v31.16b, #0xa5",
            out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _,
            out("v6") _, out("v7") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            out("v12") _, out("v13") _, out("v14") _, out("v15") _, out("v16") _, out("v17") _,
            out("v18") _, out("v19") _, out("v20") _, out("v21") _, out("v22") _, out("v23") _,
            out("v24") _, out("v25") _, out("v26") _, out("v27") _, out("v28") _, out("v29") _,
            out("v30") _, out("v31") _,
        );
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!(
            "pcmpeqd xmm0, xmm0", "pcmpeqd xmm1, xmm1", "pcmpeqd xmm2, xmm2", "pcmpeqd xmm3, xmm3",
            "pcmpeqd xmm4, xmm4", "pcmpeqd xmm5, xmm5", "pcmpeqd xmm6, xmm6", "pcmpeqd xmm7, xmm7",
            "pcmpeqd xmm8, xmm8", "pcmpeqd xmm9, xmm9", "pcmpeqd xmm10, xmm10", "pcmpeqd xmm11, xmm11",
            "pcmpeqd xmm12, xmm12", "pcmpeqd xmm13, xmm13", "pcmpeqd xmm14, xmm14", "pcmpeqd xmm15, xmm15",
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _, out("xmm4") _, out("xmm5") _,
            out("xmm6") _, out("xmm7") _, out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
        );
    }
}

unsafe fn test_handler_preserves_vector_regs() -> bool {
    let name = b"handler_preserves_vector_regs\0";
    VREG_HANDLER_RAN.store(0, Ordering::SeqCst);
    let act = zeroed_sigaction(Some(vreg_clobber_handler));
    if sigaction(SIGUSR1, &act, core::ptr::null_mut()) != 0 { return report(name, false); }
    let pid = getpid();
    // Each register i gets bytes (0x10 + i); read back into `out`.
    #[cfg(target_arch = "aarch64")]
    let (nregs, mut out) = (32usize, [0u8; 32 * 16]);
    #[cfg(target_arch = "x86_64")]
    let (nregs, mut out) = (16usize, [0u8; 16 * 16]);
    let mut pat = [0u8; 32 * 16];
    for r in 0..32 { for b in 0..16 { pat[r * 16 + b] = 0x10 + r as u8; } }
    #[cfg(target_arch = "aarch64")]
    core::arch::asm!(
        "ldp q0, q1, [{p}, #0]", "ldp q2, q3, [{p}, #32]", "ldp q4, q5, [{p}, #64]",
        "ldp q6, q7, [{p}, #96]", "ldp q8, q9, [{p}, #128]", "ldp q10, q11, [{p}, #160]",
        "ldp q12, q13, [{p}, #192]", "ldp q14, q15, [{p}, #224]", "ldp q16, q17, [{p}, #256]",
        "ldp q18, q19, [{p}, #288]", "ldp q20, q21, [{p}, #320]", "ldp q22, q23, [{p}, #352]",
        "ldp q24, q25, [{p}, #384]", "ldp q26, q27, [{p}, #416]", "ldp q28, q29, [{p}, #448]",
        "ldp q30, q31, [{p}, #480]",
        "mov x8, #129", // kill
        "svc #0",
        "stp q0, q1, [{o}, #0]", "stp q2, q3, [{o}, #32]", "stp q4, q5, [{o}, #64]",
        "stp q6, q7, [{o}, #96]", "stp q8, q9, [{o}, #128]", "stp q10, q11, [{o}, #160]",
        "stp q12, q13, [{o}, #192]", "stp q14, q15, [{o}, #224]", "stp q16, q17, [{o}, #256]",
        "stp q18, q19, [{o}, #288]", "stp q20, q21, [{o}, #320]", "stp q22, q23, [{o}, #352]",
        "stp q24, q25, [{o}, #384]", "stp q26, q27, [{o}, #416]", "stp q28, q29, [{o}, #448]",
        "stp q30, q31, [{o}, #480]",
        p = in(reg) pat.as_ptr(), o = in(reg) out.as_mut_ptr(),
        inout("x0") pid as usize => _, in("x1") SIGUSR1 as usize, out("x8") _,
        out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _,
        out("v6") _, out("v7") _, out("v8") _, out("v9") _, out("v10") _, out("v11") _,
        out("v12") _, out("v13") _, out("v14") _, out("v15") _, out("v16") _, out("v17") _,
        out("v18") _, out("v19") _, out("v20") _, out("v21") _, out("v22") _, out("v23") _,
        out("v24") _, out("v25") _, out("v26") _, out("v27") _, out("v28") _, out("v29") _,
        out("v30") _, out("v31") _,
    );
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!(
        "movdqu xmm0, [{p} + 0]", "movdqu xmm1, [{p} + 16]", "movdqu xmm2, [{p} + 32]",
        "movdqu xmm3, [{p} + 48]", "movdqu xmm4, [{p} + 64]", "movdqu xmm5, [{p} + 80]",
        "movdqu xmm6, [{p} + 96]", "movdqu xmm7, [{p} + 112]", "movdqu xmm8, [{p} + 128]",
        "movdqu xmm9, [{p} + 144]", "movdqu xmm10, [{p} + 160]", "movdqu xmm11, [{p} + 176]",
        "movdqu xmm12, [{p} + 192]", "movdqu xmm13, [{p} + 208]", "movdqu xmm14, [{p} + 224]",
        "movdqu xmm15, [{p} + 240]",
        "mov eax, 62", // kill
        "syscall",
        "movdqu [{o} + 0], xmm0", "movdqu [{o} + 16], xmm1", "movdqu [{o} + 32], xmm2",
        "movdqu [{o} + 48], xmm3", "movdqu [{o} + 64], xmm4", "movdqu [{o} + 80], xmm5",
        "movdqu [{o} + 96], xmm6", "movdqu [{o} + 112], xmm7", "movdqu [{o} + 128], xmm8",
        "movdqu [{o} + 144], xmm9", "movdqu [{o} + 160], xmm10", "movdqu [{o} + 176], xmm11",
        "movdqu [{o} + 192], xmm12", "movdqu [{o} + 208], xmm13", "movdqu [{o} + 224], xmm14",
        "movdqu [{o} + 240], xmm15",
        p = in(reg) pat.as_ptr(), o = in(reg) out.as_mut_ptr(),
        in("rdi") pid as usize, in("rsi") SIGUSR1 as usize, out("rax") _, out("rcx") _, out("r11") _,
        out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _, out("xmm4") _, out("xmm5") _,
        out("xmm6") _, out("xmm7") _, out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
        out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
    );
    let ran = VREG_HANDLER_RAN.load(Ordering::SeqCst) == 1;
    let mut bad = 0usize;
    for r in 0..nregs { if out[r * 16..r * 16 + 16] != pat[r * 16..r * 16 + 16] { bad += 1; } }
    if bad != 0 {
        puts(b"  vector registers came back holding the handler's values\0".as_ptr());
    }
    report(name, ran && bad == 0)
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
        // No sigprocmask here on purpose: this worker relies on starting with
        // the creating thread's mask (SIGCHLD and SIGUSR2 blocked, step 2),
        // as POSIX specifies and `clone_thread` now implements. The check
        // below only exercises the parked-then-claimed path if that
        // inheritance holds — with SIGCHLD unblocked here,
        // `deliver_signal_process` would hand it straight over. sigtest2's
        // `pthread_inherits_mask` tests the inheritance directly.

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

// ── 12. Main-stack overflow is a catchable SIGSEGV ──────────────────────────
//
// The main stack is a fixed 8 MiB demand-paged VMA with nothing mapped below
// it. Unbounded recursion must fault on the first page below it and deliver
// SIGSEGV (SEGV_MAPERR, si_addr just under the stack) to a handler running on
// the alternate stack — the path Rust's "has overflowed its stack" report and
// every sigaltstack-based overflow handler depend on.

#[repr(C)]
pub struct stack_t {
    pub ss_sp:    *mut c_void,
    pub ss_flags: c_int,
    pub ss_size:  usize,
}

const SIGSEGV:     c_int = 11;
const SA_ONSTACK:  c_int = 0x0800_0000;
const SEGV_MAPERR: c_int = 1;

static mut ALT_STACK: [u8; 65536] = [0; 65536];
static OVF_TOP: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

#[inline(never)]
unsafe fn recurse(depth: usize) -> usize {
    let mut buf = [0u8; 1024];
    // Escape the whole buffer, or SROA shrinks the frame to the two bytes used.
    core::hint::black_box(&mut buf);
    core::ptr::write_volatile(buf.as_mut_ptr(), depth as u8);
    core::ptr::write_volatile(buf.as_mut_ptr().add(1023), depth as u8);
    if depth == 0 { return 0; }
    let r = recurse(depth - 1);
    r + core::ptr::read_volatile(buf.as_ptr().add(1023)) as usize
}

extern "C" fn overflow_handler(_sig: c_int, info: *const siginfo_t, _uc: *mut c_void) {
    unsafe {
        let code = (*info).si_code;
        let addr = *((info as *const u8).add(16) as *const usize);
        let top = OVF_TOP.load(Ordering::SeqCst);
        // Where the handler runs: on the alternate stack, not the dead one.
        let here = &code as *const c_int as usize;
        let alt = core::ptr::addr_of!(ALT_STACK) as usize;
        let on_alt = here >= alt && here < alt + 65536;
        let depth = top.wrapping_sub(addr);
        let ok = code == SEGV_MAPERR && on_alt
            && depth > 7 * 1024 * 1024 && depth < 9 * 1024 * 1024;
        _exit(if ok { 42 } else if !on_alt { 44 } else { 43 });
    }
}

unsafe fn test_stack_overflow_sigsegv_on_altstack() -> bool {
    let name = b"stack_overflow_sigsegv_on_altstack\0";
    let child = fork();
    if child < 0 { return report(name, false); }
    if child == 0 {
        let marker = 0u8;
        OVF_TOP.store(&marker as *const u8 as usize, Ordering::SeqCst);
        let ss = stack_t {
            ss_sp: core::ptr::addr_of_mut!(ALT_STACK) as *mut c_void,
            ss_flags: 0,
            ss_size: 65536,
        };
        if sigaltstack(&ss, core::ptr::null_mut()) != 0 { _exit(50); }
        let mut act = zeroed_sigaction(Some(core::mem::transmute::<
            extern "C" fn(c_int, *const siginfo_t, *mut c_void),
            extern "C" fn(c_int),
        >(overflow_handler)));
        act.sa_flags = SA_SIGINFO | SA_ONSTACK;
        if sigaction(SIGSEGV, &act, core::ptr::null_mut()) != 0 { _exit(51); }
        let r = recurse(usize::MAX);
        _exit(if r == 7 { 52 } else { 53 });
    }
    let mut status: c_int = 0;
    let mut got = 0;
    for _ in 0..500 {
        got = waitpid(child, &mut status, WNOHANG);
        if got == child { break; }
        nap();
    }
    write(1, b"overflow: status=".as_ptr(), 17);
    put_i32(status);
    write(1, b"\n".as_ptr(), 1);
    // WIFEXITED && WEXITSTATUS == 42
    report(name, got == child && status & 0x7f == 0 && (status >> 8) & 0xff == 42)
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

// ── FUTEX_WAIT interrupted by a signal: restart vs. EINTR ───────────────────

static FUTEX_SIG_COUNT: AtomicI32 = AtomicI32::new(0);
extern "C" fn futex_sig_handler(_sig: c_int) { FUTEX_SIG_COUNT.fetch_add(1, Ordering::SeqCst); }

const FUTEX_WAIT:        c_long = 0;
const FUTEX_WAKE:        c_long = 1;
const FUTEX_WAIT_BITSET: c_long = 9;
const FUTEX_PRIVATE:     c_long = 128;
const FUTEX_BITSET_MATCH_ANY: u32 = !0;

/// The futex word every restart test waits on. 7 = "keep waiting".
static FWORD: AtomicU32 = AtomicU32::new(7);
/// Parameters for `futex_helper`, a sibling thread that signals the waiting
/// main thread with a thread-directed tgkill (so no other thread and no
/// child exit — no SIGCHLD — is involved) and then optionally wakes it.
static FH_TID:      AtomicI32 = AtomicI32::new(0);
static FH_SIG_MS:   AtomicI32 = AtomicI32::new(50);
/// < 0: never wake. 0: FUTEX_WAKE right after the signal WITHOUT changing the
/// word (races the signal wake). > 0: after this many more ms, set the word
/// to 8 and FUTEX_WAKE.
static FH_WAKE_MS:  AtomicI32 = AtomicI32::new(-1);
/// What the racing FUTEX_WAKE (FH_WAKE_MS == 0) returned.
static FH_WOKEN:    AtomicI32 = AtomicI32::new(0);
/// Signal the helper sends (SIGALRM unless a test says otherwise).
static FH_SIGNO:    AtomicI32 = AtomicI32::new(SIGALRM);

unsafe fn sleep_ms(ms: i32) {
    let ts = timespec { tv_sec: (ms / 1000) as i64, tv_nsec: (ms % 1000) as c_long * 1_000_000 };
    nanosleep(&ts, core::ptr::null_mut());
}

unsafe fn futex_wake_word() -> c_long {
    syscall(nr::FUTEX, FWORD.as_ptr() as c_long, FUTEX_WAKE | FUTEX_PRIVATE, 1 as c_long,
            0 as c_long, 0 as c_long, 0 as c_long)
}

extern "C" fn futex_helper(_: *mut c_void) -> *mut c_void {
    unsafe {
        // Never take the signal ourselves.
        let m: sigset_t = (1u64 << (SIGALRM - 1)) | (1u64 << (SIGCHLD - 1));
        sigprocmask(SIG_BLOCK, &m, core::ptr::null_mut());
        sleep_ms(FH_SIG_MS.load(Ordering::SeqCst));
        syscall(nr::TGKILL, getpid() as c_long, FH_TID.load(Ordering::SeqCst) as c_long,
                FH_SIGNO.load(Ordering::SeqCst) as c_long);
        let w = FH_WAKE_MS.load(Ordering::SeqCst);
        if w == 0 {
            FH_WOKEN.store(futex_wake_word() as i32, Ordering::SeqCst);
            // Whatever happened, end the wait eventually.
            sleep_ms(300);
            FWORD.store(8, Ordering::SeqCst);
            futex_wake_word();
        } else if w > 0 {
            sleep_ms(w);
            FWORD.store(8, Ordering::SeqCst);
            futex_wake_word();
        }
    }
    core::ptr::null_mut()
}

/// One FUTEX_WAIT on `FWORD` by the main thread with `futex_helper` running
/// alongside. Returns (r, elapsed_ns).
unsafe fn futex_signal_round(restart: bool, timeout_ms: i32, sig_ms: i32, wake_ms: i32) -> (c_long, i64) {
    futex_signal_round_op(FUTEX_WAIT, restart, timeout_ms, sig_ms, wake_ms)
}

/// `futex_signal_round` with the op chosen: FUTEX_WAIT (relative timeout) or
/// FUTEX_WAIT_BITSET (absolute CLOCK_MONOTONIC deadline `now + timeout_ms`).
unsafe fn futex_signal_round_op(op: c_long, restart: bool, timeout_ms: i32, sig_ms: i32, wake_ms: i32) -> (c_long, i64) {
    FUTEX_SIG_COUNT.store(0, Ordering::SeqCst);
    let act = sigaction {
        sa_handler: Some(futex_sig_handler),
        sa_flags: if restart { SA_RESTART } else { 0 },
        sa_restorer: None,
        sa_mask: 0,
    };
    if sigaction(SIGALRM, &act, core::ptr::null_mut()) != 0 { return (-9999, 0); }
    FWORD.store(7, Ordering::SeqCst);
    FH_TID.store(syscall(nr::GETTID) as i32, Ordering::SeqCst);
    FH_SIG_MS.store(sig_ms, Ordering::SeqCst);
    FH_WAKE_MS.store(wake_ms, Ordering::SeqCst);
    FH_WOKEN.store(-1, Ordering::SeqCst);
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), futex_helper, core::ptr::null_mut()) != 0 {
        return (-9998, 0);
    }
    let t0 = now_ns();
    let to = if op == FUTEX_WAIT_BITSET {
        let d = t0 + timeout_ms as i64 * 1_000_000;
        timespec { tv_sec: d / 1_000_000_000, tv_nsec: d % 1_000_000_000 }
    } else {
        timespec { tv_sec: (timeout_ms / 1000) as i64, tv_nsec: (timeout_ms % 1000) as c_long * 1_000_000 }
    };
    let top = if timeout_ms > 0 { &to as *const timespec as c_long } else { 0 };
    let r = syscall(nr::FUTEX, FWORD.as_ptr() as c_long, op | FUTEX_PRIVATE,
                    7 as c_long, top, 0 as c_long, FUTEX_BITSET_MATCH_ANY as c_long);
    let el = now_ns() - t0;
    pthread_join(th, core::ptr::null_mut());
    (r, el)
}

/// FUTEX_WAIT interrupted by a handled signal, Linux semantics (measured on
/// Linux 7.0, and what kernel/futex/waitwake.c does):
///   untimed + SA_RESTART    -> restarted transparently (-ERESTARTSYS)
///   untimed, no SA_RESTART  -> EINTR
///   timed, either way       -> EINTR (-ERESTART_RESTARTBLOCK: restarted only
///                              when no handler runs)
/// The signal is a tgkill from a sibling thread at ~50 ms; the untimed waits
/// are ended by that thread's FUTEX_WAKE at ~300 ms.
unsafe fn test_futex_wait_signal_restart() -> bool {
    let name = b"futex_wait_signal_restart\0";
    let mut ok = true;
    let one = |n| FUTEX_SIG_COUNT.load(Ordering::SeqCst) == n;

    let (r, el) = futex_signal_round(true, 0, 50, 250);
    let c = r == 0 && el >= 280_000_000 && one(1);
    if !c { write(1, b"  untimed+SA_RESTART r=".as_ptr(), 23); put_i32(r as i32); write(1, b" el_us=".as_ptr(), 7); put_i32((el / 1000) as i32); write(1, b"\n".as_ptr(), 1); }
    ok &= c;

    let (r, el) = futex_signal_round(false, 0, 50, 250);
    let c = r == -(EINTR as c_long) && el >= 30_000_000 && el < 250_000_000 && one(1);
    if !c { write(1, b"  untimed r=".as_ptr(), 12); put_i32(r as i32); write(1, b" el_us=".as_ptr(), 7); put_i32((el / 1000) as i32); write(1, b"\n".as_ptr(), 1); }
    ok &= c;

    for &restart in &[true, false] {
        let (r, el) = futex_signal_round(restart, 1000, 50, -1);
        let c = r == -(EINTR as c_long) && el >= 30_000_000 && el < 600_000_000 && one(1);
        if !c { write(1, b"  timed r=".as_ptr(), 10); put_i32(r as i32); write(1, b" el_us=".as_ptr(), 7); put_i32((el / 1000) as i32); write(1, b"\n".as_ptr(), 1); }
        ok &= c;
    }
    report(name, ok)
}

/// Stress the transparent-restart case (untimed + SA_RESTART) `iters` times:
/// signal at ~20 ms, genuine wake at ~100 ms. Any EINTR is a lost restart.
unsafe fn test_futex_wait_restart_stress(iters: i32) -> bool {
    let name = b"futex_wait_restart_stress\0";
    let (mut good, mut eintr, mut other) = (0i32, 0i32, 0i32);
    for i in 0..iters {
        let (r, el) = futex_signal_round(true, 0, 20, 80);
        if r == 0 && el >= 90_000_000 && FUTEX_SIG_COUNT.load(Ordering::SeqCst) == 1 { good += 1; continue; }
        if r == -(EINTR as c_long) { eintr += 1; } else { other += 1; }
        write(1, b"  restart_stress[".as_ptr(), 17);
        put_i32(i);
        write(1, b"] r=".as_ptr(), 4);
        put_i32(r as i32);
        write(1, b" el_us=".as_ptr(), 7);
        put_i32((el / 1000) as i32);
        write(1, b"\n".as_ptr(), 1);
    }
    write(1, b"  restart_stress: ok=".as_ptr(), 21);
    put_i32(good);
    write(1, b" eintr=".as_ptr(), 7);
    put_i32(eintr);
    write(1, b" other=".as_ptr(), 7);
    put_i32(other);
    write(1, b"\n".as_ptr(), 1);
    report(name, good == iters)
}

/// A FUTEX_WAKE racing the signal: when the wake reports it woke our waiter
/// (returned 1), the waiter must return 0 promptly — the wake was consumed.
/// Restarting it instead (the waiter being judged "interrupted" after a wake
/// had claimed it) would put it back to sleep with the wake lost; here it
/// would then only end at the helper's fallback wake ~300 ms later.
unsafe fn test_futex_wake_beats_restart(iters: i32) -> bool {
    let name = b"futex_wake_beats_restart\0";
    let (mut claimed, mut unclaimed, mut lost, mut bad) = (0i32, 0i32, 0i32, 0i32);
    for _ in 0..iters {
        let (r, el) = futex_signal_round(true, 0, 10, 0);
        let n = FH_WOKEN.load(Ordering::SeqCst);
        if n == 1 {
            claimed += 1;
            if r != 0 { bad += 1; } else if el >= 200_000_000 { lost += 1; }
        } else {
            unclaimed += 1;
            if r != 0 && r != -11 { bad += 1; }
        }
    }
    write(1, b"  wake_race: claimed=".as_ptr(), 21);
    put_i32(claimed);
    write(1, b" unclaimed=".as_ptr(), 11);
    put_i32(unclaimed);
    write(1, b" lost=".as_ptr(), 6);
    put_i32(lost);
    write(1, b" bad=".as_ptr(), 5);
    put_i32(bad);
    write(1, b"\n".as_ptr(), 1);
    report(name, lost == 0 && bad == 0)
}

/// A signal that is ignored (SIGCHLD at SIG_DFL, or SIG_IGN) must not end a
/// FUTEX_WAIT at all: no handler runs, so there is nothing to restart and no
/// reason for the wait to return. It used to come back as a spurious 0 at
/// the signal (the generic Blocked -> Ready wake).
unsafe fn test_futex_ignored_signal_keeps_waiting() -> bool {
    let name = b"futex_ignored_signal_keeps_waiting\0";
    let mut ok = true;
    for &ign in &[false, true] {
        let h = if ign { sig_ign() } else { None };
        sigaction(SIGCHLD, &zeroed_sigaction(h), core::ptr::null_mut());
        FH_SIGNO.store(SIGCHLD, Ordering::SeqCst);
        let (r, el) = futex_signal_round(true, 0, 20, 80);
        FH_SIGNO.store(SIGALRM, Ordering::SeqCst);
        let c = r == 0 && el >= 90_000_000 && FUTEX_SIG_COUNT.load(Ordering::SeqCst) == 0;
        if !c { write(1, b"  ignored r=".as_ptr(), 12); put_i32(r as i32); write(1, b" el_us=".as_ptr(), 7); put_i32((el / 1000) as i32); write(1, b"\n".as_ptr(), 1); }
        ok &= c;
    }
    report(name, ok)
}

/// Stress the TIMED FUTEX_WAIT against a signal at ~50 ms, `iters` times per
/// shape, with Linux's answers (re-measured on a Linux 7.2 host, 20/20 each):
///   * SA_RESTART handler, 1000 ms timeout -> EINTR at the signal, handler ran
///     once (-ERESTART_RESTARTBLOCK becomes EINTR whenever a handler runs);
///   * SIGCHLD at SIG_DFL (ignored), 350 ms timeout -> ETIMEDOUT at >= 350 ms,
///     no handler: the ignored signal neither ends nor shortens the wait.
/// Any other outcome (a 0, an EINTR for the ignored signal, an ETIMEDOUT for
/// the handled one) is a failure; load does not change either answer.
unsafe fn test_futex_timed_signal_stress(iters: i32) -> bool {
    let name = b"futex_timed_signal_stress\0";
    let (mut h_ok, mut h_bad, mut i_ok, mut i_bad) = (0i32, 0i32, 0i32, 0i32);
    for i in 0..iters {
        let (r, el) = futex_signal_round(true, 1000, 50, -1);
        if r == -(EINTR as c_long) && el >= 30_000_000 && el < 1_000_000_000
            && FUTEX_SIG_COUNT.load(Ordering::SeqCst) == 1 { h_ok += 1; } else {
            h_bad += 1;
            write(1, b"  timed_stress handler[".as_ptr(), 23); put_i32(i);
            write(1, b"] r=".as_ptr(), 4); put_i32(r as i32);
            write(1, b" el_us=".as_ptr(), 7); put_i32((el / 1000) as i32);
            write(1, b"\n".as_ptr(), 1);
        }

        sigaction(SIGCHLD, &zeroed_sigaction(None), core::ptr::null_mut());
        FH_SIGNO.store(SIGCHLD, Ordering::SeqCst);
        let (r, el) = futex_signal_round(true, 350, 50, -1);
        FH_SIGNO.store(SIGALRM, Ordering::SeqCst);
        if r == -110 && el >= 350_000_000 && FUTEX_SIG_COUNT.load(Ordering::SeqCst) == 0 { i_ok += 1; } else {
            i_bad += 1;
            write(1, b"  timed_stress ignored[".as_ptr(), 23); put_i32(i);
            write(1, b"] r=".as_ptr(), 4); put_i32(r as i32);
            write(1, b" el_us=".as_ptr(), 7); put_i32((el / 1000) as i32);
            write(1, b"\n".as_ptr(), 1);
        }
    }
    write(1, b"  timed_stress: handler_eintr=".as_ptr(), 30); put_i32(h_ok);
    write(1, b" handler_bad=".as_ptr(), 13); put_i32(h_bad);
    write(1, b" ignored_etimedout=".as_ptr(), 19); put_i32(i_ok);
    write(1, b" ignored_bad=".as_ptr(), 13); put_i32(i_bad);
    write(1, b"\n".as_ptr(), 1);
    report(name, h_bad == 0 && i_bad == 0)
}

/// Diagnostic (`sigtest futexab N`), the shape of the original stress test:
/// a forked CHILD kill()s the parent with SIGALRM at ~20 ms and then exits,
/// so a SIGCHLD follows. Run once with a SIGCHLD handler installed WITHOUT
/// SA_RESTART (what the earlier sigtest cases leave behind) and once with
/// SIGCHLD at SIG_DFL. A SIGCHLD that lands while the restarted wait is
/// parked legitimately ends it with EINTR (Linux does the same); with SIG_DFL
/// nothing may.
unsafe fn futex_sigchld_ab(iters: i32) {
    for &with_handler in &[true, false] {
        if with_handler {
            install_siginfo(SIGCHLD, handoff_chld_handler);
        } else {
            sigaction(SIGCHLD, &zeroed_sigaction(None), core::ptr::null_mut());
        }
        let (mut good, mut eintr, mut other) = (0i32, 0i32, 0i32);
        let parent = getpid();
        for _ in 0..iters {
            FUTEX_SIG_COUNT.store(0, Ordering::SeqCst);
            let act = sigaction { sa_handler: Some(futex_sig_handler), sa_flags: SA_RESTART, sa_restorer: None, sa_mask: 0 };
            sigaction(SIGALRM, &act, core::ptr::null_mut());
            FWORD.store(7, Ordering::SeqCst);
            let child = fork();
            if child == 0 {
                sleep_ms(20);
                kill(parent, SIGALRM);
                _exit(0);
            }
            let t0 = now_ns();
            // The genuine wake: a sibling thread (both signals masked) at ~150 ms.
            let mut th: pthread_t = core::ptr::null_mut();
            pthread_create(&mut th, core::ptr::null(), ab_waker, core::ptr::null_mut());
            let r = syscall(nr::FUTEX, FWORD.as_ptr() as c_long, FUTEX_WAIT | FUTEX_PRIVATE,
                            7 as c_long, 0 as c_long, 0 as c_long, 0 as c_long);
            let el = now_ns() - t0;
            pthread_join(th, core::ptr::null_mut());
            reap(child);
            if r == 0 && el >= 140_000_000 { good += 1; }
            else if r == -(EINTR as c_long) { eintr += 1; }
            else { other += 1; }
        }
        write(1, if with_handler { b"  futexab sigchld=handler ".as_ptr() } else { b"  futexab sigchld=SIG_DFL ".as_ptr() }, 26);
        write(1, b"ok=".as_ptr(), 3);
        put_i32(good);
        write(1, b" eintr=".as_ptr(), 7);
        put_i32(eintr);
        write(1, b" other=".as_ptr(), 7);
        put_i32(other);
        write(1, b"\n".as_ptr(), 1);
    }
}

extern "C" fn ab_waker(_: *mut c_void) -> *mut c_void {
    unsafe {
        let m: sigset_t = (1u64 << (SIGALRM - 1)) | (1u64 << (SIGCHLD - 1));
        sigprocmask(SIG_BLOCK, &m, core::ptr::null_mut());
        sleep_ms(150);
        FWORD.store(8, Ordering::SeqCst);
        futex_wake_word();
    }
    core::ptr::null_mut()
}

/// FUTEX_WAIT_BITSET interrupted by a handled signal answers exactly like
/// FUTEX_WAIT (Linux futex_wait: -ERESTARTSYS untimed, -ERESTART_RESTARTBLOCK
/// timed). It used to return a spurious 0 at the signal in every case:
///   untimed + SA_RESTART -> restarted, ends at the genuine wake (~300 ms)
///   untimed, no SA_RESTART -> EINTR at the signal
///   timed (absolute), either way -> EINTR at the signal
unsafe fn test_futex_wait_bitset_signal() -> bool {
    let name = b"futex_wait_bitset_signal\0";
    let mut ok = true;
    let one = || FUTEX_SIG_COUNT.load(Ordering::SeqCst) == 1;
    let cases: [(bool, i32, i32, &[u8]); 4] = [
        (true, 0, 250, b"  bitset untimed+SA_RESTART"),
        (false, 0, 250, b"  bitset untimed           "),
        (true, 1000, -1, b"  bitset timed+SA_RESTART  "),
        (false, 1000, -1, b"  bitset timed             "),
    ];
    for &(restart, timeout_ms, wake_ms, label) in &cases {
        let (r, el) = futex_signal_round_op(FUTEX_WAIT_BITSET, restart, timeout_ms, 50, wake_ms);
        let c = if restart && timeout_ms == 0 {
            r == 0 && el >= 280_000_000 && one()
        } else {
            r == -(EINTR as c_long) && el >= 30_000_000 && el < 250_000_000 && one()
        };
        write(1, label.as_ptr(), label.len());
        write(1, b" r=".as_ptr(), 3); put_i32(r as i32);
        write(1, b" el_ms=".as_ptr(), 7); put_i32((el / 1_000_000) as i32);
        write(1, if c { b" ok\n".as_ptr() } else { b" BAD\n".as_ptr() }, if c { 4 } else { 5 });
        ok &= c;
    }
    report(name, ok)
}

// ── Timed waits across a stop/continue: resume the REMAINDER ────────────────

/// A relative-timeout wait interrupted by a signal for which no handler runs
/// — here SIGSTOP then SIGCONT from a forked child — is restarted
/// transparently, and on Linux the restarted wait keeps the ORIGINAL
/// deadline (restart_block / -ERESTART_RESTARTBLOCK, or the written-back
/// timeout for select). The bug: the restart re-read the relative timeout and
/// waited the full interval again.
///
/// Each syscall waits 500 ms in a forked child, which the parent stops at
/// `stop_ms` and continues at `cont_ms`.
///   * 150 -> 250: restart must end at ~500 ms (old: ~750 ms).
///   * 300 -> 700: the deadline passes while stopped, so the restart must
///     return at once, at ~700 ms (old: ~1200 ms). That the elapsed time is
///     >= 700 also proves the stop really took effect.
/// nanosleep used to fail with EINTR at the continue instead (Linux has
/// restarted it since 2.6.24); ppoll/pselect6 likewise.
unsafe fn test_timed_wait_stop_resumes_remainder() -> bool {
    let name = b"timed_wait_stop_resumes_remainder\0";
    // SIGCHLD at SIG_DFL: the child's exit must not end the wait with a
    // handler's EINTR (earlier cases leave a SA_RESTART-less handler).
    let mut old_chld = zeroed_sigaction(None);
    sigaction(SIGCHLD, &zeroed_sigaction(None), &mut old_chld);
    let mut ok = true;
    let labels: [&[u8]; 7] = [b"  futex    ", b"  nanosleep", b"  ppoll    ", b"  pselect6 ",
                              b"  futex_bs ", b"  ppoll_msk", b"  poll     "];
    // poll(2) (nr 7) exists on x86_64 only.
    let kinds = if cfg!(target_arch = "x86_64") { 7 } else { 6 };
    for &(stop_ms, cont_ms, lo, hi) in &[(150i32, 250i32, 480i64, 680i64), (300, 700, 680, 950)] {
        for which in 0..kinds {
            // The waiter is a forked child that the parent stops: stopping
            // sigtest itself would hand the terminal back to the shell.
            let mut fds = [0 as c_int; 2];
            pipe(fds.as_mut_ptr());
            FWORD.store(7, Ordering::SeqCst);
            let child = fork();
            if child == 0 {
                let ts = timespec { tv_sec: 0, tv_nsec: 500_000_000 };
                let t0 = now_ns();
                let r = match which {
                    0 => syscall(nr::FUTEX, FWORD.as_ptr() as c_long, FUTEX_WAIT | FUTEX_PRIVATE,
                                 7 as c_long, &ts as *const timespec as c_long, 0 as c_long, 0 as c_long),
                    1 => syscall(nr::NANOSLEEP, &ts as *const timespec as c_long, 0 as c_long),
                    2 => syscall(nr::PPOLL, 0 as c_long, 0 as c_long, &ts as *const timespec as c_long,
                                 0 as c_long, 8 as c_long),
                    3 => syscall(nr::PSELECT6, 0 as c_long, 0 as c_long, 0 as c_long, 0 as c_long,
                                 &ts as *const timespec as c_long, 0 as c_long),
                    4 => {
                        // Absolute deadline t0 + 500 ms.
                        let d = t0 + 500_000_000;
                        let abs = timespec { tv_sec: d / 1_000_000_000, tv_nsec: d % 1_000_000_000 };
                        syscall(nr::FUTEX, FWORD.as_ptr() as c_long, FUTEX_WAIT_BITSET | FUTEX_PRIVATE,
                                7 as c_long, &abs as *const timespec as c_long, 0 as c_long,
                                FUTEX_BITSET_MATCH_ANY as c_long)
                    }
                    5 => {
                        // With a temporary mask (SIGUSR2 added): the restart
                        // after the continue must reinstall it, and the
                        // caller's mask must be back afterwards.
                        let before = cur_mask();
                        let temp = before | sbit(SIGUSR2);
                        let r = syscall(nr::PPOLL, 0 as c_long, 0 as c_long, &ts as *const timespec as c_long,
                                        &temp as *const sigset_t as c_long, 8 as c_long);
                        if cur_mask() != before { -7777 } else { r }
                    }
                    _ => poll_ms(500),
                };
                let out: [i64; 2] = [r as i64, now_ns() - t0];
                write(fds[1], out.as_ptr() as *const u8, 16);
                _exit(0);
            }
            close(fds[1]);
            sleep_ms(stop_ms);
            kill(child, SIGSTOP);
            sleep_ms(cont_ms - stop_ms);
            kill(child, SIGCONT);
            let mut out: [i64; 2] = [-9999, 0];
            read(fds[0], out.as_mut_ptr() as *mut u8, 16);
            close(fds[0]);
            let mut st = 0;
            waitpid(child, &mut st, 0);
            let r = out[0];
            let el_ms = out[1] / 1_000_000;
            // raw syscall() hands back the kernel's -errno.
            let want_ok = if which == 0 || which == 4 { r == -(ETIMEDOUT as i64) } else { r == 0 };
            let c = want_ok && el_ms >= lo && el_ms <= hi;
            write(1, labels[which].as_ptr(), 11);
            write(1, b" stop/cont=".as_ptr(), 11);
            put_i32(stop_ms); write(1, b"/".as_ptr(), 1); put_i32(cont_ms);
            write(1, b" r=".as_ptr(), 3); put_i32(r as i32);
            write(1, b" el_ms=".as_ptr(), 7); put_i32(el_ms as i32);
            write(1, if c { b" ok\n".as_ptr() } else { b" BAD\n".as_ptr() }, if c { 4 } else { 5 });
            ok &= c;
        }
    }
    sigaction(SIGCHLD, &old_chld, core::ptr::null_mut());
    report(name, ok)
}

/// poll(NULL, 0, ms) through the x86_64-only poll(2) (nr 7).
#[cfg(target_arch = "x86_64")]
unsafe fn poll_ms(ms: i32) -> c_long { syscall(7, 0 as c_long, 0 as c_long, ms as c_long) }
#[cfg(not(target_arch = "x86_64"))]
unsafe fn poll_ms(_ms: i32) -> c_long { -38 }

// ── ppoll/pselect6: temporary sigmask, remaining-time write-back ───────────
//
// Linux (fs/select.c): ppoll and pselect6 install their sigmask argument for
// the duration of the wait (set_user_sigmask) and put the caller's mask back
// on return — unless the wait ended in EINTR, in which case the handler runs
// under the TEMPORARY mask and the old one comes back through the handler
// frame (restore_saved_sigmask_unless). That is the race-free pselect
// pattern: block SIGUSR1, test the flag, then wait with SIGUSR1 unblocked.
// select, pselect6 and ppoll also write the time not slept back into the
// caller's timeout (poll_select_finish), whatever the result, except for a
// zero timeout.

static PM_COUNT: AtomicI32 = AtomicI32::new(0);
static PM_MASK_IN_HANDLER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
extern "C" fn pm_handler(_sig: c_int) {
    unsafe {
        let mut cur: sigset_t = 0;
        sigprocmask(SIG_BLOCK, core::ptr::null(), &mut cur);
        PM_MASK_IN_HANDLER.store(cur, Ordering::SeqCst);
    }
    PM_COUNT.fetch_add(1, Ordering::SeqCst);
}

fn sbit(s: c_int) -> u64 { 1u64 << (s - 1) }

unsafe fn cur_mask() -> sigset_t {
    let mut m: sigset_t = 0;
    sigprocmask(SIG_BLOCK, core::ptr::null(), &mut m);
    m
}

#[repr(C)]
struct pollfd { fd: c_int, events: i16, revents: i16 }

/// One masked wait. `kind` 0 = ppoll, 1 = pselect6. `fd` < 0: no fds.
/// `mask` None passes a NULL sigmask.
unsafe fn pm_wait(kind: i32, fd: c_int, ts: *mut timespec, mask: Option<&sigset_t>) -> c_long {
    let mp = mask.map(|m| m as *const sigset_t as c_long).unwrap_or(0);
    if kind == 0 {
        let mut pfd = pollfd { fd, events: 1, revents: 0 };
        let (p, n) = if fd >= 0 { (&mut pfd as *mut pollfd as c_long, 1) } else { (0, 0) };
        syscall(nr::PPOLL, p, n as c_long, ts as c_long, mp, 8 as c_long)
    } else {
        let mut set = [0u64; 16];
        let nfds = if fd >= 0 { set[fd as usize / 64] |= 1u64 << (fd % 64); fd + 1 } else { 0 };
        let sp = if fd >= 0 { set.as_mut_ptr() as c_long } else { 0 };
        // pselect6's sixth argument: { const sigset_t *ss; size_t ss_len }.
        let data: [u64; 2] = [mp as u64, 8];
        let dp = if mask.is_some() { data.as_ptr() as c_long } else { 0 };
        syscall(nr::PSELECT6, nfds as c_long, sp, 0 as c_long, 0 as c_long, ts as c_long, dp)
    }
}

unsafe fn ts_ms(ts: &timespec) -> i64 { ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000 }

unsafe fn pm_line(label: &[u8], r: c_long, el_ms: i64, rem_ms: i64, c: bool) {
    write(1, label.as_ptr(), label.len());
    write(1, b" r=".as_ptr(), 3); put_i32(r as i32);
    write(1, b" el_ms=".as_ptr(), 7); put_i32(el_ms as i32);
    write(1, b" rem_ms=".as_ptr(), 8); put_i32(rem_ms as i32);
    write(1, if c { b" ok\n".as_ptr() } else { b" BAD\n".as_ptr() }, if c { 4 } else { 5 });
}

unsafe fn test_ppoll_pselect_sigmask() -> bool {
    let name = b"ppoll_pselect_sigmask\0";
    let mut ok = true;
    let mut old_chld = zeroed_sigaction(None);
    sigaction(SIGCHLD, &zeroed_sigaction(None), &mut old_chld);
    let act = sigaction { sa_handler: Some(pm_handler), sa_flags: 0, sa_restorer: None, sa_mask: 0 };
    sigaction(SIGUSR1, &act, core::ptr::null_mut());
    let entry = cur_mask();
    let usr1 = sbit(SIGUSR1);
    let usr2 = sbit(SIGUSR2);
    for kind in 0..2 {
        let pfx: &[u8] = if kind == 0 { b"  ppoll   " } else { b"  pselect6" };
        // SIGUSR1 blocked outside the wait; the wait unblocks it and blocks
        // SIGUSR2 instead (a marker the handler must observe).
        let orig = (entry | usr1) & !usr2;
        let temp = (orig | usr2) & !usr1;

        // 1. A signal arriving during the wait interrupts it: EINTR, handler
        //    ran under the temporary mask, caller's mask back afterwards.
        sigprocmask(2 /* SIG_SETMASK */, &orig, core::ptr::null_mut());
        PM_COUNT.store(0, Ordering::SeqCst);
        PM_MASK_IN_HANDLER.store(0, Ordering::SeqCst);
        FH_TID.store(syscall(nr::GETTID) as i32, Ordering::SeqCst);
        FH_SIG_MS.store(60, Ordering::SeqCst);
        FH_WAKE_MS.store(-1, Ordering::SeqCst);
        FH_SIGNO.store(SIGUSR1, Ordering::SeqCst);
        let mut th: pthread_t = core::ptr::null_mut();
        pthread_create(&mut th, core::ptr::null(), futex_helper, core::ptr::null_mut());
        let mut ts = timespec { tv_sec: 1, tv_nsec: 0 };
        let t0 = now_ns();
        let r = pm_wait(kind, -1, &mut ts, Some(&temp));
        let el = (now_ns() - t0) / 1_000_000;
        pthread_join(th, core::ptr::null_mut());
        FH_SIGNO.store(SIGALRM, Ordering::SeqCst);
        let after = cur_mask();
        let hm = PM_MASK_IN_HANDLER.load(Ordering::SeqCst);
        let rem = ts_ms(&ts);
        let c = r == -(EINTR as c_long) && el < 500 && PM_COUNT.load(Ordering::SeqCst) == 1
            && hm & usr2 != 0 && after == orig;
        write(1, pfx.as_ptr(), pfx.len()); pm_line(b" arrives ", r, el, rem, c);
        ok &= c;
        // 2 (item 2). The time not slept was written back: ~940 ms of 1000.
        let c = rem > 700 && rem < 1000;
        write(1, pfx.as_ptr(), pfx.len()); pm_line(b" wb_eintr", r, el, rem, c);
        ok &= c;
        // Consume a USR1 the unfixed kernel leaves pending.
        sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());
        sigprocmask(2, &orig, core::ptr::null_mut());

        // 3. Pending before the call: delivered at once.
        PM_COUNT.store(0, Ordering::SeqCst);
        raise(SIGUSR1);
        let mut ts = timespec { tv_sec: 1, tv_nsec: 0 };
        let t0 = now_ns();
        let r = pm_wait(kind, -1, &mut ts, Some(&temp));
        let el = (now_ns() - t0) / 1_000_000;
        let after = cur_mask();
        let c = r == -(EINTR as c_long) && el < 100 && PM_COUNT.load(Ordering::SeqCst) == 1 && after == orig;
        write(1, pfx.as_ptr(), pfx.len()); pm_line(b" pending ", r, el, ts_ms(&ts), c);
        ok &= c;
        sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());
        sigprocmask(2, &orig, core::ptr::null_mut());

        // 4. An fd is ready AND a signal the temporary mask unblocks is
        //    pending: the fd wins, the mask is restored at once, and the
        //    signal stays pending (Linux do_poll/do_select check fds first).
        PM_COUNT.store(0, Ordering::SeqCst);
        let mut fds = [0 as c_int; 2];
        pipe(fds.as_mut_ptr());
        write(fds[1], b"x".as_ptr(), 1);
        raise(SIGUSR1);
        let mut ts = timespec { tv_sec: 1, tv_nsec: 0 };
        let t0 = now_ns();
        let r = pm_wait(kind, fds[0], &mut ts, Some(&temp));
        let el = (now_ns() - t0) / 1_000_000;
        let after = cur_mask();
        let mut pend: sigset_t = 0;
        sigpending(&mut pend);
        let rem = ts_ms(&ts);
        let c = r == 1 && PM_COUNT.load(Ordering::SeqCst) == 0 && after == orig && pend & usr1 != 0;
        write(1, pfx.as_ptr(), pfx.len()); pm_line(b" ready   ", r, el, rem, c);
        ok &= c;
        let c = rem > 900 && rem <= 1000;
        write(1, pfx.as_ptr(), pfx.len()); pm_line(b" wb_ready", r, el, rem, c);
        ok &= c;
        close(fds[0]); close(fds[1]);
        sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut()); // runs the handler
        sigprocmask(2, &orig, core::ptr::null_mut());

        // 5. Timeout: 0, mask restored, timeout written back as {0, 0}.
        let mut ts = timespec { tv_sec: 0, tv_nsec: 40_000_000 };
        let t0 = now_ns();
        let r = pm_wait(kind, -1, &mut ts, Some(&temp));
        let el = (now_ns() - t0) / 1_000_000;
        let after = cur_mask();
        let c = r == 0 && el >= 39 && after == orig && ts.tv_sec == 0 && ts.tv_nsec == 0;
        write(1, pfx.as_ptr(), pfx.len()); pm_line(b" timeout ", r, el, ts_ms(&ts), c);
        ok &= c;

        // 6. NULL sigmask: the caller's mask stands (SIGUSR1 stays blocked,
        //    the wait times out, the signal stays pending).
        PM_COUNT.store(0, Ordering::SeqCst);
        raise(SIGUSR1);
        let mut ts = timespec { tv_sec: 0, tv_nsec: 40_000_000 };
        let r = pm_wait(kind, -1, &mut ts, None);
        let c = r == 0 && PM_COUNT.load(Ordering::SeqCst) == 0 && cur_mask() == orig;
        write(1, pfx.as_ptr(), pfx.len()); pm_line(b" nullmask", r, 0, ts_ms(&ts), c);
        ok &= c;
        sigprocmask(SIG_UNBLOCK, &usr1, core::ptr::null_mut());
    }

    // select(2) (x86_64 only): the struct timeval is written back too.
    #[cfg(target_arch = "x86_64")]
    {
        sigprocmask(2, &(entry & !usr1), core::ptr::null_mut());
        PM_COUNT.store(0, Ordering::SeqCst);
        FH_TID.store(syscall(nr::GETTID) as i32, Ordering::SeqCst);
        FH_SIG_MS.store(60, Ordering::SeqCst);
        FH_WAKE_MS.store(-1, Ordering::SeqCst);
        FH_SIGNO.store(SIGUSR1, Ordering::SeqCst);
        let mut th: pthread_t = core::ptr::null_mut();
        pthread_create(&mut th, core::ptr::null(), futex_helper, core::ptr::null_mut());
        let mut tv: [i64; 2] = [1, 0];
        let t0 = now_ns();
        let r = syscall(23, 0 as c_long, 0 as c_long, 0 as c_long, 0 as c_long, tv.as_mut_ptr() as c_long);
        let el = (now_ns() - t0) / 1_000_000;
        pthread_join(th, core::ptr::null_mut());
        FH_SIGNO.store(SIGALRM, Ordering::SeqCst);
        let rem = tv[0] * 1000 + tv[1] / 1000;
        let c = r == -(EINTR as c_long) && PM_COUNT.load(Ordering::SeqCst) == 1 && rem > 700 && rem < 1000;
        pm_line(b"  select    wb_eintr", r, el, rem, c);
        ok &= c;
        let mut tv: [i64; 2] = [0, 40_000];
        let r = syscall(23, 0 as c_long, 0 as c_long, 0 as c_long, 0 as c_long, tv.as_mut_ptr() as c_long);
        let c = r == 0 && tv[0] == 0 && tv[1] == 0;
        pm_line(b"  select    timeout ", r, 0, tv[0] * 1000 + tv[1] / 1000, c);
        ok &= c;
    }

    sigprocmask(2, &entry, core::ptr::null_mut());
    sigaction(SIGUSR1, &zeroed_sigaction(None), core::ptr::null_mut());
    sigaction(SIGCHLD, &old_chld, core::ptr::null_mut());
    report(name, ok)
}
