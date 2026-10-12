//! Host tests for the kernel's e1000e hardware-independent module. The module
//! under test is included verbatim, so these exercise exactly the code the
//! kernel runs: against a simulated NIC that fills / completes descriptors by
//! the RDH/RDT and TDH/TDT index rules, a simulated ICR/IMS/MSI-edge
//! interrupt block, and (when the pinned Linux sources are in
//! ~/.cache/leandros/linux-e1000e-v6.11) Linux's own device table.

#[path = "../../src/e1000e_hw.rs"]
pub mod e1000e_hw;

#[cfg(test)]
mod tests {
    use super::e1000e_hw::*;

    // ── device table ───────────────────────────────────────────────────────

    #[test]
    fn device_table() {
        let mut ids: Vec<u16> = DEVICES.iter().map(|d| d.did).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), DEVICES.len(), "duplicate PCI ids");
        assert_eq!(DEVICES.len(), 57);
        let lm4 = lookup(0x15d7).unwrap();
        assert_eq!((lm4.name, lm4.fam), ("PCH_SPT_I219_LM4", Family::PchSpt));
        assert_eq!(lookup(0x15e3).unwrap().fam, Family::PchSpt);
        assert_eq!(lookup(0x156f).unwrap().fam, Family::PchSpt);
        assert_eq!(lookup(0x0d53).unwrap().fam, Family::PchSpt); // CMP LM12 is an SPT board in Linux
        assert_eq!(lookup(0x0d4e).unwrap().fam, Family::PchCnp);
        assert_eq!(lookup(0x15bb).unwrap().fam, Family::PchCnp);
        assert_eq!(lookup(0x15fb).unwrap().fam, Family::PchTgp);
        assert_eq!(lookup(0x0dc5).unwrap().fam, Family::PchAdp);
        assert_eq!(lookup(0x57ba).unwrap().fam, Family::PchMtp);
        assert_eq!(lookup(0x10d3).unwrap().fam, Family::I82574);
        assert!(lookup(0x100e).is_none()); // plain e1000 (82540EM): not this driver
        assert!(Family::PchCnp > Family::PchSpt && Family::PchSpt.is_pch() && !Family::I82574.is_pch());
        // 82574 last: an onboard I219 wins over an add-in 82574.
        assert_eq!(DEVICES.last().unwrap().fam, Family::I82574);
        assert!(DEVICES.iter().take_while(|d| d.fam != Family::I82574).all(|d| d.fam.is_pch()));
    }

    /// Cross-check against Linux v6.11's e1000e table (hw.h ids + netdev.c
    /// board mapping) when the pinned sources are cached; skipped otherwise.
    #[test]
    fn device_table_matches_linux() {
        let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/leandros/linux-e1000e-v6.11");
        let (hw_h, netdev_c) = match (std::fs::read_to_string(dir.join("hw.h")), std::fs::read_to_string(dir.join("netdev.c"))) {
            (Ok(a), Ok(b)) => (a, b),
            _ => { eprintln!("skipped: no Linux sources in {}", dir.display()); return; }
        };
        let mut ids = std::collections::HashMap::new();
        for l in hw_h.lines() {
            let t: Vec<&str> = l.split_whitespace().collect();
            if t.len() >= 3 && t[0] == "#define" && t[1].starts_with("E1000_DEV_ID_") {
                if let Ok(v) = u16::from_str_radix(t[2].trim_start_matches("0x").trim_start_matches("0X"), 16) {
                    ids.insert(t[1].trim_start_matches("E1000_DEV_ID_").to_string(), v);
                }
            }
        }
        let mut n = 0;
        for l in netdev_c.lines() {
            let l = l.trim();
            let Some(rest) = l.strip_prefix("{ PCI_VDEVICE(INTEL, E1000_DEV_ID_") else { continue };
            let name = rest.split(')').next().unwrap();
            // A few entries wrap the board onto the next line: none of ours.
            let Some(board) = rest.split("board_").nth(1) else { continue };
            let board = board.trim_end_matches(" },").trim_end_matches('}').trim();
            let fam = match board {
                "pch_spt" => Family::PchSpt, "pch_cnp" => Family::PchCnp, "pch_tgp" => Family::PchTgp,
                "pch_adp" => Family::PchAdp, "pch_mtp" => Family::PchMtp, "82574" => Family::I82574,
                _ => continue,
            };
            let did = ids[name];
            let d = lookup(did).unwrap_or_else(|| panic!("Linux binds {} ({:04x}) as {}, missing here", name, did, board));
            assert_eq!(d.fam, fam, "{}", name);
            assert_eq!(d.name, name);
            n += 1;
        }
        assert_eq!(n, DEVICES.len(), "every Linux I219/82574 id is here, and nothing else");
    }

    // ── simulated NIC for the rings ────────────────────────────────────────

    fn rx_alloc(n: usize) -> Vec<RxDesc> { vec![RxDesc::default(); n] }
    fn tx_alloc(n: usize) -> Vec<TxDesc> { vec![TxDesc::default(); n] }

    /// The NIC's receive side: owns descriptors RDH..RDT-1 (mod n).
    struct SimRx { rdh: usize, rdt: usize, n: usize }
    impl SimRx {
        /// Deliver a frame if a descriptor is available; returns its index.
        fn deliver(&mut self, descs: &mut [RxDesc], len: u16, errors: u8, eop: bool) -> Option<usize> {
            if self.rdh == self.rdt { return None; } // no descriptors: RNBC, frame dropped
            let i = self.rdh;
            assert_eq!(descs[i].status, 0, "NIC got a descriptor the driver had not recycled");
            descs[i].len = len;
            descs[i].errors = errors;
            descs[i].status = RXD_STAT_DD | if eop { RXD_STAT_EOP } else { 0 };
            self.rdh = (self.rdh + 1) % self.n;
            Some(i)
        }
    }

    #[test]
    fn rx_ring_wraps_and_recycles() {
        let n = 8;
        let mut descs = rx_alloc(n);
        let mut r = unsafe { RxRing::init(descs.as_mut_ptr(), n, 2048, &|i| 0x1000 * (i as u64 + 1)) };
        assert!(descs.iter().enumerate().all(|(i, d)| d.addr == 0x1000 * (i as u64 + 1) && d.status == 0));
        let mut nic = SimRx { rdh: 0, rdt: r.initial_tail() as usize, n };
        assert_eq!(r.next(), RxEvent::Empty);
        let mut expect = std::collections::VecDeque::new();
        let mut got = 0;
        for round in 0..200usize {
            // Burst bigger than the ring sometimes: the NIC must stop at RDT.
            let burst = (round * 7) % (n + 3);
            for k in 0..burst {
                let len = 60 + ((round + k) % 1400) as u16;
                if nic.deliver(&mut descs, len, 0, true).is_some() { expect.push_back(len as usize); }
            }
            // The NIC can never hold more than n-1.
            assert!(expect.len() <= n - 1);
            loop {
                match r.next() {
                    RxEvent::Empty => break,
                    RxEvent::Frame { idx, len } => {
                        assert_eq!(Some(len), expect.pop_front());
                        nic.rdt = r.release(idx) as usize;
                        got += 1;
                    }
                    e => panic!("unexpected {:?}", e),
                }
            }
            assert!(expect.is_empty());
            assert_eq!(r.filled(), 0);
            // RDT always trails the consumer by one.
            assert_eq!(nic.rdt, (r.cur + n - 1) % n);
        }
        assert!(got > 500);
    }

    #[test]
    fn rx_errors_and_fragments() {
        let n = 4;
        let mut descs = rx_alloc(n);
        let mut r = unsafe { RxRing::init(descs.as_mut_ptr(), n, 2048, &|_| 0) };
        let mut nic = SimRx { rdh: 0, rdt: r.initial_tail() as usize, n };
        nic.deliver(&mut descs, 100, 0x01, true); // CRC error
        nic.deliver(&mut descs, 2048, 0, false); // no EOP
        nic.deliver(&mut descs, 64, 0x20, true); // TCP checksum error only: still a frame
        assert!(matches!(r.next(), RxEvent::Error { idx: 0, errors: 0x01 }));
        nic.rdt = r.release(0) as usize;
        assert!(matches!(r.next(), RxEvent::Fragment { idx: 1, .. }));
        nic.rdt = r.release(1) as usize;
        assert_eq!(r.next(), RxEvent::Frame { idx: 2, len: 64 });
        nic.rdt = r.release(2) as usize;
        assert_eq!(r.next(), RxEvent::Empty);
        // reset: everything clean, back to 0
        nic.deliver(&mut descs, 70, 0, true);
        r.reset();
        assert_eq!((r.cur, r.filled()), (0, 0));
        assert!(descs.iter().all(|d| d.status == 0));
    }

    /// The NIC's transmit side: processes TDH..TDT-1, writes DD back.
    struct SimTx { tdh: usize, n: usize, sent: Vec<u16> }
    impl SimTx {
        fn run(&mut self, descs: &mut [TxDesc], tdt: usize, max: usize) {
            let mut k = 0;
            while self.tdh != tdt && k < max {
                let d = &mut descs[self.tdh];
                assert_eq!(d.lower & (TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS), TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS);
                assert_eq!(d.upper & TXD_STAT_DD, 0, "posted descriptor still carries an old DD");
                self.sent.push(d.lower as u16);
                d.upper |= TXD_STAT_DD;
                self.tdh = (self.tdh + 1) % self.n;
                k += 1;
            }
        }
    }

    #[test]
    fn tx_ring_fills_completes_and_wraps() {
        let n = 8;
        let mut descs = tx_alloc(n);
        let mut t = unsafe { TxRing::init(descs.as_mut_ptr(), n, &|i| 0x10_0000 + 2048 * i as u64) };
        let mut nic = SimTx { tdh: 0, n, sent: vec![] };
        let mut tdt = 0usize;
        let mut posted = vec![];
        for round in 0..100usize {
            // Fill until full.
            while let Some(idx) = t.slot() {
                let len = 60 + (posted.len() % 1000);
                tdt = t.post(idx, len) as usize;
                posted.push(len as u16);
                if posted.len() % 3 == round % 3 { break; }
            }
            assert!(t.in_flight() <= n - 1, "TDT may never catch up with TDH");
            nic.run(&mut descs, tdt, round % 5);
            t.reclaim();
            assert_eq!(t.in_flight(), (tdt + n - nic.tdh) % n);
        }
        nic.run(&mut descs, tdt, n);
        t.reclaim();
        assert_eq!(t.in_flight(), 0);
        assert_eq!(nic.sent, posted);
        assert_eq!(t.tail() as usize, tdt);
        t.reset();
        assert_eq!((t.cur, t.dirty), (0, 0));
        assert!(descs.iter().all(|d| d.lower == 0 && d.upper == 0));
    }

    #[test]
    fn descriptor_layout() {
        assert_eq!(core::mem::size_of::<RxDesc>(), 16);
        assert_eq!(core::mem::size_of::<TxDesc>(), 16);
        let d = RxDesc::default();
        let base = &d as *const _ as usize;
        assert_eq!(&d.len as *const _ as usize - base, 8);
        assert_eq!(&d.status as *const _ as usize - base, 12);
        assert_eq!(&d.errors as *const _ as usize - base, 13);
    }

    // ── PHY addressing ─────────────────────────────────────────────────────

    #[test]
    fn hv_phy_access() {
        // Page 0, low register: PHY address 2, no page select.
        assert_eq!(hv_access(1), Ok(HvAccess { page_select: None, addr: 2, reg: 1 }));
        // Page 0, register > 15 (I82577_CFG_REG 22): select page 0 first.
        assert_eq!(hv_access(22), Ok(HvAccess { page_select: Some(0), addr: 2, reg: 22 }));
        // CV_SMB_CTRL = PHY_REG(769, 23): page select 769 << 5 at address 1.
        assert_eq!(hv_access(phy_reg(769, 23)), Ok(HvAccess { page_select: Some(769 << 5), addr: 1, reg: 23 }));
        // I218_ULP_CONFIG1 = PHY_REG(779, 16).
        assert_eq!(hv_access(phy_reg(779, 16)), Ok(HvAccess { page_select: Some(779 << 5), addr: 1, reg: 16 }));
        // Page 768 is selected as page 0 but lives at address 1.
        assert_eq!(hv_access(phy_reg(768, 26)), Ok(HvAccess { page_select: Some(0), addr: 1, reg: 26 }));
        // Pages >= 768, register <= 15: no select needed.
        assert_eq!(hv_access(phy_reg(770, 5)), Ok(HvAccess { page_select: None, addr: 1, reg: 5 }));
        assert!(hv_access(phy_reg(800, 1)).is_err());
        assert!(hv_access(phy_reg(5, 17)).is_err());
        assert_eq!(phy_reg(776, 20), (776 << 5) | 20);
    }

    #[test]
    fn mdic_encoding() {
        assert_eq!(mdic_read_cmd(2, 1), 0x0841_0000);
        assert_eq!(mdic_write_cmd(1, 31, 0x6040), 0x043f_6040);
        assert_eq!(mdic_decode(0x0841_0000, 1), MdicResult::NotReady);
        assert_eq!(mdic_decode(0x1841_796d, 1), MdicResult::Ok(0x796d));
        assert_eq!(mdic_decode(0x5841_ffff, 1), MdicResult::Error);
        assert_eq!(mdic_decode(0x1842_0154, 1), MdicResult::WrongReg(2));
        assert!(phy_id_valid(0x0154, 0x00a3));
        assert!(!phy_id_valid(0xffff, 0x00a3));
        assert!(!phy_id_valid(0, 0));
        assert_eq!(phy_name(0x0154_00a3), "I217/I218/I219 (e1000_phy_i217)");
        assert_eq!(phy_name(0x0141_0cb1), "BME1000 (82574)");
    }

    // ── MAC / NVM ──────────────────────────────────────────────────────────

    #[test]
    fn mac_helpers() {
        let m = mac_from_ral(0x1234_5678, RAH_AV | 0x9abc);
        assert_eq!(m, [0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a]);
        assert_eq!(ral_rah_of(&m), (0x1234_5678, RAH_AV | 0x9abc));
        assert_eq!(ral_rah_of(&[0; 6]), (0, 0));
        assert!(mac_valid(&[0x52, 0x54, 0, 0x12, 0x34, 0x56]));
        assert!(!mac_valid(&[0; 6]));
        assert!(!mac_valid(&[0xff; 6]));
        assert!(!mac_valid(&[0x01, 0, 0x5e, 0, 0, 1]));
        assert_eq!(mac_from_nvm([0x5452, 0x1200, 0x5634]), [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    }

    #[test]
    fn nvm_helpers() {
        let mut w = vec![0u16; 64];
        w[0] = 0x5452; w[1] = 0x1200; w[2] = 0x5634; w[0x13] = 0x8040;
        let partial = nvm_checksum(&w);
        w[0x3f] = NVM_SUM.wrapping_sub(partial.wrapping_sub(w[0x3f]));
        assert_eq!(nvm_checksum(&w), NVM_SUM);
        w[5] ^= 1;
        assert_ne!(nvm_checksum(&w), NVM_SUM);
        // STRAP.NVMS (bits 5:1) in 4 KiB units, two banks, 16-bit words.
        assert_eq!(spt_flash_bank_words(0), 1024);
        assert_eq!(spt_flash_bank_words(7 << 1), 8192);
        assert_eq!(spt_flash_bank_words(0xffff_ffc1), 1024); // other bits ignored
        // Bank signature: word 0x13 bits 15:14 == 10b.
        assert!(nvm_sig_valid(0xabcd_8040));
        assert!(nvm_sig_valid(0x0000_bfff));
        assert!(!nvm_sig_valid(0x0000_c040));
        assert!(!nvm_sig_valid(0x0000_4000));
        assert!(!nvm_sig_valid(0xffff_ffff));
    }

    // ── link / flow control / LTR / FWSM ──────────────────────────────────

    #[test]
    fn status_decode() {
        assert_eq!(link_from_status(0x0008_0083), (true, 1000, true));
        assert_eq!(link_from_status(0x42), (true, 100, false));
        assert_eq!(link_from_status(0x03), (true, 10, true));
        assert_eq!(link_from_status(0x80), (false, 1000, false));
    }

    #[test]
    fn pause_resolution() {
        let both = ADV_PAUSE | ADV_ASYM;
        assert_eq!(fc_resolve(both, ADV_PAUSE), (true, true));
        assert_eq!(fc_resolve(both, both), (true, true));
        assert_eq!(fc_resolve(both, ADV_ASYM), (true, false));
        assert_eq!(fc_resolve(both, 0), (false, false));
        assert_eq!(fc_resolve(ADV_ASYM, both), (false, true));
        assert_eq!(fc_resolve(ADV_ASYM, ADV_PAUSE), (false, false));
        assert_eq!(fc_resolve(0, both), (false, false));
    }

    #[test]
    fn ltr() {
        // 26 KiB RX buffer, 1518-byte frames, 1 Gb/s: (26*512-1518)*16 =
        // 188704 ns -> scale 2 (1024 ns units) value 185.
        assert_eq!(ltr_encode(26, 1518, 1000, 0x1003, 0x1003), Some((2 << 10) | 185));
        // 100 Mb/s: 1887040 ns -> scale 3, value 58.
        assert_eq!(ltr_encode(26, 1518, 100, 0x1003, 0x1003), Some((3 << 10) | 58));
        // Platform tolerates less: capped at its maximum (larger of the two).
        assert_eq!(ltr_encode(26, 1518, 1000, 0x0801, 0x0802), Some(0x0802));
        // Platform reports nothing (0): capped to 0.
        assert_eq!(ltr_encode(26, 1518, 1000, 0, 0), Some(0));
        // Buffer smaller than a frame: 0.
        assert_eq!(ltr_encode(2, 1518, 1000, 0x1003, 0x1003), Some(0));
        assert_eq!(ltr_encode(26, 1518, 0, 0x1003, 0x1003), None);
        // 10 Mb/s: 11794*1600 = 18870400 ns -> scale 3 (32768 ns), value 576.
        assert_eq!(ltr_encode(26, 1518, 10, 0x1fff, 0x1fff), Some((3 << 10) | 576));
    }

    #[test]
    fn fwsm() {
        let f = fwsm_decode(0x0000_8044);
        assert!(f.fw_valid && f.phy_reset_allowed && !f.ulp_cfg_done);
        assert_eq!((f.mode, fwsm_mode_name(f.mode)), (2, "iAMT"));
        let g = fwsm_decode(0x0000_0400 | (3 << 7));
        assert!(!g.fw_valid && !g.phy_reset_allowed && g.ulp_cfg_done);
        assert_eq!(g.wlock_mac, 3);
        assert_eq!(fwsm_decode(0).mode, 0);
    }

    // ── interrupt protocol ─────────────────────────────────────────────────

    /// ICR latches causes; the interrupt condition is ICR & IMS; an MSI is
    /// sent on its rising edge. ICR is W1C here (and not cleared by reads,
    /// the worst case for the protocol).
    struct SimIrq { icr: u32, ims: u32, line: bool, msis: u32 }
    impl SimIrq {
        fn eval(&mut self) {
            let l = self.icr & self.ims != 0;
            if l && !self.line { self.msis += 1; }
            self.line = l;
        }
        fn event(&mut self, bits: u32) { self.icr |= bits; self.eval(); }
    }
    impl IcrRegs for SimIrq {
        fn icr(&mut self) -> u32 { self.icr }
        fn ack(&mut self, bits: u32) { self.icr &= !bits; self.eval(); }
        fn mask_all(&mut self) { self.ims = 0; self.eval(); }
        fn unmask(&mut self, bits: u32) { self.ims |= bits; self.eval(); }
    }

    /// Every interleaving of one extra RX event into the handler / daemon
    /// sequence: the frame is always noticed (an MSI is pending, or the
    /// daemon's after-unmask ring check sees it), and the interrupt never
    /// ends up stuck asserted with nobody to service it.
    #[test]
    fn irq_protocol_never_loses_an_event() {
        let en = ICR_RXT0 | ICR_LSC | ICR_RXO;
        for inject_at in 0..6 {
            let mut s = SimIrq { icr: 0, ims: en, line: false, msis: 0 };
            let mut ring = 0u32; // frames in the RX ring
            let mut handled_msis = 0;
            // first frame
            ring += 1;
            s.event(ICR_RXT0);
            assert_eq!(s.msis, 1);
            let mut step = 0;
            let mut maybe_inject = |s: &mut SimIrq, ring: &mut u32, step: &mut usize| {
                if *step == inject_at { *ring += 1; s.event(ICR_RXT0); }
                *step += 1;
            };
            // ISR
            handled_msis += 1;
            maybe_inject(&mut s, &mut ring, &mut step);
            let st = irq_service(&mut s);
            maybe_inject(&mut s, &mut ring, &mut step);
            assert!(st.is_some());
            assert_eq!(s.ims, 0);
            // daemon drains
            maybe_inject(&mut s, &mut ring, &mut step);
            ring = 0;
            maybe_inject(&mut s, &mut ring, &mut step);
            irq_rearm(&mut s, en);
            let poll_again = ring > 0; // the after-unmask ring check
            maybe_inject(&mut s, &mut ring, &mut step);
            // Outcome: if a frame is in the ring, either the daemon polls
            // again or an MSI was raised that nobody handled yet.
            let pending_msi = s.msis > handled_msis;
            if ring > 0 {
                assert!(poll_again || pending_msi, "inject_at {}: frame stranded (icr {:x} ims {:x})", inject_at, s.icr, s.ims);
            }
            // Service whatever is pending and check the line can fall again.
            if pending_msi {
                irq_service(&mut s);
                irq_rearm(&mut s, en);
            }
            if poll_again { irq_rearm(&mut s, en); }
            if s.line { assert!(s.icr & s.ims != 0); }
        }
    }

    #[test]
    fn irq_service_ignores_empty_and_gone() {
        let mut s = SimIrq { icr: 0, ims: 0xff, line: false, msis: 0 };
        assert_eq!(irq_service(&mut s), None);
        assert_eq!(s.ims, 0xff, "an empty look must not mask (nobody would unmask)");
        s.icr = !0;
        assert_eq!(irq_service(&mut s), None); // surprise-removed device reads all-ones
    }
}
