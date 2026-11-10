//! Credential-based permission checks — Linux credentials(7), kill(2),
//! setpgid(2), setreuid(2), setpriority(2), setrlimit(2), sched_setscheduler(2).
//!
//! There are no capability sets: root (euid 0) holds every capability and
//! everyone else holds none, so "CAP_KILL", "CAP_SYS_NICE", … all read
//! `euid == 0` here.
//!
//! Every check runs under one RUN_QUEUE hold together with the lookup of the
//! target, so the answer cannot go stale between "may I?" and "do it" for the
//! state this module itself changes.
//!
//! The kernel's own signal sources (tty job control, SIGCHLD, timers, VT
//! release/acquire) call `deliver_signal*`/`kill_pgrp` directly and are not
//! subject to these checks — exactly as in-kernel `send_sig` is not on Linux.

use crate::task::{self, Pid, Task, TaskState};
use crate::{runqueue, RUN_QUEUE, current_pid, deliver_signal, deliver_signal_process, init_pid};

const EPERM: isize = -1;
const ESRCH: isize = -3;
const EACCES: isize = -13;
const EINVAL: isize = -22;

const SIGCONT: u32 = 18;

/// Snapshot of the identity a check needs from the calling thread.
#[derive(Clone, Copy)]
struct Caller { tgid: Pid, sid: Pid, uid: u32, euid: u32, gid: u32, pidns: crate::ns::NsId, root: bool,
    /// Caller's user namespace and effective caps, for `cap_over` (the RUN_QUEUE is held, so
    /// `caps::ns_capable_in` cannot be called).
    cns: crate::ns::NsId, caps: u64 }

fn caller(rq: &runqueue::RunQueue) -> Option<Caller> {
    rq.find_pid(current_pid()).map(|t| Caller {
        tgid: t.tgid, sid: t.sid,
        uid: t.uid, euid: t.euid, gid: t.gid, pidns: t.pidns.id(),
        root: t.euid == 0 && t.userns.id() == 0,
        cns: t.userns.id(), caps: t.caps[0],
    })
}

const CAP_KILL: u32 = 5;
const CAP_SYS_PTRACE: u32 = 19;
const CAP_SYS_NICE: u32 = 23;

/// Linux `ns_capable(target->cred->user_ns, cap)` for the caller snapshot: host root, or the cap
/// in the caller's own namespace when it is the TARGET's namespace or an ancestor of it (or the
/// caller owns the target's namespace). A capability in a sibling or child namespace is not power
/// over the target.
fn cap_over(c: &Caller, t: &Task, cap: u32) -> bool {
    c.root || crate::caps::capable_snap(c.cns, c.euid, c.caps, t.userns.id(), cap)
}

/// Linux `kill_ok_by_cred` plus the two exemptions of `check_kill_permission`:
/// the caller's own thread group, and SIGCONT within the caller's session.
fn may_signal(c: &Caller, t: &Task, sig: u32) -> bool {
    if t.tgid == c.tgid { return true; }
    if cap_over(c, t, CAP_KILL) { return true; } // CAP_KILL in the target's userns (host root included)
    if c.euid == t.suid || c.euid == t.uid || c.uid == t.suid || c.uid == t.uid { return true; }
    sig == SIGCONT && t.sid == c.sid
}

/// Permission and existence check for a process-directed signal (`kill(pid)`,
/// `rt_sigqueueinfo`). 0 = allowed, else -ESRCH / -EPERM. `sig == 0` (the
/// existence probe) is checked like any other signal, as on Linux.
pub fn kill_check(tgid: Pid, sig: u32) -> isize { let r = kill_check_ns(tgid, sig).0; if r == DROPPED { 0 } else { r } }

/// Internal marker: permitted but silently dropped (namespace init).
const DROPPED: isize = isize::MIN;

/// `kill_check` plus the target's pid namespace and the caller's (for `si_pid`).
/// A target outside the caller's namespace subtree does not exist (ESRCH).
fn kill_check_ns(tgid: Pid, sig: u32) -> (isize, crate::ns::NsId, crate::ns::NsId) {
    let rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return (ESRCH, 0, 0) };
    match rq.find_pid(tgid) {
        Some(t) if t.pid == t.tgid => {
            let tn = t.pidns.id();
            if !crate::pidns::is_ancestor_or_eq(c.pidns, tn) { return (ESRCH, tn, c.pidns); }
            let r = if may_signal(&c, t, sig) { 0 } else { EPERM };
            // A namespace init without a handler ignores this signal: dropped, success.
            if r == 0 && sig != 0 && crate::pidns::init_drops(c.pidns, t, sig) { return (DROPPED, tn, c.pidns); }
            (r, tn, c.pidns)
        }
        _ => (ESRCH, 0, c.pidns),
    }
}

/// `kill(pid > 0, sig)`.
pub fn kill_process(tgid: Pid, sig: u32, info: task::SigInfo) -> isize {
    let (r, tn, sn) = kill_check_ns(tgid, sig);
    if r == DROPPED { return 0; }
    if r != 0 || sig == 0 { return r; }
    deliver_signal_process(tgid, sig, crate::pidns::fix_info(info, tn, sn))
}

/// `tkill(tid)` / `tgkill(tgid, tid)` (`tgid == None` for tkill): the thread
/// must exist (and belong to `tgid`), then the usual kill permission against
/// its process. Signal 0 stops after the checks.
pub fn kill_thread(tgid: Option<Pid>, tid: Pid, sig: u32, info: task::SigInfo) -> isize {
    let (tn, sn);
    {
        let rq = RUN_QUEUE.lock();
        let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
        let t = match rq.find_pid(tid) { Some(t) => t, None => return ESRCH };
        if let Some(g) = tgid { if t.tgid != g { return ESRCH; } }
        if !crate::pidns::is_ancestor_or_eq(c.pidns, t.pidns.id()) { return ESRCH; }
        let owner = rq.find_pid(t.tgid).unwrap_or(t);
        if !may_signal(&c, owner, sig) { return EPERM; }
        if sig != 0 && crate::pidns::init_drops(c.pidns, owner, sig) { return 0; }
        tn = t.pidns.id();
        sn = c.pidns;
    }
    if sig == 0 { return 0; }
    deliver_signal(tid, sig, crate::pidns::fix_info(info, tn, sn))
}

/// `kill(-pgid, sig)` / `killpg`: every process in the group the caller may
/// signal gets it. Linux `__kill_pgrp_info`: 0 if at least one delivery
/// succeeded, else the last error (EPERM); ESRCH for an empty group.
pub fn kill_pgrp_checked(pgid: Pid, sig: u32, info: task::SigInfo) -> isize {
    // Heap, reserved before RUN_QUEUE (16 KiB as a stack array at 4096 tasks).
    let mut targets: alloc::vec::Vec<(Pid, crate::ns::NsId)> = alloc::vec::Vec::new();
    if targets.try_reserve_exact(runqueue::MAX_TASKS).is_err() { return -12; } // ENOMEM
    let mut found = false;
    let mut dropped = false;
    let mut sn = 0;
    {
        let rq = RUN_QUEUE.lock();
        let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
        sn = c.pidns;
        for i in rq.live_slots() {
            if let Some(t) = rq.get(i) {
                if t.pgid != pgid || t.pid != t.tgid { continue; }
                if !crate::pidns::is_ancestor_or_eq(c.pidns, t.pidns.id()) { continue; }
                found = true;
                if may_signal(&c, t, sig) && sig != 0 && crate::pidns::init_drops(c.pidns, t, sig) {
                    dropped = true;
                } else if may_signal(&c, t, sig) && targets.len() < targets.capacity() {
                    targets.push((t.pid, t.pidns.id()));
                }
            }
        }
    }
    if !found { return ESRCH; }
    if targets.is_empty() { return if dropped { 0 } else { EPERM }; }
    if sig != 0 {
        for &(pid, tn) in targets.iter() {
            let _ = deliver_signal_process(pid, sig, crate::pidns::fix_info(info, tn, sn));
        }
    }
    0
}

/// `kill(-1, sig)`: every process except init and the caller's own, that the
/// caller may signal. Linux `kill_something_info(-1)` counts every candidate
/// but ignores EPERM in its result, so this is 0 whenever any other process
/// exists — even if none of them could be signalled — and ESRCH otherwise.
/// Kernel tasks (no address space) and zombies are not candidates.
pub fn kill_all_checked(sig: u32, info: task::SigInfo) -> isize {
    let init = init_pid();
    let mut targets: alloc::vec::Vec<(Pid, crate::ns::NsId)> = alloc::vec::Vec::new();
    if targets.try_reserve_exact(runqueue::MAX_TASKS).is_err() { return -12; } // ENOMEM
    let mut count = 0;
    let sn;
    {
        let rq = RUN_QUEUE.lock();
        let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
        sn = c.pidns;
        for i in rq.live_slots() {
            if let Some(t) = rq.get(i) {
                if t.pid != t.tgid || t.tgid == init || t.tgid == c.tgid { continue; }
                if !crate::pidns::is_ancestor_or_eq(c.pidns, t.pidns.id()) { continue; }
                // The caller namespace's own init is skipped like the global one.
                if c.pidns != 0 && t.tgid == crate::ns::pid_init(c.pidns) { continue; }
                if t.address_space.is_none() || t.state == TaskState::Zombie { continue; }
                count += 1;
                if may_signal(&c, t, sig) && !(sig != 0 && crate::pidns::init_drops(c.pidns, t, sig))
                    && targets.len() < targets.capacity() {
                    targets.push((t.pid, t.pidns.id()));
                }
            }
        }
    }
    if count == 0 { return ESRCH; }
    if sig != 0 {
        for &(pid, tn) in targets.iter() {
            let _ = deliver_signal_process(pid, sig, crate::pidns::fix_info(info, tn, sn));
        }
    }
    0
}

/// getsid(pid): the session of `pid` (0 = the caller), ESRCH if none.
pub fn sid_of(pid: Pid) -> isize {
    let rq = RUN_QUEUE.lock();
    let pid = if pid == 0 { current_pid() } else { pid };
    match rq.find_pid(pid) {
        Some(t) => t.sid as isize,
        None => crate::exit_logged_sid(pid).map_or(ESRCH, |s| s as isize),
    }
}

/// setpgid(2) with Linux's rules (`ksys_setpgid`): the target is the caller or
/// one of its children (else ESRCH); a child in another session, or any
/// session leader, is EPERM; joining an existing group requires that group to
/// live in the caller's session (EPERM). `pid`/`pgid` 0 mean "the caller" /
/// "the target's own pid". Not modelled: EACCES for a child that has exec'd.
pub fn set_pgid_checked(pid_raw: i32, pgid_raw: i32) -> isize {
    if pid_raw < 0 || pgid_raw < 0 { return EINVAL; }
    let mut rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
    let pid = if pid_raw == 0 { c.tgid } else { pid_raw as Pid };
    let pgid = if pgid_raw == 0 { pid } else { pgid_raw as Pid };
    let (t_tgid, t_sid, t_parent) = match rq.find_pid(pid) {
        Some(t) => {
            if t.pid != t.tgid { return EINVAL; } // a thread id, not a process
            let parent = rq.find_pid(t.ppid).map(|p| p.tgid).unwrap_or(t.ppid);
            (t.tgid, t.sid, parent)
        }
        None => {
            drop(rq);
            // Already exited but not yet reaped: keep the historical
            // record-only update for the caller's own children.
            return if crate::set_pgid_exited_child(pid, pgid, c.tgid) { 0 } else { ESRCH };
        }
    };
    if t_parent == c.tgid && t_tgid != c.tgid {
        if t_sid != c.sid { return EPERM; }
    } else if t_tgid != c.tgid {
        return ESRCH;
    }
    if t_sid == t_tgid { return EPERM; } // a session leader cannot move
    if pgid != pid {
        let mut ok = false;
        for i in rq.live_slots() {
            if let Some(t) = rq.get(i) {
                if t.pgid == pgid && t.sid == c.sid { ok = true; break; }
            }
        }
        // A zombie keeps its group alive until it is waited for, as on Linux,
        // but its run-queue slot is recycled the moment it dies; only the
        // exit record is left. `a | b` in brush puts `b` in `a`'s group, and a
        // fast `a` (cat moving a small file with splice) is a zombie by then:
        // without this, `b`'s setpgid failed with EPERM and the command with it.
        if !ok { ok = crate::exit_logged_pgid(pgid, c.sid); }
        if !ok { return EPERM; }
    }
    for i in rq.live_slots() {
        if let Some(t) = rq.get_mut(i) {
            if t.tgid == t_tgid { t.pgid = pgid; }
        }
    }
    0
}

/// setreuid(2) / setregid(2) (`gid == true`). u32::MAX = unchanged. Linux
/// rules: without privilege the real id may become the current real or
/// effective id, the effective id any of real/effective/saved; the saved id
/// follows the new effective id whenever the real id is set or the effective
/// id is set to something other than the old real id. EPERM changes nothing.
pub fn set_re_ids(r: u32, e: u32, gid: bool) -> isize {
    let mut rq = RUN_QUEUE.lock();
    let pid = current_pid();
    let t = match rq.find_pid_mut(pid) { Some(t) => t, None => return ESRCH };
    let before = [t.euid, t.egid, t.fsuid, t.fsgid];
    let old_uids = (t.uid, t.euid, t.suid);
    let privileged = crate::setid_priv(t, gid); // CAP_SETUID / CAP_SETGID
    let (or, oe, os) = if gid { (t.gid, t.egid, t.sgid) } else { (t.uid, t.euid, t.suid) };
    let (mut nr, mut ne, mut ns) = (or, oe, os);
    if r != u32::MAX {
        if !privileged && r != or && r != oe { return EPERM; }
        nr = r;
    }
    if e != u32::MAX {
        if !privileged && e != or && e != oe && e != os { return EPERM; }
        ne = e;
    }
    if r != u32::MAX || (e != u32::MAX && e != or) { ns = ne; }
    // The filesystem id follows the effective one (Linux `__sys_setreuid`).
    if gid { t.gid = nr; t.egid = ne; t.sgid = ns; t.fsgid = ne; }
    else   { crate::uidcount::moved(t.uid, nr); t.uid = nr; t.euid = ne; t.suid = ns; t.fsuid = ne; crate::caps::on_uid_change(t, old_uids); }
    crate::commit_id_change(&mut rq, pid, before);
    0
}

/// Linux `set_one_prio_perm`: root, or the caller's euid is the target's real
/// or effective uid.
fn same_owner(c: &Caller, t: &Task) -> bool {
    c.euid == t.uid || c.euid == t.euid || cap_over(c, t, CAP_SYS_NICE)
}

fn leader_rlimit(rq: &runqueue::RunQueue, tgid: Pid, res: usize) -> u64 {
    rq.find_pid(tgid).map(|l| l.rlimits[res][0]).unwrap_or(task::RLIM_INFINITY)
}

/// setpriority(2) for every task `matches` selects. Per task: EPERM unless
/// same owner; EACCES when lowering the nice value beyond what RLIMIT_NICE
/// allows (`20 - nice <= rlim`, Linux `can_nice`), root exempt. Result as
/// Linux: ESRCH if nothing matched, else the last error, else 0.
pub fn set_nice_checked(matches: impl Fn(&Task) -> bool, nice: i8) -> isize {
    let nice = nice.clamp(-20, 19);
    let weight = task::nice_to_weight(nice);
    let mut rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
    let rlim_nice = leader_rlimit(&rq, c.tgid, task::RLIMIT_NICE);
    let can_nice = c.root || (20 - nice as i64) as u64 <= rlim_nice;
    let mut err = ESRCH;
    for i in rq.live_slots() {
        let (m, owner_ok, lowering) = match rq.get(i) {
            Some(t) if matches(t) => (true, same_owner(&c, t), nice < t.priority),
            _ => (false, false, false),
        };
        if !m { continue; }
        if err == ESRCH { err = 0; }
        if !owner_ok { err = EPERM; continue; }
        if lowering && !can_nice { err = EACCES; continue; }
        if let Some(t) = rq.get_mut(i) {
            t.priority = nice;
            t.weight   = weight;
        }
        rq.place_home(i);
    }
    err
}

/// sched_setscheduler(2) / sched_setparam(2) (`policy == None`) admission.
/// Nothing is stored — every task runs under the fair class — but the answer
/// is Linux's: EINVAL for a bad policy/priority pair, ESRCH for a missing
/// target, EPERM for a target with another owner, and EPERM for a real-time
/// request beyond the target's RLIMIT_RTPRIO (0 by default) unless root.
pub fn sched_policy_check(pid: Pid, policy: Option<u32>, prio: i32) -> isize {
    const SCHED_OTHER: u32 = 0; const SCHED_FIFO: u32 = 1; const SCHED_RR: u32 = 2;
    const SCHED_BATCH: u32 = 3; const SCHED_IDLE: u32 = 5;
    let rt = match policy {
        Some(SCHED_FIFO) | Some(SCHED_RR) => true,
        Some(SCHED_OTHER) | Some(SCHED_BATCH) | Some(SCHED_IDLE) | None => false,
        Some(_) => return EINVAL,
    };
    if rt { if !(1..=99).contains(&prio) { return EINVAL; } }
    else if prio != 0 { return EINVAL; }
    let rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
    let pid = if pid == 0 { current_pid() } else { pid };
    let t = match rq.find_pid(pid) { Some(t) => t, None => return ESRCH };
    if c.root { return 0; }
    if !same_owner(&c, t) { return EPERM; }
    if rt && (prio as u64) > leader_rlimit(&rq, t.tgid, task::RLIMIT_RTPRIO) { return EPERM; }
    0
}

/// sched_setaffinity(2) admission: the target must exist and share our owner.
pub fn sched_affinity_check(pid: Pid) -> isize { sched_setaffinity(pid, None) }

/// sched_setaffinity(2): store `mask` (`None` = admission check only) on
/// thread `pid` (0 = the caller), for sched_getaffinity to report.
pub fn sched_setaffinity(pid: Pid, mask: Option<u64>) -> isize {
    let mut rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
    let pid = if pid == 0 { current_pid() } else { pid };
    let ok = match rq.find_pid(pid) {
        Some(t) => same_owner(&c, t),
        None => return ESRCH,
    };
    if !ok { return EPERM; }
    if let (Some(m), Some(idx)) = (mask, rq.find_pid_idx(pid)) {
        rq.set_affinity(idx, m);
    }
    0
}

/// The stored sched_setaffinity mask of `pid` (0 = the caller): 0 = all CPUs.
pub fn sched_affinity_of(pid: Pid) -> Result<u64, isize> {
    let rq = RUN_QUEUE.lock();
    let pid = if pid == 0 { current_pid() } else { pid };
    rq.find_pid(pid).map(|t| t.affinity).ok_or(ESRCH)
}

/// prlimit64(2) / getrlimit / setrlimit. `pid` 0 = the caller. Reading
/// another process needs the same real/effective/saved uid AND gid as ours
/// (Linux `check_prlimit_permission`), or root. A new limit with cur > max
/// is EINVAL; raising a hard limit is root-only (CAP_SYS_RESOURCE).
/// Returns the old limit (or the error) — the old value is valid on success.
pub fn prlimit(pid: Pid, res: usize, new: Option<[u64; 2]>) -> Result<[u64; 2], isize> {
    if res >= task::RLIM_NLIMITS { return Err(EINVAL); }
    if let Some([cur, max]) = new { if cur > max { return Err(EINVAL); } }
    // No descriptor limit past nr_open, for root either (Linux do_prlimit).
    if let Some([_, max]) = new {
        if res == task::RLIMIT_NOFILE && max > task::NR_OPEN { return Err(EPERM); }
    }
    let mut rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return Err(ESRCH) };
    let tgid = if pid == 0 { c.tgid } else {
        match rq.find_pid(pid) {
            Some(t) => {
                if t.tgid != c.tgid && !cap_over(&c, t, CAP_SYS_PTRACE)
                    && !(c.uid == t.uid && c.uid == t.euid && c.uid == t.suid
                         && c.gid == t.gid && c.gid == t.egid && c.gid == t.sgid) {
                    return Err(EPERM);
                }
                t.tgid
            }
            None => return Err(ESRCH),
        }
    };
    let l = match rq.find_pid_mut(tgid) { Some(l) => l, None => return Err(ESRCH) };
    let old = l.rlimits[res];
    if let Some(nl) = new {
        if nl[1] > old[1] && !c.root { return Err(EPERM); }
        l.rlimits[res] = nl;
        if res == task::RLIMIT_CPU && nl[0] != task::RLIM_INFINITY {
            super::CPU_RLIMIT_SET.store(true, core::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(old)
}

/// ([ruid, euid, suid, fsuid], [rgid, egid, sgid, fsgid]) of `pid`, for
/// /proc/<pid>/status.
pub fn ids_of(pid: Pid) -> Option<([u32; 4], [u32; 4])> {
    RUN_QUEUE.lock().find_pid(pid)
        .map(|t| ([t.uid, t.euid, t.suid, t.fsuid], [t.gid, t.egid, t.sgid, t.fsgid]))
}

/// Linux `task_dump_owner` for /proc/<pid>: the task's effective kuid/kgid;
/// a non-dumpable task (its thread group leader's flag) shows the root of its
/// user namespace instead (kuid 0 when that root is unmapped). Kernel ids:
/// the reader's view translation happens on the copy to user memory.
pub fn proc_dump_owner(pid: Pid) -> (u32, u32) {
    let rq = RUN_QUEUE.lock();
    let Some(t) = rq.find_pid(pid) else { return (0, 0) };
    let dumpable = rq.find_pid(t.tgid).unwrap_or(t).dumpable;
    if dumpable { return (t.euid, t.egid); }
    let ns = t.userns.id();
    (crate::ns::to_k(0, ns, 0).unwrap_or(0), crate::ns::to_k(1, ns, 0).unwrap_or(0))
}

/// May the calling thread put a message on a queue port owned by `owner`
/// (SYS_IPC_SEND / SYS_IPC_CALL, and a caller-chosen `reply_port`)?
/// 0 = allowed, else -EPERM.
///
/// A queued message is indistinguishable from one the owner's peer sent: a
/// reply port is where the VFS/net servers' answers land, so a foreign
/// message there is a forged syscall result. The rule is therefore
/// `ptrace_may_access`, not `kill`: root, the owner's own process, or a
/// caller whose euid/egid is every one of the owner's real, effective and
/// saved ids. The `kill` rule (caller's ruid == target's ruid) would let a
/// user forge the VFS replies a set-user-ID program it started (su, sudo)
/// waits for. A port whose owner task is gone is reachable by root only.
///
/// Server ports (a kernel handler is registered) are not checked here: the
/// handler sees the caller's pid and applies the operation's own checks.
pub fn ipc_send_check(owner: Pid) -> isize {
    let rq = RUN_QUEUE.lock();
    let c = match rq.find_pid(current_pid()) { Some(t) => t, None => return EPERM };
    if c.euid == 0 && c.userns.id() == 0 { return 0; }
    let t = match rq.find_pid(owner) { Some(t) => t, None => return EPERM };
    if t.tgid == c.tgid { return 0; }
    if crate::caps::capable_snap(c.userns.id(), c.euid, c.caps[0], t.userns.id(), CAP_SYS_PTRACE) { return 0; }
    let u = c.euid;
    let g = c.egid;
    if t.uid == u && t.euid == u && t.suid == u && t.gid == g && t.egid == g && t.sgid == g {
        0
    } else {
        EPERM
    }
}

/// Linux `__ptrace_may_access(target, PTRACE_MODE_ATTACH_REALCREDS)`, the
/// check pidfd_getfd(2) applies: the caller's own thread group always; else
/// the caller's REAL uid must be the target's real, effective and saved uid,
/// its real gid the target's real, effective and saved gid, and the target
/// must be dumpable (it has not changed ids or exec'd a set-ID image since its
/// last ordinary exec). Root (CAP_SYS_PTRACE) passes both. 0 = allowed, else
/// -EPERM, or -ESRCH for a target that is gone.
///
/// The old rule ("same real uid") let a user take descriptors out of a
/// set-user-ID program it started (real uid = the user, effective = root).
pub fn ptrace_attach_check(target: Pid) -> isize {
    let rq = RUN_QUEUE.lock();
    let c = match rq.find_pid(current_pid()) { Some(t) => t, None => return ESRCH };
    let t = match rq.find_pid(target) { Some(t) => t, None => return ESRCH };
    if t.tgid == c.tgid { return 0; }
    // CAP_SYS_PTRACE in the target's user namespace (host root included).
    if (c.euid == 0 && c.userns.id() == 0)
        || crate::caps::capable_snap(c.userns.id(), c.euid, c.caps[0], t.userns.id(), CAP_SYS_PTRACE) { return 0; }
    let (u, g) = (c.uid, c.gid);
    let ids_ok = t.uid == u && t.euid == u && t.suid == u
              && t.gid == g && t.egid == g && t.sgid == g;
    let dumpable = rq.find_pid(t.tgid).unwrap_or(t).dumpable;
    if ids_ok && dumpable { 0 } else { EPERM }
}

/// True when the calling thread is privileged (euid 0) — the gate for the
/// Leandros-private debug/bring-up syscalls (SYS_SPAWN, SYS_DBG_SERIAL_WRITE).
pub fn current_is_root() -> bool {
    RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.euid == 0 && t.userns.id() == 0).unwrap_or(false)
}

/// Linux `ptrace_may_access(target, PTRACE_MODE_READ_FSCREDS)`, the gate on opening another
/// task's /proc/<pid>/ns/*: own thread group; CAP_SYS_PTRACE in the target's user namespace;
/// else the caller's fsuid/fsgid must equal the target's real, effective and saved ids and the
/// target must be dumpable. 0 = allowed, -EACCES denied, -ESRCH target gone.
pub fn ptrace_read_check(caller_pid: Pid, target: Pid) -> isize {
    let rq = RUN_QUEUE.lock();
    let c = match rq.find_pid(caller_pid) { Some(t) => t, None => return ESRCH };
    let t = match rq.find_pid(target) { Some(t) => t, None => return ESRCH };
    if t.tgid == c.tgid { return 0; }
    if (c.euid == 0 && c.userns.id() == 0)
        || crate::caps::capable_snap(c.userns.id(), c.euid, c.caps[0], t.userns.id(), CAP_SYS_PTRACE) { return 0; }
    let (u, g) = (c.fsuid, c.fsgid);
    let ids_ok = t.uid == u && t.euid == u && t.suid == u && t.gid == g && t.egid == g && t.sgid == g;
    let dumpable = rq.find_pid(t.tgid).unwrap_or(t).dumpable;
        // A process that dropped privileges (root -> nobody, as passt/pasta do before
    // spawning its namespace child) is not dumpable. A same-uid caller may still
    // open the ns links of a process that lives in a non-initial user namespace:
    // the fd only names a namespace, and rootless pasta/podman need it (lane pasta).
    let in_userns = t.userns.id() != 0;
    if ids_ok && (dumpable || in_userns) { 0 } else { EACCES }
}
