//! bpf(2) program objects and per-cgroup attachments for the cgroup device
//! controller (design-bpfdev, lane bpfsys). The verifier and interpreter live
//! in `lib/ebpf-cgdev`; this module owns the object model.
//!
//! One leaf lock guards everything. It never calls into the VFS; it reads
//! cgroup topology only through the lock-free `cgroup::{alive,parent,generation}`.
//! A program's `refs` counts open fds plus attachments and the program is
//! freed when it reaches zero.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering::*};
use spin::Mutex;

use crate::cgroup::{self, CgId};
pub use ebpf_cgdev::{DevCtx, Program};

pub const BPF_F_ALLOW_OVERRIDE: u32 = 1;
pub const BPF_F_ALLOW_MULTI: u32 = 2;
pub const BPF_F_REPLACE: u32 = 4;
/// Linux BPF_MAX_PROGS-style cap per cgroup.
pub const MAX_PROGS_PER_CG: usize = 64;

const EPERM: i32 = -1;
const ENOENT: i32 = -2;
const ENOMEM: i32 = -12;
const EEXIST: i32 = -17;
const EINVAL: i32 = -22;
const E2BIG: i32 = -7;

struct Prog {
    prog: Program,
    refs: u32,
}

struct CgAttach {
    gen: u32,
    flags: u32,
    progs: Vec<u32>,
}

struct State {
    progs: BTreeMap<u32, Prog>,
    next_id: u32,
    cg: BTreeMap<CgId, CgAttach>,
}

static STATE: Mutex<State> = Mutex::new(State { progs: BTreeMap::new(), next_id: 1, cg: BTreeMap::new() });
/// Total attachments; zero-cost fast path for the (future) enforcement hooks.
pub static ATTACHED: AtomicU32 = AtomicU32::new(0);

fn drop_ref(st: &mut State, id: u32) {
    let dead = match st.progs.get_mut(&id) {
        Some(p) => { p.refs = p.refs.saturating_sub(1); p.refs == 0 }
        None => false,
    };
    if dead { st.progs.remove(&id); }
}

/// Entry for `cg` if present and not stale.
fn live(st: &State, cg: CgId) -> Option<&CgAttach> {
    st.cg.get(&cg).filter(|a| a.gen == cgroup::generation(cg) && !a.progs.is_empty())
}

/// Register a verified program with one reference (the caller's fd).
pub fn prog_new(prog: Program) -> Result<u32, i32> {
    let mut st = STATE.lock();
    let mut id = st.next_id;
    let mut tries = 0u32;
    while st.progs.contains_key(&id) || id == 0 {
        id = id.wrapping_add(1);
        tries += 1;
        if tries > 1 << 20 { return Err(ENOMEM); }
    }
    st.next_id = id.wrapping_add(1);
    st.progs.insert(id, Prog { prog, refs: 1 });
    Ok(id)
}

/// One more reference on `id` (fd dup / fork). False if gone.
pub fn prog_get(id: u32) -> bool {
    match STATE.lock().progs.get_mut(&id) {
        Some(p) => { p.refs += 1; true }
        None => false,
    }
}

/// Drop one reference (fd close).
pub fn prog_put(id: u32) { let mut st = STATE.lock(); drop_ref(&mut st, id); }

pub fn prog_exists(id: u32) -> bool { STATE.lock().progs.contains_key(&id) }

/// PROG_ATTACH. `prog` and `replace` are program ids resolved from fds.
pub fn attach(cg: CgId, prog: u32, flags: u32, replace: Option<u32>) -> Result<(), i32> {
    if flags & !(BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI | BPF_F_REPLACE) != 0 { return Err(EINVAL); }
    if flags & BPF_F_ALLOW_OVERRIDE != 0 && flags & BPF_F_ALLOW_MULTI != 0 { return Err(EINVAL); }
    if flags & BPF_F_REPLACE != 0 && (flags & BPF_F_ALLOW_MULTI == 0 || replace.is_none()) { return Err(EINVAL); }
    if flags & BPF_F_REPLACE == 0 && replace.is_some() { return Err(EINVAL); }
    let saved = flags & (BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI);
    let multi = flags & BPF_F_ALLOW_MULTI != 0;
    let mut st = STATE.lock();
    if !cgroup::alive(cg) { return Err(-19); } // ENODEV
    if !st.progs.contains_key(&prog) { return Err(EINVAL); }
    let gen = cgroup::generation(cg);
    // Same flags on a populated cgroup, or EPERM.
    if let Some(a) = live(&st, cg) {
        if a.flags != saved { return Err(EPERM); }
    } else {
        // Nearest ancestor with programs must permit children (OVERRIDE/MULTI).
        let mut p = cg;
        let mut hops = 0;
        while p != 0 && hops < 64 {
            p = cgroup::parent(p);
            hops += 1;
            if let Some(a) = live(&st, p) {
                if a.flags == 0 { return Err(EPERM); }
                break;
            }
        }
    }
    let have = live(&st, cg).map(|a| a.progs.clone()).unwrap_or_default();
    let mut new = have.clone();
    let mut old_to_drop: Option<u32> = None;
    if multi {
        if let Some(r) = replace {
            let pos = new.iter().position(|&x| x == r).ok_or(ENOENT)?;
            if r != prog && new.contains(&prog) { return Err(EEXIST); }
            new[pos] = prog;
            old_to_drop = Some(r);
            if r == prog { old_to_drop = None; }
        } else {
            if new.contains(&prog) { return Err(EEXIST); }
            if new.len() >= MAX_PROGS_PER_CG { return Err(E2BIG); }
            new.push(prog);
        }
    } else {
        if let Some(&o) = new.first() { old_to_drop = Some(o); }
        new = alloc::vec![prog];
    }
    let grew = new.len() as i64 - have.len() as i64;
    // Commit. A same-prog replace keeps its single attachment reference.
    let mut taking = true;
    if old_to_drop.is_none() && multi && replace.is_some() { taking = false; }
    if taking { st.progs.get_mut(&prog).unwrap().refs += 1; }
    st.cg.insert(cg, CgAttach { gen, flags: saved, progs: new });
    if let Some(o) = old_to_drop { drop_ref(&mut st, o); }
    if grew > 0 { ATTACHED.fetch_add(grew as u32, AcqRel); }
    else if grew < 0 { ATTACHED.fetch_sub((-grew) as u32, AcqRel); }
    Ok(())
}

/// PROG_DETACH. Multi cgroups need `prog`; others may omit it.
pub fn detach(cg: CgId, prog: Option<u32>) -> Result<(), i32> {
    let mut st = STATE.lock();
    if !cgroup::alive(cg) { return Err(-19); }
    let (flags, list) = match live(&st, cg) { Some(a) => (a.flags, a.progs.clone()), None => return Err(ENOENT) };
    let victim = if flags & BPF_F_ALLOW_MULTI != 0 {
        let p = prog.ok_or(EINVAL)?;
        if !list.contains(&p) { return Err(ENOENT); }
        p
    } else {
        if let Some(p) = prog { if !list.contains(&p) { return Err(ENOENT); } }
        list[0]
    };
    let rest: Vec<u32> = list.into_iter().filter(|&x| x != victim).collect();
    if rest.is_empty() { st.cg.remove(&cg); } else { st.cg.get_mut(&cg).unwrap().progs = rest; }
    drop_ref(&mut st, victim);
    ATTACHED.fetch_sub(1, AcqRel);
    Ok(())
}

/// Effective program ids for `cg` (Linux `compute_effective_progs`): walk to
/// the root, taking a level's programs when nothing is collected yet or that
/// level has MULTI.
fn effective(st: &State, cg: CgId) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let mut p = cg;
    for _ in 0..32 {
        if let Some(a) = live(st, p) {
            if out.is_empty() || a.flags & BPF_F_ALLOW_MULTI != 0 {
                let mut v = a.progs.clone();
                v.extend(out.iter().copied());
                out = v;
            }
        }
        if p == 0 { break; }
        p = cgroup::parent(p);
    }
    out
}

/// PROG_QUERY: `(attach_flags, ids)`; the caller copies at most the user's
/// count and answers ENOSPC itself when `ids.len()` exceeds it.
pub fn query(cg: CgId, eff: bool) -> Result<(u32, Vec<u32>), i32> {
    let st = STATE.lock();
    if !cgroup::alive(cg) { return Err(-19); }
    if eff { return Ok((0, effective(&st, cg))); }
    Ok(match live(&st, cg) { Some(a) => (a.flags, a.progs.clone()), None => (0, Vec::new()) })
}

/// PROG_GET_FD_BY_ID: take a reference on a live program.
pub fn get_by_id(id: u32) -> Result<(), i32> {
    if prog_get(id) { Ok(()) } else { Err(ENOENT) }
}

/// cgroup rmdir: drop the attachments (Linux `cgroup_bpf_release`).
pub fn cg_released(cg: CgId) {
    let mut st = STATE.lock();
    if let Some(a) = st.cg.remove(&cg) {
        let n = a.progs.len() as u32;
        for p in a.progs { drop_ref(&mut st, p); }
        if n != 0 { ATTACHED.fetch_sub(n, AcqRel); }
    }
}

/// Run every effective program of `cg` against a device access; true = allowed.
pub fn dev_allowed(cg: CgId, ctx: &DevCtx) -> bool {
    if ATTACHED.load(Acquire) == 0 { return true; }
    let st = STATE.lock();
    effective(&st, cg).iter().all(|id| st.progs.get(id).map(|p| ebpf_cgdev::run(&p.prog, ctx) == 1).unwrap_or(true))
}

/// Device-cgroup enforcement (Linux `devcgroup_check_permission`): every
/// program in the effective list of `pid`'s cgroup must allow the access.
/// `acc` is a mask of BPF_DEVCG_ACC_{MKNOD,READ,WRITE}. Zero cost (one atomic
/// load) while no program is attached anywhere. Err is -EPERM.
pub fn dev_check(pid: u32, is_blk: bool, major: u32, minor: u32, acc: u32) -> Result<(), i32> {
    if ATTACHED.load(Acquire) == 0 { return Ok(()); }
    let ty = if is_blk { ebpf_cgdev::BPF_DEVCG_DEV_BLOCK } else { ebpf_cgdev::BPF_DEVCG_DEV_CHAR };
    let ctx = DevCtx::new(ty, acc, major, minor);
    if dev_allowed(cgroup::cg_of(pid), &ctx) { Ok(()) } else { Err(EPERM) }
}
