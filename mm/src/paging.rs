//! Virtual memory / page table management.
//!
//! Architecture-agnostic interface; arch crates provide the concrete
//! page-table walk (x86-64 4-level PT, AArch64 TTBR0/TTBR1, etc.).

use bitflags::bitflags;

bitflags! {
    /// Page mapping flags (architecture-agnostic).
    #[derive(Clone, Copy, Debug)]
    pub struct PageFlags: u64 {
        const PRESENT   = 1 << 0;
        const WRITABLE  = 1 << 1;
        const USER      = 1 << 2;
        const EXECUTE   = 1 << 3;
        const NOCACHE   = 1 << 4;
    }
}

extern "C" {
    fn arch_get_current_root() -> usize;
    /// Arch-provided: map `phys` at `virt` with the given flags in the page
    /// table rooted at `page_table_root`.  Returns `true` on success, `false`
    /// if an intermediate page-table node could not be allocated (OOM).
    /// Implemented by each arch crate and resolved at link time.
    fn arch_map_page(page_table_root: usize, virt: usize, phys: usize, flags: u64) -> bool;
    /// Arch-provided: remove the mapping for `virt` and flush the TLB entry.
    fn arch_unmap_page(page_table_root: usize, virt: usize);
    /// Arch-provided: broadcast TLB invalidation for all user-space entries to
    /// all CPUs (inner-shareable TLBI on AArch64; CR3 reload on x86-64).
    fn arch_tlb_shootdown_all();
}

/// Map a single virtual page to a physical frame in the given address space.
pub unsafe fn map_page(
    page_table_root: usize,
    virt: usize,
    phys: usize,
    flags: PageFlags,
) -> bool {
    arch_map_page(page_table_root, virt, phys, flags.bits())
}

/// Unmap a virtual page and flush the TLB entry on the current CPU.
pub unsafe fn unmap_page(page_table_root: usize, virt: usize) {
    arch_unmap_page(page_table_root, virt);
}

/// Invalidate all user-space TLB entries across all CPUs.
pub fn tlb_shootdown_all() {
    unsafe { arch_tlb_shootdown_all(); }
}

pub fn get_current_root() -> usize {
    unsafe { arch_get_current_root() }
}

/// Map a hardware device (MMIO) into the kernel virtual address space.
pub unsafe fn map_kernel_device(phys: usize, size: usize, flags: PageFlags) -> Option<usize> {
    let root = get_current_root();
    let virt = crate::phys_to_virt(phys);
    let page_phys = phys & !(crate::buddy::PAGE_SIZE - 1);
    let page_virt = virt & !(crate::buddy::PAGE_SIZE - 1);
    let pages = (size + (phys - page_phys) + crate::buddy::PAGE_SIZE - 1) / crate::buddy::PAGE_SIZE;

    for i in 0..pages {
        if !map_page(root, page_virt + i * crate::buddy::PAGE_SIZE, page_phys + i * crate::buddy::PAGE_SIZE, flags) {
            return None;
        }
    }
    Some(virt)
}
