//! I/O APIC driver.
//!
//! Configures the IOAPIC to route legacy IRQs to the Local APIC.

use crate::paging;

const IOAPIC_BASE_PHYS: usize = 0xFEC0_0000;
static mut IOAPIC_BASE_VIRT: usize = 0;

unsafe fn write(reg: u8, val: u32) {
    let base = IOAPIC_BASE_VIRT as *mut u32;
    base.write_volatile(reg as u32);
    base.add(4).write_volatile(val);
}

/// Initialise the IOAPIC and map it into the page tables.
pub unsafe fn init(hhdm_offset: u64, pt_root: usize) {
    IOAPIC_BASE_VIRT = IOAPIC_BASE_PHYS + hhdm_offset as usize;
    
    // Map the IOAPIC MMIO region
    paging::map_4k(
        pt_root,
        IOAPIC_BASE_VIRT,
        IOAPIC_BASE_PHYS,
        paging::PageTableFlags::PRESENT | paging::PageTableFlags::WRITABLE | paging::PageTableFlags::NO_CACHE
    );
    
    // Flush TLB
    core::arch::asm!("mov rax, cr3", "mov cr3, rax", out("rax") _);
}

/// Route a Global System Interrupt (GSI) to a specific LAPIC with a given vector.
pub unsafe fn set_irq(gsi: u8, apic_id: u8, vector: u8) {
    let low_index = 0x10 + (gsi * 2);
    let high_index = 0x10 + (gsi * 2) + 1;

    // Destination LAPIC ID in bits 56:59
    let high_val = (apic_id as u32) << 24;
    
    // Active high, edge triggered, unmasked, fixed delivery mode
    let low_val = vector as u32;

    write(high_index, high_val);
    write(low_index, low_val);
}
