//! A sleeping, FIFO mutex with hold/wait accounting, for locks held across
//! slow synchronous device I/O (`F2FS_MOUNTS`). Lane fsio, 2026-10-09.
//!
//! HISTORY. lane nvstall (boot6 -> boot7) replaced `F2FS_MOUNTS`' IRQ-off
//! `spin::Mutex` with a fair ticket spinlock whose waiters opened interrupt
//! windows. That cured the deaf CPUs (`[WDOG] took no timer tick`) but not
//! the real cost: on the PC's USB 2.0 stick a holder can keep the lock for
//! seconds (boot7: 40458 contended waits, avg 16 ms, max hold 18.9 s), and
//! every waiter kept its CPU spinning with preemption disabled for all of it.
//! With three of four CPUs parked in that spin, nothing else could run: the
//! COSMIC session took minutes and even VT2's shell echoed ~1 s late.
//!
//! WHAT. Waiters now SLEEP (`ksync::KRawMutex`: FIFO hand-off, the waiter is
//! parked in the scheduler, its CPU runs other tasks). The holder may sleep
//! too (the USB poll loop parks between polls once a command has run longer
//! than a short spin budget), so the lock is never held by a spinning CPU for
//! long. A caller that may not sleep (boot, IRQ, holding a spinlock or with
//! preemption disabled) degrades to the old behaviour: spin with interrupt
//! windows, and `ksync` logs `[KSYNC] sleep in atomic context` once for that
//! call site — the debug assertion for "page fault / syscall path holds a
//! spinlock across filesystem I/O".
//!
//! The holder is NOT published to `lockwatch` as a held lock: lockwatch's
//! holder slots are per CPU (a sleeping holder migrates), and a tracked
//! holder forbids sleeping (`irqctx::sleep_forbidden_reason`), which would
//! turn every nested sleep (USB disk lock, poll-loop park) back into a spin.
//!
//! DIAGNOSTICS (printed only after the lock is released — printing on this
//! kernel can cost milliseconds per line at 115200 baud on a real UART, and
//! lane nvstall's `[LOCKWAIT]` line was written while the reporter already
//! owned the lock, extending the very hold it reported):
//!   * `[LOCKWAIT] <name>: ...` for a wait >= `LONG_WAIT_NS` (1/s at most);
//!   * `[LOCKHOLD] <name>: ...` for a hold >= `LONG_HOLD_NS` (1/s at most),
//!     naming the holder's syscall (or fault/kernel) — what held it so long;
//!   * counters for a periodic summary (`stats`, `take_window`).

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::ksync::KRawMutex;

/// A wait this long is reported as `[LOCKWAIT]`.
pub const LONG_WAIT_NS: u64 = 500_000_000;
/// A hold this long is reported as `[LOCKHOLD]`.
pub const LONG_HOLD_NS: u64 = 500_000_000;

pub struct IoMutex<T> {
    raw: KRawMutex,
    name: &'static str,
    /// Holder's pid and the `monotonic_ns` it took the lock at (diagnostics).
    holder_pid: AtomicU32,
    hold_since: AtomicU64,
    /// Since boot.
    acquires: AtomicU64,
    waits: AtomicU64,
    wait_ns: AtomicU64,
    wait_max_ns: AtomicU64,
    hold_max_ns: AtomicU64,
    /// Since the last `take_window` (periodic summary).
    win_acquires: AtomicU64,
    win_waits: AtomicU64,
    win_wait_ns: AtomicU64,
    win_wait_max_ns: AtomicU64,
    win_hold_ns: AtomicU64,
    win_hold_max_ns: AtomicU64,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for IoMutex<T> {}
unsafe impl<T: Send> Send for IoMutex<T> {}

pub struct IoGuard<'a, T> {
    m: &'a IoMutex<T>,
    /// How long this acquisition waited (0 = uncontended).
    waited: u64,
}

/// `(waits, total wait ns, max wait ns, max hold ns)` since boot.
pub type IoLockStats = (u64, u64, u64, u64);

/// One summary window: acquisitions, contended waits, total wait, max wait,
/// total hold, max hold (ns).
#[derive(Clone, Copy, Default)]
pub struct IoLockWindow {
    pub acquires: u64,
    pub waits: u64,
    pub wait_ns: u64,
    pub wait_max_ns: u64,
    pub hold_ns: u64,
    pub hold_max_ns: u64,
}

static LAST_WAIT_REPORT_NS: AtomicU64 = AtomicU64::new(0);
static LAST_HOLD_REPORT_NS: AtomicU64 = AtomicU64::new(0);
static SUPPRESSED_WAIT: AtomicU32 = AtomicU32::new(0);
static SUPPRESSED_HOLD: AtomicU32 = AtomicU32::new(0);

impl<T> IoMutex<T> {
    /// `_id` is kept for source compatibility with the lockwatch-tracked
    /// spinning version; a sleeping lock is deliberately not tracked there.
    pub const fn new(_id: u8, name: &'static str, value: T) -> Self {
        Self {
            raw: KRawMutex::new(),
            name,
            holder_pid: AtomicU32::new(0),
            hold_since: AtomicU64::new(0),
            acquires: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
            wait_max_ns: AtomicU64::new(0),
            hold_max_ns: AtomicU64::new(0),
            win_acquires: AtomicU64::new(0),
            win_waits: AtomicU64::new(0),
            win_wait_ns: AtomicU64::new(0),
            win_wait_max_ns: AtomicU64::new(0),
            win_hold_ns: AtomicU64::new(0),
            win_hold_max_ns: AtomicU64::new(0),
            data: UnsafeCell::new(value),
        }
    }

    #[inline]
    #[track_caller]
    pub fn lock(&self) -> IoGuard<'_, T> {
        let mut waited = 0u64;
        if !self.raw.try_lock() {
            let t0 = crate::monotonic_ns();
            self.raw.lock();
            waited = crate::monotonic_ns().saturating_sub(t0).max(1);
            self.waits.fetch_add(1, Ordering::Relaxed);
            self.wait_ns.fetch_add(waited, Ordering::Relaxed);
            self.wait_max_ns.fetch_max(waited, Ordering::Relaxed);
            self.win_waits.fetch_add(1, Ordering::Relaxed);
            self.win_wait_ns.fetch_add(waited, Ordering::Relaxed);
            self.win_wait_max_ns.fetch_max(waited, Ordering::Relaxed);
        }
        self.acquires.fetch_add(1, Ordering::Relaxed);
        self.win_acquires.fetch_add(1, Ordering::Relaxed);
        self.hold_since.store(crate::monotonic_ns(), Ordering::Relaxed);
        self.holder_pid.store(crate::current_pid(), Ordering::Relaxed);
        IoGuard { m: self, waited }
    }

    /// Take the lock only if it is free right now (any context).
    pub fn try_lock(&self) -> Option<IoGuard<'_, T>> {
        if !self.raw.try_lock() { return None; }
        self.acquires.fetch_add(1, Ordering::Relaxed);
        self.win_acquires.fetch_add(1, Ordering::Relaxed);
        self.hold_since.store(crate::monotonic_ns(), Ordering::Relaxed);
        self.holder_pid.store(crate::current_pid(), Ordering::Relaxed);
        Some(IoGuard { m: self, waited: 0 })
    }

    /// Counters for summaries and tests (since boot).
    pub fn stats(&self) -> IoLockStats {
        (self.waits.load(Ordering::Relaxed), self.wait_ns.load(Ordering::Relaxed),
         self.wait_max_ns.load(Ordering::Relaxed), self.hold_max_ns.load(Ordering::Relaxed))
    }

    /// Counters since the previous call (for a periodic summary line).
    pub fn take_window(&self) -> IoLockWindow {
        IoLockWindow {
            acquires: self.win_acquires.swap(0, Ordering::Relaxed),
            waits: self.win_waits.swap(0, Ordering::Relaxed),
            wait_ns: self.win_wait_ns.swap(0, Ordering::Relaxed),
            wait_max_ns: self.win_wait_max_ns.swap(0, Ordering::Relaxed),
            hold_ns: self.win_hold_ns.swap(0, Ordering::Relaxed),
            hold_max_ns: self.win_hold_max_ns.swap(0, Ordering::Relaxed),
        }
    }

    pub fn is_locked(&self) -> bool { self.raw.is_locked() }

    /// Holder pid and how long it has held the lock (0, 0 when free).
    pub fn holder(&self) -> (u32, u64) {
        if !self.is_locked() { return (0, 0); }
        (self.holder_pid.load(Ordering::Relaxed),
         crate::monotonic_ns().saturating_sub(self.hold_since.load(Ordering::Relaxed)))
    }
}

extern "C" { fn arch_serial_putc(c: u8); }
fn s(m: &str) { for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } } }
fn n(v: u64) {
    if v == 0 { unsafe { arch_serial_putc(b'0') }; return; }
    let mut buf = [0u8; 20]; let mut i = 0; let mut x = v;
    while x > 0 { buf[i] = b'0' + (x % 10) as u8; x /= 10; i += 1; }
    while i > 0 { i -= 1; unsafe { arch_serial_putc(buf[i]) } }
}

fn rate_ok(last: &AtomicU64, suppressed: &AtomicU32, now: u64) -> Option<u32> {
    let l = last.load(Ordering::Relaxed);
    if l != 0 && now.saturating_sub(l) < 1_000_000_000 {
        suppressed.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    if last.compare_exchange(l, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        suppressed.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(suppressed.swap(0, Ordering::Relaxed))
}

/// ` pid N (<exe>)` of the current task (a kernel thread has no exe).
fn who() {
    s(" pid "); n(crate::current_pid() as u64);
    crate::print_current_exe();
}

impl<'a, T> Drop for IoGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        let m = self.m;
        let now = crate::monotonic_ns();
        let held = now.saturating_sub(m.hold_since.load(Ordering::Relaxed));
        m.hold_max_ns.fetch_max(held, Ordering::Relaxed);
        m.win_hold_ns.fetch_add(held, Ordering::Relaxed);
        m.win_hold_max_ns.fetch_max(held, Ordering::Relaxed);
        m.raw.unlock();
        // Reports go out with the lock released (see the module comment).
        if held >= LONG_HOLD_NS {
            if let Some(sup) = rate_ok(&LAST_HOLD_REPORT_NS, &SUPPRESSED_HOLD, now) {
                s("\n[LOCKHOLD] "); s(m.name); s(":"); who();
                s(" held it "); n(held / 1_000_000); s(" ms (I/O under the lock); max hold ms ");
                n(m.hold_max_ns.load(Ordering::Relaxed) / 1_000_000);
                if sup != 0 { s(" ("); n(sup as u64); s(" similar suppressed)"); }
                s("\n");
            }
        }
        if self.waited >= LONG_WAIT_NS {
            if let Some(sup) = rate_ok(&LAST_WAIT_REPORT_NS, &SUPPRESSED_WAIT, now) {
                let waits = m.waits.load(Ordering::Relaxed);
                s("\n[LOCKWAIT] "); s(m.name); s(":"); who();
                s(" slept "); n(self.waited / 1_000_000); s(" ms for it; waits "); n(waits);
                s(", avg wait us "); n(m.wait_ns.load(Ordering::Relaxed) / waits.max(1) / 1000);
                s(", max hold ms "); n(m.hold_max_ns.load(Ordering::Relaxed) / 1_000_000);
                if sup != 0 { s(" ("); n(sup as u64); s(" similar suppressed)"); }
                s("\n");
            }
        }
    }
}

impl<'a, T> Deref for IoGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T { unsafe { &*self.m.data.get() } }
}

impl<'a, T> DerefMut for IoGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.m.data.get() } }
}
