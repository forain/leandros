//! Held `spin::Mutex` accounting (lane pools, 2026-10-19).
//!
//! `might_sleep()` used to see only the locks that announce themselves: a
//! `KSpinLock` (`atomic_depth`), a `TrackedMutex` (lockwatch), the AS busy
//! flag. A plain `spin::Mutex` held across a park was invisible, so a sleep
//! with one held went unreported until it deadlocked something. The vendored
//! `spin` crate (vendor/spin, LEANDROS-PATCH.md) now calls [`hook`] on every
//! `SpinMutex` acquire and guard drop, and this module keeps a per-CPU count:
//!
//! * `irqctx::sleep_forbidden_reason` reports it (`might_sleep` logs the
//!   call site once);
//! * `preempt_check` defers a tick preemption while it is non-zero (a holder
//!   switched out by the tick would leave every other task on this CPU that
//!   wants the lock spinning out its whole slice);
//! * `yield_now` — every voluntary switch — logs a switch made with one held
//!   (once per reason) and carries the count with the task across the switch,
//!   so the CPU the task leaves is not left looking atomic and the CPU it
//!   resumes on balances the eventual drop.
//!
//! An IRQ handler's acquire/release pairs are balanced inside the handler,
//! so the count is exact per CPU for everything that does not sleep.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use crate::MAX_CPUS;

static HELD: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// Voluntary switches made while a `spin::Mutex` was held (all reasons).
static SWITCH_VIOLATIONS: AtomicU32 = AtomicU32::new(0);

#[inline(always)]
fn cpu() -> usize { (unsafe { crate::cpu_id() }).min(MAX_CPUS - 1) }

/// The accounting step of the hook, callable from outside (kmod `kapi_spin_held`).
pub fn account(acquired: bool) {
    let c = &HELD[cpu()];
    if acquired {
        c.fetch_add(1, Ordering::Relaxed);
    } else {
        // A guard taken before the hook was installed drops after it: the
        // count must saturate at zero rather than wrap.
        let v = c.load(Ordering::Relaxed);
        if v != 0 { c.store(v - 1, Ordering::Relaxed); }
    }
}

/// Install the hook. Called once, early in boot (before any task runs).
pub fn init() {
    spin::held::set_hook(account);
    // Self-check, one boot line: the count must read 1 under a held lock and
    // return to where it was after the drop.
    static PROBE: spin::Mutex<()> = spin::Mutex::new(());
    let before = held();
    let inside = { let _g = PROBE.lock(); held() };
    let after = held();
    crate::irqctx::raw_log(if inside == before + 1 && after == before {
        "[KSYNC] spin::Mutex held-count accounting on\n"
    } else {
        "[KSYNC] spin::Mutex held-count accounting BROKEN (self-check failed)\n"
    });
}

/// Plain `spin::Mutex`es held on this CPU right now.
#[inline]
pub fn held() -> u32 { HELD[cpu()].load(Ordering::Relaxed) }

/// Sum of the per-CPU counts (`/proc/lockstat` `spin_held` line). Holds are
/// short, so a quiet system reads near zero; a count that only grows is a
/// leaked acquire (an unbalanced forget/force_unlock), which leaves its CPU
/// looking atomic to `might_sleep` for good.
pub fn held_total() -> u32 { HELD.iter().map(|c| c.load(Ordering::Relaxed)).sum() }

/// Voluntary switches made with a `spin::Mutex` held, since boot.
pub fn switch_violations() -> u32 { SWITCH_VIOLATIONS.load(Ordering::Relaxed) }

const REASONS: usize = 32;
static REPORTED: [AtomicUsize; REASONS] = [const { AtomicUsize::new(0) }; REASONS];

/// Called by `yield_now` before switching away: takes this CPU's count (the
/// task carries it on its own stack) and reports the switch once per reason.
#[inline]
pub(crate) fn switch_out(reason: &str) -> u32 {
    let c = &HELD[cpu()];
    if c.load(Ordering::Relaxed) == 0 { return 0; }
    let n = c.swap(0, Ordering::Relaxed);
    if n == 0 { return 0; }
    SWITCH_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    let key = reason.as_ptr() as usize;
    for s in REPORTED.iter() {
        let v = s.load(Ordering::Relaxed);
        if v == key { return n; }
        if v == 0 && s.compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            crate::irqctx::raw_log("[KSYNC] task switched out holding ");
            crate::irqctx::raw_dec(n as u64);
            crate::irqctx::raw_log(" spin::Mutex (yield reason: ");
            crate::irqctx::raw_log(reason);
            crate::irqctx::raw_log(", pid ");
            crate::irqctx::raw_dec(crate::current_pid() as u64);
            crate::irqctx::raw_log(")\n");
            return n;
        }
    }
    n
}

/// Called by `yield_now` once the task runs again (possibly on another CPU).
#[inline]
pub(crate) fn switch_in(n: u32) {
    if n != 0 { HELD[cpu()].fetch_add(n, Ordering::Relaxed); }
}
