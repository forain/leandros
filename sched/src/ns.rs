//! Namespace table (nsbox design A, lane L1 "nscore").
//!
//! Namespaces are small numeric ids: 0 is the initial namespace of its kind,
//! is never reference-counted and costs nothing on fork. Only user
//! namespaces exist so far; pid/net/ipc lanes extend this table.
//!
//! Every `Task` holds one `NsRef` per non-zero id: `Clone` takes a reference,
//! `Drop` puts it (so fork failure paths and task reap cannot leak). The
//! table is all atomics: it has no lock, so it can be used under RUN_QUEUE.
//! A user namespace holds a reference on its parent.

use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU8, Ordering::*};

pub type NsId = u16;
pub const NS_MAX: usize = 256;
/// Linux `/proc/sys/user/max_user_namespaces` nesting limit.
pub const MAX_USERNS_LEVEL: u8 = 32;
/// Clone/unshare flags the namespace layer accepts so far.
pub const SUPPORTED: usize = 0x1000_0000; // CLONE_NEWUSER

const ZERO32: AtomicU32 = AtomicU32::new(0);
const ZERO16: AtomicU16 = AtomicU16::new(0);
const ZERO8: AtomicU8 = AtomicU8::new(0);
static REFS: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
static PARENT: [AtomicU16; NS_MAX] = [ZERO16; NS_MAX];
static LEVEL: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];
/// Host euid / egid of the creator (the namespace owner).
static OWNER_EUID: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
static OWNER_EGID: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];

/// One counted reference on a namespace id (0 = initial, uncounted).
pub struct NsRef(NsId);

impl NsRef {
    pub const INIT: NsRef = NsRef(0);
    pub fn id(&self) -> NsId { self.0 }
    /// Take a reference on `id` (the caller must already hold one, or id 0).
    pub fn get(id: NsId) -> NsRef {
        if id != 0 { REFS[id as usize].fetch_add(1, AcqRel); }
        NsRef(id)
    }
}

impl Clone for NsRef {
    fn clone(&self) -> NsRef { NsRef::get(self.0) }
}

impl Drop for NsRef {
    fn drop(&mut self) { put(self.0); }
}

fn put(mut id: NsId) {
    while id != 0 {
        if REFS[id as usize].fetch_sub(1, AcqRel) != 1 { return; }
        // Last reference: the slot is free again; release the parent.
        id = PARENT[id as usize].load(Acquire);
    }
}

/// Create a user namespace owned by (`euid`, `egid`) as a child of `parent`.
/// The returned reference is the caller's; clone it for each holder.
/// Errors are negative errnos: EUSERS (87) too deep, ENOSPC (28) table full.
pub fn new_userns(parent: NsId, euid: u32, egid: u32) -> Result<NsRef, isize> {
    let level = if parent == 0 { 0 } else { LEVEL[parent as usize].load(Acquire) };
    if level >= MAX_USERNS_LEVEL { return Err(-87); }
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            let p = NsRef::get(parent); // the child's reference on its parent
            PARENT[id].store(p.0, Release);
            core::mem::forget(p);
            LEVEL[id].store(level + 1, Release);
            OWNER_EUID[id].store(euid, Release);
            OWNER_EGID[id].store(egid, Release);
            return Ok(NsRef(id as NsId));
        }
    }
    Err(-28)
}

/// Nesting depth of a user namespace (0 for the initial one).
pub fn level(id: NsId) -> u8 { if id == 0 { 0 } else { LEVEL[id as usize].load(Acquire) } }
/// Parent user namespace.
pub fn parent(id: NsId) -> NsId { if id == 0 { 0 } else { PARENT[id as usize].load(Acquire) } }
/// Host (euid, egid) of the namespace's creator.
pub fn owner(id: NsId) -> (u32, u32) {
    if id == 0 { (0, 0) } else { (OWNER_EUID[id as usize].load(Acquire), OWNER_EGID[id as usize].load(Acquire)) }
}
/// Live non-initial namespaces (debug/leak checks).
pub fn live_count() -> usize { (1..NS_MAX).filter(|&i| REFS[i].load(Relaxed) != 0).count() }

/// User namespace of the calling thread.
pub fn current_userns() -> NsId { userns_of(crate::current_pid()) }
/// User namespace of `pid` (0 when it does not exist).
pub fn userns_of(pid: u32) -> NsId {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.userns.id()).unwrap_or(0)
}

/// unshare(CLONE_NEWUSER) for the calling thread: it moves into a fresh user
/// namespace and gets a full capability set there. Single-threaded callers
/// only (the caller checks). 0 or a negative errno.
pub fn unshare_user() -> isize {
    let pid = crate::current_pid();
    let (parent, euid, egid) = {
        let rq = crate::RUN_QUEUE.lock();
        match rq.find_pid(pid) { Some(t) => (t.userns.id(), t.euid, t.egid), None => return -3 }
    };
    let new = match new_userns(parent, euid, egid) { Ok(n) => n, Err(e) => return e };
    // Swap under RUN_QUEUE; the old reference is put after the unlock.
    let old = {
        let mut rq = crate::RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) {
            Some(t) => {
                t.caps = crate::caps::userns_root_caps(t.caps[3]);
                core::mem::replace(&mut t.userns, new)
            }
            None => return -3,
        }
    };
    drop(old);
    0
}

// ── pid namespaces (lane nspid; append-only) ────────────────────────────────
pub const MAX_PIDNS_LEVEL: u8 = 32;
/// Kernel tgid of the namespace's init; 0 = none yet.
static PID_INIT: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
/// bit0 PIDNS_DEAD: the init exited, so fork into the namespace fails ENOMEM.
static PID_FLAGS: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];

/// New pid namespace, a child of pid namespace `parent`. ENOSPC when too deep
/// or the table is full (as Linux).
pub fn new_pidns(parent: NsId) -> Result<NsRef, isize> {
    let level = level(parent);
    if level >= MAX_PIDNS_LEVEL { return Err(-28); }
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            let p = NsRef::get(parent);
            PARENT[id].store(p.0, Release);
            core::mem::forget(p);
            LEVEL[id].store(level + 1, Release);
            PID_INIT[id].store(0, Release);
            PID_FLAGS[id].store(0, Release);
            crate::pidns::USED.store(true, Relaxed);
            return Ok(NsRef(id as NsId));
        }
    }
    Err(-28)
}
pub fn pid_init(id: NsId) -> u32 { if id == 0 { 0 } else { PID_INIT[id as usize].load(Acquire) } }
pub fn pid_dead(id: NsId) -> bool { id != 0 && PID_FLAGS[id as usize].load(Acquire) & 1 != 0 }
pub fn set_pid_dead(id: NsId) { if id != 0 { PID_FLAGS[id as usize].fetch_or(1, AcqRel); } }
pub fn claim_pid_init(id: NsId, kpid: u32) -> bool {
    id != 0 && PID_INIT[id as usize].compare_exchange(0, kpid, AcqRel, Relaxed).is_ok()
}
