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

// ── FADT power-management registers ───────────────────────────────────────────

/// What the kernel needs from the FADT (and the DSDT's `\_S5_` object) to
/// power the machine off and reset it the ACPI way — the same registers
/// Linux's `acpi_enter_sleep_state(ACPI_STATE_S5)` and `acpi_reboot()` use.
/// Port numbers are 0 when the table does not provide that register.
#[derive(Clone, Copy, Debug, Default)]
pub struct AcpiPower {
    /// PM1a/PM1b control blocks (System I/O ports).
    pub pm1a_cnt: u16,
    pub pm1b_cnt: u16,
    /// `\_S5_` SLP_TYPa / SLP_TYPb (3-bit values for PM1x_CNT bits 10..12).
    pub slp_typ_a: u8,
    pub slp_typ_b: u8,
    /// `\_S5_` was found in the DSDT (otherwise the SLP_TYP values are the
    /// QEMU/most-firmware default of 0).
    pub s5_found: bool,
    /// SMI command port and the value that hands the chipset to the OS
    /// (sets SCI_EN), for firmware that boots in legacy mode.
    pub smi_cmd: u16,
    pub acpi_enable: u8,
    /// FADT RESET_REG, when `RESET_REG_SUP` is set and it is in System I/O
    /// space (the only kind this kernel writes): port and value.
    pub reset_port: u16,
    pub reset_value: u8,
    /// FADT flags: HW_REDUCED_ACPI (no PM1 blocks — arm64 servers).
    pub hw_reduced: bool,
    /// FADT ARM_BOOT_ARCH: PSCI_COMPLIANT / PSCI_USE_HVC (bits 0/1).
    pub arm_boot_arch: u16,
}

const FADT_FLAG_RESET_REG_SUP: u32 = 1 << 10;
const FADT_FLAG_HW_REDUCED:    u32 = 1 << 20;
const GAS_SYSTEM_IO: u8 = 1;

#[inline]
unsafe fn read_u16(p: *const u8) -> u16 {
    u16::from_le_bytes([*p, *p.add(1)])
}

/// Find a table by signature in the XSDT (or the RSDT for revision-0 RSDPs).
/// Returns its *virtual* address.
unsafe fn find_table(rsdp_phys: u64, hhdm_offset: u64, sig: &[u8; 4]) -> Option<*const u8> {
    if rsdp_phys == 0 { return None; }
    let rsdp = (rsdp_phys + hhdm_offset) as *const u8;
    if core::slice::from_raw_parts(rsdp, 8) != RSDP_SIG { return None; }
    let revision = *rsdp.add(15);
    let (root_phys, entry_size, root_sig): (u64, usize, &[u8; 4]) = if revision >= 2 && read_u64(rsdp.add(24)) != 0 {
        (read_u64(rsdp.add(24)), 8, b"XSDT")
    } else {
        (read_u32(rsdp.add(16)) as u64, 4, b"RSDT")
    };
    if root_phys == 0 { return None; }
    let root = (root_phys + hhdm_offset) as *const u8;
    if core::slice::from_raw_parts(root, 4) != root_sig { return None; }
    let len = read_u32(root.add(4)) as usize;
    if len < 36 || len > 1 << 20 { return None; }
    for i in 0..(len - 36) / entry_size {
        let e = root.add(36 + i * entry_size);
        let phys = if entry_size == 8 { read_u64(e) } else { read_u32(e) as u64 };
        if phys == 0 { continue; }
        let t = (phys + hhdm_offset) as *const u8;
        if core::slice::from_raw_parts(t, 4) == sig { return Some(t); }
    }
    None
}

/// Parse the FADT and the DSDT's `\_S5_` package. `None` when there is no
/// usable FADT (no RSDP, or a table that fails its signature check).
///
/// # Safety
/// The ACPI tables must be readable through the HHDM at `hhdm_offset`.
pub unsafe fn find_power_info(rsdp_phys: u64, hhdm_offset: u64) -> Option<AcpiPower> {
    let fadt = find_table(rsdp_phys, hhdm_offset, b"FACP")?;
    let len = read_u32(fadt.add(4)) as usize;
    if len < 116 { return None; }
    let mut p = AcpiPower::default();
    let flags = read_u32(fadt.add(112));
    p.hw_reduced = flags & FADT_FLAG_HW_REDUCED != 0;
    p.smi_cmd = read_u32(fadt.add(48)) as u16;
    p.acpi_enable = *fadt.add(52);
    p.pm1a_cnt = read_u32(fadt.add(64)) as u16;
    p.pm1b_cnt = read_u32(fadt.add(68)) as u16;
    // ACPI 2.0+ extended blocks win when they are present and in I/O space.
    if len >= 196 {
        if *fadt.add(172) == GAS_SYSTEM_IO && read_u64(fadt.add(176)) != 0 {
            p.pm1a_cnt = read_u64(fadt.add(176)) as u16;
        }
        if *fadt.add(184) == GAS_SYSTEM_IO && read_u64(fadt.add(188)) != 0 {
            p.pm1b_cnt = read_u64(fadt.add(188)) as u16;
        }
    }
    if len >= 129 && flags & FADT_FLAG_RESET_REG_SUP != 0
        && *fadt.add(116) == GAS_SYSTEM_IO && read_u64(fadt.add(120)) != 0 {
        p.reset_port = read_u64(fadt.add(120)) as u16;
        p.reset_value = *fadt.add(128);
    }
    if len >= 131 { p.arm_boot_arch = read_u16(fadt.add(129)); }

    // DSDT: X_DSDT (64-bit) when present, else the 32-bit DSDT field.
    let mut dsdt_phys = read_u32(fadt.add(40)) as u64;
    if len >= 148 && read_u64(fadt.add(140)) != 0 { dsdt_phys = read_u64(fadt.add(140)); }
    if dsdt_phys != 0 {
        let dsdt = (dsdt_phys + hhdm_offset) as *const u8;
        if core::slice::from_raw_parts(dsdt, 4) == b"DSDT" {
            let dlen = read_u32(dsdt.add(4)) as usize;
            if dlen > 36 && dlen < 4 << 20 {
                let aml = core::slice::from_raw_parts(dsdt, dlen);
                if let Some((a, b)) = find_s5(aml) {
                    p.slp_typ_a = a;
                    p.slp_typ_b = b;
                    p.s5_found = true;
                }
            }
        }
    }
    Some(p)
}

/// Locate `Name(_S5_, Package(){SLP_TYPa, SLP_TYPb, ...})` in raw AML — the
/// standard minimal-OS shortcut that avoids a full AML interpreter. QEMU's
/// DSDT encodes it as `08 5F 53 35 5F 12 06 04 00 00 00 00`.
fn find_s5(aml: &[u8]) -> Option<(u8, u8)> {
    let mut i = 36;
    while i + 4 < aml.len() {
        if &aml[i..i + 4] == b"_S5_" {
            // Preceded by NameOp (0x08), optionally with a root prefix '\'.
            let named = (i >= 1 && aml[i - 1] == 0x08)
                || (i >= 2 && aml[i - 1] == b'\\' && aml[i - 2] == 0x08);
            let mut j = i + 4;
            if named && j < aml.len() && aml[j] == 0x12 {
                j += 1; // PackageOp
                if j >= aml.len() { return None; }
                let extra = (aml[j] >> 6) as usize; // PkgLength follow bytes
                j += 1 + extra;
                j += 1; // NumElements
                let mut vals = [0u8; 2];
                for v in vals.iter_mut() {
                    if j >= aml.len() { return None; }
                    match aml[j] {
                        0x0A => { // BytePrefix
                            if j + 1 >= aml.len() { return None; }
                            *v = aml[j + 1];
                            j += 2;
                        }
                        0x00 => { *v = 0; j += 1; } // ZeroOp
                        0x01 => { *v = 1; j += 1; } // OneOp
                        b => { *v = b; j += 1; }
                    }
                }
                return Some((vals[0] & 7, vals[1] & 7));
            }
        }
        i += 1;
    }
    None
}
