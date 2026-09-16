//! AArch64 generic timer — EL1 virtual timer (CNTV), 100 Hz.
//!
//! The IRQ (PPI #27) is routed through the GIC by `gic::init()` before this
//! module is initialised.
//!
//! ## Timekeeping model
//!
//! Every CPU's tick is programmed as an ABSOLUTE compare value on a fixed grid:
//! deadline `k` is `EPOCH + k × interval` in CNTVCT_EL0 units, and `on_tick`
//! arms the next grid point strictly after "now". The interrupt latency of a
//! tick therefore never leaks into the period. It used to: the handler
//! reloaded `CNTV_TVAL_EL0` (a countdown from *now*), so a tick taken 4 ms late
//! — the cost of a `wfi` wake through Hypervisor.framework — scheduled the next
//! one 14 ms after the previous, and the "100 Hz" tick ran at 71 Hz idle and
//! 85 Hz under load. Every second of guest time was 1.2–1.4 host seconds.
//!
//! When the handler finds that more than one grid point has passed (a long
//! IRQ-masked section, a stalled vCPU, a lost timer edge later repaired by
//! `check_alive`), it does NOT fire the missed ticks back to back: it skips to
//! the next future grid point and hands the scheduler the number of grid points
//! that elapsed, so `sched::ticks()` stays a faithful 10 ms count of real time
//! and every tick-based deadline (nanosleep, poll, futex, timerfd) expires when
//! it should — without an interrupt storm.
//!
//! CLOCK_MONOTONIC (`monotonic_ns`) is read straight from CNTVCT_EL0 against
//! the same epoch, not from the tick count, so it is exact and continuous;
//! `sched::ticks() × 10 ms` trails it by at most one interval plus the latency
//! of the tick in flight, which is the direction deadlines need (never early).
//!
//! Ref: ARM Architecture Reference Manual §D11 (Generic Timer)

use core::sync::atomic::{AtomicU64, Ordering};

/// Target interrupt rate.
const TICK_HZ: u64 = 100;

/// CNTVCT_EL0 at `init` on the BSP — the origin of both the tick grid and
/// CLOCK_MONOTONIC. 0 until the BSP timer has started.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Total grid points the BSP's handler found already elapsed beyond the one it
/// was servicing (each is a tick accounted without its own interrupt).
static CATCH_UP_TICKS: AtomicU64 = AtomicU64::new(0);

const TIMER_MAX_CPUS: usize = 8;

/// Per CPU: the grid point (counter value) of the most recent tick this CPU
/// has accounted for. The programmed compare value is always `GRID + interval`.
/// 0 = timer not started on that CPU.
static GRID_CPU: [AtomicU64; TIMER_MAX_CPUS] =
    [const { AtomicU64::new(0) }; TIMER_MAX_CPUS];

/// CNTVCT_EL0 at each CPU's most recent tick INTERRUPT (0 = timer not started
/// yet). `check_alive` compares it against the counter to catch a virtual
/// timer that stopped delivering: a lost interrupt would silence that CPU's
/// tick forever — and on the BSP that freezes `sched::ticks()`, every
/// `nanosleep`, every poll deadline and the audio pump at once, with no panic
/// and no output.
static LAST_TICK_CNT_CPU: [AtomicU64; TIMER_MAX_CPUS] =
    [const { AtomicU64::new(0) }; TIMER_MAX_CPUS];
/// Times `check_alive` had to re-arm this CPU's timer (for the watchdog line).
static REARMS: [core::sync::atomic::AtomicU32; TIMER_MAX_CPUS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; TIMER_MAX_CPUS];

/// Read the always-on virtual counter.
#[inline]
fn cntvct() -> u64 {
    let c: u64;
    // ISB first: CNTVCT_EL0 reads may be speculated past earlier instructions;
    // for a clock that has to agree with the tick grid that is not acceptable.
    unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c, options(nomem, nostack)); }
    c
}

#[inline]
fn write_cval(v: u64) {
    unsafe {
        core::arch::asm!("msr cntv_cval_el0, {}", in(reg) v, options(nomem, nostack));
    }
}

/// Monotonic nanoseconds since the BSP timer started.
///
/// Read directly from the free-running counter (CNTFRQ_EL0 is exact for the
/// architecture, and CNTVCT_EL0 is the same clock on every PE). It is not
/// interpolated from the tick count any more — that made the clock inherit
/// every period error of the tick, and clamped it inside a 10 ms window so a
/// 14 ms tick period read as 10 ms of elapsed time: the guest clock ran 15–30 %
/// slow under HVF. Zero before `init` (nothing consults the clock that early
/// except the boot log, and a zero there is honest).
///
/// Sub-tick resolution is load-bearing for userspace, not cosmetic: Mesa's
/// venus ring throttles renderer wake-ups to one per 1 ms using this clock and
/// hangs when two submissions read the same timestamp.
pub fn monotonic_ns() -> u64 {
    let e = EPOCH.load(Ordering::Relaxed);
    let f = freq();
    if e == 0 || f == 0 { return 0; }
    let d = cntvct().wrapping_sub(e);
    ((d as u128) * 1_000_000_000u128 / f as u128) as u64
}

/// Whole 10 ms ticks of real time since the BSP timer started, from the
/// counter (what `sched::ticks()` converges to after each BSP tick).
#[inline]
pub fn ticks() -> u64 {
    let e = EPOCH.load(Ordering::Relaxed);
    if e == 0 { return 0; }
    cntvct().wrapping_sub(e) / interval()
}

/// Ticks the BSP has had to account without their own interrupt (diagnostic).
pub fn catch_up_ticks() -> u64 {
    CATCH_UP_TICKS.load(Ordering::Relaxed)
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
/// timer, floored at 1 ns. Falls back to a whole tick if CNTFRQ_EL0 reads zero,
/// so the answer is never better than what the clock can actually deliver.
pub fn resolution_ns() -> u64 {
    let f = freq();
    if f == 0 { return 10_000_000; }
    (1_000_000_000u64 / f).max(1)
}

/// Counter units in one tick interval.
#[inline]
fn interval() -> u64 {
    let f = freq();
    if f == 0 { 1_000_000 } else { f / TICK_HZ } // guard against uninitialised freq
}

/// Advance this CPU's grid past `now` and program the compare value for the
/// first grid point strictly after it. Returns how many grid points were at or
/// before `now` — the ticks of real time this CPU has to account for (1 on a
/// tick that arrived on time, 0 on a spurious interrupt, N after a stall).
fn advance_grid(cpu: usize, now: u64) -> u64 {
    let iv = interval();
    let grid = GRID_CPU[cpu].load(Ordering::Relaxed);
    let due = grid.wrapping_add(iv);
    let passed = if now.wrapping_sub(due) < (1u64 << 63) {
        // due <= now: the point we were armed for has passed, plus however
        // many whole intervals after it.
        now.wrapping_sub(due) / iv + 1
    } else {
        0
    };
    let new_grid = grid.wrapping_add(passed.wrapping_mul(iv));
    GRID_CPU[cpu].store(new_grid, Ordering::Relaxed);
    write_cval(new_grid.wrapping_add(iv));
    passed
}

/// Initialise this CPU's virtual timer and unmask IRQs at EL1.
///
/// Must be called after `gic::init()` / `gic::init_cpu_interface()` so the IRQ
/// reaches the CPU. The BSP defines the epoch; APs join the same grid so all
/// CPUs tick at (nominally) the same instants and `monotonic_ns` has one
/// origin.
pub fn init() {
    let cpu = unsafe { super::smp::arch_cpu_id() }.min(TIMER_MAX_CPUS - 1);
    let now = cntvct();
    let iv = interval();
    let epoch = if cpu == 0 {
        EPOCH.store(now, Ordering::Release);
        now
    } else {
        let e = EPOCH.load(Ordering::Acquire);
        if e == 0 { now } else { e }
    };
    // The grid point at or before now, on the BSP's grid.
    let grid = epoch.wrapping_add((now.wrapping_sub(epoch) / iv).wrapping_mul(iv));
    GRID_CPU[cpu].store(grid, Ordering::Relaxed);
    LAST_TICK_CNT_CPU[cpu].store(now, Ordering::Relaxed);
    unsafe {
        write_cval(grid.wrapping_add(iv));
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
/// virtual timer is not going to fire on its own (the hypervisor lost the
/// edge; the compare value sits in the past with nothing re-asserting it), so
/// program a FUTURE compare value — the next grid point after now — and say so
/// on the raw UART. The grid itself is left where it was: the tick that then
/// fires finds every missed grid point and accounts them all at once, so the
/// clock does not lose the silent stretch. Cost on the normal path: one counter
/// read and a compare. Returns true when a re-arm was needed.
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
    }
    let iv = interval();
    let grid = GRID_CPU[cpu].load(Ordering::Relaxed);
    // First grid point strictly after now (≤ one interval away).
    let next = grid.wrapping_add((now.wrapping_sub(grid) / iv + 1).wrapping_mul(iv));
    unsafe {
        write_cval(next);
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
/// Arms the next grid point on this CPU's (banked) timer and accounts the
/// elapsed ticks. Global timekeeping and device polling are BSP-only so
/// wall-clock ticks don't advance N× faster with N CPUs and the single
/// UART/virtio queues have a single consumer.
pub fn on_tick() {
    let cpu = unsafe { super::smp::arch_cpu_id() }.min(TIMER_MAX_CPUS - 1);
    let now = cntvct();
    let passed = advance_grid(cpu, now);
    LAST_TICK_CNT_CPU[cpu].store(now, Ordering::Relaxed);

    if cpu == 0 {
        if passed > 1 {
            CATCH_UP_TICKS.fetch_add(passed - 1, Ordering::Relaxed);
        }

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

    sched::timer_tick_irq(passed);
}
