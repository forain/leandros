//! Signal delivery and user-space signal-frame management.
//!
//! # Delivery flow (AArch64)
//!
//! 1. `check_and_deliver_signals(frame_ptr)` is called from the exception
//!    handler after every `syscall_dispatch` return, before `eret`.
//!
//! 2. For each pending, unmasked signal the delivery engine:
//!    a. Checks the per-task `signal_actions` table.
//!    b. SIG_DFL → terminate or ignore (depending on the signal).
//!    c. SIG_IGN → skip.
//!    d. User handler → build an `rt_sigframe` on the user stack, redirect
//!       ELR_EL1 to the handler, set x0/x1/x2/x30 per AArch64 signal ABI.
//!
//! 3. The signal handler executes in user space and eventually calls the
//!    restorer (`sa_restorer`), which issues `svc #0` with syscall number 139
//!    (`rt_sigreturn`).
//!
//! 4. `restore_signal_frame(frame_ptr)` reads back the saved registers from
//!    the `rt_sigframe` on the user stack and restores the pre-signal context.
//!
//! # x86-64
//!
//! The x86-64 SYSCALL entry builds a full `UserFrame` on the kernel stack
//! and passes it through to `syscall_dispatch` exactly like AArch64. The
//! frame layout mirrors the Linux/glibc `rt_sigframe` (`pretcode` + SysV
//! `ucontext`/`mcontext`), matching the struct layouts in relibc's
//! `src/header/signal/linux.rs` so a signal handler reading `ucontext_t`
//! fields sees what it expects.


// ── SA_* flag bits (Linux values, same on AArch64 and x86-64) ────────────────
const SA_RESTORER:  u32 = 0x04000000;
const SA_NODEFER:   u32 = 0x40000000;
const SA_RESETHAND: u32 = 0x80000000;
const SA_ONSTACK:   u32 = 0x08000000;

// ── sigaltstack() ss_flags bits (Linux values; relibc's
// `header::signal::linux` is the source of truth, not generic Linux docs —
// it previously diverged here: an earlier draft of this stub used 4). ──────
const SS_ONSTACK:  u32 = 1;
const SS_DISABLE:  u32 = 2;
const MINSIGSTKSZ: usize = 2048;

/// User VA of the kernel-provided sigreturn trampoline page, mapped by
/// sys_execve into every exec'd address space (fork/threads inherit it).
///
/// Linux aarch64 has no SA_RESTORER convention — libcs (musl in particular)
/// install handlers *without* a restorer and rely on the kernel pointing the
/// handler's return address at a vDSO `rt_sigreturn` trampoline. relibc
/// happens to always pass SA_RESTORER, which is why this gap stayed hidden
/// until the first musl binary (brush) took a signal: its handler returned
/// through LR = 0 and the process died at PC 0.
pub const SIGRET_TRAMPOLINE_VA: usize = 0x0000_7fff_ff00_0000;

// Signals whose SIG_DFL action is "ignore" (bit N = signal N+1 is default-ignore).
//   SIGCHLD = 17  (bit 16)
//   SIGURG  = 23  (bit 22)
//   SIGWINCH = 28 (bit 27)
const SIGDFL_IGNORE: u64 = (1u64 << 16) | (1u64 << 22) | (1u64 << 27);

// Signal numbers used for default-terminate calculation.
const SIGSEGV: u32 = 11;

/// True when the calling task has a pending, unmasked signal that would
/// actually be *delivered* (a user handler runs, or the default action
/// terminates) rather than discarded (SIG_IGN, or SIG_DFL for a
/// default-ignore signal like SIGCHLD).
///
/// Blocking syscalls use this as their EINTR condition: POSIX requires that
/// ignored signals do NOT interrupt a blocked syscall, so a bare
/// `pending & !mask` test would make every child exit spuriously EINTR a
/// parent that never installed a SIGCHLD handler.
pub fn has_deliverable_signal() -> bool {
    let pid = super::current_pid();
    if pid == 0 { return false; }
    let rq = super::RUN_QUEUE.lock();
    has_deliverable_signal_locked(&rq, pid)
}

/// Same predicate as [`has_deliverable_signal`], but evaluated against a
/// run-queue borrow the caller already holds.
///
/// Splitting the test and the act-on-it into two separate acquisitions of
/// `RUN_QUEUE` is not good enough for a caller that *parks* on the answer:
/// `deliver_signal` sets `signal_pending` under this same lock and only
/// performs a `Blocked → Ready` wake, so a signal landing in the gap does
/// nothing (the task is still `Running`) and is then stranded on a task that
/// has just gone to sleep. `futex::futex_wait` therefore tests this inside
/// the very critical section that marks the task `Blocked`.
pub(crate) fn has_deliverable_signal_locked(
    rq: &super::runqueue::RunQueue,
    pid: super::task::Pid,
) -> bool {
    let t = match rq.find_pid(pid) { Some(t) => t, None => return false };
    // Include signals parked at the *process* level by `deliver_signal_process`
    // (its "every thread masked it" path stores them on the leader's
    // `shared_signal_pending`). This thread may be the one that has the signal
    // unmasked, in which case `check_and_deliver_signals` claims it on the next
    // return to user space — so it is genuinely deliverable and must not be
    // slept through.
    let shared = rq.find_pid(t.tgid).map(|l| l.shared_signal_pending).unwrap_or(0);
    let mut unmasked = (t.signal_pending | shared) & !t.signal_mask;
    if unmasked == 0 { return false; }
    let leader = rq.find_pid(t.tgid);
    while unmasked != 0 {
        let bit = unmasked.trailing_zeros();
        let action = leader
            .map(|l| l.signal_actions[bit as usize])
            .unwrap_or(crate::task::DEFAULT_SIGACTION);
        match action.handler {
            0 => {
                if SIGDFL_IGNORE & (1u64 << bit) == 0 { return true; } // terminates
            }
            1 => {} // SIG_IGN — discarded, keep scanning
            _ => return true, // user handler
        }
        unmasked &= !(1u64 << bit);
    }
    false
}

/// Check for pending signals on the currently-running task and deliver the
/// first pending, unmasked signal.
///
/// Must be called at every return-to-user-space path with a valid `frame_ptr`.
/// `frame_ptr` is 0 only for trap paths that don't save a full `UserFrame`
/// (none currently call this function with 0; both architectures' EL0/
/// SYSCALL return paths pass the real frame).
///
/// `frame_ptr` — kernel virtual address of the `UserFrame` on the kernel stack,
/// which was saved by the trap entry stub (AArch64 EL0 exception handler or
/// x86-64 `syscall_entry`).
#[no_mangle]
pub extern "C" fn check_and_deliver_signals(frame_ptr: usize) {
    if frame_ptr == 0 { return; }

    let pid = super::current_pid();
    if pid == 0 { return; } // kernel idle task has no signals

    // Claim any process-level pending signals (parked by deliver_signal_process
    // when every thread masked them) that THIS thread now has unmasked, moving
    // them into our own pending set so the delivery loop below handles them.
    // This is how a process-directed SIGCHLD parked during a fork's
    // all-signals-block window reaches the reactor thread once it re-unblocks.
    {
        let mut rq = super::RUN_QUEUE.lock();
        let (tgid, mask) = match rq.find_pid(pid) { Some(t) => (t.tgid, t.signal_mask), None => return };
        let claimable = rq.find_pid(tgid).map(|l| l.shared_signal_pending & !mask).unwrap_or(0);
        if claimable != 0 {
            if let Some(li) = rq.find_pid_idx(tgid) {
                if let Some(l) = rq.get_mut(li) { l.shared_signal_pending &= !claimable; }
            }
            if let Some(ti) = rq.find_pid_idx(pid) {
                if let Some(t) = rq.get_mut(ti) { t.signal_pending |= claimable; }
            }
        }
    }

    loop {
        // Sample the pending+mask state under the queue lock, then release it
        // before any further work (signal frame writing might block elsewhere).
        let sample = {
            let rq = super::RUN_QUEUE.lock();
            match rq.find_pid(pid) {
                Some(t) => {
                    let unmasked = t.signal_pending & !t.signal_mask;
                    if unmasked == 0 { return; }
                    let bit  = unmasked.trailing_zeros() as u32;
                    let sig  = bit + 1;
                    let mask = t.signal_mask;
                    // Signal disposition is shared across the thread group: read
                    // from the TGID leader so all threads see installed handlers.
                    let action = rq.find_pid(t.tgid)
                        .map(|leader| leader.signal_actions[bit as usize])
                        .unwrap_or(crate::task::DEFAULT_SIGACTION);
                    Some((sig, action, mask))
                }
                None => return,
            }
        };

        let (sig, action, old_mask) = match sample {
            Some(r) => r,
            None    => return,
        };

        // Clear the pending bit and update the signal mask under the lock.
        {
            let mut rq = super::RUN_QUEUE.lock();
            let tgid = rq.find_pid(pid).map(|t| t.tgid).unwrap_or(0);
            // Update thread-local pending/mask.
            if let Some(idx) = rq.find_pid_idx(pid) {
                if let Some(t) = rq.get_mut(idx) {
                    t.signal_pending &= !(1u64 << (sig - 1));
                    if action.get_flags() & SA_NODEFER == 0 {
                        t.signal_mask |= (1u64 << (sig - 1)) | action.mask;
                    }
                }
            }
            // SA_RESETHAND: revert to SIG_DFL on the shared (TGID leader) table.
            if action.get_flags() & SA_RESETHAND != 0 && tgid != 0 {
                if let Some(idx) = rq.find_pid_idx(tgid) {
                    if let Some(leader) = rq.get_mut(idx) {
                        leader.signal_actions[(sig - 1) as usize].handler = 0;
                    }
                }
            }
        }

        match action.handler {
            0 => {
                // SIG_DFL
                if SIGDFL_IGNORE & (1u64 << (sig - 1)) != 0 {
                    continue; // check next pending signal
                }
                // Default action: terminate.
                super::exit(128 + sig as i32);
            }
            1 => {
                // SIG_IGN — skip, check next.
                continue;
            }
            handler => {
                let restorer = if action.get_flags() & SA_RESTORER != 0 {
                    action.get_restorer()
                } else {
                    // No SA_RESTORER: return through the kernel-provided
                    // rt_sigreturn trampoline (Linux-aarch64 convention;
                    // musl never sets SA_RESTORER there). Mapped by execve;
                    // a pre-exec task without the page simply must not
                    // return from its handler, as before.
                    #[cfg(target_arch = "aarch64")]
                    { SIGRET_TRAMPOLINE_VA }
                    #[cfg(not(target_arch = "aarch64"))]
                    { 0 }
                };

                if !arch_prepare_signal_frame(frame_ptr, sig, handler, restorer, old_mask, action.get_flags()) {
                    // Frame write failed (stack fault) — deliver SIGSEGV.
                    super::exit(128 + SIGSEGV as i32);
                }

                // One signal delivered; re-check for more on the next syscall return.
                return;
            }
        }
    }
}

/// Restore user context from the saved signal frame on the user stack.
///
/// Called from `sys_rt_sigreturn` (syscall 139 / 15) with `frame_ptr` pointing
/// to the `UserFrame` saved on the kernel stack at the time of the sigreturn
/// syscall.  Reads back the saved GPRs and signal mask from the `rt_sigframe`
/// that was written by `check_and_deliver_signals` earlier.
pub fn restore_signal_frame(frame_ptr: usize) {
    if frame_ptr == 0 { return; }
    let pid = super::current_pid();
    if pid == 0 { return; }
    arch_restore_signal_frame(frame_ptr, pid);
}

pub fn sys_sigaction(signum: u32, act_ptr: usize, oldact_ptr: usize) -> isize {
    if signum == 0 || signum > 64 { return -22; }
    let pid = super::current_pid();
    let mut rq = super::RUN_QUEUE.lock();
    // Signal actions belong to the thread group — always read/write through leader.
    let tgid = match rq.find_pid(pid) {
        Some(t) => t.tgid,
        None    => return -3,
    };
    if let Some(leader) = rq.find_pid_mut(tgid) {
        if oldact_ptr != 0 {
            let old = leader.signal_actions[(signum - 1) as usize];
            unsafe { core::ptr::write(oldact_ptr as *mut crate::task::SigAction, old); }
        }
        if act_ptr != 0 {
            let new = unsafe { core::ptr::read(act_ptr as *const crate::task::SigAction) };
            leader.signal_actions[(signum - 1) as usize] = new;
        }
        0
    } else {
        -3
    }
}

pub fn sys_sigprocmask(how: usize, set_ptr: usize, oldset_ptr: usize) -> isize {
    const SIG_BLOCK:   usize = 0;
    const SIG_UNBLOCK: usize = 1;
    const SIG_SETMASK: usize = 2;

    let pid = super::current_pid();
    let mut rq = super::RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        if oldset_ptr != 0 {
            unsafe { core::ptr::write(oldset_ptr as *mut u64, t.signal_mask); }
        }
        if set_ptr != 0 {
            let set = unsafe { core::ptr::read(set_ptr as *const u64) };
            match how {
                SIG_BLOCK   => t.signal_mask |= set,
                SIG_UNBLOCK => t.signal_mask &= !set,
                SIG_SETMASK => t.signal_mask = set,
                _           => return -22, // EINVAL
            }
        }
        0
    } else {
        -3 // ESRCH
    }
}

/// sys_sigaltstack(ss, oss) — set/get the calling thread's alternate signal
/// stack (per-thread state, like `signal_mask`, not shared across the
/// thread group the way `signal_actions` is).
///
/// `frame_ptr` supplies the live user SP so we can tell whether the thread
/// is currently executing on its alt-stack — needed both to report
/// `SS_ONSTACK` in `oss` and to reject (`EPERM`) an attempt to change an
/// alt-stack that's actively in use, matching Linux's `do_sigaltstack()`.
pub fn sys_sigaltstack(ss_ptr: usize, oss_ptr: usize, frame_ptr: usize) -> isize {
    let (cur_sp, cur_size, cur_flags) = super::current_altstack();
    let user_sp = arch_current_user_sp(frame_ptr);
    let active = cur_flags & SS_DISABLE == 0 && on_altstack(user_sp, cur_sp, cur_size);

    if oss_ptr != 0 {
        let report_flags = if active { SS_ONSTACK } else { cur_flags };
        unsafe {
            core::ptr::write(oss_ptr as *mut usize, cur_sp);
            core::ptr::write((oss_ptr + 8) as *mut u32, report_flags);
            core::ptr::write((oss_ptr + 16) as *mut usize, cur_size);
        }
    }

    if ss_ptr != 0 {
        if active { return -1; } // EPERM — alt-stack is in use
        let new_sp    = unsafe { core::ptr::read(ss_ptr as *const usize) };
        let new_flags = unsafe { core::ptr::read((ss_ptr + 8) as *const u32) };
        let new_size  = unsafe { core::ptr::read((ss_ptr + 16) as *const usize) };

        if new_flags & !SS_DISABLE != 0 { return -22; } // EINVAL — unknown flag bits
        if new_flags & SS_DISABLE != 0 {
            super::set_current_altstack(0, 0, SS_DISABLE);
        } else {
            if new_size < MINSIGSTKSZ { return -12; } // ENOMEM — too small
            super::set_current_altstack(new_sp, new_size, 0);
        }
    }
    0
}

// ── Arch dispatch ─────────────────────────────────────────────────────────────

/// True if `sp` falls within `[alt_sp, alt_sp + alt_size)`. Mirrors Linux's
/// `on_sig_stack()`; used both to compute `SS_ONSTACK` for `sigaltstack()`
/// and to decide whether `SA_ONSTACK` delivery should reuse the current SP
/// instead of restarting at the top of the alt-stack (nested-signal case).
fn on_altstack(sp: usize, alt_sp: usize, alt_size: usize) -> bool {
    alt_size != 0 && sp.wrapping_sub(alt_sp) < alt_size
}

/// Computes the base stack pointer signal delivery should build the frame
/// below: the alt-stack's top, if `SA_ONSTACK` is set on the handler, a
/// usable (non-disabled, non-empty) alt-stack is configured, and the thread
/// isn't already executing on it. The "already on it" check keeps a nested
/// signal delivered onto an active alt-stack growing that same stack
/// downward instead of restarting at its top — matching Linux's
/// `get_sigframe()`. Otherwise, just the thread's current user SP.
fn sigframe_base_sp(old_sp: usize, action_flags: u32) -> usize {
    if action_flags & SA_ONSTACK == 0 { return old_sp; }
    let (alt_sp, alt_size, alt_flags) = super::current_altstack();
    if alt_flags & SS_DISABLE == 0 && alt_size != 0 && !on_altstack(old_sp, alt_sp, alt_size) {
        alt_sp + alt_size
    } else {
        old_sp
    }
}

/// Reads the live user stack pointer out of the `UserFrame` at `frame_ptr`,
/// or 0 if there is no frame (matches `current_altstack()`'s disabled
/// default, which never reports `SS_ONSTACK` either way).
fn arch_current_user_sp(frame_ptr: usize) -> usize {
    if frame_ptr == 0 { return 0; }
    let user_frame = unsafe { &*(frame_ptr as *const crate::context::UserFrame) };
    #[cfg(target_arch = "aarch64")]
    return user_frame.sp_el0 as usize;
    #[cfg(target_arch = "x86_64")]
    return user_frame.rsp as usize;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    { let _ = user_frame; 0 }
}

fn arch_prepare_signal_frame(
    frame_ptr:    usize,
    sig:          u32,
    handler:      usize,
    restorer:     usize,
    old_mask:     u64,
    action_flags: u32,
) -> bool {
    #[cfg(target_arch = "aarch64")]
    return aarch64::prepare(frame_ptr, sig, handler, restorer, old_mask, action_flags);

    #[cfg(target_arch = "x86_64")]
    return x86_64::prepare(frame_ptr, sig, handler, restorer, old_mask, action_flags);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    { let _ = (frame_ptr, sig, handler, restorer, old_mask, action_flags); false }
}

fn arch_restore_signal_frame(frame_ptr: usize, pid: u32) {
    #[cfg(target_arch = "aarch64")]
    aarch64::restore(frame_ptr, pid);

    #[cfg(target_arch = "x86_64")]
    x86_64::restore(frame_ptr, pid);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    { let _ = (frame_ptr, pid); }
}

// ── AArch64 rt_sigframe layout ────────────────────────────────────────────────
#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use crate::context::UserFrame;

    // Offsets within rt_sigframe (from the start of the frame).
    //
    // [  0 ..  128)  siginfo (128 bytes)
    // [128 ..  136)  uc.uc_flags
    // [136 ..  144)  uc.uc_link
    // [144 ..  168)  uc.uc_stack (stack_t: void*, int, pad4, size_t = 24 bytes)
    // [168 ..  176)  uc.uc_sigmask (sigset_t = u64)
    // [176 ..  296)  uc.__unused[120]  (pad to 128-byte sigmask area)
    // [296 ..  304)  uc.uc_mcontext.fault_address
    // [304 ..  552)  uc.uc_mcontext.regs[31]  (31 × 8 bytes = 248 bytes)
    // [552 ..  560)  uc.uc_mcontext.sp
    // [560 ..  568)  uc.uc_mcontext.pc
    // [568 ..  576)  uc.uc_mcontext.pstate
    // [576 .. 4672)  uc.uc_mcontext.__reserved[4096]
    //                  → starts with null _aarch64_ctx terminator (8 zero bytes)
    //                  → rest zeroed (no FPSIMD context in Phase 2)

    const SIGINFO_SIZE:       usize = 128;
    const UC_OFFSET:          usize = SIGINFO_SIZE;              // 128
    const SIGMASK_OFFSET:     usize = UC_OFFSET + 8 + 8 + 24;   // 168
    const MCONTEXT_OFFSET:    usize = SIGMASK_OFFSET + 128;      // 296
    const REGS_OFFSET:        usize = MCONTEXT_OFFSET + 8;       // 304
    const SP_OFFSET:          usize = REGS_OFFSET + 31 * 8;      // 552
    const PC_OFFSET:          usize = SP_OFFSET + 8;             // 560
    const PSTATE_OFFSET:      usize = PC_OFFSET + 8;             // 568
    const RESERVED_OFFSET:    usize = PSTATE_OFFSET + 8;         // 576
    pub const SIGFRAME_SIZE:  usize = RESERVED_OFFSET + 4096;    // 4672

    // Offsets within siginfo.
    const SI_SIGNO_OFFSET: usize = 0; // __u32 si_signo

    /// Allowed user-restorable PSTATE bits on sigreturn: the N/Z/C/V
    /// condition flags only. Everything else — the M[4:0] exception-level
    /// field, DAIF interrupt masks, SS/IL and the rest — is forced to the
    /// same baseline a freshly created thread starts with (`spsr_el1 == 0`:
    /// EL0t, AArch64 state, interrupts unmasked; see `sched/src/context.rs`).
    /// Mirrors the x86-64 `SAFE_RFLAGS_MASK` in `mod x86_64` below: a
    /// forged value on the user-writable signal stack must not be able to
    /// request a return to EL1 via M[3:0], which an unmasked restore would
    /// allow.
    const SPSR_NZCV_MASK: u64 = 0xF000_0000;

    /// Write an AArch64 `rt_sigframe` onto the user stack and redirect the
    /// kernel's `UserFrame` to invoke `handler(sig, &siginfo, &uc)`.
    ///
    /// Builds the frame in a kernel buffer and writes it page-by-page via the
    /// TGID leader's address space, which handles HHDM translation and lazy VMAs.
    pub fn prepare(
        frame_ptr:    usize,
        sig:          u32,
        handler:      usize,
        restorer:     usize,
        old_mask:     u64,
        action_flags: u32,
    ) -> bool {
        let user_frame = unsafe { &mut *(frame_ptr as *mut UserFrame) };

        // Compute new SP below the current user SP, 16-byte aligned.
        let old_sp = user_frame.sp_el0 as usize;
        let base_sp = super::sigframe_base_sp(old_sp, action_flags);
        let new_sp = match base_sp.checked_sub(SIGFRAME_SIZE) {
            Some(p) => p & !15usize,
            None    => return false,
        };
        if new_sp == 0 || new_sp >= 0x0000_8000_0000_0000 { return false; }

        // Build the signal frame in a kernel buffer (zeroed — the null
        // _aarch64_ctx terminator at RESERVED_OFFSET falls in naturally).
        let mut buf = alloc::vec![0u8; SIGFRAME_SIZE];

        // siginfo: si_signo at offset 0.
        buf[SI_SIGNO_OFFSET..SI_SIGNO_OFFSET + 4]
            .copy_from_slice(&sig.to_le_bytes());

        // uc_sigmask (restored on sigreturn).
        buf[SIGMASK_OFFSET..SIGMASK_OFFSET + 8]
            .copy_from_slice(&old_mask.to_le_bytes());

        // uc_mcontext: save current user register state.
        for i in 0..31 {
            buf[REGS_OFFSET + i * 8..REGS_OFFSET + i * 8 + 8]
                .copy_from_slice(&user_frame.x[i].to_le_bytes());
        }
        buf[SP_OFFSET..SP_OFFSET + 8]
            .copy_from_slice(&user_frame.sp_el0.to_le_bytes());
        buf[PC_OFFSET..PC_OFFSET + 8]
            .copy_from_slice(&user_frame.elr_el1.to_le_bytes());
        buf[PSTATE_OFFSET..PSTATE_OFFSET + 8]
            .copy_from_slice(&user_frame.spsr_el1.to_le_bytes());

        // Prefault any lazy stack pages and write via TGID leader's address
        // space.  write_user_buf translates each page through the HHDM and
        // handles non-contiguous physical pages, so no physical-contiguity
        // assumption is needed.  Goes through the per-address-space lock
        // (with_address_space_mut) so it serializes with faults and mm
        // syscalls on other CPUs without pinning the run-queue lock.
        let ok = {
            let pid = super::super::current_pid();
            super::super::with_address_space_mut(pid, |as_| {
                as_.prefault_range(new_sp, SIGFRAME_SIZE);
                as_.write_user_buf(new_sp, &buf)
            }).unwrap_or(false)
        };
        if !ok { return false; }

        // Redirect UserFrame to the signal handler.
        // AArch64 signal calling convention (matches Linux):
        //   x0 = signum,  x1 = &siginfo (new_sp),  x2 = &ucontext (new_sp + UC_OFFSET)
        //   x30 = restorer,  ELR_EL1 = handler,  SP_EL0 = new_sp
        user_frame.x[0]    = sig as u64;
        user_frame.x[1]    = new_sp as u64;
        user_frame.x[2]    = (new_sp + UC_OFFSET) as u64;
        user_frame.x[30]   = restorer as u64;
        user_frame.elr_el1 = handler as u64;
        user_frame.sp_el0  = new_sp as u64;

        true
    }

    /// Restore user context from the saved `rt_sigframe` on the user stack.
    ///
    /// Called during `rt_sigreturn`: reads back GPRs, SP, PC, PSTATE, and the
    /// signal mask from the frame placed on the user stack at delivery time.
    pub fn restore(frame_ptr: usize, pid: u32) {
        let user_frame = unsafe { &mut *(frame_ptr as *mut UserFrame) };

        // On rt_sigreturn the user SP points at the sigframe set at delivery.
        let sigframe_virt = user_frame.sp_el0 as usize;

        let mut buf = alloc::vec![0u8; SIGFRAME_SIZE];

        // Read the frame via TGID leader's address space (handles HHDM and
        // non-contiguous lazy pages), under the per-address-space lock.
        let ok = super::super::with_address_space(pid, |as_| {
            as_.read_user_buf(sigframe_virt, &mut buf)
        }).unwrap_or(false);
        if !ok { super::super::exit(128 + 11); }

        // Restore GPRs from uc_mcontext.
        for i in 0..31 {
            user_frame.x[i] = u64::from_le_bytes(
                buf[REGS_OFFSET + i * 8..REGS_OFFSET + i * 8 + 8].try_into().unwrap()
            );
        }
        user_frame.sp_el0   = u64::from_le_bytes(buf[SP_OFFSET..SP_OFFSET+8].try_into().unwrap());
        user_frame.elr_el1  = u64::from_le_bytes(buf[PC_OFFSET..PC_OFFSET+8].try_into().unwrap());
        // spsr_el1 is NOT restored verbatim from user-writable memory (see
        // SPSR_NZCV_MASK above) — only condition flags pass through.
        let saved_pstate = u64::from_le_bytes(buf[PSTATE_OFFSET..PSTATE_OFFSET+8].try_into().unwrap());
        user_frame.spsr_el1 = saved_pstate & SPSR_NZCV_MASK;

        // Restore the pre-handler signal mask from uc_sigmask.
        let saved_mask =
            u64::from_le_bytes(buf[SIGMASK_OFFSET..SIGMASK_OFFSET+8].try_into().unwrap());
        {
            let mut rq = super::super::RUN_QUEUE.lock();
            if let Some(idx) = rq.find_pid_idx(pid) {
                if let Some(t) = rq.get_mut(idx) {
                    t.signal_mask = saved_mask;
                }
            }
        }
    }
}

// ── x86-64 rt_sigframe layout ─────────────────────────────────────────────────
#[cfg(target_arch = "x86_64")]
mod x86_64 {
    use crate::context::UserFrame;

    // Offsets within our kernel-internal `rt_sigframe` (from the start of the
    // frame). This is not exposed to user code as a named type — only the
    // `siginfo_t`/`ucontext_t` sub-regions are, via pointers handed to the
    // handler in rsi/rdx — but it is laid out to match the field offsets of
    // relibc's `ucontext`/`mcontext`/`siginfo` (src/header/signal/linux.rs),
    // which mirror the real Linux/glibc x86-64 ABI.
    //
    // [  0 ..   8)  pretcode                  (popped by the handler's `ret`)
    // [  8 ..  16)  uc.uc_flags
    // [ 16 ..  24)  uc.uc_link
    // [ 24 ..  48)  uc.uc_stack                (stack_t: sp, flags, pad, size)
    // [ 48 .. 232)  uc.uc_mcontext.gregs[23]    (REG_R8 .. REG_CR2, 8 bytes each)
    // [232 .. 240)  uc.uc_mcontext.fpregs       (null — no FP context in Phase 2)
    // [240 .. 304)  uc.uc_mcontext.__private    (8 * 8, zeroed)
    // [304 .. 312)  uc.uc_sigmask
    // [312 .. 824)  uc.__private                (512 bytes, zeroed)
    // [824 .. 872)  info (siginfo_t: si_signo, si_errno, si_code, si_pid,
    //                      si_uid, pad, si_addr, si_status, pad, si_value)

    const PRETCODE_OFFSET:  usize = 0;
    const UC_OFFSET:        usize = PRETCODE_OFFSET + 8;   // 8
    const STACK_OFFSET:     usize = UC_OFFSET + 16;         // 24
    const MCONTEXT_OFFSET:  usize = STACK_OFFSET + 24;      // 48
    const GREGS_OFFSET:     usize = MCONTEXT_OFFSET;        // 48
    const SIGMASK_OFFSET:   usize = MCONTEXT_OFFSET + 256;  // 304
    const INFO_OFFSET:      usize = UC_OFFSET + 816;        // 824
    const SI_SIGNO_OFFSET:  usize = INFO_OFFSET;
    pub const SIGFRAME_SIZE: usize = INFO_OFFSET + 48;      // 872

    // gregs[] indices — Linux REG_* enum order for x86-64.
    const REG_R8: usize = 0; const REG_R9: usize = 1; const REG_R10: usize = 2; const REG_R11: usize = 3;
    const REG_R12: usize = 4; const REG_R13: usize = 5; const REG_R14: usize = 6; const REG_R15: usize = 7;
    const REG_RDI: usize = 8; const REG_RSI: usize = 9; const REG_RBP: usize = 10; const REG_RBX: usize = 11;
    const REG_RDX: usize = 12; const REG_RAX: usize = 13; const REG_RCX: usize = 14; const REG_RSP: usize = 15;
    const REG_RIP: usize = 16; const REG_EFL: usize = 17; const REG_CSGSFS: usize = 18;

    fn greg_off(i: usize) -> usize { GREGS_OFFSET + i * 8 }

    /// Allowed user-restorable RFLAGS bits on sigreturn: CF/PF/AF/ZF/SF/TF/DF/OF
    /// plus RF and AC. IOPL, NT, IF and the system flags are deliberately
    /// excluded — a forged value on the user-writable signal stack must not be
    /// able to disable interrupts or escalate I/O privilege.
    const SAFE_RFLAGS_MASK: u64 = 0x0_0004_0DD5;
    const RFLAGS_FIXED: u64 = 0x202; // reserved bit 1 + IF

    /// Flip to `true` to trace every x86-64 signal-frame construction over the
    /// serial console. Prints, per delivery: the signal number, the user rsp
    /// the frame is built below, the frame base, the rsp→frame gap (must be
    /// ≥ 128 + SIGFRAME_SIZE once the red zone is reserved), and the rax being
    /// captured into uc_mcontext.gregs — which is the interrupted syscall's
    /// *result*, and must never be the syscall number. Left in, and left off:
    /// this is the evidence path for both the red-zone reservation and the
    /// return-value publication in arch/x86_64/src/syscall.rs step 7b.
    const SIGFRAME_TRACE: bool = false;

    fn trace_frame(sig: u32, old_sp: usize, frame: usize, rax: u64) {
        extern "C" { fn arch_serial_putc(c: u8); }
        fn put(s: &str) { for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } } }
        fn hex(mut v: u64) {
            put("0x");
            if v == 0 { put("0"); return; }
            let mut d = [0u8; 16];
            let mut n = 0;
            while v > 0 { d[n] = b"0123456789abcdef"[(v & 0xf) as usize]; n += 1; v >>= 4; }
            for i in (0..n).rev() { unsafe { arch_serial_putc(d[i]); } }
        }
        put("[SIGFRAME] sig="); hex(sig as u64);
        put(" rsp=");           hex(old_sp as u64);
        put(" frame=");         hex(frame as u64);
        put(" gap=");           hex(old_sp.saturating_sub(frame) as u64);
        put(" saved_rax=");     hex(rax);
        put("\n");
    }

    /// Write an x86-64 `rt_sigframe` onto the user stack and redirect the
    /// kernel's `UserFrame` to invoke `handler(sig, &siginfo, &ucontext)`.
    ///
    /// Builds the frame in a kernel buffer and writes it page-by-page via the
    /// TGID leader's address space, mirroring the AArch64 `prepare()` above.
    pub fn prepare(
        frame_ptr:    usize,
        sig:          u32,
        handler:      usize,
        restorer:     usize,
        old_mask:     u64,
        action_flags: u32,
    ) -> bool {
        let user_frame = unsafe { &mut *(frame_ptr as *mut UserFrame) };

        // Compute new SP below the current user SP. The handler is entered
        // as if `call handler` had just executed (rsp points at pretcode),
        // so rsp % 16 must equal 8, not 0.
        //
        // RED ZONE: the x86-64 System V ABI reserves the 128 bytes below rsp
        // for the interrupted function's own use — leaf functions keep live
        // locals and spills there without adjusting rsp, so that memory is not
        // dead, it is in use. Building the signal frame at rsp therefore
        // overwrites up to 128 bytes of live user data every time a signal is
        // delivered outside an alt-stack. Linux's `get_sigframe()` subtracts
        // the red zone for exactly this reason; we did not, which is why
        // x86-64 saw intermittent user-mode faults through corrupted pointers
        // in signal-heavy workloads (a shell reaping a pipeline takes a
        // SIGCHLD per member) while AArch64 — which has no red zone — never
        // did. `SA_ONSTACK` delivery onto a *fresh* alt-stack needs no such
        // reservation, but sigframe_base_sp() also returns the current rsp for
        // the nested-signal case, so subtract unconditionally: 128 wasted
        // bytes on an alt-stack is harmless, skipping them is not.
        const RED_ZONE: usize = 128;
        let old_sp = user_frame.rsp as usize;
        let base_sp = match super::sigframe_base_sp(old_sp, action_flags).checked_sub(RED_ZONE) {
            Some(p) => p,
            None    => return false,
        };
        let aligned = match base_sp.checked_sub(SIGFRAME_SIZE) {
            Some(p) => p & !15usize,
            None    => return false,
        };
        if aligned < 16 { return false; }
        let new_sp = aligned - 8;
        if new_sp >= 0x0000_8000_0000_0000 { return false; }

        let mut buf = alloc::vec![0u8; SIGFRAME_SIZE];

        // pretcode — popped by the handler's implicit `ret`.
        buf[PRETCODE_OFFSET..PRETCODE_OFFSET + 8]
            .copy_from_slice(&(restorer as u64).to_le_bytes());

        // siginfo: si_signo at offset 0.
        buf[SI_SIGNO_OFFSET..SI_SIGNO_OFFSET + 4]
            .copy_from_slice(&sig.to_le_bytes());

        // uc_sigmask (restored on sigreturn).
        buf[SIGMASK_OFFSET..SIGMASK_OFFSET + 8]
            .copy_from_slice(&old_mask.to_le_bytes());

        // uc_mcontext.gregs: save current user register state.
        let wreg = |buf: &mut alloc::vec::Vec<u8>, i: usize, v: u64| {
            let o = greg_off(i);
            buf[o..o + 8].copy_from_slice(&v.to_le_bytes());
        };
        wreg(&mut buf, REG_R8,  user_frame.r8);
        wreg(&mut buf, REG_R9,  user_frame.r9);
        wreg(&mut buf, REG_R10, user_frame.r10);
        wreg(&mut buf, REG_R11, user_frame.r11);
        wreg(&mut buf, REG_R12, user_frame.r12);
        wreg(&mut buf, REG_R13, user_frame.r13);
        wreg(&mut buf, REG_R14, user_frame.r14);
        wreg(&mut buf, REG_R15, user_frame.r15);
        wreg(&mut buf, REG_RDI, user_frame.rdi);
        wreg(&mut buf, REG_RSI, user_frame.rsi);
        wreg(&mut buf, REG_RBP, user_frame.rbp);
        wreg(&mut buf, REG_RBX, user_frame.rbx);
        wreg(&mut buf, REG_RDX, user_frame.rdx);
        wreg(&mut buf, REG_RAX, user_frame.rax);
        wreg(&mut buf, REG_RCX, user_frame.rcx);
        wreg(&mut buf, REG_RSP, user_frame.rsp);
        wreg(&mut buf, REG_RIP, user_frame.rip);
        wreg(&mut buf, REG_EFL, user_frame.rflags);
        wreg(&mut buf, REG_CSGSFS, user_frame.cs);

        // Prefault any lazy stack pages and write via TGID leader's address
        // space, exactly as the AArch64 path does — under the
        // per-address-space lock (see that path's comment).
        let ok = {
            let pid = super::super::current_pid();
            super::super::with_address_space_mut(pid, |as_| {
                as_.prefault_range(new_sp, SIGFRAME_SIZE);
                as_.write_user_buf(new_sp, &buf)
            }).unwrap_or(false)
        };
        if !ok { return false; }

        if SIGFRAME_TRACE { trace_frame(sig, old_sp, new_sp, user_frame.rax); }

        // Redirect UserFrame to the signal handler.
        // x86-64 signal calling convention (matches Linux):
        //   rdi = signum, rsi = &siginfo, rdx = &ucontext,
        //   rip = handler, rsp = new_sp (-> [rsp] = pretcode = restorer).
        // cs/ss are left untouched — never take a ring level from user data.
        user_frame.rdi    = sig as u64;
        user_frame.rsi    = (new_sp + INFO_OFFSET) as u64;
        user_frame.rdx    = (new_sp + UC_OFFSET) as u64;
        user_frame.rip    = handler as u64;
        user_frame.rsp    = new_sp as u64;
        // rax = 0 on handler entry, exactly as Linux's setup_rt_frame does:
        // in the variadic ABI al carries the number of vector registers used,
        // so a handler declared without a prototype reads it. The real rax was
        // captured into uc_mcontext.gregs above and comes back on sigreturn.
        user_frame.rax    = 0;

        true
    }

    /// Restore user context from the saved `rt_sigframe` on the user stack.
    ///
    /// Called during `rt_sigreturn`. `__restore_rt`'s `mov rax,15; syscall`
    /// runs after the handler's `ret` already popped `pretcode`, so the user
    /// rsp captured in the `UserFrame` is 8 bytes above the frame base.
    pub fn restore(frame_ptr: usize, pid: u32) {
        let user_frame = unsafe { &mut *(frame_ptr as *mut UserFrame) };

        let sigframe_virt = (user_frame.rsp as usize).wrapping_sub(8);

        let mut buf = alloc::vec![0u8; SIGFRAME_SIZE];

        let ok = super::super::with_address_space(pid, |as_| {
            as_.read_user_buf(sigframe_virt, &mut buf)
        }).unwrap_or(false);
        if !ok { super::super::exit(128 + 11); }

        let rreg = |buf: &alloc::vec::Vec<u8>, i: usize| -> u64 {
            let o = greg_off(i);
            u64::from_le_bytes(buf[o..o + 8].try_into().unwrap())
        };
        user_frame.r8  = rreg(&buf, REG_R8);
        user_frame.r9  = rreg(&buf, REG_R9);
        user_frame.r10 = rreg(&buf, REG_R10);
        user_frame.r11 = rreg(&buf, REG_R11);
        user_frame.r12 = rreg(&buf, REG_R12);
        user_frame.r13 = rreg(&buf, REG_R13);
        user_frame.r14 = rreg(&buf, REG_R14);
        user_frame.r15 = rreg(&buf, REG_R15);
        user_frame.rdi = rreg(&buf, REG_RDI);
        user_frame.rsi = rreg(&buf, REG_RSI);
        user_frame.rbp = rreg(&buf, REG_RBP);
        user_frame.rbx = rreg(&buf, REG_RBX);
        user_frame.rdx = rreg(&buf, REG_RDX);
        user_frame.rax = rreg(&buf, REG_RAX);
        user_frame.rcx = rreg(&buf, REG_RCX);
        user_frame.rsp = rreg(&buf, REG_RSP);
        user_frame.rip = rreg(&buf, REG_RIP);
        // cs/ss are NOT restored from user-writable memory (see prepare()).
        let saved_efl = rreg(&buf, REG_EFL);
        user_frame.rflags = (saved_efl & SAFE_RFLAGS_MASK) | RFLAGS_FIXED;

        // Restore the pre-handler signal mask from uc_sigmask.
        let saved_mask =
            u64::from_le_bytes(buf[SIGMASK_OFFSET..SIGMASK_OFFSET+8].try_into().unwrap());
        {
            let mut rq = super::super::RUN_QUEUE.lock();
            if let Some(idx) = rq.find_pid_idx(pid) {
                if let Some(t) = rq.get_mut(idx) {
                    t.signal_mask = saved_mask;
                }
            }
        }
    }
}
