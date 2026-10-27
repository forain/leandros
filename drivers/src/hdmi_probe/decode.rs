//! Pure decoders for the read-only BCM2712 HDMI probe (`hdmi_probe.rs`).
//!
//! No hardware access and no allocation: register words in, numbers out, so the
//! host tests in `drivers/hdmi-probe-tests` run exactly this code. Field layouts
//! are the gen-5/6 ones from Linux `vc4_hdmi.c` / `vc4_regs.h` (`VC5_HDMI_*`
//! timing fields, `VC4_HDMI_*` scheduler/CRP fields, `VC4_HD_MAI_CTL_*`).

// ── Register offsets (vc6 `hdmi` block, same for hdmi0 and hdmi1) ─────────────
pub const HDMI_FIFO_CTL: usize = 0x07c;
pub const HDMI_MAI_CHANNEL_MAP: usize = 0x0a4;
pub const HDMI_MAI_CONFIG: usize = 0x0a8;
pub const HDMI_AUDIO_PACKET_CONFIG: usize = 0x0c0;
pub const HDMI_RAM_PACKET_CONFIG: usize = 0x0c4;
pub const HDMI_RAM_PACKET_STATUS: usize = 0x0cc;
pub const HDMI_CRP_CFG: usize = 0x0d0;
pub const HDMI_CTS_0: usize = 0x0d4;
pub const HDMI_SCHEDULER_CONTROL: usize = 0x0e8;
pub const HDMI_HORZA: usize = 0x0ec;
pub const HDMI_HORZB: usize = 0x0f0;
pub const HDMI_VERTA0: usize = 0x0f4;
pub const HDMI_VERTB0: usize = 0x0f8;
pub const HDMI_HOTPLUG: usize = 0x1c8;
pub const HDMI_BLOCK_LEN: usize = 0x300;

/// `hd` block offsets per port: (MAI_CTL, MAI_FMT, MAI_SMP, FRAME_COUNT).
pub const HD_PORT_REGS: [(usize, usize, usize, usize); 2] = [(0x10, 0x18, 0x20, 0x60), (0x30, 0x38, 0x40, 0x64)];
pub const HD_BLOCK_LEN: usize = 0x100;

/// `dvp` block (`brcm,brcm2711-dvp`): reset lines and the 108 MHz audio gates.
pub const DVP_SW_INIT: usize = 0x04;
pub const DVP_MISC_CONFIG: usize = 0x08;
pub const DVP_BLOCK_LEN: usize = 0x10;

// ── Scheduler (HDMI vs DVI) ───────────────────────────────────────────────────
pub const SCHED_MODE_HDMI: u32 = 1 << 0;
pub const SCHED_HDMI_ACTIVE: u32 = 1 << 1;
pub const SCHED_MANUAL_FORMAT: u32 = 1 << 15;

pub const HOTPLUG_CONNECTED: u32 = 1 << 0;

/// The DVP's 108 MHz audio clock gate for `port`: `clk-bcm2711-dvp.c` registers
/// "hdmiN-108MHz" on MISC_CONFIG bit 3+N with `CLK_GATE_SET_TO_DISABLE`, so a
/// set bit means the gate is CLOSED.
pub fn dvp_audio_gate_open(misc_config: u32, port: usize) -> bool {
    misc_config & (1 << (3 + port)) == 0
}

/// The raw SW_INIT bit of the DVP reset line of `port` (DT `resets = <&dvp 1>` /
/// `<&dvp 2>`). Its polarity is not settled for BCM2712 (reset-simple, possibly
/// active-low), so the probe logs the bit rather than an interpretation.
pub fn dvp_reset_bit(sw_init: u32, port: usize) -> u32 {
    (sw_init >> (1 + port)) & 1
}

// ── MAI / packetizer ──────────────────────────────────────────────────────────
pub const MAI_CTL_RESET: u32 = 1 << 0;
pub const MAI_CTL_ERRORF: u32 = 1 << 1;
pub const MAI_CTL_ERRORE: u32 = 1 << 2;
pub const MAI_CTL_ENABLE: u32 = 1 << 3;
pub const MAI_CTL_EMPTY: u32 = 1 << 10;
pub const MAI_CTL_FULL: u32 = 1 << 11;
pub const MAI_CTL_BUSY: u32 = 1 << 14;
pub const CRP_CFG_EXTERNAL_CTS_EN: u32 = 1 << 24;
pub const CRP_CFG_DISABLE: u32 = 1 << 25;
pub const RAM_PACKET_ENABLE: u32 = 1 << 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaiState {
    pub enabled: bool,
    pub channels: u32,
    pub busy: bool,
    pub underflow: bool,
    pub overflow: bool,
    pub empty: bool,
    pub full: bool,
}

pub fn decode_mai_ctl(v: u32) -> MaiState {
    MaiState {
        enabled: v & MAI_CTL_ENABLE != 0,
        channels: (v >> 4) & 0xf,
        busy: v & MAI_CTL_BUSY != 0,
        underflow: v & MAI_CTL_ERRORE != 0,
        overflow: v & MAI_CTL_ERRORF != 0,
        empty: v & MAI_CTL_EMPTY != 0,
        full: v & MAI_CTL_FULL != 0,
    }
}

/// CRP_CFG: (N, external CTS enabled, CRP disabled).
pub fn decode_crp(v: u32) -> (u32, bool, bool) {
    (v & 0xf_ffff, v & CRP_CFG_EXTERNAL_CTS_EN != 0, v & CRP_CFG_DISABLE != 0)
}

/// RAM_PACKET_CONFIG: the global enable and the per-packet enable mask (bits 0..15;
/// packet id = InfoFrame type - 0x80, so AVI = 2, audio = 4).
pub fn decode_ram_packets(v: u32) -> (bool, u16) {
    (v & RAM_PACKET_ENABLE != 0, (v & 0xffff) as u16)
}

// ── Timing ────────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timing {
    pub hactive: u32,
    pub htotal: u32,
    pub vactive: u32,
    pub vtotal: u32,
    pub hsync_pos: bool,
    pub vsync_pos: bool,
}

/// Inverse of `vc5_hdmi_set_timings` (pixel_rep 1, progressive; VERTB0 holds the
/// even-field back porch, which equals the progressive one).
pub fn decode_timing(horza: u32, horzb: u32, verta0: u32, vertb0: u32) -> Timing {
    let hap = horza & 0x3fff;
    let hfp = (horza >> 16) & 0x1fff;
    let hsp = horzb & 0x7ff;
    let hbp = (horzb >> 16) & 0x7ff;
    let val = verta0 & 0x1fff;
    let vfp = (verta0 >> 16) & 0x7f;
    let vsp = (verta0 >> 24) & 0x1f;
    let vbp = vertb0 & 0x1ff;
    Timing {
        hactive: hap,
        htotal: hap + hfp + hsp + hbp,
        vactive: val,
        vtotal: val + vfp + vsp + vbp,
        hsync_pos: horza & (1 << 14) != 0,
        vsync_pos: horza & (1 << 15) != 0,
    }
}

/// Frame rate in millihertz from `frames` counted over `ns` nanoseconds.
pub fn fps_milli(frames: u32, ns: u64) -> u32 {
    if ns == 0 {
        return 0;
    }
    ((frames as u64 * 1_000_000_000_000 + ns / 2) / ns) as u32
}

/// Raw pixel clock in kHz: htotal x vtotal x fps.
pub fn pixclk_khz_raw(t: &Timing, fps_milli: u32) -> u32 {
    ((t.htotal as u64 * t.vtotal as u64 * fps_milli as u64 + 500_000) / 1_000_000) as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockClass {
    /// One of the CEA-861 / HDMI-spec clocks of the ACR tables.
    Cea,
    /// A common VESA DMT / CVT-RB clock (not in the ACR tables).
    Dmt,
    /// Nothing within tolerance: the raw measurement.
    Raw,
}

/// CEA clocks of the HDMI 1.4b / 2.0 ACR tables, in kHz (x/1.001 rounded).
pub const CEA_KHZ: [u32; 14] = [25175, 25200, 27000, 27027, 54000, 54054, 74176, 74250, 148352, 148500, 296703, 297000, 593407, 594000];
/// Common monitor clocks (VESA DMT / CVT-RB), in kHz.
pub const DMT_KHZ: [u32; 14] = [31500, 40000, 49500, 65000, 71000, 78750, 85500, 106500, 108000, 119000, 138500, 146250, 154000, 241500];

/// Snaps a measured clock to the nearest standard one within 0.5 %.
/// The 1.001 pairs are 0.1 % apart, so a measurement must be better than
/// 0.05 % to tell them apart; the edge-timed window gives that.
pub fn snap_pixclk(raw_khz: u32) -> (u32, ClockClass) {
    let mut best: Option<(u32, ClockClass, u32)> = None;
    for (list, class) in [(&CEA_KHZ, ClockClass::Cea), (&DMT_KHZ, ClockClass::Dmt)] {
        for &c in list.iter() {
            let d = c.abs_diff(raw_khz);
            if d as u64 * 200 <= c as u64 && best.map_or(true, |(_, _, bd)| d < bd) {
                best = Some((c, class, d));
            }
        }
    }
    match best {
        Some((c, class, _)) => (c, class),
        None => (raw_khz, ClockClass::Raw),
    }
}

// ── ACR: N / CTS (HDMI 1.4b tables 7-1..7-3, HDMI 2.0 for 594 MHz) ─────────────
/// Recommended N for (CEA clock kHz, fs Hz); None for a clock outside the tables.
pub fn acr_n(pix_khz: u32, fs: u32) -> Option<u32> {
    let col = match fs {
        32000 => 0,
        44100 => 1,
        48000 => 2,
        _ => return None,
    };
    let row: [u32; 3] = match pix_khz {
        25175 => [4576, 7007, 6864],
        25200 | 27000 | 27027 | 54000 | 54054 | 74250 | 148500 => [4096, 6272, 6144],
        74176 => [11648, 17836, 11648],
        148352 => [11648, 8918, 5824],
        296703 => [5824, 4459, 5824],
        297000 => [3072, 4704, 5120],
        593407 => [5824, 8918, 5824],
        594000 => [3072, 9408, 6144],
        _ => return None,
    };
    Some(row[col])
}

/// Exact pixel clock in Hz as a fraction (num, den): the x/1.001 clocks are
/// base x 1000/1001.
fn exact_hz(pix_khz: u32) -> (u64, u64) {
    match pix_khz {
        25175 => (25_200_000_000, 1001),
        27027 => (27_027_000, 1),
        54054 => (54_054_000, 1),
        74176 => (74_250_000_000, 1001),
        148352 => (148_500_000_000, 1001),
        296703 => (297_000_000_000, 1001),
        593407 => (594_000_000_000, 1001),
        k => (k as u64 * 1000, 1),
    }
}

/// (N, CTS) for the clock and sample rate: the spec table for CEA clocks, else
/// Linux's `N = 128 fs / 1000`. CTS = f_pix N / (128 fs), rounded.
pub fn acr_n_cts(pix_khz: u32, fs: u32) -> (u32, u32) {
    let n = acr_n(pix_khz, fs).unwrap_or(128 * fs / 1000);
    let (num, den) = exact_hz(pix_khz);
    let d = den * 128 * fs as u64;
    let cts = ((num * n as u64 + d / 2) / d) as u32;
    (n, cts)
}

// ── EDID ──────────────────────────────────────────────────────────────────────
/// Extension count of an EDID base block (None if the header is wrong).
pub fn edid_extensions(block0: &[u8]) -> Option<u8> {
    const HDR: [u8; 8] = [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0];
    if block0.len() < 128 || block0[..8] != HDR {
        return None;
    }
    Some(block0[126])
}

/// True if a CEA-861 extension block carries the HDMI 1.x vendor-specific data
/// block (IEEE OUI 00-0C-03): the sink is HDMI, not DVI.
pub fn cea_has_hdmi_vsdb(ext: &[u8]) -> bool {
    if ext.len() < 128 || ext[0] != 0x02 {
        return false;
    }
    let end = (ext[2] as usize).min(127);
    let mut i = 4;
    while i < end {
        let tag = ext[i] >> 5;
        let len = (ext[i] & 0x1f) as usize;
        if tag == 3 && len >= 3 && i + 3 < 128 && ext[i + 1] == 0x03 && ext[i + 2] == 0x0c && ext[i + 3] == 0x00 {
            return true;
        }
        i += 1 + len;
    }
    false
}
