//! Minimal ACPI table parser — extracts the PCI ECAM base from the MCFG table.
//!
//! Walk: RSDP → XSDT → MCFG → first allocation record → BaseAddress.
//! All addresses passed in are *virtual* (HHDM-mapped).

/// RSDP/XSDP signature (without null terminator).
const RSDP_SIG: &[u8; 8] = b"RSD PTR ";

/// Read a little-endian u32 from a raw byte pointer.
#[inline]
unsafe fn read_u32(p: *const u8) -> u32 {
    u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)])
}

/// Read a little-endian u64 from a raw byte pointer.
#[inline]
unsafe fn read_u64(p: *const u8) -> u64 {
    u64::from_le_bytes([
        *p, *p.add(1), *p.add(2), *p.add(3),
        *p.add(4), *p.add(5), *p.add(6), *p.add(7),
    ])
}

/// Parse the PCI ECAM base address from ACPI tables.
///
/// # Arguments
/// * `rsdp_phys` — physical address of the RSDP, as reported by the firmware.
/// * `hhdm_offset` — kernel HHDM offset to convert physical → virtual.
///
/// Returns the physical base address of the first MCFG ECAM window, or 0.
///
/// # Safety
/// `rsdp_phys` must be a valid physical address accessible via `rsdp_phys + hhdm_offset`.
pub unsafe fn find_ecam_base(rsdp_phys: u64, hhdm_offset: u64) -> u64 {
    if rsdp_phys == 0 {
        return 0;
    }

    let rsdp = (rsdp_phys + hhdm_offset) as *const u8;

    // Validate RSDP signature.
    if core::slice::from_raw_parts(rsdp, 8) != RSDP_SIG {
        return 0;
    }

    // Prefer XSDT (revision >= 2) over RSDT.
    let revision = *rsdp.add(15);
    let xsdt_phys: u64 = if revision >= 2 {
        read_u64(rsdp.add(24))
    } else {
        // RSDT uses 32-bit pointers; we don't support RSDT-only systems here.
        return 0;
    };

    if xsdt_phys == 0 {
        return 0;
    }

    let xsdt = (xsdt_phys + hhdm_offset) as *const u8;

    // Validate XSDT signature.
    if core::slice::from_raw_parts(xsdt, 4) != b"XSDT" {
        return 0;
    }

    let xsdt_len = read_u32(xsdt.add(4)) as usize;
    if xsdt_len < 36 {
        return 0;
    }

    // Entry array starts at offset 36; each entry is an 8-byte physical pointer.
    let num_entries = (xsdt_len - 36) / 8;
    for i in 0..num_entries {
        let entry_phys = read_u64(xsdt.add(36 + i * 8));
        if entry_phys == 0 {
            continue;
        }

        let table = (entry_phys + hhdm_offset) as *const u8;
        let sig = core::slice::from_raw_parts(table, 4);
        if sig != b"MCFG" {
            continue;
        }

        // Found MCFG. First allocation record starts at offset 44 (36-byte SDT
        // header + 8 reserved bytes). Each record is 16 bytes.
        let mcfg_len = read_u32(table.add(4)) as usize;
        if mcfg_len < 44 + 16 {
            return 0;
        }

        // First allocation: BaseAddress at offset 44.
        return read_u64(table.add(44));
    }

    0
}
