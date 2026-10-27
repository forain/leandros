#![allow(dead_code)]

#[path = "../../src/dwc3_hw.rs"]
pub mod dwc3_hw;

#[cfg(test)]
mod tests {
    use super::dwc3_hw::*;

    #[test]
    fn snpsid_accepts_dwc3_family_only() {
        assert_eq!(ip_of(0x5533_330a), Some(IP_DWC3));
        assert_eq!(ip_of(0x3331_0000), Some(IP_DWC31));
        assert_eq!(ip_of(0xffff_ffff), None); // UBUS error read (link down)
        assert_eq!(ip_of(0), None);
    }

    #[test]
    fn gctl_host_sets_prtcapdir_and_clears_reset() {
        // Reset value with PRTCAPDIR = OTG (3) and the soft reset still set.
        let v = 0x3000 | GCTL_CORESOFTRESET | 0x0020_0004;
        let h = gctl_host(v);
        assert_eq!(h & GCTL_PRTCAPDIR_MASK, 1 << 12);
        assert_eq!(h & GCTL_CORESOFTRESET, 0);
        assert_eq!(h & 0x0020_0004, 0x0020_0004);
    }

    #[test]
    fn rp1_quirks_match_linux_bits() {
        let q = &RP1_QUIRKS;
        // snps,axi-pipe-limit = 8 → PIPETRANSLIMIT field (11:8) = 7.
        assert_eq!(gsbuscfg1(0xffff_ffff, q), 0xffff_f7ff);
        assert_eq!(gsbuscfg1(0, q), 0x700);
        // tx-thr-num-pkt 2, tx-max-burst 8 → PKTCNTSEL | 2<<24 | 8<<16.
        assert_eq!(gtxthrcfg(0, q), (1 << 29) | (2 << 24) | (8 << 16));
        assert_eq!(gtxthrcfg(0x0f00_0000 | 0x00ff_0000 | 0x1, q), (1 << 29) | (2 << 24) | (8 << 16) | 1);
        // enhanced-nak-fs (19), parkmode-disable-ss (17), parkmode-disable-fsls (15).
        assert_eq!(guctl1(0, q), (1 << 19) | (1 << 17) | (1 << 15));
        // dis_rxdet_inp3 (28), PHY soft reset (31) released.
        assert_eq!(gusb3pipectl(0x8000_0002, q), 0x1000_0002);
    }

    #[test]
    fn quirks_off_leave_registers_alone() {
        let q = Quirks { axi_pipe_limit: 0, tx_thr_num_pkt: 0, tx_max_burst: 8, dis_rxdet_inp3: false,
                         enhanced_nak_fs: false, parkmode_disable_ss: false, parkmode_disable_fsls: false };
        assert_eq!(gsbuscfg1(0x1234, &q), 0x1234);
        assert_eq!(gtxthrcfg(0x1234, &q), 0x1234);
        assert_eq!(guctl1(0x1234, &q), 0x1234);
        assert_eq!(gusb3pipectl(0x1234, &q), 0x1234);
        let q17 = Quirks { axi_pipe_limit: 17, ..q };
        assert_eq!(gsbuscfg1(0x1234, &q17), 0x1234);
    }

    #[test]
    fn soft_reset_sequence() {
        let (g, u2, u3) = reset_assert(0x1000, 0x40, 0x2);
        assert_eq!((g, u2, u3), (0x1800, 0x8000_0040, 0x8000_0002));
        assert_eq!(phy_release(u2, u3), (0x40, 0x2));
    }

    #[test]
    fn rp1_layout_and_dma() {
        // DTB: usb@200000 interrupts <0x1f 1>, usb@300000 <0x24 1>.
        assert_eq!(RP1_USB[0], ("usb0", 0x20_0000, 31));
        assert_eq!(RP1_USB[1], ("usb1", 0x30_0000, 36));
        assert!(XHCI_LEN <= RP1_USB_SIZE);
        // pcie2 inbound window: PCI 0x10_0000_0000 → RAM 0.
        assert_eq!(dma_addr(0x4000_1000, 0x10_0000_0000), 0x10_4000_1000);
    }
}
