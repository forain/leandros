//! x86-64 APIC timer — 100 Hz periodic timer for the scheduler.
//!
//! The legacy 8254 PIT (ports 0x40-0x43) is disabled on UEFI systems.
//! We use the Local APIC timer instead.
//!
//! Calibration uses PIT channel 2 as a ~10 ms reference:
//!   1. Program PIT ch2 for a one-shot 10 ms countdown.
//!   2. Start APIC timer counting down from 0xFFFF_FFFF (divide-by-16).
//!   3. Wait for PIT ch2 to finish (poll bit 5 of port 0x61).
//!   4. Measure APIC ticks elapsed → derive ticks-per-100Hz-interrupt.
//!
//! The PIT is only touched during this brief calibration; after init it
//! is never programmed again and all timer IRQs come from the APIC.
//!
//! Ref: Intel SDM Vol 3A §10.5 (APIC Timer); OSDev wiki "APIC timer"

use core::sync::atomic::{AtomicU64, Ordering};
use super::apic;

const TICK_HZ: u32 = 100;

/// Global tick counter incremented on every timer interrupt.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Return the number of scheduler ticks since boot.
#[inline]
pub fn ticks() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
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

/// Calibrate the APIC timer against PIT channel 2.
///
/// Returns the number of APIC timer ticks (divide-by-16) that elapsed
/// during a ~10 ms PIT countdown.  Returns a safe fallback if the APIC
/// counter did not decrease (hardware oddity or very fast/slow clock).
unsafe fn calibrate_apic_ticks_10ms() -> u32 {
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

    // ── Wait for PIT ch2 output (bit 5 of port 0x61 goes high when done) ─────
    loop {
        if inb(KBD_PORT) & (1 << 5) != 0 { break; }
    }

    let end = apic::read(apic::LAPIC_TIMER_CURR);

    // Mask the APIC timer again; we are not yet in periodic mode.
    apic::write(apic::LAPIC_LVT_TIMER, (1 << 16) | 0xFF);

    // ── Compute elapsed APIC ticks ────────────────────────────────────────────
    // The counter counts *down*; elapsed = start - end.
    let elapsed = start.wrapping_sub(end);
    if elapsed == 0 {
        // Fallback: assume ~1 GHz bus / 16 = 62.5 MHz APIC, 10 ms = 625_000 ticks.
        625_000
    } else {
        elapsed
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Initialise the APIC timer at `TICK_HZ` (100 Hz) using PIT ch2 calibration.
///
/// # Safety
/// `apic::init()` must have been called first (LAPIC must be enabled).
pub unsafe fn init() {
    let ticks_10ms = calibrate_apic_ticks_10ms();

    // Ticks per interrupt at TICK_HZ:
    //   100 Hz → 10 ms per tick → initial count = ticks_10ms
    //   50 Hz  → 20 ms per tick → initial count = ticks_10ms * 2
    // For TICK_HZ = 100, the 10 ms measurement is exactly one tick.
    let ticks_per_irq = ticks_10ms.saturating_mul(100 / TICK_HZ)
        .max(1000); // guard: never below 1000 (avoids infinite-IRQ storm)

    // Programme APIC timer: vector 32, periodic mode, divide-by-16.
    // LVT_TIMER bits: [18:17]=00 (one-shot) / 01 (periodic) / 10 (TSC-deadline)
    //                 [16]=0 (not masked)
    //                 [7:0]=vector
    apic::write(apic::LAPIC_TIMER_DIV,  0x3);                // divide by 16
    apic::write(apic::LAPIC_LVT_TIMER,  (1 << 17) | 32);    // periodic, vec 32
    apic::write(apic::LAPIC_TIMER_INIT, ticks_per_irq);
}

/// Called from the timer IRQ handler (vector 32) on every APIC timer tick.
#[inline]
pub fn on_tick() {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);

    // Poll UART for keyboard input and push to evdev.
    // NOTE: This consumes bytes that would otherwise go to fd 0 (stdin).
    while let Some(b) = unsafe { super::serial_read_byte() } {
        evdev_server::push_event(0, 1 /* EV_KEY */, b as u16, 1); // Key down only
        evdev_server::push_event(0, 0 /* EV_SYN */, 0 /* SYN_REPORT */, 0);
    }

    // Poll PS/2 Keyboard for native input and push to evdev.
    super::keyboard::poll();

    sched::timer_tick_irq();
}
