//! Slab allocator — fixed-size object caches backed by the buddy allocator.
//!
//! Design: one `Cache` per size class (16..=4096, power-of-two).  Each cache
//! keeps a doubly-linked free list threaded through its free slots (next at
//! word 0, prev at word 1 — hence no 8-byte class: 8-byte requests use the
//! 16-byte class) and a per-page count of live objects in a side table
//! indexed by page frame number. When a cache is exhausted a buddy page is
//! split into slots and pushed onto the list; when a page's last object is
//! freed and the class already holds `EMPTY_RESERVE` empty pages, the page's
//! slots are unlinked (O(1) each, thanks to the back links) and the page goes
//! back to the buddy allocator. Until 2026-09-26 no page ever went back, so
//! every burst of kernel allocations (a greeter chain's death: ~1.5 pages)
//! stayed charged to the slab for the rest of the boot.
//!
//! For requests larger than PAGE_SIZE the buddy allocator is used directly.
//!
//! Analogues: Linux SLUB (mm/slub.c) — its per-slab `inuse` and
//! `min_partial` reserve.

use spin::Mutex;
use crate::buddy;

const PAGE_SIZE: usize = buddy::PAGE_SIZE;

// ── Size classes ──────────────────────────────────────────────────────────────

const SIZE_CLASSES: [usize; 9] = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
const NUM_CLASSES:  usize = SIZE_CLASSES.len();

/// Empty pages each class keeps instead of returning them, so an alloc/free
/// ping-pong at a page boundary does not hit the buddy allocator every time.
const EMPTY_RESERVE: usize = 4;

// ── Per-page live-object counts ───────────────────────────────────────────────
//
// One u16 per physical page (from address 0 up to the buddy allocator's
// PHYS_END), set up by `init`: 2 bytes per 4 KiB page, 0.05 % of RAM. Only
// slab pages' entries are ever non-zero. Accessed only with `CACHES` held.
static INUSE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static INUSE_PAGES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The live-object counter of the page holding HHDM address `virt`, or null
/// if there is no table (then nothing is ever reclaimed).
#[inline]
fn inuse_slot(virt: usize) -> *mut u16 {
    let base = INUSE.load(Ordering::Relaxed);
    if base == 0 { return core::ptr::null_mut(); }
    let pfn = crate::virt_to_phys(virt) / PAGE_SIZE;
    if pfn >= INUSE_PAGES.load(Ordering::Relaxed) { return core::ptr::null_mut(); }
    (base + pfn * 2) as *mut u16
}

// ── Per-class cache ───────────────────────────────────────────────────────────

struct Cache {
    obj_size:  usize,
    /// HHDM address of the first free slot, or 0.
    free_head: usize,
    /// Pages of this class with no live object (all slots on the list).
    empty_pages: usize,
}

#[inline] unsafe fn next_of(slot: usize) -> usize { *(slot as *const usize) }
#[inline] unsafe fn set_next(slot: usize, v: usize) { *(slot as *mut usize) = v; }
#[inline] unsafe fn set_prev(slot: usize, v: usize) { *((slot + 8) as *mut usize) = v; }
#[inline] unsafe fn prev_of(slot: usize) -> usize { *((slot + 8) as *const usize) }

impl Cache {
    const fn new(obj_size: usize) -> Self {
        Self { obj_size, free_head: 0, empty_pages: 0 }
    }

    unsafe fn push(&mut self, slot: usize) {
        set_next(slot, self.free_head);
        set_prev(slot, 0);
        if self.free_head != 0 { set_prev(self.free_head, slot); }
        self.free_head = slot;
    }

    unsafe fn unlink(&mut self, slot: usize) {
        let n = next_of(slot);
        let p = prev_of(slot);
        if p != 0 { set_next(p, n); } else { self.free_head = n; }
        if n != 0 { set_prev(n, p); }
    }

    /// Slice a freshly-allocated buddy page into `obj_size` chunks and push
    /// them onto the free list.  Returns `false` on OOM.
    fn refill(&mut self) -> bool {
        let phys = match buddy::alloc(0) {
            Some(p) => p,
            None    => return false,
        };
        if let Some(i) = size_class_idx(self.obj_size) {
            CLASS_PAGES[i].fetch_add(1, Ordering::Relaxed);
        }
        let virt = crate::phys_to_virt(phys);
        let c = inuse_slot(virt);
        if !c.is_null() { unsafe { *c = 0; } }
        self.empty_pages += 1;
        let n = PAGE_SIZE / self.obj_size;
        // Pushed highest-first so the list hands the page out in address order.
        for k in (0..n).rev() {
            unsafe { self.push(virt + k * self.obj_size); }
        }
        true
    }

    fn alloc(&mut self) -> Option<*mut u8> {
        if self.free_head == 0 && !self.refill() {
            return None;
        }
        let slot = self.free_head;
        unsafe { self.unlink(slot); }
        let c = inuse_slot(slot);
        if !c.is_null() {
            unsafe {
                if *c == 0 { self.empty_pages = self.empty_pages.saturating_sub(1); }
                *c += 1;
            }
        }
        Some(slot as *mut u8)
    }

    fn free(&mut self, ptr: *mut u8) {
        let slot = ptr as usize;
        unsafe { self.push(slot); }
        let c = inuse_slot(slot);
        if c.is_null() { return; }
        unsafe {
            if *c == 0 { return; } // not a counted page: never reclaim it
            *c -= 1;
            if *c != 0 { return; }
        }
        self.empty_pages += 1;
        if self.empty_pages <= EMPTY_RESERVE { return; }
        // Past the reserve: every slot of this page is on the list now.
        // Unlink them all and give the page back.
        let page = slot & !(PAGE_SIZE - 1);
        let n = PAGE_SIZE / self.obj_size;
        for k in 0..n {
            unsafe { self.unlink(page + k * self.obj_size); }
        }
        self.empty_pages -= 1;
        if let Some(i) = size_class_idx(self.obj_size) {
            CLASS_PAGES[i].fetch_sub(1, Ordering::Relaxed);
        }
        RECLAIMED_PAGES.fetch_add(1, Ordering::Relaxed);
        buddy::free(crate::virt_to_phys(page), 0);
    }
}

// ── Global cache table ────────────────────────────────────────────────────────

struct CacheTable([Cache; NUM_CLASSES]);
unsafe impl Send for CacheTable {}
unsafe impl Sync for CacheTable {}

static CACHES: Mutex<CacheTable> = Mutex::new(CacheTable([
    Cache::new(16),   Cache::new(32),   Cache::new(64),
    Cache::new(128),  Cache::new(256),  Cache::new(512),  Cache::new(1024),
    Cache::new(2048), Cache::new(4096),
]));

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Index into `SIZE_CLASSES` for the smallest class that fits `size`.
fn size_class_idx(size: usize) -> Option<usize> {
    SIZE_CLASSES.iter().position(|&s| size <= s)
}

/// Compute the buddy order needed to cover `pages` pages.
///
/// Returns `None` when `pages` exceeds the maximum buddy allocation
/// (`2^(MAX_ORDER-1)` pages = 4 MiB).  Callers that ignore this and
/// proceed would get a silently under-sized allocation — a buffer overrun
/// waiting to happen.
fn pages_to_order(pages: usize) -> Option<usize> {
    let max_pages = 1usize << (buddy::MAX_ORDER - 1);
    if pages > max_pages { return None; }
    let mut order = 0;
    let mut cap   = 1usize;
    while cap < pages { cap <<= 1; order += 1; }
    Some(order)
}

// ── Accounting ────────────────────────────────────────────────────────────────
//
// Live objects per exact size (8-byte granularity up to 4096; larger requests
// by page count up to 1024 pages, the rest lumped), plus pages each class
// currently owns (live objects + free slots + up to EMPTY_RESERVE empty
// pages) and the pages returned to the buddy so far. A leak shows as
// `LIVE_BY_SIZE` growing at one size across identical workloads, which names
// the type far better than a class does. Read by `/proc/kmemstat`.

use core::sync::atomic::{AtomicUsize, AtomicIsize, Ordering};
const SMALL_BUCKETS: usize = 4096 / 8 + 1;
const LARGE_BUCKETS: usize = 1025;
static LIVE_BY_SIZE: [AtomicIsize; SMALL_BUCKETS] = [const { AtomicIsize::new(0) }; SMALL_BUCKETS];
static LIVE_BY_PAGES: [AtomicIsize; LARGE_BUCKETS] = [const { AtomicIsize::new(0) }; LARGE_BUCKETS];
static CLASS_PAGES: [AtomicUsize; NUM_CLASSES] = [const { AtomicUsize::new(0) }; NUM_CLASSES];
static CLASS_LIVE: [AtomicIsize; NUM_CLASSES] = [const { AtomicIsize::new(0) }; NUM_CLASSES];
static RECLAIMED_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Empty slab pages given back to the buddy allocator since boot.
pub fn reclaimed_pages() -> usize { RECLAIMED_PAGES.load(Ordering::Relaxed) }

fn account(size: usize, delta: isize) {
    if size <= 4096 {
        LIVE_BY_SIZE[(size + 7) / 8].fetch_add(delta, Ordering::Relaxed);
        if let Some(i) = size_class_idx(size) { CLASS_LIVE[i].fetch_add(delta, Ordering::Relaxed); }
    } else {
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        LIVE_BY_PAGES[pages.min(LARGE_BUCKETS - 1)].fetch_add(delta, Ordering::Relaxed);
    }
}

/// `emit(class_size, pages_owned, live_objects)` per size class.
pub fn class_census(emit: &mut dyn FnMut(usize, usize, isize)) {
    for i in 0..NUM_CLASSES {
        emit(SIZE_CLASSES[i], CLASS_PAGES[i].load(Ordering::Relaxed), CLASS_LIVE[i].load(Ordering::Relaxed));
    }
}

/// `emit(size_upper_bytes, live_objects)` for every exact-size bucket with a
/// live object; sizes > 4096 are reported as `pages * 4096` (the last bucket
/// is "1024 pages or more").
pub fn size_census(emit: &mut dyn FnMut(usize, isize)) {
    for i in 1..SMALL_BUCKETS {
        let n = LIVE_BY_SIZE[i].load(Ordering::Relaxed);
        if n != 0 { emit(i * 8, n); }
    }
    for p in 1..LARGE_BUCKETS {
        let n = LIVE_BY_PAGES[p].load(Ordering::Relaxed);
        if n != 0 { emit(p * PAGE_SIZE, n); }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Set up the per-page live-object table (caches fill lazily on first
/// allocation). Called once from `mm::init_with_map`, after the buddy
/// allocator and before the first heap allocation.
pub fn init() {
    let n = buddy::phys_end() / PAGE_SIZE;
    if n == 0 { return; }
    let pages = (n * 2 + PAGE_SIZE - 1) / PAGE_SIZE;
    let mut order = 0; while (1usize << order) < pages { order += 1; }
    let phys = match buddy::alloc(order) { Some(p) => p, None => return };
    let virt = crate::phys_to_virt(phys);
    unsafe { core::ptr::write_bytes(virt as *mut u8, 0, n * 2); }
    INUSE_PAGES.store(n, Ordering::Relaxed);
    INUSE.store(virt, Ordering::Release);
}

/// Allocate `size` bytes.  Returns `None` on OOM.
///
/// For `size == 0` a dangling non-null pointer is returned (matches the
/// Rust allocator contract for zero-sized types).
pub fn alloc(size: usize) -> Option<*mut u8> {
    if size == 0 {
        return Some(core::ptr::NonNull::dangling().as_ptr());
    }
    let r = alloc_inner(size);
    if r.is_some() { account(size, 1); }
    r
}

fn alloc_inner(size: usize) -> Option<*mut u8> {
    match size_class_idx(size) {
        Some(idx) => CACHES.lock().0[idx].alloc(),
        None => {
            // Larger than the biggest class: round up to pages, use buddy.
            let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
            let order = pages_to_order(pages)?; // None → OOM (too large)
            buddy::alloc(order).map(|p| crate::phys_to_virt(p) as *mut u8)
        }
    }
}

/// Return `ptr` to its slab cache.
///
/// # Safety
/// `ptr` must have been returned by `slab::alloc` with the same `size`.
pub unsafe fn free(ptr: *mut u8, size: usize) {
    if size == 0 || ptr.is_null() { return; }
    account(size, -1);
    match size_class_idx(size) {
        Some(idx) => CACHES.lock().0[idx].free(ptr),
        None => {
            let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
            if let Some(order) = pages_to_order(pages) {
                buddy::free(crate::virt_to_phys(ptr as usize), order);
            }
        }
    }
}

// ── Global Allocator Interface ───────────────────────────────────────────────

pub struct SlabAllocator;

unsafe impl core::alloc::GlobalAlloc for SlabAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        alloc(layout.size()).unwrap_or(core::ptr::null_mut())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        free(ptr, layout.size())
    }
}
