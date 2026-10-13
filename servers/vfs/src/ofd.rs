//! Open file descriptions: the identity epoll keys its registrations by.
//!
//! On Linux every `open()`/`pipe()`/`socket()`... creates an *open file
//! description*. `dup`, `fork` and SCM_RIGHTS make more fds naming the same
//! description, and the description dies with its LAST reference, wherever
//! that reference lives. An epoll item is keyed by (description, fd number)
//! and is removed only when the description dies (`eventpoll_release`), so a
//! registered fd that is closed while a forked child (or a dup, or an
//! in-flight SCM_RIGHTS copy) still holds the description keeps reporting.
//!
//! This kernel's fd tables (the VFS's per-process tables, and the net
//! server's socket tables behind their socket entries) store each object's own refcounts (pipe ends, eventfd
//! slots, socket ends...), but had no per-description identity. This module
//! is that identity, shared by both tables: an id handed out when an fd is
//! created from nothing, copied with every fd copy (`get`), and dropped with
//! every fd retirement (`put`). The last `put` of an id that epoll registered
//! (`mark_watched`) calls the release hook the kernel installs, which removes
//! every epoll interest on it.
//!
//! Ids carry a generation in the high 12 bits so a reused slot does not match
//! a stale id. Id 0 means "no description" (the table was full): such an fd
//! behaves as before this module, keyed by number.
//!
//! Leaf lock: taken under FD_TABLES / SOCK_TABLES, never held while taking
//! another lock. The release hook runs after it is dropped.

use spin::Mutex;

/// Slots. A full COSMIC session holds a few thousand fds in total; the
/// table only has to cover the live ones. It grows on demand (OFD_GROW slots
/// at a time) up to this ceiling: lane fdmax lifted the per-process fd limit
/// to RLIMIT_NOFILE, and a fixed 16384 would have handed id 0 (no
/// description) to every fd past it.
///
/// An id is `gen << IDX_BITS | idx`: 20 bits of slot (1 Mi descriptions) and
/// 12 bits of generation, so a reused slot only matches a stale id after 4096
/// reuses of that one slot.
pub const MAX_OFDS: usize = 1 << IDX_BITS;
const IDX_BITS: u32 = 20;
const IDX_MASK: u32 = (1 << IDX_BITS) - 1;
const GEN_MASK: u16 = (1 << (32 - IDX_BITS)) - 1;
const OFD_GROW: usize = 1024;

#[derive(Clone, Copy)]
struct Slot { refs: u32, gen: u16, watched: bool }

struct Table {
    s:       alloc::vec::Vec<Slot>,
    /// Next-fit cursor, so allocation does not rescan the busy low slots.
    cursor:  usize,
    live:    usize,
}

static OFDS: Mutex<Table> = Mutex::new(Table { s: alloc::vec::Vec::new(), cursor: 0, live: 0 });

static RELEASE_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Install the function the last `put` of a watched description calls (the
/// kernel's epoll layer).
pub fn set_release_hook(f: fn(u32)) {
    RELEASE_HOOK.store(f as usize, core::sync::atomic::Ordering::Release);
}

#[inline]
fn split(id: u32) -> Option<(usize, u16)> {
    let idx = (id & IDX_MASK) as usize;
    if id == 0 || idx == 0 { None } else { Some((idx, (id >> IDX_BITS) as u16)) }
}

/// A new description with one reference, or 0 when the table is full.
pub fn alloc() -> u32 {
    let mut t = OFDS.lock();
    // Slot 0 is id 0 ("none") and is never handed out: it stays busy.
    if t.s.is_empty() {
        if t.s.try_reserve(OFD_GROW).is_err() { return 0; }
        t.s.push(Slot { refs: u32::MAX, gen: 0, watched: false });
    }
    let n = t.s.len();
    // Every slot busy (slot 0 included): grow instead of a full rescan.
    if t.live + 1 >= n && n < MAX_OFDS && t.s.try_reserve(OFD_GROW).is_ok() {
        let m = (n + OFD_GROW).min(MAX_OFDS);
        t.s.resize(m, Slot { refs: 0, gen: 0, watched: false });
    }
    let n = t.s.len();
    if n > 1 {
        let start = if t.cursor >= 1 && t.cursor < n { t.cursor } else { 1 };
        let mut i = start;
        loop {
            if t.s[i].refs == 0 {
                t.s[i].refs = 1;
                t.s[i].watched = false;
                t.cursor = if i + 1 >= n { 1 } else { i + 1 };
                t.live += 1;
                return (t.s[i].gen as u32) << IDX_BITS | i as u32;
            }
            i = if i + 1 >= n { 1 } else { i + 1 };
            if i == start { break; }
        }
    }
    drop(t);
    static REPORTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        crate::report_pool_full("open-file-description", MAX_OFDS);
    }
    0
}

/// One more fd (or in-flight copy) names `id`.
pub fn get(id: u32) {
    if let Some((i, g)) = split(id) {
        let mut t = OFDS.lock();
        if let Some(e) = t.s.get_mut(i) { if e.gen == g && e.refs != 0 { e.refs += 1; } }
    }
}

/// One fd naming `id` is gone. The caller must not hold FD_TABLES,
/// SOCK_TABLES or any epoll lock: the last put of a watched description runs
/// the release hook.
pub fn put(id: u32) {
    let (_, watched) = put_deferred(id);
    if watched { release(id); }
}

/// The reference count half of `put`, safe under any lock (this module's own
/// is a leaf): `(last, watched)`. `last`: that was the description's final
/// reference — the object behind a description with no refcount of its own
/// (an inet socket's smoltcp handles) is torn down exactly then, and id 0 (no
/// description, so never shared) is always the last. `watched`: the caller
/// must call `release(id)` once it holds none of the locks `put` names.
pub fn put_deferred(id: u32) -> (bool, bool) {
    let Some((i, g)) = split(id) else { return (true, false) };
    let mut t = OFDS.lock();
    // A stale id was already released by whoever dropped it to zero.
    let Some(e) = t.s.get_mut(i) else { return (false, false) };
    if e.gen != g || e.refs == 0 { return (false, false); }
    e.refs -= 1;
    if e.refs != 0 { return (false, false); }
    e.gen = e.gen.wrapping_add(1) & GEN_MASK;
    let w = core::mem::replace(&mut e.watched, false);
    t.live -= 1;
    (true, w)
}

/// Run the release hook for `id`, whose last reference `put_deferred` dropped
/// with `watched` set.
pub fn release(id: u32) {
    let f = RELEASE_HOOK.load(core::sync::atomic::Ordering::Acquire);
    if f != 0 {
        let f: fn(u32) = unsafe { core::mem::transmute(f) };
        f(id);
    }
}

/// More than one fd (or in-flight copy) names `id`: a change to the object's
/// state must reach the other holders' copies of it.
pub fn shared(id: u32) -> bool {
    match split(id) {
        Some((i, g)) => { let t = OFDS.lock(); t.s.get(i).map_or(false, |e| e.gen == g && e.refs > 1) }
        None => false,
    }
}

/// True while some fd or in-flight copy still names `id`.
pub fn live(id: u32) -> bool {
    match split(id) {
        Some((i, g)) => { let t = OFDS.lock(); t.s.get(i).map_or(false, |e| e.gen == g && e.refs != 0) }
        None => false,
    }
}

/// Epoll registered `id`: its last `put` must call the release hook. Returns
/// `live(id)` as of the marking (see the kernel's epoll_ctl for the race this
/// closes).
pub fn mark_watched(id: u32) -> bool {
    match split(id) {
        Some((i, g)) => {
            let mut t = OFDS.lock();
            match t.s.get_mut(i) {
                Some(e) if e.gen == g && e.refs != 0 => { e.watched = true; true }
                _ => false,
            }
        }
        None => false,
    }
}

/// (live descriptions, capacity) for the Ctrl-T census. IRQ context: try_lock.
pub fn census() -> Option<(usize, usize)> {
    OFDS.try_lock().map(|t| (t.live, t.s.len()))
}
