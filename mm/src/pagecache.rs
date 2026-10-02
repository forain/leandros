//! Shared page cache for clean file pages of private file mappings.
//!
//! Until 2026-10-02 every file-backed fault read the file into a fresh frame
//! of the faulting process, so a library page touched by ten processes was
//! ten frames. On a 2 GiB guest that was 471 MiB of duplicated library pages
//! with only the COSMIC desktop up, and 987 MiB once Firefox (seven processes
//! mapping the 143 MB libxul.so) started, which is what pushed MemAvailable
//! under init's memory-pressure floor (lane-ffmem note).
//!
//! Now a page that holds nothing but file data is read once, kept here keyed
//! by (file key, page offset in the file), and mapped read-only into every
//! private mapping that faults on it. A write to it (a private mapping's
//! first store, or a fork sibling's) goes through the copy-on-write path in
//! `vmm.rs`, which copies it because the frame has more than one owner.
//!
//! OWNERSHIP. A cached frame carries one `pageref` reference for the cache
//! itself plus one per mapping. The cache's reference goes away when
//!   * the file's last mapping registry entry goes (`key_put` to zero): the
//!     cache never outlives the mappings it serves, so memory never exceeds
//!     what the per-process copies would have cost;
//!   * the file's data changes (`invalidate`: write, truncate, fallocate,
//!     inode freed) — existing mappings keep the frame they have, as they
//!     kept their private copy before; later faults read the new data;
//!   * memory runs low (`maybe_reclaim`): frames only the cache still holds
//!     (no mapping) are freed.
//!
//! A fault reads the file with the address space unlocked, so a write can
//! land between the read and the install. `generation(key)` is sampled
//! before the read and `insert` refuses stale data when an invalidation of
//! that key (or of one hashing to the same counter) happened meanwhile; the
//! faulting process still maps what it read, exactly as before the cache.
//!
//! Lock order: `CACHE` → `pageref` → buddy/slab. Nothing here is taken while
//! one of those is held.

extern crate alloc;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;
use crate::buddy::PAGE_SIZE;

struct Cache {
    /// (key, page offset in file) → frame.
    pages: BTreeMap<(u64, u64), usize>,
    /// Live registry entries per key (mapping caps naming that file).
    users: BTreeMap<u64, u32>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { pages: BTreeMap::new(), users: BTreeMap::new() });
static CACHED: AtomicUsize = AtomicUsize::new(0);
static RECLAIMED: AtomicUsize = AtomicUsize::new(0);
static HITS: AtomicUsize = AtomicUsize::new(0);
static FILLS: AtomicUsize = AtomicUsize::new(0);

const GEN_SLOTS: usize = 256;
static GEN: [AtomicU32; GEN_SLOTS] = [const { AtomicU32::new(0) }; GEN_SLOTS];

fn gen_slot(key: u64) -> &'static AtomicU32 {
    let h = key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56;
    &GEN[h as usize % GEN_SLOTS]
}

/// Frames currently held by the cache.
pub fn cached_pages() -> usize { CACHED.load(Ordering::Relaxed) }
/// (hits, fills, reclaimed) since boot, for `/proc/kmemstat`.
pub fn stats() -> (usize, usize, usize) {
    (HITS.load(Ordering::Relaxed), FILLS.load(Ordering::Relaxed), RECLAIMED.load(Ordering::Relaxed))
}

/// Invalidation counter for `key`; sample it before reading the file.
pub fn generation(key: u64) -> u32 { gen_slot(key).load(Ordering::Acquire) }

/// A mapping registry entry for `key` was created.
pub fn key_get(key: u64) {
    if key == 0 { return; }
    *CACHE.lock().users.entry(key).or_insert(0) += 1;
}

/// A mapping registry entry for `key` went away; the last one drops the
/// file's cached frames.
pub fn key_put(key: u64) {
    if key == 0 { return; }
    let mut c = CACHE.lock();
    let last = match c.users.get_mut(&key) {
        Some(n) if *n > 1 => { *n -= 1; false }
        Some(_) => { c.users.remove(&key); true }
        None => false,
    };
    if last { drop_key_locked(&mut c, key); }
}

/// The file's data changed: forget every cached frame of it.
///
/// A file nobody maps has nothing cached and no fill in flight (a fill needs
/// a live registry entry), so that case is one map lookup. The caller holds
/// the filesystem lock its reads also take, so a fault's read is wholly
/// before or wholly after the change.
pub fn invalidate(key: u64) {
    if key == 0 { return; }
    let mut c = CACHE.lock();
    if !c.users.contains_key(&key) { return; }
    gen_slot(key).fetch_add(1, Ordering::AcqRel);
    drop_key_locked(&mut c, key);
}

fn drop_key_locked(c: &mut Cache, key: u64) {
    let doomed: alloc::vec::Vec<(u64, u64)> =
        c.pages.range((key, 0)..=(key, u64::MAX)).map(|(k, _)| *k).collect();
    for k in doomed {
        if let Some(phys) = c.pages.remove(&k) {
            CACHED.fetch_sub(1, Ordering::Relaxed);
            crate::pageref::unref_or_free(phys, 0);
        }
    }
}

/// The cached frame for (`key`, `pgoff`), with a new reference taken for
/// the caller's mapping.
pub fn lookup(key: u64, pgoff: u64) -> Option<usize> {
    if key == 0 { return None; }
    let c = CACHE.lock();
    let phys = *c.pages.get(&(key, pgoff))?;
    crate::pageref::inc(phys);
    HITS.fetch_add(1, Ordering::Relaxed);
    Some(phys)
}

/// Offer the freshly read, caller-owned frame `phys` (one reference, the
/// caller's mapping) for (`key`, `pgoff`). Returns the frame to map:
///   * `phys` itself, now also held by the cache (a second reference); or
///   * `phys` itself, not cached (the key has no live entry, or the data may
///     be stale — `gen` no longer matches); or
///   * a frame cached meanwhile by a concurrent fault, with a reference for
///     the caller; `phys` was freed.
pub fn insert(key: u64, pgoff: u64, phys: usize, gen: u32) -> usize {
    if key == 0 { return phys; }
    let mut c = CACHE.lock();
    if let Some(&have) = c.pages.get(&(key, pgoff)) {
        crate::pageref::inc(have);
        drop(c);
        crate::buddy::free(phys, 0);
        HITS.fetch_add(1, Ordering::Relaxed);
        return have;
    }
    if !c.users.contains_key(&key) || generation(key) != gen { return phys; }
    c.pages.insert((key, pgoff), phys);
    crate::pageref::inc(phys);
    CACHED.fetch_add(1, Ordering::Relaxed);
    FILLS.fetch_add(1, Ordering::Relaxed);
    phys
}

/// Low watermark: below this many free pages the cache gives back frames
/// no mapping holds, up to the high watermark. Both sit above init's
/// memory-pressure floor (RAM/10), so the guard sees the reclaimed memory.
fn watermarks() -> (usize, usize) {
    let total = crate::buddy::total_pages();
    (total / 8, total / 6)
}

/// Free cache-only frames while free memory is under the low watermark.
/// Cheap when it is not (one atomic load). Callers hold no mm lock.
pub fn maybe_reclaim() {
    let (low, high) = watermarks();
    if crate::buddy::free_pages() >= low || CACHED.load(Ordering::Relaxed) == 0 { return; }
    reclaim(high);
}

/// Free frames only the cache holds until `target` pages are free or none
/// are left. Returns how many were freed.
pub fn reclaim(target: usize) -> usize {
    let mut freed = 0usize;
    let mut c = CACHE.lock();
    let mut victims: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
    let mut need = target.saturating_sub(crate::buddy::free_pages());
    for (k, &phys) in c.pages.iter() {
        if need == 0 { break; }
        // One reference: the cache's own; no mapping can gain one without
        // `lookup`, which needs CACHE.
        if crate::pageref::get(phys) <= 1 {
            victims.push(*k);
            need -= 1;
        }
    }
    for k in victims {
        if let Some(phys) = c.pages.remove(&k) {
            CACHED.fetch_sub(1, Ordering::Relaxed);
            crate::buddy::free(phys, 0);
            freed += 1;
        }
    }
    drop(c);
    RECLAIMED.fetch_add(freed, Ordering::Relaxed);
    freed
}

/// Byte offset `off` as a page offset, if page-aligned.
pub fn pgoff_of(off: u64) -> Option<u64> {
    if off % PAGE_SIZE as u64 == 0 { Some(off / PAGE_SIZE as u64) } else { None }
}
