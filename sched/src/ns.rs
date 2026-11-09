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
pub const NS_MAX: usize = 256;
/// Linux `/proc/sys/user/max_user_namespaces` nesting limit.
pub const MAX_USERNS_LEVEL: u8 = 32;
/// Clone/unshare flags the namespace layer accepts so far.
pub const SUPPORTED: usize = 0x1000_0000 | 0x4000_0000 | 0x0800_0000 | 0x0400_0000; // NEWUSER | NEWNET | NEWIPC | NEWUTS

const ZERO32: AtomicU32 = AtomicU32::new(0);
const ZERO16: AtomicU16 = AtomicU16::new(0);
const ZERO8: AtomicU8 = AtomicU8::new(0);
static REFS: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
static PARENT: [AtomicU16; NS_MAX] = [ZERO16; NS_MAX];
static LEVEL: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];
/// Host euid / egid of the creator (the namespace owner).
static OWNER_EUID: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
static OWNER_EGID: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
/// Admin state of a network namespace's `lo` (recorded by RTM_NEWLINK; reset
/// when the id is allocated).
static LO_UP: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];
pub fn lo_admin_up(id: NsId) -> bool { id != 0 && LO_UP[id as usize].load(Acquire) != 0 }
pub fn set_lo_admin_up(id: NsId, up: bool) { if id != 0 { LO_UP[id as usize].store(up as u8, Release); } }

/// Identity-only uid/gid maps: up to `MAP_EXT` extents per namespace, each
/// `[inside, outside, count]` (inside == outside is enforced on write).
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
        if REFS[id as usize].fetch_sub(1, AcqRel) != 1 { return; }
        if IS_MNT[id as usize].load(Acquire) != 0 {
            // Freeing a mount tree can drop superblock refs (port calls), and
            // put may run under RUN_QUEUE: park the slot as DYING and let
            // `reap` (next mntns create / mount syscall) free it.
            REFS[id as usize].store(DYING, Release);
            ZOMBIES[id as usize / 64].fetch_or(1u64 << (id as usize % 64), AcqRel);
            return;
        }
        // Last reference: the slot is free again; release the parent and any
        // owning user namespace (setns lane).
        let ou = OWNER_UNS[id as usize].swap(0, AcqRel);
        if ou != 0 { put(ou); }
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
            MAP_N[0][id].store(0, Release);
            MAP_N[1][id].store(0, Release);
            SETGROUPS_DENY[id].store(0, Release);
            OWNER_EUID[id].store(euid, Release);
            OWNER_EGID[id].store(egid, Release);
            return Ok(NsRef(id as NsId));
        }
    }
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

/// Owner user namespace recorded for namespace `id`.
pub fn owner_user(id: NsId) -> NsId { if id == 0 { 0 } else { OWNER_USER[id as usize].load(Acquire) } }

/// Create a mount namespace id owned by user namespace `owner_user`.
pub fn new_mntns(owner_user: NsId) -> Result<NsRef, isize> {
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            PARENT[id].store(0, Release);
            LEVEL[id].store(0, Release);
            OWNER_USER[id].store(owner_user, Release);
            IS_MNT[id].store(1, Release);
            MNT_LIVE.fetch_add(1, AcqRel);
            return Ok(NsRef(id as NsId));
        }
    }
    Err(-28)
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
            MNT_LIVE.fetch_sub(1, AcqRel);
            REFS[id].store(0, Release);
        }
    }
}

/// Number of parked (DYING) mount namespaces.
pub fn dying_count() -> usize { (1..NS_MAX).filter(|&i| REFS[i].load(Relaxed) == DYING).count() }

/// Create a flat (parentless) namespace id: net or ipc. Errors: ENOSPC (28).
pub fn new_flat(owner: NsId) -> Result<NsRef, isize> {
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            PARENT[id].store(0, Release);
            LEVEL[id].store(0, Release);
            set_owner_userns(id as NsId, owner);
            LO_UP[id].store(0, Release);
            return Ok(NsRef(id as NsId));
        }
    }
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
    let nn = if net { match new_flat(ou) { Ok(n) => Some(n), Err(e) => return e } } else { None };
    let ni = if ipc { match new_flat(ou) { Ok(n) => Some(n), Err(e) => return e } } else { None };
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
    let r = new_flat(owner)?;
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
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            let p = NsRef::get(parent);
            PARENT[id].store(p.0, Release);
            core::mem::forget(p);
            LEVEL[id].store(level + 1, Release);
            PID_INIT[id].store(0, Release);
            PID_FLAGS[id].store(0, Release);
            set_owner_userns(id as NsId, owner);
            crate::pidns::USED.store(true, Relaxed);
            return Ok(NsRef(id as NsId));
        }
    }
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

/// Is kernel id `kid` mapped in namespace `ns` (id 0 maps everything)?
pub fn mapped(kind: usize, ns: NsId, kid: u32) -> bool {
    if ns == 0 { return true; }
    let n = MAP_N[kind][ns as usize].load(Acquire) as usize;
    (0..n).any(|i| { let (_, o, c) = ext(kind, ns as usize, i); kid >= o && (kid - o) < c })
}

/// Every id of `ids` (u32::MAX = "leave unchanged" is skipped) is mapped in
/// the caller's user namespace (kind 0 uid, 1 gid)? Unmapped targets of the
/// set*id/chown family are EINVAL.
pub fn ids_mapped(kind: usize, ids: &[u32]) -> bool {
    let ns = current_userns();
    ids.iter().all(|&i| i == u32::MAX || mapped(kind, ns, i))
}

/// Overflow id shown for ids with no mapping.
pub const OVERFLOW_ID: u32 = 65534;

/// The id `kid` as the calling task sees it (identity maps: itself, or the
/// overflow id when unmapped in its user namespace).
pub fn view_id(kind: usize, kid: u32) -> u32 {
    let ns = current_userns();
    if mapped(kind, ns, kid) { kid } else { OVERFLOW_ID }
}

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

/// Text of uid_map (kind 0) / gid_map (kind 1) of user namespace `ns`.
pub fn map_text(kind: usize, ns: NsId, out: &mut [u8]) -> usize {
    let mut n = 0;
    if ns == 0 { put_line(0, 0, u32::MAX, out, &mut n); return n; }
    let c = MAP_N[kind][ns as usize].load(Acquire) as usize;
    for i in 0..c { let (a, b, l) = ext(kind, ns as usize, i); put_line(a, b, l, out, &mut n); }
    n
}

pub fn setgroups_denied(ns: NsId) -> bool { ns != 0 && SETGROUPS_DENY[ns as usize].load(Acquire) != 0 }

/// Identity of a process (userns, host euid/egid, effective caps) frozen at
/// open() time, like Linux `file->f_cred` of a map file.
#[derive(Clone, Copy)]
pub struct Opener(NsId, u32, u32, u64);

pub fn snapshot(pid: u32) -> Option<Opener> {
    writer_info(pid)
}

fn writer_info(pid: u32) -> Option<Opener> {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| Opener(t.userns.id(), t.euid, t.egid, t.caps[0]))
}

/// May the opener write `target`'s maps at all? (open-time EPERM)
pub fn open_allowed(op: Opener, target: u32, kind: usize) -> bool {
    let tns = userns_of(target);
    if tns == 0 { return false; }
    rights(op, tns, kind.min(1)).0
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

fn rights(op: Opener, tns: NsId, kind: usize) -> (bool, bool, u32) {
    let Opener(wns, eu, eg, caps) = op;
    let cap: u32 = if kind == 0 { 7 } else { 6 };
    let has = caps >> cap & 1 != 0;
    let (oe, og) = owner(tns);
    let (me, owner_id) = if kind == 0 { (eu, oe) } else { (eg, og) };
    let ownerish = me == owner_id;
    let in_ns = wns == tns;
    let in_parent = wns == parent(tns);
    let ok = (in_ns || in_parent) && (has || ownerish);
    (ok, in_parent && has, me)
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 10 { return None; }
    let mut v: u64 = 0;
    for &c in s { if !c.is_ascii_digit() { return None; } v = v * 10 + (c - b'0') as u64; }
    if v > u32::MAX as u64 { None } else { Some(v as u32) }
}

/// Write uid_map (kind 0) / gid_map (kind 1) of `target`'s user namespace on
/// behalf of `writer`. Identity-only. Returns bytes consumed or -errno.
pub fn write_map(opener: Opener, writer: u32, target: u32, kind: usize, data: &[u8]) -> isize {
    let _g = MapGuard::lock();
    let tns = userns_of(target);
    if tns == 0 { return -1; }
    let t = tns as usize;
    if MAP_N[kind][t].load(Acquire) != 0 { return -1; } // written once
    // Both the opener (f_cred) and the writer must be permitted; the
    // privileged branch needs both privileged, and both must be the same id.
    let (ok_o, priv_o, me_o) = rights(opener, tns, kind);
    let (ok_w, priv_w, me_w) = writer_rights(writer, tns, kind);
    if !ok_o || !ok_w { return -1; }
    let privileged = priv_o && priv_w;
    let me = me_o;
    if !privileged && me_w != me_o { return -1; }
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
        if (f[0] as u64 + f[2] as u64) > u32::MAX as u64 + 1 || (f[1] as u64 + f[2] as u64) > u32::MAX as u64 + 1 { return -22; }
        exts[n] = (f[0], f[1], f[2]); n += 1;
    }
    if n == 0 { return -22; }
    for i in 0..n {
        if exts[i].0 != exts[i].1 { return -1; } // identity maps only
        for j in 0..i {
            let (a, c) = (exts[i], exts[j]);
            if (a.0 as u64) < c.0 as u64 + c.2 as u64 && (c.0 as u64) < a.0 as u64 + a.2 as u64 { return -22; }
        }
    }
    let par = parent(tns);
    if privileged {
        for i in 0..n {
            if par != 0 {
                let (a, _, c) = exts[i];
                let covered = mapped(kind, par, a) && mapped(kind, par, a + (c - 1));
                if !covered { return -1; }
            }
        }
    } else {
        if n != 1 || exts[0].2 != 1 || exts[0].1 != me { return -1; }
        if par != 0 && !mapped(kind, par, me) { return -1; }
        if kind == 1 && SETGROUPS_DENY[t].load(Acquire) == 0 { return -1; }
    }
    for i in 0..n {
        let b = (t * MAP_EXT + i) * 3;
        MAP_DATA[kind][b].store(exts[i].0, Release);
        MAP_DATA[kind][b + 1].store(exts[i].1, Release);
        MAP_DATA[kind][b + 2].store(exts[i].2, Release);
    }
    MAP_N[kind][t].store(n as u8, Release);
    data.len() as isize
}

/// Write /proc/<target>/setgroups ("allow" / "deny"). Bytes consumed or -errno.
pub fn write_setgroups(opener: Opener, writer: u32, target: u32, data: &[u8]) -> isize {
    let _g = MapGuard::lock();
    let tns = userns_of(target);
    if tns == 0 { return -1; }
    let (ok, _, _) = writer_rights(writer, tns, 1);
    let (ok_o, _, _) = rights(opener, tns, 1);
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
        7 => t.pidns_child.id(), 5 => t.utsns.id(), _ => 0,
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
struct Ctx { userns: NsId, admin: bool, pidns: NsId, euid: u32 }

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
            ctx.userns = id; ctx.admin = true;
            0
        }
        1 | 7 => {
            if !cap_in(ctx, owner_userns(1, id)) { return -1; }
            if !covers(ctx.pidns, id) { return -22; }
            0
        }
        2 | 3 => if cap_in(ctx, owner_userns(kind, id)) { 0 } else { -1 },
        5 => if cap_in(ctx, UTS[id as usize].lock().owner) { 0 } else { -1 },
        _ => -22, // mnt, cgroup: other lanes
    }
}

/// Join the namespaces in `items` (kind, held reference). All entries are
/// checked before any is applied. 0 or a negative errno.
pub fn join(items: alloc::vec::Vec<(usize, NsRef)>, alone: bool, chrooted: bool) -> isize {
    let pid = crate::current_pid();
    let mut ctx = {
        let rq = crate::RUN_QUEUE.lock();
        match rq.find_pid(pid) {
            Some(t) => Ctx { userns: t.userns.id(), admin: t.caps[0] >> 21 & 1 != 0, pidns: t.pidns.id(), euid: t.euid },
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
        let Some(t) = rq.find_pid_mut(pid) else { return -3 };
        for (kind, n) in items {
            match kind {
                0 => {
                    t.caps = crate::caps::userns_root_caps(t.caps[3]);
                    old.push(core::mem::replace(&mut t.userns, n));
                }
                1 | 7 => old.push(core::mem::replace(&mut t.pidns_child, n)),
                2 => old.push(core::mem::replace(&mut t.netns, n)),
                5 => old.push(core::mem::replace(&mut t.utsns, n)),
                _ => old.push(core::mem::replace(&mut t.ipcns, n)),
            }
        }
    }
    drop(old);
    0
}

/// setns(nsfd, nstype): `kind`/`id` come from the fd, which keeps `id` alive.
pub fn setns_fd(kind: usize, id: NsId, nstype: usize, alone: bool, chrooted: bool) -> isize {
    if nstype != 0 && nstype != clone_flag(kind) { return -22; }
    if !matches!(kind, 0 | 1 | 2 | 3 | 5 | 7) { return -22; }
    join(alloc::vec![(kind, NsRef::get(id))], alone, chrooted)
}

/// setns(pidfd, flags): join the namespaces of process `tgid` named by `flags`.
pub fn setns_pid(tgid: u32, flags: usize, alone: bool, chrooted: bool) -> isize {
    const KNOWN: usize = CLONE_NEWNS | CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET;
    if flags == 0 || flags & !KNOWN != 0 { return -22; }
    if flags & (CLONE_NEWNS | CLONE_NEWCGROUP) != 0 { return -22; }
    let mut items = alloc::vec::Vec::new();
    {
        let rq = crate::RUN_QUEUE.lock();
        let Some(t) = rq.find_pid(tgid) else { return -3 };
        if flags & CLONE_NEWUSER != 0 { items.push((0usize, t.userns.clone())); }
        if flags & CLONE_NEWPID != 0 { items.push((1usize, t.pidns.clone())); }
        if flags & CLONE_NEWNET != 0 { items.push((2usize, t.netns.clone())); }
        if flags & CLONE_NEWIPC != 0 { items.push((3usize, t.ipcns.clone())); }
        if flags & CLONE_NEWUTS != 0 { items.push((5usize, t.utsns.clone())); }
    }
    join(items, alone, chrooted)
}
