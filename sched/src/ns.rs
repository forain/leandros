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
pub const SUPPORTED: usize = 0x1000_0000 | 0x4000_0000 | 0x0800_0000; // NEWUSER | NEWNET | NEWIPC

const ZERO32: AtomicU32 = AtomicU32::new(0);
const ZERO16: AtomicU16 = AtomicU16::new(0);
const ZERO8: AtomicU8 = AtomicU8::new(0);
static REFS: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
static PARENT: [AtomicU16; NS_MAX] = [ZERO16; NS_MAX];
static LEVEL: [AtomicU8; NS_MAX] = [ZERO8; NS_MAX];
/// Host euid / egid of the creator (the namespace owner).
static OWNER_EUID: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];
static OWNER_EGID: [AtomicU32; NS_MAX] = [ZERO32; NS_MAX];

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
        // A cgroup namespace unpins its root cgroup (its owner userns, held as
        // PARENT, is released by the loop below).
        if CGNS_FLAG[id as usize].swap(0, AcqRel) != 0 {
            crate::cgroup::ns_unpin(CGNS_ROOT[id as usize].load(Acquire));
        }
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

/// Create a flat (parentless) namespace id: net or ipc. Errors: ENOSPC (28).
pub fn new_flat() -> Result<NsRef, isize> {
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            PARENT[id].store(0, Release);
            LEVEL[id].store(0, Release);
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
    let nn = if net { match new_flat() { Ok(n) => Some(n), Err(e) => return e } } else { None };
    let ni = if ipc { match new_flat() { Ok(n) => Some(n), Err(e) => return e } } else { None };
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
        _ => 0,
    }
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
    for id in 1..NS_MAX {
        if REFS[id].compare_exchange(0, 1, AcqRel, Relaxed).is_ok() {
            let p = NsRef::get(owner);
            PARENT[id].store(p.0, Release);
            core::mem::forget(p);
            LEVEL[id].store(0, Release);
            CGNS_ROOT[id].store(root, Release);
            crate::cgroup::ns_pin(root);
            CGNS_FLAG[id].store(1, Release);
            return Ok(NsRef(id as NsId));
        }
    }
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
