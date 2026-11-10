//! cgroup v2 filesystem (design-cgroup lane CG2): a synthetic tree served at
//! EXACTLY `/sys/fs/cgroup`, and only after a `mount -t cgroup2`.
//!
//! The 2026-09-15 /sys shadowing regression blacked out the desktop, so the
//! intercept is deliberately narrow: [`is_path`] is false until [`set_mounted`]
//! ran, and then only for `/sys/fs/cgroup` and below. Everything else in /sys
//! keeps resolving exactly as before.
//!
//! The tree is a thin view of `sched::cgroup` (the hierarchy, membership and
//! limits live there). The view root of a reader is its cgroup namespace root.
//! Every DAC check uses the host fsuid (kuid), so an in-userns root gets no
//! override on host-root-owned directories. Files are owned by their cgroup's
//! directory owner; chown of a file is therefore a no-op that succeeds only
//! when it asks for the current owner.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use sched::cgroup::{self, CgId, C_ALL, C_CPU, C_IO, C_MEMORY, C_PIDS, CTRL_NAMES, UNLIMITED};
use alloc::collections::BTreeMap;
use spin::Mutex;

pub const ROOT: &[u8] = b"/sys/fs/cgroup";
/// Directory descriptors are `SysBlock { dev: DEV_BASE | cg, level: LVL }`.
pub const DEV_BASE: u16 = 0xE000;
pub const LVL: u8 = 0xE2;
/// Fake tmpfs paths of the ephemeral snapshot files (read / write).
pub const SNAP_R: &[u8] = b"/tmp/.cgr";
pub const SNAP_W: &[u8] = b"/tmp/.cgw/";
pub const MAGIC: u64 = 0x6367_7270;

static MOUNTED: AtomicBool = AtomicBool::new(false);

pub fn set_mounted() { MOUNTED.store(true, Ordering::Release); }
pub fn mounted() -> bool { MOUNTED.load(Ordering::Acquire) }

/// Is `path` inside the served tree (mounted, `/sys/fs/cgroup` or below)?
pub fn is_path(path: &[u8]) -> bool {
    if !mounted() || !path.starts_with(ROOT) { return false; }
    path.len() == ROOT.len() || path[ROOT.len()] == b'/'
}

pub fn is_dev(dev: u16) -> bool { dev & 0xFC00 == DEV_BASE }
pub fn dev_cg(dev: u16) -> CgId { dev & 0x03FF }

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum F {
    Procs, Threads, Controllers, SubtreeControl, MaxDepth, MaxDescendants, Stat,
    Type, Events, PidsCurrent, PidsMax, PidsPeak, PidsEvents,
    MemCurrent, MemMax, MemHigh, MemLow, MemMin, MemSwapMax, MemSwapCurrent, MemEvents, MemStat,
    MemOomGroup, CpuMax, CpuWeight, CpuStat, IoMax, IoWeight, IoStat, Freeze, Kill,
}
const ALL: [F; 31] = [F::Procs, F::Threads, F::Controllers, F::SubtreeControl, F::MaxDepth,
    F::MaxDescendants, F::Stat, F::Type, F::Events, F::PidsCurrent, F::PidsMax, F::PidsPeak,
    F::PidsEvents, F::MemCurrent, F::MemMax, F::MemHigh, F::MemLow, F::MemMin, F::MemSwapMax,
    F::MemSwapCurrent, F::MemEvents, F::MemStat, F::MemOomGroup, F::CpuMax, F::CpuWeight,
    F::CpuStat, F::IoMax, F::IoWeight, F::IoStat, F::Freeze, F::Kill];

/// Stored (not enforced) controller knobs, keyed by (cgroup, file index).
static KNOBS: Mutex<BTreeMap<(CgId, u8), String>> = Mutex::new(BTreeMap::new());
pub fn forget_knobs(cg: CgId) { KNOBS.lock().retain(|k, _| k.0 != cg); }

fn ctrl_of(f: F) -> u8 {
    match f {
        F::PidsCurrent | F::PidsMax | F::PidsPeak | F::PidsEvents => C_PIDS,
        F::CpuMax | F::CpuWeight | F::CpuStat => C_CPU,
        F::IoMax | F::IoWeight | F::IoStat => C_IO,
        F::MemCurrent | F::MemMax | F::MemHigh | F::MemLow | F::MemMin | F::MemSwapMax
        | F::MemSwapCurrent | F::MemEvents | F::MemStat | F::MemOomGroup => C_MEMORY,
        _ => 0,
    }
}
fn stored(f: F) -> bool {
    matches!(f, F::MemMax | F::MemHigh | F::MemLow | F::MemMin | F::MemSwapMax | F::MemOomGroup
        | F::CpuMax | F::CpuWeight | F::IoMax | F::IoWeight | F::Freeze)
}
fn knob_default(f: F) -> &'static str {
    match f {
        F::MemMax | F::MemHigh | F::MemSwapMax => "max\n",
        F::MemLow | F::MemMin | F::MemOomGroup | F::MemCurrent | F::MemSwapCurrent => "0\n",
        F::MemEvents => "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n",
        F::MemStat => "anon 0\nfile 0\n",
        F::CpuMax => "max 100000\n",
        F::CpuWeight => "100\n",
        F::Freeze => "0\n",
        F::CpuStat => "usage_usec 0\nuser_usec 0\nsystem_usec 0\n",
        F::IoWeight => "default 100\n",
        _ => "",
    }
}

fn fname(f: F) -> &'static str {
    match f {
        F::Procs => "cgroup.procs", F::Threads => "cgroup.threads",
        F::Controllers => "cgroup.controllers", F::SubtreeControl => "cgroup.subtree_control",
        F::MaxDepth => "cgroup.max.depth", F::MaxDescendants => "cgroup.max.descendants",
        F::Stat => "cgroup.stat", F::Type => "cgroup.type", F::Events => "cgroup.events",
        F::PidsCurrent => "pids.current", F::PidsMax => "pids.max", F::PidsPeak => "pids.peak",
        F::PidsEvents => "pids.events",
        F::MemCurrent => "memory.current", F::MemMax => "memory.max", F::MemHigh => "memory.high",
        F::MemLow => "memory.low", F::MemMin => "memory.min", F::MemSwapMax => "memory.swap.max",
        F::MemSwapCurrent => "memory.swap.current", F::MemEvents => "memory.events",
        F::MemStat => "memory.stat", F::MemOomGroup => "memory.oom.group",
        F::CpuMax => "cpu.max", F::CpuWeight => "cpu.weight", F::CpuStat => "cpu.stat",
        F::IoMax => "io.max", F::IoWeight => "io.weight", F::IoStat => "io.stat",
        F::Freeze => "cgroup.freeze", F::Kill => "cgroup.kill",
    }
}

fn has_file(cg: CgId, f: F) -> bool {
    match f {
        F::Type | F::Events | F::Freeze | F::Kill => cg != 0,
        _ if ctrl_of(f) != 0 => cg != 0 && cgroup::controllers(cg) & ctrl_of(f) != 0,
        _ => true,
    }
}

fn read_only(f: F) -> bool {
    matches!(f, F::Controllers | F::Stat | F::Events | F::PidsCurrent | F::PidsPeak | F::PidsEvents
        | F::MemCurrent | F::MemSwapCurrent | F::MemEvents | F::MemStat | F::CpuStat | F::IoStat)
}

fn file_by_name(cg: CgId, name: &[u8]) -> Option<F> {
    ALL.iter().copied().find(|&f| fname(f).as_bytes() == name && has_file(cg, f))
}

pub enum Node { Dir(CgId), File(CgId, F) }

fn view_root(pid: u32) -> CgId { sched::ns::cgns_root(sched::ns::cgns_of(pid)) }

/// Resolve `path` (which `is_path`) for `pid`.
pub fn lookup(pid: u32, path: &[u8]) -> Option<Node> {
    let root = view_root(pid);
    let rest = &path[ROOT.len().min(path.len())..];
    let mut cg = root;
    let mut file: Option<F> = None;
    for comp in rest.split(|&b| b == b'/').filter(|c| !c.is_empty() && *c != b".") {
        if file.is_some() { return None; }
        if comp == b".." { if cg != root { cg = cgroup::parent(cg); } continue; }
        if let Some(c) = core::str::from_utf8(comp).ok().and_then(|n| cgroup::lookup_child(cg, n)) {
            cg = c;
        } else if let Some(f) = file_by_name(cg, comp) {
            file = Some(f);
        } else {
            return None;
        }
    }
    Some(match file { Some(f) => Node::File(cg, f), None => Node::Dir(cg) })
}

fn may_write(pid: u32, owner: u32) -> bool { sched::host_root_of(pid) || sched::fsuid_of(pid) == owner }

fn limit_text(v: u32, out: &mut String) {
    if v == UNLIMITED { out.push_str("max\n"); } else { push_u(out, v as u64); out.push('\n'); }
}
fn push_u(out: &mut String, v: u64) {
    use core::fmt::Write;
    let _ = write!(out, "{}", v);
}
fn ctrl_text(mask: u8) -> String {
    let mut s = String::new();
    for (i, n) in CTRL_NAMES.iter().enumerate() {
        if mask & (1 << i) != 0 { if !s.is_empty() { s.push(' '); } s.push_str(n); }
    }
    s.push('\n');
    s
}

/// Current text of file `f` of `cg`.
pub fn read(cg: CgId, f: F) -> Vec<u8> {
    let mut s = String::new();
    match f {
        F::Procs | F::Threads => {
            for t in cgroup::direct_tgids(cg) { push_u(&mut s, t as u64); s.push('\n'); }
        }
        F::Controllers => s = ctrl_text(cgroup::controllers(cg)),
        F::SubtreeControl => s = ctrl_text(cgroup::subtree_control(cg)),
        F::MaxDepth => limit_text(cgroup::max_depth(cg), &mut s),
        F::MaxDescendants => limit_text(cgroup::max_descendants(cg), &mut s),
        F::Stat => {
            s.push_str("nr_descendants "); push_u(&mut s, cgroup::nr_descendants(cg) as u64);
            s.push_str("\nnr_dying_descendants 0\n");
        }
        F::Type => s.push_str("domain\n"),
        F::Events => {
            s.push_str(if cgroup::populated(cg) { "populated 1\n" } else { "populated 0\n" });
            // cgroup.freeze is a recorded request, not scheduler enforcement:
            // cgroup.events reports it as `frozen 1` (see freeze_requested).
            s.push_str(if freeze_requested(cg) { "frozen 1\n" } else { "frozen 0\n" });
        }
        F::PidsCurrent => { push_u(&mut s, cgroup::subtree_tasks(cg) as u64); s.push('\n'); }
        F::PidsMax => limit_text(cgroup::pids_max(cg), &mut s),
        F::PidsPeak => { push_u(&mut s, cgroup::pids_peak(cg) as u64); s.push('\n'); }
        F::PidsEvents => { s.push_str("max "); push_u(&mut s, cgroup::pids_events_max(cg) as u64); s.push('\n'); }
        _ => match KNOBS.lock().get(&(cg, f as u8)) {
            Some(v) => s = v.clone(),
            None => s.push_str(knob_default(f)),
        },
    }
    s.into_bytes()
}

fn freeze_requested(cg: CgId) -> bool {
    KNOBS.lock().get(&(cg, F::Freeze as u8)).map(|v| v.starts_with('1')).unwrap_or(false)
}

fn parse_limit(d: &[u8]) -> Result<u32, isize> {
    let t = core::str::from_utf8(d).map_err(|_| -22isize)?.trim();
    if t == "max" { return Ok(UNLIMITED); }
    match t.parse::<u32>() { Ok(v) if v != UNLIMITED => Ok(v), _ => Err(-22) }
}

/// A write of `data` to file `f` of `cg` by `pid`; `Ok(())` or -errno.
pub fn write(pid: u32, cg: CgId, f: F, data: &[u8]) -> isize {
    if !cgroup::alive(cg) { return -19; } // ENODEV
    if read_only(f) { return -13; }
    let fsuid = sched::fsuid_of(pid);
    if !may_write(pid, cgroup::dir_owner(cg).0) { return -13; }
    match f {
        F::Procs | F::Threads => {
            let t = match core::str::from_utf8(data).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
                Some(v) => v, None => return -22,
            };
            let tgid = if t == 0 { sched::tgid_of(pid) } else { sched::tgid_of(t) };
            if sched::proc_stat_of(tgid).is_none() { return -3; } // ESRCH
            let src = cgroup::cg_of(tgid);
            // Moving needs write access on the destination and on the common
            // ancestor (Linux cgroup_procs_write_permission) and a writer
            // that owns the target process (or is host root).
            let anc = cgroup::common_ancestor(src, cg);
            if !may_write(pid, cgroup::dir_owner(anc).0) { return -13; }
            if !sched::host_root_of(pid) && fsuid != sched::euid_of(tgid) && fsuid != sched::ruid_of(tgid) { return -13; }
            cgroup::migrate_tgid(tgid, cg, view_root(pid))
        }
        F::SubtreeControl => {
            let s = match core::str::from_utf8(data) { Ok(s) => s, Err(_) => return -22 };
            let (mut add, mut del) = (0u8, 0u8);
            for tok in s.split_whitespace() {
                let (neg, name) = match tok.as_bytes()[0] {
                    b'+' => (false, &tok[1..]), b'-' => (true, &tok[1..]), _ => return -22,
                };
                let Some(i) = CTRL_NAMES.iter().position(|n| *n == name) else { return -2 };
                if neg { del |= 1 << i } else { add |= 1 << i }
            }
            if add & del != 0 { return -22; }
            let _ = C_ALL;
            cgroup::set_subtree_control(cg, add, del)
        }
        F::MaxDepth => match parse_limit(data) { Ok(v) => { cgroup::set_max_depth(cg, v); 0 } Err(e) => e },
        F::MaxDescendants => match parse_limit(data) { Ok(v) => { cgroup::set_max_descendants(cg, v); 0 } Err(e) => e },
        F::PidsMax => match parse_limit(data) { Ok(v) => { cgroup::set_pids_max(cg, v); 0 } Err(e) => e },
        _ if stored(f) => {
            let Ok(t) = core::str::from_utf8(data) else { return -22 };
            let t = t.trim();
            if t.is_empty() && f != F::IoMax { return -22; }
            let ok = match f {
                F::CpuWeight => t.parse::<u32>().map(|v| (1..=10000).contains(&v)).unwrap_or(false),
                F::MemOomGroup | F::Freeze => t == "0" || t == "1",
                F::MemMax | F::MemHigh | F::MemLow | F::MemMin | F::MemSwapMax =>
                    t == "max" || t.parse::<u64>().is_ok(),
                _ => true,
            };
            if !ok { return -22; }
            let mut v = String::from(t); v.push('\n');
            KNOBS.lock().insert((cg, f as u8), v);
            0
        }
        F::Kill => {
            // Write-only; only "1" is valid. SIGKILL every process of the subtree.
            if core::str::from_utf8(data).map(|s| s.trim() != "1").unwrap_or(true) { return -22; }
            for t in cgroup::subtree_tgids(cg) {
                let _ = sched::deliver_signal_process(t, 9, sched::task::SigInfo::KERNEL);
            }
            0
        }
        F::Type => if core::str::from_utf8(data).map(|s| s.trim() == "domain").unwrap_or(false) { 0 } else { -22 },
        _ => -13,
    }
}

/// Stat answer: (mode, nlink, ino, uid, gid).
pub fn stat(pid: u32, path: &[u8]) -> Option<(u32, u32, u64, u32, u32)> {
    match lookup(pid, path)? {
        Node::Dir(cg) => {
            let (u, g) = cgroup::dir_owner(cg);
            Some((0o040755, 2 + cgroup::nr_children(cg), 0x6000_0000 + (cg as u64) * 64, u, g))
        }
        Node::File(cg, f) => {
            let (u, g) = cgroup::dir_owner(cg);
            Some((if f == F::Kill { 0o100200 } else if read_only(f) { 0o100444 } else { 0o100644 }, 1,
                  0x6000_0000 + (cg as u64) * 64 + 1 + f as u64, u, g))
        }
    }
}

/// What an `open` resolves to.
pub enum Open { Dir(CgId), Read(Vec<u8>), Write(CgId, F, Vec<u8>), Err(i32) }

pub fn open(pid: u32, path: &[u8], flags: u32) -> Open {
    const O_ACC: u32 = 3;
    const O_CREAT: u32 = 0o100;
    const O_EXCL: u32 = 0o200;
    let wr = flags & O_ACC != 0;
    match lookup(pid, path) {
        None => Open::Err(if flags & O_CREAT != 0 { -13 } else { -2 }),
        Some(_) if flags & (O_CREAT | O_EXCL) == (O_CREAT | O_EXCL) => Open::Err(-17),
        Some(Node::Dir(cg)) => if wr { Open::Err(-21) } else { Open::Dir(cg) },
        Some(Node::File(cg, f)) => {
            if !wr {
                if f == F::Kill { return Open::Err(-22); }
                return Open::Read(read(cg, f));
            }
            if read_only(f) || !may_write(pid, cgroup::dir_owner(cg).0) {
                return Open::Err(-13);
            }
            Open::Write(cg, f, read(cg, f))
        }
    }
}

/// Fake tmpfs path of a writable snapshot: `SNAP_W` + `<cg>/<file index>`.
pub fn snap_w_path(cg: CgId, f: F) -> String {
    let mut s = String::from("/tmp/.cgw/");
    push_u(&mut s, cg as u64); s.push('/'); push_u(&mut s, f as u64);
    s
}
/// Parse a `SNAP_W` path back into (cg, file).
pub fn parse_snap_w(p: &[u8]) -> Option<(CgId, F)> {
    let r = core::str::from_utf8(p.strip_prefix(SNAP_W)?).ok()?;
    let (a, b) = r.split_once('/')?;
    let cg: CgId = a.parse().ok()?;
    let i: usize = b.parse().ok()?;
    Some((cg, *ALL.get(i)?))
}

fn split_parent(path: &[u8]) -> Option<(&[u8], &[u8])> {
    let i = path.iter().rposition(|&b| b == b'/')?;
    Some((&path[..i], &path[i + 1..]))
}

/// mkdir(path) by `pid`; 0 or -errno.
pub fn mkdir(pid: u32, path: &[u8]) -> isize {
    let Some((pp, name)) = split_parent(path) else { return -2 };
    if lookup(pid, path).is_some() || name.is_empty() { return -17; }
    let cg = match lookup(pid, pp) {
        Some(Node::Dir(c)) => c,
        Some(Node::File(..)) => return -20,
        None => return -2,
    };
    let fsuid = sched::fsuid_of(pid);
    if !may_write(pid, cgroup::dir_owner(cg).0) { return -13; }
    let Ok(n) = core::str::from_utf8(name) else { return -22 };
    // A name that collides with a control file of the parent is EEXIST.
    if file_by_name(cg, name).is_some() { return -17; }
    match cgroup::create(cg, n, fsuid, sched::fsgid_of(pid)) { Ok(c) => { forget_knobs(c); 0 } Err(e) => e }
}

/// rmdir(path) by `pid`; 0 or -errno.
pub fn rmdir(pid: u32, path: &[u8]) -> isize {
    match lookup(pid, path) {
        None => -2,
        Some(Node::File(..)) => -20,
        Some(Node::Dir(cg)) => {
            if cg == view_root(pid) { return -16; }
            let p = cgroup::parent(cg);
            if !may_write(pid, cgroup::dir_owner(p).0) { return -13; }
            let r = cgroup::remove(cg);
            if r == 0 { forget_knobs(cg); }
            r
        }
    }
}

/// chown(path, uid, gid) by `pid` (u32::MAX = unchanged); 0 or -errno.
pub fn chown(pid: u32, path: &[u8], uid: u32, gid: u32) -> isize {
    let cg = match lookup(pid, path) {
        None => return -2,
        Some(Node::File(c, _)) => {
            let (u, g) = cgroup::dir_owner(c);
            let same = (uid == u32::MAX || uid == u) && (gid == u32::MAX || gid == g);
            return if same || sched::host_root_of(pid) { 0 } else { -1 };
        }
        Some(Node::Dir(c)) => c,
    };
    if !sched::host_root_of(pid) { 
        let (u, g) = cgroup::dir_owner(cg);
        let same = (uid == u32::MAX || uid == u) && (gid == u32::MAX || gid == g);
        return if same { 0 } else { -1 };
    }
    let (u, g) = cgroup::dir_owner(cg);
    cgroup::set_dir_owner(cg, if uid == u32::MAX { u } else { uid }, if gid == u32::MAX { g } else { gid });
    0
}

/// Directory listing of `cg` (after `.` and `..`): files, then child dirs.
pub fn entries(cg: CgId) -> Vec<(String, bool)> {
    let mut v: Vec<(String, bool)> = Vec::new();
    for f in ALL { if has_file(cg, f) { v.push((String::from(fname(f)), false)); } }
    for c in cgroup::children(cg) { v.push((cgroup::name(c), true)); }
    v
}

/// `/proc/<target>/cgroup` as seen by `reader` (path relative to its cgns root).
pub fn proc_cgroup(target: u32, reader: u32) -> Vec<u8> {
    let mut s = String::from("0::");
    s.push_str(&cgroup::path_rel(cgroup::cg_of(target), view_root(reader)));
    s.push('\n');
    s.into_bytes()
}

/// `/proc/cgroups`: every controller on the (unified) hierarchy 0.
pub fn proc_cgroups() -> Vec<u8> {
    let mut s = String::from("#subsys_name\thierarchy\tnum_cgroups\tenabled\n");
    let n = 1 + cgroup::nr_descendants(0) as u64;
    for name in CTRL_NAMES.iter() {
        s.push_str(name); s.push_str("\t0\t"); push_u(&mut s, n); s.push_str("\t1\n");
    }
    s.into_bytes()
}
