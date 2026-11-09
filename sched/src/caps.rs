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
pub const ROOT_CAPS: [u64; 6] = [ALL_CAPS, ALL_CAPS, 0, ALL_CAPS, 0, 0];

/// Slots of `Task::caps` beyond [eff, prm, inh, bnd].
pub const AMB: usize = 4;
pub const SEC: usize = 5;

/// securebits (linux/securebits.h).
pub const SECURE_NOROOT: u64 = 1 << 0;
pub const SECURE_NO_SETUID_FIXUP: u64 = 1 << 2;
pub const SECURE_KEEP_CAPS: u64 = 1 << 4;
pub const SECURE_NO_CAP_AMBIENT_RAISE: u64 = 1 << 6;
/// Every lock bit (each is its flag's bit << 1).
pub const SECURE_ALL_LOCKS: u64 = 0xaa;
pub const SECURE_ALL_BITS: u64 = 0x55;
pub const CAP_SETPCAP: u32 = 8;
pub const CAP_SYS_CHROOT: u32 = 18;
pub const CAP_LAST: u32 = 40;

/// Capabilities of the first task inside a fresh user namespace: everything
/// effective and permitted, nothing inheritable, the bounding set unchanged.
pub fn userns_root_caps(bnd: u64) -> [u64; 6] { [ALL_CAPS, ALL_CAPS, 0, bnd, 0, 0] }

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
pub fn caps_of(pid: u32) -> Option<[u64; 6]> {
    let pid = if pid == 0 { current_pid() } else { pid };
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.caps)
}

/// capset(2) rules. Returns 0 or -EPERM.
pub fn set_caps(eff: u64, prm: u64, inh: u64) -> isize {
    let (eff, prm, inh) = (eff & ALL_CAPS, prm & ALL_CAPS, inh & ALL_CAPS);
    let mut rq = RUN_QUEUE.lock();
    let t = match rq.find_pid_mut(current_pid()) { Some(t) => t, None => return -3 };
    let [_, op, oi, bnd, _, _] = t.caps;
    let setpcap = t.caps[0] >> CAP_SETPCAP & 1 != 0;
    if !setpcap && inh & !(oi | op) != 0 { return -1; }
    if inh & !(oi | bnd) != 0 { return -1; }
    if prm & !op != 0 { return -1; }
    if eff & !prm != 0 { return -1; }
    // Linux cap_capset: the ambient set stays within permitted & inheritable.
    let amb = t.caps[AMB] & prm & inh;
    t.caps = [eff, prm, inh, bnd, amb, t.caps[SEC]];
    0
}

/// prctl(PR_CAP_AMBIENT, op, cap, 0, 0). Returns the answer or -errno.
pub fn ambient(op: usize, cap: usize, a4: usize, a5: usize) -> isize {
    const IS_SET: usize = 1; const RAISE: usize = 2; const LOWER: usize = 3; const CLEAR_ALL: usize = 4;
    if a4 != 0 || a5 != 0 { return -22; }
    let mut rq = RUN_QUEUE.lock();
    let t = match rq.find_pid_mut(current_pid()) { Some(t) => t, None => return -3 };
    match op {
        CLEAR_ALL => { if cap != 0 { return -22; } t.caps[AMB] = 0; 0 }
        IS_SET | RAISE | LOWER => {
            if cap > CAP_LAST as usize { return -22; }
            let bit = 1u64 << cap;
            match op {
                IS_SET => (t.caps[AMB] & bit != 0) as isize,
                LOWER => { t.caps[AMB] &= !bit; 0 }
                _ => {
                    if t.caps[1] & bit == 0 || t.caps[2] & bit == 0
                        || t.caps[SEC] & SECURE_NO_CAP_AMBIENT_RAISE != 0 { return -1; }
                    t.caps[AMB] |= bit;
                    0
                }
            }
        }
        _ => -22,
    }
}

/// prctl(PR_GET_SECUREBITS).
pub fn securebits() -> isize {
    RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.caps[SEC] as isize).unwrap_or(-3)
}

/// prctl(PR_SET_SECUREBITS): Linux cap_task_prctl rules (locks are one-way, a
/// locked flag cannot change, unknown bits and no CAP_SETPCAP are EPERM).
pub fn set_securebits(arg: u64) -> isize {
    let mut rq = RUN_QUEUE.lock();
    let t = match rq.find_pid_mut(current_pid()) { Some(t) => t, None => return -3 };
    let old = t.caps[SEC];
    if ((old & SECURE_ALL_LOCKS) >> 1) & (old ^ arg) != 0
        || old & SECURE_ALL_LOCKS & !arg != 0
        || arg & !(SECURE_ALL_LOCKS | SECURE_ALL_BITS) != 0
        || t.caps[0] >> CAP_SETPCAP & 1 == 0 { return -1; }
    t.caps[SEC] = arg;
    0
}

/// prctl(PR_SET_KEEPCAPS, v) / PR_GET_KEEPCAPS.
pub fn set_keepcaps(v: usize) -> isize {
    if v > 1 { return -22; }
    let mut rq = RUN_QUEUE.lock();
    let t = match rq.find_pid_mut(current_pid()) { Some(t) => t, None => return -3 };
    if t.caps[SEC] & (SECURE_KEEP_CAPS << 1) != 0 { return -1; }
    if v == 1 { t.caps[SEC] |= SECURE_KEEP_CAPS } else { t.caps[SEC] &= !SECURE_KEEP_CAPS }
    0
}
pub fn keepcaps() -> isize {
    RUN_QUEUE.lock().find_pid(current_pid()).map(|t| (t.caps[SEC] & SECURE_KEEP_CAPS != 0) as isize).unwrap_or(-3)
}

/// prctl(PR_CAPBSET_DROP): needs CAP_SETPCAP.
pub fn bset_drop(cap: u32) -> isize {
    let mut rq = RUN_QUEUE.lock();
    let t = match rq.find_pid_mut(current_pid()) { Some(t) => t, None => return -3 };
    if t.caps[0] >> CAP_SETPCAP & 1 == 0 { return -1; }
    if cap > CAP_LAST { return -22; }
    t.caps[3] &= !(1u64 << cap);
    0
}

/// Linux `cap_emulate_setxuid`, given the (ruid, euid, suid) before the change.
pub fn on_uid_change(t: &mut Task, old: (u32, u32, u32)) {
    if t.caps[SEC] & SECURE_NO_SETUID_FIXUP != 0 { return; }
    let had_root = old.0 == 0 || old.1 == 0 || old.2 == 0;
    if had_root && t.uid != 0 && t.euid != 0 && t.suid != 0 && t.caps[SEC] & SECURE_KEEP_CAPS == 0 {
        t.caps[0] = 0;
        t.caps[1] = 0;
        t.caps[AMB] = 0;
    }
    if old.1 == 0 && t.euid != 0 { t.caps[0] = 0; }
    if old.1 != 0 && t.euid == 0 { t.caps[0] = t.caps[1]; }
    t.caps[AMB] &= t.caps[1] & t.caps[2];
}

/// execve transition (no file caps), Linux cap_bprm_creds_from_file:
/// P' = (root ? I | bounding : 0) | A', E' = P' if euid 0 (and root rules on)
/// else A'. A set-id exec clears the ambient set first; KEEP_CAPS never
/// survives an exec.
pub fn on_exec(t: &mut Task, setid: bool) {
    let sb = t.caps[SEC];
    let root_rules = sb & SECURE_NOROOT == 0;
    let mut p = if root_rules && (t.uid == 0 || t.euid == 0) { t.caps[2] | t.caps[3] } else { 0 };
    if setid { t.caps[AMB] = 0; }
    // Linux cap_bprm_creds_from_file under no_new_privs: the new permitted set
    // may not exceed the old one (no privilege gain on exec).
    if t.no_new_privs { p &= t.caps[1]; }
    let amb = t.caps[AMB] & t.caps[1] & t.caps[2];
    t.caps[AMB] = amb;
    p |= amb;
    t.caps[0] = if root_rules && t.euid == 0 { p } else { amb };
    t.caps[1] = p;
    t.caps[SEC] &= !SECURE_KEEP_CAPS;
}
