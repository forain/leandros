//! cgroup v2 core (design-cgroup lane CG1): the hierarchy, task membership,
//! migration rules and the pids/cpu counters. The cgroupfs that exposes it
//! (lane CG2) is a thin layer over this API.
//!
//! Hot state is atomic arrays in the `ns.rs` style, usable under RUN_QUEUE.
//! Cold state (names, owners) sits behind one `Mutex<CgTree>` that is only
//! taken from VFS-side operations (create/remove/lookup/path). Lock order:
//! CgTree, then RUN_QUEUE; nothing takes CgTree while holding RUN_QUEUE.
//!
//! Membership: every `Task` has `cgroup: CgId` (inherited at clone). The
//! charge happens in `RunQueue::enqueue` (`charge`) and is dropped in
//! `RunQueue::remove` (`uncharge`), so a zombie counts until it is reaped,
//! like Linux. `NR_TASKS` is hierarchical, `DIRECT` counts the tasks that
//! sit in the node itself (no-internal-process rule).
//!
//! Only the domain cgroup type exists; threads of a tgid always share a
//! cgroup (a move updates every thread).

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering::*};
use spin::Mutex;

pub type CgId = u16;
pub const CG_MAX: usize = 1024;
/// Hard cap on nesting depth (root = 0).
pub const MAX_DEPTH: u8 = 32;
pub const UNLIMITED: u32 = u32::MAX;

/// Controller bits, in the order they are listed.
pub const C_CPU: u8 = 1;
pub const C_IO: u8 = 2;
pub const C_MEMORY: u8 = 4;
pub const C_PIDS: u8 = 8;
pub const C_ALL: u8 = C_CPU | C_IO | C_MEMORY | C_PIDS;
pub const CTRL_NAMES: [&str; 4] = ["cpu", "io", "memory", "pids"];

const E_PERM: isize = -1;
const E_NOENT: isize = -2;
const E_SRCH: isize = -3;
const E_AGAIN: isize = -11;
const E_BUSY: isize = -16;
const E_EXIST: isize = -17;
const E_NODEV: isize = -19;
const E_INVAL: isize = -22;
const E_NOSPC: isize = -28;

const Z16: AtomicU16 = AtomicU16::new(0);
const Z32: AtomicU32 = AtomicU32::new(0);
const Z64: AtomicU64 = AtomicU64::new(0);
const Z8: AtomicU8 = AtomicU8::new(0);
const UNL32: AtomicU32 = AtomicU32::new(UNLIMITED);

/// 0 free, 1 alive, 2 being created or removed.
static ALIVE: [AtomicU8; CG_MAX] = [Z8; CG_MAX];
static PARENT: [AtomicU16; CG_MAX] = [Z16; CG_MAX];
static DEPTH: [AtomicU8; CG_MAX] = [Z8; CG_MAX];
/// Bumped on every create; lets a handle detect a deleted-and-reused node.
static GEN: [AtomicU32; CG_MAX] = [Z32; CG_MAX];
/// Threads in the node and all descendants (zombies included until reap).
static NR_TASKS: [AtomicU32; CG_MAX] = [Z32; CG_MAX];
/// Threads in the node itself.
static DIRECT: [AtomicU32; CG_MAX] = [Z32; CG_MAX];
/// Live direct children.
static NR_CHILDREN: [AtomicU16; CG_MAX] = [Z16; CG_MAX];
/// Live descendants (hierarchical).
static NR_DESC: [AtomicU32; CG_MAX] = [Z32; CG_MAX];
/// cgroup namespaces whose root is this node (they pin it against rmdir).
static NS_PINS: [AtomicU32; CG_MAX] = [Z32; CG_MAX];
/// subtree_control: controllers enabled for the children.
static SUBTREE: [AtomicU8; CG_MAX] = [Z8; CG_MAX];
static PIDS_MAX: [AtomicU32; CG_MAX] = [UNL32; CG_MAX];
static PIDS_PEAK: [AtomicU32; CG_MAX] = [Z32; CG_MAX];
static PIDS_EV_MAX: [AtomicU32; CG_MAX] = [Z32; CG_MAX];
static MAX_DEPTH_LIM: [AtomicU32; CG_MAX] = [UNL32; CG_MAX];
static MAX_DESC_LIM: [AtomicU32; CG_MAX] = [UNL32; CG_MAX];
/// CPU time (ns) of exited tasks and removed children, charged to the node.
static DEAD_USER_NS: [AtomicU64; CG_MAX] = [Z64; CG_MAX];
static DEAD_SYS_NS: [AtomicU64; CG_MAX] = [Z64; CG_MAX];

struct CgTree {
    names: Vec<String>,
    owner: Vec<(u32, u32)>,
}

static TREE: Mutex<CgTree> = Mutex::new(CgTree { names: Vec::new(), owner: Vec::new() });

/// Initialise node 0 (idempotent; also called lazily by every VFS-side entry).
fn init_tree(t: &mut CgTree) {
    if t.names.is_empty() {
        t.names.resize(CG_MAX, String::new());
        t.owner.resize(CG_MAX, (0, 0));
        ALIVE[0].store(1, Release);
        GEN[0].store(1, Release);
    }
}

/// Node 0 is alive from boot, before anything calls `init_tree`.
#[inline]
pub fn alive(cg: CgId) -> bool {
    cg == 0 || ((cg as usize) < CG_MAX && ALIVE[cg as usize].load(Acquire) == 1)
}
pub fn generation(cg: CgId) -> u32 { if cg == 0 { 1 } else { GEN[cg as usize % CG_MAX].load(Acquire) } }
pub fn parent(cg: CgId) -> CgId { PARENT[cg as usize % CG_MAX].load(Acquire) }
pub fn depth(cg: CgId) -> u8 { DEPTH[cg as usize % CG_MAX].load(Acquire) }
/// Threads in the subtree of `cg` (zombies included).
pub fn subtree_tasks(cg: CgId) -> u32 { NR_TASKS[cg as usize % CG_MAX].load(Acquire) }
/// Threads directly in `cg`.
pub fn direct_tasks(cg: CgId) -> u32 { DIRECT[cg as usize % CG_MAX].load(Acquire) }
pub fn populated(cg: CgId) -> bool { subtree_tasks(cg) != 0 }
pub fn nr_children(cg: CgId) -> u32 { NR_CHILDREN[cg as usize % CG_MAX].load(Acquire) as u32 }
/// Live descendants (`cgroup.stat nr_descendants`).
pub fn nr_descendants(cg: CgId) -> u32 { NR_DESC[cg as usize % CG_MAX].load(Acquire) }
/// `cgroup.subtree_control` bits of `cg`.
pub fn subtree_control(cg: CgId) -> u8 { SUBTREE[cg as usize % CG_MAX].load(Acquire) }
/// `cgroup.controllers` bits of `cg`: the parent's subtree_control (every
/// supported controller at the root).
pub fn controllers(cg: CgId) -> u8 { if cg == 0 { C_ALL } else { subtree_control(parent(cg)) } }

/// Is `x` equal to or below `anc`?
pub fn in_subtree(mut x: CgId, anc: CgId) -> bool {
    let da = depth(anc);
    while depth(x) > da { x = parent(x); }
    x == anc
}

/// Deepest common ancestor of `a` and `b` (possibly one of them).
pub fn common_ancestor(mut a: CgId, mut b: CgId) -> CgId {
    while depth(a) > depth(b) { a = parent(a); }
    while depth(b) > depth(a) { b = parent(b); }
    while a != b { a = parent(a); b = parent(b); }
    a
}

// ── hierarchy ────────────────────────────────────────────────────────────────

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 255 && n != "." && n != ".." && !n.contains('/') && !n.contains('\0')
}

/// Child of `parent` called `name`.
pub fn lookup_child(parent_cg: CgId, name: &str) -> Option<CgId> {
    let mut t = TREE.lock();
    init_tree(&mut t);
    (1..CG_MAX).find(|&i| ALIVE[i].load(Acquire) == 1 && PARENT[i].load(Acquire) == parent_cg && t.names[i] == name)
        .map(|i| i as CgId)
}

/// Live children of `cg`, in creation-slot order.
pub fn children(cg: CgId) -> Vec<CgId> {
    let mut t = TREE.lock();
    init_tree(&mut t);
    (1..CG_MAX).filter(|&i| ALIVE[i].load(Acquire) == 1 && PARENT[i].load(Acquire) == cg).map(|i| i as CgId).collect()
}

pub fn name(cg: CgId) -> String {
    let mut t = TREE.lock();
    init_tree(&mut t);
    t.names[cg as usize % CG_MAX].clone()
}

/// Directory owner (host fsuid/fsgid of the creator).
pub fn dir_owner(cg: CgId) -> (u32, u32) {
    let mut t = TREE.lock();
    init_tree(&mut t);
    t.owner[cg as usize % CG_MAX]
}

/// chown of a cgroup directory (cgroupfs; the DAC check is the caller's).
pub fn set_dir_owner(cg: CgId, uid: u32, gid: u32) {
    let mut t = TREE.lock();
    init_tree(&mut t);
    if alive(cg) { t.owner[cg as usize % CG_MAX] = (uid, gid); }
}

/// mkdir: a new child of `parent_cg`. Errors: ENOENT parent gone, EEXIST,
/// EINVAL bad name, EAGAIN depth/descendant limit, ENOSPC table full.
pub fn create(parent_cg: CgId, name: &str, uid: u32, gid: u32) -> Result<CgId, isize> {
    if !valid_name(name) { return Err(E_INVAL); }
    let mut t = TREE.lock();
    init_tree(&mut t);
    if !alive(parent_cg) { return Err(E_NOENT); }
    if (1..CG_MAX).any(|i| ALIVE[i].load(Acquire) == 1 && PARENT[i].load(Acquire) == parent_cg && t.names[i] == name) {
        return Err(E_EXIST);
    }
    let nd = depth(parent_cg) + 1;
    if nd > MAX_DEPTH { return Err(E_AGAIN); }
    // cgroup.max.depth / max.descendants of every ancestor (and the parent).
    let mut a = parent_cg;
    loop {
        if nd - depth(a) > MAX_DEPTH_LIM[a as usize].load(Acquire).min(255) as u8 { return Err(E_AGAIN); }
        if NR_DESC[a as usize].load(Acquire) >= MAX_DESC_LIM[a as usize].load(Acquire) { return Err(E_AGAIN); }
        if a == 0 { break; }
        a = parent(a);
    }
    for i in 1..CG_MAX {
        if ALIVE[i].compare_exchange(0, 2, AcqRel, Relaxed).is_ok() {
            PARENT[i].store(parent_cg, Release);
            DEPTH[i].store(nd, Release);
            NR_TASKS[i].store(0, Release);
            DIRECT[i].store(0, Release);
            NR_CHILDREN[i].store(0, Release);
            NR_DESC[i].store(0, Release);
            NS_PINS[i].store(0, Release);
            SUBTREE[i].store(0, Release);
            PIDS_MAX[i].store(UNLIMITED, Release);
            PIDS_PEAK[i].store(0, Release);
            PIDS_EV_MAX[i].store(0, Release);
            MAX_DEPTH_LIM[i].store(UNLIMITED, Release);
            MAX_DESC_LIM[i].store(UNLIMITED, Release);
            DEAD_USER_NS[i].store(0, Release);
            DEAD_SYS_NS[i].store(0, Release);
            GEN[i].fetch_add(1, AcqRel);
            t.names[i] = String::from(name);
            t.owner[i] = (uid, gid);
            NR_CHILDREN[parent_cg as usize].fetch_add(1, AcqRel);
            let mut a = parent_cg;
            loop {
                NR_DESC[a as usize].fetch_add(1, AcqRel);
                if a == 0 { break; }
                a = parent(a);
            }
            ALIVE[i].store(1, Release);
            return Ok(i as CgId);
        }
    }
    Err(E_NOSPC)
}

/// rmdir. EBUSY while it has tasks (zombies included), children, or a
/// cgroup namespace rooted at it; EINVAL for the root; ENODEV when gone.
/// Its accumulated CPU time moves to the parent.
pub fn remove(cg: CgId) -> isize {
    if cg == 0 { return E_INVAL; }
    let mut t = TREE.lock();
    init_tree(&mut t);
    if !alive(cg) { return E_NODEV; }
    // RUN_QUEUE serialises against migrate/enqueue, which only ever grow
    // NR_TASKS of a node that is ALIVE under that lock.
    let rq = crate::RUN_QUEUE.lock();
    if NR_TASKS[cg as usize].load(Acquire) != 0 || NR_CHILDREN[cg as usize].load(Acquire) != 0
        || NS_PINS[cg as usize].load(Acquire) != 0 {
        return E_BUSY;
    }
    ALIVE[cg as usize].store(2, Release);
    drop(rq);
    let p = parent(cg);
    DEAD_USER_NS[p as usize].fetch_add(DEAD_USER_NS[cg as usize].load(Acquire), AcqRel);
    DEAD_SYS_NS[p as usize].fetch_add(DEAD_SYS_NS[cg as usize].load(Acquire), AcqRel);
    NR_CHILDREN[p as usize].fetch_sub(1, AcqRel);
    let mut a = p;
    loop {
        NR_DESC[a as usize].fetch_sub(1, AcqRel);
        if a == 0 { break; }
        a = parent(a);
    }
    t.names[cg as usize].clear();
    ALIVE[cg as usize].store(0, Release);
    crate::bpfcg::cg_released(cg);
    0
}

/// Write to `cgroup.subtree_control`: enable `add`, disable `del` (controller
/// bit sets). ENOENT: an `add` bit is not in `cgroup.controllers`; EBUSY:
/// the no-internal-process rule (add on a non-root node with tasks) or a
/// child still uses a bit being removed.
pub fn set_subtree_control(cg: CgId, add: u8, del: u8) -> isize {
    if add & !C_ALL != 0 || del & !C_ALL != 0 || add & del != 0 { return E_INVAL; }
    let mut t = TREE.lock();
    init_tree(&mut t);
    if !alive(cg) { return E_NODEV; }
    if add & !controllers(cg) != 0 { return E_NOENT; }
    let rq = crate::RUN_QUEUE.lock();
    let cur = SUBTREE[cg as usize].load(Acquire);
    let new = (cur | add) & !del;
    if new == cur { return 0; }
    if add & !cur != 0 && cg != 0 && DIRECT[cg as usize].load(Acquire) != 0 { return E_BUSY; }
    if del & cur != 0 {
        for i in 1..CG_MAX {
            if ALIVE[i].load(Acquire) == 1 && PARENT[i].load(Acquire) == cg
                && SUBTREE[i].load(Acquire) & del != 0 { return E_BUSY; }
        }
    }
    SUBTREE[cg as usize].store(new, Release);
    drop(rq);
    0
}

/// Path of `cg` relative to a reader whose cgroup namespace root is `root`:
/// `/a/b`, `/` for the root itself, `/../x` when `cg` lies outside `root`.
pub fn path_rel(cg: CgId, root: CgId) -> String {
    let mut t = TREE.lock();
    init_tree(&mut t);
    let common = common_ancestor(cg, root);
    let mut out = String::new();
    let mut r = root;
    while r != common { out.push_str("/.."); r = parent(r); }
    let mut chain: Vec<CgId> = Vec::new();
    let mut c = cg;
    while c != common { chain.push(c); c = parent(c); }
    for &c in chain.iter().rev() { out.push('/'); out.push_str(&t.names[c as usize]); }
    if out.is_empty() { out.push('/'); }
    out
}

/// Resolve an absolute-from-`root` path (`a/b`, leading `/` optional; no `..`).
pub fn resolve(root: CgId, path: &str) -> Option<CgId> {
    let mut cg = root;
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        cg = lookup_child(cg, comp)?;
    }
    Some(cg)
}

// ── task membership ──────────────────────────────────────────────────────────

/// Charge one thread to `cg` and its ancestors (RunQueue::enqueue; atomics only).
pub fn charge(cg: CgId) {
    DIRECT[cg as usize].fetch_add(1, AcqRel);
    let mut a = cg;
    loop {
        let n = NR_TASKS[a as usize].fetch_add(1, AcqRel) + 1;
        PIDS_PEAK[a as usize].fetch_max(n, AcqRel);
        if a == 0 { break; }
        a = parent(a);
    }
}

/// Undo `charge` and fold the thread's CPU time into the node (RunQueue::remove).
pub fn uncharge(cg: CgId, user_ns: u64, sys_ns: u64) {
    DEAD_USER_NS[cg as usize].fetch_add(user_ns, AcqRel);
    DEAD_SYS_NS[cg as usize].fetch_add(sys_ns, AcqRel);
    DIRECT[cg as usize].fetch_sub(1, AcqRel);
    let mut a = cg;
    loop {
        NR_TASKS[a as usize].fetch_sub(1, AcqRel);
        if a == 0 { break; }
        a = parent(a);
    }
}

/// Fork admission for a new thread in `cg`: 0, or EAGAIN when some ancestor's
/// `pids.max` would be exceeded (that ancestor's `pids.events max` is bumped).
/// The real charge follows in `enqueue`; two forks racing between check and
/// enqueue can overshoot by one, as RUN_QUEUE is held only for the check.
pub fn fork_check(cg: CgId) -> isize {
    let mut a = cg;
    loop {
        let max = PIDS_MAX[a as usize].load(Acquire);
        if max != UNLIMITED && NR_TASKS[a as usize].load(Acquire) >= max {
            PIDS_EV_MAX[a as usize].fetch_add(1, AcqRel);
            return E_AGAIN;
        }
        if a == 0 { return 0; }
        a = parent(a);
    }
}

/// Resolve the cgroup a freshly built child will be charged to. A child whose
/// inherited node was removed meanwhile falls back to the root.
pub fn adopt(cg: CgId) -> CgId { if alive(cg) { cg } else { 0 } }

/// Cgroup of thread `pid` (0 when unknown).
pub fn cg_of(pid: u32) -> CgId {
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.cgroup).unwrap_or(0)
}
/// Cgroup of the calling thread.
pub fn current() -> CgId { cg_of(crate::current_pid()) }

/// Migrate every thread of `tgid` to `dst` (write to `cgroup.procs`).
/// `scope` is the writer's cgroup-namespace root: source and destination
/// must both lie inside it (ENOENT). Errors: ESRCH no such process, ENODEV
/// destination gone, EBUSY destination has subtree_control set (no internal
/// processes; the root is exempt), EAGAIN a pids.max on the way down. The
/// caller (cgroupfs) does the DAC checks, using `common_ancestor`.
pub fn migrate_tgid(tgid: u32, dst: CgId, scope: CgId) -> isize {
    let mut rq = crate::RUN_QUEUE.lock();
    let mut src: Option<CgId> = None;
    let mut n = 0u32;
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid { src.get_or_insert(t.cgroup); n += 1; }
        }
    }
    let Some(src) = src else { return E_SRCH };
    if !alive(dst) { return E_NODEV; }
    if !in_subtree(src, scope) || !in_subtree(dst, scope) { return E_NOENT; }
    if dst != 0 && SUBTREE[dst as usize].load(Acquire) != 0 { return E_BUSY; }
    if src == dst { return 0; }
    let common = common_ancestor(src, dst);
    let mut a = dst;
    while a != common {
        let max = PIDS_MAX[a as usize].load(Acquire);
        if max != UNLIMITED && NR_TASKS[a as usize].load(Acquire).saturating_add(n) > max { return E_AGAIN; }
        a = parent(a);
    }
    DIRECT[src as usize].fetch_sub(n, AcqRel);
    DIRECT[dst as usize].fetch_add(n, AcqRel);
    let mut a = src;
    while a != common { NR_TASKS[a as usize].fetch_sub(n, AcqRel); a = parent(a); }
    let mut a = dst;
    while a != common {
        let v = NR_TASKS[a as usize].fetch_add(n, AcqRel) + n;
        PIDS_PEAK[a as usize].fetch_max(v, AcqRel);
        a = parent(a);
    }
    for i in rq.live_slots() {
        if let Some(t) = rq.get_mut(i) { if t.tgid == tgid { t.cgroup = dst; } }
    }
    0
}

/// Thread-group ids that have a thread in the subtree of `cg` (cgroup.procs,
/// cgroup.kill). One RUN_QUEUE hold; ascending, deduplicated.
pub fn subtree_tgids(cg: CgId) -> Vec<u32> {
    let rq = crate::RUN_QUEUE.lock();
    let mut v: Vec<u32> = Vec::new();
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) { if in_subtree(t.cgroup, cg) { v.push(t.tgid); } }
    }
    v.sort_unstable();
    v.dedup();
    v
}
/// Tgids directly in `cg` (the content of its `cgroup.procs`).
pub fn direct_tgids(cg: CgId) -> Vec<u32> {
    let rq = crate::RUN_QUEUE.lock();
    let mut v: Vec<u32> = Vec::new();
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) { if t.cgroup == cg { v.push(t.tgid); } }
    }
    v.sort_unstable();
    v.dedup();
    v
}

// ── pids / cpu accessors for cgroupfs ────────────────────────────────────────

pub fn pids_max(cg: CgId) -> u32 { PIDS_MAX[cg as usize % CG_MAX].load(Acquire) }
pub fn set_pids_max(cg: CgId, v: u32) { PIDS_MAX[cg as usize % CG_MAX].store(v, Release) }
pub fn pids_peak(cg: CgId) -> u32 { PIDS_PEAK[cg as usize % CG_MAX].load(Acquire) }
pub fn pids_events_max(cg: CgId) -> u32 { PIDS_EV_MAX[cg as usize % CG_MAX].load(Acquire) }
pub fn max_depth(cg: CgId) -> u32 { MAX_DEPTH_LIM[cg as usize % CG_MAX].load(Acquire) }
pub fn set_max_depth(cg: CgId, v: u32) { MAX_DEPTH_LIM[cg as usize % CG_MAX].store(v, Release) }
pub fn max_descendants(cg: CgId) -> u32 { MAX_DESC_LIM[cg as usize % CG_MAX].load(Acquire) }
pub fn set_max_descendants(cg: CgId, v: u32) { MAX_DESC_LIM[cg as usize % CG_MAX].store(v, Release) }

/// (user_ns, sys_ns) of exited tasks and removed children in the subtree of
/// `cg`. Add the live members' times for `cpu.stat`.
pub fn dead_cpu_ns(cg: CgId) -> (u64, u64) {
    let (mut u, mut s) = (0u64, 0u64);
    for i in 0..CG_MAX {
        if alive(i as CgId) && in_subtree(i as CgId, cg) {
            u += DEAD_USER_NS[i].load(Acquire);
            s += DEAD_SYS_NS[i].load(Acquire);
        }
    }
    (u, s)
}

// ── cgroup namespace pinning (used by ns.rs) ─────────────────────────────────

/// A cgroup namespace rooted at `cg` is created: pin the node against rmdir.
pub fn ns_pin(cg: CgId) { if cg != 0 { NS_PINS[cg as usize].fetch_add(1, AcqRel); } }
/// The namespace died.
pub fn ns_unpin(cg: CgId) { if cg != 0 { NS_PINS[cg as usize].fetch_sub(1, AcqRel); } }
