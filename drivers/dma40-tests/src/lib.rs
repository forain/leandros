//! Host tests for the dma40 driver's pure half. `cb.rs` is included verbatim, so these
//! exercise exactly the code the kernel runs, against a scripted register file.

#[path = "../../src/bcm2712_dma40/cb.rs"]
pub mod cb;

#[cfg(test)]
mod tests {
    use super::cb::*;
    use std::cell::RefCell;

    #[test]
    fn scb_is_32_bytes_aligned() {
        assert_eq!(core::mem::size_of::<Scb>(), 32);
        assert_eq!(core::mem::align_of::<Scb>(), 32);
    }

    #[test]
    fn cb_addr_shifts_and_checks() {
        assert_eq!(cb_addr(0x1_2345_6780), Ok(0x091a_2b3c));
        assert_eq!(cb_addr(0x40), Ok(2));
        assert_eq!(cb_addr(0x44), Err(Dma40Error::Misaligned));
        assert_eq!(cb_addr(0), Err(Dma40Error::AddrRange));
        assert_eq!(cb_addr(CB_ADDR_LIMIT), Err(Dma40Error::AddrRange));
        assert_eq!(cb_addr(CB_ADDR_LIMIT - 32), Ok(u32::MAX));
    }

    #[test]
    fn encode_splits_40_bit_addresses() {
        let s = encode(TI_WAIT_RESP, 0xab_1234_5678, XI_INC, 0x10_7c72_001c, 0, 256, 7).unwrap();
        assert_eq!((s.src, s.srci), (0x1234_5678, 0xab | XI_INC));
        assert_eq!((s.dst, s.dsti), (0x7c72_001c, 0x10));
        assert_eq!(s.src_addr(), 0xab_1234_5678);
        assert_eq!(s.dst_addr(), 0x10_7c72_001c);
        assert_eq!((s.len, s.next_cb, s.rsvd), (256, 7, 0));
        // Flags never leak into the address byte.
        let s = encode(0, 0x1000, 0xff | XI_INC, 0x2000, 0xff, 4, 0).unwrap();
        assert_eq!(s.srci, XI_INC);
        assert_eq!(s.dsti, 0);
        assert_eq!(encode(0, ADDR_LIMIT, 0, 0, 0, 4, 0), Err(Dma40Error::AddrRange));
        assert_eq!(encode(0, ADDR_LIMIT - 4, 0, 0, 0, 8, 0), Err(Dma40Error::AddrRange));
        assert_eq!(encode(0, 0, 0, 0, 0, 0, 0), Err(Dma40Error::BadLength));
    }

    #[test]
    fn hdmi_dreq_cells_decode_like_linux() {
        // 0x41fa000a (DTB): BURST(30) | WIDE_SOURCE(24) | QOS 0xa | PANIC_QOS 0xf | DREQ 10.
        let d = DreqCfg::from_cell(DREQ_CELL_HDMI0);
        assert_eq!(d.per_map, 10);
        assert_eq!(d.cs_flags, cs_qos(0xa) | cs_panic_qos(0xf));
        assert_eq!(d.ti, TI_WAIT_RESP | (10 << 9));
        assert_eq!(d.src_xi, XI_SIZE_128 | (3 << 8));
        assert_eq!(d.dst_xi, 3 << 8);
        assert_eq!(DreqCfg::from_cell(DREQ_CELL_HDMI1).per_map, 17);
        // NO_WAIT_RESP drops WAIT_RESP; a plain cell has no burst/width.
        let p = DreqCfg::from_cell((1 << 27) | 5);
        assert_eq!((p.ti, p.src_xi, p.dst_xi, p.cs_flags), (5 << 9, 0, 0, 0));
    }

    #[test]
    fn memcpy_single_and_split() {
        let mut t = [Scb::default(); 4];
        let n = build_memcpy(&mut t, 0x8000, 0x2_0000_0000, 0x1000_0000, 65536, true).unwrap();
        assert_eq!(n, 1);
        assert_eq!(t[0].ti, TI_WAIT_RESP | TI_INTEN);
        assert_eq!((t[0].src_addr(), t[0].dst_addr(), t[0].len, t[0].next_cb), (0x1000_0000, 0x2_0000_0000, 65536, 0));
        assert_eq!(t[0].srci & !0xff, XI_MEMCPY);
        assert_eq!(t[0].dsti, 0x2 | XI_MEMCPY);
        // 2.5 GiB -> 3 SCBs chained, INTEN only on the last.
        let len = MAX_CB_LEN * 2 + MAX_CB_LEN / 2;
        let n = build_memcpy(&mut t, 0x8000, 0x80_0000_0000, 0, len, true).unwrap();
        assert_eq!(n, 3);
        assert_eq!(t[0].next_cb, (0x8020 >> 5) as u32);
        assert_eq!(t[1].next_cb, (0x8040 >> 5) as u32);
        assert_eq!(t[2].next_cb, 0);
        assert_eq!(t[2].len as usize, MAX_CB_LEN / 2);
        assert_eq!(t[1].src_addr(), MAX_CB_LEN as u64);
        assert_eq!(t[2].dst_addr(), 0x80_0000_0000 + 2 * MAX_CB_LEN as u64);
        assert_eq!((t[0].ti & TI_INTEN, t[1].ti & TI_INTEN, t[2].ti & TI_INTEN), (0, 0, TI_INTEN));
        assert_eq!(build_memcpy(&mut t[..2], 0x8000, 0, 0, len, false), Err(Dma40Error::TooManyCbs));
        assert_eq!(build_memcpy(&mut t, 0x8010, 0, 0x100, 64, false), Err(Dma40Error::Misaligned));
    }

    #[test]
    fn cyclic_ring_loops_and_paces() {
        let d = DreqCfg::from_cell(DREQ_CELL_HDMI0);
        let mut t = [Scb::default(); 128];
        // HDMI audio: 128 KiB ring, 8 KiB periods, MAI_DATA at hd + 0x1c.
        let ring = 0x3_0000_0000u64;
        let n = build_cyclic_tx(&mut t, 0x10_0000, ring, 131072, 8192, 0x10_7c72_001c, &d, true).unwrap();
        assert_eq!(n, 16);
        for i in 0..n {
            let s = t[i];
            assert_eq!(s.src_addr(), ring + 8192 * i as u64);
            assert_eq!(s.dst_addr(), 0x10_7c72_001c, "fixed FIFO address");
            assert_eq!(s.len, 8192);
            assert_eq!(s.ti, TI_WAIT_RESP | TI_D_DREQ | TI_INTEN | ti_per_map(10));
            assert_eq!(s.srci & !0xff, XI_INC | XI_SIZE_128 | xi_burst_len(3));
            assert_eq!(s.dsti & !0xff & XI_INC, 0, "destination must not increment");
            let next = if i + 1 == n { 0x10_0000 } else { 0x10_0000 + 32 * (i as u64 + 1) };
            assert_eq!(s.next_cb, (next >> 5) as u32);
        }
        // Following next_cb from the head returns to it after exactly n hops.
        let mut cur = t[0].next_cb;
        let mut hops = 1;
        while cur != (0x10_0000u64 >> 5) as u32 { cur = t[(cur as usize * 32 - 0x10_0000) / 32].next_cb; hops += 1; }
        assert_eq!(hops, n);
        let n = build_cyclic_tx(&mut t, 0x10_0000, ring, 65536, 65536, 0x7c72_001c, &d, false).unwrap();
        assert_eq!((n, t[0].next_cb, t[0].ti & TI_INTEN), (1, 0x10_0000 >> 5, 0));
        assert_eq!(build_cyclic_tx(&mut t, 0x10_0000, ring, 65536, 3000, 0, &d, false), Err(Dma40Error::BadLength));
        assert_eq!(build_cyclic_tx(&mut t, 0x10_0000, ring, 65536, 6, 0, &d, false), Err(Dma40Error::BadLength));
        assert_eq!(build_cyclic_tx(&mut t[..4], 0x10_0000, ring, 65536, 4096, 0, &d, false), Err(Dma40Error::TooManyCbs));
        assert_eq!(build_cyclic_tx(&mut t, 0x10_0000, ring, 65536, 4096, 0x7c72_001e, &d, false), Err(Dma40Error::Misaligned));
    }

    #[test]
    fn ring_position_math() {
        let ring = 0x3_0000_1000u64;
        assert_eq!(ring_offset(0x0000_1000, 0x03 | XI_INC, ring, 4096), Some(0));
        assert_eq!(ring_offset(0x0000_1ffc, 0x03, ring, 4096), Some(4092));
        assert_eq!(ring_offset(0x0000_2000, 0x03, ring, 4096), Some(0), "end wraps to start");
        assert_eq!(ring_offset(0x0000_1000, 0x02, ring, 4096), None, "high byte matters");
        assert_eq!(ring_offset(0, 0, ring, 4096), None);
        assert_eq!(ring_advance(100, 300, 4096), 200);
        assert_eq!(ring_advance(4000, 96, 4096), 192);
        assert_eq!(ring_advance(5, 5, 4096), 0);
    }

    #[test]
    fn channel_pick_and_interrupts() {
        assert_eq!(pick_channel(DT_CHANNEL_MASK, 0, Some(AUDIO_CHAN)), Some(10));
        assert_eq!(pick_channel(DT_CHANNEL_MASK, 1 << 10, Some(10)), Some(9));
        assert_eq!(pick_channel(DT_CHANNEL_MASK, 0, None), Some(10));
        assert_eq!(pick_channel(DT_CHANNEL_MASK, 0, Some(11)), Some(10), "11 is not in the mask");
        assert_eq!(pick_channel(DT_CHANNEL_MASK, 0, Some(3)), Some(10), "dma32 channels are not ours");
        assert_eq!(pick_channel(DT_CHANNEL_MASK, 0x7c0, None), None);
        assert_eq!(pick_channel(0xffff_ffff, 0, None), Some(11));
        assert_eq!(intid(6), Some(32 + 0x56));
        assert_eq!(intid(10), Some(32 + 0x5a));
        assert_eq!(intid(5), None);
        assert_eq!(chan_window_off(6), Some(0));
        assert_eq!(chan_window_off(10), Some(0x400));
        assert_eq!(BLOCK_PHYS + 6 * CHAN_SIZE, 0x10_0001_0600);
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Ev { R(u32, u32), W(u32, u32), D }

    struct Mock {
        regs: RefCell<[u32; 16]>,
        log: RefCell<Vec<Ev>>,
        /// CS reads with TRANSACTIONS still set before it drops.
        busy_reads: RefCell<u32>,
        /// Polls before the chain completes (CB -> 0, END set).
        done_after: RefCell<u32>,
    }
    impl Mock {
        fn new() -> Mock {
            Mock { regs: RefCell::new([0; 16]), log: RefCell::new(vec![]), busy_reads: RefCell::new(0), done_after: RefCell::new(0) }
        }
        fn set(&self, off: u32, v: u32) { self.regs.borrow_mut()[(off / 4) as usize] = v; }
        fn writes(&self) -> Vec<(u32, u32)> {
            self.log.borrow().iter().filter_map(|e| if let Ev::W(o, v) = e { Some((*o, *v)) } else { None }).collect()
        }
    }
    impl ChanRegs for Mock {
        fn r(&self, off: u32) -> u32 {
            let mut v = self.regs.borrow()[(off / 4) as usize];
            if off == CS {
                let mut b = self.busy_reads.borrow_mut();
                // Outstanding transactions drain only once the channel is paused.
                if *b > 0 && v & CS_ACTIVE == 0 { *b -= 1; v |= CS_TRANSACTIONS; }
            }
            self.log.borrow_mut().push(Ev::R(off, v));
            v
        }
        fn w(&self, off: u32, v: u32) {
            self.log.borrow_mut().push(Ev::W(off, v));
            self.regs.borrow_mut()[(off / 4) as usize] = v;
        }
        fn delay_us(&self, _: u32) {
            self.log.borrow_mut().push(Ev::D);
            let mut d = self.done_after.borrow_mut();
            if *d > 0 {
                *d -= 1;
                if *d == 0 { self.set(CB, 0); let cs = self.regs.borrow()[0]; self.set(CS, cs | CS_END); }
            }
        }
    }

    #[test]
    fn abort_sequence_matches_linux() {
        let m = Mock::new();
        m.set(CB, 0x400);
        m.set(CS, cs_start(CS_MEMCPY_FLAGS));
        *m.busy_reads.borrow_mut() = 3;
        assert!(abort(&m));
        let w = m.writes();
        assert_eq!(w[0], (CS, CS_PROT | CS_MEMCPY_FLAGS), "pause: ACTIVE cleared, rest kept");
        assert_eq!(w[1], (CS, CS_PROT));
        assert_eq!(w[2], (DEBUG, DEBUG_RESET));
        assert_eq!(w.len(), 3);
        // Idle channel: nothing written.
        let m = Mock::new();
        assert!(abort(&m));
        assert!(m.writes().is_empty());
        // Stuck transactions: bounded, still reset, reports false.
        let m = Mock::new();
        m.set(CB, 0x400);
        *m.busy_reads.borrow_mut() = 1000;
        assert!(!abort(&m));
        assert_eq!(m.writes().last(), Some(&(DEBUG, DEBUG_RESET)));
    }

    #[test]
    fn start_then_wait_done() {
        let m = Mock::new();
        start(&m, 0x8000, CS_MEMCPY_FLAGS).unwrap();
        assert_eq!(m.writes(), vec![(CB, 0x8000 >> 5), (CS, CS_ACTIVE | CS_PROT | CS_MEMCPY_FLAGS)]);
        *m.done_after.borrow_mut() = 3;
        assert_eq!(wait_done(&m, 1000, &|| false), Ok(()));
        // Timeout is bounded.
        let m = Mock::new();
        start(&m, 0x8000, 0).unwrap();
        assert_eq!(wait_done(&m, 100, &|| false), Err(Dma40Error::Timeout));
        // Engine error surfaces CS and DEBUG.
        let m = Mock::new();
        start(&m, 0x8000, 0).unwrap();
        m.set(DEBUG, DEBUG_READ_ERR);
        assert_eq!(wait_done(&m, 100, &|| false), Err(Dma40Error::Hw(CS_ACTIVE | CS_PROT, DEBUG_READ_ERR)));
        // An interrupt finishes the wait early.
        let m = Mock::new();
        start(&m, 0x8000, 0).unwrap();
        assert_eq!(wait_done(&m, 100, &|| true), Ok(()));
        assert_eq!(start(&m, 0x8008, 0), Err(Dma40Error::Misaligned));
    }

    #[test]
    fn cs_ack_keeps_cyclic_running() {
        let f = DreqCfg::from_cell(DREQ_CELL_HDMI0).cs_flags;
        assert_eq!(cs_ack(f), CS_INT | CS_ACTIVE | CS_PROT | cs_qos(0xa) | cs_panic_qos(0xf));
        assert_eq!(cs_start(f) & CS_INT, 0);
    }

    #[test]
    fn crc_and_pattern() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
        let a: Vec<u32> = (0..16384).map(pattern_word).collect();
        let mut b = a.clone();
        b.rotate_left(1);
        let bytes = |v: &Vec<u32>| v.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        assert_ne!(crc32(&bytes(&a)), crc32(&bytes(&b)), "shifted words change the CRC");
        let mut dedup = a.clone(); dedup.sort(); dedup.dedup();
        assert_eq!(dedup.len(), a.len(), "pattern words are distinct");
    }
}
