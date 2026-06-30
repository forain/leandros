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
//! The x86-64 SYSCALL path does NOT save a full `UserFrame` (the frame_ptr
//! argument is always 0), so signal delivery is deferred to Phase 2.5 on
//! that architecture.


// ── SA_* flag bits (Linux values, same on AArch64 and x86-64) ────────────────
const SA_RESTORER:  u32 = 0x04000000;
const SA_NODEFER:   u32 = 0x40000000;
const SA_RESETHAND: u32 = 0x80000000;

// Signals whose SIG_DFL action is "ignore" (bit N = signal N+1 is default-ignore).
//   SIGCHLD = 17  (bit 16)
//   SIGURG  = 23  (bit 22)
//   SIGWINCH = 28 (bit 27)
const SIGDFL_IGNORE: u64 = (1u64 << 16) | (1u64 << 22) | (1u64 << 27);

// Signal numbers used for default-terminate calculation.
const SIGSEGV: u32 = 11;

/// Check for pending signals on the currently-running task and deliver the
/// first pending, unmasked signal.
///
/// Must be called at every return-to-user-space path with a valid `frame_ptr`.
/// When `frame_ptr == 0` (x86-64 SYSCALL path, no full frame saved), the
/// function returns immediately.
///
/// `frame_ptr` — kernel virtual address of the `UserFrame` on the kernel stack,
/// which was saved by the EL0→EL1 exception entry stub.
#[no_mangle]
pub fn check_and_deliver_signals(frame_ptr: usize) {
    if frame_ptr == 0 { return; } // x86-64: full delivery deferred to Phase 2.5

    let pid = unsafe { super::CURRENT_PID[super::cpu_id()] };
    if pid == 0 { return; } // kernel idle task has no signals

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
                    if action.flags & SA_NODEFER == 0 {
                        t.signal_mask |= (1u64 << (sig - 1)) | action.mask;
                    }
                }
            }
            // SA_RESETHAND: revert to SIG_DFL on the shared (TGID leader) table.
            if action.flags & SA_RESETHAND != 0 && tgid != 0 {
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
                let restorer = if action.flags & SA_RESTORER != 0 {
                    action.restorer
                } else {
                    0 // no restorer — signal handler must not return
                };

                if !arch_prepare_signal_frame(frame_ptr, sig, handler, restorer, old_mask) {
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
    let pid = unsafe { super::CURRENT_PID[super::cpu_id()] };
    if pid == 0 { return; }
    arch_restore_signal_frame(frame_ptr, pid);
}

pub fn sys_sigaction(signum: u32, act_ptr: usize, oldact_ptr: usize) -> isize {
    if signum == 0 || signum > 64 { return -22; }
    let pid = unsafe { super::CURRENT_PID[super::cpu_id()] };
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

    let pid = unsafe { super::CURRENT_PID[super::cpu_id()] };
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

// ── Arch dispatch ─────────────────────────────────────────────────────────────

fn arch_prepare_signal_frame(
    frame_ptr: usize,
    sig:       u32,
    handler:   usize,
    restorer:  usize,
    old_mask:  u64,
) -> bool {
    #[cfg(target_arch = "aarch64")]
    return aarch64::prepare(frame_ptr, sig, handler, restorer, old_mask);

    #[cfg(not(target_arch = "aarch64"))]
    { let _ = (frame_ptr, sig, handler, restorer, old_mask); false }
}

fn arch_restore_signal_frame(frame_ptr: usize, pid: u32) {
    #[cfg(target_arch = "aarch64")]
    aarch64::restore(frame_ptr, pid);

    #[cfg(not(target_arch = "aarch64"))]
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

    /// Write an AArch64 `rt_sigframe` onto the user stack and redirect the
    /// kernel's `UserFrame` to invoke `handler(sig, &siginfo, &uc)`.
    ///
    /// Builds the frame in a kernel buffer and writes it page-by-page via the
    /// TGID leader's address space, which handles HHDM translation and lazy VMAs.
    pub fn prepare(
        frame_ptr: usize,
        sig:       u32,
        handler:   usize,
        restorer:  usize,
        old_mask:  u64,
    ) -> bool {
        let user_frame = unsafe { &mut *(frame_ptr as *mut UserFrame) };

        // Compute new SP below the current user SP, 16-byte aligned.
        let old_sp = user_frame.sp_el0 as usize;
        let new_sp = match old_sp.checked_sub(SIGFRAME_SIZE) {
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
        // assumption is needed.
        let ok = {
            let pid = unsafe { super::super::CURRENT_PID[super::super::cpu_id()] };
            let mut rq = super::super::RUN_QUEUE.lock();
            let tgid = match rq.find_pid(pid) {
                Some(t) => t.tgid,
                None    => return false,
            };
            match rq.find_pid_mut(tgid).and_then(|t| t.address_space.as_mut()) {
                Some(as_) => {
                    as_.prefault_range(new_sp, SIGFRAME_SIZE);
                    as_.write_user_buf(new_sp, &buf)
                }
                None => false,
            }
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
        // non-contiguous lazy pages).
        let ok = {
            let rq = super::super::RUN_QUEUE.lock();
            let tgid = match rq.find_pid(pid) {
                Some(t) => t.tgid,
                None    => { super::super::exit(128 + 11); }
            };
            match rq.find_pid(tgid).and_then(|t| t.address_space.as_ref()) {
                Some(as_) => as_.read_user_buf(sigframe_virt, &mut buf),
                None      => false,
            }
        };
        if !ok { super::super::exit(128 + 11); }

        // Restore GPRs from uc_mcontext.
        for i in 0..31 {
            user_frame.x[i] = u64::from_le_bytes(
                buf[REGS_OFFSET + i * 8..REGS_OFFSET + i * 8 + 8].try_into().unwrap()
            );
        }
        user_frame.sp_el0   = u64::from_le_bytes(buf[SP_OFFSET..SP_OFFSET+8].try_into().unwrap());
        user_frame.elr_el1  = u64::from_le_bytes(buf[PC_OFFSET..PC_OFFSET+8].try_into().unwrap());
        user_frame.spsr_el1 = u64::from_le_bytes(buf[PSTATE_OFFSET..PSTATE_OFFSET+8].try_into().unwrap());

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
