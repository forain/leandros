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
        const MMIO      = 1 << 5;
        /// Non-cached, but still ordinary memory: unaligned access, ordinary
        /// loads and stores, no device semantics. Normal Inner/Outer
        /// Non-cacheable on AArch64 (MAIR index 2); the PCD bit on x86-64,
        /// which with the reset PAT means UC.
        ///
        /// Deliberately NOT merged with `NOCACHE`. On AArch64 `NOCACHE` selects
        /// MAIR index 3, which is Device-nGnRnE on every boot path we have —
        /// right for the framebuffer and for MMIO BARs, wrong for anything
        /// userspace memcpys through, because Device memory faults on the first
        /// unaligned access. Keeping the two apart is what lets a virtio-gpu
        /// blob be non-cached without turning the sound card's registers into
        /// speculatively-readable Normal memory.
        const WRITECOMBINE = 1 << 6;
    }
}

extern "C" {
    fn arch_get_current_root() -> usize;
    /// Arch-provided: return the kernel page table root.
    /// On AArch64 this is TTBR1_EL1; on x86-64 it is the same as CR3.
    /// Kernel device mappings (HHDM virtual addresses) must use this root.
    fn arch_get_kernel_root() -> usize;
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
    /// Arch-provided: invalidate `pages` pages at `va` of the address space
    /// rooted at `root` (`usize::MAX` pages: all of it) on every CPU that may
    /// cache them. x86-64: local `invlpg` + an IPI only to CPUs with `root`
    /// in CR3; AArch64: broadcast `tlbi vaae1is`.
    fn arch_tlb_flush_range(root: usize, va: usize, pages: usize);
    /// Arch-provided: invalidate one page on this CPU only.
    fn arch_tlb_flush_local_page(va: usize);
    /// Arch-provided: perform a flush another CPU requested of this one
    /// (x86-64; no-op on AArch64). Called from IRQ-masked spin loops.
    fn arch_tlb_service_pending();
    /// Arch-provided: free every intermediate page-table node below the user
    /// root `page_table_root` (not the root itself, never the kernel's shared
    /// nodes). Returns the number of 4 KiB table pages released.
    fn arch_free_user_page_tables(page_table_root: usize) -> usize;
}

/// Release the intermediate page tables of a dead user address space. Every
/// leaf must already be unmapped or about to be discarded with the root: the
/// walk frees nodes, not frames. Returns the number of table pages freed.
pub unsafe fn free_user_page_tables(page_table_root: usize) -> usize {
    arch_free_user_page_tables(page_table_root)
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

/// Invalidate `[va, va + pages * 4 KiB)` of the address space rooted at
/// `root` on every CPU that may cache it — only those (x86-64 tracks which
/// CPUs have which root loaded). Waits, bounded, for remote completion.
pub fn tlb_flush_range(root: usize, va: usize, pages: usize) {
    if pages == 0 { return; }
    unsafe { arch_tlb_flush_range(root, va, pages); }
}

/// Invalidate every user translation of the address space rooted at `root`
/// on every CPU that may cache it.
pub fn tlb_flush_as(root: usize) {
    unsafe { arch_tlb_flush_range(root, 0, usize::MAX); }
}

/// Invalidate one page on this CPU only.
pub fn tlb_flush_local_page(va: usize) {
    unsafe { arch_tlb_flush_local_page(va); }
}

/// Perform any TLB flush another CPU is waiting for on this one. Spin loops
/// that run with IRQs masked call this so a shootdown initiator is not left
/// waiting on a CPU that is waiting on it.
#[inline]
pub fn tlb_service_pending() {
    unsafe { arch_tlb_service_pending(); }
}

pub fn get_current_root() -> usize {
    unsafe { arch_get_current_root() }
}

pub fn get_kernel_root() -> usize {
    unsafe { arch_get_kernel_root() }
}

/// Map a hardware device (MMIO) into the kernel virtual address space.
pub unsafe fn map_kernel_device(phys: usize, size: usize, flags: PageFlags) -> Option<usize> {
    // Must use the kernel root (TTBR1 on AArch64, CR3 on x86-64) because
    // phys_to_virt produces HHDM addresses which live in the kernel VA range.
    let root = get_kernel_root();
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

/// TLB-maintenance and CoW-promotion counters (always on: relaxed atomics).
/// Printed as a `[TLBSTAT]` delta line every 10 s by the BSP's timer tick
/// when anything changed (`sched::tlbstat_tick`).
pub mod tlbstat {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// Cross-CPU flush requests (every shootdown call, targeted or not).
    pub static FLUSHES: AtomicU64 = AtomicU64::new(0);
    /// Flush requests that needed at least one remote CPU (x86: an IPI).
    pub static REMOTE_FLUSHES: AtomicU64 = AtomicU64::new(0);
    /// Shootdown IPIs sent (a broadcast counts once per target CPU).
    pub static IPIS: AtomicU64 = AtomicU64::new(0);
    /// Time initiators spent waiting for remote acknowledgements.
    pub static WAIT_NS: AtomicU64 = AtomicU64::new(0);
    pub static WAIT_MAX_NS: AtomicU64 = AtomicU64::new(0);
    /// Waits that gave up before every target acknowledged.
    pub static TIMEOUTS: AtomicU64 = AtomicU64::new(0);
    /// Flush requests a lock spinner serviced itself (x86).
    pub static SERVICED: AtomicU64 = AtomicU64::new(0);
    /// CoW promotions that copied the page (frame changed), with cost.
    pub static COW_COPY: AtomicU64 = AtomicU64::new(0);
    pub static COW_COPY_NS: AtomicU64 = AtomicU64::new(0);
    pub static COW_COPY_MAX_NS: AtomicU64 = AtomicU64::new(0);
    /// CoW promotions that reused the frame in place (sole owner).
    pub static COW_REUSE: AtomicU64 = AtomicU64::new(0);
    /// execve: argv/envp prefault + collection, per call.
    pub static EXEC_PRE: AtomicU64 = AtomicU64::new(0);
    pub static EXEC_PRE_NS: AtomicU64 = AtomicU64::new(0);
    pub static EXEC_PRE_MAX_NS: AtomicU64 = AtomicU64::new(0);

    extern "C" { fn arch_monotonic_ns() -> u64; }

    #[inline]
    pub fn now_ns() -> u64 { unsafe { arch_monotonic_ns() } }

    #[inline]
    pub fn add(total: &AtomicU64, max: &AtomicU64, ns: u64) {
        total.fetch_add(ns, Relaxed);
        max.fetch_max(ns, Relaxed);
    }
}
