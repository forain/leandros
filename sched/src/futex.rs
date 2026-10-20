//! Futex wait/wake implementation.
//!
//! Every futex is named by a [`FutexKey`], computed once per call by
//! [`futex_key`] the way Linux's `get_futex_key` does: a FUTEX_PRIVATE
//! futex, or a shared one in a private mapping, is (thread group, user
//! virtual address) — the threads of a group share one address space, and
//! the same address in another process is a different futex. A shared futex
//! in a `MAP_SHARED` mapping is (backing object, offset): the shared
//! anonymous object, the page-cache file, or (memfd/tmpfs frames) the
//! physical byte — so two processes, or two mappings of one object at
//! different addresses, meet on the same futex. Kernel addresses (the
//! kernel's own futex-backed mutexes, `mutex.rs`) are global.
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

/// The identity of a futex (see the module docs and [`futex_key`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FutexKey {
    /// 0: (thread group `a`, address `b`); 1..=3: shared memory, see
    /// `mm::vmm::AddressSpace::futex_backing`; 4: kernel address `b`.
    kind: u8,
    a:    u64,
    b:    u64,
}

const KEY_MM: u8 = 0;
const KEY_KERNEL: u8 = 4;

impl FutexKey {
    fn kernel(addr: usize) -> Self { FutexKey { kind: KEY_KERNEL, a: 0, b: addr as u64 } }
    fn mm(tgid: u32, addr: usize) -> Self { FutexKey { kind: KEY_MM, a: tgid as u64, b: addr as u64 } }
}

/// The key of the futex at `uaddr` for the calling task, with its
/// FUTEX_PRIVATE_FLAG. A shared futex looks its mapping up (Linux:
/// `get_futex_key`); a physically keyed page not yet present is touched —
/// with no lock held, so the fault is serviced — and looked up again.
/// `Err(-EFAULT)` when no mapping covers `uaddr`.
pub fn futex_key(uaddr: usize, private: bool) -> Result<FutexKey, isize> {
    use mm::vmm::FutexBacking;
    if uaddr >= crate::uaccess::USER_END { return Ok(FutexKey::kernel(uaddr)); }
    // Before any lock: the slow path of current_tgid takes RUN_QUEUE.
    let tgid = super::current_tgid();
    if private { return Ok(FutexKey::mm(tgid, uaddr)); }
    for _ in 0..4 {
        match super::with_current_address_space(|a| a.futex_backing(uaddr)) {
            // A kernel task has no address space: nothing but itself can
            // name the address.
            None => return Ok(FutexKey::kernel(uaddr)),
            Some(FutexBacking::Unmapped) => return Err(-14),
            Some(FutexBacking::Private) => return Ok(FutexKey::mm(tgid, uaddr)),
            Some(FutexBacking::Shared(kind, a, b)) => return Ok(FutexKey { kind, a, b }),
            Some(FutexBacking::NotPresent) => {
                if crate::uaccess::read_user::<u32>(uaddr).is_none() { return Err(-14); }
            }
        }
    }
    Err(-14)
}

#[derive(Clone, Copy)]
struct FutexWaiter {
    pid:   u32,
    /// The user address waited on (diagnostics; the key is what matches).
    uaddr: usize,
    key:   FutexKey,
    /// Waiting in FUTEX_LOCK_PI: FUTEX_UNLOCK_PI hands the lock to the first
    /// such waiter instead of releasing it.
    pi:    bool,
    /// PI hand-off state of a claimed waiter: 0 none, 1 the unlocker is
    /// writing this waiter's TID into the lock word (the waiter must stay
    /// parked), 2 done — the waiter owns the lock. See [`futex_unlock_pi`].
    handoff: u8,
    /// Set by `futex_wake`/`futex_requeue` when this waiter is the target of a
    /// wake.  The waiter — not the waker — clears the slot, so a wake that
    /// lands while the waiter is between "registered" and "Blocked" is still
    /// recorded instead of hitting an empty table.  A claimed slot is invisible
    /// to further wakes, so one `FUTEX_WAKE(n=1)` never consumes two waiters.
    woken: bool,
}

// A task waits on at most one futex at a time, so one slot per task means the
// table can never be full (it answered EAGAIN when full, which user space
// reads as "value changed" and retries: a silent spin). 256 -> MAX_TASKS
// (512) with the task-limit raise; lane procpool 2026-09-27.
const MAX_FUTEX_WAITERS: usize = crate::runqueue::MAX_TASKS;
const NIL: u16 = u16::MAX;
const _: () = assert!(MAX_FUTEX_WAITERS < NIL as usize && MAX_FUTEX_WAITERS % 64 == 0);
/// Hash buckets, keyed by the futex key.
const FUTEX_BUCKETS: usize = 256;

/// The waiter slots plus a FIFO chain per address bucket.
///
/// `FUTEX_WAKE` and requeue used to walk every slot of the table (32 B each,
/// 1024 of them: 32 KiB touched per wake, under the lock that every futex
/// caller in the system takes). They now walk one bucket's chain: the waiters
/// whose address hashes there, in arrival order — so a `FUTEX_WAKE(n=1)` also
/// releases the longest waiter first, as Linux's hash-bucket queue does. A
/// slot's index never changes while its waiter holds it (`futex_wait_intr`
/// keeps the index across the park); the chains are doubly linked so freeing
/// and re-keying a slot are O(1).
struct FutexTable {
    slot: [Option<FutexWaiter>; MAX_FUTEX_WAITERS],
    next: [u16; MAX_FUTEX_WAITERS],
    prev: [u16; MAX_FUTEX_WAITERS],
    head: [u16; FUTEX_BUCKETS],
    tail: [u16; FUTEX_BUCKETS],
    /// Bit `i` set ⇔ `slot[i].is_some()`.
    used: [u64; MAX_FUTEX_WAITERS / 64],
}

#[inline]
fn bucket(k: &FutexKey) -> usize {
    let h = (k.b >> 2) ^ k.a.rotate_left(17) ^ k.kind as u64;
    (h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as usize & (FUTEX_BUCKETS - 1)
}

impl FutexTable {
    const fn new() -> Self {
        Self {
            slot: [const { None }; MAX_FUTEX_WAITERS],
            next: [NIL; MAX_FUTEX_WAITERS],
            prev: [NIL; MAX_FUTEX_WAITERS],
            head: [NIL; FUTEX_BUCKETS],
            tail: [NIL; FUTEX_BUCKETS],
            used: [0; MAX_FUTEX_WAITERS / 64],
        }
    }

    fn link_tail(&mut self, i: usize) {
        let b = bucket(&self.slot[i].map_or(FutexKey::kernel(0), |w| w.key));
        let t = self.tail[b];
        self.prev[i] = t;
        self.next[i] = NIL;
        if t == NIL { self.head[b] = i as u16; } else { self.next[t as usize] = i as u16; }
        self.tail[b] = i as u16;
    }

    fn unlink(&mut self, i: usize) {
        let b = bucket(&self.slot[i].map_or(FutexKey::kernel(0), |w| w.key));
        let (p, n) = (self.prev[i], self.next[i]);
        if p == NIL { self.head[b] = n; } else { self.next[p as usize] = n; }
        if n == NIL { self.tail[b] = p; } else { self.prev[n as usize] = p; }
        self.prev[i] = NIL;
        self.next[i] = NIL;
    }

    /// Register `w` at the tail of its bucket; `None` if every slot is taken.
    fn insert(&mut self, w: FutexWaiter) -> Option<usize> {
        let mut found = None;
        for (k, word) in self.used.iter().enumerate() {
            if *word != u64::MAX { found = Some(k * 64 + (!*word).trailing_zeros() as usize); break; }
        }
        let i = found?;
        self.used[i / 64] |= 1u64 << (i % 64);
        self.slot[i] = Some(w);
        self.link_tail(i);
        Some(i)
    }

    /// Free slot `i` (no-op if already free).
    fn free(&mut self, i: usize) {
        if self.slot[i].is_none() { return; }
        self.unlink(i);
        self.slot[i] = None;
        self.used[i / 64] &= !(1u64 << (i % 64));
    }

    /// Move waiter `i` to `key2`'s chain (at its tail).
    fn rekey(&mut self, i: usize, key2: FutexKey, uaddr2: usize) {
        self.unlink(i);
        if let Some(w) = self.slot[i].as_mut() { w.key = key2; w.uaddr = uaddr2; }
        self.link_tail(i);
    }
}

static FUTEX_TABLE: Mutex<FutexTable> = Mutex::new(FutexTable::new());

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
/// only be woken from its own thread group (see [`key_matches`]). A release by
/// signal is reported as a spurious wake (0); callers that restart or fail
/// with EINTR use [`futex_wait_intr`].
pub fn futex_wait_keyed(uaddr: usize, expected: u32, deadline: Option<u64>, private: bool) -> isize {
    match futex_wait_intr(uaddr, expected, deadline, private) {
        FUTEX_INTERRUPTED => 0,
        r => r,
    }
}

/// [`futex_wait_intr`]'s "released by a deliverable signal, no wake claimed
/// us" result. Kernel-internal, never seen by user space: `sys_futex` turns
/// it into -ERESTARTSYS / -ERESTARTNOHAND (the numeric value is Linux's
/// ERESTARTSYS only for recognisability in traces).
pub const FUTEX_INTERRUPTED: isize = -512;

/// [`futex_wait_keyed`] with one authoritative wake reason:
///
/// * `0` — a `futex_wake`/requeue claimed this waiter (always wins, even when
///   a signal is also pending: the wake was counted by the waker, so it must
///   be consumed here, exactly as Linux returns 0 when `unqueue_me` finds the
///   waiter already dequeued), or a stray release with nothing pending;
/// * `-110` — no wake claimed it and the deadline has passed (checked before
///   signals, as Linux does);
/// * [`FUTEX_INTERRUPTED`] — no wake claimed it and a signal is deliverable.
///
/// The claim and the signal test are read in the same `FUTEX_TABLE` hold
/// that frees the slot, so after the verdict no wake can land on this waiter
/// any more: "woken" and "interrupted" can never both be true.
pub fn futex_wait_intr(uaddr: usize, expected: u32, deadline: Option<u64>, private: bool) -> isize {
    match futex_key(uaddr, private) {
        Ok(key) => wait_key(key, uaddr, expected, deadline, None, false),
        Err(e)  => e,
    }
}

/// [`futex_wait_intr`] with an absolute CLOCK_REALTIME deadline `rt_ns`
/// (FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME): the deadline is translated
/// onto the monotonic clock on every pass, and the parked waiter is
/// registered as a wall-clock waiter (`Task::rt_wait`), so a clock step
/// moves it (`crate::realtime_waiters_clock_was_set`) instead of leaving it
/// to expire at the pre-step reading.
pub fn futex_wait_intr_realtime(uaddr: usize, expected: u32, rt_ns: u64, private: bool) -> isize {
    match futex_key(uaddr, private) {
        Ok(key) => wait_key(key, uaddr, expected, None, Some(rt_ns), false),
        Err(e)  => e,
    }
}

/// [`futex_wait_intr`]'s result for a FUTEX_LOCK_PI waiter that
/// FUTEX_UNLOCK_PI handed the lock to: the lock word already names it.
pub const FUTEX_HANDOFF: isize = 1;

/// [`futex_wait_intr`] for FUTEX_LOCK_PI: the waiter is a candidate for
/// [`futex_unlock_pi`]'s hand-off, and returns [`FUTEX_HANDOFF`] when it got
/// the lock that way.
pub fn futex_wait_pi(uaddr: usize, expected: u32, deadline: Option<u64>, private: bool) -> isize {
    match futex_key(uaddr, private) {
        Ok(key) => wait_key(key, uaddr, expected, deadline, None, true),
        Err(e)  => e,
    }
}

fn wait_key(key: FutexKey, uaddr: usize, expected: u32, mono_deadline: Option<u64>, rt: Option<u64>,
            pi: bool) -> isize {
    // Each pass is one register / re-check / park round. A release that is
    // neither a claimed wake, nor the deadline, nor a deliverable signal (an
    // ignored signal's Blocked -> Ready, a job-control continue) goes round
    // again instead of returning a spurious 0 — Linux's `goto retry` in
    // futex_wait. The value re-check on the next pass still ends it with
    // EAGAIN if the word moved meanwhile.
    loop { unsafe {
        let pid = current_pid();
        // A wall-clock deadline is re-read every pass (a clock step may
        // have moved it).
        let deadline = match rt {
            Some(r) => Some(super::realtime_to_monotonic_ns(r).max(1)),
            None => mono_deadline,
        };

        // ── Phase 1: prepare ────────────────────────────────────────────────
        // Publish the waiter BEFORE looking at user memory.  Any `futex_wake`
        // from here on either finds us Blocked (and wakes us) or finds us
        // still registered (and claims us via `woken`) — it can never find an
        // empty table while we are on our way to sleep.
        let idx = {
            let mut tbl = FUTEX_TABLE.lock();
            match tbl.insert(FutexWaiter { pid, uaddr, key, pi, handoff: 0, woken: false }) {
                Some(i) => i,
                None    => return -11, // table full — let the caller retry
            }
        };

        // ── Phase 2: re-check, with NO kernel lock held ─────────────────────
        // This is the read that must not happen under a spinlock: the page can
        // be demand-paged/CoW, and `handle_page_fault` takes RUN_QUEUE and the
        // address-space lock (the `82d0cc3` rule).
        let current = crate::uaccess::load_user_u32(uaddr);

        // ── Phase 3: commit ────────────────────────────────────────────────
        let handing = {
            let mut tbl = FUTEX_TABLE.lock();

            // A wake aimed at us during phase 2 must NOT be dropped, and must
            // NOT be reported as EAGAIN either: the caller asked to be woken,
            // it was woken, so this is a plain 0.  (A slot that vanished
            // entirely means `remove_waiter` ran — we are being torn down.)
            let (claimed, ho) = match tbl.slot[idx] {
                Some(w) if w.pid == pid => (w.woken, w.handoff),
                _                       => (true, 0),
            };
            // A PI hand-off in progress (`handoff == 1`): the unlocker is
            // writing our TID into the lock word and will release us when
            // it is there. Park for it whatever the value, the deadline or
            // a signal say — we own the lock already.
            let handing = claimed && ho == 1;
            if claimed && !handing {
                clear_slot(&mut tbl, idx, pid);
                return if ho == 2 { FUTEX_HANDOFF } else { 0 };
            }

            if !handing && current != Some(expected) {
                clear_slot(&mut tbl, idx, pid);
                // EFAULT for an unreadable word, else EAGAIN — value
                // changed before we could sleep.
                return if current.is_none() { -14 } else { -11 };
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
            if !handing && super::signal::has_deliverable_signal_locked(&rq, pid) {
                drop(rq);
                // Un-register: we are not going to sleep after all.
                clear_slot(&mut tbl, idx, pid);
                // Interrupted before parking. `futex_wait_keyed` reports this
                // as a spurious wake (every futex caller re-checks its own
                // condition), `sys_futex` as a restartable interruption. Either
                // way the retry is bounded: returning to user space runs
                // `check_and_deliver_signals`, which delivers the signal, so
                // the next wait parks normally.
                return FUTEX_INTERRUPTED;
            }

            if let Some(t) = rq.find_pid_mut(pid) {
                t.set_state(TaskState::Blocked);
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
                t.poll_deadline = if handing { u64::MAX } else { deadline.unwrap_or(u64::MAX) };
                t.rt_wait = if handing { 0 } else { rt.map_or(0, |r| r.max(1)) };
            }
            if deadline.is_some() && !handing { rq.note_timed_wait(pid); }
            handing
        };

        // Publish the deadline hint (lock-free fetch_min) so poll_deadline_tick's
        // fast path knows a timed futex waiter is due — mirrors the poll path.
        if let Some(dl) = deadline {
            if !handing { super::register_poll_deadline(dl); }
        }

        let (was_woken, ho, signalled) = loop {

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
        // Our own slot says which: only a wake sets `woken`. Free it and take
        // the signal verdict in the same FUTEX_TABLE hold (lock order
        // FUTEX_TABLE -> RUN_QUEUE), so a wake can no longer claim this
        // waiter once it has been judged "interrupted": a signal-woken waiter
        // that a FUTEX_WAKE claimed in the gap before it ran returns 0, and
        // one that no wake claimed is out of the table before it restarts.
        {
            let mut tbl = FUTEX_TABLE.lock();
            let (claimed, ho) = match tbl.slot[idx] {
                Some(w) if w.pid == pid => (w.woken, w.handoff),
                _                       => (true, 0), // reclaimed by remove_waiter
            };
            let mut rq = RUN_QUEUE.lock();
            if claimed && ho == 1 {
                // Released (deadline tick, signal) while a PI hand-off to us
                // is still being written: park again until the unlocker
                // commits it.
                if let Some(t) = rq.find_pid_mut(pid) {
                    t.set_state(TaskState::Blocked);
                    t.blocked_futex = uaddr;
                    t.blocked_on    = None;
                    t.poll_deadline = u64::MAX;
                }
                drop(rq);
                drop(tbl);
                continue; // back to the switch at the top of this loop
            }
            clear_slot(&mut tbl, idx, pid);
            if let Some(t) = rq.find_pid_mut(pid) {
                t.blocked_futex = 0;
                t.poll_deadline = u64::MAX;
                t.rt_wait = 0;
            }
            let signalled = !claimed && super::signal::has_deliverable_signal_locked(&rq, pid);
            break (claimed, ho, signalled);
        }
        };

        // A wake was delivered to us — never report it as a timeout or an
        // interruption, however late this task was rescheduled.
        if was_woken {
            return if ho == 2 { FUTEX_HANDOFF } else { 0 };
        }

        // No wake claimed this waiter: a timed one whose deadline has passed
        // reports ETIMEDOUT, one released with a deliverable signal is
        // interrupted, anything else waits again (see the loop comment).
        // The deadline as it stands now: a clock step during the park moved
        // a wall-clock one.
        let deadline = match rt {
            Some(r) => Some(super::realtime_to_monotonic_ns(r).max(1)),
            None => deadline,
        };
        if let Some(dl) = deadline {
            if super::monotonic_ns() >= dl { return -110; } // ETIMEDOUT
        }
        if signalled { return FUTEX_INTERRUPTED; }
    } }
}

/// Release slot `idx` if it is still this task's registration.
///
/// Guarded by pid because a slot freed by `remove_waiter` may already have been
/// handed to a different waiter.
#[inline]
fn clear_slot(tbl: &mut FutexTable, idx: usize, pid: u32) {
    if matches!(tbl.slot[idx], Some(w) if w.pid == pid) {
        tbl.free(idx);
    }
}

/// Wake up to `n` tasks waiting on `uaddr` (a non-private futex, or a kernel
/// address).  Returns the count woken.
///
/// Pass `n = u32::MAX` to wake all waiters.
///
/// A waiter is *claimed* (`FutexWaiter::woken = true`) rather than removed: the
/// waiter owns its slot and frees it on the way out.  Claiming counts as a wake
/// even when the target is not (or no longer) `Blocked` — it may be mid-flight
/// between registering and parking, or already `Ready` because the deadline
/// tick or a signal released it a moment ago.  In both cases the waiter is
/// about to consult its own slot and will return 0, so the wake is delivered
/// exactly once and never silently dropped.
pub fn futex_wake(uaddr: usize, n: u32) -> u32 {
    futex_wake_keyed(uaddr, n, false).max(0) as u32
}

/// [`futex_wake`] with the caller's FUTEX_PRIVATE_FLAG: the count woken, or
/// -EFAULT when no mapping covers `uaddr`.
pub fn futex_wake_keyed(uaddr: usize, n: u32, private: bool) -> isize {
    match futex_key(uaddr, private) {
        Ok(key) => wake_key(key, n) as isize,
        Err(e)  => e,
    }
}

fn wake_key(key: FutexKey, n: u32) -> u32 {
    let mut woken   = 0u32;
    let mut resched = false;

    // Hold FUTEX_TABLE across collection AND wake-up so this serializes
    // completely against futex_wait's register / commit steps.
    {
        let mut tbl = FUTEX_TABLE.lock();
        let mut rq  = RUN_QUEUE.lock();

        let mut i = tbl.head[bucket(&key)];
        while i != NIL {
            if woken >= n { break; }
            let cur = i as usize;
            i = tbl.next[cur];
            let Some(w) = tbl.slot[cur] else { continue };
            if w.woken || w.key != key { continue; }

            // The task is gone (killed between registering and being reaped):
            // drop the stale slot without spending a wake on it.
            let Some(t) = rq.find_pid_mut(w.pid) else {
                tbl.free(cur);
                continue;
            };
            if t.state == TaskState::Blocked {
                t.set_state(TaskState::Ready);
                t.blocked_futex = 0;
                t.poll_deadline = u64::MAX;
                resched = true;
            }
            if let Some(slot) = tbl.slot[cur].as_mut() { slot.woken = true; }
            woken += 1;
        }
        // Phase 2 (lane percpu): queue the wakee(s) now. A single wakee of a
        // user thread's FUTEX_WAKE goes on the waker's CPU (sync wake: the
        // waker usually blocks next and this CPU then picks it; if not, the
        // deferred kick at its syscall return lets an idle CPU steal it).
        let me = unsafe { super::cpu_id() };
        if resched && woken == 1 && super::current_is_user_task(me) {
            rq.flush_woken_sync(Some(me));
        } else {
            rq.flush_woken();
        }
    }

    if resched {
        // One waiter made Ready by a user thread's FUTEX_WAKE: the classic
        // hand-off (mutex unlock, condvar signal) whose waker often blocks
        // next — defer the idle-CPU kick (lane percpu sync wake).
        if woken == 1 { super::wake_up_an_idle_cpu_sync(); } else { super::wake_up_an_idle_cpu(); }
    }
    woken
}

/// Is any task other than `except` parked (or about to park, unclaimed) on
/// the futex `uaddr` names for a (current task, `private`) caller?
///
/// FUTEX_LOCK_PI asks this when it takes a free PI lock word: if other lockers
/// are still queued it must set FUTEX_WAITERS in the word, so the new owner's
/// unlock comes back to the kernel (FUTEX_UNLOCK_PI) and wakes one of them
/// instead of releasing the word in user space past their heads.
pub fn futex_has_waiters(uaddr: usize, private: bool, except: u32) -> bool {
    let Ok(key) = futex_key(uaddr, private) else { return false };
    let tbl = FUTEX_TABLE.lock();
    let mut i = tbl.head[bucket(&key)];
    while i != NIL {
        if let Some(w) = &tbl.slot[i as usize] {
            if !w.woken && w.pid != except && w.key == key { return true; }
        }
        i = tbl.next[i as usize];
    }
    false
}

/// FUTEX_UNLOCK_PI with ownership hand-off (Linux `wake_futex_pi`): the
/// caller (TID `tid`) must own the lock word at `uaddr` (else -EPERM). If a
/// FUTEX_LOCK_PI waiter is queued, the first one becomes the owner — the word
/// is set to its TID | FUTEX_WAITERS before it runs, so no user-space cas
/// can take the lock past it — and its LOCK_PI returns 0 with the lock held.
/// With none queued the word is released to 0 (and any non-PI waiter on it
/// is woken, as before).
///
/// User memory is never touched under `FUTEX_TABLE`, so the hand-off takes
/// three steps: claim the waiter (`handoff = 1`: it may not leave, and parks
/// again if a deadline or signal releases it), write the word with no lock
/// held, then commit (`handoff = 2`) and make it runnable. A claimed waiter
/// killed before the commit (its slot vanishes) has the lock handed on to
/// the next one, or released: only the owner ever rewrites the TID bits, so
/// the word can still be rewritten safely.
pub fn futex_unlock_pi(uaddr: usize, private: bool, tid: u32) -> isize {
    const WAITERS: u32 = 0x8000_0000;
    const TID_MASK: u32 = 0x3fff_ffff;
    let key = match futex_key(uaddr, private) { Ok(k) => k, Err(e) => return e };
    // The word is only touched through the fault-tolerant uaccess helpers,
    // with no kernel lock held.
    match crate::uaccess::load_user_u32(uaddr) {
        None => return -14,
        Some(w) if w & TID_MASK != tid => return -1, // EPERM
        Some(_) => {}
    }
    loop {
        // Step 1: claim the first unclaimed PI waiter.
        let pick = {
            let mut tbl = FUTEX_TABLE.lock();
            let mut found = None;
            let mut i = tbl.head[bucket(&key)];
            while i != NIL {
                let cur = i as usize;
                i = tbl.next[cur];
                if let Some(w) = tbl.slot[cur].as_mut() {
                    if w.pi && !w.woken && w.key == key {
                        w.woken = true;
                        w.handoff = 1;
                        found = Some((cur, w.pid));
                        break;
                    }
                }
            }
            found
        };
        // Step 2: the word names the new owner (or nobody). Only the owner
        // changes the TID bits; lockers only add FUTEX_WAITERS, so retry the
        // cas until it lands. FUTEX_OWNER_DIED goes with the old ownership.
        let new = match pick { Some((_, p)) => p | WAITERS, None => 0 };
        let mut faulted = false;
        loop {
            let Some(w) = crate::uaccess::load_user_u32(uaddr) else { faulted = true; break };
            match crate::uaccess::cmpxchg_user_u32(uaddr, w, new) {
                Some(seen) if seen == w => break,
                Some(_) => continue,
                None => { faulted = true; break }
            }
        }
        let _ = faulted; // the hand-off below still releases the picked waiter
        let Some((idx, pid)) = pick else {
            wake_key(key, 1);
            return 0;
        };
        // Step 3: commit and release the new owner.
        let committed = {
            let mut tbl = FUTEX_TABLE.lock();
            let mut rq  = RUN_QUEUE.lock();
            let ours = match tbl.slot[idx].as_mut() {
                Some(w) if w.pid == pid && w.handoff == 1 => { w.handoff = 2; true }
                _ => false,
            };
            if ours {
                if let Some(t) = rq.find_pid_mut(pid) {
                    if t.state == TaskState::Blocked {
                        t.set_state(TaskState::Ready);
                        t.blocked_futex = 0;
                        t.poll_deadline = u64::MAX;
                    }
                }
                rq.flush_woken();
            }
            ours
        };
        if committed {
            super::wake_up_an_idle_cpu();
            return 0;
        }
        // The claimed waiter was torn down before it could take the lock:
        // hand it to the next one.
    }
}

/// Does a task (thread) with this pid/tid exist? FUTEX_LOCK_PI answers ESRCH
/// for a lock word whose owner TID names no task, as Linux does.
pub fn task_exists(pid: u32) -> bool {
    RUN_QUEUE.lock().find_pid(pid).is_some()
}

/// Drop any pending wait registration for `pid` without waking it.
///
/// Used when a thread is force-killed while `Blocked` in `futex_wait` (e.g.
/// a sibling reaped by `exit_group`'s group-kill loop, see
/// `kill_next_group_member` in lib.rs) — its `FUTEX_TABLE` slot would
/// otherwise linger forever since it will never reach the wake path itself.
pub fn remove_waiter(pid: u32) {
    let mut tbl = FUTEX_TABLE.lock();
    for k in 0..tbl.used.len() {
        let mut bits = tbl.used[k];
        while bits != 0 {
            let i = k * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if tbl.slot[i].map(|w| w.pid) == Some(pid) { tbl.free(i); }
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

/// [`futex_requeue`] with the caller's FUTEX_PRIVATE_FLAG (see [`futex_key`]).
pub fn futex_requeue_keyed(uaddr: usize, uaddr2: usize, val: u32, requeue_limit: u32, private: bool) -> isize {
    let key  = match futex_key(uaddr, private)  { Ok(k) => k, Err(e) => return e };
    let key2 = match futex_key(uaddr2, private) { Ok(k) => k, Err(e) => return e };
    let mut woken    = 0u32;
    let mut requeued = 0u32;
    let mut resched  = false;

    {
        let mut tbl = FUTEX_TABLE.lock();
        let mut rq  = RUN_QUEUE.lock();

        // Walk the bucket as it stands now: a waiter requeued onto a key in
        // the same bucket is appended after `end` and must not be revisited.
        let b = bucket(&key);
        let end = tbl.tail[b];
        let mut i = tbl.head[b];
        while i != NIL {
            let cur = i as usize;
            i = if i == end { NIL } else { tbl.next[cur] };
            let Some(w) = tbl.slot[cur] else { continue };
            // An already-claimed slot belongs to a waiter that is on its way
            // out; it is neither wakeable nor requeueable a second time.
            if w.woken || w.key != key { continue; }

            if woken < val {
                let Some(t) = rq.find_pid_mut(w.pid) else {
                    tbl.free(cur);
                    continue;
                };
                if t.state == TaskState::Blocked {
                    t.set_state(TaskState::Ready);
                    t.blocked_futex = 0;
                    t.poll_deadline = u64::MAX;
                    resched = true;
                }
                if let Some(slot) = tbl.slot[cur].as_mut() { slot.woken = true; }
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
                        tbl.rekey(cur, key2, uaddr2);
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
