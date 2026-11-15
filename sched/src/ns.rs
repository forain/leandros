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

use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering::*};

pub type NsId = u16;
/// Slots in the shared namespace table (ids are u16). Static, memory-bounded:
/// ~0.55 KiB per slot (~4.5 MiB of BSS in total). Every per-id array below is
/// indexed directly by id, so lookups stay O(1) and lock-free.
pub const NS_MAX: usize = 8192;
/// Linux `/proc/sys/user/max_user_namespaces` nesting limit.
pub const MAX_USERNS_LEVEL: u8 = 32;
/// Clone/unshare flags the namespace layer accepts so far.
pub const SUPPORTED: usize = 0x1000_0000 | 0x4000_0000 | 0x0800_0000 | 0x0400_0000; // NEWUSER | NEWNET | NEWIPC | NEWUTS

const ZERO32: AtomicU32 = AtomicU32::new(0);
const ZERO16: AtomicU16 = AtomicU16::new(0);
const ZERO8: AtomicU8 = AtomicU8::new(0);
static REFS: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
static PARENT: [AtomicU16; NS_MAX] = [ZERO16; NS_MAX];
/// Class of each allocated slot (see `slot_class`), set at allocation.
static SLOT_KIND: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];
static LEVEL: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];
/// Host euid / egid of the creator (the namespace owner).
static OWNER_EUID: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
static OWNER_EGID: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
/// Admin state of a network namespace's `lo` (recorded by RTM_NEWLINK; reset
/// when the id is allocated).
static LO_UP: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];
pub fn lo_admin_up(id: NsId) -> bool { id != 0 && LO_UP[id as usize].load(Acquire) != 0 }
pub fn set_lo_admin_up(id: NsId, up: bool) {
    if id == 0 { return; }
    LO_UP[id as usize].store(up as u8, Release);
    let h = LO_HOOK.load(Acquire);
    if h != 0 {
        // SAFETY: only `set_lo_hook` stores here, always a `fn(NsId, bool)`.
        let f: fn(NsId, bool) = unsafe { core::mem::transmute(h) };
        f(id, up);
    }
}
static LO_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// Register the callback that makes a namespace's loopback stack follow its admin state
/// (runs in the caller's process context, after the state is stored).
pub fn set_lo_hook(f: fn(NsId, bool)) { LO_HOOK.store(f as usize, Release); }

/// uid/gid maps stored COMPOSED: up to `MAP_EXT` extents per namespace, each
/// `[upper_first, lower_kuid_first, count]` where the lower id is already a
/// kernel id (translated through the parent map at write time), so a
/// translation is one lookup at any nesting depth. While `idmap_real()` is
/// false the write path only accepts identity extents.
pub const MAP_EXT: usize = 5;
static MAP_DATA: [[AtomicU32; NS_MAX * MAP_EXT * 3]; 2] =
    [[ZERO32; NS_MAX * MAP_EXT * 3], [ZERO32; NS_MAX * MAP_EXT * 3]];
static MAP_N: [[AtomicU8; NS_MAX]; 2] = [[ZERO8; NS_MAX], [ZERO8; NS_MAX]];
static SETGROUPS_DENY: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];

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
        if IS_MNT[id as usize].load(Acquire) != 0 {
            // Mount namespace: the last reference moves the slot straight from
            // 1 to DYING with one CAS, so the slot is never visible as free
            // (0) between the count reaching zero and the DYING mark. Freeing
            // the tree can drop superblock refs (port calls) and put may run
            // under RUN_QUEUE: `reap` frees it later from a lock-free context.
            let r = &REFS[id as usize];
            loop {
                let c = r.load(Acquire);
                if c == 1 {
                    if r.compare_exchange(1, DYING, AcqRel, Relaxed).is_ok() {
                        ZOMBIES[id as usize / 64].fetch_or(1u64 << (id as usize % 64), AcqRel);
                        MNT_LIVE.fetch_sub(1, AcqRel);
                        return;
                    }
                } else if r.compare_exchange(c, c - 1, AcqRel, Relaxed).is_ok() {
                    return;
                }
            }
        }
        // Last reference: 1 -> DYING in one CAS so the slot cannot be
        // reallocated (or held by name) while its fields are torn down; it
        // becomes free (0) only after PARENT/owner state is cleared.
        {
            let r = &REFS[id as usize];
            loop {
                let c = r.load(Acquire);
                if c == 1 {
                    if r.compare_exchange(1, DYING, AcqRel, Relaxed).is_ok() { break; }
                } else if r.compare_exchange(c, c - 1, AcqRel, Relaxed).is_ok() {
                    return;
                }
            }
        }
        // A cgroup namespace unpins its root cgroup (its owner userns, held as
        // PARENT, is released by the loop below).
        if CGNS_FLAG[id as usize].swap(0, AcqRel) != 0 {
            crate::cgroup::ns_unpin(CGNS_ROOT[id as usize].load(Acquire));
        }
        // Last reference: the slot is free again; release the parent and any
        // owning user namespace (setns lane).
        let ou = OWNER_UNS[id as usize].swap(0, AcqRel);
        if ou != 0 { put(ou); }
        let h = GONE_HOOK.load(Acquire);
        if h != 0 {
            // SAFETY: only `set_ns_gone_hook` stores here, always a `fn(NsId)`.
            let f: fn(NsId) = unsafe { core::mem::transmute(h) };
            f(id);
        }
        let next = PARENT[id as usize].swap(0, AcqRel);
        OWNER_EUID[id as usize].store(0, Release);
        OWNER_EGID[id as usize].store(0, Release);
        LEVEL[id as usize].store(0, Release);
        uncharge(SLOT_KIND[id as usize].load(Acquire));
        REFS[id as usize].store(0, Release);
        id = next;
    }
}

/// Per-kind namespace budgets (Linux `/proc/sys/user/max_<kind>_namespaces`).
/// Indexed by slot class: 1 user, 2 mnt, 3 net, 4 pid, 5 time (unsupported,
/// reserved), 6 cgroup, 7 ipc, 8 uts. The sum of the limits may never exceed
/// `NS_MAX - 1` (the shared table), so every kind keeps its own reservation and
/// one kind can never starve another; creation past a kind's limit is ENOSPC.
pub const LIMIT_N: usize = 9;
pub const LIMIT_NAMES: [&[u8]; LIMIT_N] =
    [b"", b"max_user_namespaces", b"max_mnt_namespaces", b"max_net_namespaces", b"max_pid_namespaces",
     b"max_time_namespaces", b"max_cgroup_namespaces", b"max_ipc_namespaces", b"max_uts_namespaces"];
const LIMIT_DEFAULT: [u32; LIMIT_N] = [0, 2048, 1024, 2048, 512, 8, 512, 1024, 1000];
static LIMIT: [AtomicU32; LIMIT_N] = {
    let mut a = [const { AtomicU32::new(0) }; LIMIT_N];
    let mut i = 0;
    while i < LIMIT_N { a[i] = AtomicU32::new(LIMIT_DEFAULT[i]); i += 1; }
    a
};
static USED: [AtomicU32; LIMIT_N] = [ZERO32; LIMIT_N];

pub fn limit_get(class: usize) -> u32 { if class < LIMIT_N { LIMIT[class].load(Acquire) } else { 0 } }
/// Live namespaces of `class` (including mount namespaces awaiting reap).
pub fn limit_used(class: usize) -> u32 { if class < LIMIT_N { USED[class].load(Acquire) } else { 0 } }
/// Set a kind's limit. EINVAL (-22) for a bad class or when the sum of all
/// limits would exceed the shared table (`NS_MAX - 1`).
pub fn limit_set(class: usize, v: u32) -> isize {
    if class == 0 || class >= LIMIT_N { return -22; }
    loop {
        let cur = LIMIT[class].load(Acquire);
        let others: u64 = (1..LIMIT_N).filter(|&c| c != class).map(|c| LIMIT[c].load(Acquire) as u64).sum();
        if others + v as u64 > (NS_MAX - 1) as u64 { return -22; }
        if LIMIT[class].compare_exchange(cur, v, AcqRel, Relaxed).is_ok() { return 0; }
    }
}
/// Reserve one namespace of `class`: false (ENOSPC) at the kind's limit.
fn charge(class: u8) -> bool {
    let c = class as usize;
    if USED[c].fetch_add(1, AcqRel) >= LIMIT[c].load(Acquire) { USED[c].fetch_sub(1, AcqRel); return false; }
    true
}
fn uncharge(class: u8) { USED[class as usize].fetch_sub(1, AcqRel); }

/// Per-kuid namespace quota (Linux ucounts; host root exempt): a creator may
/// own at most `MAX_USERNS_PER_UID` user namespaces and `MAX_NS_PER_UID`
/// namespaces of all kinds, so one uid cannot exhaust the shared table.
pub const MAX_USERNS_PER_UID: usize = 32;
pub const MAX_NS_PER_UID: usize = 64;
static SLOT_USER: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];

/// Userns whose owner chain a live slot is charged to (0 = host-created, uncharged).
fn slot_charge_start(id: usize) -> NsId {
    if SLOT_USER[id].load(Acquire) != 0 { return id as NsId; }
    if IS_MNT[id].load(Acquire) != 0 { OWNER_USER[id].load(Acquire) }
    else if CGNS_FLAG[id].load(Acquire) != 0 { PARENT[id].load(Acquire) as NsId }
    else { OWNER_UNS[id].load(Acquire) }
}

/// Linux `inc_ucount` walk: a namespace is charged to its creator kuid and to
/// the owner of every ancestor user namespace up to the initial one. True when
/// the chain starting at userns `o` (plus optional leading kuid `first`) holds `kuid`.
fn chain_has(first: Option<u32>, mut o: NsId, kuid: u32) -> bool {
    if first == Some(kuid) { return true; }
    while o != 0 {
        if OWNER_EUID[o as usize].load(Acquire) == kuid { return true; }
        o = PARENT[o as usize].load(Acquire);
    }
    false
}

/// ENOSPC when any kuid in the charge chain already owns its share.
fn quota_chain_ok(first: Option<u32>, start: NsId, want_user: bool) -> bool {
    if let Some(k) = first { if k != 0 && !quota_one(k, want_user) { return false; } }
    let mut o = start;
    while o != 0 {
        let k = OWNER_EUID[o as usize].load(Acquire);
        if k != 0 && !quota_one(k, want_user) { return false; }
        o = PARENT[o as usize].load(Acquire);
    }
    true
}

fn quota_one(kuid: u32, want_user: bool) -> bool {
    let (mut all, mut users) = (0usize, 0usize);
    for id in 1..NS_MAX {
        let r = REFS[id].load(Relaxed);
        if r == 0 || r == DYING { continue; }
        if chain_has(None, slot_charge_start(id) as NsId, kuid) {
            all += 1;
            if SLOT_USER[id].load(Acquire) != 0 { users += 1; }
        }
    }
    all < MAX_NS_PER_UID && !(want_user && users >= MAX_USERNS_PER_UID)
}

/// Quota check for a non-user namespace created under owner user namespace `owner`.
fn quota_owner_ok(owner: NsId) -> bool {
    owner == 0 || quota_chain_ok(None, owner, false)
}

/// Create a user namespace owned by (`euid`, `egid`) as a child of `parent`.
/// The returned reference is the caller's; clone it for each holder.
/// Errors are negative errnos: EUSERS (87) too deep, ENOSPC (28) table full.
pub fn new_userns(parent: NsId, euid: u32, egid: u32) -> Result<NsRef, isize> {
    // Linux create_user_ns: the creator's euid and egid must be mapped in its
    // own (current) namespace, else EPERM (rule 8).
    if from_k(0, parent, euid).is_none() || from_k(1, parent, egid).is_none() { return Err(-1); }
    let level = if parent == 0 { 0 } else { LEVEL[parent as usize].load(Acquire) };
    if level >= MAX_USERNS_LEVEL { return Err(-87); }
    if !quota_chain_ok(Some(euid), parent, true) { return Err(-28); }
    if !charge(1) { return Err(-28); }
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            SLOT_KIND[id].store(1, Release);
            let p = NsRef::get(parent); // the child's reference on its parent
            PARENT[id].store(p.0, Release);
            core::mem::forget(p);
            LEVEL[id].store(level + 1, Release);
            MAP_N[0][id].store(0, Release);
            MAP_N[1][id].store(0, Release);
            // setgroups=deny is inherited from the parent (rule 7).
            SETGROUPS_DENY[id].store(if parent != 0 { SETGROUPS_DENY[parent as usize].load(Acquire) } else { 0 }, Release);
            OWNER_EUID[id].store(euid, Release);
            OWNER_EGID[id].store(egid, Release);
            SLOT_USER[id].store(1, Release);
            return Ok(NsRef(id as NsId));
        }
    }
    uncharge(1);
    Err(-28)
}

/// Slot marker: a mount namespace whose tree has not been freed yet.
const DYING: u32 = 0x8000_0000;
static IS_MNT: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];
/// Owner user namespace of a mount namespace (also for netns/cgroupns later).
static OWNER_USER: [AtomicU16; NS_MAX] = [ZERO16; NS_MAX];
const ZERO64: AtomicU64 = AtomicU64::new(0);
static ZOMBIES: [AtomicU64; NS_MAX / 64] = [ZERO64; NS_MAX / 64];
/// Number of live mount namespaces other than the initial one: lets every
/// path operation skip the per-task lookup while it is zero.
static MNT_LIVE: AtomicU32 = AtomicU32::new(0);

/// True while any non-initial mount namespace exists.
#[inline]
pub fn mnt_active() -> bool { MNT_LIVE.load(Relaxed) != 0 }

static GONE_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Register the callback run when the last reference to a (non-mount)
/// namespace id drops, for any kind: the id is free again. It can run in any
/// context (possibly under the run queue), so it must not block or take
/// scheduler locks: record the id and reap later.
pub fn set_ns_gone_hook(f: fn(NsId)) { GONE_HOOK.store(f as usize, Release); }

/// Owner user namespace recorded for namespace `id`.
pub fn owner_user(id: NsId) -> NsId { if id == 0 { 0 } else { OWNER_USER[id as usize].load(Acquire) } }

/// Create a mount namespace id owned by user namespace `owner_user`.
pub fn new_mntns(owner_user: NsId) -> Result<NsRef, isize> {
    if !quota_owner_ok(owner_user) || !charge(2) { return Err(-28); }
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            SLOT_KIND[id].store(2, Release);
            PARENT[id].store(0, Release);
            LEVEL[id].store(0, Release);
            // The namespace pins its owner user namespace (dropped by reap), so
            // the owner id cannot be reused while the mount tree lives.
            core::mem::forget(NsRef::get(owner_user));
            OWNER_USER[id].store(owner_user, Release);
            SLOT_USER[id].store(0, Release);
            IS_MNT[id].store(1, Release);
            MNT_LIVE.fetch_add(1, AcqRel);
            return Ok(NsRef(id as NsId));
        }
    }
    uncharge(2);
    Err(-28)
}

/// Move the calling thread into mount namespace `n`, returning the old
/// reference (dropped by the caller outside any lock).
pub fn swap_mntns(n: NsRef) -> Result<NsRef, isize> {
    let pid = crate::current_pid();
    let mut rq = crate::RUN_QUEUE.lock();
    match rq.find_pid_mut(pid) { Some(t) => Ok(core::mem::replace(&mut t.mntns, n)), None => Err(-3) }
}

/// Mount namespace of `pid` (0 = initial, also when `pid` is unknown).
pub fn mntns_of(pid: u32) -> NsId {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.mntns.id()).unwrap_or(0)
}

/// Mount namespace ids whose last reference is gone but whose slot is still
/// parked: call `free` for each (it frees the vfs tree; no scheduler locks
/// are held here), then the slot becomes reusable.
pub fn reap(mut free: impl FnMut(NsId)) {
    for w in 0..NS_MAX / 64 {
        let mut bits = ZOMBIES[w].swap(0, AcqRel);
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let id = w * 64 + b;
            free(id as NsId);
            IS_MNT[id].store(0, Release);
            let ou = OWNER_USER[id].swap(0, AcqRel);
            uncharge(2);
            REFS[id].store(0, Release);
            drop(NsRef(ou)); // the owner pin taken by new_mntns
        }
    }
}

/// True when a dead mount namespace awaits `reap`.
pub fn has_zombies() -> bool { ZOMBIES.iter().any(|z| z.load(Relaxed) != 0) }

/// Number of parked (DYING) mount namespaces.
pub fn dying_count() -> usize { (1..NS_MAX).filter(|&i| REFS[i].load(Relaxed) == DYING).count() }

/// Create a flat (parentless) namespace id of slot class `class` (3 net, 7 ipc,
/// 8 uts; see `link_class`). Errors: ENOSPC (28).
pub fn new_flat(owner: NsId, class: u8) -> Result<NsRef, isize> {
    if !quota_owner_ok(owner) || !charge(class) { return Err(-28); }
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            SLOT_KIND[id].store(class, Release);
            PARENT[id].store(0, Release);
            LEVEL[id].store(0, Release);
            set_owner_userns(id as NsId, owner);
            LO_UP[id].store(0, Release);
            netctl_reset(id);
            SLOT_USER[id].store(0, Release);
            return Ok(NsRef(id as NsId));
        }
    }
    uncharge(class);
    Err(-28)
}

/// Network namespace of `pid` (0 = initial, also when `pid` is unknown).
pub fn netns_of(pid: u32) -> NsId {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.netns.id()).unwrap_or(0)
}
/// IPC namespace of `pid`.
pub fn ipcns_of(pid: u32) -> NsId {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.ipcns.id()).unwrap_or(0)
}

/// unshare(CLONE_NEWNET / CLONE_NEWIPC) for the calling thread: it moves
/// into a fresh namespace of that kind. 0 or a negative errno.
pub fn unshare_flat(net: bool, ipc: bool) -> isize {
    let ou = current_userns();
    let nn = if net { match new_flat(ou, 3) { Ok(n) => Some(n), Err(e) => return e } } else { None };
    let ni = if ipc { match new_flat(ou, 7) { Ok(n) => Some(n), Err(e) => return e } } else { None };
    let pid = crate::current_pid();
    let (o1, o2) = {
        let mut rq = crate::RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) {
            Some(t) => (
                nn.map(|n| core::mem::replace(&mut t.netns, n)),
                ni.map(|n| core::mem::replace(&mut t.ipcns, n)),
            ),
            None => return -3,
        }
    };
    drop((o1, o2));
    0
}

// ── UTS namespaces ───────────────────────────────────────────────────────────

/// Host and NIS domain names of one UTS namespace (bytes, length) plus the
/// user namespace that owns it. Slot 0 is the initial namespace.
#[derive(Clone, Copy)]
struct UtsNames { host: [u8; 64], hl: usize, dom: [u8; 64], dl: usize, owner: NsId }
const UTS_INIT: UtsNames = {
    let mut host = [0u8; 64];
    let mut dom = [0u8; 64];
    let (h, d) = (b"leandros", b"(none)");
    let mut i = 0;
    while i < h.len() { host[i] = h[i]; i += 1; }
    i = 0;
    while i < d.len() { dom[i] = d[i]; i += 1; }
    UtsNames { host, hl: h.len(), dom, dl: d.len(), owner: 0 }
};
static UTS: [spin::Mutex<UtsNames>; NS_MAX] = [const { spin::Mutex::new(UTS_INIT) }; NS_MAX];

/// Create a UTS namespace owned by user namespace `owner`, names copied from `src`.
pub fn new_uts(src: NsId, owner: NsId) -> Result<NsRef, isize> {
    let names = *UTS[src as usize].lock();
    let r = new_flat(owner, 8)?;
    *UTS[r.0 as usize].lock() = UtsNames { owner, ..names };
    Ok(r)
}

/// (host, domain) names of UTS namespace `id`, with their lengths.
pub fn uts_names(id: NsId) -> (([u8; 64], usize), ([u8; 64], usize)) {
    let u = *UTS[id as usize].lock();
    ((u.host, u.hl), (u.dom, u.dl))
}

/// UTS namespace of `pid` (0 = initial).
pub fn utsns_of(pid: u32) -> NsId {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.utsns.id()).unwrap_or(0)
}

/// Does the caller hold CAP_SYS_ADMIN over its own UTS namespace, i.e. in the
/// owning user namespace (its own, or an ancestor of the owner)?
pub fn uts_admin() -> bool {
    let (caps, mut u, uts) = match crate::RUN_QUEUE.lock().find_pid(crate::current_pid()) {
        Some(t) => (t.caps[0], t.userns.id(), t.utsns.id()),
        None => return false,
    };
    if caps >> 21 & 1 == 0 { return false; }
    let mut o = UTS[uts as usize].lock().owner;
    loop {
        if o == u { return true; }
        if o == 0 { return false; }
        o = parent(o);
    }
}

/// sethostname / setdomainname for the caller's UTS namespace.
pub fn uts_set(domain: bool, name: &[u8]) {
    let uts = utsns_of(crate::current_pid());
    let mut u = UTS[uts as usize].lock();
    let mut b = [0u8; 64];
    b[..name.len()].copy_from_slice(name);
    if domain { u.dom = b; u.dl = name.len(); } else { u.host = b; u.hl = name.len(); }
}

/// unshare(CLONE_NEWUTS): the caller moves into a copy of its UTS namespace
/// owned by its (possibly just created) user namespace. 0 or a negative errno.
pub fn unshare_uts() -> isize {
    let pid = crate::current_pid();
    let owner = userns_of(pid);
    let n = match new_uts(utsns_of(pid), owner) { Ok(n) => n, Err(e) => return e };
    let old = {
        let mut rq = crate::RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) { Some(t) => core::mem::replace(&mut t.utsns, n), None => return -3 }
    };
    drop(old);
    0
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
pub fn live_count() -> usize { (1..NS_MAX).filter(|&i| { let r = REFS[i].load(Relaxed); r != 0 && r != DYING }).count() }

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
pub fn new_pidns(parent: NsId, owner: NsId) -> Result<NsRef, isize> {
    let level = level(parent);
    if level >= MAX_PIDNS_LEVEL { return Err(-28); }
    if !quota_owner_ok(owner) || !charge(4) { return Err(-28); }
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            SLOT_KIND[id].store(4, Release);
            let p = NsRef::get(parent);
            PARENT[id].store(p.0, Release);
            core::mem::forget(p);
            LEVEL[id].store(level + 1, Release);
            PID_INIT[id].store(0, Release);
            PID_FLAGS[id].store(0, Release);
            SLOT_USER[id].store(0, Release);
            set_owner_userns(id as NsId, owner);
            crate::pidns::USED.store(true, Relaxed);
            return Ok(NsRef(id as NsId));
        }
    }
    uncharge(4);
    Err(-28)
}
pub fn pid_init(id: NsId) -> u32 { if id == 0 { 0 } else { PID_INIT[id as usize].load(Acquire) } }
pub fn pid_dead(id: NsId) -> bool { id != 0 && PID_FLAGS[id as usize].load(Acquire) & 1 != 0 }
/// `id` or any ancestor pid namespace is dying.
pub fn pid_dead_in_chain(id: NsId) -> bool {
    let mut n = id; let mut steps = 0;
    while n != 0 && steps <= MAX_PIDNS_LEVEL as usize + 1 {
        if pid_dead(n) { return true; }
        n = parent(n); steps += 1;
    }
    false
}
pub fn set_pid_dead(id: NsId) { if id != 0 { PID_FLAGS[id as usize].fetch_or(1, AcqRel); } }
pub fn claim_pid_init(id: NsId, kpid: u32) -> bool {
    id != 0 && PID_INIT[id as usize].compare_exchange(0, kpid, AcqRel, Relaxed).is_ok()
}

// ---- /proc/<pid>/{uid_map,gid_map,setgroups} and uid/gid views ----------

fn ext(kind: usize, id: usize, i: usize) -> (u32, u32, u32) {
    let b = (id * MAP_EXT + i) * 3;
    (MAP_DATA[kind][b].load(Acquire), MAP_DATA[kind][b + 1].load(Acquire), MAP_DATA[kind][b + 2].load(Acquire))
}

/// Non-identity maps are on by default (flip 2026-11-10). `write_map` rule 9
/// refuses `upper != lower` only when the kernel cmdline carries `idmap=off`
/// (one-release opt-out). `idmap=test` is accepted and ignored. Neither lifts
/// the kuid-0 refusal.
pub const IDMAP_REAL: bool = true;
static IDMAP_OFF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub fn set_idmap_off() { IDMAP_OFF.store(true, Release); }
pub fn idmap_real() -> bool { IDMAP_REAL && !IDMAP_OFF.load(Acquire) }

/// Namespace id `id` -> kernel id through namespace `ns` (None if unmapped or
/// `id` is u32::MAX). Namespace 0 is the identity.
pub fn to_k(kind: usize, ns: NsId, id: u32) -> Option<u32> {
    if id == u32::MAX { return None; }
    if ns == 0 { return Some(id); }
    let n = MAP_N[kind][ns as usize].load(Acquire) as usize;
    for i in 0..n {
        let (u, l, c) = ext(kind, ns as usize, i);
        if id >= u && (id - u) < c { return Some(l + (id - u)); }
    }
    None
}

/// Kernel id `kid` -> the id namespace `ns` calls it (None if unmapped).
pub fn from_k(kind: usize, ns: NsId, kid: u32) -> Option<u32> {
    if ns == 0 { return Some(kid); }
    let n = MAP_N[kind][ns as usize].load(Acquire) as usize;
    for i in 0..n {
        let (u, l, c) = ext(kind, ns as usize, i);
        if kid >= l && (kid - l) < c { return Some(u + (kid - l)); }
    }
    None
}

/// Overflow id shown for ids with no mapping.
pub const OVERFLOW_ID: u32 = 65534;

pub fn from_k_munged(kind: usize, ns: NsId, kid: u32) -> u32 { from_k(kind, ns, kid).unwrap_or(OVERFLOW_ID) }

/// The kernel uid that is root (id 0) of `ns`, if mapped.
pub fn root_k(ns: NsId) -> Option<u32> { to_k(0, ns, 0) }

/// An id argument taken from user space, in the caller's user namespace, as a
/// kernel id. u32::MAX ("leave unchanged") passes through; an id with no
/// mapping is None (callers answer EINVAL). Never pass the raw argument on.
pub fn arg_id(kind: usize, id: u32) -> Option<u32> {
    if id == u32::MAX { return Some(u32::MAX); }
    to_k(kind, current_userns(), id)
}

/// The id `kid` as the calling task sees it (overflow id when unmapped).
pub fn view_id(kind: usize, kid: u32) -> u32 {
    from_k_munged(kind, current_userns(), kid)
}

/// Copyable snapshot of a user namespace's maps (for credentials that must
/// translate without touching the scheduler).
#[derive(Clone, Copy)]
pub struct IdView { pub ns: NsId, pub n: [u8; 2], pub ext: [[(u32, u32, u32); MAP_EXT]; 2] }
impl IdView {
    pub fn to_k(&self, kind: usize, id: u32) -> Option<u32> {
        if id == u32::MAX { return None; }
        if self.ns == 0 { return Some(id); }
        (0..self.n[kind] as usize).find_map(|i| { let (u, l, c) = self.ext[kind][i]; if id >= u && id - u < c { Some(l + (id - u)) } else { None } })
    }
    pub fn from_k(&self, kind: usize, kid: u32) -> Option<u32> {
        if self.ns == 0 { return Some(kid); }
        (0..self.n[kind] as usize).find_map(|i| { let (u, l, c) = self.ext[kind][i]; if kid >= l && kid - l < c { Some(u + (kid - l)) } else { None } })
    }
    pub fn from_k_munged(&self, kind: usize, kid: u32) -> u32 { self.from_k(kind, kid).unwrap_or(OVERFLOW_ID) }
}
pub fn view_of_ns(ns: NsId) -> IdView {
    let mut v = IdView { ns, n: [0; 2], ext: [[(0, 0, 0); MAP_EXT]; 2] };
    if ns != 0 {
        for k in 0..2 {
            let n = (MAP_N[k][ns as usize].load(Acquire) as usize).min(MAP_EXT);
            v.n[k] = n as u8;
            for i in 0..n { v.ext[k][i] = ext(k, ns as usize, i); }
        }
    }
    v
}
pub fn view_of(pid: u32) -> IdView { view_of_ns(userns_of(pid)) }

fn put_num(v: u32, out: &mut [u8], n: &mut usize) {
    let mut d = [b' '; 10];
    let mut x = v; let mut k = 10;
    loop { k -= 1; d[k] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
    if *n + 10 <= out.len() { out[*n..*n + 10].copy_from_slice(&d); *n += 10; }
}
fn put_line(a: u32, b: u32, c: u32, out: &mut [u8], n: &mut usize) {
    put_num(a, out, n); if *n < out.len() { out[*n] = b' '; *n += 1; }
    put_num(b, out, n); if *n < out.len() { out[*n] = b' '; *n += 1; }
    put_num(c, out, n); if *n < out.len() { out[*n] = b'\n'; *n += 1; }
}

/// Text of uid_map (kind 0) / gid_map (kind 1) of user namespace `map_ns` as
/// read from `reader_ns`: lower ids are shown in the reader's namespace (the
/// parent's, when the reader sits in `map_ns` itself); unmapped -> 4294967295.
pub fn map_text_for(kind: usize, map_ns: NsId, reader_ns: NsId, out: &mut [u8]) -> usize {
    let mut n = 0;
    if map_ns == 0 { put_line(0, 0, u32::MAX, out, &mut n); return n; }
    let l = if reader_ns == map_ns { parent(map_ns) } else { reader_ns };
    let c = MAP_N[kind][map_ns as usize].load(Acquire) as usize;
    for i in 0..c {
        let (u, lk, cnt) = ext(kind, map_ns as usize, i);
        put_line(u, from_k(kind, l, lk).unwrap_or(u32::MAX), cnt, out, &mut n);
    }
    n
}

/// Text of the map as seen from the parent namespace.
pub fn map_text(kind: usize, ns: NsId, out: &mut [u8]) -> usize { map_text_for(kind, ns, ns, out) }

/// Has `ns` written its gid_map (setgroups(2) needs it, rule 7)? Namespace 0: always.
pub fn gid_map_written(ns: NsId) -> bool { ns == 0 || MAP_N[1][ns as usize].load(Acquire) != 0 }

pub fn setgroups_denied(ns: NsId) -> bool { ns != 0 && SETGROUPS_DENY[ns as usize].load(Acquire) != 0 }

/// Credentials (userns, host euid/egid, effective caps) of a process.
#[derive(Clone, Copy)]
pub struct Cr(NsId, u32, u32, u64);

/// Identity of a process frozen at open() time, like Linux `file->f_cred` of a
/// map file, plus the target namespace captured at open (a counted reference,
/// so the slot cannot be reused under the fd) and the target thread group.
pub struct Opener { cr: Cr, _own: NsRef, tns: NsRef, tgid: u32 }

/// Snapshot `pid` (the opener) and the user namespace of `target` (a tgid).
pub fn snapshot(pid: u32, target: u32) -> Option<Opener> {
    let rq = crate::RUN_QUEUE.lock();
    let o = rq.find_pid(pid)?;
    let t = rq.find_pid(target)?;
    Some(Opener { cr: Cr(o.userns.id(), o.euid, o.egid, o.caps[0]), _own: o.userns.clone(), tns: t.userns.clone(), tgid: target })
}

fn writer_info(pid: u32) -> Option<Cr> {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| Cr(t.userns.id(), t.euid, t.egid, t.caps[0]))
}

/// May the opener write the captured target namespace's maps at all? (open-time EPERM)
pub fn open_allowed(op: &Opener, kind: usize) -> bool {
    let tns = op.tns.id();
    if tns == 0 { return false; }
    rights(op.cr, tns, kind.min(1)).0
}

static MAP_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
struct MapGuard;
impl MapGuard {
    fn lock() -> MapGuard {
        while MAP_LOCK.compare_exchange(false, true, Acquire, Relaxed).is_err() { core::hint::spin_loop(); }
        MapGuard
    }
}
impl Drop for MapGuard { fn drop(&mut self) { MAP_LOCK.store(false, Release); } }

/// May `w` (writer pid) set up namespace `tns`'s ids: it sits in `tns` or its
/// parent and holds `cap` or is the owner. Returns (ok, privileged-over-parent).
fn writer_rights(w: u32, tns: NsId, kind: usize) -> (bool, bool, u32) {
    let Some(op) = writer_info(w) else { return (false, false, 0) };
    rights(op, tns, kind)
}

fn rights(op: Cr, tns: NsId, kind: usize) -> (bool, bool, u32) {
    let Cr(wns, eu, eg, caps) = op;
    let cap: u32 = if kind == 0 { 7 } else { 6 };
    let has = caps >> cap & 1 != 0;
    let (oe, _) = owner(tns);
    let me = if kind == 0 { eu } else { eg };
    let ownerish = eu == oe;
    let in_ns = wns == tns;
    let in_parent = wns == parent(tns);
    let ok = (in_ns || in_parent) && (has || ownerish);
    // CAP_SETUID / CAP_SETGID over the parent namespace (rule 5).
    let privileged = crate::caps::capable_snap(wns, eu, caps, parent(tns), cap);
    (ok, privileged, me)
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 10 { return None; }
    let mut v: u64 = 0;
    for &c in s { if !c.is_ascii_digit() { return None; } v = v * 10 + (c - b'0') as u64; }
    if v > u32::MAX as u64 { None } else { Some(v as u32) }
}

/// Translate the lower range `[lower, lower+count)` (given in the parent's
/// view) to kernel ids; the whole range must lie in ONE parent extent.
fn lower_to_k(par: NsId, lower: u32, count: u32, kind: usize) -> Option<u32> {
    if par == 0 { return Some(lower); }
    let n = MAP_N[kind][par as usize].load(Acquire) as usize;
    for i in 0..n {
        let (u, l, c) = ext(kind, par as usize, i);
        if lower >= u && (lower as u64 + count as u64) <= u as u64 + c as u64 { return Some(l + (lower - u)); }
    }
    None
}

/// Write uid_map (kind 0) / gid_map (kind 1) of `target`'s user namespace on
/// behalf of `writer` (Linux map_write / new_idmap_permitted). Returns bytes
/// consumed or -errno.
pub fn write_map(opener: &Opener, writer: u32, kind: usize, data: &[u8]) -> isize {
    let _g = MapGuard::lock();
    // The namespace captured at open; if the target no longer lives in it
    // (exited, pid reused) the write fails instead of landing elsewhere.
    let tns = opener.tns.id();
    if tns == 0 { return -1; }
    if userns_of(opener.tgid) != tns { return -3; }
    let t = tns as usize;
    if MAP_N[kind][t].load(Acquire) != 0 { return -1; } // written once
    // Rule 1: both the opener (f_cred) and the writer sit in the target
    // namespace or its parent; the privileged branch needs both privileged,
    // and both must be the same id.
    let (ok_o, priv_o, me_o) = rights(opener.cr, tns, kind);
    let (ok_w, priv_w, me_w) = writer_rights(writer, tns, kind);
    if !ok_o || !ok_w { return -1; }
    let privileged = priv_o && priv_w;
    let me = me_o;
    if !privileged && me_w != me_o { return -1; }
    // Rule 2: syntax, at most MAP_EXT lines, no u32::MAX, no wrap, no overlap.
    let mut exts = [(0u32, 0u32, 0u32); MAP_EXT];
    let mut n = 0usize;
    for line in data.split(|&c| c == b'\n') {
        if line.iter().all(|&c| c == b' ' || c == b'\t') { continue; }
        if n == MAP_EXT { return -22; }
        let mut f = [0u32; 3]; let mut k = 0;
        for w in line.split(|&c| c == b' ' || c == b'\t').filter(|w| !w.is_empty()) {
            if k == 3 { return -22; }
            match parse_u32(w) { Some(v) => f[k] = v, None => return -22 }
            k += 1;
        }
        if k != 3 || f[2] == 0 { return -22; }
        if f[0] == u32::MAX || f[1] == u32::MAX { return -22; }
        if (f[0] as u64 + f[2] as u64) > u32::MAX as u64 || (f[1] as u64 + f[2] as u64) > u32::MAX as u64 { return -22; }
        exts[n] = (f[0], f[1], f[2]); n += 1;
    }
    if n == 0 { return -22; }
    for i in 0..n {
        for j in 0..i {
            let (a, c) = (exts[i], exts[j]);
            if (a.0 as u64) < c.0 as u64 + c.2 as u64 && (c.0 as u64) < a.0 as u64 + a.2 as u64 { return -22; } // upper
            if (a.1 as u64) < c.1 as u64 + c.2 as u64 && (c.1 as u64) < a.1 as u64 + a.2 as u64 { return -22; } // lower
        }
    }
    // Rule 3: lower ranges translated through the parent map.
    let par = parent(tns);
    let mut comp = [(0u32, 0u32, 0u32); MAP_EXT];
    for i in 0..n {
        let Some(lk) = lower_to_k(par, exts[i].1, exts[i].2, kind) else { return -1 };
        comp[i] = (exts[i].0, lk, exts[i].2);
        // Rule 6: no extent covers kuid 0 until lane idroot0.
        if lk == 0 { return -1; }
        // Rule 9: identity maps only when the idmap=off opt-out is set.
        if !idmap_real() && exts[i].0 != lk { return -1; }
    }
    if !privileged {
        // Rule 4: one single-id extent, the translated lower id is the
        // opener's own euid (egid for gid_map); owner is the opener.
        if n != 1 || comp[0].2 != 1 || comp[0].1 != me { return -1; }
        if owner(tns).0 != opener.cr.1 { return -1; }
        if kind == 1 && SETGROUPS_DENY[t].load(Acquire) == 0 { return -1; }
    }
    for i in 0..n {
        let b = (t * MAP_EXT + i) * 3;
        MAP_DATA[kind][b].store(comp[i].0, Release);
        MAP_DATA[kind][b + 1].store(comp[i].1, Release);
        MAP_DATA[kind][b + 2].store(comp[i].2, Release);
    }
    MAP_N[kind][t].store(n as u8, Release);
    data.len() as isize
}

/// Write /proc/<target>/setgroups ("allow" / "deny"). Bytes consumed or -errno.
pub fn write_setgroups(opener: &Opener, writer: u32, data: &[u8]) -> isize {
    let _g = MapGuard::lock();
    let tns = opener.tns.id();
    if tns == 0 { return -1; }
    if userns_of(opener.tgid) != tns { return -3; }
    let (ok, _, _) = writer_rights(writer, tns, 1);
    let (ok_o, _, _) = rights(opener.cr, tns, 1);
    if !ok || !ok_o { return -1; }
    let t = tns as usize;
    let s = data.strip_suffix(b"\n").unwrap_or(data);
    if s == b"deny" {
        if MAP_N[1][t].load(Acquire) != 0 { return -1; }
        SETGROUPS_DENY[t].store(1, Release);
    } else if s == b"allow" {
        if MAP_N[1][t].load(Acquire) != 0 || SETGROUPS_DENY[t].load(Acquire) != 0 { return -1; }
    } else { return -22; }
    data.len() as isize
}

/// inode of namespace link `kind` (0 user,1 pid,2 net,3 ipc,4 mnt,5 uts,
/// 6 cgroup,7 pid_for_children). Only the user namespace is per-task so far.
pub fn ns_ino(kind: usize, id: NsId) -> u64 {
    let kind = if kind == 7 { 1 } else { kind };
    if id != 0 { return 0xF000_0000 + id as u64 * 4 + (kind as u64 & 3); }
    match kind { 0 => 4026531837, 1 | 7 => 4026531836, 2 => 4026531840, 3 => 4026531839,
                 4 => 4026531841, 5 => 4026531838, _ => 4026531835 }
}

pub const NS_NAMES: [&[u8]; 8] = [b"user", b"pid", b"net", b"ipc", b"mnt", b"uts", b"cgroup", b"pid_for_children"];

/// Namespace id of `pid` for link `kind` (non-user kinds are initial).
pub fn ns_id_of(pid: u32, kind: usize) -> NsId {
    match kind {
        6 => cgns_of(pid),
        0 => userns_of(pid), 2 => netns_of(pid), 3 => ipcns_of(pid),
        1 => crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.pidns.id()).unwrap_or(0),
        7 => crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.pidns_child.id()).unwrap_or(0),
        4 => mntns_of(pid),
        5 => utsns_of(pid),
        _ => 0,
    }
}

// ── setns(2) and nsfs-like fds (lane setns; append-only) ────────────────────

/// Owning user namespace of a net/ipc/pid namespace (a counted reference,
/// released when the namespace itself is freed).
static OWNER_UNS: [AtomicU16; NS_MAX] = [ZERO16; NS_MAX];

/// Record `owner` as the user namespace that owns the fresh namespace `id`.
pub fn set_owner_userns(id: NsId, owner: NsId) {
    if id == 0 { return; }
    core::mem::forget(NsRef::get(owner));
    OWNER_UNS[id as usize].store(owner, Release);
}

/// Take one raw reference on `id` (an nsfd's hold); pair with `put_raw`.
pub fn hold_raw(id: NsId) { core::mem::forget(NsRef::get(id)); }

/// Class of an nsfs link kind: 1 user, 2 mnt, 3 net, 4 pid, 6 cgroup, 7 ipc, 8 uts.
fn link_class(kind: u8) -> u8 {
    match kind { 0 => 1, 4 => 2, 2 => 3, 3 => 7, 5 => 8, 1 | 7 => 4, 6 => 6, _ => 0 }
}

/// Take a reference on a namespace named by an id that did not come from a
/// held reference (a synthetic nsfs source). Refuses ids past the table,
/// free or dying slots, and slots of another kind. `false` = nothing taken.
pub fn hold_checked(kind: u8, id: NsId) -> bool {
    let i = id as usize;
    if i >= NS_MAX { return false; }
    if id == 0 { return true; }
    let c = link_class(kind);
    if c == 0 || SLOT_KIND[i].load(Acquire) != c { return false; }
    let r = &REFS[i];
    loop {
        let v = r.load(Acquire);
        if v == 0 || v & DYING != 0 { return false; }
        if r.compare_exchange(v, v + 1, AcqRel, Relaxed).is_ok() { return true; }
    }
}
/// Drop a reference taken by `hold_raw`.
pub fn put_raw(id: NsId) { drop(NsRef(id)); }

/// Take a reference on the namespace of link `kind` of `pid`, atomically with
/// reading it. `None` when the task does not exist. Kinds without a per-task
/// table (mnt, uts, cgroup) yield the initial namespace 0.
pub fn grab(pid: u32, kind: usize) -> Option<NsId> {
    let rq = crate::RUN_QUEUE.lock();
    let t = rq.find_pid(pid)?;
    let id = match kind {
        0 => t.userns.id(), 1 => t.pidns.id(), 2 => t.netns.id(), 3 => t.ipcns.id(),
        7 => t.pidns_child.id(), 5 => t.utsns.id(), 4 => t.mntns.id(), 6 => t.cgns.id(), _ => 0,
    };
    core::mem::forget(NsRef::get(id));
    Some(id)
}

pub const CLONE_NEWNS: usize = 0x0002_0000;
pub const CLONE_NEWCGROUP: usize = 0x0200_0000;
pub const CLONE_NEWUTS: usize = 0x0400_0000;
pub const CLONE_NEWIPC: usize = 0x0800_0000;
pub const CLONE_NEWUSER: usize = 0x1000_0000;
pub const CLONE_NEWPID: usize = 0x2000_0000;
pub const CLONE_NEWNET: usize = 0x4000_0000;

/// CLONE_NEW* flag of link `kind` (NS_GET_NSTYPE).
pub fn clone_flag(kind: usize) -> usize {
    match kind {
        0 => CLONE_NEWUSER, 1 | 7 => CLONE_NEWPID, 2 => CLONE_NEWNET, 3 => CLONE_NEWIPC,
        4 => CLONE_NEWNS, 5 => CLONE_NEWUTS, _ => CLONE_NEWCGROUP,
    }
}

/// `anc` is `n` or one of its ancestors in the same (user or pid) tree.
pub fn covers(anc: NsId, mut n: NsId) -> bool {
    if anc == 0 { return true; }
    let mut steps = 0;
    while steps <= MAX_USERNS_LEVEL as usize + 2 {
        if n == anc { return true; }
        if n == 0 { return false; }
        n = PARENT[n as usize].load(Acquire);
        steps += 1;
    }
    false
}

/// Owning user namespace of namespace `id` of link `kind` (the parent for a
/// user namespace). 0 for the initial namespaces.
pub fn owner_userns(kind: usize, id: NsId) -> NsId {
    if id == 0 { return 0; }
    if kind == 0 { PARENT[id as usize].load(Acquire) } else { OWNER_UNS[id as usize].load(Acquire) }
}

/// The caller's state a join is checked against.
struct Ctx { userns: NsId, admin: bool, chroot: bool, pidns: NsId, euid: u32 }

fn cap_in(ctx: &Ctx, target_userns: NsId) -> bool { ctx.admin && covers(ctx.userns, target_userns) }

/// Linux's per-type install checks for joining namespace `id` of link `kind`.
/// A user-namespace join updates `ctx` so later entries are checked with the
/// caps it grants. 0 or a negative errno.
fn check_join(ctx: &mut Ctx, kind: usize, id: NsId, alone: bool, chrooted: bool) -> isize {
    match kind {
        0 => {
            if id == ctx.userns { return -22; }
            if !alone { return -22; }
            if chrooted { return -1; }
            let creator = id != 0 && PARENT[id as usize].load(Acquire) == ctx.userns
                && OWNER_EUID[id as usize].load(Acquire) == ctx.euid;
            if !cap_in(ctx, id) && !creator { return -1; }
            ctx.userns = id; ctx.admin = true; ctx.chroot = true;
            0
        }
        1 | 7 => {
            if !cap_in(ctx, owner_userns(1, id)) { return -1; }
            if !covers(ctx.pidns, id) { return -22; }
            0
        }
        2 | 3 => if cap_in(ctx, owner_userns(kind, id)) { 0 } else { -1 },
        5 => if cap_in(ctx, UTS[id as usize].lock().owner) { 0 } else { -1 },
        6 => if !is_cgns(id) { -22 } else if cap_in(ctx, cgns_owner(id)) { 0 } else { -1 },
        // Linux mntns_install: CAP_SYS_ADMIN over the namespace's owner, plus
        // CAP_SYS_CHROOT and CAP_SYS_ADMIN in the caller's own user namespace.
        // A slot that is not a live mount namespace fails closed.
        4 => {
            if id != 0 && (IS_MNT[id as usize].load(Acquire) == 0 || REFS[id as usize].load(Acquire) & DYING != 0) { return -22; }
            if !cap_in(ctx, owner_user(id)) || !ctx.chroot || !ctx.admin { -1 } else { 0 }
        }
        _ => -22,
    }
}

/// Join the namespaces in `items` (kind, held reference). All entries are
/// checked before any is applied. 0 or a negative errno.
pub fn join(items: alloc::vec::Vec<(usize, NsRef)>, alone: bool, chrooted: bool) -> isize {
    let pid = crate::current_pid();
    let mut ctx = {
        let rq = crate::RUN_QUEUE.lock();
        match rq.find_pid(pid) {
            Some(t) => Ctx { userns: t.userns.id(), admin: t.caps[0] >> 21 & 1 != 0, chroot: t.caps[0] >> 18 & 1 != 0, pidns: t.pidns.id(), euid: t.euid },
            None => return -3,
        }
    };
    // The user namespace first: later entries are judged with its caps.
    let mut order: alloc::vec::Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| if items[i].0 == 0 { 0 } else { 1 });
    for &i in &order {
        let r = check_join(&mut ctx, items[i].0, items[i].1.id(), alone, chrooted);
        if r < 0 { return r; }
    }
    let mut old: alloc::vec::Vec<NsRef> = alloc::vec::Vec::new();
    {
        let mut rq = crate::RUN_QUEUE.lock();
        let Some(t) = rq.find_pid(pid) else { return -3 };
        // A mount-namespace join replaces the task's fs struct view: refuse
        // while root/cwd are shared with other tasks (CLONE_FS), before any
        // entry is applied.
        if items.iter().any(|(k, _)| *k == 4) && t.fs_id != 0 {
            let fid = t.fs_id;
            if rq.live_slots().filter_map(|i| rq.get(i)).any(|o| o.pid != pid && o.fs_id == fid && o.state != crate::task::TaskState::Zombie) {
                return -22;
            }
        }
        let Some(t) = rq.find_pid_mut(pid) else { return -3 };
        for (kind, n) in items {
            match kind {
                4 => {
                    // Root and cwd become the root of the new namespace.
                    t.root = None;
                    t.cwd = None;
                    old.push(core::mem::replace(&mut t.mntns, n));
                }
                0 => {
                    t.caps = crate::caps::userns_root_caps(t.caps[3]);
                    old.push(core::mem::replace(&mut t.userns, n));
                }
                1 | 7 => old.push(core::mem::replace(&mut t.pidns_child, n)),
                2 => old.push(core::mem::replace(&mut t.netns, n)),
                5 => old.push(core::mem::replace(&mut t.utsns, n)),
                6 => old.push(core::mem::replace(&mut t.cgns, n)),
                _ => old.push(core::mem::replace(&mut t.ipcns, n)),
            }
        }
    }
    drop(old);
    0
}

// ── cgroup namespaces (design-cgroup lane CG0/CG1; append-only) ─────────────
//
// A cgroup namespace is a flat id that records its root cgroup and its owner
// user namespace (held as PARENT, so the owner id cannot be reused while the
// namespace lives). It pins the root cgroup against rmdir while alive.

static CGNS_ROOT: [AtomicU16; NS_MAX] = [ZERO16; NS_MAX];
static CGNS_FLAG: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];

/// New cgroup namespace rooted at cgroup `root`, owned by user namespace
/// `owner`. The returned reference is the caller's. ENOSPC (28) when full.
pub fn new_cgns(root: crate::cgroup::CgId, owner: NsId) -> Result<NsRef, isize> {
    if !quota_owner_ok(owner) || !charge(6) { return Err(-28); }
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            SLOT_KIND[id].store(6, Release);
            let p = NsRef::get(owner);
            PARENT[id].store(p.0, Release);
            core::mem::forget(p);
            LEVEL[id].store(0, Release);
            CGNS_ROOT[id].store(root, Release);
            crate::cgroup::ns_pin(root);
            SLOT_USER[id].store(0, Release);
            CGNS_FLAG[id].store(1, Release);
            return Ok(NsRef(id as NsId));
        }
    }
    uncharge(6);
    Err(-28)
}

/// Is `id` a live cgroup namespace? (Id 0, the initial one, is: root cgroup 0.)
pub fn is_cgns(id: NsId) -> bool { id == 0 || (id as usize) < NS_MAX && CGNS_FLAG[id as usize].load(Acquire) != 0 }
/// Root cgroup of cgroup namespace `id` (0 for the initial one).
pub fn cgns_root(id: NsId) -> crate::cgroup::CgId { if id == 0 { 0 } else { CGNS_ROOT[id as usize].load(Acquire) } }
/// Owner user namespace of cgroup namespace `id` (0 for the initial one).
pub fn cgns_owner(id: NsId) -> NsId { if id == 0 { 0 } else { PARENT[id as usize].load(Acquire) } }
/// Cgroup namespace of `pid` (0 = initial, also when `pid` is unknown).
pub fn cgns_of(pid: u32) -> NsId {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.cgns.id()).unwrap_or(0)
}
/// Cgroup namespace of the calling thread.
pub fn current_cgns() -> NsId { cgns_of(crate::current_pid()) }

/// unshare(CLONE_NEWCGROUP) for the calling thread: a fresh namespace rooted
/// at its current cgroup, owned by its current user namespace. The caller has
/// checked CAP_SYS_ADMIN (and run any NEWUSER first). Membership is unchanged.
pub fn unshare_cgroup() -> isize {
    let pid = crate::current_pid();
    let (cg, owner) = {
        let rq = crate::RUN_QUEUE.lock();
        match rq.find_pid(pid) { Some(t) => (t.cgroup, t.userns.id()), None => return -3 }
    };
    let new = match new_cgns(cg, owner) { Ok(n) => n, Err(e) => return e };
    let old = {
        let mut rq = crate::RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) { Some(t) => core::mem::replace(&mut t.cgns, new), None => return -3 }
    };
    drop(old);
    0
}

/// setns(nsfd, nstype): `kind`/`id` come from the fd, which keeps `id` alive.
pub fn setns_fd(kind: usize, id: NsId, nstype: usize, alone: bool, chrooted: bool) -> isize {
    if nstype != 0 && nstype != clone_flag(kind) { return -22; }
    if !matches!(kind, 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7) { return -22; }
    join(alloc::vec![(kind, NsRef::get(id))], alone, chrooted)
}

/// setns(pidfd, flags): join the namespaces of process `tgid` named by `flags`.
pub fn setns_pid(tgid: u32, flags: usize, alone: bool, chrooted: bool) -> isize {
    const KNOWN: usize = CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET;
    if flags == 0 || flags & !KNOWN != 0 { return -22; }
    let mut items = alloc::vec::Vec::new();
    {
        let rq = crate::RUN_QUEUE.lock();
        let Some(t) = rq.find_pid(tgid) else { return -3 };
        if flags & CLONE_NEWUSER != 0 { items.push((0usize, t.userns.clone())); }
        if flags & CLONE_NEWPID != 0 { items.push((1usize, t.pidns.clone())); }
        if flags & CLONE_NEWNET != 0 { items.push((2usize, t.netns.clone())); }
        if flags & CLONE_NEWIPC != 0 { items.push((3usize, t.ipcns.clone())); }
        if flags & CLONE_NEWUTS != 0 { items.push((5usize, t.utsns.clone())); }
        if flags & CLONE_NEWNS != 0 { items.push((4usize, t.mntns.clone())); }
        if flags & CLONE_NEWCGROUP != 0 { items.push((6usize, t.cgns.clone())); }
    }
    join(items, alone, chrooted)
}

/// Hook for setns(fd, CLONE_NEWCGROUP) (lane setns): move the calling thread
/// into existing cgroup namespace `id`, which the caller must hold a reference
/// to (it came from an ns fd). The caller checks CAP_SYS_ADMIN in
/// `cgns_owner(id)` and in its own user namespace. Membership is unchanged.
/// 0, or -22 when `id` is not a cgroup namespace.
pub fn setns_cgroup(id: NsId) -> isize {
    if !is_cgns(id) { return -22; }
    let new = NsRef::get(id);
    let pid = crate::current_pid();
    let old = {
        let mut rq = crate::RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) { Some(t) => core::mem::replace(&mut t.cgns, new), None => return -3 }
    };
    drop(old);
    0
}

// ── /proc/sys/net (per network namespace) ────────────────────────────────────

pub struct NetCtl { pub path: &'static str, pub def: u32, pub max: u32, pub pair: bool }
pub const NETCTL_N: usize = 40;
/// Entries with `pair` own this slot and the next ("lo hi" files); the next has an empty path.
pub static NETCTL: [NetCtl; NETCTL_N] = [
    NetCtl { path: "core/somaxconn", def: 4096, max: 2147483647, pair: false },
    NetCtl { path: "core/rmem_max", def: 212992, max: 2147483647, pair: false },
    NetCtl { path: "core/wmem_max", def: 212992, max: 2147483647, pair: false },
    NetCtl { path: "ipv4/ip_forward", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/ip_unprivileged_port_start", def: 1024, max: 65535, pair: false },
    NetCtl { path: "ipv4/ip_local_port_range", def: 32768, max: 65535, pair: true },
    NetCtl { path: "", def: 60999, max: 65535, pair: false },
    NetCtl { path: "ipv4/ping_group_range", def: 1, max: 4294967294, pair: true },
    NetCtl { path: "", def: 0, max: 4294967294, pair: false },
    NetCtl { path: "ipv4/ip_nonlocal_bind", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/ip_default_ttl", def: 64, max: 255, pair: false },
    NetCtl { path: "ipv4/tcp_syncookies", def: 1, max: 2, pair: false },
    NetCtl { path: "ipv4/conf/all/forwarding", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/conf/all/route_localnet", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/conf/all/rp_filter", def: 0, max: 2, pair: false },
    NetCtl { path: "ipv4/conf/all/arp_notify", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/conf/default/forwarding", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/conf/default/route_localnet", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/conf/default/rp_filter", def: 0, max: 2, pair: false },
    NetCtl { path: "ipv4/conf/default/arp_notify", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/conf/lo/forwarding", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/conf/lo/route_localnet", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv4/conf/lo/rp_filter", def: 0, max: 2, pair: false },
    NetCtl { path: "ipv4/conf/lo/arp_notify", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/all/disable_ipv6", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/all/forwarding", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/all/accept_ra", def: 1, max: 2, pair: false },
    NetCtl { path: "ipv6/conf/all/accept_dad", def: 1, max: 2, pair: false },
    NetCtl { path: "ipv6/conf/default/disable_ipv6", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/default/forwarding", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/default/accept_ra", def: 1, max: 2, pair: false },
    NetCtl { path: "ipv6/conf/default/accept_dad", def: 1, max: 2, pair: false },
    NetCtl { path: "ipv6/conf/lo/disable_ipv6", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/lo/forwarding", def: 0, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/lo/accept_ra", def: 1, max: 2, pair: false },
    NetCtl { path: "ipv6/conf/lo/accept_dad", def: 1, max: 2, pair: false },
    NetCtl { path: "ipv6/conf/all/autoconf", def: 1, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/default/autoconf", def: 1, max: 1, pair: false },
    NetCtl { path: "ipv6/conf/lo/autoconf", def: 1, max: 1, pair: false },
    NetCtl { path: "ipv6/bindv6only", def: 0, max: 1, pair: false },
];
const fn netctl_init() -> [[AtomicU32; NETCTL_N]; NS_MAX] {
    let mut out = [const { [const { AtomicU32::new(0) }; NETCTL_N] }; NS_MAX];
    let mut i = 0;
    while i < NS_MAX { let mut k = 0; while k < NETCTL_N { out[i][k] = AtomicU32::new(NETCTL[k].def); k += 1; } i += 1; }
    // The initial namespace is open to every group, as on common distributions
    // (systemd sysctl.d); a new namespace starts at the kernel default "1 0".
    out[0][7] = AtomicU32::new(0); out[0][8] = AtomicU32::new(2147483647);
    out
}
static NETCTL_V: [[AtomicU32; NETCTL_N]; NS_MAX] = netctl_init();
fn netctl_reset(id: usize) {
    for k in 0..NETCTL_N { NETCTL_V[id][k].store(NETCTL[k].def, Release); }
    let mut t = IFCTL.lock();
    for e in t.iter_mut() { if e.used && e.ns as usize == id { e.used = false; } }
}
/// Slot of the file `rel` (path below /proc/sys/net/).
pub fn netctl_find(rel: &[u8]) -> Option<usize> {
    NETCTL.iter().position(|c| !c.path.is_empty() && c.path.as_bytes() == rel)
}
pub fn netctl_get(ns: NsId, k: usize) -> u32 {
    if k >= NETCTL_N { return ifctl_get(ns, k); }
    NETCTL_V[ns as usize][k].load(Acquire)
}
/// Text of file `k` in netns `ns` (value, or "lo\thi" for pairs), with trailing newline.
pub fn netctl_text(ns: NsId, k: usize, out: &mut [u8]) -> usize {
    let mut n = 0;
    fn num(mut v: u32, out: &mut [u8], n: &mut usize) {
        let mut d = [0u8; 10]; let mut i = 0;
        loop { d[i] = b'0' + (v % 10) as u8; i += 1; v /= 10; if v == 0 { break; } }
        while i > 0 { i -= 1; out[*n] = d[i]; *n += 1; }
    }
    num(netctl_get(ns, k), out, &mut n);
    if k < NETCTL_N && NETCTL[k].pair { out[n] = b'\t'; n += 1; num(netctl_get(ns, k + 1), out, &mut n); }
    out[n] = b'\n';
    n + 1
}
/// Parse and store a write to file `k` of netns `ns`. Negative errno on failure.
pub fn netctl_set(ns: NsId, k: usize, data: &[u8]) -> isize {
    if k >= NETCTL_N { return ifctl_set(ns, k, data); }
    let mut v = [0u64; 2]; let mut cnt = 0; let mut i = 0;
    while i < data.len() {
        let c = data[i];
        if c.is_ascii_digit() {
            if cnt >= 2 { return -22; }
            let mut x = 0u64;
            while i < data.len() && data[i].is_ascii_digit() { x = x * 10 + (data[i] - b'0') as u64; if x > u32::MAX as u64 { return -22; } i += 1; }
            v[cnt] = x; cnt += 1;
        } else if c == b' ' || c == b'\t' || c == b'\n' || c == b'\0' { i += 1; } else { return -22; }
    }
    let want = if NETCTL[k].pair { 2 } else { 1 };
    if cnt != want { return -22; }
    for j in 0..want { if v[j] > NETCTL[k + j].max as u64 { return -22; } }
    if want == 2 && NETCTL[k].path != "ipv4/ping_group_range" && v[0] > v[1] { return -22; }
    // Linux devinet_sysctl_forward: ip_forward and conf/all/forwarding are one variable and
    // fan out to every interface ONLY when the value changes (`*valp != val`), so a repeated
    // ip_forward=1 keeps a per-bridge forwarding=0 (netavark --internal).
    let p = NETCTL[k].path;
    let fwd_all = p == "ipv4/ip_forward" || p == "ipv4/conf/all/forwarding";
    let changed = fwd_all && netctl_find(b"ipv4/conf/all/forwarding").map(|a| netctl_get(ns, a) != v[0] as u32).unwrap_or(true);
    for j in 0..want { NETCTL_V[ns as usize][k + j].store(v[j] as u32, Release); }
    if changed {
        fwd4_fanout(ns, v[0] as u32);
    }
    0
}

// ── per-interface entries (/proc/sys/net/ipv{4,6}/conf/<if>/...) ─────────────

/// Fields of one interface, in slot order. Slot = NETCTL_N + entry * IF_STRIDE + field.
const IF_FIELDS: [(&str, &str, u32); 9] = [
    ("ipv4", "forwarding", 1), ("ipv4", "rp_filter", 2), ("ipv4", "route_localnet", 1), ("ipv4", "arp_notify", 1),
    ("ipv6", "disable_ipv6", 1), ("ipv6", "forwarding", 1), ("ipv6", "accept_ra", 2), ("ipv6", "accept_dad", 2),
    ("ipv6", "autoconf", 1),
];
const IF_STRIDE: usize = 16;
const IF_MAX: usize = 64;
#[derive(Clone, Copy)]
struct IfEnt { used: bool, ns: u16, nlen: u8, name: [u8; 16], v: [u32; 9] }
static IFCTL: spin::Mutex<[IfEnt; IF_MAX]> =
    spin::Mutex::new([IfEnt { used: false, ns: 0, nlen: 0, name: [0; 16], v: [0; 9] }; IF_MAX]);
/// `fn(ns, index, name_out) -> len` (0 = no interface at `index`): the names of the links of a netns.
static IF_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub fn set_ifnames_hook(f: fn(NsId, usize, &mut [u8; 16]) -> usize) { IF_HOOK.store(f as usize, Release); }
fn if_name(ns: NsId, idx: usize, out: &mut [u8; 16]) -> usize {
    let h = IF_HOOK.load(Acquire);
    if h == 0 { return 0; }
    // SAFETY: only `set_ifnames_hook` stores here.
    let f: fn(NsId, usize, &mut [u8; 16]) -> usize = unsafe { core::mem::transmute(h) };
    f(ns, idx, out)
}
fn if_exists(ns: NsId, name: &[u8]) -> bool {
    let mut b = [0u8; 16];
    let mut i = 0;
    loop {
        let l = if_name(ns, i, &mut b);
        if l == 0 { return false; }
        if &b[..l] == name { return true; }
        i += 1;
        if i > 64 { return false; }
    }
}
fn static_field(ns: NsId, scope: &str, ipv: &str, field: &str) -> u32 {
    let mut buf = [0u8; 64]; let mut n = 0;
    for part in [ipv, "/conf/", scope, "/", field] { for &c in part.as_bytes() { buf[n] = c; n += 1; } }
    netctl_find(&buf[..n]).map(|k| netctl_get(ns, k)).unwrap_or(0)
}
/// Find or create the entry of `(ns, name)`, new ones inheriting `conf/default`. Drops stale entries of `ns`.
fn ifctl_entry(ns: NsId, name: &[u8]) -> Option<usize> {
    if name.is_empty() || name.len() > 15 { return None; }
    if !if_exists(ns, name) { return None; }
    let mut t = IFCTL.lock();
    let mut free = None;
    for (i, e) in t.iter().enumerate() {
        if e.used && e.ns == ns && &e.name[..e.nlen as usize] == name { return Some(i); }
        if !e.used && free.is_none() { free = Some(i); }
    }
    // Reclaim entries of links that are gone before giving up.
    if free.is_none() {
        for i in 0..IF_MAX {
            let e = t[i];
            if e.used && !if_exists_unlocked(e.ns, &e.name[..e.nlen as usize]) { t[i].used = false; if free.is_none() { free = Some(i); } }
        }
    }
    let i = free?;
    let mut v = [0u32; 9];
    for (j, f) in IF_FIELDS.iter().enumerate() { v[j] = static_field(ns, "default", f.0, f.1); }
    let mut nm = [0u8; 16]; nm[..name.len()].copy_from_slice(name);
    t[i] = IfEnt { used: true, ns, nlen: name.len() as u8, name: nm, v };
    Some(i)
}
fn if_exists_unlocked(ns: NsId, name: &[u8]) -> bool { if_exists(ns, name) }
/// Forget entries of `ns` whose link no longer exists (a re-created link starts from the defaults again).
fn ifctl_prune(ns: NsId) {
    let mut i = 0;
    while i < IF_MAX {
        let e = { let t = IFCTL.lock(); t[i] };
        if e.used && e.ns == ns && !if_exists(ns, &e.name[..e.nlen as usize]) {
            let mut t = IFCTL.lock();
            if t[i].used && t[i].ns == ns && t[i].name == e.name { t[i].used = false; }
        }
        i += 1;
    }
}
/// Slot of `/proc/sys/net/<rel>` as seen from netns `ns` (static files, then per-interface ones).
pub fn netctl_find_ns(ns: NsId, rel: &[u8]) -> Option<usize> {
    if let Some(k) = netctl_find(rel) { return Some(k); }
    let rest = if let Some(r) = rel.strip_prefix(b"ipv4/conf/") { (0usize, r) }
        else if let Some(r) = rel.strip_prefix(b"ipv6/conf/") { (1usize, r) } else { return None };
    let sl = rest.1.iter().position(|&c| c == b'/')?;
    let (name, field) = (&rest.1[..sl], &rest.1[sl + 1..]);
    let ipv = if rest.0 == 0 { "ipv4" } else { "ipv6" };
    let f = IF_FIELDS.iter().position(|f| f.0 == ipv && f.1.as_bytes() == field)?;
    ifctl_prune(ns);
    let e = ifctl_entry(ns, name)?;
    Some(NETCTL_N + e * IF_STRIDE + f)
}
fn ifctl_get(ns: NsId, k: usize) -> u32 {
    let (e, f) = ((k - NETCTL_N) / IF_STRIDE, (k - NETCTL_N) % IF_STRIDE);
    let t = IFCTL.lock();
    if e < IF_MAX && f < 9 && t[e].used && t[e].ns == ns { t[e].v[f] } else { 0 }
}
fn ifctl_set(ns: NsId, k: usize, data: &[u8]) -> isize {
    let (e, f) = ((k - NETCTL_N) / IF_STRIDE, (k - NETCTL_N) % IF_STRIDE);
    if e >= IF_MAX || f >= 9 { return -2; }
    let mut x = 0u64; let mut seen = false;
    for &c in data {
        if c.is_ascii_digit() { x = x * 10 + (c - b'0') as u64; seen = true; if x > u32::MAX as u64 { return -22; } }
        else if c == b'\n' || c == b' ' || c == b'\0' || c == b'\t' { } else { return -22; }
    }
    if !seen || x > IF_FIELDS[f].2 as u64 { return -22; }
    let mut t = IFCTL.lock();
    if !(t[e].used && t[e].ns == ns) { return -2; }
    t[e].v[f] = x as u32;
    0
}
/// Linux `devinet_ctl` forward-all: set all/default/lo and every link's `ipv4 forwarding`; mirrors ip_forward.
fn fwd4_fanout(ns: NsId, v: u32) {
    for p in ["ipv4/ip_forward", "ipv4/conf/all/forwarding", "ipv4/conf/default/forwarding", "ipv4/conf/lo/forwarding"] {
        if let Some(k) = netctl_find(p.as_bytes()) { NETCTL_V[ns as usize][k].store(v, Release); }
    }
    let mut i = 0; let mut b = [0u8; 16];
    loop {
        let l = if_name(ns, i, &mut b);
        if l == 0 || i > 64 { break; }
        if let Some(e) = ifctl_entry(ns, &b[..l]) { IFCTL.lock()[e].v[0] = v; }
        i += 1;
    }
}
/// IPv4 forwarding state of interface `ifname` in netns `ns` (the forward gate of the net server).
pub fn forwarding(ns: NsId, ifname: &[u8]) -> bool {
    match ifname {
        b"all" | b"default" | b"lo" => {
            let p: &[u8] = match ifname { b"all" => b"ipv4/conf/all/forwarding", b"default" => b"ipv4/conf/default/forwarding", _ => b"ipv4/conf/lo/forwarding" };
            netctl_find(p).map(|k| netctl_get(ns, k) != 0).unwrap_or(false)
        }
        _ => {
            { let t = IFCTL.lock();
              for e in t.iter() { if e.used && e.ns == ns && &e.name[..e.nlen as usize] == ifname { return e.v[0] != 0; } } }
            static_field(ns, "default", "ipv4", "forwarding") != 0
        }
    }
}
/// IPv6 forwarding of interface `ifname` in netns `ns`: `conf/all/forwarding` or the interface's own.
pub fn forwarding6(ns: NsId, ifname: &[u8]) -> bool {
    if netctl_find(b"ipv6/conf/all/forwarding").map_or(false, |k| netctl_get(ns, k) != 0) { return true; }
    { let t = IFCTL.lock();
      for e in t.iter() { if e.used && e.ns == ns && &e.name[..e.nlen as usize] == ifname { return e.v[5] != 0; } } }
    static_field(ns, "default", "ipv6", "forwarding") != 0
}
/// Is IPv6 disabled on interface `ifname` of netns `ns` (`conf/all/disable_ipv6` or the interface's own)?
pub fn ipv6_disabled(ns: NsId, ifname: &[u8]) -> bool {
    if netctl_find(b"ipv6/conf/all/disable_ipv6").map_or(false, |k| netctl_get(ns, k) != 0) { return true; }
    { let t = IFCTL.lock();
      for e in t.iter() { if e.used && e.ns == ns && &e.name[..e.nlen as usize] == ifname { return e.v[4] != 0; } } }
    static_field(ns, "default", "ipv6", "disable_ipv6") != 0
}
/// Per-interface IPv6 conf value `field` ("accept_ra", "autoconf") of `ifname` in netns `ns`.
pub fn ipv6_conf(ns: NsId, ifname: &[u8], field: &str) -> u32 {
    if let Some(j) = IF_FIELDS.iter().position(|f| f.0 == "ipv6" && f.1 == field) {
        let t = IFCTL.lock();
        for e in t.iter() { if e.used && e.ns == ns && &e.name[..e.nlen as usize] == ifname { return e.v[j]; } }
    }
    static_field(ns, "default", "ipv6", field)
}
/// May `pid` (an unprivileged ICMP "ping" socket) be created in its netns?
pub fn ping_allowed(pid: u32) -> bool {
    let ns = netns_of(pid);
    let k = netctl_find(b"ipv4/ping_group_range").unwrap();
    let (lo, hi) = (netctl_get(ns, k), netctl_get(ns, k + 1));
    let g = crate::egid_of(pid);
    if lo <= g && g <= hi { return true; }
    crate::caps::pid_capable_in(pid, owner_userns(2, ns), 13) // CAP_NET_RAW
}
/// net.ipv6.bindv6only of namespace `ns`: the IPV6_V6ONLY a new AF_INET6 socket starts with.
pub fn bindv6only(ns: NsId) -> bool {
    netctl_find(b"ipv6/bindv6only").map_or(false, |k| netctl_get(ns, k) != 0)
}
/// May `pid` bind `port` in its netns `ns` (ip_unprivileged_port_start / CAP_NET_BIND_SERVICE)?
pub fn bind_port_allowed(pid: u32, ns: NsId, port: u16) -> bool {
    let k = netctl_find(b"ipv4/ip_unprivileged_port_start").unwrap();
    port as u32 >= netctl_get(ns, k) || crate::caps::pid_capable_in(pid, owner_userns(2, ns), 10)
}
