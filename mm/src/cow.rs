//! Address-space cloning for `fork()`, with real copy-on-write.
//!
//! Parent and child end up mapping the *same* physical pages read-only
//! wherever content could later diverge; a write fault then promotes just
//! that one 4 KiB page (see `AddressSpace::handle_user_page_fault` in
//! `vmm.rs`). `pageref` tracks how many owners a shared page has, so the
//! last one to let go of it is the one that actually frees it back to the
//! buddy allocator.
//!
//! `MAP_SHARED` regions are the one exception: both sides keep full original
//! permissions (writes are meant to be visible to every owner immediately),
//! refcounted only so teardown frees the frame exactly once.
//!
//! Device (MMIO) mappings — identified by the `file_cap == usize::MAX`
//! sentinel `map_device` uses — are duplicated into fresh private RAM rather
//! than shared, preserving the pre-CoW behavior; sharing a live device
//! physical range across two address spaces via this path isn't a case this
//! phase changes.
//!
//! Pages that were never faulted in before the fork are left as absent on
//! both sides; the first touch by either sibling afterward demand-pages
//! independently, which is correct since nobody has read or written that
//! memory yet.

extern crate alloc;
use alloc::vec::Vec;
use crate::vmm::{AddressSpace, VmaRegion, MAP_SHARED};
use crate::paging::{map_page, tlb_shootdown_all, PageFlags};
use crate::buddy::{PAGE_SIZE, alloc as buddy_alloc};
use crate::pageref;

/// Serializes every compound pageref transaction: `clone_as`'s inc+downgrade
/// sweep and `handle_user_page_fault`'s get→copy→dec promotion. The two run
/// under *different* per-address-space busy locks (parent vs child), so
/// without this a child promoting a shared frame races the parent's next
/// fork over the same refcounts — mis-deciding "sole owner, reuse in place"
/// leaves one frame writable in two processes. Lock order is always
/// own-AS-busy → COW_LOCK, so the two-lock combination cannot deadlock.
pub static COW_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Clone `src` into a fresh `AddressSpace` rooted at `new_page_table_root`.
///
/// Takes `src` by mutable reference: sharing a page copy-on-write requires
/// downgrading the *parent's* existing mapping to read-only too, and marking
/// the parent's own `VmaRegion`s as CoW-tracked, not just the child's.
///
/// Returns `None` on out-of-memory.
pub fn clone_as(src: &mut AddressSpace, new_page_table_root: usize) -> Option<AddressSpace> {
    let _cow_guard = COW_LOCK.lock();
    let src_root = src.root();
    let mut dst = AddressSpace::new(new_page_table_root);
    dst.heap_start = src.heap_start;
    dst.heap_end   = src.heap_end;

    dst.regions.resize(src.regions.len(), None);

    for (src_slot, dst_slot) in src.regions.iter_mut().zip(dst.regions.iter_mut()) {
        let region = match src_slot.as_mut() {
            Some(r) => r,
            None    => continue,
        };

        let is_shared = region.map_flags & MAP_SHARED != 0;
        let is_device = region.file_cap == usize::MAX;

        if !region.lazy && is_device {
            // Device (MMIO) mapping: duplicate into fresh RAM rather than
            // sharing the physical device range (unchanged pre-CoW behavior).
            let n_pages = (region.end - region.start) / PAGE_SIZE;
            let order   = pages_to_order(n_pages);
            let dst_phys = buddy_alloc(order)?;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    crate::phys_to_virt(region.phys) as *const u8,
                    crate::phys_to_virt(dst_phys)    as *mut u8,
                    n_pages * PAGE_SIZE,
                );
                for i in 0..n_pages {
                    map_page(new_page_table_root, region.start + i * PAGE_SIZE,
                             dst_phys + i * PAGE_SIZE, region.flags);
                }
            }
            *dst_slot = Some(VmaRegion {
                start: region.start, end: region.end, phys: dst_phys,
                flags: region.flags, lazy: false, lazy_pages: Vec::new(), lazy_count: 0,
                prot: region.prot, map_flags: region.map_flags,
                file_cap: region.file_cap, file_off: region.file_off,
                file_len: region.file_len, cow: false,
            });
            continue;
        }

        let downgraded = region.flags & !PageFlags::WRITABLE;
        let mut dst_lazy_pages = Vec::new();
        let mut dst_lazy_count = 0usize;

        // Writable private regions (stack, heap, .data/.bss, RW mmaps) are
        // *eagerly copied* into the child rather than shared copy-on-write.
        //
        // Sharing a writable page CoW with a just-forked child exposes an SMP
        // race in the runtime copy-promotion: when the parent (a multithreaded
        // process — brush's tokio runtime) writes such a page, its fault
        // handler copies the shared frame to a fresh one and remaps only the
        // parent, while the child still references the original. Under
        // concurrent access from the forking process's other threads this
        // deterministically-by-layout corrupts one small parent struct
        // (observed: std's `Process.pidfd` reading 0 instead of -1, so tokio's
        // reaper takes the `waitid(P_PIDFD)` path and brush exits with
        // "Invalid argument"). Giving the child its own copies here removes the
        // shared-writable window entirely: no writable page is ever both
        // parent- and child-mapped, so the copy-promotion never runs on a page
        // another address space still holds.
        //
        // Read-only private regions (the bulk of a static musl image: .text,
        // .rodata, demand-paged executable pages) keep full CoW below — they
        // are never written, so they never promote and cannot hit the race,
        // and sharing them keeps fork cheap.
        let is_writable = region.flags.contains(PageFlags::WRITABLE);
        if !is_shared && is_writable {
            if !region.lazy {
                let n_pages = (region.end - region.start) / PAGE_SIZE;
                dst_lazy_pages.resize(n_pages, 0);
                for i in 0..n_pages {
                    let src_phys = region.phys + i * PAGE_SIZE;
                    let np = match buddy_alloc(0) { Some(p) => p, None => return None };
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            crate::phys_to_virt(src_phys) as *const u8,
                            crate::phys_to_virt(np) as *mut u8, PAGE_SIZE);
                        map_page(new_page_table_root, region.start + i * PAGE_SIZE, np, region.flags);
                    }
                    dst_lazy_pages[i] = np;
                    dst_lazy_count += 1;
                }
            } else {
                dst_lazy_pages.resize(region.lazy_pages.len(), 0);
                for (i, &phys) in region.lazy_pages.iter().enumerate() {
                    if phys == 0 { continue; } // absent: child demand-pages it independently
                    let np = match buddy_alloc(0) { Some(p) => p, None => return None };
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            crate::phys_to_virt(phys) as *const u8,
                            crate::phys_to_virt(np) as *mut u8, PAGE_SIZE);
                        map_page(new_page_table_root, region.start + i * PAGE_SIZE, np, region.flags);
                    }
                    dst_lazy_pages[i] = np;
                    dst_lazy_count += 1;
                }
            }
            // Parent VMA is left exactly as it was — it keeps its own frames,
            // fully writable, and never takes a CoW fault for this region.
            if crate::vmm::is_file_backed(region.file_cap) { crate::vmm::file_retain(region.file_cap); }
            *dst_slot = Some(VmaRegion {
                start: region.start, end: region.end, phys: 0, flags: region.flags,
                lazy: true, lazy_pages: dst_lazy_pages, lazy_count: dst_lazy_count,
                prot: region.prot, map_flags: region.map_flags, file_cap: region.file_cap,
                file_off: region.file_off, file_len: region.file_len, cow: false,
            });
            continue;
        }

        if !region.lazy {
            // Still-contiguous, never-forked eager block: convert both
            // parent and child to per-page tracking so each side can later
            // promote individual pages independently of the other.
            let n_pages = (region.end - region.start) / PAGE_SIZE;
            dst_lazy_pages.resize(n_pages, 0);
            for i in 0..n_pages {
                let phys = region.phys + i * PAGE_SIZE;
                pageref::inc(phys);
                dst_lazy_pages[i] = phys;
                dst_lazy_count += 1;
                let install_flags = if is_shared { region.flags } else { downgraded };
                unsafe {
                    map_page(new_page_table_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    map_page(src_root, region.start + i * PAGE_SIZE, phys, install_flags);
                }
            }
            region.lazy = true;
            region.phys = 0;
            region.lazy_pages = dst_lazy_pages.clone();
            region.lazy_count = dst_lazy_count;
            region.cow = !is_shared;
        } else {
            // Already per-page tracked (ordinary lazy mmap/heap, or a region
            // that went through this same conversion in an earlier fork).
            for (i, &phys) in region.lazy_pages.iter().enumerate() {
                if phys == 0 { continue; }
                pageref::inc(phys);
                if dst_lazy_pages.len() <= i { dst_lazy_pages.resize(i + 1, 0); }
                dst_lazy_pages[i] = phys;
                dst_lazy_count += 1;
                let install_flags = if is_shared { region.flags } else { downgraded };
                unsafe {
                    map_page(new_page_table_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    if !is_shared {
                        map_page(src_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    }
                }
            }
            if !is_shared { region.cow = true; }
        }

        // The child VMA holds its own reference to any backing file: pages
        // still absent after the fork are demand-read by whichever side
        // touches them first, so the file must outlive both address spaces.
        if crate::vmm::is_file_backed(region.file_cap) {
            crate::vmm::file_retain(region.file_cap);
        }

        *dst_slot = Some(VmaRegion {
            start:      region.start,
            end:        region.end,
            phys:       0,
            flags:      region.flags,
            lazy:       true,
            lazy_pages: dst_lazy_pages,
            lazy_count: dst_lazy_count,
            prot:       region.prot,
            map_flags:  region.map_flags,
            file_cap:   region.file_cap,
            file_off:   region.file_off,
            file_len:   region.file_len,
            cow:        !is_shared,
        });
    }

    // The downgrades above rewrote *live* PTEs of the calling (parent)
    // process from writable to read-only. arch_map_page does not invalidate
    // existing translations (its barrier reasoning covers invalid→valid
    // transitions only), so this CPU's TLB still holds stale writable
    // entries for the parent's pages — most critically its user stack. If
    // the parent resumes and writes through such an entry before its next
    // page-table switch, the write silently lands on the still-shared frame
    // (no fault, no copy) and the child later reads the corruption. Flush
    // now, while the parent's root is the active one, so the parent's first
    // post-fork write takes the CoW fault it must.
    tlb_shootdown_all();

    Some(dst)
}

/// Minimal buddy-order calculation: smallest order such that `2^order ≥ pages`.
fn pages_to_order(pages: usize) -> usize {
    let mut order = 0;
    let mut cap   = 1usize;
    while cap < pages { cap <<= 1; order += 1; }
    order
}
