//! Address-space cloning for `fork()`.
//!
//! Phase 1 uses a simple page-by-page copy.  True CoW (copy-on-write with
//! deferred page promotion on write fault) is deferred to Phase 4 when the
//! `PAGE_REFS` reference-count table will be added.
//!
//! # Guarantee
//!
//! `clone_as` returns `Some(child_as)` only when ALL VMAs and their backing
//! pages have been successfully duplicated.  On any partial-OOM failure the
//! partially-built `child_as` is dropped (freeing whatever was allocated so
//! far) and `None` is returned — there is no leak.

extern crate alloc;
use alloc::vec::Vec;
use crate::vmm::{AddressSpace, VmaRegion};
use crate::paging::map_page;
use crate::buddy::{PAGE_SIZE, alloc as buddy_alloc};

/// Clone `src` into a fresh `AddressSpace` rooted at `new_page_table_root`.
///
/// Both eager VMAs (contiguous buddy allocation) and lazy VMAs (per-page
/// demand-allocated pages) are handled.  Only pages that have already been
/// faulted in (non-zero `lazy_pages` entries) are copied.
///
/// Returns `None` on out-of-memory.
pub fn clone_as(src: &AddressSpace, new_page_table_root: usize) -> Option<AddressSpace> {
    let mut dst = AddressSpace::new(new_page_table_root);
    dst.heap_start = src.heap_start;
    dst.heap_end   = src.heap_end;

    for (src_slot, dst_slot) in src.regions.iter().zip(dst.regions.iter_mut()) {
        let region = match src_slot.as_ref() {
            Some(r) => r,
            None    => continue,
        };

        // Initialise the destination VMA with the same metadata.
        *dst_slot = Some(VmaRegion {
            start:      region.start,
            end:        region.end,
            phys:       0,
            flags:      region.flags,
            lazy:       region.lazy,
            lazy_pages: Vec::new(),
            lazy_count: 0,
            prot:       region.prot,
            map_flags:  region.map_flags,
            file_cap:   region.file_cap,
            file_off:   region.file_off,
            cow:        region.cow,
        });
        let dst_region = dst_slot.as_mut().unwrap();

        if region.lazy {
            // Copy only the pages that have actually been faulted in; un-faulted
            // pages remain absent (the child will fault them on demand).
            for (i, &src_phys) in region.lazy_pages.iter().enumerate() {
                if src_phys == 0 { continue; }

                let dst_phys = buddy_alloc(0)?; // order 0 = one 4 KiB page
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        crate::phys_to_virt(src_phys) as *const u8,
                        crate::phys_to_virt(dst_phys) as *mut u8,
                        PAGE_SIZE,
                    );
                    map_page(
                        new_page_table_root,
                        region.start + i * PAGE_SIZE,
                        dst_phys,
                        region.flags,
                    );
                }
                // Grow the dst tracking Vec to cover index i.
                if dst_region.lazy_pages.len() <= i {
                    dst_region.lazy_pages.resize(i + 1, 0);
                }
                dst_region.lazy_pages[i] = dst_phys;
                dst_region.lazy_count   += 1;
            }
        } else if region.phys != 0 {
            // Eager VMA: copy the whole contiguous allocation in one shot.
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
                    map_page(
                        new_page_table_root,
                        region.start + i * PAGE_SIZE,
                        dst_phys    + i * PAGE_SIZE,
                        region.flags,
                    );
                }
            }
            dst_region.phys = dst_phys;
        }
        // else: a lazy VMA with no pages yet; nothing to copy.
    }

    Some(dst)
}

/// Minimal buddy-order calculation: smallest order such that `2^order ≥ pages`.
fn pages_to_order(pages: usize) -> usize {
    let mut order = 0;
    let mut cap   = 1usize;
    while cap < pages { cap <<= 1; order += 1; }
    order
}
