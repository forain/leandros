//! Kernel services the USB stack needs but must not depend on directly.
//!
//! `usb` sits below `sched` and `drivers` in the crate graph, so the clock,
//! the "let other tasks run" primitive and the serial console arrive as plain
//! function pointers installed once by `drivers::usb_hcd` before any
//! controller is touched.
//!
//! Every wait in the xHCI driver is a deadline against [`now_ns`]; there is
//! no spin-count timeout anywhere. If the hooks were never installed the
//! clock falls back to a counter that advances 1 µs per read, which still
//! guarantees every wait terminates (a loop that reads the clock makes
//! progress towards its deadline on every iteration).

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Hooks supplied by the kernel.
pub struct Hooks {
    /// CLOCK_MONOTONIC in nanoseconds (must be readable with IRQs masked).
    pub now_ns: fn() -> u64,
    /// Called on every iteration of a polling loop. In task context this
    /// should yield the CPU; outside it, a pause hint.
    pub relax: fn(),
    /// Raw console output (serial).
    pub log: fn(&str),
}

static NOW_NS: AtomicUsize = AtomicUsize::new(0);
static RELAX: AtomicUsize = AtomicUsize::new(0);
static LOG: AtomicUsize = AtomicUsize::new(0);
static FALLBACK_CLOCK: AtomicU64 = AtomicU64::new(0);

/// Verbose per-command / per-transfer tracing (`usb-debug` feature or a
/// runtime toggle via [`set_verbose`]).
static VERBOSE: AtomicBool = AtomicBool::new(false);
/// Run the BOT self-test on every mass-storage interface at enumeration.
static SELFTEST: AtomicBool = AtomicBool::new(false);
/// Force every DMA allocation below 4 GiB even on an AC64=1 controller —
/// exercises the 32-bit-only path real AC64=0 hardware takes.
static FORCE_DMA32: AtomicBool = AtomicBool::new(false);

pub fn install(h: Hooks) {
    NOW_NS.store(h.now_ns as usize, Ordering::Release);
    RELAX.store(h.relax as usize, Ordering::Release);
    LOG.store(h.log as usize, Ordering::Release);
}

pub fn set_verbose(on: bool) { VERBOSE.store(on, Ordering::Relaxed); }
pub fn verbose() -> bool { VERBOSE.load(Ordering::Relaxed) }
pub fn set_selftest(on: bool) { SELFTEST.store(on, Ordering::Relaxed); }
pub fn selftest() -> bool { SELFTEST.load(Ordering::Relaxed) }
pub fn set_force_dma32(on: bool) { FORCE_DMA32.store(on, Ordering::Relaxed); }
pub fn force_dma32() -> bool { FORCE_DMA32.load(Ordering::Relaxed) }

pub fn now_ns() -> u64 {
    let f = NOW_NS.load(Ordering::Acquire);
    if f != 0 {
        let f: fn() -> u64 = unsafe { core::mem::transmute(f) };
        f()
    } else {
        FALLBACK_CLOCK.fetch_add(1_000, Ordering::Relaxed) + 1_000
    }
}

pub fn relax() {
    let f = RELAX.load(Ordering::Acquire);
    if f != 0 {
        let f: fn() = unsafe { core::mem::transmute(f) };
        f()
    } else {
        core::hint::spin_loop();
    }
}

pub fn log_str(s: &str) {
    let f = LOG.load(Ordering::Acquire);
    if f != 0 {
        let f: fn(&str) = unsafe { core::mem::transmute(f) };
        f(s)
    }
}

/// A deadline on the monotonic clock.
#[derive(Clone, Copy)]
pub struct Deadline(u64);

impl Deadline {
    pub fn after_ms(ms: u32) -> Self {
        Deadline(now_ns().saturating_add(ms as u64 * 1_000_000))
    }
    pub fn expired(&self) -> bool { now_ns() >= self.0 }
}

/// Sleep (polling) for `ms` milliseconds.
pub fn sleep_ms(ms: u32) {
    let d = Deadline::after_ms(ms);
    while !d.expired() { relax(); }
}

/// Poll `cond` until it returns true or `ms` elapse. Returns the final value.
pub fn wait_until(ms: u32, mut cond: impl FnMut() -> bool) -> bool {
    let d = Deadline::after_ms(ms);
    loop {
        if cond() { return true; }
        if d.expired() { return cond(); }
        relax();
    }
}

/// `core::fmt` sink for the log hook (no allocation, no buffering).
pub struct LogWriter;

impl core::fmt::Write for LogWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        log_str(s);
        Ok(())
    }
}

/// Always-on USB log line.
#[macro_export]
macro_rules! usb_log {
    ($($arg:tt)*) => {{
        let _ = core::fmt::Write::write_fmt(&mut $crate::platform::LogWriter, format_args!($($arg)*));
    }};
}

/// Log line printed only when verbose tracing is enabled.
#[macro_export]
macro_rules! usb_dbg {
    ($($arg:tt)*) => {{
        if $crate::platform::verbose() {
            let _ = core::fmt::Write::write_fmt(&mut $crate::platform::LogWriter, format_args!($($arg)*));
        }
    }};
}
