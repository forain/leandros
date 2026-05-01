//! Buddy allocator — Linux-style power-of-two physical page allocator.
//!
//! See: linux/mm/page_alloc.c

use spin::Mutex;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const PAGE_SIZE: usize = 4096;
pub const MAX_ORDER: usize = 11; // 2^10 pages = 4 MiB max contiguous block.

/// Total pages ever freed into the allocator (proxy for physical RAM size).
static TOTAL_PAGES: AtomicUsize = AtomicUsize::new(0);
/// Current free page count (updated on alloc/free).
static FREE_PAGES:  AtomicUsize = AtomicUsize::new(0);

/// Return total pages registered with the buddy allocator.
pub fn total_pages() -> usize { TOTAL_PAGES.load(Ordering::Relaxed) }
/// Return approximate number of free pages.
pub fn free_pages()  -> usize { FREE_PAGES.load(Ordering::Relaxed) }

/// A free list for one order level.
struct FreeList {
    head: Option<usize>, // physical address of first free block
}

impl FreeList {
    const fn empty() -> Self { Self { head: None } }
}

static FREE_LISTS: Mutex<[FreeList; MAX_ORDER]> = Mutex::new([const { FreeList::empty() }; MAX_ORDER]);

/// Initialise the buddy allocator from the boot memory map.
///
/// Each Available region is broken into the largest possible order-aligned
/// blocks and inserted into the free lists — mirroring `free_area_init` in
/// Linux `mm/page_alloc.c`.
pub fn init_from_map(regions: &[boot::MemoryRegion]) {
    for region in regions {
        if region.kind != boot::MemoryType::Available { continue; }
        // Skip the first 2 MiB — reserved for kernel image, page tables, etc.
        let start = leandros_lib::align_up(
            region.base as usize,
            PAGE_SIZE << (MAX_ORDER - 1),
        );
        let end = leandros_lib::align_down(
            (region.base + region.length) as usize,
            PAGE_SIZE,
        );
        if start >= end { continue; }

        // Walk from start to end, releasing the largest aligned block each time.
        let mut addr = start;
        while addr < end {
            let remaining_pages = (end - addr) / PAGE_SIZE;
            let max_order = usize::min(MAX_ORDER - 1,
                (usize::BITS - 1 - remaining_pages.leading_zeros()) as usize);
            // Also constrain by alignment.
            let align_order = (addr / PAGE_SIZE).trailing_zeros() as usize;
            let order = usize::min(max_order, usize::min(align_order, MAX_ORDER - 1));
            free(addr, order);
            addr += PAGE_SIZE << order;
        }
    }
    // Snapshot total = free pages right after init (before any allocations).
    TOTAL_PAGES.store(FREE_PAGES.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// Allocate 2^order contiguous physical pages. Returns physical address or None.
pub fn alloc(order: usize) -> Option<usize> {
    assert!(order < MAX_ORDER);
    let mut lists = FREE_LISTS.lock();
    // Walk up from requested order looking for a free block.
    for o in order..MAX_ORDER {
        if let Some(addr) = lists[o].head.take() {
            // Split excess blocks back down.
            for split in (order..o).rev() {
                let buddy = addr + (PAGE_SIZE << split);
                lists[split].head = Some(buddy);
            }
            FREE_PAGES.fetch_sub(1 << order, Ordering::Relaxed);
            return Some(addr);
        }
    }
    None
}

/// Free 2^order contiguous pages starting at `addr`.
pub fn free(addr: usize, order: usize) {
    assert!(order < MAX_ORDER);
    FREE_PAGES.fetch_add(1 << order, Ordering::Relaxed);
    let mut lists = FREE_LISTS.lock();
    let mut current = addr;
    let mut current_order = order;
    // Merge with buddy while possible.
    while current_order < MAX_ORDER - 1 {
        let buddy = current ^ (PAGE_SIZE << current_order);
        if lists[current_order].head == Some(buddy) {
            lists[current_order].head = None;
            current = current.min(buddy);
            current_order += 1;
        } else {
            break;
        }
    }
    lists[current_order].head = Some(current);
}
