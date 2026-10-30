//! Pipe B / DPLL1 / DDB bring-up self-tests (lane lapdispC2a; design
//! artifacts/notes/design-lapdisp2-2026-10-27.md lane C2). Everything is off
//! unless a knob asks, and nothing here lights a display:
//!
//!   `i915.pipeb_test=pll`     program DPLL1 for 148.5 MHz (HDMI 1080p60), wait
//!                             for lock, log the lock time, disable it again.
//!                             No DDI is mapped to the PLL.
//!   `i915.pipeb_test=dryrun`  the same, plus the whole pipe-B HDMI register
//!                             plan (Linux hsw_crtc_enable / intel_ddi_pre_enable_hdmi
//!                             on gen9) computed and LOGGED, never written; the
//!                             power wells it would take are taken and put back.
//!   `i915.pipeb_test=hdmi1|hdmi2`  the light-up (lane C2b, needs a monitor):
//!                             not implemented here, logs that and stops.
//!   `i915.ddb_test=1`         `kms::ddb_test`: shrink pipe A's DDB to half for
//!                             10 s on the panel alone, underruns must stay 0.
//!   `i915.pipeb_port=1|2`     DDI port for the plan (1 = B, default; 2 = C).
//!   `i915.pipeb_dpll=1|2|3`   the PLL to program (default 1; eDP owns DPLL0).
//!
//! All of it runs in one kthread started at the end of `kms::init`, skipped
//! on the fake device.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::buf_trans;
use super::ddb::{ddb_entry, skl_ddb_plan};
use super::hw::reg;
use super::power::{self, Dom};
use super::wrpll::{self, Params};
use super::{ilog, opt, word, Device, Mmio};

/// DPLL_CTRL1/2 and the per-PLL enable and config registers (gen9 SKL/KBL).
const DPLL_CTRL1: u32 = reg::DPLL_CTRL1;
const DPLL_CTRL2: u32 = reg::DPLL_CTRL2;
const DPLL_STATUS: u32 = reg::DPLL_STATUS;
/// PLL enable register of DPLL1..3: LCPLL2_CTL, WRPLL_CTL1, WRPLL_CTL2.
const fn pll_ctl(id: u32) -> u32 { match id { 1 => 0x46014, 2 => 0x46040, _ => 0x46060 } }
const fn cfgcr1(id: u32) -> u32 { 0x6C040 + 8 * (id - 1) }
const fn cfgcr2(id: u32) -> u32 { cfgcr1(id) + 4 }
const PLL_ENABLE: u32 = 1 << 31;

/// HDMI 1080p60 (CEA VIC 16), the plan's timing.
struct Mode { clock: u32, hd: u32, hss: u32, hse: u32, ht: u32, vd: u32, vss: u32, vse: u32, vt: u32 }
const M1080: Mode = Mode { clock: 148_500, hd: 1920, hss: 2008, hse: 2052, ht: 2200, vd: 1080, vss: 1084, vse: 1089, vt: 1125 };

pub(super) fn init(d: &Device) {
    let test = word("i915.pipeb_test");
    let ddb = opt("i915.ddb_test") == Some(1);
    if test.is_none() && !ddb { return; }
    if d.fake {
        ilog!("pipeb: i915.pipeb_test/ddb_test: skipped on the fake device");
        return;
    }
    if crate::kenv::sched::kthread::spawn("i915-pipeb", move || worker(test, ddb)).is_none() {
        ilog!("pipeb: could not start the test thread");
    }
}

fn worker(test: Option<&'static str>, ddb: bool) {
    if ddb { super::kms::ddb_test(); }
    let m = super::kms::mmio();
    match test {
        None => {}
        Some("pll") => { let _ = pll_try(&m, opt("i915.pipeb_dpll").unwrap_or(1) as u32, M1080.clock); }
        Some("dryrun") => dryrun(&m),
        Some(t @ ("hdmi1" | "hdmi2")) => ilog!("pipeb: i915.pipeb_test={}: the light-up is lane C2b ([MONITOR]); not in this build", t),
        Some(t) => ilog!("pipeb: unknown i915.pipeb_test={} (want pll, dryrun, hdmi1, hdmi2)", t),
    }
}

/// Program DPLL `id` for `clock_khz`, wait for the lock, log it, disable it
/// again and put back the CTRL1 field and CFGCRs. Refuses a PLL anything
/// else uses. Returns the lock time in us.
fn pll_try(m: &Mmio, id: u32, clock_khz: u32) -> Option<u64> {
    if !(1..=3).contains(&id) { ilog!("pipeb: i915.pipeb_dpll={}: want 1..3 (DPLL0 is eDP's)", id); return None; }
    let Some(p) = wrpll::calculate(clock_khz, wrpll::REF_KHZ) else {
        ilog!("pipeb: no WRPLL divider for {} kHz", clock_khz);
        return None;
    };
    let (ctl, c1, c2, st) = (m.r(pll_ctl(id)), m.r(DPLL_CTRL1), m.r(DPLL_CTRL2), m.r(DPLL_STATUS));
    if ctl & PLL_ENABLE != 0 || st & (1 << (id * 8)) != 0 {
        ilog!("pipeb: DPLL{} is in use (ctl 0x{:08x} status 0x{:08x}); not touching it", id, ctl, st);
        return None;
    }
    for port in 0..5u32 {
        if c2 & (1 << (3 * port)) != 0 && (c2 >> (3 * port + 1)) & 3 == id {
            ilog!("pipeb: DPLL{} is mapped to DDI port {} (CTRL2 0x{:08x}); not touching it", id, port, c2);
            return None;
        }
    }
    let (o1, o2) = (m.r(cfgcr1(id)), m.r(cfgcr2(id)));
    let sh = 6 * id;
    // OVERRIDE (bit 0 of the field) | HDMI_MODE (bit 5); rate bits stay 0.
    m.w(DPLL_CTRL1, (c1 & !(0x3F << sh)) | 1 << sh | 1 << (sh + 5));
    m.w(cfgcr1(id), p.cfgcr1());
    m.w(cfgcr2(id), p.cfgcr2());
    let _ = m.r(cfgcr2(id));
    m.w(pll_ctl(id), ctl | PLL_ENABLE);
    let lock = m.wait(DPLL_STATUS, 1 << (id * 8), 1 << (id * 8), 5000);
    m.w(pll_ctl(id), m.r(pll_ctl(id)) & !PLL_ENABLE);
    let off = m.wait(DPLL_STATUS, 1 << (id * 8), 0, 5000).is_ok();
    m.w(DPLL_CTRL1, (m.r(DPLL_CTRL1) & !(0x3F << sh)) | (c1 & (0x3F << sh)));
    m.w(cfgcr1(id), o1);
    m.w(cfgcr2(id), o2);
    match lock {
        Ok(us) => ilog!("pipeb: DPLL{} {} kHz dco {} kHz p {} (p0 {} p1 {} p2 {}) cfgcr1 0x{:08x} cfgcr2 0x{:08x} locked in {} us, disabled{}",
                        id, clock_khz, p.dco_khz, p.p, p.p0, p.p1, p.p2, p.cfgcr1(), p.cfgcr2(), us, if off { "" } else { " (lock bit still set!)" }),
        Err(s) => ilog!("pipeb: DPLL{} {} kHz cfgcr1 0x{:08x} cfgcr2 0x{:08x}: NOT locked in 5 ms (DPLL_STATUS 0x{:08x}), disabled",
                        id, clock_khz, p.cfgcr1(), p.cfgcr2(), s),
    }
    lock.ok()
}

struct Plan(Vec<(u32, u32, String)>);
impl Plan {
    fn add(&mut self, reg: u32, val: u32, what: impl Into<String>) { self.0.push((reg, val, what.into())); }
}

/// The pipe-B HDMI (DVI mode) register plan of design lane C2 steps 0-12
/// for `mode` on DDI `port` using DPLL `id`, computed from read-only
/// register state. `ddb_a` / `ddb_b` are `[start, end)` for each pipe.
fn build_plan(m: &Mmio, mode: &Mode, port: u32, id: u32, p: &Params, ddb_b: ((u32, u32), (u32, u32)), wm: Option<[u32; 9]>) -> Plan {
    let mut pl = Plan(Vec::new());
    let b = |a: u32| reg::pipe(a, 1);
    // 2-3: PLL
    let sh = 6 * id;
    pl.add(DPLL_CTRL1, (m.r(DPLL_CTRL1) & !(0x3F << sh)) | 1 << sh | 1 << (sh + 5), format!("DPLL_CTRL1 DPLL{} OVERRIDE|HDMI_MODE", id));
    pl.add(cfgcr1(id), p.cfgcr1(), "DPLL_CFGCR1 FREQ_ENABLE|fraction|integer");
    pl.add(cfgcr2(id), p.cfgcr2(), "DPLL_CFGCR2 qdiv|kdiv|pdiv|central");
    pl.add(pll_ctl(id), m.r(pll_ctl(id)) | PLL_ENABLE, "PLL enable (wait DPLL_STATUS lock <= 5 ms)");
    let c2 = (m.r(DPLL_CTRL2) & !(1 << (port + 15)) & !(7 << (port * 3))) | id << (port * 3 + 1) | 1 << (port * 3);
    pl.add(DPLL_CTRL2, c2, format!("DPLL_CTRL2 clear DDI_CLK_OFF({}), DDI_CLK_SEL={} | SEL_OVERRIDE", port, id));
    // 4: buffer translation, HDMI slot 9
    let (idx, en) = buf_trans::hdmi_entry(None);
    pl.add(buf_trans::lo_reg(port, buf_trans::HDMI_HW_ENTRY), en.lo, format!("DDI_BUF_TRANS_LO slot 9 = skl_trans_hdmi[{}]", idx));
    pl.add(buf_trans::hi_reg(port, buf_trans::HDMI_HW_ENTRY), en.hi, "DDI_BUF_TRANS_HI slot 9");
    pl.add(buf_trans::DISPIO_CR_TX_BMU_CR0, buf_trans::iboost_value(m.r(buf_trans::DISPIO_CR_TX_BMU_CR0), port, en.iboost),
           format!("DISPIO_CR_TX_BMU_CR0 iboost {}", en.iboost));
    // 5-7: clock select, timings, linetime
    pl.add(0x46144, (port + 1) << 29, "TRANS_CLK_SEL_B");
    let t = |a: u32, v: u32| (a, v);
    for (a, v, n) in [
        t(b(reg::TRANS_HTOTAL_A), (mode.ht - 1) << 16 | (mode.hd - 1)), t(b(reg::TRANS_HTOTAL_A) + 4, (mode.ht - 1) << 16 | (mode.hd - 1)),
        t(b(reg::TRANS_HTOTAL_A) + 8, (mode.hse - 1) << 16 | (mode.hss - 1)), t(b(reg::TRANS_HTOTAL_A) + 0xC, (mode.vt - 1) << 16 | (mode.vd - 1)),
        t(b(reg::TRANS_HTOTAL_A) + 0x10, (mode.vt - 1) << 16 | (mode.vd - 1)), t(b(reg::TRANS_HTOTAL_A) + 0x14, (mode.vse - 1) << 16 | (mode.vss - 1)),
        t(b(reg::PIPESRC_A), (mode.hd - 1) << 16 | (mode.vd - 1)),
    ].into_iter().zip(["HTOTAL_B", "HBLANK_B", "HSYNC_B", "VTOTAL_B", "VBLANK_B", "VSYNC_B", "PIPESRC_B"]).map(|((a, v), n)| (a, v, n)) {
        pl.add(a, v, n);
    }
    pl.add(0x45274, (mode.ht * 1000 * 8 + mode.clock / 2) / mode.clock, "WM_LINETIME_B (1/8 us, DIV_ROUND_CLOSEST)");
    // 8-10: transcoder, port, pipe
    pl.add(b(reg::TRANS_DDI_FUNC_CTL_A), 1 << 31 | port << 28 | 1 << 24 | 1 << 17 | 1 << 16, "TRANS_DDI_FUNC_CTL_B enable, port, DVI, +vsync +hsync, 8 bpc");
    pl.add(reg::port(reg::DDI_BUF_CTL_A, port), m.r(reg::port(reg::DDI_BUF_CTL_A, port)) | 1 << 31, "DDI_BUF_CTL enable");
    pl.add(b(reg::TRANSCONF_A), 1 << 31, "TRANSCONF_B enable (wait state bit 30 <= 1 ms)");
    // 11: plane 1B
    let (ps, pe) = ddb_b.0;
    let (cs, ce) = ddb_b.1;
    pl.add(b(reg::PLANE_BUF_CFG_1_A), ddb_entry(ps, pe), format!("PLANE_BUF_CFG_1_B [{},{})", ps, pe));
    pl.add(b(0x7017C), ddb_entry(cs, ce), format!("CUR_BUF_CFG_B [{},{})", cs, ce));
    if let Some(w) = wm {
        for i in 0..8 { pl.add(b(0x70240) + 4 * i as u32, w[i], format!("PLANE_WM_1_B level {}", i)); }
        pl.add(b(0x70268), w[8], "PLANE_WM_TRANS_1_B");
    } else {
        pl.add(b(0x70240), 0, "PLANE_WM_1_B: pcode latencies unavailable, not computed");
    }
    pl.add(b(reg::PLANE_STRIDE_1_A), mode.hd * 4 / 64, "PLANE_STRIDE_1_B (64 B units, linear)");
    pl.add(b(reg::PLANE_POS_1_A), 0, "PLANE_POS_1_B");
    pl.add(b(reg::PLANE_SIZE_1_A), (mode.vd - 1) << 16 | (mode.hd - 1), "PLANE_SIZE_1_B");
    pl.add(b(reg::PLANE_OFFSET_1_A), 0, "PLANE_OFFSET_1_B");
    pl.add(b(reg::PLANE_CTL_1_A), 1 << 31 | 0x4 << 24, "PLANE_CTL_1_B enable, XRGB8888, linear");
    pl.add(b(reg::PLANE_SURF_1_A), 0, "PLANE_SURF_1_B = GGTT address of the test fb (arms)");
    // 12: interrupts
    pl.add(0x44414, !0x9, "DE_PIPE_B_IMR unmask vblank + flip done");
    pl.add(0x4441C, 0x9, "DE_PIPE_B_IER vblank + flip done");
    pl
}

fn dryrun(m: &Mmio) {
    let port = (opt("i915.pipeb_port").unwrap_or(1) as u32).clamp(1, 2);
    let id = opt("i915.pipeb_dpll").unwrap_or(1) as u32;
    let mode = &M1080;
    let Some(p) = wrpll::calculate(mode.clock, wrpll::REF_KHZ) else { ilog!("pipeb: dryrun: no WRPLL divider"); return };
    let wa = (m.r(reg::pipe(reg::PIPESRC_A, 0)) >> 16 & 0x1FFF) + 1;
    let Some(((ps, pe), (cs, ce))) = skl_ddb_plan(super::kms::DDB_SIZE, &[wa, mode.hd], 1, 0) else { ilog!("pipeb: dryrun: no DDB plan"); return };
    let a = skl_ddb_plan(super::kms::DDB_SIZE, &[wa, mode.hd], 0, 0).map(|(pl, c)| (pl.0, c.1)).unwrap_or((0, 0));
    let wm = super::kms::skl_wm_latency(m).map(|lat| super::kms::skl_plane_wm(&lat, mode.clock as u64, mode.ht, mode.hd, pe - ps));
    // 0: power
    let io = Dom::ddi_io(port);
    let up = power::get(m, Dom::Pw2) & io.map_or(true, |d| power::get(m, d));
    power::report(m, "dryrun, wells taken");
    ilog!("pipeb: dryrun {}x{} {} kHz DDI-{} DPLL{}: WRPLL dco {} kHz p {} (p0 {} p1 {} p2 {}); wells up {}; DDB A [{},{}) B [{},{}) (cursor B [{},{})); WM B lvl0 {:08x}",
          mode.hd, mode.vd, mode.clock, (b'A' + port as u8) as char, id, p.dco_khz, p.p, p.p0, p.p1, p.p2, up, a.0, a.1, ps, ce, cs, ce,
          wm.map_or(0, |w| w[0]));
    for (r, v, what) in build_plan(m, mode, port, id, &p, ((ps, pe), (cs, ce)), wm).0 {
        ilog!("pipeb: plan  0x{:05x} <- 0x{:08x}  {}", r, v, what);
    }
    let lock = pll_try(m, id, mode.clock);
    if let Some(d) = io { power::put(m, d); }
    power::put(m, Dom::Pw2);
    power::report(m, "dryrun, end");
    ilog!("pipeb: dryrun done: DPLL{} {}; power: PW2 refs {} at end", id, if lock.is_some() { "locked" } else { "not locked" }, power::refs(Dom::Pw2));
}
