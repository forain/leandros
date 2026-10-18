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
use core::sync::atomic::Ordering;
/// Every thread in the system. 256 -> 512 (lane multiterm, 2026-09-27): a GPU
/// COSMIC session runs ~200 threads with an empty desktop and each
/// cosmic-term adds ~19 (more while it starts), so the table filled at the
/// third terminal ("[SCHED] task table FULL") and its thread spawn failed.
/// 512 -> 1024 (lane procpool, 2026-09-27): on x86_64/KVM Venus a cosmic-term
/// runs ~18 threads over a ~220-thread session, so 512 filled at 16 terminals.
/// 1024 -> 4096 (lane procmax, 2026-10-12): an in-guest parallel `cargo build`
/// (cargo, its jobserver, N rustc with their own thread pools) on top of the
/// desktop. Raising it is cheap only because nothing walks `0..MAX_TASKS` any
/// more: scans go through the occupancy bitmap (`live_slots`), lookups through
/// the authoritative pid index, `min_vruntime` through the runnable hint set,
/// and futex wakes through per-address buckets — so costs follow the live
/// (or runnable) task count, not this constant.
pub const MAX_TASKS: usize = 4096;

/// Processes (thread groups), as opposed to threads. The per-process tables in
/// the servers and the kernel (vfs fd tables, net socket tables, tty termios and
/// timers, stdio flags, exe paths, the zombie exit-record pool) are sized to
/// this. `fork_admission` enforces it — live leaders plus unwaited zombie
/// records, EAGAIN beyond — and the vfs fd-table pool backs it up. It is
/// below MAX_TASKS because a process costs ~20 KiB of fd table in .bss while a
/// thread costs a pointer here, and a desktop runs ~18 threads per process.
/// 512 -> 2048 with MAX_TASKS (lane procmax).
pub const MAX_PROCESSES: usize = 2048;
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

/// Must a wake scan be able to find `t`? Blocked on a port or wait channel,
/// or Blocked in a futex wait with a deadline (the tick releases it).
#[inline]
fn wake_scannable(t: &Task) -> bool {
    t.state == TaskState::Blocked
        && (t.blocked_on.is_some() || (t.blocked_futex != 0 && t.poll_deadline != u64::MAX))
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
    /// Wake-scan candidate set: a SUPERSET of the tasks parked where a wake
    /// scan must find them — on a port or wait channel (`blocked_on`, whose
    /// only writer is `block_on_port_until`, which sets the bit) or in a timed
    /// futex wait (`note_timed_wait`). The port wakes, `widen_port_masks` and
    /// the deadline tick walk this set instead of every task, so they cost the
    /// parked pollers, not the threads parked in untimed futex waits (most of
    /// a parallel build's thousands). A bit is cleared lazily by those walks
    /// when its task is no longer parked, and repaired by `pick_next`'s
    /// once-per-tick full scan, which counts a repair as a hint miss.
    parked: [u64; OCC_WORDS],
    /// Park mirror (lane deskbusy2): the `blocked_on` port, `poll_mask` and
    /// `poll_deadline` each slot's task registered at its LAST park
    /// (`block_on_port_until*`, `widen_port_masks`, `note_timed_wait` — the
    /// only writers of those fields that leave a task parked), packed
    /// densely so the wake and deadline scans filter the ~600 parked tasks
    /// of a desktop without dereferencing each boxed `Task` (a cache miss
    /// apiece, under RUN_QUEUE, thousands of scans a second). The mirror
    /// may be stale only towards "still parked" (wakes do not clear it), so
    /// it only ever skips a slot whose task cannot match; every candidate
    /// is still checked against the task itself.
    pk_port: [u32; MAX_TASKS],
    pk_mask: [u64; MAX_TASKS],
    pk_deadline: [u64; MAX_TASKS],
    /// Poll-wake listeners (lane epollready, see `crate::PollListen`): bit
    /// `i` set while `tasks[i].listen.on`, and `ls_mask[i]` mirrors its filter
    /// mask. The wake scans walk `parked | listening`, so a listener's wakes
    /// are recorded while it runs, too. Cleared by `listen_end`, by the keyed
    /// poll park that replaces the filter, and with the slot.
    listening: [u64; OCC_WORDS],
    ls_mask: [u64; MAX_TASKS],
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
    /// Candidates the last `pick_next` saw (the picked one included); lane
    /// percpu's sync wake pays a dropped kick when others were waiting.
    pub last_pick_cands: usize,
    /// Phase 2 (lane percpu): one ready queue per CPU, all under RUN_QUEUE
    /// for now (see "Phase 2" above `cq_insert`).
    cq: [CpuQueue; crate::MAX_CPUS],
    /// `SCHED[i].gen` when slot `i` was queued (Q3).
    q_gen: [u32; MAX_TASKS],
    /// Hog value slot `i` contributed to its queue's `hog_q` when queued.
    q_hog: [u16; MAX_TASKS],
}

/// One CPU's ready queue: the queued slots (bitmap), their count, the
/// queue's monotonic minimum vruntime, and the slot running there (+1).
pub struct CpuQueue {
    queued: [u64; OCC_WORDS],
    nr: u32,
    min_vr: u64,
    curr: u32,
    /// Sum of the queued tasks' hog contributions (`q_hog`), and the hog of
    /// the task running here: the load the balance compares.
    hog_q: u32,
    hog_curr: u32,
}

impl CpuQueue {
    const fn new() -> Self { Self { queued: [0; OCC_WORDS], nr: 0, min_vr: 0, curr: 0, hog_q: 0, hog_curr: 0 } }
}

impl RunQueue {
    pub const fn new() -> Self {
        Self {
            tasks: [const { None }; MAX_TASKS],
            len: 0,
            nproc: 0,
            occupied: [0; OCC_WORDS],
            parked: [0; OCC_WORDS],
            pk_port: [u32::MAX; MAX_TASKS],
            pk_mask: [0; MAX_TASKS],
            pk_deadline: [u64::MAX; MAX_TASKS],
            listening: [0; OCC_WORDS],
            ls_mask: [0; MAX_TASKS],
            last_full_scan: 0,
            pid_index: [0; PID_INDEX_SLOTS],
            cand: [0; MAX_TASKS],
            last_pick_cands: 0,
            cq: [const { CpuQueue::new() }; crate::MAX_CPUS],
            q_gen: [0; MAX_TASKS],
            q_hog: [0; MAX_TASKS],
        }
    }

    /// Snapshot iterator over the occupied slots — the replacement for every
    /// `for i in 0..MAX_TASKS { rq.get(i) ... }` walk. See [`LiveSlots`].
    #[inline]
    pub fn live_slots(&self) -> LiveSlots {
        LiveSlots { bits: self.occupied, w: 0, cur: self.occupied[0] }
    }

    /// Snapshot iterator over the `parked` candidate set (see the field).
    #[inline]
    fn parked_slots(&self) -> LiveSlots {
        let mut bits = [0u64; OCC_WORDS];
        for w in 0..OCC_WORDS { bits[w] = self.parked[w] & self.occupied[w]; }
        LiveSlots { bits, w: 0, cur: bits[0] }
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
        // Phase 2: a new task starts on the least-loaded CPU's queue.
        let target = self.least_loaded();
        task.place(self.cq[target].min_vr);
        // Publish pid → tgid before the task becomes reachable, so no
        // CPU can pick it up and then miss in the side table. Safe to
        // do under this lock: the table is only ever written from here
        // and from `remove`, both of which hold it.
        crate::pid_tgid_insert(task.pid, task.tgid);
        let pid = task.pid;
        if task.pid == task.tgid { self.nproc += 1; }
        crate::slot::on_insert(i, &mut task);
        self.tasks[i] = Some(task);
        self.pk_port[i] = u32::MAX;
        self.pk_mask[i] = 0;
        self.pk_deadline[i] = u64::MAX;
        self.listening[i / 64] &= !(1u64 << (i % 64));
        self.ls_mask[i] = 0;
        self.occupied[i / 64] |= 1u64 << (i % 64);
        crate::slot::SCHED[i].cpu.store(target as u32, Ordering::Relaxed);
        self.cq_insert(target, i);
        self.index_insert(pid, i);
        self.len += 1;
        crate::percpu::pexit_bump();
        true
    }

    /// Thread-group leaders in the table: live processes plus zombie leaders
    /// whose slot is not recycled yet. Maintained by `enqueue`/`remove` and
    /// `note_leader_promoted`, so the fork limit check is O(1).
    pub fn process_count(&self) -> usize { self.nproc }

    /// `take_over_leader` promoted a thread to leader in place (its leader
    /// was already gone): one more task now has `pid == tgid`.
    pub fn note_leader_promoted(&mut self) { self.nproc += 1; }

    // ── Phase 2 (lane percpu): per-CPU CpuQueues, still under RUN_QUEUE ─────
    //
    // Invariants (checked once per tick by `check_queues`, counted in
    // `/proc/rqhold`, all must stay 0):
    //   Q1  a slot is in at most one CpuQueue, and `SCHED[i].on_rq` names it
    //       (`rq_bad`);
    //   Q2  every Ready task with no `on_cpu` claim is queued or has a pending
    //       wake (`rqmiss`, healed by enqueueing it);
    //   Q3  a queued slot's `q_gen` equals its `SCHED[i].gen` (`gen_miss`).
    // Entries may go STALE (the task blocked, stopped or died after it was
    // queued, without a dequeue): pick and steal drop them lazily (`stale`);
    // a stale entry whose task becomes Ready again is simply valid again.
    // Ready ↔ Running happens only at dispatch/switch-back on the owning CPU.

    #[inline]
    fn home_of(idx: usize) -> usize {
        (crate::slot::SCHED[idx].cpu.load(Ordering::Relaxed) as usize).min(crate::MAX_CPUS - 1)
    }

    /// Put slot `idx` on CPU `c`'s queue (no-op if it is queued anywhere).
    fn cq_insert(&mut self, c: usize, idx: usize) {
        let s = &crate::slot::SCHED[idx];
        if s.on_rq.load(Ordering::Relaxed) != 0 { return; }
        let q = &mut self.cq[c];
        q.queued[idx / 64] |= 1u64 << (idx % 64);
        q.nr += 1;
        crate::slot::NR_QUEUED[c].store(q.nr, Ordering::Relaxed);
        s.on_rq.store(c as u32 + 1, Ordering::Relaxed);
        s.cpu.store(c as u32, Ordering::Relaxed);
        self.q_gen[idx] = s.gen.load(Ordering::Relaxed);
        let h = crate::slot::hog(idx);
        self.q_hog[idx] = h as u16;
        self.cq[c].hog_q += h;
    }

    /// Take slot `idx` off whatever queue holds it.
    fn cq_remove(&mut self, idx: usize) {
        let s = &crate::slot::SCHED[idx];
        let r = s.on_rq.load(Ordering::Relaxed);
        if r == 0 { return; }
        let c = (r - 1) as usize;
        let q = &mut self.cq[c];
        let b = 1u64 << (idx % 64);
        if q.queued[idx / 64] & b != 0 {
            q.queued[idx / 64] &= !b;
            q.nr -= 1;
            q.hog_q = q.hog_q.saturating_sub(self.q_hog[idx] as u32);
            crate::slot::NR_QUEUED[c].store(q.nr, Ordering::Relaxed);
        } else {
            crate::slot::RQ_BAD.fetch_add(1, Ordering::Relaxed);
        }
        s.on_rq.store(0, Ordering::Relaxed);
    }

    /// CPU to queue a woken task on: its home CPU if that is idle with an
    /// empty queue; else an idle CPU with an empty queue, preferring one
    /// whose whole core is idle (SMT, as `wake_up_an_idle_cpu`); else home.
    fn select_cpu(&self, home: usize) -> usize {
        let n = crate::active_cpus();
        let home = if home < n { home } else { 0 };
        let idle = |c: usize| crate::cpu_is_idle(c) && self.cq[c].nr == 0;
        if idle(home) { return home; }
        let mut fallback = None;
        for c in 0..n {
            if !idle(c) { continue; }
            if crate::core_idle_except(c) { return c; }
            if fallback.is_none() { fallback = Some(c); }
        }
        fallback.unwrap_or(home)
    }

    /// Least-loaded CPU (queued + running) for a new task; ties → lowest.
    fn least_loaded(&self) -> usize {
        let n = crate::active_cpus();
        let mut best = (u32::MAX, 0usize);
        for c in 0..n {
            let load = self.cq[c].nr + (self.cq[c].curr != 0) as u32;
            if load < best.0 { best = (load, c); }
        }
        best.1
    }

    /// A woken (Ready) task gets its queue: placed relative to the target
    /// queue's `min_vr` (its lag against its old home carried over), queued.
    /// A task still on a CPU is left to its switch-back, which queues it.
    pub fn make_ready_on(&mut self, idx: usize, prefer: Option<usize>) {
        let home = Self::home_of(idx);
        let r = crate::slot::SCHED[idx].on_rq.load(Ordering::Relaxed);
        let Some(t) = self.tasks[idx].as_deref() else { return };
        if t.state != TaskState::Ready { return; }
        if t.on_cpu.is_some() || r != 0 {
            // On a CPU (switch-back queues it) or a stale entry revived:
            // just re-place against the queue it belongs to.
            let c = if r != 0 { (r - 1) as usize } else { home };
            let m = self.cq[c].min_vr;
            if let Some(t) = self.tasks[idx].as_deref_mut() { t.place(m); }
            return;
        }
        let target = match prefer { Some(c) if c < crate::active_cpus() => c, _ => self.select_cpu(home) };
        let (src, dst) = (self.cq[home].min_vr, self.cq[target].min_vr);
        if let Some(t) = self.tasks[idx].as_deref_mut() {
            if target != home { t.vruntime = dst + t.vruntime.saturating_sub(src); }
            t.place(dst);
        }
        self.cq_insert(target, idx);
    }

    /// Queue every task made Ready since the last drain (see
    /// `slot::drain_woken`). `sync`: the waker's CPU, for a single-wakee
    /// hand-off whose waker is expected to block (sync wake).
    pub fn flush_woken_sync(&mut self, sync: Option<usize>) {
        let mut list = [0u16; 32];
        let mut n = 0usize;
        let mut more = false;
        crate::slot::drain_woken(|i| {
            if n < list.len() { list[n] = i as u16; n += 1; } else { more = true; crate::slot::requeue_woken(i); }
        });
        let prefer = if n == 1 && !more { sync } else { None };
        for k in 0..n { self.make_ready_on(list[k] as usize, prefer); }
        if more { self.flush_woken_sync(None); }
    }

    #[inline]
    pub fn flush_woken(&mut self) { self.flush_woken_sync(None); }

    /// Re-place a task (renice) against its own queue's `min_vr`.
    pub fn place_home(&mut self, idx: usize) {
        let r = crate::slot::SCHED[idx].on_rq.load(Ordering::Relaxed);
        let c = if r != 0 { (r - 1) as usize } else { Self::home_of(idx) };
        let m = self.cq[c].min_vr;
        if let Some(t) = self.tasks[idx].as_deref_mut() { t.place(m); }
    }

    /// EEVDF over CPU `c`'s queue: the eligible candidate with the earliest
    /// virtual deadline. Drops stale entries; skips quiesce-filtered tasks
    /// (they stay queued). Advances `min_vr` (monotonic) over the queued
    /// Ready tasks and the task running there. Returns (slot, candidates).
    fn eevdf(&mut self, c: usize) -> (Option<usize>, usize) {
        let mut n = 0usize;
        let mut sum_w: u64 = 0;
        let mut sum_wv: u128 = 0;
        let mut vmin = u64::MAX;
        for w in 0..OCC_WORDS {
            let mut bits = self.cq[c].queued[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let ok = match self.tasks[i].as_deref() {
                    Some(t) => t.state == TaskState::Ready && t.on_cpu.is_none(),
                    None => false,
                };
                if !ok || self.q_gen[i] != crate::slot::SCHED[i].gen.load(Ordering::Relaxed) {
                    if ok { crate::slot::GEN_MISS.fetch_add(1, Ordering::Relaxed); }
                    crate::slot::STALE.fetch_add(1, Ordering::Relaxed);
                    self.cq_remove(i);
                    continue;
                }
                let t = self.tasks[i].as_deref().unwrap();
                if t.vruntime < vmin { vmin = t.vruntime; }
                if super::quiesce_filtered(t.tgid, t.pid) { continue; }
                sum_w  += t.weight as u64;
                sum_wv += t.weight as u128 * t.vruntime as u128;
                self.cand[n] = i as u16;
                n += 1;
            }
        }
        let cur = self.cq[c].curr;
        if cur != 0 {
            if let Some(t) = self.tasks[cur as usize - 1].as_deref() {
                if t.vruntime < vmin { vmin = t.vruntime; }
            }
        }
        if vmin != u64::MAX && vmin > self.cq[c].min_vr { self.cq[c].min_vr = vmin; }
        if sum_w == 0 { return (None, 0); }
        let mut best: Option<(usize, u64)> = None;
        let mut fallback: Option<(usize, u64)> = None;
        for k in 0..n {
            let i = self.cand[k] as usize;
            if let Some(t) = self.tasks[i].as_deref() {
                let eligible = (t.vruntime as u128) * (sum_w as u128) <= sum_wv;
                if eligible && best.map_or(true, |(_, d)| t.vdeadline < d) { best = Some((i, t.vdeadline)); }
                if fallback.map_or(true, |(_, v)| t.vruntime < v) { fallback = Some((i, t.vruntime)); }
            }
        }
        (best.or(fallback).map(|(i, _)| i), n)
    }

    /// Move a queued task from `from`'s queue to `to`'s, renormalising its
    /// vruntime by its lag against `from`'s `min_vr`.
    fn migrate(&mut self, idx: usize, from: usize, to: usize) {
        self.cq_remove(idx);
        let (src, dst) = (self.cq[from].min_vr, self.cq[to].min_vr);
        if let Some(t) = self.tasks[idx].as_deref_mut() {
            t.vruntime = dst + t.vruntime.saturating_sub(src);
        }
        self.cq_insert(to, idx);
    }

    /// Busiest other CPU with something queued, by LOAD = queued + the
    /// task running there (lock-free peek of the queue length), with its
    /// load. Counting the running task matters: a CPU with one spinner and
    /// an empty queue must pull from one with a spinner and two queued
    /// (loads 1 vs 3), which queue lengths alone (0 vs 2, the caller's own
    /// spinner just requeued: 1 vs 2) never trigger — schedstress `fair`
    /// measured ±83 % that way.
    fn busiest(&self, me: usize) -> Option<(usize, u32)> {
        let n = crate::active_cpus();
        let mut best: Option<(usize, u32)> = None;
        for c in 0..n {
            if c == me { continue; }
            let nr = crate::slot::NR_QUEUED[c].load(Ordering::Relaxed);
            if nr == 0 { continue; }
            let load = nr + (self.cq[c].curr != 0) as u32;
            if best.map_or(true, |(_, b)| load > b) { best = Some((c, load)); }
        }
        best
    }

    /// Hog balance (see `slot::note_run`): if some CPU's hog load (queued
    /// + running) exceeds ours (our queue; our task was just requeued) by
    /// 1.5 hogs (in effect: two), move its most hog-like pickable queued task here.
    fn pulled_hog(&mut self, me: usize) -> bool {
        let n = crate::active_cpus();
        let mine = self.cq[me].hog_q;
        let mut best: Option<(usize, u32)> = None;
        for c in 0..n {
            if c == me || self.cq[c].nr == 0 { continue; }
            let load = self.cq[c].hog_q + self.cq[c].hog_curr;
            // 1.5 hogs, not 2: a hog's EWMA settles just under HOG_FULL
            // (253), so "3 hogs vs 1" is 759 vs 253 + 512 and never fired.
            if load >= mine + 3 * crate::slot::HOG_FULL / 2 && best.map_or(true, |(_, b)| load > b) {
                best = Some((c, load));
            }
        }
        let Some((victim, _)) = best else { return false };
        match self.queued_by_hog(victim, true) {
            Some((i, h)) if h as u32 >= crate::slot::HOG_FULL / 2 => {
                self.migrate(i, victim, me);
                crate::slot::HOG_PULLS.fetch_add(1, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// The pickable queued task of CPU `c` with the highest (`most`) or
    /// lowest hog contribution, with that value.
    fn queued_by_hog(&self, c: usize, most: bool) -> Option<(usize, u16)> {
        let mut pick: Option<(usize, u16)> = None;
        for w in 0..OCC_WORDS {
            let mut bits = self.cq[c].queued[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let ok = matches!(self.tasks[i].as_deref(), Some(t) if t.state == TaskState::Ready
                    && t.on_cpu.is_none() && !super::quiesce_filtered(t.tgid, t.pid));
                let h = self.q_hog[i];
                if ok && pick.map_or(true, |(_, b)| if most { h > b } else { h < b }) { pick = Some((i, h)); }
            }
        }
        pick
    }

    /// Take one runnable task from `victim`'s queue onto `me`'s.
    fn steal_from(&mut self, victim: usize, me: usize) -> bool {
        let (pick, _) = self.eevdf(victim);
        match pick {
            Some(i) => { self.migrate(i, victim, me); true }
            None => false,
        }
    }

    /// Pick the next task for CPU `me` and claim it off the queues.
    ///
    /// Order: drain pending wakes (safety net), pull one task when another
    /// queue is at least 2 longer than ours (keeps busy CPUs fair; the
    /// spec's phase-4 tick balance, done at every pick while the lock is
    /// global), EEVDF over our own queue, else steal from the busiest.
    /// Once per tick the pick also runs the full-table checker.
    pub fn pick_next(&mut self, me: usize) -> Option<usize> {
        let t0 = crate::lockwatch::pick_clock();
        let now = crate::ticks();
        let full = now != self.last_full_scan;
        if full { self.last_full_scan = now; self.check_queues(me); }
        self.flush_woken();
        if self.len == 0 { return None; }

        // Our own load here is just our queue: the task we ran was requeued
        // by the switch-back (or is gone).
        if let Some((c, load)) = self.busiest(me) {
            if load >= self.cq[me].nr + 2 {
                // The least hog-like task: queue length is a latency
                // matter; moving CPU hogs is the hog balance's job (the two
                // would otherwise undo each other).
                if let Some((i, h)) = self.queued_by_hog(c, false) {
                    if (h as u32) < crate::slot::HOG_FULL / 2 {
                        self.migrate(i, c, me);
                        crate::slot::PULLS.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        // Hog balance: a CPU running two or more CPU hogs more than us gives
        // us one (its most hog-like queued task).
        self.pulled_hog(me);
        let (mut pick, n) = self.eevdf(me);
        self.last_pick_cands = n;
        if pick.is_none() {
            // New-idle steal: the busiest queue first, then any other.
            let first = self.busiest(me).map(|x| x.0);
            let ncpu = crate::active_cpus();
            for k in 0..=ncpu {
                let c = if k == 0 { match first { Some(c) => c, None => break } } else { k - 1 };
                if c == me || (k > 0 && Some(c) == first) || self.cq[c].nr == 0 { continue; }
                if self.steal_from(c, me) {
                    crate::slot::STEALS.fetch_add(1, Ordering::Relaxed);
                    pick = self.eevdf(me).0;
                    break;
                }
            }
        }
        if let Some(i) = pick {
            self.cq_remove(i);
            self.cq[me].curr = i as u32 + 1;
            self.cq[me].hog_curr = crate::slot::hog(i);
            crate::slot::SCHED[i].cpu.store(me as u32, Ordering::Relaxed);
        }
        crate::lockwatch::note_pick(full, n as u32, crate::lockwatch::pick_clock().wrapping_sub(t0));
        pick
    }

    /// Switch-back of slot `idx` on CPU `me`, after its final state is
    /// resolved: no longer current there; queued on `me` if still Ready.
    pub fn switch_back(&mut self, me: usize, idx: usize) {
        if self.cq[me].curr == idx as u32 + 1 { self.cq[me].curr = 0; self.cq[me].hog_curr = 0; }
        let ready = matches!(self.tasks[idx].as_deref(), Some(t) if t.state == TaskState::Ready && t.on_cpu.is_none());
        if ready { self.cq_insert(me, idx); }
    }

    /// Once-per-tick checker over every live task (see Q1–Q3 above) plus the
    /// old full-scan duties: park-hint repair and the slot checks (state
    /// mirror, hung-ready watchdog).
    fn check_queues(&mut self, _me: usize) {
        let scan_ns = crate::monotonic_ns();
        let idle_cpu = crate::some_other_cpu_idle();
        let mut queued_total = [0u32; crate::MAX_CPUS];
        let mut miss = [0u16; 16];
        let mut nmiss = 0usize;
        for w in 0..OCC_WORDS {
            let mut bits = self.occupied[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let bit = 1u64 << (i % 64);
                let t = match &self.tasks[i] { Some(t) => t, None => continue };
                crate::slot::scan_check(i, t, scan_ns, idle_cpu, super::quiesce_filtered(t.tgid, t.pid));
                if self.parked[w] & bit == 0 && wake_scannable(t) {
                    self.parked[w] |= bit;
                    crate::lockwatch::note_ready_hint_miss();
                }
                let r = crate::slot::SCHED[i].on_rq.load(Ordering::Relaxed);
                if r != 0 {
                    let c = (r - 1) as usize;
                    if c >= crate::MAX_CPUS || self.cq[c].queued[w] & bit == 0 {
                        crate::slot::RQ_BAD.fetch_add(1, Ordering::Relaxed);
                    } else {
                        queued_total[c] += 1;
                    }
                } else if t.state == TaskState::Ready && t.on_cpu.is_none()
                    && !crate::slot::woken_pending(i)
                {
                    crate::slot::RQMISS.fetch_add(1, Ordering::Relaxed);
                    crate::slot::RQMISS_PID.store(t.pid, Ordering::Relaxed);
                    if nmiss < miss.len() { miss[nmiss] = i as u16; nmiss += 1; }
                }
            }
        }
        for c in 0..crate::MAX_CPUS {
            if queued_total[c] != self.cq[c].nr { crate::slot::RQ_BAD.fetch_add(1, Ordering::Relaxed); }
        }
        // Debug snapshot for /proc/rqhold (balance tuning).
        let mut hist = [0u32; 4];
        for w in 0..OCC_WORDS {
            let mut bits = self.occupied[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if matches!(self.tasks[i].as_deref(), Some(t) if matches!(t.state, TaskState::Ready | TaskState::Running)) {
                    hist[(crate::slot::hog(i) / 64).min(3) as usize] += 1;
                }
            }
        }
        for k in 0..4 { crate::slot::HOG_HIST[k].store(hist[k], Ordering::Relaxed); }
        for c in 0..crate::active_cpus().min(8) {
            let q = &self.cq[c];
            crate::slot::CQ_SNAP[c].store(((q.nr as u64) << 48) | (((q.hog_q as u64) & 0xFF_FFFF) << 24)
                | ((q.hog_curr as u64) & 0xFF_FFFF) | (1u64 << 63) * 0, Ordering::Relaxed);
        }
        // Heal: queue the missed tasks (the counter already recorded them).
        for k in 0..nmiss { self.make_ready_on(miss[k] as usize, None); }
    }

    /// Kick every idle CPU whose queue holds work (after a quiesce ends:
    /// its filtered tasks stayed queued while their CPUs went idle).
    pub fn kick_idle_with_work() {
        for c in 0..crate::active_cpus() {
            if crate::slot::NR_QUEUED[c].load(Ordering::Relaxed) != 0 && crate::cpu_is_idle(c) {
                crate::trigger_preempt(c);
            }
        }
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Task> {
        self.tasks[idx].as_mut().map(|boxed_task| boxed_task.as_mut())
    }

    pub fn get(&self, idx: usize) -> Option<&Task> {
        self.tasks[idx].as_ref().map(|boxed_task| boxed_task.as_ref())
    }

    /// `Task::sig_kick` on every thread of group `tgid`: for writers of
    /// leader-held state every thread's signal check reads (the
    /// process-level pending set, `group_exit_owner`). `&self`: the hint is
    /// atomic and no scheduling state changes.
    pub fn sig_kick_group(&self, tgid: Pid) {
        for i in self.live_slots() {
            if let Some(t) = self.tasks[i].as_deref() {
                if t.tgid == tgid { t.sig_kick(); }
            }
        }
    }

    pub fn find_pid(&self, pid: Pid) -> Option<&Task> {
        let idx = self.find_pid_idx(pid)?;
        self.tasks[idx].as_deref()
    }

    pub fn find_pid_mut(&mut self, pid: Pid) -> Option<&mut Task> {
        let idx = self.find_pid_idx(pid)?;
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
        let Some(idx) = self.find_pid_idx(pid) else { return };
        self.parked[idx / 64] |= 1u64 << (idx % 64);
        self.pk_port[idx] = port;
        self.pk_mask[idx] = mask;
        self.pk_deadline[idx] = deadline;
        if let Some(task) = self.get_mut(idx) {
            task.set_state(TaskState::Blocked);
            task.blocked_on    = Some(port);
            task.poll_deadline = deadline;
            task.poll_mask     = mask;
            task.poll_kbits    = 0;
            task.poll_nfp      = 0;
        }
    }

    /// `block_on_port_until` with the exact-key refinement of `mask` (see
    /// `crate::PollSet`): `kbits` are the bits only keyed interests explain,
    /// `fps` their fingerprints. More fingerprints than fit leave every keyed
    /// bit unrefined (safe: refinement only ever removes wakes).
    pub fn block_on_port_until_keyed(&mut self, pid: Pid, port: u32, deadline: u64,
                                     mask: u64, kbits: u64, fps: &[u16]) {
        let Some(idx) = self.find_pid_idx(pid) else { return };
        // This park overwrites `poll_fps`, which a listener's filter shares:
        // a poll()/select()/other-instance park ends the listener.
        self.listening[idx / 64] &= !(1u64 << (idx % 64));
        self.parked[idx / 64] |= 1u64 << (idx % 64);
        self.pk_port[idx] = port;
        self.pk_mask[idx] = mask;
        self.pk_deadline[idx] = deadline;
        if let Some(task) = self.get_mut(idx) {
            task.listen.on     = false;
            task.listen.ring   = crate::WakeRing::EMPTY;
            task.set_state(TaskState::Blocked);
            task.blocked_on    = Some(port);
            task.poll_deadline = deadline;
            task.poll_mask     = mask;
            if fps.len() <= crate::POLL_FP_MAX && kbits != 0 {
                task.poll_fps[..fps.len()].copy_from_slice(fps);
                task.poll_nfp   = fps.len() as u16;
                task.poll_kbits = kbits & mask;
            } else {
                task.poll_kbits = 0;
                task.poll_nfp   = 0;
            }
        }
    }

    /// Start listening (see `crate::poll_listen_begin`); returns the new id.
    pub fn listen_begin(&mut self, pid: Pid, mask: u64, kbits: u64, fps: &[u16]) -> u32 {
        let Some(idx) = self.find_pid_idx(pid) else { return 0 };
        let mask = if mask == 0 { crate::POLL_TAG_ALL } else { mask };
        self.listening[idx / 64] |= 1u64 << (idx % 64);
        self.ls_mask[idx] = mask;
        match self.tasks[idx].as_deref_mut() {
            Some(t) => {
                Self::listen_install(t, mask, kbits, fps);
                t.listen.on   = true;
                t.listen.id   = t.listen.id.wrapping_add(1);
                t.listen.ring = crate::WakeRing::EMPTY;
                t.listen.id
            }
            None => 0,
        }
    }

    fn listen_install(t: &mut Task, mask: u64, kbits: u64, fps: &[u16]) {
        t.listen.mask = mask;
        if fps.len() <= crate::POLL_FP_MAX && kbits != 0 {
            t.poll_fps[..fps.len()].copy_from_slice(fps);
            t.listen.nfp   = fps.len() as u16;
            t.listen.kbits = kbits & mask;
        } else {
            t.listen.nfp   = 0;
            t.listen.kbits = 0;
        }
    }

    fn listener(&self, pid: Pid, id: u32) -> Option<usize> {
        let idx = self.find_pid_idx(pid)?;
        let t = self.tasks[idx].as_deref()?;
        if t.listen.on && t.listen.id == id && self.listening[idx / 64] & (1u64 << (idx % 64)) != 0 {
            Some(idx)
        } else { None }
    }

    /// See `crate::poll_listen_update`.
    pub fn listen_update(&mut self, pid: Pid, id: u32, mask: u64, kbits: u64, fps: &[u16])
        -> Option<crate::WakeRing>
    {
        let idx = self.listener(pid, id)?;
        let mask = if mask == 0 { crate::POLL_TAG_ALL } else { mask };
        self.ls_mask[idx] = mask;
        let t = self.tasks[idx].as_deref_mut()?;
        Self::listen_install(t, mask, kbits, fps);
        Some(core::mem::replace(&mut t.listen.ring, crate::WakeRing::EMPTY))
    }

    /// See `crate::poll_listen_take`.
    pub fn listen_take(&mut self, pid: Pid, id: u32) -> Option<crate::WakeRing> {
        let idx = self.listener(pid, id)?;
        let t = self.tasks[idx].as_deref_mut()?;
        Some(core::mem::replace(&mut t.listen.ring, crate::WakeRing::EMPTY))
    }

    /// See `crate::poll_listen_park`: park with the listener's filter iff its
    /// ring is empty, in this one hold.
    pub fn listen_park(&mut self, pid: Pid, id: u32, deadline: u64) -> crate::ListenPark {
        let Some(idx) = self.listener(pid, id) else { return crate::ListenPark::Off };
        let port = crate::POLL_WAIT_CHANNEL;
        let mask = self.ls_mask[idx];
        {
            let Some(t) = self.tasks[idx].as_deref_mut() else { return crate::ListenPark::Off };
            if !t.listen.ring.is_empty() {
                return crate::ListenPark::Taken(core::mem::replace(&mut t.listen.ring, crate::WakeRing::EMPTY));
            }
        }
        self.parked[idx / 64] |= 1u64 << (idx % 64);
        self.pk_port[idx] = port;
        self.pk_mask[idx] = mask;
        self.pk_deadline[idx] = deadline;
        if let Some(t) = self.get_mut(idx) {
            t.set_state(TaskState::Blocked);
            t.blocked_on    = Some(port);
            t.poll_deadline = deadline;
            t.poll_mask     = mask;
            t.poll_kbits    = t.listen.kbits;
            t.poll_nfp      = t.listen.nfp;
        }
        crate::ListenPark::Parked
    }

    /// See `crate::poll_listen_end`.
    pub fn listen_end(&mut self, pid: Pid) {
        let Some(idx) = self.find_pid_idx(pid) else { return };
        self.listening[idx / 64] &= !(1u64 << (idx % 64));
        if let Some(t) = self.tasks[idx].as_deref_mut() {
            t.listen.on   = false;
            t.listen.ring = crate::WakeRing::EMPTY;
        }
    }

    /// Does a wake (`tag`; for a keyed wake `tag == poll_key_bit(key)`) pass
    /// `t`'s listener filter? The park filter's rule (see
    /// `unblock_port_tagged_inner`), on the listener's own mask and kbits.
    #[inline]
    fn listen_match(t: &Task, tag: u64, exclusive: bool, key: Option<u64>) -> bool {
        let l = &t.listen;
        if !l.on || (l.mask & tag == 0 && tag != crate::POLL_TAG_ALL) { return false; }
        if exclusive && l.mask == crate::POLL_TAG_ALL { return false; }
        match key {
            Some(k) if l.kbits & tag != 0 => {
                let fp = crate::poll_key_fp(k);
                t.poll_fps[..l.nfp as usize].iter().any(|&f| f == fp)
            }
            _ => true,
        }
    }

    /// Record a wake that passed `listen_match` in `t`'s ring.
    #[inline]
    fn listen_record(t: &mut Task, tag: u64, key: Option<u64>) {
        match key {
            Some(k) => t.listen.ring.push_key(k),
            None => {
                let m = if tag == crate::POLL_TAG_ALL { tag } else { tag & t.listen.mask };
                t.listen.ring.push_tags(m);
            }
        }
    }

    /// Record the timerfd expiries of one deadline pass that `t`'s listener
    /// filter matches: their exact keys when the pass has them all, else
    /// their bits.
    fn listen_record_tfd(t: &mut Task, timerfd_tags: u64, keys: Option<&[u64]>) {
        if !t.listen.on { return; }
        let hit = t.listen.mask & timerfd_tags;
        if hit == 0 { return; }
        match keys {
            Some(ks) => {
                for &k in ks {
                    let b = crate::poll_key_bit(k);
                    if b & hit == 0 { continue; }
                    if b & t.listen.kbits != 0
                        && !t.poll_fps[..t.listen.nfp as usize].contains(&crate::poll_key_fp(k)) { continue; }
                    t.listen.ring.push_key(k);
                }
            }
            None => t.listen.ring.push_tags(if timerfd_tags == crate::POLL_TAG_ALL { timerfd_tags } else { hit }),
        }
    }

    /// `pid` parked in a timed futex wait (`poll_deadline` set, not on a
    /// port): the deadline tick must find it. See `parked`.
    pub fn note_timed_wait(&mut self, pid: Pid) {
        if let Some(idx) = self.find_pid_idx(pid) {
            self.parked[idx / 64] |= 1u64 << (idx % 64);
            if let Some(t) = self.tasks[idx].as_deref() { self.pk_deadline[idx] = t.poll_deadline; }
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
        self.unblock_port_tagged_inner(port, tag, false, None)
    }

    /// Keyed wake (see `crate::wake_poll_key`): like `unblock_port_tagged`
    /// with `poll_key_bit(key)`, except that a task whose `poll_kbits` holds
    /// that bit is woken only if one of its fingerprints is the key's.
    pub fn unblock_port_keyed(&mut self, port: u32, key: u64) -> usize {
        self.unblock_port_tagged_inner(port, crate::poll_key_bit(key), false, Some(key))
    }

    /// `unblock_port_tagged` that does NOT reach broadcast-mask
    /// (`POLL_TAG_ALL`) waiters: only tasks whose own narrow mask names `tag`.
    /// Sound only for a tag no broadcast-mask park can be waiting on — see
    /// `crate::try_wake_poll_exclusive`.
    pub fn unblock_port_exclusive(&mut self, port: u32, tag: u64) -> usize {
        self.unblock_port_tagged_inner(port, tag, true, None)
    }

    /// `unblock_port_exclusive` with the keyed refinement of `unblock_port_keyed`.
    pub fn unblock_port_exclusive_keyed(&mut self, port: u32, key: u64) -> usize {
        self.unblock_port_tagged_inner(port, crate::poll_key_bit(key), true, Some(key))
    }

    fn unblock_port_tagged_inner(&mut self, port: u32, tag: u64, exclusive: bool,
                                 key: Option<u64>) -> usize {
        let mut woken = 0;
        let mut allmask = 0u32;
        let poll_port = port == crate::POLL_WAIT_CHANNEL;
        for w in 0..OCC_WORDS {
          let lwords = if poll_port { self.listening[w] } else { 0 };
          let mut bits = (self.parked[w] | lwords) & self.occupied[w];
          while bits != 0 {
            let i = w * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let b = 1u64 << (i % 64);
            // Park-mirror prefilter: a task parked on `port` registered it
            // (and its mask, or a superset after widening) here.
            let pm = self.parked[w] & b != 0 && self.pk_port[i] == port
                && (self.pk_mask[i] & tag != 0 || tag == crate::POLL_TAG_ALL);
            let lm = lwords & b != 0
                && (self.ls_mask[i] & tag != 0 || tag == crate::POLL_TAG_ALL);
            if !pm && !lm { continue; }
            if let Some(task) = self.tasks[i].as_deref_mut() {
                // A listener records the wake whatever its state (parked
                // here, running, or blocked elsewhere): see `PollListen`.
                if lm && Self::listen_match(task, tag, exclusive, key) {
                    Self::listen_record(task, tag, key);
                }
                if !pm { continue; }
                if !wake_scannable(task) { self.parked[w] &= !b; continue; }
                if task.blocked_on == Some(port) && task.state == TaskState::Blocked
                    && ((task.poll_mask & tag) != 0 || tag == crate::POLL_TAG_ALL)
                    && !(exclusive && task.poll_mask == crate::POLL_TAG_ALL)
                    && match key {
                        // Keyed wake on a bit only keyed interests explain:
                        // deliver on a fingerprint match only.
                        Some(k) if task.poll_kbits & tag != 0 => {
                            let fp = crate::poll_key_fp(k);
                            let hit = task.poll_fps[..task.poll_nfp as usize].iter().any(|&f| f == fp);
                            let c = if hit { &crate::wakestat::KEY_FP_HIT } else { &crate::wakestat::KEY_SKIPPED };
                            c.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                            hit
                        }
                        Some(_) => {
                            crate::wakestat::KEY_UNREFINED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                            true
                        }
                        None => true,
                    } {
                    if task.poll_mask == crate::POLL_TAG_ALL { allmask += 1; }
                    if crate::gdwake::enabled() {
                        crate::gdwake::mark(task.pid, crate::gdwake::SITE.load(core::sync::atomic::Ordering::Relaxed));
                    }
                    task.set_state(TaskState::Ready);
                    task.blocked_on    = None;
                    task.poll_deadline = u64::MAX;
                    task.poll_mask     = crate::POLL_TAG_ALL;
                    woken += 1;
                }
            }
          }
        }
        if crate::gdwake::enabled() {
            crate::gdwake::SITE.store(crate::gdwake::UNATTRIBUTED, core::sync::atomic::Ordering::Relaxed);
        }
        crate::wakestat::LAST_ALLMASK.store(allmask, core::sync::atomic::Ordering::Relaxed);
        if woken > 0 { self.flush_woken(); }
        woken
    }

    /// Wake `pid` if it is parked on the kernel wait channel
    /// (`crate::KWAIT_CHANNEL`, used by `ksync`). Returns true if it was
    /// made Ready. A task that is not parked there (still running between
    /// prepare and commit is impossible: prepare publishes Blocked first) is
    /// left alone; its waiter loop re-checks its condition anyway.
    pub fn wake_kwaiter(&mut self, pid: Pid) -> bool {
        match self.find_pid_mut(pid) {
            Some(task) if task.state == TaskState::Blocked
                && task.blocked_on == Some(crate::KWAIT_CHANNEL) => {
                task.set_state(TaskState::Ready);
                task.blocked_on    = None;
                task.poll_deadline = u64::MAX;
                task.poll_mask     = crate::POLL_TAG_ALL;
                self.flush_woken();
                true
            }
            _ => false,
        }
    }

    /// Wake every task parked on the kernel wait channel (the fallback when a
    /// deferred-wake record overflowed: spurious wakes are harmless, every
    /// `ksync` waiter loops on its condition).
    pub fn wake_all_kwaiters(&mut self) -> usize {
        let mut woken = 0;
        for i in self.parked_slots() {
            if let Some(task) = self.tasks[i].as_deref_mut() {
                if task.state == TaskState::Blocked && task.blocked_on == Some(crate::KWAIT_CHANNEL) {
                    task.set_state(TaskState::Ready);
                    task.blocked_on    = None;
                    task.poll_deadline = u64::MAX;
                    task.poll_mask     = crate::POLL_TAG_ALL;
                    woken += 1;
                }
            }
        }
        if woken > 0 { self.flush_woken(); }
        woken
    }

    /// See `crate::widen_poll_masks`: OR `add` into the `poll_mask` of every
    /// task Blocked on `port` whose mask intersects `match_tag`.
    ///
    /// A poll-wake LISTENER's filter (`ls_mask`, `Task::listen`) is NOT
    /// widened. A park mask is rebuilt from scratch at every park, so a
    /// widening lasts one sleep; a listener's filter is rebuilt only when one
    /// of its answers changes, so a widening stuck to it for good. `match_tag`
    /// is an instance's self tag, a bloom bit the filters of unrelated
    /// instances share, and relibc's poll() is an epoll instance with an ADD
    /// per fd per call: every ADD of a broadcast-tagged fd anywhere turned
    /// such listeners into `POLL_TAG_ALL` listeners, woken by every wake in
    /// the system and re-probing everything each time (desktop A/B
    /// 2026-10-17: idle COSMIC clients at 0.1-1 s of kernel CPU per 12 s,
    /// ~10x the woken count of the legacy loop).
    /// Nothing is lost without it: the instance's own listeners re-read the
    /// interest list at every pass (the ADD bumped `EPOLL_CTL_GEN` before
    /// this, which a parking listener re-checks after parking), and a parked
    /// one is widened below like any parked task, with `add` noted in its
    /// ring so the wake the widening lets through re-probes only what it can
    /// concern (the new interest is unprobed, so it always is).
    pub fn widen_port_masks(&mut self, port: u32, match_tag: u64, add: u64) {
        for i in self.parked_slots() {
            if let Some(task) = self.tasks[i].as_deref_mut() {
                if task.blocked_on == Some(port) && task.state == TaskState::Blocked
                    && (task.poll_mask & match_tag) != 0 {
                    task.poll_mask |= add;
                    self.pk_mask[i] |= add;
                    // The added interest's key is unknown here: its bits must
                    // not be refined.
                    task.poll_kbits &= !add;
                    if port == crate::POLL_WAIT_CHANNEL && task.listen.on {
                        task.listen.ring.push_tags(add);
                    }
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
        self.wake_due_poll_deadlines_keyed(port, now, timerfd_tags, None)
    }

    /// `wake_due_poll_deadlines` with the expired timerfds' exact keys
    /// (`Some` = complete): a poller whose matching bits are all refined
    /// (`poll_kbits`) is woken by the expiry only on a fingerprint match.
    pub fn wake_due_poll_deadlines_keyed(&mut self, port: u32, now: u64, timerfd_tags: u64,
                                         keys: Option<&[u64]>) -> (u64, usize)
    {
        let mut new_min = u64::MAX;
        let mut woken = 0;
        for w in 0..OCC_WORDS {
          let lwords = if timerfd_tags != 0 && port == crate::POLL_WAIT_CHANNEL { self.listening[w] } else { 0 };
          let mut bits = (self.parked[w] | lwords) & self.occupied[w];
          while bits != 0 {
            let i = w * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            // A listener records the expiries its filter matches, whatever
            // its state (see `PollListen`).
            if lwords & (1u64 << (i % 64)) != 0 && self.ls_mask[i] & timerfd_tags != 0 {
                if let Some(t) = self.tasks[i].as_deref_mut() {
                    Self::listen_record_tfd(t, timerfd_tags, keys);
                }
            }
            if self.parked[w] & (1u64 << (i % 64)) == 0 { continue; }
            // Park-mirror prefilter: not due and no expired timerfd in its
            // registered mask ⇒ only its deadline matters, and the mirror
            // holds it (a stale mirror deadline is only ever EARLIER-or-equal
            // in effect: it re-wakes the scan, which then looks at the task).
            let pd = self.pk_deadline[i];
            if pd > now && (self.pk_mask[i] & timerfd_tags == 0
                || (self.pk_port[i] != port && self.pk_port[i] != crate::KWAIT_CHANNEL)) {
                if pd < new_min { new_min = pd; }
                continue;
            }
            if let Some(task) = self.tasks[i].as_deref_mut() {
                if !wake_scannable(task) { self.parked[w] &= !(1u64 << (i % 64)); continue; }
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
                let tfd_hit = is_poll && {
                    let hit = task.poll_mask & timerfd_tags;
                    hit != 0 && match keys {
                        Some(ks) if hit & !task.poll_kbits == 0 => {
                            let fps = &task.poll_fps[..task.poll_nfp as usize];
                            ks.iter().any(|&k| crate::poll_key_bit(k) & hit != 0
                                && fps.contains(&crate::poll_key_fp(k)))
                        }
                        _ => true,
                    }
                };
                if tfd_hit || task.poll_deadline <= now {
                    if crate::idlestat::ENABLED {
                        crate::idlestat::note_deadline_wake(
                            task.poll_deadline <= now,
                            is_poll && task.poll_mask == crate::POLL_TAG_ALL);
                    }
                    if crate::gdwake::enabled() {
                        crate::gdwake::mark(task.pid, if task.poll_deadline <= now {
                            crate::gdwake::DL_OWN } else { crate::gdwake::DL_TFD });
                    }
                    if is_poll && task.listen.on && task.poll_deadline <= now {
                        task.listen.ring.flags |= crate::WAKE_DEADLINE;
                    }
                    task.set_state(TaskState::Ready);
                    if is_poll  { task.blocked_on = None; task.poll_mask = crate::POLL_TAG_ALL; }
                    if is_futex { task.blocked_futex = 0;    }
                    task.poll_deadline = u64::MAX;
                    woken += 1;
                } else if task.poll_deadline < new_min {
                    new_min = task.poll_deadline;
                }
            }
          }
        }
        if woken > 0 { self.flush_woken(); }
        (new_min, woken)
    }

    /// Mark a task as Zombie (terminal; will not be scheduled again).
    pub fn mark_zombie(&mut self, pid: Pid) {
        if let Some(task) = self.find_pid_mut(pid) {
            task.set_state(TaskState::Zombie);
        }
    }

    /// Remove the task at `idx` from the run queue and return it so the caller
    /// can free its resources.  Decrements the task count.
    pub fn remove(&mut self, idx: usize) -> Option<Box<Task>> {
        let mut t = self.tasks[idx].take();
        if t.is_some() {
            self.cq_remove(idx);
            crate::slot::clear_woken(idx);
            for c in 0..crate::MAX_CPUS {
                if self.cq[c].curr == idx as u32 + 1 { self.cq[c].curr = 0; self.cq[c].hog_curr = 0; }
            }
        }
        if let Some(task) = t.as_mut() { crate::slot::on_remove(idx, task); }
        if let Some(task) = t.as_ref() {
            self.len = self.len.saturating_sub(1);
            if task.pid == task.tgid { self.nproc = self.nproc.saturating_sub(1); }
            self.occupied[idx / 64] &= !(1u64 << (idx % 64));
            self.parked[idx / 64] &= !(1u64 << (idx % 64));
            self.listening[idx / 64] &= !(1u64 << (idx % 64));
            self.index_remove(task.pid);
            // Retire the side-table entry with the slot. Leaving it would hand
            // a later caller the tgid of a task that no longer exists, where
            // `tgid_of`'s contract is to fall back to the pid itself.
            crate::pid_tgid_remove(task.pid);
            crate::percpu::pexit_bump();
        }
        t
    }
}
