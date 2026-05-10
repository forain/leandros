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
/// Reloads the countdown register and increments the tick counter.
pub fn on_tick() {
    unsafe {
        core::arch::asm!("msr cntv_tval_el0, {}", in(reg) interval(),
                         options(nomem, nostack));
    }
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);

    // Poll UART for keyboard input and push to evdev.
    // NOTE: This consumes bytes that would otherwise go to fd 0 (stdin).
    while let Some(b) = unsafe { super::uart::getc() } {
        evdev_server::push_event(0, 1 /* EV_KEY */, b as u16, 1); // Key down only
        evdev_server::push_event(0, 0 /* EV_SYN */, 0 /* SYN_REPORT */, 0);
    }

    sched::timer_tick_irq();
}
