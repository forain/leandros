//! Lane percpu (2026-10-18): the road to per-CPU run queues.
//!
//! Phase 0 (this file's first half): small independent RUN_QUEUE savings and
//! the counters every later phase is gated on.
//!
//! - **Deadline servicer.** Every CPU that ever armed a poll deadline keeps
//!   re-arming its one-shot timer for the GLOBAL next deadline, so N CPUs fire
//!   on the same instant and each scans the parked set under RUN_QUEUE. Now
//!   one CPU at a time owns the pass (`deadline_service_enter`); a CPU that
//!   finds the pass taken leaves a request (`DL_AGAIN`) the owner honours
//!   with another pass before it lets go, and does not re-arm for the global
//!   deadline itself (the owner does). Same safety argument as a contended
//!   `try_lock_spin` (the old behaviour of a loser), with the loser's work
//!   handed to the winner instead of retried 200 µs later.
//! - **process_exit_state cache** (`pexit_*`): pidfd readiness probes answered
//!   from a per-tgid cache entry tagged with a global generation that every
//!   write able to change any answer bumps under RUN_QUEUE.
//! - **Sync wake** (`sync_*`): a user task that wakes one futex waiter from a
//!   syscall defers the idle-CPU kick to the end of that syscall; if it blocks
//!   first, this CPU picks the wakee itself and the kick is dropped.
//!
//! `SchedStatsLine` prints every counter as one `/proc/rqhold` line.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::MAX_CPUS;

// ── Counters ────────────────────────────────────────────────────────────────

pub static DEADLINE_PASS: AtomicU64 = AtomicU64::new(0);
pub static DEADLINE_SKIP: AtomicU64 = AtomicU64::new(0);
pub static DEADLINE_AGAIN: AtomicU64 = AtomicU64::new(0);
pub static PEXIT_FAST: AtomicU64 = AtomicU64::new(0);
pub static PEXIT_SLOW: AtomicU64 = AtomicU64::new(0);
pub static PEXIT_MISS: AtomicU64 = AtomicU64::new(0);
pub static SYNC_DEFER: AtomicU64 = AtomicU64::new(0);
pub static SYNC_DROP: AtomicU64 = AtomicU64::new(0);
pub static SYNC_FLUSH: AtomicU64 = AtomicU64::new(0);

#[inline]
pub(crate) fn bump(c: &AtomicU64) { c.fetch_add(1, Ordering::Relaxed); }

/// One `/proc/rqhold` line with every lane-percpu counter.
pub struct SchedStatsLine;

impl core::fmt::Display for SchedStatsLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
        write!(f, "# percpu deadline_pass {} deadline_skip {} deadline_again {} pexit_fast {} pexit_slow {} pexit_miss {} sync_defer {} sync_drop {} sync_flush {}",
            l(&DEADLINE_PASS), l(&DEADLINE_SKIP), l(&DEADLINE_AGAIN),
            l(&PEXIT_FAST), l(&PEXIT_SLOW), l(&PEXIT_MISS),
            l(&SYNC_DEFER), l(&SYNC_DROP), l(&SYNC_FLUSH))
    }
}

// ── Deadline servicer ───────────────────────────────────────────────────────

/// 0 = no pass running; else (cpu + 1), with `DL_AGAIN` set by a CPU that
/// found the pass taken and wants one more.
static DL_SERVICER: AtomicU32 = AtomicU32::new(0);
const DL_AGAIN: u32 = 1 << 31;

/// Try to become the CPU running the deadline pass. `false`: another CPU runs
/// it, and has been asked (`DL_AGAIN`) to run one more pass before it lets go,
/// which covers whatever made this CPU's timer fire. The request is an RMW on
/// the same word the owner's `deadline_service_exit` swaps, so either the
/// owner sees it, or this CPU saw the word free and became the owner.
pub fn deadline_service_enter() -> bool {
    let me = unsafe { crate::cpu_id() } as u32 + 1;
    let mut cur = DL_SERVICER.load(Ordering::Acquire);
    loop {
        let new = if cur == 0 { me } else { cur | DL_AGAIN };
        match DL_SERVICER.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                if cur == 0 { bump(&DEADLINE_PASS); } else { bump(&DEADLINE_SKIP); }
                return cur == 0;
            }
            Err(c) => cur = c,
        }
    }
}

/// End of one pass by the owner. `true`: another CPU asked for a pass while
/// this one ran; the caller still owns the servicer and must run another
/// pass (then call this again). `false`: released.
pub fn deadline_service_exit() -> bool {
    let me = unsafe { crate::cpu_id() } as u32 + 1;
    loop {
        let cur = DL_SERVICER.load(Ordering::Acquire);
        let new = if cur & DL_AGAIN != 0 { me } else { 0 };
        if DL_SERVICER.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            if new != 0 { bump(&DEADLINE_AGAIN); }
            return new != 0;
        }
    }
}

/// Give up ownership unconditionally (pass budget exhausted; the caller
/// re-arms its own timer shortly, which covers any dropped request).
pub fn deadline_service_release() {
    DL_SERVICER.store(0, Ordering::Release);
}

// ── process_exit_state cache ────────────────────────────────────────────────

/// Bumped (under RUN_QUEUE, after the write) by every write that can change
/// any `process_exit_state` answer: a task entering or leaving Zombie, a slot
/// filled or freed, `wait_reported`, an exit-log record inserted or consumed,
/// a pid/tgid exchange.
static PEXIT_GEN: AtomicU32 = AtomicU32::new(1);
const PEXIT_SLOTS: usize = 256;
const PEXIT_GEN_MASK: u64 = 0x3FFF_FFFF;
/// `(tgid << 32) | (gen & PEXIT_GEN_MASK) << 2 | exited << 1 | reaped`.
static PEXIT_CACHE: [AtomicU64; PEXIT_SLOTS] = [const { AtomicU64::new(0) }; PEXIT_SLOTS];
/// One fast answer in this many is re-derived under the lock and compared.
const PEXIT_VERIFY_EVERY: u64 = 256;

#[inline]
pub(crate) fn pexit_bump() { PEXIT_GEN.fetch_add(1, Ordering::Release); }

/// Generation to tag a slow-path answer with. Load it BEFORE reading the
/// state the answer derives from (writers bump after writing).
#[inline]
pub(crate) fn pexit_gen() -> u32 { PEXIT_GEN.load(Ordering::Acquire) }

#[inline]
fn pexit_pack(tgid: u32, gen: u32, a: (bool, bool)) -> u64 {
    ((tgid as u64) << 32) | (((gen as u64) & PEXIT_GEN_MASK) << 2)
        | ((a.0 as u64) << 1) | (a.1 as u64)
}

/// The cached answer for `tgid`, if it was computed at the current generation.
/// Returns `(answer, verify)` — `verify` asks the caller to re-derive it
/// under the lock and report a mismatch with `pexit_check`.
pub(crate) fn pexit_lookup(tgid: u32) -> Option<((bool, bool), bool)> {
    let g = pexit_gen();
    let e = PEXIT_CACHE[tgid as usize % PEXIT_SLOTS].load(Ordering::Acquire);
    if (e >> 32) as u32 != tgid || ((e >> 2) & PEXIT_GEN_MASK) != (g as u64 & PEXIT_GEN_MASK) || e == 0 {
        return None;
    }
    let n = PEXIT_FAST.fetch_add(1, Ordering::Relaxed);
    Some(((e & 2 != 0, e & 1 != 0), n % PEXIT_VERIFY_EVERY == PEXIT_VERIFY_EVERY - 1))
}

pub(crate) fn pexit_store(tgid: u32, gen: u32, a: (bool, bool)) {
    bump(&PEXIT_SLOW);
    PEXIT_CACHE[tgid as usize % PEXIT_SLOTS].store(pexit_pack(tgid, gen, a), Ordering::Release);
}

/// A sampled fast answer was re-derived under the lock: `fast` vs `locked`
/// at generation `gen_then` (when the fast answer was read) and `gen_now`
/// (under the lock). Only a mismatch at an unchanged generation is a miss —
/// a write in between legitimately changes the answer.
pub(crate) fn pexit_check(fast: (bool, bool), locked: (bool, bool), gen_then: u32, gen_now: u32) {
    if fast != locked && gen_then == gen_now { bump(&PEXIT_MISS); }
}

// ── Sync wake ───────────────────────────────────────────────────────────────

/// Per CPU: idle-CPU kicks deferred by a waker still running on this CPU.
static SYNC_KICK: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

/// A task-context wake made one task Ready. If the caller is a user task in a
/// syscall on this CPU (it returns to user space, or blocks, soon), defer the
/// idle-CPU kick: the flush at its syscall return pays it, and a switch-out
/// that leaves this CPU free to pick the wakee drops it. Else kick now.
pub fn wake_up_an_idle_cpu_sync() {
    let me = unsafe { crate::cpu_id() };
    if me < MAX_CPUS && crate::current_is_user_task(me) {
        SYNC_KICK[me].fetch_add(1, Ordering::Relaxed);
        bump(&SYNC_DEFER);
        return;
    }
    crate::wake_up_an_idle_cpu();
}

/// Pay this CPU's deferred kicks now (syscall return, tick, or the waker was
/// switched out still runnable).
#[inline]
pub fn flush_sync_kick() {
    let me = unsafe { crate::cpu_id() };
    if me >= MAX_CPUS { return; }
    if SYNC_KICK[me].load(Ordering::Relaxed) == 0 { return; }
    if SYNC_KICK[me].swap(0, Ordering::Relaxed) != 0 {
        bump(&SYNC_FLUSH);
        crate::wake_up_an_idle_cpu();
    }
}

/// The waker left the CPU Blocked: this CPU picks next, so a single deferred
/// kick is not needed. Returns the number of kicks still owed (more than one
/// wake deferred: pay them).
#[inline]
pub(crate) fn take_sync_kicks(cpu: usize) -> u32 {
    if cpu >= MAX_CPUS { return 0; }
    if SYNC_KICK[cpu].load(Ordering::Relaxed) == 0 { return 0; }
    SYNC_KICK[cpu].swap(0, Ordering::Relaxed)
}
