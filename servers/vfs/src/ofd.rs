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
//! This kernel's fd tables (VFS below `SOCK_FD_BASE`, the net server's socket
//! tables above it) store each object's own refcounts (pipe ends, eventfd
//! slots, socket ends...), but had no per-description identity. This module
//! is that identity, shared by both tables: an id handed out when an fd is
//! created from nothing, copied with every fd copy (`get`), and dropped with
//! every fd retirement (`put`). The last `put` of an id that epoll registered
//! (`mark_watched`) calls the release hook the kernel installs, which removes
//! every epoll interest on it.
//!
//! Ids carry a generation in the high 16 bits so a reused slot never matches
//! a stale id. Id 0 means "no description" (the table was full): such an fd
//! behaves as before this module, keyed by number.
//!
//! Leaf lock: taken under FD_TABLES / SOCK_TABLES, never held while taking
//! another lock. The release hook runs after it is dropped.

use spin::Mutex;

/// Slots. A full COSMIC session holds a few thousand fds in total; the
/// table only has to cover the live ones.
pub const MAX_OFDS: usize = 16384;
const _: () = assert!(MAX_OFDS <= 1 << 16);

struct Table {
    refs:    [u32; MAX_OFDS],
    gen:     [u16; MAX_OFDS],
    watched: [bool; MAX_OFDS],
    /// Next-fit cursor, so allocation does not rescan the busy low slots.
    cursor:  usize,
    live:    usize,
}

static OFDS: Mutex<Table> = Mutex::new(Table {
    refs: [0; MAX_OFDS], gen: [0; MAX_OFDS], watched: [false; MAX_OFDS], cursor: 0, live: 0,
});

static RELEASE_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Install the function the last `put` of a watched description calls (the
/// kernel's epoll layer).
pub fn set_release_hook(f: fn(u32)) {
    RELEASE_HOOK.store(f as usize, core::sync::atomic::Ordering::Release);
}

#[inline]
fn split(id: u32) -> Option<(usize, u16)> {
    let idx = (id & 0xFFFF) as usize;
    if id == 0 || idx == 0 || idx >= MAX_OFDS { None } else { Some((idx, (id >> 16) as u16)) }
}

/// A new description with one reference, or 0 when the table is full.
pub fn alloc() -> u32 {
    let mut t = OFDS.lock();
    // All-zero initial state (.bss): cursor 0 means slot 1. Slot 0 is id 0.
    let start = t.cursor.max(1);
    let mut i = start;
    loop {
        if t.refs[i] == 0 {
            t.refs[i] = 1;
            t.watched[i] = false;
            t.cursor = if i + 1 >= MAX_OFDS { 1 } else { i + 1 };
            t.live += 1;
            return (t.gen[i] as u32) << 16 | i as u32;
        }
        i = if i + 1 >= MAX_OFDS { 1 } else { i + 1 };
        if i == start { break; }
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
        if t.gen[i] == g && t.refs[i] != 0 { t.refs[i] += 1; }
    }
}

/// One fd naming `id` is gone. The caller must not hold FD_TABLES,
/// SOCK_TABLES or any epoll lock: the last put of a watched description runs
/// the release hook.
pub fn put(id: u32) {
    let Some((i, g)) = split(id) else { return };
    let watched = {
        let mut t = OFDS.lock();
        if t.gen[i] != g || t.refs[i] == 0 { return; }
        t.refs[i] -= 1;
        if t.refs[i] != 0 { return; }
        t.gen[i] = t.gen[i].wrapping_add(1);
        t.live -= 1;
        core::mem::replace(&mut t.watched[i], false)
    };
    if watched {
        let f = RELEASE_HOOK.load(core::sync::atomic::Ordering::Acquire);
        if f != 0 {
            let f: fn(u32) = unsafe { core::mem::transmute(f) };
            f(id);
        }
    }
}

/// True while some fd or in-flight copy still names `id`.
pub fn live(id: u32) -> bool {
    match split(id) {
        Some((i, g)) => { let t = OFDS.lock(); t.gen[i] == g && t.refs[i] != 0 }
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
            if t.gen[i] == g && t.refs[i] != 0 { t.watched[i] = true; true } else { false }
        }
        None => false,
    }
}

/// (live descriptions, capacity) for the Ctrl-T census. IRQ context: try_lock.
pub fn census() -> Option<(usize, usize)> {
    OFDS.try_lock().map(|t| (t.live, MAX_OFDS))
}
