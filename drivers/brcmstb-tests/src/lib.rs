extern crate alloc;

#[path = "../../src/brcmstb_hw.rs"]
pub mod brcmstb_hw;

#[cfg(test)]
mod tests {
    use super::brcmstb_hw::*;

    #[test]
    fn config_index_and_offsets() {
        assert_eq!(cfg_index(1, 0, 0), 0x0010_0000);
        assert_eq!(cfg_index(1, 2, 3), 0x0010_0000 | (2 << 15) | (3 << 12));
        // Root port: its own registers, only function 0.
        assert_eq!(cfg_offset(0, 0, 0, 0x18), Some(0x18));
        assert_eq!(cfg_offset(0, 1, 0, 0), None);
        assert_eq!(cfg_offset(0, 0, 1, 0), None);
        // Below the port: the 4 KiB EXT_CFG_DATA window, device 0 only.
        assert_eq!(cfg_offset(1, 0, 0, 0x10), Some(0x8010));
        assert_eq!(cfg_offset(1, 0, 0, 0xffc), Some(0x8ffc));
        assert_eq!(cfg_offset(1, 1, 0, 0), None);
        assert_eq!(cfg_offset(1, 0, 0, 0x1000), None);
        assert!(EXT_CFG_INDEX + 4 <= REG_SIZE && 0x9310 <= REG_SIZE);
    }

    #[test]
    fn outbound_windows_match_pi5_dtb() {
        let w0 = outbound_win_regs(0, OB_MEM_CPU, OB_MEM_PCI, OB_MEM_SIZE, 0);
        assert_eq!(w0, [(0x400c, 0), (0x4010, 0), (0x4070, 0xfff0_0000), (0x4080, 0x1f), (0x4084, 0x1f)]);
        let w1 = outbound_win_regs(1, OB_PREF_CPU, OB_PREF_PCI, OB_PREF_SIZE, 0x000f_000f);
        assert_eq!(w1, [(0x4014, 0), (0x4018, 4), (0x4074, 0xbfff_000f), (0x4088, 0x1c), (0x408c, 0x1c)]);
    }

    #[test]
    fn inbound_size_encoding() {
        assert_eq!(encode_ibar_size(0x1000), 0x1c);
        assert_eq!(encode_ibar_size(0x8000), 0x1f);
        assert_eq!(encode_ibar_size(0x1_0000), 1);
        assert_eq!(encode_ibar_size(1 << 32), 17);
        assert_eq!(encode_ibar_size(0x10_0000_0000), 0x15);
        assert_eq!(encode_ibar_size(1 << 37), 0);
        assert_eq!(encode_ibar_size(0x3000), 0);
        assert_eq!(encode_ibar_size(0), 0);
    }

    #[test]
    fn inbound_windows_match_pi5_dtb() {
        assert_eq!(inbound_win_regs(1, IB_RAM_PCI, IB_RAM_CPU, IB_RAM_SIZE),
            [(0x402c, 0x15), (0x4030, 0x10), (0x40ac, 1), (0x40b0, 0)]);
        assert_eq!(inbound_win_regs(2, MSI_PCI_ADDR, MIP0_REGS, 0x1000),
            [(0x4034, 0xffff_f01c), (0x4038, 0xff), (0x40b4, 0x0013_0001), (0x40b8, 0x10)]);
        // Disabled: size 0 → encoding 0 and no UBUS access enable.
        assert_eq!(inbound_win_regs(3, 0, 0, 0), [(0x403c, 0), (0x4040, 0), (0x40bc, 0), (0x40c0, 0)]);
        // Windows must not overlap: RAM ends below the MSI doorbell.
        assert!(IB_RAM_PCI + IB_RAM_SIZE <= MSI_PCI_ADDR);
        assert_eq!(dma_addr(0x1234_5000), 0x10_1234_5000);
    }

    #[test]
    fn resets_mdio_link() {
        assert_eq!(sw_init_set(PCIE2_BRIDGE_RESET_ID), (0x18, 1 << 12));
        assert_eq!(mdio_pkt(0, 0x1f, MDIO_CMD_WRITE), 0x1f);
        assert_eq!(mdio_pkt(0, 0x16, 1), 0x0010_0016);
        assert!(link_up(0x30) && link_up(0xb0) && !link_up(0x10) && !link_up(0x20));
        // LNKSTA 0x1042: 5 GT/s, x4.
        assert_eq!(lnksta_decode(0x1042), (2, 4));
    }

    #[test]
    fn bar_sizing_and_assignment_rp1_layout() {
        assert_eq!(bar_size(0xffc0_0000, None), 0x40_0000);
        assert_eq!(bar_size(0xffff_c000, None), 0x4000);
        assert_eq!(bar_size(0xffff_000c, Some(0xffff_ffff)), 0x1_0000);
        assert_eq!(bar_size(0, None), 0);
        // RP1-like: BAR0 16 KiB, BAR1 4 MiB peripherals, BAR2 64 KiB SRAM.
        let reqs = [
            BarReq { index: 0, size: 0x4000, is64: false },
            BarReq { index: 1, size: 0x40_0000, is64: false },
            BarReq { index: 2, size: 0x1_0000, is64: false },
        ];
        let (placed, end) = assign_bars(&reqs, 0, 0xffff_fffc);
        assert_eq!(placed, vec![(0, 0x41_0000), (1, 0), (2, 0x40_0000)]);
        assert_eq!(end, 0x41_4000);
        // BAR1 at 0 and BAR2 right after it is what the DTB's rp1 `ranges`
        // (0xc0_4000_0000 → PCI 0, 0x41_0000 bytes) assume.
        assert_eq!(bridge_mem_window(0, end), 0x0040_0000);
        assert_eq!(bridge_mem_window(0, 0), 0x0000_fff0);
        // Does not fit → left out; non-power-of-two ignored.
        let (p, _) = assign_bars(&[BarReq { index: 0, size: 0x1000, is64: false },
                                   BarReq { index: 1, size: 0x3000, is64: false }], 0, 0x800);
        assert!(p.is_empty());
    }

    #[test]
    fn rp1_interrupt_routing() {
        assert_eq!(mip_intid(0), 160);
        assert_eq!(mip_intid(RP1_INT_ETH), 166);
        assert_eq!(mip_intid(MIP0_VECTORS - 1), 223);
        assert!(RP1_INT_END <= MIP0_VECTORS);
        assert_eq!(mip_msg(6), (0xff_ffff_f000, 6));
        assert_eq!(rp1_msix_cfg(0, 0), 0x10_8008);
        assert_eq!(rp1_msix_cfg(6, RP1_REG_SET), 0x10_8820);
        assert_eq!(rp1_msix_cfg(30, RP1_REG_CLR), 0x10_8c80);
    }
}
