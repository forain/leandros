//! Cache maintenance for streaming DMA on non-coherent aarch64 parts.
//!
//! x86_64 PCIe and QEMU `virt` (its DTB marks the PCIe host `dma-coherent`)
//! snoop the CPU caches, so a DMA sync point is an ordering barrier there.
//! The Pi 5's RP1 sits behind a PCIe root complex that does not: a buffer
//! handed to a device must be cleaned to the point of coherency (`dc cvac`)
//! first, and a buffer a device wrote must be invalidated (`dc ivac`) before
//! the CPU reads it, or the CPU sees stale lines. [`set_coherent`] (from the
//! DTB, overridable by `dma.coherent=`) picks the mode once at boot.
//!
//! Range rule (Linux `dcache_inval_poc`): a partial cache line at either end
//! of an invalidate range is cleaned *and* invalidated (`dc civac`), because
//! bytes outside the buffer share it and plain invalidation would throw away
//! a dirty neighbour; whole lines inside get `dc ivac`.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static NONCOHERENT: AtomicBool = AtomicBool::new(false);
/// Lines touched by the last maintenance call (selftest hook).
static LAST_LINES: AtomicUsize = AtomicUsize::new(0);

/// Platform DMA coherency (default: coherent).
pub fn set_coherent(c: bool) { NONCOHERENT.store(!c, Ordering::Relaxed); }
pub fn coherent() -> bool { !NONCOHERENT.load(Ordering::Relaxed) }
pub fn last_lines() -> usize { LAST_LINES.load(Ordering::Relaxed) }

/// How an invalidate of `[va, va + len)` splits on `line`-byte lines:
/// (head partial line, whole lines `[body.0, body.1)`, tail partial line).
/// The head and tail are line bases, `None` when the range starts or ends on
/// a line boundary; a range inside a single line is a head only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan { pub head: Option<usize>, pub body: (usize, usize), pub tail: Option<usize> }

pub fn plan(va: usize, len: usize, line: usize) -> Plan {
    let empty = Plan { head: None, body: (0, 0), tail: None };
    if len == 0 || line == 0 { return empty; }
    let end = va.saturating_add(len);
    let first = va & !(line - 1);
    let last = (end - 1) & !(line - 1);
    let head_partial = va != first;
    let tail_partial = end != last + line;
    if first == last {
        return if head_partial || tail_partial { Plan { head: Some(first), ..empty } }
               else { Plan { body: (first, first + line), ..empty } };
    }
    let lo = if head_partial { first + line } else { first };
    let hi = if tail_partial { last } else { last + line };
    Plan {
        head: head_partial.then_some(first),
        body: if lo < hi { (lo, hi) } else { (0, 0) },
        tail: tail_partial.then_some(last),
    }
}

/// Number of lines a clean of `[va, va + len)` touches.
pub fn clean_lines(va: usize, len: usize, line: usize) -> usize {
    if len == 0 || line == 0 { return 0; }
    let first = va & !(line - 1);
    let end = (va + len + line - 1) & !(line - 1);
    (end - first) / line
}

/// Smallest D-cache line in bytes (`CTR_EL0.DminLine`, words, log2).
#[cfg(all(target_arch = "aarch64", not(test)))]
pub fn dline() -> usize {
    let ctr: u64;
    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)); }
    4usize << ((ctr >> 16) & 0xF)
}

/// Clean `[va, va + len)` to the point of coherency (to-device), always,
/// whatever the coherency mode (for callers that need it regardless).
#[cfg(all(target_arch = "aarch64", not(test)))]
pub fn clean_range(va: usize, len: usize) {
    let line = dline();
    let n = clean_lines(va, len, line);
    let mut a = va & !(line - 1);
    for _ in 0..n {
        unsafe { core::arch::asm!("dc cvac, {}", in(reg) a, options(nostack, preserves_flags)); }
        a += line;
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)); }
    LAST_LINES.store(n, Ordering::Relaxed);
}

/// Invalidate `[va, va + len)` (from-device); partial edge lines are cleaned
/// and invalidated so neighbouring bytes survive.
#[cfg(all(target_arch = "aarch64", not(test)))]
pub fn invalidate_range(va: usize, len: usize) {
    let line = dline();
    let p = plan(va, len, line);
    let mut n = 0;
    for e in [p.head, p.tail].into_iter().flatten() {
        unsafe { core::arch::asm!("dc civac, {}", in(reg) e, options(nostack, preserves_flags)); }
        n += 1;
    }
    let mut a = p.body.0;
    while a < p.body.1 {
        unsafe { core::arch::asm!("dc ivac, {}", in(reg) a, options(nostack, preserves_flags)); }
        a += line;
        n += 1;
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)); }
    LAST_LINES.store(n, Ordering::Relaxed);
}

/// DMA sync, CPU -> device. Coherent: order prior writes before the doorbell.
#[inline]
pub fn sync_for_device(va: usize, len: usize) {
    #[cfg(all(target_arch = "aarch64", not(test)))]
    {
        if NONCOHERENT.load(Ordering::Relaxed) { return clean_range(va, len); }
        unsafe { core::arch::asm!("dsb st", options(nostack, preserves_flags)); }
    }
    #[cfg(not(all(target_arch = "aarch64", not(test))))]
    { let _ = (va, len); core::sync::atomic::fence(Ordering::SeqCst); }
}

/// DMA sync, device -> CPU. Coherent: order the ownership read before the data.
#[inline]
pub fn sync_for_cpu(va: usize, len: usize) {
    #[cfg(all(target_arch = "aarch64", not(test)))]
    {
        if NONCOHERENT.load(Ordering::Relaxed) { return invalidate_range(va, len); }
        unsafe { core::arch::asm!("dsb ld", options(nostack, preserves_flags)); }
    }
    #[cfg(not(all(target_arch = "aarch64", not(test))))]
    { let _ = (va, len); core::sync::atomic::fence(Ordering::SeqCst); }
}
