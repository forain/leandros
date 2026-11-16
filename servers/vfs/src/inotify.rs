//! inotify core (design-inotify.md, lane L1).
//!
//! Watches live here, in the VFS; events are emitted from the VFS's own
//! routing points (see `inotify_hooks` in lib.rs) after an operation succeeds.
//! An instance is keyed by the id of the open file description it lives on
//! (`VnodeKind::Inotify { inst }`), so dup/fork/SCM_RIGHTS share it and the
//! last `ofd::put` frees it (`released`).
//!
//! Lock rule: `STATE` is a leaf lock. It is never held across a wake
//! (`sched::wake_poll_key`) and never takes another lock.
//!
//! L1 scope: tmpfs directory-entry events (CREATE, DELETE, MOVED_FROM/TO),
//! DELETE_SELF + IGNORED for a removed watched directory, IGNORED on rm_watch.
//! Watches on other filesystems are accepted and never fire.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU16, AtomicU8, AtomicUsize, Ordering::Relaxed};
use spin::Mutex;

pub const IN_ACCESS: u32 = 0x1;
pub const IN_MODIFY: u32 = 0x2;
pub const IN_ATTRIB: u32 = 0x4;
pub const IN_CLOSE_WRITE: u32 = 0x8;
pub const IN_CLOSE_NOWRITE: u32 = 0x10;
pub const IN_OPEN: u32 = 0x20;
pub const IN_MOVED_FROM: u32 = 0x40;
pub const IN_MOVED_TO: u32 = 0x80;
pub const IN_CREATE: u32 = 0x100;
pub const IN_DELETE: u32 = 0x200;
pub const IN_DELETE_SELF: u32 = 0x400;
pub const IN_MOVE_SELF: u32 = 0x800;
pub const IN_UNMOUNT: u32 = 0x2000;
pub const IN_Q_OVERFLOW: u32 = 0x4000;
pub const IN_IGNORED: u32 = 0x8000;
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
pub const IN_MASK_CREATE: u32 = 0x1000_0000;
pub const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
pub const IN_ONESHOT: u32 = 0x8000_0000;
pub const IN_ALL_EVENTS: u32 = 0xFFF;

/// /proc/sys/fs/inotify/{max_queued_events,max_user_instances,max_user_watches}.
static MAX_QUEUED: AtomicUsize = AtomicUsize::new(16384);
static MAX_USER_INST: AtomicUsize = AtomicUsize::new(128);
static MAX_USER_WATCH: AtomicUsize = AtomicUsize::new(65536);
/// Monotonic counters (read via /proc/sys/fs/inotify/{events_queued,events_coalesced,events_overflow,instances,watches}).
static EV_QUEUED: AtomicUsize = AtomicUsize::new(0);
static EV_COALESCED: AtomicUsize = AtomicUsize::new(0);
static EV_OVERFLOW: AtomicUsize = AtomicUsize::new(0);
/// Safety cap on live instances in the whole system.
const MAX_INSTANCES: usize = 8192;

/// 0 = max_queued_events, 1 = max_user_instances, 2 = max_user_watches.
pub fn limit_get(k: usize) -> usize {
    match k { 3 => EV_QUEUED.load(Relaxed), 4 => EV_COALESCED.load(Relaxed), 5 => EV_OVERFLOW.load(Relaxed),
        6 => STATE.lock().insts.len(), 7 => STATE.lock().nwatches, 0 => MAX_QUEUED.load(Relaxed), 1 => MAX_USER_INST.load(Relaxed), _ => MAX_USER_WATCH.load(Relaxed) }
}
pub fn limit_set(k: usize, v: usize) -> Result<(), i32> {
    if k > 2 || v == 0 || v > i32::MAX as usize { return Err(-22); }
    match k { 0 => MAX_QUEUED.store(v, Relaxed), 1 => MAX_USER_INST.store(v, Relaxed), _ => MAX_USER_WATCH.store(v, Relaxed) }
    Ok(())
}

/// ucounts key: (owning user namespace, kuid). A task is charged at its own
/// (userns, kuid) and, like Linux `inc_ucount`, at each ancestor level under
/// the kuid that created the child namespace.
pub type UKey = (u16, u32);
const BLOOM: usize = 4096;

/// Kill switch: 0 unknown, 1 off, 2 on (default on; `vfs.inotify=0` on the command line turns it off).
static KNOB: AtomicU8 = AtomicU8::new(0);
/// Live watch count. Every hook starts with one load of this.
static ARMED: AtomicU32 = AtomicU32::new(0);
/// Live watches that ask for any inode event (ACCESS/MODIFY/ATTRIB/CLOSE_*/OPEN),
/// and for ACCESS alone: the write/read/open hooks cost nothing below them.
static FILEW: AtomicU32 = AtomicU32::new(0);
static ACCESSW: AtomicU32 = AtomicU32::new(0);
const FILE_BITS: u32 = IN_ACCESS | IN_MODIFY | IN_ATTRIB | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE | IN_OPEN;
fn track(old: u32, new: u32) {
    let (o, n) = (old & FILE_BITS != 0, new & FILE_BITS != 0);
    if n && !o { FILEW.fetch_add(1, Relaxed); } else if o && !n { FILEW.fetch_sub(1, Relaxed); }
    let (o, n) = (old & IN_ACCESS != 0, new & IN_ACCESS != 0);
    if n && !o { ACCESSW.fetch_add(1, Relaxed); } else if o && !n { ACCESSW.fetch_sub(1, Relaxed); }
}
/// Identity for a mount-root path that has no inode record (a built-in tmpfs root).
pub fn path_id(p: &[u8]) -> Id { (0xFFFF_FFFF, fnv(p)) }
pub fn multi_watched() -> bool { MULTI.load(Relaxed) != 0 }
pub fn files_watched() -> bool { FILEW.load(Relaxed) != 0 }
pub fn access_watched() -> bool { ACCESSW.load(Relaxed) != 0 }
/// Watches whose inode has (or had) more than one name: their self events are
/// matched by (st_dev, st_ino), so every hook must resolve the identity.
static MULTI: AtomicU32 = AtomicU32::new(0);
/// fanotify mount marks (matched by path prefix).
static FANM: AtomicU32 = AtomicU32::new(0);
/// (st_dev, st_ino); (0, 0) = unknown.
pub type Id = (u64, u64);
pub const FAN_ONDIR: u32 = 0x4000_0000;
pub const FAN_ON_CHILD: u32 = 0x0800_0000;
/// Events a fanotify (notification class) group can ask for.
pub const FAN_EVENTS: u32 = IN_ACCESS | IN_MODIFY | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE | IN_OPEN;
pub const FAN_OPEN_PERM: u32 = 0x1_0000;
pub const FAN_ACCESS_PERM: u32 = 0x2_0000;
pub const FAN_OPEN_EXEC_PERM: u32 = 0x4_0000;
pub const FAN_PERM: u32 = FAN_OPEN_PERM | FAN_ACCESS_PERM | FAN_OPEN_EXEC_PERM;
/// Events a FAN_REPORT_FID / FAN_REPORT_DIR_FID group may add.
pub const FAN_FID_EVENTS: u32 = IN_ATTRIB | IN_MOVED_FROM | IN_MOVED_TO | IN_CREATE | IN_DELETE | IN_DELETE_SELF | IN_MOVE_SELF;
pub const FAN_OPEN_EXEC: u32 = 0x1000;
pub const FAN_RENAME: u32 = 0x1000_0000;
pub const FAN_REPORT_TARGET_FID: u32 = 0x1000;
/// Everything a fanotify mark may hold that keeps the mark alive.
pub const FAN_ALLMARK: u32 = FAN_EVENTS | FAN_PERM | FAN_OPEN_EXEC | FAN_RENAME | FAN_FID_EVENTS;
pub const FAN_REPORT_FID: u32 = 0x200;
pub const FAN_REPORT_DIR_FID: u32 = 0x400;
pub const FAN_REPORT_NAME: u32 = 0x800;
pub const FAN_MARK_IGNORED_MASK: u32 = 0x20;
pub const FAN_MARK_IGNORED_SURV_MODIFY: u32 = 0x40;
/// Permission-class fanotify groups alive (gates every permission check).
static PERMG: AtomicU32 = AtomicU32::new(0);
static PERM_ID: AtomicU32 = AtomicU32::new(1);
/// An unanswered permission event is allowed after this long.
const PERM_TIMEOUT_NS: u64 = 30_000_000_000;
static COOKIE: AtomicU32 = AtomicU32::new(1);
/// Counting bloom over the hash of each watch's directory path.
static DIR_BLOOM: [AtomicU16; BLOOM] = [const { AtomicU16::new(0) }; BLOOM];

struct Watch { wd: i32, mask: u32, ignore: u32, surv: bool, ino: Id, isdir: bool, multi: bool, mount: bool, path: Vec<u8>, hash: u64 }
/// What the indexes (and the MULTI/FANM counters) need to forget a watch.
#[derive(Clone, Copy)]
struct Wk { hash: u64, ino: Id, multi: bool, mount: bool }
impl Watch { fn k(&self) -> Wk { Wk { hash: self.hash, ino: self.ino, multi: self.multi, mount: self.mount } } }
struct Ev { ino: Id, named: bool, wd: i32, mask: u32, cookie: u32, name: Vec<u8> }
struct Pend { id: u32, fd: i32, verdict: u32, errno: i32 }
struct Instance { fid: u32, fan: bool, perm: bool, owner: u32, pend: Vec<Pend>, evflags: u32, next_wd: i32, queue: VecDeque<Ev>, bytes: usize, seq: u64, watches: Vec<Watch>, charge: Vec<UKey> }
struct State {
    insts: BTreeMap<u32, Instance>,
    by_path: BTreeMap<u64, Vec<(u32, i32)>>,
    by_ino: BTreeMap<Id, Vec<(u32, i32)>>,
    nwatches: usize,
    /// (instances, watches) charged per ucounts key.
    uc: BTreeMap<UKey, (usize, usize)>,
}
static STATE: Mutex<State> = Mutex::new(State { insts: BTreeMap::new(), by_path: BTreeMap::new(), by_ino: BTreeMap::new(), nwatches: 0, uc: BTreeMap::new() });

fn uc_adj(uc: &mut BTreeMap<UKey, (usize, usize)>, chain: &[UKey], inst: isize, watch: isize) {
    for k in chain {
        let e = uc.entry(*k).or_insert((0, 0));
        e.0 = (e.0 as isize + inst).max(0) as usize;
        e.1 = (e.1 as isize + watch).max(0) as usize;
        if *e == (0, 0) { uc.remove(k); }
    }
}

/// Resolve the knob from the command line on first use (cached once the
/// command line is known).
pub fn enabled(cmdline: Option<&str>) -> bool {
    match KNOB.load(Relaxed) {
        1 => false,
        2 => true,
        _ => match cmdline {
            Some(c) => {
                let on = c.split_ascii_whitespace().any(|t| t == "vfs.inotify=0");
                let on = !on;
                KNOB.store(if on { 2 } else { 1 }, Relaxed);
                on
            }
            None => false,
        },
    }
}

/// True iff at least one watch exists.
#[inline]
pub fn armed() -> bool { ARMED.load(Relaxed) != 0 }

pub fn new_cookie() -> u32 {
    loop { let c = COOKIE.fetch_add(1, Relaxed); if c != 0 { return c; } }
}

pub fn key(inst: u32) -> u64 { sched::poll_key(sched::poll_class::INOTIFY, inst) }

fn fnv(p: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in p { h ^= b as u64; h = h.wrapping_mul(0x100_0000_01b3); }
    h
}

fn bloom_add(h: u64) { DIR_BLOOM[(h as usize) % BLOOM].fetch_add(1, Relaxed); }
fn bloom_del(h: u64) { DIR_BLOOM[(h as usize) % BLOOM].fetch_sub(1, Relaxed); }

fn index_add(st: &mut State, inst: u32, wd: i32, k: Wk) {
    st.by_path.entry(k.hash).or_default().push((inst, wd));
    bloom_add(k.hash);
    if k.ino != (0, 0) { st.by_ino.entry(k.ino).or_default().push((inst, wd)); }
    if k.multi { MULTI.fetch_add(1, Relaxed); }
    if k.mount { FANM.fetch_add(1, Relaxed); }
}
fn index_del(st: &mut State, inst: u32, wd: i32, k: Wk) {
    if let Some(v) = st.by_path.get_mut(&k.hash) {
        if let Some(p) = v.iter().position(|&e| e == (inst, wd)) { v.swap_remove(p); bloom_del(k.hash); }
        if v.is_empty() { st.by_path.remove(&k.hash); }
    }
    if k.ino != (0, 0) {
        if let Some(v) = st.by_ino.get_mut(&k.ino) {
            if let Some(p) = v.iter().position(|&e| e == (inst, wd)) { v.swap_remove(p); }
            if v.is_empty() { st.by_ino.remove(&k.ino); }
        }
    }
    if k.multi { MULTI.fetch_sub(1, Relaxed); }
    if k.mount { FANM.fetch_sub(1, Relaxed); }
}

fn wake(keys: &[u64]) {
    for &k in keys { sched::wake_poll_key(k); }
}

/// Append `ev` to `i`'s queue (tail coalescing, overflow cap). True if the
/// queue changed.
fn push(i: &mut Instance, ev: Ev) -> bool {
    if i.fan && ev.mask & (FAN_EVENTS | FAN_FID_EVENTS | FAN_PERM | FAN_OPEN_EXEC | FAN_RENAME | IN_Q_OVERFLOW) == 0 { return false; }
    let unit = if i.fan { 24 } else { 16 };
    if let Some(t) = i.queue.back() {
        if t.wd == ev.wd && t.mask == ev.mask && t.cookie == ev.cookie && t.name == ev.name { EV_COALESCED.fetch_add(1, Relaxed); return false; }
    }
    if i.queue.len() >= MAX_QUEUED.load(Relaxed) {
        if i.queue.back().map_or(false, |t| t.mask == IN_Q_OVERFLOW) { return false; }
        EV_OVERFLOW.fetch_add(1, Relaxed);
        i.bytes += unit;
        i.queue.push_back(Ev { wd: -1, mask: IN_Q_OVERFLOW, cookie: 0, name: Vec::new(), ino: (0, 0), named: false });
        i.seq += 1;
        return true;
    }
    EV_QUEUED.fetch_add(1, Relaxed);
    i.bytes += if i.fan { 24 } else { ev_len(&ev) };
    i.queue.push_back(ev);
    i.seq += 1;
    true
}

fn name_len(name: &[u8]) -> usize { if name.is_empty() { 0 } else { (name.len() + 1 + 15) & !15 } }
fn ev_len(e: &Ev) -> usize { 16 + name_len(&e.name) }

// ── instances ────────────────────────────────────────────────────────────────

/// Create the instance for description `inst`. False at the instance limit.
pub fn create(inst: u32, chain: &[UKey]) -> bool {
    let mut st = STATE.lock();
    if st.insts.len() >= MAX_INSTANCES { return false; }
    let max = MAX_USER_INST.load(Relaxed);
    if chain.iter().any(|k| st.uc.get(k).map_or(0, |e| e.0) >= max) { return false; }
    uc_adj(&mut st.uc, chain, 1, 0);
    st.insts.insert(inst, Instance { fid: 0, fan: false, perm: false, owner: 0, pend: Vec::new(), evflags: 0, next_wd: 1, queue: VecDeque::new(), bytes: 0, seq: 0, watches: Vec::new(), charge: chain.to_vec() });
    true
}

/// The last reference to description `inst` is gone: drop its watches.
pub fn released(inst: u32) {
    if files_watched() || !NOTES.lock().is_empty() { note_closed(inst); }
    let mut st = STATE.lock();
    if let Some(i) = st.insts.remove(&inst) {
        if i.perm { PERMG.fetch_sub(1, Relaxed); sched::wake_poll_key(pkey(inst)); }
        let n = i.watches.len();
        for w in &i.watches { track(w.mask, 0); index_del(&mut st, inst, w.wd, w.k()); }
        st.nwatches -= n;
        ARMED.fetch_sub(n as u32, Relaxed);
        uc_adj(&mut st.uc, &i.charge, -1, -(n as isize));
    }
}

pub fn exists(inst: u32) -> bool { STATE.lock().insts.contains_key(&inst) }

/// Add (or update) a watch on `path`, whose inode is `id` (`nlink` names).
/// A second name of the same inode finds the existing watch (same wd).
pub fn add(inst: u32, path: &[u8], mask: u32, id: Id, nlink: u64, isdir: bool) -> Result<i32, i32> {
    let ev_mask = (mask & IN_ALL_EVENTS) | (mask & IN_ONESHOT);
    let mut st = STATE.lock();
    let st = &mut *st;
    let i = match st.insts.get_mut(&inst) { Some(i) => i, None => return Err(-22) };
    if let Some(w) = i.watches.iter_mut().find(|w| !w.mount && if id != (0, 0) && w.ino != (0, 0) { w.ino == id } else { w.path == path }) {
        if mask & IN_MASK_CREATE != 0 { return Err(-17); }
        let nm = if mask & IN_MASK_ADD != 0 { w.mask | ev_mask } else { ev_mask };
        track(w.mask, nm);
        w.mask = nm;
        return Ok(w.wd);
    }
    let maxw = MAX_USER_WATCH.load(Relaxed);
    if i.charge.iter().any(|k| st.uc.get(k).map_or(0, |e| e.1) >= maxw) { return Err(-28); }
    uc_adj(&mut st.uc, &i.charge, 0, 1);
    // Cyclic wd allocation that skips live descriptors.
    let mut wd = i.next_wd;
    while i.watches.iter().any(|w| w.wd == wd) { wd = if wd == i32::MAX { 1 } else { wd + 1 }; }
    i.next_wd = if wd == i32::MAX { 1 } else { wd + 1 };
    let h = fnv(path);
    let w = Watch { wd, mask: ev_mask, ignore: 0, surv: false, ino: id, isdir, multi: nlink > 1 && !isdir, mount: false, path: path.to_vec(), hash: h };
    let k = w.k();
    i.watches.push(w);
    st.nwatches += 1;
    ARMED.fetch_add(1, Relaxed);
    track(0, ev_mask);
    index_add(st, inst, wd, k);
    Ok(wd)
}

// ── fanotify marks (a notification-class group is an instance with `fan`) ────

pub const FAN_MARK_ADD: u32 = 1;
pub const FAN_MARK_REMOVE: u32 = 2;
pub const FAN_MARK_MOUNT: u32 = 0x10;
pub const FAN_MARK_FLUSH: u32 = 0x80;

/// Turn the fresh instance `inst` into a fanotify group.
pub fn fan_set(inst: u32, evflags: u32, perm: bool, owner: u32, fid: u32) {
    if let Some(i) = STATE.lock().insts.get_mut(&inst) { i.fid = fid; i.fan = true; i.evflags = evflags; i.perm = perm; i.owner = owner; if perm { PERMG.fetch_add(1, Relaxed); } }
}
pub fn is_perm_group(inst: u32) -> bool { STATE.lock().insts.get(&inst).map_or(false, |i| i.perm) }
pub fn perm_watched() -> bool { PERMG.load(Relaxed) != 0 }
pub fn is_fan(inst: u32) -> bool { STATE.lock().insts.get(&inst).map_or(false, |i| i.fan) }
pub fn fan_fid(inst: u32) -> u32 { STATE.lock().insts.get(&inst).map_or(0, |i| i.fid) }
pub fn fan_evflags(inst: u32) -> u32 { STATE.lock().insts.get(&inst).map_or(0, |i| i.evflags) }

fn wdrop(st: &mut State, inst: u32, p: usize) {
    let i = st.insts.get_mut(&inst).unwrap();
    let w = i.watches.remove(p);
    track(w.mask, 0);
    let ch = i.charge.clone();
    index_del(st, inst, w.wd, w.k());
    st.nwatches -= 1;
    ARMED.fetch_sub(1, Relaxed);
    uc_adj(&mut st.uc, &ch, 0, -1);
}

/// fanotify_mark ADD/REMOVE on an inode (`mount == false`) or on the mount
/// rooted at `path`. `mask` is the fanotify mask (events | ONDIR | ON_CHILD).
pub fn fan_mark(inst: u32, remove: bool, path: &[u8], mask: u32, id: Id, nlink: u64, isdir: bool, mount: bool, ignore: bool, surv: bool) -> Result<(), i32> {
    let mut st = STATE.lock();
    let st = &mut *st;
    let i = match st.insts.get_mut(&inst) { Some(i) if i.fan => i, _ => return Err(-22) };
    let found = i.watches.iter().position(|w| w.mount == mount && if mount { w.path == path } else { w.ino == id && id != (0, 0) });
    if remove {
        let p = found.ok_or(-2)?;
        let w = &mut i.watches[p];
        if ignore { w.ignore &= !mask; } else { let nm = w.mask & !mask; track(w.mask, nm); w.mask = nm; }
        if w.mask & FAN_ALLMARK == 0 && w.ignore == 0 { wdrop(st, inst, p); }
        return Ok(());
    }
    if let Some(p) = found {
        let w = &mut i.watches[p];
        if ignore {
            w.ignore |= mask & (FAN_EVENTS | FAN_PERM);
            w.mask |= mask & (FAN_ONDIR | FAN_ON_CHILD);
            w.surv = surv;
        } else {
            let nm = w.mask | mask;
            track(w.mask, nm);
            w.mask = nm;
        }
        return Ok(());
    }
    let maxw = MAX_USER_WATCH.load(Relaxed);
    if i.charge.iter().any(|k| st.uc.get(k).map_or(0, |e| e.1) >= maxw) { return Err(-28); }
    uc_adj(&mut st.uc, &i.charge, 0, 1);
    let mut wd = i.next_wd;
    while i.watches.iter().any(|w| w.wd == wd) { wd = if wd == i32::MAX { 1 } else { wd + 1 }; }
    i.next_wd = if wd == i32::MAX { 1 } else { wd + 1 };
    let w = Watch { wd, mask: if ignore { mask & (FAN_ONDIR | FAN_ON_CHILD) } else { mask }, ignore: if ignore { mask & (FAN_EVENTS | FAN_PERM) } else { 0 }, surv: ignore && surv, ino: if mount { (0, 0) } else { id }, isdir, multi: !mount && nlink > 1 && !isdir, mount, path: path.to_vec(), hash: fnv(path) };
    let k = w.k();
    i.watches.push(w);
    st.nwatches += 1;
    ARMED.fetch_add(1, Relaxed);
    track(0, if ignore { 0 } else { mask });
    index_add(st, inst, wd, k);
    Ok(())
}

/// FAN_MARK_FLUSH: drop every mark of the kind (mount or inode).
pub fn fan_flush(inst: u32, mount: bool) {
    let mut st = STATE.lock();
    let st = &mut *st;
    loop {
        let p = match st.insts.get(&inst) { Some(i) if i.fan => i.watches.iter().position(|w| w.mount == mount), _ => None };
        match p { Some(p) => wdrop(st, inst, p), None => break }
    }
}

/// Pop up to `max` events: (mask, path, pid). Err(-11) when empty.
pub fn fan_read(inst: u32, max: usize) -> Result<Vec<(u32, Vec<u8>, u32, u32, Id, bool)>, i32> {
    let mut st = STATE.lock();
    let i = match st.insts.get_mut(&inst) { Some(i) => i, None => return Err(-22) };
    if i.queue.is_empty() { return Err(-11); }
    let mut v = Vec::new();
    while v.len() < max {
        let Some(e) = i.queue.pop_front() else { break };
        i.bytes = i.bytes.saturating_sub(24);
        let permid = if e.mask & FAN_PERM != 0 { (-e.wd) as u32 } else { 0 };
        v.push((e.mask, e.name, e.cookie, permid, e.ino, e.named));
    }
    Ok(v)
}

/// Mount marks: an event on `path` reaches every mark whose mount root is a
/// prefix of it.
fn fan_mount(path: &[u8], mask: u32, named: bool) {
    let mut keys: Vec<u64> = Vec::new();
    {
        let mut st = STATE.lock();
        for (&inst, i) in st.insts.iter_mut() {
            if !i.fan { continue; }
            let mut hit = false;
            let pid = sched::tgid_of(sched::current_pid());
            let mut evs: Vec<Ev> = Vec::new();
            if i.watches.iter().any(|w| !w.mount && w.path == path && w.ignore & mask != 0) { continue; }
            for w in i.watches.iter() {
                if w.mount && w.ignore & mask != 0 && w.path == path { break; }
                if !w.mount || w.mask & mask & (FAN_EVENTS | FAN_FID_EVENTS) == 0 { continue; }
                if mask & IN_ISDIR != 0 && w.mask & FAN_ONDIR == 0 { continue; }
                let under = w.path == path || w.path == b"/" || (path.len() > w.path.len() && path.starts_with(&w.path) && path[w.path.len()] == b'/');
                if under { evs.push(Ev { wd: w.wd, mask: mask & (FAN_EVENTS | FAN_FID_EVENTS) | (mask & IN_ISDIR), cookie: pid as u32, name: path.to_vec(), ino: (0, 0), named }); break; }
            }
            for e in evs { hit |= push(i, e); }
            if hit { keys.push(key(inst)); }
        }
    }
    wake(&keys);
}

/// A link to `id` was made: its watches now match by identity, and the open
/// descriptions on `path` learn theirs.
pub fn promote(id: Id, path: &[u8]) {
    {
        let mut st = STATE.lock();
        let st = &mut *st;
        let list = st.by_ino.get(&id).cloned().unwrap_or_default();
        for (inst, wd) in list {
            if let Some(w) = st.insts.get_mut(&inst).and_then(|i| i.watches.iter_mut().find(|w| w.wd == wd)) {
                if !w.multi && !w.isdir { w.multi = true; MULTI.fetch_add(1, Relaxed); }
            }
        }
    }
    for n in NOTES.lock().values_mut() { if n.path == path && n.id == (0, 0) { n.id = id; } }
}

/// rm_watch: EINVAL for an unknown wd; queues IN_IGNORED.
pub fn rm(inst: u32, wd: i32) -> Result<(), i32> {
    let k;
    {
        let mut st = STATE.lock();
        let st = &mut *st;
        let i = match st.insts.get_mut(&inst) { Some(i) => i, None => return Err(-22) };
        let p = match i.watches.iter().position(|w| w.wd == wd) { Some(p) => p, None => return Err(-22) };
        let w = i.watches.remove(p);
        track(w.mask, 0);
        push(i, Ev { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new(), ino: (0, 0), named: false });
        let ch = i.charge.clone();
        index_del(st, inst, wd, w.k());
        st.nwatches -= 1;
        ARMED.fetch_sub(1, Relaxed);
        uc_adj(&mut st.uc, &ch, 0, -1);
        k = key(inst);
    }
    wake(&[k]);
    Ok(())
}

// ── readiness / read ─────────────────────────────────────────────────────────

/// (queue non-empty, edge seq).
pub fn ready(inst: u32) -> (bool, u64) {
    match STATE.lock().insts.get(&inst) { Some(i) => (!i.queue.is_empty(), i.seq), None => (false, 0) }
}

/// FIONREAD: bytes a read would return.
pub fn nbytes(inst: u32) -> usize {
    STATE.lock().insts.get(&inst).map_or(0, |i| i.bytes)
}

/// Serialise whole events into `out` (at most `max` bytes). `Err(-11)` when
/// the queue is empty, `Err(-22)` when the first event does not fit.
pub fn read(inst: u32, max: usize, out: &mut Vec<u8>) -> Result<(), i32> {
    let mut st = STATE.lock();
    let i = match st.insts.get_mut(&inst) { Some(i) => i, None => return Err(-22) };
    if i.queue.is_empty() { return Err(-11); }
    while let Some(e) = i.queue.front() {
        let n = ev_len(e);
        if out.len() + n > max {
            if out.is_empty() { return Err(-22); }
            break;
        }
        let e = i.queue.pop_front().unwrap();
        i.bytes -= n;
        out.extend_from_slice(&e.wd.to_le_bytes());
        out.extend_from_slice(&e.mask.to_le_bytes());
        out.extend_from_slice(&e.cookie.to_le_bytes());
        out.extend_from_slice(&((n - 16) as u32).to_le_bytes());
        out.extend_from_slice(&e.name);
        out.resize(out.len() + (n - 16 - e.name.len()), 0);
    }
    Ok(())
}

// ── producers ────────────────────────────────────────────────────────────────

/// A directory-entry event: `name` inside directory `parent` (both backing
/// paths). `mask` carries IN_ISDIR for directory objects.
pub fn dirent(parent: &[u8], name: &[u8], mask: u32, cookie: u32) { dirent_id(parent, name, mask, cookie, None); }

/// Candidate (instance, wd) pairs for an event on `path` / inode `id`.
fn candidates(st: &State, h: u64, id: Option<Id>) -> Vec<(u32, i32)> {
    let mut list: Vec<(u32, i32)> = st.by_path.get(&h).cloned().unwrap_or_default();
    if let Some(id) = id { if let Some(l) = st.by_ino.get(&id) { list.extend_from_slice(l); } }
    list.sort();
    list.dedup();
    list
}

/// Same, with the inode identity of a name-less (self) event: a watch matches
/// by (dev, ino) when it was made on a multiply-linked inode, by path otherwise.
pub fn dirent_id(parent: &[u8], name: &[u8], mask: u32, cookie: u32, id: Option<Id>) {
    if !armed() { return; }
    if !name.is_empty() && FANM.load(Relaxed) != 0 && mask & (IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO) != 0 {
        let mut full = parent.to_vec();
        if full != b"/" { full.push(b'/'); }
        full.extend_from_slice(name);
        fan_mount(&full, mask, true);
    }
    let h = fnv(parent);
    let by_i = id.is_some() && MULTI.load(Relaxed) != 0;
    if DIR_BLOOM[(h as usize) % BLOOM].load(Relaxed) == 0 && !by_i { return; }
    let is_self = name.is_empty();
    let mut keys: [u64; 8] = [0; 8];
    let mut nk = 0;
    let mut more: Vec<u64> = Vec::new();
    {
        let mut st = STATE.lock();
        let st = &mut *st;
        let list = candidates(st, h, if by_i { id } else { None });
        if list.is_empty() { return; }
        let pid = sched::tgid_of(sched::current_pid()) as u32;
        let mut gone: Vec<(u32, i32, Wk, Vec<UKey>)> = Vec::new();
        for (inst, wd) in list {
            let i = match st.insts.get_mut(&inst) { Some(i) => i, None => continue };
            if mask & IN_MODIFY != 0 {
                if let Some(w) = i.watches.iter_mut().find(|w| w.wd == wd && w.ignore != 0 && !w.surv) { w.ignore = 0; }
            }
            let w = match i.watches.iter().find(|w| w.wd == wd) { Some(w) => w, None => continue };
            if w.mount { continue; }
            let hit = if is_self { (by_i && id == Some(w.ino) && w.ino != (0, 0)) || (!w.multi && w.path == parent) } else { w.isdir && w.path == parent };
            if !hit || w.mask & mask & IN_ALL_EVENTS == 0 { continue; }
            if i.fan {
                if w.ignore & mask != 0 { continue; }
                if !is_self {
                    let mut full = parent.to_vec();
                    if full != b"/" { full.push(b'/'); }
                    full.extend_from_slice(name);
                    if i.watches.iter().any(|x| !x.mount && x.path == full && x.ignore & mask != 0) { continue; }
                }
                if mask & IN_ISDIR != 0 && w.mask & FAN_ONDIR == 0 { continue; }
                if !is_self && w.mask & FAN_ON_CHILD == 0 && mask & (IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO) == 0 { continue; }
            }
            let oneshot = w.mask & IN_ONESHOT != 0;
            let wmask = w.mask;
            let w_ino = w.ino;
            let wk = w.k();
            let ev = if i.fan {
                let mut full = parent.to_vec();
                if !is_self { if full != b"/" { full.push(b'/'); } full.extend_from_slice(name); }
                Ev { wd, mask: mask & (FAN_EVENTS | FAN_FID_EVENTS | IN_ISDIR), cookie: pid, name: full, ino: w_ino, named: !is_self }
            } else {
                Ev { wd, mask, cookie, name: name.to_vec(), ino: (0, 0), named: false }
            };
            let changed = push(i, ev);
            if oneshot {
                track(wmask, 0);
                i.watches.retain(|w| w.wd != wd);
                push(i, Ev { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new(), ino: (0, 0), named: false });
                gone.push((inst, wd, wk, i.charge.clone()));
            }
            if changed || oneshot {
                let k = key(inst);
                if nk < keys.len() { keys[nk] = k; nk += 1; } else { more.push(k); }
            }
        }
        for (inst, wd, wk, ch) in gone { index_del(st, inst, wd, wk); st.nwatches -= 1; ARMED.fetch_sub(1, Relaxed); uc_adj(&mut st.uc, &ch, 0, -1); }
    }
    wake(&keys[..nk]);
    wake(&more);
}

/// `path` (a directory, or a file whose last name and last open description
/// are gone) was removed: DELETE_SELF + IGNORED for every watch on it, and
/// the watches go away.
pub fn removed(path: &[u8], isdir: bool) { removed_id(path, isdir, None); }

pub fn removed_id(path: &[u8], isdir: bool, id: Option<Id>) {
    if !armed() { return; }
    let h = fnv(path);
    let by_i = id.is_some() && MULTI.load(Relaxed) != 0;
    if DIR_BLOOM[(h as usize) % BLOOM].load(Relaxed) == 0 && !by_i { return; }
    let mut keys: Vec<u64> = Vec::new();
    {
        let mut st = STATE.lock();
        let st = &mut *st;
        let list = candidates(st, h, if by_i { id } else { None });
        for (inst, wd) in list {
            let i = match st.insts.get_mut(&inst) { Some(i) => i, None => continue };
            let p = match i.watches.iter().position(|w| w.wd == wd && !w.mount
                && ((by_i && id == Some(w.ino) && w.ino != (0, 0)) || (!w.multi && w.path == path))) { Some(p) => p, None => continue };
            let w = i.watches.remove(p);
            track(w.mask, 0);
            let ifan = i.fan;
            if w.mask & IN_DELETE_SELF != 0 { push(i, Ev { wd, mask: IN_DELETE_SELF | if isdir { IN_ISDIR } else { 0 }, cookie: if ifan { sched::tgid_of(sched::current_pid()) as u32 } else { 0 }, name: if ifan { path.to_vec() } else { Vec::new() }, ino: w.ino, named: false }); }
            push(i, Ev { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new(), ino: (0, 0), named: false });
            keys.push(key(inst));
            let ch = i.charge.clone();
            index_del(st, inst, wd, w.k());
            st.nwatches -= 1;
            ARMED.fetch_sub(1, Relaxed);
            uc_adj(&mut st.uc, &ch, 0, -1);
        }
    }
    wake(&keys);
}

/// `old` (a file or directory) was renamed to `new`: every watch on it or
/// below it keeps following (design §2.2), and so do the open-file notes.
pub fn renamed(old: &[u8], new: &[u8]) {
    notes_renamed(old, new);
    if !armed() { return; }
    let mut st = STATE.lock();
    let st = &mut *st;
    let mut moves: Vec<(u32, i32, Wk, Wk)> = Vec::new();
    for (&inst, i) in st.insts.iter_mut() {
        for w in i.watches.iter_mut() {
            let hit = w.path == old || (w.path.len() > old.len() && w.path.starts_with(old) && w.path[old.len()] == b'/');
            if !hit { continue; }
            let mut np = new.to_vec();
            np.extend_from_slice(&w.path[old.len()..]);
            let nh = fnv(&np);
            let ok = w.k();
            w.path = np;
            w.hash = nh;
            moves.push((inst, w.wd, ok, w.k()));
        }
    }
    // The MULTI/FANM counters balance across the del/add pair.
    for (inst, wd, oh, nh) in moves { index_del(st, inst, wd, oh); index_add(st, inst, wd, nh); }
}


/// Directory component split: ("/a/b/c") -> ("/a/b", "c"); None for "/".
fn split(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() < 2 { return None; }
    let i = path.iter().rposition(|&b| b == b'/')?;
    Some((if i == 0 { &path[..1] } else { &path[..i] }, &path[i + 1..]))
}

fn bloom_hit(h: u64) -> bool { DIR_BLOOM[(h as usize) % BLOOM].load(Relaxed) != 0 }

/// True if some watch may be on `path` itself or on its parent directory.
pub fn may_watch(path: &[u8]) -> bool {
    if !armed() { return false; }
    if MULTI.load(Relaxed) != 0 || FANM.load(Relaxed) != 0 { return true; }
    if bloom_hit(fnv(path)) { return true; }
    split(path).map_or(false, |(p, _)| bloom_hit(fnv(p)))
}

/// An inode event on `path`: the watch on the object itself (no name) and the
/// watch on its parent directory (with the name). `mask` carries IN_ISDIR for
/// directories.
pub fn inode(path: &[u8], mask: u32) {
    if !armed() { return; }
    let id = if MULTI.load(Relaxed) != 0 { crate::inotify_id(path) } else { None };
    inode_id(path, id, mask);
}

/// Same, with the inode identity already resolved (it cannot be once the name
/// is gone).
pub fn inode_id(path: &[u8], id: Option<Id>, mask: u32) {
    if !armed() { return; }
    dirent_id(path, b"", mask, 0, id);
    if let Some((par, name)) = split(path) { dirent(par, name, mask, 0); }
    if FANM.load(Relaxed) != 0 { fan_mount(path, mask, false); }
}

/// The superblock/mount whose backing root is `prefix` went away: UNMOUNT +
/// IGNORED for every watch on or below it, and those watches die.
pub fn unmounted(prefix: &[u8]) {
    if !armed() { return; }
    let mut keys: Vec<u64> = Vec::new();
    {
        let mut st = STATE.lock();
        let st = &mut *st;
        let mut dead: Vec<(u32, i32, Wk)> = Vec::new();
        for (&inst, i) in st.insts.iter_mut() {
            let mut k = 0;
            while k < i.watches.len() {
                let w = &i.watches[k];
                let hit = w.path == prefix || (w.path.len() > prefix.len() && w.path.starts_with(prefix) && w.path[prefix.len()] == b'/');
                if !hit { k += 1; continue; }
                let w = i.watches.remove(k);
                track(w.mask, 0);
                push(i, Ev { wd: w.wd, mask: IN_UNMOUNT, cookie: 0, name: Vec::new(), ino: (0, 0), named: false });
                push(i, Ev { wd: w.wd, mask: IN_IGNORED, cookie: 0, name: Vec::new(), ino: (0, 0), named: false });
                dead.push((inst, w.wd, w.k()));
                let ch = i.charge.clone();
                uc_adj(&mut st.uc, &ch, 0, -1);
                if !keys.contains(&key(inst)) { keys.push(key(inst)); }
            }
        }
        for (inst, wd, h) in dead { index_del(st, inst, wd, h); st.nwatches -= 1; ARMED.fetch_sub(1, Relaxed); }
    }
    wake(&keys);
}

// ── open-file notes (design §2.3, path-keyed) ────────────────────────────────

/// What an armed open file description remembers: the backing path it was
/// opened by (kept current across renames), for MODIFY/CLOSE/ACCESS events.
struct Note { path: Vec<u8>, id: Id, hash: u64, phash: u64, writable: bool, isdir: bool, unlinked: bool }
static NOTES: Mutex<BTreeMap<u32, Note>> = Mutex::new(BTreeMap::new());

pub fn note_set(ofd: u32, path: &[u8], id: Id, writable: bool, isdir: bool) {
    let hash = fnv(path);
    let phash = split(path).map_or(0, |(p, _)| fnv(p));
    NOTES.lock().insert(ofd, Note { path: path.to_vec(), id, hash, phash, writable, isdir, unlinked: false });
}

/// Emit `mask` for the description's file. False if it has no note yet.
pub fn note_event(ofd: u32, mask: u32) -> bool {
    let path = {
        let n = NOTES.lock();
        let Some(note) = n.get(&ofd) else { return false };
        if MULTI.load(Relaxed) == 0 && FANM.load(Relaxed) == 0
            && !(bloom_hit(note.hash) || (note.phash != 0 && bloom_hit(note.phash))) { return true; }
        (note.path.clone(), if note.isdir { IN_ISDIR } else { 0 }, note.id)
    };
    inode_id(&path.0, if path.2 != (0, 0) { Some(path.2) } else { None }, mask | path.1);
    true
}

/// `path` was unlinked: mark the open descriptions on it. True if any is open
/// (DELETE_SELF then waits for the last close).
pub fn note_unlink(path: &[u8], id: Option<Id>) -> bool {
    let mut any = false;
    for n in NOTES.lock().values_mut() {
        if (n.path == path || (id.is_some() && id == Some(n.id))) && !n.unlinked { n.unlinked = true; any = true; }
    }
    any
}

fn notes_renamed(old: &[u8], new: &[u8]) {
    let mut ns = NOTES.lock();
    for n in ns.values_mut() {
        let hit = n.path == old || (n.path.len() > old.len() && n.path.starts_with(old) && n.path[old.len()] == b'/');
        if !hit { continue; }
        let mut np = new.to_vec();
        np.extend_from_slice(&n.path[old.len()..]);
        n.hash = fnv(&np);
        n.phash = split(&np).map_or(0, |(p, _)| fnv(p));
        n.path = np;
    }
}

/// Last reference to description `ofd` is gone: CLOSE event, and DELETE_SELF
/// if its file was unlinked while open and nothing else holds it.
fn note_closed(ofd: u32) {
    let (note, still) = {
        let mut ns = NOTES.lock();
        let Some(note) = ns.remove(&ofd) else { return };
        let still = ns.values().any(|o| (o.path == note.path || (o.id != (0, 0) && o.id == note.id)) && o.unlinked);
        (note, still)
    };
    let d = if note.isdir { IN_ISDIR } else { 0 };
    let id = if note.id != (0, 0) { Some(note.id) } else { None };
    inode_id(&note.path, id, d | if note.writable { IN_CLOSE_WRITE } else { IN_CLOSE_NOWRITE });
    if note.unlinked && !still { removed_id(&note.path, note.isdir, id); }
}


// ── fanotify permission events ───────────────────────────────────────────────

/// Wait key of the opener blocked on a verdict of group `inst`.
fn pkey(inst: u32) -> u64 { sched::poll_key(sched::poll_class::INOTIFY, inst | 0x8000_0000) }

/// Ask every permission group marking `path` (an inode mark on it, a
/// directory mark with ON_CHILD on its parent, or a covering mount mark)
/// whether the access `mask` (FAN_OPEN_PERM / FAN_ACCESS_PERM) may proceed.
/// Blocks the caller until each verdict arrives; a group that closes, or
/// stays silent for `PERM_TIMEOUT_NS`, allows. The group's own thread group
/// never waits on it (Linux gives the listener's event fds FMODE_NONOTIFY;
/// here the whole listener process is exempt, which can never deadlock).
pub fn fan_perm(path: &[u8], mask: u32, isdir: bool) -> i32 {
    if !perm_watched() { return 0; }
    let pid = sched::tgid_of(sched::current_pid()) as u32;
    let parent = split(path).map(|(p, _)| p);
    let mut waits: Vec<(u32, u32)> = Vec::new();
    {
        let mut st = STATE.lock();
        for (&inst, i) in st.insts.iter_mut() {
            if !i.perm || i.owner == pid { continue; }
            if i.watches.iter().any(|w| w.path == path && w.ignore & mask != 0) { continue; }
            let hit = i.watches.iter().any(|w| {
                if w.mask & mask == 0 || w.ignore & mask != 0 { return false; }
                if isdir && w.mask & FAN_ONDIR == 0 { return false; }
                if w.mount { return w.path == b"/" || w.path == path || (path.len() > w.path.len() && path.starts_with(&w.path) && path[w.path.len()] == b'/'); }
                w.path == path || (w.isdir && w.mask & FAN_ON_CHILD != 0 && parent == Some(&w.path[..]))
            });
            if !hit { continue; }
            let id = loop { let c = PERM_ID.fetch_add(1, Relaxed) & 0x7fff_ffff; if c != 0 { break c; } };
            i.pend.push(Pend { id, fd: -1, verdict: 0, errno: 0 });
            i.queue.push_back(Ev { wd: -(id as i32), mask: (mask & FAN_PERM) | if isdir { IN_ISDIR } else { 0 }, cookie: pid, name: path.to_vec(), ino: (0, 0), named: false });
            i.bytes += 24;
            i.seq += 1;
            waits.push((inst, id));
        }
    }
    if waits.is_empty() { return 0; }
    for &(inst, _) in &waits { sched::wake_poll_key(key(inst)); }
    let mut rc = 0;
    for (inst, id) in waits { let e = perm_wait(inst, id); if e != 0 && rc == 0 { rc = e; } }
    rc
}

/// Verdict of one pending event: Some(allow) once decided, the group gone
/// or the event dropped.
fn perm_poll(inst: u32, id: u32) -> Option<i32> {
    let mut st = STATE.lock();
    let Some(i) = st.insts.get_mut(&inst) else { return Some(0) };
    let Some(p) = i.pend.iter().position(|e| e.id == id) else { return Some(0) };
    if i.pend[p].verdict == 0 { return None; }
    let pe = i.pend.remove(p);
    Some(if pe.verdict == 1 { 0 } else if pe.errno != 0 { -pe.errno } else { -1 })
}

fn perm_drop(inst: u32, id: u32) {
    let mut st = STATE.lock();
    if let Some(i) = st.insts.get_mut(&inst) {
        i.pend.retain(|e| e.id != id);
        if let Some(p) = i.queue.iter().position(|e| e.wd == -(id as i32) && e.mask & FAN_PERM != 0) { i.queue.remove(p); i.bytes = i.bytes.saturating_sub(24); }
    }
}

fn perm_wait(inst: u32, id: u32) -> i32 {
    let deadline = sched::monotonic_ns() + PERM_TIMEOUT_NS;
    let k = pkey(inst);
    loop {
        if let Some(v) = perm_poll(inst, id) { return v; }
        let now = sched::monotonic_ns();
        if now >= deadline { perm_drop(inst, id); return 0; }
        sched::block_on_poll_prepare_tag(deadline, &sched::PollTag::keyed(&[k], 0));
        if let Some(v) = perm_poll(inst, id) { sched::block_on_poll_cancel(); return v; }
        sched::block_on_poll_commit();
    }
}

/// The reader opened event fd `fd` for permission event `id`; an event with
/// no fd cannot be answered, so it is allowed.
pub fn fan_bind(inst: u32, id: u32, fd: i32) {
    let mut wake_it = false;
    {
        let mut st = STATE.lock();
        if let Some(e) = st.insts.get_mut(&inst).and_then(|i| i.pend.iter_mut().find(|e| e.id == id)) {
            e.fd = fd;
            if fd < 0 { e.verdict = 1; wake_it = true; }
        }
    }
    if wake_it { sched::wake_poll_key(pkey(inst)); }
}

/// write(group, struct fanotify_response { s32 fd; u32 response }).
pub fn fan_verdict(inst: u32, fd: i32, response: u32) -> Result<(), i32> {
    let errno = ((response >> 24) & 0xff) as i32;
    let v = match response & 0x00ff_ffff & !0x10 { 1 => 1, 2 => 2, _ => return Err(-22) };
    if errno != 0 && v != 2 { return Err(-22); }
    {
        let mut st = STATE.lock();
        let i = st.insts.get_mut(&inst).ok_or(-22)?;
        if !i.perm { return Err(-22); }
        let e = i.pend.iter_mut().find(|e| e.fd == fd && fd >= 0 && e.verdict == 0).ok_or(-2)?;
        e.verdict = v;
        e.errno = errno;
    }
    sched::wake_poll_key(pkey(inst));
    Ok(())
}

/// FAN_RENAME: one event per FID_NAME group with a directory mark on the old or
/// the new parent (or a filesystem mark covering either). The event name holds
/// `old NUL new`.
pub fn fan_rename(old: &[u8], new: &[u8], isdir: bool) {
    if !armed() { return; }
    let (po, pn) = (split(old).map(|(p, _)| p), split(new).map(|(p, _)| p));
    let under = |w: &Watch, p: &[u8]| w.path == b"/" || w.path == p || (p.len() > w.path.len() && p.starts_with(&w.path) && p[w.path.len()] == b'/');
    let mut name = old.to_vec();
    name.push(0);
    name.extend_from_slice(new);
    let pid = sched::tgid_of(sched::current_pid()) as u32;
    let mut keys: Vec<u64> = Vec::new();
    {
        let mut st = STATE.lock();
        for (&inst, i) in st.insts.iter_mut() {
            if !i.fan || i.fid & FAN_REPORT_NAME == 0 { continue; }
            let hit = i.watches.iter().any(|w| {
                if w.mask & FAN_RENAME == 0 || w.ignore & FAN_RENAME != 0 { return false; }
                if isdir && w.mask & FAN_ONDIR == 0 { return false; }
                if w.mount { return under(w, old) || under(w, new); }
                w.isdir && (Some(&w.path[..]) == po || Some(&w.path[..]) == pn)
            });
            if !hit { continue; }
            let wd = i.watches.iter().find(|w| w.mask & FAN_RENAME != 0).map_or(0, |w| w.wd);
            if push(i, Ev { wd, mask: FAN_RENAME | if isdir { IN_ISDIR } else { 0 }, cookie: pid, name: name.clone(), ino: (0, 0), named: true }) { keys.push(key(inst)); }
        }
    }
    wake(&keys);
}

/// FAN_OPEN_EXEC (non-perm): execve of `path` reaches the marks that cover it.
pub fn fan_open_exec(path: &[u8]) {
    if !armed() { return; }
    let parent = split(path).map(|(p, _)| p);
    let pid = sched::tgid_of(sched::current_pid()) as u32;
    let mut keys: Vec<u64> = Vec::new();
    {
        let mut st = STATE.lock();
        for (&inst, i) in st.insts.iter_mut() {
            if !i.fan { continue; }
            if i.watches.iter().any(|w| w.path == path && w.ignore & FAN_OPEN_EXEC != 0) { continue; }
            let wd = i.watches.iter().find(|w| {
                if w.mask & FAN_OPEN_EXEC == 0 || w.ignore & FAN_OPEN_EXEC != 0 { return false; }
                if w.mount { return w.path == b"/" || w.path == path || (path.len() > w.path.len() && path.starts_with(&w.path) && path[w.path.len()] == b'/'); }
                w.path == path || (w.isdir && w.mask & FAN_ON_CHILD != 0 && parent == Some(&w.path[..]))
            }).map(|w| w.wd);
            let Some(wd) = wd else { continue };
            if push(i, Ev { wd, mask: FAN_OPEN_EXEC, cookie: pid, name: path.to_vec(), ino: (0, 0), named: false }) { keys.push(key(inst)); }
        }
    }
    wake(&keys);
}
