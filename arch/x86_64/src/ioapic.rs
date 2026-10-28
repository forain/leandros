//! I/O APIC driver.
//!
//! Configures the IOAPIC to route legacy IRQs to the Local APIC.

use crate::paging;
use core::sync::atomic::{AtomicU32, Ordering};

const IOAPIC_BASE_PHYS: usize = 0xFEC0_0000;
static mut IOAPIC_BASE_VIRT: usize = 0;

unsafe fn read(reg: u8) -> u32 {
    let base = IOAPIC_BASE_VIRT as *mut u32;
    base.write_volatile(reg as u32);
    base.add(4).read_volatile()
}

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

/// MADT Interrupt Source Overrides for ISA IRQs 0-15, as
/// `VALID | flags << 16 | gsi` (0 = identity-mapped, ISA defaults).
static ISO: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
const ISO_VALID: u32 = 1 << 31;

/// Read the MADT's ISA Interrupt Source Overrides (before any `set_isa_irq`).
pub unsafe fn load_overrides(rsdp_phys: u64, hhdm_offset: u64) {
    if rsdp_phys == 0 { return; }
    boot::acpi::for_each_madt_iso(rsdp_phys, hhdm_offset, &mut |irq, gsi, flags| {
        if irq < 16 && gsi < 0x1_0000 {
            ISO[irq as usize].store(ISO_VALID | (flags as u32 & 0xF) << 16 | gsi, Ordering::Relaxed);
        }
    });
}

/// ISA IRQ -> (GSI, redirection-entry polarity/trigger bits) under an
/// optional override `(gsi, MPS INTI flags)`: polarity bits 0-1 (11 = active
/// low -> bit 13), trigger bits 2-3 (11 = level -> bit 15); 00 ("conforms")
/// keeps the ISA defaults, active high and edge.
pub fn isa_redirection(irq: u8, iso: Option<(u32, u16)>) -> (u32, u32) {
    match iso {
        None => (irq as u32, 0),
        Some((gsi, flags)) => {
            let mut bits = 0;
            if flags & 0x3 == 0x3 { bits |= 1 << 13; }
            if (flags >> 2) & 0x3 == 0x3 { bits |= 1 << 15; }
            (gsi, bits)
        }
    }
}

/// [`isa_redirection`] with the overrides [`load_overrides`] read.
pub fn isa_route(irq: u8) -> (u32, u32) {
    let v = if irq < 16 { ISO[irq as usize].load(Ordering::Relaxed) } else { 0 };
    let iso = if v & ISO_VALID != 0 { Some((v & 0xFFFF, ((v >> 16) & 0xF) as u16)) } else { None };
    isa_redirection(irq, iso)
}

/// Route ISA IRQ `irq` through the MADT overrides (GSI, polarity, trigger).
pub unsafe fn set_isa_irq(irq: u8, apic_id: u8, vector: u8) {
    let (gsi, bits) = isa_route(irq);
    let n = ((read(0x01) >> 16) & 0xFF) + 1;
    if gsi >= n.min(120) { return; }
    let gsi = gsi as u8;
    write(0x10 + gsi * 2 + 1, (apic_id as u32) << 24);
    write(0x10 + gsi * 2, vector as u32 | bits);
}

/// Mask ISA IRQ `irq`'s redirection entry (through the overrides).
pub unsafe fn mask_isa_irq(irq: u8, vector: u8) {
    let (gsi, _) = isa_route(irq);
    if gsi < 120 { mask_irq(gsi as u8, vector); }
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

/// Mask a GSI's redirection entry (vector kept, delivery off).
pub unsafe fn mask_irq(gsi: u8, vector: u8) {
    if IOAPIC_BASE_VIRT == 0 { return; }
    write(0x10 + gsi * 2, (1 << 16) | vector as u32);
}

/// kexec: mask every redirection entry (vector kept), so no stale route
/// fires into the next kernel before its own `set_irq` calls. No locks.
pub unsafe fn mask_all() {
    if IOAPIC_BASE_VIRT == 0 { return; }
    let n = ((read(0x01) >> 16) & 0xFF) + 1;
    for i in 0..n.min(120) as u8 {
        let lo = read(0x10 + i * 2);
        write(0x10 + i * 2, lo | (1 << 16));
    }
}

/// Saved IOAPIC state for S3 (ID register + every redirection entry).
pub struct PmState { id: u32, n: u8, rte: [u64; 120] }

pub unsafe fn pm_save() -> Option<PmState> {
    if IOAPIC_BASE_VIRT == 0 { return None; }
    let n = (((read(0x01) >> 16) & 0xFF) + 1).min(120) as u8;
    let mut s = PmState { id: read(0x00), n, rte: [0; 120] };
    for i in 0..n {
        s.rte[i as usize] = (read(0x10 + i * 2) as u64) | ((read(0x11 + i * 2) as u64) << 32);
    }
    Some(s)
}

/// Write the ID, then every entry masked, then the saved values (Linux order).
pub unsafe fn pm_restore(s: &PmState) {
    if IOAPIC_BASE_VIRT == 0 { return; }
    write(0x00, s.id);
    for i in 0..s.n {
        let v = s.rte[i as usize];
        write(0x10 + i * 2, (v as u32) | (1 << 16));
        write(0x11 + i * 2, (v >> 32) as u32);
    }
    for i in 0..s.n {
        write(0x10 + i * 2, s.rte[i as usize] as u32);
    }
}
