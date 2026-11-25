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

    // ── GET_THROTTLED / undervoltage monitor ─────────────────────────────────

    const S: u64 = 1_000_000_000;

    fn text(m: uvolt::Msg, v: u32) -> String {
        let mut b = [0u8; 160];
        let mut w = Buf::new(&mut b);
        uvolt::render(m, v, &mut w);
        let n = w.n;
        String::from_utf8(b[..n].to_vec()).unwrap()
    }

    #[test]
    fn uvolt_bit_decoding() {
        assert!(uvolt::undervolt_now(0x1));
        assert!(!uvolt::undervolt_now(0x50000));
        assert!(uvolt::undervolt_occurred(0x10000));
        assert!(!uvolt::undervolt_occurred(0x1));
        assert_eq!(uvolt::live(0x5_000d), 0xd);
        assert_eq!(uvolt::sticky(0x5_000d), 0x5);
        assert!(uvolt::freq_capped(0x2) && uvolt::throttled(0x4) && uvolt::soft_temp(0x8));
    }

    #[test]
    fn uvolt_request_does_not_clear_sticky() {
        let mut msg = [0u32; fw::MSG_WORDS];
        fw::build(uvolt::GET_THROTTLED, &uvolt::REQUEST, &mut msg);
        assert_eq!(msg[2], 0x0003_0046);
        assert_eq!(msg[5], 0, "a nonzero value would clear sticky bits");
    }

    #[test]
    fn uvolt_first_read_logs_once_even_when_ok() {
        let mut m = uvolt::Monitor::new(false);
        let (k, v) = m.feed(Some(0), 0).expect("first read logs");
        assert_eq!(k, uvolt::Msg::BootOk);
        assert!(text(k, v).starts_with("[power] voltage ok"));
        assert!(m.feed(Some(0), 2 * S).is_none());
        assert_eq!(m.events(), 0);
    }

    #[test]
    fn uvolt_boot_with_sticky_occurred_has_clear_line() {
        let mut m = uvolt::Monitor::new(false);
        let (k, v) = m.feed(Some(0x50000), 0).unwrap();
        assert_eq!(k, uvolt::Msg::BootOccurred);
        let t = text(k, v);
        assert!(t.contains("undervoltage occurred") && t.contains("throttled=0x50000"), "{t}");
        assert_eq!(m.events(), 0, "an old sticky bit is not an event since boot");
    }

    #[test]
    fn uvolt_boot_live_undervoltage_is_an_event() {
        let mut m = uvolt::Monitor::new(false);
        let (k, v) = m.feed(Some(0x50005), 0).unwrap();
        assert_eq!(k, uvolt::Msg::Under);
        assert_eq!(text(k, v), "[power] undervoltage detected (throttled=0x50005)");
        assert_eq!(m.events(), 1);
    }

    #[test]
    fn uvolt_change_detection_and_events() {
        let mut m = uvolt::Monitor::new(false);
        m.feed(Some(0), 0);
        let (k, v) = m.feed(Some(0x50005), 20 * S).unwrap();
        assert_eq!(k, uvolt::Msg::Under);
        assert_eq!(m.events(), 1);
        assert_eq!(m.last_event_ns(), Some(20 * S));
        assert!(m.feed(Some(0x50005), 40 * S).is_none(), "unchanged: silent");
        assert_eq!(m.events(), 1, "staying low is one event");
        let (k, v2) = m.feed(Some(0x50000), 60 * S).unwrap();
        assert_eq!(k, uvolt::Msg::Normal);
        assert_eq!(text(k, v2), "[power] voltage normal (throttled=0x50000)");
        assert_eq!(v, 0x50005);
        m.feed(Some(0x50001), 80 * S);
        assert_eq!(m.events(), 2);
        assert_eq!(m.last_event_ns(), Some(80 * S));
    }

    #[test]
    fn uvolt_sticky_only_growth_counts_an_event() {
        let mut m = uvolt::Monitor::new(false);
        m.feed(Some(0), 0);
        // a dip entirely between two polls: live clear, sticky newly set
        m.feed(Some(0x10000), 2 * S);
        assert_eq!(m.events(), 1);
        m.feed(Some(0x10000), 4 * S);
        assert_eq!(m.events(), 1, "sticky staying set is not a new event");
    }

    #[test]
    fn uvolt_sticky_alone_does_not_log_change() {
        let mut m = uvolt::Monitor::new(false);
        m.feed(Some(0), 0);
        // only sticky bits move: live word unchanged, no log line (the event is counted)
        assert!(m.feed(Some(0x10000), 30 * S).is_none());
        assert_eq!(m.live(), Some(0));
        assert_eq!(m.sticky(), Some(1));
    }

    #[test]
    fn uvolt_rate_limit_defers_not_drops() {
        let mut m = uvolt::Monitor::new(false);
        m.feed(Some(0), 0);
        assert!(m.feed(Some(0x1), 2 * S).is_none(), "inside the rate limit window");
        assert_eq!(m.events(), 1, "still counted");
        assert!(m.feed(Some(0x1), 4 * S).is_none());
        let (k, _) = m.feed(Some(0x1), uvolt::LOG_MIN_NS + 1).expect("deferred line is emitted later");
        assert_eq!(k, uvolt::Msg::Under);
        assert_eq!(m.events(), 1);
        assert!(m.feed(Some(0x1), uvolt::LOG_MIN_NS + 3 * S).is_none());
    }

    #[test]
    fn uvolt_other_live_bits_log_throttle_change() {
        let mut m = uvolt::Monitor::new(false);
        m.feed(Some(0), 0);
        let (k, v) = m.feed(Some(0xc), 30 * S).unwrap();
        assert_eq!(k, uvolt::Msg::Changed);
        assert_eq!(text(k, v), "[power] throttle state changed (throttled=0xc)");
        assert_eq!(m.events(), 0);
    }

    #[test]
    fn uvolt_mailbox_failure_disables_after_three_with_one_line() {
        let mut m = uvolt::Monitor::new(false);
        m.feed(Some(0), 0);
        assert!(m.feed(None, 2 * S).is_none());
        assert!(m.feed(Some(0), 4 * S).is_none(), "a success resets the failure count");
        assert!(m.feed(None, 6 * S).is_none());
        assert!(m.feed(None, 8 * S).is_none());
        let (k, v) = m.feed(None, 10 * S).expect("one disable line");
        assert_eq!(k, uvolt::Msg::Disabled);
        assert!(text(k, v).starts_with("[power] undervoltage monitor disabled"));
        assert!(m.disabled());
        assert!(m.feed(None, 12 * S).is_none());
        assert!(m.feed(Some(0x1), 100 * S).is_none(), "stays off");
        assert_eq!(m.events(), 0);
    }

    #[test]
    fn uvolt_first_read_failure_path() {
        let mut m = uvolt::Monitor::new(false);
        for i in 0..2 { assert!(m.feed(None, i * S).is_none()); }
        assert_eq!(m.feed(None, 3 * S).unwrap().0, uvolt::Msg::Disabled);
    }

    #[test]
    fn uvolt_knob_off_is_inert() {
        let mut m = uvolt::Monitor::new(true);
        assert!(m.disabled());
        assert!(m.feed(Some(0x50005), 0).is_none());
        assert_eq!(m.events(), 0);
        assert_eq!(m.live(), None);
        assert!(uvolt::knob_off(Some("off")));
        assert!(uvolt::knob_off(Some("0")));
        assert!(!uvolt::knob_off(Some("on")));
        assert!(!uvolt::knob_off(None));
    }

    #[test]
    fn uvolt_proc_report_lines() {
        let mut m = uvolt::Monitor::new(false);
        m.feed(Some(0x50005), 7 * S);
        let mut b = [0u8; 256];
        let mut w = Buf::new(&mut b);
        m.report(&mut w);
        let n = w.n;
        let t = String::from_utf8(b[..n].to_vec()).unwrap();
        assert!(t.contains("power: throttled=0x50005 live=0x5 sticky=0x5 undervolt_events=1 last_event_s=7"), "{t}");
        let mut off = uvolt::Monitor::new(true);
        let mut b2 = [0u8; 64];
        let mut w2 = Buf::new(&mut b2);
        off.report(&mut w2);
        let n2 = w2.n;
        assert!(String::from_utf8(b2[..n2].to_vec()).unwrap().contains("power: disabled"));
    }
}
