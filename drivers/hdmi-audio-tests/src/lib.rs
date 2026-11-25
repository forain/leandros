//! Host tests for the HDMI audio backend. `pure.rs`, the probe's `decode.rs` (N/CTS table) and
//! the dma40 `cb.rs` (ring SCBs) are included verbatim: these exercise the code the kernel runs.

#[path = "../../src/hdmi_audio/pure.rs"]
pub mod pure;
#[path = "../../src/hdmi_probe/decode.rs"]
pub mod decode;
#[allow(dead_code)]
#[path = "../../src/bcm2712_dma40/cb.rs"]
pub mod cb;

#[cfg(test)]
mod tests {
    use super::pure::*;

    /// The driver may hand the DMA engine exactly one address: MAI_DATA at its CPU physical
    /// address. A 32-bit "bus" alias (0x7c72_xxxx) is ordinary DRAM on >= 2 GiB boards.
    #[test]
    fn no_bus_alias_fifo_address_in_driver() {
        let src = include_str!("../../src/hdmi_audio/mod.rs");
        let code: String = src.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
        assert!(!code.contains("FIFO_BUS"));
        let norm = code.to_lowercase().replace('_', "");
        // The CPU physical form is 0x10_7c72_xxxx (normalised 0x107c72..); a bare 0x7c72 is the alias.
        assert!(!norm.contains("0x7c72"), "bare 0x7c72.. address (bus alias) found");
    }

    /// alsa-lib pcm_iec958.c `iec958_subframe` for an S32 (left-justified) sample, transcribed.
    fn alsa_subframe(s32: u32, channel: usize, counter: usize, status: &[u8; 24]) -> u32 {
        let byte = counter >> 3;
        let mask = 1u8 << (counter - (byte << 3));
        let mut data = s32 >> 4;
        data &= !0xf;
        if status[byte] & mask != 0 {
            data |= 0x4000_0000;
        }
        // iec958_parity: data >>= 4, parity over 27 bits
        let mut p = 0u32;
        let mut d = data >> 4;
        for _ in 0..27 {
            p += d & 1;
            d >>= 1;
        }
        if p & 1 != 0 {
            data |= 0x8000_0000;
        }
        data | if channel != 0 { 0x04 } else if counter == 0 { 0x08 } else { 0x02 }
    }

    #[test]
    fn subframe_matches_alsa_iec958() {
        let cs = channel_status(48_000, 16);
        let mut x: u32 = 0x1234_5678;
        for i in 0..5000usize {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let s16 = x as i16;
            let f = i % FRAMES_PER_BLOCK;
            for ch in 0..2 {
                assert_eq!(subframe(s16_to_24(s16), ch, f, &cs), alsa_subframe((s16 as i32 as u32) << 16, ch, f, &cs));
                let s24 = (x as i32) >> 8;
                assert_eq!(subframe(s24, ch, f, &cs), alsa_subframe((s24 as u32) << 8, ch, f, &cs));
            }
        }
        // Literal vector: 24-bit 0x123456, left, block start, C=0 at frame 0, parity odd -> P set.
        assert_eq!(subframe(0x123456, 0, 0, &cs), 0x8123_4568);
        // S16 full scale lands in bits 12..27.
        assert_eq!(subframe(s16_to_24(0x7fff), 1, 5, &cs) & 0x0fff_fff0, 0x07ff_f000);
        assert_eq!(s24le_to_24([0x56, 0x34, 0x92]), 0x923456u32 as i32 - 0x0100_0000);
    }

    #[test]
    fn channel_status_bytes_and_c_bits() {
        let cs = channel_status(48_000, 16);
        assert_eq!(&cs[..5], &[0x04, 0x82, 0x00, 0x02, 0xd2]);
        assert!(cs[5..].iter().all(|&b| b == 0));
        let cs44 = channel_status(44_100, 24);
        assert_eq!(&cs44[..5], &[0x04, 0x82, 0x00, 0x00, 0xfb]);
        // byte 0 = 0x04 -> frame 2 carries C=1, frames 0/1 C=0; byte 3 = 0x02 -> frame 25.
        let c = |f| subframe(0, 0, f, &cs) & SUBFRAME_C != 0;
        assert!(!c(0) && !c(1) && c(2) && !c(3) && c(25) && !c(24));
        let mut blk = [0u32; FRAMES_PER_BLOCK * 2];
        silence_block(&cs, &mut blk);
        assert_eq!(blk[0] & 0xf, PREAMBLE_Z);
        assert_eq!(blk[1] & 0xf, PREAMBLE_Y);
        assert_eq!(blk[2] & 0xf, PREAMBLE_X);
        assert!(blk.iter().all(|w| w & 0x0fff_fff0 == 0 && (w & 0x7fff_fff0).count_ones() % 2 == (w >> 31)));
        // Ring offsets map to the same block position the template uses.
        assert_eq!(pack_frame(0, 0, 3 * BLOCK_BYTES + 2 * FRAME_BYTES, &cs), [blk[4], blk[5]]);
    }

    #[test]
    fn mai_register_images() {
        assert_eq!(mai_ctl_startup(), 0x0000_8207);
        assert_eq!(mai_ctl_run(2), 0x0000_3028);
        assert_eq!(mai_thr(true), 0x0808_0e1c, "D0: VC6_D 23/15/7/0 fields");
        assert_eq!(mai_thr(false), 0x1010_1c1c, "C0/C1: gen-5 24/16/8/0 fields");
        assert_eq!(mai_fmt(48_000), Some(0x0002_0900));
        assert_eq!(mai_fmt(44_100), Some(0x0002_0800));
        assert_eq!(mai_fmt(12_345), None);
        assert_eq!(mai_smp(AUDIO_CLOCK_HZ, 48_000), 2250 << 8);
        assert_eq!(mai_smp(AUDIO_CLOCK_HZ, 44_100), 120_000 << 8 | 48);
        assert_eq!(rational_best_approximation(31_415_926, 10_000_000, 1000, 1000), (355, 113));
        assert_eq!(mai_config(3), 0x0c00_0003);
        assert_eq!(channel_map(3), 0x10);
        assert_eq!(channel_map(0xff), 0x7654_3210);
        assert_eq!(audio_packet_config(3), 0x2100_2003);
        assert_eq!(crp_cfg(6144), 0x0100_1800);
        assert_eq!(dvp_gate_bit(0), 8);
    }

    #[test]
    fn n_cts_from_measured_clock() {
        use super::decode::acr_n_cts;
        assert_eq!(acr_n_cts(148_500, 48_000), (6144, 148_500));
        assert_eq!(acr_n_cts(148_500, 44_100), (6272, 165_000));
        assert_eq!(acr_n_cts(74_250, 48_000), (6144, 74_250));
        assert_eq!(acr_n_cts(25_200, 48_000), (6144, 25_200));
        assert_eq!(n_cts_fallback(148_500, 48_000), (6144, 148_500));
        // Fallback is self-consistent: 128 fs = pix N / CTS.
        let (n, cts) = n_cts_fallback(106_500, 48_000);
        assert_eq!(n, 6144);
        assert!((106_500_000u64 * n as u64 / cts as u64).abs_diff(128 * 48_000) < 128);
    }

    #[test]
    fn audio_infoframe_and_packet_ram_words() {
        let f = audio_infoframe(2);
        assert_eq!(f, [0x84, 0x01, 0x0a, 0x70, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(f.iter().fold(0u8, |s, &x| s.wrapping_add(x)), 0);
        let w = infoframe_words(&f);
        assert_eq!(w, [0x000a_0184, 0x0000_0170, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(AUDIO_PACKET_ID as usize * PACKET_STRIDE, 0x90);
    }

    #[test]
    fn ring_geometry_fits_dma40() {
        assert_eq!(RING_BYTES % BLOCK_BYTES, 0);
        assert_eq!(PERIOD_BYTES % BLOCK_BYTES, 0);
        assert_eq!(RING_BYTES % PERIOD_BYTES, 0);
        assert!(RING_BYTES <= 32 * 4096 && MAX_INFLIGHT + GUARD + RESYNC_PRIME < RING_BYTES as u64);
        let mut scbs = [super::cb::Scb::default(); 128];
        let d = super::cb::DreqCfg::from_cell(0x41fa_000a);
        let n = super::cb::build_cyclic_tx(&mut scbs, 0x4000_0000, 0x4010_0000, RING_BYTES, PERIOD_BYTES, 0x10_7c72_001c, &d, true);
        assert_eq!(n, Ok(PERIODS));
        // The 32-bit bus alias is ordinary DRAM on a >= 2 GiB board: the engine must refuse it.
        let n = super::cb::build_cyclic_tx(&mut scbs, 0x4000_0000, 0x4010_0000, RING_BYTES, PERIOD_BYTES, 0x7c72_001c, &d, true);
        assert_eq!(n, Err(super::cb::Dma40Error::AddrRange));
    }

    #[test]
    fn ring_advance_refill_and_underrun() {
        let mut r = Ring::default();
        // Idle stream: resync silently, no underrun.
        let a = r.advance(0);
        assert!(!a.starved);
        assert_eq!(r.written, RESYNC_PRIME);
        r.written += 20_000;
        r.was_ahead = true;
        let a = r.advance(8_000 + 3); // rounded down to a frame
        assert_eq!(a.refill, [(0, 8_000), (0, 0)]);
        assert_eq!(r.played, 8_000);
        assert!(!a.starved);
        // Wrap: from 8000 around to 4000.
        let a = r.advance(4_000);
        assert_eq!(a.refill, [(8_000, RING_BYTES), (0, 4_000)]);
        assert_eq!(r.played, RING_BYTES as u64 + 4_000 - 0);
        assert!(a.starved, "writer fell behind with queued audio");
        assert_eq!(r.underruns, 1);
        assert_eq!(r.written, r.played + RESYNC_PRIME);
        assert!(r.write_off() % FRAME_BYTES == 0);
        // Out-of-ring position is ignored.
        let before = r.played;
        r.advance(RING_BYTES + 8);
        assert_eq!(r.played, before);
        assert_eq!(r.space(), MAX_INFLIGHT - RESYNC_PRIME);
    }

    #[test]
    fn convert_channels_and_rates() {
        let pcm: Vec<u8> = [100i16, -100, 200, -200, 300, -300].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut rs = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        let (used, n) = convert_s16(&pcm, 2, true, &mut rs, 2, |l, r| out.push((l, r)));
        assert_eq!((used, n), (8, 2));
        assert_eq!(out, vec![(100 << 8, -100 << 8), (200 << 8, -200 << 8)]);
        // Mono duplicates.
        out.clear();
        let (used, n) = convert_s16(&pcm[..4], 1, true, &mut rs, 99, |l, r| out.push((l, r)));
        assert_eq!((used, n, out[1]), (4, 2, (-100 << 8, -100 << 8)));
        // 24 kHz -> 48 kHz doubles the frame count (linear interpolation).
        assert_eq!(hw_rate_for(24_000), 48_000);
        assert_eq!(hw_rate_for(44_100), 44_100);
        let ramp: Vec<u8> = (0..1000i16).flat_map(|i| [i * 10, i * 10]).flat_map(|s| s.to_le_bytes()).collect();
        let mut rs = Resampler::new(24_000, 48_000);
        out.clear();
        let (used, n) = convert_s16(&ramp, 2, false, &mut rs, 100_000, |l, r| out.push((l, r)));
        assert_eq!(used, ramp.len());
        assert_eq!(n, 2 * 999);
        assert_eq!(out[1].0, 5 << 8);
        assert!(out.windows(2).all(|w| w[1].0 >= w[0].0));
    }

    // ----- DREQ cell selection against the real Pi 5 device trees (dtc output, data/) -----

    fn dts_cells(text: &str) -> [u32; 2] {
        let mut out = [0u32; 2];
        let mut port = 0;
        for l in text.lines().filter(|l| l.contains("dmas")) {
            let v = l.split("0x16 0x").nth(1).unwrap().trim_end_matches(">;").trim();
            out[port] = u32::from_str_radix(v, 16).unwrap();
            port += 1;
        }
        assert_eq!(port, 2);
        out
    }

    #[test]
    fn dreq_cells_match_both_device_trees() {
        use super::cb::*;
        let c0 = dts_cells(include_str!("../data/dts-hdmi-c0.txt"));
        let d0 = dts_cells(include_str!("../data/dts-hdmi-d0.txt"));
        assert_eq!(c0, [hdmi_dreq_cell(false, 0), hdmi_dreq_cell(false, 1)]);
        assert_eq!(d0, [hdmi_dreq_cell(true, 0), hdmi_dreq_cell(true, 1)]);
        assert_eq!(DreqCfg::from_cell(hdmi_dreq_cell(true, 0)).per_map, 12);
        assert_eq!(DreqCfg::from_cell(hdmi_dreq_cell(true, 1)).per_map, 13);
        assert_eq!(DreqCfg::from_cell(hdmi_dreq_cell(false, 0)).per_map, 10);
        assert_eq!(DreqCfg::from_cell(hdmi_dreq_cell(false, 1)).per_map, 17);
        assert_eq!(cell_with_per_map(hdmi_dreq_cell(true, 0), 10), DREQ_CELL_HDMI0);
    }

    #[test]
    fn dreq_knob_parses() {
        assert_eq!(dreq_knob("quiet hdmi.dreq=12 x"), Some(12));
        assert_eq!(dreq_knob("hdmi.dreq=32"), None);
        assert_eq!(dreq_knob("hdmi.dreq=x"), None);
        assert_eq!(dreq_knob("hdmi.audio=0"), None);
    }

    // ----- RateMeter (probe verification) -----

    /// Engine at `mbpu` milli-bytes per microsecond from ring offset `start`, sampled every
    /// 500 us through the 5 ms skip and the 30 ms window; the verdict is taken once at the end.
    fn trace(mbpu: u64, start: usize, hw: u32) -> bool {
        let mut m = RateMeter::new();
        let at = |t: u64| start as u64 + mbpu * t / 1000; // unwrapped byte position
        let mut t = 0;
        while t <= (VERIFY_SKIP_MS + VERIFY_MEASURE_MS) * 1000 {
            let u = at(t);
            m.sample(Some((u % RING_BYTES as u64) as usize), u / PERIOD_BYTES as u64);
            if t == VERIFY_SKIP_MS * 1000 { m.mark(); }
            t += VERIFY_SAMPLE_US;
        }
        m.verdict(VERIFY_MEASURE_MS * 1000, hw)
    }

    const NOMINAL: u64 = 384; // 48 kHz stereo IEC958: 384 B/ms = 384 milli-B/us

    #[test]
    fn meter_accepts_the_hardware_rate_at_any_ring_offset_including_wrap() {
        for start in [0usize, 1000, 40_000, RING_BYTES - 5_000, RING_BYTES - 11_520, RING_BYTES - 8] {
            for pct in [90u64, 95, 100, 105, 110] {
                assert!(trace(NOMINAL * pct / 100, start & !7, 48_000), "start {} pct {}", start, pct);
            }
        }
        assert!(trace(353, 0, 44_100) && trace(353, RING_BYTES - 3_000, 44_100));
        assert!(trace(NOMINAL / 2, 100, 48_000) && trace(NOMINAL * 3 / 2, 100, 48_000), "50 % and 150 % edges pass");
    }

    #[test]
    fn meter_rejects_half_and_double_rate_and_a_stall() {
        assert!(!trace(NOMINAL * 45 / 100, 0, 48_000) && !trace(NOMINAL * 160 / 100, 0, 48_000));
        assert!(!trace(NOMINAL * 2, 0, 48_000) && !trace(NOMINAL / 4, 0, 48_000));
        assert!(!trace(0, 777 * 8, 48_000), "a stalled engine is silent");
        assert!(!trace(20, 0, 48_000), "a crawl under 2 KiB is silent");
    }

    /// The false pass the review simulated: a free-running engine measured mod the ring size.
    /// Raw speeds from 1.6x to 1000x, and speeds that alias to the nominal advance per sample.
    #[test]
    fn meter_rejects_a_racing_engine_at_many_speeds() {
        let mut speeds: Vec<u64> = (160..=400).step_by(20).map(|p| NOMINAL * p / 100).collect();
        speeds.extend([NOMINAL * 5, NOMINAL * 10, NOMINAL * 25, NOMINAL * 50, NOMINAL * 100, NOMINAL * 250, NOMINAL * 1000]);
        // One sample (500 us) advances k rings plus the nominal 192 B: alias of a paced engine.
        for k in 1..=6u64 { speeds.push((k * RING_BYTES as u64 + 192) * 2); }
        // And k rings less / plus a little, so the modular delta lands anywhere in 50..150 %.
        for k in 1..=3u64 { for d in [96u64, 144, 192, 240, 288] { speeds.push((k * RING_BYTES as u64 + d) * 2); } }
        for &sp in &speeds {
            for start in [0usize, 4096, RING_BYTES - 64] {
                assert!(!trace(sp, start, 48_000), "racing speed {} milli-B/us start {} passed", sp, start);
            }
        }
    }

    #[test]
    fn meter_decides_once_at_the_end_not_on_the_first_sample_in_range() {
        // Paced for the first 20 ms, then the engine races: an early return would have said ok.
        let mut m = RateMeter::new();
        let mut pos = 0u64;
        let mut t = 0;
        while t <= 35_000 {
            if t == 5_000 { m.mark(); }
            m.sample(Some((pos % RING_BYTES as u64) as usize), pos / PERIOD_BYTES as u64);
            pos += if t < 25_000 { 192 } else { 20_000 };
            t += 500;
        }
        assert!(!m.verdict(30_000, 48_000));
    }

    #[test]
    fn meter_needs_a_continuous_position_and_at_most_one_period() {
        let mk = |none_at: Option<u64>, periods_extra: u64| {
            let mut m = RateMeter::new();
            let mut t = 0u64;
            while t <= 35_000 {
                let u = 192 * t / 500;
                let pos = if Some(t) == none_at { None } else { Some((u % RING_BYTES as u64) as usize) };
                m.sample(pos, if t > 20_000 { periods_extra } else { 0 });
                if t == 5_000 { m.mark(); }
                t += 500;
            }
            m.verdict(30_000, 48_000)
        };
        assert!(mk(None, 0));
        assert!(mk(Some(1_000), 0), "a missing position before the mark is the start-up transient");
        assert!(!mk(Some(20_000), 0), "a missing position inside the window");
        assert!(mk(None, 1) && !mk(None, 2) && !mk(None, 9), "more than one completed period");
        // A step of half a ring or more is ambiguous.
        let mut m = RateMeter::new();
        m.sample(Some(0), 0); m.mark(); m.sample(Some(RING_BYTES / 2), 0); m.sample(Some(RING_BYTES / 2 + 11_520), 0);
        assert!(!m.verdict(30_000, 48_000));
        assert!(!RateMeter::new().verdict(30_000, 48_000), "never marked");
    }

    // ----- Ring::update (position unavailable) -----

    #[test]
    fn ring_update_none_counts_as_nothing_moved() {
        let mut r = Ring::default();
        r.written = 4096;
        let (adv, moved) = r.update(None);
        assert_eq!((moved, adv.refill, adv.starved, r.played), (0, [(0, 0); 2], false, 0));
        let (adv, moved) = r.update(Some(2048));
        assert_eq!((moved, adv.refill[0], r.played), (2048, (0, 2048), 2048));
    }

    // ----- setup_intact (firmware re-train beyond N) -----

    #[test]
    fn setup_intact_checks_more_than_n() {
        let (n, cts) = (6144u32, 148_500u32);
        let (crp, c0, apc, mai) = (crp_cfg(n), cts, audio_packet_config(3), mai_config(3));
        assert!(setup_intact(crp, c0, apc, mai, n, cts, 3));
        assert!(setup_intact(crp | 0x00f0_0000, c0, apc | 0x1_0000, mai, n, cts, 3), "unrelated bits are ignored");
        assert!(!setup_intact(crp_cfg(n + 1), c0, apc, mai, n, cts, 3), "N changed");
        assert!(!setup_intact(0, c0, apc, mai, n, cts, 3), "CRP cleared");
        assert!(!setup_intact(crp, 0, apc, mai, n, cts, 3), "CTS lost with N intact");
        assert!(!setup_intact(crp, c0, 0, mai, n, cts, 3), "audio packet config lost with N intact");
        assert!(!setup_intact(crp, c0, apc, 0, n, cts, 3), "MAI config lost with N intact");
    }

    // ----- InfoframeRetry -----

    #[test]
    fn infoframe_retry_never_blocks_and_logs_once() {
        let mut r = InfoframeRetry::new();
        assert_eq!(r.tick(false), InfoframeStep::Nothing, "nothing owed");
        assert!(r.defer(), "first deferral is logged");
        assert!(r.is_pending());
        assert!(!{ r.done(); r.defer() }, "later deferrals are not");
        assert_eq!(r.tick(true), InfoframeStep::Disable, "stop the slot first");
        assert_eq!(r.tick(true), InfoframeStep::Nothing, "still busy: wait for a later tick");
        assert_eq!(r.tick(false), InfoframeStep::Write, "idle: rewrite and re-enable");
        assert!(!r.is_pending());
        assert_eq!(r.tick(false), InfoframeStep::Nothing);
    }

    #[test]
    fn infoframe_retry_gives_up_and_re_enables_the_old_frame() {
        let mut r = InfoframeRetry::new();
        r.defer();
        assert_eq!(r.tick(true), InfoframeStep::Disable);
        for _ in 0..INFOFRAME_RETRY_TICKS - 1 { assert_eq!(r.tick(true), InfoframeStep::Nothing); }
        assert_eq!(r.tick(true), InfoframeStep::GiveUp);
        assert!(!r.is_pending());
    }

    // ----- StallPolicy (wall clock) -----

    const MS: u64 = 1_000_000;
    const S: u64 = 1_000 * MS;

    #[test]
    fn stall_is_time_based_not_call_count() {
        let mut p = StallPolicy::new();
        // A constant clock never stalls however often update() runs.
        for _ in 0..10_000 { assert_eq!(p.observe(5 * S, 0), StallAction::None); }
        // Two calls 300 ms apart do.
        let mut q = StallPolicy::new();
        assert_eq!(q.observe(S, 0), StallAction::None);
        assert_eq!(q.observe(S + 299 * MS, 0), StallAction::None);
        assert_eq!(q.observe(S + 300 * MS, 0), StallAction::Restart(1));
    }

    #[test]
    fn single_glitch_recovers_after_300_ms() {
        let mut p = StallPolicy::new();
        for i in 0..30 { assert_eq!(p.observe(i * 10 * MS, 0), StallAction::None); }
        assert_eq!(p.observe(300 * MS, 0), StallAction::Restart(1));
        // Audio flows again: 4 KiB since the restart re-arms the whole ladder.
        assert_eq!(p.observe(400 * MS, 4096), StallAction::None);
        assert_eq!(p.restarts(), 0);
        for i in 0..30 { assert_eq!(p.observe(S + i * 10 * MS, 0), StallAction::None); }
        assert_eq!(p.observe(S + 300 * MS, 0), StallAction::Restart(1));
    }

    #[test]
    fn movement_restarts_the_idle_clock() {
        let mut p = StallPolicy::new();
        p.observe(0, 0);
        p.observe(250 * MS, 0);
        p.observe(260 * MS, 8);
        assert_eq!(p.observe(559 * MS, 0), StallAction::None);
        assert_eq!(p.observe(560 * MS, 0), StallAction::Restart(1));
    }

    #[test]
    fn dead_after_five_restarts_with_512_bytes_each_and_one_dead_action() {
        let mut p = StallPolicy::new();
        let mut now = 0u64;
        p.observe(now, 0);
        now += 300 * MS;
        assert_eq!(p.observe(now, 0), StallAction::Restart(1));
        let waits = [1, 2, 4, 8, 8];
        let mut restarts = 1;
        let mut dead = 0;
        for (k, w) in waits.iter().enumerate() {
            // 512 B per restart is not progress (< 4 KiB), and nothing happens before the wait.
            assert_eq!(p.observe(now + w * S - 1, 512), StallAction::None, "wait {}", k);
            now += w * S;
            match p.observe(now, 0) {
                StallAction::Restart(n) => { restarts += 1; assert_eq!(n, restarts); }
                StallAction::Dead => dead += 1,
                StallAction::None => panic!("expected an action at wait {}", k),
            }
        }
        assert_eq!((restarts, dead), (5, 1));
        assert!(p.is_dead());
        for i in 0..100 { assert_eq!(p.observe(now + i * S, 0), StallAction::None, "dead is reported once"); }
        assert_eq!(p.request(now + 1000 * S), StallAction::None);
    }

    #[test]
    fn progress_resets_the_ladder() {
        let mut p = StallPolicy::new();
        p.observe(0, 0);
        p.observe(300 * MS, 0);
        assert_eq!(p.restarts(), 1);
        assert_eq!(p.observe(S / 2, 3000), StallAction::None); // 3000 B: not yet
        assert_eq!(p.observe(S * 9 / 10, 1096), StallAction::None); // 4096 B total: progress
        assert_eq!((p.restarts(), p.is_dead()), (0, false));
    }

    fn kill_through_ladder(p: &mut StallPolicy) -> u64 {
        let mut now = 0;
        p.observe(now, 0);
        now += 300 * MS;
        p.observe(now, 0);
        for w in [1, 2, 4, 8, 8] { now += w * S; p.observe(now, 0); }
        assert!(p.is_dead());
        now
    }

    #[test]
    fn rearm_after_dead_by_a_retrain_is_the_full_ladder() {
        let mut p = StallPolicy::new();
        let now = kill_through_ladder(&mut p);
        p.rearm();
        assert!(!p.is_dead() && p.restarts() == 0 && !p.on_probation());
        p.observe(now, 0);
        assert_eq!(p.observe(now + 300 * MS, 0), StallAction::Restart(1));
        assert_eq!(p.observe(now + 300 * MS + S, 0), StallAction::Restart(2), "ladder continues");
    }

    #[test]
    fn set_params_after_death_gets_one_restart_then_dead() {
        let mut p = StallPolicy::new();
        let now = kill_through_ladder(&mut p);
        p.rearm_client();
        assert!(!p.is_dead() && p.on_probation());
        // The reconfigure itself was the restart; a second stall goes straight to dead.
        p.observe(now, 0);
        assert_eq!(p.observe(now + 299 * MS, 0), StallAction::None);
        assert_eq!(p.observe(now + 300 * MS, 0), StallAction::Dead);
        assert!(p.is_dead());
        assert_eq!(p.observe(now + 400 * MS, 0), StallAction::None, "reported once");
        // The external detector on probation also goes straight to dead, not to a restart.
        p.rearm_client();
        assert_eq!(p.request(now + 500 * MS), StallAction::Dead);
    }

    #[test]
    fn probation_ends_with_real_progress_and_the_full_ladder_returns() {
        let mut p = StallPolicy::new();
        let now = kill_through_ladder(&mut p);
        p.rearm_client();
        p.observe(now, 0);
        assert_eq!(p.observe(now + 10 * MS, 2048), StallAction::None);
        assert!(p.on_probation(), "2 KiB is not progress");
        assert_eq!(p.observe(now + 20 * MS, 2048), StallAction::None);
        assert!(!p.on_probation(), "4 KiB is");
        p.observe(now + 20 * MS, 0);
        assert_eq!(p.observe(now + 320 * MS, 0), StallAction::Restart(1), "a stall now restarts, not dies");
    }

    #[test]
    fn retrain_ends_probation() {
        let mut p = StallPolicy::new();
        let now = kill_through_ladder(&mut p);
        p.rearm_client();
        p.rearm();
        assert!(!p.on_probation());
        p.observe(now, 0);
        assert_eq!(p.observe(now + 300 * MS, 0), StallAction::Restart(1));
    }

    #[test]
    fn set_params_on_a_healthy_stream_stays_a_full_rearm() {
        let mut p = StallPolicy::new();
        p.observe(0, 0);
        p.observe(300 * MS, 0);
        assert_eq!(p.restarts(), 1);
        p.rearm_client();
        assert!(!p.on_probation() && p.restarts() == 0);
    }

    #[test]
    fn external_detector_goes_through_the_same_ladder() {
        let mut p = StallPolicy::new();
        assert_eq!(p.request(0), StallAction::Restart(1));
        // Another 250 ms detector hit inside the 1 s backoff is ignored.
        assert_eq!(p.request(250 * MS), StallAction::None);
        assert_eq!(p.request(500 * MS), StallAction::None);
        assert_eq!(p.request(S), StallAction::Restart(2));
        let mut now = S;
        for (w, want) in [(2, StallAction::Restart(3)), (4, StallAction::Restart(4)), (8, StallAction::Restart(5)), (8, StallAction::Dead)] {
            now += w * S;
            assert_eq!(p.request(now), want);
        }
        assert_eq!(p.request(now + 100 * S), StallAction::None);
    }

    // ----- NullSink -----

    #[test]
    fn null_sink_drains_at_exactly_the_wall_clock_rate() {
        let bps = 48_000 * 2 * 2; // S16 stereo 48 kHz
        let mut n = NullSink::new();
        n.start(5 * S);
        let mut total = 0usize;
        // 1 s in 7 ms steps with a writer always offering more than it can drain.
        let mut now = 5 * S;
        while now < 6 * S { now = (now + 7 * MS).min(6 * S); total += n.drain(now, 1 << 20, bps); }
        assert_eq!(total, bps as usize);
        assert_eq!(n.consumed, bps);
        // Nothing offered, nothing consumed; the backlog is capped at 100 ms.
        assert_eq!(n.drain(6 * S, 0, bps), 0);
        assert_eq!(n.drain(60 * S, 1 << 30, bps), bps as usize / 10);
        // An odd rate keeps its remainder instead of rounding each call.
        let mut m = NullSink::new();
        m.start(0);
        let mut got = 0;
        for i in 1..=1000u64 { got += m.drain(i * MS, 1 << 20, 44_100 * 3); }
        assert_eq!(got, 44_100 * 3);
    }
}
