//! System-call time accounting: the user/kernel split of `Task::cpu_ns`, and
//! a per-(thread group, syscall) census of calls and on-CPU kernel time.
//!
//! WHY. /proc/<pid>/stat reported all CPU time as utime (stime 0), so "the
//! panel burns 60 % during video" could not say whether that was the panel's
//! own code or the kernel servicing its syscalls (wake herds, re-probes,
//! lock spin) — lane deskbusy2, 2026-10-17.
//!
//! MODEL. Each CPU keeps the start of the current on-CPU segment of the
//! syscall it is servicing (`SEG_START`, 0 = not in one), the on-CPU time that
//! syscall already used before the segment (`SC_ACC`), and syscall time not
//! yet charged to the running task (`PENDING`). A task switched out
//! mid-syscall closes its segment and carries the syscall's number and
//! accumulated time in its `Task` (`sc_saved_*`); the next dispatch, on any
//! CPU, reopens it. So `sys_ns` is on-CPU time between syscall entry and exit
//! — blocking does not count — and is charged to the task under the same
//! RUN_QUEUE critical section that charges `cpu_ns`.
//!
//! Census: `/proc/sysstat` lines `tgid nr calls ns zero wall` (cumulative
//! since boot; diff two reads). `zero` counts returns of 0 (an epoll_wait/poll
//! that timed out or found nothing). `wall` is entry-to-exit wall-clock time
//! of the completed calls, so `wall - ns` is the time they spent blocked
//! (lane procfix: cosmic-comp's epoll µs/call against deskbusy2's 613 µs
//! baseline). The entry time follows the task across switches like `SC_ACC`
//! (`Task::sc_saved_t0`). Fixed open-addressed table; a thread group's rows are
//! freed (tombstoned) when its leader is reaped (`free_tgid`), overflow is counted.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use crate::MAX_CPUS;

static SEG_START: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static SC_ACC: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static PENDING: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Wall-clock entry time of the syscall the CPU is servicing.
static SC_T0: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

const SLOTS: usize = 16384;
static KEY: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static CALLS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static NS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static ZERO: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static WALL: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// Key of a freed row: probe chains continue past it, `record` reuses it.
const TOMB: u64 = u64::MAX;
pub static OVERFLOW: AtomicU64 = AtomicU64::new(0);

#[inline]
fn now() -> u64 { unsafe { crate::arch_monotonic_ns() } }

/// Syscall entry on `cpu`.
#[inline]
pub(crate) fn enter(cpu: usize) {
    SC_ACC[cpu].store(0, Relaxed);
    let t = now().max(1);
    SC_T0[cpu].store(t, Relaxed);
    SEG_START[cpu].store(t, Relaxed);
}

/// Syscall exit on `cpu`: charge the open segment, record the census row.
#[inline]
pub(crate) fn exit(cpu: usize, tgid: u32, nr: u32, ret: isize) {
    let s = SEG_START[cpu].swap(0, Relaxed);
    if s == 0 { return; } // dispatched fresh (fork child) — nothing open
    let t = now();
    let seg = t.saturating_sub(s);
    PENDING[cpu].fetch_add(seg, Relaxed);
    let total = SC_ACC[cpu].load(Relaxed) + seg;
    let wall = t.saturating_sub(SC_T0[cpu].load(Relaxed)).max(total);
    record(tgid, nr, total, wall, ret == 0);
}

fn record(tgid: u32, nr: u32, ns: u64, wall: u64, zero: bool) {
    let key = ((tgid as u64) << 32) | (nr as u64 & 0xffff) | (1 << 31);
    let mut h = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 50) as usize;
    let mut tomb: Option<usize> = None;
    for _ in 0..64 {
        let k = KEY[h].load(Relaxed);
        let mut slot = None;
        if k == key {
            slot = Some(h);
        } else if k == TOMB {
            if tomb.is_none() { tomb = Some(h); }
        } else if k == 0 {
            // End of chain: key absent. Prefer the first tombstone seen.
            let t = tomb.unwrap_or(h);
            let tk = if tomb.is_some() { TOMB } else { 0 };
            slot = KEY[t].compare_exchange(tk, key, Relaxed, Relaxed)
                .map_or_else(|cur| if cur == key { Some(t) } else { None }, |_| Some(t));
            if slot.is_none() { tomb = None; continue; } // lost a race: re-read this slot
        }
        if let Some(i) = slot {
            CALLS[i].fetch_add(1, Relaxed);
            NS[i].fetch_add(ns, Relaxed);
            WALL[i].fetch_add(wall, Relaxed);
            if zero { ZERO[i].fetch_add(1, Relaxed); }
            return;
        }
        h = (h + 1) % SLOTS;
    }
    // No empty slot within the probe window (tombstones fill a churned table):
    // the key is absent, so claim the first tombstone.
    if let Some(i) = tomb {
        if KEY[i].compare_exchange(TOMB, key, Relaxed, Relaxed).is_ok() {
            CALLS[i].fetch_add(1, Relaxed);
            NS[i].fetch_add(ns, Relaxed);
            WALL[i].fetch_add(wall, Relaxed);
            if zero { ZERO[i].fetch_add(1, Relaxed); }
            return;
        }
    }
    OVERFLOW.fetch_add(1, Relaxed);
}

/// Free every row of thread group `tgid` (its leader was reaped).
pub fn free_tgid(tgid: u32) {
    for i in 0..SLOTS {
        let k = KEY[i].load(Relaxed);
        if k == 0 || k == TOMB || (k >> 32) as u32 != tgid { continue; }
        CALLS[i].store(0, Relaxed);
        NS[i].store(0, Relaxed);
        ZERO[i].store(0, Relaxed);
        WALL[i].store(0, Relaxed);
        KEY[i].store(TOMB, Relaxed);
    }
}

/// Dispatch of a task onto `cpu` (scheduler context, RUN_QUEUE held by the
/// caller or not — only per-CPU state is touched). `saved_nr`/`saved_ns`/
/// `saved_t0`: the task's `sc_saved_*`.
#[inline]
pub(crate) fn on_dispatch(cpu: usize, saved_nr: u32, saved_ns: u64, saved_t0: u64, at: u64) {
    PENDING[cpu].store(0, Relaxed);
    if saved_nr != crate::NO_SYSCALL {
        SC_ACC[cpu].store(saved_ns, Relaxed);
        SC_T0[cpu].store(saved_t0, Relaxed);
        SEG_START[cpu].store(at.max(1), Relaxed);
    } else {
        SEG_START[cpu].store(0, Relaxed);
    }
}

/// Switch-back on `cpu` at time `at`: returns (syscall ns to charge to the
/// task, the syscall it is left in or `NO_SYSCALL`, that syscall's on-CPU ns
/// so far, its entry time). `cur_nr` is the per-CPU current-syscall number.
#[inline]
pub(crate) fn on_switch_back(cpu: usize, cur_nr: u32, at: u64) -> (u64, u32, u64, u64) {
    let s = SEG_START[cpu].swap(0, Relaxed);
    let mut pend = PENDING[cpu].swap(0, Relaxed);
    if s == 0 || cur_nr == crate::NO_SYSCALL {
        return (pend, crate::NO_SYSCALL, 0, 0);
    }
    let seg = at.saturating_sub(s);
    pend += seg;
    (pend, cur_nr, SC_ACC[cpu].load(Relaxed) + seg, SC_T0[cpu].load(Relaxed))
}

/// Every census row: `(tgid, nr, calls, ns, zero_returns, wall_ns)`.
pub fn for_each(mut f: impl FnMut(u32, u32, u64, u64, u64, u64)) {
    for i in 0..SLOTS {
        let k = KEY[i].load(Relaxed);
        if k == 0 || k == TOMB { continue; }
        f((k >> 32) as u32, (k & 0xffff) as u32, CALLS[i].load(Relaxed),
          NS[i].load(Relaxed), ZERO[i].load(Relaxed), WALL[i].load(Relaxed));
    }
}
