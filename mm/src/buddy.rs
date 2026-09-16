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

/// One past the highest physical address `init_from_map` handed to the
/// allocator. Every block that is ever freed must lie below it.
static PHYS_END: AtomicUsize = AtomicUsize::new(0);
/// Number of `free()` calls the sanity checks refused (leaked instead of
/// corrupting the lists). Shown by the Ctrl-T census.
static BAD_FREES: AtomicUsize = AtomicUsize::new(0);
pub fn bad_frees() -> usize { BAD_FREES.load(Ordering::Relaxed) }

/// Sanity checks on every `free()` and on every link the allocator follows.
/// Each is a handful of compares (plus one bitmap read for the double-free
/// test), so they stay on in release builds; a refused free leaks the block
/// and prints who asked, which is strictly better than the alternative — a
/// corrupt intrusive list that faults in some later, unrelated `free()`.
const CHECKS: bool = true;

extern "C" { fn serial_write_byte_direct(b: u8); }
fn dbg_str(s: &[u8]) { for &b in s { unsafe { serial_write_byte_direct(b); } } }
fn dbg_hex(v: usize) {
    const HEX: &[u8] = b"0123456789abcdef";
    dbg_str(b"0x");
    let mut started = false;
    for i in (0..16).rev() {
        let n = (v >> (i * 4)) & 0xF;
        if n != 0 || started || i == 0 { dbg_str(&HEX[n..n + 1]); started = true; }
    }
}
fn dbg_dec(mut v: usize) {
    let mut buf = [0u8; 20]; let mut i = buf.len();
    loop { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    dbg_str(&buf[i..]);
}

/// Report a refused `free(addr, order)`; `why` names the failed check.
#[cold]
fn report_bad_free(addr: usize, order: usize, why: &[u8], loc: &core::panic::Location<'_>) {
    BAD_FREES.fetch_add(1, Ordering::Relaxed);
    dbg_str(b"[BUDDY] BAD FREE refused: addr="); dbg_hex(addr);
    dbg_str(b" order="); dbg_dec(order);
    dbg_str(b" ("); dbg_str(why); dbg_str(b") from ");
    dbg_str(loc.file().as_bytes()); dbg_str(b":"); dbg_dec(loc.line() as usize);
    dbg_str(b" phys_end="); dbg_hex(PHYS_END.load(Ordering::Relaxed));
    dbg_str(b"\n");
}

/// Report a link (`next`/`prev`) that cannot be a free block's address, and
/// halt: the list is already corrupt and every later step would fault
/// somewhere less informative.
#[cold]
fn report_bad_link(node: usize, order: usize, what: &[u8], val: usize) -> ! {
    dbg_str(b"[BUDDY] CORRUPT LINK: node="); dbg_hex(node);
    dbg_str(b" order="); dbg_dec(order);
    dbg_str(b" "); dbg_str(what); dbg_str(b"="); dbg_hex(val);
    dbg_str(b" phys_end="); dbg_hex(PHYS_END.load(Ordering::Relaxed));
    dbg_str(b" node[0..4]=");
    unsafe {
        let p = crate::phys_to_virt(node) as *const usize;
        for i in 0..4 { dbg_hex(*p.add(i)); dbg_str(b" "); }
    }
    dbg_str(b"\n");
    panic!("buddy free list corrupt");
}

/// A link is sane if it is the null sentinel or a page-aligned address
/// inside the RAM the allocator was given.
#[inline]
fn link_ok(val: usize) -> bool {
    val == 0 || (val & (PAGE_SIZE - 1) == 0 && val < PHYS_END.load(Ordering::Relaxed))
}
#[inline]
fn check_links(node: usize, order: usize, next: usize, prev: usize) {
    if !CHECKS { return; }
    if !link_ok(next) { report_bad_link(node, order, b"next", next); }
    if !link_ok(prev) { report_bad_link(node, order, b"prev", prev); }
}

/// Return total pages registered with the buddy allocator.
pub fn total_pages() -> usize { TOTAL_PAGES.load(Ordering::Relaxed) }
/// Return approximate number of free pages.
pub fn free_pages()  -> usize { FREE_PAGES.load(Ordering::Relaxed) }

/// A free list for one order level.
///
/// Doubly-linked: each free block stores its own `next` pointer at byte
/// offset 0, `prev` pointer at byte offset 8 and its order at byte offset 16
/// (accessed via the HHDM). This gives O(1) removal of an arbitrary node,
/// which `free()` needs to unlink a buddy from the middle of its list when
/// coalescing. `0` is never a valid block address (`init_from_map` keeps
/// everything below `LOW_RESERVED_END` out of the pool) so it doubles as
/// the "no link" sentinel, matching the convention the allocator already
/// used before coalescing.
struct FreeList {
    head: Option<usize>, // physical address of first free block
}

impl FreeList {
    const fn empty() -> Self { Self { head: None } }
}

static FREE_LISTS: Mutex<[FreeList; MAX_ORDER]> = Mutex::new([const { FreeList::empty() }; MAX_ORDER]);

/// One bit per physical page: set while that page is the head of a block on
/// a free list. This is what lets `free()` answer "is my buddy free, and at
/// my order?" in O(1) (the order is then read from the node itself, which
/// the bit guarantees is free memory and so holds real link data). Without
/// it the coalesce check was a walk of the whole order-0 list per page
/// freed — fine on a fresh boot (one block), but fork's eager copies
/// fragment RAM fast (2,285 order-0 blocks after 40 fork+exec cycles) and
/// the exec-time teardown of a forked child went from 50 ms to seconds,
/// growing with uptime.
///
/// Covers `BITMAP_COVERAGE` bytes of physical address space from 0; pages
/// beyond it fall back to the list walk, so coverage is a speed bound, not a
/// correctness one. Accessed only with `FREE_LISTS` held.
const BITMAP_COVERAGE: usize = 16 << 30; // 16 GiB
const BITMAP_WORDS: usize = BITMAP_COVERAGE / PAGE_SIZE / 64;
struct FreeHeadBitmap(core::cell::UnsafeCell<[u64; BITMAP_WORDS]>);
unsafe impl Sync for FreeHeadBitmap {}
static FREE_HEAD: FreeHeadBitmap = FreeHeadBitmap(core::cell::UnsafeCell::new([0; BITMAP_WORDS]));

#[inline] fn covered(addr: usize) -> bool { addr < BITMAP_COVERAGE }
/// Caller holds `FREE_LISTS`.
#[inline] unsafe fn head_bit_set(addr: usize, v: bool) {
    if !covered(addr) { return; }
    let page = addr / PAGE_SIZE;
    let w = &mut (*FREE_HEAD.0.get())[page / 64];
    if v { *w |= 1 << (page % 64); } else { *w &= !(1 << (page % 64)); }
}
/// Caller holds `FREE_LISTS`. Only meaningful for `covered` addresses.
#[inline] unsafe fn head_bit(addr: usize) -> bool {
    let page = addr / PAGE_SIZE;
    (*FREE_HEAD.0.get())[page / 64] & (1 << (page % 64)) != 0
}

unsafe fn node_next(addr: usize) -> usize { *(crate::phys_to_virt(addr) as *const usize) }
unsafe fn node_set_next(addr: usize, v: usize) { *(crate::phys_to_virt(addr) as *mut usize) = v; }
unsafe fn node_prev(addr: usize) -> usize { *((crate::phys_to_virt(addr) + 8) as *const usize) }
unsafe fn node_set_prev(addr: usize, v: usize) { *((crate::phys_to_virt(addr) + 8) as *mut usize) = v; }
unsafe fn node_order(addr: usize) -> usize { *((crate::phys_to_virt(addr) + 16) as *const usize) }
unsafe fn node_set_order(addr: usize, v: usize) { *((crate::phys_to_virt(addr) + 16) as *mut usize) = v; }

/// Push `addr` onto the head of `lists[order]`.
fn push_front(lists: &mut [FreeList; MAX_ORDER], order: usize, addr: usize) {
    let old_head = lists[order].head;
    unsafe {
        node_set_next(addr, old_head.unwrap_or(0));
        node_set_prev(addr, 0);
        node_set_order(addr, order);
        if let Some(h) = old_head { node_set_prev(h, addr); }
        head_bit_set(addr, true);
    }
    lists[order].head = Some(addr);
}

/// Unlink `addr`, known to be on `lists[order]`, in O(1) via its own links.
fn unlink(lists: &mut [FreeList; MAX_ORDER], order: usize, addr: usize) {
    unsafe {
        let next = node_next(addr);
        let prev = node_prev(addr);
        check_links(addr, order, next, prev);
        if prev != 0 {
            node_set_next(prev, next);
        } else {
            lists[order].head = if next == 0 { None } else { Some(next) };
        }
        if next != 0 { node_set_prev(next, prev); }
        head_bit_set(addr, false);
    }
}

/// If `target` is the head of a free block of exactly `order`, unlink and
/// remove it. O(1) through the bitmap where it covers `target`; otherwise a
/// walk of `lists[order]`.
///
/// Safe to walk: every node visited here is, by definition, already-free
/// memory holding real next/prev link data (never arbitrary or allocated
/// memory), since the only way an address gets onto this list is via
/// `push_front`.
fn try_remove(lists: &mut [FreeList; MAX_ORDER], order: usize, target: usize) -> bool {
    if covered(target) {
        unsafe {
            if !head_bit(target) || node_order(target) != order { return false; }
        }
        unlink(lists, order, target);
        return true;
    }
    let mut cur = lists[order].head;
    while let Some(addr) = cur {
        let next = unsafe { node_next(addr) };
        check_links(addr, order, next, 0);
        if addr == target {
            unlink(lists, order, addr);
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

/// Physical memory below this is never handed to the allocator, whatever
/// the memory map says about it.
///
/// Page 0 first: the free lists are intrusive and use address 0 as their
/// "no link" sentinel (`push_front`, `unlink`), and `alloc` returning 0 reads
/// as failure to half its callers. The x86-64 UEFI map from OVMF lists
/// `[0, 0x87000)` as usable, so before this `init_from_map` put a 512 KiB
/// block *at address 0* on the order-7 list: the block behind it became
/// unreachable the first time anything was pushed in front of it (its `next`
/// was written as 0 = end of list), its buddy-coalescing state went stale
/// with it, and the frame the allocator did hand out as "0" was reported as
/// an out-of-memory failure. The rest of the first MiB is the AP startup
/// trampoline at 0x7000 (`arch/x86_64/src/smp.rs`), which the buddy must not
/// hand out from under a SIPI, plus real-mode firmware structures. Linux
/// reserves the same megabyte for the same reasons.
const LOW_RESERVED_END: usize = 1 << 20;

/// Initialise the buddy allocator from the boot memory map.
pub fn init_from_map(regions: &[boot::MemoryRegion]) {
    for region in regions {
        if region.kind != boot::MemoryType::Available { continue; }

        // Use all available RAM. Limine marks kernel/modules as reserved.
        let start = leandros_lib::align_up(region.base as usize, PAGE_SIZE).max(LOW_RESERVED_END);
        let end = leandros_lib::align_down((region.base + region.length) as usize, PAGE_SIZE);

        if start >= end { continue; }
        if end > PHYS_END.load(Ordering::Relaxed) { PHYS_END.store(end, Ordering::Relaxed); }

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
                check_links(addr, o, next_val, 0);
                if next_val != 0 {
                    node_set_prev(next_val, 0);
                    lists[o].head = Some(next_val);
                } else {
                    lists[o].head = None;
                }
                head_bit_set(addr, false);
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
    
    dbg_str(b"[BUDDY] Allocation failed! Out of memory.\n");
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
#[track_caller]
pub fn free(addr: usize, order: usize) {
    let loc = core::panic::Location::caller();
    if order >= MAX_ORDER { report_bad_free(addr, order, b"order", loc); return; }
    if CHECKS {
        let size = PAGE_SIZE << order;
        let end = PHYS_END.load(Ordering::Relaxed);
        if addr == 0 { report_bad_free(addr, order, b"null", loc); return; }
        if addr & (size - 1) != 0 { report_bad_free(addr, order, b"misaligned", loc); return; }
        if end != 0 && addr.checked_add(size).map_or(true, |e| e > end) {
            report_bad_free(addr, order, b"beyond RAM", loc); return;
        }
        if overlaps_reserved(addr, size) { report_bad_free(addr, order, b"reserved", loc); return; }
    }
    let mut lists = FREE_LISTS.lock();
    if CHECKS && covered(addr) && unsafe { head_bit(addr) } {
        drop(lists);
        report_bad_free(addr, order, b"double free", loc); return;
    }
    FREE_PAGES.fetch_add(1 << order, Ordering::Relaxed);

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

/// Free-list census for the Ctrl-T task dump: the number of free blocks at
/// each order, written with `emit`. `try_remove` walks the order-0 list once
/// per page freed, so the order-0 count is the per-page cost of tearing an
/// address space down. Uses `try_lock` so an IRQ-context caller cannot
/// deadlock against an allocation in progress.
pub fn free_list_census(emit: &mut dyn FnMut(usize, usize)) -> bool {
    let lists = match FREE_LISTS.try_lock() { Some(l) => l, None => return false };
    for order in 0..MAX_ORDER {
        let mut n = 0usize;
        let mut cur = lists[order].head;
        // Bounded: a corrupted (cyclic) list must not turn a diagnostic into
        // a hang. `usize::MAX` reports a walk that hit the bound.
        while let Some(addr) = cur {
            n += 1;
            if n > 4_000_000 { n = usize::MAX; break; }
            let next = unsafe { node_next(addr) };
            cur = if next == 0 { None } else { Some(next) };
        }
        emit(order, n);
    }
    true
}
