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
//! SMP (lane percpu): this table is the TASKS side, guarded by the
//! `RUN_QUEUE` mutex in lib.rs; the ready sets, the EEVDF pick and the
//! dispatch state are per CPU, in `cpuq.rs` and `slot.rs`.

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

/// 256-bit bloom of a fingerprint set (see `RunQueue::fpb`): bit `fp & 255`
/// (key bits 32..40, independent of the key's bloom-mask bit 0..6).
#[inline]
fn fp_bloom(fps: &[u16]) -> [u64; 4] {
    let mut b = [0u64; 4];
    for &f in fps { let x = (f & 255) as usize; b[x >> 6] |= 1u64 << (x & 63); }
    b
}

#[inline]
fn fp_bloom_has(b: &[u64; 4], fp: u16) -> bool {
    let x = (fp & 255) as usize;
    b[x >> 6] & (1u64 << (x & 63)) != 0
}

/// Wake-scan index mode (`sched.wakeidx`, see `RunQueue::wx_row`): 0 = full
/// scan, 1 = indexed (default), 2 = full scan counting the index's misses.
pub static WAKE_IDX: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(1);
/// A tag wake with more bits than this scans the old way.
const WX_MAX_BITS: u32 = 8;

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
    /// Wake index (lane wakescan). A poll-channel wake scan used to visit
    /// every parked/listening slot and dereference every Task whose mirror
    /// mask had the wake's bit — on the laptop desktop ~600 parked tasks, with
    /// busy reactors' masks covering most of the 64 bits, so a refined keyed
    /// wake that ends up waking one task still paid ~11 µs (a ~380 ns cache
    /// miss per candidate) under RUN_QUEUE.
    ///
    /// `wx_row[b]` holds the slots whose indexed mask (`wx_mask`, see
    /// `row_mask`: the poll-channel park mirror OR the listener filter) has
    /// bit `b`; `wx_all` the slots whose indexed mask is `POLL_TAG_ALL`. A
    /// tagged or keyed wake visits only `wx_all | wx_row[bits of tag]`, and a
    /// keyed wake then skips, without a dereference, every candidate whose
    /// matching bit is refined (`pk_kb` / `ls_kb`, mirrors of
    /// `Task::poll_kbits` / `PollListen::kbits`) and whose fingerprint bloom
    /// (`fpb`, 256 bits over `Task::poll_fps`) lacks the key's fingerprint.
    ///
    /// SAFETY (a lost wake is a hang): every write of `pk_port`, `pk_mask`,
    /// `ls_mask` or a `listening` bit is followed by `wx_sync`, so the rows
    /// are exactly `row_mask` of every slot, a superset of what the full
    /// scan's mirror prefilter accepts. `pk_kb` / `ls_kb` are written next to
    /// the Task fields they mirror (equal), and `fpb` is rebuilt from every
    /// array copied into `poll_fps` (a superset). So the index only drops a
    /// slot the full scan would drop too. `sched.wakeidx=0` scans the old way;
    /// `sched.wakeidx=2` scans the old way and counts every slot the index
    /// would have wrongly dropped (`# wakeidx ... miss`, must stay 0).
    wx_row: [[u64; OCC_WORDS]; 64],
    wx_all: [u64; OCC_WORDS],
    wx_mask: [u64; MAX_TASKS],
    pk_kb: [u64; MAX_TASKS],
    ls_kb: [u64; MAX_TASKS],
    fpb: [[u64; 4]; MAX_TASKS],
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
    /// Queued real-time signal instances beyond each task's one pending
    /// slot (see `crate::rtq`). Guarded by RUN_QUEUE with the bits it
    /// complements.
    pub rtq: crate::rtq::RtQueue,
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
            wx_row: [[0; OCC_WORDS]; 64],
            wx_all: [0; OCC_WORDS],
            wx_mask: [0; MAX_TASKS],
            pk_kb: [0; MAX_TASKS],
            ls_kb: [0; MAX_TASKS],
            fpb: [[0; 4]; MAX_TASKS],
            pid_index: [0; PID_INDEX_SLOTS],
            rtq: crate::rtq::RtQueue::new(),
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
        // Lane percpu: a new task starts on the least-loaded CPU's queue.
        let target = Self::least_loaded(task.affinity);
        // Publish pid → tgid before the task becomes reachable, so no
        // CPU can pick it up and then miss in the side table. Safe to
        // do under this lock: the table is only ever written from here
        // and from `remove`, both of which hold it.
        crate::pid_tgid_insert(task.pid, task.tgid);
        let pid = task.pid;
        if task.pid == task.tgid { self.nproc += 1; }
        crate::slot::on_insert(i, &mut task);
        crate::slot::SCHED[i].aff.store(task.affinity, Ordering::Relaxed);
        if task.state != TaskState::Zombie { crate::uidcount::inc(task.uid); }
        self.tasks[i] = Some(task);
        self.pk_port[i] = u32::MAX;
        self.pk_mask[i] = 0;
        self.pk_deadline[i] = u64::MAX;
        self.listening[i / 64] &= !(1u64 << (i % 64));
        self.ls_mask[i] = 0;
        self.pk_kb[i] = 0;
        self.ls_kb[i] = 0;
        self.wx_sync(i);
        self.occupied[i / 64] |= 1u64 << (i % 64);
        crate::slot::SCHED[i].cpu.store(target as u32, Ordering::Relaxed);
        if crate::cpuq::held_here() != 0 { panic!("[SCHED] enqueue under an RQ lock"); }
        {
            let mut g = crate::cpuq::lock(target);
            crate::slot::place(i, g.place_floor());
            if crate::slot::state_of(i) == crate::slot::state_code(TaskState::Ready) { g.insert(i); }
        }
        if target != unsafe { crate::cpu_id() } && crate::cpu_is_idle(target) {
            crate::trigger_preempt(target);
        }
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

    // ── Lane percpu: the TASKS side of the per-CPU queues (see cpuq.rs) ─────
    //
    // Invariants (checked once per tick by `check_all` under TASKS and every
    // RQ lock, counted in `/proc/rqhold`, all must stay 0):
    //   Q1  a slot is in at most one CpuQueue, and `SCHED[i].on_rq` names it
    //       (`rq_bad`);
    //   Q2  every Ready task with no run-guard claim is queued (`rqmiss`;
    //       healed by re-marking it woken) — no wake is pending placement
    //       while TASKS is free (`woken_late`);
    //   Q3  a queued slot's `q_gen` equals its `SCHED[i].gen` (`gen_miss`).

    /// CPU to queue a woken task on: its home CPU if that is idle with an
    /// empty queue; else an idle CPU with an empty queue, preferring one
    /// whose whole core is idle (SMT, as `wake_up_an_idle_cpu`); else home.
    /// Lock-free peeks.
    fn select_cpu(home: usize, aff: u64) -> usize {
        let n = crate::active_cpus();
        let ok = |c: usize| crate::slot::mask_allows(aff, c);
        let home = if home < n && ok(home) { home } else { (0..n).find(|&c| ok(c)).unwrap_or(0) };
        let idle = |c: usize| crate::cpu_is_idle(c)
            && crate::cpuq::NR_QUEUED[c].load(Ordering::Relaxed) == 0;
        if idle(home) { return home; }
        let mut fallback = None;
        for c in 0..n {
            if !idle(c) || !ok(c) { continue; }
            if crate::core_idle_except(c) { return c; }
            if fallback.is_none() { fallback = Some(c); }
        }
        fallback.unwrap_or(home)
    }

    /// Least-loaded CPU (queued + running) for a new task; ties → lowest.
    fn least_loaded(aff: u64) -> usize {
        let n = crate::active_cpus();
        let mut best = (u32::MAX, 0usize);
        for c in 0..n {
            if !crate::slot::mask_allows(aff, c) { continue; }
            let load = crate::cpuq::NR_QUEUED[c].load(Ordering::Relaxed)
                + (crate::cpuq::CURR[c].load(Ordering::Relaxed) != 0) as u32;
            if load < best.0 { best = (load, c); }
        }
        best.1
    }

    /// A woken (Ready) task gets its queue (rule R3), under TASKS + the
    /// queue's lock. A task still on a CPU (it was woken between its park
    /// and its switch-back) or already queued is only re-placed: its
    /// switch-back queues it. Otherwise it goes to `prefer` (sync wake) or
    /// `select_cpu`; an idle target other than this CPU is noted in `kicks`.
    fn place_woken(&mut self, idx: usize, prefer: Option<usize>, kicks: &mut u64) {
        match self.tasks[idx].as_deref() {
            Some(t) if t.state == TaskState::Ready => {}
            _ => return,
        }
        let s = &crate::slot::SCHED[idx];
        let mut g = crate::cpuq::lock_slot(idx);
        if s.on_cpu.load(Ordering::Acquire) != 0 || s.on_rq.load(Ordering::Relaxed) != 0 {
            crate::slot::place(idx, g.place_floor());
            return;
        }
        // Ready, off-CPU, unqueued: nothing but a TASKS holder (us) can
        // touch it now, so the home lock may go before the target's.
        let home = g.cpu();
        let aff = s.aff.load(Ordering::Relaxed);
        let target = match prefer {
            Some(c) if c < crate::active_cpus() && crate::slot::mask_allows(aff, c) => c,
            _ => Self::select_cpu(home, aff),
        };
        if target != home {
            drop(g);
            g = crate::cpuq::lock(target);
        }
        crate::slot::place(idx, g.place_floor());
        g.insert(idx);
        drop(g);
        if target != unsafe { crate::cpu_id() } && crate::cpu_is_idle(target) {
            *kicks |= 1u64 << target;
        }
    }

    /// Queue every task made Ready since the last drain (see
    /// `slot::drain_woken`), then kick the idle CPUs that got work (lane
    /// percpu phase 4: the wake's own target, not "some idle CPU").
    /// `sync`: the waker's CPU, for a single-wakee hand-off whose waker is
    /// expected to block (sync wake). Runs at the latest when the TASKS
    /// guard is released (`lockwatch`), so every wake is placed in the hold
    /// that made it.
    pub fn flush_woken_sync(&mut self, sync: Option<usize>) {
        if !crate::slot::woken_any() { return; }
        if crate::cpuq::held_here() != 0 { panic!("[SCHED] wake placement under an RQ lock"); }
        let mut kicks = 0u64;
        loop {
            let mut list = [0u16; 32];
            let mut n = 0usize;
            let mut more = false;
            crate::slot::drain_woken(|i| {
                if n < list.len() { list[n] = i as u16; n += 1; } else { more = true; crate::slot::requeue_woken(i); }
            });
            let prefer = if n == 1 && !more { sync } else { None };
            for k in 0..n { self.place_woken(list[k] as usize, prefer, &mut kicks); }
            if !more { break; }
        }
        while kicks != 0 {
            let c = kicks.trailing_zeros() as usize;
            kicks &= kicks - 1;
            crate::trigger_preempt(c);
        }
    }

    #[inline]
    pub fn flush_woken(&mut self) { self.flush_woken_sync(None); }

    /// sched_setaffinity changed slot `idx`'s mask (TASKS held): refresh the
    /// mirror, move it off a queue it may no longer be on, and make a
    /// running one reschedule so its switch-back migrates it.
    pub fn set_affinity(&mut self, idx: usize, m: u64) {
        if let Some(t) = self.tasks[idx].as_deref_mut() { t.affinity = m; }
        self.reaffine(idx);
    }

    fn reaffine(&mut self, idx: usize) {
        let Some(a) = self.tasks[idx].as_deref().map(|t| t.affinity) else { return };
        let s = &crate::slot::SCHED[idx];
        s.aff.store(a, Ordering::Relaxed);
        let mut g = crate::cpuq::lock_slot(idx);
        let run = s.on_cpu.load(Ordering::Acquire);
        if run != 0 {
            drop(g);
            let c = run as usize - 1;
            if !crate::slot::mask_allows(a, c) { crate::trigger_preempt(c); }
            return;
        }
        if s.on_rq.load(Ordering::Relaxed) == 0 || crate::slot::mask_allows(a, g.cpu()) { return; }
        let home = g.cpu();
        g.remove(idx);
        let target = Self::select_cpu(home, a);
        drop(g);
        let mut d = crate::cpuq::lock(target);
        crate::slot::place(idx, d.place_floor());
        d.insert(idx);
        drop(d);
        if crate::cpu_is_idle(target) { crate::trigger_preempt(target); }
    }

    /// Re-place a task (renice) against its own queue, with its new weight.
    pub fn place_home(&mut self, idx: usize) {
        let Some(w) = self.tasks[idx].as_deref().map(|t| t.weight) else { return };
        let g = crate::cpuq::lock_slot(idx);
        crate::slot::SCHED[idx].weight.store(w.max(1), Ordering::Relaxed);
        crate::slot::place(idx, g.place_floor());
    }

    /// Rule R4 for a caller that must know the outcome: under TASKS and the
    /// task's queue lock, if `idx` is on a CPU return that CPU; else set its
    /// state to `new` and dequeue it (it cannot be dispatched again until a
    /// TASKS holder makes it Ready).
    pub fn try_park(&mut self, idx: usize, new: TaskState) -> Result<(), usize> {
        if crate::cpuq::held_here() != 0 { panic!("[SCHED] try_park under an RQ lock"); }
        let mut g = crate::cpuq::lock_slot(idx);
        if let Some(c) = crate::slot::on_cpu(idx) { return Err(c); }
        if let Some(t) = self.tasks[idx].as_deref_mut() { t.set_state_held(new); }
        g.remove(idx);
        Ok(())
    }

    /// Once-per-tick checker over every live task (see Q1–Q3 above), under
    /// TASKS and every queue lock (`guards`), plus the old full-scan duties:
    /// park-hint repair and the slot checks (state and dispatch mirrors,
    /// hung-ready watchdog).
    pub fn check_all(&mut self, guards: &[Option<crate::cpuq::RqGuard>; crate::MAX_CPUS]) {
        let scan_ns = crate::monotonic_ns();
        let idle_cpu = crate::some_other_cpu_idle();
        let mut queued_total = [0u32; crate::MAX_CPUS];
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
                let s = &crate::slot::SCHED[i];
                let r = s.on_rq.load(Ordering::Relaxed);
                if r != 0 {
                    let c = (r - 1) as usize;
                    match guards.get(c).and_then(|g| g.as_ref()) {
                        Some(g) if g.is_queued(i) => {
                            queued_total[c] += 1;
                            if s.q_gen.load(Ordering::Relaxed) != s.gen.load(Ordering::Relaxed) {
                                crate::slot::GEN_MISS.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        _ => { crate::slot::RQ_BAD.fetch_add(1, Ordering::Relaxed); }
                    }
                } else if t.state == TaskState::Ready && s.on_cpu.load(Ordering::Relaxed) == 0 {
                    if crate::slot::woken_pending(i) {
                        crate::slot::WOKEN_LATE.fetch_add(1, Ordering::Relaxed);
                    } else {
                        crate::slot::RQMISS.fetch_add(1, Ordering::Relaxed);
                        crate::slot::RQMISS_PID.store(t.pid, Ordering::Relaxed);
                        // Heal: placed when the caller releases TASKS.
                        crate::slot::mark_woken(i);
                    }
                }
            }
        }
        for c in 0..crate::MAX_CPUS {
            if let Some(g) = guards[c].as_ref() {
                if queued_total[c] != g.nr { crate::slot::RQ_BAD.fetch_add(1, Ordering::Relaxed); }
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
        self.pk_kb[idx] = 0;
        self.wx_sync(idx);
        if let Some(task) = self.get_mut(idx) {
            task.set_state(TaskState::Blocked);
            task.blocked_on    = Some(port);
            task.poll_deadline = deadline;
            task.poll_mask     = mask;
            task.poll_kbits    = 0;
            task.poll_nfp      = 0;
            task.poll_nx       = 0;
        }
    }

    /// `block_on_port_until` with the exact-key refinement of `mask` (see
    /// `crate::PollSet`): `kbits` are the bits only keyed interests explain,
    /// `fps` their fingerprints. More fingerprints than fit leave every keyed
    /// bit unrefined (safe: refinement only ever removes wakes).
    pub fn block_on_port_until_keyed(&mut self, pid: Pid, port: u32, deadline: u64,
                                     mask: u64, kbits: u64, fps: &[u16], xk: &[u64]) {
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
            crate::put_xkeys(&mut task.poll_xkeys, &mut task.poll_nx, xk);
            if fps.len() <= crate::POLL_FP_MAX && kbits != 0 {
                task.poll_fps[..fps.len()].copy_from_slice(fps);
                task.poll_nfp   = fps.len() as u16;
                task.poll_kbits = kbits & mask;
            } else {
                task.poll_kbits = 0;
                task.poll_nfp   = 0;
            }
        }
        if fps.len() <= crate::POLL_FP_MAX && kbits != 0 {
            self.fpb[idx] = fp_bloom(fps);
            self.pk_kb[idx] = kbits & mask;
        } else {
            self.pk_kb[idx] = 0;
        }
        self.wx_sync(idx);
    }

    /// Start listening (see `crate::poll_listen_begin`); returns the new id.
    pub fn listen_begin(&mut self, pid: Pid, mask: u64, kbits: u64, fps: &[u16], xk: &[u64]) -> u32 {
        let Some(idx) = self.find_pid_idx(pid) else { return 0 };
        let mask = if mask == 0 { crate::POLL_TAG_ALL } else { mask };
        self.listening[idx / 64] |= 1u64 << (idx % 64);
        self.ls_mask[idx] = mask;
        self.ls_install_mirror(idx, mask, kbits, fps);
        match self.tasks[idx].as_deref_mut() {
            Some(t) => {
                Self::listen_install(t, mask, kbits, fps, xk);
                t.listen.on   = true;
                t.listen.id   = t.listen.id.wrapping_add(1);
                t.listen.ring = crate::WakeRing::EMPTY;
                t.listen.id
            }
            None => 0,
        }
    }

    /// The wake-index side of `listen_install` (same condition), then
    /// `wx_sync`.
    fn ls_install_mirror(&mut self, idx: usize, mask: u64, kbits: u64, fps: &[u16]) {
        if fps.len() <= crate::POLL_FP_MAX && kbits != 0 {
            self.fpb[idx] = fp_bloom(fps);
            self.ls_kb[idx] = kbits & mask;
        } else {
            self.ls_kb[idx] = 0;
        }
        self.wx_sync(idx);
    }

    /// The mask slot `i` is indexed under (see `wx_row`): its park mirror
    /// when that names the poll channel, OR its listener filter.
    #[inline]
    fn row_mask(&self, i: usize) -> u64 {
        let p = if self.pk_port[i] == crate::POLL_WAIT_CHANNEL { self.pk_mask[i] } else { 0 };
        let l = if self.listening[i / 64] & (1u64 << (i % 64)) != 0 { self.ls_mask[i] } else { 0 };
        if p == crate::POLL_TAG_ALL || l == crate::POLL_TAG_ALL { crate::POLL_TAG_ALL } else { p | l }
    }

    /// Bring slot `i`'s wake-index rows in line with `row_mask(i)`. Costs the
    /// bits that changed: nothing for a re-park with the same filter.
    fn wx_sync(&mut self, i: usize) {
        let new = self.row_mask(i);
        let old = self.wx_mask[i];
        if new == old { return; }
        self.wx_mask[i] = new;
        let (w, b) = (i / 64, 1u64 << (i % 64));
        let oa = old == crate::POLL_TAG_ALL;
        let na = new == crate::POLL_TAG_ALL;
        if oa != na {
            if na { self.wx_all[w] |= b } else { self.wx_all[w] &= !b }
        }
        let ob = if oa { 0 } else { old };
        let nb = if na { 0 } else { new };
        let mut clr = ob & !nb;
        while clr != 0 { let k = clr.trailing_zeros() as usize; clr &= clr - 1; self.wx_row[k][w] &= !b; }
        let mut set = nb & !ob;
        while set != 0 { let k = set.trailing_zeros() as usize; set &= set - 1; self.wx_row[k][w] |= b; }
    }

    fn listen_install(t: &mut Task, mask: u64, kbits: u64, fps: &[u16], xk: &[u64]) {
        t.listen.mask = mask;
        crate::put_xkeys(&mut t.listen.xkeys, &mut t.listen.nx, xk);
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
    pub fn listen_update(&mut self, pid: Pid, id: u32, mask: u64, kbits: u64, fps: &[u16], xk: &[u64])
        -> Option<crate::WakeRing>
    {
        let idx = self.listener(pid, id)?;
        let mask = if mask == 0 { crate::POLL_TAG_ALL } else { mask };
        self.ls_mask[idx] = mask;
        self.ls_install_mirror(idx, mask, kbits, fps);
        let t = self.tasks[idx].as_deref_mut()?;
        Self::listen_install(t, mask, kbits, fps, xk);
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
        self.pk_kb[idx] = self.ls_kb[idx];
        self.wx_sync(idx);
        if let Some(t) = self.get_mut(idx) {
            t.set_state(TaskState::Blocked);
            t.blocked_on    = Some(port);
            t.poll_deadline = deadline;
            t.poll_mask     = mask;
            t.poll_kbits    = t.listen.kbits;
            t.poll_nfp      = t.listen.nfp;
            t.poll_xkeys    = t.listen.xkeys;
            t.poll_nx       = t.listen.nx;
        }
        crate::ListenPark::Parked
    }

    /// See `crate::poll_listen_end`.
    pub fn listen_end(&mut self, pid: Pid) {
        let Some(idx) = self.find_pid_idx(pid) else { return };
        self.listening[idx / 64] &= !(1u64 << (idx % 64));
        self.wx_sync(idx);
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
        if poll_port { crate::bump_poll_wake_epoch(); }
        // EPOLLEXCLUSIVE: a parker that holds `key` only through exclusive
        // registrations is woken only if no such parker was woken yet by
        // this wake (Linux: the first exclusive entry ends the walk).
        let mut excl_taken = false;
        // Wake index (see `wx_row`): a narrow wake on the poll channel visits
        // only the slots indexed under one of its bits or under the
        // broadcast mask; a keyed one then drops refined fingerprint misses
        // by the mirrors, without a Task dereference.
        let mode = WAKE_IDX.load(Ordering::Relaxed);
        let narrow = poll_port && tag != crate::POLL_TAG_ALL && tag.count_ones() <= WX_MAX_BITS;
        let use_idx = narrow && mode == 1;
        let verify = narrow && mode == 2;
        let kfp = key.map(crate::poll_key_fp);
        let (mut visited, mut derefs, mut mskip, mut misses) = (0u32, 0u32, 0u32, 0u32);
        for w in 0..OCC_WORDS {
          let lwords = if poll_port { self.listening[w] } else { 0 };
          let mut bits = (self.parked[w] | lwords) & self.occupied[w];
          let cand = if use_idx || verify {
              let mut c = self.wx_all[w];
              let mut t = tag;
              while t != 0 { let k = t.trailing_zeros() as usize; t &= t - 1; c |= self.wx_row[k][w]; }
              c
          } else { !0 };
          if use_idx { bits &= cand; }
          while bits != 0 {
            let i = w * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            visited += 1;
            let b = 1u64 << (i % 64);
            // Park-mirror prefilter: a task parked on `port` registered it
            // (and its mask, or a superset after widening) here.
            let pm = self.parked[w] & b != 0 && self.pk_port[i] == port
                && (self.pk_mask[i] & tag != 0 || tag == crate::POLL_TAG_ALL);
            let lm = lwords & b != 0
                && (self.ls_mask[i] & tag != 0 || tag == crate::POLL_TAG_ALL);
            if !pm && !lm { continue; }
            // Mirror refinement of a keyed wake: a refined bit whose
            // fingerprint bloom lacks the key cannot match (see `fpb`).
            let (pm_ok, lm_ok) = match kfp {
                Some(fp) if narrow => {
                    let bh = fp_bloom_has(&self.fpb[i], fp);
                    (pm && (self.pk_kb[i] & tag == 0 || bh) && cand & b != 0,
                     lm && (self.ls_kb[i] & tag == 0 || bh) && cand & b != 0)
                }
                _ => (pm && cand & b != 0, lm && cand & b != 0),
            };
            let (pm, lm) = if use_idx {
                if pm && !pm_ok { mskip += 1; }
                (pm_ok, lm_ok)
            } else { (pm, lm) };
            if !pm && !lm { continue; }
            derefs += 1;
            if let Some(task) = self.tasks[i].as_deref_mut() {
                // A listener records the wake whatever its state (parked
                // here, running, or blocked elsewhere): see `PollListen`.
                if lm && Self::listen_match(task, tag, exclusive, key) {
                    if verify && !lm_ok { misses += 1; }
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
                    if verify && !pm_ok { misses += 1; }
                    if let Some(k) = key {
                        if task.poll_nx != 0 && task.poll_xkeys[..task.poll_nx as usize].contains(&k) {
                            if excl_taken {
                                crate::wakestat::EXCL_SKIPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                                continue;
                            }
                            excl_taken = true;
                        }
                    }
                    if task.poll_mask == crate::POLL_TAG_ALL { allmask += 1; }
                    if crate::gdwake::enabled() {
                        crate::gdwake::mark(task.pid, crate::gdwake::SITE.load(core::sync::atomic::Ordering::Relaxed));
                    }
                    task.set_state(TaskState::Ready);
                    task.blocked_on    = None;
                    task.poll_deadline = u64::MAX;
                    task.poll_mask     = crate::POLL_TAG_ALL;
                    task.poll_wakes    = task.poll_wakes.wrapping_add(1);
                    woken += 1;
                }
            }
          }
        }
        if poll_port && tag != crate::POLL_TAG_ALL {
            crate::wakestat::note_scan(use_idx, visited, derefs, woken as u32, mskip, misses);
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
    pub fn widen_port_masks(&mut self, port: u32, match_key: u64, add: u64) {
        let match_tag = crate::poll_key_bit(match_key);
        let fp = crate::poll_key_fp(match_key);
        // No wake epoch bump: widening concerns parkers holding an epoll
        // instance's tag, and a poll() set holding an epoll fd re-checks
        // EPOLL_CTL_GEN instead (every widening follows an epoll_ctl).
        for i in self.parked_slots() {
            if let Some(task) = self.tasks[i].as_deref_mut() {
                // The parker must watch the instance itself, not only share
                // its bit: a refined bit needs the key's fingerprint.
                if task.blocked_on == Some(port) && task.state == TaskState::Blocked
                    && (task.poll_mask & match_tag) != 0
                    && (task.poll_kbits & match_tag == 0
                        || task.poll_fps[..task.poll_nfp as usize].contains(&fp)) {
                    task.poll_mask |= add;
                    self.pk_mask[i] |= add;
                    // The added interest's key is unknown here: its bits must
                    // not be refined.
                    task.poll_kbits &= !add;
                    if port == crate::POLL_WAIT_CHANNEL && task.listen.on {
                        task.listen.ring.push_tags(add);
                    }
                } else { continue; }
            } else { continue; }
            self.pk_kb[i] &= !add;
            self.wx_sync(i);
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
        if timerfd_tags != 0 && port == crate::POLL_WAIT_CHANNEL {
            crate::bump_poll_wake_epoch();
        }
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

    /// The wall clock was stepped: every parked task waiting for a
    /// CLOCK_REALTIME instant (`Task::rt_wait`) gets its monotonic deadline
    /// re-translated by `to_mono`, in the task and in the park mirror.
    /// Returns the earliest new deadline (`u64::MAX` = none) for the caller
    /// to arm; a deadline now in the past is released by that pass.
    pub fn retarget_rt_waiters(&mut self, to_mono: impl Fn(u64) -> u64) -> u64 {
        let mut min = u64::MAX;
        for w in 0..OCC_WORDS {
            let mut bits = self.parked[w] & self.occupied[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if let Some(t) = self.tasks[i].as_deref_mut() {
                    if t.rt_wait == 0 || t.state != TaskState::Blocked || t.poll_deadline == u64::MAX {
                        continue;
                    }
                    let d = to_mono(t.rt_wait).max(1);
                    t.poll_deadline = d;
                    self.pk_deadline[i] = d;
                    min = min.min(d);
                }
            }
        }
        min
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
        if let Some(task) = t.as_mut() {
            // R5: freed under TASKS + its queue lock, off-CPU (on_remove
            // panics otherwise), dequeued, gen bumped.
            if crate::cpuq::held_here() != 0 { panic!("[SCHED] remove under an RQ lock"); }
            let mut g = crate::cpuq::lock_slot(idx);
            g.remove(idx);
            crate::slot::clear_woken(idx);
            if task.state != TaskState::Zombie { crate::uidcount::dec(task.uid); }
            crate::slot::on_remove(idx, task);
            drop(g);
        }
        if let Some(task) = t.as_ref() {
            self.len = self.len.saturating_sub(1);
            if task.pid == task.tgid { self.nproc = self.nproc.saturating_sub(1); }
            self.occupied[idx / 64] &= !(1u64 << (idx % 64));
            self.parked[idx / 64] &= !(1u64 << (idx % 64));
            self.listening[idx / 64] &= !(1u64 << (idx % 64));
            self.pk_port[idx] = u32::MAX;
            self.wx_sync(idx);
            self.index_remove(task.pid);
            // Its queued RT signals die with it: nothing can dequeue them,
            // and the pid may be handed out again.
            self.rtq.purge(task.pid);
            // Retire the side-table entry with the slot. Leaving it would hand
            // a later caller the tgid of a task that no longer exists, where
            // `tgid_of`'s contract is to fall back to the pid itself.
            crate::pid_tgid_remove(task.pid);
            crate::percpu::pexit_bump();
        }
        t
    }
}
