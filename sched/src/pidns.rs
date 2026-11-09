//! Real PID namespaces (lane nspid). Kernel pids are global and never reused;
//! a namespace changes only how pids are *shown*: "identity except init". The
//! init of namespace `n` is kernel pid `PID_INIT[n]` and is seen as 1 inside
//! `n`; every other task keeps its kernel number, and tasks outside `n` (or
//! in a sibling namespace) are invisible from inside. Every mapping goes
//! through this module. All `n == 0` paths return before taking a lock.

use crate::ns::{self, NsId};
use crate::runqueue::RunQueue;
use core::sync::atomic::{AtomicBool, Ordering};

/// Set once any pid namespace has been created: lets cold paths skip lookups.
pub(crate) static USED: AtomicBool = AtomicBool::new(false);

/// Pid namespace of the calling CPU's current task (0 = initial).
#[inline]
pub fn cur() -> NsId {
    crate::CURRENT_PIDNS[unsafe { crate::cpu_id() }].load(Ordering::Relaxed)
}

/// Is `kn` equal to `n` or a descendant of it (so a viewer in `n` sees it)?
pub fn is_ancestor_or_eq(n: NsId, kn: NsId) -> bool {
    if n == 0 { return true; }
    let ln = ns::level(n);
    let mut k = kn;
    let mut steps = 0;
    while ns::level(k) > ln && steps <= ns::MAX_PIDNS_LEVEL as usize + 1 {
        k = ns::parent(k);
        steps += 1;
    }
    k == n
}

/// How a viewer in `n` sees kernel pid `k` living in namespace `kn`
/// (0 = invisible). Lock-free.
pub fn view(n: NsId, k: u32, kn: NsId) -> u32 {
    if n == 0 { return k; }
    if k == ns::pid_init(n) { return 1; }
    if is_ancestor_or_eq(n, kn) { k } else { 0 }
}

/// The caller's own tid/tgid `k` as the caller sees it.
#[inline]
pub fn self_v(k: u32) -> u32 {
    let n = cur();
    if n == 0 || k != ns::pid_init(n) { k } else { 1 }
}

pub fn pidns_of(k: u32) -> NsId {
    crate::RUN_QUEUE.lock().find_pid(k).map(|t| t.pidns.id()).unwrap_or(0)
}

/// Kernel pid `k` as seen from namespace `n`; a dead `k` stays visible.
pub fn to_v(n: NsId, k: u32) -> u32 {
    if n == 0 { return k; }
    let rq = crate::RUN_QUEUE.lock();
    to_v_rq(&rq, n, k)
}
pub fn to_v_rq(rq: &RunQueue, n: NsId, k: u32) -> u32 {
    if n == 0 { return k; }
    match rq.find_pid(k) {
        Some(t) => view(n, k, t.pidns.id()),
        None => if k == ns::pid_init(n) { 1 } else { k },
    }
}

/// A pid number `v` given by a task in namespace `n` → kernel pid.
/// `-3` (ESRCH) when it names nothing visible.
pub fn from_v(n: NsId, v: u32) -> Result<u32, isize> {
    if n == 0 { return Ok(v); }
    let rq = crate::RUN_QUEUE.lock();
    from_v_rq(&rq, n, v)
}
pub fn from_v_rq(rq: &RunQueue, n: NsId, v: u32) -> Result<u32, isize> {
    if n == 0 { return Ok(v); }
    let init = ns::pid_init(n);
    if v == 1 {
        return if init != 0 && rq.find_pid(init).is_some() { Ok(init) } else { Err(-3) };
    }
    if init != 0 && v == init { return Err(-3); }
    if let Some(t) = rq.find_pid(v) {
        if !is_ancestor_or_eq(n, t.pidns.id()) { return Err(-3); }
    }
    Ok(v)
}

/// `from_v` for the calling task's own namespace.
#[inline]
pub fn from_cur(v: u32) -> Result<u32, isize> {
    let n = cur();
    if n == 0 { Ok(v) } else { from_v(n, v) }
}

/// unshare(CLONE_NEWPID): the calling thread's *children* go to a new pid
/// namespace; its own pid does not change. 0 or a negative errno.
pub fn unshare_pid() -> isize {
    if !crate::caps::ns_capable(21) { return -1; }
    let pid = crate::current_pid();
    let parent = {
        let rq = crate::RUN_QUEUE.lock();
        match rq.find_pid(pid) {
            Some(t) => {
                if t.pidns_child.id() != t.pidns.id() { return -22; }
                t.pidns.id()
            }
            None => return -3,
        }
    };
    let new = match ns::new_pidns(parent) { Ok(n) => n, Err(e) => return e };
    let old = {
        let mut rq = crate::RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) {
            Some(t) => core::mem::replace(&mut t.pidns_child, new),
            None => return -3,
        }
    };
    drop(old);
    0
}

/// Called when process `tgid` is exiting (first thing in `reparent_children`).
/// If it is a namespace init, further forks into that namespace fail ENOMEM.
pub fn on_process_exit(tgid: u32) {
    let n = pidns_of(tgid);
    if n != 0 && ns::pid_init(n) == tgid {
        ns::set_pid_dead(n);
        zap(n, tgid);
    }
}

/// (active, for-children) pid namespaces of the calling task.
pub fn cur_and_child() -> (NsId, NsId) {
    crate::RUN_QUEUE.lock().find_pid(crate::current_pid())
        .map(|t| (t.pidns.id(), t.pidns_child.id())).unwrap_or((0, 0))
}

/// Namespace a new child of `parent` is placed in. `thread`: a CLONE_THREAD
/// sibling joins the parent's active namespace; otherwise pid_for_children
/// (or `explicit`, the namespace clone(NEWPID) just created). `-12` ENOMEM
/// when the target's init is gone.
pub fn fork_target(parent: u32, thread: bool, explicit: NsId) -> Result<NsId, isize> {
    let t = if explicit != 0 {
        explicit
    } else {
        match crate::RUN_QUEUE.lock().find_pid(parent) {
            Some(p) => if thread { p.pidns.id() } else { p.pidns_child.id() },
            None => 0,
        }
    };
    if t != 0 && ns::pid_dead(t) { return Err(-12); }
    Ok(t)
}

/// Just before the child is enqueued: the first child of a fresh namespace
/// becomes its init.
pub fn claim_init(target: NsId, child_pid: u32) {
    if target != 0 && ns::pid_init(target) == 0 { ns::claim_pid_init(target, child_pid); }
}

/// `si_pid` of a user-sent signal as the receiver (in namespace `tn`) sees the
/// sender (kernel pid in namespace `sn`). Only SI_USER/SI_TKILL (pid set by the kernel) are
/// translated; a user-supplied SI_QUEUE si_pid passes through as given; kernel-generated infos are returned untouched.
pub fn fix_info(mut info: crate::task::SigInfo, tn: NsId, sn: NsId) -> crate::task::SigInfo {
    if tn == 0 && sn == 0 { return info; }
    if matches!(info.si_code, 0 | -6) {
        info.si_pid = view(tn, info.si_pid as u32, sn) as i32;
    }
    info
}

/// Task `k`'s own number for itself (1 when it is its namespace's init): what
/// it writes into futex words and robust lists. Identity when the caller is
/// outside any pid namespace (avoids a RUN_QUEUE lookup).
pub fn own_v(k: u32) -> u32 {
    if cur() == 0 { return k; }
    let n = pidns_of(k);
    if n != 0 && ns::pid_init(n) == k { 1 } else { k }
}

/// Like `own_v` for a (possibly exiting, possibly other-CPU) task regardless of
/// the caller's namespace; falls back to `k` when the task is already gone.
pub fn own_v_of_task(k: u32) -> u32 {
    if !USED.load(Ordering::Relaxed) { return k; }
    let n = pidns_of(k);
    if n != 0 && ns::pid_init(n) == k { 1 } else { k }
}

// ---- P2 (lane nspidinit) ----

/// A pid argument of a by-pid syscall: 0 ("self") stays 0, else `from_cur`.
#[inline]
pub fn arg(v: u32) -> Result<u32, isize> {
    if v == 0 { Ok(0) } else { from_cur(v) }
}

/// A kernel pid/pgid/sid as the caller sees it (identity outside a namespace).
#[inline]
pub fn out(k: u32) -> u32 {
    let n = cur();
    if n == 0 { k } else { to_v(n, k) }
}

/// Should a user-sent `sig` from a task in `sender_ns` to `target` (a thread
/// group leader) be silently dropped? Only for a namespace init without a
/// handler: same-ns senders can't kill/stop it, ancestors may (SIGKILL/SIGSTOP).
pub fn init_drops(sender_ns: NsId, target: &crate::task::Task, sig: u32) -> bool {
    let tn = target.pidns.id();
    if tn == 0 || sig == 0 || sig > 64 || target.tgid != ns::pid_init(tn) { return false; }
    if target.signal_actions[(sig - 1) as usize].handler != 0 { return false; }
    if sender_ns == tn { return true; }
    !matches!(sig, 9 | 19)
}

/// Reaper for the orphans of exiting process `dead`: the nearest live
/// namespace init up the chain, else the global init (0 if none).
pub fn reaper_for(dead: u32) -> u32 {
    let mut n = pidns_of(dead);
    let mut steps = 0;
    while n != 0 && steps <= ns::MAX_PIDNS_LEVEL as usize + 1 {
        let i = ns::pid_init(n);
        if i != 0 && i != dead && !ns::pid_dead(n) {
            let rq = crate::RUN_QUEUE.lock();
            if rq.find_pid(i).map_or(false, |t| t.state != crate::task::TaskState::Zombie) { return i; }
        }
        n = ns::parent(n);
        steps += 1;
    }
    crate::init_pid()
}

/// Init `tgid` of namespace `n` is dying: SIGKILL every other process in `n`
/// and its descendants, then wait (bounded) until they are all zombies.
pub fn zap(n: NsId, tgid: u32) {
    // (tid, tgid) of every live task in the subtree: leaderless groups (main
    // thread exited, other threads running) have no live leader to find.
    let mut victims: alloc::vec::Vec<(u32, u32)> = alloc::vec::Vec::new();
    let collect = |v: &mut alloc::vec::Vec<(u32, u32)>| {
        v.clear();
        let rq = crate::RUN_QUEUE.lock();
        for i in rq.live_slots() {
            if let Some(t) = rq.get(i) {
                if t.tgid != tgid && is_ancestor_or_eq(n, t.pidns.id())
                    && t.state != crate::task::TaskState::Zombie && t.address_space.is_some() {
                    let _ = v.try_reserve(1);
                    if v.len() < v.capacity() { v.push((t.pid, t.tgid)); }
                }
            }
        }
    };
    let kill = |v: &[(u32, u32)]| {
        let mut last = 0u32;
        for &(tid, tg) in v.iter() {
            if tg == last { continue; }
            last = tg;
            let sig = || crate::task::SigInfo::user(tgid, 0);
            if crate::deliver_signal_process(tg, 9, sig()) != 0 {
                let _ = crate::deliver_signal(tid, 9, sig());
            }
        }
        // Threads of a leaderless group: signal each one directly.
        for &(tid, tg) in v.iter() {
            if tid != tg { let _ = crate::deliver_signal(tid, 9, crate::task::SigInfo::user(tgid, 0)); }
        }
    };
    collect(&mut victims);
    kill(&victims);
    let deadline = crate::monotonic_ns().saturating_add(5_000_000_000);
    loop {
        collect(&mut victims);
        if victims.is_empty() || crate::monotonic_ns() > deadline { return; }
        kill(&victims);
        crate::irq_window();
        crate::yield_now("pidns_zap");
    }
}
