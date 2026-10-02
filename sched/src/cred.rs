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
struct Caller { tgid: Pid, sid: Pid, uid: u32, euid: u32, gid: u32 }

fn caller(rq: &runqueue::RunQueue) -> Option<Caller> {
    rq.find_pid(current_pid()).map(|t| Caller {
        tgid: t.tgid, sid: t.sid,
        uid: t.uid, euid: t.euid, gid: t.gid,
    })
}

/// Linux `kill_ok_by_cred` plus the two exemptions of `check_kill_permission`:
/// the caller's own thread group, and SIGCONT within the caller's session.
fn may_signal(c: &Caller, t: &Task, sig: u32) -> bool {
    if t.tgid == c.tgid { return true; }
    if c.euid == 0 { return true; } // CAP_KILL
    if c.euid == t.suid || c.euid == t.uid || c.uid == t.suid || c.uid == t.uid { return true; }
    sig == SIGCONT && t.sid == c.sid
}

/// Permission and existence check for a process-directed signal (`kill(pid)`,
/// `rt_sigqueueinfo`). 0 = allowed, else -ESRCH / -EPERM. `sig == 0` (the
/// existence probe) is checked like any other signal, as on Linux.
pub fn kill_check(tgid: Pid, sig: u32) -> isize {
    let rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
    match rq.find_pid(tgid) {
        Some(t) if t.pid == t.tgid => if may_signal(&c, t, sig) { 0 } else { EPERM },
        _ => ESRCH,
    }
}

/// `kill(pid > 0, sig)`.
pub fn kill_process(tgid: Pid, sig: u32, info: task::SigInfo) -> isize {
    let r = kill_check(tgid, sig);
    if r != 0 || sig == 0 { return r; }
    deliver_signal_process(tgid, sig, info)
}

/// `tkill(tid)` / `tgkill(tgid, tid)` (`tgid == None` for tkill): the thread
/// must exist (and belong to `tgid`), then the usual kill permission against
/// its process. Signal 0 stops after the checks.
pub fn kill_thread(tgid: Option<Pid>, tid: Pid, sig: u32, info: task::SigInfo) -> isize {
    {
        let rq = RUN_QUEUE.lock();
        let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
        let t = match rq.find_pid(tid) { Some(t) => t, None => return ESRCH };
        if let Some(g) = tgid { if t.tgid != g { return ESRCH; } }
        let owner = rq.find_pid(t.tgid).unwrap_or(t);
        if !may_signal(&c, owner, sig) { return EPERM; }
    }
    if sig == 0 { return 0; }
    deliver_signal(tid, sig, info)
}

/// `kill(-pgid, sig)` / `killpg`: every process in the group the caller may
/// signal gets it. Linux `__kill_pgrp_info`: 0 if at least one delivery
/// succeeded, else the last error (EPERM); ESRCH for an empty group.
pub fn kill_pgrp_checked(pgid: Pid, sig: u32, info: task::SigInfo) -> isize {
    let mut targets = [0 as Pid; runqueue::MAX_TASKS];
    let mut n = 0;
    let mut found = false;
    {
        let rq = RUN_QUEUE.lock();
        let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get(i) {
                if t.pgid != pgid || t.pid != t.tgid { continue; }
                found = true;
                if may_signal(&c, t, sig) && n < targets.len() {
                    targets[n] = t.pid;
                    n += 1;
                }
            }
        }
    }
    if !found { return ESRCH; }
    if n == 0 { return EPERM; }
    if sig != 0 {
        for &pid in &targets[..n] {
            let _ = deliver_signal_process(pid, sig, info);
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
    let mut targets = [0 as Pid; runqueue::MAX_TASKS];
    let mut n = 0;
    let mut count = 0;
    {
        let rq = RUN_QUEUE.lock();
        let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get(i) {
                if t.pid != t.tgid || t.tgid == init || t.tgid == c.tgid { continue; }
                if t.address_space.is_none() || t.state == TaskState::Zombie { continue; }
                count += 1;
                if may_signal(&c, t, sig) && n < targets.len() {
                    targets[n] = t.pid;
                    n += 1;
                }
            }
        }
    }
    if count == 0 { return ESRCH; }
    if sig != 0 {
        for &pid in &targets[..n] {
            let _ = deliver_signal_process(pid, sig, info);
        }
    }
    0
}

/// getsid(pid): the session of `pid` (0 = the caller), ESRCH if none.
pub fn sid_of(pid: Pid) -> isize {
    let rq = RUN_QUEUE.lock();
    let pid = if pid == 0 { current_pid() } else { pid };
    match rq.find_pid(pid) { Some(t) => t.sid as isize, None => ESRCH }
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
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get(i) {
                if t.pgid == pgid && t.sid == c.sid { ok = true; break; }
            }
        }
        if !ok { return EPERM; }
    }
    for i in 0..runqueue::MAX_TASKS {
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
    let t = match rq.find_pid_mut(current_pid()) { Some(t) => t, None => return ESRCH };
    let privileged = t.euid == 0; // CAP_SETUID / CAP_SETGID
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
    if gid { t.gid = nr; t.egid = ne; t.sgid = ns; } else { t.uid = nr; t.euid = ne; t.suid = ns; }
    0
}

/// Linux `set_one_prio_perm`: root, or the caller's euid is the target's real
/// or effective uid.
fn same_owner(c: &Caller, t: &Task) -> bool {
    c.euid == 0 || c.euid == t.uid || c.euid == t.euid
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
    let can_nice = c.euid == 0 || (20 - nice as i64) as u64 <= rlim_nice;
    let min_vr = rq.min_vruntime();
    let mut err = ESRCH;
    for i in 0..runqueue::MAX_TASKS {
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
            t.place(min_vr);
        }
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
    if c.euid == 0 { return 0; }
    if !same_owner(&c, t) { return EPERM; }
    if rt && (prio as u64) > leader_rlimit(&rq, t.tgid, task::RLIMIT_RTPRIO) { return EPERM; }
    0
}

/// sched_setaffinity(2) admission: the target must exist and share our owner.
pub fn sched_affinity_check(pid: Pid) -> isize {
    let rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return ESRCH };
    let pid = if pid == 0 { current_pid() } else { pid };
    match rq.find_pid(pid) {
        Some(t) => if same_owner(&c, t) { 0 } else { EPERM },
        None => ESRCH,
    }
}

/// prlimit64(2) / getrlimit / setrlimit. `pid` 0 = the caller. Reading
/// another process needs the same real/effective/saved uid AND gid as ours
/// (Linux `check_prlimit_permission`), or root. A new limit with cur > max
/// is EINVAL; raising a hard limit is root-only (CAP_SYS_RESOURCE).
/// Returns the old limit (or the error) — the old value is valid on success.
pub fn prlimit(pid: Pid, res: usize, new: Option<[u64; 2]>) -> Result<[u64; 2], isize> {
    if res >= task::RLIM_NLIMITS { return Err(EINVAL); }
    if let Some([cur, max]) = new { if cur > max { return Err(EINVAL); } }
    let mut rq = RUN_QUEUE.lock();
    let c = match caller(&rq) { Some(c) => c, None => return Err(ESRCH) };
    let tgid = if pid == 0 { c.tgid } else {
        match rq.find_pid(pid) {
            Some(t) => {
                if t.tgid != c.tgid && c.euid != 0
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
        if nl[1] > old[1] && c.euid != 0 { return Err(EPERM); }
        l.rlimits[res] = nl;
    }
    Ok(old)
}

/// ([ruid, euid, suid], [rgid, egid, sgid]) of `pid`, for /proc/<pid>/status.
pub fn ids_of(pid: Pid) -> Option<([u32; 3], [u32; 3])> {
    RUN_QUEUE.lock().find_pid(pid)
        .map(|t| ([t.uid, t.euid, t.suid], [t.gid, t.egid, t.sgid]))
}
