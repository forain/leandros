//! AArch64 page table management (4 KiB granule, 4-level translation).
//!
//! Implements the ARMv8-A VMSAv8-64 translation table format.
//! TTBR0_EL1 addresses user space; TTBR1_EL1 addresses the kernel.
//! We use a 48-bit VA size (256 TiB per half).

use bitflags::bitflags;

bitflags! {
    /// AArch64 stage 1 page descriptor bits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PageDescFlags: u64 {
        /// Entry is valid.
        const VALID     = 1 << 0;
        /// Entry is a table (L0-L2) or page (L3).  Must be 1 for L3 entries.
        const TABLE     = 1 << 1;
        /// Memory attributes index (MAIR_EL1).
        const ATTR_NORM    = 0 << 2; // index 0 (normal WB/WA)
        const ATTR_DEV     = 1 << 2; // index 1 (device nGnRE)
        const ATTR_STRICT  = 2 << 2; // index 2 (device nGnRnE)
        const ATTR_NOCACHE = 3 << 2; // index 3 (normal NC)
        /// Non-secure access.
        const NS        = 1 << 5;
        /// User (EL0) access allowed.
        const USER      = 1 << 6;
        /// Read-only access (for both EL0 and EL1).
        const RDONLY    = 1 << 7;
        /// Shareability: Inner Shareable.
        const INNER_SHR = 3 << 8;
        /// Access Flag: set to 1 to avoid access faults.
        const AF        = 1 << 10;
        /// Unprivileged Execute-Never.
        const UXN       = 1u64 << 53;
        /// Privileged Execute-Never.
        const PXN       = 1u64 << 54;
        /// Helper for NO_EXEC (sets both UXN and PXN).
        const NO_EXEC   = (1u64 << 53) | (1u64 << 54);
    }
}

// ── Low-level Page Table Walker ──────────────────────────────────────────────

/// Map a single 4 KiB page into the specified PGD.
///
/// # Safety
/// `pgd` must point to a valid, 4-KiB-aligned Level-0 (PGD) page table that
/// lies within a region addressable without MMU (identity-mapped or physical
/// address space).
pub unsafe fn map_4k(pgd_phys: *mut u64, virt: usize, phys: usize, flags: PageDescFlags) -> bool {
    let pgd = mm::phys_to_virt(pgd_phys as usize) as *mut u64;
    let l0 = (virt >> 39) & 0x1FF;
    let l1 = (virt >> 30) & 0x1FF;
    let l2 = (virt >> 21) & 0x1FF;
    let l3 = (virt >> 12) & 0x1FF;

    let p1 = match ensure_table(pgd, l0) { Some(p) => mm::phys_to_virt(p as usize) as *mut u64, None => return false };
    let p2 = match ensure_table(p1,  l1) { Some(p) => mm::phys_to_virt(p as usize) as *mut u64, None => return false };
    let p3 = match ensure_table(p2,  l2) { Some(p) => mm::phys_to_virt(p as usize) as *mut u64, None => return false };

    // L3 entry: page descriptor (bit 1 = 1, bit 0 = 1).
    let final_entry = phys as u64 | flags.bits() | 0b11;
    p3.add(l3).write(final_entry);

    true
}

/// Unmap a single 4 KiB page and invalidate its TLB entry.
///
/// # Safety
/// `pgd` must point to a valid PGD and `virt` must be 4-KiB aligned.
pub unsafe fn unmap_4k(pgd_phys: *mut u64, virt: usize) {
    let pgd = mm::phys_to_virt(pgd_phys as usize) as *mut u64;
    let l0 = (virt >> 39) & 0x1FF;
    let l1 = (virt >> 30) & 0x1FF;
    let l2 = (virt >> 21) & 0x1FF;
    let l3 = (virt >> 12) & 0x1FF;

    let e0 = pgd.add(l0).read();
    if e0 & PageDescFlags::VALID.bits() == 0 { return; }
    let p1 = mm::phys_to_virt((e0 & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;

    let e1 = p1.add(l1).read();
    if e1 & PageDescFlags::VALID.bits() == 0 { return; }
    let p2 = mm::phys_to_virt((e1 & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;

    let e2 = p2.add(l2).read();
    if e2 & PageDescFlags::VALID.bits() == 0 { return; }
    let p3 = mm::phys_to_virt((e2 & 0x0000_FFFF_FFFF_F000) as usize) as *mut u64;

    p3.add(l3).write(0);
}

/// Returns `Some(ptr)` if the intermediate table at `parent[idx]` exists or
/// was just allocated, or `None` on OOM.
unsafe fn ensure_table(parent: *mut u64, idx: usize) -> Option<*mut u64> {
    let entry = parent.add(idx).read();
    if entry & PageDescFlags::VALID.bits() != 0 {
        // If this is a block (not a table), we can't traverse deeper.
        // Bit 1 is 1 for table (L0-L2) or page (L3).
        if entry & 0b10 == 0 {
            return None;
        }
        // Table is already present; extract the physical address.
        return Some((entry & 0x0000_FFFF_FFFF_F000) as *mut u64);
    }
    let table_phys = mm::buddy::alloc(0)? as *mut u64;
    let table_virt = mm::phys_to_virt(table_phys as usize) as *mut u8;
    table_virt.write_bytes(0, mm::buddy::PAGE_SIZE);

    parent.add(idx).write(
        table_phys as u64 | PageDescFlags::TABLE.bits() | PageDescFlags::VALID.bits()
    );
    Some(table_phys)
}

// ── arch_map_page / arch_unmap_page ──────────────────────────────────────────

/// Broadcast a TLB invalidation for all user-space entries to all CPUs.
///
/// # SMP correctness requirement
///
/// `arch_set_page_table` only writes TTBR0_EL1 on the **current** CPU.  When a
/// thread's mappings are changed on CPU A, `tlb_shootdown_all` must be called to
/// ensure CPU B sees the changes.
#[no_mangle]
pub unsafe extern "C" fn arch_tlb_shootdown_all() {
    core::arch::asm!(
        "dsb ishst",
        "tlbi vmalle1is",   // broadcast across inner-shareable domain
        "dsb ish",
        "isb",
        options(nostack)
    );
}

/// Translate mm::PageFlags bits to AArch64 page-descriptor flags.
fn translate_flags(bits: u64) -> PageDescFlags {
    use mm::paging::PageFlags;
    let src = PageFlags::from_bits_truncate(bits);
    // Always-required bits for a valid page descriptor with access flag.
    let mut f = PageDescFlags::VALID | PageDescFlags::AF | PageDescFlags::INNER_SHR;
    if src.contains(PageFlags::USER)     { f |= PageDescFlags::USER; }
    if !src.contains(PageFlags::WRITABLE){ f |= PageDescFlags::RDONLY; }
    if !src.contains(PageFlags::EXECUTE) { f |= PageDescFlags::NO_EXEC; }
    if src.contains(PageFlags::NOCACHE)  { f |= PageDescFlags::ATTR_NOCACHE; } // MAIR index 3

    f
}

#[no_mangle]
pub unsafe extern "C" fn arch_map_page(
    page_table_root: usize,
    virt: usize,
    phys: usize,
    flags: u64,
) -> bool {
    map_4k(page_table_root as *mut u64, virt, phys, translate_flags(flags))
}

#[no_mangle]
pub unsafe extern "C" fn arch_unmap_page(page_table_root: usize, virt: usize) {
    unmap_4k(page_table_root as *mut u64, virt);
}

// ── arch_set_page_table ───────────────────────────────────────────────────────

/// Load `root` into TTBR0_EL1.
///
/// If `root` is 0 the TTBR0 is cleared (safe — kernel code uses TTBR1_EL1).
/// Called by the scheduler immediately before every `cpu_switch_to` into a
/// user task, and with 0 on return to the scheduler idle loop.
#[no_mangle]
pub unsafe extern "C" fn arch_set_page_table(root: usize) {
    core::arch::asm!(
        "msr ttbr0_el1, {r}",
        "isb",
        "tlbi vmalle1",
        "dsb nsh",
        "isb",
        r = in(reg) root as u64,
        options(nostack)
    );
}

// ── arch_alloc_page_table_root ────────────────────────────────────────────────

/// Allocate a new page-table root (Level 0) for a user address space.
///
/// Returns the physical address of the page, or 0 on OOM.
/// Called by `sched::spawn_user` via an `extern "C"` declaration.
#[no_mangle]
pub unsafe extern "C" fn arch_alloc_page_table_root() -> usize {
    match mm::buddy::alloc(0) {
        Some(phys) => {
            let virt = mm::phys_to_virt(phys) as *mut u8;
            virt.write_bytes(0, mm::buddy::PAGE_SIZE);

            // Map critical device regions (UART for debug output, GIC for interrupts)
            // Identity mapping for early boot/ret_to_user debug prints and IRQ handlers
            let device_flags = PageDescFlags::VALID | PageDescFlags::AF | PageDescFlags::INNER_SHR | PageDescFlags::ATTR_DEV;
            
            // 1. UART
            map_4k(phys as *mut u64, crate::uart::BASE, crate::uart::BASE, device_flags);
            
            // 2. GIC Distributor
            map_4k(phys as *mut u64, crate::gic::GICD_BASE, crate::gic::GICD_BASE, device_flags);
            
            // 3. GIC CPU Interface
            map_4k(phys as *mut u64, crate::gic::GICC_BASE, crate::gic::GICC_BASE, device_flags);

            phys
        }
        None => 0,
    }
}
