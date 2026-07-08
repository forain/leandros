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
/// `deadline` bypasses the FUTEX_TABLE/Blocked-state path entirely and
/// instead yield-loops with a `ticks()` deadline check, exactly like every
/// other bounded wait in this kernel (`sys_nanosleep`, `sys_epoll_wait`) —
/// there is no scheduler-tick-driven mechanism to wake a genuinely `Blocked`
/// task at a deadline, only `futex_wake`/signal delivery can do that today.
/// This is safe for real callers: `parking_lot`'s futex-based parker (the
/// confirmed source of this codepath — `std::process::Command`/crossterm's
/// bounded mutex waits under musl) always re-checks its own park flag in a
/// loop around `futex_wait` and treats a spurious/value-driven wake
/// identically to an explicit `FUTEX_WAKE`, so polling for the value change
/// instead of registering for an explicit wake is observably equivalent —
/// it just costs up to one tick (~10ms) of extra latency on a real wake,
/// never a correctness difference. A timed waiter that's never explicitly
/// registered also never appears in `futex_wake`'s counted total, but no
/// caller in this tree inspects that count.
pub fn futex_wait(uaddr: usize, expected: u32, deadline: Option<u64>) -> isize {
    if let Some(deadline) = deadline {
        loop {
            let current = unsafe { core::ptr::read_volatile(uaddr as *const u32) };
            if current != expected { return -11; } // EAGAIN — value already differs
            if super::ticks() >= deadline { return -110; } // ETIMEDOUT
            super::irq_window();
            super::yield_now("futex_wait_timed");
        }
    }

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
            if let Some(t) = rq.find_pid_mut(pid) {
                t.state         = TaskState::Blocked;
                t.blocked_futex = uaddr;
            }
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
            }
        }
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
