//! Host tests for the kernel's r8169 hardware-independent module. The module
//! under test is included verbatim, so these exercise exactly the code the
//! kernel runs, against a simulated NIC that owns/returns descriptors.

extern crate alloc;

#[path = "../../src/r8169_hw.rs"]
pub mod r8169_hw;

#[cfg(test)]
mod tests {
    use super::r8169_hw::*;

    #[test]
    fn identify_common_chips() {
        // RTL8168h: TxConfig 0x54100f00-style value -> XID 0x541.
        let tx = 0x5410_0f00u32;
        assert_eq!(xid_of(tx), 0x541);
        let c = identify(0x541, true);
        assert_eq!((c.ver, c.name), (46, "RTL8168h/8111h"));
        assert_eq!(c.fw, Some("rtl_nic/rtl8168h-2.fw"));
        assert_eq!(identify(0x541, false).ver, 48); // RTL8107e on a 10ec:8136
        assert_eq!(identify(0x4c0, true).ver, 40);
        assert_eq!(identify(0x509, true).ver, 42);
        assert_eq!(identify(0x509, false).name, "RTL8106eus");
        assert_eq!(identify(0x2c8, true).ver, 34);
        assert_eq!(identify(0x2c9, true).ver, 34); // 0x7c8 mask ignores low bits
        assert_eq!(identify(0x380, true).ver, 17);
        assert_eq!(identify(0x7c8, true).ver, VER_NONE);
        assert_eq!(identify(0x000, true).ver, VER_NONE);
        assert!(is_8168g_up(46) && !is_8168g_up(38));
        assert!(is_8168evl_up(46) && !is_8168evl_up(39) && !is_8168evl_up(33));
    }

    /// A simulated NIC: receives a frame into the next RX descriptor it owns.
    fn nic_rx(descs: &mut [Desc], next: &mut usize, len_with_fcs: u32, flags: u32) -> bool {
        let d = &mut descs[*next];
        if d.opts1 & DESC_OWN == 0 { return false; }
        let eor = d.opts1 & DESC_RING_END;
        d.opts1 = eor | flags | (len_with_fcs & RX_LEN_MASK);
        *next = if eor != 0 { 0 } else { *next + 1 };
        true
    }

    #[test]
    fn rx_ring_receive_wrap_and_errors() {
        const N: usize = 8;
        let mut descs = vec![Desc::default(); N];
        let mut ring = unsafe { RxRing::init(descs.as_mut_ptr(), N, 2048, &|i| 0x1000 + i as u64 * 2048) };
        for (i, d) in descs.iter().enumerate() {
            assert_eq!(d.addr, 0x1000 + i as u64 * 2048);
            assert_eq!(d.opts1 & DESC_OWN, DESC_OWN);
            assert_eq!(d.opts1 & 0x3fff, 2048);
            assert_eq!(d.opts1 & DESC_RING_END != 0, i == N - 1);
        }
        assert_eq!(ring.next(), RxEvent::Empty);
        let mut nic = 0usize;
        // Three rounds through the ring: wrap-around must keep working.
        for k in 0..3 * N {
            assert!(nic_rx(&mut descs, &mut nic, 64 + 4 + k as u32, DESC_FIRST | DESC_LAST));
            match ring.next() {
                RxEvent::Frame { idx, len } => { assert_eq!(idx, k % N); assert_eq!(len, 64 + k); ring.release(idx); }
                e => panic!("unexpected {:?}", e),
            }
            assert_eq!(ring.next(), RxEvent::Empty);
        }
        // The ring fills up when the host is slow: the NIC stops at the first
        // descriptor it does not own.
        assert_eq!(ring.filled(), 0);
        for _ in 0..N { assert!(nic_rx(&mut descs, &mut nic, 100, DESC_FIRST | DESC_LAST)); }
        assert!(!nic_rx(&mut descs, &mut nic, 100, DESC_FIRST | DESC_LAST));
        assert_eq!(ring.filled(), N);
        for _ in 0..N {
            match ring.next() { RxEvent::Frame { idx, len: 96 } => ring.release(idx), e => panic!("{:?}", e) }
        }
        assert_eq!(ring.next(), RxEvent::Empty);
        // Error and fragment descriptors are reported and recycled.
        assert!(nic_rx(&mut descs, &mut nic, 100, DESC_FIRST | DESC_LAST | RX_RES | RX_CRC));
        assert!(nic_rx(&mut descs, &mut nic, 2048, DESC_FIRST));
        assert!(nic_rx(&mut descs, &mut nic, 3000 - 2048, DESC_LAST));
        assert!(nic_rx(&mut descs, &mut nic, 2, DESC_FIRST | DESC_LAST)); // shorter than the FCS
        let mut kinds = Vec::new();
        loop {
            match ring.next() {
                RxEvent::Empty => break,
                RxEvent::Error { idx, .. } => { kinds.push('E'); ring.release(idx); }
                RxEvent::Fragment { idx, .. } => { kinds.push('F'); ring.release(idx); }
                RxEvent::Frame { idx, .. } => { kinds.push('P'); ring.release(idx); }
            }
        }
        assert_eq!(kinds, vec!['E', 'F', 'F', 'E']);
        // reset() gives everything back and restarts at 0.
        ring.reset();
        assert_eq!(ring.cur, 0);
        assert!(descs.iter().all(|d| d.opts1 & DESC_OWN != 0));
    }

    #[test]
    fn tx_ring_post_reclaim_full() {
        const N: usize = 8;
        let mut descs = vec![Desc::default(); N];
        let mut ring = unsafe { TxRing::init(descs.as_mut_ptr(), N, &|i| 0x9000 + i as u64 * 2048) };
        assert_eq!(descs[N - 1].opts1, DESC_RING_END);
        assert_eq!(descs[3].addr, 0x9000 + 3 * 2048);
        // Fill: one slot always stays free.
        for k in 0..N - 1 {
            let idx = ring.slot().expect("slot");
            assert_eq!(idx, k);
            ring.post(idx, 60 + k);
            let o = descs[idx].opts1;
            assert_eq!(o & (DESC_OWN | DESC_FIRST | DESC_LAST), DESC_OWN | DESC_FIRST | DESC_LAST);
            assert_eq!((o & 0x3fff) as usize, 60 + k);
        }
        assert_eq!(ring.slot(), None);
        assert_eq!(ring.reclaim(), 0);
        // The NIC completes the first three.
        for d in descs.iter_mut().take(3) { d.opts1 &= !DESC_OWN; }
        assert_eq!(ring.reclaim(), 3);
        assert_eq!(ring.in_flight(), N - 4);
        // Wrap: the last descriptor keeps RING_END when posted.
        let idx = ring.slot().unwrap();
        assert_eq!(idx, N - 1);
        ring.post(idx, 100);
        assert!(descs[N - 1].opts1 & DESC_RING_END != 0);
        assert_eq!(ring.slot(), Some(0));
        for d in descs.iter_mut() { d.opts1 &= !DESC_OWN; }
        assert_eq!(ring.reclaim(), N - 3);
        assert_eq!(ring.in_flight(), 0);
        // Long run of post/complete cycles.
        for k in 0..10 * N {
            let idx = ring.slot().unwrap();
            assert_eq!(idx, (ring.cur % N as u64) as usize);
            ring.post(idx, 64);
            descs[idx].opts1 &= !DESC_OWN;
            assert_eq!(ring.reclaim(), 1, "cycle {}", k);
        }
        ring.reset();
        assert_eq!((ring.cur, ring.dirty), (0, 0));
        assert_eq!(descs[N - 1].opts1, DESC_RING_END);
        assert_eq!(descs[0].opts1, 0);
    }

    fn act(op: u32, reg: u32, data: u32) -> u32 { (op << 28) | ((reg & 0xfff) << 16) | (data & 0xffff) }

    fn with_header(code: &[u32], version: &str) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&0u32.to_le_bytes());
        let mut v = [0u8; 32];
        v[..version.len()].copy_from_slice(version.as_bytes());
        f.extend_from_slice(&v);
        let start = 48u32;
        f.extend_from_slice(&start.to_le_bytes());
        f.extend_from_slice(&(code.len() as u32).to_le_bytes());
        f.push(0); // checksum byte, fixed below
        while f.len() < start as usize { f.push(0); }
        for c in code { f.extend_from_slice(&c.to_le_bytes()); }
        let sum = f.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        f[44] = 0u8.wrapping_sub(sum);
        f
    }

    #[derive(Default)]
    struct Mock { phy: std::collections::HashMap<u32, u32>, mcu: std::collections::HashMap<u32, u32>, log: Vec<String>, slept: u32 }
    impl FwTarget for Mock {
        fn phy_write(&mut self, r: u32, v: u32) { self.log.push(format!("pw {:x}={:x}", r, v)); self.phy.insert(r, v); }
        fn phy_read(&mut self, r: u32) -> u32 { *self.phy.get(&r).unwrap_or(&0) }
        fn mcu_write(&mut self, r: u32, v: u32) { self.log.push(format!("mw {:x}={:x}", r, v)); self.mcu.insert(r, v); }
        fn mcu_read(&mut self, r: u32) -> u32 { *self.mcu.get(&r).unwrap_or(&0) }
        fn delay_ms(&mut self, ms: u32) { self.slept += ms; }
    }

    #[test]
    fn firmware_parse_and_run() {
        let code = vec![
            act(FW_PHY_WRITE, 0x1f, 0x0b82),
            act(FW_PHY_READ, 0x10, 0),
            act(FW_PHY_DATA_OR, 0, 0x0010),
            act(FW_PHY_DATA_AND, 0, 0xfff0 | 0x0010),
            act(FW_PHY_WRITE_PREVIOUS, 0x10, 0),
            act(FW_PHY_COMP_EQ_SKIPN, 1, 0x0010), // predata == 0x10 -> skip next
            act(FW_PHY_WRITE, 0x11, 0xdead),
            act(FW_PHY_MDIO_CHG, 0, 1),
            act(FW_PHY_WRITE, 0x05, 0x1234),
            act(FW_PHY_MDIO_CHG, 0, 0),
            act(FW_PHY_DELAY_MS, 0, 7),
            act(FW_PHY_SKIPN, 1, 0),
            act(FW_PHY_WRITE, 0x12, 0xbad),
            act(FW_PHY_WRITE, 0x1f, 0),
        ];
        let file = with_header(&code, "rtl8168h-2_0.0.2 02/26/15");
        let p = fw_parse(&file, "x").expect("parse");
        assert_eq!(p.version_str(), "rtl8168h-2_0.0.2 02/26/15");
        assert_eq!(p.code, code);
        let mut m = Mock::default();
        assert!(fw_run(&p.code, &mut m, 1000));
        assert_eq!(m.log, vec!["pw 1f=b82", "pw 10=10", "mw 5=1234", "pw 1f=0"]);
        assert_eq!(m.slept, 7);

        // Headerless file: the name stands in for the version.
        let mut raw = Vec::new();
        for c in &code { raw.extend_from_slice(&c.to_le_bytes()); }
        let p = fw_parse(&raw, "rtl_nic/rtl8168g-2.fw").unwrap();
        assert_eq!(p.version_str(), "rtl_nic/rtl8168g-2.fw");

        // Corruption is refused.
        let mut bad = file.clone();
        bad[60] ^= 1;
        assert_eq!(fw_parse(&bad, "x").err(), Some("checksum mismatch"));
        assert!(fw_parse(&raw[..raw.len() - 1], "x").is_err());
        assert_eq!(fw_validate(&[act(0x5, 0, 0)]), Err("unknown opcode"));
        assert_eq!(fw_validate(&[act(FW_PHY_SKIPN, 3, 0), 0]), Err("skip out of range"));
        assert_eq!(fw_validate(&[act(FW_PHY_BJMPN, 1, 0)]), Err("backward jump out of range"));
        assert_eq!(fw_validate(&[act(FW_PHY_MDIO_CHG, 0, 2)]), Err("bad MDIO_CHG"));
    }

    #[test]
    fn firmware_loop_until_bit_and_step_bound() {
        // Poll reg 0x10 until bit 0 is set (BJMPN back to the READ).
        let code = vec![
            act(FW_PHY_READ, 0x10, 0),
            act(FW_PHY_DATA_AND, 0, 0x0001),
            act(FW_PHY_COMP_NEQ_SKIPN, 1, 0x0001), // not set -> skip the exit jump
            act(FW_PHY_SKIPN, 1, 0),               // set: skip over the loop jump
            act(FW_PHY_BJMPN, 4, 0),               // back to index 0
            act(FW_PHY_WRITE, 0x11, 0x1),
        ];
        fw_validate(&code).unwrap();
        struct Flip { reads: u32 }
        impl FwTarget for Flip {
            fn phy_write(&mut self, _: u32, _: u32) {}
            fn phy_read(&mut self, _: u32) -> u32 { self.reads += 1; if self.reads >= 5 { 1 } else { 0 } }
            fn mcu_write(&mut self, _: u32, _: u32) {}
            fn mcu_read(&mut self, _: u32) -> u32 { 0 }
            fn delay_ms(&mut self, _: u32) {}
        }
        let mut f = Flip { reads: 0 };
        assert!(fw_run(&code, &mut f, 1000));
        assert_eq!(f.reads, 5);
        // A bit that never sets: the bound stops it.
        struct Never;
        impl FwTarget for Never {
            fn phy_write(&mut self, _: u32, _: u32) {}
            fn phy_read(&mut self, _: u32) -> u32 { 0 }
            fn mcu_write(&mut self, _: u32, _: u32) {}
            fn mcu_read(&mut self, _: u32) -> u32 { 0 }
            fn delay_ms(&mut self, _: u32) {}
        }
        assert!(!fw_run(&code, &mut Never, 500));
    }

    /// The real linux-firmware files, when ports/rtl_nic/fetch.sh has fetched
    /// them: every one must parse and validate, and run to completion against
    /// a PHY whose registers read back what was written.
    #[test]
    fn real_firmware_files_parse() {
        let dir = std::env::var("RTL_NIC_CACHE").unwrap_or_else(|_| {
            format!("{}/.cache/leandros/rtl_nic", std::env::var("HOME").unwrap_or_default())
        });
        let Ok(rd) = std::fs::read_dir(&dir) else { eprintln!("no {} (skipped)", dir); return };
        let mut n = 0;
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("fw") { continue; }
            let data = std::fs::read(&p).unwrap();
            let name = p.file_name().unwrap().to_str().unwrap();
            let prog = fw_parse(&data, name).unwrap_or_else(|e| panic!("{}: {}", name, e));
            let mut m = Mock::default();
            // Loops that wait on a PHY bit may not terminate against a mock;
            // only the bound must hold.
            let _ = fw_run(&prog.code, &mut m, 200_000);
            eprintln!("{}: version '{}', {} actions, {} writes", name, prog.version_str(), prog.code.len(), m.log.len());
            n += 1;
        }
        eprintln!("{} firmware files parsed", n);
    }

    #[test]
    fn phystatus_and_mac() {
        assert_eq!(decode_phystatus(0x13), (true, 1000, true));
        assert_eq!(decode_phystatus(0x0b), (true, 100, true));
        assert_eq!(decode_phystatus(0x06), (true, 10, false));
        assert_eq!(decode_phystatus(0x00), (false, 0, false));
        assert!(mac_valid(&[0x00, 0xe0, 0x4c, 0x68, 0x00, 0x01]));
        assert!(!mac_valid(&[0; 6]));
        assert!(!mac_valid(&[0xff; 6]));
    }
}

/// The interrupt status/mask protocol against a model of the chip's
/// interrupt logic: the interrupt condition is `status & mask != 0` and an
/// MSI is sent on its rising edge only. Events can land between any two
/// register accesses, and the handler can run between any two of the poll
/// side's accesses (it is on another CPU).
#[cfg(test)]
mod irq_model {
    use super::r8169_hw::*;

    const RX_OK: u16 = 0x0001;
    const RX_ERR: u16 = 0x0002;
    const TX_OK: u16 = 0x0004;
    const TX_ERR: u16 = 0x0008;
    const RDU: u16 = 0x0010;
    const LINK: u16 = 0x0020;
    const FIFO: u16 = 0x0040;
    const TDU: u16 = 0x0080;
    /// What the driver enables on 8168-class parts now.
    const EN: u16 = RX_OK | RX_ERR | TX_ERR | LINK;
    /// What it enabled before (RDU and FIFO overflow included).
    const OLD_EN_NORX: u16 = LINK | RX_ERR | TX_ERR | RDU | FIFO | 0x8000;
    const OLD_EN: u16 = OLD_EN_NORX | RX_OK;

    #[derive(Clone, Copy, PartialEq)]
    enum Proto { Old, New }

    struct Chip {
        proto: Proto,
        en: u16,
        status: u16,
        mask: u16,
        line: bool,
        msis: u64,
        serviced: u64,
        kicked: bool,
        in_isr: bool,
        rng: u64,
        /// Per-access chance (out of 256) of an event / of the handler
        /// running in between.
        p_event: u64,
        p_isr: u64,
        events_on: bool,
        /// Scripted injection: after access number `.0`, raise `.1`.
        script: Option<(u64, u16)>,
        accesses: u64,
    }

    impl Chip {
        fn new(proto: Proto, seed: u64) -> Chip {
            let en = if proto == Proto::New { EN } else { OLD_EN };
            let mut c = Chip { proto, en, status: 0, mask: 0, line: false, msis: 0, serviced: 0, kicked: false,
                               in_isr: false, rng: seed | 1, p_event: 40, p_isr: 60, events_on: true,
                               script: None, accesses: 0 };
            c.mask = en;
            c
        }
        fn rand(&mut self) -> u64 {
            self.rng ^= self.rng << 13; self.rng ^= self.rng >> 7; self.rng ^= self.rng << 17;
            self.rng
        }
        fn eval(&mut self) {
            let l = self.status & self.mask != 0;
            if l && !self.line { self.msis += 1; }
            self.line = l;
        }
        fn raise(&mut self, bits: u16) { self.status |= bits; self.eval(); }
        fn random_event(&mut self) {
            const EVS: [u16; 8] = [RX_OK, RX_OK, RX_OK, RDU | FIFO, FIFO, TX_OK | TDU, LINK, RX_ERR];
            let e = EVS[(self.rand() % EVS.len() as u64) as usize];
            self.raise(e);
        }
        fn after_access(&mut self) {
            self.accesses += 1;
            if let Some((n, bits)) = self.script {
                if self.accesses == n { self.script = None; self.raise(bits); }
            }
            if self.events_on && self.rand() % 256 < self.p_event { self.random_event(); }
            if !self.in_isr && self.msis > self.serviced && self.rand() % 256 < self.p_isr { self.isr(); }
        }
        /// The MSI handler (runs when an MSI is outstanding).
        fn isr(&mut self) {
            self.in_isr = true;
            self.serviced = self.msis;
            match self.proto {
                Proto::New => { if irq_service(self).is_some() { self.kicked = true; } }
                Proto::Old => {
                    let st = self.status();
                    if st != 0 && st != 0xffff {
                        let rx = st & (RX_OK | RDU | FIFO) != 0;
                        if rx { self.set_mask(OLD_EN_NORX); }
                        self.ack(st);
                        if rx { self.kicked = true; }
                    }
                }
            }
            self.in_isr = false;
        }
        /// One poll pass's end: re-arm until armed.
        fn daemon(&mut self) {
            self.kicked = false;
            for _ in 0..1000 {
                match self.proto {
                    Proto::New => match irq_rearm(self, EN) {
                        Rearm::Pending { .. } => continue,
                        Rearm::Armed { .. } => return,
                    },
                    Proto::Old => { self.ack(RX_OK); self.set_mask(OLD_EN); return; }
                }
            }
            panic!("re-arm never settled");
        }
        /// Run handler and poller until nothing is outstanding. False if the
        /// interrupt is wedged: an enabled event latched, the mask enabled,
        /// and no MSI on its way.
        fn settle(&mut self) -> bool {
            self.events_on = false;
            for _ in 0..1000 {
                if self.msis > self.serviced { self.isr(); continue; }
                if self.kicked { self.daemon(); continue; }
                break;
            }
            !(self.status & self.mask != 0 && self.msis == self.serviced)
        }
    }

    impl IrqRegs for Chip {
        fn status(&mut self) -> u16 { let s = self.status; self.after_access(); s }
        fn set_mask(&mut self, m: u16) { self.mask = m; self.eval(); self.after_access(); }
        fn ack(&mut self, b: u16) { self.status &= !b; self.eval(); self.after_access(); }
    }

    /// The boot16 sequence under the old protocol: RxDescUnavail raises the
    /// MSI, RxFIFOOver lands right after the handler read IntrStatus, the
    /// handler acks only what it read and leaves FIFOOver enabled: the
    /// condition never drops, and no later event (RxOK included) makes an
    /// edge again.
    #[test]
    fn old_protocol_wedges_on_fifo_after_rdu() {
        let mut c = Chip::new(Proto::Old, 1);
        c.p_event = 0; c.p_isr = 0;
        c.raise(RDU);
        assert_eq!(c.msis, 1);
        c.script = Some((c.accesses + 1, FIFO)); // right after the status read
        c.isr();
        c.daemon();
        assert!(c.line, "condition still asserted");
        let before = c.msis;
        c.raise(RX_OK);
        c.raise(LINK);
        assert_eq!(c.msis, before, "no edge: the interrupt is dead");
        assert_eq!(c.status & FIFO, FIFO, "the late FIFOOver is still latched");
    }

    #[test]
    fn new_protocol_survives_the_same_sequence() {
        let mut c = Chip::new(Proto::New, 1);
        c.p_event = 0; c.p_isr = 0;
        c.raise(RX_OK);
        assert_eq!(c.msis, 1);
        // A late link change + FIFOOver (a late RxOK would merge into the
        // RxOK bit already read and be acked with it: harmless, the poller
        // looks at the ring anyway).
        c.script = Some((c.accesses + 1, LINK | FIFO));
        c.isr();
        assert!(!c.line, "handler masks everything: the condition drops");
        assert!(c.kicked);
        // Re-arm: the late link change is pending -> polled again; then armed, and
        // FIFOOver (outside the mask) acked rather than left latched.
        assert!(matches!(irq_rearm(&mut c, EN), Rearm::Pending { .. }));
        assert_eq!(irq_rearm(&mut c, EN), Rearm::Armed { stale: 0 });
        assert_eq!(c.status, 0);
        let before = c.msis;
        c.raise(RX_OK);
        assert_eq!(c.msis, before + 1, "next frame interrupts again");
    }

    #[test]
    fn rearm_acks_latched_bits_outside_the_mask() {
        let mut c = Chip::new(Proto::New, 1);
        c.p_event = 0; c.p_isr = 0;
        c.raise(RDU | FIFO | TX_OK | TDU);
        assert_eq!(c.msis, 0, "none of these is enabled");
        assert_eq!(irq_rearm(&mut c, EN), Rearm::Armed { stale: 0xd4 });
        assert_eq!(c.status, 0);
        assert_eq!(c.mask, EN);
    }

    #[test]
    fn handler_ignores_nothing_and_all_ones() {
        let mut c = Chip::new(Proto::New, 1);
        c.p_event = 0; c.p_isr = 0;
        assert_eq!(irq_service(&mut c), None);
        assert_eq!(c.mask, EN, "an empty look leaves the mask alone");
        c.status = 0xffff;
        assert_eq!(irq_service(&mut c), None);
    }

    /// Random events and handler runs at every access boundary: the new
    /// protocol never wedges; the old one does.
    #[test]
    fn random_interleavings() {
        let mut old_wedged = 0;
        for seed in 1..2000u64 {
            for proto in [Proto::New, Proto::Old] {
                let mut c = Chip::new(proto, seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
                for _ in 0..300 {
                    let r = c.rand() % 4;
                    if r == 0 { c.random_event(); }
                    if c.msis > c.serviced && r < 3 { c.isr(); }
                    if c.kicked && r != 1 { c.daemon(); }
                }
                let ok = c.settle();
                match proto {
                    Proto::New => {
                        assert!(ok, "seed {}: wedged: status {:04x} mask {:04x}", seed, c.status, c.mask);
                        assert_eq!(c.mask, EN, "seed {}: left masked", seed);
                        assert_eq!(c.status & EN, 0, "seed {}: enabled bits latched", seed);
                    }
                    Proto::Old => if !ok { old_wedged += 1; },
                }
            }
        }
        assert!(old_wedged > 0, "the model should reproduce the old wedge");
        eprintln!("old protocol wedged in {} of 1999 runs", old_wedged);
    }
}
