//! Lock-free wedge diagnostics (lane piwedge, Raspberry Pi 5 bring-up).
//!
//! The tick watchdog (`watchdog_scan` in lib.rs) already notices a CPU that
//! stopped taking timer interrupts, but it prints through the console stage
//! (`arch_serial_putc` -> per-CPU stage -> `OUTBOX` spin lock -> framebuffer
//! mirror). When the stuck CPU holds any of those, or every CPU ends up
//! spinning behind it, the report itself wedges and the machine goes silent
//! with nothing on the UART. This module is the part that must not depend on
//! anything the stuck CPU could hold:
//!
//! * per-CPU IRQ trail (last INTID, IRQ count, interrupted PC) written by the
//!   arch IRQ entry with relaxed stores;
//! * a few subsystem stage markers (brcmf data path, SDIO host command, net
//!   glue) drivers update on every state change;
//! * an SPI storm detector (one INTID taken back to back without any other
//!   SPI in between, more than [`STORM_LIMIT`] times inside one second);
//! * a contended-spin probe for lock loops ([`Spin`]): a CPU spinning for
//!   more than 2 s prints itself, so even "every CPU is spinning" speaks;
//! * a printer that goes straight to the UART data register through
//!   `arch_wedge_putc` (bounded FIFO wait, no lock, no allocation).
//!
//! Everything is gated by [`enabled`] (one relaxed load). It is switched on by
//! default on the rpi5 build and with `wedge=1` on the command line elsewhere,
//! so x86_64/QEMU pay one predictable branch per IRQ and per marker.
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};

use crate::MAX_CPUS;

static ON: AtomicBool = AtomicBool::new(false);

#[inline(always)]
pub fn enabled() -> bool { ON.load(Relaxed) }

pub fn set_enabled(on: bool) { ON.store(on, Relaxed); }

// ── raw printer ─────────────────────────────────────────────────────────────

extern "C" {
    /// One byte straight to the console UART: bounded FIFO wait, no lock, no
    /// klog, no framebuffer (kernel/src/main.rs).
    fn arch_wedge_putc(c: u8);
}

/// Serialises whole reports between CPUs, but never blocks for long: a
/// holder that is itself the wedged CPU must not silence the others.
static PRINTING: AtomicBool = AtomicBool::new(false);

struct Out { held: bool }

impl Out {
    fn begin() -> Out {
        for _ in 0..2_000_000u32 {
            if PRINTING.compare_exchange(false, true, Acquire, Relaxed).is_ok() { return Out { held: true }; }
            core::hint::spin_loop();
        }
        Out { held: false }
    }
    fn s(&self, m: &str) { for &b in m.as_bytes() { put(b) } }
    fn n(&self, v: u64) { fmt_dec(v, &mut |b| put(b)); }
    fn h(&self, v: u64) { fmt_hex(v, &mut |b| put(b)); }
}

impl Drop for Out {
    fn drop(&mut self) { if self.held { PRINTING.store(false, Release); } }
}

#[inline]
fn put(b: u8) {
    #[cfg(not(test))]
    unsafe { arch_wedge_putc(b) }
    #[cfg(test)]
    let _ = b;
}

pub fn fmt_dec(mut v: u64, f: &mut dyn FnMut(u8)) {
    let mut buf = [0u8; 20];
    let mut i = 0;
    loop {
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
        if v == 0 { break; }
    }
    while i > 0 { i -= 1; f(buf[i]); }
}

pub fn fmt_hex(v: u64, f: &mut dyn FnMut(u8)) {
    f(b'0'); f(b'x');
    let mut started = false;
    for k in (0..16).rev() {
        let d = ((v >> (k * 4)) & 0xf) as u8;
        if d != 0 || started || k == 0 {
            started = true;
            f(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        }
    }
}

/// Raw text for callers that format their own line (lib.rs watchdog).
pub fn raw_str(m: &str) { for &b in m.as_bytes() { put(b) } }
pub fn raw_dec(v: u64) { fmt_dec(v, &mut |b| put(b)); }
pub fn raw_hex(v: u64) { fmt_hex(v, &mut |b| put(b)); }

fn now_ms() -> u64 {
    #[cfg(not(test))]
    { crate::monotonic_ns() / 1_000_000 }
    #[cfg(test)]
    { test_clock::NOW_MS.load(Relaxed) }
}

#[cfg(test)]
pub mod test_clock {
    pub static NOW_MS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
}

fn me() -> usize {
    #[cfg(not(test))]
    { (unsafe { crate::cpu_id() }).min(MAX_CPUS - 1) }
    #[cfg(test)]
    { 0 }
}

// ── per-CPU IRQ trail + storm detector ──────────────────────────────────────

static LAST_IRQ: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(u32::MAX) }; MAX_CPUS];
static IRQ_N: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static LAST_ELR: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Last SPI on this CPU, how many times in a row, and the counter value when
/// the run started.
static RUN_ID: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(u32::MAX) }; MAX_CPUS];
static RUN_N: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
static RUN_T0: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Back-to-back takes of one SPI within one second that count as a storm.
/// A real level-triggered storm runs at hundreds of thousands per second;
/// the busiest legitimate Pi source (the SD card) stays far below this and is
/// interleaved with other SPIs anyway.
pub const STORM_LIMIT: u32 = 50_000;
pub static STORMS: AtomicU32 = AtomicU32::new(0);

/// IRQ entry (arch, any CPU, IRQs masked). `now`/`per_s` are the arch
/// counter and its frequency. Returns true when `id` (an SPI, >= 32) is
/// storming: the caller masks it, and this has printed the one line for it.
#[inline]
pub fn irq_note(cpu: usize, id: u32, elr: u64, now: u64, per_s: u64) -> bool {
    if !enabled() { return false; }
    let c = cpu.min(MAX_CPUS - 1);
    LAST_IRQ[c].store(id, Relaxed);
    IRQ_N[c].fetch_add(1, Relaxed);
    LAST_ELR[c].store(elr, Relaxed);
    // Timer PPIs and IPIs neither count nor break a run: the GIC keeps
    // delivering the (lower-numbered) timer in the middle of an SPI storm.
    if id < 32 || id >= 1020 { return false; }
    storm_step(c, id, now, per_s)
}

fn storm_step(c: usize, id: u32, now: u64, per_s: u64) -> bool {
    if RUN_ID[c].load(Relaxed) != id {
        RUN_ID[c].store(id, Relaxed);
        RUN_N[c].store(1, Relaxed);
        RUN_T0[c].store(now, Relaxed);
        return false;
    }
    let n = RUN_N[c].load(Relaxed) + 1;
    if n < STORM_LIMIT { RUN_N[c].store(n, Relaxed); return false; }
    let dt = now.wrapping_sub(RUN_T0[c].load(Relaxed));
    RUN_N[c].store(0, Relaxed);
    RUN_T0[c].store(now, Relaxed);
    if dt > per_s.max(1) { return false; }
    STORMS.fetch_add(1, Relaxed);
    let o = Out::begin();
    o.s("\n[WEDGE] irq storm: intid "); o.n(id as u64);
    o.s(" taken "); o.n(STORM_LIMIT as u64); o.s("x back to back in ");
    o.n(dt.saturating_mul(1000) / per_s.max(1)); o.s(" ms on cpu"); o.n(c as u64);
    o.s(", masked at the GIC; elr="); o.h(LAST_ELR[c].load(Relaxed));
    marks_into(&o);
    o.s("\n");
    true
}

// ── subsystem stage markers ─────────────────────────────────────────────────

pub const SLOT_BRCMF: usize = 0;
pub const SLOT_SDIO: usize = 1;
pub const SLOT_NET: usize = 2;
pub const SLOT_WM: usize = 3;
/// The net daemon: 1 = in a run of passes without parking (arg unused).
/// Cleared at every park, so "stuck" means it has not parked for STUCK_MS.
pub const SLOT_NETD: usize = 4;
pub const N_SLOTS: usize = 5;
const SLOT_NAMES: [&str; N_SLOTS] = ["brcmf", "sdio", "net", "wm", "netd"];

/// Code 0 = idle. The rest is the driver's own stage numbering, decoded at
/// print time by the namer it registered (or printed in hex).
static SLOT_CODE: [AtomicU32; N_SLOTS] = [const { AtomicU32::new(0) }; N_SLOTS];
static SLOT_CPU: [AtomicU32; N_SLOTS] = [const { AtomicU32::new(0) }; N_SLOTS];
static SLOT_MS: [AtomicU64; N_SLOTS] = [const { AtomicU64::new(0) }; N_SLOTS];
static SLOT_ARG: [AtomicU64; N_SLOTS] = [const { AtomicU64::new(0) }; N_SLOTS];
static SLOT_NAMER: [AtomicUsize; N_SLOTS] = [const { AtomicUsize::new(0) }; N_SLOTS];
/// SLOT_MS value of the episode already reported as stuck (one line each).
static SLOT_TOLD: [AtomicU64; N_SLOTS] = [const { AtomicU64::new(u64::MAX) }; N_SLOTS];

/// A slot not idle for this long is reported by the watchdog scan.
pub const STUCK_MS: u64 = 3_000;

pub fn set_namer(slot: usize, f: fn(u32) -> &'static str) {
    if slot < N_SLOTS { SLOT_NAMER[slot].store(f as usize, Relaxed); }
}

/// Record that `slot` entered stage `code` (0 = idle) with an argument
/// (command, length, ...). Relaxed stores; a no-op when disabled.
#[inline]
pub fn mark(slot: usize, code: u32, arg: u64) {
    if !enabled() || slot >= N_SLOTS { return; }
    SLOT_ARG[slot].store(arg, Relaxed);
    SLOT_CPU[slot].store(me() as u32, Relaxed);
    SLOT_MS[slot].store(now_ms(), Relaxed);
    SLOT_CODE[slot].store(code, Release);
}

fn slot_name(slot: usize, code: u32) -> Option<&'static str> {
    let f = SLOT_NAMER[slot].load(Relaxed);
    if f == 0 { return None; }
    let f: fn(u32) -> &'static str = unsafe { core::mem::transmute::<usize, fn(u32) -> &'static str>(f) };
    Some(f(code))
}

fn marks_into(o: &Out) {
    let now = now_ms();
    o.s(" |");
    for k in 0..N_SLOTS {
        let code = SLOT_CODE[k].load(Acquire);
        let t = SLOT_MS[k].load(Relaxed);
        if code == 0 && t == 0 { continue; }
        o.s(" "); o.s(SLOT_NAMES[k]); o.s("=");
        match slot_name(k, code) { Some(n) => o.s(n), None => o.h(code as u64) }
        o.s("/"); o.h(SLOT_ARG[k].load(Relaxed));
        o.s("@cpu"); o.n(SLOT_CPU[k].load(Relaxed) as u64);
        o.s(" "); o.n(now.saturating_sub(t)); o.s("ms");
    }
}

/// Watchdog scan (any live CPU, every 0.5 s): a non-idle slot older than
/// [`STUCK_MS`] is reported once per episode.
pub fn scan_slots() {
    if !enabled() { return; }
    let now = now_ms();
    for k in 0..N_SLOTS {
        if SLOT_CODE[k].load(Acquire) == 0 { continue; }
        let t = SLOT_MS[k].load(Relaxed);
        if now.saturating_sub(t) < STUCK_MS || SLOT_TOLD[k].swap(t, Relaxed) == t { continue; }
        let cpu = SLOT_CPU[k].load(Relaxed) as usize;
        let o = Out::begin();
        o.s("\n[WEDGE] "); o.s(SLOT_NAMES[k]); o.s(" stage stuck "); o.n(now.saturating_sub(t)); o.s(" ms on cpu"); o.n(cpu as u64);
        cpu_into(&o, cpu);
        marks_into(&o);
        o.s("\n");
    }
}

fn cpu_into(o: &Out, cpu: usize) {
    let c = cpu.min(MAX_CPUS - 1);
    o.s(" [cpu"); o.n(c as u64); o.s(" irq#");
    let id = LAST_IRQ[c].load(Relaxed);
    if id == u32::MAX { o.s("-"); } else { o.n(id as u64); }
    o.s(" n="); o.n(IRQ_N[c].load(Relaxed));
    o.s(" elr="); o.h(LAST_ELR[c].load(Relaxed)); o.s("]");
}

/// Extra fields for the watchdog's `[WDOG] cpuN took no timer tick` line.
pub fn wdog_extra(cpu: usize) {
    if !enabled() { return; }
    let o = Out { held: false };
    cpu_into(&o, cpu);
    marks_into(&o);
}

/// Full state of every online CPU plus the markers, one line each.
pub fn dump_all(ncpu: usize, why: &str) {
    let o = Out::begin();
    o.s("\n[WEDGE] "); o.s(why); o.s(" (seen from cpu"); o.n(me() as u64); o.s(")");
    for c in 0..ncpu.min(MAX_CPUS) { cpu_into(&o, c); }
    marks_into(&o);
    o.s("\n");
}

pub fn armed_banner(ncpu: usize) {
    if !enabled() { return; }
    let o = Out::begin();
    o.s("[WEDGE] armed: raw-UART stall/storm reports, cpus="); o.n(ncpu as u64);
    o.s(" storm>="); o.n(STORM_LIMIT as u64); o.s("/s stuck>="); o.n(STUCK_MS); o.s("ms\n");
}

// ── contended-spin probe ────────────────────────────────────────────────────

/// Put one in a lock's spin loop: `let mut sp = Spin::new(); loop { ...; sp.tick("RUN_QUEUE"); }`.
/// Costs a counter increment per iteration and a clock read every 64 Ki.
pub struct Spin { n: u32, t0: u64, told: bool }

/// Spinning this long on one lock is reported (once per wait).
pub const SPIN_REPORT_MS: u64 = 2_000;

impl Spin {
    pub const fn new() -> Spin { Spin { n: 0, t0: 0, told: false } }
    #[inline]
    pub fn tick(&mut self, what: &str) {
        self.n = self.n.wrapping_add(1);
        if self.n & 0xFFFF != 0 || !enabled() || self.told { return; }
        let now = now_ms();
        if self.t0 == 0 { self.t0 = now.max(1); return; }
        if now.saturating_sub(self.t0) < SPIN_REPORT_MS { return; }
        self.told = true;
        SPIN_REPORTS.fetch_add(1, Relaxed);
        #[cfg(not(test))]
        let n = crate::active_cpu_count();
        #[cfg(test)]
        let n = 1;
        let mut msg = [0u8; 96];
        let mut l = 0;
        for &b in b"cpu spinning >2 s for ".iter().chain(what.as_bytes()) {
            if l < msg.len() { msg[l] = b; l += 1; }
        }
        dump_all(n, core::str::from_utf8(&msg[..l]).unwrap_or("spin"));
    }
    pub fn reported(&self) -> bool { self.told }
}

pub static SPIN_REPORTS: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storm_needs_back_to_back_within_a_second() {
        set_enabled(true);
        let per_s = 1_000_000u64;
        // A storm: LIMIT takes of SPI 140 in 0.1 s, timer PPIs interleaved.
        let mut hit = false;
        for i in 0..STORM_LIMIT as u64 {
            if i % 1000 == 0 { assert!(!irq_note(1, 27, 0, i * 2, per_s)); }
            hit |= irq_note(1, 140, 0x1234, i * 2, per_s);
        }
        assert!(hit, "SPI storm with interleaved timer ticks not detected");
        // Busy but interleaved with another SPI: never a storm.
        for i in 0..(3 * STORM_LIMIT as u64) {
            let id = if i % 2 == 0 { 150 } else { 151 };
            assert!(!irq_note(2, id, 0, i, per_s));
        }
        // Back to back but spread over more than a second: not a storm.
        for i in 0..(2 * STORM_LIMIT as u64) {
            assert!(!irq_note(3, 160, 0, i * 100, per_s));
        }
    }

    #[test]
    fn spin_and_stuck_slot_report_once() {
        set_enabled(true);
        test_clock::NOW_MS.store(1000, Relaxed);
        let mut sp = Spin::new();
        for _ in 0..0x30000 { sp.tick("X"); }
        assert!(!sp.reported());
        test_clock::NOW_MS.store(3500, Relaxed);
        for _ in 0..0x30000 { sp.tick("X"); }
        assert!(sp.reported());
        // Same test (one shared fake clock): a stuck slot is told once.
        test_clock::NOW_MS.store(10_000, Relaxed);
        mark(SLOT_WM, 7, 0);
        test_clock::NOW_MS.store(14_000, Relaxed);
        SLOT_TOLD[SLOT_WM].store(u64::MAX, Relaxed);
        scan_slots();
        assert_eq!(SLOT_TOLD[SLOT_WM].load(Relaxed), 10_000);
        mark(SLOT_WM, 0, 0);
    }

    #[test]
    fn number_formatting() {
        let mut v = [0u8; 24];
        let mut l = 0;
        fmt_hex(0xffff_0000_8001_2340, &mut |b| { v[l] = b; l += 1; });
        assert_eq!(&v[..l], b"0xffff000080012340");
        l = 0;
        fmt_dec(0, &mut |b| { v[l] = b; l += 1; });
        assert_eq!(&v[..l], b"0");
    }
}
