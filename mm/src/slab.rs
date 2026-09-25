//! Slab allocator — fixed-size object caches backed by the buddy allocator.
//!
//! Design: one `Cache` per size class (8..=4096, power-of-two).  Each cache
//! maintains a singly-linked free list threaded through the first word of
//! every free slot.  When a cache is exhausted a full buddy page is split
//! into slots and pushed onto the list.
//!
//! For requests larger than PAGE_SIZE the buddy allocator is used directly.
//!
//! Analogues: Linux SLUB (mm/slub.c).

use spin::Mutex;
use crate::buddy;

const PAGE_SIZE: usize = buddy::PAGE_SIZE;

// ── Size classes ──────────────────────────────────────────────────────────────

const SIZE_CLASSES: [usize; 10] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
const NUM_CLASSES:  usize = SIZE_CLASSES.len();

// ── Per-class cache ───────────────────────────────────────────────────────────

struct Cache {
    obj_size:  usize,
    /// Physical address of the first free slot (linked list), or 0.
    /// The first `usize`-word of every free slot stores the next pointer.
    free_head: usize,
}

impl Cache {
    const fn new(obj_size: usize) -> Self {
        Self { obj_size, free_head: 0 }
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
        let n = PAGE_SIZE / self.obj_size;
        let mut addr = virt;
        for _ in 0..n {
            unsafe { (addr as *mut usize).write(self.free_head); }
            self.free_head = addr;
            addr += self.obj_size;
        }
        true
    }

    fn alloc(&mut self) -> Option<*mut u8> {
        if self.free_head == 0 && !self.refill() {
            return None;
        }
        let slot = self.free_head as *mut u8;
        let next = unsafe { (self.free_head as *const usize).read() };
        self.free_head = next;
        Some(slot)
    }

    fn free(&mut self, ptr: *mut u8) {
        unsafe { (ptr as *mut usize).write(self.free_head); }
        self.free_head = ptr as usize;
    }
}

// ── Global cache table ────────────────────────────────────────────────────────

struct CacheTable([Cache; NUM_CLASSES]);
unsafe impl Send for CacheTable {}
unsafe impl Sync for CacheTable {}

static CACHES: Mutex<CacheTable> = Mutex::new(CacheTable([
    Cache::new(8),    Cache::new(16),   Cache::new(32),   Cache::new(64),
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
// by page count up to 1024 pages, the rest lumped), plus pages each class has
// pulled from the buddy. Caches never give pages back, so `CLASS_PAGES` is the
// high-water mark of that class; a leak shows as `LIVE_BY_SIZE` growing at
// one size across identical workloads, which names the type far better than
// a class does. Read by `/proc/kmemstat`.

use core::sync::atomic::{AtomicUsize, AtomicIsize, Ordering};
const SMALL_BUCKETS: usize = 4096 / 8 + 1;
const LARGE_BUCKETS: usize = 1025;
static LIVE_BY_SIZE: [AtomicIsize; SMALL_BUCKETS] = [const { AtomicIsize::new(0) }; SMALL_BUCKETS];
static LIVE_BY_PAGES: [AtomicIsize; LARGE_BUCKETS] = [const { AtomicIsize::new(0) }; LARGE_BUCKETS];
static CLASS_PAGES: [AtomicUsize; NUM_CLASSES] = [const { AtomicUsize::new(0) }; NUM_CLASSES];
static CLASS_LIVE: [AtomicIsize; NUM_CLASSES] = [const { AtomicIsize::new(0) }; NUM_CLASSES];

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

/// No-op — caches are lazily filled on first allocation.
pub fn init() {}

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
