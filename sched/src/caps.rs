//! Per-thread Linux capability sets: [effective, permitted, inheritable,
//! bounding], bits 0..40 (CAP_CHOWN..CAP_CHECKPOINT_RESTORE).
//!
//! A root task starts with everything but an empty inheritable set; uid
//! transitions and execve follow capabilities(7) without file capabilities
//! or ambient caps. Most privilege checks in the kernel still test euid 0;
//! `capable()` is the entry point for the ones converted to real caps.

use crate::task::Task;
use crate::{current_pid, RUN_QUEUE};

pub const ALL_CAPS: u64 = (1u64 << 41) - 1;
pub const ROOT_CAPS: [u64; 4] = [ALL_CAPS, ALL_CAPS, 0, ALL_CAPS];
pub const CAP_SETPCAP: u32 = 8;
pub const CAP_SYS_CHROOT: u32 = 18;
pub const CAP_LAST: u32 = 40;

/// Capabilities of the first task inside a fresh user namespace: everything
/// effective and permitted, nothing inheritable, the bounding set unchanged.
pub fn userns_root_caps(bnd: u64) -> [u64; 4] { [ALL_CAPS, ALL_CAPS, 0, bnd] }

/// Does the calling thread hold `cap` in its effective set, in ITS user
/// namespace? In the initial namespace that is host power; in a child one it
/// is power over that namespace only (see `host_capable`).
pub fn ns_capable(cap: u32) -> bool {
    cap <= CAP_LAST
        && RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.caps[0] >> cap & 1 != 0).unwrap_or(false)
}

/// Like `ns_capable` but only true in the initial user namespace: for
/// operations that act on the host (never convert an euid==0 site to a
/// namespace-scoped check without a reason).
pub fn host_capable(cap: u32) -> bool {
    cap <= CAP_LAST
        && RUN_QUEUE.lock().find_pid(current_pid())
            .map(|t| t.userns.id() == 0 && t.caps[0] >> cap & 1 != 0).unwrap_or(false)
}

/// Historic name of `ns_capable`.
pub fn capable(cap: u32) -> bool { ns_capable(cap) }

/// [eff, prm, inh, bnd] of `pid` (0 = caller); None when it does not exist.
pub fn caps_of(pid: u32) -> Option<[u64; 4]> {
    let pid = if pid == 0 { current_pid() } else { pid };
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.caps)
}

/// capset(2) rules. Returns 0 or -EPERM.
pub fn set_caps(eff: u64, prm: u64, inh: u64) -> isize {
    let (eff, prm, inh) = (eff & ALL_CAPS, prm & ALL_CAPS, inh & ALL_CAPS);
    let mut rq = RUN_QUEUE.lock();
    let t = match rq.find_pid_mut(current_pid()) { Some(t) => t, None => return -3 };
    let [_, op, oi, bnd] = t.caps;
    let setpcap = t.caps[0] >> CAP_SETPCAP & 1 != 0;
    if !setpcap && inh & !(oi | op) != 0 { return -1; }
    if inh & !(oi | bnd) != 0 { return -1; }
    if prm & !op != 0 { return -1; }
    if eff & !prm != 0 { return -1; }
    t.caps = [eff, prm, inh, bnd];
    0
}

/// prctl(PR_CAPBSET_DROP): needs CAP_SETPCAP.
pub fn bset_drop(cap: u32) -> isize {
    if cap > CAP_LAST { return -22; }
    let mut rq = RUN_QUEUE.lock();
    let t = match rq.find_pid_mut(current_pid()) { Some(t) => t, None => return -3 };
    if t.caps[0] >> CAP_SETPCAP & 1 == 0 { return -1; }
    t.caps[3] &= !(1u64 << cap);
    0
}

/// Linux `cap_emulate_setxuid`, given the (ruid, euid, suid) before the change.
pub fn on_uid_change(t: &mut Task, old: (u32, u32, u32)) {
    let had_root = old.0 == 0 || old.1 == 0 || old.2 == 0;
    if had_root && t.uid != 0 && t.euid != 0 && t.suid != 0 {
        t.caps[0] = 0;
        t.caps[1] = 0;
    }
    if old.1 == 0 && t.euid != 0 { t.caps[0] = 0; }
    if old.1 != 0 && t.euid == 0 { t.caps[0] = t.caps[1]; }
}

/// execve transition (no file caps): a task with uid or euid 0 gets
/// P' = I | bounding, E' = P'; anyone else ends with empty P and E.
pub fn on_exec(t: &mut Task) {
    let p = if t.uid == 0 || t.euid == 0 { t.caps[2] | t.caps[3] } else { 0 };
    t.caps[0] = p;
    t.caps[1] = p;
}
