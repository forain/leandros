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
    }

    sched::timer_tick_irq();
}
