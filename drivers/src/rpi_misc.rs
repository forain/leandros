//! Raspberry Pi 5 (BCM2712) SoC housekeeping: hardware RNG, firmware RTC,
//! PM watchdog, SoC temperature and ARM clock scaling. Every probe is inert
//! on failure: one greppable line, then the boot goes on.
//!
//! Boot lines (one each):
//!   `[rng] iproc-rng200 ...`   entropy source registered with `sched::random`
//!   `[RTC] rpi ...`            firmware RTC → CLOCK_REALTIME
//!   `[WDT] bcm2712-pm ...`     wedge watchdog, on by default (`wdt.off=1`)
//!   `[thermal] ...`            AVS monitor (firmware GET_TEMPERATURE fallback)
//!   `[power] ...`              undervoltage/throttle changes (GET_THROTTLED, polled 2 s by
//!                              the board thread; `power.uvolt=off` disables; /proc/thermal `power:`)
//!   `[cpufreq] rpi ...`        ARM clock min/cur/max + governor (`pi.cpufreq=0` off,
//!                              `pi.cpufreq=max` pins the max rate)
//!
//! Firmware property calls after boot go through [`fw_tag`], which serialises
//! them on one lock (the property buffer is a single static).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use leandros_lib::mmio::{MmioRd, MmioWr};
use spin::Mutex;

use crate::pci::serial_debug;
use crate::rpi_misc_logic::{avs, fw, rng200, uvolt, Buf, Governor, Regs32};

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

/// Raw property call for `kapi_mbox_property`: serialised against `fw_tag`, reply copied out
/// under the lock. Err(-19): mailbox not up yet; Err(-22): oversize; Err(-5): firmware/timeout.
pub fn fw_raw(words: &[u32], out: &mut [u32]) -> Result<usize, i32> {
    if !MBOX_READY.load(Ordering::Acquire) { return Err(-19); }
    let _g = FW_LOCK.lock();
    match unsafe { crate::rpi_mailbox::property_call(HHDM.load(Ordering::Relaxed), words) } {
        Ok(r) => { let n = r.len().min(out.len()); out[..n].copy_from_slice(&r[..n]); Ok(n) }
        Err(crate::rpi_mailbox::MboxError::TooLong) => Err(-22),
        Err(_) => Err(-5),
    }
}

// ── Undervoltage monitor (GET_THROTTLED) ─────────────────────────────────────

static UVOLT: Mutex<uvolt::Monitor> = Mutex::new(uvolt::Monitor::new(false));

fn uvolt_init(cmdline: &str) {
    if uvolt::knob_off(opt(cmdline, "power.uvolt")) {
        *UVOLT.lock() = uvolt::Monitor::new(true);
        serial_debug("[power] undervoltage monitor off (power.uvolt=off)\n");
        return;
    }
    uvolt_poll();
}

/// One poll from the board thread (never from IRQ context). `fw_tag` is a
/// bounded mailbox call under the shared firmware lock; the monitor lock is
/// not held across it.
pub fn uvolt_poll() {
    if UVOLT.lock().disabled() { return; }
    let r = fw_tag(uvolt::GET_THROTTLED, &uvolt::REQUEST).map(|v| v[0]);
    let out = UVOLT.lock().feed(r, sched::monotonic_ns());
    if let Some((m, v)) = out {
        line(|b| uvolt::render(m, v, b));
    }
}

/// The `power:` line for `/proc/thermal`.
pub fn uvolt_report(b: &mut Buf) { UVOLT.lock().report(b); }

static TRIAL_BOOT: AtomicBool = AtomicBool::new(false);
static TRIAL_CLEARED: AtomicBool = AtomicBool::new(false);

/// This boot is a tryboot trial: a panic or watchdog reset must clear the
/// one-shot firmware reboot flag first (`clear_trial_flag_nonblocking`).
pub fn set_trial_boot(on: bool) { TRIAL_BOOT.store(on, Ordering::Release); }

/// SET_REBOOT_FLAGS 0, once, best effort, never blocking: skipped when the
/// mailbox lock is held (the panicking CPU may own it), and the mailbox call
/// itself is a bounded spin. Safe from panic and watchdog paths.
pub fn clear_trial_flag_nonblocking() {
    if !TRIAL_BOOT.load(Ordering::Acquire) || !MBOX_READY.load(Ordering::Acquire) { return; }
    if TRIAL_CLEARED.swap(true, Ordering::AcqRel) { return; }
    let mut msg = [0u32; fw::MSG_WORDS];
    fw::build(0x0003_8064, &[0], &mut msg);
    if let Some(_g) = FW_LOCK.try_lock() {
        let _ = unsafe { crate::rpi_mailbox::property_call(HHDM.load(Ordering::Relaxed), &msg) };
    }
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

/// Housekeeping thread: governor samples every 100 ms.
fn housekeeping() {
    let mut g = Governor::new(CPU_MIN.load(Ordering::Relaxed), CPU_MAX.load(Ordering::Relaxed),
                              CPU_CUR.load(Ordering::Relaxed));
    loop {
        sched::kthread::msleep(100);
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

// Wedge recovery (lane piwdt). The board is powered from a port that cannot
// be switched, so a wedged kernel must reset the board itself; the firmware
// then boots the card's kernel.img. The PM watchdog is armed with its 15 s
// maximum and re-fed from cpu0's tick watchdog scan (every 0.5 s, IRQ
// context) only while every busy online CPU took a timer tick within
// `wdt::STUCK_SECS`: a CPU stuck with IRQs masked (spinning on a lock) stops
// the feeding, and the board resets ~60 s into the stall. A stuck cpu0
// stops it too (no scan runs). `wdt.off=1` disables it.

static PM_OK: AtomicBool = AtomicBool::new(false);
/// The serial reset handler for the console UART's RX path, once the PM
/// block is known to exist (DTB node + mapped). None elsewhere.
pub fn serial_reset_hook() -> Option<fn(u8, bool, u64)> {
    PM_OK.load(Ordering::Acquire).then_some(serial_magic as fn(u8, bool, u64))
}
/// The PM block exists and is mapped (the restart path may write PM_RSTS).
pub fn pm_ok() -> bool { PM_OK.load(Ordering::Acquire) }

/// Log the one-shot boot-partition request this boot came from (PM_RSTS
/// partition field, set by `reboot N` on Linux or LeandrOS) and clear it, so
/// a later watchdog/panic reset boots the firmware default (autoboot.txt's
/// boot_partition on a dual-boot card), as a plain reboot does.
fn note_boot_partition() {
    use crate::rpi_misc_logic::pm;
    let Some(p) = crate::rpi_pm::with(|r| pm::partition_of(r.r(pm::RSTS))) else { return };
    if p == 0 || p == 63 { return; }
    line(|b| { b.s("[pifw] booted via reboot partition request ").x(p as u64).s(": cleared"); });
    let _ = crate::rpi_pm::with(|r| pm::set_partition(r, 0));
}
static WDT_FED: AtomicBool = AtomicBool::new(true);

fn wdt_feed_hook() {
    use crate::rpi_misc_logic::wdt;
    // After a kernel panic nothing may feed: the panicking CPU can still take
    // timer ticks (the panic loop leaves IRQs as they were), so the per-CPU
    // tick-age test alone would keep the board alive forever.
    if PANICKED.load(Ordering::Acquire) { return; }
    let n = sched::active_cpu_count();
    let stuck = wdt::stuck_cpu((1..n).map(|c| (c, sched::wdog_tick_age_secs(c), sched::cpu_idle(c))), wdt::STUCK_SECS);
    match stuck {
        None => {
            if crate::rpi_pm::wdt_kick_try(wdt::HW_SECS) && !WDT_FED.swap(true, Ordering::Relaxed) {
                serial_debug("[WDT] all CPUs ticking again: feeding resumed\n");
            }
        }
        Some((cpu, secs)) => {
            clear_trial_flag_nonblocking();
            if WDT_FED.swap(false, Ordering::Relaxed) {
                line(|b| { b.s("[WDT] cpu").u(cpu as u64).s(" stuck ").u(secs as u64).s("s: letting hardware watchdog reset"); });
            }
        }
    }
}

/// Serial reset matcher state. Only touched from `uart::rx_hw_byte`, which
/// runs under the UART's RX_LOCK, so accesses never overlap.
static mut MAGIC: crate::rpi_misc_logic::wdt::Magic = crate::rpi_misc_logic::wdt::Magic::new();

/// Every byte read from the console PL011 (any CPU, IRQs masked, no lock
/// taken here): 5x Ctrl-] within 2 s, or BREAK then 'b', resets the board.
pub fn serial_magic(b: u8, brk: bool, now_ms: u64) {
    let hit = unsafe { (*core::ptr::addr_of_mut!(MAGIC)).feed(b, brk, now_ms) };
    if !hit { return; }
    serial_debug("\n[WDT] serial reset sequence: full reset now\n");
    crate::rpi_pm::restart();
    loop { core::hint::spin_loop(); }
}

/// Prove the armed countdown runs: PM_WDOG must drop by ~655 ticks
/// (1/65536 s units) over 10 ms. A counter that does not move would make the
/// stall reset a no-op, so say so loudly.
fn wdt_countdown_check() {
    let Some(a) = crate::rpi_pm::wdt_ticks_nolock() else { return };
    let t0 = sched::monotonic_ns();
    while sched::monotonic_ns().wrapping_sub(t0) < 10_000_000 { core::hint::spin_loop(); }
    let us = sched::monotonic_ns().wrapping_sub(t0) / 1000;
    let b = crate::rpi_pm::wdt_ticks_nolock().unwrap_or(a);
    let rstc = crate::rpi_pm::with(|r| r.r(crate::rpi_misc_logic::pm::RSTC)).unwrap_or(0);
    let d = a.wrapping_sub(b) & crate::rpi_misc_logic::pm::WDOG_TIME_SET;
    line(|l| {
        l.s("[WDT] countdown ").s(if d > 0 && d < 0x10000 { "ok" } else { "NOT RUNNING" }).s(": wdog ").x(a as u64)
            .s(" -> ").x(b as u64).s(" in ").u(us).s(" us (expect ~").u(us * 65536 / 1_000_000).s(" ticks), rstc ").x(rstc as u64);
    });
}

fn wdt_init(cmdline: &str) {
    use crate::rpi_misc_logic::wdt;
    let dt_pm = unsafe { boot::device_tree::recorded_dtb() }.map(wdt::dtb_has_pm).unwrap_or(false);
    // After a kexec the DTB is our own minimal FDT (no PM node). If the SoC id
    // registers decode as BCM2712, use the fixed PM block, but only after its
    // registers read back plausibly; anything else stays inert.
    let chip = !dt_pm && crate::bcm2712_id::stepping().source == crate::bcm2712_id::Source::Hw;
    if !dt_pm && !chip { return serial_debug("[WDT] no bcm2712-pm node in the DTB and no BCM2712 chip id: watchdog off\n"); }
    let Some(_) = crate::rpi_pm::base() else { return serial_debug("[WDT] bcm2712-pm fail map\n") };
    if chip {
        let (rstc, rsts, wdog) = crate::rpi_pm::with(|r| (r.r(crate::rpi_misc_logic::pm::RSTC),
            r.r(crate::rpi_misc_logic::pm::RSTS), r.r(crate::rpi_misc_logic::pm::WDOG))).unwrap_or((u32::MAX, u32::MAX, u32::MAX));
        if !wdt::pm_regs_plausible(rstc, rsts, wdog) {
            return line(|b| { b.s("[WDT] BCM2712 chip id but PM regs implausible rstc=").x(rstc as u64).s(" rsts=").x(rsts as u64).s(" wdog=").x(wdog as u64).s(": watchdog off"); });
        }
        serial_debug("[WDT] no PM node in the DTB (kexec FDT): BCM2712 chip id, PM regs sane, using 0x107d200000\n");
    }
    match opt(cmdline, "panic.reset").map(|v| v.parse::<u32>()) {
        Some(Ok(0)) => PANIC_RESET_SECS.store(u32::MAX, Ordering::Relaxed),
        Some(Ok(n)) => PANIC_RESET_SECS.store(n.min(3600), Ordering::Relaxed),
        _ => {}
    }
    PM_OK.store(true, Ordering::Release);
    note_boot_partition();
    let was = crate::rpi_pm::with(|r| crate::rpi_misc_logic::pm::is_running(r)).unwrap_or(false);
    if opt(cmdline, "wdt.off") == Some("1") {
        if was { crate::rpi_pm::wdt_stop(); }
        return line(|b| { b.s("[WDT] off (wdt.off=1)").s(if was { ", running watchdog stopped" } else { "" }); });
    }
    crate::rpi_pm::wdt_start(wdt::HW_SECS);
    sched::set_hw_watchdog_hook(wdt_feed_hook);
    wdt_countdown_check();
    line(|b| {
        b.s("[WDT] bcm2712-pm armed ").u(wdt::HW_SECS as u64).s(" s, fed while all CPUs tick; reset ~")
            .u((wdt::STUCK_SECS + wdt::HW_SECS) as u64).s(" s into a stall (wdt.off=1 disables); serial reset: 5x Ctrl-] or BREAK b");
    });
}

// ── Panic reset (lane piwdtpanic) ────────────────────────────────────────────
//
// The kernel panic handler (and the EL1 fault halts) call `panic_begin` first
// and `panic_end` once the message is on serial. `panic_begin` stops cpu0's
// feeding and re-arms the full 15 s, so a panic path that hangs (log flush)
// still resets. `panic_end` resets after `panic.reset=<s>` seconds (default 5,
// Linux `panic=` semantics); `panic.reset=0` stops the watchdog and halts so
// the screen/serial state can be inspected.

static PANICKED: AtomicBool = AtomicBool::new(false);
/// `panic.reset=` seconds; u32::MAX = never (halt). Parsed at `wdt_init`.
static PANIC_RESET_SECS: AtomicU32 = AtomicU32::new(5);

/// First thing on panic: no more feeding, full hardware timeout from now.
pub fn panic_begin() {
    if PANICKED.swap(true, Ordering::AcqRel) { return; }
    clear_trial_flag_nonblocking();
    if PM_OK.load(Ordering::Acquire) && crate::rpi_pm::wdt_running_nolock() {
        crate::rpi_pm::wdt_kick_nolock(crate::rpi_misc_logic::wdt::HW_SECS);
    }
}

/// After the panic message is printed: reset the board (or halt with
/// `panic.reset=0`). Returns only when the PM block is unavailable.
pub fn panic_end() {
    if !PM_OK.load(Ordering::Acquire) { return; }
    let secs = PANIC_RESET_SECS.load(Ordering::Relaxed);
    if secs == u32::MAX {
        crate::rpi_pm::wdt_stop_nolock();
        serial_debug("[WDT] panic: halted, watchdog stopped (panic.reset=0)\n");
        return;
    }
    line(|b| { b.s("[WDT] panic: full reset in ").u(secs as u64).s(" s (panic.reset=0 halts instead)"); });
    let t0 = sched::monotonic_ns();
    let mut last = 0u64;
    loop {
        let el = sched::monotonic_ns().wrapping_sub(t0);
        if el >= secs as u64 * 1_000_000_000 { break; }
        // Keep the hardware timer as the backstop; it fires 15 s after the last
        // kick even if this clock stops (EPOCH unset reads 0 forever).
        if el / 1_000_000_000 != last || el == 0 {
            last = el / 1_000_000_000;
            crate::rpi_pm::wdt_kick_nolock(crate::rpi_misc_logic::wdt::HW_SECS);
        }
        core::hint::spin_loop();
    }
    serial_debug("[WDT] panic: full reset now\n");
    crate::rpi_pm::restart();
    loop { core::hint::spin_loop(); }
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
    uvolt_init(cmdline);
    let want_thread = GOV_MODE.load(Ordering::Relaxed) == 1;
    if want_thread && sched::kthread::spawn("pi-house", housekeeping).is_none() {
        serial_debug("[cpufreq] rpi fail kthread\n");
    }
}
