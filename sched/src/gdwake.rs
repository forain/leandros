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

static LOG: AtomicBool = AtomicBool::new(false);

/// `sched.wakelog=1`: one serial line per keyed/tagged wake that lands on a
/// task parked with a narrow (<= 2 bits) poll mask. Off by default; the only
/// cost when off is one relaxed load inside the already-rare wake delivery.
#[inline(always)]
pub fn log_enabled() -> bool { LOG.load(Relaxed) }
pub fn set_log(on: bool) { LOG.store(on, Relaxed); }

/// Serial line `[WAKELOG] tid= tgid= mask= tag= key= kind= waker= site=file:line`.
/// `kind`: `key-fp` (delivered on a 16-bit fingerprint match), `key-unrefined`
/// (keyed wake, the bit is explained by unkeyed interests), `bits` (tag-only).
/// Caller holds RUN_QUEUE, so the waker's pid comes from the per-CPU slot.
pub fn log_wake(tid: u32, tgid: u32, mask: u64, tag: u64, key: Option<u64>, kind: &str, waker: u32) {
    extern "C" { fn arch_serial_putc(c: u8); }
    use core::fmt::Write;
    struct S;
    impl Write for S {
        fn write_str(&mut self, m: &str) -> core::fmt::Result {
            for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } }
            Ok(())
        }
    }
    let site = SITE.load(Relaxed);
    let _ = write!(S, "[WAKELOG] tid={} tgid={} mask={:#x} tag={:#x} key={:#x} kind={} waker={} site=", tid, tgid, mask, tag, key.unwrap_or(0), kind, waker);
    site_name(site, &mut |f, l| { let _ = write!(S, "{}:{}", f, l); });
    let _ = write!(S, "\r\n");
}

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

const SITES: usize = 1024;
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

// ── Last poll-set park per thread (exact-key diagnostics) ───────────────────
//
// `(tgid, popcount(mask), popcount(kbits), keyed interests, unkeyed
// interests)` of each thread's most recent `block_on_poll_prepare_set`,
// indexed like `WOKE`. `kbits` far below `mask` with many unkeyed interests
// says which processes still take bloom-collision wakes.
static PARK: [AtomicU64; 1024] = [const { AtomicU64::new(0) }; 1024];
static PARK_PID: [AtomicU32; 1024] = [const { AtomicU32::new(0) }; 1024];

pub fn note_park(pid: u32, tgid: u32, mask: u64, kbits: u64, keyed: u16, unkeyed: u16) {
    if !enabled() { return; }
    let v = (tgid as u64) << 32
        | (mask.count_ones() as u64) << 24
        | (kbits.count_ones() as u64) << 16
        | (keyed.min(255) as u64) << 8
        | unkeyed.min(255) as u64;
    PARK[pid as usize & 1023].store(v, Relaxed);
    PARK_PID[pid as usize & 1023].store(pid, Relaxed);
}

/// Visit `(pid, tgid, mask bits, kbits bits, keyed, unkeyed)` of every
/// recorded park.
pub fn for_each_park(mut f: impl FnMut(u32, u32, u32, u32, u32, u32)) {
    for i in 0..1024 {
        let v = PARK[i].load(Relaxed);
        if v == 0 { continue; }
        f(PARK_PID[i].load(Relaxed), (v >> 32) as u32, (v >> 24 & 0xff) as u32,
          (v >> 16 & 0xff) as u32, (v >> 8 & 0xff) as u32, (v & 0xff) as u32);
    }
}

// ── Keyed-wake collision selftest (`sched.keyselftest=1`, lane wakecatch) ────
//
// A kernel thread parks narrowly on key K (`POLL_WAIT_CHANNEL`, one keyed
// bit). A driver thread then delivers a wake for K2, a DIFFERENT key with
// the same bloom bit and the same 16-bit fingerprint: the victim must stay
// parked (false wake = the 16-bit-fingerprint bug). Then a wake for K must
// wake it (lost wake = a broken fix). One serial line reports the result.
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering as KOrd};

static KST_PARKED: AtomicBool = AtomicBool::new(false);
static KST_WOKE: AtomicU32 = AtomicU32::new(0);
static KST_VICTIM: AtomicU32 = AtomicU32::new(0);

fn kst_keys() -> (u64, u64) {
    let k = crate::poll_key(77, 1234);
    // Same low 6 bits (bloom bit) and bits 32..48 (fingerprint); differs in bit 20.
    (k, k ^ (1 << 20))
}

extern "C" fn kst_victim(_: usize) -> ! {
    let (k, _) = kst_keys();
    let tag = crate::PollTag { bits: crate::poll_key_bit(k), n: 1, keys: [k, 0, 0], nested: 0, foreign: false };
    crate::block_on_poll_prepare_tag(u64::MAX, &tag);
    KST_PARKED.store(true, KOrd::SeqCst);
    crate::block_on_poll_commit();
    KST_WOKE.fetch_add(1, KOrd::SeqCst);
    loop { crate::yield_now("keyselftest"); }
}

fn kst_victim_blocked() -> bool {
    let pid = KST_VICTIM.load(KOrd::SeqCst);
    let mut rq = crate::RUN_QUEUE.lock();
    rq.find_pid_mut(pid).map_or(false, |t| t.state == crate::task::TaskState::Blocked)
}

fn kst_wait(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..200_000 {
        if f() { return true; }
        crate::yield_now("keyselftest");
    }
    false
}

extern "C" fn kst_driver(_: usize) -> ! {
    let (k, k2) = kst_keys();
    let ok_setup = match crate::spawn_with_arg(kst_victim, 0) {
        Some(p) => { KST_VICTIM.store(p as u32, KOrd::SeqCst); true }
        None => false,
    };
    let parked = ok_setup && kst_wait(|| KST_PARKED.load(KOrd::SeqCst)) && kst_wait(kst_victim_blocked);
    let mut false_wake = 0u32;
    let mut lost_wake = 1u32;
    if parked {
        crate::wake_poll_key(k2);
        for _ in 0..2000 { crate::yield_now("keyselftest"); }
        false_wake = KST_WOKE.load(KOrd::SeqCst);
        if false_wake == 0 {
            crate::wake_poll_key(k);
            if kst_wait(|| KST_WOKE.load(KOrd::SeqCst) != 0) { lost_wake = 0; }
        }
    }
    let line = if !parked { "[KEYSELFTEST] result=FAIL setup (victim never parked)\n" }
        else if false_wake != 0 { "[KEYSELFTEST] result=FAIL false_wake=1 (colliding key woke the waiter)\n" }
        else if lost_wake != 0 { "[KEYSELFTEST] result=FAIL lost_wake=1 (real key did not wake the waiter)\n" }
        else { "[KEYSELFTEST] result=PASS false_wake=0 lost_wake=0\n" };
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in line.as_bytes() { unsafe { arch_serial_putc(b) } }
    loop { crate::yield_now("keyselftest"); }
}

/// `sched.keyselftest=1`: spawn the selftest threads (see above).
pub fn key_selftest_start() { let _ = crate::spawn_with_arg(kst_driver, 0); }
