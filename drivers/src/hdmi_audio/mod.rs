//! HDMI audio output on the Raspberry Pi 5 (BCM2712 D0, hdmi0), a PipeWire backend with the same
//! seven calls as `hda::Hda`. The firmware owns the video mode; we add only the audio side, as
//! Linux `vc4_hdmi` does after a mode set: open the DVP audio clock gate, program MAI (sample
//! clock, format, D0 threshold layout), the audio packet / channel map, ACR N/CTS from the pixel
//! clock the boot probe measured, and the audio InfoFrame (packet RAM slot 4, read-modify-write so
//! the firmware's AVI InfoFrame survives). PCM is packed into IEC958 subframes by the CPU and a
//! dma40 cyclic ring (channel 10, DREQ 10) feeds MAI_DATA.
//!
//! Inert unless `hdmi_probe` saw a sink on hdmi0 in HDMI (not DVI) mode with a known pixel clock.
//! `hdmi.audio=0` disables it. Log tag `[hdmiaud]`.

pub mod pure;

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::*;

#[cfg(not(all(target_arch = "aarch64", feature = "rpi5")))]
pub use stub::*;

pub type SerrorCheck = fn() -> Option<u64>;

#[cfg(not(all(target_arch = "aarch64", feature = "rpi5")))]
mod stub {
    use alloc::string::String;
    /// Builds without the rpi5 feature have no HDMI audio.
    pub struct HdmiAudio;
    pub fn set_serror_check(_f: super::SerrorCheck) {}
    /// True when a later `probe` could still succeed (only the rpi5 build, before the probe ran).
    pub fn may_appear(_cmdline: &str) -> bool { false }
    impl HdmiAudio {
        pub fn new() -> Self { HdmiAudio }
        pub fn probe(&mut self, _cmdline: &str) -> Result<(), &'static str> { Err("not an rpi5 build") }
        pub fn reconfigure_stream(&mut self, _id: u32, _freq: u32, _ch: u8) {}
        pub fn send_pcm_data(&mut self, _data: &[u8]) -> usize { 0 }
        pub fn tx_used_idx(&self) -> u16 { 0 }
        pub fn queued_bytes(&self) -> u64 { 0 }
        pub fn top_up_silence(&mut self) {}
        pub fn recover_stream(&mut self) {}
        pub fn info(&mut self) -> String { String::from("backend: hdmi (absent)\n") }
    }
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::pure::*;
    use crate::bcm2712_dma40 as dma;
    use crate::hdmi_probe as probe;
    use crate::pci::serial_debug;
    use alloc::format;
    use alloc::string::String;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use leandros_lib::mmio::{MmioRd, MmioWr};
    use mm::kva::{ioremap, iounmap, CacheMode};
    use mm::{buddy, phys_to_virt};

    const DVP_PHYS: u64 = 0x10_7c70_0000;
    const HD_PHYS: u64 = 0x10_7c72_0000;
    const HDMI0_PHYS: u64 = 0x10_7c70_1400;
    const PACKET0_PHYS: u64 = 0x10_7c70_3800;
    /// MAI_DATA for hdmi0: CPU physical (dma-ranges are the identity) and the legacy 32-bit
    /// `hd` reg cell Linux vc4 hands the DMA engine.
    const FIFO_CPU: u64 = HD_PHYS + HD_MAI_DATA as u64;
    const FIFO_BUS: u64 = 0x7c72_0000 + HD_MAI_DATA as u64;
    /// `dmas = <&dma40 0x41fa000a>` on the hdmi0 node (PER_MAP 10).
    const DREQ_CELL_HDMI0: u32 = 0x41fa_000a;
    const DMA_CHANNEL: u8 = 10;
    const RING_ORDER: usize = 5; // 32 pages >= RING_BYTES
    const STALL_TICKS: u32 = 30;

    static SERROR: AtomicUsize = AtomicUsize::new(0);

    pub fn set_serror_check(f: super::SerrorCheck) {
        SERROR.store(f as usize, Ordering::Release);
    }

    fn serror() -> Option<u64> {
        let f = SERROR.load(Ordering::Acquire);
        if f == 0 { return None; }
        let f: super::SerrorCheck = unsafe { core::mem::transmute(f) };
        f()
    }

    fn disabled(cmdline: &str) -> bool {
        cmdline.split_whitespace().any(|t| t == "hdmi.audio=0")
    }

    pub fn may_appear(cmdline: &str) -> bool {
        !disabled(cmdline) && probe::state() == probe::PROBE_NOT_RUN
    }

    #[inline]
    fn rd(base: usize, off: usize) -> u32 { unsafe { ((base + off) as *const u32).mmio_read() } }
    #[inline]
    fn wr(base: usize, off: usize, v: u32) { unsafe { ((base + off) as *mut u32).mmio_write(v) } }

    fn spin_us(us: u64) {
        let end = sched::monotonic_ns() + us * 1000;
        while sched::monotonic_ns() < end { core::hint::spin_loop(); }
    }

    pub struct HdmiAudio {
        initialized: bool,
        stream_active: bool,
        dvp: usize,
        hd: usize,
        hdmi: usize,
        pkt: usize,
        ring: usize,
        ring_phys: usize,
        chan: Option<dma::Channel>,
        fifo: u64,
        d0: bool,
        pix_khz: u32,
        n: u32,
        cts: u32,
        in_rate: u32,
        in_channels: u8,
        hw_rate: u32,
        resamp: Resampler,
        cs: [u8; 24],
        silence: [u32; FRAMES_PER_BLOCK * 2],
        r: Ring,
        stall: u32,
        mai_errs: u64,
        retrains: u64,
        restarts: u64,
        stalls: u64,
        summary: String,
    }

    impl HdmiAudio {
        pub fn new() -> Self {
            HdmiAudio {
                initialized: false, stream_active: false, dvp: 0, hd: 0, hdmi: 0, pkt: 0, ring: 0, ring_phys: 0,
                chan: None, fifo: 0, d0: true, pix_khz: 0, n: 0, cts: 0, in_rate: 48_000, in_channels: 2,
                hw_rate: 48_000, resamp: Resampler::new(48_000, 48_000), cs: [0; 24],
                silence: [0; FRAMES_PER_BLOCK * 2], r: Ring::default(), stall: 0, mai_errs: 0, retrains: 0,
                restarts: 0, stalls: 0, summary: String::new(),
            }
        }

        /// Checks the probe's verdict, maps the blocks, opens the gate, claims dma40 ch10 and
        /// starts a 48 kHz stereo stream of silence (proving the DMA path). Inert on any failure.
        pub fn probe(&mut self, cmdline: &str) -> Result<(), &'static str> {
            // PipeWire's first init runs before the HDMI probe; init.rs retries after it, quietly.
            if may_appear(cmdline) { return Err("hdmi probe has not run yet"); }
            let r = self.probe_inner(cmdline);
            match r {
                Ok(()) => serial_debug(&format!("[hdmiaud] ready: {}", self.summary)),
                Err(why) => {
                    self.teardown();
                    serial_debug(&format!("[hdmiaud] inert: {}\n", why));
                }
            }
            r
        }

        fn probe_inner(&mut self, cmdline: &str) -> Result<(), &'static str> {
            if disabled(cmdline) { return Err("disabled (hdmi.audio=0)"); }
            match probe::state() {
                probe::PROBE_DONE => {}
                probe::PROBE_NOT_RUN => return Err("hdmi probe has not run yet"),
                probe::PROBE_SKIPPED => return Err("hdmi probe skipped (hdmi.probe=0)"),
                _ => return Err("hdmi probe failed (nomap/serror)"),
            }
            let f = probe::PORT_FLAGS[0].load(Ordering::Relaxed);
            if f & probe::PORT_VALID == 0 { return Err("no hdmi0 probe data"); }
            if f & (probe::PORT_HPD | probe::PORT_ACTIVE) == 0 { return Err("no sink on hdmi0 (hpd=0 active=0)"); }
            if f & probe::PORT_HDMI_MODE == 0 { return Err("hdmi0 runs DVI (no audio; try hdmi_drive=2)"); }
            let sink = probe::EDID_SINK.load(Ordering::Relaxed);
            self.pix_khz = probe::PORT_PIX_KHZ[0].load(Ordering::Relaxed);
            if self.pix_khz == 0 { return Err("hdmi0 pixel clock unknown"); }
            self.d0 = crate::bcm2712_id::stepping().stepping != crate::bcm2712_id::Stepping::C0;

            let map = |phys: u64, len: usize| ioremap(phys, len, CacheMode::Uncached).unwrap_or(0);
            self.dvp = map(DVP_PHYS, 0x10);
            self.hd = map(HD_PHYS, 0x100);
            self.hdmi = map(HDMI0_PHYS, 0x300);
            self.pkt = map(PACKET0_PHYS, 0x400);
            if self.dvp == 0 || self.hd == 0 || self.hdmi == 0 || self.pkt == 0 { return Err("ioremap failed"); }
            if let Some(syn) = serror() {
                serial_debug(&format!("[hdmiaud] serror pending before setup syn=0x{:016x}\n", syn));
            }

            // Audio clock gate (MISC_CONFIG bit 3 set = closed): our first write, if needed.
            let misc = rd(self.dvp, DVP_MISC_CONFIG);
            let gate_was = if misc & dvp_gate_bit(0) != 0 {
                wr(self.dvp, DVP_MISC_CONFIG, misc & !dvp_gate_bit(0));
                "opened"
            } else {
                "open"
            };
            if serror().is_some() { return Err("serror at the dvp gate"); }

            let p = buddy::alloc(RING_ORDER).ok_or("out of memory for the ring")?;
            self.ring_phys = p;
            self.ring = phys_to_virt(p);
            self.chan = Some(dma::claim(Some(DMA_CHANNEL)).map_err(|_| "dma40 channel unavailable")?);

            self.initialized = true;
            self.in_rate = 48_000;
            self.in_channels = 2;
            self.program(48_000)?;
            // Try the CPU-physical FIFO first, then the 32-bit bus address.
            let mut how = "";
            for (fifo, name) in [(FIFO_CPU, "cpu"), (FIFO_BUS, "bus")] {
                self.fifo = fifo;
                match self.start_dma(true) {
                    Ok(()) => { how = name; break; }
                    Err(why) => serial_debug(&format!("[hdmiaud] fifo {} 0x{:x}: {}\n", name, fifo, why)),
                }
            }
            if how.is_empty() { return Err("dma never moved (DREQ 10 silent with both FIFO addresses)"); }
            let ch_id = self.chan.as_ref().map_or(0, |c| c.id());
            self.summary = format!(
                "hdmi0 pix={} kHz N={} CTS={} gate={} thr={} dma ch{} fifo={}(0x{:x}) {} edid={}\n",
                self.pix_khz, self.n, self.cts, gate_was, if self.d0 { "d0" } else { "c" }, ch_id, how, self.fifo,
                if dma::polled() { "poll" } else { "irq" },
                match sink { probe::SINK_HDMI => "hdmi", probe::SINK_DVI => "dvi(forced hdmi mode)", _ => "unknown" }
            );
            Ok(())
        }

        /// Programs MAI, packetizer, ACR and the InfoFrame for `fs` (Linux startup + prepare).
        fn program(&mut self, fs: u32) -> Result<(), &'static str> {
            let fmt = mai_fmt(fs).ok_or("unsupported rate")?;
            let (mut n, mut cts) = crate::hdmi_probe::decode::acr_n_cts(self.pix_khz, fs);
            if n == 0 || cts == 0 { (n, cts) = n_cts_fallback(self.pix_khz, fs); }
            self.n = n;
            self.cts = cts;
            self.hw_rate = fs;
            self.cs = channel_status(fs, 16);
            silence_block(&self.cs, &mut self.silence);
            let (hd, h) = (self.hd, self.hdmi);
            let mask = 0b11;
            wr(hd, HD_MAI_CTL, mai_ctl_startup());
            wr(hd, HD_MAI_SMP, mai_smp(AUDIO_CLOCK_HZ, fs));
            wr(hd, HD_MAI_CTL, mai_ctl_run(2));
            wr(hd, HD_MAI_FMT, fmt);
            wr(hd, HD_MAI_THR, mai_thr(self.d0));
            wr(h, HDMI_MAI_CONFIG, mai_config(mask));
            wr(h, HDMI_MAI_CHANNEL_MAP, channel_map(mask));
            wr(h, HDMI_AUDIO_PACKET_CONFIG, audio_packet_config(mask));
            wr(h, HDMI_CRP_CFG, crp_cfg(n));
            wr(h, HDMI_CTS_0, cts);
            wr(h, HDMI_CTS_1, cts);
            self.write_infoframe();
            if let Some(syn) = serror() {
                serial_debug(&format!("[hdmiaud] serror programming MAI/CRP syn=0x{:016x}\n", syn));
                return Err("serror while programming MAI/CRP");
            }
            Ok(())
        }

        /// `vc4_hdmi_write_infoframe` for the audio InfoFrame: stop slot 4, wait idle, write the
        /// slot, re-enable it; the other slot bits (AVI = 2) are preserved.
        fn write_infoframe(&mut self) {
            let h = self.hdmi;
            let bit = 1u32 << AUDIO_PACKET_ID;
            let cfg = rd(h, HDMI_RAM_PACKET_CONFIG);
            wr(h, HDMI_RAM_PACKET_CONFIG, cfg & !bit);
            let end = sched::monotonic_ns() + 100_000_000;
            while rd(h, HDMI_RAM_PACKET_STATUS) & bit != 0 && sched::monotonic_ns() < end { core::hint::spin_loop(); }
            let words = infoframe_words(&audio_infoframe(2));
            let base = self.pkt + AUDIO_PACKET_ID as usize * PACKET_STRIDE;
            for (i, w) in words.iter().enumerate() { wr(base, i * 4, *w); }
            let cfg = rd(h, HDMI_RAM_PACKET_CONFIG);
            wr(h, HDMI_RAM_PACKET_CONFIG, cfg | bit | RAM_PACKET_ENABLE);
        }

        fn fill_silence(&mut self, a: usize, b: usize) {
            if b <= a { return; }
            let ring = self.ring as *mut u32;
            for off in (a..b).step_by(4) {
                let w = (off % BLOCK_BYTES) / 4;
                unsafe { core::ptr::write_volatile(ring.add(off / 4), self.silence[w]); }
            }
            dma::dcache_clean_inval(self.ring + a, b - a);
        }

        /// Fills the ring with silence and starts the cyclic DMA into MAI_DATA. With `verify`,
        /// waits up to 40 ms for the position to move (DREQ pacing proven).
        fn start_dma(&mut self, verify: bool) -> Result<(), &'static str> {
            self.fill_silence(0, RING_BYTES);
            self.r.reset();
            self.stall = 0;
            let Some(ch) = self.chan.as_mut() else { return Err("no channel") };
            ch.start_cyclic(self.ring_phys as u64, RING_BYTES, PERIOD_BYTES, self.fifo, DREQ_CELL_HDMI0, true)
                .map_err(|_| "start_cyclic refused")?;
            if let Some(syn) = serror() {
                serial_debug(&format!("[hdmiaud] serror starting dma syn=0x{:016x}\n", syn));
                ch.stop();
                return Err("serror at dma start");
            }
            // Clear the underflow flags the empty FIFO raised before the first word.
            wr(self.hd, HD_MAI_CTL, mai_ctl_run(2) | MAI_CTL_ERRS);
            self.stream_active = true;
            if !verify { return Ok(()); }
            let first = ch.position();
            let end = sched::monotonic_ns() + 40_000_000;
            while sched::monotonic_ns() < end {
                ch.poll();
                let p = ch.position();
                if p.is_some() && p != first {
                    return Ok(());
                }
                spin_us(500);
            }
            let (cs, dbg) = ch.status();
            serial_debug(&format!("[hdmiaud] no progress pos={:?} cs=0x{:08x} debug=0x{:08x} mai_ctl=0x{:08x}\n",
                first, cs, dbg, rd(self.hd, HD_MAI_CTL)));
            ch.stop();
            self.stream_active = false;
            Err("no progress")
        }

        fn teardown(&mut self) {
            self.stream_active = false;
            if let Some(mut c) = self.chan.take() { c.stop(); }
            if self.hd != 0 && self.initialized {
                wr(self.hd, HD_MAI_CTL, MAI_CTL_RESET);
                wr(self.hd, HD_MAI_CTL, MAI_CTL_ERRORF);
                wr(self.hd, HD_MAI_CTL, MAI_CTL_FLUSH);
            }
            if self.ring_phys != 0 { buddy::free(self.ring_phys, RING_ORDER); }
            self.ring = 0;
            self.ring_phys = 0;
            for va in [self.dvp, self.hd, self.hdmi, self.pkt] { if va != 0 { iounmap(va); } }
            self.dvp = 0; self.hd = 0; self.hdmi = 0; self.pkt = 0;
            self.initialized = false;
        }

        pub fn reconfigure_stream(&mut self, _id: u32, freq: u32, channels: u8) {
            if !self.initialized { return; }
            self.stream_active = false;
            if let Some(c) = self.chan.as_mut() { c.stop(); }
            self.in_rate = if freq == 0 { 48_000 } else { freq };
            self.in_channels = channels.max(1);
            let hw = hw_rate_for(self.in_rate);
            self.resamp = Resampler::new(self.in_rate, hw);
            if let Err(why) = self.program(hw) {
                serial_debug(&format!("[hdmiaud] reprogram failed: {}\n", why));
                return;
            }
            serial_debug(&format!("[hdmiaud] SET_PARAMS {} Hz x{} -> hw {} Hz N={} CTS={}\n",
                self.in_rate, self.in_channels, hw, self.n, self.cts));
            if let Err(why) = self.start_dma(false) {
                serial_debug(&format!("[hdmiaud] restart failed: {}\n", why));
            }
        }

        pub fn recover_stream(&mut self) {
            self.restarts += 1;
            if self.restarts <= 8 {
                serial_debug(&format!("[hdmiaud] stream stalled - restarting (mai_ctl=0x{:08x} crp=0x{:08x})\n",
                    if self.hd != 0 { rd(self.hd, HD_MAI_CTL) } else { 0 },
                    if self.hdmi != 0 { rd(self.hdmi, HDMI_CRP_CFG) } else { 0 }));
            }
            let (f, c) = (self.in_rate, self.in_channels);
            self.reconfigure_stream(0, f, c);
        }

        /// Position update, MAI error / firmware re-train check, silence refill, underrun resync.
        fn update(&mut self) {
            if !self.stream_active { return; }
            let Some(ch) = self.chan.as_ref() else { return };
            ch.poll();
            let pos = ch.position();
            let ctl = rd(self.hd, HD_MAI_CTL);
            if ctl & MAI_CTL_ERRS != 0 {
                self.mai_errs += 1;
                wr(self.hd, HD_MAI_CTL, mai_ctl_run(2) | (ctl & MAI_CTL_ERRS));
            }
            // A hotplug re-train by the firmware drops our setup: MAI disabled or N changed.
            let crp = rd(self.hdmi, HDMI_CRP_CFG);
            if self.retrains < 20 && (ctl & MAI_CTL_ENABLE == 0 || crp & 0x010f_ffff != crp_cfg(self.n)) {
                self.retrains += 1;
                if self.retrains <= 4 {
                    serial_debug(&format!("[hdmiaud] setup lost (mai_ctl=0x{:08x} crp=0x{:08x}) - reprogramming\n", ctl, crp));
                }
                self.recover_stream();
                return;
            }
            let Some(pos) = pos else { return };
            let before = self.r.played;
            let adv = self.r.advance(pos);
            for (a, b) in adv.refill { self.fill_silence(a, b); }
            if adv.starved && (self.r.underruns <= 8 || self.r.underruns % 100 == 0) {
                serial_debug(&format!("[hdmiaud] writer starved #{} (end of every playback counts once)\n", self.r.underruns));
            }
            if self.r.played == before {
                self.stall += 1;
                if self.stall == STALL_TICKS {
                    self.stalls += 1;
                    self.stall = 0;
                    self.recover_stream();
                }
            } else {
                self.stall = 0;
            }
        }

        pub fn send_pcm_data(&mut self, data: &[u8]) -> usize {
            if !self.initialized || !self.stream_active { return 0; }
            self.update();
            if !self.stream_active { return 0; }
            let out_frames = (self.r.space() as usize) / FRAME_BYTES;
            let start = self.r.write_off();
            let mut off = start;
            let ring = self.ring as *mut u32;
            let cs = self.cs;
            let same = self.hw_rate == self.in_rate;
            let (used, produced) = convert_s16(data, self.in_channels as usize, same, &mut self.resamp, out_frames, |l, r| {
                let [a, b] = pack_frame(l, r, off, &cs);
                unsafe {
                    core::ptr::write_volatile(ring.add(off / 4), a);
                    core::ptr::write_volatile(ring.add(off / 4 + 1), b);
                }
                off = (off + FRAME_BYTES) % RING_BYTES;
            });
            if produced == 0 { return used; }
            let n = produced * FRAME_BYTES;
            let first = n.min(RING_BYTES - start);
            dma::dcache_clean_inval(self.ring + start, first);
            if n > first { dma::dcache_clean_inval(self.ring, n - first); }
            self.r.written += n as u64;
            self.r.was_ahead = true;
            used
        }

        /// Queued audio in S16-stereo-equivalent bytes (the ring holds 8-byte IEC958 frames).
        pub fn queued_bytes(&self) -> u64 { self.r.queued() / 2 }

        pub fn tx_used_idx(&self) -> u16 { (self.r.played / 2 / crate::snd::TX_BUF_BYTES as u64) as u16 }

        pub fn top_up_silence(&mut self) { self.update(); }

        pub fn info(&mut self) -> String {
            self.update();
            let mut s = String::from("backend: hdmi\n");
            s.push_str(&self.summary);
            s.push_str(&format!(
                "stream: {} Hz x{} -> {} Hz IEC958 S16, queued {} B, played {} B\n\
                 underruns {} mai_errors {} retrains {} restarts {} stalls {}\n",
                self.in_rate, self.in_channels, self.hw_rate, self.r.queued(), self.r.played,
                self.r.underruns, self.mai_errs, self.retrains, self.restarts, self.stalls));
            s
        }
    }
}
