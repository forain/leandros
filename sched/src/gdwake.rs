//! Wake attribution for parked pollers: WHO made a poll-channel waiter Ready.
//!
//! `idlestat` counts dispatches per thread and wake calls per site, but not
//! the pairing that matters for an idle event loop: a poller woken by site X
//! that then re-probes, finds nothing ready and parks again is a wake the
//! kernel spent for nothing (a spurious wake), while one that returns events
//! to user space is real work. `unblock_port_tagged` and the deadline service
//! stamp each woken task with its waker here; the epoll/poll loops in
//! `kernel::syscall` read the stamp back after the park and classify the wake.
//! Runtime gated by the kernel command line knob `sched.wakestat=1` (see
//! `set_enabled`): off, every hook is one relaxed load of a never-written
//! flag. On, the per-(waker site, tgid) useful/spurious table below is read
//! through `/proc/wakestat` (and the per-thread `[GD*]` serial tables in
//! `kernel::syscall` are kept, but printed only when `GD_SERIAL` is set).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

static ON: AtomicBool = AtomicBool::new(false);

#[inline(always)]
pub fn enabled() -> bool { ON.load(Relaxed) }

/// Turn the census on (boot, from `sched.wakestat=1`, before user space).
pub fn set_enabled(on: bool) { ON.store(on, Relaxed); }

/// The waker of the task in slot `pid & 1023`: a `&'static Location` address
/// for a tagged wake, or one of the small codes below.
static WOKE: [AtomicU64; 1024] = [const { AtomicU64::new(0) }; 1024];
/// The site of the unblock currently running under RUN_QUEUE (set by the
/// caller while it holds the lock; reset to `UNATTRIBUTED` after each use).
pub static SITE: AtomicU64 = AtomicU64::new(UNATTRIBUTED);

pub const DL_OWN: u64 = 1;
pub const DL_TFD: u64 = 2;
pub const UNATTRIBUTED: u64 = 3;

#[inline]
pub fn set_site(loc: &'static core::panic::Location<'static>) {
    if enabled() { SITE.store(loc as *const _ as u64, Relaxed); }
}

#[inline]
pub fn mark(pid: u32, v: u64) {
    if enabled() { WOKE[pid as usize & 1023].store(v, Relaxed); }
}

/// The waker recorded for `pid` since its last park, cleared (0 = none).
#[inline]
pub fn take(pid: u32) -> u64 {
    if !enabled() { return 0; }
    WOKE[pid as usize & 1023].swap(0, Relaxed)
}

// ── Useful vs spurious wakes per (waker site, tgid) ─────────────────────────
//
// Filled by the epoll/poll loops after each park (`note_outcome`): `useful` =
// the loop then reported something (or timed out on its own deadline),
// `spurious` = it re-probed, found nothing and parked again. Cumulative; the
// table holds the first `SITES` (site, tgid) pairs seen and counts the rest
// in `OUTCOME_OVERFLOW`.

const SITES: usize = 128;
static SITE_KEY: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_TG: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];
static SITE_USE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_SPUR: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static OUTCOME_OVERFLOW: AtomicU64 = AtomicU64::new(0);

pub fn note_outcome(site: u64, tg: u32, useful: bool) {
    if !enabled() || site == 0 { return; }
    let h = ((site ^ (tg as u64).wrapping_mul(0x9E37_79B9)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 57) as usize % SITES;
    for k in 0..SITES {
        let i = (h + k) % SITES;
        let cur = SITE_KEY[i].load(Relaxed);
        if cur == 0 && SITE_KEY[i].compare_exchange(0, site, Relaxed, Relaxed).is_ok() {
            SITE_TG[i].store(tg, Relaxed);
        }
        if SITE_KEY[i].load(Relaxed) == site && SITE_TG[i].load(Relaxed) == tg {
            if useful { SITE_USE[i].fetch_add(1, Relaxed); } else { SITE_SPUR[i].fetch_add(1, Relaxed); }
            return;
        }
    }
    OUTCOME_OVERFLOW.fetch_add(1, Relaxed);
}

/// A waker code as text: a `Location` (`file:line`) or one of the codes.
pub fn site_name(site: u64, out: &mut dyn FnMut(&str, u32)) {
    match site {
        DL_OWN => out("deadline", 0),
        DL_TFD => out("timerfd", 0),
        UNATTRIBUTED => out("other", 0),
        _ => {
            // SAFETY: every other value stored by `set_site` is the address
            // of a `&'static Location`.
            let loc = unsafe { &*(site as *const core::panic::Location<'static>) };
            out(loc.file(), loc.line())
        }
    }
}

/// Visit `(site, tgid, useful, spurious)` for every recorded pair.
pub fn for_each_outcome(mut f: impl FnMut(u64, u32, u64, u64)) {
    for i in 0..SITES {
        let k = SITE_KEY[i].load(Relaxed);
        if k == 0 { continue; }
        f(k, SITE_TG[i].load(Relaxed), SITE_USE[i].load(Relaxed), SITE_SPUR[i].load(Relaxed));
    }
}

pub fn outcome_overflow() -> u64 { OUTCOME_OVERFLOW.load(Relaxed) }
