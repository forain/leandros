//! Memory Manager — physical and virtual memory subsystem.
//!
//! Mirrors Linux mm/ but restricted to the microkernel nucleus:
//!   - Physical frame allocator (buddy system)
//!   - Kernel virtual address space
//!   - Per-process page table management
//!   - Slab/slub-style object allocator

#![no_std]

pub mod atomic_pool;
pub mod buddy;
pub mod cow;
pub mod dma_cache;
pub mod gap2;
pub mod kmodwin;
pub mod kva;
pub mod pagecache;
pub mod pageref;
pub mod shmem;
pub mod pagevec;
pub mod paging;
pub mod slab;
pub mod vmm;

use core::sync::atomic::{AtomicUsize, Ordering};

/// Higher-Half Direct Map offset.
static HHDM_OFFSET: AtomicUsize = AtomicUsize::new(0);

pub fn set_hhdm_offset(offset: usize) {
    HHDM_OFFSET.store(offset, Ordering::Relaxed);
    // Note: Debug logging would be added here but we don't have access to kernel print functions
    // The debug info will be logged from the kernel where this is called
}

pub fn phys_to_virt(phys: usize) -> usize {
    phys + HHDM_OFFSET.load(Ordering::Relaxed)
}

pub fn virt_to_phys(virt: usize) -> usize {
    virt - HHDM_OFFSET.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn mm_get_hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed) as u64
}

/// Initialise all memory subsystems with a physical memory map.
/// Called once from `kernel_main` after boot info is parsed.
pub fn init_with_map(regions: &[boot::MemoryRegion], hhdm_offset: usize) {
    set_hhdm_offset(hhdm_offset);
    let v = buddy::verbose();
    if v {
        buddy::dbg_str(b"[mm] init: "); buddy::dbg_dec(regions.len());
        buddy::dbg_str(b" regions, hhdm "); buddy::dbg_hex(hhdm_offset); buddy::dbg_str(b"\n");
    }
    buddy::init_from_map(regions);
    buddy::init_site_tags();
    if v { buddy::dbg_str(b"[mm] slab\n"); }
    slab::init();
    if v { buddy::dbg_str(b"[mm] atomic pool\n"); }
    atomic_pool::init();
    if v { buddy::dbg_str(b"[mm] init done\n"); }
}

/// Fallback init with no memory map (used in unit tests).
pub fn init() {
    set_hhdm_offset(0);
    buddy::init_from_map(&[]);
    slab::init();
}
