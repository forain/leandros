//! ACPI platform description for aarch64 (no DTB): MADT, SPCR, GTDT, MCFG, FADT.
//!
//! Every parser is a pure function over a table's bytes so the host tests in
//! `boot/bootparse-tests` can feed it synthetic tables. `discover` walks the
//! XSDT (through `acpi::find_table`) and fills a `Platform`, which the arch
//! layer turns into its runtime GIC / UART / timer / PSCI values.

pub const MAX_GICR: usize = 8;
pub const MAX_ITS: usize = 8;
pub const MAX_ECAM: usize = 8;
pub const MAX_GICC: usize = 512;

fn u16_at(t: &[u8], o: usize) -> u16 { u16::from_le_bytes([t[o], t[o + 1]]) }
fn u32_at(t: &[u8], o: usize) -> u32 { u32::from_le_bytes([t[o], t[o + 1], t[o + 2], t[o + 3]]) }
fn u64_at(t: &[u8], o: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&t[o..o + 8]);
    u64::from_le_bytes(b)
}

// ── MADT ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Madt {
    /// GICD physical base (0 = no distributor entry) and architecture
    /// version (1..=4, 0 = unspecified).
    pub gicd_base: u64,
    pub gic_version: u8,
    /// GICR discovery ranges (GICR structure, type 0x0E): (base, length).
    pub gicr: [(u64, u32); MAX_GICR],
    pub gicr_n: usize,
    /// GICv2 CPU interface base from the first GICC entry (0 on GICv3).
    pub gicc_base: u64,
    /// Per-GICC redistributor base (GICv3 without a GICR structure).
    pub gicc_gicr_base: u64,
    /// GIC ITS bases (type 0x0F).
    pub its: [u64; MAX_ITS],
    pub its_n: usize,
    /// Number of enabled GICC entries (CPUs).
    pub cpus: usize,
}

pub fn parse_madt(t: &[u8]) -> Option<Madt> {
    if t.len() < 44 || &t[0..4] != b"APIC" { return None; }
    let len = (u32_at(t, 4) as usize).min(t.len());
    let mut m = Madt::default();
    let mut off = 44;
    while off + 2 <= len {
        let kind = t[off];
        let elen = t[off + 1] as usize;
        if elen < 2 || off + elen > len { break; }
        let e = &t[off..off + elen];
        match kind {
            0x0B if elen >= 80 => {
                if u32_at(e, 12) & 1 != 0 { m.cpus += 1; }
                if m.gicc_base == 0 { m.gicc_base = u64_at(e, 32); }
                if m.gicc_gicr_base == 0 { m.gicc_gicr_base = u64_at(e, 60); }
            }
            0x0C if elen >= 24 => {
                if m.gicd_base == 0 {
                    m.gicd_base = u64_at(e, 8);
                    m.gic_version = e[20];
                }
            }
            0x0E if elen >= 16 && m.gicr_n < MAX_GICR => {
                m.gicr[m.gicr_n] = (u64_at(e, 4), u32_at(e, 12));
                m.gicr_n += 1;
            }
            0x0F if elen >= 20 && m.its_n < MAX_ITS => {
                m.its[m.its_n] = u64_at(e, 8);
                m.its_n += 1;
            }
            _ => {}
        }
        off += elen;
    }
    Some(m)
}

// ── SPCR ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UartKind {
    /// ARM PL011 (SPCR interface 3 = PL011-class "ARM PL011", 4, 5 = SBSA 32-bit, 0xE = SBSA generic).
    Pl011,
    /// 16550-compatible (interface 0, 1, 3 = DBGP rev 1 is treated as 16550 too, 0x12 = 16550 with 32-bit regs).
    Ns16550,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spcr {
    pub kind: UartKind,
    pub base: u64,
    /// Register stride shift: 0 for byte registers, 2 for 32-bit spaced.
    pub reg_shift: u8,
    /// Register access width in bytes (1, 2 or 4).
    pub access_bytes: u8,
    /// GSI (0 = none / polled).
    pub gsi: u32,
    /// Baud rate code (3 = 9600, 4 = 19200, 6 = 57600, 7 = 115200, 0 = as configured).
    pub baud_code: u8,
}

pub fn parse_spcr(t: &[u8]) -> Option<Spcr> {
    if t.len() < 52 || &t[0..4] != b"SPCR" { return None; }
    let iface = t[36];
    let space = t[40];
    if space != 0 { return None; } // system memory only; port IO has no meaning on aarch64
    let bit_width = t[41];
    let access = t[43]; // 0 undefined, 1 byte, 2 word, 3 dword, 4 qword
    let base = u64_at(t, 44);
    if base == 0 { return None; }
    let kind = match iface {
        0x00 | 0x01 | 0x03 | 0x12 => UartKind::Ns16550,
        0x04 | 0x05 | 0x0E => UartKind::Pl011,
        _ => return None,
    };
    let access_bytes = match access { 1 => 1, 2 => 2, 3 => 4, _ => if bit_width >= 32 { 4 } else { 1 } };
    let reg_shift = match access_bytes { 4 => 2, 2 => 1, _ => 0 };
    let gsi = if t.len() >= 58 && t[52] & 0x08 != 0 { u32_at(t, 54) } else { 0 };
    let baud_code = if t.len() > 58 { t[58] } else { 0 };
    Some(Spcr { kind, base, reg_shift, access_bytes, gsi, baud_code })
}

// ── GTDT ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gtdt {
    pub secure_el1: u32,
    pub ns_el1: u32,
    pub virt: u32,
    pub el2: u32,
}

/// Timer interrupts as GIC INTIDs (GTDT carries the GSIV, which is the INTID).
pub fn parse_gtdt(t: &[u8]) -> Option<Gtdt> {
    if t.len() < 76 || &t[0..4] != b"GTDT" { return None; }
    Some(Gtdt { secure_el1: u32_at(t, 48), ns_el1: u32_at(t, 56), virt: u32_at(t, 64), el2: u32_at(t, 72) })
}

// ── MCFG ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct EcamRegion { pub base: u64, pub segment: u16, pub start_bus: u8, pub end_bus: u8 }

pub fn parse_mcfg(t: &[u8], out: &mut [EcamRegion; MAX_ECAM]) -> usize {
    if t.len() < 44 || &t[0..4] != b"MCFG" { return 0; }
    let len = (u32_at(t, 4) as usize).min(t.len());
    let mut n = 0;
    let mut off = 44;
    while off + 16 <= len && n < MAX_ECAM {
        out[n] = EcamRegion { base: u64_at(t, off), segment: u16_at(t, off + 8), start_bus: t[off + 10], end_bus: t[off + 11] };
        n += 1;
        off += 16;
    }
    n
}

// ── FADT ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Psci { None, Smc, Hvc }

/// ARM_BOOT_ARCH (FADT offset 129): bit 0 PSCI_COMPLIANT, bit 1 PSCI_USE_HVC.
pub fn parse_fadt_psci(t: &[u8]) -> Psci {
    if t.len() < 131 || &t[0..4] != b"FACP" { return Psci::None; }
    let f = u16_at(t, 129);
    if f & 1 == 0 { Psci::None } else if f & 2 != 0 { Psci::Hvc } else { Psci::Smc }
}

// ── Whole platform ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct Platform {
    pub madt: Option<Madt>,
    pub spcr: Option<Spcr>,
    pub gtdt: Option<Gtdt>,
    pub ecam: [EcamRegion; MAX_ECAM],
    pub ecam_n: usize,
    pub psci: Psci,
}

unsafe fn table<'a>(rsdp: u64, hhdm: u64, sig: &[u8; 4]) -> Option<&'a [u8]> {
    let p = crate::acpi::find_table(rsdp, hhdm, sig)?;
    let len = u32::from_le_bytes([*p.add(4), *p.add(5), *p.add(6), *p.add(7)]) as usize;
    if !(36..=1 << 20).contains(&len) { return None; }
    Some(core::slice::from_raw_parts(p, len))
}

/// Read every aarch64-relevant table through the HHDM.
///
/// # Safety
/// The ACPI tables must be readable through the HHDM at `hhdm`.
pub unsafe fn discover(rsdp: u64, hhdm: u64) -> Platform {
    let mut ecam = [EcamRegion::default(); MAX_ECAM];
    let ecam_n = table(rsdp, hhdm, b"MCFG").map_or(0, |t| parse_mcfg(t, &mut ecam));
    Platform {
        madt: table(rsdp, hhdm, b"APIC").and_then(parse_madt),
        spcr: table(rsdp, hhdm, b"SPCR").and_then(parse_spcr),
        gtdt: table(rsdp, hhdm, b"GTDT").and_then(parse_gtdt),
        ecam,
        ecam_n,
        psci: table(rsdp, hhdm, b"FACP").map_or(Psci::None, parse_fadt_psci),
    }
}

// ── 16550 register mapping ───────────────────────────────────────────────────
//
// The console driver is written against PL011 registers. A 16550 behind SPCR is
// served by translating those accesses: `Reg16550::of_pl011` names the 16550
// register for a PL011 offset, `fr_from_lsr` synthesises PL011's flag register.

pub const PL011_DR: usize = 0x00;
pub const PL011_FR: usize = 0x18;
pub const PL011_IMSC: usize = 0x38;

pub const NS_RBR: usize = 0; // RBR / THR
pub const NS_IER: usize = 1;
pub const NS_LSR: usize = 5;
pub const LSR_DR: u32 = 1 << 0;
pub const LSR_THRE: u32 = 1 << 5;
pub const FR_RXFE: u32 = 1 << 4;
pub const FR_TXFF: u32 = 1 << 5;

/// PL011 FR value for a 16550 LSR byte.
pub fn fr_from_lsr(lsr: u32) -> u32 {
    (if lsr & LSR_DR == 0 { FR_RXFE } else { 0 }) | (if lsr & LSR_THRE == 0 { FR_TXFF } else { 0 })
}

/// Byte offset of 16550 register `idx` for a given register shift.
pub fn ns_off(idx: usize, reg_shift: u8) -> usize { idx << reg_shift }
