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

/// Clone `src` into a fresh `AddressSpace` rooted at `new_page_table_root`.
///
/// Takes `src` by mutable reference: sharing a page copy-on-write requires
/// downgrading the *parent's* existing mapping to read-only too, and marking
/// the parent's own `VmaRegion`s as CoW-tracked, not just the child's.
///
/// Returns `None` on out-of-memory.
pub fn clone_as(src: &mut AddressSpace, new_page_table_root: usize) -> Option<AddressSpace> {
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
                file_cap: region.file_cap, file_off: region.file_off, cow: false,
            });
            continue;
        }

        let downgraded = region.flags & !PageFlags::WRITABLE;
        let mut dst_lazy_pages = Vec::new();
        let mut dst_lazy_count = 0usize;

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
