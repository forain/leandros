//! HD Audio beyond the playback path (lane lapaudio2): microphone capture,
//! headphone jack detection with speaker auto-mute, headset-mic switching,
//! and enumeration of the display (HDMI/DP) codec that shares the link.
//!
//! Written from the Intel High Definition Audio Specification rev 1.0a
//! (input stream descriptors §3.3.35, pin widget control / pin sense §7.3.3.13
//! and §7.3.3.15, HDMI ELD data §7.3.3.36 of the HDMI/DP addendum); Linux
//! sound/pci/hda was read as documentation only.
//!
//! - Capture: one input stream descriptor (the first one), 48 kHz 16-bit
//!   stereo, a 64 KiB cyclic buffer like the playback ring. The ADC → mux →
//!   mic-pin path is found by walking connection lists from every ADC; the
//!   internal (fixed) mic is the default, the jack (headset) mic wins while
//!   its pin senses presence. Software keeps `cap_total` (from LPIB deltas)
//!   and `cap_read`; a reader that falls a ring behind skips ahead (overrun).
//!   The stream runs only while an open of /dev/pipewire reads it.
//! - Jack: interrupts stay disabled (the playback design), so presence is
//!   polled from the audio server's 100 Hz pump, throttled to ~3 Hz. A
//!   headphone pin that senses a plug turns the speaker pins' output off.
//! - HDMI/DP: every other codec in STATESTS is walked read-only for the
//!   report (Intel's "enable all pins/converters" vendor bit is set first),
//!   with pin sense, ELD-valid and the monitor name from the ELD. Playback
//!   through it needs the i915 side (ELD/audio enable, N/M) and is not done.

use super::*;
use core::fmt::Write as _;

const CAP_TAG: u8 = 2;
const PINCAP_IN: u32 = 1 << 5;
const PINCTL_IN_EN: u32 = 1 << 5;
const V_GET_DIP_SIZE: u32 = 0xF2E;
const V_GET_ELDD: u32 = 0xF2F;
const INTEL_VENDOR_NID: u16 = 0x08;
const INTEL_GET_VENDOR: u32 = 0xF81;
const INTEL_SET_VENDOR: u32 = 0x781;
const JACK_POLL_US: u64 = 300_000;

pub(super) struct Ext {
    // capture paths: ADC first, the mic pin last; (nid, conn index to next)
    int_path: Vec<(u16, u8)>,
    jack_path: Vec<(u16, u8)>,
    cur_path: Vec<(u16, u8)>,
    cap_sd: usize,
    cap_sd_index: u8,
    cap_bdl: usize,
    cap_bdl_phys: usize,
    cap_ring: usize,
    cap_ring_phys: usize,
    pub(super) cap_active: bool,
    cap_last_pos: u32,
    cap_total: u64,
    cap_read: u64,
    cap_overruns: u32,
    cap_starts: u32,
    cap_dma_errors: u32,
    cap_logged_errs: u32,
    cap_restarts: u32,
    cap_stalled: bool,
    cap_move_us: u64,
    cap_fifo: u32,
    // jack
    hp_pins: Vec<u16>,
    /// Speaker pins the playback path enabled, with their original PIN_CTL.
    spk_saved: Vec<(u16, u32)>,
    hp_present: Option<bool>,
    mic_present: Option<bool>,
    last_poll_us: u64,
    jack_switches: u32,
    // display codec
    hdmi_summary: String,
    display: Option<(u8, u16)>,
}

impl Ext {
    pub(super) const fn new() -> Self {
        Self {
            int_path: Vec::new(), jack_path: Vec::new(), cur_path: Vec::new(),
            cap_sd: 0, cap_sd_index: 0, cap_bdl: 0, cap_bdl_phys: 0, cap_ring: 0, cap_ring_phys: 0,
            cap_active: false, cap_last_pos: 0, cap_total: 0, cap_read: 0, cap_overruns: 0, cap_starts: 0,
            cap_dma_errors: 0, cap_logged_errs: 0, cap_restarts: 0, cap_stalled: false, cap_move_us: 0, cap_fifo: 0,
            hp_pins: Vec::new(), spk_saved: Vec::new(), display: None, hp_present: None, mic_present: None,
            last_poll_us: 0, jack_switches: 0, hdmi_summary: String::new(),
        }
    }
}

fn path_str(p: &[(u16, u8)]) -> String {
    let mut s = String::new();
    for (i, (n, _)) in p.iter().enumerate() {
        let _ = write!(s, "{}{:#04x}", if i > 0 { "<-" } else { "" }, n);
    }
    if s.is_empty() { s.push_str("none"); }
    s
}

impl Hda {
    /// Called once after the playback paths are programmed.
    pub(super) fn ext_probe(&mut self, statests: u16) {
        self.find_capture_paths();
        self.find_jack_pins();
        if let Err(e) = self.alloc_capture() {
            hlog!("[HDA] capture: {}\n", e);
        }
        let main = self.cad;
        for cad in 0..15u8 {
            if cad == main || statests & (1 << cad) == 0 { continue; }
            self.probe_display_codec(cad);
        }
        self.cad = main;
        self.poll_jacks(true);
        let mut s = String::new();
        let _ = writeln!(s, "[HDA] capture: internal mic {} jack mic {} input stream {}",
                         path_str(&self.ext.int_path), path_str(&self.ext.jack_path), self.ext.cap_sd_index);
        let _ = writeln!(s, "[HDA] jack: headphone pins {:x?} speaker pins {:x?}", self.ext.hp_pins,
                         self.ext.spk_saved.iter().map(|&(n, _)| n).collect::<Vec<_>>());
        s.push_str(&self.ext.hdmi_summary.clone());
        for line in s.lines() { hlog!("{}\n", line); }
        self.summary.push_str(&s);
    }

    // ── capture path discovery ──

    fn is_mic_pin(&self, nid: u16) -> Option<bool> {
        let w = self.widget(nid)?;
        if w.wtype != W_PIN || w.pincap & PINCAP_IN == 0 { return None; }
        let conn = w.cfg >> 30;
        if (w.cfg >> 20) & 0xF != 0xA || conn == 1 { return None; }
        // Some(true) = internal (fixed or location "internal"), false = jack.
        Some(conn == 2 || (w.cfg >> 24) & 0x3F == 0x10 || conn == 3)
    }

    fn capture_dfs(&self, nid: u16, depth: u8, path: &mut Vec<(u16, u8)>, out: &mut Vec<Vec<(u16, u8)>>) {
        if depth > 5 || path.iter().any(|p| p.0 == nid) { return; }
        let w = match self.widget(nid) { Some(w) => w.clone(), None => return };
        if w.wtype == W_PIN {
            if self.is_mic_pin(nid).is_some() {
                path.push((nid, 0));
                out.push(path.clone());
                path.pop();
            }
            return;
        }
        if depth > 0 && !(w.wtype == W_MIXER || w.wtype == W_SELECTOR) { return; }
        for (i, c) in w.conns.iter().enumerate() {
            path.push((nid, i as u8));
            self.capture_dfs(*c, depth + 1, path, out);
            path.pop();
        }
    }

    fn find_capture_paths(&mut self) {
        let adcs: Vec<u16> = self.widgets.iter()
            .filter(|w| w.wtype == W_ADC && w.caps & WCAP_DIGITAL == 0).map(|w| w.nid).collect();
        let mut all = Vec::new();
        for a in adcs {
            let mut p = Vec::new();
            self.capture_dfs(a, 0, &mut p, &mut all);
        }
        // Shortest path per kind; prefer distinct ADCs is unnecessary (one
        // stream at a time).
        all.sort_by_key(|p| p.len());
        for p in all {
            let pin = p.last().unwrap().0;
            match self.is_mic_pin(pin) {
                Some(true) if self.ext.int_path.is_empty() => self.ext.int_path = p,
                Some(false) if self.ext.jack_path.is_empty() => self.ext.jack_path = p,
                _ => {}
            }
        }
    }

    fn find_jack_pins(&mut self) {
        for p in self.pins.clone() {
            let w = match self.widget(p.nid) { Some(w) => w.clone(), None => continue };
            match (w.cfg >> 20) & 0xF {
                2 if w.pincap & PINCAP_PRES_DETECT != 0 && (w.cfg >> 8) & 1 == 0 => self.ext.hp_pins.push(p.nid),
                // Unconnected pins (default config 0x411111f0 and kin) are
                // left alone; only speakers the playback path switched on.
                1 if (w.cfg >> 30) != 1 => {
                    let ctl = self.verb(p.nid, V_GET_PIN_CTL, 0).unwrap_or(0) & 0xFF;
                    if ctl & PINCTL_OUT_EN != 0 { self.ext.spk_saved.push((p.nid, ctl)); }
                }
                _ => {}
            }
        }
    }

    fn alloc_capture(&mut self) -> Result<(), &'static str> {
        let iss = (self.gcap >> 8) & 0xF;
        if iss == 0 { return Err("no input stream descriptor"); }
        if self.ext.int_path.is_empty() && self.ext.jack_path.is_empty() { return Err("no ADC -> mic pin path"); }
        const LIMIT: usize = 0x1_0000_0000;
        let order = (RING_BYTES / 4096).trailing_zeros() as usize;
        let p = buddy::alloc_below(order, LIMIT).ok_or("out of DMA memory")?;
        let v = phys_to_virt(p);
        unsafe { core::ptr::write_bytes(v as *mut u8, 0, RING_BYTES); }
        self.ext.cap_ring_phys = p;
        self.ext.cap_ring = v;
        // The playback BDL uses 256 bytes of its page; ours sits at +2 KiB.
        self.ext.cap_bdl = self.bdl + 2048;
        self.ext.cap_bdl_phys = self.bdl_phys + 2048;
        for i in 0..BDL_ENTRIES {
            unsafe {
                let e = (self.ext.cap_bdl as *mut BdlEntry).add(i);
                core::ptr::write_volatile(e, BdlEntry { addr: (p + i * BDL_CHUNK) as u64, len: BDL_CHUNK as u32, ioc: 0 });
            }
        }
        self.ext.cap_sd_index = 0;
        self.ext.cap_sd = SD_BASE;
        fence(Ordering::SeqCst);
        Ok(())
    }

    // ── capture stream ──

    /// Program `path` (ADC first, pin last) for input.
    fn program_capture_path(&mut self, path: &[(u16, u8)], fmt: u16) {
        let n = path.len();
        for k in 0..n {
            let (nid, ci) = path[k];
            let w = match self.widget(nid) { Some(w) => w.clone(), None => continue };
            self.power_up(nid);
            if k + 1 == n {
                // Mic pin: input enable, VREF 80 % on a jack that offers it.
                let vref = (w.pincap >> 8) & 0xFF;
                let v = if (w.cfg >> 30) == 0 && vref & (1 << 4) != 0 { 4 } else { 0 };
                self.verb(nid, V_SET_PIN_CTL, PINCTL_IN_EN | v);
                if w.caps & WCAP_IN_AMP != 0 {
                    // Boost amp: one step above its 0 dB point.
                    let steps = (w.amp_in >> 8) & 0x7F;
                    let g = (Self::amp_gain_0db(w.amp_in) + 1).min(steps);
                    self.verb4(nid, V4_SET_AMP, (1 << 14) | (1 << 13) | (1 << 12) | g);
                }
                continue;
            }
            match w.wtype {
                W_MIXER => {
                    for i in 0..w.conns.len().min(16) { self.set_in_amp(nid, i as u8, i as u8 == ci); }
                    self.set_out_amp(nid);
                }
                _ => {
                    if w.conns.len() > 1 { self.verb(nid, V_SET_CONN_SEL, ci as u32); }
                    if w.caps & WCAP_IN_AMP != 0 {
                        // ADC/selector input amp: 0 dB, unmuted, on the selected input and on index 0.
                        self.set_in_amp(nid, ci, true);
                        if ci != 0 { self.set_in_amp(nid, 0, true); }
                    }
                    self.set_out_amp(nid);
                }
            }
            if k == 0 {
                self.verb4(nid, V4_SET_FMT, fmt as u32);
                self.verb(nid, V_SET_STREAM, (CAP_TAG as u32) << 4);
            }
        }
    }

    fn capture_stop(&mut self) {
        let sd = self.ext.cap_sd;
        if sd == 0 { return; }
        let ctl = self.r8(sd + SD_CTL0);
        self.w8(sd + SD_CTL0, ctl & !(SD_CTL_RUN | 0x1C));
        wait_for(1000, || self.r8(sd + SD_CTL0) & SD_CTL_RUN == 0);
        if let Some(&(adc, _)) = self.ext.cur_path.first() { self.verb(adc, V_SET_STREAM, 0); }
        // Mic bias and input off.
        if self.ext.cur_path.len() > 1 {
            if let Some(&(pin, _)) = self.ext.cur_path.last() { self.verb(pin, V_SET_PIN_CTL, 0); }
        }
        self.ext.cap_active = false;
    }

    fn capture_start(&mut self) -> bool {
        let sd = self.ext.cap_sd;
        if sd == 0 { return false; }
        self.capture_stop();
        let use_jack = self.ext.mic_present == Some(true) && !self.ext.jack_path.is_empty();
        let path = if use_jack || self.ext.int_path.is_empty() { self.ext.jack_path.clone() } else { self.ext.int_path.clone() };
        let fmt = stream_format(48000, 2).unwrap_or(0x0011);
        self.w8(sd + SD_CTL0, SD_CTL_SRST);
        wait_for(1000, || self.r8(sd + SD_CTL0) & SD_CTL_SRST != 0);
        udelay(20);
        self.w8(sd + SD_CTL0, 0);
        wait_for(1000, || self.r8(sd + SD_CTL0) & SD_CTL_SRST == 0);
        self.w8(sd + SD_STS, SD_STS_BCIS | SD_STS_FIFOE | SD_STS_DESE);
        self.w8(sd + SD_CTL2, (self.r8(sd + SD_CTL2) & 0x07) | (CAP_TAG << 4));
        self.w32(sd + SD_CBL, RING_BYTES as u32);
        self.w16(sd + SD_LVI, (BDL_ENTRIES - 1) as u16);
        self.w16(sd + SD_FMT, fmt);
        self.w32(sd + SD_BDPL, self.ext.cap_bdl_phys as u32);
        self.w32(sd + SD_BDPU, (self.ext.cap_bdl_phys as u64 >> 32) as u32);
        self.w32(SSYNC, self.r32(SSYNC) & !(1u32 << self.ext.cap_sd_index));
        self.program_capture_path(&path, fmt);
        self.ext.cur_path = path;
        fence(Ordering::SeqCst);
        self.w8(sd + SD_CTL0, SD_CTL_RUN);
        let ok = wait_for(1000, || self.r8(sd + SD_CTL0) & SD_CTL_RUN != 0);
        self.ext.cap_fifo = (((self.r16(sd + SD_FIFOS) as u32) + 1 + 3) & !3).min(256);
        self.ext.cap_move_us = monotonic_us();
        self.ext.cap_stalled = false;
        self.ext.cap_last_pos = 0;
        self.ext.cap_total = 0;
        self.ext.cap_read = 0;
        self.ext.cap_active = ok;
        self.ext.cap_starts += 1;
        hlog!("[HDA] capture stream {} running={} fmt {:#06x} path {}\n",
              self.ext.cap_sd_index, ok, fmt, path_str(&self.ext.cur_path));
        ok
    }

    /// MMIO only (runs from the tick): fold the DMA position into the byte
    /// counters, count DMA errors, note a stuck LPIB.
    pub(super) fn capture_update(&mut self) {
        if !self.ext.cap_active { return; }
        let sd = self.ext.cap_sd;
        let sts = self.r8(sd + SD_STS);
        if sts & (SD_STS_FIFOE | SD_STS_DESE) != 0 { self.ext.cap_dma_errors += 1; }
        if sts != 0 { self.w8(sd + SD_STS, sts & (SD_STS_BCIS | SD_STS_FIFOE | SD_STS_DESE)); }
        let pos = self.r32(sd + SD_LPIB);
        fence(Ordering::Acquire);
        if pos as usize >= RING_BYTES { return; }
        let now = monotonic_us();
        if pos != self.ext.cap_last_pos { self.ext.cap_move_us = now; self.ext.cap_stalled = false; }
        else if now.wrapping_sub(self.ext.cap_move_us) >= 100_000 { self.ext.cap_stalled = true; }
        let delta = ((pos as usize + RING_BYTES - self.ext.cap_last_pos as usize) % RING_BYTES) as u64;
        self.ext.cap_total += delta;
        self.ext.cap_last_pos = pos;
    }

    /// Thread context: make sure the capture stream runs (the only place it
    /// is started, so a read racing a close cannot revive it).
    pub fn capture_ensure(&mut self) -> Result<(), i32> {
        if !self.initialized || self.ext.cap_sd == 0 { return Err(-19); } // ENODEV
        if !self.ext.cap_active && !self.capture_start() { return Err(-5); }
        Ok(())
    }

    /// Thread context: restart a stalled stream.
    pub fn capture_restart(&mut self) -> bool { self.ext.cap_restarts += 1; self.capture_start() }

    /// Bytes ready to read (behind the FIFO's reach); no MMIO.
    pub fn capture_pending(&self) -> usize {
        if !self.ext.cap_active { return 0; }
        ((self.ext.cap_total - self.ext.cap_read) as usize).saturating_sub(self.ext.cap_fifo as usize)
    }

    /// Copy captured 48 kHz s16le stereo into `out`. Never 0: -EAGAIN when
    /// nothing is there yet (the caller decides whether to wait).
    pub fn capture_read(&mut self, out: &mut [u8]) -> Result<usize, i32> {
        if !self.initialized || self.ext.cap_sd == 0 { return Err(-19); } // ENODEV
        if out.len() < 4 { return Err(-22); }
        if !self.ext.cap_active { return Err(-11); }
        self.capture_update();
        let avail = self.ext.cap_total - self.ext.cap_read;
        if avail > (RING_BYTES - 8192) as u64 {
            self.ext.cap_overruns += 1;
            self.ext.cap_read = self.ext.cap_total - (RING_BYTES / 2) as u64;
        }
        let n = self.capture_pending().min(out.len()) & !3;
        if n == 0 { return Err(-11); }
        let mut done = 0;
        while done < n {
            let off = ((self.ext.cap_read + done as u64) % RING_BYTES as u64) as usize;
            let k = (n - done).min(RING_BYTES - off);
            unsafe { core::ptr::copy_nonoverlapping((self.ext.cap_ring + off) as *const u8, out[done..].as_mut_ptr(), k); }
            done += k;
        }
        self.ext.cap_read += n as u64;
        Ok(n)
    }

    /// Thread context, every ~100 ms: jack sense, DMA-error log, stall restart.
    pub fn jack_service(&mut self) {
        if !self.initialized { return; }
        self.poll_jacks(false);
        if self.ext.cap_dma_errors != self.ext.cap_logged_errs {
            if self.ext.cap_logged_errs < 4 { hlog!("[HDA] capture DMA error (FIFOE/DESE), {} so far\n", self.ext.cap_dma_errors); }
            self.ext.cap_logged_errs = self.ext.cap_dma_errors;
        }
        if self.ext.cap_active && self.ext.cap_stalled {
            hlog!("[HDA] capture LPIB stuck for 100 ms; restarting the stream\n");
            self.capture_restart();
        }
    }

    /// Display codec to D3 (kexec/reboot quiesce).
    pub(super) fn shutdown_display(&mut self) {
        if let Some((cad, fg)) = self.ext.display.take() {
            let main = self.cad;
            self.cad = cad;
            self.no_fallback = true;
            self.verb(fg, V_SET_POWER, 3);
            self.no_fallback = false;
            self.cad = main;
        }
    }

    /// The reading open went away.
    pub fn capture_close(&mut self) {
        if self.ext.cap_active { self.capture_stop(); }
    }

    // ── jack detection ──

    fn pin_present(&mut self, nid: u16) -> Option<bool> {
        let w = self.widget(nid)?.clone();
        if w.pincap & PINCAP_PRES_DETECT == 0 { return None; }
        if w.pincap & PINCAP_TRIG_REQ != 0 { self.verb(nid, V_EXEC_PIN_SENSE, 0); udelay(100); }
        self.verb(nid, V_GET_PIN_SENSE, 0).map(|r| r & 0x8000_0000 != 0)
    }

    /// Re-sense the headphone and jack-mic pins (throttled unless `force`)
    /// and apply speaker auto-mute / capture source changes.
    pub(super) fn poll_jacks(&mut self, force: bool) {
        let now = monotonic_us();
        if !force && now.wrapping_sub(self.ext.last_poll_us) < JACK_POLL_US { return; }
        self.ext.last_poll_us = now;
        let mut hp = None;
        for nid in self.ext.hp_pins.clone() {
            if let Some(p) = self.pin_present(nid) { hp = Some(hp.unwrap_or(false) | p); }
        }
        if hp.is_some() && hp != self.ext.hp_present {
            let prev = self.ext.hp_present;
            self.ext.hp_present = hp;
            let on = hp != Some(true);
            // The first reading with nothing plugged only records state.
            if prev.is_some() || hp == Some(true) {
                self.ext.jack_switches += 1;
                for (nid, saved) in self.ext.spk_saved.clone() {
                    self.verb(nid, V_SET_PIN_CTL, if on { saved } else { saved & !PINCTL_OUT_EN });
                }
                if !force { hlog!("[HDA] jack: headphone {} -> speakers {}\n", if on { "unplugged" } else { "plugged" }, if on { "on" } else { "muted" }); }
            }
        }
        let mic = match self.ext.jack_path.last() { Some(&(n, _)) => self.pin_present(n), None => None };
        if mic.is_some() && mic != self.ext.mic_present {
            self.ext.mic_present = mic;
            if !force {
                self.ext.jack_switches += 1;
                hlog!("[HDA] jack: headset mic {}\n", if mic == Some(true) { "plugged" } else { "unplugged" });
                if self.ext.cap_active { self.capture_start(); }
            }
        }
    }

    /// Raw pin-sense words of every pin with presence detect.
    fn pin_sense_report(&mut self, s: &mut String) {
        let pins: Vec<(u16, u32, u32)> = self.widgets.iter()
            .filter(|w| w.wtype == W_PIN && w.pincap & PINCAP_PRES_DETECT != 0).map(|w| (w.nid, w.cfg, w.pincap)).collect();
        for (nid, cfg, pincap) in pins {
            if pincap & PINCAP_TRIG_REQ != 0 { self.verb(nid, V_EXEC_PIN_SENSE, 0); udelay(100); }
            let r = self.verb(nid, V_GET_PIN_SENSE, 0).unwrap_or(0xFFFF_FFFF);
            let pc = self.verb(nid, V_GET_PIN_CTL, 0).unwrap_or(0xFF);
            let _ = writeln!(s, "pin {:#04x} {} {} sense {:#010x} present={} pinctl {:#04x}",
                             nid, location_name(cfg), device_name(cfg), r, r >> 31, pc);
        }
    }

    pub(super) fn ext_info(&mut self, s: &mut String) {
        self.capture_update();
        let _ = writeln!(s, "capture: active={} path {} total {} read {} overruns {} starts {} dmaerr {} restarts {} lpib {}",
                         self.ext.cap_active, path_str(&self.ext.cur_path), self.ext.cap_total, self.ext.cap_read,
                         self.ext.cap_overruns, self.ext.cap_starts, self.ext.cap_dma_errors, self.ext.cap_restarts,
                         if self.ext.cap_sd != 0 { self.r32(self.ext.cap_sd + SD_LPIB) } else { 0 });
        let _ = writeln!(s, "jack: headphone={:?} headset_mic={:?} switches {}",
                         self.ext.hp_present, self.ext.mic_present, self.ext.jack_switches);
        self.pin_sense_report(s);
    }

    // ── display (HDMI/DP) codec ──

    fn probe_display_codec(&mut self, cad: u8) {
        let t = self.cmd_timeouts;
        self.no_fallback = true;
        self.probe_display_codec_inner(cad);
        self.no_fallback = false;
        if self.cmd_timeouts != t {
            hlog!("[HDA] display codec {} stopped answering; left alone\n", cad);
            self.ext.display = None;
        }
    }

    fn probe_display_codec_inner(&mut self, cad: u8) {
        self.cad = cad;
        let mut s = String::new();
        let vid = match self.verb(0, V_GET_PARAM, P_VENDOR) { Some(v) => v, None => return };
        let sub = self.param(0, P_SUBNODES);
        let fg = ((sub >> 16) & 0xFF) as u16;
        if self.param(fg, P_FG_TYPE) & 0xFF != 0x01 { return; }
        self.ext.display = Some((cad, fg));
        self.verb(fg, V_SET_POWER, 0);
        udelay(10_000);
        if vid >> 16 == 0x8086 {
            // Intel display codec: expose the 2nd/3rd pins and converters.
            if let Some(v) = self.verb(INTEL_VENDOR_NID, INTEL_GET_VENDOR, 0) {
                if v & 1 == 0 { self.verb(INTEL_VENDOR_NID, INTEL_SET_VENDOR, (v | 1) & 0xFF); }
            }
        }
        let sub = self.param(fg, P_SUBNODES);
        let (start, count) = ((sub >> 16) & 0xFF, sub & 0xFF);
        let mut cvts = Vec::new();
        let mut pins = 0;
        let _ = writeln!(s, "[HDA] display codec {}: {:04x}:{:04x}{} afg {:#04x} widgets {}", cad, vid >> 16, vid & 0xFFFF,
                         codec_name(vid).map(|n| alloc::format!(" ({})", n)).unwrap_or_default(), fg, count);
        for nid in start..start + count {
            let nid = nid as u16;
            let caps = self.param(nid, P_WCAPS);
            let t = ((caps >> 20) & 0xF) as u8;
            if t == W_DAC { cvts.push(nid); continue; }
            if t != W_PIN { continue; }
            pins += 1;
            let pincap = self.param(nid, P_PINCAP);
            let cfg = self.verb(nid, V_GET_CFG_DEFAULT, 0).unwrap_or(0);
            let conns = if caps & WCAP_CONN_LIST != 0 { self.read_conns(nid) } else { Vec::new() };
            if pincap & PINCAP_TRIG_REQ != 0 { self.verb(nid, V_EXEC_PIN_SENSE, 0); udelay(100); }
            let sense = self.verb(nid, V_GET_PIN_SENSE, 0).unwrap_or(0);
            let mut eld = String::new();
            if sense & (1 << 30) != 0 {
                let size = (self.verb(nid, V_GET_DIP_SIZE, 1 << 3).unwrap_or(0) & 0xFF) as usize;
                let mut b = Vec::new();
                for i in 0..size.min(96) {
                    match self.verb(nid, V_GET_ELDD, i as u32) { Some(r) if r & 0x8000_0000 != 0 => b.push(r as u8), _ => break }
                }
                if b.len() > 20 {
                    let mnl = (b[4] & 0x1F) as usize;
                    let sads = (b[5] >> 4) as usize;
                    let name: String = b[20..(20 + mnl).min(b.len())].iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '.' }).collect();
                    let _ = write!(eld, " eld {} B monitor \"{}\" {} SAD(s)", b.len(), name, sads);
                }
            }
            let _ = writeln!(s, "[HDA]   pin {:#04x} {} pincap {:08x} cfg {:08x} conn {:x?} sense {:#010x} present={} eld_valid={}{}",
                             nid, if pincap & PINCAP_DP != 0 { "DP/HDMI" } else if pincap & PINCAP_HDMI != 0 { "HDMI" } else { "pin" },
                             pincap, cfg, conns, sense, sense >> 31, (sense >> 30) & 1, eld);
        }
        let _ = writeln!(s, "[HDA]   converters {:x?}, {} pin(s); playback needs the i915 audio enable (not done)", cvts, pins);
        self.ext.hdmi_summary.push_str(&s);
    }
}
