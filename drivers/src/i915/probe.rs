//! G0: read-only first contact with an Intel Gen9 GPU.
//!
//! Everything a first boot on new hardware should tell us, logged once:
//! config space, BARs, stolen memory and the GGTT, the ACPI OpRegion and the
//! VBT (panel and port description), the display pipeline the UEFI GOP left
//! running (which transcoder/pipe/plane scans out the boot framebuffer, and
//! whether the GGTT really maps it into stolen memory), the forcewake
//! handshake, fuses (slice/subslice/EU topology), clocks, RC6 state and the
//! idle engines' registers.
//!
//! The only writes: PCI BAR sizing (decode off, every register restored, as
//! Linux does at enumeration), PCI memory decode switched on if firmware
//! left it off, and the forcewake request (taken, then given back).
//! Summary lines go to the console and the log; the bulk register dumps go
//! to the persistent log only (`[i915]` prefix on both).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::hw::{self, reg};
use super::{dump, fmt_size, forcewake_get, forcewake_put, log, now_ns, opt, BootEnv, Device, Gsm, Mmio};
use super::{idump, ilog};
use crate::pci::{self, PciDevice, PciFunction};

pub(super) fn run(env: &BootEnv) -> Option<Device> {
    let cands: Vec<PciDevice> = pci::scan().into_iter()
        .filter(|d| d.vendor_id == hw::INTEL_VENDOR && d.class == 0x03)
        .collect();
    if cands.is_empty() { return None; }
    let verbose = opt("i915.probe") != Some(0);
    let t0 = now_ns();
    let pd = cands[0];
    let f = pd.function();
    let platform = hw::identify(pd.device_id);
    if verbose {
        ilog!("=== i915 probe: {} Intel display function(s); using {:02x}:{:02x}.{} ===",
            cands.len(), pd.bus, pd.dev, pd.func);
        log_config(&f, &platform);
    }
    let platform = match platform {
        Some(p) => p,
        None => {
            ilog!("device 0x{:04x} is not a Gen9 GPU this driver knows; config space only", pd.device_id);
            return None;
        }
    };

    // BARs (sized the safe way: decode off, every register restored).
    let s = f.size_bars();
    let bar = |i: usize| s.bars.iter().find(|b| b.index == i && !b.io).copied();
    if verbose {
        for b in &s.bars {
            if b.io { ilog!("BAR{}: io base=0x{:x} size={}", b.index, b.base, b.size); }
            else {
                ilog!("BAR{}: mem{} {} base=0x{:x} size={}{}", b.index, if b.is64 { "64" } else { "32" },
                    if b.prefetchable { "prefetchable" } else { "non-prefetchable" }, b.base, fmt_size(b.size),
                    match b.index { 0 => " (GTTMMADR: MMIO + GGTT)", 2 => " (GMADR: CPU aperture)", _ => "" });
            }
        }
    }
    let (bar0, gmadr) = match (bar(hw::cfg::GTTMMADR_BAR), bar(hw::cfg::GMADR_BAR)) {
        (Some(b0), Some(b2)) if b0.base != 0 && b0.size >= 16 << 20 && b2.base != 0 => (b0, b2),
        _ => { log("BAR0/BAR2 missing or unassigned; stopping"); return None; }
    };

    // Stolen memory and the GGTT.
    let ggc = f.read16(hw::cfg::GGC);
    let gmch = hw::decode_ggc(ggc);
    let (stolen_base, bdsm_lock) = hw::decode_bdsm(f.read32(hw::cfg::BDSM));
    if verbose {
        ilog!("GGC=0x{:04x}: GGTT {} ({} PTEs, {} of PTE space), stolen {} at 0x{:x} (BDSM lock={}), GGC lock={} VGA {}",
            ggc, fmt_size(gmch.ggtt_size), gmch.ggtt_entries, fmt_size(gmch.gtt_table_bytes),
            fmt_size(gmch.stolen_size), stolen_base, bdsm_lock as u8, gmch.locked as u8,
            if gmch.vga_disabled { "disabled" } else { "enabled" });
    }

    // Boot framebuffer vs aperture / stolen.
    let fb_size = env.fb_pitch as u64 * env.fb_height as u64;
    let fb_ggtt = if env.fb_phys >= gmadr.base && env.fb_phys + fb_size <= gmadr.base + gmadr.size {
        Some(env.fb_phys - gmadr.base)
    } else { None };
    if verbose {
        match fb_ggtt {
            Some(o) => ilog!("boot framebuffer phys 0x{:x} {}x{} pitch {} ({}) = GMADR+0x{:x} -> GGTT offset 0x{:x}",
                env.fb_phys, env.fb_width, env.fb_height, env.fb_pitch, fmt_size(fb_size), o, o),
            None if env.fb_phys >= stolen_base && env.fb_phys < stolen_base + gmch.stolen_size =>
                ilog!("boot framebuffer phys 0x{:x} is inside stolen memory directly (not through the aperture)", env.fb_phys),
            None => ilog!("boot framebuffer phys 0x{:x} is not in GMADR [0x{:x}, +{}) nor in stolen", env.fb_phys,
                gmadr.base, fmt_size(gmadr.size)),
        }
    }

    // MMIO.
    if f.command() & pci::CMD_MEMORY == 0 {
        log("PCI memory decode was off; enabling it");
        f.enable_memory();
    }
    let mva = match mm::kva::ioremap(bar0.base, hw::MMIO_SIZE, mm::kva::CacheMode::Uncached) {
        Some(v) => v,
        None => { log("cannot map the BAR0 register window; stopping"); return None; }
    };
    let m = Mmio { va: mva };
    let gsm = if gmch.gtt_table_bytes != 0 && gmch.gtt_table_bytes <= 8 << 20 {
        mm::kva::ioremap(bar0.base + hw::GTTADR_OFFSET, gmch.gtt_table_bytes as usize, mm::kva::CacheMode::Uncached)
            .map(|va| Gsm { va, entries: gmch.ggtt_entries })
    } else { None };
    if gsm.is_none() { log("GGTT page table not mapped (GGMS=0 or mapping failed); GGTT checks skipped"); }

    let dev = Device {
        func: f, platform, mmio: m, gsm, gmch, stolen_base,
        gmadr_base: gmadr.base, gmadr_size: gmadr.size, fb_ggtt, fake: false,
    };
    if !verbose { return Some(dev); }

    // A dead BAR reads all-ones everywhere.
    let strap = m.r(reg::SFUSE_STRAP);
    if strap == 0xFFFF_FFFF && m.r(reg::PIPESRC_A) == 0xFFFF_FFFF {
        log("MMIO reads 0xffffffff: BAR0 not decoding (device off the bus?); stopping");
        return None;
    }
    log_display(&dev, env);
    log_opregion(&f);
    log_gt(&dev);
    log_ggtt(&dev, env);
    ilog!("=== i915 probe done in {} ms (no engine touched) ===", now_ns().wrapping_sub(t0) / 1_000_000);
    Some(dev)
}

// ── config space ─────────────────────────────────────────────────────────────

fn log_config(f: &PciFunction, platform: &Option<hw::Platform>) {
    let ids = f.read32(0);
    let cr = f.read32(0x08);
    let sub = f.read32(0x2C);
    ilog!("device {:02x}:{:02x}.{} vendor=0x{:04x} device=0x{:04x} subsys=0x{:04x}:0x{:04x} rev=0x{:02x} class={:06x}",
        f.bus, f.dev, f.func, ids & 0xFFFF, ids >> 16, sub & 0xFFFF, sub >> 16, cr & 0xFF, cr >> 8);
    match platform {
        Some(p) => ilog!("identified: {} GT{} {} (Gen9{}; LLC={}){}", p.family.name(), p.gt, p.variant,
            if p.family.is_gen95() { ".5" } else { "" }, p.has_llc() as u8,
            if p.is_target_laptop_gpu() { " -- the target laptop's UHD Graphics 620" } else { "" }),
        None => log("device id not in the Gen9 table"),
    }
    let cmd = f.command();
    ilog!("COMMAND=0x{:04x} (io={} mem={} busmaster={} intx-disable={}) STATUS=0x{:04x} INT line={} pin={}",
        cmd, cmd & 1, (cmd >> 1) & 1, (cmd >> 2) & 1, (cmd >> 10) & 1, f.read16(0x06), f.read8(0x3C), f.read8(0x3D));
    let mut caps = String::new();
    for (id, off) in f.capabilities() {
        let name = match id { 0x01 => "PM", 0x05 => "MSI", 0x09 => "vendor", 0x10 => "PCIe", 0x11 => "MSI-X", 0x13 => "AF", _ => "?" };
        caps.push_str(&format!(" {}@0x{:02x}", name, off));
        if id == 0x05 {
            let c = f.read16(off + 2);
            caps.push_str(&format!("(ctrl=0x{:04x},en={})", c, c & 1));
        }
        if id == 0x01 { caps.push_str(&format!("(PMCSR=0x{:04x})", f.read16(off + 4))); }
    }
    ilog!("capabilities:{}", if caps.is_empty() { String::from(" none") } else { caps });
    let ext = f.ext_capabilities();
    if !ext.is_empty() {
        let mut s = String::new();
        for (id, v, off) in ext { s.push_str(&format!(" 0x{:04x}v{}@0x{:03x}", id, v, off)); }
        ilog!("extended capabilities:{}", s);
    }
    ilog!("config: GGC(0x50)=0x{:04x} BDSM(0x5c)=0x{:08x} MSAC(0x62)=0x{:02x} ASLS(0xfc)=0x{:08x}",
        f.read16(hw::cfg::GGC), f.read32(hw::cfg::BDSM), f.read8(hw::cfg::MSAC), f.read32(hw::cfg::ASLS));
    // Host bridge (00:00.0): the memory map the GPU's stolen ranges live in.
    let hb = PciFunction::new(0, 0, 0);
    if hb.vendor_id() == hw::INTEL_VENDOR {
        let touud = (hb.read32(0xAC) as u64) << 32 | hb.read32(0xA8) as u64;
        ilog!("host bridge 00:00.0 device=0x{:04x} TOLUD=0x{:08x} TOUUD=0x{:x} BDSM=0x{:08x} BGSM=0x{:08x} TSEG=0x{:08x}",
            hb.device_id(), hb.read32(0xBC), touud & !1, hb.read32(0xB0), hb.read32(0xB4), hb.read32(0xB8));
    }
    let lpc = PciFunction::new(0, 0x1F, 0);
    if lpc.vendor_id() == hw::INTEL_VENDOR {
        ilog!("PCH: LPC/eSPI 00:1f.0 device=0x{:04x} -> {}", lpc.device_id(), hw::pch_name(lpc.device_id()));
    }
}

// ── display ──────────────────────────────────────────────────────────────────

fn log_display(d: &Device, env: &BootEnv) {
    let m = &d.mmio;
    let fpga0 = m.r(reg::FPGA_DBG);
    let pw1 = m.r(reg::HSW_PWR_WELL_CTL1);
    let pw2 = m.r(reg::HSW_PWR_WELL_CTL2);
    ilog!("display power: FUSE_STATUS=0x{:08x} PWR_WELL_CTL1(BIOS)=0x{:08x} [{}] CTL2(driver)=0x{:08x} [{}] DC_STATE_EN=0x{:08x}",
        m.r(reg::SKL_FUSE_STATUS), pw1, hw::skl_power_wells(pw1), pw2, hw::skl_power_wells(pw2), m.r(reg::DC_STATE_EN));
    idump!("PWR_WELL_CTL3(KVMR)=0x{:08x} CTL4(debug)=0x{:08x} CDCLK_CTL=0x{:08x} LCPLL1_CTL=0x{:08x} LCPLL2_CTL=0x{:08x}",
        m.r(reg::HSW_PWR_WELL_CTL3), m.r(reg::HSW_PWR_WELL_CTL4), m.r(reg::CDCLK_CTL), m.r(reg::LCPLL1_CTL), m.r(reg::LCPLL2_CTL));
    ilog!("clocks: CDCLK_CTL=0x{:08x} LCPLL1_CTL=0x{:08x} (enabled={}) DPLL_CTRL1=0x{:08x} DPLL_CTRL2=0x{:08x} DPLL_STATUS=0x{:08x}",
        m.r(reg::CDCLK_CTL), m.r(reg::LCPLL1_CTL), m.r(reg::LCPLL1_CTL) >> 31, m.r(reg::DPLL_CTRL1), m.r(reg::DPLL_CTRL2), m.r(reg::DPLL_STATUS));

    // Transcoders: A, B, C (index 0..2) and eDP.
    let mut scanout: Option<(u32, String)> = None;
    let edp = m.r(reg::TRANS_DDI_FUNC_CTL_EDP);
    let edp_conf = m.r(reg::TRANSCONF_EDP);
    for t in 0..4u32 {
        let (name, tbase, conf, ddi) = if t < 3 {
            ((b'A' + t as u8) as char, reg::pipe(reg::TRANS_HTOTAL_A, t), m.r(reg::pipe(reg::TRANSCONF_A, t)),
             m.r(reg::pipe(reg::TRANS_DDI_FUNC_CTL_A, t)))
        } else {
            ('E', reg::TRANS_HTOTAL_EDP, edp_conf, edp)
        };
        let tname = if t < 3 { format!("transcoder {}", name) } else { String::from("transcoder eDP") };
        let h = hw::timing_pair(m.r(tbase));
        let hs = hw::timing_pair(m.r(tbase + 8));
        let v = hw::timing_pair(m.r(tbase + 0xC));
        let vs = hw::timing_pair(m.r(tbase + 0x14));
        let f = hw::decode_trans_ddi_func_ctl(ddi);
        idump!("{}: TRANSCONF=0x{:08x} DDI_FUNC_CTL=0x{:08x} HTOTAL=0x{:08x} HBLANK=0x{:08x} HSYNC=0x{:08x} VTOTAL=0x{:08x} VBLANK=0x{:08x} VSYNC=0x{:08x} DATA_M1=0x{:08x} N1=0x{:08x} LINK_M1=0x{:08x} N1=0x{:08x}",
            tname, conf, ddi, m.r(tbase), m.r(tbase + 4), m.r(tbase + 8), m.r(tbase + 0xC), m.r(tbase + 0x10), m.r(tbase + 0x14),
            m.r(tbase + 0x30), m.r(tbase + 0x34), m.r(tbase + 0x40), m.r(tbase + 0x44));
        if conf >> 31 != 0 || f.enabled {
            ilog!("{}: {} (state={}) DDI {} {} x{} {}bpc{}; active {}x{} total {}x{}, hsync {}-{}, vsync {}-{}",
                tname, if conf >> 31 != 0 { "ENABLED" } else { "conf off" }, (conf >> 30) & 1,
                f.port, f.mode, f.lanes, f.bpc,
                if t == 3 { format!(", input {}", f.edp_input) } else { String::new() },
                h.0, v.0, h.1, v.1, hs.0, hs.1, vs.0, vs.1);
        }
        if t == 3 && f.enabled && scanout.is_none() {
            let pipe = match (ddi >> 12) & 7 { 0 | 4 => 0, 5 => 1, 6 => 2, _ => 0 };
            scanout = Some((pipe, String::from("eDP")));
        } else if t < 3 && f.enabled && scanout.is_none() {
            scanout = Some((t, format!("transcoder {}", name)));
        }
    }

    // Pipes and planes.
    for p in 0..3u32 {
        let conf = m.r(reg::pipe(reg::TRANSCONF_A, p));
        let src = hw::pipe_src(m.r(reg::pipe(reg::PIPESRC_A, p)));
        let mut active_planes = 0;
        for pl in 0..3u32 {
            let ctl = m.r(reg::plane(reg::PLANE_CTL_1_A, p, pl));
            let stride = m.r(reg::plane(reg::PLANE_STRIDE_1_A, p, pl));
            let pos = m.r(reg::plane(reg::PLANE_POS_1_A, p, pl));
            let size = m.r(reg::plane(reg::PLANE_SIZE_1_A, p, pl));
            let surf = m.r(reg::plane(reg::PLANE_SURF_1_A, p, pl));
            let off = m.r(reg::plane(reg::PLANE_OFFSET_1_A, p, pl));
            let buf = m.r(reg::plane(reg::PLANE_BUF_CFG_1_A, p, pl));
            idump!("plane {}{}: CTL=0x{:08x} STRIDE=0x{:08x} POS=0x{:08x} SIZE=0x{:08x} SURF=0x{:08x} OFFSET=0x{:08x} BUF_CFG=0x{:08x}",
                pl + 1, (b'A' + p as u8) as char, ctl, stride, pos, size, surf, off, buf);
            if ctl >> 31 != 0 {
                active_planes += 1;
                let til = hw::plane_tiling(ctl);
                let sb = hw::plane_stride_bytes(stride, til);
                let w = (size & 0x1FFF) + 1;
                let h = ((size >> 16) & 0x1FFF) + 1;
                let surf_ggtt = (surf & 0xFFFF_F000) as u64;
                let fbmatch = match d.fb_ggtt {
                    Some(o) if o == surf_ggtt => String::from(" == boot framebuffer GGTT offset (OK)"),
                    Some(o) => format!(" != boot framebuffer GGTT offset 0x{:x} (MISMATCH)", o),
                    None => String::new(),
                };
                ilog!("pipe {} plane {}: ENABLED {} {:?} {}x{} stride {} B (fb pitch {}), surface GGTT 0x{:08x}, offset x={} y={}{}",
                    (b'A' + p as u8) as char, pl + 1, hw::plane_format_name(ctl), til, w, h, sb, env.fb_pitch,
                    surf_ggtt, off & 0x1FFF, (off >> 16) & 0x1FFF, fbmatch);
            }
        }
        let cur = m.r(reg::pipe(reg::CUR_CTL_A, p));
        let ps1 = m.r(reg::PS_CTRL_1A + 0x800 * p);
        let ps2 = m.r(reg::PS_CTRL_1A + 0x100 + 0x800 * p);
        idump!("pipe {}: TRANSCONF=0x{:08x} PIPESRC={}x{} PIPEDSL=0x{:08x} FRMCOUNT=0x{:08x} CUR_CTL=0x{:08x} CUR_BASE=0x{:08x} PS_CTRL1=0x{:08x} PS_CTRL2=0x{:08x} PS_WIN_SZ1=0x{:08x}",
            (b'A' + p as u8) as char, conf, src.0, src.1, m.r(reg::pipe(reg::PIPEDSL_A, p)), m.r(reg::pipe(reg::PIPE_FRMCOUNT_A, p)),
            cur, m.r(reg::pipe(reg::CUR_BASE_A, p)), ps1, ps2, m.r(reg::PS_WIN_SZ_1A + 0x800 * p));
        if conf >> 31 != 0 || active_planes != 0 {
            // A moving frame counter proves the pipe is really scanning out.
            let f0 = m.r(reg::pipe(reg::PIPE_FRMCOUNT_A, p));
            super::delay_us(40_000);
            let f1 = m.r(reg::pipe(reg::PIPE_FRMCOUNT_A, p));
            ilog!("pipe {}: source {}x{}, {} plane(s) on, scaler1=0x{:08x}, frame counter {} -> {} in 40 ms ({})",
                (b'A' + p as u8) as char, src.0, src.1, active_planes, ps1, f0, f1,
                if f1 != f0 { "scanning out" } else { "NOT moving" });
        }
    }
    match scanout {
        Some((p, via)) => ilog!("GOP scanout: {} fed by pipe {}", via, (b'A' + p as u8) as char),
        None => log("GOP scanout: no enabled transcoder found (display off or unexpected state)"),
    }

    // DDI ports.
    for port in 0..5u32 {
        let buf = m.r(reg::port(reg::DDI_BUF_CTL_A, port));
        let tp = m.r(reg::port(reg::DP_TP_CTL_A, port));
        let tps = m.r(reg::port(reg::DP_TP_STATUS_A, port));
        idump!("DDI {}: BUF_CTL=0x{:08x} DP_TP_CTL=0x{:08x} DP_TP_STATUS=0x{:08x}", (b'A' + port as u8) as char, buf, tp, tps);
        if buf >> 31 != 0 {
            ilog!("DDI {}: buffer ENABLED (idle={}, width x{}), DP_TP_CTL=0x{:08x}", (b'A' + port as u8) as char,
                (buf >> 7) & 1, ((buf >> 1) & 7) + 1, tp);
        }
    }
    // Panel power and backlight (south display, SPT PCH).
    let pps = m.r(reg::PCH_PP_STATUS);
    let ppc = m.r(reg::PCH_PP_CONTROL);
    let blc1 = m.r(reg::BLC_PWM_PCH_CTL1);
    let blc2 = m.r(reg::BLC_PWM_PCH_CTL2);
    ilog!("panel power: PP_STATUS=0x{:08x} (on={}) PP_CONTROL=0x{:08x} ON_DELAYS=0x{:08x} OFF_DELAYS=0x{:08x}; backlight PCH_CTL1=0x{:08x} (enabled={}) PCH_CTL2=0x{:08x} (period {} duty {})",
        pps, pps >> 31, ppc, m.r(reg::PCH_PP_ON_DELAYS), m.r(reg::PCH_PP_OFF_DELAYS), blc1, blc1 >> 31, blc2, blc2 >> 16, blc2 & 0xFFFF);
    idump!("BLC_PWM_CPU_CTL2=0x{:08x} BLC_PWM_CPU_CTL=0x{:08x} PP_DIVISOR=0x{:08x} SFUSE_STRAP=0x{:08x} SDEISR=0x{:08x} SHOTPLUG_CTL=0x{:08x} SHOTPLUG_CTL2=0x{:08x}",
        m.r(reg::BLC_PWM_CPU_CTL2), m.r(reg::BLC_PWM_CPU_CTL), m.r(reg::PCH_PP_DIVISOR), m.r(reg::SFUSE_STRAP),
        m.r(reg::SDEISR), m.r(reg::SHOTPLUG_CTL), m.r(reg::SHOTPLUG_CTL2));
    let strap = m.r(reg::SFUSE_STRAP);
    ilog!("SFUSE_STRAP=0x{:08x}: DDI B {} C {} D {}", strap,
        if strap & 4 != 0 { "present" } else { "-" }, if strap & 2 != 0 { "present" } else { "-" },
        if strap & 1 != 0 { "present" } else { "-" });
    let dmc0 = m.r(reg::DMC_PROGRAM0);
    ilog!("DMC: PROGRAM[0]=0x{:08x} SSP_BASE=0x{:08x} HTP=0x{:08x} DC3->DC5={} DC5->DC6={} ({})",
        dmc0, m.r(reg::DMC_SSP_BASE), m.r(reg::DMC_HTP_SKL), m.r(reg::SKL_DMC_DC3_DC5_COUNT), m.r(reg::SKL_DMC_DC5_DC6_COUNT),
        if dmc0 != 0 { "firmware present" } else { "no DMC firmware loaded" });
    let fpga1 = m.r(reg::FPGA_DBG);
    ilog!("FPGA_DBG before/after display reads: 0x{:08x} / 0x{:08x}{}", fpga0, fpga1,
        if fpga1 & reg::FPGA_DBG_RM_NOCLAIM != 0 { " (unclaimed register access flagged: some well is powered down)" } else { "" });
}

// ── OpRegion / VBT ───────────────────────────────────────────────────────────

fn copy_phys(phys: u64, len: usize) -> Option<Vec<u8>> {
    let va = mm::kva::ioremap(phys & !0xFFF, len + (phys & 0xFFF) as usize, mm::kva::CacheMode::WriteBack)?;
    let base = va + (phys & 0xFFF) as usize;
    let mut v = alloc::vec![0u8; len];
    for (i, b) in v.iter_mut().enumerate() {
        *b = unsafe { core::ptr::read_volatile((base + i) as *const u8) };
    }
    mm::kva::iounmap(va);
    Some(v)
}

fn log_opregion(f: &PciFunction) {
    let asls = f.read32(hw::cfg::ASLS) as u64;
    if asls == 0 || asls == 0xFFFF_FFFF {
        log("OpRegion: ASLS is 0 (no ACPI OpRegion: VBT unavailable)");
        return;
    }
    let op = match copy_phys(asls, hw::OPREGION_SIZE) {
        Some(v) => v,
        None => { ilog!("OpRegion at 0x{:x}: cannot map", asls); return; }
    };
    let info = match hw::parse_opregion(&op, asls) {
        Some(i) => i,
        None => { ilog!("OpRegion at 0x{:x}: bad signature ({:02x?})", asls, &op[..16]); return; }
    };
    ilog!("OpRegion at 0x{:x}: version {}.{}.{} size {} KiB mailboxes=0x{:x} BIOS \"{}\" VBT {:?}",
        asls, info.version.0, info.version.1, info.version.2, info.size_kb, info.mboxes, info.bios_ver, info.vbt);
    let vbt: Vec<u8> = match info.vbt {
        hw::VbtLocation::Mailbox4(o, n) => op[o..o + n].to_vec(),
        hw::VbtLocation::Rvda(a, n) if n <= 64 * 1024 => match copy_phys(a, n) { Some(v) => v, None => { log("VBT (RVDA): cannot map"); return; } },
        hw::VbtLocation::Rvda(_, n) => { ilog!("VBT (RVDA): implausible size {}", n); return; }
        hw::VbtLocation::None => { log("VBT: none in the OpRegion"); return; }
    };
    let vi = match hw::parse_vbt(&vbt) {
        Some(v) => v,
        None => { log("VBT: invalid header"); return; }
    };
    let mut blocks = String::new();
    for &(id, size) in &vi.blocks {
        let n = hw::bdb_block_name(id);
        if n.is_empty() { blocks.push_str(&format!(" {}:{}", id, size)); }
        else { blocks.push_str(&format!(" {}({}):{}", id, n, size)); }
    }
    ilog!("VBT \"{}\" version {} size {} BDB version {} panel_type={:?}", vi.signature.trim_end(), vi.version, vi.vbt_size,
        vi.bdb_version, vi.panel_type);
    ilog!("VBT blocks:{}", blocks);
    if opt("i915.dumpvbt") != Some(0) {
        let n = vi.vbt_size.min(vbt.len());
        ilog!("raw VBT follows in the log ({} bytes; decode with base64 -d | intel_vbt_decode --file=/dev/stdin)", n);
        super::emit_logonly("-----BEGIN VBT-----\n");
        b64(&vbt[..n]);
        super::emit_logonly("-----END VBT-----\n");
    }
}

fn b64(data: &[u8]) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut line = String::new();
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16 | (*ch.get(1).unwrap_or(&0) as u32) << 8 | *ch.get(2).unwrap_or(&0) as u32;
        line.push(T[(n >> 18) as usize & 63] as char);
        line.push(T[(n >> 12) as usize & 63] as char);
        line.push(if ch.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        line.push(if ch.len() > 2 { T[n as usize & 63] as char } else { '=' });
        if line.len() >= 76 { line.push('\n'); super::emit_logonly(&line); line.clear(); }
    }
    if !line.is_empty() { line.push('\n'); super::emit_logonly(&line); }
}

// ── GT (needs forcewake) ─────────────────────────────────────────────────────

fn log_gt(d: &Device) {
    let m = &d.mmio;
    let ack0: Vec<u32> = hw::FW_DOMAINS.iter().map(|x| m.r(x.2)).collect();
    ilog!("forcewake acks before: gt=0x{:08x} render=0x{:08x} media=0x{:08x}", ack0[0], ack0[1], ack0[2]);
    let lat = match forcewake_get(m) {
        Ok(l) => l,
        Err(dom) => {
            ilog!("forcewake: domain '{}' did not ack within 50 ms; GT registers skipped (GT asleep or powered off?)", dom);
            return;
        }
    };
    ilog!("forcewake: acked in gt={} us render={} us media={} us", lat[0], lat[1], lat[2]);

    let fuse2 = m.r(reg::GEN8_FUSE2);
    let eud = [m.r(reg::GEN9_EU_DISABLE0), m.r(reg::GEN9_EU_DISABLE0 + 4), m.r(reg::GEN9_EU_DISABLE0 + 8)];
    let s = hw::gen9_sseu(fuse2, eud, d.platform.family.is_lp());
    ilog!("topology: FUSE2=0x{:08x} EU_DISABLE=0x{:08x}/0x{:08x}/0x{:08x} -> slices=0x{:x} subslices=0x{:x} ({} total) EUs={} ({}/subslice, 7-EU subslices {:?})",
        fuse2, eud[0], eud[1], eud[2], s.slice_mask, s.subslice_mask, s.subslices_total(), s.eu_total, s.eu_per_subslice, s.subslice_7eu);

    let ctc = m.r(reg::CTC_MODE);
    let tso = m.r(reg::GEN9_TIMESTAMP_OVERRIDE);
    let hz = hw::gen9_cs_timestamp_hz(ctc, tso, d.platform.family.is_lp());
    let ts = reg::RENDER_RING_BASE + reg::RING_TIMESTAMP;
    let (t0, n0) = (m.r(ts), now_ns());
    super::delay_us(2000);
    let (t1, n1) = (m.r(ts), now_ns());
    let measured = (t1.wrapping_sub(t0) as u64) * 1_000_000_000 / n1.wrapping_sub(n0).max(1);
    ilog!("CS timestamp: CTC_MODE=0x{:08x} TIMESTAMP_OVERRIDE=0x{:08x} -> {} Hz; measured {} Hz ({} ticks in {} us)",
        ctc, tso, hz, measured, t1.wrapping_sub(t0), n1.wrapping_sub(n0) / 1000);

    let cap_reg = if d.platform.family.is_lp() { reg::BXT_RP_STATE_CAP } else { reg::GEN6_RP_STATE_CAP };
    let cap = m.r(cap_reg);
    let (rp0, rp1, rpn) = hw::decode_rp_state_cap(cap);
    let rpstat = m.r(reg::GEN6_RPSTAT1);
    ilog!("GT frequency: RP_STATE_CAP=0x{:08x} -> RP0 {} MHz RP1 {} MHz RPn {} MHz; RPSTAT1=0x{:08x} -> current {} MHz",
        cap, rp0, rp1, rpn, rpstat, hw::decode_cagf_mhz(rpstat));
    ilog!("RC6: RC_CONTROL=0x{:08x} RC_STATE=0x{:08x} GT_GFX_RC6=0x{:08x}",
        m.r(reg::GEN6_RC_CONTROL), m.r(reg::GEN6_RC_STATE), m.r(reg::GEN6_GT_GFX_RC6));
    let pat = (m.r(reg::GEN8_PRIVATE_PAT_HI) as u64) << 32 | m.r(reg::GEN8_PRIVATE_PAT_LO) as u64;
    ilog!("private PAT=0x{:016x} (Linux programs 0x{:016x})", pat, hw::gen9_private_ppat());
    let sr = m.r(reg::GEN6_STOLEN_RESERVED);
    match hw::decode_stolen_reserved(sr, d.stolen_base + d.gmch.stolen_size) {
        Some((b, n)) => ilog!("stolen reserved: GEN6_STOLEN_RESERVED=0x{:08x} -> [0x{:x}, +{}) (not usable)", sr, b, fmt_size(n)),
        None => ilog!("stolen reserved: GEN6_STOLEN_RESERVED=0x{:08x} -> none", sr),
    }
    ilog!("errors: GDRST=0x{:08x} ERROR_GEN6=0x{:08x} RING_FAULT=0x{:08x} FAULT_TLB_DATA=0x{:08x}/0x{:08x}",
        m.r(reg::GEN6_GDRST), m.r(reg::ERROR_GEN6), m.r(reg::GEN8_RING_FAULT_REG),
        m.r(reg::GEN8_FAULT_TLB_DATA0), m.r(reg::GEN8_FAULT_TLB_DATA1));
    let mut irq = format!("MASTER_IRQ=0x{:08x}", m.r(reg::GEN8_MASTER_IRQ));
    for b in 0..4u32 {
        let r = reg::GEN8_GT_ISR0 + 0x10 * b;
        irq.push_str(&format!(" GT{}[isr=0x{:08x} imr=0x{:08x} iir=0x{:08x} ier=0x{:08x}]", b, m.r(r), m.r(r + 4), m.r(r + 8), m.r(r + 12)));
    }
    idump!("{}", irq);

    for e in hw::GEN9_ENGINES.iter() {
        let b = e.mmio_base;
        let r = |o: u32| m.r(b + o);
        let mode = r(reg::RING_MODE_GEN7);
        let mi = r(reg::RING_MI_MODE);
        let ctl = r(reg::RING_CTL);
        ilog!("{}: MODE=0x{:08x} (execlists={}) MI_MODE=0x{:08x} (idle={} stop={}) RING_CTL=0x{:08x} HEAD=0x{:08x} TAIL=0x{:08x} ACTHD=0x{:08x} HWS_PGA=0x{:08x} EXECLIST_STATUS=0x{:08x}_{:08x}",
            e.name, mode, (mode >> 15) & 1, mi, (mi >> 9) & 1, (mi >> 8) & 1, ctl, r(reg::RING_HEAD), r(reg::RING_TAIL),
            r(reg::RING_ACTHD), r(reg::RING_HWS_PGA), r(reg::RING_EXECLIST_STATUS_HI), r(reg::RING_EXECLIST_STATUS_LO));
        idump!("{}: START=0x{:08x} CSB_PTR=0x{:08x} RESET_CTL=0x{:08x} IMR=0x{:08x} EIR=0x{:08x} EMR=0x{:08x} ESR=0x{:08x} INSTDONE=0x{:08x} IPEHR=0x{:08x} BBADDR=0x{:08x} TIMESTAMP=0x{:08x} CTX_TIMESTAMP=0x{:08x} HWSTAM=0x{:08x}",
            e.name, r(reg::RING_START), r(reg::RING_CONTEXT_STATUS_PTR), r(reg::RING_RESET_CTL), r(reg::RING_IMR),
            r(reg::RING_EIR), r(reg::RING_EMR), r(reg::RING_ESR), r(reg::RING_INSTDONE), r(reg::RING_IPEHR),
            r(reg::RING_BBADDR), r(reg::RING_TIMESTAMP), r(reg::RING_CTX_TIMESTAMP), r(reg::RING_HWSTAM));
    }
    forcewake_put(m);
    let ack1: Vec<u32> = hw::FW_DOMAINS.iter().map(|x| m.r(x.2)).collect();
    ilog!("forcewake released; acks now gt=0x{:08x} render=0x{:08x} media=0x{:08x}", ack1[0], ack1[1], ack1[2]);
}

// ── GGTT ─────────────────────────────────────────────────────────────────────

fn log_ggtt(d: &Device, env: &BootEnv) {
    let g = match d.gsm { Some(g) => g, None => return };
    let stolen_end = d.stolen_base + d.gmch.stolen_size;
    // 1. The boot framebuffer's PTEs: present, inside stolen, contiguous?
    if let Some(off) = d.fb_ggtt {
        let pages = ((env.fb_pitch as u64 * env.fb_height as u64) + 4095) / 4096;
        let first = off / 4096;
        let mut contiguous = true;
        let mut in_stolen = true;
        let mut present = 0u64;
        let p0 = hw::ggtt_pte_phys(g.read(first));
        for i in 0..pages {
            let pte = g.read(first + i);
            if let Some(ph) = hw::ggtt_pte_phys(pte) {
                present += 1;
                if ph < d.stolen_base || ph >= stolen_end { in_stolen = false; }
                if p0.map(|p| p + i * 4096) != Some(ph) { contiguous = false; }
            }
        }
        ilog!("GGTT: boot framebuffer [0x{:x}, +{} pages): first PTE=0x{:016x} -> phys {:?}; {}/{} present, contiguous={}, all in stolen [0x{:x}, 0x{:x})={}",
            off, pages, g.read(first), p0.map(|p| format!("0x{:x}", p)), present, pages, contiguous as u8,
            d.stolen_base, stolen_end, in_stolen as u8);
    }
    if opt("i915.ggttscan") == Some(0) { return; }
    // 2. Occupancy scan (time-bounded): runs of present PTEs, classified as
    //    linear (consecutive physical pages) or repeated (one page, e.g. a
    //    scratch page).
    let t0 = now_ns();
    let budget = 400_000_000u64;
    let mut runs: Vec<(u64, u64, u64, bool)> = Vec::new(); // start, len, first phys, repeated
    let mut present = 0u64;
    let mut cur: Option<(u64, u64, u64, u64, bool)> = None; // start, len, first, last, repeated
    let mut scanned = 0u64;
    for i in 0..g.entries {
        if i & 0xFFF == 0 && now_ns().wrapping_sub(t0) > budget { break; }
        scanned = i + 1;
        let pte = g.read(i);
        match hw::ggtt_pte_phys(pte) {
            Some(ph) => {
                present += 1;
                cur = match cur {
                    Some((s, n, f, last, rep)) => {
                        let linear_next = ph == last.wrapping_add(4096);
                        let same = ph == last;
                        if (n == 1 || !rep) && linear_next { Some((s, n + 1, f, ph, false)) }
                        else if (n == 1 || rep) && same { Some((s, n + 1, f, ph, true)) }
                        else { runs.push((s, n, f, rep)); Some((i, 1, ph, ph, false)) }
                    }
                    None => Some((i, 1, ph, ph, false)),
                };
            }
            None => if let Some(c) = cur.take() { runs.push((c.0, c.1, c.2, c.4)); },
        }
    }
    if let Some(c) = cur.take() { runs.push((c.0, c.1, c.2, c.4)); }
    let ms = now_ns().wrapping_sub(t0) / 1_000_000;
    ilog!("GGTT scan: {}/{} entries in {} ms{}: {} present in {} run(s)", scanned, g.entries, ms,
        if scanned < g.entries { " (TRUNCATED by time budget)" } else { "" }, present, runs.len());
    for (k, r) in runs.iter().enumerate() {
        if k >= 48 { idump!("GGTT runs: {} more not listed", runs.len() - 48); break; }
        let line = format!("GGTT run [0x{:09x}, +{}) -> phys 0x{:x}{} {}", r.0 * 4096, fmt_size(r.1 * 4096), r.2,
            if r.3 { " (same page repeated)" } else { " (linear)" },
            if r.2 >= d.stolen_base && r.2 < stolen_end { "in stolen" } else { "in system RAM" });
        if k < 8 { log(&line); } else { dump(&line); }
    }
}
