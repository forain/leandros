//! Intel High Definition Audio (HDA) playback driver.
//!
//! Written from the public Intel High Definition Audio Specification rev 1.0a
//! (controller registers §3, codec verbs/parameters §7); Linux sound/pci/hda
//! and FreeBSD sys/dev/sound/pci/hda were read as documentation only.
//!
//! Scope: one playback stream, 16-bit stereo PCM, on the first controller
//! that has a codec with an analog output path. It plugs into the in-kernel
//! audio server (servers/pipewire) behind the same five calls the virtio-sound
//! backend offers (`reconfigure_stream`, `send_pcm_data`, `tx_level`,
//! `tx_used_idx`, `top_up_silence`, `recover_stream`), so PipeWire's
//! leandros-snd-sink, aplay and DOOM see no difference.
//!
//! Design:
//! - Controller: CRST reset, CORB/RIRB rings (immediate-command interface as
//!   the fallback when the rings time out), DMA position buffer for
//!   diagnostics, interrupts left disabled — the audio server's 100 Hz tick
//!   pump polls LPIB, which is all a ring-buffer playback stream needs.
//! - Codec: every codec in STATESTS is walked (root → AFG → widgets); output
//!   pins come from the BIOS pin configuration defaults (line out, speaker,
//!   headphone; jack, fixed or both), each gets the shortest path back to an
//!   analog DAC through mixers/selectors. ALL chosen DACs listen to the same
//!   stream tag, so rear line-out, front headphone and any speaker play the
//!   same stereo signal at once (no auto-mute, no jack retasking).
//! - Stream: a 64 KiB cyclic buffer (16 BDL entries of 4 KiB) in DMA-able
//!   memory below 4 GiB. Software keeps two monotonic byte counters, `played`
//!   (from LPIB deltas) and `written`; the region the DMA engine has passed is
//!   zeroed on every update, so a producer underrun plays silence instead of
//!   looping stale audio, and the writer is re-synchronised just ahead of the
//!   DMA fetch point. The ring always holds 16-bit stereo at a rate the codec
//!   supports; mono input is duplicated, unsupported rates are linearly
//!   resampled to 48 kHz.

use super::pci;
use crate::snd::monotonic_us;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{fence, Ordering};
use mm::{buddy, phys_to_virt};
use mm::paging::{map_kernel_device, PageFlags};

/// Same unit as the virtio backend's TX buffer, for the server's
/// level/used-index arithmetic.
pub const TX_BUF_BYTES: usize = crate::snd::TX_BUF_BYTES;

// ── Controller registers (spec §3.3) ─────────────────────────────────────────
const GCAP: usize = 0x00;
const VMIN: usize = 0x02;
const VMAJ: usize = 0x03;
const GCTL: usize = 0x08;
const STATESTS: usize = 0x0E;
const INTCTL: usize = 0x20;
const INTSTS: usize = 0x24;
const SSYNC: usize = 0x38;
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48;
const CORBRP: usize = 0x4A;
const CORBCTL: usize = 0x4C;
const CORBSIZE: usize = 0x4E;
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58;
const RINTCNT: usize = 0x5A;
const RIRBCTL: usize = 0x5C;
const RIRBSTS: usize = 0x5D;
const RIRBSIZE: usize = 0x5E;
const ICOI: usize = 0x60;
const ICII: usize = 0x64;
const ICIS: usize = 0x68;
const DPLBASE: usize = 0x70;
const DPUBASE: usize = 0x74;
const SD_BASE: usize = 0x80;
const SD_STRIDE: usize = 0x20;
// Stream descriptor register offsets.
const SD_CTL0: usize = 0x00; // bits 7:0 of CTL: SRST, RUN, IOCE, FEIE, DEIE
const SD_CTL2: usize = 0x02; // bits 23:16 of CTL: stripe, TP, DIR, stream tag (7:4)
const SD_STS: usize = 0x03;
const SD_LPIB: usize = 0x04;
const SD_CBL: usize = 0x08;
const SD_LVI: usize = 0x0C;
const SD_FIFOS: usize = 0x10;
const SD_FMT: usize = 0x12;
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1C;

const GCTL_CRST: u32 = 1 << 0;
const SD_CTL_SRST: u8 = 1 << 0;
const SD_CTL_RUN: u8 = 1 << 1;
const SD_STS_BCIS: u8 = 1 << 2;
const SD_STS_FIFOE: u8 = 1 << 3;
const SD_STS_DESE: u8 = 1 << 4;

// ── Codec verbs (spec §7.3) ──────────────────────────────────────────────────
const V_GET_PARAM: u32 = 0xF00;
const V_GET_CONN_SEL: u32 = 0xF01;
const V_SET_CONN_SEL: u32 = 0x701;
const V_GET_CONN_LIST: u32 = 0xF02;
const V_SET_POWER: u32 = 0x705;
const V_SET_STREAM: u32 = 0x706;
const V_GET_PIN_CTL: u32 = 0xF07;
const V_SET_PIN_CTL: u32 = 0x707;
const V_SET_UNSOL: u32 = 0x708;
const V_GET_PIN_SENSE: u32 = 0xF09;
const V_EXEC_PIN_SENSE: u32 = 0x709;
const V_GET_EAPD: u32 = 0xF0C;
const V_SET_EAPD: u32 = 0x70C;
const V_GET_CFG_DEFAULT: u32 = 0xF1C;
const V_GET_SUBSYS: u32 = 0xF20;
// 4-bit verbs (16-bit payload).
const V4_SET_FMT: u32 = 0x2;
const V4_SET_AMP: u32 = 0x3;
const V4_GET_AMP: u32 = 0xB;

const P_VENDOR: u32 = 0x00;
const P_REVISION: u32 = 0x02;
const P_SUBNODES: u32 = 0x04;
const P_FG_TYPE: u32 = 0x05;
const P_WCAPS: u32 = 0x09;
const P_PCM: u32 = 0x0A;
const P_PINCAP: u32 = 0x0C;
const P_AMP_IN: u32 = 0x0D;
const P_CONNLEN: u32 = 0x0E;
const P_AMP_OUT: u32 = 0x12;

const W_DAC: u8 = 0;
#[allow(dead_code)]
const W_ADC: u8 = 1;
const W_MIXER: u8 = 2;
const W_SELECTOR: u8 = 3;
const W_PIN: u8 = 4;

const WCAP_IN_AMP: u32 = 1 << 1;
const WCAP_OUT_AMP: u32 = 1 << 2;
const WCAP_AMP_OVRD: u32 = 1 << 3;
const WCAP_FMT_OVRD: u32 = 1 << 4;
const WCAP_CONN_LIST: u32 = 1 << 8;
const WCAP_DIGITAL: u32 = 1 << 9;
const WCAP_POWER: u32 = 1 << 10;

const PINCAP_TRIG_REQ: u32 = 1 << 1;
const PINCAP_PRES_DETECT: u32 = 1 << 2;
const PINCAP_HP_DRIVE: u32 = 1 << 3;
const PINCAP_OUT: u32 = 1 << 4;
const PINCAP_HDMI: u32 = 1 << 7;
const PINCAP_EAPD: u32 = 1 << 16;
const PINCAP_DP: u32 = 1 << 24;

const PINCTL_OUT_EN: u32 = 1 << 6;
const PINCTL_HP_EN: u32 = 1 << 7;

// ── Stream buffer geometry ──────────────────────────────────────────────────
const BDL_ENTRIES: usize = 16;
const BDL_CHUNK: usize = 4096;
const RING_BYTES: usize = BDL_ENTRIES * BDL_CHUNK; // 64 KiB ≈ 341 ms at 48 kHz
/// Bytes the writer may run ahead of the DMA engine (≈ 170 ms at 48 kHz
/// stereo). Above the PipeWire sink's 4096-frame (16 KiB) delay target, so
/// its spool stays nearly empty and the delay it reads is the ring's.
const MAX_INFLIGHT: u64 = 32 * 1024;
/// Silence cushion a starved (or idle) writer is re-synchronised to: the
/// counterpart of virtio-sound's 16 KiB top-up watermark (about 85 ms at
/// 48 kHz). PipeWire's leandros-snd-sink steers its delay to 4096 frames
/// (16 KiB) and converges far too slowly from an empty queue, so without it
/// every stream start ran a few ms from starvation (measured: sink delay
/// stuck at 128 frames, 137 "underruns" in 471 cycles). Costs that much
/// latency, exactly like virtio.
const RESYNC_PRIME: u64 = 16 * 1024;
/// Output stream tag (1..15) bound to the stream descriptor and every DAC.
const STREAM_TAG: u8 = 1;
const OUT_RATE_FALLBACK: u32 = 48000;

// ── Logging ─────────────────────────────────────────────────────────────────
struct Uart;
impl core::fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> core::fmt::Result { pci::serial_debug(s); Ok(()) }
}
/// UART + persisted klog (the stick's boot log), any context.
macro_rules! hlog {
    ($($t:tt)*) => {{ let _ = core::fmt::Write::write_fmt(&mut Uart, format_args!($($t)*)); }};
}

extern "C" {
    /// Kernel whole-line console print: UART + klog + framebuffer console.
    fn serial_print_str_raw(s: *const u8, len: usize);
}

/// A line that should also be readable on the PC's monitor (boot context).
fn screen(s: &str) {
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()) };
}

// ── MMIO (HVF-safe on aarch64: plain ldr/str, never a writeback form) ─────────
#[inline(always)]
unsafe fn rd8(a: usize) -> u8 {
    #[cfg(target_arch = "aarch64")]
    { let v: u32; core::arch::asm!("ldrb {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, readonly)); v as u8 }
    #[cfg(not(target_arch = "aarch64"))]
    { core::ptr::read_volatile(a as *const u8) }
}
#[inline(always)]
unsafe fn rd16(a: usize) -> u16 {
    #[cfg(target_arch = "aarch64")]
    { let v: u32; core::arch::asm!("ldrh {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, readonly)); v as u16 }
    #[cfg(not(target_arch = "aarch64"))]
    { core::ptr::read_volatile(a as *const u16) }
}
#[inline(always)]
unsafe fn rd32(a: usize) -> u32 {
    #[cfg(target_arch = "aarch64")]
    { let v: u32; core::arch::asm!("ldr {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, readonly)); v }
    #[cfg(not(target_arch = "aarch64"))]
    { core::ptr::read_volatile(a as *const u32) }
}
#[inline(always)]
unsafe fn wr8(a: usize, v: u8) {
    #[cfg(target_arch = "aarch64")]
    { core::arch::asm!("strb {v:w}, [{a}]", a = in(reg) a, v = in(reg) v as u32, options(nostack)); }
    #[cfg(not(target_arch = "aarch64"))]
    { core::ptr::write_volatile(a as *mut u8, v) }
}
#[inline(always)]
unsafe fn wr16(a: usize, v: u16) {
    #[cfg(target_arch = "aarch64")]
    { core::arch::asm!("strh {v:w}, [{a}]", a = in(reg) a, v = in(reg) v as u32, options(nostack)); }
    #[cfg(not(target_arch = "aarch64"))]
    { core::ptr::write_volatile(a as *mut u16, v) }
}
#[inline(always)]
unsafe fn wr32(a: usize, v: u32) {
    #[cfg(target_arch = "aarch64")]
    { core::arch::asm!("str {v:w}, [{a}]", a = in(reg) a, v = in(reg) v, options(nostack)); }
    #[cfg(not(target_arch = "aarch64"))]
    { core::ptr::write_volatile(a as *mut u32, v) }
}

fn udelay(us: u64) {
    let t0 = monotonic_us();
    while monotonic_us().wrapping_sub(t0) < us { core::hint::spin_loop(); }
}

/// Spin until `cond` or `timeout_us`; true when the condition held.
fn wait_for(timeout_us: u64, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = monotonic_us();
    loop {
        if cond() { return true; }
        if monotonic_us().wrapping_sub(t0) > timeout_us { return cond(); }
        core::hint::spin_loop();
    }
}

// ── Codec graph ─────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Widget {
    nid: u16,
    caps: u32,
    wtype: u8,
    pincap: u32,
    cfg: u32,
    conns: Vec<u16>,
    amp_in: u32,
    amp_out: u32,
}

#[derive(Clone)]
struct OutPin {
    nid: u16,
    cfg: u32,
    /// Nodes from the pin to the DAC (inclusive), each with the connection
    /// index used to reach the next node (unused for the DAC).
    path: Vec<(u16, u8)>,
}

fn wtype_name(t: u8) -> &'static str {
    match t { 0 => "DAC", 1 => "ADC", 2 => "mixer", 3 => "selector", 4 => "pin",
              5 => "power", 6 => "vol-knob", 7 => "beep", 0xF => "vendor", _ => "?" }
}

fn device_name(cfg: u32) -> &'static str {
    match (cfg >> 20) & 0xF {
        0x0 => "line-out", 0x1 => "speaker", 0x2 => "headphone", 0x3 => "CD",
        0x4 => "SPDIF-out", 0x5 => "digital-out", 0x8 => "line-in", 0x9 => "aux",
        0xA => "mic", 0xB => "telephony", 0xC => "SPDIF-in", 0xD => "digital-in", _ => "other",
    }
}

fn color_name(cfg: u32) -> &'static str {
    match (cfg >> 12) & 0xF {
        0x1 => "black", 0x2 => "grey", 0x3 => "blue", 0x4 => "green", 0x5 => "red",
        0x6 => "orange", 0x7 => "yellow", 0x8 => "purple", 0x9 => "pink", 0xE => "white", _ => "",
    }
}

fn location_name(cfg: u32) -> &'static str {
    let loc = (cfg >> 24) & 0x3F;
    match loc {
        0x01 => "rear", 0x02 => "front", 0x03 => "left", 0x04 => "right", 0x05 => "top", 0x06 => "bottom",
        0x10 | 0x00 => "", 0x17 | 0x18 => "internal-hdmi", 0x11..=0x16 | 0x19..=0x1F => "internal",
        0x20..=0x2F => "separate", _ => "other",
    }
}

fn connectivity_name(cfg: u32) -> &'static str {
    match cfg >> 30 { 0 => "jack", 1 => "none", 2 => "fixed", _ => "jack+fixed" }
}

/// HDA SDnFMT/converter format for 16-bit, `ch` channels, or None for a rate
/// the base/mult/div grid cannot express.
fn stream_format(rate: u32, ch: u8) -> Option<u16> {
    // (base44, mult-1, div-1)
    let (b, m, d) = match rate {
        8000 => (0, 0, 5), 11025 => (1, 0, 3), 16000 => (0, 0, 2), 22050 => (1, 0, 1),
        32000 => (0, 1, 2), 44100 => (1, 0, 0), 48000 => (0, 0, 0), 88200 => (1, 1, 0),
        96000 => (0, 1, 0), 176400 => (1, 3, 0), 192000 => (0, 3, 0),
        _ => return None,
    };
    Some((b << 14) | (m << 11) | (d << 8) | (1 << 4) | ((ch.max(1) - 1) as u16 & 0xF))
}

/// The PCM-parameter rate bit (param 0x0A bits 11:0) for `rate`.
fn rate_bit(rate: u32) -> Option<u32> {
    Some(match rate {
        8000 => 0, 11025 => 1, 16000 => 2, 22050 => 3, 32000 => 4, 44100 => 5, 48000 => 6,
        88200 => 7, 96000 => 8, 176400 => 9, 192000 => 10, _ => return None,
    })
}

#[repr(C)]
struct BdlEntry { addr: u64, len: u32, ioc: u32 }

/// Linear-interpolating rate converter state (stereo S16).
#[derive(Clone, Copy)]
struct Resampler { step: u32, frac: u32, prev: [i16; 2], primed: bool }

pub struct Hda {
    base: usize,
    func: pci::PciFunction,
    vendor: u16,
    device: u16,
    class_sub: u8,
    gcap: u16,
    // Command transport
    corb: usize, corb_phys: usize, corb_entries: u16, corb_wp: u16,
    rirb: usize, rirb_phys: usize, rirb_entries: u16, rirb_rp: u16,
    immediate: bool,
    cmd_timeouts: u32,
    // DMA
    posbuf: usize,
    bdl: usize, bdl_phys: usize,
    ring: usize, ring_phys: usize,
    sd: usize, sd_index: u8,
    fifos: u32,
    // Codec
    cad: u8,
    codec_vid: u32,
    codec_rev: u32,
    codec_subsys: u32,
    afg: u16,
    widgets: Vec<Widget>,
    pins: Vec<OutPin>,
    dacs: Vec<u16>,
    dac_rates: u32,
    // Stream state
    pub initialized: bool,
    stream_active: bool,
    hw_rate: u32,
    in_rate: u32,
    in_channels: u8,
    last_pos: u32,
    played: u64,
    written: u64,
    underruns: u32,
    was_ahead: bool,
    resamp: Resampler,
    restarts: u32,
    dma_errors: u32,
    summary: String,
}

unsafe impl Send for Hda {}
unsafe impl Sync for Hda {}

impl Hda {
    pub const fn new() -> Self {
        Self {
            base: 0, func: pci::PciFunction::new(0, 0, 0), vendor: 0, device: 0, class_sub: 0, gcap: 0,
            corb: 0, corb_phys: 0, corb_entries: 0, corb_wp: 0,
            rirb: 0, rirb_phys: 0, rirb_entries: 0, rirb_rp: 0,
            immediate: false, cmd_timeouts: 0,
            posbuf: 0, bdl: 0, bdl_phys: 0, ring: 0, ring_phys: 0, sd: 0, sd_index: 0, fifos: 0,
            cad: 0, codec_vid: 0, codec_rev: 0, codec_subsys: 0, afg: 0,
            widgets: Vec::new(), pins: Vec::new(), dacs: Vec::new(), dac_rates: 0,
            initialized: false, stream_active: false,
            hw_rate: 48000, in_rate: 48000, in_channels: 2,
            last_pos: 0, played: 0, written: 0, underruns: 0, was_ahead: false,
            resamp: Resampler { step: 65536, frac: 0, prev: [0; 2], primed: false },
            restarts: 0, dma_errors: 0,
            summary: String::new(),
        }
    }

    // ── register helpers ──
    #[inline] fn r8(&self, o: usize) -> u8 { unsafe { rd8(self.base + o) } }
    #[inline] fn r16(&self, o: usize) -> u16 { unsafe { rd16(self.base + o) } }
    #[inline] fn r32(&self, o: usize) -> u32 { unsafe { rd32(self.base + o) } }
    #[inline] fn w8(&self, o: usize, v: u8) { unsafe { wr8(self.base + o, v) } }
    #[inline] fn w16(&self, o: usize, v: u16) { unsafe { wr16(self.base + o, v) } }
    #[inline] fn w32(&self, o: usize, v: u32) { unsafe { wr32(self.base + o, v) } }

    /// Find and bring up a controller with a usable analog output.
    pub fn probe(&mut self) -> Result<(), ()> {
        // Candidates: HDA (0x0403) first, Intel before everyone else; then
        // Intel audio-DSP-class (0x0401) functions, which still expose the
        // legacy HDA register block (Linux snd-hda-intel drives them too).
        let all = pci::scan();
        let mut cands: Vec<pci::PciDevice> = Vec::new();
        for d in all.iter() {
            if d.class == 0x04 && d.subclass == 0x03 {
                hlog!("[HDA] PCI {:02x}:{:02x}.{} {:04x}:{:04x} class 0403 (HD Audio)\n",
                      d.bus, d.dev, d.func, d.vendor_id, d.device_id);
            } else if d.class == 0x04 && d.subclass == 0x01 && d.vendor_id == 0x8086 {
                hlog!("[HDA] PCI {:02x}:{:02x}.{} {:04x}:{:04x} class 0401: Intel audio DSP (SST/SOF) mode; \
                       trying its legacy HDA interface (no DSP firmware, no DMIC)\n",
                      d.bus, d.dev, d.func, d.vendor_id, d.device_id);
            }
        }
        for pass in 0..3 {
            for d in all.iter() {
                let hda = d.class == 0x04 && d.subclass == 0x03;
                let dsp = d.class == 0x04 && d.subclass == 0x01 && d.vendor_id == 0x8086;
                let pick = match pass {
                    0 => hda && d.vendor_id == 0x8086,
                    1 => hda && d.vendor_id != 0x8086,
                    _ => dsp,
                };
                if pick { cands.push(*d); }
            }
        }
        if cands.is_empty() {
            hlog!("[HDA] no HD Audio controller (PCI class 0403) found\n");
            return Err(());
        }
        for d in cands {
            if d.vendor_id == 0x10de || d.vendor_id == 0x1002 {
                hlog!("[HDA] {:04x}:{:04x}: GPU HDMI/DP audio function, skipped (not supported)\n",
                      d.vendor_id, d.device_id);
                continue;
            }
            match self.try_controller(&d) {
                Ok(()) => return Ok(()),
                Err(why) => {
                    hlog!("[HDA] {:04x}:{:04x}: {}\n", d.vendor_id, d.device_id, why);
                    let s = alloc::format!("[HDA] controller {:04x}:{:04x} not usable: {}\n",
                                           d.vendor_id, d.device_id, why);
                    screen(&s);
                    self.shutdown_controller();
                }
            }
        }
        Err(())
    }

    fn shutdown_controller(&mut self) {
        if self.base != 0 {
            self.w8(CORBCTL, 0);
            self.w8(RIRBCTL, 0);
            if self.sd != 0 { self.w8(self.sd + SD_CTL0, 0); }
        }
        self.base = 0;
        self.widgets.clear();
        self.pins.clear();
        self.dacs.clear();
    }

    fn try_controller(&mut self, d: &pci::PciDevice) -> Result<(), &'static str> {
        let f = pci::PciFunction::new(d.bus, d.dev, d.func);
        self.func = f;
        self.vendor = d.vendor_id;
        self.device = d.device_id;
        self.class_sub = d.subclass;
        let bar = f.bar(0).ok_or("no BAR0")?;
        if bar.io || bar.base == 0 { return Err("BAR0 unassigned or I/O"); }

        // Intel PCH tweaks: traffic class 0 (TCSEL), and snooped DMA (the
        // SCH/PCH "no snoop" bit 11 of DEVC at 0x78 cleared) so the
        // controller sees cache-coherent buffers. Skipped on ICH-era parts
        // (incl. QEMU's ich6/ich9 models), which have neither register.
        if d.vendor_id == 0x8086 {
            let tcsel = f.read8(0x44);
            if tcsel & 0x07 != 0 { f.write8(0x44, tcsel & !0x07); }
            if !(0x2600..=0x3fff).contains(&d.device_id) {
                let devc = f.read16(0x78);
                if devc & (1 << 11) != 0 {
                    f.write16(0x78, devc & !(1 << 11));
                    hlog!("[HDA] PCH snoop enabled (DEVC {:#06x} -> {:#06x})\n", devc, f.read16(0x78));
                }
            }
        }
        f.enable_mem_and_bus_master();
        f.set_intx_disable(true);

        let base = unsafe { map_kernel_device(bar.base as usize, 0x4000,
                                     PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NOCACHE) }
            .ok_or("cannot map BAR0")?;
        self.base = base;
        let gcap = self.r16(GCAP);
        if gcap == 0xFFFF { return Err("registers read all-ones (device off or BAR wrong)"); }
        self.gcap = gcap;
        let (oss, iss, bss) = ((gcap >> 12) & 0xF, (gcap >> 8) & 0xF, (gcap >> 3) & 0x1F);
        hlog!("[HDA] controller {:04x}:{:04x} at {:02x}:{:02x}.{} BAR0 {:#x}: HDA {}.{} GCAP {:#06x} \
               (out {} in {} bidir {} 64bit {})\n",
              d.vendor_id, d.device_id, d.bus, d.dev, d.func, bar.base, self.r8(VMAJ), self.r8(VMIN),
              gcap, oss, iss, bss, gcap & 1);
        if oss == 0 && bss == 0 { return Err("no output stream descriptors"); }

        self.alloc_dma()?;
        self.reset_controller()?;
        self.setup_corb_rirb();

        let statests = self.r16(STATESTS);
        self.w16(STATESTS, statests); // RW1C
        hlog!("[HDA] STATESTS {:#06x}\n", statests);
        if statests & 0x7FFF == 0 { return Err("no codec answered the link reset (STATESTS = 0)"); }

        // Walk every codec; keep the first with an analog output path.
        let mut found = false;
        for cad in 0..15u8 {
            if statests & (1 << cad) == 0 { continue; }
            match self.probe_codec(cad) {
                Ok(true) => { found = true; break; }
                Ok(false) => {}
                Err(e) => hlog!("[HDA] codec {}: {}\n", cad, e),
            }
        }
        if !found { return Err("no codec with an analog output pin + DAC path"); }

        // Output stream descriptor: the first output one (after the inputs),
        // or the first bidirectional one when there are no output-only ones.
        self.sd_index = if oss > 0 { iss as u8 } else { (iss + oss) as u8 };
        self.sd = SD_BASE + self.sd_index as usize * SD_STRIDE;
        self.program_codec_paths();
        self.build_summary();
        for line in self.summary.clone().lines() {
            let mut s = String::from(line);
            s.push('\n');
            screen(&s);
        }
        self.initialized = true;
        // Start the stream at the default format right away, like the
        // virtio backend's first SET_PARAMS: the DMA engine runs (silence)
        // from now on and producers only ever append.
        self.reconfigure_stream(0, 48000, 2);
        Ok(())
    }

    fn alloc_dma(&mut self) -> Result<(), &'static str> {
        const LIMIT: usize = 0x1_0000_0000; // below 4 GiB whatever GCAP.64OK says
        // Page 0: CORB (1 KiB) + RIRB (2 KiB) + position buffer (at +3 KiB).
        let p = buddy::alloc_below(0, LIMIT).ok_or("out of DMA memory")?;
        let v = phys_to_virt(p);
        unsafe { core::ptr::write_bytes(v as *mut u8, 0, 4096); }
        self.corb_phys = p; self.corb = v;
        self.rirb_phys = p + 1024; self.rirb = v + 1024;
        self.posbuf = v + 3072;
        // Page 1: BDL.
        let p = buddy::alloc_below(0, LIMIT).ok_or("out of DMA memory")?;
        let v = phys_to_virt(p);
        unsafe { core::ptr::write_bytes(v as *mut u8, 0, 4096); }
        self.bdl_phys = p; self.bdl = v;
        // Ring: 64 KiB = order 4.
        let order = (RING_BYTES / 4096).trailing_zeros() as usize;
        let p = buddy::alloc_below(order, LIMIT).ok_or("out of DMA memory")?;
        let v = phys_to_virt(p);
        unsafe { core::ptr::write_bytes(v as *mut u8, 0, RING_BYTES); }
        self.ring_phys = p; self.ring = v;
        for i in 0..BDL_ENTRIES {
            unsafe {
                let e = (self.bdl as *mut BdlEntry).add(i);
                core::ptr::write_volatile(e, BdlEntry {
                    addr: (self.ring_phys + i * BDL_CHUNK) as u64, len: BDL_CHUNK as u32, ioc: 0,
                });
            }
        }
        fence(Ordering::SeqCst);
        Ok(())
    }

    fn reset_controller(&mut self) -> Result<(), &'static str> {
        // Quiesce: no interrupts, no DMA.
        self.w32(INTCTL, 0);
        let streams = ((self.gcap >> 12) & 0xF) + ((self.gcap >> 8) & 0xF) + ((self.gcap >> 3) & 0x1F);
        for i in 0..streams as usize {
            let sd = SD_BASE + i * SD_STRIDE;
            self.w8(sd + SD_CTL0, self.r8(sd + SD_CTL0) & !SD_CTL_RUN);
            self.w8(sd + SD_STS, 0x1C);
        }
        self.w8(CORBCTL, 0);
        self.w8(RIRBCTL, 0);
        wait_for(1000, || self.r8(CORBCTL) & 2 == 0 && self.r8(RIRBCTL) & 2 == 0);
        self.w32(DPLBASE, 0);
        self.w32(DPUBASE, 0);
        self.w16(STATESTS, 0x7FFF);

        // Enter reset, hold, leave reset (spec §4.2.2).
        self.w32(GCTL, self.r32(GCTL) & !GCTL_CRST);
        if !wait_for(100_000, || self.r32(GCTL) & GCTL_CRST == 0) {
            return Err("GCTL.CRST did not clear (controller stuck)");
        }
        udelay(1000);
        self.w32(GCTL, self.r32(GCTL) | GCTL_CRST);
        if !wait_for(100_000, || self.r32(GCTL) & GCTL_CRST != 0) {
            return Err("GCTL.CRST did not set (controller stuck in reset)");
        }
        // Codecs request a STATESTS bit within 521 us of the reset release;
        // slow ones (and some Realteks) take longer, so wait up to 100 ms
        // for the first bit and a little more for stragglers.
        if wait_for(100_000, || self.r16(STATESTS) & 0x7FFF != 0) { udelay(2000); }
        Ok(())
    }

    fn setup_corb_rirb(&mut self) {
        let corbsize_cap = self.r8(CORBSIZE) >> 4;
        let (sel, n) = if corbsize_cap & 0x4 != 0 { (2u8, 256u16) } else if corbsize_cap & 0x2 != 0 { (1, 16) } else { (0, 2) };
        self.w8(CORBSIZE, (self.r8(CORBSIZE) & !3) | sel);
        self.corb_entries = n;
        self.w32(CORBLBASE, self.corb_phys as u32);
        self.w32(CORBUBASE, (self.corb_phys as u64 >> 32) as u32);
        // CORB read-pointer reset handshake (some parts never echo the bit;
        // that is fine, the pointer still resets).
        self.w16(CORBRP, 0x8000);
        wait_for(1000, || self.r16(CORBRP) & 0x8000 != 0);
        self.w16(CORBRP, 0);
        wait_for(1000, || self.r16(CORBRP) & 0x8000 == 0);
        self.w16(CORBWP, 0);
        self.corb_wp = 0;

        let rirbsize_cap = self.r8(RIRBSIZE) >> 4;
        let (sel, n) = if rirbsize_cap & 0x4 != 0 { (2u8, 256u16) } else if rirbsize_cap & 0x2 != 0 { (1, 16) } else { (0, 2) };
        self.w8(RIRBSIZE, (self.r8(RIRBSIZE) & !3) | sel);
        self.rirb_entries = n;
        self.w32(RIRBLBASE, self.rirb_phys as u32);
        self.w32(RIRBUBASE, (self.rirb_phys as u64 >> 32) as u32);
        self.w16(RIRBWP, 0x8000); // reset write pointer
        self.w16(RINTCNT, 1);
        self.rirb_rp = 0;
        self.w8(RIRBSTS, 0x05);
        // DMA on + RINTCTL. Nothing is delivered (INTCTL.GIE stays 0), but
        // QEMU's model stops fetching CORB entries once RINTCNT responses are
        // pending unless RINTCTL lets it raise RIRBSTS.RINTFL, which every
        // response read clears again.
        self.w8(RIRBCTL, 0x03);
        self.w8(CORBCTL, 0x02); // CORBRUN
        wait_for(1000, || self.r8(CORBCTL) & 2 != 0);
        // DMA position buffer (diagnostic second opinion next to LPIB).
        let pb = self.posbuf;
        let pb_phys = self.corb_phys + 3072;
        let _ = pb;
        self.w32(DPUBASE, (pb_phys as u64 >> 32) as u32);
        self.w32(DPLBASE, pb_phys as u32 | 1);
        hlog!("[HDA] CORB {} entries, RIRB {} entries\n", self.corb_entries, self.rirb_entries);
    }

    // ── codec command transport ──

    fn send_corb(&mut self, cmd: u32) -> Option<u32> {
        let wp = (self.corb_wp + 1) % self.corb_entries;
        unsafe { core::ptr::write_volatile((self.corb as *mut u32).add(wp as usize), cmd); }
        fence(Ordering::SeqCst);
        self.corb_wp = wp;
        self.w16(CORBWP, wp);
        let t0 = monotonic_us();
        loop {
            let hw = self.r16(RIRBWP) & 0xFF;
            let hw = hw % self.rirb_entries;
            while self.rirb_rp != hw {
                self.rirb_rp = (self.rirb_rp + 1) % self.rirb_entries;
                fence(Ordering::SeqCst);
                let e = unsafe { core::ptr::read_volatile((self.rirb as *const u64).add(self.rirb_rp as usize)) };
                let ex = (e >> 32) as u32;
                if ex & 0x10 != 0 { continue; } // unsolicited
                self.w8(RIRBSTS, 0x05);
                return Some(e as u32);
            }
            if monotonic_us().wrapping_sub(t0) > 20_000 { return None; }
            core::hint::spin_loop();
        }
    }

    fn send_immediate(&mut self, cmd: u32) -> Option<u32> {
        if !wait_for(2000, || self.r16(ICIS) & 1 == 0) {
            // Busy forever: kick it (clear ICB) and try once more.
            self.w16(ICIS, 0);
            if !wait_for(2000, || self.r16(ICIS) & 1 == 0) { return None; }
        }
        self.w16(ICIS, 0x2); // clear IRV (RW1C)
        self.w32(ICOI, cmd);
        self.w16(ICIS, 0x1); // ICB: send
        if !wait_for(20_000, || self.r16(ICIS) & 0x2 != 0) { return None; }
        let r = self.r32(ICII);
        self.w16(ICIS, 0x2);
        Some(r)
    }

    fn command(&mut self, cmd: u32) -> Option<u32> {
        if !self.immediate {
            if let Some(r) = self.send_corb(cmd) { return Some(r); }
            self.cmd_timeouts += 1;
            hlog!("[HDA] CORB/RIRB command {:#010x} timed out; switching to the immediate command interface\n", cmd);
            // Stop the rings: the immediate interface is only defined with
            // CORB/RIRB DMA off.
            self.w8(CORBCTL, 0);
            self.w8(RIRBCTL, 0);
            wait_for(1000, || self.r8(CORBCTL) & 2 == 0 && self.r8(RIRBCTL) & 2 == 0);
            self.immediate = true;
        }
        let r = self.send_immediate(cmd);
        if r.is_none() { self.cmd_timeouts += 1; }
        r
    }

    fn verb(&mut self, nid: u16, verb: u32, payload: u32) -> Option<u32> {
        let cmd = ((self.cad as u32) << 28) | ((nid as u32 & 0x7F) << 20) | ((verb & 0xFFF) << 8) | (payload & 0xFF);
        self.command(cmd)
    }

    fn verb4(&mut self, nid: u16, verb: u32, payload: u32) -> Option<u32> {
        let cmd = ((self.cad as u32) << 28) | ((nid as u32 & 0x7F) << 20) | ((verb & 0xF) << 16) | (payload & 0xFFFF);
        self.command(cmd)
    }

    fn param(&mut self, nid: u16, p: u32) -> u32 {
        self.verb(nid, V_GET_PARAM, p).unwrap_or(0)
    }

    // ── codec discovery ──

    /// Ok(true) = codec `cad` has an AFG with at least one output path.
    fn probe_codec(&mut self, cad: u8) -> Result<bool, &'static str> {
        self.cad = cad;
        let vid = self.verb(0, V_GET_PARAM, P_VENDOR).ok_or("no response to GET_PARAMETER(vendor)")?;
        let rev = self.param(0, P_REVISION);
        let sub = self.param(0, P_SUBNODES);
        let (start, count) = ((sub >> 16) & 0xFF, sub & 0xFF);
        hlog!("[HDA] codec {}: vendor {:04x}:{:04x} rev {:#x}, {} function group(s) from nid {}\n",
              cad, vid >> 16, vid & 0xFFFF, rev, count, start);
        for fg in start..start + count {
            let fg = fg as u16;
            let t = self.param(fg, P_FG_TYPE) & 0xFF;
            if t != 0x01 {
                hlog!("[HDA] codec {} nid {}: function group type {:#x} (not audio), skipped\n", cad, fg, t);
                continue;
            }
            self.codec_vid = vid;
            self.codec_rev = rev;
            self.codec_subsys = self.verb(fg, V_GET_SUBSYS, 0).unwrap_or(0);
            self.afg = fg;
            // AFG to D0 before touching the widgets.
            self.verb(fg, V_SET_POWER, 0);
            udelay(10_000);
            self.verb(fg, V_SET_UNSOL, 0);
            self.parse_afg(fg);
            if self.choose_paths() { return Ok(true); }
            hlog!("[HDA] codec {} ({:08x}): no analog output path\n", cad, vid);
        }
        Ok(false)
    }

    fn parse_afg(&mut self, afg: u16) {
        let afg_amp_in = self.param(afg, P_AMP_IN);
        let afg_amp_out = self.param(afg, P_AMP_OUT);
        let afg_pcm = self.param(afg, P_PCM);
        let sub = self.param(afg, P_SUBNODES);
        let (start, count) = ((sub >> 16) & 0xFF, sub & 0xFF);
        self.widgets.clear();
        for nid in start..start + count {
            let nid = nid as u16;
            let caps = self.param(nid, P_WCAPS);
            let wtype = ((caps >> 20) & 0xF) as u8;
            let mut w = Widget { nid, caps, wtype, pincap: 0, cfg: 0, conns: Vec::new(),
                                 amp_in: afg_amp_in, amp_out: afg_amp_out };
            if caps & WCAP_AMP_OVRD != 0 {
                if caps & WCAP_IN_AMP != 0 { w.amp_in = self.param(nid, P_AMP_IN); }
                if caps & WCAP_OUT_AMP != 0 { w.amp_out = self.param(nid, P_AMP_OUT); }
            }
            if wtype == W_PIN {
                w.pincap = self.param(nid, P_PINCAP);
                w.cfg = self.verb(nid, V_GET_CFG_DEFAULT, 0).unwrap_or(0);
            }
            if caps & WCAP_CONN_LIST != 0 { w.conns = self.read_conns(nid); }
            if wtype == W_DAC {
                let pcm = if caps & WCAP_FMT_OVRD != 0 { self.param(nid, P_PCM) } else { afg_pcm };
                w.pincap = pcm; // stash: PCM rates/bits for DACs
            }
            hlog!("[HDA]   nid {:#04x} {:8} caps {:08x}", nid, wtype_name(wtype), caps);
            if wtype == W_PIN {
                hlog!(" pincap {:08x} cfg {:08x} ({} {} {} {})", w.pincap, w.cfg,
                      connectivity_name(w.cfg), location_name(w.cfg), color_name(w.cfg), device_name(w.cfg));
            }
            if wtype == W_DAC { hlog!(" pcm {:08x}", w.pincap); }
            if !w.conns.is_empty() {
                hlog!(" conn [");
                for (i, c) in w.conns.iter().enumerate() { hlog!("{}{:#04x}", if i > 0 { " " } else { "" }, c); }
                hlog!("]");
            }
            hlog!("\n");
            self.widgets.push(w);
        }
    }

    fn read_conns(&mut self, nid: u16) -> Vec<u16> {
        let len = self.param(nid, P_CONNLEN);
        let long = len & 0x80 != 0;
        let n = (len & 0x7F) as usize;
        let mut out: Vec<u16> = Vec::new();
        let per = if long { 2 } else { 4 };
        let mut i = 0;
        let mut prev: u16 = 0;
        while i < n && out.len() < 64 {
            let r = match self.verb(nid, V_GET_CONN_LIST, i as u32) { Some(r) => r, None => break };
            for k in 0..per {
                if i + k >= n { break; }
                let (e, range, v) = if long {
                    let e = (r >> (16 * k)) & 0xFFFF;
                    (e, e & 0x8000 != 0, (e & 0x7FFF) as u16)
                } else {
                    let e = (r >> (8 * k)) & 0xFF;
                    (e, e & 0x80 != 0, (e & 0x7F) as u16)
                };
                let _ = e;
                if range && prev != 0 && v > prev {
                    let mut x = prev + 1;
                    while x <= v && out.len() < 64 { out.push(x); x += 1; }
                } else {
                    out.push(v);
                }
                prev = v;
            }
            i += per;
        }
        out
    }

    fn widget(&self, nid: u16) -> Option<&Widget> { self.widgets.iter().find(|w| w.nid == nid) }

    /// Shortest path (BFS) from `pin` to an analog DAC through mixers and
    /// selectors, at most 5 hops. Returns (node, connection index toward the
    /// next node) pairs, pin first, DAC last.
    fn find_path(&self, pin: u16) -> Option<Vec<(u16, u8)>> {
        // (nid, parent index in `queue`, depth)
        let mut queue: Vec<(u16, usize, u8)> = alloc::vec![(pin, usize::MAX, 0)];
        let mut head = 0;
        while head < queue.len() {
            let (nid, _, depth) = queue[head];
            let w = match self.widget(nid) { Some(w) => w, None => { head += 1; continue; } };
            if head != 0 && w.wtype == W_DAC && w.caps & WCAP_DIGITAL == 0 {
                let mut nodes: Vec<u16> = Vec::new();
                let mut i = head;
                loop {
                    nodes.push(queue[i].0);
                    if queue[i].1 == usize::MAX { break; }
                    i = queue[i].1;
                }
                nodes.reverse();
                let mut path: Vec<(u16, u8)> = Vec::new();
                for k in 0..nodes.len() {
                    let ci = if k + 1 < nodes.len() {
                        self.widget(nodes[k])
                            .and_then(|w| w.conns.iter().position(|&c| c == nodes[k + 1]))
                            .unwrap_or(0) as u8
                    } else { 0 };
                    path.push((nodes[k], ci));
                }
                return Some(path);
            }
            if depth < 5 && (head == 0 || w.wtype == W_MIXER || w.wtype == W_SELECTOR) {
                for &c in w.conns.iter() {
                    if queue.iter().any(|q| q.0 == c) { continue; }
                    if let Some(cw) = self.widget(c) {
                        if matches!(cw.wtype, W_DAC | W_MIXER | W_SELECTOR) {
                            queue.push((c, head, depth + 1));
                        }
                    }
                }
            }
            head += 1;
        }
        None
    }

    fn choose_paths(&mut self) -> bool {
        self.pins.clear();
        self.dacs.clear();
        let pins: Vec<Widget> = self.widgets.iter().filter(|w| w.wtype == W_PIN).cloned().collect();
        let analog_out = |w: &Widget| w.pincap & PINCAP_OUT != 0 && w.caps & WCAP_DIGITAL == 0
            && w.pincap & (PINCAP_HDMI | PINCAP_DP) == 0;
        // Pass 0: the BIOS says it is a connected line-out/speaker/headphone.
        // Pass 1 (bad pin configs): any connected analog output-capable pin
        // that is not described as an input. Pass 2: any analog output pin.
        for pass in 0..3 {
            for w in pins.iter() {
                if !analog_out(w) { continue; }
                let conn = w.cfg >> 30;
                let dev = (w.cfg >> 20) & 0xF;
                let ok = match pass {
                    0 => conn != 1 && dev <= 2,
                    1 => conn != 1 && !matches!(dev, 0x8 | 0xA | 0xC | 0xD),
                    _ => true,
                };
                if !ok { continue; }
                if let Some(path) = self.find_path(w.nid) {
                    let dac = path.last().unwrap().0;
                    if !self.dacs.contains(&dac) { self.dacs.push(dac); }
                    self.pins.push(OutPin { nid: w.nid, cfg: w.cfg, path });
                }
            }
            if !self.pins.is_empty() {
                if pass > 0 {
                    hlog!("[HDA] pin configuration defaults list no usable output; fallback pass {} picked {} pin(s)\n",
                          pass, self.pins.len());
                }
                break;
            }
        }
        if self.pins.is_empty() { return false; }
        // Rates every chosen DAC supports.
        let mut rates = 0xFFF;
        for &d in self.dacs.iter() {
            if let Some(w) = self.widget(d) { rates &= w.pincap & 0xFFF; }
        }
        self.dac_rates = rates;
        true
    }

    fn amp_gain_0db(caps: u32) -> u32 {
        let offset = caps & 0x7F;
        let steps = (caps >> 8) & 0x7F;
        offset.min(steps)
    }

    fn set_out_amp(&mut self, nid: u16) {
        let w = match self.widget(nid) { Some(w) => w.clone(), None => return };
        if w.caps & WCAP_OUT_AMP == 0 { return; }
        let g = Self::amp_gain_0db(w.amp_out);
        // output amp, left + right, unmuted, 0 dB
        self.verb4(nid, V4_SET_AMP, (1 << 15) | (1 << 13) | (1 << 12) | g);
    }

    fn set_in_amp(&mut self, nid: u16, idx: u8, unmute: bool) {
        let w = match self.widget(nid) { Some(w) => w.clone(), None => return };
        if w.caps & WCAP_IN_AMP == 0 { return; }
        let g = Self::amp_gain_0db(w.amp_in);
        let m = if unmute { g } else { 1 << 7 };
        self.verb4(nid, V4_SET_AMP, (1 << 14) | (1 << 13) | (1 << 12) | ((idx as u32 & 0xF) << 8) | m);
    }

    fn power_up(&mut self, nid: u16) {
        if let Some(w) = self.widget(nid) {
            if w.caps & WCAP_POWER != 0 { self.verb(nid, V_SET_POWER, 0); }
        }
    }

    /// Program every chosen pin → DAC path (selectors, amps, pin control,
    /// EAPD). Mixers on any path keep only the inputs some path uses.
    fn program_codec_paths(&mut self) {
        let pins = self.pins.clone();
        // Union of used inputs per mixer.
        let mut mixer_inputs: Vec<(u16, Vec<u8>)> = Vec::new();
        for p in pins.iter() {
            for k in 0..p.path.len() {
                let (nid, ci) = p.path[k];
                let w = match self.widget(nid) { Some(w) => w.clone(), None => continue };
                self.power_up(nid);
                let last = k + 1 == p.path.len();
                match w.wtype {
                    W_PIN => {
                        if w.conns.len() > 1 { self.verb(nid, V_SET_CONN_SEL, ci as u32); }
                        self.set_out_amp(nid);
                        let hp = (w.cfg >> 20) & 0xF == 2 && w.pincap & PINCAP_HP_DRIVE != 0;
                        let cur = self.verb(nid, V_GET_PIN_CTL, 0).unwrap_or(0) & 0x07; // keep VREF bits
                        let ctl = cur | PINCTL_OUT_EN | if hp { PINCTL_HP_EN } else { 0 };
                        self.verb(nid, V_SET_PIN_CTL, ctl);
                        if w.pincap & PINCAP_EAPD != 0 {
                            let e = self.verb(nid, V_GET_EAPD, 0).unwrap_or(0) & 0x7;
                            self.verb(nid, V_SET_EAPD, e | 0x2);
                        }
                    }
                    W_MIXER => {
                        match mixer_inputs.iter_mut().find(|m| m.0 == nid) {
                            Some(m) => if !m.1.contains(&ci) { m.1.push(ci) },
                            None => mixer_inputs.push((nid, alloc::vec![ci])),
                        }
                        self.set_out_amp(nid);
                    }
                    W_SELECTOR => {
                        self.verb(nid, V_SET_CONN_SEL, ci as u32);
                        self.set_in_amp(nid, ci, true);
                        self.set_out_amp(nid);
                    }
                    W_DAC if last => {
                        self.set_out_amp(nid);
                    }
                    _ => {}
                }
            }
        }
        for (nid, used) in mixer_inputs {
            let n = self.widget(nid).map(|w| w.conns.len()).unwrap_or(0);
            for i in 0..n.min(16) {
                self.set_in_amp(nid, i as u8, used.contains(&(i as u8)));
            }
        }
        // EAPD (external amplifier power-down pin) on every pin that has
        // one, chosen or not: on many boards it gates the line-out amp.
        let eapd: Vec<u16> = self.widgets.iter()
            .filter(|w| w.wtype == W_PIN && w.pincap & PINCAP_EAPD != 0).map(|w| w.nid).collect();
        for nid in eapd {
            let e = self.verb(nid, V_GET_EAPD, 0).unwrap_or(0) & 0x7;
            self.verb(nid, V_SET_EAPD, e | 0x2);
        }
        // Read back what the codec accepted (diagnostic).
        for p in pins.iter() {
            let pc = self.verb(p.nid, V_GET_PIN_CTL, 0).unwrap_or(0xFFFF_FFFF);
            let amp = self.verb4(p.nid, V4_GET_AMP, 1 << 15 | 1 << 13).unwrap_or(0xFFFF_FFFF);
            let dac = p.path.last().map(|x| x.0).unwrap_or(0);
            let damp = self.verb4(dac, V4_GET_AMP, 1 << 15 | 1 << 13).unwrap_or(0xFFFF_FFFF);
            let sel = if self.widget(p.nid).map(|w| w.conns.len() > 1).unwrap_or(false) {
                self.verb(p.nid, V_GET_CONN_SEL, 0).unwrap_or(0xFF)
            } else { 0 };
            hlog!("[HDA] pin {:#04x} readback: pinctl {:#04x} out-amp(L) {:#04x} sel {} ; DAC {:#04x} amp(L) {:#04x}\n",
                  p.nid, pc, amp, sel, dac, damp);
        }
    }

    /// Presence-detect state of each chosen pin ("plugged", "empty", "-" if
    /// the pin cannot tell).
    fn sense(&mut self, nid: u16) -> &'static str {
        let w = match self.widget(nid) { Some(w) => w.clone(), None => return "-" };
        if w.pincap & PINCAP_PRES_DETECT == 0 || (w.cfg >> 8) & 1 != 0 { return "-"; }
        if w.pincap & PINCAP_TRIG_REQ != 0 {
            self.verb(nid, V_EXEC_PIN_SENSE, 0);
            udelay(200);
        }
        match self.verb(nid, V_GET_PIN_SENSE, 0) {
            Some(r) if r & 0x8000_0000 != 0 => "plugged",
            Some(_) => "empty",
            None => "-",
        }
    }

    fn build_summary(&mut self) {
        let mut s = String::new();
        let dsp = if self.class_sub == 0x01 { " (audio-DSP class 0401, legacy HDA mode)" } else { "" };
        let _ = writeln!(s, "[HDA] controller {:04x}:{:04x} at {:02x}:{:02x}.{}{}, {} command interface",
                         self.vendor, self.device, self.func.bus, self.func.dev, self.func.func, dsp,
                         if self.immediate { "immediate" } else { "CORB/RIRB" });
        let _ = writeln!(s, "[HDA] codec {}: {:04x}:{:04x} rev {:#x} subsystem {:08x}{}",
                         self.cad, self.codec_vid >> 16, self.codec_vid & 0xFFFF, self.codec_rev, self.codec_subsys,
                         codec_name(self.codec_vid).map(|n| alloc::format!(" ({})", n)).unwrap_or_default());
        let pins = self.pins.clone();
        for p in pins.iter() {
            let mut path = String::new();
            for (i, (n, _)) in p.path.iter().enumerate() {
                let _ = write!(path, "{}{:#04x}", if i > 0 { "->" } else { "" }, n);
            }
            let sense = self.sense(p.nid);
            let _ = writeln!(s, "[HDA] output pin {:#04x}: {} {} {} {} [{}] path {} jack:{}",
                             p.nid, connectivity_name(p.cfg), location_name(p.cfg), color_name(p.cfg),
                             device_name(p.cfg), alloc::format!("{:08x}", p.cfg), path, sense);
        }
        let _ = writeln!(s, "[HDA] DACs {:?} rates {:#05x}, stream descriptor {} tag {}",
                         self.dacs, self.dac_rates, self.sd_index, STREAM_TAG);
        self.summary = s;
    }

    // ── stream control ──

    fn stop_dma(&mut self) {
        if self.sd == 0 { return; }
        let ctl = self.r8(self.sd + SD_CTL0);
        self.w8(self.sd + SD_CTL0, ctl & !(SD_CTL_RUN | 0x1C));
        wait_for(1000, || self.r8(self.sd + SD_CTL0) & SD_CTL_RUN == 0);
    }

    fn start_dma(&mut self) -> bool {
        let sd = self.sd;
        self.stop_dma();
        // Stream reset.
        self.w8(sd + SD_CTL0, SD_CTL_SRST);
        wait_for(1000, || self.r8(sd + SD_CTL0) & SD_CTL_SRST != 0);
        udelay(20);
        self.w8(sd + SD_CTL0, 0);
        if !wait_for(1000, || self.r8(sd + SD_CTL0) & SD_CTL_SRST == 0) {
            hlog!("[HDA] stream reset did not complete\n");
        }
        self.w8(sd + SD_STS, SD_STS_BCIS | SD_STS_FIFOE | SD_STS_DESE);
        let fmt = stream_format(self.hw_rate, 2).unwrap_or(0x0011);
        // Stream tag in bits 7:4; DIR (bit 3) = output, meaningful only on a
        // bidirectional descriptor (read-only 0 elsewhere).
        let bidir = (self.sd_index as u16) >= ((self.gcap >> 8) & 0xF) + ((self.gcap >> 12) & 0xF);
        let dir = if bidir { 0x08 } else { 0 };
        self.w8(sd + SD_CTL2, (self.r8(sd + SD_CTL2) & 0x07) | dir | (STREAM_TAG << 4));
        self.w32(sd + SD_CBL, RING_BYTES as u32);
        self.w16(sd + SD_LVI, (BDL_ENTRIES - 1) as u16);
        self.w16(sd + SD_FMT, fmt);
        self.w32(sd + SD_BDPL, self.bdl_phys as u32);
        self.w32(sd + SD_BDPU, (self.bdl_phys as u64 >> 32) as u32);
        // SSYNC: make sure our stream is not held.
        self.w32(SSYNC, self.r32(SSYNC) & !(1u32 << self.sd_index));
        self.fifos = self.r16(sd + SD_FIFOS) as u32;

        // Codec side: every DAC converts our stream.
        let dacs = self.dacs.clone();
        for d in dacs {
            self.verb4(d, V4_SET_FMT, fmt as u32);
            self.verb(d, V_SET_STREAM, (STREAM_TAG as u32) << 4);
        }
        fence(Ordering::SeqCst);
        self.w8(sd + SD_CTL0, SD_CTL_RUN);
        let ok = wait_for(1000, || self.r8(sd + SD_CTL0) & SD_CTL_RUN != 0);
        hlog!("[HDA] stream {} running={} fmt {:#06x} ({} Hz) fifos {} cbl {} lvi {}\n",
              self.sd_index, ok, fmt, self.hw_rate, self.fifos, self.r32(sd + SD_CBL), self.r16(sd + SD_LVI));
        ok
    }

    pub fn reconfigure_stream(&mut self, _stream_id: u32, freq: u32, channels: u8) {
        if !self.initialized { return; }
        self.stream_active = false;
        self.stop_dma();
        let ch = channels.max(1);
        self.in_rate = if freq == 0 { 48000 } else { freq };
        self.in_channels = ch;
        let native = rate_bit(self.in_rate).map(|b| self.dac_rates & (1 << b) != 0).unwrap_or(false);
        self.hw_rate = if native { self.in_rate } else { OUT_RATE_FALLBACK };
        self.resamp = Resampler {
            step: ((self.in_rate as u64 * 65536) / self.hw_rate as u64) as u32,
            frac: 0, prev: [0; 2], primed: false,
        };
        hlog!("[HDA] SET_PARAMS {} Hz x{} -> hw {} Hz stereo{}{}\n", self.in_rate, ch, self.hw_rate,
              if native { "" } else { " (resampled)" }, if ch == 1 { " (mono duplicated)" } else { "" });
        unsafe { core::ptr::write_bytes(self.ring as *mut u8, 0, RING_BYTES); }
        self.last_pos = 0;
        self.played = 0;
        self.written = 0;
        self.was_ahead = false;
        self.stream_active = self.start_dma();
    }

    pub fn recover_stream(&mut self) {
        self.restarts += 1;
        hlog!("[HDA] stream stalled (LPIB {:#x}, STS {:#04x}) - restarting\n",
              self.r32(self.sd + SD_LPIB), self.r8(self.sd + SD_STS));
        let (f, c) = (self.in_rate, self.in_channels);
        self.reconfigure_stream(0, f, c);
    }

    /// Bytes the DMA engine fetches ahead of LPIB, plus margin: where a
    /// re-synchronised writer must start.
    fn guard(&self) -> u64 {
        ((self.fifos as u64 + 256 + 3) & !3).min(4096)
    }

    /// Advance `played` from LPIB, zero what the DMA engine has consumed,
    /// and re-synchronise a writer that fell behind (underrun).
    fn update(&mut self) {
        if !self.stream_active { return; }
        let sts = self.r8(self.sd + SD_STS);
        if sts & (SD_STS_FIFOE | SD_STS_DESE) != 0 {
            self.dma_errors += 1;
            self.w8(self.sd + SD_STS, sts & (SD_STS_FIFOE | SD_STS_DESE));
            if self.dma_errors <= 4 {
                hlog!("[HDA] stream status {:#04x} ({}{})\n", sts,
                      if sts & SD_STS_FIFOE != 0 { "FIFO error " } else { "" },
                      if sts & SD_STS_DESE != 0 { "descriptor error" } else { "" });
            }
        }
        if sts & SD_STS_BCIS != 0 { self.w8(self.sd + SD_STS, SD_STS_BCIS); }
        let pos = self.r32(self.sd + SD_LPIB);
        if pos as usize >= RING_BYTES { return; }
        let delta = ((pos as usize + RING_BYTES - self.last_pos as usize) % RING_BYTES) as u64;
        if delta > 0 {
            // Zero [last_pos, pos): already fetched by the DMA engine.
            let a = self.last_pos as usize;
            let b = pos as usize;
            unsafe {
                if b > a {
                    core::ptr::write_bytes((self.ring + a) as *mut u8, 0, b - a);
                } else {
                    core::ptr::write_bytes((self.ring + a) as *mut u8, 0, RING_BYTES - a);
                    core::ptr::write_bytes(self.ring as *mut u8, 0, b);
                }
            }
            self.played += delta;
            self.last_pos = pos;
        }
        let guard = self.guard();
        if self.written < self.played + guard {
            // `was_ahead` = a producer queued PCM since the last resync: only
            // then is running dry a starvation (idle streams resync silently).
            if self.was_ahead {
                self.underruns += 1;
                if self.underruns <= 8 || self.underruns % 100 == 0 {
                    hlog!("[HDA] writer starved #{} at t={} ms ({} B behind; the end of every playback counts once)\n",
                          self.underruns, monotonic_us() / 1000,
                          (self.played + guard).saturating_sub(self.written));
                }
            }
            self.was_ahead = false;
            self.written = self.played + guard.max(RESYNC_PRIME);
        }
    }

    pub fn queued_bytes(&self) -> u64 { self.written.saturating_sub(self.played) }

    pub fn tx_level(&self) -> u16 {
        ((self.queued_bytes() + TX_BUF_BYTES as u64 - 1) / TX_BUF_BYTES as u64).min(u16::MAX as u64) as u16
    }

    /// Monotonic progress counter: changes whenever the DMA engine advances.
    pub fn tx_used_idx(&self) -> u16 { (self.played / TX_BUF_BYTES as u64) as u16 }

    /// The tick-pump hook: just the position update (the hardware never
    /// stops on underrun; consumed regions are already silent).
    pub fn top_up_silence(&mut self) { self.update(); }

    #[inline]
    fn put_frame(&mut self, l: i16, r: i16) {
        let off = (self.written % RING_BYTES as u64) as usize;
        let v = (l as u16 as u32) | ((r as u16 as u32) << 16);
        unsafe { core::ptr::write_volatile((self.ring + off) as *mut u32, v); }
        self.written += 4;
    }

    /// Non-blocking: queue as much of `data` (interleaved S16LE at the
    /// configured rate/channels) as fits; returns input bytes consumed
    /// (whole input frames only).
    pub fn send_pcm_data(&mut self, data: &[u8]) -> usize {
        if !self.initialized || !self.stream_active { return 0; }
        self.update();
        let space = MAX_INFLIGHT.saturating_sub(self.queued_bytes()) as usize;
        let ch = self.in_channels.max(1) as usize;
        let in_frame = ch * 2;
        let frames = data.len() / in_frame;
        if frames == 0 { return 0; }
        let rd = |i: usize, c: usize| -> i16 {
            let o = i * in_frame + c * 2;
            i16::from_le_bytes([data[o], data[o + 1]])
        };
        if self.hw_rate == self.in_rate && ch == 2 {
            // Fast path: straight copy, wrapping.
            let n = (frames * 4).min(space) & !3;
            if n == 0 { return 0; }
            let off = (self.written % RING_BYTES as u64) as usize;
            let first = n.min(RING_BYTES - off);
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), (self.ring + off) as *mut u8, first);
                if n > first {
                    core::ptr::copy_nonoverlapping(data.as_ptr().add(first), self.ring as *mut u8, n - first);
                }
            }
            fence(Ordering::SeqCst);
            self.written += n as u64;
            self.was_ahead = true;
            return n;
        }
        let mut out_space = space / 4;
        let mut consumed = 0usize;
        if self.hw_rate == self.in_rate {
            // Channel conversion only.
            while consumed < frames && out_space > 0 {
                let l = rd(consumed, 0);
                let r = if ch >= 2 { rd(consumed, 1) } else { l };
                self.put_frame(l, r);
                out_space -= 1;
                consumed += 1;
            }
        } else {
            // Linear interpolation; at most ceil(65536/step)+1 outputs per input.
            let max_out = (65536 / self.resamp.step.max(1)) as usize + 2;
            while consumed < frames && out_space >= max_out {
                let l = rd(consumed, 0);
                let r = if ch >= 2 { rd(consumed, 1) } else { l };
                if !self.resamp.primed {
                    self.resamp.prev = [l, r];
                    self.resamp.primed = true;
                    consumed += 1;
                    continue;
                }
                let [pl, pr] = self.resamp.prev;
                while self.resamp.frac < 65536 {
                    let f = self.resamp.frac as i32;
                    let ol = pl as i32 + (((l as i32 - pl as i32) * f) >> 16);
                    let or = pr as i32 + (((r as i32 - pr as i32) * f) >> 16);
                    self.put_frame(ol as i16, or as i16);
                    out_space -= 1;
                    self.resamp.frac += self.resamp.step;
                }
                self.resamp.frac -= 65536;
                self.resamp.prev = [l, r];
                consumed += 1;
            }
        }
        fence(Ordering::SeqCst);
        if consumed > 0 { self.was_ahead = true; }
        consumed * in_frame
    }

    /// Human-readable state for the SND_GET_INFO ioctl (sndtest).
    pub fn info(&mut self) -> String {
        self.update();
        let mut s = String::from("backend: hda\n");
        s.push_str(&self.summary);
        let lpib = if self.sd != 0 { self.r32(self.sd + SD_LPIB) } else { 0 };
        let posbuf = if self.posbuf != 0 {
            unsafe { core::ptr::read_volatile((self.posbuf as *const u32).add(self.sd_index as usize * 2)) }
        } else { 0 };
        let ctl = if self.sd != 0 { self.r8(self.sd + SD_CTL0) } else { 0 };
        let _ = writeln!(s, "stream: active={} run={} in {} Hz x{} hw {} Hz lpib {} posbuf {} sts {:#04x}",
                         self.stream_active, ctl & SD_CTL_RUN != 0, self.in_rate, self.in_channels, self.hw_rate,
                         lpib, posbuf, if self.sd != 0 { self.r8(self.sd + SD_STS) } else { 0 });
        let _ = writeln!(s, "counters: played {} written {} queued {} starved {} restarts {} dma_errors {} cmd_timeouts {} intsts {:#x}",
                         self.played, self.written, self.queued_bytes(), self.underruns, self.restarts,
                         self.dma_errors, self.cmd_timeouts, self.r32(INTSTS));
        s
    }
}

fn codec_name(vid: u32) -> Option<&'static str> {
    Some(match vid {
        0x10ec0887 => "Realtek ALC887",
        0x10ec0888 => "Realtek ALC888",
        0x10ec0892 => "Realtek ALC892",
        0x10ec0897 => "Realtek ALC897",
        0x10ec0899 => "Realtek ALC898",
        0x10ec0900 => "Realtek ALC1150",
        0x10ec0662 => "Realtek ALC662",
        0x10ec0668 => "Realtek ALC668",
        0x10ec0867 => "Realtek ALC891",
        0x10ec1168 => "Realtek ALC1220",
        0x10ec1220 => "Realtek ALC1220",
        0x14f15051 => "Conexant",
        0x11060397 => "VIA VT1708S",
        0x80862809 | 0x8086280b | 0x8086280d => "Intel HDMI",
        v if v >> 16 == 0x1af4 => "QEMU emulated codec",
        _ => return None,
    })
}
