//! Read-only boot probe of the BCM2712 HDMI0/1 audio path (Raspberry Pi 5, rpi5 only).
//!
//! The firmware owns the display mode; this probe never writes a display register.
//! It answers, per port, the three questions HDMI audio depends on
//! (artifacts/notes/lane-pisound-design-2026-10-26.md, section 1):
//! - is the link in HDMI mode, not DVI (SCHEDULER_CONTROL, plus the EDID's HDMI VSDB);
//! - what is the pixel clock (htotal x vtotal from HORZA/B + VERTA0/B0, times the frame
//!   rate measured on the `hd` FRAME_COUNT, snapped to the standard clocks);
//! - is the DVP 108 MHz audio clock gate open, and what state are the MAI and the
//!   packetizer (MAI_CTL, CRP_CFG, AUDIO_PACKET_CONFIG, RAM_PACKET_CONFIG) in.
//!
//! Output (serial): `[hdmi] dvp ...`, one `[hdmi] hdmiN ...` line per port,
//! `[hdmi] mbox ...`, `[dma40] ...`. Inert on failure: every block read is followed
//! by `serror_checkpoint()`, a bus error is logged and the port skipped.
//! `hdmi.probe=0` on the command line disables it.

pub mod decode;

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::probe_boot;

use core::sync::atomic::{AtomicU32, Ordering};

pub const PROBE_NOT_RUN: u32 = 0;
pub const PROBE_SKIPPED: u32 = 1;
pub const PROBE_DONE: u32 = 2;
pub const PROBE_NOMAP: u32 = 3;
pub const PROBE_SERROR: u32 = 4;
/// Outcome of the boot probe (one of the `PROBE_*` values).
pub static PROBE_STATE: AtomicU32 = AtomicU32::new(PROBE_NOT_RUN);

pub fn probe_enabled(cmdline: &str) -> bool {
    !cmdline.split_ascii_whitespace().any(|t| t == "hdmi.probe=0")
}

pub fn state() -> u32 {
    PROBE_STATE.load(Ordering::Relaxed)
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::decode::*;
    use super::*;
    use crate::pci::serial_debug;
    use alloc::format;
    use alloc::string::String;
    use leandros_lib::mmio::MmioRd;
    use mm::kva::{ioremap, iounmap, CacheMode};

    /// SoC peripheral addresses (`soc@107c000000`, CPU = 0x10_0000_0000 + DT address).
    const DVP_PHYS: u64 = 0x10_7c70_0000;
    const HD_PHYS: u64 = 0x10_7c72_0000;
    const HDMI_PHYS: [u64; 2] = [0x10_7c70_1400, 0x10_7c70_6400];
    /// `dma40: dma@10600` under `/axi` (identity-mapped): 0x600 = six 0x100 channel slots.
    const DMA40_PHYS: u64 = 0x10_0001_0600;
    const DMA40_LEN: usize = 0x600;

    const TAG_GET_CLOCK_RATE: u32 = 0x0003_0002;
    const TAG_GET_EDID_BLOCK: u32 = 0x0003_0020;
    const TAG_GET_NUM_DISPLAYS: u32 = 0x0004_0013;

    pub type SerrorCheck = fn() -> Option<u64>;

    #[inline]
    fn rd(base: usize, off: usize) -> u32 {
        unsafe { ((base + off) as *const u32).mmio_read() }
    }

    struct Ports {
        dvp: usize,
        hd: usize,
        hdmi: [usize; 2],
        dma: usize,
    }

    impl Ports {
        fn unmap(&self) {
            for va in [self.dvp, self.hd, self.hdmi[0], self.hdmi[1], self.dma] {
                if va != 0 {
                    iounmap(va);
                }
            }
        }
    }

    pub fn probe_boot(cmdline: &str, hhdm_offset: u64, serror_check: SerrorCheck) {
        if !probe_enabled(cmdline) {
            serial_debug("[hdmi] skipped (hdmi.probe=0)\n");
            PROBE_STATE.store(PROBE_SKIPPED, Ordering::Relaxed);
            return;
        }
        let map = |phys: u64, len: usize| ioremap(phys, len, CacheMode::Uncached).unwrap_or(0);
        let p = Ports {
            dvp: map(DVP_PHYS, DVP_BLOCK_LEN),
            hd: map(HD_PHYS, HD_BLOCK_LEN),
            hdmi: [map(HDMI_PHYS[0], HDMI_BLOCK_LEN), map(HDMI_PHYS[1], HDMI_BLOCK_LEN)],
            dma: map(DMA40_PHYS, DMA40_LEN),
        };
        let mbox = map(crate::rpi_mailbox::MBOX_BASE_PHYS as u64, 0x40);
        if p.dvp == 0 || p.hd == 0 || p.hdmi[0] == 0 || p.hdmi[1] == 0 || mbox == 0 {
            serial_debug("[hdmi] fail map ioremap\n");
            p.unmap();
            PROBE_STATE.store(PROBE_NOMAP, Ordering::Relaxed);
            return;
        }
        // The HHDM does not cover the mailbox registers this late in boot; the
        // mapping is kept (later mailbox users go through it too).
        crate::rpi_mailbox::set_mbox_virt(mbox);
        let mut serror = false;

        // DVP: reset lines and the 108 MHz audio clock gates (read before `hd`).
        let (sw_init, misc) = (rd(p.dvp, DVP_SW_INIT), rd(p.dvp, DVP_MISC_CONFIG));
        if let Some(syn) = serror_check() {
            serial_debug(&format!("[hdmi] dvp serror syn=0x{:016x}; hdmi probe stopped\n", syn));
            serror = true;
        } else {
            serial_debug(&format!("[hdmi] dvp sw_init=0x{:08x} misc=0x{:08x}\n", sw_init, misc));
        }

        let mut pix = [0u32; 2];
        if !serror {
            for port in 0..2 {
                match probe_port(&p, port, sw_init, misc, serror_check) {
                    Ok((line, khz)) => {
                        serial_debug(&line);
                        pix[port] = khz;
                    }
                    Err(syn) => {
                        serial_debug(&format!("[hdmi] hdmi{} serror syn=0x{:016x} (port skipped)\n", port, syn));
                        serror = true;
                    }
                }
            }
        }

        // Firmware's view: clocks 9 (PIXEL), 13 (M2MC = HDMI hsm), 14 (PIXEL_BVB),
        // the EDID it read, and whether the fkms display tags answer (read-only tags only).
        let mut env = Mbox { hhdm: hhdm_offset as usize };
        let clk = |env: &mut Mbox, id: u32| env.tag(TAG_GET_CLOCK_RATE, &[id, 0], 2).map(|v| v[1]);
        let fmt_clk = |v: Option<u32>| v.map_or(String::from("refused"), |hz| format!("{}", hz / 1000));
        let (c9, c13, c14) = (clk(&mut env, 9), clk(&mut env, 13), clk(&mut env, 14));
        let edid = env.edid_summary();
        let fkms = env.tag(TAG_GET_NUM_DISPLAYS, &[0], 1).map_or(String::from("refused"), |v| format!("{}", v[0]));
        serial_debug(&format!(
            "[hdmi] mbox clk9={} clk13={} clk14={} kHz edid={} fkms={} (measured hdmi0={} hdmi1={} kHz)\n",
            fmt_clk(c9), fmt_clk(c13), fmt_clk(c14), edid, fkms, pix[0], pix[1]
        ));

        // dma40 channel slots (ARM mask 0x7c0 = ch6..10): CS only, all expected idle.
        if !serror && p.dma != 0 {
            let cs: [u32; 6] = core::array::from_fn(|i| rd(p.dma, i * 0x100));
            if let Some(syn) = serror_check() {
                serial_debug(&format!("[dma40] serror syn=0x{:016x}\n", syn));
                serror = true;
            } else {
                let active = cs.iter().filter(|&&c| c & 1 != 0).count();
                serial_debug(&format!(
                    "[dma40] slots@0x{:x} cs={:08x},{:08x},{:08x},{:08x},{:08x},{:08x} active={}\n",
                    DMA40_PHYS, cs[0], cs[1], cs[2], cs[3], cs[4], cs[5], active
                ));
            }
        }

        p.unmap();
        PROBE_STATE.store(if serror { PROBE_SERROR } else { PROBE_DONE }, Ordering::Relaxed);
    }

    /// One port: a single read pass over its `hdmi` and `hd` registers, an SError
    /// checkpoint, then the frame-rate measurement. Returns the line and the clock.
    fn probe_port(p: &Ports, port: usize, sw_init: u32, misc: u32, serror_check: SerrorCheck) -> Result<(String, u32), u64> {
        let h = p.hdmi[port];
        let (mai_ctl_off, mai_fmt_off, mai_smp_off, fc_off) = HD_PORT_REGS[port];
        let hotplug = rd(h, HDMI_HOTPLUG);
        let sched = rd(h, HDMI_SCHEDULER_CONTROL);
        let (horza, horzb, verta0, vertb0) = (rd(h, HDMI_HORZA), rd(h, HDMI_HORZB), rd(h, HDMI_VERTA0), rd(h, HDMI_VERTB0));
        let crp = rd(h, HDMI_CRP_CFG);
        let cts = rd(h, HDMI_CTS_0);
        let apc = rd(h, HDMI_AUDIO_PACKET_CONFIG);
        let ram_cfg = rd(h, HDMI_RAM_PACKET_CONFIG);
        let ram_st = rd(h, HDMI_RAM_PACKET_STATUS);
        let mai_cfg = rd(h, HDMI_MAI_CONFIG);
        let fifo = rd(h, HDMI_FIFO_CTL);
        let mai_ctl = rd(p.hd, mai_ctl_off);
        let mai_fmt = rd(p.hd, mai_fmt_off);
        let mai_smp = rd(p.hd, mai_smp_off);
        let fc0 = rd(p.hd, fc_off);
        if let Some(syn) = serror_check() {
            return Err(syn);
        }

        let t = decode_timing(horza, horzb, verta0, vertb0);
        let fps = measure_fps(p.hd + fc_off, fc0);
        if let Some(syn) = serror_check() {
            return Err(syn);
        }
        let raw = pixclk_khz_raw(&t, fps);
        let (khz, class) = if fps == 0 || t.htotal == 0 || t.vtotal == 0 { (0, ClockClass::Raw) } else { snap_pixclk(raw) };
        let class_s = match class {
            ClockClass::Cea => "cea",
            ClockClass::Dmt => "dmt",
            ClockClass::Raw => "raw",
        };
        let (n48, cts48) = if khz != 0 { acr_n_cts(khz, 48000) } else { (0, 0) };
        let m = decode_mai_ctl(mai_ctl);
        let (crp_n, crp_ext, crp_dis) = decode_crp(crp);
        let (ram_en, ram_mask) = decode_ram_packets(ram_cfg);
        let line = format!(
            "[hdmi] hdmi{} hpd={} sched=0x{:08x} hdmi_mode={} active={} gate={} rst_bit={} \
             mai_ctl=0x{:08x}(en={} ch={} errE={} errF={}) mai_fmt=0x{:08x} mai_smp=0x{:08x} mai_cfg=0x{:08x} fifo=0x{:08x} \
             crp=0x{:08x}(n={} ext={} dis={}) cts=0x{:08x} apc=0x{:08x} ram_pkt=0x{:08x}(en={} mask=0x{:04x}) ram_st=0x{:08x} \
             h={}/{} v={}/{} fps={}.{:02} pixclk={} kHz ({} raw={}) n48={} cts48={}\n",
            port,
            (hotplug & HOTPLUG_CONNECTED != 0) as u8,
            sched,
            (sched & SCHED_MODE_HDMI != 0) as u8,
            (sched & SCHED_HDMI_ACTIVE != 0) as u8,
            if dvp_audio_gate_open(misc, port) { "open" } else { "closed" },
            dvp_reset_bit(sw_init, port),
            mai_ctl, m.enabled as u8, m.channels, m.underflow as u8, m.overflow as u8,
            mai_fmt, mai_smp, mai_cfg, fifo,
            crp, crp_n, crp_ext as u8, crp_dis as u8, cts, apc,
            ram_cfg, ram_en as u8, ram_mask, ram_st,
            t.hactive, t.htotal, t.vactive, t.vtotal,
            fps / 1000, (fps % 1000) / 10,
            khz, class_s, raw, n48, cts48,
        );
        Ok((line, khz))
    }

    /// Frame rate in mHz from the `hd` FRAME_COUNT: wait for an edge (<= 100 ms),
    /// then count edges until >= 100 ms have passed (<= 300 ms total), timing
    /// edge to edge so the error is the poll granularity, not a whole frame.
    /// 0 if the counter does not move (port not scanning out).
    fn measure_fps(fc_va: usize, first: u32) -> u32 {
        let rd_fc = || unsafe { (fc_va as *const u32).mmio_read() };
        let start = sched::monotonic_ns();
        let mut last = first;
        let t0 = loop {
            let v = rd_fc();
            let now = sched::monotonic_ns();
            if v != last {
                last = v;
                break now;
            }
            if now - start > 100_000_000 {
                return 0;
            }
        };
        let base = last;
        loop {
            let v = rd_fc();
            let now = sched::monotonic_ns();
            if v != last {
                last = v;
                if now - t0 >= 100_000_000 {
                    return fps_milli(v.wrapping_sub(base), now - t0);
                }
            }
            if now - start > 300_000_000 {
                return 0;
            }
        }
    }

    struct Mbox {
        hhdm: usize,
    }

    impl Mbox {
        /// One single-tag property call with `vals` in a value buffer of `buf_words`
        /// (>= vals.len()); returns the whole value buffer.
        fn tag(&mut self, tag: u32, vals: &[u32], buf_words: usize) -> Option<[u32; 34]> {
            let n = buf_words.max(vals.len()).min(34);
            let words = 5 + n + 1;
            let mut msg = [0u32; 40];
            msg[0] = (words * 4) as u32;
            msg[2] = tag;
            msg[3] = (n * 4) as u32;
            msg[5..5 + vals.len()].copy_from_slice(vals);
            let r = unsafe { crate::rpi_mailbox::property_call(self.hhdm, &msg[..words]) }.ok()?;
            if r.len() < 5 + n || r[4] & 0x8000_0000 == 0 {
                return None;
            }
            let mut out = [0u32; 34];
            out[..n].copy_from_slice(&r[5..5 + n]);
            Some(out)
        }

        fn edid_block(&mut self, block: u32) -> Option<[u8; 128]> {
            let v = self.tag(TAG_GET_EDID_BLOCK, &[block, 0], 34)?;
            if v[0] != block || v[1] != 0 {
                return None;
            }
            let mut b = [0u8; 128];
            for i in 0..32 {
                b[i * 4..i * 4 + 4].copy_from_slice(&v[2 + i].to_le_bytes());
            }
            Some(b)
        }

        /// `<n> blocks sink=hdmi|dvi` or `refused`.
        fn edid_summary(&mut self) -> String {
            let Some(b0) = self.edid_block(0) else { return String::from("refused") };
            let Some(ext) = edid_extensions(&b0) else { return String::from("badhdr") };
            let mut hdmi = false;
            for i in 1..=ext.min(3) as u32 {
                if let Some(b) = self.edid_block(i) {
                    hdmi |= cea_has_hdmi_vsdb(&b);
                }
            }
            format!("{}blk sink={}", 1 + ext as u32, if hdmi { "hdmi" } else { "dvi" })
        }
    }
}
