//! Unified page cache: regular-file data kept in memory after it was read
//! once (lane pagecache, 2026-10-11).
//!
//! One frame per (file, page index), shared by everything that reads the
//! file: read(2)/pread/readv copy out of it, mmap(2) and exec map it. Pages
//! stay after close and after the last unmap; memory pressure, a cap, or the
//! file going away is what frees them.
//!
//! IDENTITY. A file is a `key` the filesystem chooses: (mount, inode) for
//! f2fs (`f2fs_server::page_cache_key`), never 0. Rename does not change it;
//! hard links share it; the filesystem drops it (`drop_file`) before an
//! inode number can be reused.
//!
//! COHERENCY MODEL: write-through. Every change of a file's data runs under
//! the filesystem lock, and at the end of the request (its final no-I/O
//! round) the filesystem applies it to the cached pages as well:
//!   * write(2)/pwrite: `wt_pin` + copy + `wt_done` — the cached pages of the
//!     written range get the new bytes in place, so MAP_SHARED and private
//!     mappings that still map the frame see the write, exactly as on Linux;
//!     `extend` zeroes the gap a write past EOF leaves in the old last page;
//!   * truncate: `truncate` frees the pages past the new end and zeroes the
//!     tail of the new last page; grow is `extend` + `note_size`;
//!   * an inode freed (last link and last opener gone): `drop_file`.
//! Invalidate-on-write was the alternative and is wrong here: without a
//! reverse map a mapped frame cannot be taken away from the processes that
//! map it, so dropping it from the cache would split the file into the
//! mapping's stale copy and the filesystem's new one. Updating the one frame
//! everyone maps keeps a single copy of the truth.
//!
//! FILLS. A miss inserts FILLING placeholders for the pages it will read
//! (`begin_fill`) before it asks the filesystem for the data, then publishes
//! the frames (`end_fill`). A write or truncate that meets a placeholder
//! marks it STALE and the fill discards that page: the read may predate the
//! change. Concurrent misses of the same page see the placeholder (`Busy`)
//! and wait for the one fill instead of reading the device again.
//!
//! SHARED MAPPINGS. A MAP_SHARED file mapping maps the cached frame itself,
//! read-only until its first write; the write fault marks the page DIRTY
//! (`mark_dirty`) and makes it writable. Dirty pages are never evicted and
//! are written back through the filesystem (`take_dirty` / `writeback_done`)
//! by msync, fsync/sync, and the background task once no mapping holds them.
//! Without a reverse map a writable PTE cannot be write-protected again, so a
//! page stays dirty while it is still mapped.
//!
//! EVICTION: a clock over (key, page) order. Only clean, published pages no
//! one but the cache references (`pageref` count 1: no mapping, no reader
//! copying out of it) are candidates; a page referenced since the hand last
//! passed gets a second chance. Driven by
//!   * the cap (`pcache.max_mb`, default half of RAM) and the free-memory
//!     low watermark, checked before every fill (`make_room`);
//!   * the background task, polling the watermark;
//!   * the buddy allocator's failure path (`emergency_reclaim`): try-locks
//!     only, no heap traffic, so it is safe under any lock an allocating
//!     path may hold.
//!
//! `pcache=0` on the command line: read(2) bypasses the cache and pages
//! cached for mappings are dropped when the last mapping goes (the
//! behaviour before this lane), for A/B runs.
//!
//! Lock order: `CACHE` → `pageref` → buddy/slab. Nothing here is called with
//! `CACHE` held except those, and `CACHE` is never taken under them except
//! by the try-lock emergency path.

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;
use crate::buddy::PAGE_SIZE;

const ST_FILLING: u8 = 0;
const ST_UPTODATE: u8 = 1;

/// Referenced since the clock hand last passed.
const F_REF: u8 = 1;
/// Written through a shared mapping; not written back yet.
const F_DIRTY: u8 = 2;
/// Has been mapped executable: a change needs I-cache maintenance.
const F_EXEC: u8 = 4;
/// FILLING only: the data being read is already out of date.
const F_STALE: u8 = 8;
/// Hint: handed to a mapping, not seen unmapped by the hand since.
const F_MAPPED: u8 = 16;

#[derive(Clone, Copy)]
struct Page {
    /// The frame (0: evicted by the emergency path, entry not yet purged,
    /// or a placeholder still being filled).
    phys: usize,
    token: u32,
    state: u8,
    flags: u8,
}

struct FileEnt {
    pages: BTreeMap<u64, Page>,
    size: u64,
    size_known: bool,
    /// Last name gone while still open: free the pages at the last close.
    orphan: bool,
    /// Live mapping-registry entries (`key_get`/`key_put`, pcache=0 only).
    users: u32,
    /// Readahead: page after the last fill, and the current window.
    ra_next: u64,
    ra_win: u32,
    /// `monotonic` ns of the last relatime check that needed no update (0:
    /// due).
    atime_ok: u64,
    ndirty: u32,
}

impl FileEnt {
    const fn new() -> Self {
        FileEnt { pages: BTreeMap::new(), size: 0, size_known: false, orphan: false, users: 0,
                  ra_next: u64::MAX, ra_win: 0, atime_ok: 0, ndirty: 0 }
    }
}

struct Cache {
    files: BTreeMap<u64, FileEnt>,
    /// Clock hand: (key, page index) where the next scan starts.
    hand: (u64, u64),
    next_token: u32,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { files: BTreeMap::new(), hand: (0, 0), next_token: 1 });

/// Published frames the cache holds.
static CACHED: AtomicUsize = AtomicUsize::new(0);
static DIRTY: AtomicUsize = AtomicUsize::new(0);
static FILLING: AtomicUsize = AtomicUsize::new(0);
static MAPPED: AtomicUsize = AtomicUsize::new(0);

/// Cumulative counters (pages unless noted).
pub struct Counters {
    pub hits: AtomicUsize,
    pub misses: AtomicUsize,
    pub readahead: AtomicUsize,
    pub evicted: AtomicUsize,
    pub emergency: AtomicUsize,
    pub waits: AtomicUsize,
    pub stale: AtomicUsize,
    pub wrote_through: AtomicUsize,
    pub written_back: AtomicUsize,
    pub bypass: AtomicUsize,
    pub fills: AtomicUsize,
}
pub static COUNTERS: Counters = Counters {
    hits: AtomicUsize::new(0), misses: AtomicUsize::new(0), readahead: AtomicUsize::new(0),
    evicted: AtomicUsize::new(0), emergency: AtomicUsize::new(0), waits: AtomicUsize::new(0),
    stale: AtomicUsize::new(0), wrote_through: AtomicUsize::new(0), written_back: AtomicUsize::new(0),
    bypass: AtomicUsize::new(0), fills: AtomicUsize::new(0),
};

#[inline]
fn bump(c: &AtomicUsize, n: usize) { if n != 0 { c.fetch_add(n, Ordering::Relaxed); } }

// ── configuration ─────────────────────────────────────────────────────────────

/// read(2) goes through the cache and pages outlive their mappings
/// (`pcache=0` clears it).
static READ_CACHE: AtomicBool = AtomicBool::new(true);
/// Cap in pages (0: half of RAM).
static CAP_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Apply the command line: `pcache=0` turns the read cache off,
/// `pcache.max_mb=N` caps it.
pub fn configure(read_cache: bool, cap_mb: Option<u64>) {
    READ_CACHE.store(read_cache, Ordering::Relaxed);
    if let Some(mb) = cap_mb {
        CAP_PAGES.store(((mb as usize) << 20) / PAGE_SIZE, Ordering::Relaxed);
    }
}

pub fn read_cache_enabled() -> bool { READ_CACHE.load(Ordering::Relaxed) }

pub fn cap_pages() -> usize {
    match CAP_PAGES.load(Ordering::Relaxed) {
        0 => crate::buddy::total_pages() / 2,
        n => n,
    }
}

/// (low, high) free-page watermarks: under `low` the cache gives frames
/// back until `high` are free. Both sit above init's memory-pressure floor
/// (RAM/10), so the guard sees memory the cache could not give back.
pub fn watermarks() -> (usize, usize) {
    let total = crate::buddy::total_pages();
    (total / 8, total / 6)
}

/// Fills may not take free memory below this (they are then served
/// uncached): RAM/12, between the guard's floor and the low watermark.
fn hard_floor() -> usize { crate::buddy::total_pages() / 12 }

// ── statistics ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub cached: usize,
    pub dirty: usize,
    pub filling: usize,
    pub mapped_hint: usize,
    pub files: usize,
    pub cap: usize,
    pub hits: usize,
    pub misses: usize,
    pub readahead: usize,
    pub evicted: usize,
    pub emergency: usize,
    pub waits: usize,
    pub stale: usize,
    pub wrote_through: usize,
    pub written_back: usize,
    pub bypass: usize,
    pub fills: usize,
}

/// Snapshot of the counters (no lock; `files` needs one and is a try).
pub fn stats() -> Stats {
    let c = &COUNTERS;
    Stats {
        cached: CACHED.load(Ordering::Relaxed),
        dirty: DIRTY.load(Ordering::Relaxed),
        filling: FILLING.load(Ordering::Relaxed),
        mapped_hint: MAPPED.load(Ordering::Relaxed),
        files: CACHE.try_lock().map_or(0, |c| c.files.len()),
        cap: cap_pages(),
        hits: c.hits.load(Ordering::Relaxed),
        misses: c.misses.load(Ordering::Relaxed),
        readahead: c.readahead.load(Ordering::Relaxed),
        evicted: c.evicted.load(Ordering::Relaxed),
        emergency: c.emergency.load(Ordering::Relaxed),
        waits: c.waits.load(Ordering::Relaxed),
        stale: c.stale.load(Ordering::Relaxed),
        wrote_through: c.wrote_through.load(Ordering::Relaxed),
        written_back: c.written_back.load(Ordering::Relaxed),
        bypass: c.bypass.load(Ordering::Relaxed),
        fills: c.fills.load(Ordering::Relaxed),
    }
}

/// Frames currently held by the cache.
pub fn cached_pages() -> usize { CACHED.load(Ordering::Relaxed) }
pub fn dirty_pages() -> usize { DIRTY.load(Ordering::Relaxed) }

/// Pages the cache could give back now, as far as it knows without a scan:
/// clean published pages not believed mapped (for MemAvailable).
pub fn reclaimable_pages() -> usize {
    CACHED.load(Ordering::Relaxed)
        .saturating_sub(DIRTY.load(Ordering::Relaxed))
        .saturating_sub(MAPPED.load(Ordering::Relaxed))
}

/// Count the counters the old `/proc/kmemstat` line printed.
pub fn legacy_stats() -> (usize, usize, usize) {
    let c = &COUNTERS;
    (c.hits.load(Ordering::Relaxed), c.fills.load(Ordering::Relaxed),
     c.evicted.load(Ordering::Relaxed) + c.emergency.load(Ordering::Relaxed))
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Byte offset `off` as a page offset, if page-aligned.
pub fn pgoff_of(off: u64) -> Option<u64> {
    if off % PAGE_SIZE as u64 == 0 { Some(off / PAGE_SIZE as u64) } else { None }
}

fn note_gone(p: &Page, fe: &mut FileEnt) {
    if p.state == ST_UPTODATE && p.phys != 0 {
        CACHED.fetch_sub(1, Ordering::Relaxed);
        if p.flags & F_DIRTY != 0 {
            DIRTY.fetch_sub(1, Ordering::Relaxed);
            fe.ndirty = fe.ndirty.saturating_sub(1);
        }
        if p.flags & F_MAPPED != 0 { MAPPED.fetch_sub(1, Ordering::Relaxed); }
    } else if p.state == ST_FILLING {
        FILLING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Make the bytes of a frame that changed in place visible to instruction
/// fetch (AArch64; x86 keeps the I-cache coherent).
#[allow(unused_variables)]
fn sync_icache(phys: usize, off: usize, len: usize) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let base = crate::phys_to_virt(phys) + off;
        let mut line = base & !63;
        while line < base + len {
            core::arch::asm!("dc cvau, {}", in(reg) line);
            line += 64;
        }
        core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb");
    }
}

/// Clean a freshly filled frame to the point of coherency, so a later
/// executable mapping of it (in any process) fetches these bytes.
#[allow(unused_variables)]
pub fn clean_dcache(virt: usize, len: usize) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let mut line = virt & !63;
        while line < virt + len {
            core::arch::asm!("dc cvac, {}", in(reg) line);
            line += 64;
        }
        core::arch::asm!("dsb ish");
    }
}

// ── file-level state ──────────────────────────────────────────────────────────

/// The file's size, if the cache knows it (set by the filesystem).
pub fn file_size(key: u64) -> Option<u64> {
    let c = CACHE.lock();
    c.files.get(&key).filter(|f| f.size_known).map(|f| f.size)
}

/// The filesystem's word on the file's size (under its lock, from a request
/// that completes). Creates the entry: from now on every size change of the
/// file must reach `note_size`/`truncate`.
pub fn set_size(key: u64, size: u64) {
    if key == 0 { return; }
    let mut c = CACHE.lock();
    let fe = c.files.entry(key).or_insert_with(FileEnt::new);
    fe.size = size;
    fe.size_known = true;
}

/// The file's size changed to `size` (no-op unless the cache tracks it).
pub fn note_size(key: u64, size: u64) {
    let mut c = CACHE.lock();
    if let Some(fe) = c.files.get_mut(&key) {
        if fe.size_known { fe.size = size; }
        fe.atime_ok = 0;
    }
}

/// The file's metadata changed in a way relatime looks at (ctime/mtime):
/// the next read checks atime again.
pub fn note_ctime(key: u64) {
    let mut c = CACHE.lock();
    if let Some(fe) = c.files.get_mut(&key) { fe.atime_ok = 0; }
}

/// relatime: does a read at `now` need the filesystem to look at atime?
/// `period` bounds how long a "no update needed" answer is trusted.
pub fn atime_due(key: u64, now: u64, period: u64) -> bool {
    let c = CACHE.lock();
    match c.files.get(&key) {
        Some(fe) => fe.atime_ok == 0 || now.saturating_sub(fe.atime_ok) >= period,
        None => true,
    }
}

/// The filesystem checked (and if needed updated) atime at `now`.
pub fn atime_checked(key: u64, now: u64) {
    let mut c = CACHE.lock();
    if let Some(fe) = c.files.get_mut(&key) { fe.atime_ok = now.max(1); }
}

/// The file's last name went while it was still open or mapped.
pub fn set_orphan(key: u64) {
    let mut c = CACHE.lock();
    if let Some(fe) = c.files.get_mut(&key) { fe.orphan = true; }
}

pub fn is_orphan(key: u64) -> bool {
    CACHE.lock().files.get(&key).map_or(false, |f| f.orphan)
}

/// Keys of orphaned files (background sweep).
pub fn orphans(out: &mut Vec<u64>) {
    let c = CACHE.lock();
    for (k, fe) in c.files.iter() { if fe.orphan { out.push(*k); } }
}

/// Forget the file: every cached page goes (frames a mapping still holds
/// stay with it, detached), dirty data included — the file is gone or its
/// identity is about to be reused. In-flight fills find no placeholder and
/// discard what they read.
pub fn drop_file(key: u64) {
    if key == 0 { return; }
    let gone = {
        let mut c = CACHE.lock();
        match c.files.remove(&key) {
            Some(mut fe) => {
                let pages = core::mem::take(&mut fe.pages);
                let mut v: Vec<usize> = Vec::new();
                for (_, p) in pages.iter() {
                    note_gone(p, &mut fe);
                    if p.state == ST_UPTODATE && p.phys != 0 { v.push(p.phys); }
                }
                v
            }
            None => return,
        }
    };
    for phys in gone { crate::pageref::unref_or_free(phys, 0); }
}

/// A mapping registry entry for `key` was created.
pub fn key_get(key: u64) {
    if key == 0 { return; }
    let mut c = CACHE.lock();
    c.files.entry(key).or_insert_with(FileEnt::new).users += 1;
}

/// A mapping registry entry for `key` went away. With `pcache=0` the last
/// one drops the file's pages (mapping-lifetime caching, as before).
pub fn key_put(key: u64) {
    if key == 0 { return; }
    let last = {
        let mut c = CACHE.lock();
        match c.files.get_mut(&key) {
            Some(fe) => { fe.users = fe.users.saturating_sub(1); fe.users == 0 }
            None => false,
        }
    };
    if last && !read_cache_enabled() { drop_file(key); }
}

// ── reads ─────────────────────────────────────────────────────────────────────

/// Result of [`pin_run`].
pub enum Run {
    /// This many frames, from the requested page on, were pinned.
    Hit(usize),
    /// The first page is not cached: fill it.
    Miss,
    /// The first page is being filled by someone else: wait and retry.
    Busy,
}

/// Pin up to `out.len()` consecutive cached pages of `key` starting at
/// `pgoff` (one `pageref` reference each, dropped with [`unpin`]), so they
/// can be copied out with no lock held.
pub fn pin_run(key: u64, pgoff: u64, out: &mut [usize]) -> Run {
    let mut c = CACHE.lock();
    let fe = match c.files.get_mut(&key) { Some(f) => f, None => return Run::Miss };
    let mut n = 0usize;
    let mut first_state = None;
    for (&pg, p) in fe.pages.range_mut(pgoff..) {
        if pg != pgoff + n as u64 || n == out.len() { break; }
        if n == 0 { first_state = Some(p.state); }
        if p.state != ST_UPTODATE || p.phys == 0 { break; }
        p.flags |= F_REF;
        out[n] = p.phys;
        n += 1;
    }
    if n == 0 {
        return if first_state == Some(ST_FILLING) { Run::Busy } else { Run::Miss };
    }
    crate::pageref::inc_many(&out[..n]);
    drop(c);
    bump(&COUNTERS.hits, n);
    Run::Hit(n)
}

/// Drop the references [`pin_run`]/[`pin_page`] took.
pub fn unpin(frames: &[usize]) {
    for &p in frames { crate::pageref::unref_or_free(p, 0); }
}

/// The cached, published frame for (`key`, `pgoff`), pinned, if any.
pub fn pin_page(key: u64, pgoff: u64) -> Option<usize> {
    let mut one = [0usize; 1];
    match pin_run(key, pgoff, &mut one) { Run::Hit(_) => Some(one[0]), _ => None }
}

/// Readahead: how many pages a miss at `pgoff` for a request of `req` pages
/// should fill (at most `max`). A miss right where the previous fill ended
/// doubles the window; any other miss starts again at `RA_INIT`.
pub fn ra_window(key: u64, pgoff: u64, req: usize, max: usize) -> usize {
    const RA_INIT: u32 = 16;
    let mut c = CACHE.lock();
    let fe = match c.files.get_mut(&key) { Some(f) => f, None => return req.max(1) };
    let seq = pgoff == fe.ra_next;
    let win = if seq { (fe.ra_win.saturating_mul(2)).max(RA_INIT) } else { RA_INIT };
    let win = (win as usize).min(max.max(1));
    fe.ra_win = win as u32;
    let n = req.max(win).min(max.max(req)).max(1);
    fe.ra_next = pgoff + n as u64;
    n
}

/// Insert FILLING placeholders for up to `want` consecutive absent pages of
/// `key` from `pgoff` (stopping at the first page that is cached or being
/// filled, and at end of file). None if the file is not tracked or the
/// first page is not absent; else the fill's token and the page count.
pub fn begin_fill(key: u64, pgoff: u64, want: usize) -> Option<(u32, usize)> {
    let mut c = CACHE.lock();
    let token = c.next_token;
    c.next_token = c.next_token.wrapping_add(1).max(1);
    let fe = c.files.get_mut(&key)?;
    if !fe.size_known || fe.size == 0 { return None; }
    let last = (fe.size - 1) / PAGE_SIZE as u64;
    let mut n = 0usize;
    while n < want {
        let pg = pgoff + n as u64;
        if pg > last { break; }
        let absent = match fe.pages.get(&pg) {
            None => true,
            Some(p) => p.state == ST_UPTODATE && p.phys == 0,
        };
        if !absent { break; }
        fe.pages.insert(pg, Page { phys: 0, token, state: ST_FILLING, flags: 0 });
        n += 1;
    }
    if n == 0 { return None; }
    FILLING.fetch_add(n, Ordering::Relaxed);
    Some((token, n))
}

/// Publish the frames read for the fill `token` (one per placeholder from
/// `pgoff`; `valid` bytes of real data from the first). A placeholder that
/// went stale or away, and a page wholly past `valid`, discards its frame.
/// Every frame passed in is either taken by the cache or freed. Returns how
/// many were published.
pub fn end_fill(key: u64, token: u32, pgoff: u64, frames: &[usize], valid: usize) -> usize {
    let mut freed: Vec<usize> = Vec::new();
    let _ = freed.try_reserve(frames.len());
    let mut published = 0usize;
    let mut stale = 0usize;
    {
        let mut c = CACHE.lock();
        match c.files.get_mut(&key) {
            None => freed.extend_from_slice(frames),
            Some(fe) => {
                for (i, &phys) in frames.iter().enumerate() {
                    let pg = pgoff + i as u64;
                    let ours = matches!(fe.pages.get(&pg),
                        Some(p) if p.state == ST_FILLING && p.token == token);
                    if !ours { freed.push(phys); continue; }
                    let p = fe.pages.get_mut(&pg).unwrap();
                    FILLING.fetch_sub(1, Ordering::Relaxed);
                    if p.flags & F_STALE != 0 || i * PAGE_SIZE >= valid {
                        if p.flags & F_STALE != 0 { stale += 1; }
                        fe.pages.remove(&pg);
                        freed.push(phys);
                        continue;
                    }
                    p.phys = phys;
                    p.state = ST_UPTODATE;
                    p.flags = F_REF;
                    published += 1;
                }
            }
        }
    }
    CACHED.fetch_add(published, Ordering::Relaxed);
    bump(&COUNTERS.fills, published);
    bump(&COUNTERS.stale, stale);
    for phys in freed { crate::buddy::free(phys, 0); }
    published
}

/// Remove the placeholders of fill `token` for `n` pages from `pgoff` (the
/// read failed or was cut short).
pub fn abort_fill(key: u64, token: u32, pgoff: u64, n: usize) {
    let mut c = CACHE.lock();
    if let Some(fe) = c.files.get_mut(&key) {
        for i in 0..n as u64 {
            let ours = matches!(fe.pages.get(&(pgoff + i)),
                Some(p) if p.state == ST_FILLING && p.token == token);
            if ours {
                fe.pages.remove(&(pgoff + i));
                FILLING.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

// ── mappings ──────────────────────────────────────────────────────────────────

/// The cached frame for (`key`, `pgoff`), with a new reference taken for
/// the caller's mapping. `exec`: the mapping is executable.
pub fn lookup(key: u64, pgoff: u64, exec: bool) -> Option<usize> {
    if key == 0 { return None; }
    let mut c = CACHE.lock();
    let p = c.files.get_mut(&key)?.pages.get_mut(&pgoff)?;
    if p.state != ST_UPTODATE || p.phys == 0 { return None; }
    if p.flags & F_MAPPED == 0 { MAPPED.fetch_add(1, Ordering::Relaxed); }
    p.flags |= F_REF | F_MAPPED | if exec { F_EXEC } else { 0 };
    let phys = p.phys;
    crate::pageref::inc(phys);
    drop(c);
    bump(&COUNTERS.hits, 1);
    Some(phys)
}

/// A shared mapping is about to write (`key`, `pgoff`): it must be written
/// back. False if the page is not the cached frame `phys` (anymore).
pub fn mark_dirty(key: u64, pgoff: u64, phys: usize) -> bool {
    let mut c = CACHE.lock();
    let fe = match c.files.get_mut(&key) { Some(f) => f, None => return false };
    let p = match fe.pages.get_mut(&pgoff) { Some(p) => p, None => return false };
    if p.state != ST_UPTODATE || p.phys != phys { return false; }
    if p.flags & F_DIRTY == 0 {
        p.flags |= F_DIRTY;
        fe.ndirty += 1;
        DIRTY.fetch_add(1, Ordering::Relaxed);
    }
    true
}

/// A dirty page pinned for write-back (see [`take_dirty`]).
#[derive(Clone, Copy)]
pub struct DirtyPage { pub key: u64, pub pgoff: u64, pub phys: usize }

/// Pin up to `max` dirty pages of the files with keys in `lo..=hi` for
/// write-back. Pages a mapping still holds are included only with `mapped`.
pub fn take_dirty(lo: u64, hi: u64, mapped: bool, max: usize, out: &mut Vec<DirtyPage>) {
    if DIRTY.load(Ordering::Relaxed) == 0 { return; }
    let c = CACHE.lock();
    let mut pins: Vec<usize> = Vec::new();
    for (&key, fe) in c.files.range(lo..=hi) {
        if fe.ndirty == 0 { continue; }
        for (&pg, p) in fe.pages.iter() {
            if out.len() >= max { break; }
            if p.state != ST_UPTODATE || p.phys == 0 || p.flags & F_DIRTY == 0 { continue; }
            if !mapped && crate::pageref::get(p.phys) > 1 { continue; }
            out.push(DirtyPage { key, pgoff: pg, phys: p.phys });
            pins.push(p.phys);
        }
        if out.len() >= max { break; }
    }
    crate::pageref::inc_many(&pins);
}

/// Is `d` still the dirty cached page it was when taken? (Checked by the
/// filesystem under its lock right before it writes the page back: a file
/// freed meanwhile dropped its pages under that same lock.)
pub fn still_dirty(key: u64, pgoff: u64, phys: usize) -> bool {
    let c = CACHE.lock();
    c.files.get(&key).and_then(|f| f.pages.get(&pgoff))
        .map_or(false, |p| p.state == ST_UPTODATE && p.phys == phys && p.flags & F_DIRTY != 0)
}

/// Write-back of `d` finished (`ok`: the filesystem took the data). The
/// page is clean again unless a mapping may still write it. Unpins.
pub fn writeback_done(d: &DirtyPage, ok: bool) {
    {
        let mut c = CACHE.lock();
        if let Some(fe) = c.files.get_mut(&d.key) {
            if let Some(p) = fe.pages.get_mut(&d.pgoff) {
                // Two references: the cache's and this write-back's pin.
                if ok && p.phys == d.phys && p.flags & F_DIRTY != 0
                    && crate::pageref::get(d.phys) <= 2
                {
                    p.flags &= !F_DIRTY;
                    fe.ndirty = fe.ndirty.saturating_sub(1);
                    DIRTY.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
    }
    if ok { bump(&COUNTERS.written_back, 1); }
    crate::pageref::unref_or_free(d.phys, 0);
}

// ── writes ────────────────────────────────────────────────────────────────────

/// A cached page pinned for a write-through copy.
#[derive(Clone, Copy)]
pub struct Pinned { pub pgoff: u64, pub phys: usize, pub exec: bool }

/// A write of `len` bytes at `pos` is being applied (filesystem lock held,
/// final round): pin the cached pages it overlaps for the caller to copy the
/// new bytes into, and doom fills of those pages in flight.
pub fn wt_pin(key: u64, pos: u64, len: usize, out: &mut Vec<Pinned>) {
    if len == 0 { return; }
    let first = pos / PAGE_SIZE as u64;
    let last = (pos + len as u64 - 1) / PAGE_SIZE as u64;
    let mut c = CACHE.lock();
    let fe = match c.files.get_mut(&key) { Some(f) => f, None => return };
    fe.atime_ok = 0;
    let mut pins: Vec<usize> = Vec::new();
    for (&pg, p) in fe.pages.range_mut(first..=last) {
        if p.state == ST_FILLING { p.flags |= F_STALE; continue; }
        if p.phys == 0 { continue; }
        out.push(Pinned { pgoff: pg, phys: p.phys, exec: p.flags & F_EXEC != 0 });
        pins.push(p.phys);
    }
    crate::pageref::inc_many(&pins);
}

/// The caller copied the new bytes of [`wt_pin`]'s range into the pinned
/// frames: make them visible to instruction fetch where needed, unpin.
pub fn wt_done(pos: u64, len: usize, pins: &[Pinned]) {
    for p in pins {
        if p.exec {
            let pstart = p.pgoff * PAGE_SIZE as u64;
            let lo = pos.max(pstart);
            let hi = (pos + len as u64).min(pstart + PAGE_SIZE as u64);
            if hi > lo { sync_icache(p.phys, (lo - pstart) as usize, (hi - lo) as usize); }
        }
        crate::pageref::unref_or_free(p.phys, 0);
    }
    bump(&COUNTERS.wrote_through, pins.len());
}

/// Zero `[from, upto)` of the cached page holding byte `from` (bytes past an
/// old end of file that a write or truncate now brings inside it). A fill
/// of that page in flight is doomed.
pub fn extend(key: u64, from: u64, upto: u64) {
    if upto <= from { return; }
    let pg = from / PAGE_SIZE as u64;
    let off = (from % PAGE_SIZE as u64) as usize;
    if off == 0 { return; } // the old end was page-aligned: nothing cached past it
    let end = ((upto - pg * PAGE_SIZE as u64) as usize).min(PAGE_SIZE);
    let mut c = CACHE.lock();
    let fe = match c.files.get_mut(&key) { Some(f) => f, None => return };
    fe.atime_ok = 0;
    if let Some(p) = fe.pages.get_mut(&pg) {
        if p.state == ST_FILLING { p.flags |= F_STALE; return; }
        if p.phys == 0 || end <= off { return; }
        unsafe { core::ptr::write_bytes((crate::phys_to_virt(p.phys) + off) as *mut u8, 0, end - off); }
        if p.flags & F_EXEC != 0 { sync_icache(p.phys, off, end - off); }
    }
}

/// The file was truncated to `size`: pages wholly past it go (dirty or not;
/// a mapping keeps its frame, detached), the tail of the new last page is
/// zeroed, fills of affected pages in flight are doomed.
pub fn truncate(key: u64, size: u64) {
    let gone = {
        let mut c = CACHE.lock();
        let fe = match c.files.get_mut(&key) { Some(f) => f, None => return };
        if fe.size_known { fe.size = size; }
        fe.atime_ok = 0;
        let first_gone = (size + PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64;
        let doomed: Vec<u64> = fe.pages.range(first_gone..).map(|(k, _)| *k).collect();
        let mut v: Vec<usize> = Vec::new();
        for pg in doomed {
            let p = *fe.pages.get(&pg).unwrap();
            if p.state == ST_FILLING {
                fe.pages.get_mut(&pg).unwrap().flags |= F_STALE;
                continue;
            }
            fe.pages.remove(&pg);
            note_gone(&p, fe);
            if p.phys != 0 { v.push(p.phys); }
        }
        let off = (size % PAGE_SIZE as u64) as usize;
        if off != 0 {
            if let Some(p) = fe.pages.get_mut(&(size / PAGE_SIZE as u64)) {
                if p.state == ST_FILLING {
                    p.flags |= F_STALE;
                } else if p.phys != 0 {
                    unsafe {
                        core::ptr::write_bytes((crate::phys_to_virt(p.phys) + off) as *mut u8, 0,
                                               PAGE_SIZE - off);
                    }
                    if p.flags & F_EXEC != 0 { sync_icache(p.phys, off, PAGE_SIZE - off); }
                }
            }
        }
        v
    };
    for phys in gone { crate::pageref::unref_or_free(phys, 0); }
}

// ── reclaim ───────────────────────────────────────────────────────────────────

/// Before a fill of `n` pages: keep the cache under its cap and free memory
/// above the low watermark, evicting as needed. False if the fill should be
/// served uncached instead (`over_cap_ok`: a mapping's fault, which must be
/// cached to share its frames, may exceed the cap and the hard floor).
pub fn make_room(n: usize, over_cap_ok: bool) -> bool {
    let cap = cap_pages();
    let cached = CACHED.load(Ordering::Relaxed) + FILLING.load(Ordering::Relaxed);
    if cached + n > cap {
        reclaim_pages(cached + n - cap + 64);
    }
    let (low, high) = watermarks();
    let free = crate::buddy::free_pages();
    if free < low + n {
        reclaim_pages((high + n).saturating_sub(free));
    }
    // A fault must be cached (a shared mapping maps the cached frame
    // itself); only a failed frame allocation stops it.
    if over_cap_ok { return true; }
    let free = crate::buddy::free_pages();
    if free < hard_floor() + n { return false; }
    CACHED.load(Ordering::Relaxed) + FILLING.load(Ordering::Relaxed) + n <= cap
}

/// Background/opportunistic: free memory under the low watermark or the
/// cache over its cap → evict back to the high watermark / the cap. Cheap
/// when neither holds (two atomic loads).
pub fn maybe_reclaim() {
    let (low, high) = watermarks();
    let cached = CACHED.load(Ordering::Relaxed);
    if cached == 0 { return; }
    let free = crate::buddy::free_pages();
    let cap = cap_pages();
    let mut want = 0;
    if free < low { want = high - free; }
    if cached > cap { want = want.max(cached - cap); }
    if want != 0 { reclaim_pages(want); }
}

/// Clock eviction of up to `want` pages. Normal context only (it edits the
/// index, which allocates and frees heap memory). Returns pages freed.
pub fn reclaim_pages(want: usize) -> usize {
    const BATCH: usize = 512;
    let mut freed = 0usize;
    // Scan budget: every entry twice (a referenced page gets one pass to
    // lose its bit, the next to go).
    let mut budget = 2 * (CACHED.load(Ordering::Relaxed) + FILLING.load(Ordering::Relaxed)) + BATCH;
    let mut victims: Vec<(u64, u64)> = Vec::with_capacity(BATCH);
    let mut frames: Vec<usize> = Vec::with_capacity(BATCH);
    while freed < want && budget > 0 {
        victims.clear();
        frames.clear();
        let mut scanned = 0usize;
        {
            let mut c = CACHE.lock();
            if c.files.is_empty() { break; }
            let (mut hk, mut hp) = c.hand;
            let need = (want - freed).min(BATCH);
            let mut wraps = 0;
            loop {
                let k = match c.files.range(hk..).next() {
                    Some((k, _)) => *k,
                    None => {
                        wraps += 1;
                        if wraps > 1 { break; }
                        hk = 0; hp = 0;
                        continue;
                    }
                };
                if k != hk { hk = k; hp = 0; }
                let fe = c.files.get_mut(&k).unwrap();
                let orphan = fe.orphan;
                let mut stopped = None;
                for (&pg, p) in fe.pages.range_mut(hp..) {
                    if scanned >= BATCH || victims.len() >= need { stopped = Some(pg); break; }
                    scanned += 1;
                    if p.state != ST_UPTODATE { continue; }
                    if p.phys == 0 { victims.push((k, pg)); continue; }
                    if p.flags & F_DIRTY != 0 { continue; }
                    if crate::pageref::get(p.phys) > 1 {
                        if p.flags & F_MAPPED == 0 {
                            p.flags |= F_MAPPED;
                            MAPPED.fetch_add(1, Ordering::Relaxed);
                        }
                        continue;
                    }
                    if p.flags & F_MAPPED != 0 {
                        p.flags &= !F_MAPPED;
                        MAPPED.fetch_sub(1, Ordering::Relaxed);
                    }
                    if p.flags & F_REF != 0 && !orphan {
                        p.flags &= !F_REF;
                        continue;
                    }
                    victims.push((k, pg));
                }
                if let Some(pg) = stopped { hp = pg; break; }
                // This file is done: on to the next key.
                match k.checked_add(1) {
                    Some(n) => { hk = n; hp = 0; }
                    None => { hk = 0; hp = 0; wraps += 1; if wraps > 1 { break; } }
                }
                if scanned >= BATCH || victims.len() >= need { break; }
            }
            c.hand = (hk, hp);
            for &(k, pg) in victims.iter() {
                let fe = match c.files.get_mut(&k) { Some(f) => f, None => continue };
                if let Some(p) = fe.pages.remove(&pg) {
                    if p.phys != 0 {
                        CACHED.fetch_sub(1, Ordering::Relaxed);
                        frames.push(p.phys);
                    }
                }
                if fe.pages.is_empty() && fe.users == 0 { c.files.remove(&k); }
            }
        }
        budget = budget.saturating_sub(scanned.max(1));
        for &phys in frames.iter() { crate::buddy::free(phys, 0); }
        freed += frames.len();
        bump(&COUNTERS.evicted, frames.len());
        if scanned == 0 { break; }
    }
    freed
}

/// Recursion guard for [`emergency_reclaim`].
static IN_EMERGENCY: AtomicBool = AtomicBool::new(false);
static EMERGENCY_HAND: AtomicU32 = AtomicU32::new(0);

/// The buddy allocator found no block: free up to `want` clean, unmapped
/// pages right now. Callable from any context with any lock held: it only
/// try-locks the cache and the refcount table, allocates nothing and frees
/// no heap memory (an evicted entry keeps its slot with `phys = 0` until
/// the background task purges it). Returns pages freed.
pub fn emergency_reclaim(want: usize) -> usize {
    extern "C" {
        fn arch_interrupt_save() -> usize;
        fn arch_interrupt_restore(flags: usize);
    }
    if CACHED.load(Ordering::Relaxed) == 0 { return 0; }
    if IN_EMERGENCY.swap(true, Ordering::Acquire) { return 0; }
    // Never be preempted holding the cache lock (an allocation may come
    // from a context that runs with interrupts on).
    let irq = unsafe { arch_interrupt_save() };
    let mut freed = 0usize;
    if let Some(mut c) = CACHE.try_lock() {
        // Start at a rotating file so repeated emergencies spread out.
        let skip = EMERGENCY_HAND.fetch_add(1, Ordering::Relaxed) as usize % c.files.len().max(1);
        let n_files = c.files.len();
        let mut scanned = 0usize;
        // Sweep 0 spares pages referenced since the clock last passed (a
        // fill just published, about to be mapped by the fault that read
        // it); sweep 1 takes any clean unmapped page. Each sweep starts at
        // the rotating file and wraps.
        for pass in 0..4 {
            let spare_ref = pass < 2;
            let first_half = pass % 2 == 0;
            for (_, fe) in c.files.iter_mut().skip(if first_half { skip } else { 0 })
                .take(if first_half { n_files } else { skip })
            {
                for (_, p) in fe.pages.iter_mut() {
                    if freed >= want || scanned >= 1 << 18 { break; }
                    scanned += 1;
                    if p.state != ST_UPTODATE || p.phys == 0 || p.flags & F_DIRTY != 0 { continue; }
                    if spare_ref && p.flags & F_REF != 0 { continue; }
                    match crate::pageref::try_get(p.phys) {
                        Some(1) => {}
                        _ => continue,
                    }
                    if p.flags & F_MAPPED != 0 {
                        p.flags &= !F_MAPPED;
                        MAPPED.fetch_sub(1, Ordering::Relaxed);
                    }
                    let phys = p.phys;
                    p.phys = 0;
                    CACHED.fetch_sub(1, Ordering::Relaxed);
                    crate::buddy::free(phys, 0);
                    freed += 1;
                }
                if freed >= want { break; }
            }
            if freed >= want { break; }
        }
    }
    unsafe { arch_interrupt_restore(irq); }
    IN_EMERGENCY.store(false, Ordering::Release);
    bump(&COUNTERS.emergency, freed);
    bump(&HOLES, freed);
    freed
}

/// Entries the emergency path emptied (`phys = 0`) and `purge` has not
/// removed yet.
static HOLES: AtomicUsize = AtomicUsize::new(0);

/// Background housekeeping: drop the entries the emergency path emptied.
pub fn purge() {
    if HOLES.swap(0, Ordering::Relaxed) == 0 { return; }
    let mut c = CACHE.lock();
    let mut keys: Vec<(u64, u64)> = Vec::new();
    for (&k, fe) in c.files.iter() {
        for (&pg, p) in fe.pages.iter() {
            if p.state == ST_UPTODATE && p.phys == 0 { keys.push((k, pg)); }
        }
    }
    for (k, pg) in keys {
        if let Some(fe) = c.files.get_mut(&k) { fe.pages.remove(&pg); }
    }
}
