//! Futex wait/wake implementation.
//!
//! Futex keys are user-space virtual addresses.  Since all threads in a thread
//! group share the same virtual address space, keying on VA is sufficient for
//! FUTEX_PRIVATE (process-private) futexes.  Shared futexes (across processes)
//! would require a physical-address key and are deferred to a later phase.
//!
//! Race-freedom: the scheduler is cooperative; no preemption occurs between
//! the caller's value-check (in sys_futex) and the context switch here, so
//! FUTEX_WAIT is race-free without additional locking.

use super::{CURRENT_PID, CURRENT_CTX, SCHEDULER_CTX, RUN_QUEUE, cpu_id, arch_set_page_table};
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
/// Returns 0 on normal wake-up.  Signal delivery will also unblock the task
/// (via `deliver_signal`'s Blocked → Ready transition); in that case the
/// FUTEX_TABLE entry is cleaned up here so no stale waiter remains.
pub fn futex_wait(uaddr: usize, _timeout_ptr: usize) -> isize {
    unsafe {
        let id  = cpu_id();
        let pid = CURRENT_PID[id];

        // 1. Register in table BEFORE marking Blocked so futex_wake can find us.
        {
            let mut tbl = FUTEX_TABLE.lock();
            for slot in tbl.iter_mut() {
                if slot.is_none() {
                    *slot = Some(FutexWaiter { pid, uaddr });
                    break;
                }
            }
        }

        // 2. Mark Blocked.
        {
            let mut rq = RUN_QUEUE.lock();
            if let Some(idx) = rq.find_pid_idx(pid) {
                if let Some(t) = rq.get_mut(idx) {
                    t.state         = TaskState::Blocked;
                    t.blocked_futex = uaddr;
                }
            }
        }

        // 3. Yield to scheduler.
        let ctx = CURRENT_CTX[id];
        if !ctx.is_null() {
            // Switch back to kernel page table and then to scheduler
            arch_set_page_table(0);
            context::cpu_switch_to(ctx, core::ptr::addr_of!(SCHEDULER_CTX[id]));
        }

        // 4. Woken (by futex_wake or by signal delivery).  Either way, ensure
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
            if let Some(idx) = rq.find_pid_idx(pid) {
                if let Some(t) = rq.get_mut(idx) {
                    t.blocked_futex = 0;
                }
            }
        }
    }

    0
}

/// Wake up to `n` tasks waiting on `uaddr`.  Returns the count woken.
///
/// Pass `n = u32::MAX` to wake all waiters (used by `clear_child_tid`).
pub fn futex_wake(uaddr: usize, n: u32) -> u32 {
    // Collect PIDs under FUTEX_TABLE lock, then wake under RUN_QUEUE lock.
    let mut pids  = [0u32; 32];
    let mut count = 0usize;

    {
        let mut tbl = FUTEX_TABLE.lock();
        for slot in tbl.iter_mut() {
            if count as u32 >= n { break; }
            if let Some(w) = *slot {
                if w.uaddr == uaddr && count < pids.len() {
                    pids[count] = w.pid;
                    count += 1;
                    *slot = None;
                }
            }
        }
    }

    let mut woken = 0u32;
    {
        let mut rq = RUN_QUEUE.lock();
        for &pid in &pids[..count] {
            if let Some(idx) = rq.find_pid_idx(pid) {
                if let Some(t) = rq.get_mut(idx) {
                    if t.state == TaskState::Blocked {
                        t.state         = TaskState::Ready;
                        t.blocked_futex = 0;
                        woken += 1;
                    }
                }
            }
        }
    }

    woken
}
