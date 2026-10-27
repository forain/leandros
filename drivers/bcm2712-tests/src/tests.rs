use crate::bcm2712_gpio::tables::{self, NONE};
use crate::bcm2712_gpio::*;
use crate::bcm2712_id::{Stepping, C0_PINS, D0_PINS};
use crate::bcm2712_otp::*;

/// Register file with an ordered write log; offsets in `w1c` clear on write-1.
struct Mock { r: [u32; 64], log: Vec<(usize, u32)>, w1c: Vec<usize> }
impl Default for Mock { fn default() -> Self { Mock { r: [0; 64], log: vec![], w1c: vec![] } } }
impl Regs for &mut Mock {
    fn rd(&mut self, off: usize) -> u32 { self.r[off / 4] }
    fn wr(&mut self, off: usize, v: u32) {
        self.log.push((off, v));
        if self.w1c.contains(&off) { self.r[off / 4] &= !v } else { self.r[off / 4] = v }
    }
}

// ── pinctrl ──

/// The generated tables reproduce the hand-checked sdio2 rows that the Wi-Fi
/// probe used before (bcm2712_id C0_PINS / D0_PINS), so the move onto this
/// driver programs exactly the same registers and values.
#[test]
fn sdio2_group_matches_previous_rows() {
    for (st, rows) in [(Stepping::C0, &C0_PINS), (Stepping::D0, &D0_PINS)] {
        let tab = table(Block::Main, st).unwrap();
        for (i, &(pin_no, mr, mb, pr, pb, fsel, pull)) in rows.iter().enumerate() {
            let p = pin(tab, pin_no).unwrap();
            assert_eq!(bit_reg(p.mux), (mr * 4, mb * 4), "{:?} pin {} mux", st, pin_no);
            assert_eq!(bit_reg(p.pad), (pr * 4, pb * 2), "{:?} pin {} pad", st, pin_no);
            let c = GROUP_SDIO2_WL_ON[i];
            assert_eq!(c.pin, pin_no);
            assert_eq!(fsel_for(p, func_id(c.func).unwrap()), Some(fsel), "{:?} pin {} fsel", st, pin_no);
            assert_eq!(c.pull.map(|p| p as u32), Some(pull));
        }
    }
}

/// Same register writes as the old ad-hoc pinmux: per pin, mux then pad, only on change.
#[test]
fn apply_sdio2_d0_writes_once() {
    let mut m = Mock::default();
    let tab = table(Block::Main, Stepping::D0).unwrap();
    let mut old = Mock::default();
    // Old algorithm, verbatim semantics.
    for &(_pin, mr, mb, pr, pb, fsel, pull) in D0_PINS.iter() {
        let v = old.r[mr]; let cur = (v >> (mb * 4)) & 0xF;
        if cur != fsel { let n = (v & !(0xF << (mb * 4))) | (fsel << (mb * 4)); old.r[mr] = n; old.log.push((mr * 4, n)); }
        let p = old.r[pr]; let curp = (p >> (pb * 2)) & 3;
        if curp != pull { let n = (p & !(3 << (pb * 2))) | (pull << (pb * 2)); old.r[pr] = n; old.log.push((pr * 4, n)); }
    }
    let w = Pinctrl::new(&mut m, tab).apply(&GROUP_SDIO2_WL_ON).unwrap();
    assert_eq!(w as usize, old.log.len());
    assert_eq!(m.log, old.log);
    assert_eq!(m.r, old.r);
    m.log.clear();
    assert_eq!(Pinctrl::new(&mut m, tab).apply(&GROUP_SDIO2_WL_ON).unwrap(), 0);
    assert!(m.log.is_empty());
}

#[test]
fn uarta_and_bt_on_functions() {
    // D0: uart0 is f4 on 24..27; C0: f3 on 24, f4 on 25..27.
    for (st, want) in [(Stepping::D0, [4, 4, 4, 4]), (Stepping::C0, [3, 4, 4, 4])] {
        let tab = table(Block::Main, st).unwrap();
        let mut m = Mock::default();
        let mut pc = Pinctrl::new(&mut m, tab);
        pc.apply(&GROUP_UARTA).unwrap();
        pc.apply(&GROUP_BT_SHUTDOWN).unwrap();
        for (i, p) in (24..28).enumerate() {
            assert_eq!(pc.get_fsel(p), Some(want[i]));
            assert_eq!(pc.get_function(p), Some("uart0"));
        }
        assert_eq!(pc.get_pull(25), Some(Pull::Up as u32));
        assert_eq!(pc.get_function(29), Some("gpio"));
        assert_eq!(pc.set_function(29, "nonsense"), Err(PinError::NoFunction));
        assert_eq!(pc.set_function(99, "gpio"), Err(PinError::NoPin));
        assert_eq!(pc.set_function(30, "uart0"), Err(PinError::NoFunction));
    }
    assert!(table(Block::Main, Stepping::Unknown).is_none());
}

#[test]
fn tables_fit_mappings_and_raw_alt() {
    for st in [Stepping::C0, Stepping::D0] {
        for b in [Block::Main, Block::Aon] {
            let t = table(b, st).unwrap();
            assert!(table_span(t) <= 0x30, "{:?} {:?} span {:#x}", st, b, table_span(t));
        }
    }
    let d0 = table(Block::Main, Stepping::D0).unwrap();
    assert_eq!(fsel_for(pin(d0, 30).unwrap(), func_id("alt3").unwrap()), Some(3));
    assert_eq!(fsel_for(pin(d0, 30).unwrap(), func_id("gpio").unwrap()), Some(0));
    // emmc pads have no mux.
    let emmc = d0.iter().find(|p| p.name == "emmc_cmd").unwrap();
    assert_eq!(emmc.mux, NONE);
    // aon: D0 aon_gpio5 "sd_card_g" (dts emmc_aon_cd_pins), sgpio4 avs_pmu_bsc has no pad.
    let aon = table(Block::Aon, Stepping::D0).unwrap();
    assert!(fsel_for(pin(aon, 5).unwrap(), func_id("sd_card_g").unwrap()).is_some());
    let s4 = pin(aon, 36).unwrap();
    assert_eq!(s4.name, "aon_sgpio4");
    assert_eq!(s4.pad, NONE);
    assert!(fsel_for(s4, func_id("avs_pmu_bsc").unwrap()).is_some());
    assert_eq!(tables::FUNC_NAMES[0], "gpio");
}

// ── GPIO ──

#[test]
fn gpio_locate_by_stepping() {
    let d0 = bank_widths(Chip::Gio, Stepping::D0);
    assert_eq!(line_count(d0), 36);
    assert_eq!(locate(d0, WL_ON), Ok((0, 28)));
    assert_eq!(locate(d0, BT_ON), Ok((0, 29)));
    assert_eq!(locate(d0, 33), Ok((0x20, 1)));
    assert_eq!(locate(d0, 36), Err(GpioError::BadLine));
    let c0 = bank_widths(Chip::Gio, Stepping::C0);
    assert_eq!(locate(c0, 53), Ok((0x20, 21)));
    assert_eq!(line_count(bank_widths(Chip::GioAon, Stepping::D0)), 21);
    assert_eq!(locate(bank_widths(Chip::GioAon, Stepping::D0), 15), Ok((0x20, 0)));
    assert_eq!(locate(bank_widths(Chip::GioAon, Stepping::C0), 17), Ok((0x20, 0)));
}

/// WL_REG_ON sequence identical to the old sdio_host code: DATA always
/// written first, IODIR cleared only if the line was an input.
#[test]
fn set_output_order() {
    let mut m = Mock::default();
    m.r[GIO_IODIR / 4] = 0xFFFF_FFFF;
    m.r[GIO_DATA / 4] = 0x1;
    let w = bank_widths(Chip::Gio, Stepping::D0);
    Gpio::new(&mut m, w).set_output(BT_ON, true).unwrap();
    assert_eq!(m.log, vec![(GIO_DATA, 0x1 | 1 << 29), (GIO_IODIR, !(1u32 << 29))]);
    m.log.clear();
    let mut g = Gpio::new(&mut m, w);
    g.set_output(BT_ON, false).unwrap();
    assert_eq!(g.get(BT_ON), Ok(false));
    assert_eq!(g.is_output(BT_ON), Ok(true));
    assert_eq!(m.log, vec![(GIO_DATA, 0x1)]);
    Gpio::new(&mut m, w).set_output(33, true).unwrap();
    assert_eq!(m.r[(0x20 + GIO_DATA) / 4], 1 << 1);
    let mut g = Gpio::new(&mut m, w);
    g.set_input(33).unwrap();
    assert_eq!(g.is_output(33), Ok(false));
}

#[test]
fn gpio_irq_config_and_dispatch() {
    let mut m = Mock::default();
    m.w1c = vec![GIO_STAT, 0x20 + GIO_STAT];
    let w = bank_widths(Chip::Gio, Stepping::D0);
    let mut g = Gpio::new(&mut m, w);
    g.set_trigger(5, Trigger::Both).unwrap();
    g.set_trigger(6, Trigger::High).unwrap();
    g.set_trigger(33, Trigger::Falling).unwrap();
    g.set_irq_enabled(5, true).unwrap();
    g.set_irq_enabled(33, true).unwrap();
    assert_eq!(trigger_bits(Trigger::Low), (true, false, false));
    assert_eq!(m.r[GIO_EI / 4], 1 << 5);
    assert_eq!(m.r[GIO_LEVEL / 4], 1 << 6);
    assert_eq!(m.r[GIO_EC / 4], 1 << 6);
    // Pending: 5 (enabled), 7 (masked), 33 (enabled, bank 1), bank-1 bit 9 beyond width 4.
    m.r[GIO_STAT / 4] = (1 << 5) | (1 << 7);
    m.r[(0x20 + GIO_STAT) / 4] = (1 << 1) | (1 << 9);
    m.r[(0x20 + GIO_MASK) / 4] |= 1 << 9;
    let mut seen = vec![];
    let n = Gpio::new(&mut m, w).dispatch(|l| seen.push(l));
    assert_eq!((n, seen), (2, vec![5, 33]));
    assert_eq!(m.r[GIO_STAT / 4], 1 << 7);
    Gpio::new(&mut m, w).irq_reset();
    assert_eq!(m.r[GIO_MASK / 4], 0);
    assert_eq!(m.r[GIO_STAT / 4], 0);
}

// ── l2 intc ──

#[test]
fn l2_offsets() {
    assert_eq!(l2_offs(L2Kind::Level7271), L2Offs { status: 0, clear: None, mask_status: 4, mask_set: 8, mask_clear: 0xc });
    assert_eq!(l2_offs(L2Kind::Edge).clear, Some(8));
    assert_eq!(l2_offs(L2Kind::Edge).mask_clear, 0x14);
    let main = L2_CONTROLLERS[L2Id::MainIrq as usize];
    assert_eq!((main.phys, spi_intid(main.spi)), (0x10_7d50_8400, 276));
    assert_eq!(spi_intid(L2_CONTROLLERS[L2Id::BscIrq as usize].spi), 274);
    assert_eq!(spi_intid(L2_CONTROLLERS[L2Id::CpuL2 as usize].spi), 270);
}

#[test]
fn l2_level_dispatch_masks_orphans() {
    let mut m = Mock::default();
    let mut l2 = L2::new(&mut m, L2Kind::Level7271);
    l2.reset();
    l2.unmask(0);
    l2.unmask(3);
    // mask_status is a status view; model it: set/clear registers update it.
    drop(l2);
    m.r[1] = !((1 << 0) | (1 << 3));
    m.r[0] = (1 << 0) | (1 << 3) | (1 << 4);
    let mut seen = vec![];
    let (n, masked) = L2::new(&mut m, L2Kind::Level7271).dispatch(|hw| { seen.push(hw); hw == 0 });
    assert_eq!((n, masked, seen), (1, 1 << 3, vec![0, 3]));
    assert_eq!(m.log[0], (8, 0xFFFF_FFFF));
    assert_eq!(&m.log[1..3], &[(0xc, 1), (0xc, 8)]);
    assert_eq!(*m.log.last().unwrap(), (8, 1 << 3));
}

#[test]
fn l2_edge_acks_before_handler() {
    let mut m = Mock::default();
    m.w1c = vec![0x00, 0x08];
    m.r[0] = 1 << 2;
    let (n, masked) = L2::new(&mut m, L2Kind::Edge).dispatch(|_| true);
    assert_eq!((n, masked), (1, 0));
    assert_eq!(m.log, vec![(0x08, 1 << 2)]);
}

// ── OTP ──

#[test]
fn otp_decode_and_wifi_mac() {
    // 2c:cf:67:12:34:56 as the firmware returns it (bytes in order, LE words).
    let w = [u32::from_le_bytes([0x2c, 0xcf, 0x67, 0x12]), u32::from_le_bytes([0x34, 0x56, 0, 0]), 0];
    let mac = mac_from_words(&w).unwrap();
    assert_eq!(mac, [0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56]);
    assert_eq!(&fmt_mac(&mac), b"2c:cf:67:12:34:56");
    assert_eq!(wifi_from_eth(&mac), [0x2c, 0xcf, 0x67, 0x12, 0x34, 0x57]);
    assert_eq!(wifi_from_eth(&[0x2c, 0xcf, 0x67, 0xff, 0xff, 0xff]), [0x2c, 0xcf, 0x67, 0, 0, 0]);
    assert_eq!(wifi_from_eth(&[0x2c, 0xcf, 0x67, 0x00, 0xff, 0xff]), [0x2c, 0xcf, 0x67, 1, 0, 0]);
    assert!(mac_from_words(&[0, 0, 0]).is_none());
    assert!(mac_from_words(&[0x01, 0, 0]).is_none()); // multicast
    assert_eq!(serial_from_words(&[0x89ab_cdef, 0x0123_4567, 0]), Some(0x0123_4567_89ab_cdef));
    assert!(serial_from_words(&[0, 0, 0]).is_none());
    assert!(eth_mac().is_none() && serial().is_none());
    set(Some(7), Some(mac));
    assert_eq!(eth_mac(), Some(mac));
    assert_eq!(wifi_mac(), Some([0x2c, 0xcf, 0x67, 0x12, 0x34, 0x57]));
    assert_eq!(serial(), Some(7));
}

/// Regs that also append to a log shared across blocks, so the order of
/// pinctrl and gio writes can be checked together.
struct Tagged<'a> { r: [u32; 64], tag: char, log: &'a std::cell::RefCell<Vec<(char, usize, u32)>> }
impl Regs for &mut Tagged<'_> {
    fn rd(&mut self, off: usize) -> u32 { self.r[off / 4] }
    fn wr(&mut self, off: usize, v: u32) { self.log.borrow_mut().push((self.tag, off, v)); self.r[off / 4] = v }
}

/// Real Pi 5 D0 boot state (main 4b975f56): 24-27 fsel 0, BT_ON an input.
/// bt_pins_prepare must leave 24-27 on uart0 (fsel 4) with CTS/RXD pulled
/// up and RTS/TXD unbiased, BT_ON a low output, and drive BT_ON low before
/// the first uart mux write (the controller never sees a powered, unmuxed UART).
#[test]
fn bt_pins_prepare_from_d0_boot_state() {
    for st in [Stepping::D0, Stepping::C0] {
        let log = std::cell::RefCell::new(vec![]);
        let tab = table(Block::Main, st).unwrap();
        let mut pm = Tagged { r: [0; 64], tag: 'p', log: &log };
        let mut gm = Tagged { r: [0; 64], tag: 'g', log: &log };
        let (b29, bit29) = locate(bank_widths(Chip::Gio, st), BT_ON).unwrap();
        gm.r[(b29 + GIO_IODIR) / 4] = 1 << bit29; // input at reset
        gm.r[(b29 + GIO_DATA) / 4] = 1 << bit29; // stale high latch
        {
            let mut pc = Pinctrl::new(&mut pm, tab);
            let mut g = Gpio::new(&mut gm, bank_widths(Chip::Gio, st));
            let f = bt_pins_prepare(&mut pc, &mut g).unwrap();
            if st == Stepping::D0 { assert_eq!(f, [4, 4, 4, 4]); }
            for p in 24..28 { assert_eq!(pc.get_function(p), Some("uart0"), "{:?} {}", st, p); }
            let pulls: Vec<_> = (24..28).map(|p| pc.get_pull(p).unwrap()).collect();
            assert_eq!(pulls, [Pull::None as u32, Pull::Up as u32, Pull::None as u32, Pull::Up as u32]);
            assert_eq!(pc.get_function(29), Some("gpio"));
            assert_eq!((g.is_output(BT_ON).unwrap(), g.get(BT_ON).unwrap()), (true, false));
        }
        let log = log.borrow();
        let first_mux = log.iter().position(|e| e.0 == 'p').unwrap();
        let gpio_low = log.iter().position(|e| e.0 == 'g').unwrap();
        assert!(gpio_low < first_mux, "{:?}: BT_ON must go low before 24-27 are muxed: {:?}", st, *log);
        // Idempotent: a second run (kexec) writes no pinctrl register.
        let n = log.len();
        drop(log);
        let mut pc = Pinctrl::new(&mut pm, tab);
        let mut g = Gpio::new(&mut gm, bank_widths(Chip::Gio, st));
        bt_pins_prepare(&mut pc, &mut g).unwrap();
        assert!(log_borrow_tail(&pm, n).iter().all(|e| e.0 == 'g'));
    }
    assert!(BT_OFF_MS >= 10 && BT_ON_SETTLE_MS >= 100);
}

fn log_borrow_tail(t: &Tagged<'_>, from: usize) -> Vec<(char, usize, u32)> { t.log.borrow()[from..].to_vec() }
