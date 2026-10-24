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

/// GPU fence waits that found a fence pending (drivers' `wait_fences`), for
/// the `f` line of `/proc/sysstat`: how much of each wait was a busy spin and
/// how much a park. Cumulative since boot. `FENCE_SPIN_US` is the spin budget
/// in effect (`drm.fencespin=<us>`), `u64::MAX` until the first wait reads it.
pub static FENCE_WAITS: AtomicU64 = AtomicU64::new(0);
/// Waits whose fence signalled during the spin (never parked).
pub static FENCE_SPIN_HITS: AtomicU64 = AtomicU64::new(0);
/// Wall ns spent spinning, summed over all waits.
pub static FENCE_SPIN_NS: AtomicU64 = AtomicU64::new(0);
/// Wall ns spent parked (after the spin), summed over all waits.
pub static FENCE_SLEEP_NS: AtomicU64 = AtomicU64::new(0);
/// Longest single spin, ns.
pub static FENCE_SPIN_MAX_NS: AtomicU64 = AtomicU64::new(0);
pub static FENCE_SPIN_US: AtomicU64 = AtomicU64::new(u64::MAX);

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
    record(tgid, nr & 0xffff, total, wall, ret == 0);
    let sub = nr >> 16;
    if sub != 0 { record(tgid, NR_SUB_BASE | (sub & 0xfff), total, wall, ret == 0); }
}

/// Rows hash by tgid alone and probe linearly, so a group's rows sit within
/// `PROBE` slots of `home(tgid)` and `free_tgid` scans only that window.
const PROBE: usize = 256;
#[inline]
fn home(tgid: u32) -> usize { ((tgid as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 50) as usize }

fn record(tgid: u32, nr: u32, ns: u64, wall: u64, zero: bool) {
    let key = ((tgid as u64) << 32) | (nr as u64 & 0xffff) | (1 << 31);
    let mut h = home(tgid);
    let mut tomb: Option<usize> = None;
    for _ in 0..PROBE {
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
    let h = home(tgid);
    for j in 0..PROBE {
        let i = (h + j) % SLOTS;
        let k = KEY[i].load(Relaxed);
        if k == 0 { break; } // end of chain: no row of ours lies beyond
        if k == TOMB || (k >> 32) as u32 != tgid { continue; }
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

/// Pseudo syscall number of the epoll park census row: `s <tgid> 65535
/// <parks> 0 0` counts the real sleeps a thread group's epoll waits took
/// (lane nowake). One epoll_wait that parks, is woken by something that does
/// not concern it and parks again counts 2, so a test can see the stray wakes
/// of ITS OWN waiter, whatever the rest of the system is doing.
pub const NR_EPOLL_PARK: u32 = 0xffff;

/// Pseudo syscall numbers of the epoll ready-list cache-miss census rows, by
/// cause: the thread's first ready-list epoll_wait (no snapshot yet); its
/// snapshot is of another epoll instance (a thread alternating instances);
/// its listener is off or was restarted (a poll()/select() park in between
/// ends it).
pub const NR_EPOLL_MISS_FIRST: u32 = 0xfffe;
pub const NR_EPOLL_MISS_INST: u32 = 0xfffd;
pub const NR_EPOLL_MISS_OFF: u32 = 0xfffc;

/// Pseudo syscall numbers `0xE000 | sub`: a syscall tagged with
/// `note_syscall_subkey(sub)` is recorded under its own nr AND here (lane
/// rddsys). ioctl tags: `0x800 | nr` for a DRM request (type 'd'), the low 11
/// bits of the request otherwise. So `s <tgid> 59497 ...` (0xE869) is
/// DRM_IOCTL_I915_GEM_EXECBUFFER2.
pub const NR_SUB_BASE: u32 = 0xE000;

/// Pseudo syscall number of the user page-fault row: `calls` = faults taken
/// from user mode (outside any syscall), `ns` = `wall` = their handling time
/// (charged as user time in `cpu_ns`/`sys_ns`, so NOT part of `sys_ns`).
pub const NR_UFAULT: u32 = 0xfffb;

/// Count one event of pseudo syscall `nr` for `tgid` taking `ns`.
#[inline]
pub fn note_ns(tgid: u32, nr: u32, ns: u64) { record(tgid, nr, ns, ns, false); }

/// Count one event of pseudo syscall `nr` for `tgid` (no time).
#[inline]
pub fn note(tgid: u32, nr: u32) { record(tgid, nr, 0, 0, false); }

/// Every census row: `(tgid, nr, calls, ns, zero_returns, wall_ns)`.
pub fn for_each(mut f: impl FnMut(u32, u32, u64, u64, u64, u64)) {
    for i in 0..SLOTS {
        let k = KEY[i].load(Relaxed);
        if k == 0 || k == TOMB { continue; }
        f((k >> 32) as u32, (k & 0xffff) as u32, CALLS[i].load(Relaxed),
          NS[i].load(Relaxed), ZERO[i].load(Relaxed), WALL[i].load(Relaxed));
    }
}
