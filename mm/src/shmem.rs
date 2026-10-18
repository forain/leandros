//! Shared anonymous memory objects — the kernel's stand-in for the Linux
//! shmem file behind `mmap(MAP_SHARED | MAP_ANONYMOUS)` and `MAP_SHARED` of
//! `/dev/zero`.
//!
//! A shared anonymous mapping must stay ONE memory across `fork()`: a page
//! either side touches for the first time *after* the fork has to be the page
//! the other side sees. Per-VMA demand paging cannot give that — each address
//! space would fault its own zero page — so every such VMA names a [`ShmObj`]
//! (an `Arc`, cloned by fork and by VMA splits) and resolves absent pages
//! through it: the object allocates a page's frame once, and every mapping of
//! that page, in every process, maps the same frame.
//!
//! Ownership is plain `pageref` counting. The object holds one reference per
//! frame it has allocated; each VMA that maps the frame holds one more (taken
//! by [`ShmObj::get_or_alloc`], released by munmap / exit / MADV_DONTNEED
//! exactly like any other lazy page). A frame is freed when the last of those
//! goes: the object's own references go when the last VMA naming the object
//! drops its `Arc` (munmap of the last mapping, exit or execve of the last
//! process). Until then the object keeps every page it ever allocated, even
//! ones no VMA maps any more — Linux's shmem does the same, which is what
//! makes `MADV_DONTNEED` on shared anonymous memory a zap-and-refault that
//! returns the same data rather than zeros.
//!
//! Lock order: address-space `busy` → object lock → `pageref` table.

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

use crate::buddy::PAGE_SIZE;

/// Live objects, and frames they hold (`Shmem:` in `/proc/meminfo`).
static LIVE_OBJS: AtomicUsize = AtomicUsize::new(0);
static LIVE_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Live shared anonymous objects.
pub fn objects() -> usize { LIVE_OBJS.load(Ordering::Relaxed) }
/// Frames held by live shared anonymous objects.
pub fn pages() -> usize { LIVE_PAGES.load(Ordering::Relaxed) }

/// One shared anonymous memory object: page offset → frame. Sparse, so a
/// large reservation that is mostly untouched costs nothing.
pub struct ShmObj {
    pages: Mutex<BTreeMap<usize, usize>>,
}

impl ShmObj {
    pub fn new() -> Arc<ShmObj> {
        LIVE_OBJS.fetch_add(1, Ordering::Relaxed);
        Arc::new(ShmObj { pages: Mutex::new(BTreeMap::new()) })
    }

    /// The frame of page `pgoff`, allocated (zeroed) on first use, with one
    /// new reference taken for the caller's mapping. None on out-of-memory.
    pub fn get_or_alloc(&self, pgoff: usize) -> Option<usize> {
        let mut pages = self.pages.lock();
        let phys = match pages.get(&pgoff) {
            Some(&f) => f,
            None => {
                let f = crate::buddy::alloc(0)?;
                unsafe { (crate::phys_to_virt(f) as *mut u8).write_bytes(0, PAGE_SIZE); }
                pages.insert(pgoff, f);
                LIVE_PAGES.fetch_add(1, Ordering::Relaxed);
                f
            }
        };
        // Under the object lock, so the reference is in place before any
        // other mapper can observe the frame.
        crate::pageref::inc(phys);
        Some(phys)
    }

    /// Frames the object holds.
    pub fn resident(&self) -> usize { self.pages.lock().len() }
}

impl Drop for ShmObj {
    fn drop(&mut self) {
        let pages = core::mem::take(&mut *self.pages.lock());
        LIVE_PAGES.fetch_sub(pages.len(), Ordering::Relaxed);
        LIVE_OBJS.fetch_sub(1, Ordering::Relaxed);
        for (_, phys) in pages {
            crate::pageref::unref_or_free(phys, 0);
        }
    }
}
