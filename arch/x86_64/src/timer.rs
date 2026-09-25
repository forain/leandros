//! x86-64 APIC timer — 100 Hz periodic timer for the scheduler.
//!
//! The legacy 8254 PIT (ports 0x40-0x43) is disabled on UEFI systems.
//! We use the Local APIC timer instead.
//!
//! Calibration uses PIT channel 2 as a ~10 ms reference:
//!   1. Program PIT ch2 for a one-shot 10 ms countdown.
//!   2. Start APIC timer counting down from 0xFFFF_FFFF (divide-by-16).
//!   3. Wait for PIT ch2 to finish (poll bit 5 of port 0x61).
//!   4. Measure APIC ticks elapsed → derive ticks-per-100Hz-interrupt,
//!      and TSC cycles elapsed → the TSC frequency.
//!
//! The PIT is only touched during this brief calibration; after init it
//! is never programmed again and all timer IRQs come from the APIC.
//!
//! The TSC frequency is what every kernel clock derives from (`monotonic_ns`,
//! and through it `arch_monotonic_ns` / `drivers::snd::monotonic_us`), so it is
//! resolved with some care — see `resolve_tsc_khz`: a frequency the CPU or the
//! hypervisor states through CPUID is preferred, the PIT window is the
//! measurement that confirms it or stands in for it, and the result is printed
//! once at boot as `[TSC] … MHz`.
//!
//! Ref: Intel SDM Vol 3A §10.5 (APIC Timer), Vol 2A CPUID leaves 15H/16H;
//! OSDev wiki "APIC timer"

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use super::apic;

const TICK_HZ: u32 = 100;

/// Global tick counter incremented on every timer interrupt.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// APIC-timer initial count for one 100 Hz interrupt, measured once by the
/// BSP's PIT calibration.  APs program their local timers directly from this
/// value (`init_local_timer`) — the PIT is a single shared device and must
/// not be re-touched concurrently, and all LAPIC timers in one package run
/// off the same clock anyway.
static TICKS_PER_IRQ: AtomicU32 = AtomicU32::new(0);

/// TSC cycles in one 10 ms tick — `TSC_KHZ × 10`. See `monotonic_ns`.
static TSC_PER_TICK:  AtomicU64 = AtomicU64::new(0);
/// The resolved TSC frequency in kHz (0 until `init`). See `resolve_tsc_khz`.
static TSC_KHZ: AtomicU64 = AtomicU64::new(0);
/// Where `TSC_KHZ` came from (a `TscSource` discriminant), for `/proc/cpuinfo`
/// readers and the boot line.
static TSC_SOURCE: AtomicU32 = AtomicU32::new(0);
/// TSC at `init` on the BSP: the origin of CLOCK_MONOTONIC and of the tick
/// grid. 0 until the BSP timer has started.
static EPOCH_TSC: AtomicU64 = AtomicU64::new(0);
/// The grid point (TSC value `EPOCH_TSC + TICK_COUNT × TSC_PER_TICK`) of the
/// most recent tick the BSP has accounted. See `on_tick`.
static GRID_TSC: AtomicU64 = AtomicU64::new(0);
/// Ticks the BSP accounted without an interrupt of their own (diagnostic).
static CATCH_UP_TICKS: AtomicU64 = AtomicU64::new(0);

#[inline]
fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Monotonic nanoseconds since the BSP timer started, read from the TSC.
///
/// A 100 Hz tick counter alone answers `clock_gettime` in 10 ms steps, which is
/// not merely coarse — it is wrong in a way userspace acts on. Mesa's venus ring
/// throttles the "wake the idle renderer" notification to one per 1 ms, and
/// decides using this clock; with a 10 ms clock two submissions up to 10 ms
/// apart read the *same* timestamp, the second notification is suppressed, and
/// virglrenderer's ring thread — which re-idles after 1 ms and only ever waits
/// on an explicit notify — sleeps forever. That is the whole `vktest`-under-TCG
/// hang.
///
/// The scale is the TSC frequency `init` resolved (`resolve_tsc_khz`: CPUID
/// when the CPU or hypervisor states it, else the PIT window that already
/// calibrates the APIC timer), so it is stated or measured, never assumed: a
/// real host TSC (~2–5 GHz) and TCG's virtual one both come out right with no
/// per-accelerator special case. The clock is the counter against that scale —
/// not the tick count with a clamped fraction, which inherited every lost tick
/// (see `on_tick`). Falls back to the tick count until `init` has run.
pub fn monotonic_ns() -> u64 {
    let per = TSC_PER_TICK.load(Ordering::Relaxed);
    let e = EPOCH_TSC.load(Ordering::Relaxed);
    if per == 0 || e == 0 {
        return TICK_COUNT.load(Ordering::Relaxed).wrapping_mul(10_000_000);
    }
    let d = rdtsc().wrapping_sub(e);
    ((d as u128) * 10_000_000u128 / per as u128) as u64
}

/// Ticks the BSP has had to account without their own interrupt (diagnostic).
pub fn catch_up_ticks() -> u64 {
    CATCH_UP_TICKS.load(Ordering::Relaxed)
}

/// Resolution of `monotonic_ns` in nanoseconds: the period of the TSC the
/// sub-tick fraction is interpolated from, floored at 1 ns. Falls back to a
/// whole tick if the PIT window never established the scale, so the answer is
/// never better than what the clock can actually deliver.
pub fn resolution_ns() -> u64 {
    let per = TSC_PER_TICK.load(Ordering::Relaxed);
    if per == 0 { return 10_000_000; }
    (10_000_000u64 / per).max(1)
}

/// Return the number of scheduler ticks since boot.
#[inline]
pub fn ticks() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

/// The TSC frequency `init` resolved, in kHz; 0 before `init`.
pub fn tsc_khz() -> u64 {
    TSC_KHZ.load(Ordering::Relaxed)
}

/// TSC cycles per 10 ms scheduler tick; 0 before `init`. The unit anything
/// that wants to wait "a few milliseconds" against a raw `rdtsc` should use.
pub fn tsc_per_tick() -> u64 {
    TSC_PER_TICK.load(Ordering::Relaxed)
}

/// Where the TSC frequency came from.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum TscSource {
    /// `init` has not run.
    None = 0,
    /// Hypervisor timing leaf CPUID.4000_0010H (VMware convention; QEMU/KVM
    /// exposes it with `vmware-cpuid-freq` when the TSC is stable and known).
    HvLeaf = 1,
    /// CPUID.15H with the core crystal frequency enumerated: exact.
    Crystal = 2,
    /// CPUID.15H ratio × CPUID.16H nominal frequency, with the implied crystal
    /// snapped to the standard part it must be (Linux does the same in
    /// `native_calibrate_tsc`, less the snap).
    Nominal = 3,
    /// Measured against the PIT (min of three 10 ms windows).
    Pit = 4,
}

/// Where the TSC frequency came from.
pub fn tsc_source() -> TscSource {
    match TSC_SOURCE.load(Ordering::Relaxed) {
        1 => TscSource::HvLeaf,
        2 => TscSource::Crystal,
        3 => TscSource::Nominal,
        4 => TscSource::Pit,
        _ => TscSource::None,
    }
}

impl TscSource {
    pub fn name(self) -> &'static str {
        match self {
            TscSource::None => "unresolved",
            TscSource::HvLeaf => "cpuid 0x40000010",
            TscSource::Crystal => "cpuid 0x15",
            TscSource::Nominal => "cpuid 0x15+0x16",
            TscSource::Pit => "pit",
        }
    }
}

/// TSC frequency stated by CPUID, if any: the hypervisor's timing leaf first
/// (it describes the virtual TSC the guest actually sees), then Intel's leaf
/// 15H (exact when the crystal is enumerated), then 15H's ratio against the
/// nominal frequency of 16H. Returns `None` on CPUs and hypervisors that state
/// nothing (QEMU TCG, `-cpu` models below level 0x15, pre-Skylake parts).
fn tsc_khz_from_cpuid() -> Option<(u64, TscSource)> {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    {
        if __cpuid(1).ecx & (1 << 31) != 0 {
            let hv = __cpuid(0x4000_0000);
            if hv.eax >= 0x4000_0010 {
                let t = __cpuid(0x4000_0010);
                if t.eax != 0 { return Some((t.eax as u64, TscSource::HvLeaf)); }
            }
        }
        let max = __cpuid(0).eax;
        if max < 0x15 { return None; }
        let l = __cpuid_count(0x15, 0);
        let (den, num, crystal_hz) = (l.eax as u64, l.ebx as u64, l.ecx as u64);
        if den == 0 || num == 0 { return None; }
        if crystal_hz != 0 {
            return Some((crystal_hz * num / den / 1000, TscSource::Crystal));
        }
        if max < 0x16 { return None; }
        let base_mhz = __cpuid_count(0x16, 0).eax as u64;
        if base_mhz == 0 { return None; }
        // 16H's nominal frequency is the TSC frequency rounded to a MHz. The
        // crystal it implies is one of the standard parts; snapping to it
        // recovers the exact value (a 1.9 GHz nominal Kaby Lake is really
        // 24 MHz × 79 = 1896 MHz, and 16H says 1900).
        let implied_khz = base_mhz * 1000 * den / num;
        let mut khz = base_mhz * 1000;
        for &c in &[19_200u64, 24_000, 25_000, 38_400] {
            if implied_khz.abs_diff(c) * 100 < c {
                khz = c * num / den;
                break;
            }
        }
        Some((khz, TscSource::Nominal))
    }
}

// ── PIT channel 2 calibration helpers ────────────────────────────────────────
//
// PIT input clock: 1_193_182 Hz
// Divisor for 10 ms: 1_193_182 / 100 = 11_932

const PIT_CMD:      u16 = 0x43; // Mode/command register
const PIT_CH2:      u16 = 0x42; // Channel 2 data port
const KBD_PORT:     u16 = 0x61; // PC/AT keyboard controller miscellaneous
const PIT_DIV_10MS: u16 = 11_932;

#[cfg(target_arch = "x86_64")]
unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!(
        "out dx, al",
        in("dx") port, in("al") val,
        options(nomem, nostack)
    );
}

#[cfg(target_arch = "x86_64")]
unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    core::arch::asm!(
        "in al, dx",
        out("al") v, in("dx") port,
        options(nomem, nostack)
    );
    v
}

/// One PIT channel 2 window: the APIC timer ticks (divide-by-16) and the TSC
/// cycles that elapsed during a ~10 ms PIT countdown. `None` if the PIT never
/// signalled — no 8254 on this board, or its gate is not ours to open — after
/// a bounded wait, so a missing PIT costs a moment of boot, not the boot.
unsafe fn pit_window_10ms() -> Option<(u32, u64)> {
    // ── Enable PIT channel 2 gate via keyboard controller port 0x61 ──────────
    // Bits [1:0] control the gate and speaker:
    //   bit 0 = gate for PIT ch2  (1 = enable)
    //   bit 1 = speaker output    (0 = muted)
    let kbd = inb(KBD_PORT);
    outb(KBD_PORT, (kbd & 0xFC) | 0x01);

    // ── Program PIT ch2: one-shot (mode 0), binary, load-then-count ──────────
    // Command byte: CH=10, ACCESS=11 (lo/hi), MODE=000 (one-shot), BCD=0
    outb(PIT_CMD, 0xB0);
    outb(PIT_CH2, (PIT_DIV_10MS & 0xFF) as u8);
    outb(PIT_CH2, (PIT_DIV_10MS >> 8)   as u8);

    // ── Start APIC timer (masked, one-shot, divide by 16) ────────────────────
    apic::write(apic::LAPIC_TIMER_DIV,  0x3);          // divide by 16
    apic::write(apic::LAPIC_LVT_TIMER,  (1 << 16) | 0xFF); // masked, vec=0xFF
    apic::write(apic::LAPIC_TIMER_INIT, 0xFFFF_FFFF);

    let start = apic::read(apic::LAPIC_TIMER_CURR);
    // The PIT window is exactly one 100 Hz tick, so the TSC delta across it is
    // TSC-cycles-per-tick — the scale `monotonic_ns` needs, measured on the same
    // reference the APIC timer is calibrated against.
    let tsc_start = rdtsc();

    // ── Wait for PIT ch2 output (bit 5 of port 0x61 goes high when done) ─────
    // Bounded: 2^32 cycles is ~1 s at 4 GHz, ~4 s on a 1 GHz virtual TSC —
    // either way two orders of magnitude past the window.
    let mut done = false;
    loop {
        if inb(KBD_PORT) & (1 << 5) != 0 { done = true; break; }
        if rdtsc().wrapping_sub(tsc_start) > (1u64 << 32) { break; }
    }

    let end = apic::read(apic::LAPIC_TIMER_CURR);
    let tsc_elapsed = rdtsc().wrapping_sub(tsc_start);

    // Mask the APIC timer again; we are not yet in periodic mode.
    apic::write(apic::LAPIC_LVT_TIMER, (1 << 16) | 0xFF);

    if !done { return None; }
    // The counter counts *down*; elapsed = start - end.
    Some((start.wrapping_sub(end), tsc_elapsed))
}

/// Calibrate against the PIT: the APIC timer ticks and TSC cycles in one
/// 10 ms window. Three windows are taken and the shortest kept: a window can
/// only ever read long (the host deschedules the vCPU, an SMI, a slow port
/// exit on the poll that would have seen the flag), never short, so the
/// minimum is the truest and a single preempted window cannot skew the clock
/// by whatever the host stole. Both figures come from the same window so the
/// APIC/TSC ratio stays consistent. `None` if the PIT is not there.
unsafe fn calibrate_pit() -> Option<(u32, u64)> {
    let mut best: Option<(u32, u64)> = None;
    for _ in 0..3 {
        let w = pit_window_10ms()?;
        best = match best {
            Some(b) if b.1 <= w.1 => Some(b),
            _ => Some(w),
        };
    }
    best
}

/// Decide the TSC frequency from what CPUID states and what the PIT measured.
///
/// A stated frequency wins when it exists and the measurement agrees with it
/// to 5 %: the hypervisor leaf describes the very TSC the guest sees, and
/// leaf 15H is the hardware's own ratio to a crystal, both exact where a
/// 10 ms window is good to ~0.05 %. When they disagree by more than that the
/// measurement wins — a hypervisor that scales the TSC but forwards the host's
/// CPUID would state a frequency the counter does not run at, and the PIT is
/// the one comparing the counter against real time. Without a PIT the stated
/// value stands alone; without either, the last resort is 1 GHz, which is what
/// TCG's virtual TSC runs at on the hosts this kernel is developed on.
fn resolve_tsc_khz(stated: Option<(u64, TscSource)>, measured_khz: Option<u64>) -> (u64, TscSource) {
    match (stated, measured_khz) {
        (Some((s, src)), Some(m)) if s.abs_diff(m) * 20 < s => (s, src),
        (_, Some(m)) => (m, TscSource::Pit),
        (Some((s, src)), None) => (s, src),
        (None, None) => (1_000_000, TscSource::None),
    }
}

/// `[TSC] 1896.000 MHz (cpuid 0x15+0x16); pit measured 1895.912 MHz` — once,
/// so a reader of any later `*_us` diagnostic can trust its unit.
fn log_tsc(khz: u64, src: TscSource, stated: Option<(u64, TscSource)>, measured_khz: Option<u64>) {
    fn s(m: &str) { for &b in m.as_bytes() { unsafe { super::putc(b) } } }
    fn dec(mut v: u64) {
        let mut buf = [0u8; 20]; let mut n = 0;
        loop { buf[n] = b'0' + (v % 10) as u8; n += 1; v /= 10; if v == 0 { break; } }
        while n > 0 { n -= 1; unsafe { super::putc(buf[n]) } }
    }
    fn mhz(khz: u64) {
        dec(khz / 1000); s(".");
        let f = khz % 1000; if f < 100 { s("0"); } if f < 10 { s("0"); } dec(f); s(" MHz");
    }
    s("[TSC] "); mhz(khz); s(" ("); s(src.name()); s(")");
    if let Some((v, ssrc)) = stated {
        if ssrc != src { s("; "); s(ssrc.name()); s(" states "); mhz(v); }
    }
    match measured_khz {
        Some(m) => { if src != TscSource::Pit { s("; pit measured "); mhz(m); } }
        None => s("; no pit"),
    }
    s("\n");
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Initialise the APIC timer at `TICK_HZ` (100 Hz) using PIT ch2 calibration.
///
/// # Safety
/// `apic::init()` must have been called first (LAPIC must be enabled).
pub unsafe fn init() {
    let pit = calibrate_pit();
    let stated = tsc_khz_from_cpuid();
    let measured_khz = pit.map(|(_, tsc)| tsc / 10);
    let (khz, src) = resolve_tsc_khz(stated, measured_khz);
    TSC_KHZ.store(khz, Ordering::Release);
    TSC_PER_TICK.store(khz * 10, Ordering::Release);
    TSC_SOURCE.store(src as u32, Ordering::Release);
    log_tsc(khz, src, stated, measured_khz);

    // APIC ticks in the PIT window; without a PIT, or if the APIC counter did
    // not decrease (hardware oddity), assume ~1 GHz bus / 16 = 62.5 MHz APIC,
    // 10 ms = 625_000 ticks.
    let ticks_10ms = match pit {
        Some((apic, _)) if apic != 0 => apic,
        _ => 625_000,
    };

    // Ticks per interrupt at TICK_HZ:
    //   100 Hz → 10 ms per tick → initial count = ticks_10ms
    //   50 Hz  → 20 ms per tick → initial count = ticks_10ms * 2
    // For TICK_HZ = 100, the 10 ms measurement is exactly one tick.
    let ticks_per_irq = ticks_10ms.saturating_mul(100 / TICK_HZ)
        .max(1000); // guard: never below 1000 (avoids infinite-IRQ storm)

    TICKS_PER_IRQ.store(ticks_per_irq, Ordering::Release);

    // Programme APIC timer: vector 32, periodic mode, divide-by-16.
    // LVT_TIMER bits: [18:17]=00 (one-shot) / 01 (periodic) / 10 (TSC-deadline)
    //                 [16]=0 (not masked)
    //                 [7:0]=vector
    let now = rdtsc();
    EPOCH_TSC.store(now, Ordering::Release);
    GRID_TSC.store(now, Ordering::Relaxed);
    apic::write(apic::LAPIC_TIMER_DIV,  0x3);                // divide by 16
    apic::write(apic::LAPIC_LVT_TIMER,  (1 << 17) | 32);    // periodic, vec 32
    apic::write(apic::LAPIC_TIMER_INIT, ticks_per_irq);
}

/// Programme this CPU's Local APIC timer using the BSP's calibration.
///
/// Called from `smp::sched_ap_entry` on each AP so secondary CPUs receive
/// their own 100 Hz preemption ticks.  No PIT access, no re-calibration.
///
/// # Safety
/// `apic::init()` must have run on this CPU, and the BSP must have completed
/// `timer::init()` (so `TICKS_PER_IRQ` is populated).
pub unsafe fn init_local_timer() {
    let ticks_per_irq = TICKS_PER_IRQ.load(Ordering::Acquire)
        .max(1000); // fallback guard if calibration never ran

    apic::write(apic::LAPIC_TIMER_DIV,  0x3);                // divide by 16
    apic::write(apic::LAPIC_LVT_TIMER,  (1 << 17) | 32);    // periodic, vec 32
    apic::write(apic::LAPIC_TIMER_INIT, ticks_per_irq);
}

const TIMER_MAX_CPUS: usize = sched::MAX_CPUS;

/// Per CPU: the earliest pending one-shot deadline (TSC units) armed by
/// `arm_deadline`, `u64::MAX` = none. Only this CPU touches its slot, with
/// IRQs masked.
static ONESHOT_TSC: [AtomicU64; TIMER_MAX_CPUS] =
    [const { AtomicU64::new(u64::MAX) }; TIMER_MAX_CPUS];
/// Per CPU: what the LAPIC countdown is currently programmed for.
/// `ST_PERIODIC` = the plain 100 Hz periodic mode; `ST_DEADLINE` = a ONE-SHOT
/// countdown to a pending deadline; `ST_REALIGN` = a one-shot countdown to the
/// next tick after a deadline interrupt, whose handler restores periodic mode.
///
/// The short countdowns must be one-shot, never a shortened *period*: QEMU's
/// `apic_timer` re-arms a periodic timer from its previous expiry, so a
/// microsecond-scale period that has fallen behind makes the main loop run
/// the timer callback forever under the BQL — every vCPU and every device
/// stops (seen on x86_64/TCG: guest wedged right after the login prompt).
/// `check_alive` restores periodic mode if a one-shot interrupt is ever lost.
static LAPIC_STATE: [AtomicU32; TIMER_MAX_CPUS] =
    [const { AtomicU32::new(ST_PERIODIC) }; TIMER_MAX_CPUS];
const ST_PERIODIC: u32 = 0;
const ST_DEADLINE: u32 = 1;
const ST_REALIGN: u32 = 2;
/// Per CPU: TSC at its most recent timer interrupt of any kind (`check_alive`).
static LAST_IRQ_TSC: [AtomicU64; TIMER_MAX_CPUS] =
    [const { AtomicU64::new(0) }; TIMER_MAX_CPUS];

const LVT_ONESHOT: u32 = 32;               // one-shot, vector 32
const LVT_PERIODIC: u32 = (1 << 17) | 32;  // periodic, vector 32

/// Program a one-shot countdown of `count` (IRQs masked by the caller).
unsafe fn program_oneshot(count: u32) {
    apic::write(apic::LAPIC_LVT_TIMER, LVT_ONESHOT);
    apic::write(apic::LAPIC_TIMER_INIT, count.max(1));
}

/// Back to the plain 100 Hz periodic tick (IRQs masked by the caller).
///
/// ORDER MATTERS: load the full initial count while still in one-shot mode,
/// THEN flip the mode bit. The other order leaves the timer periodic with the
/// tiny leftover one-shot count for one MMIO gap, and QEMU's main loop can
/// take the BQL in that gap and spin re-arming a ~30 ns period forever (the
/// same wedge the one-shot mode exists to avoid). Flipping the mode does not
/// restart the countdown, so the tick phase is unchanged.
unsafe fn program_periodic() {
    apic::write(apic::LAPIC_TIMER_INIT, TICKS_PER_IRQ.load(Ordering::Relaxed).max(1000));
    apic::write(apic::LAPIC_LVT_TIMER, LVT_PERIODIC);
}

/// Per AP: TSC at its most recent tick (APs keep no global time; this only
/// decides whether an interrupt was a tick or a pure deadline interrupt).
static LAST_TICK_TSC: [AtomicU64; TIMER_MAX_CPUS] =
    [const { AtomicU64::new(0) }; TIMER_MAX_CPUS];

/// Absolute `monotonic_ns()` instant → TSC value, rounded up.
fn ns_to_tsc(ns: u64) -> u64 {
    let per = TSC_PER_TICK.load(Ordering::Relaxed);
    let e = EPOCH_TSC.load(Ordering::Relaxed);
    let d = ((ns as u128) * per as u128 + 9_999_999) / 10_000_000u128;
    e.wrapping_add(d.min(u64::MAX as u128 / 2) as u64)
}

/// TSC cycles from now until `target` → LAPIC initial count (divide-by-16
/// units), biased 1/256 late so the APIC and TSC calibrations disagreeing by
/// a hair cannot make the interrupt land before the instant it is for (an
/// early one is harmless — the handler re-arms — but costs an interrupt).
fn tsc_delta_to_count(delta: u64) -> u32 {
    let per = TSC_PER_TICK.load(Ordering::Relaxed).max(1);
    let tpi = TICKS_PER_IRQ.load(Ordering::Relaxed) as u128;
    let c = (delta as u128) * tpi / per as u128;
    (c + c / 256 + 1).min(u32::MAX as u128) as u32
}

/// Arm this CPU's one-shot timer for the absolute `monotonic_ns()` instant
/// `deadline_ns` when it is sooner than what is armed already (see
/// `sched::register_poll_deadline`). Safe from task or IRQ context.
pub fn arm_deadline(deadline_ns: u64) {
    if TSC_PER_TICK.load(Ordering::Relaxed) == 0 || TICKS_PER_IRQ.load(Ordering::Relaxed) == 0 {
        return;
    }
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nomem)); }
    let cpu = unsafe { super::smp::arch_cpu_id() }.min(TIMER_MAX_CPUS - 1);
    let t = ns_to_tsc(deadline_ns);
    if t < ONESHOT_TSC[cpu].load(Ordering::Relaxed) {
        ONESHOT_TSC[cpu].store(t, Ordering::Relaxed);
        let want = tsc_delta_to_count(t.saturating_sub(rdtsc()));
        // CURR reads 0 when a one-shot already expired with its interrupt
        // still pending (IRQs are masked here): that handler will see the new
        // ONESHOT_TSC and program it, so leave the hardware alone.
        let cur = unsafe { apic::read(apic::LAPIC_TIMER_CURR) };
        if want < cur {
            unsafe { program_oneshot(want); }
            LAPIC_STATE[cpu].store(ST_DEADLINE, Ordering::Relaxed);
        }
    }
    if flags & (1 << 9) != 0 { unsafe { core::arch::asm!("sti", options(nomem, nostack)); } }
}

/// Service a due one-shot deadline and reprogram the countdown for whichever
/// comes first, the deadline or the next tick at `next_tick` (TSC). Returns the
/// LAPIC state the interrupt was taken in.
fn deadline_irq(cpu: usize, next_tick: u64) -> u32 {
    let st = LAPIC_STATE[cpu].load(Ordering::Relaxed);
    let now = rdtsc();
    LAST_IRQ_TSC[cpu].store(now, Ordering::Relaxed);
    let os = ONESHOT_TSC[cpu].load(Ordering::Relaxed);
    if os != u64::MAX && now >= os {
        ONESHOT_TSC[cpu].store(u64::MAX, Ordering::Relaxed);
        let next = sched::timer_deadline_irq();
        if next != u64::MAX {
            ONESHOT_TSC[cpu].store(ns_to_tsc(next), Ordering::Relaxed);
        }
    }
    let os = ONESHOT_TSC[cpu].load(Ordering::Relaxed);
    let now = rdtsc();
    unsafe {
        if os < next_tick {
            program_oneshot(tsc_delta_to_count(os.saturating_sub(now)));
            LAPIC_STATE[cpu].store(ST_DEADLINE, Ordering::Relaxed);
        } else if st == ST_DEADLINE {
            program_oneshot(tsc_delta_to_count(next_tick.saturating_sub(now)));
            LAPIC_STATE[cpu].store(ST_REALIGN, Ordering::Relaxed);
        } else if st == ST_REALIGN {
            program_periodic();
            LAPIC_STATE[cpu].store(ST_PERIODIC, Ordering::Relaxed);
        }
    }
    st
}

/// Self-check for a lost one-shot interrupt (the periodic tick cannot die on
/// its own, a one-shot can): if this CPU is in a one-shot state and has taken
/// no timer interrupt for 4 ticks, restore periodic mode. Called by the
/// scheduler wherever it opens an IRQ window. Returns true when it re-armed.
pub fn check_alive() -> bool {
    let per = TSC_PER_TICK.load(Ordering::Relaxed);
    if per == 0 { return false; }
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nomem)); }
    let cpu = unsafe { super::smp::arch_cpu_id() }.min(TIMER_MAX_CPUS - 1);
    let last = LAST_IRQ_TSC[cpu].load(Ordering::Relaxed);
    let mut rearmed = false;
    if LAPIC_STATE[cpu].load(Ordering::Relaxed) != ST_PERIODIC && last != 0
        && rdtsc().wrapping_sub(last) > 4 * per
    {
        unsafe { program_periodic(); }
        LAPIC_STATE[cpu].store(ST_PERIODIC, Ordering::Relaxed);
        LAST_IRQ_TSC[cpu].store(rdtsc(), Ordering::Relaxed);
        rearmed = true;
    }
    if flags & (1 << 9) != 0 { unsafe { core::arch::asm!("sti", options(nomem, nostack)); } }
    rearmed
}

/// Called from the timer IRQ handler (vector 32) on every APIC timer tick.
///
/// Every CPU ticks its own LAPIC timer; global timekeeping and UART polling
/// are BSP-only so wall-clock ticks don't advance N× faster with N CPUs and
/// the single serial FIFO has a single consumer.
///
/// The LAPIC timer is periodic on an exact grid, but an interrupt is not a
/// tick of time: when the BSP takes one late by more than a period (QEMU TCG
/// delivers the expiry from its main loop and skips an expiry it is already
/// past; a long IRQ-masked section merges two into one pending bit), the
/// missed periods used to vanish from `TICK_COUNT` — measured as the guest
/// clock running 1–36 % slow on x86_64/TCG, worse under load. So the tick
/// count is anchored to a TSC grid instead: each interrupt accounts however
/// many whole `TSC_PER_TICK` periods have elapsed since the last accounted
/// grid point (normally one; zero on an early or spurious one), and the
/// scheduler is told the number so `sched::ticks()` keeps time.
#[inline]
pub fn on_tick() {
    let cpu = unsafe { super::smp::arch_cpu_id() };
    let c = cpu.min(TIMER_MAX_CPUS - 1);
    let mut elapsed = 1u64;
    let per = TSC_PER_TICK.load(Ordering::Relaxed);
    if cpu == 0 {
        let grid = GRID_TSC.load(Ordering::Relaxed);
        if per != 0 && grid != 0 {
            elapsed = rdtsc().wrapping_sub(grid) / per;
            GRID_TSC.store(grid.wrapping_add(elapsed.wrapping_mul(per)), Ordering::Relaxed);
            if elapsed > 1 { CATCH_UP_TICKS.fetch_add(elapsed - 1, Ordering::Relaxed); }
        }
        if per != 0 {
            let next_tick = GRID_TSC.load(Ordering::Relaxed).wrapping_add(per);
            let st = deadline_irq(c, next_tick);
            // A pure deadline interrupt (no grid point passed) is not a tick.
            if st == ST_DEADLINE && elapsed == 0 { return; }
        }
        TICK_COUNT.fetch_add(elapsed, Ordering::Relaxed);

        // Poll VirtIO input devices (keyboard + tablet). The primary x86_64
        // console keyboard still comes from the UART drain below; this drains
        // the virtio-tablet's absolute-pointer events into evdev event1.
        drivers::virtio_keyboard::poll_events();

        // Poll UART for keyboard input and push to evdev.
        // NOTE: This consumes bytes that would otherwise go to fd 0 (stdin).
        while let Some(b) = unsafe { super::serial_read_byte() } {
            // Line-discipline ISIG intercept: ^C/^\/^Z become signals to the
            // foreground process group instead of input bytes.
            if tty_server::console_intercept_byte(b) { continue; }
            evdev_server::push_event(0, 1 /* EV_KEY */, b as u16, 2); // 2 = typematic/serial
            evdev_server::push_event(0, 0 /* EV_SYN */, 0 /* SYN_REPORT */, 0);
        }

        // One poll wake for everything this tick drained — virtio input above
        // and the UART bytes just now. Must come after BOTH, which is why it is
        // here and not at the end of `poll_events`. No-op unless the burst mode
        // is compiled in (see `evdev_server::WAKE_MODE`).
        evdev_server::flush_pending_wake();
    } else if per != 0 {
        let now = rdtsc();
        let last = LAST_TICK_TSC[c].load(Ordering::Relaxed);
        let is_tick = LAPIC_STATE[c].load(Ordering::Relaxed) != ST_DEADLINE
            || last == 0 || now.wrapping_sub(last) >= per;
        if is_tick { LAST_TICK_TSC[c].store(now, Ordering::Relaxed); }
        let base = if is_tick { now } else { last };
        deadline_irq(c, base.wrapping_add(per));
        if !is_tick { return; }
    }

    sched::timer_tick_irq(elapsed);
}
