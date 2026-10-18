//! Lane percpu phase 3: per-CPU run queues, each under its own lock `RQ[c]`.
//!
//! RUN_QUEUE stays the global TASKS lock (task table, pid index, wait/park
//! registry, signals, process tree). `RQ[c]` owns only CPU `c`'s ready set
//! and dispatch state, so a context switch — switch-back, pick, dispatch —
//! takes `RQ[me]` alone (the switch-back hold is carried into the pick) and
//! no TASKS hold at all unless the switched-out task needs one (`sb_slow`,
//! Zombie). See the lane notes for the rules R1–R6; in short:
//!
//! - Lock order: FUTEX_TABLE → TASKS → `RQ[i]`. A CPU never waits for a
//!   second RQ lock while holding one, except the tick checker, which takes
//!   them all in index order; balance and steal use `try_lock`. An RQ holder
//!   never takes TASKS (asserted in `lockwatch`), allocates, or opens an IRQ
//!   window, so IRQ-context TASKS holders may spin on an RQ lock.
//! - Ready ↔ running only on the owning CPU under `RQ[cpu]` (dispatch claims
//!   `on_cpu`, the switch-back releases it and requeues a Ready task).
//! - A wake (TASKS) marks the slot in `slot::WOKEN`; `RunQueue::flush_woken`
//!   places it — under TASKS + `RQ[target]` — at the latest when the TASKS
//!   guard is released (`lockwatch`). A task still on a CPU is left to its
//!   switch-back, which sees the Ready state under the same `RQ[cpu]`.
//! - Others moving a task out of Ready hold TASKS + `RQ[cpu]`
//!   (`slot::note_state`): off-CPU → dequeued, on-CPU → `sb_slow`.
//! - Virtual time is global: vruntime is charged in real ns and is NOT
//!   renormalised when a task changes queue, so the tick balance can compare
//!   queues (`VT`) and move the most lagging task to a CPU whose tasks are
//!   ahead — that is what keeps CPU hogs fair across CPUs. `GVT` (the
//!   largest queue minimum) bounds how far behind any placement may start.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use crate::runqueue::MAX_TASKS;
use crate::slot::{self, SCHED};
use crate::task::TaskState;
use crate::MAX_CPUS;

const OCC_WORDS: usize = MAX_TASKS / 64;
const _: () = assert!(OCC_WORDS <= 64);

/// The tick balance moves a task when another queue lags ours by this much
/// virtual time (2 ticks of nice-0 CPU).
const VT_PULL_THRESH: u64 = 2 * crate::task::VT_PER_TICK;
/// No queue minimum (and so no placement) trails `GVT` by more than this
/// (32 ticks); a migrated task keeps at most this much lag.
const LAG_MAX: u64 = 32 * crate::task::VT_PER_TICK;

extern "C" { fn arch_monotonic_ns() -> u64; }

#[inline(always)]
fn now() -> u64 { unsafe { arch_monotonic_ns() } }

#[inline(always)]
fn me() -> usize { unsafe { crate::cpu_id() }.min(MAX_CPUS - 1) }

/// One CPU's ready set: the queued slots (bitmap), their count, the
/// queue's monotonic minimum vruntime, and the slot running there (+1).
pub struct CpuQueue {
    queued: [u64; OCC_WORDS],
    /// Bit `w` set ⇔ `queued[w] != 0`: scans visit only non-empty words.
    summary: u64,
    pub nr: u32,
    pub min_vr: u64,
    pub curr: u32,
    /// Candidates the last pick saw (sync wake pays a dropped kick when
    /// others were waiting).
    pub last_pick_cands: u32,
}

impl CpuQueue {
    const fn new() -> Self { Self { queued: [0; OCC_WORDS], summary: 0, nr: 0, min_vr: 0, curr: 0, last_pick_cands: 0 } }

    #[inline]
    pub fn is_queued(&self, idx: usize) -> bool { self.queued[idx / 64] & (1u64 << (idx % 64)) != 0 }
}

#[repr(C, align(64))]
pub struct RqLock {
    /// cpu + 1 of the holder, 0 = free.
    owner: AtomicU32,
    q: UnsafeCell<CpuQueue>,
}

unsafe impl Sync for RqLock {}

impl RqLock {
    const fn new() -> Self { Self { owner: AtomicU32::new(0), q: UnsafeCell::new(CpuQueue::new()) } }
}

pub static RQ: [RqLock; MAX_CPUS] = [const { RqLock::new() }; MAX_CPUS];

/// Lock-free peeks (written under `RQ[c]`): queue length, the slot running
/// there (+1), the queue's monotonic minimum vruntime and its current
/// minimum (`VT`, non-monotonic: what the tick balance compares).
pub static NR_QUEUED: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
pub static CURR: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
pub static MIN_VR: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
pub static VT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Largest queue minimum seen (monotonic): the leading edge of virtual time.
pub static GVT: AtomicU64 = AtomicU64::new(0);
/// Set by each CPU's tick: its next pick runs the balance.
pub static BALANCE_DUE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// RQ locks each CPU holds (lock-order assertions).
static HELD: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

pub static STEALS: AtomicU64 = AtomicU64::new(0);
pub static CNT_PULLS: AtomicU64 = AtomicU64::new(0);
pub static VT_PULLS: AtomicU64 = AtomicU64::new(0);
pub static TRY_FAIL: AtomicU64 = AtomicU64::new(0);

/// RQ locks the calling CPU holds.
#[inline]
pub fn held_here() -> u32 { HELD[me()].load(Ordering::Relaxed) }

/// A held `RQ[c]`.
pub struct RqGuard { c: usize, t0: u64 }

impl Deref for RqGuard {
    type Target = CpuQueue;
    #[inline]
    fn deref(&self) -> &CpuQueue { unsafe { &*RQ[self.c].q.get() } }
}

impl DerefMut for RqGuard {
    #[inline]
    fn deref_mut(&mut self) -> &mut CpuQueue { unsafe { &mut *RQ[self.c].q.get() } }
}

impl Drop for RqGuard {
    #[inline]
    fn drop(&mut self) {
        crate::lockwatch::row_note_hold(crate::lockwatch::row_rq(self.c), now().saturating_sub(self.t0));
        HELD[me()].fetch_sub(1, Ordering::Relaxed);
        RQ[self.c].owner.store(0, Ordering::Release);
    }
}

/// Lock `RQ[c]`. Spins servicing TLB shootdowns, like the TASKS spin.
#[inline]
pub fn lock(c: usize) -> RqGuard {
    let cpu = me();
    let mine = cpu as u32 + 1;
    let l = &RQ[c];
    if l.owner.compare_exchange(0, mine, Ordering::Acquire, Ordering::Relaxed).is_err() {
        if l.owner.load(Ordering::Relaxed) == mine {
            panic!("[SCHED] RQ{} locked twice by cpu {}", c, cpu);
        }
        crate::lockwatch::note_wait(crate::lockwatch::L_RQ);
        let w0 = now();
        loop {
            if l.owner.load(Ordering::Relaxed) == 0
                && l.owner.compare_exchange_weak(0, mine, Ordering::Acquire, Ordering::Relaxed).is_ok()
            {
                break;
            }
            mm::paging::tlb_service_pending();
            core::hint::spin_loop();
        }
        crate::lockwatch::note_wait(0);
        crate::lockwatch::row_note_spin(crate::lockwatch::row_rq(c), now().saturating_sub(w0));
    }
    HELD[cpu].fetch_add(1, Ordering::Relaxed);
    RqGuard { c, t0: now() }
}

#[inline]
pub fn try_lock(c: usize) -> Option<RqGuard> {
    let cpu = me();
    if RQ[c].owner.compare_exchange(0, cpu as u32 + 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return None;
    }
    HELD[cpu].fetch_add(1, Ordering::Relaxed);
    Some(RqGuard { c, t0: now() })
}

/// Lock the queue slot `idx` belongs to (`SCHED[idx].cpu`, rechecked under
/// the lock: it changes only under the old queue's lock or, for an
/// unqueued off-CPU task, under TASKS).
pub fn lock_slot(idx: usize) -> RqGuard {
    loop {
        let c = (SCHED[idx].cpu.load(Ordering::Acquire) as usize).min(MAX_CPUS - 1);
        let g = lock(c);
        if SCHED[idx].cpu.load(Ordering::Acquire) as usize == c { return g; }
    }
}

/// Every active CPU's queue, locked in index order (the tick checker).
pub fn lock_all(out: &mut [Option<RqGuard>; MAX_CPUS]) {
    for c in 0..crate::active_cpus() { out[c] = Some(lock(c)); }
}

/// `min_vr` floored to trail the leading edge by at most `LAG_MAX`.
#[inline]
fn floor(min_vr: u64) -> u64 { min_vr.max(GVT.load(Ordering::Relaxed).saturating_sub(LAG_MAX)) }

#[inline]
fn pickable(i: usize) -> bool {
    let s = &SCHED[i];
    s.state.load(Ordering::Acquire) == slot::state_code(TaskState::Ready)
        && s.on_cpu.load(Ordering::Relaxed) == 0
        && s.q_gen.load(Ordering::Relaxed) == s.gen.load(Ordering::Relaxed)
}

#[inline]
fn filtered(i: usize) -> bool {
    let s = &SCHED[i];
    crate::quiesce_filtered(s.m_tgid.load(Ordering::Relaxed), s.m_pid.load(Ordering::Relaxed))
}

impl RqGuard {
    #[inline]
    pub fn cpu(&self) -> usize { self.c }

    /// Queue slot `idx` here (no-op if it is queued already).
    pub fn insert(&mut self, idx: usize) {
        let s = &SCHED[idx];
        let r = s.on_rq.load(Ordering::Relaxed);
        if r != 0 {
            if r as usize != self.c + 1 { slot::RQ_BAD.fetch_add(1, Ordering::Relaxed); }
            return;
        }
        let c = self.c;
        let q = &mut **self;
        q.queued[idx / 64] |= 1u64 << (idx % 64);
        q.summary |= 1u64 << (idx / 64);
        q.nr += 1;
        NR_QUEUED[c].store(q.nr, Ordering::Relaxed);
        s.cpu.store(c as u32, Ordering::Release);
        s.q_gen.store(s.gen.load(Ordering::Relaxed), Ordering::Relaxed);
        s.on_rq.store(c as u32 + 1, Ordering::Relaxed);
    }

    /// Take slot `idx` off this queue (it must be here, if anywhere).
    pub fn remove(&mut self, idx: usize) -> bool {
        let s = &SCHED[idx];
        let r = s.on_rq.load(Ordering::Relaxed);
        if r == 0 { return false; }
        if r as usize != self.c + 1 { slot::RQ_BAD.fetch_add(1, Ordering::Relaxed); return false; }
        let c = self.c;
        let q = &mut **self;
        let b = 1u64 << (idx % 64);
        if q.queued[idx / 64] & b != 0 {
            q.queued[idx / 64] &= !b;
            if q.queued[idx / 64] == 0 { q.summary &= !(1u64 << (idx / 64)); }
            q.nr -= 1;
            NR_QUEUED[c].store(q.nr, Ordering::Relaxed);
        } else {
            slot::RQ_BAD.fetch_add(1, Ordering::Relaxed);
        }
        s.on_rq.store(0, Ordering::Relaxed);
        true
    }

    /// Placement floor for a task joining this queue.
    #[inline]
    pub fn place_floor(&self) -> u64 { floor(self.min_vr) }

    /// EEVDF over this queue: the eligible candidate with the earliest
    /// virtual deadline. Drops stale entries; skips quiesce-filtered tasks
    /// (they stay queued). Advances `min_vr` (monotonic, floored) and
    /// publishes `VT`. Returns (slot, candidates).
    fn eevdf(&mut self) -> (Option<usize>, u32) {
        let c = self.c;
        let mut n = 0u32;
        let mut sum_w: u64 = 0;
        let mut sum_wv: u128 = 0;
        let mut vmin = u64::MAX;
        let mut only = 0usize;
        let mut sw = self.summary;
        while sw != 0 {
            let w = sw.trailing_zeros() as usize;
            sw &= sw - 1;
            let mut bits = self.queued[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if !pickable(i) {
                    // Every way out of Ready dequeues (R4/R5), so none of
                    // these should happen: a queued slot that was freed and
                    // reused (gen), is running somewhere (a double queue),
                    // or is no longer Ready (stale).
                    let s = &SCHED[i];
                    if s.q_gen.load(Ordering::Relaxed) != s.gen.load(Ordering::Relaxed) {
                        slot::GEN_MISS.fetch_add(1, Ordering::Relaxed);
                    } else if s.on_cpu.load(Ordering::Relaxed) != 0 {
                        slot::RQ_BAD.fetch_add(1, Ordering::Relaxed);
                    } else {
                        slot::STALE.fetch_add(1, Ordering::Relaxed);
                    }
                    self.remove(i);
                    continue;
                }
                let v = slot::vr(i);
                if v < vmin { vmin = v; }
                if filtered(i) { continue; }
                let wt = slot::weight(i) as u64;
                sum_w += wt;
                sum_wv += wt as u128 * v as u128;
                n += 1;
                only = i;
            }
        }
        if self.curr != 0 {
            let v = slot::vr(self.curr as usize - 1);
            if v < vmin { vmin = v; }
        }
        if vmin != u64::MAX {
            VT[c].store(vmin, Ordering::Relaxed);
            if vmin > self.min_vr { self.min_vr = vmin; }
        }
        let m = floor(self.min_vr);
        if m != self.min_vr { self.min_vr = m; }
        MIN_VR[c].store(self.min_vr, Ordering::Relaxed);
        GVT.fetch_max(self.min_vr, Ordering::Relaxed);
        if sum_w == 0 { return (None, 0); }
        if n == 1 { return (Some(only), 1); }
        let mut best: Option<(usize, u64)> = None;
        let mut fallback: Option<(usize, u64)> = None;
        let mut sw = self.summary;
        while sw != 0 {
            let w = sw.trailing_zeros() as usize;
            sw &= sw - 1;
            let mut bits = self.queued[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if filtered(i) { continue; }
                let v = slot::vr(i);
                let d = slot::vd(i);
                let eligible = (v as u128) * (sum_w as u128) <= sum_wv;
                if eligible && best.map_or(true, |(_, b)| d < b) { best = Some((i, d)); }
                if fallback.map_or(true, |(_, b)| v < b) { fallback = Some((i, v)); }
            }
        }
        (best.or(fallback).map(|(i, _)| i), n)
    }

    /// The pickable, unfiltered queued task with the lowest vruntime.
    fn most_lagging(&self) -> Option<usize> {
        let mut pick: Option<(usize, u64)> = None;
        let mut sw = self.summary;
        while sw != 0 {
            let w = sw.trailing_zeros() as usize;
            sw &= sw - 1;
            let mut bits = self.queued[w];
            while bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if !pickable(i) || filtered(i) { continue; }
                let v = slot::vr(i);
                if pick.map_or(true, |(_, b)| v < b) { pick = Some((i, v)); }
            }
        }
        pick.map(|(i, _)| i)
    }
}

/// Move queued slot `idx` from `src` to `dst`. Virtual time is global, so
/// the vruntime moves as is, only clamped to trail `dst` by at most
/// `LAG_MAX`.
fn migrate(src: &mut RqGuard, dst: &mut RqGuard, idx: usize) {
    src.remove(idx);
    let lo = dst.place_floor().saturating_sub(LAG_MAX);
    let s = &SCHED[idx];
    let v = s.vr.load(Ordering::Relaxed);
    if v < lo {
        s.vr.store(lo, Ordering::Relaxed);
        s.vd.store(lo + crate::task::Task::slice_vt(slot::weight(idx)), Ordering::Relaxed);
    }
    dst.insert(idx);
}

/// Tick balance (once per tick per CPU, at its first pick after the tick):
/// - count: some CPU's load (queued + running) is ≥ 2 above our queue
///   length (our previous task is already requeued) → take its most
///   lagging task;
/// - virtual time: some queue's current minimum vruntime trails ours by
///   more than `VT_PULL_THRESH` → its tasks are getting less CPU than ours:
///   take its most lagging task, which then catches up here.
fn tick_balance(g: &mut RqGuard, me: usize) {
    let n = crate::active_cpus();
    if n <= 1 { return; }
    let my_nr = g.nr;
    let my_vt = match g.most_lagging() { Some(i) => slot::vr(i), None => return };
    let mut cnt: Option<(usize, u32)> = None;
    let mut lag: Option<(usize, u64)> = None;
    for c in 0..n {
        if c == me { continue; }
        let nr = NR_QUEUED[c].load(Ordering::Relaxed);
        if nr == 0 { continue; }
        let load = nr + (CURR[c].load(Ordering::Relaxed) != 0) as u32;
        if load >= my_nr + 2 && cnt.map_or(true, |(_, b)| load > b) { cnt = Some((c, load)); }
        let vt = VT[c].load(Ordering::Relaxed);
        if my_vt > vt.saturating_add(VT_PULL_THRESH) && lag.map_or(true, |(_, b)| my_vt - vt > b) {
            lag = Some((c, my_vt - vt));
        }
    }
    if let Some((c, _)) = cnt {
        match try_lock(c) {
            Some(mut v) => if let Some(i) = v.most_lagging() {
                migrate(&mut v, g, i);
                CNT_PULLS.fetch_add(1, Ordering::Relaxed);
                return;
            },
            None => { TRY_FAIL.fetch_add(1, Ordering::Relaxed); }
        }
    }
    if let Some((c, _)) = lag {
        match try_lock(c) {
            Some(mut v) => if let Some(i) = v.most_lagging() {
                if my_vt > slot::vr(i).saturating_add(VT_PULL_THRESH) {
                    migrate(&mut v, g, i);
                    VT_PULLS.fetch_add(1, Ordering::Relaxed);
                }
            },
            None => { TRY_FAIL.fetch_add(1, Ordering::Relaxed); }
        }
    }
}

/// New-idle steal: our queue has nothing to run; take the most lagging
/// task of the busiest queue (then any other). `try_lock` only — a few
/// rounds while some queue has work, so a briefly held victim lock does
/// not send this CPU idle.
fn newidle_steal(g: &mut RqGuard, me: usize) -> bool {
    let n = crate::active_cpus();
    for _round in 0..8 {
        let mut any = false;
        let mut order = [0u8; MAX_CPUS];
        let mut k = 0usize;
        // Busiest first, then the rest in index order.
        let mut best: Option<(usize, u32)> = None;
        for c in 0..n {
            if c == me { continue; }
            let nr = NR_QUEUED[c].load(Ordering::Relaxed);
            if nr == 0 { continue; }
            any = true;
            if best.map_or(true, |(_, b)| nr > b) { best = Some((c, nr)); }
        }
        if !any { return false; }
        if let Some((b, _)) = best { order[k] = b as u8; k += 1; }
        for c in 0..n {
            if c == me || Some(c) == best.map(|x| x.0) || NR_QUEUED[c].load(Ordering::Relaxed) == 0 { continue; }
            order[k] = c as u8; k += 1;
        }
        for j in 0..k {
            let c = order[j] as usize;
            match try_lock(c) {
                Some(mut v) => if let Some(i) = v.most_lagging() {
                    migrate(&mut v, g, i);
                    STEALS.fetch_add(1, Ordering::Relaxed);
                    return true;
                },
                None => { TRY_FAIL.fetch_add(1, Ordering::Relaxed); }
            }
        }
        for _ in 0..64 { core::hint::spin_loop(); }
    }
    false
}

/// Pick the next task for CPU `me` and take it off the queue (the caller
/// claims it). Order: the tick balance if a tick passed, EEVDF over our
/// queue, else a new-idle steal.
pub fn pick_next(g: &mut RqGuard, me: usize) -> Option<usize> {
    let t0 = crate::lockwatch::pick_clock();
    if BALANCE_DUE[me].load(Ordering::Relaxed) {
        BALANCE_DUE[me].store(false, Ordering::Relaxed);
        tick_balance(g, me);
    }
    let (mut pick, n) = g.eevdf();
    g.last_pick_cands = n;
    if pick.is_none() && newidle_steal(g, me) {
        pick = g.eevdf().0;
    }
    if let Some(i) = pick {
        g.remove(i);
        g.curr = i as u32 + 1;
        CURR[me].store(i as u32 + 1, Ordering::Relaxed);
    }
    crate::lockwatch::note_pick(false, n, crate::lockwatch::pick_clock().wrapping_sub(t0));
    pick
}

/// Switch-back of slot `idx` on CPU `me` (after its run guard is
/// released): no longer current here; queued here if `requeue` (it is
/// Ready), waiting since `now_ns` (the hung-ready watchdog's clock: a
/// running task stays Ready, so no state transition restarts it).
#[inline]
pub fn switch_back(g: &mut RqGuard, me: usize, idx: usize, requeue: bool, now_ns: u64) {
    if g.curr == idx as u32 + 1 {
        g.curr = 0;
        CURR[me].store(0, Ordering::Relaxed);
    }
    if requeue {
        SCHED[idx].ready_since.store(now_ns, Ordering::Relaxed);
        g.insert(idx);
    }
}

/// Kick every idle CPU whose queue holds work (after a quiesce ends: its
/// filtered tasks stayed queued while their CPUs went idle).
pub fn kick_idle_with_work() {
    for c in 0..crate::active_cpus() {
        if NR_QUEUED[c].load(Ordering::Relaxed) != 0 && crate::cpu_is_idle(c) {
            crate::trigger_preempt(c);
        }
    }
}

/// Some CPU that is busy (or is the caller's) has queued work another CPU
/// could steal.
pub fn stealable_work(me: usize) -> bool {
    (0..crate::active_cpus()).any(|c| NR_QUEUED[c].load(Ordering::Relaxed) != 0
        && (c == me || !crate::cpu_is_idle(c)))
}

pub fn fmt_stats(f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
    write!(f, " steals {} cnt_pulls {} vt_pulls {} try_fail {}",
        l(&STEALS), l(&CNT_PULLS), l(&VT_PULLS), l(&TRY_FAIL))?;
    write!(f, "\n# cq gvt {}", GVT.load(Ordering::Relaxed))?;
    for c in 0..crate::active_cpus().min(8) {
        write!(f, " cpu{}:nr={},vt={},min={}", c, NR_QUEUED[c].load(Ordering::Relaxed),
            VT[c].load(Ordering::Relaxed), MIN_VR[c].load(Ordering::Relaxed))?;
    }
    Ok(())
}
