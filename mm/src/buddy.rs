//! Buddy allocator — Linux-style power-of-two physical page allocator.
//!
//! See: linux/mm/page_alloc.c

use spin::Mutex;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const PAGE_SIZE: usize = 4096;
pub const MAX_ORDER: usize = 19; // 2^18 pages = 1 GiB max contiguous block.

/// Total pages ever freed into the allocator (proxy for physical RAM size).
static TOTAL_PAGES: AtomicUsize = AtomicUsize::new(0);
/// Current free page count (updated on alloc/free).
static FREE_PAGES:  AtomicUsize = AtomicUsize::new(0);

/// Return total pages registered with the buddy allocator.
pub fn total_pages() -> usize { TOTAL_PAGES.load(Ordering::Relaxed) }
/// Return approximate number of free pages.
pub fn free_pages()  -> usize { FREE_PAGES.load(Ordering::Relaxed) }

/// A free list for one order level.
///
/// Doubly-linked: each free block stores its own `next` pointer at byte
/// offset 0 and `prev` pointer at byte offset 8 (accessed via the HHDM).
/// This gives O(1) removal of an arbitrary node once located, which
/// `free()` needs to unlink a buddy from the middle of its list when
/// coalescing.  `0` is never a valid block address (page 0 is always
/// reserved by the boot memory map) so it doubles as the "no link" sentinel,
/// matching the convention the allocator already used before coalescing.
struct FreeList {
    head: Option<usize>, // physical address of first free block
}

impl FreeList {
    const fn empty() -> Self { Self { head: None } }
}

static FREE_LISTS: Mutex<[FreeList; MAX_ORDER]> = Mutex::new([const { FreeList::empty() }; MAX_ORDER]);

unsafe fn node_next(addr: usize) -> usize { *(crate::phys_to_virt(addr) as *const usize) }
unsafe fn node_set_next(addr: usize, v: usize) { *(crate::phys_to_virt(addr) as *mut usize) = v; }
unsafe fn node_prev(addr: usize) -> usize { *((crate::phys_to_virt(addr) + 8) as *const usize) }
unsafe fn node_set_prev(addr: usize, v: usize) { *((crate::phys_to_virt(addr) + 8) as *mut usize) = v; }

/// Push `addr` onto the head of `lists[order]`.
fn push_front(lists: &mut [FreeList; MAX_ORDER], order: usize, addr: usize) {
    let old_head = lists[order].head;
    unsafe {
        node_set_next(addr, old_head.unwrap_or(0));
        node_set_prev(addr, 0);
        if let Some(h) = old_head { node_set_prev(h, addr); }
    }
    lists[order].head = Some(addr);
}

/// If `target` is present in `lists[order]`, unlink and remove it.
///
/// Safe to walk: every node visited here is, by definition, already-free
/// memory holding real next/prev link data (never arbitrary or allocated
/// memory), since the only way an address gets onto this list is via
/// `push_front`.
fn try_remove(lists: &mut [FreeList; MAX_ORDER], order: usize, target: usize) -> bool {
    let mut cur = lists[order].head;
    while let Some(addr) = cur {
        let next = unsafe { node_next(addr) };
        if addr == target {
            let prev = unsafe { node_prev(addr) };
            if prev != 0 {
                unsafe { node_set_next(prev, next); }
            } else {
                lists[order].head = if next == 0 { None } else { Some(next) };
            }
            if next != 0 {
                unsafe { node_set_prev(next, prev); }
            }
            return true;
        }
        cur = if next == 0 { None } else { Some(next) };
    }
    false
}

/// Physical [start, end) ranges that must never be handed out by the allocator
/// (kernel image, page tables, initrd, …). Limine marks these reserved in its
/// memory map; on direct boot the DTB map calls all RAM available, so the
/// kernel registers them explicitly via `reserve_range` before `init_from_map`.
const MAX_RESERVED: usize = 8;
static RESERVED: Mutex<[(usize, usize); MAX_RESERVED]> =
    Mutex::new([(0, 0); MAX_RESERVED]);

/// Record a physical [start, end) range to exclude from the free pool.
/// Must be called before `init_from_map`.
pub fn reserve_range(start: usize, end: usize) {
    if start >= end { return; }
    let mut r = RESERVED.lock();
    for slot in r.iter_mut() {
        if slot.0 == slot.1 { // empty slot
            *slot = (leandros_lib::align_down(start, PAGE_SIZE),
                     leandros_lib::align_up(end, PAGE_SIZE));
            return;
        }
    }
}

/// True if the page-aligned block [addr, addr + size) touches any reserved range.
fn overlaps_reserved(addr: usize, size: usize) -> bool {
    let r = RESERVED.lock();
    for &(s, e) in r.iter() {
        if s == e { continue; }
        if addr < e && s < addr + size { return true; }
    }
    false
}

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
            // Skip pages that fall inside a reserved range.
            if overlaps_reserved(addr, PAGE_SIZE) {
                addr += PAGE_SIZE;
                continue;
            }
            let remaining_pages = (end - addr) / PAGE_SIZE;
            let max_order = usize::min(MAX_ORDER - 1,
                (usize::BITS - 1 - remaining_pages.leading_zeros()) as usize);
            // Also constrain by alignment.
            let align_order = (addr / PAGE_SIZE).trailing_zeros() as usize;
            let mut order = usize::min(max_order, usize::min(align_order, MAX_ORDER - 1));
            // Shrink the block until it no longer spans a reserved range.
            while order > 0 && overlaps_reserved(addr, PAGE_SIZE << order) {
                order -= 1;
            }
            free(addr, order);
            addr += PAGE_SIZE << order;
        }
    }
    // Snapshot total = free pages right after init (before any allocations).
    TOTAL_PAGES.store(FREE_PAGES.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// Allocate 2^order contiguous physical pages. Returns physical address or None.
pub fn alloc(order: usize) -> Option<usize> {
    if order >= MAX_ORDER { return None; }
    let mut lists = FREE_LISTS.lock();
    // Walk up from requested order looking for a free block.
    for o in order..MAX_ORDER {
        if let Some(addr) = lists[o].head.take() {
            // Pop from head: the new head (if any) becomes the list head
            // with no predecessor.
            unsafe {
                let next_val = node_next(addr);
                if next_val != 0 {
                    node_set_prev(next_val, 0);
                    lists[o].head = Some(next_val);
                } else {
                    lists[o].head = None;
                }
            }

            // Split excess blocks back down, pushing each buddy half onto
            // its own order's free list.
            for split in (order..o).rev() {
                let buddy = addr + (PAGE_SIZE << split);
                push_front(&mut lists, split, buddy);
            }
            FREE_PAGES.fetch_sub(1 << order, Ordering::Relaxed);
            return Some(addr);
        }
    }
    
    extern "C" { fn serial_write_byte_direct(b: u8); }
    let msg = b"[BUDDY] Allocation failed! Out of memory.\n";
    for &b in msg {
        unsafe { serial_write_byte_direct(b); }
    }
    None
}

/// Free 2^order contiguous pages starting at `addr`.
///
/// Coalesces with the buddy block repeatedly (bounded by `MAX_ORDER`) before
/// inserting, so freed memory is always merged back into the largest
/// available contiguous block instead of fragmenting permanently.  Every
/// block this allocator hands out is naturally aligned to its own order
/// (preserved by both `init_from_map`'s alignment-constrained order pick and
/// `alloc`'s splitting), so `addr ^ (PAGE_SIZE << order)` always yields the
/// correct buddy address.
pub fn free(addr: usize, order: usize) {
    if order >= MAX_ORDER { return; }
    FREE_PAGES.fetch_add(1 << order, Ordering::Relaxed);
    let mut lists = FREE_LISTS.lock();

    let mut addr = addr;
    let mut order = order;
    while order + 1 < MAX_ORDER {
        let buddy = addr ^ (PAGE_SIZE << order);
        if overlaps_reserved(buddy, PAGE_SIZE << order) { break; }
        if !try_remove(&mut lists, order, buddy) { break; }
        addr = addr.min(buddy);
        order += 1;
    }
    push_front(&mut lists, order, addr);
}
