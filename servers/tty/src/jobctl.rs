//! Terminal job control — the one rule both the console and every pty apply
//! before a read, a write with `TOSTOP`, or a `tcsetpgrp`.
//!
//! This is Linux's `__tty_check_change()` (tty_io.c) and the `job_control()`
//! gate at the top of `n_tty_read()`:
//!
//! * A terminal only exerts job control over processes for which it is the
//!   **controlling terminal** — same session. Anything else proceeds untouched,
//!   which is what keeps a daemon whose stdio happens to be the console from
//!   ever being stopped by it.
//! * The **foreground group** proceeds.
//! * A background group gets `SIGTTIN` (read) or `SIGTTOU` (write, ioctl)
//!   sent to the *whole group* — unless the signal is ignored or blocked, in
//!   which case a read fails with `EIO` and a write/ioctl simply proceeds;
//!   and unless the group is orphaned (nothing could ever continue it), in
//!   which case everything is `EIO`.
//!
//! Linux then returns `-ERESTARTSYS`. This kernel has no syscall restart, so
//! [`check`] runs the stop right here (`sched::signal::stop_now_if_pending`)
//! and reports [`Verdict::Retry`]: the caller re-evaluates from the top once
//! the group has been continued, which is exactly what a restarted syscall
//! would do. A process that *catches* the signal gets `EINTR` instead, as on
//! Linux without `SA_RESTART`.

/// What the caller does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Go ahead with the operation.
    Proceed,
    /// Fail with `-EIO`.
    Eio,
    /// Fail with `-EINTR`: a handler for the job-control signal will run on
    /// the way back to user space.
    Eintr,
    /// The group was stopped and has now been continued: re-run the check.
    Retry,
}

pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;

/// Evaluate the job-control rule for the calling task against a terminal
/// whose controlling session is `term_sid` and foreground group `term_pgrp`
/// (either 0 = not established). `sig` is `SIGTTIN` for a read, `SIGTTOU`
/// for a write or `tcsetpgrp`.
///
/// Takes no terminal lock: the caller samples `(term_sid, term_pgrp)` under
/// its own lock, drops it, and calls this — the stop parks the calling thread
/// for an unbounded time and `kill_pgrp` takes the run-queue lock, neither of
/// which may happen under a terminal pool lock.
pub fn check(term_sid: u32, term_pgrp: u32, sig: u32) -> Verdict {
    if term_sid == 0 || term_pgrp == 0 {
        return Verdict::Proceed;
    }
    let (my_sid, my_pgid) = sched::current_sid_pgid();
    if my_sid != term_sid || my_pgid == 0 || my_pgid == term_pgrp {
        return Verdict::Proceed;
    }
    if sched::signal::signal_ignored_or_blocked(sig) {
        // A reader that cannot be told to wait gets an error; a writer or
        // tcsetpgrp caller that has opted out of SIGTTOU is allowed through.
        return if sig == SIGTTIN { Verdict::Eio } else { Verdict::Proceed };
    }
    if sched::signal::pgrp_is_orphaned(my_pgid) {
        return Verdict::Eio;
    }
    // A SIGKILL resumed us out of the stop below: leave, so the return to
    // user space acts on it. Retrying would re-raise the signal and park
    // again, and the process could then never be killed while its group
    // stays in the background (Linux: `-ERESTARTSYS` on a pending signal).
    if sched::signal::fatal_signal_pending() {
        return Verdict::Eintr;
    }
    // SI_KERNEL, as for ^C: the terminal, not a process, raised this.
    let _ = sched::kill_pgrp(my_pgid, sig, sched::SigInfo::KERNEL);
    if sched::signal::stop_now_if_pending(sig) {
        Verdict::Retry
    } else {
        Verdict::Eintr
    }
}

/// [`check`] for a `write`: only applies when the terminal has `TOSTOP`.
pub fn check_write(term_sid: u32, term_pgrp: u32, tostop: bool) -> Verdict {
    if !tostop {
        return Verdict::Proceed;
    }
    check(term_sid, term_pgrp, SIGTTOU)
}

/// Map a final verdict to an errno for callers that have already looped on
/// `Retry`.
pub fn errno(v: Verdict) -> isize {
    match v {
        Verdict::Proceed | Verdict::Retry => 0,
        Verdict::Eio => -5,
        Verdict::Eintr => -4,
    }
}
