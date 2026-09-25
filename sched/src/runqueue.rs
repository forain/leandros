//! Run queue — fixed-size array of task slots with EEVDF selection.
//!
//! Selection policy: Earliest Eligible Virtual Deadline First (EEVDF), the
//! same family as Linux ≥ 6.6's default scheduler.
//!
//!  * Every runnable task has a `vruntime` (weighted virtual runtime) and a
//!    `vdeadline` (`vruntime + slice/weight` at last renewal).
//!  * A task is **eligible** when its `vruntime` is at or below the weighted
//!    average vruntime of the runnable set — i.e. it has received no more
//!    than its fair share of CPU so far.
//!  * `pick_next` returns the eligible task with the earliest virtual
//!    deadline.
//!
//! SMP: the queue is shared by all CPUs (guarded by the `RUN_QUEUE` mutex in
//! lib.rs).  Tasks with `on_cpu.is_some()` are skipped — their register state
//! is still live on another core.

use super::task::{Pid, Task, TaskState};
pub const MAX_TASKS: usize = 256;

use alloc::boxed::Box;

/// Words in the slot-occupancy bitmap.
const OCC_WORDS: usize = MAX_TASKS / 64;
// Slot indices are packed into a byte (`pid_index`, `pick_next`'s candidates).
const _: () = assert!(MAX_TASKS <= 256 && MAX_TASKS % 64 == 0);
/// pid → slot hint table: `PID_INDEX_SLOTS` entries, each pid hashed to a
/// window of `PID_INDEX_PROBE` consecutive entries.
const PID_INDEX_SLOTS: usize = 1024;
const PID_INDEX_PROBE: usize = 8;

pub struct RunQueue {
    /// Private on purpose: every `&mut Task` must come from a method that
    /// maintains `maybe_ready` (`get_mut`, `find_pid_mut`, or a loop in this
    /// file that sets the bit itself).
    tasks: [Option<Box<Task>>; MAX_TASKS],
    len:       usize,
    /// Bit `i` set ⇔ `tasks[i].is_some()`. Maintained by `enqueue`/`remove`
    /// (the only writers of `tasks`), so scans skip empty slots.
    occupied:  [u64; OCC_WORDS],
    /// `pick_next`'s candidate set: a SUPERSET of the pickable slots. Set
    /// whenever a `&mut Task` is handed out (and on enqueue); cleared only by
    /// `pick_next` itself on finding the task not `Ready`. A task can only
    /// become pickable through a `&mut` to it, so a clear bit means "not
    /// Ready" — and the once-per-tick full scan in `pick_next` checks that.
    maybe_ready: [u64; OCC_WORDS],
    /// `ticks()` of the last full-scan pick.
    last_full_scan: u64,
    /// pid → slot index, packed `(pid << 8) | idx`, `0` = empty.
    ///
    /// Every pid lookup (`find_pid*`) used to be a linear scan of all 256
    /// slots under RUN_QUEUE, dereferencing each live `Task` — ~1 µs per
    /// lookup with ~120 tasks on a COSMIC desktop, and several lookups per
    /// hold (`has_deliverable_signal` does two). This table is only a HINT:
    /// a hit is verified against `tasks[idx].pid` and anything else falls back
    /// to the scan, so a stale or missing entry can cost time but can never
    /// return the wrong task. Probing is bounded to a fixed window (no
    /// tombstones, no unbounded chains): pids are sequential, so with ≤ 256
    /// live tasks in 1024 entries a window is essentially never full; if it
    /// is, the pid just isn't indexed.
    pid_index: [u64; PID_INDEX_SLOTS],
}

impl RunQueue {
    pub const fn new() -> Self {
        Self {
            tasks: [const { None }; MAX_TASKS],
            len: 0,
            occupied: [0; OCC_WORDS],
            maybe_ready: [0; OCC_WORDS],
            last_full_scan: 0,
            pid_index: [0; PID_INDEX_SLOTS],
        }
    }

    #[inline]
    fn index_home(pid: Pid) -> usize { (pid as usize) & (PID_INDEX_SLOTS - 1) }

    /// Record `tasks[idx]`'s pid in the hint table (pid 0 is never indexed).
    fn index_insert(&mut self, pid: Pid, idx: usize) {
        if pid == 0 { return; }
        let want = ((pid as u64) << 8) | idx as u64;
        let home = Self::index_home(pid);
        let mut free = None;
        for k in 0..PID_INDEX_PROBE {
            let s = (home + k) & (PID_INDEX_SLOTS - 1);
            let e = self.pid_index[s];
            if e == want { return; }
            if (e >> 8) as u32 == pid { self.pid_index[s] = want; return; }
            if free.is_none() && (e == 0 || !self.index_entry_live(e)) { free = Some(s); }
        }
        if let Some(s) = free { self.pid_index[s] = want; }
    }

    /// Drop `pid`'s hint entry, if any.
    fn index_remove(&mut self, pid: Pid) {
        if pid == 0 { return; }
        let home = Self::index_home(pid);
        for k in 0..PID_INDEX_PROBE {
            let s = (home + k) & (PID_INDEX_SLOTS - 1);
            if self.pid_index[s] != 0 && (self.pid_index[s] >> 8) as u32 == pid {
                self.pid_index[s] = 0;
            }
        }
    }

    /// An entry still describes the task in its slot.
    #[inline]
    fn index_entry_live(&self, e: u64) -> bool {
        let idx = (e & 0xff) as usize;
        self.tasks[idx].as_ref().map_or(false, |t| t.pid == (e >> 8) as u32)
    }

    /// Slot of `pid` via the hint table (verified), else `None`.
    #[inline]
    fn index_lookup(&self, pid: Pid) -> Option<usize> {
        if pid == 0 { return None; }
        let home = Self::index_home(pid);
        for k in 0..PID_INDEX_PROBE {
            let e = self.pid_index[(home + k) & (PID_INDEX_SLOTS - 1)];
            if e != 0 && (e >> 8) as u32 == pid {
                let idx = (e & 0xff) as usize;
                if self.tasks[idx].as_ref().map_or(false, |t| t.pid == pid) {
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Re-index the task in slot `idx` after its pid was changed in place
    /// (`take_over_leader`'s pid exchange). Without it the lookups still
    /// work — the stale entry fails verification and the scan runs.
    pub fn reindex(&mut self, idx: usize) {
        if let Some(pid) = self.tasks[idx].as_ref().map(|t| t.pid) {
            self.index_insert(pid, idx);
        }
    }

    /// Linear-scan fallback for `find_pid*`.
    #[inline(never)]
    fn scan_pid(&self, pid: Pid) -> Option<usize> {
        self.tasks.iter().position(|s| {
            s.as_ref().map(|t| t.pid == pid).unwrap_or(false)
        })
    }

    /// Get the number of tasks currently in the run queue.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Occupied slots as a fraction of the table. `enqueue` fails — and both
    /// `fork()` and `clone()` then return ENOMEM — the moment this reaches
    /// `MAX_TASKS`, with plenty of physical memory still free, so a plain
    /// "out of memory" from userspace can mean this and nothing about RAM.
    pub fn capacity(&self) -> usize {
        MAX_TASKS
    }

    /// Minimum `vruntime` over all runnable (Ready or Running) tasks.
    ///
    /// Used to place newly enqueued or freshly woken tasks so they compete
    /// fairly from "now" instead of replaying the virtual time they slept
    /// through.
    pub fn min_vruntime(&self) -> u64 {
        self.tasks.iter()
            .filter_map(|s| s.as_ref())
            .filter(|t| matches!(t.state, TaskState::Ready | TaskState::Running))
            .map(|t| t.vruntime)
            .min()
            .unwrap_or(0)
    }

    /// Insert a task into the first free slot. Returns false if the queue is full.
    ///
    /// The task is EEVDF-placed relative to the current runnable set.
    pub fn enqueue(&mut self, mut task: Box<Task>) -> bool {
        let min_vr = self.min_vruntime();
        task.place(min_vr);
        for i in 0..MAX_TASKS {
            if self.tasks[i].is_none() {
                // Publish pid → tgid before the task becomes reachable, so no
                // CPU can pick it up and then miss in the side table. Safe to
                // do under this lock: the table is only ever written from here
                // and from `remove`, both of which hold it.
                crate::pid_tgid_insert(task.pid, task.tgid);
                let pid = task.pid;
                self.tasks[i] = Some(task);
                self.occupied[i / 64] |= 1u64 << (i % 64);
                self.touch(i);
                self.index_insert(pid, i);
                self.len += 1;
                return true;
            }
        }
        false
    }

    /// EEVDF pick: the eligible Ready task with the earliest virtual deadline.
    ///
    /// Tasks already running on another CPU (`on_cpu.is_some()`) are skipped.
    /// Returns the slot index so the caller can track which task is active.
    ///
    /// Cost: proportional to the tasks in `maybe_ready`, not to `MAX_TASKS`
    /// (was: two passes over all 256 slots, touching every live `Task` —
    /// ~2 µs per dispatch with ~110 tasks under a COSMIC desktop, the
    /// longest regular RUN_QUEUE hold). Once per tick one pick walks every
    /// occupied slot instead and repairs `maybe_ready`, so a hint the
    /// invariant somehow missed costs at most one tick, never a lost task.
    pub fn pick_next(&mut self) -> Option<usize> {
        if self.len == 0 { return None; }

        let t0 = crate::lockwatch::pick_clock();
        let mut visited = 0u32;
        let now = crate::ticks();
        let full = now != self.last_full_scan;
        if full { self.last_full_scan = now; }

        // Pass 1: total weight and weighted vruntime sum of the candidates —
        // the weighted average sum_wv / sum_w is the virtual time "V" against
        // which eligibility is judged — and the candidates' slot indices, in
        // slot order (the same tie-break order as a full scan).
        let mut cand = [0u8; MAX_TASKS];
        let mut n = 0usize;
        let mut sum_w:  u64  = 0;
        let mut sum_wv: u128 = 0;
        for w in 0..OCC_WORDS {
            let mut bits = if full { self.occupied[w] } else { self.maybe_ready[w] & self.occupied[w] };
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                visited += 1;
                let bit = 1u64 << (i % 64);
                let t = match &self.tasks[i] { Some(t) => t, None => continue };
                if t.state != TaskState::Ready {
                    // Blocked / Running / Stopped / Zombie: not pickable until
                    // something takes `&mut` to it again, which re-sets the bit.
                    self.maybe_ready[w] &= !bit;
                    continue;
                }
                if full && self.maybe_ready[w] & bit == 0 {
                    // A Ready task no `&mut` path flagged: the invariant was
                    // broken somewhere. Repair and count it (RQPROF prints it).
                    self.maybe_ready[w] |= bit;
                    crate::lockwatch::note_ready_hint_miss();
                }
                // Stop-the-world fork: siblings of a mid-clone_as thread group
                // stay parked until the CoW downgrade + TLB shootdown are
                // complete (see sched::quiesce_thread_group). They, and Ready
                // tasks whose registers are still live on another CPU, keep
                // their bit.
                if t.on_cpu.is_some() || super::quiesce_filtered(t.tgid, t.pid) { continue; }
                sum_w  += t.weight as u64;
                sum_wv += t.weight as u128 * t.vruntime as u128;
                cand[n] = i as u8;
                n += 1;
            }
        }
        if sum_w == 0 {
            crate::lockwatch::note_pick(full, visited, crate::lockwatch::pick_clock().wrapping_sub(t0));
            return None;
        }

        // Pass 2: earliest virtual deadline among eligible candidates.  The
        // task with the minimum vruntime is always eligible, so `best` is
        // always found; `fallback` guards against arithmetic corner cases only.
        let mut best:     Option<(usize, u64)> = None; // (idx, vdeadline)
        let mut fallback: Option<(usize, u64)> = None; // (idx, vruntime)
        for &c in &cand[..n] {
            let i = c as usize;
            if let Some(t) = &self.tasks[i] {
                let eligible = (t.vruntime as u128) * (sum_w as u128) <= sum_wv;
                if eligible && best.map_or(true, |(_, d)| t.vdeadline < d) {
                    best = Some((i, t.vdeadline));
                }
                if fallback.map_or(true, |(_, v)| t.vruntime < v) {
                    fallback = Some((i, t.vruntime));
                }
            }
        }
        crate::lockwatch::note_pick(full, visited, crate::lockwatch::pick_clock().wrapping_sub(t0));
        best.or(fallback).map(|(i, _)| i)
    }

    /// Flag slot `idx` as possibly pickable. Every path that can hand out a
    /// `&mut Task` calls this, so any state change — in particular any
    /// transition to `Ready`, or `on_cpu` being released — leaves the bit set.
    #[inline(always)]
    fn touch(&mut self, idx: usize) {
        self.maybe_ready[idx / 64] |= 1u64 << (idx % 64);
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Task> {
        self.touch(idx);
        self.tasks[idx].as_mut().map(|boxed_task| boxed_task.as_mut())
    }

    pub fn get(&self, idx: usize) -> Option<&Task> {
        self.tasks[idx].as_ref().map(|boxed_task| boxed_task.as_ref())
    }

    pub fn find_pid(&self, pid: Pid) -> Option<&Task> {
        let idx = self.find_pid_idx(pid)?;
        self.tasks[idx].as_deref()
    }

    pub fn find_pid_mut(&mut self, pid: Pid) -> Option<&mut Task> {
        let idx = self.find_pid_idx(pid)?;
        self.touch(idx);
        self.tasks[idx].as_deref_mut()
    }

    /// Slot of `pid`: the verified hint table first, the full scan on a miss
    /// (a dead pid, pid 0, or an unindexed task), so the answer is always
    /// the scan's answer.
    #[inline]
    pub fn find_pid_idx(&self, pid: Pid) -> Option<usize> {
        match self.index_lookup(pid) {
            Some(i) => Some(i),
            None => self.scan_pid(pid),
        }
    }

    /// Block the task with `pid`, recording the port it is waiting on.
    pub fn block_on_port(&mut self, pid: Pid, port: u32) {
        // IPC/generic waiters are woken by an untagged `unblock_port`; register
        // the broadcast mask so a tagged poll-wake also always reaches them.
        self.block_on_port_until(pid, port, u64::MAX, crate::POLL_TAG_ALL);
    }

    /// Block the task with `pid` on `port`, recording its wake deadline
    /// (absolute `monotonic_ns()`; `u64::MAX` = none) and its poll interest-set `mask`
    /// (see `poll_tag`; `POLL_TAG_ALL` = broadcast). All three are set
    /// atomically with `state`/`blocked_on` under the one RUN_QUEUE hold so the
    /// poll-deadline tick and `unblock_port_tagged` see a consistent snapshot,
    /// and so no window exists in which `blocked_on == POLL_WAIT_CHANNEL` is
    /// visible with a stale `poll_mask`.
    pub fn block_on_port_until(&mut self, pid: Pid, port: u32, deadline: u64, mask: u64) {
        if let Some(task) = self.find_pid_mut(pid) {
            task.state         = TaskState::Blocked;
            task.blocked_on    = Some(port);
            task.poll_deadline = deadline;
            task.poll_mask     = mask;
        }
    }

    /// Wake all tasks blocked on `port`.  Returns the number woken so the
    /// caller can kick an idle CPU when work became available.
    pub fn unblock_port(&mut self, port: u32) -> usize {
        // Broadcast: `POLL_TAG_ALL` intersects every non-zero `poll_mask`, and
        // an IPC waiter's mask is `POLL_TAG_ALL`, so this is exactly the old
        // wake-everyone behaviour.
        self.unblock_port_tagged(port, crate::POLL_TAG_ALL)
    }

    /// Wake tasks blocked on `port` whose `poll_mask` intersects `tag`. IPC
    /// waiters carry `poll_mask == POLL_TAG_ALL` and so are matched by any
    /// non-zero tag; a poll waiter carries the OR of its interests' tags. On
    /// wake the mask is reset to `POLL_TAG_ALL` so a future non-poll block
    /// (which does not go through `block_on_port_until` — e.g. nothing) can
    /// never inherit a stale narrow value; the read here is under the same lock
    /// that the write in `block_on_port_until` took, with `blocked_on` and
    /// `Blocked` both still true, so the value is always the one this park set.
    pub fn unblock_port_tagged(&mut self, port: u32, tag: u64) -> usize {
        let min_vr = self.min_vruntime();
        let mut woken = 0;
        for (i, slot) in self.tasks.iter_mut().enumerate() {
            if let Some(task) = slot {
                if task.blocked_on == Some(port) && task.state == TaskState::Blocked
                    && (task.poll_mask & tag) != 0 {
                    self.maybe_ready[i / 64] |= 1u64 << (i % 64);
                    task.state         = TaskState::Ready;
                    task.blocked_on    = None;
                    task.poll_deadline = u64::MAX;
                    task.poll_mask     = crate::POLL_TAG_ALL;
                    task.place(min_vr);
                    woken += 1;
                }
            }
        }
        woken
    }

    /// Poll-deadline tick service: wake every task on `port` whose
    /// `poll_deadline` is due (`<= now`) or, when `timerfd_due`, all of them
    /// (a timerfd expired — every parked poller must re-probe it). Returns
    /// `(earliest_remaining_deadline, woken)` so the caller can republish the
    /// exact next deadline (no single-global clobber) and kick an idle CPU.
    /// The whole scan runs under one RUN_QUEUE hold, so a woken task can only
    /// re-register its next deadline after this returns — the recomputed
    /// minimum can never be stale-clobbered by a concurrent register.
    pub fn wake_due_poll_deadlines(&mut self, port: u32, now: u64, timerfd_due: bool)
        -> (u64, usize)
    {
        let min_vr = self.min_vruntime();
        let mut new_min = u64::MAX;
        let mut woken = 0;
        for (i, slot) in self.tasks.iter_mut().enumerate() {
            if let Some(task) = slot {
                if task.state != TaskState::Blocked { continue; }
                let is_poll  = task.blocked_on == Some(port);
                // Timed futex waiters (see sched::futex_wait) register a
                // `poll_deadline` and must be released at timeout too — they are
                // Blocked on `blocked_futex`, not the poll channel. `timerfd_due`
                // is a poll-channel concept (an expired timerfd every poller must
                // re-probe) and never mass-wakes futex waiters.
                let is_futex = task.blocked_futex != 0;
                if !is_poll && !is_futex { continue; }
                if (timerfd_due && is_poll) || task.poll_deadline <= now {
                    self.maybe_ready[i / 64] |= 1u64 << (i % 64);
                    task.state         = TaskState::Ready;
                    if is_poll  { task.blocked_on = None; task.poll_mask = crate::POLL_TAG_ALL; }
                    if is_futex { task.blocked_futex = 0;    }
                    task.poll_deadline = u64::MAX;
                    task.place(min_vr);
                    woken += 1;
                } else if task.poll_deadline < new_min {
                    new_min = task.poll_deadline;
                }
            }
        }
        (new_min, woken)
    }

    /// Mark a task as Zombie (terminal; will not be scheduled again).
    pub fn mark_zombie(&mut self, pid: Pid) {
        if let Some(task) = self.find_pid_mut(pid) {
            task.state = TaskState::Zombie;
        }
    }

    /// Remove the task at `idx` from the run queue and return it so the caller
    /// can free its resources.  Decrements the task count.
    pub fn remove(&mut self, idx: usize) -> Option<Box<Task>> {
        let t = self.tasks[idx].take();
        if let Some(task) = t.as_ref() {
            self.len = self.len.saturating_sub(1);
            self.occupied[idx / 64] &= !(1u64 << (idx % 64));
            self.maybe_ready[idx / 64] &= !(1u64 << (idx % 64));
            self.index_remove(task.pid);
            // Retire the side-table entry with the slot. Leaving it would hand
            // a later caller the tgid of a task that no longer exists, where
            // `tgid_of`'s contract is to fall back to the pid itself.
            crate::pid_tgid_remove(task.pid);
        }
        t
    }
}
