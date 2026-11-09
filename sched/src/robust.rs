//! Robust futex lists (`set_robust_list(2)` / `get_robust_list(2)`).
//!
//! A thread registers the head of a user-space list of the robust mutexes it
//! holds (Linux `struct robust_list_head`: `list` — the first entry, or the
//! head itself when empty; `futex_offset` — from an entry to its lock word;
//! `list_op_pending` — an entry being locked or unlocked right now). Bit 0 of
//! an entry pointer marks a PI mutex. When the thread dies (or execs), the
//! kernel walks that list: every lock word still naming the thread's TID gets
//! FUTEX_OWNER_DIED (keeping FUTEX_WAITERS, owner cleared) and one waiter is
//! woken, so the next locker takes it and reports EOWNERDEAD instead of
//! waiting forever — Linux `exit_robust_list` / `handle_futex_death`.
//!
//! The walk needs the dead thread's address space and runs in task context,
//! with no lock held (the user accesses fault like any other). A thread that
//! exits itself walks its own list in `exit`. A sibling killed by a group
//! exit or an exec never runs again: its head is stashed here, under its
//! TID, when it is marked dead, and the surviving thread of the group (the
//! one running the kill, which exits or execs next while the address space
//! is still the group's) walks it.

use spin::Mutex;

use crate::uaccess::{read_user, user_range_ok};
use crate::Pid;

/// `sizeof(struct robust_list_head)` on a 64-bit ABI.
pub const HEAD_LEN: usize = 24;
/// Linux's `ROBUST_LIST_LIMIT`: a cyclic or absurd list ends the walk.
const LIST_LIMIT: usize = 2048;

const FUTEX_WAITERS: u32 = 0x8000_0000;
const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
const FUTEX_TID_MASK: u32 = 0x3fff_ffff;

/// Heads of dead threads that never walked their own list: (tgid, tid, head).
const ORPHAN_SLOTS: usize = crate::runqueue::MAX_TASKS;
static ORPHANS: Mutex<[(Pid, Pid, usize); ORPHAN_SLOTS]> = Mutex::new([(0, 0, 0); ORPHAN_SLOTS]);

/// Record a killed thread's list for its group's survivor (see the module
/// docs). Called with `RUN_QUEUE` held; this lock is a leaf.
pub(crate) fn stash(tgid: Pid, tid: Pid, head: usize) {
    if head == 0 { return; }
    let mut o = ORPHANS.lock();
    if let Some(s) = o.iter_mut().find(|s| s.2 == 0) { *s = (tgid, tid, head); }
}

/// `set_robust_list(head, len)` for the calling thread.
pub fn set(head: usize, len: usize) -> isize {
    if len != HEAD_LEN { return -22; } // EINVAL
    let pid = crate::current_pid();
    if let Some(t) = crate::RUN_QUEUE.lock().find_pid_mut(pid) { t.robust_list = head; }
    0
}

/// The robust-list head of thread `tid` (0 = the caller); None if no such
/// thread.
pub fn get(tid: Pid) -> Option<usize> {
    let tid = if tid == 0 { crate::current_pid() } else { tid };
    crate::RUN_QUEUE.lock().find_pid(tid).map(|t| t.robust_list)
}

/// The calling thread is exiting or exec'ing: walk (and drop) its own list,
/// then those of its killed siblings. Its address space must still be the
/// one the lists live in.
pub fn release_current() {
    let pid = crate::current_pid();
    let tgid = crate::current_tgid();
    let head = {
        let mut rq = crate::RUN_QUEUE.lock();
        rq.find_pid_mut(pid).map(|t| core::mem::take(&mut t.robust_list)).unwrap_or(0)
    };
    if head != 0 { walk(head, pid); }
    release_orphans(tgid);
}

/// Walk every stashed list of group `tgid` (see [`stash`]).
pub fn release_orphans(tgid: Pid) {
    loop {
        let next = {
            let mut o = ORPHANS.lock();
            o.iter_mut().find(|s| s.2 != 0 && s.0 == tgid).map(|s| core::mem::replace(s, (0, 0, 0)))
        };
        let Some((_, tid, head)) = next else { break };
        walk(head, tid);
    }
}

/// One entry pointer: (address, is PI).
fn fetch_entry(at: usize) -> Option<(usize, bool)> {
    let v: usize = read_user(at)?;
    Some((v & !1, v & 1 != 0))
}

/// Linux `exit_robust_list` for the list at `head`, owned by `tid`.
fn walk(head: usize, tid: Pid) {
    // The lock words hold the dying thread's own view of its tid.
    let tid = crate::pidns::own_v_of_task(tid);
    if head & 7 != 0 || !user_range_ok(head, HEAD_LEN) { return; }
    let Some((mut entry, mut pi)) = fetch_entry(head) else { return };
    let Some(offset) = read_user::<isize>(head + 8) else { return };
    let Some((pending, ppi)) = fetch_entry(head + 16) else { return };
    let mut n = 0;
    while entry != head && entry != 0 {
        // Fetch the next entry first: the death handling may let a waiter
        // free or reuse this one.
        let next = fetch_entry(entry);
        if entry != pending {
            handle_death(entry.wrapping_add_signed(offset), tid, pi, false);
        }
        let Some((e, p)) = next else { return };
        entry = e;
        pi = p;
        n += 1;
        if n >= LIST_LIMIT { break; }
    }
    if pending != 0 {
        handle_death(pending.wrapping_add_signed(offset), tid, ppi, true);
    }
}

/// Linux `handle_futex_death`: the lock word at `uaddr` still names `tid`.
fn handle_death(uaddr: usize, tid: Pid, pi: bool, pending_op: bool) {
    if uaddr & 3 != 0 || !user_range_ok(uaddr, 4) { return; }
    // Only a writable mapping: anything else is garbage in the list, and the
    // store below must not fault unresolvably.
    let writable = crate::with_current_address_space(|a| {
        a.find(uaddr).map_or(false, |v| v.prot & mm::vmm::PROT_WRITE != 0)
    }).unwrap_or(false);
    if !writable { return; }
    crate::uaccess::prefault(uaddr, 4, false);
    // An aligned user address in a writable mapping of the current address
    // space, accessed with no lock held, through the fault-tolerant helpers.
    loop {
        let Some(uval) = crate::uaccess::load_user_u32(uaddr) else { return };
        // Died between taking the lock in user space and recording it
        // (list_op_pending, word still 0): a waiter may have been passed
        // over by the unlock it raced; wake one so it re-checks.
        if pending_op && !pi && uval == 0 {
            crate::futex::futex_wake(uaddr, 1);
            return;
        }
        if uval & FUTEX_TID_MASK != tid { return; }
        let mval = (uval & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
        let Some(seen) = crate::uaccess::cmpxchg_user_u32(uaddr, uval, mval) else { return };
        if seen == uval {
            // PI lockers here wait on the ordinary futex queue too
            // (sched::futex), so both kinds are woken the same way.
            if uval & FUTEX_WAITERS != 0 { crate::futex::futex_wake(uaddr, 1); }
            return;
        }
    }
}
