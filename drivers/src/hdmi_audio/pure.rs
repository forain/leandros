//! Pure parts of the BCM2712 HDMI audio backend: IEC958 subframe packing, the MAI / CRP /
//! audio-packet register images (Linux `vc4_hdmi_audio_prepare`, gen 6), the audio InfoFrame,
//! rate conversion and the cyclic-ring arithmetic. No MMIO and no allocation, so the host tests
//! in `drivers/hdmi-audio-tests` include this file verbatim.

// ---------------------------------------------------------------------------------------------
// Ring geometry
// ---------------------------------------------------------------------------------------------

/// IEC 60958 block: channel status is 192 bits, one per frame.
pub const FRAMES_PER_BLOCK: usize = 192;
/// One stereo frame = two 32-bit IEC958 subframes.
pub const FRAME_BYTES: usize = 8;
pub const BLOCK_BYTES: usize = FRAMES_PER_BLOCK * FRAME_BYTES; // 1536
/// The ring holds a whole number of IEC958 blocks, so a ring offset fixes the frame's position
/// inside its block: preamble Z and the channel-status bit are a function of the offset alone,
/// and a consumed region can be refilled with valid silence from one template.
pub const RING_BLOCKS: usize = 64;
pub const RING_BYTES: usize = BLOCK_BYTES * RING_BLOCKS; // 98304 = 24 pages, 512 ms at 48 kHz
pub const PERIODS: usize = 8;
pub const PERIOD_BYTES: usize = RING_BYTES / PERIODS; // 12288 = 64 ms at 48 kHz
/// Producer limit ahead of the play position (~170 ms at 48 kHz, as HDA's 32 KiB of S16).
pub const MAX_INFLIGHT: u64 = 64 * 1024;
/// Bytes the engine and the MAI FIFO may hold ahead of the reported position.
pub const GUARD: u64 = 1024;
/// Where a starved writer restarts, ahead of the play position (8 ms at 48 kHz).
pub const RESYNC_PRIME: u64 = 2 * BLOCK_BYTES as u64;

// ---------------------------------------------------------------------------------------------
// IEC958 subframes (alsa-lib pcm_iec958.c, IEC958_SUBFRAME_LE)
// ---------------------------------------------------------------------------------------------

/// alsa-lib default preambles. `AUDIO_PACKET_CONFIG.B_FRAME_IDENTIFIER = 0x8` matches Z.
pub const PREAMBLE_Z: u32 = 0x08;
pub const PREAMBLE_X: u32 = 0x02;
pub const PREAMBLE_Y: u32 = 0x04;
pub const SUBFRAME_C: u32 = 1 << 30;
pub const SUBFRAME_P: u32 = 1 << 31;

/// IEC 60958-3 consumer channel status for linear PCM: 24 bytes, bit `i` of the block is
/// `cs[i / 8] >> (i % 8) & 1`.
pub fn channel_status(fs: u32, bits: u8) -> [u8; 24] {
    let mut cs = [0u8; 24];
    cs[0] = 0x04; // consumer, linear PCM, no copyright asserted, no pre-emphasis
    cs[1] = 0x82; // category: PCM encoder/decoder, original (IEC958_AES1_CON_PCM_CODER|ORIGINAL)
    cs[2] = 0x00; // source / channel number not specified
    cs[3] = fs_code(fs); // clock accuracy level II
    let (wl, orig) = (
        match bits {
            24 => 0x0b, // max 24 bits, 24 bits
            20 => 0x0a, // max 24 bits, 20 bits
            _ => 0x02,  // max 20 bits, 16 bits
        },
        match fs {
            44_100 => 0xf0,
            48_000 => 0xd0,
            32_000 => 0xc0,
            88_200 => 0x70,
            96_000 => 0x50,
            _ => 0x00,
        },
    );
    cs[4] = wl | orig;
    cs
}

/// Channel-status byte 3 sampling-frequency code.
pub fn fs_code(fs: u32) -> u8 {
    match fs {
        44_100 => 0x00,
        48_000 => 0x02,
        32_000 => 0x03,
        88_200 => 0x08,
        96_000 => 0x0a,
        176_400 => 0x0c,
        192_000 => 0x0e,
        _ => 0x01, // not indicated
    }
}

#[inline]
pub fn cs_bit(cs: &[u8; 24], frame: usize) -> bool {
    let f = frame % FRAMES_PER_BLOCK;
    cs[f / 8] >> (f % 8) & 1 != 0
}

/// One subframe. `sample` is a signed 24-bit value (S16 sources pass `s << 8`); it lands in
/// bits 4..27, C in 30, even parity over 4..30 in 31, the preamble nibble in 0..3.
#[inline]
pub fn subframe(sample: i32, channel: usize, frame_in_block: usize, cs: &[u8; 24]) -> u32 {
    let mut d = ((sample as u32) & 0x00ff_ffff) << 4;
    if cs_bit(cs, frame_in_block) {
        d |= SUBFRAME_C;
    }
    if (d & 0x7fff_fff0).count_ones() & 1 != 0 {
        d |= SUBFRAME_P;
    }
    d | if channel != 0 {
        PREAMBLE_Y
    } else if frame_in_block % FRAMES_PER_BLOCK == 0 {
        PREAMBLE_Z
    } else {
        PREAMBLE_X
    }
}

#[inline]
pub fn s16_to_24(s: i16) -> i32 {
    (s as i32) << 8
}

/// Little-endian S24 in 3 bytes (S24_3LE) to a signed 24-bit value.
#[inline]
pub fn s24le_to_24(b: [u8; 3]) -> i32 {
    ((b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16) << 8) as i32 >> 8
}

/// Both subframes of a stereo frame at ring byte offset `off` (a multiple of 8).
#[inline]
pub fn pack_frame(l: i32, r: i32, off: usize, cs: &[u8; 24]) -> [u32; 2] {
    let f = (off / FRAME_BYTES) % FRAMES_PER_BLOCK;
    [subframe(l, 0, f, cs), subframe(r, 1, f, cs)]
}

/// One block of valid digital silence (zero samples, correct preambles, C and P bits).
pub fn silence_block(cs: &[u8; 24], out: &mut [u32; FRAMES_PER_BLOCK * 2]) {
    for f in 0..FRAMES_PER_BLOCK {
        out[2 * f] = subframe(0, 0, f, cs);
        out[2 * f + 1] = subframe(0, 1, f, cs);
    }
}

// ---------------------------------------------------------------------------------------------
// Register images (vc4_regs.h; Linux vc4_hdmi.c, gen 6)
// ---------------------------------------------------------------------------------------------

/// `hd` block offsets for hdmi0 (hdmi1: MAI_CTL/DATA/SMP at 0x30/0x3c/0x40).
pub const HD_MAI_CTL: usize = 0x10;
pub const HD_MAI_THR: usize = 0x14;
pub const HD_MAI_FMT: usize = 0x18;
pub const HD_MAI_DATA: usize = 0x1c;
pub const HD_MAI_SMP: usize = 0x20;
/// `hdmi` block offsets.
pub const HDMI_MAI_CHANNEL_MAP: usize = 0x0a4;
pub const HDMI_MAI_CONFIG: usize = 0x0a8;
pub const HDMI_AUDIO_PACKET_CONFIG: usize = 0x0c0;
pub const HDMI_RAM_PACKET_CONFIG: usize = 0x0c4;
pub const HDMI_RAM_PACKET_STATUS: usize = 0x0cc;
pub const HDMI_CRP_CFG: usize = 0x0d0;
pub const HDMI_CTS_0: usize = 0x0d4;
pub const HDMI_CTS_1: usize = 0x0d8;
/// Vertical timing registers (read-only here, logged raw for the pixel-clock follow-up).
pub const HDMI_VERTA0: usize = 0x0f4;
pub const HDMI_VERTB0: usize = 0x0f8;
pub const HDMI_VERTA1: usize = 0x0fc;
pub const HDMI_VERTB1: usize = 0x100;
/// DVP clock block: MISC_CONFIG bit 3 + port gates the port's audio clock (set = closed).
pub const DVP_MISC_CONFIG: usize = 0x08;
pub fn dvp_gate_bit(port: usize) -> u32 {
    1 << (3 + port)
}

pub const MAI_CTL_RESET: u32 = 1 << 0;
pub const MAI_CTL_ERRORF: u32 = 1 << 1;
pub const MAI_CTL_ERRORE: u32 = 1 << 2;
pub const MAI_CTL_ENABLE: u32 = 1 << 3;
pub const MAI_CTL_FLUSH: u32 = 1 << 9;
pub const MAI_CTL_WHOLSMP: u32 = 1 << 12;
pub const MAI_CTL_CHALIGN: u32 = 1 << 13;
pub const MAI_CTL_DLATE: u32 = 1 << 15;
/// Write-1-to-clear error bits.
pub const MAI_CTL_ERRS: u32 = MAI_CTL_ERRORE | MAI_CTL_ERRORF | MAI_CTL_DLATE;

/// `vc4_hdmi_audio_startup`.
pub const fn mai_ctl_startup() -> u32 {
    MAI_CTL_RESET | MAI_CTL_FLUSH | MAI_CTL_DLATE | MAI_CTL_ERRORE | MAI_CTL_ERRORF
}
/// `vc4_hdmi_audio_prepare`: CHNUM(ch) | WHOLSMP | CHALIGN | ENABLE.
pub const fn mai_ctl_run(ch: u32) -> u32 {
    (ch & 0xf) << 4 | MAI_CTL_WHOLSMP | MAI_CTL_CHALIGN | MAI_CTL_ENABLE
}

/// MAI_THR panic-high/low 0x10, DREQ-high/low 0x1c. BCM2712 D0 (`VC4_GEN_6_D`) uses the
/// 7-bit `VC6_D_HD_MAI_THR_*` fields at 23/15/7/0; C0/C1 the gen-5 6-bit fields at 24/16/8/0.
pub const fn mai_thr(d0: bool) -> u32 {
    if d0 {
        0x10 << 23 | 0x10 << 15 | 0x1c << 7 | 0x1c
    } else {
        0x10 << 24 | 0x10 << 16 | 0x1c << 8 | 0x1c
    }
}

/// MAI_FMT: SAMPLE_RATE code (15:8) | AUDIO_FORMAT PCM = 2 (23:16).
pub fn mai_fmt(fs: u32) -> Option<u32> {
    let code = match fs {
        8000 => 1,
        11_025 => 2,
        12_000 => 3,
        16_000 => 4,
        22_050 => 5,
        24_000 => 6,
        32_000 => 7,
        44_100 => 8,
        48_000 => 9,
        64_000 => 10,
        88_200 => 11,
        96_000 => 12,
        128_000 => 13,
        176_400 => 14,
        192_000 => 15,
        _ => return None,
    };
    Some(code << 8 | 2 << 16)
}

/// Linux `rational_best_approximation` (lib/math/rational.c).
pub fn rational_best_approximation(gn: u64, gd: u64, max_n: u64, max_d: u64) -> (u64, u64) {
    let (mut n, mut d) = (gn, gd);
    let (mut n0, mut d1) = (0u64, 0u64);
    let (mut n1, mut d0) = (1u64, 1u64);
    loop {
        if d == 0 {
            break;
        }
        let dp = d;
        let a = n / d;
        d = n % d;
        n = dp;
        let n2 = n0 + a * n1;
        let d2 = d0 + a * d1;
        if n2 > max_n || d2 > max_d {
            let mut t = u64::MAX;
            if d1 != 0 {
                t = (max_d - d0) / d1;
            }
            if n1 != 0 {
                t = t.min((max_n - n0) / n1);
            }
            if d1 == 0 || 2 * t > a || (2 * t == a && d0 * dp > d1 * d) {
                n1 = n0 + t * n1;
                d1 = d0 + t * d1;
            }
            break;
        }
        n0 = n1;
        n1 = n2;
        d0 = d1;
        d1 = d2;
    }
    (n1, d1)
}

/// The DVP audio clock: fixed 108 MHz parent (`clk-bcm2711-dvp`, phandle 0x11 in the DTB).
pub const AUDIO_CLOCK_HZ: u64 = 108_000_000;

/// MAI_SMP = N (31:8) | M - 1 (7:0) with N/M ~ audio_clock / fs (`vc4_hdmi_audio_set_mai_clock`).
pub fn mai_smp(clock: u64, fs: u32) -> u32 {
    let (n, m) = rational_best_approximation(clock, fs as u64, 0x00ff_ffff, 0x100);
    ((n as u32) << 8) | (m.max(1) as u32 - 1)
}

pub const MAI_CONFIG_BIT_REVERSE: u32 = 1 << 26;
pub const MAI_CONFIG_FORMAT_REVERSE: u32 = 1 << 27;
pub const fn mai_config(mask: u32) -> u32 {
    MAI_CONFIG_BIT_REVERSE | MAI_CONFIG_FORMAT_REVERSE | (mask & 0xffff)
}

/// `vc5_hdmi_channel_map`: channel i -> slot i.
pub fn channel_map(mask: u32) -> u32 {
    (0..8).filter(|i| mask & (1 << i) != 0).fold(0, |m, i| m | (i as u32) << (4 * i))
}

/// ZERO_DATA_ON_SAMPLE_FLAT | ZERO_DATA_ON_INACTIVE_CHANNELS | B_FRAME_IDENTIFIER(8) | CEA_MASK.
pub const fn audio_packet_config(mask: u32) -> u32 {
    1 << 29 | 1 << 24 | 0x8 << 10 | (mask & 0xff)
}

pub const CRP_CFG_EXTERNAL_CTS_EN: u32 = 1 << 24;
pub const fn crp_cfg(n: u32) -> u32 {
    CRP_CFG_EXTERNAL_CTS_EN | (n & 0xf_ffff)
}

/// N/CTS for the ACR packets. Spec-table values come from the probe (`acr_n_cts`); for a clock
/// that table lacks, Linux `vc4_hdmi_set_n_cts`: N = 128 fs / 1000, CTS = pix N / (128 fs).
pub fn n_cts_fallback(pix_khz: u32, fs: u32) -> (u32, u32) {
    let n = 128 * fs / 1000;
    let cts = (pix_khz as u64 * 1000 * n as u64 / (128 * fs as u64)) as u32;
    (n, cts)
}

// ---------------------------------------------------------------------------------------------
// Audio InfoFrame (CEA-861 type 0x84) in packet RAM
// ---------------------------------------------------------------------------------------------

pub const PACKET_STRIDE: usize = 0x24;
pub const AUDIO_PACKET_ID: u32 = 0x84 - 0x80; // 4
pub const RAM_PACKET_ENABLE: u32 = 1 << 16;

/// Header (type, version 1, length 10), checksum, PB1..PB10: CT = 0 (refer to stream),
/// CC = channels - 1, SF/SS = 0, CA = 0 (FL/FR), level shift 0.
pub fn audio_infoframe(channels: u8) -> [u8; 14] {
    let mut b = [0u8; 14];
    b[0] = 0x84;
    b[1] = 0x01;
    b[2] = 0x0a;
    b[4] = channels.saturating_sub(1) & 7;
    let sum = b.iter().fold(0u8, |s, &x| s.wrapping_add(x));
    b[3] = 0u8.wrapping_sub(sum);
    b
}

/// The packet-RAM words `vc4_hdmi_write_infoframe` writes: per 7 bytes a 3-byte then a 4-byte
/// word, the rest of the 0x24-byte slot zeroed.
pub fn infoframe_words(frame: &[u8]) -> [u32; PACKET_STRIDE / 4] {
    let mut buf = [0u8; 28];
    buf[..frame.len().min(28)].copy_from_slice(&frame[..frame.len().min(28)]);
    let mut w = [0u32; PACKET_STRIDE / 4];
    let mut k = 0;
    let mut i = 0;
    while i < frame.len() && k + 1 < w.len() {
        w[k] = buf[i] as u32 | (buf[i + 1] as u32) << 8 | (buf[i + 2] as u32) << 16;
        w[k + 1] = buf[i + 3] as u32 | (buf[i + 4] as u32) << 8 | (buf[i + 5] as u32) << 16 | (buf[i + 6] as u32) << 24;
        k += 2;
        i += 7;
    }
    w
}

// ---------------------------------------------------------------------------------------------
// Ring bookkeeping (the HDA model: monotonic played/written, LPIB = DMA source position)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct Ring {
    pub played: u64,
    pub written: u64,
    pub last_pos: usize,
    pub was_ahead: bool,
    pub underruns: u64,
}

/// What `Ring::advance` asks the caller to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advance {
    /// Consumed byte ranges to refill with silence: `[a, b)` then `[c, d)` (wrap), maybe empty.
    pub refill: [(usize, usize); 2],
    /// The writer fell behind while it had queued audio (an underrun worth counting).
    pub starved: bool,
}

impl Ring {
    pub fn reset(&mut self) {
        *self = Ring { underruns: self.underruns, ..Ring::default() };
    }

    pub fn queued(&self) -> u64 {
        self.written.saturating_sub(self.played)
    }

    pub fn space(&self) -> u64 {
        MAX_INFLIGHT.saturating_sub(self.queued())
    }

    /// Ring offset of the next write.
    pub fn write_off(&self) -> usize {
        (self.written % RING_BYTES as u64) as usize
    }

    /// Position update where the position may be unavailable (the engine is not yet inside the
    /// ring): returns the advance and the bytes played by it. `None` counts as nothing moved.
    pub fn update(&mut self, pos: Option<usize>) -> (Advance, u64) {
        let Some(pos) = pos else { return (Advance { refill: [(0, 0); 2], starved: false }, 0) };
        let before = self.played;
        let adv = self.advance(pos);
        (adv, self.played - before)
    }

    /// New engine position `pos` (byte offset; rounded down to a frame).
    pub fn advance(&mut self, pos: usize) -> Advance {
        let mut out = Advance { refill: [(0, 0); 2], starved: false };
        if pos >= RING_BYTES {
            return out;
        }
        let pos = pos & !(FRAME_BYTES - 1);
        let delta = (pos + RING_BYTES - self.last_pos) % RING_BYTES;
        if delta > 0 {
            let a = self.last_pos;
            out.refill = if pos > a { [(a, pos), (0, 0)] } else { [(a, RING_BYTES), (0, pos)] };
            self.played += delta as u64;
            self.last_pos = pos;
        }
        if self.written < self.played + GUARD {
            if self.was_ahead {
                self.underruns += 1;
                out.starved = true;
            }
            self.was_ahead = false;
            self.written = self.played + GUARD.max(RESYNC_PRIME);
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Input conversion: interleaved S16LE at any rate / channel count -> stereo 24-bit at hw rate
// ---------------------------------------------------------------------------------------------

/// Native MAI rates we program; anything else is resampled to 48 kHz.
pub fn hw_rate_for(in_rate: u32) -> u32 {
    match in_rate {
        44_100 | 48_000 => in_rate,
        _ => 48_000,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Resampler {
    pub step: u32,
    pub frac: u32,
    pub prev: [i32; 2],
    pub primed: bool,
}

impl Resampler {
    pub fn new(in_rate: u32, hw_rate: u32) -> Self {
        Resampler { step: ((in_rate as u64 * 65536) / hw_rate.max(1) as u64) as u32, frac: 0, prev: [0; 2], primed: false }
    }
}

/// Converts whole input frames from `data` while `out_frames` allows and calls `put(l, r)` per
/// output frame. Returns `(input bytes consumed, output frames produced)`.
pub fn convert_s16(data: &[u8], ch: usize, same_rate: bool, rs: &mut Resampler, out_frames: usize,
                   mut put: impl FnMut(i32, i32)) -> (usize, usize) {
    let ch = ch.max(1);
    let in_frame = ch * 2;
    let frames = data.len() / in_frame;
    let rd = |i: usize, c: usize| -> i32 {
        let o = i * in_frame + c * 2;
        s16_to_24(i16::from_le_bytes([data[o], data[o + 1]]))
    };
    let (mut consumed, mut produced) = (0usize, 0usize);
    if same_rate {
        while consumed < frames && produced < out_frames {
            let l = rd(consumed, 0);
            put(l, if ch >= 2 { rd(consumed, 1) } else { l });
            consumed += 1;
            produced += 1;
        }
        return (consumed * in_frame, produced);
    }
    let max_out = (65536 / rs.step.max(1)) as usize + 2;
    while consumed < frames && produced + max_out <= out_frames {
        let l = rd(consumed, 0);
        let r = if ch >= 2 { rd(consumed, 1) } else { l };
        if !rs.primed {
            rs.prev = [l, r];
            rs.primed = true;
            consumed += 1;
            continue;
        }
        let [pl, pr] = rs.prev;
        while rs.frac < 65536 {
            let f = rs.frac as i64;
            let ol = pl as i64 + (((l - pl) as i64 * f) >> 16);
            let or = pr as i64 + (((r - pr) as i64 * f) >> 16);
            put(ol as i32, or as i32);
            produced += 1;
            rs.frac += rs.step;
        }
        rs.frac -= 65536;
        rs.prev = [l, r];
        consumed += 1;
    }
    (consumed * in_frame, produced)
}

// ---------------------------------------------------------------------------------------------
// DMA pacing verification (probe) and the stall policy (runtime)
// ---------------------------------------------------------------------------------------------

/// `hdmi.dreq=N` (0..=31): forces the HDMI audio PER_MAP; the cell flags still come from the table.
pub fn dreq_knob(cmdline: &str) -> Option<u32> {
    cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix("hdmi.dreq="))
        .and_then(|v| v.parse::<u32>().ok()).filter(|&n| n < 32)
}

/// Probe verification: skip the start-up transient, then measure over a fixed window.
pub const VERIFY_SKIP_MS: u64 = 5;
pub const VERIFY_MEASURE_MS: u64 = 30;
/// Position sampling period (us).
pub const VERIFY_SAMPLE_US: u64 = 500;
/// At or below this many bytes the engine is considered silent (not paced by the FIFO).
pub const VERIFY_SILENT_BYTES: u64 = 2048;
/// More than this many periods completing inside the window means the engine is not paced.
pub const VERIFY_MAX_PERIODS: u64 = 1;

/// Accumulates the engine's position samples into an unwrapped byte count and decides once.
///
/// Every sample adds the forward distance from the previous one (mod the ring) to a `u64`, so a
/// racing engine cannot hide behind the ring size: its total grows without bound while a paced
/// one stays at the hardware rate. A single step of half a ring or more is ambiguous (the
/// engine may have lapped) and fails at once, as does a lost position inside the window and a
/// window with more than [`VERIFY_MAX_PERIODS`] completed periods.
#[derive(Clone, Copy, Debug, Default)]
pub struct RateMeter {
    last: Option<usize>,
    total: u64,
    mark_total: u64,
    mark_periods: u64,
    periods: u64,
    marked: bool,
    bad: bool,
}

impl RateMeter {
    pub const fn new() -> Self {
        RateMeter { last: None, total: 0, mark_total: 0, mark_periods: 0, periods: 0, marked: false, bad: false }
    }

    /// One sample (every [`VERIFY_SAMPLE_US`], the start-up skip included). `periods` is the
    /// channel's completed-period counter.
    pub fn sample(&mut self, pos: Option<usize>, periods: u64) {
        self.periods = periods;
        match pos {
            Some(p) if p < RING_BYTES => {
                if let Some(l) = self.last {
                    let d = (p + RING_BYTES - l) % RING_BYTES;
                    if d >= RING_BYTES / 2 { self.bad = true; }
                    self.total += d as u64;
                }
                self.last = Some(p);
            }
            _ => if self.marked { self.bad = true; },
        }
    }

    /// End of the start-up transient: everything counted so far is excluded from the verdict.
    pub fn mark(&mut self) {
        self.marked = true;
        self.mark_total = self.total;
        self.mark_periods = self.periods;
    }

    /// Bytes moved since the mark (unwrapped).
    pub fn moved(&self) -> u64 { self.total - self.mark_total }
    pub fn periods_since_mark(&self) -> u64 { self.periods.saturating_sub(self.mark_periods) }

    /// The verdict, taken once at the end of the window of `elapsed_us` since the mark: more
    /// than 2 KiB moved, 50..=150 % of `hw_rate * 8` bytes per second, a continuous position and
    /// at most one completed period.
    pub fn verdict(&self, elapsed_us: u64, hw_rate: u32) -> bool {
        if !self.marked || self.bad || elapsed_us == 0 || self.periods_since_mark() > VERIFY_MAX_PERIODS { return false; }
        let moved = self.moved();
        if moved <= VERIFY_SILENT_BYTES { return false; }
        let got = moved * 1_000_000;
        let want = hw_rate as u64 * FRAME_BYTES as u64 * elapsed_us;
        got * 2 >= want && got * 2 <= want * 3
    }
}

/// Re-train check: does the programmed audio setup still read back? CRP_CFG must keep the
/// external-CTS enable and N, CTS_0 its value, and the audio packet / MAI config registers all
/// the bits we set (a firmware re-train that keeps N can still drop the rest).
pub fn setup_intact(crp: u32, cts0: u32, apc: u32, mai_cfg: u32, n: u32, cts: u32, mask: u32) -> bool {
    let (want_apc, want_mai) = (audio_packet_config(mask), mai_config(mask));
    crp & 0x010f_ffff == crp_cfg(n)
        && cts0 & 0xf_ffff == cts & 0xf_ffff
        && apc & want_apc == want_apc
        && mai_cfg & want_mai == want_mai
}

/// The audio InfoFrame rewrite when the packet slot was busy: retried without waiting, one
/// tick at a time. `Disable` stops the slot, `Write` rewrites and re-enables it once idle,
/// `GiveUp` re-enables the old contents after too many busy ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoframeStep { Nothing, Disable, Write, GiveUp }

pub const INFOFRAME_RETRY_TICKS: u32 = 100;

#[derive(Clone, Copy, Debug, Default)]
pub struct InfoframeRetry {
    pending: bool,
    disabled: bool,
    busy_ticks: u32,
    logged: bool,
}

impl InfoframeRetry {
    pub const fn new() -> Self { InfoframeRetry { pending: false, disabled: false, busy_ticks: 0, logged: false } }
    pub fn is_pending(&self) -> bool { self.pending }
    /// The blocking write found the slot busy. True the first time only (the caller logs once).
    pub fn defer(&mut self) -> bool {
        self.pending = true;
        self.disabled = false;
        self.busy_ticks = 0;
        let first = !self.logged;
        self.logged = true;
        first
    }
    /// A blocking write succeeded: nothing is owed any more.
    pub fn done(&mut self) { self.pending = false; self.disabled = false; self.busy_ticks = 0; }
    /// One tick; `slot_busy` is the packet status bit (read non-blockingly).
    pub fn tick(&mut self, slot_busy: bool) -> InfoframeStep {
        if !self.pending { return InfoframeStep::Nothing; }
        if !self.disabled { self.disabled = true; return InfoframeStep::Disable; }
        if !slot_busy { self.done(); return InfoframeStep::Write; }
        self.busy_ticks += 1;
        if self.busy_ticks >= INFOFRAME_RETRY_TICKS { self.done(); return InfoframeStep::GiveUp; }
        InfoframeStep::Nothing
    }
}

/// Wait before the next action after restart number `k` (1-based): 1, 2, 4, 8 s, then 8 s again.
pub const fn restart_wait_ns(k: u32) -> u64 {
    const S: u64 = 1_000_000_000;
    match k { 0 | 1 => S, 2 => 2 * S, 3 => 4 * S, _ => 8 * S }
}
/// Time without any played byte before the first restart of a stall (wall clock, not ticks:
/// update() runs from the timer IRQ and from every write, at no fixed rate).
pub const STALL_NS: u64 = 300_000_000;
/// Restarts without progress before the stream is declared dead.
pub const STALL_MAX_RESTARTS: u32 = 5;
/// Bytes played since a restart that count as progress.
pub const STALL_PROGRESS_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallAction {
    None,
    /// Restart the stream; the number is the restart count so far (1-based).
    Restart(u32),
    /// Give up: stop DMA and MAI, drain through the null sink. Returned once.
    Dead,
}

/// Decides when a stalled stream is restarted and when it is given up on. Pure: the caller
/// supplies the clock and the bytes played since the previous call.
///
/// After a death a client reconfigure puts the policy on probation: the reconfigure itself is
/// the one restart, a second stall goes straight back to dead, and only real progress
/// ([`STALL_PROGRESS_BYTES`] played) or a firmware re-train ([`StallPolicy::rearm`]) restores
/// the full ladder.
#[derive(Clone, Copy, Debug, Default)]
pub struct StallPolicy {
    restarts: u32,
    last_move_ns: u64,
    seen: bool,
    since: u64,
    deadline_ns: u64,
    dead: bool,
    probation: bool,
}

impl StallPolicy {
    pub const fn new() -> Self {
        StallPolicy { restarts: 0, last_move_ns: 0, seen: false, since: 0, deadline_ns: 0, dead: false, probation: false }
    }
    pub fn is_dead(&self) -> bool { self.dead }
    pub fn restarts(&self) -> u32 { self.restarts }
    pub fn on_probation(&self) -> bool { self.probation }
    /// Marks the stream dead without a stall (a restart that could not even start).
    pub fn kill(&mut self) { self.dead = true; }
    /// A firmware re-train starts over completely, including out of dead and probation.
    pub fn rearm(&mut self) { *self = StallPolicy::new(); }
    /// A client reconfigure: starts over, but out of dead (or still on probation) it only
    /// earns the one restart the reconfigure itself performs.
    pub fn rearm_client(&mut self) {
        let was = self.dead || self.probation;
        *self = StallPolicy::new();
        self.probation = was;
    }

    fn restart(&mut self, now_ns: u64) -> StallAction {
        self.restarts += 1;
        self.since = 0;
        self.deadline_ns = now_ns + restart_wait_ns(self.restarts);
        StallAction::Restart(self.restarts)
    }

    fn die(&mut self) -> StallAction {
        self.dead = true;
        StallAction::Dead
    }

    fn after_wait(&mut self, now_ns: u64) -> StallAction {
        if now_ns < self.deadline_ns { return StallAction::None; }
        if self.restarts >= STALL_MAX_RESTARTS { return self.die(); }
        self.restart(now_ns)
    }

    /// One update tick: `moved` bytes played since the previous call (0 when the position is
    /// unavailable).
    pub fn observe(&mut self, now_ns: u64, moved: u64) -> StallAction {
        if self.dead { return StallAction::None; }
        if self.restarts == 0 {
            if self.probation {
                self.since += moved;
                if self.since >= STALL_PROGRESS_BYTES { *self = StallPolicy::new(); }
            }
            if !self.seen || moved > 0 {
                self.seen = true;
                self.last_move_ns = now_ns;
                return StallAction::None;
            }
            if now_ns.saturating_sub(self.last_move_ns) < STALL_NS { return StallAction::None; }
            if self.probation { return self.die(); }
            return self.restart(now_ns);
        }
        self.since += moved;
        if self.since >= STALL_PROGRESS_BYTES {
            *self = StallPolicy::new();
            return StallAction::None;
        }
        self.after_wait(now_ns)
    }

    /// An external detector (PipeWire: no TX progress for 250 ms) reports a stall. The first one
    /// restarts at once; later ones only once the backoff since the last restart has passed.
    pub fn request(&mut self, now_ns: u64) -> StallAction {
        if self.dead { return StallAction::None; }
        if self.probation && self.restarts == 0 { return self.die(); }
        if self.restarts == 0 { return self.restart(now_ns); }
        self.after_wait(now_ns)
    }
}

/// Wall-clock sink used while dead: it accepts input at the stream's own rate so a blocked
/// writer keeps moving. Exact: the sub-byte remainder is carried.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink {
    last_ns: u64,
    credit: u128, // byte * ns
    pub consumed: u64,
}

impl NullSink {
    pub const fn new() -> Self { NullSink { last_ns: 0, credit: 0, consumed: 0 } }
    /// Starts the clock at `now_ns`.
    pub fn start(&mut self, now_ns: u64) { self.last_ns = now_ns; self.credit = 0; }
    /// Consumes up to `offered` bytes at `bytes_per_sec`; never more than 100 ms of backlog.
    pub fn drain(&mut self, now_ns: u64, offered: usize, bytes_per_sec: u64) -> usize {
        let dt = now_ns.saturating_sub(self.last_ns);
        self.last_ns = self.last_ns.max(now_ns);
        self.credit += dt as u128 * bytes_per_sec as u128;
        let cap = bytes_per_sec as u128 * 100_000_000;
        if self.credit > cap { self.credit = cap; }
        let avail = (self.credit / 1_000_000_000) as usize;
        let n = avail.min(offered);
        self.credit -= n as u128 * 1_000_000_000;
        self.consumed += n as u64;
        n
    }
}
