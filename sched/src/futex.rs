//! Futex wait/wake implementation.
//!
//! Futex keys are user-space virtual addresses.  Since all threads in a thread
//! group share the same virtual address space, keying on VA is sufficient for
//! FUTEX_PRIVATE (process-private) futexes.  Shared futexes (across processes)
//! would require a physical-address key and are deferred to a later phase.
//!
//! # SMP race-freedom
//!
//! On SMP the classic lost-wake-up race is: CPU A reads `*uaddr == val`,
//! decides to sleep; CPU B changes the value and calls `futex_wake` before A
//! registers itself; A then blocks forever.  We close it by serializing on
//! the `FUTEX_TABLE` lock:
//!
//!  * `futex_wait` re-reads the user value, registers the waiter, **and**
//!    marks the task Blocked all inside one `FUTEX_TABLE` critical section.
//!  * `futex_wake` collects and wakes waiters while holding `FUTEX_TABLE`.
//!
//! A waker's value-store happens before its `futex_wake` lock acquisition, so
//! either the waiter sees the new value (returns `EAGAIN`), or the waker sees
//! the registered waiter (wakes it).  Lock order is always
//! `FUTEX_TABLE → RUN_QUEUE`.

use super::{CURRENT_CTX, SCHEDULER_CTX, RUN_QUEUE, cpu_id, current_pid, arch_set_page_table};
use super::context;
use super::task::TaskState;
use spin::Mutex;

#[derive(Clone, Copy)]
struct FutexWaiter {
    pid:   u32,
    uaddr: usize,
}

const MAX_FUTEX_WAITERS: usize = 256;

static FUTEX_TABLE: Mutex<[Option<FutexWaiter>; MAX_FUTEX_WAITERS]> =
    Mutex::new([const { None }; MAX_FUTEX_WAITERS]);

/// Block the current task on `uaddr` until a `futex_wake` targets it, or
/// (if `deadline` is `Some`) until `ticks() >= deadline`.
///
/// `expected` is validated against `*uaddr` under the `FUTEX_TABLE` lock; if
/// the value already changed, returns `-EAGAIN` (-11) without blocking.
/// The caller must have validated that `uaddr` is a mapped, aligned user
/// address (the read below must not fault: kernel-mode page faults are fatal).
///
/// Returns 0 on wake-up.  Signal delivery also unblocks the task (via
/// `deliver_signal`'s Blocked → Ready transition); in that case the
/// FUTEX_TABLE entry is cleaned up here so no stale waiter remains.
///
/// Timed and untimed waiters take the SAME register-and-block path: a timed
/// waiter registers in `FUTEX_TABLE` (so a cross-thread `FUTEX_WAKE` reaches
/// it, exactly as Linux wakes timed and untimed waiters identically) AND
/// records its wake `deadline` in `Task::poll_deadline`, so the poll-deadline
/// tick (`service_poll_deadlines` → `wake_due_poll_deadlines`) releases it at
/// timeout even if no `FUTEX_WAKE` ever arrives. The waiter truly blocks
/// (no CPU-burning yield-loop). Returns `-ETIMEDOUT` only when the deadline
/// has actually passed on wake-up.
pub fn futex_wait(uaddr: usize, expected: u32, deadline: Option<u64>) -> isize {
    // Timed and untimed waiters take the SAME register-and-block path. A timed
    // waiter records its wake `deadline` in `Task::poll_deadline` so the M7a
    // poll-deadline tick (`service_poll_deadlines` → `wake_due_poll_deadlines`)
    // wakes it at timeout, while `FUTEX_TABLE` registration lets a cross-thread
    // `FUTEX_WAKE` wake it too. The prior timed path yield-looped without
    // registering, so a `FUTEX_WAKE` that didn't also change `*uaddr` was lost
    // (Linux wakes timed and untimed waiters identically) and the waiter burned
    // a CPU spinning — both fixed here.
    unsafe {
        let id  = cpu_id();
        let pid = current_pid();

        // 1. Under the FUTEX_TABLE lock: re-check the value, register the
        //    waiter, and mark the task Blocked.  All three must be one
        //    atomic step relative to futex_wake (see module docs).
        {
            let mut tbl = FUTEX_TABLE.lock();

            let current = core::ptr::read_volatile(uaddr as *const u32);
            if current != expected {
                return -11; // EAGAIN — value changed before we could sleep
            }

            let mut registered = false;
            for slot in tbl.iter_mut() {
                if slot.is_none() {
                    *slot = Some(FutexWaiter { pid, uaddr });
                    registered = true;
                    break;
                }
            }
            if !registered {
                return -11; // table full — let the caller retry
            }

            let mut rq = RUN_QUEUE.lock();

            // Don't park on top of a signal that is already pending.
            //
            // `deliver_signal` only ever performs a `Blocked → Ready` wake. A
            // signal raised while this task was still `Running` therefore set
            // `signal_pending` and woke nothing — correct at the time, because
            // a running task collects its signals on the next return to user
            // space. But if that task then descends straight into an *untimed*
            // futex wait, it parks with the bit still set and nothing will ever
            // wake it: no `futex_wake` is coming (the condition it is waiting
            // on is the signal), and `deliver_signal` has already run. The task
            // sleeps forever, and with it every task waiting on whatever it was
            // supposed to do next.
            //
            // Concretely: tokio's SIGCHLD-driven child reaping. A child that
            // exits in the narrow window after `deliver_signal_process` picked
            // a *running* worker thread but before that worker parks strands
            // the notification permanently — the handler never runs, the signal
            // self-pipe is never written, the reactor never learns the child is
            // gone. A short-lived child (`sleep 0`) lands squarely in that
            // window; a long-lived one (`sleep 1`) exits once the runtime has
            // quiesced, when `deliver_signal_process` finds an already-`Blocked`
            // thread and takes the waking path instead.
            //
            // This must be tested here, under the same `RUN_QUEUE` acquisition
            // that sets `Blocked` — `deliver_signal` mutates `signal_pending`
            // under this lock, so checking it before or after reopens exactly
            // the race being closed. Ignored signals must not count (POSIX), so
            // this uses the disposition-aware predicate rather than a bare
            // `pending & !mask`.
            if super::signal::has_deliverable_signal_locked(&rq, pid) {
                drop(rq);
                // Un-register: we are not going to sleep after all.
                for slot in tbl.iter_mut() {
                    if let Some(w) = *slot {
                        if w.pid == pid { *slot = None; break; }
                    }
                }
                // Report a spurious wake rather than -EINTR. Every futex caller
                // re-checks its own condition in a loop and treats a spurious
                // wake as a retry, whereas -EINTR escapes to callers that may
                // not expect it. The retry is bounded: returning to user space
                // runs `check_and_deliver_signals`, which delivers (or discards)
                // the pending signal, so the next `futex_wait` parks normally.
                return 0;
            }

            if let Some(t) = rq.find_pid_mut(pid) {
                t.state         = TaskState::Blocked;
                t.blocked_futex = uaddr;
                // A timed waiter records its wake deadline so the poll-deadline
                // tick releases it at timeout even if no FUTEX_WAKE arrives.
                t.poll_deadline = deadline.unwrap_or(u64::MAX);
            }
        }

        // Publish the deadline hint (lock-free fetch_min) so poll_deadline_tick's
        // fast path knows a timed futex waiter is due — mirrors the poll path.
        if let Some(dl) = deadline {
            super::register_poll_deadline(dl);
        }

        // 2. Yield to scheduler.  A wake racing with this switch only makes
        //    the task Ready again; the scheduler's on_cpu claim keeps other
        //    CPUs from dispatching it until our registers are fully saved.
        let ctx = CURRENT_CTX[id];
        if !ctx.is_null() {
            // Switch back to kernel page table and then to scheduler
            arch_set_page_table(0);
            context::cpu_switch_to(ctx, core::ptr::addr_of!(SCHEDULER_CTX[id]));
        }

        // 3. Woken (by futex_wake or by signal delivery).  Either way, ensure
        //    our FUTEX_TABLE slot is freed and blocked_futex is cleared.
        {
            let mut tbl = FUTEX_TABLE.lock();
            for slot in tbl.iter_mut() {
                if let Some(w) = *slot {
                    if w.pid == pid { *slot = None; break; }
                }
            }
        }
        {
            let mut rq = RUN_QUEUE.lock();
            if let Some(t) = rq.find_pid_mut(pid) {
                t.blocked_futex = 0;
                t.poll_deadline = u64::MAX;
            }
        }
    }

    // A timed waiter whose deadline has passed reports ETIMEDOUT; otherwise it
    // was released by FUTEX_WAKE or signal delivery (both reported as 0, since
    // every futex caller re-checks its own condition on wake).
    if let Some(dl) = deadline {
        if super::ticks() >= dl { return -110; } // ETIMEDOUT
    }
    0
}

/// Wake up to `n` tasks waiting on `uaddr`.  Returns the count woken.
///
/// Pass `n = u32::MAX` to wake all waiters (used by `clear_child_tid`).
pub fn futex_wake(uaddr: usize, n: u32) -> u32 {
    let mut woken = 0u32;

    // Hold FUTEX_TABLE across collection AND wake-up so this serializes
    // completely against futex_wait's register-and-block step.
    {
        let mut tbl = FUTEX_TABLE.lock();
        let mut rq  = RUN_QUEUE.lock();
        let min_vr  = rq.min_vruntime();

        for slot in tbl.iter_mut() {
            if woken >= n { break; }
            let Some(w) = *slot else { continue };
            if w.uaddr != uaddr { continue; }

            if let Some(t) = rq.find_pid_mut(w.pid) {
                if t.state == TaskState::Blocked {
                    t.state         = TaskState::Ready;
                    t.blocked_futex = 0;
                    t.poll_deadline = u64::MAX;
                    t.place(min_vr);
                    woken += 1;
                }
            }
            *slot = None;
        }
    }

    if woken > 0 {
        super::wake_up_an_idle_cpu();
    }
    woken
}

/// Drop any pending wait registration for `pid` without waking it.
///
/// Used when a thread is force-killed while `Blocked` in `futex_wait` (e.g.
/// a sibling reaped by `exit_group`'s group-kill loop, see
/// `kill_next_group_member` in lib.rs) — its `FUTEX_TABLE` slot would
/// otherwise linger forever since it will never reach the wake path itself.
pub fn remove_waiter(pid: u32) {
    let mut tbl = FUTEX_TABLE.lock();
    for slot in tbl.iter_mut() {
        if slot.map(|w| w.pid) == Some(pid) {
            *slot = None;
        }
    }
}

/// Requeue waiters from `uaddr` to `uaddr2`.
///
/// Wakes up to `val` waiters on `uaddr`, and moves up to `requeue_limit` remaining waiters to `uaddr2`.
/// Returns the total number of waiters woken + requeued.
pub fn futex_requeue(uaddr: usize, uaddr2: usize, val: u32, requeue_limit: u32) -> isize {
    let mut tbl = FUTEX_TABLE.lock();
    let mut rq  = RUN_QUEUE.lock();
    let min_vr  = rq.min_vruntime();
    let mut woken = 0u32;
    let mut requeued = 0u32;

    for slot in tbl.iter_mut() {
        let Some(w) = *slot else { continue };
        if w.uaddr != uaddr { continue; }

        if woken < val {
            if let Some(t) = rq.find_pid_mut(w.pid) {
                if t.state == TaskState::Blocked {
                    t.state         = TaskState::Ready;
                    t.blocked_futex = 0;
                    t.poll_deadline = u64::MAX;
                    t.place(min_vr);
                    woken += 1;
                }
            }
            *slot = None;
        } else if requeued < requeue_limit {
            if let Some(t) = rq.find_pid_mut(w.pid) {
                if t.state == TaskState::Blocked {
                    t.blocked_futex = uaddr2;
                    *slot = Some(FutexWaiter { pid: w.pid, uaddr: uaddr2 });
                    requeued += 1;
                }
            }
        }
    }

    if woken > 0 {
        super::wake_up_an_idle_cpu();
    }

    (woken + requeued) as isize
}
