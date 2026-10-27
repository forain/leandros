//! Raspberry Pi 5 (BCM2712) SoC housekeeping: hardware RNG, firmware RTC,
//! PM watchdog, SoC temperature and ARM clock scaling. Every probe is inert
//! on failure: one greppable line, then the boot goes on.
//!
//! Boot lines (one each):
//!   `[rng] iproc-rng200 ...`   entropy source registered with `sched::random`
//!   `[RTC] rpi ...`            firmware RTC → CLOCK_REALTIME
//!   `[wdt] bcm2712-pm ...`     watchdog (opt-in `pi.wdt=<1..15 s>`)
//!   `[thermal] ...`            AVS monitor (firmware GET_TEMPERATURE fallback)
//!   `[cpufreq] rpi ...`        ARM clock min/cur/max + governor (`pi.cpufreq=0` off,
//!                              `pi.cpufreq=max` pins the max rate)
//!
//! Firmware property calls after boot go through [`fw_tag`], which serialises
//! them on one lock (the property buffer is a single static).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use leandros_lib::mmio::{MmioRd, MmioWr};
use spin::Mutex;

use crate::pci::serial_debug;
use crate::rpi_misc_logic::{avs, fw, rng200, Buf, Governor, Regs32};

const RNG_PHYS: u64 = 0x10_7D20_8000;
const AVS_PHYS: u64 = 0x10_7D54_2000;

static HHDM: AtomicUsize = AtomicUsize::new(0);
static MBOX_READY: AtomicBool = AtomicBool::new(false);
static FW_LOCK: Mutex<()> = Mutex::new(());
static RNG_VIRT: AtomicUsize = AtomicUsize::new(0);
pub(crate) static AVS_VIRT: AtomicUsize = AtomicUsize::new(0);
static RTC_OK: AtomicBool = AtomicBool::new(false);

static CPU_MIN: AtomicU32 = AtomicU32::new(0);
static CPU_MAX: AtomicU32 = AtomicU32::new(0);
static CPU_CUR: AtomicU32 = AtomicU32::new(0);
static CPU_SETS: AtomicU32 = AtomicU32::new(0);
/// 0 = no driver, 1 = ondemand, 2 = fixed (governor off / pinned).
static GOV_MODE: AtomicU32 = AtomicU32::new(0);
static WDT_SECS: AtomicU32 = AtomicU32::new(0);

struct Win(usize);
impl Regs32 for Win {
    fn r(&mut self, off: usize) -> u32 { unsafe { ((self.0 + off) as *const u32).mmio_read() } }
    fn w(&mut self, off: usize, v: u32) { unsafe { ((self.0 + off) as *mut u32).mmio_write(v) } }
}

fn map(phys: u64, len: usize) -> Option<usize> {
    mm::kva::ioremap(phys, len, mm::kva::CacheMode::Uncached)
}

fn opt<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace().find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
}

fn line(f: impl FnOnce(&mut Buf)) {
    let mut b = [0u8; 160];
    let mut w = Buf::new(&mut b);
    f(&mut w);
    w.s("\n");
    let n = w.n;
    serial_debug(core::str::from_utf8(&b[..n]).unwrap_or("?\n"));
}

// ── Firmware mailbox, post-boot ──────────────────────────────────────────────

/// One single-tag property call, serialised. None: mailbox unavailable, the
/// call timed out, or the firmware did not handle the tag.
pub fn fw_tag(tag: u32, vals: &[u32]) -> Option<[u32; fw::VAL_WORDS]> {
    if !MBOX_READY.load(Ordering::Acquire) { return None; }
    let mut msg = [0u32; fw::MSG_WORDS];
    fw::build(tag, vals, &mut msg);
    let _g = FW_LOCK.lock();
    let r = unsafe { crate::rpi_mailbox::property_call(HHDM.load(Ordering::Relaxed), &msg) }.ok()?;
    fw::parse(r)
}

// ── RNG ──────────────────────────────────────────────────────────────────────

fn rng_source(out: &mut [u8]) -> usize {
    match RNG_VIRT.load(Ordering::Acquire) {
        0 => 0,
        v => rng200::read(&mut Win(v), out),
    }
}

fn rng_init() {
    let Some(v) = map(RNG_PHYS, 0x1000) else { return serial_debug("[rng] iproc-rng200 fail map\n") };
    let mut r = Win(v);
    rng200::restart(&mut r);
    // Warm-up: the first word arrives after the startup tests (~ms).
    let t0 = sched::monotonic_ns();
    while rng200::fifo_count(&mut r) == 0 && sched::monotonic_ns() - t0 < 100_000_000 {
        core::hint::spin_loop();
    }
    let mut seed = [0u8; 32];
    let n = rng200::read(&mut r, &mut seed);
    let distinct = n == 32 && seed.chunks(4).any(|c| c != &seed[..4]);
    if !distinct {
        return line(|b| { b.s("[rng] iproc-rng200 fail no data (got ").u(n as u64).s(" bytes)"); });
    }
    RNG_VIRT.store(v, Ordering::Release);
    sched::random::add_boot_seed("rng200", &seed);
    sched::random::register_source("rng200", rng_source);
    for x in seed.iter_mut() { unsafe { core::ptr::write_volatile(x, 0) } }
    line(|b| { b.s("[rng] iproc-rng200 ok fifo=").u(rng200::fifo_count(&mut r) as u64).s(" (entropy source registered)"); });
}

// ── RTC ──────────────────────────────────────────────────────────────────────

fn rtc_init() {
    match fw_tag(fw::GET_RTC_REG, &[fw::RTC_TIME, 0]) {
        None => serial_debug("[RTC] rpi: no firmware RTC; CLOCK_REALTIME starts at boot\n"),
        Some(v) if !fw::rtc_plausible(v[1]) => {
            RTC_OK.store(true, Ordering::Relaxed);
            line(|b| { b.s("[RTC] rpi: clock unset (").u(v[1] as u64).s(" s); set it with date -s"); });
        }
        Some(v) => {
            RTC_OK.store(true, Ordering::Relaxed);
            sched::set_realtime_ns((v[1] as u64).saturating_mul(1_000_000_000));
            line(|b| { b.s("[RTC] rpi: epoch seconds: ").u(v[1] as u64); });
        }
    }
}

/// Write the wall clock back to the firmware RTC (clock_settime/settimeofday).
pub fn rtc_set_secs(secs: u64) {
    if !RTC_OK.load(Ordering::Relaxed) || secs > u32::MAX as u64 { return; }
    if fw_tag(fw::SET_RTC_REG, &[fw::RTC_TIME, secs as u32]).is_none() {
        serial_debug("[RTC] rpi: set failed\n");
    }
}

// ── Thermal ──────────────────────────────────────────────────────────────────

/// SoC temperature in millidegrees Celsius.
pub fn soc_millicelsius() -> Option<i32> {
    if let v @ 1.. = AVS_VIRT.load(Ordering::Acquire) {
        if let Some(t) = avs::millicelsius(Win(v).r(avs::TEMP_STATUS)) { return Some(t); }
    }
    fw_tag(fw::GET_TEMPERATURE, &[0, 0]).map(|v| v[1] as i32)
}

fn thermal_init() {
    let src = match map(AVS_PHYS, 0x1000) {
        Some(v) if avs::millicelsius(Win(v).r(avs::TEMP_STATUS)).is_some() => {
            AVS_VIRT.store(v, Ordering::Release);
            "avs"
        }
        _ => "firmware",
    };
    match soc_millicelsius() {
        Some(t) => line(|b| { b.s("[thermal] soc ").mc(t).s(" C (").s(src).s(")"); }),
        None => serial_debug("[thermal] fail no sensor\n"),
    }
}

// ── cpufreq ──────────────────────────────────────────────────────────────────

fn set_arm_hz(hz: u32) -> bool {
    match fw_tag(fw::SET_CLOCK_RATE, &[fw::CLK_ARM, hz, 0]) {
        Some(v) => {
            CPU_CUR.store(v[1], Ordering::Relaxed);
            CPU_SETS.fetch_add(1, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

fn cpufreq_init(cmdline: &str) {
    let get = |t| fw_tag(t, &[fw::CLK_ARM, 0]).map(|v| v[1]).filter(|&hz| hz != 0);
    let (Some(min), Some(cur), Some(max)) =
        (get(fw::GET_MIN_CLOCK_RATE), get(fw::GET_CLOCK_RATE), get(fw::GET_MAX_CLOCK_RATE))
    else {
        return serial_debug("[cpufreq] rpi fail firmware clock tags\n");
    };
    CPU_MIN.store(min, Ordering::Relaxed);
    CPU_MAX.store(max, Ordering::Relaxed);
    CPU_CUR.store(cur, Ordering::Relaxed);
    let mode = opt(cmdline, "pi.cpufreq").unwrap_or("ondemand");
    let gov = match mode {
        "0" | "off" => "off",
        "max" => { set_arm_hz(max); "max" }
        _ => "ondemand",
    };
    GOV_MODE.store(if gov == "ondemand" { 1 } else { 2 }, Ordering::Relaxed);
    line(|b| {
        b.s("[cpufreq] rpi arm min=").u((min / 1_000_000) as u64).s(" cur=")
            .u((CPU_CUR.load(Ordering::Relaxed) / 1_000_000) as u64).s(" max=")
            .u((max / 1_000_000) as u64).s(" MHz governor=").s(gov);
    });
}

/// Housekeeping thread: governor samples every 100 ms, watchdog kicks.
fn housekeeping() {
    let mut g = Governor::new(CPU_MIN.load(Ordering::Relaxed), CPU_MAX.load(Ordering::Relaxed),
                              CPU_CUR.load(Ordering::Relaxed));
    loop {
        sched::kthread::msleep(100);
        let wdt = WDT_SECS.load(Ordering::Relaxed);
        if wdt != 0 { crate::rpi_pm::wdt_kick(wdt); }
        if GOV_MODE.load(Ordering::Relaxed) == 1 {
            let busy = crate::rpi_misc_logic::busy_from_nr_running(sched::nr_running());
            if let Some(hz) = g.sample(busy) {
                if !set_arm_hz(hz) { g.cur_hz = CPU_CUR.load(Ordering::Relaxed); }
            }
        }
    }
}

/// `/proc/cpufreq` body on the Pi.
pub fn cpufreq_report(out: &mut [u8]) -> usize {
    let mut b = Buf::new(out);
    match GOV_MODE.load(Ordering::Relaxed) {
        0 => { b.s("cpufreq: no driver\n"); }
        m => {
            b.s("driver: raspberrypi-firmware\ngovernor: ").s(if m == 1 { "ondemand" } else { "fixed" })
                .s("\ncpuinfo_min_freq: ").u((CPU_MIN.load(Ordering::Relaxed) / 1000) as u64)
                .s(" kHz\ncpuinfo_max_freq: ").u((CPU_MAX.load(Ordering::Relaxed) / 1000) as u64)
                .s(" kHz\nscaling_cur_freq: ").u((CPU_CUR.load(Ordering::Relaxed) / 1000) as u64)
                .s(" kHz\nrate_changes: ").u(CPU_SETS.load(Ordering::Relaxed) as u64).s("\n");
        }
    }
    b.n
}

// ── Watchdog ─────────────────────────────────────────────────────────────────

fn wdt_init(cmdline: &str) {
    let Some(_) = crate::rpi_pm::base() else { return serial_debug("[wdt] bcm2712-pm fail map\n") };
    let secs = opt(cmdline, "pi.wdt").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0)
        .min(crate::rpi_misc_logic::pm::MAX_SECS);
    let was = crate::rpi_pm::with(|r| crate::rpi_misc_logic::pm::is_running(r)).unwrap_or(false);
    if secs == 0 {
        if was { crate::rpi_pm::wdt_stop(); }
        return line(|b| { b.s("[wdt] bcm2712-pm ready (off; pi.wdt=<1..15> arms it)").s(if was { ", firmware watchdog stopped" } else { "" }); });
    }
    crate::rpi_pm::wdt_start(secs);
    WDT_SECS.store(secs, Ordering::Relaxed);
    line(|b| { b.s("[wdt] bcm2712-pm armed ").u(secs as u64).s(" s, kicked every 100 ms"); });
}

// ── Boot ─────────────────────────────────────────────────────────────────────

/// Probe everything. Runs once on the boot CPU after `mm` is up (ioremap).
pub fn boot(cmdline: &str, hhdm_offset: u64) {
    HHDM.store(hhdm_offset as usize, Ordering::Relaxed);
    match map(crate::rpi_mailbox::MBOX_BASE_PHYS as u64, 0x40) {
        Some(m) => {
            crate::rpi_mailbox::set_mbox_virt(m);
            MBOX_READY.store(true, Ordering::Release);
        }
        None => serial_debug("[pimisc] fail map mailbox\n"),
    }
    rng_init();
    rtc_init();
    wdt_init(cmdline);
    thermal_init();
    cpufreq_init(cmdline);
    let want_thread = GOV_MODE.load(Ordering::Relaxed) == 1 || WDT_SECS.load(Ordering::Relaxed) != 0;
    if want_thread && sched::kthread::spawn("pi-house", housekeeping).is_none() {
        serial_debug("[cpufreq] rpi fail kthread\n");
        if WDT_SECS.load(Ordering::Relaxed) != 0 { crate::rpi_pm::wdt_stop(); }
    }
}
