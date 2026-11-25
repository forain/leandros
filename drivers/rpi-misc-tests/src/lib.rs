//! Host tests: `rpi_misc_logic.rs` included verbatim, driven against a mock
//! register file that records every access.
#[path = "../../src/rpi_misc_logic.rs"]
pub mod logic;

#[cfg(test)]
mod tests {
    use super::logic::*;
    use std::collections::{HashMap, VecDeque};

    #[derive(Default)]
    struct Mock {
        regs: HashMap<usize, u32>,
        fifo: VecDeque<u32>,
        writes: Vec<(usize, u32)>,
        /// The PM block reuses rng200's FIFO offsets (0x20/0x24) for RSTS/WDOG.
        pm: bool,
    }
    impl Regs32 for Mock {
        fn r(&mut self, off: usize) -> u32 {
            match off {
                _ if self.pm => *self.regs.get(&off).unwrap_or(&0),
                rng200::FIFO_COUNT => self.fifo.len() as u32 | 0xab00, // upper bits = threshold junk
                rng200::FIFO_DATA => self.fifo.pop_front().unwrap_or(0),
                _ => *self.regs.get(&off).unwrap_or(&0),
            }
        }
        fn w(&mut self, off: usize, v: u32) { self.writes.push((off, v)); self.regs.insert(off, v); }
    }

    #[test]
    fn rng_reads_whole_and_partial_words() {
        let mut m = Mock::default();
        m.fifo.extend([0x04030201, 0x08070605]);
        let mut out = [0u8; 6];
        assert_eq!(rng200::read(&mut m, &mut out), 6);
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
        assert!(m.fifo.is_empty());
    }

    #[test]
    fn rng_empty_fifo_gives_nothing() {
        let mut m = Mock::default();
        let mut out = [0u8; 8];
        assert_eq!(rng200::read(&mut m, &mut out), 0);
    }

    #[test]
    fn rng_failure_restarts_and_returns_zero() {
        let mut m = Mock::default();
        m.regs.insert(rng200::INT_STATUS, rng200::INT_NIST_FAIL);
        m.regs.insert(rng200::CTRL, 0xffff_e001);
        m.fifo.push_back(7);
        let mut out = [0u8; 4];
        assert_eq!(rng200::read(&mut m, &mut out), 0);
        // disable, clear lockup, pulse both resets, enable
        let offs: Vec<usize> = m.writes.iter().map(|w| w.0).collect();
        assert_eq!(offs, [rng200::CTRL, rng200::INT_STATUS, rng200::SOFT_RESET, rng200::SOFT_RESET,
                          rng200::RBG_SOFT_RESET, rng200::RBG_SOFT_RESET, rng200::CTRL]);
        assert_eq!(m.writes[0].1, 0xffff_e000);
        assert_eq!(m.writes[2].1 & 1, 1);
        assert_eq!(m.writes[3].1 & 1, 0);
        assert_eq!(*m.writes.last().unwrap(), (rng200::CTRL, 0xffff_e001));
    }

    #[test]
    fn pm_arm_kick_disarm() {
        let mut m = Mock { pm: true, ..Default::default() };
        m.regs.insert(pm::RSTC, 0x0000_0030);
        pm::arm(&mut m, pm::secs_to_ticks(10));
        assert_eq!(m.writes[0], (pm::WDOG, 0x5a00_0000 | (10 << 16)));
        assert_eq!(m.writes[1], (pm::RSTC, 0x5a00_0000 | 0x20));
        assert!(pm::is_running(&mut m));
        pm::disarm(&mut m);
        assert_eq!(*m.writes.last().unwrap(), (pm::RSTC, 0x5a00_0102));
    }

    #[test]
    fn pm_ticks_clamped() {
        assert_eq!(pm::secs_to_ticks(0), 1 << 16);
        assert_eq!(pm::secs_to_ticks(100), 15 << 16);
        assert_eq!(pm::MAX_SECS, 15);
    }

    #[test]
    fn pm_power_off_sets_halt_partition_then_resets() {
        let mut m = Mock { pm: true, ..Default::default() };
        m.regs.insert(pm::RSTS, 0x1000);
        pm::power_off(&mut m);
        assert_eq!(m.writes[0], (pm::RSTS, 0x5a00_0000 | 0x1000 | 0x555));
        assert_eq!(m.writes[1], (pm::WDOG, 0x5a00_0000 | 10));
        assert_eq!(m.writes[2].0, pm::RSTC);
        assert_eq!(m.writes[2].1 & 0x30, 0x20);
    }

    #[test]
    fn avs_conversion_matches_dts_coefficients() {
        assert_eq!(avs::millicelsius(0x3ff), None);
        // 450000 - 550 * 727 = 50150 mC
        assert_eq!(avs::millicelsius((1 << 16) | (1 << 10) | 727), Some(50_150));
        assert_eq!(avs::millicelsius((1 << 10) | 0), Some(450_000));
    }

    #[test]
    fn fw_build_and_parse() {
        let mut msg = [0u32; fw::MSG_WORDS];
        fw::build(fw::GET_CLOCK_RATE, &[fw::CLK_ARM, 0], &mut msg);
        assert_eq!(msg, [36, 0, 0x30002, 12, 0, 3, 0, 0, 0]);
        let mut reply = msg;
        reply[1] = 0x8000_0000;
        assert_eq!(fw::parse(&reply), None, "tag not handled");
        reply[4] = 0x8000_0008;
        reply[6] = 2_400_000_000;
        assert_eq!(fw::parse(&reply), Some([3, 2_400_000_000, 0]));
        reply[1] = 0x8000_0001;
        assert_eq!(fw::parse(&reply), None, "request error");
        assert_eq!(fw::parse(&reply[..6]), None, "short");
    }

    #[test]
    fn rtc_plausibility() {
        assert!(!fw::rtc_plausible(12));
        assert!(fw::rtc_plausible(1_793_000_000));
    }

    #[test]
    fn governor_up_fast_down_slow() {
        let mut g = Governor::new(1_500_000_000, 2_400_000_000, 2_400_000_000);
        // idle window: steps down once the history is mostly idle
        let mut changes = vec![];
        for _ in 0..3 { if let Some(hz) = g.sample(false) { changes.push(hz); } }
        assert_eq!(changes, [1_500_000_000]);
        // load: straight to max on the first busy sample
        assert_eq!(g.sample(true), Some(2_400_000_000));
        assert_eq!(g.sample(true), None);
        // two busy samples in the window keep it up until they age out
        let mut n = 0;
        loop { n += 1; if g.sample(false).is_some() { break; } }
        assert_eq!(n, Governor::WINDOW - 1);
        assert_eq!(g.cur_hz, 1_500_000_000);
    }

    #[test]
    fn governor_without_rates_never_acts() {
        let mut g = Governor::new(0, 0, 0);
        assert_eq!(g.sample(true), None);
        assert_eq!(g.sample(false), None);
    }

    #[test]
    fn busy_excludes_sampler() {
        assert!(!busy_from_nr_running(1));
        assert!(busy_from_nr_running(2));
    }

    #[test]
    fn buf_formats_millicelsius() {
        let mut b = [0u8; 32];
        let mut w = Buf::new(&mut b);
        w.mc(50_150).s(" ").i(-3).s(" ").mc(-1_500);
        let n = w.n;
        assert_eq!(std::str::from_utf8(&b[..n]).unwrap(), "50.1 -3 -1.5");
    }

    #[test]
    fn wdt_feeds_only_while_every_busy_cpu_ticks() {
        use super::logic::wdt::*;
        assert_eq!(stuck_cpu([(1, 0, false), (2, 3, false), (3, 44, false)].into_iter(), STUCK_SECS), None);
        // an idle CPU whose timer sleeps never stops the feeding
        assert_eq!(stuck_cpu([(1, 0, false), (2, 300, true)].into_iter(), STUCK_SECS), None);
        // the worst stuck CPU is named
        assert_eq!(stuck_cpu([(1, 45, false), (2, 50, false), (3, 0, false)].into_iter(), STUCK_SECS), Some((2, 50)));
        assert_eq!(stuck_cpu(core::iter::empty(), STUCK_SECS), None);
    }

    #[test]
    fn wdt_serial_magic() {
        use super::logic::wdt::*;
        let mut m = Magic::new();
        for i in 0..4 { assert!(!m.feed(0x1d, false, 100 * i)); }
        assert!(m.feed(0x1d, false, 500));
        // an intervening byte restarts the count
        let mut m = Magic::new();
        for i in 0..4 { m.feed(0x1d, false, i); }
        assert!(!m.feed(b'x', false, 5));
        assert!(!m.feed(0x1d, false, 6));
        // too slow: five presses spread over more than 2 s
        let mut m = Magic::new();
        for i in 0..5 { assert!(!m.feed(0x1d, false, 600 * i)); }
        // BREAK then 'b'
        let mut m = Magic::new();
        assert!(!m.feed(0, true, 10));
        assert!(m.feed(b'b', false, 20));
        let mut m = Magic::new();
        assert!(!m.feed(0, true, 10));
        assert!(!m.feed(b'b', false, 5000));
        assert!(!m.feed(b'b', false, 5001));
        // plain 'b' never resets
        assert!(!Magic::new().feed(b'b', false, 0));
        assert!(dtb_has_pm(b"xx\0brcm,bcm2712-pm\0yy"));
        assert!(!dtb_has_pm(b"brcm,bcm2712-pmx"));
        // chip-id fallback sanity read (kexec FDT has no PM node)
        assert!(pm_regs_plausible(0, 0x1000, 0));
        assert!(pm_regs_plausible(0x20, 0, 0x000f_0000));
        assert!(!pm_regs_plausible(u32::MAX, u32::MAX, u32::MAX));
        assert!(!pm_regs_plausible(0, 0, 0x0010_0000));
        assert!(!pm_regs_plausible(0x5a00_0020, 0, 0));
        let mut t = [0u8; 32];
        let mut b = Buf::new(&mut t);
        b.x(0).s(" ").x(0x1f_ab00);
        assert_eq!(&b.b[..b.n], b"0x0 0x1fab00");
    }

    // ── SD write grant ──────────────────────────────────────────────────────

    fn mbr_of(entries: &[(u8, u32)]) -> [u8; 512] {
        let mut m = [0u8; 512];
        for (i, &(ty, lba)) in entries.iter().enumerate() {
            let e = &mut m[446 + i * 16..446 + i * 16 + 16];
            e[4] = ty;
            e[8..12].copy_from_slice(&lba.to_le_bytes());
        }
        m[510] = 0x55; m[511] = 0xAA;
        m
    }

    #[test]
    fn batocera_style_card_has_no_f2fs_root() {
        // p1 vfat 0x0c, p2 ext4 0x83 (magic 0xEF53 at the superblock), no f2fs.
        let m = mbr_of(&[(0x0c, 8192), (0x83, 1_056_768)]);
        assert_eq!(sdmbr::pick_f2fs(&m, |_| Some(0xEF53)), None);
        assert_eq!(sdmbr::pick_f2fs(&m, |_| None), None);
        // root_guard (reads through the same picker) finds nothing either.
        assert_eq!(sdmbr::root_guard(&m, |_, b| { b.fill(0); true }), None);
    }

    #[test]
    fn grant_table() {
        use sdgrant::*;
        let ev = |f, rw, cl, lb| evaluate(f, rw, cl, lb).0;
        let l = "leandros-root";
        // no root=, or a root naming this card: granted
        assert!(ev(true, true, "", l));
        assert!(ev(true, true, "console=ttyAMA10,115200", l));
        assert!(ev(true, true, "root=LABEL=leandros-root", l));
        assert!(ev(true, true, "root=/dev/vdb", l));
        assert!(ev(true, true, "root=/dev/mmcblk0p2 rootfstype=ext4 rootwait", l));
        assert!(ev(true, true, "a root=/dev/mmcblk0p17 b", l));
        // another OS's root / not provably ours: locked
        assert!(!ev(true, true, "root=PARTUUID=1234-02", l));
        assert!(!ev(true, true, "root=/dev/sda2", l));
        assert!(!ev(true, true, "root=LABEL=other", l));
        assert!(!ev(true, true, "root=LABEL=leandros-root", "other"));
        assert!(!ev(true, true, "root=LABEL=leandros-root", ""));
        assert!(!ev(true, true, "root=/dev/mmcblk0p", l));
        assert!(!ev(true, true, "root=/dev/mmcblk0", l));
        assert!(!ev(true, true, "root=/dev/nvme0n1p2", l));
        // the last root= wins
        assert!(!ev(true, true, "root=/dev/vdb root=/dev/sda1", l));
        // no magic or rw off: locked whatever root says
        assert_eq!(evaluate(false, true, "", l), (false, R_NO_F2FS));
        assert_eq!(evaluate(false, true, "root=/dev/vdb", l), (false, R_NO_F2FS));
        assert_eq!(evaluate(true, false, "", l), (false, R_RW_OFF));
        assert_eq!(evaluate(true, true, "root=/dev/sda1", l), (false, R_OTHER_ROOT));
        assert_eq!(evaluate(true, true, "", l), (true, R_OK));
        // raspi4b/QEMU: no cmdline, magic at offset 0, no label needed
        assert!(ev(true, true, "", ""));
    }

    #[test]
    fn command_filter() {
        use sdgrant::*;
        for c in [24u8, 25, 27, 28, 29, 32, 33, 38, 42, 56] {
            assert!(is_write_cmd(c, false), "CMD{c}");
            assert!(!allows(false, c, false), "CMD{c} locked");
            assert!(allows(true, c, false), "CMD{c} granted");
        }
        // ACMD23 is a write-class command, but CMD23 (SET_BLOCK_COUNT) is not
        assert!(is_write_cmd(23, true));
        assert!(!is_write_cmd(23, false));
        // an ACMD sharing an index with a write CMD is not blocked
        for c in [24u8, 25, 27, 28, 29, 32, 33, 38, 42, 56] { assert!(!is_write_cmd(c, true), "ACMD{c}"); }
        // CMD6 / ACMD6 / CMD11 / reads / identification / ACMD41 stay open when locked
        for (c, app) in [(6u8, false), (6, true), (11, false), (17, false), (18, false), (12, false),
                         (0, false), (2, false), (3, false), (7, false), (8, false), (9, false),
                         (16, false), (19, false), (55, false), (41, true), (51, true)] {
            assert!(allows(false, c, app), "CMD{c} app={app}");
        }
    }

    #[test]
    fn locked_means_no_write_command_reaches_the_controller() {
        use sdgrant::*;
        let mut m = Mock::default();
        // locked: every write-class command is refused with ZERO register access
        for c in [24u8, 25, 27, 28, 29, 32, 33, 38, 42, 56] {
            assert!(!issue(&mut m, false, c, false, 0x1234, (c as u16) << 8));
        }
        assert!(!issue(&mut m, false, 23, true, 8, 23 << 8));
        assert!(m.writes.is_empty(), "locked writes touched MMIO: {:?}", m.writes);
        // locked: a read still programs the controller
        assert!(issue(&mut m, false, 17, false, 5, 17 << 8));
        assert_eq!(m.writes, vec![(REG_ARGUMENT1, 5), (REG_COMMAND, 17 << 8)]);
        // granted: CMD24 goes out
        m.writes.clear();
        assert!(issue(&mut m, true, 24, false, 7, 24 << 8));
        assert_eq!(m.writes, vec![(REG_ARGUMENT1, 7), (REG_COMMAND, 24 << 8)]);
    }
}
