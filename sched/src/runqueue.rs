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
/// Every thread in the system. 256 -> 512 (lane multiterm, 2026-09-27): a GPU
/// COSMIC session runs ~200 threads with an empty desktop and each
/// cosmic-term adds ~19 (more while it starts), so the table filled at the
/// third terminal ("[SCHED] task table FULL") and its thread spawn failed.
/// 512 -> 1024 (lane procpool, 2026-09-27): on x86_64/KVM Venus a cosmic-term
/// runs ~18 threads over a ~220-thread session, so 512 filled at 16 terminals.
pub const MAX_TASKS: usize = 1024;

/// Processes (thread groups), as opposed to threads. The per-process tables in
/// the servers and the kernel (vfs fd tables, net socket tables, tty termios and
/// timers, stdio flags, exe paths) are sized to this; the vfs fd-table pool is
/// what enforces it, failing fork/vfork with EAGAIN plus one serial line. It is
/// below MAX_TASKS because a process costs ~10 KiB of fd table in .bss while a
/// thread costs a pointer here, and a desktop runs ~18 threads per process.
pub const MAX_PROCESSES: usize = 512;
const _: () = assert!(MAX_PROCESSES <= MAX_TASKS);

use alloc::boxed::Box;

/// Words in the slot-occupancy bitmap.
const OCC_WORDS: usize = MAX_TASKS / 64;
// Slot indices are stored as u16 in `pick_next`'s candidate buffer.
const _: () = assert!(MAX_TASKS <= 1 << 16 && MAX_TASKS % 64 == 0);
/// pid → slot index table: open addressing, linear probing, four slots per
/// task so the load factor never exceeds 1/4.
const PID_INDEX_SLOTS: usize = 4 * MAX_TASKS;
const _: () = assert!(PID_INDEX_SLOTS.is_power_of_two());
const PID_INDEX_MASK: usize = PID_INDEX_SLOTS - 1;

/// Iterator over the occupied slot indices of a [`RunQueue`], in slot order,
/// from a snapshot of the occupancy bitmap taken by [`RunQueue::live_slots`].
///
/// It owns its snapshot (512 B at 4096 slots), so the loop body may take
/// `&mut RunQueue`: a slot emptied during the walk just reads `None` from
/// `get`, as it always did, and a task enqueued during the walk is not
/// visited. Cost is one word test per 64 slots plus one step per live task,
/// where `for i in 0..MAX_TASKS` paid one `Option` test per slot.
pub struct LiveSlots {
    bits: [u64; OCC_WORDS],
    w:    usize,
    cur:  u64,
}

impl Iterator for LiveSlots {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        loop {
            if self.cur != 0 {
                let b = self.cur.trailing_zeros() as usize;
                self.cur &= self.cur - 1;
                return Some(self.w * 64 + b);
            }
            self.w += 1;
            if self.w >= OCC_WORDS { return None; }
            self.cur = self.bits[self.w];
        }
    }
}

pub struct RunQueue {
    /// Private on purpose: every `&mut Task` must come from a method that
    /// maintains `maybe_ready` (`get_mut`, `find_pid_mut`, or a loop in this
    /// file that sets the bit itself).
    tasks: [Option<Box<Task>>; MAX_TASKS],
    len:       usize,
    /// Tasks with `pid == tgid` (see `process_count`).
    nproc:     usize,
    /// Bit `i` set ⇔ `tasks[i].is_some()`. Maintained by `enqueue`/`remove`
    /// (the only writers of `tasks`), so scans skip empty slots.
    occupied:  [u64; OCC_WORDS],
    /// `pick_next`'s candidate set: a SUPERSET of the pickable slots, and of
    /// the Running ones (which `pick_next` leaves set for `min_vruntime`). Set
    /// whenever a `&mut Task` is handed out (and on enqueue); cleared only by
    /// `pick_next` itself on finding the task not `Ready`. A task can only
    /// become pickable through a `&mut` to it, so a clear bit means "not
    /// Ready" — and the once-per-tick full scan in `pick_next` checks that.
    maybe_ready: [u64; OCC_WORDS],
    /// `ticks()` of the last full-scan pick.
    last_full_scan: u64,
    /// pid → slot index, packed `(pid << 32) | idx`, `0` = empty.
    ///
    /// Every pid lookup (`find_pid*`) used to be a linear scan of all 256
    /// slots under RUN_QUEUE, dereferencing each live `Task` — ~1 µs per
    /// lookup with ~120 tasks on a COSMIC desktop, and several lookups per
    /// hold (`has_deliverable_signal` does two).
    ///
    /// AUTHORITATIVE since lane procmax (2026-10-12): every task with a
    /// non-zero pid is indexed (`enqueue`), unindexed on `remove`, and the
    /// one in-place pid change (`take_over_leader`'s exchange) calls
    /// `reindex` for both slots. So a lookup that reaches an empty entry is a
    /// definite miss and no longer falls back to a scan of the whole table —
    /// a dead-pid lookup (kill(pid, 0), tgid_of of an exited thread) used to
    /// walk every slot. Linear probing with backward-shift deletion: no
    /// tombstones, so chains never grow with pid churn. A hit is still
    /// verified against `tasks[idx].pid`; a mismatch (a broken invariant)
    /// falls back to the scan rather than answer wrong.
    pid_index: [u64; PID_INDEX_SLOTS],
    /// `pick_next`'s candidate buffer. A field, not a stack array: a local
    /// `[u16; MAX_TASKS]` is 8 KiB of kernel stack at 4096 slots, zero-filled
    /// on every dispatch.
    cand: [u16; MAX_TASKS],
}

impl RunQueue {
    pub const fn new() -> Self {
        Self {
            tasks: [const { None }; MAX_TASKS],
            len: 0,
            nproc: 0,
            occupied: [0; OCC_WORDS],
            maybe_ready: [0; OCC_WORDS],
            last_full_scan: 0,
            pid_index: [0; PID_INDEX_SLOTS],
            cand: [0; MAX_TASKS],
        }
    }

    /// Snapshot iterator over the occupied slots — the replacement for every
    /// `for i in 0..MAX_TASKS { rq.get(i) ... }` walk. See [`LiveSlots`].
    #[inline]
    pub fn live_slots(&self) -> LiveSlots {
        LiveSlots { bits: self.occupied, w: 0, cur: self.occupied[0] }
    }

    #[inline]
    fn index_home(pid: Pid) -> usize { (pid as usize) & PID_INDEX_MASK }

    /// Index `tasks[idx]` under `pid` (pid 0 is never indexed). Replaces an
    /// existing entry for the pid. Cannot fail: entries never outnumber tasks
    /// and the table has four slots per task.
    fn index_insert(&mut self, pid: Pid, idx: usize) {
        if pid == 0 { return; }
        let want = ((pid as u64) << 32) | idx as u64;
        let mut s = Self::index_home(pid);
        for _ in 0..PID_INDEX_SLOTS {
            let e = self.pid_index[s];
            if e == 0 || (e >> 32) as u32 == pid {
                self.pid_index[s] = want;
                return;
            }
            s = (s + 1) & PID_INDEX_MASK;
        }
    }

    /// Drop `pid`'s entry, if any, closing the hole by backward shift so no
    /// probe chain is ever broken and no tombstone is ever left behind.
    fn index_remove(&mut self, pid: Pid) {
        if pid == 0 { return; }
        let mut s = Self::index_home(pid);
        let mut found = None;
        for _ in 0..PID_INDEX_SLOTS {
            let e = self.pid_index[s];
            if e == 0 { break; }
            if (e >> 32) as u32 == pid { found = Some(s); break; }
            s = (s + 1) & PID_INDEX_MASK;
        }
        let Some(mut hole) = found else { return };
        let mut j = hole;
        loop {
            j = (j + 1) & PID_INDEX_MASK;
            let e = self.pid_index[j];
            if e == 0 { break; }
            let home = Self::index_home((e >> 32) as u32);
            // `e` may move into the hole unless its home lies cyclically in
            // (hole, j] — moving it in front of its home would hide it.
            if (j.wrapping_sub(home) & PID_INDEX_MASK) >= (j.wrapping_sub(hole) & PID_INDEX_MASK) {
                self.pid_index[hole] = e;
                hole = j;
            }
        }
        self.pid_index[hole] = 0;
    }

    /// Slot the index names for `pid` (unverified); `None` = definite miss.
    #[inline]
    fn index_lookup(&self, pid: Pid) -> Option<usize> {
        let mut s = Self::index_home(pid);
        for _ in 0..PID_INDEX_SLOTS {
            let e = self.pid_index[s];
            if e == 0 { return None; }
            if (e >> 32) as u32 == pid { return Some((e & 0xFFFF_FFFF) as usize); }
            s = (s + 1) & PID_INDEX_MASK;
        }
        None
    }

    /// Re-index the task in slot `idx` after its pid was changed in place
    /// (`take_over_leader`'s pid exchange). REQUIRED for both slots of the
    /// exchange, since the index is authoritative.
    pub fn reindex(&mut self, idx: usize) {
        if let Some(pid) = self.tasks[idx].as_ref().map(|t| t.pid) {
            self.index_insert(pid, idx);
        }
    }

    /// Scan fallback for `find_pid*`: pid 0 (never indexed), and a hit that
    /// failed verification (a broken index invariant).
    #[inline(never)]
    fn scan_pid(&self, pid: Pid) -> Option<usize> {
        self.live_slots().find(|&i| self.tasks[i].as_ref().map_or(false, |t| t.pid == pid))
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
    ///
    /// Called on every wake, so it walks `maybe_ready` — a superset of the
    /// Ready and Running tasks (see the field) — instead of every task: the
    /// cost is the runnable count, not the sleeping crowd (a parallel build
    /// parks thousands of threads, a desktop ~200).
    pub fn min_vruntime(&self) -> u64 {
        let mut min = u64::MAX;
        for w in 0..OCC_WORDS {
            let mut bits = self.maybe_ready[w] & self.occupied[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if let Some(t) = &self.tasks[i] {
                    if matches!(t.state, TaskState::Ready | TaskState::Running) && t.vruntime < min {
                        min = t.vruntime;
                    }
                }
            }
        }
        if min == u64::MAX { 0 } else { min }
    }

    /// First free slot, from the occupancy bitmap.
    #[inline]
    fn free_slot(&self) -> Option<usize> {
        for w in 0..OCC_WORDS {
            let free = !self.occupied[w];
            if free != 0 { return Some(w * 64 + free.trailing_zeros() as usize); }
        }
        None
    }

    /// Insert a task into the first free slot. Returns false if the queue is full.
    ///
    /// The task is EEVDF-placed relative to the current runnable set.
    pub fn enqueue(&mut self, mut task: Box<Task>) -> bool {
        let Some(i) = self.free_slot() else { return false };
        let min_vr = self.min_vruntime();
        task.place(min_vr);
        // Publish pid → tgid before the task becomes reachable, so no
        // CPU can pick it up and then miss in the side table. Safe to
        // do under this lock: the table is only ever written from here
        // and from `remove`, both of which hold it.
        crate::pid_tgid_insert(task.pid, task.tgid);
        let pid = task.pid;
        if task.pid == task.tgid { self.nproc += 1; }
        self.tasks[i] = Some(task);
        self.occupied[i / 64] |= 1u64 << (i % 64);
        self.touch(i);
        self.index_insert(pid, i);
        self.len += 1;
        true
    }

    /// Thread-group leaders in the table: live processes plus zombie leaders
    /// whose slot is not recycled yet. Maintained by `enqueue`/`remove` and
    /// `note_leader_promoted`, so the fork limit check is O(1).
    pub fn process_count(&self) -> usize { self.nproc }

    /// `take_over_leader` promoted a thread to leader in place (its leader
    /// was already gone): one more task now has `pid == tgid`.
    pub fn note_leader_promoted(&mut self) { self.nproc += 1; }

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
                    // Blocked / Stopped / Zombie: not pickable until something
                    // takes `&mut` to it again, which re-sets the bit. Running
                    // keeps its bit: `min_vruntime` walks this set and must see
                    // the tasks on a CPU (at most one per CPU, so a pick visits
                    // only a handful more).
                    if t.state != TaskState::Running {
                        self.maybe_ready[w] &= !bit;
                    } else if full && self.maybe_ready[w] & bit == 0 {
                        self.maybe_ready[w] |= bit;
                    }
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
                self.cand[n] = i as u16;
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
        for k in 0..n {
            let i = self.cand[k] as usize;
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

    /// Slot of `pid`, from the authoritative pid index (each hit verified
    /// against the slot); pid 0, never indexed, is scanned for.
    #[inline]
    pub fn find_pid_idx(&self, pid: Pid) -> Option<usize> {
        if pid == 0 { return self.scan_pid(pid); }
        match self.index_lookup(pid) {
            Some(i) if self.tasks[i].as_ref().map_or(false, |t| t.pid == pid) => Some(i),
            Some(_) => self.scan_pid(pid),
            None => None,
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
        for w in 0..OCC_WORDS {
          let mut bits = self.occupied[w];
          while bits != 0 {
            let i = w * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if let Some(task) = self.tasks[i].as_deref_mut() {
                if task.blocked_on == Some(port) && task.state == TaskState::Blocked
                    && ((task.poll_mask & tag) != 0 || tag == crate::POLL_TAG_ALL) {
                    if crate::gdwake::ENABLED {
                        crate::gdwake::mark(task.pid, crate::gdwake::SITE.load(core::sync::atomic::Ordering::Relaxed));
                    }
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
        }
        if crate::gdwake::ENABLED {
            crate::gdwake::SITE.store(crate::gdwake::UNATTRIBUTED, core::sync::atomic::Ordering::Relaxed);
        }
        woken
    }

    /// Wake `pid` if it is parked on the kernel wait channel
    /// (`crate::KWAIT_CHANNEL`, used by `ksync`). Returns true if it was
    /// made Ready. A task that is not parked there (still running between
    /// prepare and commit is impossible: prepare publishes Blocked first) is
    /// left alone; its waiter loop re-checks its condition anyway.
    pub fn wake_kwaiter(&mut self, pid: Pid) -> bool {
        let min_vr = self.min_vruntime();
        match self.find_pid_mut(pid) {
            Some(task) if task.state == TaskState::Blocked
                && task.blocked_on == Some(crate::KWAIT_CHANNEL) => {
                task.state         = TaskState::Ready;
                task.blocked_on    = None;
                task.poll_deadline = u64::MAX;
                task.poll_mask     = crate::POLL_TAG_ALL;
                task.place(min_vr);
                true
            }
            _ => false,
        }
    }

    /// Wake every task parked on the kernel wait channel (the fallback when a
    /// deferred-wake record overflowed: spurious wakes are harmless, every
    /// `ksync` waiter loops on its condition).
    pub fn wake_all_kwaiters(&mut self) -> usize {
        let min_vr = self.min_vruntime();
        let mut woken = 0;
        for i in self.live_slots() {
            if let Some(task) = self.tasks[i].as_deref_mut() {
                if task.state == TaskState::Blocked && task.blocked_on == Some(crate::KWAIT_CHANNEL) {
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

    /// See `crate::widen_poll_masks`: OR `add` into the `poll_mask` of every
    /// task Blocked on `port` whose mask intersects `match_tag`.
    pub fn widen_port_masks(&mut self, port: u32, match_tag: u64, add: u64) {
        for i in self.live_slots() {
            if let Some(task) = self.tasks[i].as_deref_mut() {
                if task.blocked_on == Some(port) && task.state == TaskState::Blocked
                    && (task.poll_mask & match_tag) != 0 {
                    task.poll_mask |= add;
                }
            }
        }
    }

    /// Poll-deadline tick service: wake every task on `port` whose
    /// `poll_deadline` is due (`<= now`), and every poller whose interest mask
    /// names a timerfd that expired on this pass (`timerfd_tags`, an OR of
    /// `poll_tag(TIMERFD, slot)`; a broadcast-mask waiter always matches).
    /// This used to wake EVERY parked poller on any timerfd expiry: on the
    /// idle COSMIC desktop that was ~100 threads re-probing their epoll sets
    /// at each of the ~6.6 expiries a second — most of the guest's wakeups.
    /// Returns
    /// `(earliest_remaining_deadline, woken)` so the caller can republish the
    /// exact next deadline (no single-global clobber) and kick an idle CPU.
    /// The whole scan runs under one RUN_QUEUE hold, so a woken task can only
    /// re-register its next deadline after this returns — the recomputed
    /// minimum can never be stale-clobbered by a concurrent register.
    pub fn wake_due_poll_deadlines(&mut self, port: u32, now: u64, timerfd_tags: u64)
        -> (u64, usize)
    {
        let min_vr = self.min_vruntime();
        let mut new_min = u64::MAX;
        let mut woken = 0;
        for w in 0..OCC_WORDS {
          let mut bits = self.occupied[w];
          while bits != 0 {
            let i = w * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if let Some(task) = self.tasks[i].as_deref_mut() {
                if task.state != TaskState::Blocked { continue; }
                // Timed `ksync` waiters (KWAIT_CHANNEL) are released at their
                // deadline exactly like poll waiters; their mask is 0, so no
                // timerfd tag ever matches them.
                let is_poll  = task.blocked_on == Some(port)
                    || task.blocked_on == Some(crate::KWAIT_CHANNEL);
                // Timed futex waiters (see sched::futex_wait) register a
                // `poll_deadline` and must be released at timeout too — they are
                // Blocked on `blocked_futex`, not the poll channel. A timerfd
                // expiry is a poll-channel concept and never wakes futex waiters.
                let is_futex = task.blocked_futex != 0;
                if !is_poll && !is_futex { continue; }
                if (is_poll && task.poll_mask & timerfd_tags != 0) || task.poll_deadline <= now {
                    if crate::idlestat::ENABLED {
                        crate::idlestat::note_deadline_wake(
                            task.poll_deadline <= now,
                            is_poll && task.poll_mask == crate::POLL_TAG_ALL);
                    }
                    if crate::gdwake::ENABLED {
                        crate::gdwake::mark(task.pid, if task.poll_deadline <= now {
                            crate::gdwake::DL_OWN } else { crate::gdwake::DL_TFD });
                    }
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
            if task.pid == task.tgid { self.nproc = self.nproc.saturating_sub(1); }
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
