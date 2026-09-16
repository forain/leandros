//! AArch64 generic timer — EL1 physical timer (CNTP).
//!
//! Configured for 100 Hz using CNTFRQ_EL0 as the frequency reference.
//! The IRQ (PPI #30) is routed through the GIC by `gic::init()` before
//! this module is initialised.
//!
//! Ref: ARM Architecture Reference Manual §D7 (Generic Timer)

use core::sync::atomic::{AtomicU64, Ordering};

/// Target interrupt rate.
const TICK_HZ: u64 = 100;

/// Global tick counter — incremented on every timer interrupt.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// CNTVCT_EL0 sampled at the instant of the most recent BSP tick, and the
/// highest value `monotonic_ns` has ever returned. Together they give
/// CLOCK_MONOTONIC sub-tick resolution: see `monotonic_ns`.
static LAST_TICK_CNT: AtomicU64 = AtomicU64::new(0);
static MONO_LAST_NS:  AtomicU64 = AtomicU64::new(0);

/// CNTVCT_EL0 at each CPU's most recent tick (0 = timer not started yet).
/// `check_alive` compares it against the counter to catch a virtual timer
/// that stopped delivering: the countdown is only ever re-armed from inside
/// `on_tick`, so a single lost interrupt silences that CPU's tick forever —
/// and on the BSP that freezes `TICK_COUNT`, every `nanosleep`, every poll
/// deadline and the audio pump at once, with no panic and no output.
const TIMER_MAX_CPUS: usize = 8;
static LAST_TICK_CNT_CPU: [AtomicU64; TIMER_MAX_CPUS] =
    [const { AtomicU64::new(0) }; TIMER_MAX_CPUS];
/// Times `check_alive` had to re-arm this CPU's timer (for the watchdog line).
static REARMS: [core::sync::atomic::AtomicU32; TIMER_MAX_CPUS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; TIMER_MAX_CPUS];

/// Read the always-on virtual counter.
#[inline]
fn cntvct() -> u64 {
    let c: u64;
    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) c, options(nomem, nostack)); }
    c
}

/// Monotonic nanoseconds since boot, interpolated *inside* the current tick.
///
/// A 100 Hz tick counter alone answers `clock_gettime` in 10 ms steps, which is
/// not merely coarse — it is wrong in a way userspace acts on. Mesa's venus ring
/// throttles the "wake the idle renderer" notification to one per 1 ms, and
/// decides using this clock; with a 10 ms clock two submissions up to 10 ms
/// apart read the *same* timestamp, the second notification is suppressed, and
/// virglrenderer's ring thread — which re-idles after 1 ms and only ever waits
/// on an explicit notify — sleeps forever. That is the whole `vktest`-under-TCG
/// hang. The generic timer is free-running, per-architecture exact (CNTFRQ_EL0),
/// and readable at EL0 cost, so the fraction is real, not estimated.
///
/// The tick and its counter anchor are published from `on_tick` one after the
/// other, so a reader can catch them mid-update; the anchor is re-read to detect
/// that, the fraction is clamped below one tick, and the result is passed
/// through a `fetch_max` so the clock can never step backwards.
pub fn monotonic_ns() -> u64 {
    let f = freq();
    let ns = loop {
        let a = LAST_TICK_CNT.load(Ordering::Acquire);
        let t = TICK_COUNT.load(Ordering::Acquire);
        let b = LAST_TICK_CNT.load(Ordering::Acquire);
        if a != b { continue; }
        let base = t.wrapping_mul(10_000_000);
        if a == 0 || f == 0 { break base; }
        let d = cntvct().wrapping_sub(a);
        let frac = ((d as u128) * 1_000_000_000u128 / f as u128) as u64;
        break base + frac.min(9_999_999);
    };
    let prev = MONO_LAST_NS.fetch_max(ns, Ordering::Relaxed);
    if prev > ns { prev } else { ns }
}

/// Return the number of timer ticks since boot.
#[inline]
pub fn ticks() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

/// Read the hardware timer frequency (CNTFRQ_EL0).
pub fn freq() -> u64 {
    let f: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack));
    }
    f
}

/// Resolution of `monotonic_ns` in nanoseconds: the period of the generic
/// timer the sub-tick fraction is interpolated from, floored at 1 ns. Falls
/// back to a whole tick if CNTFRQ_EL0 reads zero, so the answer is never better
/// than what the clock can actually deliver.
pub fn resolution_ns() -> u64 {
    let f = freq();
    if f == 0 { return 10_000_000; }
    (1_000_000_000u64 / f).max(1)
}

/// Compute the reload value for one tick interval.
fn interval() -> u64 {
    let f = freq();
    if f == 0 { 1_000_000 } else { f / TICK_HZ } // guard against uninitialised freq
}

/// Initialise the virtual timer and unmask IRQs at EL1.
///
/// Must be called after `gic::init()` so the IRQ reaches the CPU.
pub fn init() {
    unsafe {
        // Load the countdown value (CNTV_TVAL_EL0).
        core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval(),
                         options(nomem, nostack));
        // Enable the timer: ENABLE=1, IMASK=0.
        core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64,
                         options(nomem, nostack));
        core::arch::asm!("isb", options(nomem, nostack));

        // Unmask IRQ exceptions at EL1 (clear DAIF.I, bit 7).
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }
}

/// Self-check for a silently dead local timer; re-arms it if so.
///
/// Called by the scheduler wherever it opens an IRQ window. If this CPU has
/// had interrupts enabled and still not ticked for `2 s` of counter time, the
/// virtual timer is not going to fire on its own (its countdown expired and
/// was never reloaded — the `on_tick` reload is the only one), so reload it
/// here and say so on the raw UART. Cost on the normal path: one counter read
/// and a compare. Returns true when a re-arm was needed.
pub fn check_alive() -> bool {
    let cpu = unsafe { super::smp::arch_cpu_id() }.min(TIMER_MAX_CPUS - 1);
    let last = LAST_TICK_CNT_CPU[cpu].load(Ordering::Relaxed);
    if last == 0 { return false; }
    let now = cntvct();
    let f = freq();
    if f == 0 || now.wrapping_sub(last) < 2 * f { return false; }
    // Claim the report so a spinning caller prints once per episode.
    if LAST_TICK_CNT_CPU[cpu].compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return false;
    }
    let n = REARMS[cpu].fetch_add(1, Ordering::Relaxed) + 1;
    let ctl: u64;
    let cval: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntv_ctl_el0", out(reg) ctl, options(nomem, nostack));
        core::arch::asm!("mrs {}, cntv_cval_el0", out(reg) cval, options(nomem, nostack));
        core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval(), options(nomem, nostack));
        core::arch::asm!("msr cntv_ctl_el0, {}", in(reg) 1u64, options(nomem, nostack));
        core::arch::asm!("isb", options(nomem, nostack));
    }
    extern "C" { fn arch_serial_putc(c: u8); fn print_number(n: u32); fn print_hex(n: usize); }
    fn s(m: &str) { for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } } }
    s("\n[TIMER] cpu"); unsafe { print_number(cpu as u32) };
    s(" virtual timer silent for "); unsafe { print_number((now.wrapping_sub(last) / (f / 1000).max(1)) as u32) };
    s(" ms with IRQ windows open: cntv_ctl="); unsafe { print_hex(ctl as usize) };
    s(" cval-now="); unsafe { print_hex(cval.wrapping_sub(now) as usize) };
    s(" re-armed (#"); unsafe { print_number(n) }; s(")\n");
    true
}

/// Called from the IRQ handler when PPI #27 fires (Virtual Timer).
///
/// Reloads the (banked, per-CPU) countdown register.  Global timekeeping and
/// device polling are BSP-only so wall-clock ticks don't advance N× faster
/// with N CPUs and the single UART/virtio queues have a single consumer.
pub fn on_tick() {
    unsafe {
        core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval(),
                         options(nomem, nostack));
    }

    let cpu = unsafe { super::smp::arch_cpu_id() };
    LAST_TICK_CNT_CPU[cpu.min(TIMER_MAX_CPUS - 1)].store(cntvct(), Ordering::Relaxed);
    if cpu == 0 {
        // Anchor the sub-tick interpolation BEFORE publishing the new tick, so a
        // concurrent reader can only ever see an anchor that is at most one tick
        // stale (bounded by the clamp in `monotonic_ns`), never one from the
        // future.
        LAST_TICK_CNT.store(cntvct(), Ordering::Release);
        let _count = TICK_COUNT.fetch_add(1, Ordering::Relaxed);

        // Poll VirtIO Keyboard
        drivers::virtio_keyboard::poll_events();

        // Poll UART for keyboard input and push to evdev (fallback drain;
        // the primary aarch64 path is the UART IRQ in exception.rs).
        while let Some(b) = unsafe { super::uart::getc() } {
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
    }

    sched::timer_tick_irq();
}
