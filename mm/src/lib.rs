//! Memory Manager — physical and virtual memory subsystem.
//!
//! Mirrors Linux mm/ but restricted to the microkernel nucleus:
//!   - Physical frame allocator (buddy system)
//!   - Kernel virtual address space
//!   - Per-process page table management
//!   - Slab/slub-style object allocator

#![no_std]

pub mod buddy;
pub mod cow;
pub mod paging;
pub mod slab;
pub mod vmm;

use core::sync::atomic::{AtomicUsize, Ordering};

/// Higher-Half Direct Map offset.
static HHDM_OFFSET: AtomicUsize = AtomicUsize::new(0);

pub fn set_hhdm_offset(offset: usize) {
    HHDM_OFFSET.store(offset, Ordering::Relaxed);
}

pub fn phys_to_virt(phys: usize) -> usize {
    phys + HHDM_OFFSET.load(Ordering::Relaxed)
}

pub fn virt_to_phys(virt: usize) -> usize {
    virt - HHDM_OFFSET.load(Ordering::Relaxed)
}

/// Initialise all memory subsystems with a physical memory map.
/// Called once from `kernel_main` after boot info is parsed.
pub fn init_with_map(regions: &[boot::MemoryRegion], hhdm_offset: usize) {
    set_hhdm_offset(hhdm_offset);
    buddy::init_from_map(regions);
    slab::init();
}

/// Fallback init with no memory map (used in unit tests).
pub fn init() {
    set_hhdm_offset(0);
    buddy::init_from_map(&[]);
    slab::init();
}
