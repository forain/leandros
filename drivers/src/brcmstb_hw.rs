//! Register layout and address arithmetic of the Broadcom STB PCIe root
//! complex as found on the BCM2712 (Raspberry Pi 5), plus the RP1 / MIP
//! constants that sit on top of it. Pure functions only: no MMIO, so the
//! window, inbound-BAR, config-address and BAR-assignment math is covered by
//! host unit tests (`drivers/brcmstb-tests`). The hardware sequence that uses
//! it lives in `brcmstb_pcie.rs` and `rp1.rs` (feature `rpi5`).
//!
//! Offsets follow Linux `drivers/pci/controller/pcie-brcmstb.c` (the
//! BCM2712 / BCM7712 register map: HARD_DEBUG at 0x4304, the 4 KiB
//! EXT_CFG_DATA window at 0x8000 selected by EXT_CFG_INDEX at 0x9000).

// ── Root-complex config space (offset 0 of the register block) ──────────────
/// PCI Express capability of the root port inside its own config space.
pub const RC_CAP_REGS: usize = 0x00ac;
pub const RC_LNKCAP: usize = RC_CAP_REGS + 0x0c;
pub const RC_LNKSTA: usize = RC_CAP_REGS + 0x12;
pub const RC_RTCTL: usize = RC_CAP_REGS + 0x1c;
pub const RC_LNKCTL2: usize = RC_CAP_REGS + 0x30;
pub const RTCTL_CRSSVE: u32 = 1 << 4;
pub const RC_VENDOR_SPECIFIC_REG1: usize = 0x0188;
pub const VENDOR_REG1_ENDIAN_MODE_BAR2_MASK: u32 = 0xc;
pub const RC_PRIV1_ID_VAL3: usize = 0x043c;
pub const PRIV1_ID_VAL3_CLASS_MASK: u32 = 0x00ff_ffff;
pub const RC_PRIV1_LINK_CAPABILITY: usize = 0x04dc;
pub const PRIV1_LINKCAP_WIDTH_MASK: u32 = 0x3f0;
pub const PRIV1_LINKCAP_ASPM_MASK: u32 = 0xc00;
pub const RC_DL_MDIO_ADDR: usize = 0x1100;
pub const RC_DL_MDIO_WR_DATA: usize = 0x1104;
pub const RC_PL_PHY_CTL_15: usize = 0x184c;
pub const PHY_CTL_15_PM_CLK_PERIOD_MASK: u32 = 0xff;

// ── MISC block ──────────────────────────────────────────────────────────────
pub const MISC_CTRL: usize = 0x4008;
pub const MISC_CTRL_RCB_64B_MODE: u32 = 0x80;
pub const MISC_CTRL_RCB_MPS_MODE: u32 = 0x400;
pub const MISC_CTRL_SCB_ACCESS_EN: u32 = 0x1000;
pub const MISC_CTRL_CFG_READ_UR_MODE: u32 = 0x2000;
pub const MISC_CTRL_MAX_BURST_MASK: u32 = 0x0030_0000;
pub const MISC_CTRL_MAX_BURST_SHIFT: u32 = 20;
pub const MISC_CPU2PCIE_WIN0_LO: usize = 0x400c;
pub const MISC_RC_BAR1_CONFIG_LO: usize = 0x402c;
pub const MISC_RC_CONFIG_RETRY_TIMEOUT: usize = 0x405c;
pub const MISC_PCIE_CTRL: usize = 0x4064;
pub const PCIE_CTRL_PERSTB: u32 = 0x4;
pub const MISC_PCIE_STATUS: usize = 0x4068;
pub const STATUS_PHYLINKUP: u32 = 0x10;
pub const STATUS_DL_ACTIVE: u32 = 0x20;
pub const MISC_REVISION: usize = 0x406c;
pub const MISC_CPU2PCIE_WIN0_BASE_LIMIT: usize = 0x4070;
pub const MISC_CPU2PCIE_WIN0_BASE_HI: usize = 0x4080;
pub const MISC_CPU2PCIE_WIN0_LIMIT_HI: usize = 0x4084;
pub const MISC_UBUS_CTRL: usize = 0x40a4;
pub const UBUS_CTRL_REPLY_ERR_DIS: u32 = 1 << 13;
pub const UBUS_CTRL_REPLY_DECERR_DIS: u32 = 1 << 19;
pub const MISC_UBUS_TIMEOUT: usize = 0x40a8;
pub const MISC_UBUS_BAR1_CONFIG_REMAP: usize = 0x40ac;
pub const UBUS_REMAP_ACCESS_EN: u32 = 0x1;
pub const MISC_AXI_READ_ERROR_DATA: usize = 0x4170;
pub const HARD_DEBUG: usize = 0x4304;
pub const HARD_DEBUG_CLKREQ_DEBUG_ENABLE: u32 = 0x2;
pub const HARD_DEBUG_L1SS_ENABLE: u32 = 0x0020_0000;
pub const HARD_DEBUG_SERDES_IDDQ: u32 = 0x0800_0000;

// ── Config-space access to devices below the root port ──────────────────────
pub const EXT_CFG_DATA: usize = 0x8000;
pub const EXT_CFG_INDEX: usize = 0x9000;
/// Size of the register block (the DTB `reg` is 0x9310).
pub const REG_SIZE: usize = 0xa000;

/// Value written to EXT_CFG_INDEX to select `bus:dev.func` (ECAM layout).
pub const fn cfg_index(bus: u8, dev: u8, func: u8) -> u32 {
    ((bus as u32) << 20) | (((dev as u32) & 0x1f) << 15) | (((func as u32) & 7) << 12)
}

/// Where a config access to `bus:dev.func` + `off` lands inside the register
/// block, or None if the function cannot exist: the root port is the only
/// function on bus 0, and a PCIe link carries only device 0 below it.
pub const fn cfg_offset(bus: u8, dev: u8, func: u8, off: u16) -> Option<usize> {
    if off >= 0x1000 { return None; }
    if bus == 0 {
        if dev != 0 || func != 0 { return None; }
        return Some(off as usize);
    }
    if dev != 0 { return None; }
    Some(EXT_CFG_DATA + off as usize)
}

// ── MDIO (SerDes) ───────────────────────────────────────────────────────────
pub const MDIO_PORT0: u32 = 0;
pub const MDIO_CMD_WRITE: u32 = 0;
pub const MDIO_SET_ADDR_OFFSET: u32 = 0x1f;
pub const MDIO_DATA_DONE: u32 = 0x8000_0000;

pub const fn mdio_pkt(port: u32, regad: u32, cmd: u32) -> u32 {
    ((cmd & 0xfff) << 20) | ((port & 0xf) << 16) | (regad & 0xffff)
}

/// BCM2712 PLL setup for the 54 MHz crystal reference clock (Linux
/// `brcm_pcie_post_setup_bcm2712`): MDIO block 0x1600, then these registers.
pub const PLL_54MHZ_BLOCK: u32 = 0x1600;
pub const PLL_54MHZ_REGS: [(u32, u32); 7] = [
    (0x16, 0x50b9), (0x17, 0xbda1), (0x18, 0x0094), (0x19, 0x97b4),
    (0x1b, 0x5030), (0x1c, 0x5030), (0x1e, 0x0007),
];

// ── Outbound (CPU → PCI) windows ────────────────────────────────────────────

/// Register writes `(offset, value)` that program outbound window `win`
/// (CPU `cpu`..+`size` → PCI `pci`). `base_limit` is the current
/// BASE_LIMIT register value (only its 12-bit MB fields are replaced).
pub fn outbound_win_regs(win: usize, cpu: u64, pci: u64, size: u64, base_limit: u32) -> [(usize, u32); 5] {
    let cpu_mb = cpu >> 20;
    let limit_mb = (cpu + size - 1) >> 20;
    let bl = (base_limit & !0xfff0_fff0)
        | (((cpu_mb & 0xfff) as u32) << 4)
        | (((limit_mb & 0xfff) as u32) << 20);
    [
        (MISC_CPU2PCIE_WIN0_LO + win * 8, pci as u32),
        (MISC_CPU2PCIE_WIN0_LO + 4 + win * 8, (pci >> 32) as u32),
        (MISC_CPU2PCIE_WIN0_BASE_LIMIT + win * 4, bl),
        (MISC_CPU2PCIE_WIN0_BASE_HI + win * 8, ((cpu_mb >> 12) & 0xff) as u32),
        (MISC_CPU2PCIE_WIN0_LIMIT_HI + win * 8, ((limit_mb >> 12) & 0xff) as u32),
    ]
}

// ── Inbound (PCI → CPU) windows: RC_BARn + UBUS remap ───────────────────────

/// RC_BAR size encoding (Linux `brcm_pcie_encode_ibar_size`); 0 = disabled.
pub fn encode_ibar_size(size: u64) -> u32 {
    if size == 0 || !size.is_power_of_two() { return 0; }
    let l = size.trailing_zeros();
    match l {
        12..=15 => (l - 12) + 0x1c,
        16..=36 => l - 15,
        _ => 0,
    }
}

/// RC_BAR`n`_CONFIG_LO offset, n = 1..=3.
pub const fn rc_bar_lo(n: usize) -> usize { MISC_RC_BAR1_CONFIG_LO + (n - 1) * 8 }
/// UBUS_BAR`n`_CONFIG_REMAP offset, n = 1..=3.
pub const fn ubus_remap_lo(n: usize) -> usize { MISC_UBUS_BAR1_CONFIG_REMAP + (n - 1) * 8 }

/// Writes that make PCI `pci`..+`size` reach CPU `cpu` through RC BAR `n`.
/// `size` must be a power of two and `pci` aligned to it.
pub fn inbound_win_regs(n: usize, pci: u64, cpu: u64, size: u64) -> [(usize, u32); 4] {
    let enc = encode_ibar_size(size);
    [
        (rc_bar_lo(n), (pci as u32 & !0x1f) | enc),
        (rc_bar_lo(n) + 4, (pci >> 32) as u32),
        (ubus_remap_lo(n), (cpu as u32 & !0xfff) | if enc != 0 { UBUS_REMAP_ACCESS_EN } else { 0 }),
        (ubus_remap_lo(n) + 4, (cpu >> 32) as u32),
    ]
}

// ── Pi 5 pcie2 (RP1) layout, from the firmware DTB ──────────────────────────
pub const PCIE2_REGS: u64 = 0x10_0012_0000;
/// `pcie_rescal` (brcm,bcm7216-pcie-sata-rescal) and `bcm_reset`
/// (brcm,brcmstb-reset, line 44 = pcie2 bridge).
pub const RESCAL_REGS: u64 = 0x10_0011_9500;
pub const RESCAL_START: usize = 0x0;
pub const RESCAL_STATUS: usize = 0x8;
pub const SW_INIT_REGS: u64 = 0x10_0150_4318;
pub const PCIE2_BRIDGE_RESET_ID: u32 = 44;
/// (offset of SW_INIT_SET for that bank, bit) of a brcmstb-reset line.
pub const fn sw_init_set(id: u32) -> (usize, u32) { ((id as usize >> 5) * 0x18, 1 << (id & 31)) }
pub const SW_INIT_CLEAR: usize = 0x4;
pub const SW_INIT_STATUS: usize = 0x8;

/// Outbound window 0: 32-bit non-prefetchable memory, CPU 0x1f_0000_0000 → PCI 0.
pub const OB_MEM_CPU: u64 = 0x1f_0000_0000;
pub const OB_MEM_PCI: u64 = 0;
pub const OB_MEM_SIZE: u64 = 0xffff_fffc;
/// Outbound window 1: 64-bit prefetchable, CPU 0x1c_0000_0000 → PCI 0x4_0000_0000.
pub const OB_PREF_CPU: u64 = 0x1c_0000_0000;
pub const OB_PREF_PCI: u64 = 0x4_0000_0000;
pub const OB_PREF_SIZE: u64 = 0xc000_0000;
/// Inbound: system RAM (CPU 0) seen at PCI 0x10_0000_0000, 64 GiB.
pub const IB_RAM_PCI: u64 = 0x10_0000_0000;
pub const IB_RAM_CPU: u64 = 0;
pub const IB_RAM_SIZE: u64 = 0x10_0000_0000;
/// Inbound: the MIP0 MSI doorbell. Devices write PCI 0xff_ffff_f000, which
/// lands on MIP0's INT_RAISE register (CPU 0x10_0013_0000).
pub const MSI_PCI_ADDR: u64 = 0xff_ffff_f000;
pub const MIP0_REGS: u64 = 0x10_0013_0000;
pub const MIP_SIZE: u64 = 0x1000;

/// DMA address a device below pcie2 must use for CPU physical address `pa`.
pub const fn dma_addr(pa: u64) -> u64 { pa + IB_RAM_PCI }

// ── Link status ─────────────────────────────────────────────────────────────
pub const fn link_up(status: u32) -> bool {
    status & (STATUS_PHYLINKUP | STATUS_DL_ACTIVE) == (STATUS_PHYLINKUP | STATUS_DL_ACTIVE)
}
/// (generation, width) from the root port's Link Status register.
pub const fn lnksta_decode(lnksta: u16) -> (u8, u8) {
    ((lnksta & 0xf) as u8, ((lnksta >> 4) & 0x3f) as u8)
}

// ── Bridge windows and BAR assignment ───────────────────────────────────────

/// Type-1 header memory base/limit dword (offset 0x20) for `[base, end)`,
/// 1 MiB granular. An empty range yields a closed window.
pub fn bridge_mem_window(base: u64, end: u64) -> u32 {
    if end <= base { return 0x0000_fff0; }
    let lim = end - 1;
    ((((lim >> 16) as u32) & 0xfff0) << 16) | (((base >> 16) as u32) & 0xfff0)
}

/// Memory BARs to place: (index, size, 64-bit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarReq { pub index: usize, pub size: u64, pub is64: bool }

/// Natural-alignment placement, largest first, from `start` within
/// `[start, end)`. Returns (index, PCI address) per placed BAR plus the end
/// of the used space; BARs that do not fit are left out.
pub fn assign_bars(reqs: &[BarReq], start: u64, end: u64) -> (alloc::vec::Vec<(usize, u64)>, u64) {
    let mut order: alloc::vec::Vec<BarReq> = reqs.iter().copied().filter(|r| r.size.is_power_of_two()).collect();
    order.sort_by(|a, b| b.size.cmp(&a.size).then(a.index.cmp(&b.index)));
    let mut next = start;
    let mut out = alloc::vec::Vec::new();
    for r in order {
        let at = (next + r.size - 1) & !(r.size - 1);
        if at.checked_add(r.size).map_or(true, |e| e > end) { continue; }
        if !r.is64 && at + r.size > 1 << 32 { continue; }
        out.push((r.index, at));
        next = at + r.size;
    }
    out.sort_by_key(|&(i, _)| i);
    (out, next)
}

/// Size from a BAR sizing read-back (`lo`, and `hi` for 64-bit BARs).
pub fn bar_size(lo: u32, hi: Option<u32>) -> u64 {
    let mask = ((hi.unwrap_or(0xffff_ffff) as u64) << 32) | (lo & 0xffff_fff0) as u64;
    if lo & 0xffff_fff0 == 0 && hi.map_or(true, |h| h == 0) { return 0; }
    (!mask).wrapping_add(1)
}

// ── MIP (BCM2712 MSI-X → GIC) ───────────────────────────────────────────────
pub const MIP_INT_RAISE: usize = 0x00;
pub const MIP_INT_CLEAR: usize = 0x10;
pub const MIP_INT_CFGL_HOST: usize = 0x20;
pub const MIP_INT_CFGH_HOST: usize = 0x30;
pub const MIP_INT_MASKL_HOST: usize = 0x40;
pub const MIP_INT_MASKH_HOST: usize = 0x50;
pub const MIP_INT_MASKL_VPU: usize = 0x60;
pub const MIP_INT_MASKH_VPU: usize = 0x70;
pub const MIP_INT_STATUSL_HOST: usize = 0x80;
/// MIP0 `msi-ranges = <&gic GIC_SPI 128 IRQ_TYPE_EDGE_RISING 64>`.
pub const MIP0_SPI_BASE: u32 = 128;
pub const MIP0_VECTORS: u32 = 64;

/// GIC INTID of MIP0 vector `v` (edge).
pub const fn mip_intid(v: u32) -> u32 { 32 + MIP0_SPI_BASE + v }
/// MSI message (address, data) that raises MIP0 vector `v`.
pub const fn mip_msg(v: u32) -> (u64, u32) { (MSI_PCI_ADDR, v) }

// ── RP1 ─────────────────────────────────────────────────────────────────────
pub const RP1_VENDOR: u16 = 0x1de4;
pub const RP1_DEVICE: u16 = 0x0001;
/// SYSINFO block at BAR1 offset 0.
pub const RP1_SYSINFO_CHIP_ID: usize = 0x0;
pub const RP1_SYSINFO_PLATFORM: usize = 0x4;
pub const RP1_C0_CHIP_ID: u32 = 0x2000_1927;
/// RP1 interrupt sources (MSI-X vectors 0..61).
pub const RP1_INT_END: u32 = 61;
pub const RP1_PCIE_APBS: usize = 0x10_8000;
pub const RP1_REG_SET: usize = 0x800;
pub const RP1_REG_CLR: usize = 0xc00;
pub const RP1_MSIX_CFG_ENABLE: u32 = 1 << 0;
pub const RP1_MSIX_CFG_IACK: u32 = 1 << 2;
pub const RP1_MSIX_CFG_IACK_EN: u32 = 1 << 3;
/// BAR1 offset of RP1's per-vector MSIX_CFG register (plain, SET or CLR alias).
pub const fn rp1_msix_cfg(n: u32, alias: usize) -> usize { RP1_PCIE_APBS + alias + 0x8 + 4 * n as usize }
/// Well-known RP1 sources (Linux `rp1.h`).
pub const RP1_INT_ETH: u32 = 6;
pub const RP1_INT_USBHOST0: u32 = 30;
pub const RP1_INT_USBHOST1: u32 = 35;

// ── RP1 GPIO (SYS_RIO software control), Linux `pinctrl-rp1.c` ──────────────
/// FUNCSEL value that hands a pin to the RIO (software GPIO) block.
pub const RP1_FSEL_SYS_RIO: u32 = 5;
pub const RP1_PAD_OD: u32 = 1 << 7;
pub const RP1_PAD_IE: u32 = 1 << 6;
/// BAR1 offsets for RP1 GPIO `g`: (CTRL, RIO bank base, PAD, bit). Banks
/// hold 28, 6 and 20 pins; each bank is 0x4000 apart in IO (0xd0000), RIO
/// (0xe0000; +0x2000 SET, +0x3000 CLR alias) and PADS (0xf0000, +4 skip).
pub const fn rp1_gpio_regs(g: u32) -> Option<(usize, usize, usize, u32)> {
    let (bank, pin) = if g < 28 { (0, g) } else if g < 34 { (1, g - 28) } else if g < 54 { (2, g - 34) } else { return None };
    Some((0xd_0000 + bank * 0x4000 + 8 * pin as usize + 4,
          0xe_0000 + bank * 0x4000,
          0xf_0000 + bank * 0x4000 + 4 + 4 * pin as usize,
          1 << pin))
}
pub const RIO_OUT: usize = 0x0;
pub const RIO_OE: usize = 0x4;
pub const RIO_SET: usize = 0x2000;
pub const RIO_CLR: usize = 0x3000;
/// GEM PHY reset: `phy-reset-gpios = <&rp1_gpio 32 GPIO_ACTIVE_LOW>`, 5 ms.
pub const RP1_ETH_PHY_RESET_GPIO: u32 = 32;

// ── Bring-up stages (lane pipcie2) ──────────────────────────────────────────
// `pcie.stop=N` runs stages 1..=N and stops before N+1 (inert: no further MMIO).
// Each stage prints `[pcie] stage N name` straight to the UART *before* its
// first access and stores N in the wedge `pcie` slot, so a stalled AXI read
// names itself in the `[WDOG]` line seen from another CPU.
pub const STAGES: [&str; 17] = [
    "idle",          // 0
    "map",           // 1  ioremap only, no access
    "swinit-read",   // 2  read the brcmstb-reset SW_INIT status (reset block, not the RC)
    "rescal",        // 3  rescal START/STATUS (SATA/PCIe rescal block)
    "rc-read",       // 4  first RC register reads: REVISION, PCIE_STATUS, PCIE_CTRL
    "bridge-assert", // 5  bridge SW_INIT assert + status read (reset block only, NO RC access)
    "bridge-release",// 6  bridge SW_INIT clear, poll status released, then one RC REVISION read
    "perst-serdes",  // 7  first RC writes: PERST# assert (if released), HARD_DEBUG SerDes IDDQ off
    "pll",           // 8  MDIO 54 MHz PLL, PHY_CTL_15
    "misc",          // 9  MISC_CTRL, UBUS error suppression, timeouts
    "windows",       // 10 inbound + outbound windows
    "link-params",   // 11 class code, x4/ASPM, gen2, PERST# release
    "link-poll",     // 12 bounded link-up poll (MISC_PCIE_STATUS only)
    "bus-numbers",   // 13 RC config (bus 0) primary/secondary/subordinate
    "rp1-id",        // 14 first config TLP: bus 1 dev 0 vendor/device
    "rp1-bars",      // 15 RP1 BAR sizing/placement + bridge windows + enable
    "rp1-probe",     // 16 RP1 BAR1 MMIO (SYSINFO, MIP0, GEM, USB)
];
pub const LAST_STAGE: u32 = STAGES.len() as u32 - 1;

pub fn stage_name(n: u32) -> &'static str {
    STAGES.get(n as usize).copied().unwrap_or("?")
}

/// `pcie.stop=N` from the command line; absent or malformed = run everything.
pub fn parse_stop(cmdline: &str) -> u32 {
    for t in cmdline.split_ascii_whitespace() {
        if let Some(v) = t.strip_prefix("pcie.stop=") {
            return v.parse::<u32>().unwrap_or(LAST_STAGE);
        }
    }
    LAST_STAGE
}

/// Reuse a link the firmware already trained (skip resets, PLL, PERST#) only
/// when asked (`pcie.reuse=1`) and the RC reports the link up.
pub fn reuse_link(cmdline: &str, pcie_status: u32) -> bool {
    cmdline.split_ascii_whitespace().any(|t| t == "pcie.reuse=1") && link_up(pcie_status)
}

// ── Bridge reset + first RC writes (stages 5–7), host-tested ───────────────
// On BCM2712 the pcie2 "bridge" reset is bit 44 of the brcmstb-reset
// controller (DT `resets = <&pcie_rescal>, <&bcm_reset 44>`), and while it is
// asserted the RC register block does not answer: an AXI read of e.g.
// MISC_PCIE_CTRL never completes and the CPU stalls with no SError (Pi 5,
// 2026-10-27: cpu0 hung in stage 5 after `bridge_reset(assert)` + `perst`).
// Linux `brcm_pcie_setup()` therefore touches nothing between
// `bridge_sw_init_set(1)` and `bridge_sw_init_set(0)` except a 100–200 us
// sleep (PERST# is asserted there only for BCM2711, whose PERST# lives in the
// RGR1 block), and BCM2712 sets CFG_QUIRK_AVOID_BRIDGE_SHUTDOWN so
// `brcm_pcie_turn_off()` never re-asserts it. The sequence below mirrors that
// and refuses to touch the RC unless the reset controller reports the bridge
// released and the RC answers a REVISION read with a plausible value.

/// Register blocks the bring-up touches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blk { Rc, SwInit }

/// MMIO + delay seam so the sequencing runs against a recording mock on the host.
pub trait PcieBus {
    fn rd(&mut self, blk: Blk, off: usize) -> u32;
    fn wr(&mut self, blk: Blk, off: usize, v: u32);
    fn udelay(&mut self, us: u32);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeErr {
    /// SW_INIT status still shows the bridge in reset after CLEAR (RC untouched).
    StuckInReset,
    /// RC REVISION read back 0 or all-ones after release (no further RC access).
    RcDead(u32),
}

/// Stage 5: assert the bridge reset. Reset-controller accesses only; returns
/// the SW_INIT status read back (bit set = held in reset).
pub fn bridge_assert<B: PcieBus>(b: &mut B) -> u32 {
    let (bank, bit) = sw_init_set(PCIE2_BRIDGE_RESET_ID);
    b.wr(Blk::SwInit, bank, bit);
    let st = b.rd(Blk::SwInit, bank + SW_INIT_STATUS);
    b.udelay(150); // Linux: usleep_range(100, 200)
    st
}

/// Stage 6: release the bridge, wait (<= 1 ms) for SW_INIT status to confirm,
/// then prove the RC answers with one read of MISC_REVISION.
pub fn bridge_release<B: PcieBus>(b: &mut B) -> Result<u32, BridgeErr> {
    let (bank, bit) = sw_init_set(PCIE2_BRIDGE_RESET_ID);
    b.wr(Blk::SwInit, bank + SW_INIT_CLEAR, bit);
    let mut released = false;
    for _ in 0..100 {
        if b.rd(Blk::SwInit, bank + SW_INIT_STATUS) & bit == 0 { released = true; break; }
        b.udelay(10);
    }
    if !released { return Err(BridgeErr::StuckInReset); }
    b.udelay(100);
    let rev = b.rd(Blk::Rc, MISC_REVISION);
    if rev == 0 || rev == 0xffff_ffff { return Err(BridgeErr::RcDead(rev)); }
    Ok(rev)
}

/// Stage 7: the first RC writes. PERST# asserted (BCM7278-style PERSTB in
/// MISC_PCIE_CTRL, written only if the firmware left it released), then the
/// SerDes out of IDDQ (Linux: right after the bridge leaves reset).
pub fn perst_serdes<B: PcieBus>(b: &mut B) {
    let ctrl = b.rd(Blk::Rc, MISC_PCIE_CTRL);
    if ctrl & PCIE_CTRL_PERSTB != 0 { b.wr(Blk::Rc, MISC_PCIE_CTRL, ctrl & !PCIE_CTRL_PERSTB); }
    let hd = b.rd(Blk::Rc, HARD_DEBUG);
    b.wr(Blk::Rc, HARD_DEBUG, hd & !HARD_DEBUG_SERDES_IDDQ);
    b.udelay(150);
}

// ── RP1 BAR placement + root-port window (stage 15) ─────────────────────────
// Every config write here is a full dword (sub-word writes into the
// EXT_CFG_DATA window are avoided) and every placement is read back: the
// root-port memory window is computed from where the BARs really are, never
// from what was asked for. On the first Pi 5 boot the window was derived from
// the assignment's `end`; nothing got placed, `end` stayed 0, the window was
// written closed and every BAR access came back all-ones.

/// Dword config access to `bus` 0 (root port) or 1 (RP1), device 0.
pub trait CfgSpace {
    fn rd(&mut self, bus: u8, off: u16) -> u32;
    fn wr(&mut self, bus: u8, off: u16, v: u32);
}

/// RP1 BAR sizes (BAR0 MSI-X/config 16 KiB, BAR1 peripherals 4 MiB, BAR2
/// shared SRAM 64 KiB). Used only when a sizing read-back is not a valid mask.
pub const RP1_BAR_SIZES: [u64; 3] = [0x4000, 0x40_0000, 0x1_0000];
pub const CMD_MEM: u32 = 1 << 1;
pub const CMD_MASTER: u32 = 1 << 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rp1Bar {
    /// Original value, sizing read-back, size used, PCI address read back.
    pub orig: u32, pub mask: u32, pub size: u64, pub at: u64,
    /// The sizing read-back was not a valid mask (size taken from RP1_BAR_SIZES).
    pub fallback: bool,
    /// The placement write did not read back.
    pub moved: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rp1Layout {
    pub bars: [Rp1Bar; 3],
    /// Root-port window [base, limit] as PCI addresses, None = closed.
    pub win: Option<(u64, u64)>,
    /// Root-port command and window dwords read back.
    pub rc_cmd: u32, pub rc_win: u32,
    pub rp1_cmd: u32,
}

impl Rp1Layout {
    /// PCI address and size of BAR `i`, if it is mapped inside the window.
    pub fn bar(&self, i: usize) -> Option<(u64, u64)> {
        let b = self.bars.get(i)?;
        let (lo, hi) = self.win?;
        (b.size != 0 && b.at >= lo && b.at + b.size - 1 <= hi).then_some((b.at, b.size))
    }
}

fn valid_mask_size(mask: u32, orig: u32) -> u64 {
    if mask == orig || mask == 0xffff_ffff { return 0; }
    let s = bar_size(mask, None);
    if s.is_power_of_two() && s >= 16 && s <= 0x1000_0000 { s } else { 0 }
}

/// Stage 15: size RP1's three 32-bit memory BARs, place them at the DTB
/// layout (BAR1 at PCI 0, BAR2 after it, BAR0 after that), read every BAR
/// back, then open the root port's memory window over the BARs as they really
/// are, enable root-port memory + bus master and RP1 memory decode.
pub fn setup_rp1_bars<C: CfgSpace>(c: &mut C) -> Rp1Layout {
    let mut l = Rp1Layout::default();
    let cmd = c.rd(1, 0x04) & 0xffff;
    c.wr(1, 0x04, cmd & !(CMD_MEM | CMD_MASTER | 1));
    let mut reqs = alloc::vec::Vec::new();
    for i in 0..3 {
        let off = 0x10 + 4 * i as u16;
        let orig = c.rd(1, off);
        c.wr(1, off, 0xffff_ffff);
        let mask = c.rd(1, off);
        c.wr(1, off, orig);
        let b = &mut l.bars[i];
        b.orig = orig;
        b.mask = mask;
        if orig & 1 != 0 { continue; } // I/O: RP1 has none
        b.size = valid_mask_size(mask, orig);
        if b.size == 0 { b.size = RP1_BAR_SIZES[i]; b.fallback = true; }
        reqs.push(BarReq { index: i, size: b.size, is64: false });
    }
    let (placed, _) = assign_bars(&reqs, OB_MEM_PCI, OB_MEM_PCI + OB_MEM_SIZE);
    for (i, at) in placed {
        let off = 0x10 + 4 * i as u16;
        c.wr(1, off, at as u32 | (l.bars[i].orig & 0xf));
        let rb = c.rd(1, off);
        if rb & !0xf != at as u32 { l.bars[i].moved = true; }
    }
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    for i in 0..3 {
        let off = 0x10 + 4 * i as u16;
        let b = &mut l.bars[i];
        if b.orig & 1 != 0 || b.size == 0 { continue; }
        b.at = (c.rd(1, off) & !0xf) as u64;
        lo = lo.min(b.at);
        hi = hi.max(b.at + b.size - 1);
    }
    if lo != u64::MAX {
        let end = (hi | 0xf_ffff) + 1;
        if end <= 1 << 32 { l.win = Some((lo & !0xf_ffff, end - 1)); }
    }
    let w = match l.win { Some((b, lim)) => bridge_mem_window(b, lim + 1), None => 0x0000_fff0 };
    c.wr(0, 0x20, w);
    c.wr(0, 0x24, 0x0000_fff0); // prefetchable window closed
    c.wr(0, 0x28, 0);
    c.wr(0, 0x2c, 0);
    c.wr(0, 0x1c, 0x0000_00f0); // I/O window closed, secondary status untouched
    let rc = c.rd(0, 0x04) & 0xffff;
    c.wr(0, 0x04, rc | CMD_MEM | CMD_MASTER);
    c.wr(1, 0x04, cmd | CMD_MEM);
    l.rc_cmd = c.rd(0, 0x04) & 0xffff;
    l.rc_win = c.rd(0, 0x20);
    l.rp1_cmd = c.rd(1, 0x04) & 0xffff;
    l
}
