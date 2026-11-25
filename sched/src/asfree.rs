//! `/proc/leandros_asfree`: the address-space teardown ring and its
//! fault-injection knobs (lane memexact).
//!
//! Reading shows `total <n>` (teardowns recorded since boot; more than 256
//! newer than the reader's baseline means the ring wrapped) and one
//! `<seq> <tgid> <pages_freed> <pt_pages_freed>` row per entry, oldest first.
//!
//! Writes (root only, checked by the VFS), one command per write:
//!   `hold <pid>`            the kernel keeps a strong `Arc` of pid's space
//!                           (without `busy`), so its teardown cannot run;
//!   `busyhold <pid> <us>`   a kernel thread takes the space's `busy` flag and
//!                           holds it `us` microseconds (<= 5000), then drops;
//!   `release <pid>`         drop the hold now (teardown runs here if last);
//!   `legacy <pid>`          exit AND reap of that pid use the old Arc-count
//!                           rule (teardown by the last Arc holder, after
//!                           wait4): the permanent negative control. Until
//!                           `release <pid>` or 10 s;
//!   `pause <pid>`           one shot: between the sole-owner decision and the
//!                           close, exit takes an extra strong reference (a
//!                           racing observer's upgrade), and after the close
//!                           an observer tries the space; `pause_result` is
//!                           1 when it got nothing, 2 when it got the space.
//! `busyhold` is gated on the exit itself: the kernel observer takes `busy`
//! (IRQs masked, like real holders) when the pid's exit has decided to close
//! and keeps it until the closer is seen waiting (`busy_waits` rose, counted
//! in `overlaps`) or `us` microseconds pass. Also rendered: `cpus <n>`.
//! One fixed slot: a second arm while it is taken fails with EBUSY. The slot
//! releases itself after 10 s; the release runs on the slot's own kernel
//! thread (task context), never in an IRQ handler. Nothing here is on any hot
//! path: with the slot empty no code of this file runs.
//!

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use mm::vmm::AddressSpace;
use crate::task::Pid;

/// Anything armed that exit/reap must look at (`legacy`, `pause`, `busyhold`):
/// the one relaxed load of an unarmed exit.
static ANY: AtomicBool = AtomicBool::new(false);
static LEGACY_PID: AtomicU32 = AtomicU32::new(0);
static LEGACY_DEADLINE: AtomicU64 = AtomicU64::new(0);
static PAUSE_PID: AtomicU32 = AtomicU32::new(0);
static PAUSE_RESULT: AtomicU32 = AtomicU32::new(0);
static GATE_PID: AtomicU32 = AtomicU32::new(0);
/// Exit -> holder: take `busy` now. Holder -> exit: it holds it.
static BUSY_GO: AtomicBool = AtomicBool::new(false);
static BUSY_HELD: AtomicBool = AtomicBool::new(false);
/// Closers (exit, exec) that found `busy` held; holders that were still
/// holding when one arrived.
pub static BUSY_WAITS: AtomicU64 = AtomicU64::new(0);
static OVERLAPS: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
pub fn any_armed() -> bool { ANY.load(Ordering::Relaxed) }

fn refresh_any() {
    ANY.store(LEGACY_PID.load(Ordering::Relaxed) != 0 || PAUSE_PID.load(Ordering::Relaxed) != 0
        || GATE_PID.load(Ordering::Relaxed) != 0, Ordering::Relaxed);
}

/// Is `pid` on the old Arc-count rule (armed by `legacy`, not expired)?
/// Call only after `any_armed()`.
pub fn legacy_is(pid: Pid) -> bool {
    LEGACY_PID.load(Ordering::Relaxed) == pid
        && crate::monotonic_ns() < LEGACY_DEADLINE.load(Ordering::Relaxed)
}

/// Exit of `pid` decided to close `old`; run the armed probes. Returns the
/// extra strong reference of a `pause` for [`exit_after_close`]. Call only
/// after `any_armed()`.
pub fn exit_before_close(pid: Pid, old: &Arc<AddressSpace>) -> Option<Arc<AddressSpace>> {
    let mut extra = None;
    if PAUSE_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        PAUSE_RESULT.store(0, Ordering::Relaxed);
        extra = Some(old.clone()); // the racing observer's upgrade
    }
    if GATE_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        BUSY_HELD.store(false, Ordering::Relaxed);
        BUSY_GO.store(true, Ordering::Release);
        let t0 = crate::monotonic_ns();
        while !BUSY_HELD.load(Ordering::Acquire) && crate::monotonic_ns().saturating_sub(t0) < 200_000_000 {
            // Park when allowed: the holder may be queued on this very CPU.
            if crate::irqctx::can_sleep() { crate::kthread::msleep(1); } else { crate::irq_window(); core::hint::spin_loop(); }
        }
    }
    refresh_any();
    extra
}

/// After the close: the paused observer tries the space and must get nothing.
pub fn exit_after_close(pid: Pid, extra: Option<Arc<AddressSpace>>) {
    if let Some(e) = extra {
        let w = Arc::downgrade(&e);
        match crate::acquire_foreign_as(&w, pid) {
            None => PAUSE_RESULT.store(1, Ordering::Relaxed),
            Some(a) => {
                a.lock_epoch.fetch_add(1, Ordering::Relaxed);
                a.busy.store(false, Ordering::Release);
                PAUSE_RESULT.store(2, Ordering::Relaxed);
            }
        }
        drop(e);
    }
}

const AUTO_RELEASE_NS: u64 = 10_000_000_000;
const BUSYHOLD_MAX_US: u64 = 5000;

struct Held { gen: u64, pid: Pid, _arc: Option<Arc<AddressSpace>> }
static SLOT: spin::Mutex<Option<Held>> = spin::Mutex::new(None);
static GEN: AtomicU64 = AtomicU64::new(1);

/// Text of the file.
pub fn render(s: &mut alloc::string::String) {
    use core::fmt::Write;
    let mut v = alloc::vec::Vec::with_capacity(mm::vmm::ASFREE_RING);
    let total = mm::vmm::asfree_snapshot(&mut v);
    let _ = writeln!(s, "total {}", total);
    let _ = writeln!(s, "cpus {}", crate::active_cpu_count());
    let _ = writeln!(s, "busy_waits {}", BUSY_WAITS.load(Ordering::Relaxed));
    let _ = writeln!(s, "overlaps {}", OVERLAPS.load(Ordering::Relaxed));
    let _ = writeln!(s, "pause_result {}", PAUSE_RESULT.load(Ordering::Relaxed));
    for (n, t, p, q) in v { let _ = writeln!(s, "{} {} {} {}", n, t, p, q); }
}

fn num(w: Option<&str>) -> Option<u64> { w?.parse().ok() }

/// Run one command; returns 0 or a negative errno.
pub fn control(cmd: &str) -> i32 {
    let mut it = cmd.split_ascii_whitespace();
    let verb = it.next();
    let pid = match num(it.next()) { Some(p) if p > 0 && p <= u32::MAX as u64 => p as Pid, _ => return -22 };
    match verb {
        Some("release") => {
            let h = { let mut s = SLOT.lock(); if s.as_ref().map_or(false, |h| h.pid == pid) { s.take() } else { None } };
            if LEGACY_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() { refresh_any(); }
            drop(h); // the teardown, if this was the last reference, runs here
            0
        }
        Some("hold") => arm(pid, 0),
        Some("legacy") => {
            LEGACY_DEADLINE.store(crate::monotonic_ns().saturating_add(AUTO_RELEASE_NS), Ordering::Relaxed);
            LEGACY_PID.store(pid, Ordering::Relaxed);
            ANY.store(true, Ordering::Relaxed);
            0
        }
        Some("pause") => {
            PAUSE_PID.store(pid, Ordering::Relaxed);
            ANY.store(true, Ordering::Relaxed);
            0
        }
        Some("busyhold") => match num(it.next()) {
            Some(us) if us >= 1 && us <= BUSYHOLD_MAX_US => arm(pid, us),
            _ => -22,
        },
        _ => -22,
    }
}

fn arm(pid: Pid, busy_us: u64) -> i32 {
    let arc = match crate::task_as(pid) { Some(a) => a, None => return -3 };
    let gen = GEN.fetch_add(1, Ordering::Relaxed);
    {
        let mut s = SLOT.lock();
        if s.is_some() { return -16; }
        *s = Some(Held { gen, pid, _arc: if busy_us == 0 { Some(arc.clone()) } else { None } });
    }
    if busy_us != 0 {
        BUSY_GO.store(false, Ordering::Relaxed);
        GATE_PID.store(pid, Ordering::Release);
        ANY.store(true, Ordering::Relaxed);
    }
    let deadline = crate::monotonic_ns().saturating_add(AUTO_RELEASE_NS);
    let spawned = crate::kthread::spawn("asfree_hold", move || worker(gen, arc, busy_us, deadline));
    if spawned.is_none() {
        // Taken out and dropped after the unlock: this may be the last Arc,
        // and a teardown must not run under a spin lock.
        let h = { let mut s = SLOT.lock(); if s.as_ref().map_or(false, |h| h.gen == gen) { s.take() } else { None } };
        drop(h);
        GATE_PID.store(0, Ordering::Relaxed);
        refresh_any();
        return -12;
    }
    0
}

fn slot_is(gen: u64) -> bool { SLOT.lock().as_ref().map_or(false, |h| h.gen == gen) }

fn worker(gen: u64, arc: Arc<AddressSpace>, busy_us: u64, deadline: u64) {
    if busy_us != 0 {
        // Observer-style holder: only a Weak is kept; it takes `busy` when
        // the pid's exit says it is about to close (`exit_before_close`), with
        // IRQs masked as real holders have them, and keeps it until the closer
        // is seen waiting on it or `busy_us` passed.
        let w = Arc::downgrade(&arc);
        drop(arc);
        while !BUSY_GO.load(Ordering::Acquire) && slot_is(gen) && crate::monotonic_ns() < deadline {
            crate::kthread::msleep(1);
        }
        if BUSY_GO.load(Ordering::Acquire) {
            if let Some(a) = crate::acquire_foreign_as(&w, 0) {
                let waits0 = BUSY_WAITS.load(Ordering::Acquire);
                let fl = crate::ksync::irq_save();
                BUSY_HELD.store(true, Ordering::Release);
                let t0 = crate::monotonic_ns();
                let mut overlapped = false;
                while crate::monotonic_ns().saturating_sub(t0) < busy_us * 1000 {
                    if BUSY_WAITS.load(Ordering::Acquire) != waits0 { overlapped = true; break; }
                    mm::paging::tlb_service_pending();
                    core::hint::spin_loop();
                }
                a.lock_epoch.fetch_add(1, Ordering::Relaxed);
                a.busy.store(false, Ordering::Release);
                crate::ksync::irq_restore(fl);
                if overlapped { OVERLAPS.fetch_add(1, Ordering::Relaxed); }
                drop(a);
            }
            BUSY_HELD.store(true, Ordering::Release); // never leave the exit waiting
        }
        BUSY_GO.store(false, Ordering::Relaxed);
        GATE_PID.store(0, Ordering::Relaxed);
        refresh_any();
        let h = { let mut s = SLOT.lock(); if s.as_ref().map_or(false, |h| h.gen == gen) { s.take() } else { None } };
        drop(h);
        return;
    }
    drop(arc); // the slot holds the strong reference, not this copy
    loop {
        crate::kthread::msleep(5);
        let mut s = SLOT.lock();
        match s.as_ref() {
            Some(h) if h.gen == gen => {
                if crate::monotonic_ns() >= deadline {
                    let h = s.take();
                    drop(s);
                    drop(h); // task context: the teardown may run here
                    return;
                }
            }
            _ => return, // released explicitly
        }
    }
}
