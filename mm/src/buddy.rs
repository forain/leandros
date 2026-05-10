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
pub fn init_from_map(regions: &[boot::MemoryRegion]) {
    for region in regions {
        if region.kind != boot::MemoryType::Available { continue; }
        
        // Use all available RAM. Limine marks kernel/modules as reserved.
        let start = leandros_lib::align_up(region.base as usize, PAGE_SIZE);
        let end = leandros_lib::align_down((region.base + region.length) as usize, PAGE_SIZE);
        
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
            // Pop from head: set head to next block stored in the page.
            unsafe {
                let next_ptr = crate::phys_to_virt(addr) as *const usize;
                let next_val = *next_ptr;
                lists[o].head = if next_val == 0 { None } else { Some(next_val) };
            }

            // Split excess blocks back down.
            for split in (order..o).rev() {
                let buddy = addr + (PAGE_SIZE << split);
                // Push buddy to head of its list.
                unsafe {
                    let next_ptr = crate::phys_to_virt(buddy) as *mut usize;
                    *next_ptr = lists[split].head.unwrap_or(0);
                    lists[split].head = Some(buddy);
                }
            }
            FREE_PAGES.fetch_sub(1 << order, Ordering::Relaxed);
            return Some(addr);
        }
    }
    
    extern "C" { fn serial_print(s: *const u8, len: usize); }
    let msg = b"[BUDDY] Allocation failed! Out of memory.\n";
    unsafe { serial_print(msg.as_ptr(), msg.len()); }
    None
}

/// Free 2^order contiguous pages starting at `addr`.
pub fn free(addr: usize, order: usize) {
    assert!(order < MAX_ORDER);
    FREE_PAGES.fetch_add(1 << order, Ordering::Relaxed);
    let mut lists = FREE_LISTS.lock();
    
    // For now, we skip complex merging of non-head buddies to avoid O(N) scans.
    // We just push the freed block to the head.
    unsafe {
        let next_ptr = crate::phys_to_virt(addr) as *mut usize;
        *next_ptr = lists[order].head.unwrap_or(0);
        lists[order].head = Some(addr);
    }
}
