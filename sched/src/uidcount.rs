//! Per-uid task counts for RLIMIT_NPROC (lane wakescan).
//!
//! `fork_admission` used to count the caller's tasks by walking every live
//! task under RUN_QUEUE (~380 ns per Task dereference, ~600 tasks on the
//! desktop), and since RLIMIT_NPROC defaults to the finite `MAX_PROCESSES`
//! that walk ran on every fork and thread spawn of every non-root process.
//!
//! The count kept here is exactly what that walk counted: tasks in the run
//! queue table (`Task::slot` set) that are not Zombie, by real uid. The
//! writers are the only places those three things change: `RunQueue::enqueue`
//! / `remove`, `Task::set_state*` (Zombie in or out), and the real-uid writes
//! of setresuid/setreuid (`moved`). All run under RUN_QUEUE; the entries are
//! atomics only so the table needs no lock of its own.
//!
//! An entry is never freed (a count may sit at 0). If more distinct uids
//! appear than the table holds, `count` answers `None` from then on and the
//! caller walks, as before. `fork_admission` re-derives one answer in
//! `VERIFY_EVERY` by the walk and counts disagreements (`MISS`, must be 0).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

const SLOTS: usize = 128;
/// `(uid + 1) << 32 | count`; 0 = empty.
static TAB: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static FULL: AtomicBool = AtomicBool::new(false);

pub static FAST: AtomicU64 = AtomicU64::new(0);
pub static WALKED: AtomicU64 = AtomicU64::new(0);
pub static MISS: AtomicU64 = AtomicU64::new(0);
pub const VERIFY_EVERY: u64 = 64;

fn entry(uid: u32, create: bool) -> Option<&'static AtomicU64> {
    let tag = (uid as u64 + 1) << 32;
    let h = (uid as usize).wrapping_mul(0x9E37_79B9) % SLOTS;
    for k in 0..SLOTS {
        let e = &TAB[(h + k) % SLOTS];
        let v = e.load(Relaxed);
        if v & !0xFFFF_FFFF == tag { return Some(e); }
        if v == 0 {
            if !create { return None; }
            match e.compare_exchange(0, tag, Relaxed, Relaxed) {
                Ok(_) => return Some(e),
                Err(v2) if v2 & !0xFFFF_FFFF == tag => return Some(e),
                Err(_) => continue,
            }
        }
    }
    if create { FULL.store(true, Relaxed); }
    None
}

/// One more live (non-Zombie, in-table) task of `uid`.
#[inline]
pub(crate) fn inc(uid: u32) {
    if let Some(e) = entry(uid, true) { e.fetch_add(1, Relaxed); }
}

/// One fewer live task of `uid`.
#[inline]
pub(crate) fn dec(uid: u32) {
    if let Some(e) = entry(uid, false) {
        // Never wrap into the uid half: a count is only ever decremented
        // after its increment, but a bug must not corrupt the key.
        let _ = e.fetch_update(Relaxed, Relaxed, |v| if v & 0xFFFF_FFFF == 0 { None } else { Some(v - 1) });
    }
}

/// A live task's real uid changed from `old` to `new`.
#[inline]
pub(crate) fn moved(old: u32, new: u32) {
    if old != new { dec(old); inc(new); }
}

/// Live tasks of `uid`, or `None` when the table overflowed (walk instead).
pub fn count(uid: u32) -> Option<u64> {
    if FULL.load(Relaxed) { return None; }
    Some(entry(uid, false).map_or(0, |e| e.load(Relaxed) & 0xFFFF_FFFF))
}
