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
//!   `busyhold <pid> <wait_us>`
//!                           a kernel thread takes the space's `busy` flag when
//!                           the pid's exit decides to close, and keeps it until
//!                           the closer is seen waiting on THAT space or
//!                           `wait_us` (<= 5000) plus a fixed 20 ms margin have
//!                           passed: `wait_us` is how long the holder waits for
//!                           the closer, not a hold time. The real maximum hold
//!                           is therefore 5000 us + 20 ms = 25 ms;
//!   `release <pid>`         drop the hold now (teardown runs here if last). For
//!                           a `busyhold` it asks the worker to stop and WAITS
//!                           (at most 500 ms) until the worker has finished and
//!                           freed the slot, so a following arm cannot meet the
//!                           old worker's tail. Also disarms `pause`/`legacy`;
//!   `legacy <pid>`          exit AND reap of that pid use the old Arc-count
//!                           rule (teardown by the last Arc holder, after
//!                           wait4): the permanent negative control. Until
//!                           `release <pid>` or 10 s (ESRCH for an unknown pid);
//!   `pause <pid>`           one shot: between the sole-owner decision and the
//!                           close, exit takes an extra strong reference (a
//!                           racing observer's upgrade), and after the close
//!                           an observer tries the space; `pause_result` is
//!                           1 when it got nothing, 2 when it got the space.
//!                           Cleared when armed; disarmed by `release` or 10 s
//!                           (ESRCH for an unknown pid).
//!   `exitrace <tgid> <n> <w|l>`
//!                           lane lateleader: in `exit`, after the thread's
//!                           space was released and before it turns Zombie, a
//!                           thread of `tgid` waits (no lock held, IRQ window
//!                           and yield) until `n` threads have arrived or 2 s
//!                           passed, so that they become Zombies together.
//!                           `w` = only non-leader threads count and wait, `l`
//!                           = the leader too;
//!   `dropdelay <tgid> <us>` (us <= 2000) the dispatch loop holds the drop of a
//!                           removed member of `tgid` open for `us` after it
//!                           released RUN_QUEUE (the window in which
//!                           `AS_DROP_TGID` is set), and the leader of `tgid`
//!                           waits `us` once before its final yield. Both
//!                           disarm with `release <tgid>` or after 10 s.
//! `busyhold` is gated on the exit itself: the kernel observer takes `busy`
//! (IRQs masked, like real holders) when the pid's exit has decided to close
//! and keeps it until the closer of the SAME space is seen waiting
//! (`busy_waits`, counted per armed space; counted in `overlaps`) or the
//! 25 ms bound above. Also rendered: `cpus <n>`, `armed <0|1>`.
//! One fixed slot: a second arm while it is taken fails with EBUSY. The slot
//! releases itself after 10 s; the release runs on the slot's own kernel
//! thread (task context), never in an IRQ handler. Nothing here is on any hot
//! path: with nothing armed, exit and reap pay one relaxed load of `ANY` and
//! no other code of this file runs.
//!
//! A teardown is recorded when the space's contents are released. For a space
//! shared by `CLONE_VM` sharers in different thread groups (vfork child and
//! parent) the last user does the free, which can be after one group's wait4:
//! no entry is expected at that wait4 for the group that did not free it.
//!

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use mm::vmm::AddressSpace;
use crate::task::Pid;

/// Anything armed that exit/reap must look at (`legacy`, `pause`, `busyhold`):
/// the one relaxed load of an unarmed exit.
static ANY: AtomicBool = AtomicBool::new(false);
static LEGACY_PID: AtomicU32 = AtomicU32::new(0);
static LEGACY_DEADLINE: AtomicU64 = AtomicU64::new(0);
static PAUSE_PID: AtomicU32 = AtomicU32::new(0);
static PAUSE_DEADLINE: AtomicU64 = AtomicU64::new(0);
static PAUSE_RESULT: AtomicU32 = AtomicU32::new(0);
static GATE_PID: AtomicU32 = AtomicU32::new(0);
/// Exit -> holder: take `busy` now. Holder -> exit: it holds it.
static BUSY_GO: AtomicBool = AtomicBool::new(false);
static BUSY_HELD: AtomicBool = AtomicBool::new(false);
/// Address of the space the armed `busyhold` is about (0 = none), and the
/// closers (exit, exec) of THAT space that found `busy` held. A close of any
/// other space neither counts nor ends the hold.
static ARMED_SPACE: AtomicUsize = AtomicUsize::new(0);
static BUSY_WAITS: AtomicU64 = AtomicU64::new(0);
static OVERLAPS: AtomicU64 = AtomicU64::new(0);
// Lane lateleader knobs.
static RACE_PID: AtomicU32 = AtomicU32::new(0);
static RACE_N: AtomicU32 = AtomicU32::new(0);
static RACE_LEADER: AtomicBool = AtomicBool::new(false);
static RACE_ARRIVED: AtomicU32 = AtomicU32::new(0);
static RACE_DEADLINE: AtomicU64 = AtomicU64::new(0);
static DELAY_PID: AtomicU32 = AtomicU32::new(0);
static DELAY_US: AtomicU64 = AtomicU64::new(0);
static DELAY_DEADLINE: AtomicU64 = AtomicU64::new(0);
static DELAY_LEADER_ONCE: AtomicBool = AtomicBool::new(false);

#[inline(always)]
pub fn any_armed() -> bool { ANY.load(Ordering::Relaxed) }

/// Drop expired `legacy`/`pause` arms, then recompute `ANY`. Run by every
/// closing exit while armed, by `render` and by `control`, so a pid that never
/// exits cannot leave `ANY` set past the 10 s deadline.
fn refresh_any() {
    let now = crate::monotonic_ns();
    if LEGACY_PID.load(Ordering::Relaxed) != 0 && now >= LEGACY_DEADLINE.load(Ordering::Relaxed) {
        LEGACY_PID.store(0, Ordering::Relaxed);
    }
    if PAUSE_PID.load(Ordering::Relaxed) != 0 && now >= PAUSE_DEADLINE.load(Ordering::Relaxed) {
        PAUSE_PID.store(0, Ordering::Relaxed);
    }
    if RACE_PID.load(Ordering::Relaxed) != 0 && now >= RACE_DEADLINE.load(Ordering::Relaxed) {
        RACE_PID.store(0, Ordering::Relaxed);
    }
    if DELAY_PID.load(Ordering::Relaxed) != 0 && now >= DELAY_DEADLINE.load(Ordering::Relaxed) {
        DELAY_PID.store(0, Ordering::Relaxed);
    }
    ANY.store(RACE_PID.load(Ordering::Relaxed) != 0 || DELAY_PID.load(Ordering::Relaxed) != 0
        || LEGACY_PID.load(Ordering::Relaxed) != 0 || PAUSE_PID.load(Ordering::Relaxed) != 0
        || GATE_PID.load(Ordering::Relaxed) != 0 || ARMED_SPACE.load(Ordering::Relaxed) != 0, Ordering::Relaxed);
}

/// Is `pid` on the old Arc-count rule (armed by `legacy`, not expired)?
/// Call only after `any_armed()`.
pub fn legacy_is(pid: Pid) -> bool {
    LEGACY_PID.load(Ordering::Relaxed) == pid
        && crate::monotonic_ns() < LEGACY_DEADLINE.load(Ordering::Relaxed)
}

/// A closer of `old` found `busy` held. Counts only for the space the armed
/// `busyhold` is about. Call only after `any_armed()`.
pub fn note_closer_wait(old: &Arc<AddressSpace>) {
    if ARMED_SPACE.load(Ordering::Acquire) == Arc::as_ptr(old) as usize {
        BUSY_WAITS.fetch_add(1, Ordering::Release);
    }
}

/// Exit of `pid` decided to close `old`; run the armed probes. Returns the
/// extra strong reference of a `pause` for [`exit_after_close`]. Call only
/// after `any_armed()`.
pub fn exit_before_close(pid: Pid, old: &Arc<AddressSpace>) -> Option<Arc<AddressSpace>> {
    let mut extra = None;
    if PAUSE_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        && crate::monotonic_ns() < PAUSE_DEADLINE.load(Ordering::Relaxed) {
        PAUSE_RESULT.store(0, Ordering::Relaxed);
        extra = Some(old.clone()); // the racing observer's upgrade
    }
    if GATE_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        BUSY_HELD.store(false, Ordering::Relaxed);
        BUSY_GO.store(true, Ordering::Release);
        let t0 = crate::monotonic_ns();
        while !BUSY_HELD.load(Ordering::Acquire) && crate::monotonic_ns().saturating_sub(t0) < 200_000_000 {
            // Spin briefly (the holder is normally on another CPU and a park
            // costs a tick or more), then park when allowed: the holder may
            // be queued on this very CPU.
            if crate::irqctx::can_sleep() && crate::monotonic_ns().saturating_sub(t0) > 1_000_000 {
                crate::kthread::msleep(1);
            } else { crate::irq_window(); core::hint::spin_loop(); }
        }
    }
    refresh_any();
    extra
}

/// `exitrace`: called by `exit` of `pid` (thread group `tgid`) after its space
/// was released, before it becomes a Zombie. No lock held. Call only after
/// `any_armed()`.
pub fn exit_race(pid: Pid, tgid: Pid) {
    if RACE_PID.load(Ordering::Relaxed) != tgid { return; }
    let now = crate::monotonic_ns();
    if now >= RACE_DEADLINE.load(Ordering::Relaxed) { LL_RACE_SKIP.fetch_add(1, Ordering::Relaxed); return; }
    if pid == tgid && !RACE_LEADER.load(Ordering::Relaxed) { LL_RACE_SKIP.fetch_add(1, Ordering::Relaxed); return; }
    LL_RACE_ENTER.fetch_add(1, Ordering::Relaxed);
    crate::klog_line(format_args!("[llred] race enter pid={} tgid={} arrived={}\n", pid, tgid, RACE_ARRIVED.load(Ordering::Relaxed)));
    RACE_ARRIVED.fetch_add(1, Ordering::AcqRel);
    let n = RACE_N.load(Ordering::Relaxed);
    while RACE_ARRIVED.load(Ordering::Acquire) < n && crate::monotonic_ns().saturating_sub(now) < 2_000_000_000 {
        crate::irq_window();
        crate::yield_now("asfree_exitrace");
    }
    crate::klog_line(format_args!("[llred] race leave pid={} arrived={} n={}\n", pid, RACE_ARRIVED.load(Ordering::Relaxed), n));
    if RACE_ARRIVED.load(Ordering::Acquire) >= n { LL_RACE_FULL.fetch_add(1, Ordering::Relaxed); }
    else { LL_RACE_TIMEOUT.fetch_add(1, Ordering::Relaxed); }
}

/// `dropdelay`: the dispatch loop, with a removed member of `tgid` whose drop
/// is pending (`AS_DROP_TGID` set, RUN_QUEUE released). Call only after
/// `any_armed()`.
pub fn drop_delay(tgid: Pid) {
    if DELAY_PID.load(Ordering::Relaxed) != tgid { return; }
    let t0 = crate::monotonic_ns();
    if t0 >= DELAY_DEADLINE.load(Ordering::Relaxed) { return; }
    let us = DELAY_US.load(Ordering::Relaxed);
    while crate::monotonic_ns().saturating_sub(t0) < us * 1000 {
        mm::paging::tlb_service_pending();
        core::hint::spin_loop();
    }
}

/// `dropdelay`, leader side: once, before the leader's final yield. Task
/// context, no lock held. Call only after `any_armed()`.
pub fn leader_delay(pid: Pid, tgid: Pid) {
    if pid != tgid || DELAY_PID.load(Ordering::Relaxed) != tgid { return; }
    if !DELAY_LEADER_ONCE.swap(false, Ordering::AcqRel) { return; }
    let t0 = crate::monotonic_ns();
    // Half the drop delay: the leader's switch-out lands inside the window.
    let us = DELAY_US.load(Ordering::Relaxed) / 2;
    while crate::monotonic_ns().saturating_sub(t0) < us * 1000 { crate::irq_window(); core::hint::spin_loop(); }
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
/// Largest `wait_us` of `busyhold`.
const BUSYHOLD_MAX_US: u64 = 5000;
/// Margin added to `wait_us` for the closer to arrive after its wake-up. The
/// real maximum hold is `BUSYHOLD_MAX_US` * 1000 + this = 25 ms.
const BUSYHOLD_MARGIN_NS: u64 = 20_000_000;
/// `release` of a busyhold waits at most this long for the worker's tail.
const RELEASE_WAIT_NS: u64 = 500_000_000;

struct Held { gen: u64, pid: Pid, busy: bool, cancel: bool, _arc: Option<Arc<AddressSpace>> }
static SLOT: spin::Mutex<Option<Held>> = spin::Mutex::new(None);
static GEN: AtomicU64 = AtomicU64::new(1);
/// Late-leader accounting (lane llred): reaps/notifies done by the post-drop
/// recheck, and by the settle-at-removal branch (defensive, unreachable for
/// user tasks).
pub static LL_RECHECK_REAPS: AtomicU64 = AtomicU64::new(0);
pub static LL_SETTLE_REAPS: AtomicU64 = AtomicU64::new(0);
pub static LL_RACE_FULL: AtomicU64 = AtomicU64::new(0);
pub static LL_RACE_TIMEOUT: AtomicU64 = AtomicU64::new(0);
pub static LL_RACE_ENTER: AtomicU64 = AtomicU64::new(0);
pub static LL_RACE_SKIP: AtomicU64 = AtomicU64::new(0);
pub static LL_EXIT_REAPS: AtomicU64 = AtomicU64::new(0);

/// Text of the file.
pub fn render(s: &mut alloc::string::String) {
    use core::fmt::Write;
    refresh_any();
    let mut v = alloc::vec::Vec::with_capacity(mm::vmm::ASFREE_RING);
    let total = mm::vmm::asfree_snapshot(&mut v);
    let _ = writeln!(s, "total {}", total);
    let _ = writeln!(s, "cpus {}", crate::active_cpu_count());
    let _ = writeln!(s, "busy_waits {}", BUSY_WAITS.load(Ordering::Relaxed));
    let _ = writeln!(s, "overlaps {}", OVERLAPS.load(Ordering::Relaxed));
    let _ = writeln!(s, "pause_result {}", PAUSE_RESULT.load(Ordering::Relaxed));
    let _ = writeln!(s, "armed {}", any_armed() as u8);
    let _ = writeln!(s, "ll_recheck_reaps {}", LL_RECHECK_REAPS.load(Ordering::Relaxed));
    let _ = writeln!(s, "ll_settle_reaps {}", LL_SETTLE_REAPS.load(Ordering::Relaxed));
    let _ = writeln!(s, "ll_race_full {}", LL_RACE_FULL.load(Ordering::Relaxed));
    let _ = writeln!(s, "ll_race_timeout {}", LL_RACE_TIMEOUT.load(Ordering::Relaxed));
    let _ = writeln!(s, "ll_race_enter {}", LL_RACE_ENTER.load(Ordering::Relaxed));
    let _ = writeln!(s, "ll_race_skip {}", LL_RACE_SKIP.load(Ordering::Relaxed));
    let _ = writeln!(s, "ll_exit_reaps {}", LL_EXIT_REAPS.load(Ordering::Relaxed));
    for (n, t, p, q) in v { let _ = writeln!(s, "{} {} {} {}", n, t, p, q); }
}

fn num(w: Option<&str>) -> Option<u64> { w?.parse().ok() }

/// Run one command; returns 0 or a negative errno.
pub fn control(cmd: &str) -> i32 {
    let mut it = cmd.split_ascii_whitespace();
    let verb = it.next();
    let pid = match num(it.next()) { Some(p) if p > 0 && p <= u32::MAX as u64 => p as Pid, _ => return -22 };
    match verb {
        Some("release") => release(pid),
        Some("hold") => arm(pid, 0),
        Some("legacy") => {
            if crate::task_as(pid).is_none() { return -3; }
            LEGACY_DEADLINE.store(crate::monotonic_ns().saturating_add(AUTO_RELEASE_NS), Ordering::Relaxed);
            LEGACY_PID.store(pid, Ordering::Relaxed);
            ANY.store(true, Ordering::Relaxed);
            0
        }
        Some("pause") => {
            if crate::task_as(pid).is_none() { return -3; }
            PAUSE_RESULT.store(0, Ordering::Relaxed);
            PAUSE_DEADLINE.store(crate::monotonic_ns().saturating_add(AUTO_RELEASE_NS), Ordering::Relaxed);
            PAUSE_PID.store(pid, Ordering::Relaxed);
            ANY.store(true, Ordering::Relaxed);
            0
        }
        Some("exitrace") => {
            if crate::task_as(pid).is_none() { return -3; }
            let n = match num(it.next()) { Some(n) if n >= 1 && n <= 64 => n as u32, _ => return -22 };
            let leader = match it.next() { Some("w") => false, Some("l") => true, _ => return -22 };
            RACE_N.store(n, Ordering::Relaxed);
            RACE_LEADER.store(leader, Ordering::Relaxed);
            RACE_ARRIVED.store(0, Ordering::Relaxed);
            RACE_DEADLINE.store(crate::monotonic_ns().saturating_add(AUTO_RELEASE_NS), Ordering::Relaxed);
            RACE_PID.store(pid, Ordering::Release);
            ANY.store(true, Ordering::Relaxed);
            0
        }
        Some("dropdelay") => {
            if crate::task_as(pid).is_none() { return -3; }
            let us = match num(it.next()) { Some(u) if u >= 1 && u <= 2000 => u, _ => return -22 };
            DELAY_US.store(us, Ordering::Relaxed);
            DELAY_LEADER_ONCE.store(true, Ordering::Relaxed);
            DELAY_DEADLINE.store(crate::monotonic_ns().saturating_add(AUTO_RELEASE_NS), Ordering::Relaxed);
            DELAY_PID.store(pid, Ordering::Release);
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

fn release(pid: Pid) -> i32 {
    // A plain hold is taken out now (dropped after the unlock: this may be the
    // last Arc and a teardown must not run under a spin lock). A busyhold's
    // worker owns the globals until its tail is done: it is asked to stop and
    // waited for, so no new arm can start before that tail has finished.
    let (h, wait_gen) = {
        let mut s = SLOT.lock();
        match s.as_mut() {
            Some(h) if h.pid == pid && h.busy => { h.cancel = true; (None, Some(h.gen)) }
            Some(h) if h.pid == pid => (s.take(), None),
            _ => (None, None),
        }
    };
    if LEGACY_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() { refresh_any(); }
    if PAUSE_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
        PAUSE_RESULT.store(0, Ordering::Relaxed);
        refresh_any();
    }
    if RACE_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() { refresh_any(); }
    if DELAY_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() { refresh_any(); }
    drop(h); // the teardown, if this was the last reference, runs here
    if let Some(g) = wait_gen {
        let t0 = crate::monotonic_ns();
        while slot_is(g) && crate::monotonic_ns().saturating_sub(t0) < RELEASE_WAIT_NS {
            if crate::irqctx::can_sleep() { crate::kthread::msleep(1); }
            else { crate::irq_window(); core::hint::spin_loop(); }
        }
    }
    0
}

fn arm(pid: Pid, wait_us: u64) -> i32 {
    let arc = match crate::task_as(pid) { Some(a) => a, None => return -3 };
    let gen = GEN.fetch_add(1, Ordering::Relaxed);
    {
        let mut s = SLOT.lock();
        if s.is_some() { return -16; }
        *s = Some(Held { gen, pid, busy: wait_us != 0, cancel: false,
            _arc: if wait_us == 0 { Some(arc.clone()) } else { None } });
        // The globals are set while this generation owns the slot, so no
        // worker tail of an older generation can still be touching them.
        if wait_us != 0 {
            BUSY_GO.store(false, Ordering::Relaxed);
            ARMED_SPACE.store(Arc::as_ptr(&arc) as usize, Ordering::Release);
            GATE_PID.store(pid, Ordering::Release);
            ANY.store(true, Ordering::Relaxed);
        }
    }
    let deadline = crate::monotonic_ns().saturating_add(AUTO_RELEASE_NS);
    let spawned = crate::kthread::spawn("asfree_hold", move || worker(gen, pid, arc, wait_us, deadline));
    if spawned.is_none() {
        finish(gen);
        return -12;
    }
    0
}

fn slot_is(gen: u64) -> bool { SLOT.lock().as_ref().map_or(false, |h| h.gen == gen) }
/// Still armed: the slot is this generation's and `release` has not asked it to stop.
fn slot_live(gen: u64) -> bool { SLOT.lock().as_ref().map_or(false, |h| h.gen == gen && !h.cancel) }

/// The end of generation `gen`: while it still owns the SLOT, reset the
/// globals it set, then free the slot (after which a new arm may start). The
/// removed `Held` is dropped after the unlock: it may hold the last Arc, and
/// a teardown must not run under a spin lock.
fn finish(gen: u64) {
    let h = {
        let mut s = SLOT.lock();
        if s.as_ref().map_or(false, |h| h.gen == gen) {
            BUSY_GO.store(false, Ordering::Relaxed);
            GATE_PID.store(0, Ordering::Relaxed);
            ARMED_SPACE.store(0, Ordering::Release);
            refresh_any();
            s.take()
        } else { None }
    };
    drop(h);
}

fn worker(gen: u64, pid: Pid, arc: Arc<AddressSpace>, wait_us: u64, deadline: u64) {
    if wait_us != 0 {
        // Observer-style holder: only a Weak is kept; it takes `busy` when
        // the pid's exit says it is about to close (`exit_before_close`), with
        // IRQs masked as real holders have them, and keeps it until a closer
        // of this space is seen waiting on it or `wait_us` plus the margin
        // passed (25 ms at most).
        let w = Arc::downgrade(&arc);
        drop(arc);
        while !BUSY_GO.load(Ordering::Acquire) && slot_live(gen) && crate::monotonic_ns() < deadline {
            crate::kthread::msleep(1);
        }
        if !BUSY_GO.load(Ordering::Acquire) && GATE_PID.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            // Stopping, but the exit already consumed the gate: it sets
            // BUSY_GO next, and then waits for BUSY_HELD below.
            let t0 = crate::monotonic_ns();
            while !BUSY_GO.load(Ordering::Acquire) && crate::monotonic_ns().saturating_sub(t0) < 200_000_000 {
                crate::irq_window();
                core::hint::spin_loop();
            }
        }
        if BUSY_GO.load(Ordering::Acquire) {
            // IRQs are masked BEFORE the space is taken, so no tick or IPI
            // handler runs between taking `busy` and the hold loop.
            let fl = crate::ksync::irq_save();
            if let Some(a) = crate::acquire_foreign_as(&w, 0) {
                let waits0 = BUSY_WAITS.load(Ordering::Acquire);
                BUSY_HELD.store(true, Ordering::Release);
                let t0 = crate::monotonic_ns();
                let mut overlapped = false;
                while crate::monotonic_ns().saturating_sub(t0) < wait_us * 1000 + BUSYHOLD_MARGIN_NS {
                    if BUSY_WAITS.load(Ordering::Acquire) != waits0 { overlapped = true; break; }
                    mm::paging::tlb_service_pending();
                    core::hint::spin_loop();
                }
                a.lock_epoch.fetch_add(1, Ordering::Relaxed);
                a.busy.store(false, Ordering::Release);
                crate::ksync::irq_restore(fl);
                if overlapped { OVERLAPS.fetch_add(1, Ordering::Relaxed); }
                drop(a);
            } else {
                crate::ksync::irq_restore(fl);
            }
            BUSY_HELD.store(true, Ordering::Release); // never leave the exit waiting
        }
        finish(gen);
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
