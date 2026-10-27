//! Synopsys DesignWare USB3 (DWC3) global-register math for host mode, as the
//! Raspberry Pi 5's RP1 uses it (two controllers, `snps,dwc3`, `dr_mode = "host"`).
//! Pure functions only; host-tested in drivers/dwc3-tests. Register layout and
//! quirk bits follow Linux `drivers/usb/dwc3/core.{c,h}` (rpi-6.12.y).

/// xHCI register space starts at the DWC3 base; the DWC3 globals start here.
pub const GLOBALS_START: usize = 0xc100;

pub const GSBUSCFG1: usize = 0xc104;
pub const GTXTHRCFG: usize = 0xc108;
pub const GCTL: usize = 0xc110;
pub const GSTS: usize = 0xc118;
pub const GUCTL1: usize = 0xc11c;
pub const GSNPSID: usize = 0xc120;
pub const GUCTL: usize = 0xc12c;
pub const GUSB2PHYCFG0: usize = 0xc200;
pub const GUSB3PIPECTL0: usize = 0xc2c0;
pub const GFLADJ: usize = 0xc630;

pub const GCTL_CORESOFTRESET: u32 = 1 << 11;
pub const GCTL_PRTCAPDIR_MASK: u32 = 3 << 12;
pub const GCTL_PRTCAP_HOST: u32 = 1 << 12;
pub const GSTS_CURMOD_MASK: u32 = 3;
pub const GSTS_CURMOD_HOST: u32 = 1;
pub const GUCTL1_NAK_PER_ENH_FS: u32 = 1 << 19;
pub const GUCTL1_PARKMODE_DISABLE_SS: u32 = 1 << 17;
pub const GUCTL1_PARKMODE_DISABLE_FSLS: u32 = 1 << 15;
pub const PHYSOFTRST: u32 = 1 << 31;
pub const GUSB3PIPECTL_DISRXDETINP3: u32 = 1 << 28;
pub const GTXTHRCFG_PKTCNTSEL: u32 = 1 << 29;

pub const IP_DWC3: u32 = 0x5533;
pub const IP_DWC31: u32 = 0x3331;
pub const IP_DWC32: u32 = 0x3332;

/// The `snps,*` properties both RP1 controllers carry in the Pi 5 DTB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quirks {
    /// `snps,axi-pipe-limit` (0 = leave the reset value).
    pub axi_pipe_limit: u8,
    /// `snps,tx-thr-num-pkt` / `snps,tx-max-burst` (both non-zero to apply).
    pub tx_thr_num_pkt: u8,
    pub tx_max_burst: u8,
    pub dis_rxdet_inp3: bool,
    pub enhanced_nak_fs: bool,
    pub parkmode_disable_ss: bool,
    pub parkmode_disable_fsls: bool,
}

/// RP1 usb@200000 and usb@300000 (identical property sets).
pub const RP1_QUIRKS: Quirks = Quirks {
    axi_pipe_limit: 8,
    tx_thr_num_pkt: 2,
    tx_max_burst: 8,
    dis_rxdet_inp3: true,
    enhanced_nak_fs: true,
    parkmode_disable_ss: true,
    parkmode_disable_fsls: true,
};

/// GSNPSID upper half: which IP this is, or None for a value that is not a DWC3.
pub fn ip_of(snpsid: u32) -> Option<u32> {
    match snpsid >> 16 {
        ip @ (IP_DWC3 | IP_DWC31 | IP_DWC32) => Some(ip),
        _ => None,
    }
}

/// GCTL with the port capability direction set to host (soft reset clear).
pub fn gctl_host(v: u32) -> u32 { (v & !(GCTL_PRTCAPDIR_MASK | GCTL_CORESOFTRESET)) | GCTL_PRTCAP_HOST }

/// GSBUSCFG1.PIPETRANSLIMIT = limit - 1 (Linux rejects limits above 16).
pub fn gsbuscfg1(v: u32, q: &Quirks) -> u32 {
    if q.axi_pipe_limit == 0 || q.axi_pipe_limit > 16 { return v; }
    (v & !(0xf << 8)) | (((q.axi_pipe_limit as u32 - 1) & 0xf) << 8)
}

/// GTXTHRCFG for DWC_usb3 (non-periodic TX threshold).
pub fn gtxthrcfg(v: u32, q: &Quirks) -> u32 {
    if q.tx_thr_num_pkt == 0 || q.tx_max_burst == 0 { return v; }
    let v = v & !((0xf << 24) | (0xff << 16));
    v | GTXTHRCFG_PKTCNTSEL | ((q.tx_thr_num_pkt as u32 & 0xf) << 24) | ((q.tx_max_burst as u32 & 0xff) << 16)
}

pub fn guctl1(v: u32, q: &Quirks) -> u32 {
    let mut v = v;
    if q.enhanced_nak_fs { v |= GUCTL1_NAK_PER_ENH_FS; }
    if q.parkmode_disable_ss { v |= GUCTL1_PARKMODE_DISABLE_SS; }
    if q.parkmode_disable_fsls { v |= GUCTL1_PARKMODE_DISABLE_FSLS; }
    v
}

pub fn gusb3pipectl(v: u32, q: &Quirks) -> u32 {
    let v = v & !PHYSOFTRST;
    if q.dis_rxdet_inp3 { v | GUSB3PIPECTL_DISRXDETINP3 } else { v }
}

/// Step of the core/PHY soft reset (the classic dwc3 sequence: core + both
/// PHYs into reset, wait, PHYs out, wait, core out).
pub fn reset_assert(gctl: u32, u2: u32, u3: u32) -> (u32, u32, u32) {
    (gctl | GCTL_CORESOFTRESET, u2 | PHYSOFTRST, u3 | PHYSOFTRST)
}
pub fn phy_release(u2: u32, u3: u32) -> (u32, u32) { (u2 & !PHYSOFTRST, u3 & !PHYSOFTRST) }

/// RP1 controllers: (name, BAR1 offset, RP1 interrupt source from the DTB).
pub const RP1_USB: [(&str, usize, u32); 2] = [("usb0", 0x20_0000, 31), ("usb1", 0x30_0000, 36)];
/// Size of each DWC3 window in BAR1 and the xHCI part of it.
pub const RP1_USB_SIZE: usize = 0x10_0000;
pub const XHCI_LEN: usize = GLOBALS_START;
/// RP1 GPIO 42 = USB_VBUS_EN (Pi 5 DTB gpio-line-names), driven high.
pub const RP1_USB_VBUS_GPIO: u32 = 42;

/// xHCI device address for a CPU physical address with a fixed inbound offset.
pub const fn dma_addr(pa: u64, offset: u64) -> u64 { pa + offset }
