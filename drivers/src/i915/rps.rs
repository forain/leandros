//! GT frequency (RPS) on gen9, and the activity counters that judge it.
//!
//! Nothing else asks the GT for a frequency: with RP_CONTROL off and no
//! RPNSWREQ written the GT idles at ~100-300 MHz however busy it is (the
//! laptop's boot log: RPSTAT1 CAGF 100 MHz against an RP0 of 1100 MHz).
//!
//! `enable` (exec bring-up, forcewake held for good) follows Linux's
//! `gen9_rps_enable` + `rps_reset` + `rps_set_power`: RC_VIDEO_FREQ = RP1,
//! idle hysteresis 10, the LOW_POWER evaluation intervals/thresholds,
//! RP_CONTROL enabled with busy/idle averaging, a frequency request in
//! RPNSWREQ (gen9: 16.67 MHz units << 23) and the matching
//! RP_INTERRUPT_LIMITS. The up/down interrupts stay masked (PMINTRMSK all
//! ones): the frequency is picked by a software governor (`hw::RpsGov`) run
//! from the exec submit path and poll thread — RP0 on the first submit after
//! idle, RPn after `i915.rps_idle_ms` (100) with no request queued or in flight.
//!
//! Knobs: `i915.rps=0` (registers untouched, as before), `i915.rps=max|min|<MHz>`
//! (one fixed request), default `auto` (the governor).
//!
//! Instrumentation: one `[i915] gt:` line every `i915.stat_s` (10) seconds
//! while there was GPU work or KMS flips, and `/proc/i915` (counters since
//! boot, `name value` lines).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};

use alloc::string::String;
use core::fmt::Write;

use super::hw::{self, reg, RpsGov, RpsMode};
use super::{delay_us, ilog, now_ns, opt, word, Mmio};

/// MMIO base once exec is up (forcewake held from then on): 0 = no reads.
static MMIO_VA: AtomicUsize = AtomicUsize::new(0);
/// RP0, RP1, RPn in gen9 units.
static RP: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
/// 0 untouched (`i915.rps=0`), 1 fixed, 2 auto.
static MODE: AtomicU32 = AtomicU32::new(0);
/// The current request (gen9 units; 0 = none written).
static REQ: AtomicU32 = AtomicU32::new(0);
static UPS: AtomicU64 = AtomicU64::new(0);
static DOWNS: AtomicU64 = AtomicU64::new(0);

// ── counters since boot ──
/// User requests queued per engine (rcs0, bcs0, vcs0).
pub(super) static SUBMITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Per engine: ns from ELSP submission to the breadcrumb, summed.
pub(super) static BUSY_NS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// CAGF samples (MHz) taken by the poll thread while work was queued or in flight.
static CAGF_SUM: AtomicU64 = AtomicU64::new(0);
static CAGF_N: AtomicU64 = AtomicU64::new(0);
static FLIPS: AtomicU64 = AtomicU64::new(0);
static NOT_LATCHED: AtomicU64 = AtomicU64::new(0);
/// Present-time breakdown sums (ns): fence wait, clflush, latch.
static PRES_NS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Window maxima (ns), reset by each stat line.
static PRES_MAX: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static ENABLED: AtomicBool = AtomicBool::new(false);

fn mmio() -> Option<Mmio> {
    let va = MMIO_VA.load(Relaxed);
    if va == 0 { None } else { Some(Mmio { va }) }
}

fn cagf_mhz(m: &Mmio) -> u32 { hw::decode_cagf_mhz(m.r(reg::GEN6_RPSTAT1)) }

fn mhz(u: u32) -> u32 { hw::gen9_units_to_mhz(u) }

/// Bring RPS up (exec bring-up; forcewake held). Returns the governor the
/// exec core drives, or None when the frequency is left alone.
pub(super) fn enable(m: &Mmio) -> Option<RpsGov> {
    let cap = m.r(reg::GEN6_RP_STATE_CAP);
    let (rp0, rp1, rpn) = hw::gen9_rp_caps_units(cap);
    for (a, v) in RP.iter().zip([rp0, rp1, rpn]) { a.store(v, Relaxed); }
    MMIO_VA.store(m.va, Relaxed);
    ENABLED.store(true, Relaxed);
    let mode = hw::parse_rps_mode(word("i915.rps"), rpn, rp0);
    let (ctl, sw, stat, lim, msk) = (m.r(reg::GEN6_RP_CONTROL), m.r(reg::GEN6_RPNSWREQ), m.r(reg::GEN6_RPSTAT1),
                                     m.r(reg::GEN6_RP_INTERRUPT_LIMITS), m.r(reg::GEN6_PMINTRMSK));
    ilog!("rps: as found: RP_CONTROL=0x{:08x} RPNSWREQ=0x{:08x} RP_INTERRUPT_LIMITS=0x{:08x} PMINTRMSK=0x{:08x} RPSTAT1=0x{:08x} (CAGF {} MHz); RP0 {} RP1 {} RPn {} MHz",
          ctl, sw, lim, msk, stat, hw::decode_cagf_mhz(stat), mhz(rp0), mhz(rp1), mhz(rpn));
    if mode == RpsMode::Off {
        log_off("i915.rps=0");
        return None;
    }
    if rp0 <= rpn || rpn == 0 {
        // Linux: no room for dynamic reclocking, RPS left disabled.
        log_off("RP0 <= RPn");
        return None;
    }
    let hz = hw::gen9_cs_timestamp_hz(m.r(reg::CTC_MODE), m.r(reg::GEN9_TIMESTAMP_OVERRIDE), false) as u64;
    // gen9_rps_enable
    m.w(reg::GEN6_RC_VIDEO_FREQ, hw::gen9_rpnswreq(rp1));
    m.w(reg::GEN6_RP_IDLE_HYSTERSIS, 0xA);
    // No PM interrupts: the governor is software (Linux masks all of them
    // while parked, GEN8_PMINTR_DISABLE_REDIRECT_TO_GUC included).
    m.w(reg::GEN6_PMINTRMSK, !0);
    // rps_reset -> rps_set(min): gen6_rps_set, then gen6_rps_set_thresholds
    // (rps_set_power(LOW_POWER) and the interrupt limits).
    m.w(reg::GEN6_RPNSWREQ, hw::gen9_rpnswreq(rpn));
    let [uei, uth, dei, dth] = hw::gen9_rp_thresholds(hz);
    m.w(reg::GEN6_RP_UP_EI, uei);
    m.w(reg::GEN6_RP_UP_THRESHOLD, uth);
    m.w(reg::GEN6_RP_DOWN_EI, dei);
    m.w(reg::GEN6_RP_DOWN_THRESHOLD, dth);
    m.w(reg::GEN6_RP_CONTROL, hw::gen9_rp_control());
    m.w(reg::GEN6_RP_INTERRUPT_LIMITS, hw::gen9_rp_limits(rp0, rpn, rpn));
    delay_us(1000);
    let at_min = cagf_mhz(m);
    // Check the request is honoured at the top too (the governor's busy level).
    m.w(reg::GEN6_RPNSWREQ, hw::gen9_rpnswreq(rp0));
    m.w(reg::GEN6_RP_INTERRUPT_LIMITS, hw::gen9_rp_limits(rp0, rpn, rp0));
    delay_us(1000);
    let at_max = cagf_mhz(m);
    let idle_ms = opt("i915.rps_idle_ms").unwrap_or(100).clamp(5, 10_000);
    let mut g = RpsGov::new(mode, rpn, rp0, idle_ms * 1_000_000);
    g.cur = rp0;
    if let Some(f) = g.start(now_ns()) { write_req(m, &g, f); } else { REQ.store(rp0, Relaxed); }
    MODE.store(if let RpsMode::Fixed(_) = mode { 1 } else { 2 }, Relaxed);
    ilog!("rps: enabled (software governor, PM interrupts masked): RP_CONTROL=0x{:08x} UP_EI/TH={}/{} DOWN_EI/TH={}/{} (CS {} Hz); \
           request RPn {} MHz -> CAGF {} MHz after 1 ms, RP0 {} MHz -> CAGF {} MHz after 1 ms; mode {} (request now {} MHz{})",
          m.r(reg::GEN6_RP_CONTROL), uei, uth, dei, dth, hz, mhz(rpn), at_min, mhz(rp0), at_max, mode_name(), mhz(g.cur),
          if mode == RpsMode::Auto { alloc::format!(", RPn after {} ms idle", idle_ms) } else { String::new() });
    Some(g)
}

fn log_off(why: &str) {
    MODE.store(0, Relaxed);
    ilog!("rps: {}: GT frequency registers left as found (no RPNSWREQ request)", why);
}

fn mode_name() -> &'static str {
    match MODE.load(Relaxed) { 0 => "off", 1 => "fixed", _ => "auto" }
}

/// Write request `f` (gen9 units): RPNSWREQ, then the interrupt limits (`rps_set`).
pub(super) fn write_req(m: &Mmio, g: &RpsGov, f: u32) {
    m.w(reg::GEN6_RPNSWREQ, hw::gen9_rpnswreq(f));
    m.w(reg::GEN6_RP_INTERRUPT_LIMITS, hw::gen9_rp_limits(g.max, g.min, f));
    REQ.store(f, Relaxed);
    UPS.store(g.ups, Relaxed);
    DOWNS.store(g.downs, Relaxed);
}

/// Poll thread, while work is queued or in flight: one CAGF sample.
pub(super) fn sample_busy() {
    if let Some(m) = mmio() {
        CAGF_SUM.fetch_add(cagf_mhz(&m) as u64, Relaxed);
        CAGF_N.fetch_add(1, Relaxed);
    }
}

/// One KMS present (kms.rs): fence wait t0..t1, clflush t1..t2, latch t2..t3.
pub(super) fn flip_stat(t0: u64, t1: u64, t2: u64, t3: u64, latched: bool) {
    FLIPS.fetch_add(1, Relaxed);
    if !latched { NOT_LATCHED.fetch_add(1, Relaxed); }
    for (i, d) in [t1.wrapping_sub(t0), t2.wrapping_sub(t1), t3.wrapping_sub(t2)].into_iter().enumerate() {
        PRES_NS[i].fetch_add(d, Relaxed);
        PRES_MAX[i].fetch_max(d, Relaxed);
    }
}

#[derive(Clone, Copy, Default)]
struct Win { ns: u64, sub: [u64; 3], busy: [u64; 3], flips: u64, nl: u64, pres: [u64; 3], cs: u64, cn: u64, ups: u64, downs: u64 }

fn now_win(ns: u64) -> Win {
    Win {
        ns,
        sub: [SUBMITS[0].load(Relaxed), SUBMITS[1].load(Relaxed), SUBMITS[2].load(Relaxed)],
        busy: [BUSY_NS[0].load(Relaxed), BUSY_NS[1].load(Relaxed), BUSY_NS[2].load(Relaxed)],
        flips: FLIPS.load(Relaxed), nl: NOT_LATCHED.load(Relaxed),
        pres: [PRES_NS[0].load(Relaxed), PRES_NS[1].load(Relaxed), PRES_NS[2].load(Relaxed)],
        cs: CAGF_SUM.load(Relaxed), cn: CAGF_N.load(Relaxed), ups: UPS.load(Relaxed), downs: DOWNS.load(Relaxed),
    }
}

static LAST: spin::Mutex<Option<Win>> = spin::Mutex::new(None);

/// `x` per second over `dt_ns`, with one decimal (tenths).
fn per_s10(x: u64, dt_ns: u64) -> u64 { x.saturating_mul(10_000_000_000) / dt_ns.max(1) }

/// Poll thread: one `[i915] gt:` line every `i915.stat_s` seconds, only when
/// there were requests or flips in the window.
pub(super) fn stat_tick() {
    let period = opt("i915.stat_s").unwrap_or(10);
    if period == 0 || !ENABLED.load(Relaxed) { return; }
    let now = now_ns();
    let mut last = LAST.lock();
    let Some(p) = *last else { *last = Some(now_win(now)); return };
    let dt = now.wrapping_sub(p.ns);
    if dt < period * 1_000_000_000 { return; }
    let c = now_win(now);
    *last = Some(c);
    drop(last);
    let maxes = [PRES_MAX[0].swap(0, Relaxed), PRES_MAX[1].swap(0, Relaxed), PRES_MAX[2].swap(0, Relaxed)];
    let subs = (c.sub[0] - p.sub[0]) + (c.sub[1] - p.sub[1]) + (c.sub[2] - p.sub[2]);
    let flips = c.flips - p.flips;
    if subs == 0 && flips == 0 { return; }
    let mut s = String::new();
    let cagf_now = mmio().map_or(0, |m| cagf_mhz(&m));
    let cn = c.cn - p.cn;
    let _ = write!(s, "gt: cagf {} MHz (avg while busy {} MHz) req {} MHz ({}, +{} up/+{} down) | exec ",
                   cagf_now, if cn == 0 { 0 } else { (c.cs - p.cs) / cn }, mhz(REQ.load(Relaxed)), mode_name(),
                   c.ups - p.ups, c.downs - p.downs);
    let t = |x: u64| { let v = per_s10(x, dt); alloc::format!("{}.{}", v / 10, v % 10) };
    let _ = write!(s, "rcs {}/s bcs {}/s vcs {}/s, busy rcs {} bcs {} vcs {} ms/s | flips {}/s",
                   t(c.sub[0] - p.sub[0]), t(c.sub[1] - p.sub[1]), t(c.sub[2] - p.sub[2]),
                   t((c.busy[0] - p.busy[0]) / 1_000_000), t((c.busy[1] - p.busy[1]) / 1_000_000),
                   t((c.busy[2] - p.busy[2]) / 1_000_000), t(flips));
    if flips != 0 {
        let avg = |i: usize| (c.pres[i] - p.pres[i]) / flips / 1000;
        let _ = write!(s, ": fence {}/{} clflush {}/{} latch {}/{} us avg/max, not latched {}",
                       avg(0), maxes[0] / 1000, avg(1), maxes[1] / 1000, avg(2), maxes[2] / 1000, c.nl - p.nl);
    }
    ilog!("{}", s);
}

/// `/proc/i915`: counters since boot, `name value` per line, then the
/// uapi counters and its error ring (`uapi_err ...`), which are there even
/// before (or without) the exec bring-up.
pub fn proc_text() -> Option<String> {
    let mut s = String::new();
    let gt = proc_gt(&mut s).is_some();
    // KMS flush / CPU-mapping counters: there in the fake (QEMU) mode too.
    let kms = [super::kms::FLUSHED_FLIPS.load(Relaxed) as u64, super::uapi::MMAP_BY_TYPE[0].load(Relaxed),
               super::uapi::MMAP_BY_TYPE[1].load(Relaxed), super::uapi::MMAP_BY_TYPE[2].load(Relaxed)];
    let kms_any = gt || kms.iter().any(|&v| v != 0);
    if kms_any {
        for (k, v) in ["flips_clflushed", "mmap_wb", "mmap_wc", "mmap_uc"].iter().zip(kms) {
            let _ = writeln!(s, "{} {}", k, v);
        }
    }
    // Live display-buffer allocation (raw BUF_CFG values: start | end << 16).
    if let Some((pl, cur)) = super::kms::ddb_regs() {
        let _ = writeln!(s, "ddb_plane {}\nddb_cursor {}", pl, cur);
    }
    let uapi = super::uapi::proc_errors(&mut s);
    let prime = crate::kenv::drm::proc_prime(&mut s);
    if gt || kms_any || uapi || prime { Some(s) } else { None }
}

fn proc_gt(s: &mut String) -> Option<()> {
    if !ENABLED.load(Relaxed) { return None; }
    let m = mmio()?;
    let mut put = |k: &str, v: u64| { let _ = writeln!(s, "{} {}", k, v); };
    let rp = [RP[0].load(Relaxed), RP[1].load(Relaxed), RP[2].load(Relaxed)];
    put("rps_mode", MODE.load(Relaxed) as u64);
    put("rp0_mhz", mhz(rp[0]) as u64);
    put("rp1_mhz", mhz(rp[1]) as u64);
    put("rpn_mhz", mhz(rp[2]) as u64);
    put("req_mhz", mhz(REQ.load(Relaxed)) as u64);
    put("cagf_mhz", cagf_mhz(&m) as u64);
    let (cs, cn) = (CAGF_SUM.load(Relaxed), CAGF_N.load(Relaxed));
    put("cagf_busy_avg_mhz", if cn == 0 { 0 } else { cs / cn });
    put("rps_ups", UPS.load(Relaxed));
    put("rps_downs", DOWNS.load(Relaxed));
    put("rpnswreq", m.r(reg::GEN6_RPNSWREQ) as u64);
    put("rp_control", m.r(reg::GEN6_RP_CONTROL) as u64);
    put("exec_rcs", SUBMITS[0].load(Relaxed));
    put("exec_bcs", SUBMITS[1].load(Relaxed));
    put("exec_vcs", SUBMITS[2].load(Relaxed));
    put("busy_rcs_ms", BUSY_NS[0].load(Relaxed) / 1_000_000);
    put("busy_bcs_ms", BUSY_NS[1].load(Relaxed) / 1_000_000);
    put("busy_vcs_ms", BUSY_NS[2].load(Relaxed) / 1_000_000);
    put("flips", FLIPS.load(Relaxed));
    put("flips_not_latched", NOT_LATCHED.load(Relaxed));
    put("flip_fence_us", PRES_NS[0].load(Relaxed) / 1000);
    put("flip_clflush_us", PRES_NS[1].load(Relaxed) / 1000);
    put("fifo_underruns", super::kms::underruns() as u64);
    put("flip_latch_us", PRES_NS[2].load(Relaxed) / 1000);
    put("uptime_ms", now_ns() / 1_000_000);
    Some(())
}
