use crate::bus::*;
use crate::mock::*;
use crate::sdio_host::*;

fn up() -> SdioHost<MockHost> {
    let mut h = SdioHost::new(MockHost::new());
    h.host_init().unwrap();
    h.init_card(true).unwrap();
    h.io.ops53.clear();
    h
}

#[test]
fn arg_encoding() {
    assert_eq!(cmd52_arg(true, 1, 0x1000E, 0x28), 0x8000_0000 | 1 << 28 | 0x1000E << 9 | 0x28);
    assert_eq!(cmd52_arg(false, 0, 0x13, 0xAA), 0x13 << 9 | 0xAA);
    assert_eq!(cmd53_arg(false, 2, 0x8000, true, false, 3), 2 << 28 | 1 << 27 | 0x8000 << 9 | 3);
    assert_eq!(cmd53_arg(true, 1, 0x1_FFFF, false, true, 512), 0x8000_0000 | 1 << 28 | 1 << 26 | 0x1_FFFF << 9);
}

#[test]
fn split_matches_linux_helper() {
    let mut v = vec![];
    split_cmd53::<()>(0x8000, true, 1000, 64, true, |c| { v.push(c); Ok(()) }).unwrap();
    assert_eq!(v, vec![Chunk { addr: 0x8000, off: 0, block: true, count: 15 },
                       Chunk { addr: 0x8000 + 960, off: 960, block: false, count: 40 }]);
    v.clear();
    split_cmd53::<()>(0, false, 600, 512, true, |c| { v.push(c); Ok(()) }).unwrap();
    assert_eq!(v, vec![Chunk { addr: 0, off: 0, block: true, count: 1 }, Chunk { addr: 0, off: 512, block: false, count: 88 }]);
    v.clear();
    // Up to one byte-mode command's worth stays in byte mode.
    split_cmd53::<()>(0, true, 64, 64, true, |c| { v.push(c); Ok(()) }).unwrap();
    assert_eq!(v, vec![Chunk { addr: 0, off: 0, block: false, count: 64 }]);
    v.clear();
    // 511-block cap.
    split_cmd53::<()>(0, true, 64 * 600 + 3, 64, true, |c| { v.push(c); Ok(()) }).unwrap();
    assert_eq!(v.iter().map(|c| (c.block, c.count)).collect::<Vec<_>>(), vec![(true, 511), (true, 89), (false, 3)]);
    v.clear();
    // No multi-block support: byte mode in min(blksz, 512) pieces.
    split_cmd53::<()>(0, true, 300, 128, false, |c| { v.push(c); Ok(()) }).unwrap();
    assert_eq!(v.iter().map(|c| (c.block, c.count, c.off)).collect::<Vec<_>>(), vec![(false, 128, 0), (false, 128, 128), (false, 44, 256)]);
    // Error stops the split.
    let mut n = 0;
    assert_eq!(split_cmd53(0, true, 2000, 64, false, |_| { n += 1; if n == 2 { Err(7) } else { Ok(()) } }), Err(7));
    assert_eq!(n, 2);
}

#[test]
fn card_init_sequence() {
    let mut h = SdioHost::new(MockHost::new());
    h.host_init().unwrap();
    let ci = h.init_card(true).unwrap();
    assert_eq!((ci.rca, ci.cccr_rev, ci.manf, ci.card, ci.nfuncs), (1, 0x43, 0x02d0, 0xa9bf, 2));
    assert!(ci.ocr & 0x0030_0000 != 0);
    assert!(ci.width4 && ci.high_speed);
    assert_eq!(ci.clk_khz, 50_000);
    assert_eq!(h.io.byte(0, 0x07), 0x82); // 4-bit + CD disable
    assert_eq!(h.io.byte(0, 0x13) & 0x02, 0x02); // EHS
    assert_eq!(h.io.regs[REG_HOST_CONTROL1] & (HC1_DATA_WIDTH_4 | HC1_HIGH_SPEED), HC1_DATA_WIDTH_4 | HC1_HIGH_SPEED);
    assert_eq!(h.io.byte(0, 0x02), 0x02); // F1 enabled
    assert_eq!((h.io.byte(0, 0x110), h.io.byte(0, 0x111)), (64, 0)); // F1 block size via FBR1
    assert!(h.multi_block);
    let order: Vec<u8> = h.io.cmds.iter().copied().filter(|&c| c != 52).collect();
    assert_eq!(order, vec![0, 5, 5, 5, 5, 3, 7]);
}

#[test]
fn no_high_speed_when_not_wanted() {
    let mut h = SdioHost::new(MockHost::new());
    h.host_init().unwrap();
    let ci = h.init_card(false).unwrap();
    assert!(!ci.high_speed);
    assert_eq!(ci.clk_khz, 25_000);
}

#[test]
fn cmd53_roundtrip_block_and_byte() {
    let mut h = up();
    h.set_block_size(FUNC_WLAN, 64).unwrap();
    assert_eq!((h.io.byte(0, 0x210), h.io.byte(0, 0x211)), (64, 0));
    let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
    h.write_bytes(FUNC_WLAN, 0x100, true, &data).unwrap();
    let ops: Vec<_> = h.io.ops53.iter().map(|o| (o.write, o.block, o.count, o.addr, o.incr)).collect();
    assert_eq!(ops, vec![(true, true, 15, 0x100, true), (true, false, 40, 0x100 + 960, true)]);
    for i in 0..1000 { assert_eq!(h.io.byte(2, 0x100 + i as u32), data[i]); }
    let mut back = vec![0u8; 1000];
    h.read_bytes(FUNC_WLAN, 0x100, true, &mut back).unwrap();
    assert_eq!(back, data);
    // Odd lengths in byte mode.
    let mut b3 = [0u8; 3];
    h.read_bytes(FUNC_WLAN, 0x101, true, &mut b3).unwrap();
    assert_eq!(&b3, &data[1..4]);
}

#[test]
fn cmd53_fixed_address_fifo() {
    let mut h = up();
    h.set_block_size(FUNC_WLAN, 512).unwrap();
    let data = vec![0x5Au8; 600];
    h.write_bytes(FUNC_WLAN, 0x8000, false, &data).unwrap();
    let ops: Vec<_> = h.io.ops53.iter().map(|o| (o.block, o.count, o.addr, o.incr)).collect();
    assert_eq!(ops, vec![(true, 1, 0x8000, false), (false, 88, 0x8000, false)]);
}

#[test]
fn chip_id_through_sbaddr_window() {
    let mut h = up();
    for (i, b) in 0x2908_4345u32.to_le_bytes().iter().enumerate() { h.io.mem.insert((1, 0x8000 + i as u32), *b); }
    let (id, alp) = read_chip_id(&mut h).unwrap();
    assert_eq!(id & 0xFFFF, 0x4345);
    assert!(alp);
    assert_eq!((h.io.byte(1, 0x1000A), h.io.byte(1, 0x1000B), h.io.byte(1, 0x1000C)), (0x00, 0x00, 0x18));
    let op = h.io.ops53[0];
    assert_eq!((op.func, op.addr, op.block, op.count, op.incr), (1, 0x8000, false, 4, true));
    assert_eq!(h.io.byte(1, SBSDIO_FUNC1_CHIPCLKCSR), 0); // clock request dropped again
}

#[test]
fn timeout_maps_and_recovers() {
    let mut h = up();
    h.io.swrst_writes.clear();
    h.io.timeout_next = true;
    assert_eq!(h.read8(0, 0), Err(BusError::Timeout));
    assert_eq!(h.io.swrst_writes, vec![SWRST_CMD | SWRST_DAT]);
    assert_eq!(h.last_fail & 0xFFFF, EI_CMD_TIMEOUT as u32);
    assert_eq!(h.read8(0, 0), Ok(0x43)); // next command works
}

#[test]
fn r5_error_flags_are_errors() {
    let mut h = up();
    h.io.r5_flags = 0x01; // OUT_OF_RANGE
    assert_eq!(h.read8(1, 0x1FFFF), Err(BusError::Io));
    h.io.r5_flags = 0x80; // COM_CRC_ERROR
    assert_eq!(h.read8(0, 0), Err(BusError::Crc));
    h.io.r5_flags = 0x10; // state bits only: not an error
    assert_eq!(h.read8(0, 0), Ok(0x43));
}

#[test]
fn card_irq_wait() {
    let mut h = up();
    assert!(!h.wait_card_irq(500));
    h.io.card_irq = true;
    assert!(h.wait_card_irq(500));
    assert_eq!(h.io.regs[REG_NORMAL_INT_ENABLE + 1] & 1, 0); // disarmed after the hit
}

#[test]
fn cmd5_never_ready_respects_deadline() {
    let mut m = MockHost::new();
    m.cmd5_never_ready = true;
    let mut h = SdioHost::new(m);
    h.deadline_us = Some(2_000_000);
    h.host_init().unwrap();
    let e = h.init_card(true).unwrap_err();
    assert_eq!(e.stage, "cmd5-busy");
    assert!(h.io.time < 2_100_000, "took {} us", h.io.time);
}

#[test]
fn no_controller_is_reported() {
    let mut m = MockHost::new();
    m.regs[REG_HOST_VERSION] = 0xFF;
    m.regs[REG_HOST_VERSION + 1] = 0xFF;
    let mut h = SdioHost::new(m);
    assert_eq!(h.host_init().unwrap_err().stage, "hostver");
}

#[test]
fn clock_divider_v3() {
    assert_eq!(clock_divider(200_000, 400, true).1, 400);
    assert_eq!(clock_divider(200_000, 50_000, true), (2 << 8, 50_000));
    assert_eq!(clock_divider(200_000, 25_000, true), (4 << 8, 25_000));
}

// ── BCM2712 stepping (drivers/src/bcm2712_id.rs) ────────────────────────────
mod bcm2712 {
    use crate::bcm2712_id::*;

    #[test]
    fn chip_id_decodes_c1_and_d0() {
        // Linux brcmstb/common.c: revision = ((id & 0xf0) >> 4) + 'A', id & 0xf.
        assert_eq!(decode_chip_id(0x2712_0000, 0x2712_0021), Some((Stepping::C0, 0x21)));
        assert_eq!(decode_chip_id(0x2712_0000, 0x2712_0030), Some((Stepping::D0, 0x30)));
        assert_eq!(decode_chip_id(0x2712_0030, 0x0000_0000), Some((Stepping::D0, 0x30)));
        assert_eq!(decode_chip_id(0x2712_0010, 0x2712_0010), Some((Stepping::Unknown, 0x10)));
        assert_eq!(decode_chip_id(0, 0), None);
        assert_eq!(decode_chip_id(0xffff_ffff, 0xffff_ffff), None);
        assert_eq!(decode_chip_id(0x2711_0020, 0x2711_0020), None);
    }

    /// The real-board case of 2026-10-26: D0 chip, C1 tree (no overlay).
    #[test]
    fn hardware_beats_device_tree() {
        assert_eq!(select(Some(Stepping::D0), Stepping::C0), (Stepping::D0, Source::Hw));
        assert_eq!(select(Some(Stepping::C0), Stepping::D0), (Stepping::C0, Source::Hw));
        assert_eq!(select(Some(Stepping::Unknown), Stepping::D0), (Stepping::D0, Source::Dt));
        assert_eq!(select(None, Stepping::C0), (Stepping::C0, Source::Dt));
        assert_eq!(select(None, Stepping::Unknown), (Stepping::Unknown, Source::None));
    }

    /// Rows re-checked against raspberrypi/linux rpi-6.12.y pinctrl-bcm2712.c:
    /// bcm2712_{c0,d0}_gpio_pin_regs GPIO_REGS(28..35) and the sd2 position in
    /// bcm2712_{c0,d0}_gpio_pin_funcs (fsel = 1-based function index).
    #[test]
    fn pin_tables_match_linux() {
        let lin_c0 = [(28, 3, 4, 9, 5), (30, 3, 6, 9, 7), (31, 3, 7, 9, 8), (32, 4, 0, 9, 9),
                      (33, 4, 1, 9, 10), (34, 4, 2, 9, 11), (35, 4, 3, 9, 12)];
        let lin_d0 = [(28, 2, 4, 5, 10), (30, 2, 6, 5, 12), (31, 2, 7, 5, 13), (32, 3, 0, 5, 14),
                      (33, 3, 1, 6, 0), (34, 3, 2, 6, 1), (35, 3, 3, 6, 2)];
        let c0_sd2 = [0, 4, 4, 4, 3, 4, 3];
        for i in 0..7 {
            let (p, mr, mb, pr, pb, f, _) = C0_PINS[i];
            assert_eq!((p, mr, mb, pr, pb), lin_c0[i]);
            assert_eq!(f, c0_sd2[i]);
            let (p, mr, mb, pr, pb, f, _) = D0_PINS[i];
            assert_eq!((p, mr, mb, pr, pb), lin_d0[i]);
            assert_eq!(f, if p == 28 { 0 } else { 1 });
        }
        assert!(pin_rows(Stepping::Unknown).is_none());
    }

    /// The C0 mux slots of pins 30..35 (reg 3 nibbles 6-7, reg 4 nibbles 0-3) are
    /// not D0 pin-mux fields at all: D0 muxes stop at reg 3 nibble 3 (gpio35),
    /// and D0 reg 4 is the first pad register. So the logged
    /// sd2-fsel-before=569900 never described sdio2 on the D0 board.
    #[test]
    fn c0_slots_miss_d0_sd2_fields() {
        let d0_mux: Vec<(usize, u32)> = D0_PINS.iter().map(|r| (r.1, r.2)).collect();
        for r in &C0_PINS[1..] {
            assert!(!d0_mux.contains(&(r.1, r.2)), "C0 slot {:?} aliases a D0 sd2 mux field", (r.1, r.2));
        }
        assert!(D0_PINS.iter().all(|r| r.1 <= 3) && D0_PINS.iter().any(|r| r.3 == 5));
    }
}
