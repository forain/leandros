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
// SA_RESTORER only gates x86-64 frame construction; aarch64 always uses the
// kernel trampoline, so the const is unreferenced there.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
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
//   SIGCONT = 18  (bit 17) — its default action is "resume if stopped", which
//                 `deliver_signal*` performs at send time; by the time the
//                 signal itself is dequeued there is nothing left to do.
//   SIGURG  = 23  (bit 22)
//   SIGWINCH = 28 (bit 27)
const SIGDFL_IGNORE: u64 = (1u64 << 16) | (1u64 << 17) | (1u64 << 22) | (1u64 << 27);

/// Signals whose SIG_DFL action is "stop the process": SIGSTOP 19, SIGTSTP 20,
/// SIGTTIN 21, SIGTTOU 22 (bits 18..=21).
pub(crate) const SIGDFL_STOP: u64 = (1u64 << 18) | (1u64 << 19) | (1u64 << 20) | (1u64 << 21);

// Signal numbers used for default-terminate calculation.
const SIGSEGV: u32 = 11;
const SIGCHLD: u32 = 17;

pub(crate) const SIGKILL: u32 = 9;
pub(crate) const SIGCONT: u32 = 18;
pub(crate) const SIGSTOP: u32 = 19;

/// `sa_flags` bit on the parent's SIGCHLD action: do not send SIGCHLD when a
/// child stops or continues (Linux value).
const SA_NOCLDSTOP: u32 = 0x0000_0001;

/// The two signals POSIX makes undeniable: they can never be blocked, caught,
/// or ignored.
///
/// Enforcing this is not pedantry. `sigprocmask` used to apply the caller's set
/// verbatim, and musl (and Rust's `std`) block the *entire* signal set around
/// `fork`/`posix_spawn` — so a process could, and routinely did, end up with
/// SIGKILL masked on every thread. `deliver_signal_process` then found no
/// eligible thread, parked the bit on the leader's `shared_signal_pending`, and
/// returned success. `kill -9` reported that it had worked while the target
/// went on running.
pub(crate) const UNBLOCKABLE: u64 = (1u64 << (SIGKILL - 1)) | (1u64 << (SIGSTOP - 1));

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
            // The `SigInfo` payload has to travel with the bit, one slot at a
            // time, and ONLY for the bits actually being claimed.
            //
            // This is the single place where getting it wrong would be worse
            // than shipping nothing: `claimable` is a *subset* of the leader's
            // parked signals (the ones this thread has unmasked), so a
            // whole-array copy would also overwrite the claiming thread's own
            // payloads for signals it never claimed — a SIGCHLD delivered
            // carrying, say, a SIGUSR1's `SI_USER`/`si_pid`. The loop below
            // therefore walks the set bits of `claimable` and touches exactly
            // those indices.
            //
            // Per-slot it keeps the same first-writer-wins rule the producers
            // use: if this thread already had that signal pending, the parked
            // process-level instance is a duplicate of an already-pending
            // standard signal, so its payload is discarded with it.
            let mut c = claimable;
            while c != 0 {
                let b = c.trailing_zeros() as usize;
                c &= !(1u64 << b);
                let parked = rq.find_pid(tgid)
                    .map(|l| l.signal_info[b])
                    .unwrap_or(crate::task::SigInfo::NONE);
                if let Some(ti) = rq.find_pid_idx(pid) {
                    if let Some(t) = rq.get_mut(ti) {
                        if t.signal_pending & (1u64 << b) == 0 {
                            t.signal_info[b] = parked;
                        }
                        t.signal_pending |= 1u64 << b;
                    }
                }
            }
            if let Some(li) = rq.find_pid_idx(tgid) {
                if let Some(l) = rq.get_mut(li) { l.shared_signal_pending &= !claimable; }
            }
        }
    }

    // One `RUN_QUEUE` acquisition per iteration decides between three
    // outcomes: nothing to do (return), park this thread because a sibling
    // stopped the group, or deliver one signal.
    enum Step {
        Park,
        Deliver(u32, crate::task::SigAction, u64, crate::task::SigInfo),
    }

    loop {
        // Sample the pending+mask state under the queue lock, then release it
        // before any further work (signal frame writing might block elsewhere).
        let step = {
            let mut rq = super::RUN_QUEUE.lock();
            let idx = match rq.find_pid_idx(pid) { Some(i) => i, None => return };
            let park = match rq.get(idx) {
                Some(t) => t.stop_pending && t.state != crate::task::TaskState::Zombie,
                None => return,
            };
            if park {
                // Another thread of this group took a stop signal and asked
                // every sibling to park (see `do_signal_stop`). Honour it
                // here, on the way back to user space, rather than waiting
                // for the next preemption to route through the scheduler's
                // post-dispatch check. Same critical section as the sample,
                // so the common no-signal path still costs one lock.
                if let Some(t) = rq.get_mut(idx) {
                    t.state = crate::task::TaskState::Stopped;
                    clear_block_fields(t);
                }
                Step::Park
            } else { match rq.get(idx) {
                Some(t) => {
                    let unmasked = t.signal_pending & !t.signal_mask;
                    if unmasked == 0 { return; }
                    // A synchronous fault signal (SIGSEGV et al.) is delivered
                    // ahead of anything else pending: its handler must run
                    // before the faulting instruction is retried.
                    let sync = unmasked & crate::task::SYNCHRONOUS_MASK;
                    let pick = if sync != 0 { sync } else { unmasked };
                    let bit  = pick.trailing_zeros() as u32;
                    let sig  = bit + 1;
                    let mask = t.signal_mask;
                    // Signal disposition is shared across the thread group: read
                    // from the TGID leader so all threads see installed handlers.
                    //
                    // SIGKILL/SIGSTOP ignore that table entirely: POSIX says
                    // they can be neither caught nor ignored, so a handler
                    // registered for them (which `sys_sigaction` now rejects,
                    // but an already-installed one could predate that) must not
                    // be able to divert them.
                    let action = if UNBLOCKABLE & (1u64 << bit) != 0 {
                        crate::task::DEFAULT_SIGACTION
                    } else {
                        rq.find_pid(t.tgid)
                            .map(|leader| leader.signal_actions[bit as usize])
                            .unwrap_or(crate::task::DEFAULT_SIGACTION)
                    };
                    // Read the payload in the same critical section that picks
                    // the signal, indexed by the same `bit` — the two can never
                    // be sampled from different signals.
                    let info = t.signal_info[bit as usize];
                    Step::Deliver(sig, action, mask, info)
                }
                None => return,
            } }
        };

        let (sig, action, old_mask, info) = match step {
            Step::Deliver(sig, action, mask, info) => (sig, action, mask, info),
            Step::Park => {
                // Resumed by SIGCONT/SIGKILL, which cleared `stop_pending`
                // before making us Ready; loop again so anything that arrived
                // while stopped is delivered on this same return to user
                // space. A stop that raced in between simply parks us again.
                super::yield_now("stopped");
                continue;
            }
        };

        // Dequeue: clear the pending bit under the lock.
        //
        // The signal mask and SA_RESETHAND are deliberately NOT touched here.
        // They belong to *running a handler*, and this loop reaches all three
        // dispositions. Applying them unconditionally left every ignored signal
        // permanently blocked: SIG_IGN and default-ignore both `continue` below
        // without ever building a sigframe, so nothing ever ran the
        // `rt_sigreturn` that restores the pre-handler mask. One ignored
        // SIGUSR2 and the process could never receive SIGUSR2 again — and the
        // common case was worse, because SIGCHLD is in `SIGDFL_IGNORE`: a
        // program that had not (yet) installed a SIGCHLD handler had SIGCHLD
        // silently added to its mask by its first child exit, so a handler
        // installed afterwards was never called and `sigprocmask` reported a
        // block the program never asked for. Linux applies the blocked set in
        // `handle_signal()`, on the caught path only, for exactly this reason.
        {
            let mut rq = super::RUN_QUEUE.lock();
            if let Some(idx) = rq.find_pid_idx(pid) {
                if let Some(t) = rq.get_mut(idx) {
                    t.signal_pending &= !(1u64 << (sig - 1));
                }
            }
        }

        match action.handler {
            0 => {
                // SIG_DFL
                if SIGDFL_IGNORE & (1u64 << (sig - 1)) != 0 {
                    continue; // check next pending signal
                }
                // Default action "stop": park the whole thread group in
                // `TaskState::Stopped` until SIGCONT (or SIGKILL). Returns once
                // the group has been continued; loop again so a SIGCONT
                // handler, or anything that arrived while stopped, is
                // delivered on this same return to user space.
                if SIGDFL_STOP & (1u64 << (sig - 1)) != 0 {
                    do_signal_stop(pid, sig);
                    continue;
                }
                // Default action: terminate — the whole thread group, not just
                // this thread. A fatal signal ends a *process*; `exit` alone
                // would reap only the thread that happened to take delivery,
                // and `deliver_signal_process` prefers a blocked thread (an
                // epoll-parked tokio worker, typically) over the leader. That
                // is what made `kill -9` on a threaded process a coin flip.
                //
                // `exit_group_signal`, not `exit_group(128 + sig)`: the latter
                // reports the death as a *normal exit* with a strange code, so
                // every WIFSIGNALED/WTERMSIG consumer (brush's job control,
                // Rust's Command::status, cosmic-term's SIGCHLD reaper) misses
                // that the process was killed at all.
                super::exit_group_signal(sig);
            }
            1 => {
                // SIG_IGN — skip, check next.
                continue;
            }
            handler => {
                // A handler is going to run: now apply its blocked set (the
                // signal itself unless SA_NODEFER, plus `sa_mask`) and the
                // one-shot SA_RESETHAND. `old_mask`, sampled before any of
                // this, is what goes into the frame's `uc_sigmask` and what
                // `rt_sigreturn` puts back.
                {
                    let mut rq = super::RUN_QUEUE.lock();
                    let tgid = rq.find_pid(pid).map(|t| t.tgid).unwrap_or(0);
                    if action.get_flags() & SA_NODEFER == 0 {
                        if let Some(idx) = rq.find_pid_idx(pid) {
                            if let Some(t) = rq.get_mut(idx) {
                                t.signal_mask |= (1u64 << (sig - 1)) | action.mask;
                            }
                        }
                    }
                    // SA_RESETHAND: revert to SIG_DFL on the shared (TGID
                    // leader) table.
                    if action.get_flags() & SA_RESETHAND != 0 && tgid != 0 {
                        if let Some(idx) = rq.find_pid_idx(tgid) {
                            if let Some(leader) = rq.get_mut(idx) {
                                leader.signal_actions[(sig - 1) as usize].handler = 0;
                            }
                        }
                    }
                }

                // aarch64: always return through the kernel-provided
                // rt_sigreturn trampoline, ignoring any userspace
                // sa_restorer. This mirrors the Linux-aarch64 kernel, which
                // has no SA_RESTORER — the restorer field still round-trips
                // through `sigaction(oldact)`, it is just not used to build
                // the frame. Mapped by execve; a pre-exec task without the
                // page simply must not return from its handler, as before.
                #[cfg(target_arch = "aarch64")]
                let restorer = SIGRET_TRAMPOLINE_VA;
                // x86_64: honor the userspace-supplied sa_restorer when
                // SA_RESTORER is set (relibc always sets it), else 0.
                #[cfg(not(target_arch = "aarch64"))]
                let restorer = if action.get_flags() & SA_RESTORER != 0 {
                    action.get_restorer()
                } else {
                    0
                };

                if !arch_prepare_signal_frame(frame_ptr, sig, handler, restorer, old_mask, action.get_flags(), info) {
                    // Frame write failed (stack fault) — deliver SIGSEGV.
                    super::exit_group_signal(SIGSEGV);
                }

                // One signal delivered; re-check for more on the next syscall return.
                return;
            }
        }
    }
}

// ── Job control: stopping and resuming a thread group ────────────────────────

/// Reset the wait-channel bookkeeping of a task that is being taken out of
/// `Blocked` by something other than the channel it was parked on (a stop or
/// a continue). Leaves nothing that a later `unblock_port`/deadline scan could
/// mistake for a live registration.
pub(crate) fn clear_block_fields(t: &mut crate::task::Task) {
    t.blocked_on    = None;
    t.poll_deadline = u64::MAX;
    t.poll_mask     = super::POLL_TAG_ALL;
    t.blocked_futex = 0;
}

/// Default action of SIGSTOP/SIGTSTP/SIGTTIN/SIGTTOU, run by the thread that
/// dequeued the signal: stop every thread of the group, tell the parent, and
/// park until SIGCONT (or SIGKILL) resumes the group.
///
/// Threads that are `Ready` or `Blocked` (not on any CPU) are moved to
/// `Stopped` right here. A thread mid-flight on another CPU cannot be — its
/// registers are still live there — so it gets `stop_pending` plus a
/// reschedule IPI, and parks itself either in `check_and_deliver_signals` on its
/// next return to user space or in the scheduler's post-dispatch check the
/// moment it stops running (see `scheduler_run_loop`). Only the pick-side
/// state matters for correctness: `pick_next` dispatches `Ready` tasks only.
///
/// A thread parked while `Blocked` loses any channel wake that arrives during
/// the stop (`unblock_port*`/`futex_wake`/the deadline tick all require
/// `Blocked`). That is deliberate: SIGCONT makes it `Ready`, it returns from
/// its yield as a spurious wake, and every blocking primitive re-checks its
/// condition and re-parks. The alternative — leaving it `Blocked` and having
/// every wake path consult a stop flag — spreads job control over a dozen
/// wake sites.
fn do_signal_stop(pid: super::task::Pid, sig: u32) {
    let mut kick: [Option<usize>; super::MAX_CPUS] = [None; super::MAX_CPUS];
    let (ppid, uid, tgid) = {
        let mut rq = super::RUN_QUEUE.lock();
        let (tgid, ppid, uid) = match rq.find_pid(pid) {
            Some(t) => (t.tgid, t.ppid, t.uid),
            None => return,
        };
        // Leader bookkeeping for wait4/waitid. A fresh stop supersedes any
        // continue that was still unreported; a stop while already stopped
        // (SIGTSTP after SIGSTOP) is not a new state change.
        if let Some(l) = rq.find_pid_mut(tgid) {
            if l.stop_signal == 0 {
                l.stop_signal   = sig as u8;
                l.stop_reported = false;
            }
            l.cont_pending = false;
        }
        let mut n = 0;
        for i in 0..super::runqueue::MAX_TASKS {
            if let Some(t) = rq.get_mut(i) {
                if t.tgid != tgid || t.state == crate::task::TaskState::Zombie { continue; }
                // POSIX: a stop signal discards a pending SIGCONT.
                t.signal_pending &= !(1u64 << (SIGCONT - 1));
                t.stop_pending = true;
                if t.pid == pid {
                    t.state = crate::task::TaskState::Stopped;
                    clear_block_fields(t);
                } else if let Some(cpu) = t.on_cpu {
                    if n < kick.len() { kick[n] = Some(cpu); n += 1; }
                } else if matches!(t.state, crate::task::TaskState::Ready | crate::task::TaskState::Blocked) {
                    t.state = crate::task::TaskState::Stopped;
                    clear_block_fields(t);
                }
            }
        }
        (ppid, uid, tgid)
    };
    for cpu in kick.iter().flatten() {
        super::trigger_preempt(*cpu);
    }
    // Tell the parent (SIGCHLD/CLD_STOPPED) unless it opted out with
    // SA_NOCLDSTOP. `deliver_signal_process` also wakes the poll channel, so a
    // parent blocked in wait4(WUNTRACED) re-scans and sees the stop.
    notify_parent_state_change(tgid, ppid, uid,
        crate::task::SigInfo::child_state(crate::task::CLD_STOPPED, tgid, uid, sig));
    super::yield_now("stopped");
}

// ── Terminal job control: SIGTTIN/SIGTTOU from inside a syscall ──────────────

/// SIGHUP, for the orphaned-process-group rule.
const SIGHUP: u32 = 1;

/// Is `sig` ignored by the calling process — `SIG_IGN`, or blocked on the
/// calling thread? This is Linux's `is_ignored()` in `n_tty`/`tty_io`, the
/// test that decides whether a background read/write/`tcsetpgrp` raises the
/// job-control signal or is answered without one (EIO for a read, silent
/// success for the other two).
pub fn signal_ignored_or_blocked(sig: u32) -> bool {
    if sig == 0 || sig > 64 { return true; }
    let pid = super::current_pid();
    if pid == 0 { return true; }
    let bit = 1u64 << (sig - 1);
    let rq = super::RUN_QUEUE.lock();
    let (tgid, mask) = match rq.find_pid(pid) {
        Some(t) => (t.tgid, t.signal_mask),
        None => return true,
    };
    if mask & bit != 0 { return true; }
    rq.find_pid(tgid)
        .map(|l| l.signal_actions[(sig - 1) as usize].handler == 1)
        .unwrap_or(true)
}

/// The terminal just sent `sig` (SIGTTIN/SIGTTOU) to the calling process's
/// own group from inside a read/write/ioctl, and the syscall cannot complete
/// until the group is continued in the foreground. Linux returns
/// `-ERESTARTSYS` and re-executes the syscall after `SIGCONT`; this kernel
/// has no restart mechanism, so the stop runs *here*, inside the syscall,
/// and the caller loops back to re-check the foreground group once
/// `do_signal_stop` returns.
///
/// Returns `true` when the default action ran (the caller retries) and
/// `false` when the process catches the signal — then the handler must run
/// on the way back to user space and the syscall reports `EINTR`, which is
/// what Linux does without `SA_RESTART`.
///
/// `kill_pgrp` may have parked the bit on a sibling thread of a threaded
/// process; it is claimed from whichever thread holds it so the stop is not
/// taken twice — once here and once on that thread's next return to user
/// space. If the bit is already gone (a sibling dequeued it first and
/// stopped the group, leaving this thread `stop_pending`), park directly.
pub fn stop_now_if_pending(sig: u32) -> bool {
    if sig == 0 || sig > 64 || SIGDFL_STOP & (1u64 << (sig - 1)) == 0 { return false; }
    let pid = super::current_pid();
    if pid == 0 { return false; }
    let bit = 1u64 << (sig - 1);
    let park_only = {
        let mut rq = super::RUN_QUEUE.lock();
        let tgid = match rq.find_pid(pid) { Some(t) => t.tgid, None => return false };
        let handler = rq.find_pid(tgid)
            .map(|l| l.signal_actions[(sig - 1) as usize].handler)
            .unwrap_or(0);
        if handler != 0 { return false; } // caught (or SIG_IGN, never sent): EINTR path
        let mut found = false;
        for i in 0..super::runqueue::MAX_TASKS {
            if let Some(t) = rq.get_mut(i) {
                if t.tgid != tgid { continue; }
                if t.signal_pending & bit != 0 {
                    t.signal_pending &= !bit;
                    found = true;
                }
            }
        }
        if let Some(l) = rq.find_pid_mut(tgid) {
            if l.shared_signal_pending & bit != 0 {
                l.shared_signal_pending &= !bit;
                found = true;
            }
        }
        if found {
            false
        } else {
            match rq.find_pid_mut(pid) {
                Some(t) if t.stop_pending => {
                    t.state = crate::task::TaskState::Stopped;
                    clear_block_fields(t);
                    true
                }
                _ => return true, // already continued: nothing to wait for
            }
        }
    };
    if park_only {
        super::yield_now("stopped");
    } else {
        do_signal_stop(pid, sig);
    }
    true
}

/// POSIX orphaned process group: every member's parent is either in the
/// group itself or in another session (a dead parent counts as gone). A
/// background job in such a group can never be continued by a shell, so the
/// terminal answers its reads with EIO instead of stopping it forever.
///
/// Evaluated against a run-queue borrow: `kill_orphaned_pgrps` scans several
/// groups under one acquisition.
fn pgrp_orphaned_locked(rq: &super::runqueue::RunQueue, pgid: super::task::Pid) -> bool {
    for i in 0..super::runqueue::MAX_TASKS {
        if let Some(t) = rq.get(i) {
            if t.pgid != pgid || t.pid != t.tgid
                || t.state == crate::task::TaskState::Zombie { continue; }
            // The parent's group/session are read from ITS leader: a child
            // forked by a worker thread has that thread as `ppid`, and only
            // leaders carry authoritative `pgid`/`sid` (see current_sid_pgid).
            let parent = rq.find_pid(t.ppid)
                .and_then(|p| if p.pid == p.tgid { Some(p) } else { rq.find_pid(p.tgid) });
            if let Some(p) = parent {
                if p.state != crate::task::TaskState::Zombie
                    && p.pgid != pgid && p.sid == t.sid {
                    return false;
                }
            }
        }
    }
    true
}

pub fn pgrp_is_orphaned(pgid: super::task::Pid) -> bool {
    if pgid == 0 { return true; }
    let rq = super::RUN_QUEUE.lock();
    pgrp_orphaned_locked(&rq, pgid)
}

/// Any live process of `pgid` with a job-control stop on record. Leaders
/// only: `stop_signal` lives there, and a non-leader thread's `pgid` may be
/// stale (see `current_sid_pgid`).
fn pgrp_has_stopped_locked(rq: &super::runqueue::RunQueue, pgid: super::task::Pid) -> bool {
    for i in 0..super::runqueue::MAX_TASKS {
        if let Some(t) = rq.get(i) {
            if t.pgid != pgid || t.pid != t.tgid
                || t.state == crate::task::TaskState::Zombie { continue; }
            if t.state == crate::task::TaskState::Stopped || t.stop_signal != 0 {
                return true;
            }
        }
    }
    false
}

/// The orphaned-process-group rule (POSIX 2.2.2.52, Linux
/// `kill_orphaned_pgrp`): when `exiting` leaves, any process group it was
/// anchoring — its own, or a child's group in the same session — that has
/// just become orphaned *and* has stopped members gets SIGHUP followed by
/// SIGCONT. Without it a job stopped under a shell that then died stays
/// stopped for ever, with nothing left that could ever `fg` it.
///
/// Called from `exit` once the leader is already a zombie, so the scan
/// naturally ignores it. Runs with no locks held: the signals go out after
/// the run-queue scan.
pub(crate) fn kill_orphaned_pgrps(exiting: super::task::Pid) {
    const MAX_GROUPS: usize = 16;
    let mut doomed = [0 as super::task::Pid; MAX_GROUPS];
    let mut n = 0usize;
    {
        let rq = super::RUN_QUEUE.lock();
        let (my_pgid, my_sid) = match rq.find_pid(exiting) {
            Some(t) => (t.pgid, t.sid),
            None => return,
        };
        let mut cands = [0 as super::task::Pid; MAX_GROUPS];
        let mut nc = 0usize;
        if my_pgid != 0 { cands[nc] = my_pgid; nc += 1; }
        for i in 0..super::runqueue::MAX_TASKS {
            if let Some(t) = rq.get(i) {
                if t.pid != t.tgid || t.state == crate::task::TaskState::Zombie
                    || t.sid != my_sid || t.pgid == 0 || t.pgid == my_pgid { continue; }
                // A child of the exiting process, in its session, in another
                // group: the exiting process was what kept that group anchored.
                // The cheap field tests above run first; `find_pid` is a scan,
                // and this runs on every process exit.
                let parent_tgid = if t.ppid == exiting { exiting } else {
                    rq.find_pid(t.ppid).map(|p| p.tgid).unwrap_or(t.ppid)
                };
                if parent_tgid != exiting { continue; }
                if cands[..nc].contains(&t.pgid) { continue; }
                if nc < MAX_GROUPS { cands[nc] = t.pgid; nc += 1; }
            }
        }
        // Stopped members first: that is one linear pass and almost always
        // false, and only then the parent-per-member orphan scan.
        for &g in &cands[..nc] {
            if pgrp_has_stopped_locked(&rq, g) && pgrp_orphaned_locked(&rq, g) {
                doomed[n] = g; n += 1;
            }
        }
    }
    for &g in &doomed[..n] {
        let _ = super::kill_pgrp(g, SIGHUP, crate::task::SigInfo::KERNEL);
        let _ = super::kill_pgrp(g, SIGCONT, crate::task::SigInfo::KERNEL);
    }
}

/// SIGCHLD to the parent of process `tgid` for a stop/continue, honouring
/// the parent's SA_NOCLDSTOP. Runs with no locks held.
pub(crate) fn notify_parent_state_change(tgid: super::task::Pid, ppid: super::task::Pid,
                                         _uid: u32, info: crate::task::SigInfo) {
    if tgid == 0 || ppid == 0 { return; }
    let parent_tgid = super::tgid_of(ppid);
    let suppressed = {
        let rq = super::RUN_QUEUE.lock();
        rq.find_pid(parent_tgid)
            .map(|p| p.signal_actions[(SIGCHLD - 1) as usize].get_flags() & SA_NOCLDSTOP != 0)
            .unwrap_or(true)
    };
    if suppressed {
        // Still wake a parent parked in wait4/waitid: the state change is
        // reportable even when the signal is not wanted.
        super::wake_poll();
        return;
    }
    let _ = super::deliver_signal_process(parent_tgid, SIGCHLD, info);
}

// ── Synchronous faults ───────────────────────────────────────────────────────

/// Route a user-mode CPU fault to the task's signal handler, if one can run.
///
/// Returns `true` when `sig` has been queued on the calling thread carrying
/// `SigInfo::fault(si_code, addr)`; the arch fault stub then falls through to
/// `check_and_deliver_signals`, which builds the signal frame and redirects
/// the return-to-user to the handler. Returns `false` when the fault must
/// kill the process — SIG_DFL, SIG_IGN, or the signal blocked — mirroring
/// Linux's `force_sig_fault`: an ignored or blocked synchronous signal is
/// not deferred, because the faulting instruction would just fault again.
/// The caller prints its diagnostics and calls `exit_group_signal`.
///
/// The blocked check is also the re-entrancy guard. A handler runs with its
/// own signal masked (unless SA_NODEFER), so a handler that faults *again*
/// arrives here with the bit set and is killed instead of recursing.
///
/// The payload overwrites any earlier pending instance of the same signal:
/// a fault's `si_addr` is what the handler is about to inspect, and a stale
/// `kill(2)` payload for the same number would misdirect it.
pub fn fault_signal(sig: u32, si_code: i32, addr: usize) -> bool {
    if sig == 0 || sig > 64 { return false; }
    let pid = super::current_pid();
    if pid == 0 { return false; }
    let bit = 1u64 << (sig - 1);
    let mut rq = super::RUN_QUEUE.lock();
    let (tgid, mask) = match rq.find_pid(pid) {
        Some(t) => (t.tgid, t.signal_mask),
        None => return false,
    };
    let handler = rq.find_pid(tgid)
        .map(|l| l.signal_actions[(sig - 1) as usize].handler)
        .unwrap_or(0);
    if handler <= 1 || mask & bit != 0 { return false; }
    if let Some(t) = rq.find_pid_mut(pid) {
        t.signal_info[(sig - 1) as usize] = crate::task::SigInfo::fault(si_code, addr);
        t.signal_pending |= bit;
    }
    true
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
    let idx = (signum - 1) as usize;
    let is_unblockable = UNBLOCKABLE & (1u64 << idx) != 0;

    // Read the incoming action from user memory BEFORE taking RUN_QUEUE — a
    // fault on act_ptr under the lock self-deadlocks the scheduler (see the
    // detailed note in sys_sigprocmask). SIGKILL/SIGSTOP can't be installed, so
    // their act is never dereferenced, matching the original "return EINVAL
    // without reading act" behavior.
    let new = if act_ptr != 0 && !is_unblockable {
        Some(unsafe { core::ptr::read(act_ptr as *const crate::task::SigAction) })
    } else {
        None
    };

    // Signal actions belong to the thread group — always read/write through leader.
    let old;
    let mut ret = 0isize;
    {
        let mut rq = super::RUN_QUEUE.lock();
        let tgid = match rq.find_pid(pid) {
            Some(t) => t.tgid,
            None    => return -3,
        };
        let leader = match rq.find_pid_mut(tgid) {
            Some(l) => l,
            None    => return -3,
        };
        old = leader.signal_actions[idx];
        if act_ptr != 0 {
            // Installing a disposition for SIGKILL/SIGSTOP is EINVAL — they can
            // be neither caught nor ignored. Reported *after* `oldact` is
            // filled, matching Linux: querying the current (always SIG_DFL)
            // action is legal, only changing it is not.
            if is_unblockable {
                ret = -22;
            } else if let Some(new) = new {
                leader.signal_actions[idx] = new;
            }
        }
    }
    // oldact is written outside the lock (fault-safe) and reports the pre-call
    // action even on EINVAL, preserving the original ordering.
    if oldact_ptr != 0 {
        unsafe { core::ptr::write(oldact_ptr as *mut crate::task::SigAction, old); }
    }
    ret
}

/// POSIX `execve`: signal dispositions that were being *caught* (a real handler
/// function) revert to `SIG_DFL` in the new process image; `SIG_IGN` and
/// `SIG_DFL` are preserved, and the signal mask / pending set are left untouched.
///
/// Runs at successful exec, before `replace_address_space` hands control to the
/// new image. Skipping it leaves a stale handler installed across the exec —
/// and because a program can exec *itself* (LeandrOS ships `/bin/sh` as a
/// hardlink to the same fixed-base ET_EXEC), the handler can reappear at the
/// identical address in the new image. A fresh `signal_hook_registry` then reads
/// that stale handler back as the "previous handler" via `sigaction(oldact)`,
/// stores it as its own `prev`, and chains to it on the next signal — calling
/// itself, unboundedly, until the stack faults (the COSMIC launcher's
/// `exec /bin/sh dbus-run-session …` + SIGCHLD reproduced this exactly).
///
/// Dispositions belong to the thread-group leader, so this takes the tgid.
pub fn reset_handlers_on_exec(tgid: super::task::Pid) {
    let mut rq = super::RUN_QUEUE.lock();
    if let Some(leader) = rq.find_pid_mut(tgid) {
        for act in leader.signal_actions.iter_mut() {
            // handler encoding: 0 = SIG_DFL, 1 = SIG_IGN, >= 2 = caught fn ptr.
            // Only caught handlers revert; SIG_IGN and SIG_DFL are preserved.
            if act.handler >= 2 {
                *act = crate::task::DEFAULT_SIGACTION;
            }
        }
    }
}

pub fn sys_sigprocmask(how: usize, set_ptr: usize, oldset_ptr: usize) -> isize {
    const SIG_BLOCK:   usize = 0;
    const SIG_UNBLOCK: usize = 1;
    const SIG_SETMASK: usize = 2;

    let pid = super::current_pid();

    // Touch user memory OUTSIDE the RUN_QUEUE lock. A page fault on set_ptr or
    // oldset_ptr re-enters the scheduler via handle_page_fault ->
    // lock_leader_address_space, which re-acquires RUN_QUEUE. Dereferencing a
    // user pointer while already holding RUN_QUEUE therefore self-deadlocks the
    // faulting CPU, and every other CPU that then needs the run queue piles up
    // behind it with interrupts masked — the whole machine (and the global
    // timer tick) freezes. tokio's multi-thread runtime bootstrap hit this
    // reliably on x86-64: its signal-driver setup calls rt_sigprocmask with a
    // mask pointer whose page wasn't yet resident, faulting under the lock.
    let set = if set_ptr != 0 {
        Some(unsafe { core::ptr::read(set_ptr as *const u64) })
    } else {
        None
    };

    let old_mask;
    let mut ret = 0isize;
    {
        let mut rq = super::RUN_QUEUE.lock();
        let t = match rq.find_pid_mut(pid) {
            Some(t) => t,
            None    => return -3, // ESRCH
        };
        old_mask = t.signal_mask;
        if let Some(set) = set {
            // Silently drop SIGKILL/SIGSTOP from anything that would *add* to
            // the blocked set, exactly as Linux's `sigprocmask` does — the call
            // still succeeds, those two bits just never take. Unblocking needs
            // no filtering: clearing a bit that can never be set is a no-op.
            match how {
                SIG_BLOCK   => t.signal_mask |= set & !UNBLOCKABLE,
                SIG_UNBLOCK => t.signal_mask &= !set,
                SIG_SETMASK => t.signal_mask = set & !UNBLOCKABLE,
                _           => ret = -22, // EINVAL — mask left unchanged
            }
        }
    }
    // oldset is reported with the pre-call mask even on EINVAL, matching the
    // original ordering (Linux fills oldset before validating `how`).
    if oldset_ptr != 0 {
        unsafe { core::ptr::write(oldset_ptr as *mut u64, old_mask); }
    }
    ret
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
    info:         crate::task::SigInfo,
) -> bool {
    #[cfg(target_arch = "aarch64")]
    return aarch64::prepare(frame_ptr, sig, handler, restorer, old_mask, action_flags, info);

    #[cfg(target_arch = "x86_64")]
    return x86_64::prepare(frame_ptr, sig, handler, restorer, old_mask, action_flags, info);

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    { let _ = (frame_ptr, sig, handler, restorer, old_mask, action_flags, info); false }
}

/// The `siginfo_t` field offsets both architectures share.
///
/// LP64 Linux uses one `siginfo_t` layout for x86-64 and AArch64 alike — a
/// three-`int` header (`si_signo`, `si_errno`, `si_code`) followed by the
/// `_sifields` union at 16, whose `_kill` and `_sigchld` members start with
/// `si_pid` then `si_uid`, and whose `_sigchld` continues with `si_status`.
/// relibc's `header::signal::linux` mirrors it, so a handler taking
/// `siginfo_t *` reads exactly these offsets on both.
mod si_off {
    pub const SIGNO:  usize = 0;
    pub const CODE:   usize = 8;
    pub const PID:    usize = 16;
    pub const UID:    usize = 20;
    pub const STATUS: usize = 24;
    /// `_sifields._sigfault.si_addr` — the union's first word, overlaying
    /// `si_pid`/`si_uid`.
    pub const ADDR:   usize = 16;
}

/// Serialise the carried `siginfo_t` fields into a zeroed frame buffer at
/// `base` (the start of the frame's siginfo region).
///
/// Shared by both architectures so the offsets cannot drift between them —
/// they are the same offsets, and one function is the cheapest way to keep
/// saying so. The `_sifields` member is chosen by signal class: a
/// kernel-generated (`si_code > 0`) SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGTRAP
/// carries `_sigfault` (`si_addr`), everything else `_kill`/`_sigchld`.
fn write_siginfo(buf: &mut [u8], base: usize, sig: u32, info: crate::task::SigInfo) {
    let put = |buf: &mut [u8], off: usize, v: [u8; 4]| {
        buf[base + off..base + off + 4].copy_from_slice(&v);
    };
    put(buf, si_off::SIGNO,  sig.to_le_bytes());
    put(buf, si_off::CODE,   info.si_code.to_le_bytes());
    let is_fault = sig >= 1 && sig <= 64
        && crate::task::SYNCHRONOUS_MASK & (1u64 << (sig - 1)) != 0
        && info.si_code > 0;
    if is_fault {
        buf[base + si_off::ADDR..base + si_off::ADDR + 8]
            .copy_from_slice(&(info.si_addr as u64).to_le_bytes());
    } else {
        put(buf, si_off::PID,    info.si_pid.to_le_bytes());
        put(buf, si_off::UID,    info.si_uid.to_le_bytes());
        put(buf, si_off::STATUS, info.si_status.to_le_bytes());
    }
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

    // Offsets within siginfo: see `super::si_off`, shared with x86-64.
    const SI_OFFSET: usize = 0;

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
        info:         crate::task::SigInfo,
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

        // siginfo: si_signo/si_code/si_pid/si_uid/si_status.
        super::write_siginfo(&mut buf, SI_OFFSET, sig, info);

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
        if !ok { super::super::exit_group_signal(super::SIGSEGV); }

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
    // [824 .. 952)  info (the full 128-byte siginfo_t: si_signo, si_errno,
    //                      si_code, then the _sifields union from +16 —
    //                      si_pid, si_uid, si_status for _kill/_sigchld)

    const PRETCODE_OFFSET:  usize = 0;
    const UC_OFFSET:        usize = PRETCODE_OFFSET + 8;   // 8
    const STACK_OFFSET:     usize = UC_OFFSET + 16;         // 24
    const MCONTEXT_OFFSET:  usize = STACK_OFFSET + 24;      // 48
    const GREGS_OFFSET:     usize = MCONTEXT_OFFSET;        // 48
    const SIGMASK_OFFSET:   usize = MCONTEXT_OFFSET + 256;  // 304
    const INFO_OFFSET:      usize = UC_OFFSET + 816;        // 824
    // The full `siginfo_t` is 128 bytes on LP64 Linux. The frame used to
    // reserve only the leading 48 — enough for every field the kernel wrote
    // (si_status ends at 28) but not for a handler that copies `*info` by
    // value, which would take the tail out of whatever user stack happened to
    // sit above the frame. Reserving all 128 costs 80 bytes of user stack per
    // delivery and makes the region an actual siginfo_t.
    pub const SIGFRAME_SIZE: usize = INFO_OFFSET + 128;     // 952

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
        info:         crate::task::SigInfo,
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

        // siginfo: si_signo/si_code/si_pid/si_uid/si_status.
        super::write_siginfo(&mut buf, INFO_OFFSET, sig, info);

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
        if !ok { super::super::exit_group_signal(super::SIGSEGV); }

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
