//! HDMI audio output on the Raspberry Pi 5 (BCM2712 D0, hdmi0), a PipeWire backend with the same
//! seven calls as `hda::Hda`. The firmware owns the video mode; we add only the audio side, as
//! Linux `vc4_hdmi` does after a mode set: open the DVP audio clock gate, program MAI (sample
//! clock, format, D0 threshold layout), the audio packet / channel map, ACR N/CTS from the pixel
//! clock the boot probe measured, and the audio InfoFrame (packet RAM slot 4, read-modify-write so
//! the firmware's AVI InfoFrame survives). PCM is packed into IEC958 subframes by the CPU and a
//! dma40 cyclic ring (channel 10; DREQ 12 on D0, 10 on C0) feeds MAI_DATA.
//!
//! Inert unless `hdmi_probe` saw a sink on hdmi0 in HDMI (not DVI) mode with a known pixel clock.
//! `hdmi.audio=0` disables it. Log tag `[hdmiaud]`.

pub mod pure;

#[cfg(all(target_arch = "aarch64", feature = "rpi5", feature = "hdmi-audio"))]
pub use hw::*;

#[cfg(not(all(target_arch = "aarch64", feature = "rpi5", feature = "hdmi-audio")))]
pub use stub::*;

pub type SerrorCheck = fn() -> Option<u64>;

#[cfg(not(all(target_arch = "aarch64", feature = "rpi5", feature = "hdmi-audio")))]
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

#[cfg(all(target_arch = "aarch64", feature = "rpi5", feature = "hdmi-audio"))]
mod hw {
    use super::pure::*;
    use crate::kenv::hdmi as env;
    use env::{dma, buddy, phys_to_virt, serial_debug};
    use alloc::format;
    use alloc::string::String;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use leandros_lib::mmio::{MmioRd, MmioWr};

    const DVP_LEN: usize = 0x10;
    const HD_LEN: usize = 0x100;
    const HDMI_LEN: usize = 0x300;
    const PKT_LEN: usize = 0x400;
    const DVP_PHYS: u64 = 0x10_7c70_0000;
    const HD_PHYS: u64 = 0x10_7c72_0000;
    const HDMI0_PHYS: u64 = 0x10_7c70_1400;
    const PACKET0_PHYS: u64 = 0x10_7c70_3800;
    /// MAI_DATA for hdmi0 as a CPU physical address (dma-ranges are the identity). This is the
    /// only FIFO address ever given to the DMA engine: a 32-bit "bus" alias would be ordinary
    /// DRAM on boards with 2 GiB or more.
    const FIFO_CPU: u64 = HD_PHYS + HD_MAI_DATA as u64;
    const DMA_CHANNEL: u8 = 10;
    const RING_ORDER: usize = 5; // 32 pages >= RING_BYTES
    /// Longest the audio InfoFrame rewrite may wait for the packet slot to go idle when a
    /// recovery runs inside the timer IRQ (STATE held); probe and client reconfigure wait 100 ms.
    const IRQ_INFOFRAME_SPIN_NS: u64 = 1_000_000;
    const TASK_INFOFRAME_SPIN_NS: u64 = 100_000_000;

    static SERROR: AtomicUsize = AtomicUsize::new(0);

    pub fn set_serror_check(f: super::SerrorCheck) {
        SERROR.store(f as usize, Ordering::Release);
    }

    fn serror() -> Option<u64> {
        let f = SERROR.load(Ordering::Acquire);
        if f == 0 { return env::serror(); }
        let f: super::SerrorCheck = unsafe { core::mem::transmute(f) };
        f()
    }

    fn disabled(cmdline: &str) -> bool {
        cmdline.split_whitespace().any(|t| t == "hdmi.audio=0")
    }

    pub fn may_appear(cmdline: &str) -> bool {
        !disabled(cmdline) && env::sink().state == env::PROBE_NOT_RUN
    }

    #[inline]
    fn rd(base: usize, off: usize) -> u32 { unsafe { ((base + off) as *const u32).mmio_read() } }
    #[inline]
    fn wr(base: usize, off: usize, v: u32) { unsafe { ((base + off) as *mut u32).mmio_write(v) } }

    /// Fixed stack buffer behind `logf!`: the timer-IRQ paths log without touching the heap
    /// (the IRQ may have interrupted the allocator).
    struct Line { buf: [u8; 224], len: usize }
    impl core::fmt::Write for Line {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }
    fn logf(args: core::fmt::Arguments) {
        use core::fmt::Write;
        let mut l = Line { buf: [0; 224], len: 0 };
        let _ = l.write_fmt(args);
        serial_debug(core::str::from_utf8(&l.buf[..l.len]).unwrap_or("[hdmiaud] (log line cut)\n"));
    }
    macro_rules! logf { ($($a:tt)*) => { logf(format_args!($($a)*)) } }

    fn spin_us(us: u64) {
        let end = env::monotonic_ns() + us * 1000;
        while env::monotonic_ns() < end { core::hint::spin_loop(); }
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
        d0_unknown: bool,
        dreq_cell: u32,
        policy: StallPolicy,
        infoframe: InfoframeRetry,
        null: NullSink,
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
                chan: None, fifo: 0, d0: true, d0_unknown: false, dreq_cell: 0, policy: StallPolicy::new(), infoframe: InfoframeRetry::new(), null: NullSink::new(), pix_khz: 0, n: 0, cts: 0, in_rate: 48_000, in_channels: 2,
                hw_rate: 48_000, resamp: Resampler::new(48_000, 48_000), cs: [0; 24],
                silence: [0; FRAMES_PER_BLOCK * 2], r: Ring::default(), mai_errs: 0, retrains: 0,
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
            let sk = env::sink();
            match sk.state {
                env::PROBE_DONE => {}
                env::PROBE_NOT_RUN => return Err("hdmi probe has not run yet"),
                env::PROBE_SKIPPED => return Err("hdmi probe skipped (hdmi.probe=0)"),
                _ => return Err("hdmi probe failed (nomap/serror)"),
            }
            let f = sk.flags;
            if f & env::PORT_VALID == 0 { return Err("no hdmi0 probe data"); }
            if f & (env::PORT_HPD | env::PORT_ACTIVE) == 0 { return Err("no sink on hdmi0 (hpd=0 active=0)"); }
            if f & env::PORT_HDMI_MODE == 0 { return Err("hdmi0 runs DVI (no audio; try hdmi_drive=2)"); }
            let sink = sk.sink;
            self.pix_khz = sk.pix_khz;
            if self.pix_khz == 0 { return Err("hdmi0 pixel clock unknown"); }
            self.d0 = sk.d0;
            self.d0_unknown = sk.d0_unknown;
            let map = |phys: u64, len: usize| env::ioremap_uc(phys, len);
            self.dvp = map(DVP_PHYS, DVP_LEN);
            self.hd = map(HD_PHYS, HD_LEN);
            self.hdmi = map(HDMI0_PHYS, HDMI_LEN);
            self.pkt = map(PACKET0_PHYS, PKT_LEN);
            if self.dvp == 0 || self.hd == 0 || self.hdmi == 0 || self.pkt == 0 { return Err("ioremap failed"); }
            // Raw vertical timing registers for the pixel-clock follow-up (read-only, before any write).
            serial_debug(&format!("[hdmiaud] timing regs VERTA0=0x{:08x} VERTB0=0x{:08x} VERTA1=0x{:08x} VERTB1=0x{:08x}\n",
                rd(self.hdmi, HDMI_VERTA0), rd(self.hdmi, HDMI_VERTB0), rd(self.hdmi, HDMI_VERTA1), rd(self.hdmi, HDMI_VERTB1)));
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
            self.program(48_000, TASK_INFOFRAME_SPIN_NS)?;
            self.fifo = FIFO_CPU;
            // DREQ: hdmi.dreq=N wins; otherwise the cell of this stepping. Only when the stepping is
            // unknown and the pacing check fails is the other stepping's cell tried, once (never
            // another FIFO address).
            let forced = dreq_knob(cmdline);
            let first = env::cb::hdmi_dreq_cell(self.d0, 0);
            self.dreq_cell = match forced { Some(n) => env::cb::cell_with_per_map(first, n), None => first };
            if let Err(why) = self.start_dma(true) {
                // One full retry with the same cell (stop, reprogram MAI/ACR, restart): a first
                // verify can lose to the start-up transient without the cell being wrong.
                serial_debug(&format!("[hdmiaud] first pacing check failed ({}): retrying once with DREQ cell 0x{:08x}\n", why, self.dreq_cell));
                let mut res = self.retry_dma();
                // Only when the stepping is unknown is the other stepping's cell tried, once
                // (never another FIFO address).
                if res.is_err() && forced.is_none() && self.d0_unknown {
                    self.dreq_cell = env::cb::hdmi_dreq_cell(!self.d0, 0);
                    serial_debug(&format!("[hdmiaud] stepping unknown: retrying with DREQ cell 0x{:08x}\n", self.dreq_cell));
                    res = self.retry_dma();
                }
                res?;
            }
            let ch_id = self.chan.as_ref().map_or(0, |c| c.id());
            self.summary = format!(
                "hdmi0 pix={} kHz N={} CTS={} gate={} thr={} dma ch{} fifo=0x{:x} dreq=0x{:08x}({}) {} edid={}\n",
                self.pix_khz, self.n, self.cts, gate_was, if self.d0 { "d0" } else { "c" }, ch_id, self.fifo,
                self.dreq_cell, self.dreq_cell & 31,
                if dma::polled() { "poll" } else { "irq" },
                match sink { env::SINK_HDMI => "hdmi", env::SINK_DVI => "dvi(forced hdmi mode)", _ => "unknown" }
            );
            Ok(())
        }

        /// Stops the engine, reprograms MAI/ACR and starts the ring again with verification.
        fn retry_dma(&mut self) -> Result<(), &'static str> {
            self.stream_active = false;
            if let Some(c) = self.chan.as_mut() { c.stop(); }
            self.program(48_000, TASK_INFOFRAME_SPIN_NS)?;
            self.start_dma(true)
        }

        /// Programs MAI, packetizer, ACR and the InfoFrame for `fs` (Linux startup + prepare).
        fn program(&mut self, fs: u32, infoframe_spin_ns: u64) -> Result<(), &'static str> {
            let fmt = mai_fmt(fs).ok_or("unsupported rate")?;
            let (mut n, mut cts) = env::acr_n_cts(self.pix_khz, fs);
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
            self.write_infoframe(infoframe_spin_ns);
            if let Some(syn) = serror() {
                logf!("[hdmiaud] serror programming MAI/CRP syn=0x{:016x}\n", syn);
                return Err("serror while programming MAI/CRP");
            }
            Ok(())
        }

        /// `vc4_hdmi_write_infoframe` for the audio InfoFrame: stop slot 4, wait idle (at most
        /// `spin_ns`), write the slot, re-enable it; the other slot bits (AVI = 2) are preserved.
        /// If the slot is still busy after `spin_ns` (the IRQ-context recovery allows ~1 ms) the
        /// slot is re-enabled with its previous contents: the InfoFrame does not change on a
        /// restart, so nothing is lost and no long spin happens with STATE held.
        fn write_infoframe(&mut self, spin_ns: u64) {
            let h = self.hdmi;
            let bit = 1u32 << AUDIO_PACKET_ID;
            let cfg = rd(h, HDMI_RAM_PACKET_CONFIG);
            wr(h, HDMI_RAM_PACKET_CONFIG, cfg & !bit);
            let end = env::monotonic_ns() + spin_ns;
            while rd(h, HDMI_RAM_PACKET_STATUS) & bit != 0 && env::monotonic_ns() < end { core::hint::spin_loop(); }
            if rd(h, HDMI_RAM_PACKET_STATUS) & bit != 0 {
                wr(h, HDMI_RAM_PACKET_CONFIG, cfg | bit | RAM_PACKET_ENABLE);
                // Retried on later ticks without waiting; logged once.
                if self.infoframe.defer() {
                    logf!("[hdmiaud] audio InfoFrame slot busy: rewrite deferred to later ticks\n");
                }
                return;
            }
            self.infoframe.done();
            self.store_infoframe();
        }

        fn store_infoframe(&mut self) {
            let h = self.hdmi;
            let bit = 1u32 << AUDIO_PACKET_ID;
            let words = infoframe_words(&audio_infoframe(2));
            let base = self.pkt + AUDIO_PACKET_ID as usize * PACKET_STRIDE;
            for (i, w) in words.iter().enumerate() { wr(base, i * 4, *w); }
            let cfg = rd(h, HDMI_RAM_PACKET_CONFIG);
            wr(h, HDMI_RAM_PACKET_CONFIG, cfg | bit | RAM_PACKET_ENABLE);
        }

        /// One non-blocking step of a deferred InfoFrame rewrite (timer tick, STATE held).
        fn infoframe_tick(&mut self) {
            if !self.infoframe.is_pending() || self.hdmi == 0 { return; }
            let h = self.hdmi;
            let bit = 1u32 << AUDIO_PACKET_ID;
            match self.infoframe.tick(rd(h, HDMI_RAM_PACKET_STATUS) & bit != 0) {
                InfoframeStep::Nothing => {}
                InfoframeStep::Disable => { let cfg = rd(h, HDMI_RAM_PACKET_CONFIG); wr(h, HDMI_RAM_PACKET_CONFIG, cfg & !bit); }
                InfoframeStep::Write => self.store_infoframe(),
                InfoframeStep::GiveUp => {
                    let cfg = rd(h, HDMI_RAM_PACKET_CONFIG);
                    wr(h, HDMI_RAM_PACKET_CONFIG, cfg | bit | RAM_PACKET_ENABLE);
                    logf!("[hdmiaud] audio InfoFrame slot stayed busy: keeping the previous frame\n");
                }
            }
        }

        fn fill_silence(&mut self, a: usize, b: usize) {
            if b <= a { return; }
            let ring = self.ring as *mut u32;
            for off in (a..b).step_by(4) {
                let w = (off % BLOCK_BYTES) / 4;
                unsafe { core::ptr::write_volatile(ring.add(off / 4), self.silence[w]); }
            }
            env::dcache_clean_inval(self.ring + a, b - a);
        }

        /// Fills the ring with silence and starts the cyclic DMA into MAI_DATA. With `verify`
        /// (probe only, task context), proves the FIFO paces the engine: the position is sampled
        /// every 500 us from the start and the deltas accumulate unwrapped; after a 5 ms skip and
        /// a 30 ms window the verdict is taken once: more than 2 KiB at 50..150 % of the hardware
        /// rate, a valid position throughout, and at most one completed period.
        fn start_dma(&mut self, verify: bool) -> Result<(), &'static str> {
            self.fill_silence(0, RING_BYTES);
            self.r.reset();
            let cell = self.dreq_cell;
            let hw = self.hw_rate;
            let Some(ch) = self.chan.as_mut() else { return Err("no channel") };
            ch.start_cyclic(self.ring_phys as u64, RING_BYTES, PERIOD_BYTES, self.fifo, cell, true)
                .map_err(|_| "start_cyclic refused")?;
            if let Some(syn) = serror() {
                logf!("[hdmiaud] serror starting dma syn=0x{:016x}\n", syn);
                ch.stop();
                return Err("serror at dma start");
            }
            // Clear the underflow flags the empty FIFO raised before the first word.
            wr(self.hd, HD_MAI_CTL, mai_ctl_run(2) | MAI_CTL_ERRS);
            self.stream_active = true;
            if !verify { return Ok(()); }
            let t0 = env::monotonic_ns();
            let mut meter = RateMeter::new();
            let mut marked_at = None;
            let mut last;
            let (el_us, first) = loop {
                ch.poll();
                last = ch.position();
                let el_us = (env::monotonic_ns() - t0) / 1000;
                meter.sample(last, ch.periods());
                if marked_at.is_none() && el_us >= VERIFY_SKIP_MS * 1000 { meter.mark(); marked_at = Some(el_us); }
                if let Some(m) = marked_at {
                    if el_us - m >= VERIFY_MEASURE_MS * 1000 { break (el_us - m, meter.moved()); }
                }
                spin_us(VERIFY_SAMPLE_US);
            };
            if meter.verdict(el_us, hw) { return Ok(()); }
            let (cs, dbg) = ch.status();
            let ti = env::cb::DreqCfg::from_cell(cell).ti;
            let src = last.map_or(0, |p| self.ring_phys as u64 + p as u64);
            serial_debug(&format!(
                "[hdmiaud] dma not paced by the FIFO: moved {} B (periods {}) pos {:?} in {} us, src=0x{:x} cs=0x{:08x} debug=0x{:08x} ti=0x{:08x} (programmed) mai_ctl=0x{:08x} dreq=0x{:08x}\n",
                first, meter.periods_since_mark(), last, el_us, src, cs, dbg, ti, rd(self.hd, HD_MAI_CTL), cell));
            ch.stop();
            self.stream_active = false;
            Err("dma not paced by the FIFO (wrong DREQ?)")
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
            for (va, len) in [(self.dvp, DVP_LEN), (self.hd, HD_LEN), (self.hdmi, HDMI_LEN), (self.pkt, PKT_LEN)] { if va != 0 { env::iounmap(va, len); } }
            self.dvp = 0; self.hd = 0; self.hdmi = 0; self.pkt = 0;
            self.initialized = false;
        }

        /// Client SET_PARAMS: new input format and a restart. The stall policy starts over; out of
        /// dead it earns only this one restart (a second stall is dead again) until real progress.
        pub fn reconfigure_stream(&mut self, _id: u32, freq: u32, channels: u8) {
            if !self.initialized { return; }
            self.policy.rearm_client();
            self.in_rate = if freq == 0 { 48_000 } else { freq };
            self.in_channels = channels.max(1);
            match self.restart_hw(TASK_INFOFRAME_SPIN_NS) {
                Ok(()) => logf!("[hdmiaud] SET_PARAMS {} Hz x{} -> hw {} Hz N={} CTS={}\n",
                    self.in_rate, self.in_channels, self.hw_rate, self.n, self.cts),
                Err(why) => {
                    logf!("[hdmiaud] reprogram failed: {}\n", why);
                    self.go_dead(why);
                }
            }
        }

        /// Stops the engine, reprograms MAI/ACR for the current input format and restarts the ring.
        fn restart_hw(&mut self, infoframe_spin_ns: u64) -> Result<(), &'static str> {
            self.stream_active = false;
            if let Some(c) = self.chan.as_mut() { c.stop(); }
            let hw = hw_rate_for(self.in_rate);
            self.resamp = Resampler::new(self.in_rate, hw);
            self.program(hw, infoframe_spin_ns)?;
            self.null = NullSink::new();
            self.start_dma(false)
        }

        /// Gives up on the hardware: DMA and MAI stopped, input drained through the null sink at
        /// the stream's own rate so the writer never blocks. One line.
        fn go_dead(&mut self, why: &str) {
            self.policy.kill();
            let (cs, dbg) = self.chan.as_ref().map_or((0, 0), |c| c.status());
            let pos = self.chan.as_ref().and_then(|c| c.position());
            logf!("[hdmiaud] dead: {} (pos={:?} cs=0x{:08x} debug=0x{:08x} mai_ctl=0x{:08x}); audio is dropped until the next SET_PARAMS or re-train\n",
                why, pos, cs, dbg, if self.hd != 0 { rd(self.hd, HD_MAI_CTL) } else { 0 });
            self.stream_active = false;
            if let Some(c) = self.chan.as_mut() { c.stop(); }
            if self.hd != 0 { wr(self.hd, HD_MAI_CTL, mai_ctl_run(2) & !MAI_CTL_ENABLE); }
            self.null = NullSink::new();
            self.null.start(env::monotonic_ns());
        }

        fn act(&mut self, a: StallAction) {
            match a {
                StallAction::None => {}
                StallAction::Restart(n) => {
                    self.stalls += 1;
                    self.restarts += 1;
                    if self.restarts <= 8 {
                        logf!("[hdmiaud] stream stalled - restart #{} (mai_ctl=0x{:08x} crp=0x{:08x})\n",
                            n, rd(self.hd, HD_MAI_CTL), rd(self.hdmi, HDMI_CRP_CFG));
                    }
                    // Runs in the timer IRQ with STATE held: no long spin.
                    if let Err(why) = self.restart_hw(IRQ_INFOFRAME_SPIN_NS) { self.go_dead(why); }
                }
                StallAction::Dead => {
                    let why = if self.policy.restarts() == 0 { "stalled again after the SET_PARAMS restart" } else { "no progress after 5 restarts" };
                    self.go_dead(why);
                }
            }
        }

        /// A stall reported from outside (PipeWire's 250 ms no-TX-progress detector). It goes
        /// through the same policy as the internal detector.
        pub fn recover_stream(&mut self) {
            if !self.initialized { return; }
            let a = self.policy.request(env::monotonic_ns());
            self.act(a);
        }

        /// The programmed audio setup still reads back (see `pure::setup_intact`).
        fn setup_intact(&self) -> bool {
            let h = self.hdmi;
            setup_intact(rd(h, HDMI_CRP_CFG), rd(h, HDMI_CTS_0), rd(h, HDMI_AUDIO_PACKET_CONFIG), rd(h, HDMI_MAI_CONFIG),
                self.n, self.cts, 0b11)
        }

        /// Position update, MAI error / firmware re-train check, silence refill, underrun resync.
        fn update(&mut self) {
            self.infoframe_tick();
            if self.policy.is_dead() {
                // Dead: only a firmware re-train (the audio setup lost behind our back) re-arms.
                if self.retrains < 20 && self.hdmi != 0 && !self.setup_intact() {
                    self.retrains += 1;
                    serial_debug("[hdmiaud] re-train while dead - re-arming\n");
                    self.policy.rearm();
                    if let Err(why) = self.restart_hw(IRQ_INFOFRAME_SPIN_NS) { self.go_dead(why); }
                }
                return;
            }
            if !self.stream_active { return; }
            let Some(ch) = self.chan.as_ref() else { return };
            ch.poll();
            let pos = ch.position();
            let ctl = rd(self.hd, HD_MAI_CTL);
            if ctl & MAI_CTL_ERRS != 0 {
                self.mai_errs += 1;
                wr(self.hd, HD_MAI_CTL, mai_ctl_run(2) | (ctl & MAI_CTL_ERRS));
            }
            // A hotplug re-train by the firmware drops our setup: MAI disabled, N changed, or
            // the CTS / audio packet / MAI config gone with N untouched. That is not a stall:
            // reprogram directly and start the policy over.
            let crp = rd(self.hdmi, HDMI_CRP_CFG);
            if self.retrains < 20 && (ctl & MAI_CTL_ENABLE == 0 || !self.setup_intact()) {
                self.retrains += 1;
                if self.retrains <= 4 {
                    logf!("[hdmiaud] setup lost (mai_ctl=0x{:08x} crp=0x{:08x}) - reprogramming\n", ctl, crp);
                }
                self.policy.rearm();
                if let Err(why) = self.restart_hw(IRQ_INFOFRAME_SPIN_NS) { self.go_dead(why); }
                return;
            }
            // No position yet counts as nothing moved: the stall clock must still run.
            let (adv, moved) = self.r.update(pos);
            for (a, b) in adv.refill { self.fill_silence(a, b); }
            if adv.starved && (self.r.underruns <= 8 || self.r.underruns % 100 == 0) {
                logf!("[hdmiaud] writer starved #{} (end of every playback counts once)\n", self.r.underruns);
            }
            let a = self.policy.observe(env::monotonic_ns(), moved);
            self.act(a);
        }

        pub fn send_pcm_data(&mut self, data: &[u8]) -> usize {
            if !self.initialized { return 0; }
            if self.policy.is_dead() {
                self.update();
                if self.policy.is_dead() {
                    // Null sink: whole input frames at the stream's own rate.
                    let fb = self.in_channels as usize * 2;
                    let frames = self.null.drain(env::monotonic_ns(), data.len() / fb, self.in_rate as u64);
                    return frames * fb;
                }
            }
            if !self.stream_active { return 0; }
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
            env::dcache_clean_inval(self.ring + start, first);
            if n > first { env::dcache_clean_inval(self.ring, n - first); }
            self.r.written += n as u64;
            self.r.was_ahead = true;
            used
        }

        /// Queued audio in S16-stereo-equivalent bytes (the ring holds 8-byte IEC958 frames).
        pub fn queued_bytes(&self) -> u64 { if self.policy.is_dead() { 0 } else { self.r.queued() / 2 } }

        pub fn tx_used_idx(&self) -> u16 {
            if self.policy.is_dead() {
                return (self.null.consumed * (self.in_channels as u64 * 2) / env::TX_BUF_BYTES as u64) as u16;
            }
            (self.r.played / 2 / env::TX_BUF_BYTES as u64) as u16
        }

        pub fn top_up_silence(&mut self) { self.update(); }

        pub fn info(&mut self) -> String {
            self.update();
            let mut s = String::from("backend: hdmi\n");
            s.push_str(&self.summary);
            s.push_str(&format!(
                "stream: {} Hz x{} -> {} Hz IEC958 S16, queued {} B, played {} B\n\
                 underruns {} mai_errors {} retrains {} restarts {} stalls {} dead {}\n",
                self.in_rate, self.in_channels, self.hw_rate, self.r.queued(), self.r.played,
                self.r.underruns, self.mai_errs, self.retrains, self.restarts, self.stalls, self.policy.is_dead()));
            s
        }
    }
}
