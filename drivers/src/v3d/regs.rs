//! V3D 7.1 (BCM2712) register map shared by every `v3d` module, and the
//! hardware seam ([`V3dRegs`]) that keeps the driver logic host-testable.
//!
//! Offsets and bits follow upstream Linux `drivers/gpu/drm/v3d/v3d_regs.h`
//! for `ver >= 71`. Layout and provenance: artifacts/notes/lane-piv3d-design-2026-10-26.md.

/// Register access for the three V3D blocks (hub, core0, sms). The real
/// implementation is MMIO; host tests use a scripted register file.
pub trait V3dRegs { fn hub_r(&self, off: u32) -> u32; fn hub_w(&self, off: u32, v: u32);
                    fn core_r(&self, off: u32) -> u32; fn core_w(&self, off: u32, v: u32);
                    fn sms_r(&self, off: u32) -> u32;  fn sms_w(&self, off: u32, v: u32); }

/// A powered, identified V3D: what phase A (power) hands phase B (MMU/IRQ).
/// `ver` = TVER * 10 + REV from HUB_IDENT1 (71 on a Pi 5); `va_width` is the
/// GPU virtual-address width in bits from MMU_DEBUG_INFO (32 on a Pi 5).
pub struct V3d<R: V3dRegs> {
    pub regs: R,
    pub ver: u32,
    pub va_width: u32,
}

#[inline]
pub const fn bit(n: u32) -> u32 { 1 << n }
/// Extracts bits `[hi:lo]` of `v`.
#[inline]
pub const fn field(v: u32, hi: u32, lo: u32) -> u32 { (v >> lo) & ((1u32 << (hi - lo + 1)) - 1) }

// ── Physical layout (DT `v3d@2000000`, `brcm,2712-v3d`; `pm` @ 0x7d200000) ──
pub const HUB_PHYS: u64 = 0x10_0200_0000;
pub const HUB_SIZE: usize = 0x4000;
pub const CORE0_PHYS: u64 = 0x10_0200_8000;
pub const CORE0_SIZE: usize = 0x6000;
pub const SMS_PHYS: u64 = 0x10_0203_0800;
pub const SMS_SIZE: usize = 0x700;
/// hub interrupt: SPI 250 (DT `interrupts` index 0).
pub const HUB_INTID: u32 = 282;
/// core0 interrupt: SPI 249 (DT `interrupts` index 1).
pub const CORE_INTID: u32 = 281;

// ── Hub ──
pub const HUB_AXICFG: u32 = 0x0000;
pub const HUB_UIFCFG: u32 = 0x0004;
pub const HUB_IDENT0: u32 = 0x0008;
pub const HUB_IDENT1: u32 = 0x000c;
pub const HUB_IDENT1_WITH_MSO: u32 = bit(19);
pub const HUB_IDENT1_WITH_TSY: u32 = bit(18);
pub const HUB_IDENT1_WITH_TFU: u32 = bit(17);
pub const HUB_IDENT1_WITH_L3C: u32 = bit(16);
/// NHOSTS [15:12], NCORES [11:8], REV [7:4], TVER [3:0].
pub const fn hub_ident1_ncores(v: u32) -> u32 { field(v, 11, 8) }
pub const fn hub_ident1_rev(v: u32) -> u32 { field(v, 7, 4) }
pub const fn hub_ident1_tver(v: u32) -> u32 { field(v, 3, 0) }
pub const HUB_IDENT2: u32 = 0x0010;
pub const HUB_IDENT3: u32 = 0x0014;
/// IPREV [15:8] is the "rev" Mesa keys on (6 on a Pi 5).
pub const fn hub_ident3_iprev(v: u32) -> u32 { field(v, 15, 8) }

pub const HUB_INT_STS: u32 = 0x0050;
pub const HUB_INT_SET: u32 = 0x0054;
pub const HUB_INT_CLR: u32 = 0x0058;
pub const HUB_INT_MSK_STS: u32 = 0x005c;
pub const HUB_INT_MSK_SET: u32 = 0x0060;
pub const HUB_INT_MSK_CLR: u32 = 0x0064;
pub const HUB_INT_TFUC: u32 = bit(1);
pub const HUB_INT_MMU_CAP: u32 = bit(3);
pub const HUB_INT_MMU_PTI: u32 = bit(4);
pub const HUB_INT_MMU_WRV: u32 = bit(5);
pub const HUB_INT_GMPV: u32 = bit(6);

/// GMP moved to the hub on 7.1.
pub const GMP_STATUS: u32 = 0x0600;
pub const GMP_CFG: u32 = 0x0604;
pub const GMP_CFG_STOP_REQ: u32 = bit(1);

/// TFU, 7.1 offsets (4.2 had them at 0x400).
pub const TFU_CS: u32 = 0x0700;
pub const TFU_ICFG: u32 = 0x0708;
pub const TFU_ICFG_IOC: u32 = bit(0);
pub const TFU_IIA: u32 = 0x070c;
/// Linux v3d_regs.h (7.1): ICA (input chroma address) is 0x710, IIS (input image stride) 0x714.
pub const TFU_ICA: u32 = 0x0710;
pub const TFU_IIS: u32 = 0x0714;
pub const TFU_IUA: u32 = 0x0718;
/// 7.1 only.
pub const TFU_IOC: u32 = 0x071c;
pub const TFU_IOA: u32 = 0x0720;
pub const TFU_IOS: u32 = 0x0724;
pub const TFU_COEF0: u32 = 0x0728;
pub const TFU_COEF1: u32 = 0x072c;
pub const TFU_COEF2: u32 = 0x0730;
pub const TFU_COEF3: u32 = 0x0734;

pub const MMUC_CONTROL: u32 = 0x1000;
pub const MMUC_CONTROL_ENABLE: u32 = bit(0);
pub const MMUC_CONTROL_FLUSH: u32 = bit(1);
pub const MMUC_CONTROL_FLUSHING: u32 = bit(2);
pub const MMUC_CONTROL_CLEAR: u32 = bit(11);

pub const MMU_CTL: u32 = 0x1200;
pub const MMU_CTL_ENABLE: u32 = bit(0);
pub const MMU_CTL_TLB_STALL: u32 = bit(1);
pub const MMU_CTL_TLB_CLEAR: u32 = bit(2);
pub const MMU_CTL_TLB_STALLED: u32 = bit(6);
pub const MMU_CTL_TLB_CLEARING: u32 = bit(7);
pub const MMU_CTL_WRITE_VIOLATION_EXCEPTION: u32 = bit(9);
pub const MMU_CTL_WRITE_VIOLATION_INT: u32 = bit(10);
pub const MMU_CTL_WRITE_VIOLATION_ABORT: u32 = bit(11);
pub const MMU_CTL_WRITE_VIOLATION: u32 = bit(12);
pub const MMU_CTL_PT_INVALID_ENABLE: u32 = bit(16);
pub const MMU_CTL_PT_INVALID_EXCEPTION: u32 = bit(17);
pub const MMU_CTL_PT_INVALID_INT: u32 = bit(18);
pub const MMU_CTL_PT_INVALID_ABORT: u32 = bit(19);
pub const MMU_CTL_PT_INVALID: u32 = bit(20);
pub const MMU_CTL_CAP_EXCEEDED_EXCEPTION: u32 = bit(24);
pub const MMU_CTL_CAP_EXCEEDED_INT: u32 = bit(25);
pub const MMU_CTL_CAP_EXCEEDED_ABORT: u32 = bit(26);
pub const MMU_CTL_CAP_EXCEEDED: u32 = bit(27);
pub const MMU_PT_PA_BASE: u32 = 0x1204;
pub const MMU_VIO_ID: u32 = 0x122c;
pub const MMU_ILLEGAL_ADDR: u32 = 0x1230;
pub const MMU_ILLEGAL_ADDR_ENABLE: u32 = bit(31);
pub const MMU_VIO_ADDR: u32 = 0x1234;
pub const MMU_DEBUG_INFO: u32 = 0x1238;
/// PA width = 30 + [11:8], VA width = 30 + [7:4].
pub const fn mmu_pa_width(debug_info: u32) -> u32 { 30 + field(debug_info, 11, 8) }
pub const fn mmu_va_width(debug_info: u32) -> u32 { 30 + field(debug_info, 7, 4) }

// ── Core 0 ──
pub const CTL_IDENT0: u32 = 0x0000;
/// Low 24 bits of CTL_IDENT0 spell "V3D"; [31:24] is the technology version.
pub const CTL_IDENT0_MAGIC: u32 = 0x0044_3356;
pub const CTL_IDENT1: u32 = 0x0004;
/// QPUs = NSLC [7:4] * QUPS [11:8].
pub const fn ctl_ident1_qpus(v: u32) -> u32 { field(v, 7, 4) * field(v, 11, 8) }
pub const CTL_IDENT2: u32 = 0x0008;
pub const CTL_MISCCFG: u32 = 0x0018;
pub const CTL_L2CACTL: u32 = 0x0020;
pub const CTL_SLCACTL: u32 = 0x0024;
pub const CTL_L2TCACTL: u32 = 0x0030;
pub const L2TCACTL_L2TFLS: u32 = bit(0);
pub const L2TCACTL_FLM_SHIFT: u32 = 1;
pub const L2TCACTL_FLM_FLUSH: u32 = 0;
pub const L2TCACTL_FLM_CLEAR: u32 = 1;
pub const L2TCACTL_FLM_CLEAN: u32 = 2;
pub const L2TCACTL_TMUWCF: u32 = bit(8);
pub const CTL_L2TFLSTA: u32 = 0x0034;
pub const CTL_L2TFLEND: u32 = 0x0038;
pub const CTL_INT_STS: u32 = 0x0050;
pub const CTL_INT_SET: u32 = 0x0054;
pub const CTL_INT_CLR: u32 = 0x0058;
pub const CTL_INT_MSK_STS: u32 = 0x005c;
pub const CTL_INT_MSK_SET: u32 = 0x0060;
pub const CTL_INT_MSK_CLR: u32 = 0x0064;
pub const INT_FRDONE: u32 = bit(0);
pub const INT_FLDONE: u32 = bit(1);
pub const INT_OUTOMEM: u32 = bit(2);
/// 7.1 positions (4.2 had CSDDONE at bit 7).
pub const INT_PCTR_71: u32 = bit(5);
pub const INT_CSDDONE_71: u32 = bit(6);

/// Control-list executor (upstream offsets; phase C re-verifies before use).
pub const CLE_CT0CS: u32 = 0x0100;
pub const CLE_CT1CS: u32 = 0x0104;
pub const CLE_CT0EA: u32 = 0x0108;
pub const CLE_CT1EA: u32 = 0x010c;
pub const CLE_CT0CA: u32 = 0x0110;
pub const CLE_CT1CA: u32 = 0x0114;
pub const CLE_CT0RA: u32 = 0x0118;
pub const CLE_CT1RA: u32 = 0x011c;
pub const CLE_CT0QTS: u32 = 0x015c;
pub const CLE_CT0QTS_ENABLE: u32 = bit(1);
pub const CLE_CT0QBA: u32 = 0x0160;
pub const CLE_CT1QBA: u32 = 0x0164;
pub const CLE_CT0QEA: u32 = 0x0168;
pub const CLE_CT1QEA: u32 = 0x016c;
pub const CLE_CT0QMA: u32 = 0x0170;
pub const CLE_CT0QMS: u32 = 0x0174;
pub const CLE_CT1QCFG: u32 = 0x0178;
pub const PTB_BPCA: u32 = 0x0300;
pub const PTB_BPCS: u32 = 0x0304;
pub const PTB_BPOA: u32 = 0x0308;
pub const PTB_BPOS: u32 = 0x030c;
/// CSD, 7.1 offsets.
pub const CSD_STATUS: u32 = 0x0900;
pub const CSD_QUEUED_CFG0: u32 = 0x0930;
pub const CSD_QUEUED_CFG7: u32 = 0x094c;
/// `QUEUED_CFGn`, n = 0..=6 (CFG0 starts the dispatch).
pub const fn csd_queued_cfg(n: u32) -> u32 { CSD_QUEUED_CFG0 + 4 * n }
/// Batch counter the hang check watches (Linux `V3D_CSD_CURRENT_CFG4`, 7.1).
pub const CSD_CURRENT_CFG4: u32 = 0x0968;

// ── SMS (7.1 power/reset state machine). REE and TEE share the bit layout. ──
pub const SMS_REE_CS: u32 = 0x0000;
pub const SMS_TEE_CS: u32 = 0x0400;
pub const SMS_INTERRUPT: u32 = bit(31);
pub const SMS_POWER_OFF: u32 = bit(30);
pub const SMS_CLEAR_POWER_OFF: u32 = bit(29);
pub const SMS_LOCK: u32 = bit(28);
pub const SMS_CLEAR_LOCK: u32 = bit(27);
/// STATE is bits [3:0] (Linux `V3D_SMS_STATE_MASK`); [5:4] is OLD_MODE, so a
/// 5-bit decode turned an idle `0x50` into a bogus state 0x10 (Pi boot 2026-10-26).
pub const fn sms_state(v: u32) -> u32 { field(v, 3, 0) }
pub const SMS_STATE_IDLE: u32 = 0x0;
/// Writing this state to SMS_REE_CS requests a reset.
pub const SMS_STATE_RESET_REQ: u32 = 0x4;
pub const SMS_STATE_ISOLATING_FOR_RESET: u32 = 0xa;
pub const SMS_STATE_RESETTING: u32 = 0xb;
pub const SMS_STATE_ISOLATING_FOR_POWER_OFF: u32 = 0xc;
pub const SMS_STATE_POWER_OFF: u32 = 0xd;

// ── BCM2712 power manager (`brcm,bcm2712-pm`) ──
pub const PM_PHYS: u64 = 0x10_7D20_0000;
pub const PM_SIZE: usize = 0x308;
/// Every PM write carries this password in [31:24].
pub const PM_PASSWORD: u32 = 0x5a00_0000;
/// GRAFX power island (Linux `PM_GRAFX`): the parent domain of GRAFX_V3D.
/// Linux genpd powers it before GRAFX_V3D (bcm2835-power `domain_deps`).
pub const PM_GRAFX: u32 = 0x10c;
pub const PM_POWUP: u32 = bit(0);
pub const PM_POWOK: u32 = bit(1);
pub const PM_ISPOW: u32 = bit(2);
pub const PM_MEMREP: u32 = bit(3);
pub const PM_MRDONE: u32 = bit(4);
pub const PM_ISFUNC: u32 = bit(5);
pub const PM_INRUSH_SHIFT: u32 = 13;
pub const PM_INRUSH_MASK: u32 = 3 << PM_INRUSH_SHIFT;
/// 3.5 mA .. 20 mA, ramped up until POWOK.
pub const PM_INRUSH_MAX: u32 = 3;
/// Island fully on: power OK, electrical and functional isolation off.
/// BCM2712 firmware leaves GRAFX at 0x706d (POWUP|ISPOW|ISFUNC, POWOK and MRDONE clear) and the
/// V3D behind it answers; POWOK is therefore not required (pigpu, measured on the Pi 5).
pub const PM_GRAFX_ON: u32 = PM_POWUP | PM_ISPOW | PM_ISFUNC;
/// BCM2712 GRAFX_V3D domain control (no ASB on the 2712).
pub const PM_GRAFX_2712: u32 = 0x304;
/// Reset deassert (1 = out of reset).
pub const PM_V3DRSTN: u32 = bit(6);

// ── Firmware clock (mailbox property interface) ──
pub const FW_CLK_V3D: u32 = 5;
pub const TAG_GET_MAX_CLOCK_RATE: u32 = 0x0003_0004;
/// RPI_FIRMWARE_SET_DOMAIN_STATE; domain 7 = V3D (dt-bindings raspberrypi-power.h: I2C0..H264=0..6, V3D=7).
pub const TAG_SET_DOMAIN_STATE: u32 = 0x0003_8030;
pub const FW_DOMAIN_V3D: u32 = 7;
pub const TAG_GET_DOMAIN_STATE: u32 = 0x0003_0030;
pub const TAG_SET_CLOCK_STATE: u32 = 0x0003_8001;
pub const TAG_SET_CLOCK_RATE: u32 = 0x0003_8002;
