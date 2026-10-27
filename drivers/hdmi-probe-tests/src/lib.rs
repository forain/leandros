//! Host tests for the HDMI probe. `decode.rs` is included verbatim, so these
//! exercise exactly the code the kernel runs.

#[path = "../../src/hdmi_probe/decode.rs"]
pub mod decode;

#[cfg(test)]
mod tests {
    use super::decode::*;

    /// Encodes a mode the way Linux `vc5_hdmi_set_timings` does (progressive, pixel_rep 1).
    fn encode(hd: u32, hss: u32, hse: u32, ht: u32, vd: u32, vss: u32, vse: u32, vt: u32, hpos: bool, vpos: bool) -> (u32, u32, u32, u32) {
        let horza = (vpos as u32) << 15 | (hpos as u32) << 14 | hd | (hss - hd) << 16;
        let horzb = (ht - hse) << 16 | (hse - hss);
        let verta = (vse - vss) << 24 | (vss - vd) << 16 | vd;
        let vertb0 = vt - vse;
        (horza, horzb, verta, vertb0)
    }

    #[test]
    fn timing_1080p60() {
        let (a, b, c, d) = encode(1920, 2008, 2052, 2200, 1080, 1084, 1089, 1125, true, true);
        let t = decode_timing(a, b, c, d);
        assert_eq!((t.hactive, t.htotal, t.vactive, t.vtotal), (1920, 2200, 1080, 1125));
        assert!(t.hsync_pos && t.vsync_pos);
    }

    #[test]
    fn timing_720p_and_vga_and_4k() {
        let (a, b, c, d) = encode(1280, 1390, 1430, 1650, 720, 725, 730, 750, true, true);
        let t = decode_timing(a, b, c, d);
        assert_eq!((t.htotal, t.vtotal), (1650, 750));
        let (a, b, c, d) = encode(640, 656, 752, 800, 480, 490, 492, 525, false, false);
        let t = decode_timing(a, b, c, d);
        assert_eq!((t.hactive, t.htotal, t.vactive, t.vtotal), (640, 800, 480, 525));
        assert!(!t.hsync_pos && !t.vsync_pos);
        let (a, b, c, d) = encode(3840, 4016, 4104, 4400, 2160, 2168, 2178, 2250, true, true);
        let t = decode_timing(a, b, c, d);
        assert_eq!((t.hactive, t.htotal, t.vactive, t.vtotal), (3840, 4400, 2160, 2250));
    }

    #[test]
    fn fps_and_pixclk_snap() {
        let t = Timing { hactive: 1920, htotal: 2200, vactive: 1080, vtotal: 1125, ..Default::default() };
        // 6 frames edge to edge at exactly 60 Hz.
        let f = fps_milli(6, 100_000_000);
        assert_eq!(f, 60_000);
        assert_eq!(snap_pixclk(pixclk_khz_raw(&t, f)), (148500, ClockClass::Cea));
        // 59.94 Hz: 6 frames in 100.1 ms.
        let f = fps_milli(6, 100_100_000);
        assert_eq!(f, 59_940);
        assert_eq!(snap_pixclk(pixclk_khz_raw(&t, f)), (148352, ClockClass::Cea));
        // 50 Hz 1080p (htotal 2640) is 148.5 too.
        let t50 = Timing { htotal: 2640, ..t };
        assert_eq!(snap_pixclk(pixclk_khz_raw(&t50, fps_milli(5, 100_000_000))).0, 148500);
        // 0.2 % poll jitter still snaps.
        assert_eq!(snap_pixclk(148_800), (148500, ClockClass::Cea));
        // 4k60, 720p60, VGA.
        assert_eq!(snap_pixclk(4400 * 2250 * 60 / 1000), (594000, ClockClass::Cea));
        assert_eq!(snap_pixclk(1650 * 750 * 60 / 1000), (74250, ClockClass::Cea));
        assert_eq!(snap_pixclk((800u64 * 525 * 59940 / 1_000_000) as u32), (25175, ClockClass::Cea));
        // A DMT monitor mode (1280x1024@60) and an unknown one.
        assert_eq!(snap_pixclk(108_050), (108000, ClockClass::Dmt));
        assert_eq!(snap_pixclk(200_000), (200_000, ClockClass::Raw));
        assert_eq!(fps_milli(3, 0), 0);
    }

    #[test]
    fn n_cts_hdmi14_tables() {
        // (clock kHz, fs, N, CTS) from HDMI 1.4b tables 7-1..7-3 and HDMI 2.0 for 594 MHz.
        let rows: &[(u32, u32, u32, u32)] = &[
            (25175, 32000, 4576, 28125), (25175, 44100, 7007, 31250), (25175, 48000, 6864, 28125),
            (25200, 32000, 4096, 25200), (25200, 44100, 6272, 28000), (25200, 48000, 6144, 25200),
            (27000, 32000, 4096, 27000), (27000, 44100, 6272, 30000), (27000, 48000, 6144, 27000),
            (27027, 48000, 6144, 27027), (54000, 44100, 6272, 60000), (54054, 48000, 6144, 54054),
            (74176, 44100, 17836, 234375), (74176, 48000, 11648, 140625),
            (74250, 32000, 4096, 74250), (74250, 44100, 6272, 82500), (74250, 48000, 6144, 74250),
            (148352, 32000, 11648, 421875), (148352, 44100, 8918, 234375), (148352, 48000, 5824, 140625),
            (148500, 32000, 4096, 148500), (148500, 44100, 6272, 165000), (148500, 48000, 6144, 148500),
            (296703, 32000, 5824, 421875), (296703, 44100, 4459, 234375), (296703, 48000, 5824, 281250),
            (297000, 32000, 3072, 222750), (297000, 44100, 4704, 247500), (297000, 48000, 5120, 247500),
            (593407, 32000, 5824, 843750), (593407, 44100, 8918, 937500), (593407, 48000, 5824, 562500),
            (594000, 32000, 3072, 445500), (594000, 44100, 9408, 990000), (594000, 48000, 6144, 594000),
        ];
        for &(k, fs, n, cts) in rows {
            assert_eq!(acr_n_cts(k, fs), (n, cts), "{} kHz {} Hz", k, fs);
        }
        // 74.25/1.001 at 32 kHz alternates 210937/210938.
        assert_eq!(acr_n_cts(74176, 32000).0, 11648);
        assert!((210937..=210938).contains(&acr_n_cts(74176, 32000).1));
        // Outside the tables: Linux's N = 128 fs / 1000.
        assert_eq!(acr_n_cts(108000, 48000), (6144, 108000));
        assert_eq!(acr_n(108000, 48000), None);
        assert_eq!(acr_n(148500, 96000), None);
    }

    #[test]
    fn dvp_mai_crp_decoders() {
        assert!(dvp_audio_gate_open(0, 0) && dvp_audio_gate_open(0, 1));
        assert!(!dvp_audio_gate_open(1 << 3, 0) && dvp_audio_gate_open(1 << 3, 1));
        assert!(!dvp_audio_gate_open(1 << 4, 1));
        assert_eq!((dvp_reset_bit(0b010, 0), dvp_reset_bit(0b010, 1)), (1, 0));
        let m = decode_mai_ctl(MAI_CTL_ENABLE | (2 << 4) | MAI_CTL_ERRORE | (1 << 12) | (1 << 13));
        assert!(m.enabled && m.underflow && !m.overflow);
        assert_eq!(m.channels, 2);
        assert_eq!(decode_crp(CRP_CFG_EXTERNAL_CTS_EN | 6144), (6144, true, false));
        assert_eq!(decode_ram_packets(RAM_PACKET_ENABLE | (1 << 2) | (1 << 4)), (true, 0x14));
    }

    fn edid_base(ext: u8) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        b[126] = ext;
        b
    }

    #[test]
    fn edid_hdmi_vs_dvi() {
        assert_eq!(edid_extensions(&edid_base(1)), Some(1));
        assert_eq!(edid_extensions(&[0u8; 128]), None);
        // CEA ext: video data block (tag 2, len 3), audio (tag 1, len 3), HDMI VSDB (tag 3, len 5).
        let mut e = [0u8; 128];
        e[0] = 0x02;
        e[1] = 0x03;
        let blocks: [u8; 15] = [0x43, 0x10, 0x04, 0x03, 0x23, 0x09, 0x07, 0x07, 0x65, 0x03, 0x0c, 0x00, 0x10, 0x00, 0x00];
        e[4..4 + blocks.len()].copy_from_slice(&blocks);
        e[2] = (4 + blocks.len()) as u8;
        assert!(cea_has_hdmi_vsdb(&e));
        // Same without the VSDB: DVI-only (or HDMI-less CEA) sink.
        let mut d = e;
        d[8 + 4] = 0x00; // corrupt the OUI
        assert!(!cea_has_hdmi_vsdb(&d));
        let mut short = e;
        short[2] = 12; // descriptor area ends before the VSDB
        assert!(!cea_has_hdmi_vsdb(&short));
        let mut notcea = e;
        notcea[0] = 0x10;
        assert!(!cea_has_hdmi_vsdb(&notcea));
    }
}
