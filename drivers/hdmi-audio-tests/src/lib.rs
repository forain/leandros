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
        for fifo in [0x10_7c72_001cu64, 0x7c72_001c] {
            let n = super::cb::build_cyclic_tx(&mut scbs, 0x4000_0000, 0x4010_0000, RING_BYTES, PERIOD_BYTES, fifo, &d, true);
            assert_eq!(n, Ok(PERIODS));
        }
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
}
