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

/// Block the current task on `uaddr` until a `futex_wake` targets it.
///
/// `expected` is validated against `*uaddr` under the `FUTEX_TABLE` lock; if
/// the value already changed, returns `-EAGAIN` (-11) without blocking.
/// The caller must have validated that `uaddr` is a mapped, aligned user
/// address (the read below must not fault: kernel-mode page faults are fatal).
///
/// Returns 0 on wake-up.  Signal delivery also unblocks the task (via
/// `deliver_signal`'s Blocked → Ready transition); in that case the
/// FUTEX_TABLE entry is cleaned up here so no stale waiter remains.
pub fn futex_wait(uaddr: usize, expected: u32, _timeout_ptr: usize) -> isize {
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
