//! Hardware-independent pieces of the Intel Gen9 / Gen9.5 graphics driver
//! (Skylake, Kaby Lake, Coffee Lake, Whiskey Lake, Comet Lake, Amber Lake).
//!
//! Everything here is pure: register numbers, bit-field decoders, GTT page
//! table entry encoders, the 4-level PPGTT built over an abstract page store,
//! command-stream builders, the execlist context image ("LRC") layout and
//! descriptor, context-status-buffer decode, the hang detector state
//! machine, and parsers for the DMC firmware container and the ACPI
//! OpRegion / VBT. No MMIO, no allocation of physical memory, no unsafe: the
//! module is included verbatim by `drivers/i915-tests` and exercised on the
//! host, so the values the kernel writes into the hardware are the values
//! the tests check.
//!
//! Sources: Linux 6.12 `drivers/gpu/drm/i915` (intel_lrc.c, gen8_ppgtt.c,
//! intel_ggtt.c, intel_gtt.c, intel_sseu.c, intel_gt_clock_utils.c,
//! intel_execlists_submission.c, intel_gpu_commands.h, intel_dmc.c,
//! intel_opregion.c, i915_pciids.h) and early-quirks.c for the stolen size.
//! Register offsets carry the Linux macro name in their comment.

#![allow(dead_code)]

use alloc::vec::Vec;

// ─────────────────────────────────────────────────────────────────────────────
// Device identification
// ─────────────────────────────────────────────────────────────────────────────

pub const INTEL_VENDOR: u16 = 0x8086;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Skylake (Gen9).
    Skl,
    /// Kaby Lake, Kaby Lake-R, Amber Lake (KBL-based) (Gen9.5).
    Kbl,
    /// Coffee Lake, Whiskey Lake, Amber Lake (CFL-based) (Gen9.5).
    Cfl,
    /// Comet Lake (Gen9.5).
    Cml,
    /// Broxton / Apollo Lake (Gen9 LP: no LLC, different display PHYs).
    Bxt,
    /// Gemini Lake (Gen9 LP).
    Glk,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Skl => "Skylake",
            Family::Kbl => "Kaby Lake",
            Family::Cfl => "Coffee Lake",
            Family::Cml => "Comet Lake",
            Family::Bxt => "Broxton/Apollo Lake",
            Family::Glk => "Gemini Lake",
        }
    }
    /// Gen9 "LP" (Atom-class) parts: no LLC, 1 slice, 3 subslices, different
    /// clocks. The engine bring-up here is written for the big-core parts.
    pub fn is_lp(self) -> bool { matches!(self, Family::Bxt | Family::Glk) }
    /// Mesa / Linux "Gen9.5" (KBL and later big-core Gen9).
    pub fn is_gen95(self) -> bool { matches!(self, Family::Kbl | Family::Cfl | Family::Cml) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    pub device: u16,
    pub family: Family,
    /// GT level (1, 2, 3, 4; 0 = unknown, e.g. the "GT1.5" parts count as 1).
    pub gt: u8,
    /// Package variant as in i915_pciids.h ("ULT", "ULX", "DT", "Halo", ...).
    pub variant: &'static str,
}

impl Platform {
    /// Graphics IP version (all supported parts are 9).
    pub fn graphics_ver(&self) -> u8 { 9 }
    /// Has a shared last-level cache with the CPU (coherent GPU access to
    /// cached system memory).
    pub fn has_llc(&self) -> bool { !self.family.is_lp() }
    /// The UHD Graphics 620 of the target laptop (i5-8350U, KBL-R).
    pub fn is_target_laptop_gpu(&self) -> bool { self.device == 0x5917 }
}

/// (device id, family, GT, variant) — i915_pciids.h, Linux 6.12.
const DEVICES: &[(u16, Family, u8, &str)] = &[
    // Skylake
    (0x1906, Family::Skl, 1, "ULT"), (0x1913, Family::Skl, 1, "ULT GT1.5"),
    (0x190E, Family::Skl, 1, "ULX"), (0x1915, Family::Skl, 1, "ULX GT1.5"),
    (0x1902, Family::Skl, 1, "DT"), (0x190A, Family::Skl, 1, "SRV"),
    (0x190B, Family::Skl, 1, "Halo"), (0x1917, Family::Skl, 1, "DT GT1.5"),
    (0x1916, Family::Skl, 2, "ULT"), (0x1921, Family::Skl, 2, "ULT GT2F"),
    (0x191E, Family::Skl, 2, "ULX"), (0x1912, Family::Skl, 2, "DT"),
    (0x191A, Family::Skl, 2, "SRV"), (0x191B, Family::Skl, 2, "Halo"),
    (0x191D, Family::Skl, 2, "WKS"),
    (0x1923, Family::Skl, 3, "ULT"), (0x1926, Family::Skl, 3, "ULT GT3e"),
    (0x1927, Family::Skl, 3, "ULT GT3e"), (0x192A, Family::Skl, 3, "SRV"),
    (0x192B, Family::Skl, 3, "Halo GT3e"), (0x192D, Family::Skl, 3, "SRV GT3e"),
    (0x1932, Family::Skl, 4, "DT"), (0x193A, Family::Skl, 4, "SRV GT4e"),
    (0x193B, Family::Skl, 4, "Halo GT4e"), (0x193D, Family::Skl, 4, "WKS GT4e"),
    // Broxton / Apollo Lake, Gemini Lake
    (0x0A84, Family::Bxt, 0, "BXT"), (0x1A84, Family::Bxt, 0, "BXT"),
    (0x1A85, Family::Bxt, 0, "BXT"), (0x5A84, Family::Bxt, 0, "APL HD 505"),
    (0x5A85, Family::Bxt, 0, "APL HD 500"),
    (0x3184, Family::Glk, 0, "GLK"), (0x3185, Family::Glk, 0, "GLK"),
    // Kaby Lake
    (0x5906, Family::Kbl, 1, "ULT"), (0x5913, Family::Kbl, 1, "ULT GT1.5"),
    (0x590E, Family::Kbl, 1, "ULX"), (0x5915, Family::Kbl, 1, "ULX GT1.5"),
    (0x5902, Family::Kbl, 1, "DT"), (0x5908, Family::Kbl, 1, "Halo"),
    (0x590A, Family::Kbl, 1, "SRV"), (0x590B, Family::Kbl, 1, "Halo"),
    (0x5916, Family::Kbl, 2, "ULT"), (0x5921, Family::Kbl, 2, "ULT GT2F"),
    (0x591E, Family::Kbl, 2, "ULX"), (0x5912, Family::Kbl, 2, "DT"),
    (0x5917, Family::Kbl, 2, "Mobile (KBL-R ULT, UHD Graphics 620)"),
    (0x591A, Family::Kbl, 2, "SRV"), (0x591B, Family::Kbl, 2, "Halo"),
    (0x591D, Family::Kbl, 2, "WKS"),
    (0x5926, Family::Kbl, 3, "ULT"), (0x5923, Family::Kbl, 3, "ULT"),
    (0x5927, Family::Kbl, 3, "ULT"), (0x593B, Family::Kbl, 4, "Halo"),
    // Amber Lake
    (0x591C, Family::Kbl, 2, "AML ULX"), (0x87C0, Family::Kbl, 2, "AML ULX"),
    (0x87CA, Family::Cfl, 2, "AML ULX"),
    // Comet Lake
    (0x9BA2, Family::Cml, 1, "CML"), (0x9BA4, Family::Cml, 1, "CML"),
    (0x9BA5, Family::Cml, 1, "CML"), (0x9BA8, Family::Cml, 1, "CML"),
    (0x9B21, Family::Cml, 1, "CML-U"), (0x9BAA, Family::Cml, 1, "CML-U"),
    (0x9BAC, Family::Cml, 1, "CML-U"),
    (0x9BC2, Family::Cml, 2, "CML"), (0x9BC4, Family::Cml, 2, "CML"),
    (0x9BC5, Family::Cml, 2, "CML"), (0x9BC6, Family::Cml, 2, "CML"),
    (0x9BC8, Family::Cml, 2, "CML"), (0x9BE6, Family::Cml, 2, "CML"),
    (0x9BF6, Family::Cml, 2, "CML"),
    (0x9B41, Family::Cml, 2, "CML-U"), (0x9BCA, Family::Cml, 2, "CML-U"),
    (0x9BCC, Family::Cml, 2, "CML-U"),
    // Coffee Lake
    (0x3E90, Family::Cfl, 1, "S"), (0x3E93, Family::Cfl, 1, "S"), (0x3E99, Family::Cfl, 1, "S"),
    (0x3E91, Family::Cfl, 2, "S"), (0x3E92, Family::Cfl, 2, "S"), (0x3E96, Family::Cfl, 2, "S"),
    (0x3E98, Family::Cfl, 2, "S"), (0x3E9A, Family::Cfl, 2, "S"),
    (0x3E9C, Family::Cfl, 1, "H"), (0x3E94, Family::Cfl, 2, "H"), (0x3E9B, Family::Cfl, 2, "H"),
    (0x3EA9, Family::Cfl, 2, "U"),
    (0x3EA5, Family::Cfl, 3, "U"), (0x3EA6, Family::Cfl, 3, "U"),
    (0x3EA7, Family::Cfl, 3, "U"), (0x3EA8, Family::Cfl, 3, "U"),
    // Whiskey Lake (CFL-U)
    (0x3EA1, Family::Cfl, 1, "WHL-U"), (0x3EA4, Family::Cfl, 1, "WHL-U"),
    (0x3EA0, Family::Cfl, 2, "WHL-U"), (0x3EA3, Family::Cfl, 2, "WHL-U"),
    (0x3EA2, Family::Cfl, 3, "WHL-U"),
];

/// Identify a Gen9 GPU by PCI device id.
pub fn identify(device: u16) -> Option<Platform> {
    DEVICES.iter().find(|d| d.0 == device)
        .map(|&(device, family, gt, variant)| Platform { device, family, gt, variant })
}

/// Name of the PCH from the ISA/LPC bridge (00:1f.0) device id. Ranges as
/// Linux intel_pch.c masks them (`id & 0xff80`).
pub fn pch_name(lpc_device: u16) -> &'static str {
    match lpc_device & 0xFF80 {
        0x9D00 => "Sunrise Point-LP (SPT-LP)",
        0xA100 => "Sunrise Point-H (SPT-H)",
        0xA280 => "Kaby Point / Union Point (KBP)",
        0x9D80 => "Cannon Point-LP (CNP-LP)",
        0xA300 => "Cannon Point-H (CNP-H)",
        0x0280 => "Comet Point-LP (CMP-LP)",
        0x0680 => "Comet Point-H (CMP-H)",
        0xA380 => "Comet Point-V (CMP-V)",
        _ => "unknown PCH",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PCI configuration space of the GPU (00:02.0)
// ─────────────────────────────────────────────────────────────────────────────

pub mod cfg {
    /// SNB_GMCH_CTRL: graphics GTT / stolen sizes (16 bit).
    pub const GGC: u16 = 0x50;
    /// INTEL_BSM: base of data stolen memory, bits 31:20; bit 0 lock.
    pub const BDSM: u16 = 0x5C;
    /// ASLS: physical address of the ACPI OpRegion.
    pub const ASLS: u16 = 0xFC;
    /// MSAC (aperture size control) on some parts.
    pub const MSAC: u16 = 0x62;
    /// BAR indices.
    pub const GTTMMADR_BAR: usize = 0;
    pub const GMADR_BAR: usize = 2;
}

/// Decoded GMCH graphics control word (config 0x50).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gmch {
    pub raw: u16,
    /// GGCLCK.
    pub locked: bool,
    /// IVD: VGA decode disabled.
    pub vga_disabled: bool,
    /// Size of the GGTT page table itself (bytes of 8-byte PTEs). 0 = none.
    pub gtt_table_bytes: u64,
    /// Number of 4 KiB GGTT entries.
    pub ggtt_entries: u64,
    /// GPU virtual size of the GGTT (bytes).
    pub ggtt_size: u64,
    /// Data stolen memory size (bytes).
    pub stolen_size: u64,
}

/// `gen8_get_total_gtt_size` + early-quirks `gen9_stolen_size`.
pub fn decode_ggc(ggc: u16) -> Gmch {
    let ggms = (ggc >> 6) & 3;
    let gtt_table_bytes = if ggms != 0 { (1u64 << ggms) << 20 } else { 0 };
    let gms = ((ggc >> 8) & 0xFF) as u64;
    let stolen_size = if gms < 0xF0 { gms * (32 << 20) } else { (gms - 0xF0 + 1) * (4 << 20) };
    let ggtt_entries = gtt_table_bytes / 8;
    Gmch {
        raw: ggc,
        locked: ggc & 1 != 0,
        vga_disabled: ggc & 2 != 0,
        gtt_table_bytes,
        ggtt_entries,
        ggtt_size: ggtt_entries * 4096,
        stolen_size,
    }
}

/// Data stolen memory base from BDSM (config 0x5C): bits 31:20.
pub fn decode_bdsm(bdsm: u32) -> (u64, bool) {
    ((bdsm & 0xFFF0_0000) as u64, bdsm & 1 != 0)
}

/// GEN6_STOLEN_RESERVED (0x1082C0) on gen8/9 (`bdw_get_stolen_reserved`):
/// the reserved part of stolen memory runs from the base in bits 31:20 to the
/// top of stolen. `None` when disabled.
pub fn decode_stolen_reserved(v: u32, stolen_top: u64) -> Option<(u64, u64)> {
    if v & 1 == 0 || v & 0xFFF0_0000 == 0 { return None; }
    let base = (v & 0xFFF0_0000) as u64;
    if base > stolen_top { return None; }
    Some((base, stolen_top - base))
}

/// Offset of the GGTT page table inside BAR0 (GTTMMADR) on gen8+: the upper
/// half of the 16 MiB BAR (`gen6_gttadr_offset`).
pub const GTTADR_OFFSET: u64 = 8 << 20;
/// The MMIO register window: the lower 2 MiB of BAR0 that the driver uses
/// (registers live below 0x200000 on gen9; the rest up to 8 MiB is reserved).
pub const MMIO_SIZE: usize = 2 << 20;

// ─────────────────────────────────────────────────────────────────────────────
// Registers
// ─────────────────────────────────────────────────────────────────────────────

/// Masked-register write value enabling `bits` (upper half = write mask).
pub const fn masked_en(bits: u32) -> u32 { (bits << 16) | bits }
/// Masked-register write value disabling `bits`.
pub const fn masked_dis(bits: u32) -> u32 { bits << 16 }

pub mod reg {
    // ── forcewake (gen9) ──
    /// FORCEWAKE_GT_GEN9 / FORCEWAKE_MT.
    pub const FORCEWAKE_GT: u32 = 0xA188;
    /// FORCEWAKE_ACK_GT_GEN9.
    pub const FORCEWAKE_ACK_GT: u32 = 0x13_0044;
    /// FORCEWAKE_RENDER_GEN9.
    pub const FORCEWAKE_RENDER: u32 = 0xA278;
    /// FORCEWAKE_ACK_RENDER_GEN9.
    pub const FORCEWAKE_ACK_RENDER: u32 = 0x0D84;
    /// FORCEWAKE_MEDIA_GEN9.
    pub const FORCEWAKE_MEDIA: u32 = 0xA270;
    /// FORCEWAKE_ACK_MEDIA_GEN9.
    pub const FORCEWAKE_ACK_MEDIA: u32 = 0x0D88;
    /// FORCEWAKE_KERNEL request/ack bit.
    pub const FORCEWAKE_KERNEL: u32 = 1 << 0;
    pub const FORCEWAKE_USER: u32 = 1 << 1;
    pub const FORCEWAKE_KERNEL_FALLBACK: u32 = 1 << 15;

    // ── GT misc ──
    pub const GEN8_FUSE2: u32 = 0x9120;
    pub const GEN9_EU_DISABLE0: u32 = 0x9134;
    pub const CTC_MODE: u32 = 0xA26C;
    pub const GEN9_TIMESTAMP_OVERRIDE: u32 = 0x44074;
    /// GEN6_RP_STATE_CAP = MCHBAR mirror 0x140000 + 0x5998 (big-core gen9).
    pub const GEN6_RP_STATE_CAP: u32 = 0x14_5998;
    /// BXT_RP_STATE_CAP (Gen9 LP).
    pub const BXT_RP_STATE_CAP: u32 = 0x13_8170;
    pub const GEN6_RPSTAT1: u32 = 0xA01C;
    pub const GEN6_RC_CONTROL: u32 = 0xA090;
    pub const GEN6_RC_STATE: u32 = 0xA094;
    pub const GEN6_GT_GFX_RC6: u32 = 0x13_8108;
    pub const GEN8_PRIVATE_PAT_LO: u32 = 0x40E0;
    pub const GEN8_PRIVATE_PAT_HI: u32 = 0x40E4;
    pub const GEN6_GDRST: u32 = 0x941C;
    pub const GRDOM_FULL: u32 = 1 << 0;
    pub const GRDOM_RENDER: u32 = 1 << 1;
    pub const GRDOM_MEDIA: u32 = 1 << 2;
    pub const GRDOM_BLT: u32 = 1 << 3;
    pub const GRDOM_VECS: u32 = 1 << 4;
    /// GFX_FLSH_CNTL_GEN6: write 1 to invalidate GGTT TLBs after PTE updates.
    pub const GFX_FLSH_CNTL_GEN6: u32 = 0x10_1008;
    pub const GEN6_STOLEN_RESERVED: u32 = 0x10_82C0;
    pub const ERROR_GEN6: u32 = 0x40A0;
    pub const GEN8_RING_FAULT_REG: u32 = 0x4094;
    pub const GEN8_FAULT_TLB_DATA0: u32 = 0x4B10;
    pub const GEN8_FAULT_TLB_DATA1: u32 = 0x4B14;
    pub const GEN8_MASTER_IRQ: u32 = 0x44200;
    /// GEN8_GT_{ISR,IMR,IIR,IER}(n) = base + 0x10 * n (+0, +4, +8, +0xc).
    pub const GEN8_GT_ISR0: u32 = 0x44300;
    pub const GEN7_MISCCPCTL: u32 = 0x9424;
    pub const GEN6_UCGCTL1: u32 = 0x9400;

    // ── engine-relative (add the engine mmio base) ──
    pub const RING_TAIL: u32 = 0x30;
    pub const RING_HEAD: u32 = 0x34;
    pub const RING_START: u32 = 0x38;
    pub const RING_CTL: u32 = 0x3C;
    pub const RING_ACTHD_UDW: u32 = 0x5C;
    pub const RING_IPEIR: u32 = 0x64;
    pub const RING_IPEHR: u32 = 0x68;
    pub const RING_INSTDONE: u32 = 0x6C;
    pub const RING_ACTHD: u32 = 0x74;
    pub const RING_HWS_PGA: u32 = 0x80;
    pub const RING_HWSTAM: u32 = 0x98;
    pub const RING_MI_MODE: u32 = 0x9C;
    pub const RING_IMR: u32 = 0xA8;
    pub const RING_EIR: u32 = 0xB0;
    pub const RING_EMR: u32 = 0xB4;
    pub const RING_ESR: u32 = 0xB8;
    pub const RING_RESET_CTL: u32 = 0xD0;
    pub const RING_BBADDR: u32 = 0x140;
    pub const RING_BBADDR_UDW: u32 = 0x168;
    pub const RING_ELSP: u32 = 0x230;
    pub const RING_EXECLIST_STATUS_LO: u32 = 0x234;
    pub const RING_EXECLIST_STATUS_HI: u32 = 0x238;
    pub const RING_PDP0_LDW: u32 = 0x270;
    pub const RING_MODE_GEN7: u32 = 0x29C;
    pub const RING_TIMESTAMP: u32 = 0x358;
    pub const RING_CONTEXT_STATUS_BUF: u32 = 0x370;
    pub const RING_CONTEXT_STATUS_PTR: u32 = 0x3A0;
    pub const RING_CTX_TIMESTAMP: u32 = 0x3A8;

    pub const MODE_IDLE: u32 = 1 << 9;
    pub const STOP_RING: u32 = 1 << 8;
    pub const GFX_RUN_LIST_ENABLE: u32 = 1 << 15;
    pub const RING_VALID: u32 = 1;
    pub const RESET_CTL_REQUEST_RESET: u32 = 1 << 0;
    pub const RESET_CTL_READY_TO_RESET: u32 = 1 << 1;
    pub const RESET_CTL_CAT_ERROR: u32 = 1 << 2;

    // ── engine mmio bases ──
    pub const RENDER_RING_BASE: u32 = 0x02000;
    pub const BLT_RING_BASE: u32 = 0x22000;
    pub const GEN6_BSD_RING_BASE: u32 = 0x12000;
    pub const GEN8_BSD2_RING_BASE: u32 = 0x1C000;
    pub const VEBOX_RING_BASE: u32 = 0x1A000;

    // ── display (no forcewake; display power wells) ──
    pub const FPGA_DBG: u32 = 0x42300;
    pub const FPGA_DBG_RM_NOCLAIM: u32 = 1 << 31;
    pub const SKL_FUSE_STATUS: u32 = 0x42000;
    pub const HSW_PWR_WELL_CTL1: u32 = 0x45400;
    pub const HSW_PWR_WELL_CTL2: u32 = 0x45404;
    pub const HSW_PWR_WELL_CTL3: u32 = 0x45408;
    pub const HSW_PWR_WELL_CTL4: u32 = 0x4540C;
    pub const DC_STATE_EN: u32 = 0x45504;
    pub const CDCLK_CTL: u32 = 0x46000;
    pub const LCPLL1_CTL: u32 = 0x46010;
    pub const LCPLL2_CTL: u32 = 0x46014;
    pub const DPLL_CTRL1: u32 = 0x6C058;
    pub const DPLL_CTRL2: u32 = 0x6C05C;
    pub const DPLL_STATUS: u32 = 0x6C060;
    pub const DMC_PROGRAM0: u32 = 0x80000;
    pub const DMC_SSP_BASE: u32 = 0x8F074;
    pub const DMC_HTP_SKL: u32 = 0x8F004;
    pub const SKL_DMC_DC3_DC5_COUNT: u32 = 0x80030;
    pub const SKL_DMC_DC5_DC6_COUNT: u32 = 0x8002C;
    /// Transcoder timing base A (B +0x1000, C +0x2000, EDP 0x6F000).
    pub const TRANS_HTOTAL_A: u32 = 0x60000;
    pub const TRANS_HTOTAL_EDP: u32 = 0x6F000;
    pub const PIPESRC_A: u32 = 0x6001C;
    pub const PIPE_DATA_M1_A: u32 = 0x60030;
    pub const PIPE_LINK_M1_A: u32 = 0x60040;
    pub const TRANS_DDI_FUNC_CTL_A: u32 = 0x60400;
    pub const TRANS_DDI_FUNC_CTL_EDP: u32 = 0x6F400;
    pub const PIPEDSL_A: u32 = 0x70000;
    pub const TRANSCONF_A: u32 = 0x70008;
    pub const TRANSCONF_EDP: u32 = 0x7F008;
    pub const PIPE_FRMCOUNT_A: u32 = 0x70040;
    pub const PLANE_CTL_1_A: u32 = 0x70180;
    pub const PLANE_STRIDE_1_A: u32 = 0x70188;
    pub const PLANE_POS_1_A: u32 = 0x7018C;
    pub const PLANE_SIZE_1_A: u32 = 0x70190;
    pub const PLANE_SURF_1_A: u32 = 0x7019C;
    pub const PLANE_OFFSET_1_A: u32 = 0x701A4;
    pub const PLANE_BUF_CFG_1_A: u32 = 0x7027C;
    pub const CUR_CTL_A: u32 = 0x70080;
    pub const CUR_BASE_A: u32 = 0x70084;
    pub const PS_CTRL_1A: u32 = 0x68180;
    pub const PS_WIN_POS_1A: u32 = 0x68170;
    pub const PS_WIN_SZ_1A: u32 = 0x68174;
    pub const DDI_BUF_CTL_A: u32 = 0x64000;
    pub const DP_TP_CTL_A: u32 = 0x64040;
    pub const DP_TP_STATUS_A: u32 = 0x64044;
    pub const BLC_PWM_CPU_CTL2: u32 = 0x48250;
    pub const BLC_PWM_CPU_CTL: u32 = 0x48254;
    pub const BLC_PWM_PCH_CTL1: u32 = 0xC8250;
    pub const BLC_PWM_PCH_CTL2: u32 = 0xC8254;
    /// South display engine (PCH) panel power sequencer base.
    pub const PCH_PP_STATUS: u32 = 0xC7200;
    pub const PCH_PP_CONTROL: u32 = 0xC7204;
    pub const PCH_PP_ON_DELAYS: u32 = 0xC7208;
    pub const PCH_PP_OFF_DELAYS: u32 = 0xC720C;
    pub const PCH_PP_DIVISOR: u32 = 0xC7210;
    pub const SFUSE_STRAP: u32 = 0xC2014;
    pub const SDEISR: u32 = 0xC4000;
    pub const SHOTPLUG_CTL: u32 = 0xC4030;
    pub const SHOTPLUG_CTL2: u32 = 0xC403C;

    /// Per-pipe register: `a` + 0x1000 * pipe (A=0, B=1, C=2).
    pub const fn pipe(a: u32, pipe: u32) -> u32 { a + 0x1000 * pipe }
    /// Universal plane `plane` (0-based) of `pipe`: + 0x100 per plane.
    pub const fn plane(a: u32, pipe: u32, plane: u32) -> u32 { a + 0x1000 * pipe + 0x100 * plane }
    /// DDI port register: `a` + 0x100 * port (A=0 .. E=4).
    pub const fn port(a: u32, port: u32) -> u32 { a + 0x100 * port }
}

/// The GT forcewake domains of gen9 as (name, request register, ack register).
pub const FW_DOMAINS: [(&str, u32, u32); 3] = [
    ("gt", reg::FORCEWAKE_GT, reg::FORCEWAKE_ACK_GT),
    ("render", reg::FORCEWAKE_RENDER, reg::FORCEWAKE_ACK_RENDER),
    ("media", reg::FORCEWAKE_MEDIA, reg::FORCEWAKE_ACK_MEDIA),
];

/// Which forcewake domains a gen9 GT register needs (`__gen9_fw_ranges`):
/// bit 0 = GT, bit 1 = render, bit 2 = media; 0 = none (uncore / display).
pub fn gen9_fw_domains(off: u32) -> u8 {
    const GT: u8 = 1; const R: u8 = 2; const M: u8 = 4;
    const RANGES: &[(u32, u32, u8)] = &[
        (0x0, 0xaff, GT), (0xb00, 0x1fff, 0), (0x2000, 0x26ff, R), (0x2700, 0x2fff, GT),
        (0x3000, 0x3fff, R), (0x4000, 0x51ff, GT), (0x5200, 0x7fff, R), (0x8000, 0x812f, GT),
        (0x8130, 0x813f, M), (0x8140, 0x815f, R), (0x8160, 0x82ff, GT), (0x8300, 0x84ff, R),
        (0x8500, 0x87ff, GT), (0x8800, 0x89ff, M), (0x8a00, 0x8bff, GT), (0x8c00, 0x8cff, R),
        (0x8d00, 0x93ff, GT), (0x9400, 0x97ff, R | M), (0x9800, 0xafff, GT), (0xb000, 0xb47f, R),
        (0xb480, 0xcfff, GT), (0xd000, 0xd7ff, M), (0xd800, 0xdfff, GT), (0xe000, 0xe8ff, R),
        (0xe900, 0x11fff, GT), (0x12000, 0x13fff, M), (0x14000, 0x19fff, GT), (0x1a000, 0x1e9ff, M),
        (0x1ea00, 0x243ff, GT), (0x24400, 0x247ff, R), (0x24800, 0x2ffff, GT), (0x30000, 0x3ffff, M),
    ];
    RANGES.iter().find(|r| off >= r.0 && off <= r.1).map(|r| r.2).unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Engines
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineClass { Render, Copy, VideoDecode, VideoEnhance }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineDesc {
    pub name: &'static str,
    pub class: EngineClass,
    /// i915 UAPI engine class value (I915_ENGINE_CLASS_*).
    pub uapi_class: u16,
    pub instance: u16,
    pub mmio_base: u32,
    /// GEN6_GDRST domain bit.
    pub reset_domain: u32,
    /// Bits of the gen8 GT interrupt registers: (bank, shift).
    pub irq: (u8, u8),
}

/// The engines of a gen9 big-core GT2 (no second VCS: that is GT3/GT4 only).
pub const GEN9_ENGINES: [EngineDesc; 4] = [
    EngineDesc { name: "rcs0", class: EngineClass::Render, uapi_class: 0, instance: 0,
        mmio_base: reg::RENDER_RING_BASE, reset_domain: reg::GRDOM_RENDER, irq: (0, 0) },
    EngineDesc { name: "bcs0", class: EngineClass::Copy, uapi_class: 1, instance: 0,
        mmio_base: reg::BLT_RING_BASE, reset_domain: reg::GRDOM_BLT, irq: (0, 16) },
    EngineDesc { name: "vcs0", class: EngineClass::VideoDecode, uapi_class: 2, instance: 0,
        mmio_base: reg::GEN6_BSD_RING_BASE, reset_domain: reg::GRDOM_MEDIA, irq: (1, 0) },
    EngineDesc { name: "vecs0", class: EngineClass::VideoEnhance, uapi_class: 3, instance: 0,
        mmio_base: reg::VEBOX_RING_BASE, reset_domain: reg::GRDOM_VECS, irq: (3, 0) },
];

/// Execlist context image size (bytes, PPHWSP page included) — intel_engine_cs.c.
pub fn context_size(class: EngineClass) -> usize {
    match class {
        EngineClass::Render => 22 * 4096,
        _ => 2 * 4096,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Slice / subslice / EU topology
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Sseu {
    pub max_slices: u8,
    pub max_subslices: u8,
    pub max_eus_per_subslice: u8,
    pub slice_mask: u8,
    /// Subslice mask, identical for every enabled slice on gen9.
    pub subslice_mask: u8,
    /// Enabled EUs per (slice, subslice).
    pub eu_mask: [[u8; 4]; 3],
    pub eu_total: u32,
    /// EUs per subslice, rounded up (what Mesa calls `eu_per_subslice`).
    pub eu_per_subslice: u32,
    /// Subslices that have only 7 EUs.
    pub subslice_7eu: [u8; 3],
}

impl Sseu {
    pub fn slices(&self) -> u32 { self.slice_mask.count_ones() }
    pub fn subslices_total(&self) -> u32 { self.slices() * self.subslice_mask.count_ones() }
}

/// `gen9_sseu_info_init`: decode GEN8_FUSE2 and GEN9_EU_DISABLE(0..2).
pub fn gen9_sseu(fuse2: u32, eu_disable: [u32; 3], lp: bool) -> Sseu {
    let mut s = Sseu::default();
    s.max_slices = if lp { 1 } else { 3 };
    s.max_subslices = if lp { 3 } else { 4 };
    s.max_eus_per_subslice = 8;
    s.slice_mask = (((fuse2 >> 25) & 7) as u8) & ((1u8 << s.max_slices) - 1);
    let all_ss = (1u8 << s.max_subslices) - 1;
    s.subslice_mask = all_ss & !(((fuse2 >> 20) & 0xF) as u8);
    for sl in 0..s.max_slices as usize {
        if s.slice_mask & (1 << sl) == 0 { continue; }
        for ss in 0..s.max_subslices as usize {
            if s.subslice_mask & (1 << ss) == 0 { continue; }
            let dis = ((eu_disable[sl] >> (ss * 8)) & 0xFF) as u8;
            let en = !dis;
            s.eu_mask[sl][ss] = en;
            s.eu_total += en.count_ones();
            if en.count_ones() == 7 { s.subslice_7eu[sl] |= 1 << ss; }
        }
    }
    let ss_total = s.subslices_total();
    s.eu_per_subslice = if ss_total != 0 { (s.eu_total + ss_total - 1) / ss_total } else { 0 };
    s
}

/// DRM_I915_QUERY_TOPOLOGY_INFO answer (struct drm_i915_query_topology_info
/// followed by its data): slice mask bytes, then per-slice subslice mask
/// bytes, then per-(slice, subslice) EU mask bytes.
pub fn topology_blob(s: &Sseu) -> Vec<u8> {
    let max_s = s.max_slices as usize;
    let max_ss = s.max_subslices as usize;
    let max_eu = s.max_eus_per_subslice as usize;
    let ss_stride = (max_ss + 7) / 8;
    let eu_stride = (max_eu + 7) / 8;
    let slice_bytes = (max_s + 7) / 8;
    let ss_off = slice_bytes;
    let eu_off = ss_off + max_s * ss_stride;
    let data_len = eu_off + max_s * max_ss * eu_stride;
    let mut v = Vec::with_capacity(16 + data_len);
    let push16 = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    push16(&mut v, 0); // flags
    push16(&mut v, max_s as u16);
    push16(&mut v, max_ss as u16);
    push16(&mut v, max_eu as u16);
    push16(&mut v, ss_off as u16);
    push16(&mut v, ss_stride as u16);
    push16(&mut v, eu_off as u16);
    push16(&mut v, eu_stride as u16);
    let mut data = alloc::vec![0u8; data_len];
    for sl in 0..max_s {
        if s.slice_mask & (1 << sl) != 0 {
            data[sl / 8] |= 1 << (sl % 8);
            for ss in 0..max_ss {
                if s.subslice_mask & (1 << ss) != 0 {
                    data[ss_off + sl * ss_stride + ss / 8] |= 1 << (ss % 8);
                    data[eu_off + (sl * max_ss + ss) * eu_stride] = s.eu_mask[sl][ss];
                }
            }
        }
    }
    v.extend_from_slice(&data);
    v
}

// ─────────────────────────────────────────────────────────────────────────────
// Clocks and frequencies
// ─────────────────────────────────────────────────────────────────────────────

/// `gen9_read_clock_frequency`: command-streamer timestamp frequency (Hz)
/// from CTC_MODE and GEN9_TIMESTAMP_OVERRIDE.
pub fn gen9_cs_timestamp_hz(ctc_mode: u32, ts_override: u32, lp: bool) -> u32 {
    if ctc_mode & 1 == 1 {
        // CTC_SOURCE_DIVIDE_LOGIC: reference timestamp frequency.
        let base = ((ts_override & 0x3FF) + 1) * 1_000_000;
        let frac = 1_000_000 / (((ts_override >> 12) & 0xF) + 1);
        base + frac
    } else {
        let f: u32 = if lp { 19_200_000 } else { 24_000_000 };
        f >> (3 - ((ctc_mode >> 1) & 3))
    }
}

/// RP_STATE_CAP (big-core gen9): (RP0, RP1, RPn) in MHz. The register holds
/// 50 MHz units (gen9 multiplies by GEN9_FREQ_SCALER=3 into 16.67 MHz units,
/// which come out to the same MHz).
pub fn decode_rp_state_cap(v: u32) -> (u32, u32, u32) {
    ((v & 0xFF) * 50, ((v >> 8) & 0xFF) * 50, ((v >> 16) & 0xFF) * 50)
}

/// Current actual GT frequency (MHz) from GEN6_RPSTAT1 on gen9 (CAGF bits
/// 31:23, 16.67 MHz units).
pub fn decode_cagf_mhz(rpstat1: u32) -> u32 {
    ((rpstat1 >> 23) & 0x1FF) * 50 / 3
}

// ─────────────────────────────────────────────────────────────────────────────
// GTT page table entries
// ─────────────────────────────────────────────────────────────────────────────

pub const PAGE: u64 = 4096;
pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_RW: u64 = 1 << 1;
pub const PTE_PWT: u64 = 1 << 3;
pub const PTE_PCD: u64 = 1 << 4;
pub const PTE_PAT: u64 = 1 << 7;
/// Physical address bits a gen8/9 PTE carries (47:12; the GPU decodes 39).
pub const PTE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;
/// The GPU's physical address limit on gen9 (39 bits).
pub const GPU_PHYS_LIMIT: u64 = 1 << 39;

/// GGTT PTE (`gen8_ggtt_pte_encode`): address | present. The GGTT has no
/// per-entry cache attribute on gen8-11.
pub fn ggtt_pte(phys: u64) -> u64 { (phys & PTE_ADDR_MASK) | PTE_PRESENT }

/// Decode a GGTT PTE: Some(physical address) when present.
pub fn ggtt_pte_phys(pte: u64) -> Option<u64> {
    if pte & PTE_PRESENT != 0 { Some(pte & PTE_ADDR_MASK) } else { None }
}

/// Cache level of a PPGTT mapping → private PAT index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cache {
    /// PPAT index 4 (`_PAGE_PAT`): WB, LLC/eLLC, age 0 — Linux's
    /// `PPAT_CACHED`, what `gen8_pte_encode` gives every I915_CACHE_LLC
    /// object (normal objects).
    Cached,
    /// PPAT index 0: WB, LLC only — Linux's `PPAT_CACHED_PDE` (page
    /// directories; our PTEs before lane/gpucoh, `i915.ppat_cached=0`).
    CachedPde,
    /// PPAT index 2: display / eLLC override (scanout with eLLC).
    Display,
    /// PPAT index 3: uncached.
    Uncached,
}

/// PPGTT PTE (`gen8_pte_encode`).
pub fn ppgtt_pte(phys: u64, cache: Cache, read_only: bool) -> u64 {
    let mut pte = (phys & PTE_ADDR_MASK) | PTE_PRESENT | PTE_RW;
    if read_only { pte &= !PTE_RW; }
    pte |= match cache {
        Cache::Cached => PTE_PAT,
        Cache::CachedPde => 0,
        Cache::Display => PTE_PCD,
        Cache::Uncached => PTE_PCD | PTE_PWT,
    };
    pte
}

/// PPGTT directory entry (`gen8_pde_encode`, cached).
pub fn ppgtt_pde(phys: u64) -> u64 { (phys & PTE_ADDR_MASK) | PTE_PRESENT | PTE_RW }

/// Private PAT index selected by a PPGTT PTE (PAT:PCD:PWT).
pub fn pte_pat_index(pte: u64) -> u8 {
    (((pte >> 7) & 1) << 2 | ((pte >> 4) & 1) << 1 | ((pte >> 3) & 1)) as u8
}

/// The private PAT Linux programs on gen9 (`bdw_setup_private_ppat`).
pub fn gen9_private_ppat() -> u64 {
    const UC: u64 = 0; const WC: u64 = 1; const WB: u64 = 3;
    const ELLC_OVERRIDE: u64 = 0; const LLC: u64 = 1 << 2; const LLCELLC: u64 = 2 << 2;
    const fn age(x: u64) -> u64 { x << 4 }
    const fn ppat(i: u64, x: u64) -> u64 { x << (i * 8) }
    ppat(0, WB | LLC) | ppat(1, WC | LLCELLC) | ppat(2, WB | ELLC_OVERRIDE) | ppat(3, UC)
        | ppat(4, WB | LLCELLC | age(0)) | ppat(5, WB | LLCELLC | age(1))
        | ppat(6, WB | LLCELLC | age(2)) | ppat(7, WB | LLCELLC | age(3))
}

// ─────────────────────────────────────────────────────────────────────────────
// 4-level (48-bit) PPGTT over an abstract page store
// ─────────────────────────────────────────────────────────────────────────────

/// Where page-table pages live. The kernel implements it over physical pages
/// (HHDM writes); the host tests over a hash map.
pub trait PtStore {
    /// A new zeroed 4 KiB page; returns its physical address.
    fn alloc_page(&mut self) -> Option<u64>;
    /// Return a page to the allocator.
    fn free_page(&mut self, phys: u64);
    fn read(&self, table: u64, idx: usize) -> u64;
    fn write(&mut self, table: u64, idx: usize, val: u64);
    /// Called after a batch of table writes reached memory (the kernel makes
    /// sure the GPU sees them: a store fence on LLC parts).
    fn flush(&mut self) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtError { NoMemory, Misaligned, OutOfRange }

/// GPU virtual address space of a full PPGTT.
pub const PPGTT_VA_BITS: u32 = 48;
pub const PPGTT_SIZE: u64 = 1 << PPGTT_VA_BITS;

/// Indices of `va` at the four levels (PML4, PDP, PD, PT).
pub fn ppgtt_indices(va: u64) -> [usize; 4] {
    [((va >> 39) & 0x1FF) as usize, ((va >> 30) & 0x1FF) as usize,
     ((va >> 21) & 0x1FF) as usize, ((va >> 12) & 0x1FF) as usize]
}

/// A 4-level PPGTT. Every entry that maps nothing points at the scratch
/// structure of the next level, ending in one scratch page, exactly as Linux
/// does: a stray GPU access lands in the scratch page, never at physical 0.
pub struct Ppgtt {
    pub pml4: u64,
    /// Scratch page, scratch PT, scratch PD, scratch PDP.
    pub scratch: [u64; 4],
    /// Every non-scratch table page this PPGTT allocated (for teardown).
    tables: Vec<u64>,
}

impl Ppgtt {
    pub fn new(st: &mut dyn PtStore) -> Result<Ppgtt, PtError> {
        let page = st.alloc_page().ok_or(PtError::NoMemory)?;
        let pt = st.alloc_page().ok_or(PtError::NoMemory)?;
        let pd = st.alloc_page().ok_or(PtError::NoMemory)?;
        let pdp = st.alloc_page().ok_or(PtError::NoMemory)?;
        let pml4 = st.alloc_page().ok_or(PtError::NoMemory)?;
        // Linux maps the scratch page writable so stray writes land there
        // instead of faulting.
        let spte = ppgtt_pte(page, Cache::Cached, false);
        for i in 0..512 {
            st.write(pt, i, spte);
            st.write(pd, i, ppgtt_pde(pt));
            st.write(pdp, i, ppgtt_pde(pd));
            st.write(pml4, i, ppgtt_pde(pdp));
        }
        st.flush();
        Ok(Ppgtt { pml4, scratch: [page, pt, pd, pdp], tables: Vec::new() })
    }

    /// The scratch entry a fresh table at `level` (1 = PDP .. 3 = PT) is
    /// filled with.
    fn scratch_entry(&self, level: usize) -> u64 {
        match level {
            1 => ppgtt_pde(self.scratch[2]),
            2 => ppgtt_pde(self.scratch[1]),
            _ => ppgtt_pte(self.scratch[0], Cache::Cached, false),
        }
    }

    /// Map one 4 KiB page.
    pub fn map(&mut self, st: &mut dyn PtStore, va: u64, phys: u64, cache: Cache, ro: bool) -> Result<(), PtError> {
        if va & (PAGE - 1) != 0 || phys & (PAGE - 1) != 0 { return Err(PtError::Misaligned); }
        if va >= PPGTT_SIZE || phys >= GPU_PHYS_LIMIT { return Err(PtError::OutOfRange); }
        let idx = ppgtt_indices(va);
        let mut table = self.pml4;
        for level in 0..3 {
            let e = st.read(table, idx[level]);
            let scratch_next = self.scratch[3 - level];
            let next = e & PTE_ADDR_MASK;
            table = if e & PTE_PRESENT == 0 || next == scratch_next {
                let t = st.alloc_page().ok_or(PtError::NoMemory)?;
                let fill = self.scratch_entry(level + 1);
                for i in 0..512 { st.write(t, i, fill); }
                st.write(table, idx[level], ppgtt_pde(t));
                self.tables.push(t);
                t
            } else {
                next
            };
        }
        st.write(table, idx[3], ppgtt_pte(phys, cache, ro));
        Ok(())
    }

    /// Map a physically contiguous range.
    pub fn map_range(&mut self, st: &mut dyn PtStore, va: u64, phys: u64, len: u64, cache: Cache) -> Result<(), PtError> {
        let mut o = 0;
        while o < len {
            self.map(st, va + o, phys + o, cache, false)?;
            o += PAGE;
        }
        st.flush();
        Ok(())
    }

    /// Point `va` back at the scratch page. Tables are kept (Linux frees
    /// empty ones lazily; we keep them until the PPGTT goes away).
    pub fn unmap(&mut self, st: &mut dyn PtStore, va: u64) {
        let idx = ppgtt_indices(va);
        let mut table = self.pml4;
        for level in 0..3 {
            let e = st.read(table, idx[level]);
            let next = e & PTE_ADDR_MASK;
            if e & PTE_PRESENT == 0 || next == self.scratch[3 - level] { return; }
            table = next;
        }
        st.write(table, idx[3], self.scratch_entry(3));
    }

    /// Walk to the PTE of `va` (the scratch PTE when unmapped).
    pub fn lookup(&self, st: &dyn PtStore, va: u64) -> u64 {
        let idx = ppgtt_indices(va);
        let mut table = self.pml4;
        for level in 0..3 {
            let e = st.read(table, idx[level]);
            if e & PTE_PRESENT == 0 { return 0; }
            table = e & PTE_ADDR_MASK;
        }
        st.read(table, idx[3])
    }

    /// Physical address `va` resolves to (scratch page included).
    pub fn translate(&self, st: &dyn PtStore, va: u64) -> Option<u64> {
        let pte = self.lookup(st, va);
        if pte & PTE_PRESENT == 0 { None } else { Some((pte & PTE_ADDR_MASK) | (va & (PAGE - 1))) }
    }

    /// Number of table pages beyond the five fixed ones.
    pub fn table_pages(&self) -> usize { self.tables.len() }

    /// Free every page this PPGTT owns.
    pub fn destroy(self, st: &mut dyn PtStore) {
        for t in self.tables { st.free_page(t); }
        for s in self.scratch { st.free_page(s); }
        st.free_page(self.pml4);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Command streams
// ─────────────────────────────────────────────────────────────────────────────

pub mod cmd {
    use alloc::vec::Vec;

    pub const fn mi(opcode: u32, flags: u32) -> u32 { (opcode << 23) | flags }
    pub const MI_NOOP: u32 = 0;
    pub const MI_USER_INTERRUPT: u32 = mi(0x02, 0);
    pub const MI_ARB_CHECK: u32 = mi(0x05, 0);
    pub const MI_ARB_ON_OFF: u32 = mi(0x08, 0);
    pub const MI_ARB_ENABLE: u32 = 1;
    pub const MI_ARB_DISABLE: u32 = 0;
    pub const MI_BATCH_BUFFER_END: u32 = mi(0x0A, 0);
    pub const MI_STORE_DWORD_IMM_GEN4: u32 = mi(0x20, 2);
    pub const MI_USE_GGTT: u32 = 1 << 22;
    pub const MI_LRI_FORCE_POSTED: u32 = 1 << 12;
    pub const MI_FLUSH_DW: u32 = mi(0x26, 1);
    pub const MI_FLUSH_DW_STORE_INDEX: u32 = 1 << 21;
    pub const MI_INVALIDATE_TLB: u32 = 1 << 18;
    pub const MI_FLUSH_DW_OP_STOREDW: u32 = 1 << 14;
    pub const MI_FLUSH_DW_USE_GTT: u32 = 1 << 2;
    pub const MI_BATCH_BUFFER_START_GEN8: u32 = mi(0x31, 1);
    /// MI_BATCH_BUFFER_START bit 8: the batch lives in the PPGTT (clear =
    /// GGTT, a privileged "secure" batch).
    pub const MI_BATCH_PPGTT: u32 = 1 << 8;
    pub const XY_COLOR_BLT_CMD: u32 = (2 << 29) | (0x50 << 22);
    pub const XY_SRC_COPY_BLT_CMD: u32 = (2 << 29) | (0x53 << 22);
    pub const BLT_WRITE_RGBA: u32 = (1 << 21) | (1 << 20);
    pub const BLT_DEPTH_32: u32 = 3 << 24;
    pub const BLT_ROP_COLOR_COPY: u32 = 0xF0 << 16;
    pub const BLT_ROP_SRC_COPY: u32 = 0xCC << 16;
    pub const XY_BLT_DST_TILED: u32 = 1 << 11;
    pub const XY_BLT_SRC_TILED: u32 = 1 << 15;

    /// MI_LOAD_REGISTER_IMM header for `n` registers.
    pub const fn lri_header(n: u32) -> u32 { mi(0x22, 2 * n - 1) }

    /// MI opcode of a command header (bits 28:23), valid for MI commands.
    pub fn mi_opcode(h: u32) -> u32 { (h >> 23) & 0x3F }

    /// Store `val` at GGTT address `addr` (`MI_STORE_DWORD_IMM_GEN4 |
    /// MI_USE_GGTT`), 4 dwords.
    pub fn store_dword_ggtt(cs: &mut Vec<u32>, addr: u64, val: u32) {
        cs.extend_from_slice(&[MI_STORE_DWORD_IMM_GEN4 | MI_USE_GGTT, addr as u32, (addr >> 32) as u32, val]);
    }

    /// The xcs breadcrumb (`gen8_emit_ggtt_write`): MI_FLUSH_DW with a
    /// post-sync dword write to GGTT `addr`. 4 dwords. `addr` must be 8-byte
    /// aligned and must not have bit 5 set (w/a).
    pub fn flush_dw_store_ggtt(cs: &mut Vec<u32>, addr: u32, val: u32, invalidate_tlb: bool) {
        debug_assert!(addr & 7 == 0 && addr & (1 << 5) == 0);
        let mut h = (MI_FLUSH_DW + 1) | MI_FLUSH_DW_OP_STOREDW;
        if invalidate_tlb { h |= MI_INVALIDATE_TLB; }
        cs.extend_from_slice(&[h, addr | MI_FLUSH_DW_USE_GTT, 0, val]);
    }

    /// The request tail Linux emits after the breadcrumb on gen8/9 xcs
    /// (`gen8_emit_fini_breadcrumb_tail` without semaphores, plus the
    /// WaIdleLiteRestore padding): 4 dwords.
    pub fn fini_tail(cs: &mut Vec<u32>) {
        cs.extend_from_slice(&[MI_USER_INTERRUPT, MI_ARB_ON_OFF | MI_ARB_ENABLE, MI_ARB_CHECK, MI_NOOP]);
    }

    /// MI_BATCH_BUFFER_START (gen8, 48-bit address), 4 dwords with the
    /// arbitration enable before it.
    pub fn batch_start(cs: &mut Vec<u32>, addr: u64, ppgtt: bool) {
        cs.extend_from_slice(&[
            MI_ARB_ON_OFF | MI_ARB_ENABLE,
            MI_BATCH_BUFFER_START_GEN8 | if ppgtt { MI_BATCH_PPGTT } else { 0 },
            addr as u32, (addr >> 32) as u32,
        ]);
    }

    /// Fill a rectangle `[x1, x2) x [y1, y2)` of a 32-bpp surface with
    /// `color` (XY_COLOR_BLT, gen8+ 7-dword form with a 48-bit address).
    /// `pitch` is in bytes for linear surfaces; X-tiled surfaces take it in
    /// dwords and set the tiled bit.
    pub fn xy_color_blt(cs: &mut Vec<u32>, dst: u64, pitch_bytes: u32, x_tiled: bool,
                        x1: u16, y1: u16, x2: u16, y2: u16, color: u32) {
        let mut h = XY_COLOR_BLT_CMD | BLT_WRITE_RGBA | (7 - 2);
        let pitch = if x_tiled { h |= XY_BLT_DST_TILED; pitch_bytes / 4 } else { pitch_bytes };
        cs.extend_from_slice(&[
            h,
            BLT_DEPTH_32 | BLT_ROP_COLOR_COPY | (pitch & 0xFFFF),
            (y1 as u32) << 16 | x1 as u32,
            (y2 as u32) << 16 | x2 as u32,
            dst as u32, (dst >> 32) as u32,
            color,
        ]);
    }

    /// Copy a `w x h` rectangle between linear 32-bpp surfaces (XY_SRC_COPY_BLT,
    /// gen8+ 10-dword form).
    pub fn xy_src_copy_blt(cs: &mut Vec<u32>, dst: u64, dst_pitch: u32, dx: u16, dy: u16,
                           src: u64, src_pitch: u32, sx: u16, sy: u16, w: u16, h: u16) {
        cs.extend_from_slice(&[
            XY_SRC_COPY_BLT_CMD | BLT_WRITE_RGBA | (10 - 2),
            BLT_DEPTH_32 | BLT_ROP_SRC_COPY | (dst_pitch & 0xFFFF),
            (dy as u32) << 16 | dx as u32,
            ((dy + h) as u32) << 16 | (dx + w) as u32,
            dst as u32, (dst >> 32) as u32,
            (sy as u32) << 16 | sx as u32,
            src_pitch & 0xFFFF,
            src as u32, (src >> 32) as u32,
        ]);
    }

    /// PIPE_CONTROL (gen8+, 6 dwords) and the flag bits the kernel uses.
    pub const PIPE_CONTROL6: u32 = (3 << 29) | (3 << 27) | (2 << 24) | (6 - 2);
    pub const PC_GLOBAL_GTT_IVB: u32 = 1 << 24;
    pub const PC_CS_STALL: u32 = 1 << 20;
    pub const PC_TLB_INVALIDATE: u32 = 1 << 18;
    pub const PC_QW_WRITE: u32 = 1 << 14;
    pub const PC_RENDER_TARGET_CACHE_FLUSH: u32 = 1 << 12;
    pub const PC_INSTRUCTION_CACHE_INVALIDATE: u32 = 1 << 11;
    pub const PC_TEXTURE_CACHE_INVALIDATE: u32 = 1 << 10;
    pub const PC_FLUSH_ENABLE: u32 = 1 << 7;
    pub const PC_DC_FLUSH_ENABLE: u32 = 1 << 5;
    pub const PC_VF_CACHE_INVALIDATE: u32 = 1 << 4;
    pub const PC_CONST_CACHE_INVALIDATE: u32 = 1 << 3;
    pub const PC_STATE_CACHE_INVALIDATE: u32 = 1 << 2;
    pub const PC_DEPTH_CACHE_FLUSH: u32 = 1 << 0;

    pub fn pipe_control(cs: &mut Vec<u32>, flags: u32, addr: u32, data: u32) {
        cs.extend_from_slice(&[PIPE_CONTROL6, flags, addr, 0, data, 0]);
    }

    /// Render-engine invalidate at the start of a request
    /// (`gen8_emit_flush_rcs(EMIT_INVALIDATE)` on gen9): a null PIPE_CONTROL
    /// first (the gen9 VF-invalidate workaround), then the invalidating one
    /// with its required post-sync write, here to an absolute GGTT scratch
    /// dword (Linux uses the per-context status page index instead).
    pub fn rcs_invalidate(cs: &mut Vec<u32>, scratch_ggtt: u32) {
        pipe_control(cs, 0, 0, 0);
        pipe_control(cs, PC_CS_STALL | PC_TLB_INVALIDATE | PC_INSTRUCTION_CACHE_INVALIDATE
            | PC_TEXTURE_CACHE_INVALIDATE | PC_VF_CACHE_INVALIDATE | PC_CONST_CACHE_INVALIDATE
            | PC_STATE_CACHE_INVALIDATE | PC_QW_WRITE | PC_GLOBAL_GTT_IVB, scratch_ggtt, 0);
    }

    /// The render breadcrumb (`gen8_emit_fini_breadcrumb_rcs`): flush the
    /// render caches, then a CS-stalling qword write of `seqno` to GGTT `addr`.
    pub fn rcs_breadcrumb(cs: &mut Vec<u32>, addr: u32, seqno: u32) {
        pipe_control(cs, PC_CS_STALL | PC_TLB_INVALIDATE | PC_RENDER_TARGET_CACHE_FLUSH
            | PC_DEPTH_CACHE_FLUSH | PC_DC_FLUSH_ENABLE, 0, 0);
        pipe_control(cs, PC_FLUSH_ENABLE | PC_CS_STALL | PC_GLOBAL_GTT_IVB | PC_QW_WRITE, addr, seqno);
    }

    /// `gen8_emit_flush_rcs(EMIT_BARRIER)`: flush and invalidate everything
    /// with a CS stall (before the context workaround LRIs).
    pub fn rcs_barrier(cs: &mut Vec<u32>, scratch_ggtt: u32) {
        pipe_control(cs, 0, 0, 0);
        pipe_control(cs, PC_CS_STALL | PC_RENDER_TARGET_CACHE_FLUSH | PC_DEPTH_CACHE_FLUSH | PC_DC_FLUSH_ENABLE
            | PC_FLUSH_ENABLE | PC_TLB_INVALIDATE | PC_INSTRUCTION_CACHE_INVALIDATE | PC_TEXTURE_CACHE_INVALIDATE
            | PC_VF_CACHE_INVALIDATE | PC_CONST_CACHE_INVALIDATE | PC_STATE_CACHE_INVALIDATE | PC_QW_WRITE
            | PC_GLOBAL_GTT_IVB, scratch_ggtt, 0);
    }

    /// Pad to an 8-byte (qword) boundary with MI_NOOP: ring tails must be
    /// qword aligned.
    pub fn pad_qword(cs: &mut Vec<u32>) {
        if cs.len() % 2 != 0 { cs.push(MI_NOOP); }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Ring buffer arithmetic
// ─────────────────────────────────────────────────────────────────────────────

/// RING_CTL value for a ring of `size` bytes (multiple of 4 KiB, <= 2 MiB).
pub fn ring_ctl(size: u32) -> u32 { (size - 4096) | reg::RING_VALID }

/// One execlist request as the ring sees it, shared by the G2 self-tests
/// and the G3b submission core: invalidate (TLBs, caches), `pre` (kernel
/// commands such as the context workaround LRIs), the optional batch start
/// (`(address, ppgtt)`: a user batch runs from its context's PPGTT,
/// non-privileged; a kernel batch from the GGTT), then the breadcrumb that
/// writes `seqno` to the GGTT dword `seq_ggtt`, the user interrupt and the
/// qword-aligned tail. `scratch_ggtt` takes the invalidate's post-sync write.
pub fn build_request(class: EngineClass, scratch_ggtt: u32, seq_ggtt: u32, seqno: u32, pre: &[u32],
                     batch: Option<(u64, bool)>) -> Vec<u32> {
    let mut cs: Vec<u32> = Vec::with_capacity(pre.len() + 48);
    let render = class == EngineClass::Render;
    if render { cmd::rcs_invalidate(&mut cs, scratch_ggtt); } else { cmd::flush_dw_store_ggtt(&mut cs, scratch_ggtt, 0, true); }
    cs.extend_from_slice(pre);
    if let Some((addr, ppgtt)) = batch {
        cmd::batch_start(&mut cs, addr, ppgtt);
        cs.extend_from_slice(&[cmd::MI_ARB_ON_OFF | cmd::MI_ARB_DISABLE, cmd::MI_NOOP]);
    }
    if render { cmd::rcs_breadcrumb(&mut cs, seq_ggtt, seqno); } else { cmd::flush_dw_store_ggtt(&mut cs, seq_ggtt, seqno, false); }
    cmd::fini_tail(&mut cs);
    cmd::pad_qword(&mut cs);
    cs
}

/// Where a request of `bytes` goes in a ring of `size` bytes whose next
/// free byte is `tail`: `(noop_dwords, start)` — when it does not fit before
/// the end, the rest of the ring is filled with MI_NOOP and it starts at 0.
/// `None` if it can never fit.
pub fn ring_place(tail: u32, bytes: u32, size: u32) -> Option<(u32, u32)> {
    if bytes + 8 > size || tail % 8 != 0 || tail >= size { return None; }
    if tail + bytes <= size { Some((0, tail)) } else { Some(((size - tail) / 4, 0)) }
}

/// Free bytes between `tail` (where we write next) and `head` (where the GPU
/// reads), keeping one qword gap so tail == head always means "empty".
pub fn ring_space(head: u32, tail: u32, size: u32) -> u32 {
    let used = tail.wrapping_sub(head) & (size - 1);
    size - used - 8
}

// ─────────────────────────────────────────────────────────────────────────────
// Execlist logical ring context (LRC)
// ─────────────────────────────────────────────────────────────────────────────

/// Dword indices of the register state page (the page after the per-process
/// HW status page) — intel_lrc_reg.h. Each names the VALUE slot of a pair.
pub mod ctx {
    pub const CONTEXT_CONTROL: usize = 0x02 + 1;
    pub const RING_HEAD: usize = 0x04 + 1;
    pub const RING_TAIL: usize = 0x06 + 1;
    pub const RING_START: usize = 0x08 + 1;
    pub const RING_CTL: usize = 0x0A + 1;
    pub const BB_STATE: usize = 0x10 + 1;
    pub const TIMESTAMP: usize = 0x22 + 1;
    pub const PDP3_UDW: usize = 0x24 + 1;
    pub const PDP3_LDW: usize = 0x26 + 1;
    pub const PDP2_UDW: usize = 0x28 + 1;
    pub const PDP2_LDW: usize = 0x2A + 1;
    pub const PDP1_UDW: usize = 0x2C + 1;
    pub const PDP1_LDW: usize = 0x2E + 1;
    pub const PDP0_UDW: usize = 0x30 + 1;
    pub const PDP0_LDW: usize = 0x32 + 1;
    pub const R_PWR_CLK_STATE: usize = 0x42 + 1;
    /// RING_MI_MODE slot on gen9 (`lrc_ring_mi_mode`).
    pub const GEN9_RING_MI_MODE: usize = 0x54;
    /// RING_BB_OFFSET slot on gen9 (`lrc_ring_bb_offset`).
    pub const GEN9_BB_OFFSET: usize = 0x64;

    pub const CTRL_ENGINE_CTX_RESTORE_INHIBIT: u32 = 1 << 0;
    pub const CTRL_RS_CTX_ENABLE: u32 = 1 << 1;
    pub const CTRL_ENGINE_CTX_SAVE_INHIBIT: u32 = 1 << 2;
    pub const CTRL_INHIBIT_SYN_CTX_SWITCH: u32 = 1 << 3;

    /// Page index of the register state inside the context object.
    pub const LRC_STATE_PN: usize = 1;
    /// Scratch dword in the PPHWSP (`LRC_PPHWSP_SCRATCH`).
    pub const PPHWSP_SCRATCH: usize = 0x34;

    // Descriptor bits (intel_lrc.h).
    pub const DESC_VALID: u64 = 1 << 0;
    pub const DESC_FORCE_PD_RESTORE: u64 = 1 << 1;
    pub const DESC_FORCE_RESTORE: u64 = 1 << 2;
    pub const DESC_PRIVILEGE: u64 = 1 << 8;
    pub const DESC_ADDRESSING_SHIFT: u64 = 3;
    pub const LEGACY_32B: u64 = 1;
    pub const LEGACY_64B: u64 = 3;
    pub const DESC_ID_SHIFT: u64 = 32;
    pub const DESC_ID_WIDTH: u32 = 21;
}

const fn lri(count: u8, posted: bool) -> u8 { ((posted as u8) << 6) | count }
const fn nop(n: u8) -> u8 { 0x80 | n }

/// gen9_xcs_offsets (intel_lrc.c), encoded as Linux encodes it: NOP(n) skips
/// n dwords, LRI(count, flags) emits a header followed by `count` register
/// offsets; offsets < 0x200 take one byte (offset >> 2), larger ones two
/// bytes (high part with bit 7, then low 7 bits).
pub const GEN9_XCS_OFFSETS: &[u8] = &[
    nop(1), lri(14, true),
    0x80 | (0x244 >> 9) as u8, ((0x244 >> 2) & 0x7f) as u8,
    0x034 >> 2, 0x030 >> 2, 0x038 >> 2, 0x03c >> 2, (0x168 >> 2) as u8, (0x140 >> 2) as u8,
    (0x110 >> 2) as u8, (0x11c >> 2) as u8, (0x114 >> 2) as u8, (0x118 >> 2) as u8,
    (0x1c0 >> 2) as u8, (0x1c4 >> 2) as u8, (0x1c8 >> 2) as u8,
    nop(3), lri(9, true),
    0x81, (0x3a8 >> 2) as u8 & 0x7f, 0x81, (0x28c >> 2) as u8 & 0x7f, 0x81, (0x288 >> 2) as u8 & 0x7f,
    0x81, (0x284 >> 2) as u8 & 0x7f, 0x81, (0x280 >> 2) as u8 & 0x7f, 0x81, (0x27c >> 2) as u8 & 0x7f,
    0x81, (0x278 >> 2) as u8 & 0x7f, 0x81, (0x274 >> 2) as u8 & 0x7f, 0x81, (0x270 >> 2) as u8 & 0x7f,
    nop(13), lri(1, true),
    0x81, (0x200 >> 2) as u8 & 0x7f,
    nop(13), lri(44, true),
    0x028 >> 2, 0x09c >> 2, 0x0c0 >> 2, (0x178 >> 2) as u8, (0x17c >> 2) as u8,
    0x81, (0x358 >> 2) as u8 & 0x7f,
    (0x170 >> 2) as u8, (0x150 >> 2) as u8, (0x154 >> 2) as u8, (0x158 >> 2) as u8,
    0x82, (0x41c >> 2) as u8 & 0x7f,
    0x83, 0x00, 0x83, 0x01, 0x83, 0x02, 0x83, 0x03, 0x83, 0x04, 0x83, 0x05, 0x83, 0x06, 0x83, 0x07,
    0x83, 0x08, 0x83, 0x09, 0x83, 0x0a, 0x83, 0x0b, 0x83, 0x0c, 0x83, 0x0d, 0x83, 0x0e, 0x83, 0x0f,
    0x83, 0x10, 0x83, 0x11, 0x83, 0x12, 0x83, 0x13, 0x83, 0x14, 0x83, 0x15, 0x83, 0x16, 0x83, 0x17,
    0x83, 0x18, 0x83, 0x19, 0x83, 0x1a, 0x83, 0x1b, 0x83, 0x1c, 0x83, 0x1d, 0x83, 0x1e, 0x83, 0x1f,
    0x068 >> 2,
    0,
];

/// gen9_rcs_offsets (intel_lrc.c).
pub const GEN9_RCS_OFFSETS: &[u8] = &[
    nop(1), lri(14, true),
    0x81, (0x244 >> 2) as u8 & 0x7f,
    0x034 >> 2, 0x030 >> 2, 0x038 >> 2, 0x03c >> 2, (0x168 >> 2) as u8, (0x140 >> 2) as u8,
    (0x110 >> 2) as u8, (0x11c >> 2) as u8, (0x114 >> 2) as u8, (0x118 >> 2) as u8,
    (0x1c0 >> 2) as u8, (0x1c4 >> 2) as u8, (0x1c8 >> 2) as u8,
    nop(3), lri(9, true),
    0x81, (0x3a8 >> 2) as u8 & 0x7f, 0x81, (0x28c >> 2) as u8 & 0x7f, 0x81, (0x288 >> 2) as u8 & 0x7f,
    0x81, (0x284 >> 2) as u8 & 0x7f, 0x81, (0x280 >> 2) as u8 & 0x7f, 0x81, (0x27c >> 2) as u8 & 0x7f,
    0x81, (0x278 >> 2) as u8 & 0x7f, 0x81, (0x274 >> 2) as u8 & 0x7f, 0x81, (0x270 >> 2) as u8 & 0x7f,
    nop(13), lri(1, false),
    0xc8 >> 2,
    nop(13), lri(44, true),
    0x028 >> 2, 0x09c >> 2, 0x0c0 >> 2, (0x178 >> 2) as u8, (0x17c >> 2) as u8,
    0x81, (0x358 >> 2) as u8 & 0x7f,
    (0x170 >> 2) as u8, (0x150 >> 2) as u8, (0x154 >> 2) as u8, (0x158 >> 2) as u8,
    0x82, (0x41c >> 2) as u8 & 0x7f,
    0x83, 0x00, 0x83, 0x01, 0x83, 0x02, 0x83, 0x03, 0x83, 0x04, 0x83, 0x05, 0x83, 0x06, 0x83, 0x07,
    0x83, 0x08, 0x83, 0x09, 0x83, 0x0a, 0x83, 0x0b, 0x83, 0x0c, 0x83, 0x0d, 0x83, 0x0e, 0x83, 0x0f,
    0x83, 0x10, 0x83, 0x11, 0x83, 0x12, 0x83, 0x13, 0x83, 0x14, 0x83, 0x15, 0x83, 0x16, 0x83, 0x17,
    0x83, 0x18, 0x83, 0x19, 0x83, 0x1a, 0x83, 0x1b, 0x83, 0x1c, 0x83, 0x1d, 0x83, 0x1e, 0x83, 0x1f,
    0x068 >> 2,
    0,
];

/// `set_offsets`: lay out the MI_LOAD_REGISTER_IMM headers and register
/// offsets of a context image (values untouched). `close` appends
/// MI_BATCH_BUFFER_END (Linux does it for a fresh, restore-inhibited image).
/// Returns the number of dwords used.
pub fn lrc_set_offsets(regs: &mut [u32], table: &[u8], mmio_base: u32, close: bool) -> usize {
    let mut r = 0usize;
    let mut d = 0usize;
    while d < table.len() && table[d] != 0 {
        let b = table[d];
        if b & 0x80 != 0 {
            r += (b & 0x7F) as usize;
            d += 1;
            continue;
        }
        let count = (b & 0x3F) as u32;
        let posted = (b >> 6) & 1 != 0;
        d += 1;
        regs[r] = cmd::lri_header(count) | if posted { cmd::MI_LRI_FORCE_POSTED } else { 0 };
        r += 1;
        for _ in 0..count {
            let mut off: u32 = 0;
            loop {
                let v = table[d];
                d += 1;
                off = (off << 7) | (v & 0x7F) as u32;
                if v & 0x80 == 0 { break; }
            }
            regs[r] = mmio_base + (off << 2);
            r += 2;
        }
    }
    if close {
        regs[r] = cmd::MI_BATCH_BUFFER_END;
        r += 1;
    }
    r
}

/// The register offsets (absolute, engine base added) a context image of
/// `table` loads, in image order — for logging and tests.
pub fn lrc_registers(table: &[u8], mmio_base: u32) -> Vec<(usize, u32)> {
    let mut regs = alloc::vec![0u32; 1024];
    let used = lrc_set_offsets(&mut regs, table, mmio_base, false);
    let mut out = Vec::new();
    let mut i = 0;
    while i < used {
        let h = regs[i];
        if h != 0 && cmd::mi_opcode(h) == 0x22 {
            let n = ((h & 0xFF) + 1) / 2;
            for k in 0..n as usize {
                out.push((i + 1 + 2 * k + 1, regs[i + 1 + 2 * k]));
            }
            i += 1 + 2 * n as usize;
        } else {
            i += 1;
        }
    }
    out
}

/// Everything `lrc_init_regs` + `lrc_update_regs` put into a fresh gen9
/// context image.
#[derive(Clone, Copy, Debug)]
pub struct LrcInit {
    pub class: EngineClass,
    pub mmio_base: u32,
    pub ring_ggtt: u32,
    pub ring_size: u32,
    pub head: u32,
    pub tail: u32,
    /// Physical address of the PML4 (4-level PPGTT).
    pub pml4: u64,
    /// Restore-inhibit: the first submission of an image that was never
    /// saved by the hardware.
    pub inhibit: bool,
    /// R_PWR_CLK_STATE (render only; 0 = leave the power-on default).
    pub rpcs: u32,
}

/// Build the register state page of a fresh context (`__lrc_init_regs` with
/// `inhibit`, then `lrc_update_regs`). `regs` is the 1024-dword page at
/// LRC_STATE_PN.
pub fn lrc_init_regs(regs: &mut [u32], p: &LrcInit) {
    if p.inhibit { for r in regs.iter_mut() { *r = 0; } }
    let table = if p.class == EngineClass::Render { GEN9_RCS_OFFSETS } else { GEN9_XCS_OFFSETS };
    lrc_set_offsets(regs, table, p.mmio_base, p.inhibit);
    let mut ctl = masked_en(ctx::CTRL_INHIBIT_SYN_CTX_SWITCH) | masked_dis(ctx::CTRL_ENGINE_CTX_RESTORE_INHIBIT);
    if p.inhibit { ctl |= ctx::CTRL_ENGINE_CTX_RESTORE_INHIBIT; }
    ctl |= masked_dis(ctx::CTRL_ENGINE_CTX_SAVE_INHIBIT | ctx::CTRL_RS_CTX_ENABLE);
    regs[ctx::CONTEXT_CONTROL] = ctl;
    regs[ctx::TIMESTAMP] = 0;
    regs[ctx::GEN9_BB_OFFSET + 1] = 0;
    // 4-level PPGTT: PDP0 holds the PML4; the other PDPs are ignored.
    regs[ctx::PDP0_UDW] = (p.pml4 >> 32) as u32;
    regs[ctx::PDP0_LDW] = p.pml4 as u32;
    // __reset_stop_ring: clear STOP_RING through the mask.
    let mi = ctx::GEN9_RING_MI_MODE + 1;
    regs[mi] &= !reg::STOP_RING;
    regs[mi] |= reg::STOP_RING << 16;
    regs[ctx::RING_START] = p.ring_ggtt;
    regs[ctx::RING_HEAD] = p.head;
    regs[ctx::RING_TAIL] = p.tail;
    regs[ctx::RING_CTL] = ring_ctl(p.ring_size);
    if p.class == EngineClass::Render && p.rpcs != 0 {
        regs[ctx::R_PWR_CLK_STATE] = p.rpcs;
    }
}

/// Execlist context descriptor (`lrc_descriptor`): `lrca` is the GGTT
/// address of the context object (page aligned, PPHWSP first), `hw_id` the
/// 21-bit context id reported back in the CSB.
pub fn ctx_descriptor(lrca: u32, hw_id: u32, four_level: bool, force_restore: bool) -> u64 {
    let mode = if four_level { ctx::LEGACY_64B } else { ctx::LEGACY_32B };
    let mut d = (mode << ctx::DESC_ADDRESSING_SHIFT) | ctx::DESC_VALID | ctx::DESC_PRIVILEGE;
    if force_restore { d |= ctx::DESC_FORCE_RESTORE; }
    d | (lrca as u64 & 0xFFFF_F000) | (((hw_id & ((1 << ctx::DESC_ID_WIDTH) - 1)) as u64) << ctx::DESC_ID_SHIFT)
}

/// The four ELSP writes for a submission of `port0` (and optionally
/// `port1`) on gen8/9: element 1 upper, lower, then element 0 upper, lower
/// (the write of element 0's lower dword triggers the load).
pub fn elsp_writes(port0: u64, port1: u64) -> [u32; 4] {
    [(port1 >> 32) as u32, port1 as u32, (port0 >> 32) as u32, port0 as u32]
}

// ─────────────────────────────────────────────────────────────────────────────
// Context status buffer
// ─────────────────────────────────────────────────────────────────────────────

pub const CSB_ENTRIES: u32 = 6;
pub const CSB_IDLE_ACTIVE: u32 = 1 << 0;
pub const CSB_PREEMPTED: u32 = 1 << 1;
pub const CSB_ELEMENT_SWITCH: u32 = 1 << 2;
pub const CSB_ACTIVE_IDLE: u32 = 1 << 3;
pub const CSB_COMPLETE: u32 = 1 << 4;
pub const CSB_LITE_RESTORE: u32 = 1 << 15;
/// Index of the CSB mirror and its write pointer in the engine HWSP (dwords).
pub const HWS_CSB_BUF0_INDEX: usize = 0x10;
pub const HWS_CSB_WRITE_INDEX: usize = 0x1F;
/// Breadcrumb slot in the engine HWSP (I915_GEM_HWS_SEQNO).
pub const HWS_SEQNO_INDEX: usize = 0x40;

/// Reset value of RING_CONTEXT_STATUS_PTR: read and write pointers at the
/// last entry, all mask bits set (`reset_csb_pointers`).
pub fn csb_ptr_reset() -> u32 {
    let r = CSB_ENTRIES - 1;
    0xFFFF << 16 | r << 8 | r
}

/// Human-readable status bits of a CSB entry (lower dword); the upper dword
/// is the context id.
pub fn csb_status_str(status: u32) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    for (bit, name) in [(CSB_IDLE_ACTIVE, "idle->active"), (CSB_PREEMPTED, "preempted"),
                        (CSB_ELEMENT_SWITCH, "element-switch"), (CSB_ACTIVE_IDLE, "active->idle"),
                        (CSB_COMPLETE, "complete"), (CSB_LITE_RESTORE, "lite-restore")] {
        if status & bit != 0 {
            if !s.is_empty() { s.push('|'); }
            s.push_str(name);
        }
    }
    if s.is_empty() { s.push_str("none"); }
    s
}

/// The CSB entries written since `read` up to `write` (both in 0..6), as
/// indices, oldest first.
pub fn csb_pending(read: u32, write: u32) -> Vec<u32> {
    let mut v = Vec::new();
    let mut i = read;
    while i != write {
        i = (i + 1) % CSB_ENTRIES;
        v.push(i);
        if v.len() > CSB_ENTRIES as usize { break; }
    }
    v
}

// ─────────────────────────────────────────────────────────────────────────────
// Hang detection
// ─────────────────────────────────────────────────────────────────────────────

/// What one hang-check sample decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HangVerdict {
    /// The breadcrumb arrived.
    Done,
    /// Still running and making progress (ACTHD / HEAD moved), or within the
    /// grace period.
    Running,
    /// No progress for the stall limit, or the engine reported an error.
    Hung,
    /// The total budget ran out while the engine was still moving.
    Timeout,
}

/// Decide whether a submitted request finished, is progressing, or hung —
/// the decision Linux's hangcheck/heartbeat makes, as a pure state machine
/// over register samples. Times in nanoseconds.
pub struct HangCheck {
    pub start_ns: u64,
    pub budget_ns: u64,
    pub stall_ns: u64,
    last_acthd: u64,
    last_head: u32,
    last_progress_ns: u64,
}

impl HangCheck {
    pub fn new(now_ns: u64, budget_ns: u64, stall_ns: u64) -> HangCheck {
        HangCheck { start_ns: now_ns, budget_ns, stall_ns, last_acthd: u64::MAX, last_head: u32::MAX, last_progress_ns: now_ns }
    }

    /// One sample: `seqno_done` = the breadcrumb is visible; `acthd`/`head`
    /// = RING_ACTHD / RING_HEAD; `eir` = RING_EIR (any bit = error).
    pub fn sample(&mut self, now_ns: u64, seqno_done: bool, acthd: u64, head: u32, eir: u32) -> HangVerdict {
        if seqno_done { return HangVerdict::Done; }
        if eir != 0 { return HangVerdict::Hung; }
        if acthd != self.last_acthd || head != self.last_head {
            self.last_acthd = acthd;
            self.last_head = head;
            self.last_progress_ns = now_ns;
        }
        if now_ns.wrapping_sub(self.last_progress_ns) >= self.stall_ns { return HangVerdict::Hung; }
        if now_ns.wrapping_sub(self.start_ns) >= self.budget_ns { return HangVerdict::Timeout; }
        HangVerdict::Running
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Display decode
// ─────────────────────────────────────────────────────────────────────────────

pub fn plane_format_name(plane_ctl: u32) -> &'static str {
    match (plane_ctl >> 24) & 0xF {
        0 => "YUV422", 1 => "NV12", 2 => "XRGB2101010", 3 => "P010", 4 => "XRGB8888",
        5 => "P012", 6 => "XRGB16161616F", 7 => "P016", 8 => "XYUV", 12 => "INDEXED8",
        14 => "RGB565", _ => "?",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tiling { Linear, X, Y, Yf, Other(u8) }

pub fn plane_tiling(plane_ctl: u32) -> Tiling {
    match (plane_ctl >> 10) & 7 { 0 => Tiling::Linear, 1 => Tiling::X, 4 => Tiling::Y, 5 => Tiling::Yf, t => Tiling::Other(t as u8) }
}

/// PLANE_STRIDE to bytes: linear strides are in 64-byte units, X-tiled in
/// 512-byte units (one X tile row), Y/Yf in 128-byte units.
pub fn plane_stride_bytes(stride_reg: u32, tiling: Tiling) -> u32 {
    let units = stride_reg & 0x3FF;
    match tiling {
        Tiling::Linear => units * 64,
        Tiling::X => units * 512,
        Tiling::Y | Tiling::Yf => units * 128,
        Tiling::Other(_) => units * 64,
    }
}

/// TRANS_DDI_FUNC_CTL decode: (enabled, port letter, mode, bpc, eDP input
/// pipe for the eDP transcoder, lane count).
pub struct DdiFunc {
    pub enabled: bool,
    pub port: char,
    pub mode: &'static str,
    pub bpc: u8,
    pub edp_input: &'static str,
    pub lanes: u8,
}

pub fn decode_trans_ddi_func_ctl(v: u32) -> DdiFunc {
    let port = (b'A' + ((v >> 28) & 7) as u8) as char;
    let mode = match (v >> 24) & 7 {
        0 => "HDMI", 1 => "DVI", 2 => "DP-SST", 3 => "DP-MST", 4 => "FDI", _ => "?",
    };
    let bpc = match (v >> 20) & 7 { 0 => 8, 1 => 10, 2 => 6, 3 => 12, _ => 0 };
    let edp_input = match (v >> 12) & 7 {
        0 => "pipe A (always on)", 4 => "pipe A", 5 => "pipe B", 6 => "pipe C", 7 => "pipe D", _ => "?",
    };
    DdiFunc { enabled: v >> 31 != 0, port, mode, bpc, edp_input, lanes: (((v >> 1) & 7) + 1) as u8 }
}

/// A transcoder timing pair register (HTOTAL etc.): (active-or-start + 1,
/// total-or-end + 1).
pub fn timing_pair(v: u32) -> (u32, u32) { ((v & 0x1FFF) + 1, ((v >> 16) & 0x1FFF) + 1) }

/// PIPESRC: (width, height).
pub fn pipe_src(v: u32) -> (u32, u32) { (((v >> 16) & 0x1FFF) + 1, (v & 0xFFF) + 1) }

/// Display power well request/state bits of one HSW_PWR_WELL_CTL{1,2,3,4}
/// register on SKL/KBL (each well: request bit 2*idx+1, state bit 2*idx).
pub fn skl_power_wells(ctl: u32) -> alloc::string::String {
    // SKL_PW_CTL_IDX_*: MISC_IO 0, DDI_A_E 1, DDI_B 2, DDI_C 3, DDI_D 4,
    // PW_1 14, PW_2 15.
    let mut s = alloc::string::String::new();
    for (idx, name) in [(14u32, "PW1"), (15, "PW2"), (0, "MISC_IO"), (1, "DDI_A_E"), (2, "DDI_B"), (3, "DDI_C"), (4, "DDI_D")] {
        let req = ctl & (2 << (idx * 2)) != 0;
        let st = ctl & (1 << (idx * 2)) != 0;
        if req || st {
            if !s.is_empty() { s.push(' '); }
            s.push_str(name);
            s.push_str(match (req, st) { (true, true) => "=on", (true, false) => "=req", (false, true) => "=on(other)", _ => "" });
        }
    }
    if s.is_empty() { s.push_str("none"); }
    s
}

// ─────────────────────────────────────────────────────────────────────────────
// DMC firmware container (intel_dmc.c)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmcImage {
    /// CSS header: version (major << 16 | minor) and date (YYYYMMDD).
    pub css_version: u32,
    pub css_date: u32,
    pub package_ver: u8,
    /// Chosen entry's stepping (as in the package; '*' = any).
    pub stepping: (u8, u8),
    pub dmc_header_ver: u8,
    pub fw_version: u32,
    /// Offset of the firmware payload in the file and its size in dwords.
    pub payload_off: usize,
    pub payload_dwords: usize,
    /// (address, value) MMIO writes to do after the payload is loaded.
    pub mmio: Vec<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FwParseError { Truncated, BadCss, BadPackage, NoEntry, BadDmcHeader, BadMmio }

fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parse a gen9 DMC container (`kbl_dmc_ver1_04.bin` and friends): CSS
/// header, package header with firmware-info entries, DMC header (v1) with
/// the MMIO table, then the payload. `stepping` picks the entry ('*' wild).
pub fn parse_dmc(b: &[u8], stepping: (u8, u8)) -> Result<DmcImage, FwParseError> {
    // CSS header: 128 bytes, header_len in dwords at +4.
    let css_len = le32(b, 4).ok_or(FwParseError::Truncated)? as usize * 4;
    if css_len != 128 { return Err(FwParseError::BadCss); }
    let css_version = le32(b, 0x58).ok_or(FwParseError::Truncated)?;
    let css_date = le32(b, 0x14).ok_or(FwParseError::Truncated)?;
    // Package header.
    let p = css_len;
    let pkg_len = *b.get(p).ok_or(FwParseError::Truncated)? as usize * 4;
    let pkg_ver = *b.get(p + 1).ok_or(FwParseError::Truncated)?;
    let max_entries = match pkg_ver { 1 => 20, 2 => 32, _ => return Err(FwParseError::BadPackage) };
    if pkg_len != 16 + max_entries * 12 { return Err(FwParseError::BadPackage); }
    let n = le32(b, p + 12).ok_or(FwParseError::Truncated)? as usize;
    if n > max_entries { return Err(FwParseError::BadPackage); }
    // First match wins, as in Linux (`dmc_set_fw_offset`): the package lists
    // the more specific steppings first; '*' is a wildcard.
    let mut chosen: Option<(u32, (u8, u8))> = None;
    for i in 0..n {
        let e = p + 16 + i * 12;
        let st = *b.get(e + 2).ok_or(FwParseError::Truncated)?;
        let sst = *b.get(e + 3).ok_or(FwParseError::Truncated)?;
        let off = le32(b, e + 4).ok_or(FwParseError::Truncated)?;
        if off == 0xFFFF_FFFF { continue; }
        let matches = (sst == b'*' && st == stepping.0)
            || (st == stepping.0 && sst == stepping.1)
            || (st == b'*' && sst == b'*');
        if matches { chosen = Some((off, (st, sst))); break; }
    }
    let (off_dw, st) = chosen.ok_or(FwParseError::NoEntry)?;
    let h = p + pkg_len + off_dw as usize * 4;
    let sig = le32(b, h).ok_or(FwParseError::Truncated)?;
    if sig != 0x4040_3E3E { return Err(FwParseError::BadDmcHeader); }
    let hdr_len = *b.get(h + 4).ok_or(FwParseError::Truncated)? as usize;
    let hdr_ver = *b.get(h + 5).ok_or(FwParseError::Truncated)?;
    let fw_size = le32(b, h + 12).ok_or(FwParseError::Truncated)? as usize;
    let fw_version = le32(b, h + 16).ok_or(FwParseError::Truncated)?;
    let (mmio_count_off, max_mmio, addr_off, data_off, hdr_bytes) = match hdr_ver {
        // v1: base(20) count(4) addr[8] data[8] dfile[32] rsvd[8] = 128 bytes,
        // header_len in bytes.
        1 => (20, 8, 24, 56, hdr_len),
        // v3: base(20) start(4) rsvd[36] dfile[32] count(4) addr[20] data[20],
        // header_len in dwords.
        3 => (92, 20, 96, 176, hdr_len * 4),
        _ => return Err(FwParseError::BadDmcHeader),
    };
    let count = le32(b, h + mmio_count_off).ok_or(FwParseError::Truncated)? as usize;
    if count > max_mmio { return Err(FwParseError::BadMmio); }
    let mut mmio = Vec::new();
    for i in 0..count {
        let a = le32(b, h + addr_off + 4 * i).ok_or(FwParseError::Truncated)?;
        let d = le32(b, h + data_off + 4 * i).ok_or(FwParseError::Truncated)?;
        // Linux accepts only the DMC MMIO range on gen9 (0x80000..0x8FFFF).
        if !(0x8_0000..=0x8_FFFF).contains(&a) { return Err(FwParseError::BadMmio); }
        mmio.push((a, d));
    }
    let payload_off = h + hdr_bytes;
    if payload_off + fw_size * 4 > b.len() { return Err(FwParseError::Truncated); }
    Ok(DmcImage {
        css_version, css_date, package_ver: pkg_ver, stepping: st, dmc_header_ver: hdr_ver,
        fw_version, payload_off, payload_dwords: fw_size, mmio,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// ACPI OpRegion and VBT (intel_opregion.c, intel_vbt_defs.h)
// ─────────────────────────────────────────────────────────────────────────────

pub const OPREGION_SIZE: usize = 8 * 1024;
pub const OPREGION_SIGNATURE: &[u8; 16] = b"IntelGraphicsMem";
pub const OPREGION_ASLE_OFFSET: usize = 0x300;
pub const OPREGION_VBT_OFFSET: usize = 0x400;
pub const OPREGION_ASLE_EXT_OFFSET: usize = 0x1C00;
/// rvda (u64) / rvds (u32) inside mailbox #3.
pub const ASLE_RVDA: usize = 186;
pub const ASLE_RVDS: usize = 194;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpRegionInfo {
    pub size_kb: u32,
    pub version: (u8, u8, u8),
    pub mboxes: u32,
    /// Where the VBT is: (absolute physical address or None for "inside the
    /// OpRegion at `off`", offset or address, size).
    pub vbt: VbtLocation,
    pub bios_ver: alloc::string::String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VbtLocation {
    /// Mailbox #4 inside the OpRegion: (offset, max size).
    Mailbox4(usize, usize),
    /// Raw VBT outside the OpRegion: (physical address, size).
    Rvda(u64, usize),
    None,
}

/// Parse the 8 KiB OpRegion header and locate the VBT (`intel_opregion_setup`).
pub fn parse_opregion(b: &[u8], asls: u64) -> Option<OpRegionInfo> {
    if b.len() < OPREGION_SIZE || &b[..16] != OPREGION_SIGNATURE { return None; }
    let size_kb = le32(b, 16)?;
    let version = (b[23], b[22], b[21]);
    let mboxes = le32(b, 0x58)?;
    let bios_ver: alloc::string::String = b[24..56].iter().take_while(|&&c| c != 0)
        .map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect();
    let asle = mboxes & (1 << 2) != 0;
    let mut vbt = VbtLocation::None;
    if version.0 >= 2 && asle {
        let rvda = u64::from_le_bytes(b[OPREGION_ASLE_OFFSET + ASLE_RVDA..OPREGION_ASLE_OFFSET + ASLE_RVDA + 8].try_into().ok()?);
        let rvds = le32(b, OPREGION_ASLE_OFFSET + ASLE_RVDS)? as usize;
        if rvda != 0 && rvds != 0 {
            let addr = if version.0 > 2 || version.1 >= 1 { asls + rvda } else { rvda };
            vbt = VbtLocation::Rvda(addr, rvds);
        }
    }
    if vbt == VbtLocation::None {
        let end = if mboxes & (1 << 4) != 0 { OPREGION_ASLE_EXT_OFFSET } else { OPREGION_SIZE };
        if b.get(OPREGION_VBT_OFFSET..OPREGION_VBT_OFFSET + 4) == Some(b"$VBT") {
            vbt = VbtLocation::Mailbox4(OPREGION_VBT_OFFSET, end - OPREGION_VBT_OFFSET);
        }
    }
    Some(OpRegionInfo { size_kb, version, mboxes, vbt, bios_ver })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VbtInfo {
    pub signature: alloc::string::String,
    pub version: u16,
    pub vbt_size: usize,
    pub bdb_offset: usize,
    pub bdb_version: u16,
    /// (block id, size) in order.
    pub blocks: Vec<(u8, u16)>,
    /// LFP options (block 40) panel type, if present.
    pub panel_type: Option<u8>,
}

/// Validate a VBT (`intel_bios_is_valid_vbt`) and list its BDB blocks.
pub fn parse_vbt(b: &[u8]) -> Option<VbtInfo> {
    if b.len() < 0x30 || &b[..4] != b"$VBT" { return None; }
    let version = u16::from_le_bytes([b[20], b[21]]);
    let vbt_size = u16::from_le_bytes([b[24], b[25]]) as usize;
    let bdb_offset = le32(b, 28)? as usize;
    if vbt_size > b.len() || bdb_offset + 22 > vbt_size { return None; }
    let bdb = &b[bdb_offset..vbt_size];
    if &bdb[..16] != b"BIOS_DATA_BLOCK " { return None; }
    let bdb_version = u16::from_le_bytes([bdb[16], bdb[17]]);
    let hdr = u16::from_le_bytes([bdb[18], bdb[19]]) as usize;
    let bdb_size = (u16::from_le_bytes([bdb[20], bdb[21]]) as usize).min(bdb.len());
    let signature: alloc::string::String = b[..20].iter().take_while(|&&c| c != 0)
        .map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect();
    let mut blocks = Vec::new();
    let mut panel_type = None;
    let mut o = hdr;
    while o + 3 <= bdb_size && blocks.len() < 128 {
        let id = bdb[o];
        let mut size = u16::from_le_bytes([bdb[o + 1], bdb[o + 2]]) as usize;
        // Block 53 (MIPI sequence) v3+ carries a 32-bit size; not on KBL eDP.
        if id == 53 && o + 3 + 5 <= bdb_size && bdb[o + 3] >= 3 {
            size = le32(bdb, o + 4).unwrap_or(0) as usize;
        }
        if id == 0 || o + 3 + size > bdb_size { break; }
        if id == 40 && size >= 1 { panel_type = Some(bdb[o + 3] & 0xF); }
        blocks.push((id, size as u16));
        o += 3 + size;
    }
    Some(VbtInfo { signature, version, vbt_size, bdb_offset, bdb_version, blocks, panel_type })
}

/// Names of the BDB blocks that matter for a display driver.
pub fn bdb_block_name(id: u8) -> &'static str {
    match id {
        1 => "general features", 2 => "general definitions (child devices)", 9 => "SRD/PSR (old)",
        12 => "driver features", 22 => "SDVO panel DTDs?", 27 => "eDP", 40 => "LFP options",
        41 => "LFP data pointers", 42 => "LFP data", 43 => "LFP backlight", 44 => "LFP power",
        52 => "MIPI config", 53 => "MIPI sequence", 56 => "compression parameters",
        57 => "generic DTD", 58 => "PSR", _ => "",
    }
}
