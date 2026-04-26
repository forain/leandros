//! x86-64 four-level page table (PML4 → PDPT → PD → PT, 4 KiB pages).
//!
//! Implements the IA-32e paging structures described in Intel SDM Vol 3A §4.5.

use bitflags::bitflags;

bitflags! {
    #[derive(Clone, Copy)]
    pub struct PageTableFlags: u64 {
        const PRESENT       = 1 << 0;
        const WRITABLE      = 1 << 1;
        const USER          = 1 << 2;
        const WRITE_THROUGH = 1 << 3;
        const NO_CACHE      = 1 << 4;
        const ACCESSED      = 1 << 5;
        const DIRTY         = 1 << 6;
        const HUGE          = 1 << 7;
        const NO_EXECUTE    = 1 << 63;
    }
}

pub const PAGE_SIZE: usize = 4096;

/// Map a single 4 KiB page.
///
/// `pml4_phys` is the PHYSICAL address of the PML4 root (as stored in CR3).
/// All intermediate table nodes are accessed via the HHDM so this function
/// is safe to call both before and after a user PML4 switch.
///
/// Returns `true` on success, `false` if an intermediate page-table node
/// could not be allocated (OOM).
pub unsafe fn map_4k(pml4_phys: usize, virt: usize, phys: usize, flags: PageTableFlags) -> bool {
    let pml4_idx = (virt >> 39) & 0x1FF;
    let pdpt_idx = (virt >> 30) & 0x1FF;
    let pd_idx   = (virt >> 21) & 0x1FF;
    let pt_idx   = (virt >> 12) & 0x1FF;

    let pml4 = mm::phys_to_virt(pml4_phys) as *mut u64;
    let pdpt = match ensure_table(pml4, pml4_idx, flags) { Some(p) => p, None => return false };
    let pd   = match ensure_table(pdpt, pdpt_idx, flags) { Some(p) => p, None => return false };
    let pt   = match ensure_table(pd,   pd_idx,   flags) { Some(p) => p, None => return false };

    pt.add(pt_idx).write(phys as u64 | flags.bits());
    true
}

/// Unmap a single 4 KiB page and flush the TLB entry.
///
/// `pml4_phys` is the PHYSICAL address of the PML4 root.
/// All table nodes are accessed via the HHDM.
pub unsafe fn unmap_4k(pml4_phys: usize, virt: usize) {
    let pml4_idx = (virt >> 39) & 0x1FF;
    let pdpt_idx = (virt >> 30) & 0x1FF;
    let pd_idx   = (virt >> 21) & 0x1FF;
    let pt_idx   = (virt >> 12) & 0x1FF;

    let pml4 = mm::phys_to_virt(pml4_phys) as *mut u64;
    let pdpt_entry = pml4.add(pml4_idx).read();
    if pdpt_entry & PageTableFlags::PRESENT.bits() == 0 { return; }
    let pdpt = mm::phys_to_virt((pdpt_entry & !0xFFF) as usize) as *mut u64;

    let pd_entry = pdpt.add(pdpt_idx).read();
    if pd_entry & PageTableFlags::PRESENT.bits() == 0 { return; }
    let pd = mm::phys_to_virt((pd_entry & !0xFFF) as usize) as *mut u64;

    let pt_entry = pd.add(pd_idx).read();
    if pt_entry & PageTableFlags::PRESENT.bits() == 0 { return; }
    let pt = mm::phys_to_virt((pt_entry & !0xFFF) as usize) as *mut u64;

    pt.add(pt_idx).write(0);

    // Flush the TLB entry for this virtual address.
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!("invlpg [{addr}]", addr = in(reg) virt, options(nostack));
}

/// Ensure an intermediate page-table node exists at `parent[idx]`, creating
/// it with a zeroed page if absent.
///
/// `parent` is a VIRTUAL (HHDM) pointer to the current level table.
/// Returns a VIRTUAL (HHDM) pointer to the next-level table, or `None` on OOM.
/// Intermediate PTE entries store PHYSICAL addresses (as the hardware expects).
///
/// If the entry already exists, its R/W and U/S flags are OR'd with the
/// requested flags — because all intermediate levels must reflect the union of
/// permissions needed by any child mapping.  For example, if PML4[0] was first
/// created for a read-only segment (R/W=0), a later writable mapping (stack)
/// that shares the same PML4[0] entry would be silently denied writes unless
/// the intermediate entry is upgraded here.
unsafe fn ensure_table(parent: *mut u64, idx: usize, flags: PageTableFlags) -> Option<*mut u64> {
    // Strip NO_EXECUTE; keep only P/W/U for intermediate walk entries.
    let intermediate_flags = flags & (PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER);

    let entry = parent.add(idx).read();
    if entry & PageTableFlags::PRESENT.bits() != 0 {
        // Upgrade the existing entry with any newly required W/U bits.
        let upgraded = entry | intermediate_flags.bits();
        if upgraded != entry {
            parent.add(idx).write(upgraded);
        }
        let next_phys = (entry & !0xFFF) as usize;
        return Some(mm::phys_to_virt(next_phys) as *mut u64);
    }
    let table_phys = alloc_zeroed_page()?;
    parent.add(idx).write(table_phys as u64 | intermediate_flags.bits());
    Some(mm::phys_to_virt(table_phys) as *mut u64)
}

/// Allocate and zero a 4 KiB page for an intermediate page-table node.
/// Zeros it via the HHDM virtual address.
/// Returns the PHYSICAL address (for storage in parent PTE), or `None` on OOM.
unsafe fn alloc_zeroed_page() -> Option<usize> {
    let phys = mm::buddy::alloc(0)?;
    let virt = mm::phys_to_virt(phys) as *mut u8;
    virt.write_bytes(0, mm::buddy::PAGE_SIZE);
    Some(phys)
}

// ── arch_tlb_shootdown_all ────────────────────────────────────────────────────

/// Broadcast a TLB invalidation for all user-space entries to all CPUs.
///
/// # SMP correctness requirement
///
/// `arch_set_page_table` only writes CR3 on the **current** CPU.  On SMP,
/// unmapping a page on CPU A while other CPUs may have cached translations for
/// the same virtual address requires a TLB shootdown IPI.
///
/// **Current implementation**: single-CPU stub that reloads CR3 to flush the
/// local TLB only.
/// On a production SMP system this must:
///   1. Collect the set of CPUs running threads that share the affected page table.
///   2. Send an IPI (e.g. APIC vector 0xFE) to those CPUs.
///   3. Each receiving CPU executes `invlpg` or reloads CR3.
///   4. Wait for all CPUs to acknowledge before returning.
#[no_mangle]
pub unsafe extern "C" fn arch_tlb_shootdown_all() {
    // Reload CR3 to flush local TLB; on SMP an IPI to other CPUs is also needed.
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!(
        "mov {tmp}, cr3",
        "mov cr3, {tmp}",
        tmp = out(reg) _,
        options(nostack)
    );
}

// ── arch_set_page_table ───────────────────────────────────────────────────────

/// Load `root` into CR3.
///
/// If `root` is 0 we leave CR3 unchanged — the kernel identity map stays
/// active and there is no user-space mapping to switch away from.
/// Called by the scheduler immediately before every `cpu_switch_to` into a
/// user task, and with 0 on return to the scheduler idle loop.
#[no_mangle]
pub unsafe extern "C" fn arch_set_page_table(root: usize) {
    if root != 0 {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!(
            "mov cr3, {r}",
            r = in(reg) root as u64,
            options(nostack)
        );
    }
}

// ── arch_alloc_page_table_root ────────────────────────────────────────────────

/// Walk one page-table level for diagnostics.
/// Returns the physical address bits + flags of the entry, or 0 if not present.
unsafe fn pt_entry(table_phys: usize, hhdm: usize, idx: usize) -> u64 {
    let table = (table_phys + hhdm) as *const u64;
    table.add(idx).read()
}

/// Print a hex64 value to the COM1 serial port.
unsafe fn pt_print_hex64(v: u64) {
    for i in (0..16).rev() {
        let nibble = ((v >> (i * 4)) & 0xF) as u8;
        crate::arch_serial_putc(if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 });
    }
}

/// Walk and print the 4-level page-table chain for `virt` using `pml4_phys`.
/// Prints each entry and whether it has the XD (NX) bit set.
pub unsafe fn debug_walk_pte(pml4_phys: usize, virt: usize) {
    let hhdm    = mm::phys_to_virt(0);
    let idx4    = (virt >> 39) & 0x1FF;
    let idx3    = (virt >> 30) & 0x1FF;
    let idx2    = (virt >> 21) & 0x1FF;
    let idx1    = (virt >> 12) & 0x1FF;

    // PML4
    let e4 = pt_entry(pml4_phys, hhdm, idx4);
    for b in b"  PML4[" { crate::arch_serial_putc(*b); }
    pt_print_hex64(idx4 as u64);
    for b in b"]=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(e4);
    if e4 >> 63 != 0 { for b in b" XD!" { crate::arch_serial_putc(*b); } }
    crate::arch_serial_putc(b'\n');
    if e4 & 1 == 0 { return; }

    // PDPT
    let pdpt_phys = (e4 & 0x000F_FFFF_FFFF_F000) as usize;
    let e3 = pt_entry(pdpt_phys, hhdm, idx3);
    for b in b"  PDPT[" { crate::arch_serial_putc(*b); }
    pt_print_hex64(idx3 as u64);
    for b in b"]=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(e3);
    if e3 >> 63 != 0 { for b in b" XD!" { crate::arch_serial_putc(*b); } }
    crate::arch_serial_putc(b'\n');
    if e3 & 1 == 0 { return; }
    if e3 & (1 << 7) != 0 { for b in b"  (1GB page)\n" { crate::arch_serial_putc(*b); } return; }

    // PD
    let pd_phys = (e3 & 0x000F_FFFF_FFFF_F000) as usize;
    let e2 = pt_entry(pd_phys, hhdm, idx2);
    for b in b"  PD[" { crate::arch_serial_putc(*b); }
    pt_print_hex64(idx2 as u64);
    for b in b"]=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(e2);
    if e2 >> 63 != 0 { for b in b" XD!" { crate::arch_serial_putc(*b); } }
    crate::arch_serial_putc(b'\n');
    if e2 & 1 == 0 { return; }
    if e2 & (1 << 7) != 0 { for b in b"  (2MB page)\n" { crate::arch_serial_putc(*b); } return; }

    // PT
    let pt_phys = (e2 & 0x000F_FFFF_FFFF_F000) as usize;
    let e1 = pt_entry(pt_phys, hhdm, idx1);
    for b in b"  PT[" { crate::arch_serial_putc(*b); }
    pt_print_hex64(idx1 as u64);
    for b in b"]=0x" { crate::arch_serial_putc(*b); }
    pt_print_hex64(e1);
    if e1 >> 63 != 0 { for b in b" XD!" { crate::arch_serial_putc(*b); } }
    crate::arch_serial_putc(b'\n');
}

/// Allocate a zeroed 4 KiB page to serve as a process's PML4 root.
///
/// Returns the physical address of the page, or 0 on OOM.
/// Called by `sched::spawn_user` via an `extern "C"` declaration.
#[no_mangle]
pub unsafe extern "C" fn arch_alloc_page_table_root() -> usize {
    match mm::buddy::alloc(0) {
        Some(phys) => {
            let hhdm_offset = mm::phys_to_virt(0);
            let new_pml4 = (phys + hhdm_offset) as *mut u64;

            for i in 0..512 {
                new_pml4.add(i).write(0);
            }

            // Read CR3; mask off flag bits [11:0] to get the physical address.
            let cr3_raw: usize;
            core::arch::asm!("mov {}, cr3", out(reg) cr3_raw, options(nomem, nostack));
            let cr3_phys = cr3_raw & !0xFFF;

            let src_pml4 = (cr3_phys + hhdm_offset) as *const u64;
            for i in 256..512 {
                new_pml4.add(i).write(src_pml4.add(i).read());
            }

            phys
        }
        None => 0,
    }
}

// ── arch_map_page / arch_unmap_page ──────────────────────────────────────────
// Resolved at link time by mm::paging — no circular crate dependency.

/// Translate mm::PageFlags bits to x86-64 page-table flags.
fn translate_flags(bits: u64) -> PageTableFlags {
    use mm::paging::PageFlags;
    let src = PageFlags::from_bits_truncate(bits);
    let mut f = PageTableFlags::empty();
    if src.contains(PageFlags::PRESENT)  { f |= PageTableFlags::PRESENT; }
    if src.contains(PageFlags::WRITABLE) { f |= PageTableFlags::WRITABLE; }
    if src.contains(PageFlags::USER)     { f |= PageTableFlags::USER; }
    if src.contains(PageFlags::NOCACHE)  { f |= PageTableFlags::NO_CACHE; }
    // NO_EXECUTE if EXECUTE is NOT requested.
    if !src.contains(PageFlags::EXECUTE) { f |= PageTableFlags::NO_EXECUTE; }
    f
}

#[no_mangle]
pub unsafe extern "C" fn arch_map_page(
    page_table_root: usize, // physical address of PML4
    virt: usize,
    phys: usize,
    flags: u64,
) -> bool {
    let f = translate_flags(flags);
    /*
    if virt >= 0x200000 && virt < 0x300000 {
        for b in b"[PGT] Mapping 0x" { crate::arch_serial_putc(*b); }
        pt_print_hex64(virt as u64);
        for b in b" to 0x" { crate::arch_serial_putc(*b); }
        pt_print_hex64(phys as u64);
        for b in b" flags=0x" { crate::arch_serial_putc(*b); }
        pt_print_hex64(f.bits());
        crate::arch_serial_putc(b'\n');
    }
    */
    map_4k(page_table_root, virt, phys, f)
}

#[no_mangle]
pub unsafe extern "C" fn arch_unmap_page(page_table_root: usize, virt: usize) {
    unmap_4k(page_table_root, virt);
}
