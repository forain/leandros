//! Host tests for the kernel's Cadence GEM (macb) hardware-independent
//! module. The module under test is included verbatim, so these exercise
//! exactly the code the kernel runs: rings against descriptors a simulated
//! controller hands back, MDIO against a mock register file with a
//! simulated BCM54213PE behind the MAN register, and the DTB walker against
//! a hand-built flattened tree.

#[path = "../../src/macb_hw.rs"]
pub mod macb_hw;

#[cfg(test)]
mod tests {
    use super::macb_hw::*;
    use std::collections::HashMap;

    const DMA_OFF: u64 = 0x10_0000_0000;

    // ── rings ───────────────────────────────────────────────────────────────

    fn hw_rx_fill(d: &mut [Desc], i: usize, len: u32, sof: bool, eof: bool) {
        d[i].ctrl = len | if sof { RX_SOF } else { 0 } | if eof { RX_EOF } else { 0 };
        d[i].addr |= RX_USED;
    }

    #[test]
    fn rx_ring_init_and_wrap() {
        let mut d = vec![Desc::default(); 4];
        let mut r = unsafe { RxRing::init(d.as_mut_ptr(), 4, &|i| DMA_OFF + 0x8000 + i as u64 * 2048) };
        for (i, x) in d.iter().enumerate() {
            assert_eq!(x.addr_hi, 0x10);
            assert_eq!(x.addr & RX_ADDR_MASK, 0x8000 + i as u32 * 2048);
            assert_eq!(x.addr & RX_USED, 0, "slot {} must start owned by the controller", i);
            assert_eq!(x.addr & RX_WRAP != 0, i == 3);
        }
        assert_eq!(r.next(), RxEvent::Empty);
        // Two full laps: ownership flips back each time, WRAP is never lost.
        for lap in 0..2 {
            for i in 0..4 {
                hw_rx_fill(&mut d, i, 60 + i as u32 + lap, true, true);
                assert_eq!(r.next(), RxEvent::Frame { idx: i, len: 60 + i + lap as usize });
                r.release(i);
                assert_eq!(d[i].addr & RX_USED, 0);
                assert_eq!(d[i].ctrl, 0);
                assert_eq!(d[i].addr & RX_WRAP != 0, i == 3);
                assert_eq!(d[i].addr & RX_ADDR_MASK, 0x8000 + i as u32 * 2048);
            }
            assert_eq!(r.head(), 0);
        }
        // Out-of-order completion is not visible before the head completes.
        hw_rx_fill(&mut d, 1, 64, true, true);
        assert_eq!(r.next(), RxEvent::Empty);
        hw_rx_fill(&mut d, 0, 70, true, false);
        assert_eq!(r.next(), RxEvent::Fragment { idx: 0 });
        r.release(0);
        assert_eq!(r.next(), RxEvent::Frame { idx: 1, len: 64 });
    }

    #[test]
    fn tx_ring_ownership_and_wrap() {
        let mut d = vec![Desc::default(); 4];
        let mut t = unsafe { TxRing::init(d.as_mut_ptr(), 4, &|i| DMA_OFF + 0x20000 + i as u64 * 2048) };
        for (i, x) in d.iter().enumerate() {
            assert_eq!(x.ctrl & TX_USED, TX_USED, "idle slots stop the controller");
            assert_eq!(x.ctrl & TX_WRAP != 0, i == 3);
            assert_eq!((x.addr, x.addr_hi), (0x20000 + i as u32 * 2048, 0x10));
        }
        for i in 0..3 {
            let s = t.slot().unwrap();
            assert_eq!(s, i);
            t.post(s, 100 + i);
            assert_eq!(d[i].ctrl, (100 + i as u32) | TX_LAST);
        }
        assert_eq!(t.in_flight(), 3);
        assert_eq!(t.slot(), None, "one slot always stays idle");
        assert_eq!(t.reclaim(), 0);
        // Controller finishes slots 0 and 1 (sets USED on the first word).
        d[0].ctrl |= TX_USED;
        d[1].ctrl |= TX_USED | (1 << 29);
        assert_eq!(t.reclaim(), 2);
        assert_eq!(t.errors, 1);
        // Head wraps: slot 3 carries WRAP, then slot 0 again.
        assert_eq!(t.slot(), Some(3));
        t.post(3, 60);
        assert_eq!(d[3].ctrl, 60 | TX_LAST | TX_WRAP);
        assert_eq!(t.slot(), Some(0));
        t.post(0, 61);
        assert_eq!(d[0].ctrl & TX_USED, 0);
        assert_eq!(t.in_flight(), 3);
        d[2].ctrl |= TX_USED;
        d[3].ctrl |= TX_USED;
        d[0].ctrl |= TX_USED;
        assert_eq!(t.reclaim(), 3);
        assert_eq!(t.in_flight(), 0);
    }

    // ── mock GEM + BCM54213PE ───────────────────────────────────────────────

    #[derive(Default)]
    struct Partner { lpa: u16, gbsr: u16 }

    struct Mock {
        regs: HashMap<usize, u32>,
        phy_addr: u8,
        mii: [u16; 32],
        misc: u16,
        shd: [u16; 32],
        shd_sel: usize,
        reset_left: u32,
        reset_stuck: bool,
        aneg_left: u32,
        partner: Option<Partner>,
        latched_down: bool,
        busy_reads: u32,
        now_us: u64,
    }

    impl Mock {
        fn new(addr: u8) -> Mock {
            let mut m = Mock { regs: HashMap::new(), phy_addr: addr, mii: [0; 32], misc: 0x0000, shd: [0; 32],
                shd_sel: 0, reset_left: 0, reset_stuck: false, aneg_left: 0, partner: None, latched_down: false,
                busy_reads: 0, now_us: 0 };
            m.power_on();
            m
        }
        fn power_on(&mut self) {
            self.mii = [0; 32];
            self.mii[0] = BMCR_ANENABLE | BMCR_PDOWN;
            self.mii[1] = 0x7949;
            self.mii[2] = 0x600d;
            self.mii[3] = 0x84a2;
            self.mii[4] = 0x0de1;
            self.mii[9] = 0;
        }
        fn phy_read(&mut self, reg: usize) -> u16 {
            match reg {
                0 => {
                    if self.reset_left > 0 && !self.reset_stuck {
                        self.reset_left -= 1;
                        if self.reset_left == 0 { self.mii[0] &= !BMCR_RESET; }
                    }
                    self.mii[0]
                }
                1 => {
                    if self.latched_down { self.latched_down = false; return self.mii[1] & !BMSR_LSTATUS; }
                    if self.aneg_left > 0 {
                        self.aneg_left -= 1;
                        if self.aneg_left == 0 {
                            if let Some(p) = &self.partner {
                                self.mii[1] |= BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
                                self.mii[5] = p.lpa;
                                self.mii[10] = p.gbsr;
                            }
                        }
                    }
                    self.mii[1]
                }
                0x18 => self.misc | 0x7,
                0x1c => ((self.shd_sel as u16) << 10) | self.shd[self.shd_sel],
                r => self.mii[r],
            }
        }
        fn phy_write(&mut self, reg: usize, v: u16) {
            match reg {
                0 => {
                    if v & BMCR_RESET != 0 {
                        let stuck = self.reset_stuck;
                        self.power_on();
                        self.mii[0] = BMCR_RESET | BMCR_ANENABLE | BMCR_PDOWN;
                        self.reset_left = 3;
                        self.reset_stuck = stuck;
                        return;
                    }
                    self.mii[0] = v & !BMCR_ANRESTART;
                    if v & BMCR_ANRESTART != 0 && v & BMCR_ANENABLE != 0 && v & BMCR_PDOWN == 0 {
                        self.mii[1] &= !(BMSR_LSTATUS | BMSR_ANEGCOMPLETE);
                        self.aneg_left = 5;
                    }
                }
                0x18 => {
                    if v & 0x7 == 0x7 && v & AUXCTL_MISC_WREN == 0 { return; } // shadow-select for read
                    if v & 0x7 == 0x7 { self.misc = v & !0x7 & !AUXCTL_MISC_WREN; }
                }
                0x1c => {
                    let sel = ((v >> 10) & 0x1f) as usize;
                    self.shd_sel = sel;
                    if v & 0x8000 != 0 { self.shd[sel] = v & 0x3ff; }
                }
                r => self.mii[r] = v,
            }
        }
        fn unplug(&mut self) {
            self.partner = None;
            self.mii[1] &= !(BMSR_LSTATUS | BMSR_ANEGCOMPLETE);
            self.latched_down = true;
        }
        fn plug(&mut self, p: Partner) {
            self.partner = Some(p);
            self.aneg_left = 4;
        }
        fn get(&self, off: usize) -> u32 { *self.regs.get(&off).unwrap_or(&0) }
    }

    impl Regs for Mock {
        fn r32(&self, off: usize) -> u32 {
            if off == NSR {
                return if self.busy_reads > 0 { 0 } else { NSR_MDIO_IDLE };
            }
            self.get(off)
        }
        fn w32(&mut self, off: usize, v: u32) {
            if off == MAN {
                assert_eq!(v & (3 << 30), MAN_SOF_C22);
                assert_eq!(v & (3 << 16), MAN_CODE);
                let addr = ((v >> 23) & 31) as u8;
                let reg = ((v >> 18) & 31) as usize;
                let data = if addr != self.phy_addr {
                    0xffff
                } else if v & (3 << 28) == MAN_RW_READ {
                    self.phy_read(reg)
                } else {
                    self.phy_write(reg, v as u16);
                    v as u16
                };
                self.regs.insert(MAN, (v & 0xffff_0000) | data as u32);
                return;
            }
            self.regs.insert(off, v);
        }
        fn delay_us(&mut self, us: u32) { self.now_us += us as u64; }
    }

    fn bring_up(m: &mut Mock, mode: PhyMode) -> LinkWatch {
        m.w32(NCFGR, ncfgr_base(2 << 25));
        m.w32(NCR, NCR_MPE);
        let (addr, id) = find_phy(m, 1).unwrap();
        assert_eq!(addr, m.phy_addr);
        assert_eq!(id & !0xf, BCM54213PE_ID);
        phy_setup(m, addr, mode).unwrap();
        LinkWatch::default()
    }

    fn poll_until_change(m: &mut Mock, w: &mut LinkWatch, n: usize) -> Option<Link> {
        let a = m.phy_addr;
        for _ in 0..n {
            if let Some(l) = w.poll(m, a).unwrap() { return Some(l); }
        }
        None
    }

    #[test]
    fn autoneg_gigabit_full_programs_ncfgr() {
        let mut m = Mock::new(1);
        m.partner = Some(Partner { lpa: 0x45e1, gbsr: GBSR_LP_1000FULL | GBSR_LP_1000HALF });
        let mut w = bring_up(&mut m, PhyMode::RgmiiId);
        // Reset cleared the power-down bit, advertisement and autoneg restart landed.
        assert_eq!(m.mii[0] & (BMCR_PDOWN | BMCR_RESET), 0);
        assert_eq!(m.mii[4], ANAR_DEFAULT);
        assert_eq!(m.mii[9], GBCR_1000FULL | GBCR_1000HALF);
        // rgmii-id: both delays in the PHY.
        assert_ne!(m.misc & AUXCTL_MISC_RGMII_SKEW_EN, 0);
        assert_ne!(m.shd[SHD_CLK_CTL as usize] & SHD_CLK_GTXCLK_EN, 0);
        assert_eq!(m.get(NCR) & (NCR_RE | NCR_TE), 0, "RX/TX stay off until link");
        let l = poll_until_change(&mut m, &mut w, 10).expect("link up");
        assert_eq!(l, Link { up: true, speed: 1000, full: true });
        let ncfgr = m.get(NCFGR);
        assert_eq!(ncfgr & (NCFGR_GBE | NCFGR_FD | NCFGR_SPD), NCFGR_GBE | NCFGR_FD);
        assert_eq!(ncfgr & NCFGR_CLK_MASK, CLK_DIV96 << NCFGR_CLK_SHIFT, "MDC divider kept");
        assert_eq!(m.get(NCR) & (NCR_RE | NCR_TE | NCR_MPE), NCR_RE | NCR_TE | NCR_MPE);
        // Stable link: no further change reported.
        assert_eq!(poll_until_change(&mut m, &mut w, 5), None);
        assert_eq!(w.changes, 1);
    }

    #[test]
    fn link_change_down_then_100_full() {
        let mut m = Mock::new(1);
        m.partner = Some(Partner { lpa: 0x45e1, gbsr: GBSR_LP_1000FULL });
        let mut w = bring_up(&mut m, PhyMode::RgmiiId);
        assert!(poll_until_change(&mut m, &mut w, 10).unwrap().up);
        m.unplug();
        assert_eq!(poll_until_change(&mut m, &mut w, 3), Some(Link::default()));
        // A 100M full-duplex-only partner: GBE must be cleared, SPD+FD set.
        m.plug(Partner { lpa: 0x4101, gbsr: 0 });
        let l = poll_until_change(&mut m, &mut w, 10).unwrap();
        assert_eq!(l, Link { up: true, speed: 100, full: true });
        assert_eq!(m.get(NCFGR) & (NCFGR_GBE | NCFGR_FD | NCFGR_SPD), NCFGR_SPD | NCFGR_FD);
        // 10M half: all three clear.
        m.unplug();
        poll_until_change(&mut m, &mut w, 3).unwrap();
        m.plug(Partner { lpa: 0x0021, gbsr: 0 });
        assert_eq!(poll_until_change(&mut m, &mut w, 10).unwrap(), Link { up: true, speed: 10, full: false });
        assert_eq!(m.get(NCFGR) & (NCFGR_GBE | NCFGR_FD | NCFGR_SPD), 0);
        assert_eq!(w.changes, 5);
    }

    #[test]
    fn rgmii_without_delays_clears_both() {
        let mut m = Mock::new(1);
        m.misc = AUXCTL_MISC_RGMII_SKEW_EN;
        m.shd[SHD_CLK_CTL as usize] = SHD_CLK_GTXCLK_EN | 0x5;
        let mut m2 = m;
        bring_up(&mut m2, PhyMode::Rgmii);
        assert_eq!(m2.misc & AUXCTL_MISC_RGMII_SKEW_EN, 0);
        assert_eq!(m2.shd[SHD_CLK_CTL as usize], 0x5, "other clock-control bits preserved");
        let mut m3 = Mock::new(1);
        bring_up(&mut m3, PhyMode::RgmiiTxid);
        assert_eq!(m3.misc & AUXCTL_MISC_RGMII_SKEW_EN, 0);
        assert_ne!(m3.shd[SHD_CLK_CTL as usize] & SHD_CLK_GTXCLK_EN, 0);
    }

    #[test]
    fn phy_scan_reset_stuck_and_mdio_timeout() {
        let mut m = Mock::new(7);
        assert_eq!(find_phy(&mut m, 1).unwrap().0, 7);
        let mut e = Mock::new(9);
        e.phy_addr = 40; // nothing answers anywhere
        assert_eq!(find_phy(&mut e, 1), Err(MdioError::NoPhy));
        let mut s = Mock::new(1);
        s.reset_stuck = true;
        assert_eq!(phy_setup(&mut s, 1, PhyMode::RgmiiId), Err(MdioError::ResetStuck));
        assert!(s.now_us >= 500_000 && s.now_us < 2_000_000, "bounded wait, got {} us", s.now_us);
        let mut b = Mock::new(1);
        b.busy_reads = 1;
        assert_eq!(mdio_read(&mut b, 1, 2), Err(MdioError::Timeout));
        // No link partner: autoneg never completes, link stays down, no change.
        let mut n = Mock::new(1);
        let mut w = bring_up(&mut n, PhyMode::RgmiiId);
        assert_eq!(poll_until_change(&mut n, &mut w, 20), None);
        assert_eq!(n.get(NCR) & (NCR_RE | NCR_TE), 0);
    }

    // ── register values ─────────────────────────────────────────────────────

    #[test]
    fn register_values() {
        assert_eq!(ncfgr_for_link(NCFGR_SPD | NCFGR_BIG, 1000, true), NCFGR_GBE | NCFGR_FD | NCFGR_BIG);
        assert_eq!(ncfgr_for_link(NCFGR_GBE | NCFGR_FD, 100, false), NCFGR_SPD);
        assert_eq!(ncfgr_for_link(0xffff_ffff, 10, false), !(NCFGR_SPD | NCFGR_FD | NCFGR_GBE));
        let d = dmacfg_value(DMACFG_ENDIA_PKT | DMACFG_TXCOEN | (5 << 16) | 4, 2048, true);
        assert_eq!((d & DMACFG_RXBS_MASK) >> DMACFG_RXBS_SHIFT, 32);
        assert_eq!(d & DMACFG_FBLDO_MASK, 16);
        assert_ne!(d & DMACFG_ADDR64, 0);
        assert_eq!(d & (DMACFG_ENDIA_PKT | DMACFG_ENDIA_DESC | DMACFG_TXCOEN), 0);
        assert_eq!(d & (DMACFG_RXBMS_MASK | DMACFG_TXPBMS), DMACFG_RXBMS_MASK | DMACFG_TXPBMS);
        assert_eq!(dmacfg_value(0, 1536, false) & DMACFG_ADDR64, 0);
        assert_eq!(dbw_from_dcfg1(4 << 25), 2);
        assert_eq!(dbw_from_dcfg1(2 << 25), 1);
        assert_eq!(dbw_from_dcfg1(1 << 25), 0);
        assert_eq!(ncfgr_base(2 << 25) & NCFGR_DBW_MASK, 1 << NCFGR_DBW_SHIFT);
        assert_eq!(num_queues(0), 1);
        assert_eq!(num_queues(DCFG6_DAW64 | 0b1110), 4);
        let mac = [0x2c, 0xcf, 0x67, 0x01, 0x02, 0x03];
        let (b, t) = sa_from_mac(&mac);
        assert_eq!((b, t), (0x0167_cf2c, 0x0302));
        assert_eq!(mac_from_sa(b, t), mac);
        assert!(mac_valid(&mac));
        assert!(!mac_valid(&[0; 6]));
        assert!(!mac_valid(&[0xff; 6]));
        assert!(!mac_valid(&[0x01, 0, 0x5e, 0, 0, 1]));
    }

    // ── DTB ─────────────────────────────────────────────────────────────────

    struct Fdt { st: Vec<u8>, strs: Vec<u8> }
    impl Fdt {
        fn new() -> Fdt { Fdt { st: vec![], strs: vec![] } }
        fn tok(&mut self, t: u32) { self.st.extend_from_slice(&t.to_be_bytes()); }
        fn pad(&mut self) { while self.st.len() % 4 != 0 { self.st.push(0); } }
        fn begin(&mut self, name: &str) { self.tok(1); self.st.extend_from_slice(name.as_bytes()); self.st.push(0); self.pad(); }
        fn end(&mut self) { self.tok(2); }
        fn prop(&mut self, name: &str, v: &[u8]) {
            let off = self.strs.len() as u32;
            self.strs.extend_from_slice(name.as_bytes());
            self.strs.push(0);
            self.tok(3);
            self.tok(v.len() as u32);
            self.tok(off);
            self.st.extend_from_slice(v);
            self.pad();
        }
        fn finish(mut self) -> Vec<u8> {
            self.tok(9);
            let hdr = 40u32;
            let st_off = hdr;
            let ss_off = st_off + self.st.len() as u32;
            let total = ss_off + self.strs.len() as u32;
            let mut b = vec![];
            for v in [0xd00d_feedu32, total, st_off, ss_off, 0, 17, 16, 0, self.strs.len() as u32, self.st.len() as u32] {
                b.extend_from_slice(&v.to_be_bytes());
            }
            b.extend_from_slice(&self.st);
            b.extend_from_slice(&self.strs);
            b
        }
    }

    fn rp1_tree(mac: [u8; 6], status: Option<&str>) -> Vec<u8> {
        let mut f = Fdt::new();
        f.begin("");
        f.prop("compatible", b"raspberrypi,5-model-b\0brcm,bcm2712\0");
        f.begin("axi");
        f.begin("pcie@1000120000");
        f.begin("rp1");
        f.begin("serial@30000");
        f.prop("compatible", b"arm,pl011\0");
        f.prop("local-mac-address", &[2, 0, 0, 0, 0, 9]);
        f.end();
        f.begin("ethernet@100000");
        f.prop("reg", &[0, 0, 0, 0xc0, 0x40, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0]);
        f.prop("compatible", b"raspberrypi,rp1-gem\0cdns,macb\0");
        f.prop("phy-mode", b"rgmii-id\0");
        f.prop("local-mac-address", &mac);
        if let Some(s) = status { f.prop("status", s.as_bytes()); }
        f.begin("ethernet-phy@1");
        f.prop("reg", &[0, 0, 0, 1]);
        f.end();
        f.end();
        f.end();
        f.end();
        f.end();
        f.end();
        f.finish()
    }

    #[test]
    fn dtb_gem_lookup() {
        let mac = [0x2c, 0xcf, 0x67, 0xaa, 0xbb, 0xcc];
        let g = fdt_find_gem(&rp1_tree(mac, Some("okay\0")));
        assert_eq!(g, GemDt { found: true, mac: Some(mac), phy_mode: Some(PhyMode::RgmiiId), phy_addr: Some(1) });
        // Firmware did not fill the address in: no MAC, the rest still found.
        let g = fdt_find_gem(&rp1_tree([0; 6], None));
        assert!(g.found);
        assert_eq!(g.mac, None);
        assert_eq!(g.phy_addr, Some(1));
        assert!(!fdt_find_gem(&rp1_tree(mac, Some("disabled\0"))).found);
        assert!(!fdt_find_gem(&[0u8; 64]).found);
        let mut t = rp1_tree(mac, None);
        t.truncate(120);
        let _ = fdt_find_gem(&t); // truncated: must not panic
    }

    struct StatRegs { v: std::cell::RefCell<std::collections::HashMap<usize, u32>> }
    impl Regs for StatRegs {
        fn r32(&self, off: usize) -> u32 { self.v.borrow_mut().remove(&off).unwrap_or(0) } // clear on read
        fn w32(&mut self, _off: usize, _v: u32) {}
        fn delay_us(&mut self, _us: u32) {}
    }

    #[test]
    fn hw_stats_accumulate_clear_on_read_and_rx_verdict() {
        let mut r = StatRegs { v: Default::default() };
        let mut s = HwStats::default();
        r.v.borrow_mut().extend([(STAT_TX_FRAMES, 4), (STAT_RX_FCS, 3), (STAT_OCT_TX_LO, 100), (STAT_OCT_TX_HI, 1)]);
        s.accumulate(&mut r);
        let w0 = s;
        assert_eq!((s.tx_frames, s.rx_fcs, s.tx_octets), (4, 3, (1 << 32) + 100));
        assert_eq!(rx_verdict(&s), RxVerdict::BadTiming);
        s.accumulate(&mut r); // registers cleared: nothing added
        assert_eq!(s, w0);
        r.v.borrow_mut().extend([(STAT_RX_FRAMES, 2), (STAT_TX_FRAMES, 1)]);
        s.accumulate(&mut r);
        let d = s.delta(&w0);
        assert_eq!((d.tx_frames, d.rx_frames, d.rx_fcs), (1, 2, 0));
        assert_eq!(rx_verdict(&d), RxVerdict::Good);
        assert_eq!(rx_verdict(&HwStats::default()), RxVerdict::Silent);
        assert_eq!(PhyMode::RgmiiId.flip_rx_delay(), PhyMode::RgmiiTxid);
        assert!(!PhyMode::RgmiiId.flip_rx_delay().rx_delay() && PhyMode::RgmiiId.flip_rx_delay().tx_delay());
        let mut i = IsrCounts::default();
        i.add(INT_TCOMP | INT_HRESP);
        assert_eq!((i.tcomp, i.hresp, i.rcomp), (1, 1, 0));
    }

    // ── lane pigem-tx ───────────────────────────────────────────────────────

    #[test]
    fn ep_devctl_follows_root_port_mps_and_caps_mrrs() {
        // Root port at 512 B, RP1 supports 512 B but sits at 128 B with the
        // spec-default 512 B read request: reads would come back in 512 B
        // completions RP1 must reject.
        let ep_dc: u16 = (2 << 12) | 0x0010; // MRRS 512, MPS 128, relaxed ordering
        assert_eq!(pcie_ep_devctl(2 << 5, 2, ep_dc), (2 << 12) | (2 << 5) | 0x0010);
        // Root port larger than what RP1 supports: both drop to RP1's 256 B.
        assert_eq!(pcie_ep_devctl(2 << 5, 1, ep_dc), (1 << 12) | (1 << 5) | 0x0010);
        // Root port at 128 B: MRRS follows down to 128 B.
        assert_eq!(pcie_ep_devctl(0, 2, ep_dc), 0x0010);
    }

    #[test]
    fn tsr_names_decode_the_pi_boot_value() {
        let mut s = String::new();
        tsr_names(0x110, &mut s);
        assert_eq!(s, "tfc|hresp");
        let mut s = String::new();
        tsr_names(0, &mut s);
        assert_eq!(s, "-");
        assert!(0x110 & TSR_TX_HALT_ERR != 0 && 0x21 & TSR_TX_HALT_ERR == 0);
    }

    #[test]
    fn burst_replaces_only_fbldo() {
        assert_eq!(dmacfg_with_burst(0x4020_0710, 4), 0x4020_0704);
        assert_eq!(dmacfg_with_burst(0x4020_0704, 7), 0x4020_0710);
        assert_eq!((shorter_burst(16), shorter_burst(8), shorter_burst(4), shorter_burst(1)), (8, 4, 1, 1));
    }

    #[test]
    fn tx_reset_returns_every_slot_and_rewinds() {
        let mut d = vec![Desc::default(); 4];
        let mut t = unsafe { TxRing::init(d.as_mut_ptr(), 4, &|i| DMA_OFF + 0x20000 + i as u64 * 2048) };
        let a = t.slot().unwrap();
        t.post(a, 60);
        let b = t.slot().unwrap();
        t.post(b, 60);
        assert_eq!(t.in_flight(), 2);
        assert_eq!(t.reset(), 2);
        assert_eq!((t.in_flight(), t.slot(), t.tail()), (0, Some(0), 0));
        for (i, x) in d.iter().enumerate() {
            assert_eq!(x.ctrl, TX_USED | if i == 3 { TX_WRAP } else { 0 });
            assert_eq!(x.addr_hi, 0x10);
        }
    }

    #[test]
    fn ring_slot_maps_queue_pointer_to_index() {
        assert_eq!(ring_slot(0x103f020, 0x103f000, 256), Some(2));
        assert_eq!(ring_slot(0x103f000, 0x103f000, 256), Some(0));
        assert_eq!(ring_slot(0x103f000 + 256 * 16, 0x103f000, 256), None);
        assert_eq!(ring_slot(0x103f008, 0x103f000, 256), None);
        assert_eq!(ring_slot(0x103e000, 0x103f000, 256), None);
    }

    #[test]
    fn tx_stall_fires_only_on_an_idle_queue_that_does_not_move() {
        // The Pi boot: TBQP parked on slot 2, frames queued, TGO clear.
        let mut st = TxStall::default();
        let fired: Vec<bool> = (0..TX_STALL_POLLS + 1).map(|_| st.tick(Some(2), 2, 4, false)).collect();
        assert_eq!(fired.iter().filter(|&&f| f).count(), 1);
        assert!(*fired.last().unwrap());
        // Progress (tail or TBQP moving) restarts the count.
        let mut st = TxStall::default();
        for k in 0..20 { assert!(!st.tick(Some(k), k, 3, false)); }
        // Busy transmitter or nothing in flight never counts as a stall.
        let mut st = TxStall::default();
        for _ in 0..20 { assert!(!st.tick(Some(2), 2, 4, true)); }
        for _ in 0..20 { assert!(!st.tick(Some(2), 2, 0, false)); }
    }
}
