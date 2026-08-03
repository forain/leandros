//! Gap-2 instrumentation: why does a memfd + MAP_SHARED wl_shm pool that is
//! repainted in place show the compositor stale pixels forever?
//!
//! Compile-time gated, UART-direct serial tracing shared by `kernel`,
//! `servers/vfs` and `mm::vmm` — `mm` is the only crate all three depend on.
//! Modeled on `drivers::drm_device_interface::DRM_STATS`: output goes straight
//! to the UART via `arch_serial_putc`, bypassing CONSOLE_OUT_LOCK and the
//! framebuffer console, so it is callable from IRQ context and from under leaf
//! spinlocks without deadlocking or painting over the live desktop.
//!
//! Everything here prints integers and kernel-side byte slices ONLY. No call in
//! this module may ever be handed a user pointer.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Master switch. Flip to `true` to trace, rebuild release, run the session.
pub const ON: bool = false;

/// Also trace `AddressSpace::map_shared_frames`. That site runs with the
/// address space `busy` flag held; the information is redundant with `[G2MAP]`.
/// Leave `false` unless `[G2MAP]` comes back inconclusive.
pub const MAP_SHARED_FRAMES_TRACE: bool = false;

/// Hard ceiling on event lines, so a mis-aimed filter cannot flood a session.
/// The 0.5 Hz `[G2SUM]` sampler does NOT draw from this budget.
static BUDGET: AtomicUsize = AtomicUsize::new(400);

/// Number of pools the `[G2SUM]` sampler can checksum concurrently.
///
/// Slot 0 is RESERVED for the primary pool (the applet's, armed by name from
/// `sys_memfd_create`). Slots 1.. are filled by `set_watch` for secondary pools
/// (the panel's own bar pools, armed by size from `vmo_acquire_frames`). The
/// reservation matters because arming order is not guaranteed: without it, three
/// bar pools appearing first would starve the applet out of the watch set.
pub const WATCH_SLOTS: usize = 4;

/// tmpfs slot indices of the pools being watched by the `[G2SUM]` sampler.
/// `usize::MAX` = slot free.
static WATCH: [AtomicUsize; WATCH_SLOTS] =
    [const { AtomicUsize::new(usize::MAX) }; WATCH_SLOTS];

/// True at most `BUDGET` times, and only when `ON`. Every event site calls this
/// as its gate so the budget is shared across all of them.
pub fn budget_ok() -> bool {
    if !ON { return false; }
    BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed,
                      |v| if v == 0 { None } else { Some(v - 1) })
        .is_ok()
}

/// tmpfs idx watched by slot `s`, or `usize::MAX` when the slot is free.
pub fn watch_slot(s: usize) -> usize {
    if s >= WATCH_SLOTS { return usize::MAX; }
    WATCH[s].load(Ordering::Relaxed)
}

/// True once at least one pool is armed.
pub fn watch_any() -> bool {
    (0..WATCH_SLOTS).any(|s| WATCH[s].load(Ordering::Relaxed) != usize::MAX)
}

/// Arm the PRIMARY pool (slot 0). First writer wins: the applet's pool is the
/// first `leandros-applet` memfd.
pub fn set_watch_primary(idx: usize) {
    let _ = WATCH[0].compare_exchange(usize::MAX, idx,
                                      Ordering::Relaxed, Ordering::Relaxed);
}

/// Arm a SECONDARY pool in the first free slot of 1.., ignoring duplicates.
/// Silently does nothing once the slots are full — a truncated watch set is
/// always preferable to evicting a pool the run depends on.
pub fn set_watch(idx: usize) {
    for s in 0..WATCH_SLOTS {
        if WATCH[s].load(Ordering::Relaxed) == idx { return; }
    }
    for s in 1..WATCH_SLOTS {
        if WATCH[s].compare_exchange(usize::MAX, idx,
                                     Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            return;
        }
    }
}

extern "C" { fn arch_serial_putc(c: u8); }

pub fn s(m: &str) { for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } } }

/// Print a KERNEL-side byte slice (a path buffer, a memfd name). Never a user
/// pointer — the caller must have copied it into kernel memory already.
pub fn bytes(b: &[u8]) {
    for &c in b {
        let c = if (0x20..0x7f).contains(&c) { c } else { b'.' };
        unsafe { arch_serial_putc(c) }
    }
}

/// Compact hex, no leading zeros (keeps lines short on a polled UART).
pub fn h(v: usize) {
    s("0x");
    let mut started = false;
    for i in (0..16).rev() {
        let d = ((v >> (i * 4)) & 0xF) as u8;
        if d != 0 { started = true; }
        if started || i == 0 {
            unsafe { arch_serial_putc(if d < 10 { b'0' + d } else { b'a' + d - 10 }) }
        }
    }
}

pub fn kv(k: &str, v: usize) { s(k); h(v); }
pub fn nl() { s("\n"); }
