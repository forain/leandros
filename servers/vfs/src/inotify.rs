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
use core::sync::atomic::{AtomicU32, AtomicU16, AtomicU8, Ordering::Relaxed};
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
pub const IN_Q_OVERFLOW: u32 = 0x4000;
pub const IN_IGNORED: u32 = 0x8000;
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
pub const IN_MASK_CREATE: u32 = 0x1000_0000;
pub const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
pub const IN_ONESHOT: u32 = 0x8000_0000;
pub const IN_ALL_EVENTS: u32 = 0xFFF;

const MAX_QUEUED: usize = 16384;
const MAX_INSTANCES: usize = 1024;
const MAX_WATCHES: usize = 65536;
const BLOOM: usize = 4096;

/// Kill switch: 0 unknown, 1 off, 2 on (`vfs.inotify=1` on the command line).
static KNOB: AtomicU8 = AtomicU8::new(0);
/// Live watch count. Every hook starts with one load of this.
static ARMED: AtomicU32 = AtomicU32::new(0);
static COOKIE: AtomicU32 = AtomicU32::new(1);
/// Counting bloom over the hash of each watch's directory path.
static DIR_BLOOM: [AtomicU16; BLOOM] = [const { AtomicU16::new(0) }; BLOOM];

struct Watch { wd: i32, mask: u32, #[allow(dead_code)] ino_key: u64, path: Vec<u8>, hash: u64 }
struct Ev { wd: i32, mask: u32, cookie: u32, name: Vec<u8> }
struct Instance { next_wd: i32, queue: VecDeque<Ev>, bytes: usize, seq: u64, watches: Vec<Watch> }
struct State {
    insts: BTreeMap<u32, Instance>,
    by_path: BTreeMap<u64, Vec<(u32, i32)>>,
    nwatches: usize,
}
static STATE: Mutex<State> = Mutex::new(State { insts: BTreeMap::new(), by_path: BTreeMap::new(), nwatches: 0 });

/// Resolve the knob from the command line on first use (cached once the
/// command line is known).
pub fn enabled(cmdline: Option<&str>) -> bool {
    match KNOB.load(Relaxed) {
        1 => false,
        2 => true,
        _ => match cmdline {
            Some(c) => {
                let on = c.split_ascii_whitespace().any(|t| t == "vfs.inotify=1");
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

fn index_add(st: &mut State, inst: u32, wd: i32, h: u64) {
    st.by_path.entry(h).or_default().push((inst, wd));
    bloom_add(h);
}
fn index_del(st: &mut State, inst: u32, wd: i32, h: u64) {
    if let Some(v) = st.by_path.get_mut(&h) {
        if let Some(p) = v.iter().position(|&e| e == (inst, wd)) { v.swap_remove(p); bloom_del(h); }
        if v.is_empty() { st.by_path.remove(&h); }
    }
}

fn wake(keys: &[u64]) {
    for &k in keys { sched::wake_poll_key(k); }
}

/// Append `ev` to `i`'s queue (tail coalescing, overflow cap). True if the
/// queue changed.
fn push(i: &mut Instance, ev: Ev) -> bool {
    if let Some(t) = i.queue.back() {
        if t.wd == ev.wd && t.mask == ev.mask && t.cookie == ev.cookie && t.name == ev.name { return false; }
    }
    if i.queue.len() >= MAX_QUEUED {
        if i.queue.back().map_or(false, |t| t.mask == IN_Q_OVERFLOW) { return false; }
        i.bytes += 16;
        i.queue.push_back(Ev { wd: -1, mask: IN_Q_OVERFLOW, cookie: 0, name: Vec::new() });
        i.seq += 1;
        return true;
    }
    i.bytes += ev_len(&ev);
    i.queue.push_back(ev);
    i.seq += 1;
    true
}

fn name_len(name: &[u8]) -> usize { if name.is_empty() { 0 } else { (name.len() + 1 + 15) & !15 } }
fn ev_len(e: &Ev) -> usize { 16 + name_len(&e.name) }

// ── instances ────────────────────────────────────────────────────────────────

/// Create the instance for description `inst`. False at the instance limit.
pub fn create(inst: u32) -> bool {
    let mut st = STATE.lock();
    if st.insts.len() >= MAX_INSTANCES { return false; }
    st.insts.insert(inst, Instance { next_wd: 1, queue: VecDeque::new(), bytes: 0, seq: 0, watches: Vec::new() });
    true
}

/// The last reference to description `inst` is gone: drop its watches.
pub fn released(inst: u32) {
    let mut st = STATE.lock();
    if let Some(i) = st.insts.remove(&inst) {
        let n = i.watches.len();
        for w in &i.watches { index_del(&mut st, inst, w.wd, w.hash); }
        st.nwatches -= n;
        ARMED.fetch_sub(n as u32, Relaxed);
    }
}

pub fn exists(inst: u32) -> bool { STATE.lock().insts.contains_key(&inst) }

/// Add (or update) a watch on `path`. Returns the watch descriptor.
pub fn add(inst: u32, path: &[u8], mask: u32, ino_key: u64) -> Result<i32, i32> {
    let ev_mask = (mask & IN_ALL_EVENTS) | (mask & IN_ONESHOT);
    let mut st = STATE.lock();
    let st = &mut *st;
    let i = match st.insts.get_mut(&inst) { Some(i) => i, None => return Err(-22) };
    if let Some(w) = i.watches.iter_mut().find(|w| w.path == path) {
        if mask & IN_MASK_CREATE != 0 { return Err(-17); }
        w.mask = if mask & IN_MASK_ADD != 0 { w.mask | ev_mask } else { ev_mask };
        return Ok(w.wd);
    }
    if st.nwatches >= MAX_WATCHES { return Err(-28); }
    // Cyclic wd allocation that skips live descriptors.
    let mut wd = i.next_wd;
    while i.watches.iter().any(|w| w.wd == wd) { wd = if wd == i32::MAX { 1 } else { wd + 1 }; }
    i.next_wd = if wd == i32::MAX { 1 } else { wd + 1 };
    let h = fnv(path);
    i.watches.push(Watch { wd, mask: ev_mask, ino_key, path: path.to_vec(), hash: h });
    st.nwatches += 1;
    ARMED.fetch_add(1, Relaxed);
    index_add(st, inst, wd, h);
    Ok(wd)
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
        push(i, Ev { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
        index_del(st, inst, wd, w.hash);
        st.nwatches -= 1;
        ARMED.fetch_sub(1, Relaxed);
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
pub fn dirent(parent: &[u8], name: &[u8], mask: u32, cookie: u32) {
    if !armed() { return; }
    let h = fnv(parent);
    if DIR_BLOOM[(h as usize) % BLOOM].load(Relaxed) == 0 { return; }
    let mut keys: [u64; 8] = [0; 8];
    let mut nk = 0;
    let mut more: Vec<u64> = Vec::new();
    {
        let mut st = STATE.lock();
        let st = &mut *st;
        let list = match st.by_path.get(&h) { Some(l) => l.clone(), None => return };
        let mut gone: Vec<(u32, i32, u64)> = Vec::new();
        for (inst, wd) in list {
            let i = match st.insts.get_mut(&inst) { Some(i) => i, None => continue };
            let w = match i.watches.iter().find(|w| w.wd == wd) { Some(w) => w, None => continue };
            if w.path != parent || w.mask & mask & IN_ALL_EVENTS == 0 { continue; }
            let oneshot = w.mask & IN_ONESHOT != 0;
            let hh = w.hash;
            let changed = push(i, Ev { wd, mask, cookie, name: name.to_vec() });
            if oneshot {
                i.watches.retain(|w| w.wd != wd);
                push(i, Ev { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
                gone.push((inst, wd, hh));
            }
            if changed || oneshot {
                let k = key(inst);
                if nk < keys.len() { keys[nk] = k; nk += 1; } else { more.push(k); }
            }
        }
        for (inst, wd, hh) in gone { index_del(st, inst, wd, hh); st.nwatches -= 1; ARMED.fetch_sub(1, Relaxed); }
    }
    wake(&keys[..nk]);
    wake(&more);
}

/// `path` (a directory) was removed: DELETE_SELF + IGNORED for every watch
/// on it, and the watches go away.
pub fn dir_removed(path: &[u8]) {
    if !armed() { return; }
    let h = fnv(path);
    if DIR_BLOOM[(h as usize) % BLOOM].load(Relaxed) == 0 { return; }
    let mut keys: Vec<u64> = Vec::new();
    {
        let mut st = STATE.lock();
        let st = &mut *st;
        let list = match st.by_path.get(&h) { Some(l) => l.clone(), None => return };
        for (inst, wd) in list {
            let i = match st.insts.get_mut(&inst) { Some(i) => i, None => continue };
            let p = match i.watches.iter().position(|w| w.wd == wd && w.path == path) { Some(p) => p, None => continue };
            let w = i.watches.remove(p);
            if w.mask & IN_DELETE_SELF != 0 { push(i, Ev { wd, mask: IN_DELETE_SELF | IN_ISDIR, cookie: 0, name: Vec::new() }); }
            push(i, Ev { wd, mask: IN_IGNORED, cookie: 0, name: Vec::new() });
            keys.push(key(inst));
            index_del(st, inst, wd, h);
            st.nwatches -= 1;
            ARMED.fetch_sub(1, Relaxed);
        }
    }
    wake(&keys);
}

/// Directory `old` was renamed to `new`: every watch on it or below it keeps
/// following (design §2.2).
pub fn dir_renamed(old: &[u8], new: &[u8]) {
    if !armed() { return; }
    let mut st = STATE.lock();
    let st = &mut *st;
    let mut moves: Vec<(u32, i32, u64, u64)> = Vec::new();
    for (&inst, i) in st.insts.iter_mut() {
        for w in i.watches.iter_mut() {
            let hit = w.path == old || (w.path.len() > old.len() && w.path.starts_with(old) && w.path[old.len()] == b'/');
            if !hit { continue; }
            let mut np = new.to_vec();
            np.extend_from_slice(&w.path[old.len()..]);
            let nh = fnv(&np);
            moves.push((inst, w.wd, w.hash, nh));
            w.path = np;
            w.hash = nh;
        }
    }
    for (inst, wd, oh, nh) in moves { index_del(st, inst, wd, oh); index_add(st, inst, wd, nh); }
}
