//! Futex wait/wake implementation.
//!
//! A FUTEX_PRIVATE futex is keyed by (thread group, user virtual address): the
//! threads of a group share one address space, and the same address in another
//! process is a different futex (see `key_matches`).  Shared futexes (across
//! processes) would require a physical-address key and are deferred to a later
//! phase; until then two non-private callers match by address alone.
//!
//! # SMP race-freedom
//!
//! The classic lost-wake-up race is: CPU A reads `*uaddr == val` and decides to
//! sleep; CPU B changes the value and calls `futex_wake` before A has made
//! itself findable; A then blocks until its timeout (or forever).  Linux closes
//! this by comparing the user word *under* the hash-bucket lock.  We cannot:
//! `*uaddr` is user memory, a read of it can take a demand-paging fault, and
//! `handle_page_fault` re-enters the scheduler — the standing rule from
//! `82d0cc3` is that user memory is never touched under a kernel spinlock.
//!
//! So we register first and look second — the same three-phase protocol as
//! `block_on_port_prepare / re-check / block_on_port_commit` in lib.rs, with a
//! per-`uaddr` registry instead of a single wait-channel (a futex wake targets
//! `n` waiters on one key, which `unblock_port`'s wake-everyone cannot express):
//!
//!  1. **prepare** — take `FUTEX_TABLE`, claim a slot for this task, drop it.
//!     From this instant on, every `futex_wake` on `uaddr` can see us.
//!  2. **re-check** — read `*uaddr` with *no* kernel lock held (so a fault is
//!     serviceable) and compare against `expected`.
//!  3. **commit** — take `FUTEX_TABLE` again.  If our slot was claimed in the
//!     window (`FutexWaiter::woken`), a wake was aimed at us: deregister and
//!     return 0, the wake is *not* dropped.  Otherwise, if the value moved,
//!     deregister and return `EAGAIN`.  Otherwise mark the task `Blocked`
//!     under `RUN_QUEUE` (still inside the `FUTEX_TABLE` hold) and yield.
//!
//! The `woken` flag is also what decides the *return value*: a waiter returns 0
//! iff a wake claimed it, and `-ETIMEDOUT` only when no wake did and the
//! deadline has passed.  Deciding that from the clock alone (the previous
//! behaviour) reported `ETIMEDOUT` for a wake that arrived just after the
//! deadline tick flipped the task `Ready` — a delivered-then-discarded wake,
//! exactly the symptom `smpwaketest` counts as `lost`.
//!
//! Consequently `futex_wake` **claims** slots (`woken = true`) rather than
//! freeing them; the waiter frees its own slot on the way out, on every exit
//! path, and `remove_waiter` covers a thread force-killed while parked.
//! Lock order is always `FUTEX_TABLE → RUN_QUEUE`.

use super::{CURRENT_CTX, SCHEDULER_CTX, RUN_QUEUE, cpu_id, current_pid, arch_set_page_table};
use super::context;
use super::task::TaskState;
use spin::Mutex;

#[derive(Clone, Copy)]
struct FutexWaiter {
    pid:   u32,
    uaddr: usize,
    /// Thread group the waiter belongs to, and whether it waited with
    /// FUTEX_PRIVATE_FLAG. Together with `uaddr` they form the futex key; see
    /// [`key_matches`].
    tgid:    u32,
    private: bool,
    /// Set by `futex_wake`/`futex_requeue` when this waiter is the target of a
    /// wake.  The waiter — not the waker — clears the slot, so a wake that
    /// lands while the waiter is between "registered" and "Blocked" is still
    /// recorded instead of hitting an empty table.  A claimed slot is invisible
    /// to further wakes, so one `FUTEX_WAKE(n=1)` never consumes two waiters.
    woken: bool,
}

const MAX_FUTEX_WAITERS: usize = 256;

static FUTEX_TABLE: Mutex<[Option<FutexWaiter>; MAX_FUTEX_WAITERS]> =
    Mutex::new([const { None }; MAX_FUTEX_WAITERS]);

/// Block the current task on `uaddr` until a `futex_wake` targets it, or
/// (if `deadline` is `Some`) until `monotonic_ns() >= deadline`.
///
/// `expected` is compared against `*uaddr` with no kernel lock held, between
/// this task's registration in `FUTEX_TABLE` and its commit to `Blocked` (see
/// the module docs): if the value already changed, returns `-EAGAIN` (-11)
/// without blocking.  `sys_futex` has already validated that `uaddr` is a
/// mapped, aligned user address; a demand-paging fault on the read below is
/// serviceable precisely because no spinlock is held across it.
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
/// (no CPU-burning yield-loop). Returns `-ETIMEDOUT` only when no wake claimed
/// this waiter *and* the deadline has passed.
pub fn futex_wait(uaddr: usize, expected: u32, deadline: Option<u64>) -> isize {
    futex_wait_keyed(uaddr, expected, deadline, false)
}

/// [`futex_wait`] with the caller's FUTEX_PRIVATE_FLAG: a private waiter can
/// only be woken from its own thread group (see [`key_matches`]).
pub fn futex_wait_keyed(uaddr: usize, expected: u32, deadline: Option<u64>, private: bool) -> isize {
    // Before any lock: the slow path of current_tgid takes RUN_QUEUE.
    let tgid = super::current_tgid();
    unsafe {
        let pid = current_pid();

        // ── Phase 1: prepare ────────────────────────────────────────────────
        // Publish the waiter BEFORE looking at user memory.  Any `futex_wake`
        // from here on either finds us Blocked (and wakes us) or finds us
        // still registered (and claims us via `woken`) — it can never find an
        // empty table while we are on our way to sleep.
        let idx = {
            let mut tbl = FUTEX_TABLE.lock();
            let mut found: Option<usize> = None;
            for (i, slot) in tbl.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(FutexWaiter { pid, uaddr, woken: false, tgid, private });
                    found = Some(i);
                    break;
                }
            }
            match found {
                Some(i) => i,
                None    => return -11, // table full — let the caller retry
            }
        };

        // ── Phase 2: re-check, with NO kernel lock held ─────────────────────
        // This is the read that must not happen under a spinlock: the page can
        // be demand-paged/CoW, and `handle_page_fault` takes RUN_QUEUE and the
        // address-space lock (the `82d0cc3` rule).
        let current = core::ptr::read_volatile(uaddr as *const u32);

        // ── Phase 3: commit ────────────────────────────────────────────────
        {
            let mut tbl = FUTEX_TABLE.lock();

            // A wake aimed at us during phase 2 must NOT be dropped, and must
            // NOT be reported as EAGAIN either: the caller asked to be woken,
            // it was woken, so this is a plain 0.  (A slot that vanished
            // entirely means `remove_waiter` ran — we are being torn down.)
            let claimed = match tbl[idx] {
                Some(w) if w.pid == pid => w.woken,
                _                       => true,
            };
            if claimed {
                clear_slot(&mut tbl, idx, pid);
                return 0;
            }

            if current != expected {
                clear_slot(&mut tbl, idx, pid);
                return -11; // EAGAIN — value changed before we could sleep
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
                clear_slot(&mut tbl, idx, pid);
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
                // Leave no stale poll-channel registration behind: a thread
                // that previously parked in epoll_wait and was released by a
                // signal (or by `futex_wake`) still carries
                // `blocked_on == Some(POLL_WAIT_CHANNEL)`, and `unblock_port`
                // — which every net/vfs readiness edge calls via `wake_poll`
                // — would then yank this futex waiter off its key for an
                // entirely unrelated reason.
                t.blocked_on    = None;
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

        // Yield to scheduler.  A wake racing with this switch only makes the
        // task Ready again; the scheduler's on_cpu claim keeps other CPUs from
        // dispatching it until our registers are fully saved.  (`cpu_id()` is
        // stable across this: syscalls run with IRQs masked, so nothing can
        // preempt or migrate us between here and the switch.)
        let id  = cpu_id();
        let ctx = CURRENT_CTX[id];
        if !ctx.is_null() {
            // Switch back to kernel page table and then to scheduler
            arch_set_page_table(0);
            context::cpu_switch_to(ctx, core::ptr::addr_of!(SCHEDULER_CTX[id]));
        }

        // ── Woken: by futex_wake, by the deadline tick, or by signal delivery.
        // Our own slot says which: only a wake sets `woken`.  Free it either
        // way, so no stale waiter remains.
        let was_woken = {
            let mut tbl = FUTEX_TABLE.lock();
            let claimed = match tbl[idx] {
                Some(w) if w.pid == pid => w.woken,
                _                       => true, // reclaimed by remove_waiter
            };
            clear_slot(&mut tbl, idx, pid);
            claimed
        };
        {
            let mut rq = RUN_QUEUE.lock();
            if let Some(t) = rq.find_pid_mut(pid) {
                t.blocked_futex = 0;
                t.poll_deadline = u64::MAX;
            }
        }

        // A wake was delivered to us — never report it as a timeout, however
        // late this task was rescheduled.
        if was_woken {
            return 0;
        }
    }

    // No wake claimed this waiter: a timed one whose deadline has passed
    // reports ETIMEDOUT, anything else is a spurious wake (signal delivery, a
    // stray poll wake) and returns 0, since every futex caller re-checks its
    // own condition on wake.
    if let Some(dl) = deadline {
        if super::monotonic_ns() >= dl { return -110; } // ETIMEDOUT
    }
    0
}

/// Does waiter `w` belong to the futex a (`tgid`, `private`) caller names by
/// `uaddr`?
///
/// A virtual address alone names a futex only within ONE address space. The
/// table used to be keyed by `uaddr` alone, so every process's waiters on the
/// same virtual address shared one queue — and processes started from the
/// same binary lay out their stacks, heaps and statics at the same addresses.
/// A `FUTEX_WAKE(n=1)` from one process could then claim a waiter of
/// another: that one woke spuriously (harmless — it re-checks and waits
/// again), and the waker's own waiter slept on with its wake consumed. The
/// five cosmic-panel-button instances cosmic-panel spawns together hit it at
/// startup (2026-09-25): libwayland-client's read_events parks non-last
/// readers in pthread_cond_wait, and musl's condvar waits on a barrier word
/// in the waiter's stack frame, at the same address in every instance. One
/// instance's signal woke another's waiter, its own reader slept forever
/// with the word already 0, and its panel or dock button never appeared.
/// Musl's static locks in libc's .data collide the same way, dozens of times
/// per session start.
///
/// Linux keys a private futex by (mm, address). Here the thread group stands
/// in for the mm: CLONE_THREAD siblings share both. A non-private (shared)
/// futex keeps matching by address across processes when BOTH sides are
/// non-private: there is no inode/offset key yet, and the kernel's own
/// futex-backed mutexes (kernel addresses, `mutex.rs`) and the exit path's
/// clear_child_tid wake rely on that.
#[inline]
fn key_matches(w: &FutexWaiter, uaddr: usize, tgid: u32, private: bool) -> bool {
    if w.uaddr != uaddr { return false; }
    w.tgid == tgid || (!w.private && !private)
}

/// Release slot `idx` if it is still this task's registration.
///
/// Guarded by pid because a slot freed by `remove_waiter` may already have been
/// handed to a different waiter.
#[inline]
fn clear_slot(tbl: &mut [Option<FutexWaiter>; MAX_FUTEX_WAITERS], idx: usize, pid: u32) {
    if matches!(tbl[idx], Some(w) if w.pid == pid) {
        tbl[idx] = None;
    }
}

/// Wake up to `n` tasks waiting on `uaddr`.  Returns the count woken.
///
/// Pass `n = u32::MAX` to wake all waiters (used by `clear_child_tid`).
///
/// A waiter is *claimed* (`FutexWaiter::woken = true`) rather than removed: the
/// waiter owns its slot and frees it on the way out.  Claiming counts as a wake
/// even when the target is not (or no longer) `Blocked` — it may be mid-flight
/// between registering and parking, or already `Ready` because the deadline
/// tick or a signal released it a moment ago.  In both cases the waiter is
/// about to consult its own slot and will return 0, so the wake is delivered
/// exactly once and never silently dropped.
pub fn futex_wake(uaddr: usize, n: u32) -> u32 {
    futex_wake_keyed(uaddr, n, false)
}

/// [`futex_wake`] with the caller's FUTEX_PRIVATE_FLAG (see [`key_matches`]).
pub fn futex_wake_keyed(uaddr: usize, n: u32, private: bool) -> u32 {
    let tgid = super::current_tgid();
    let mut woken   = 0u32;
    let mut resched = false;

    // Hold FUTEX_TABLE across collection AND wake-up so this serializes
    // completely against futex_wait's register / commit steps.
    {
        let mut tbl = FUTEX_TABLE.lock();
        let mut rq  = RUN_QUEUE.lock();
        let min_vr  = rq.min_vruntime();

        for i in 0..MAX_FUTEX_WAITERS {
            if woken >= n { break; }
            let Some(w) = tbl[i] else { continue };
            if w.woken || !key_matches(&w, uaddr, tgid, private) { continue; }

            // The task is gone (killed between registering and being reaped):
            // drop the stale slot without spending a wake on it.
            let Some(t) = rq.find_pid_mut(w.pid) else {
                tbl[i] = None;
                continue;
            };
            if t.state == TaskState::Blocked {
                t.state         = TaskState::Ready;
                t.blocked_futex = 0;
                t.poll_deadline = u64::MAX;
                t.place(min_vr);
                resched = true;
            }
            if let Some(slot) = tbl[i].as_mut() { slot.woken = true; }
            woken += 1;
        }
    }

    if resched {
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
    futex_requeue_keyed(uaddr, uaddr2, val, requeue_limit, false)
}

/// [`futex_requeue`] with the caller's FUTEX_PRIVATE_FLAG (see [`key_matches`]).
pub fn futex_requeue_keyed(uaddr: usize, uaddr2: usize, val: u32, requeue_limit: u32, private: bool) -> isize {
    let tgid = super::current_tgid();
    let mut woken    = 0u32;
    let mut requeued = 0u32;
    let mut resched  = false;

    {
        let mut tbl = FUTEX_TABLE.lock();
        let mut rq  = RUN_QUEUE.lock();
        let min_vr  = rq.min_vruntime();

        for i in 0..MAX_FUTEX_WAITERS {
            let Some(w) = tbl[i] else { continue };
            // An already-claimed slot belongs to a waiter that is on its way
            // out; it is neither wakeable nor requeueable a second time.
            if w.woken || !key_matches(&w, uaddr, tgid, private) { continue; }

            if woken < val {
                let Some(t) = rq.find_pid_mut(w.pid) else {
                    tbl[i] = None;
                    continue;
                };
                if t.state == TaskState::Blocked {
                    t.state         = TaskState::Ready;
                    t.blocked_futex = 0;
                    t.poll_deadline = u64::MAX;
                    t.place(min_vr);
                    resched = true;
                }
                if let Some(slot) = tbl[i].as_mut() { slot.woken = true; }
                woken += 1;
            } else if requeued < requeue_limit {
                // Only a waiter that has actually parked can be moved: one
                // still in `futex_wait`'s re-check window is about to compare
                // `*uaddr` against its own `expected`, and re-keying it behind
                // its back would make that comparison meaningless. Leaving it
                // on `uaddr` is always safe — it parks there and a later wake
                // on `uaddr` releases it.
                if let Some(t) = rq.find_pid_mut(w.pid) {
                    if t.state == TaskState::Blocked {
                        t.blocked_futex = uaddr2;
                        if let Some(slot) = tbl[i].as_mut() { slot.uaddr = uaddr2; }
                        requeued += 1;
                    }
                }
            }
        }
    }

    if resched {
        super::wake_up_an_idle_cpu();
    }

    (woken + requeued) as isize
}
