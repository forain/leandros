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
