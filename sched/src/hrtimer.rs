//! High-resolution one-shot / periodic timers with callbacks.
//!
//! Expiry is driven by the per-CPU one-shot timer interrupt the poll-deadline
//! code already uses (`arch_timer_arm_deadline` → `sched::timer_deadline_irq`),
//! with every CPU's 100 Hz tick as a fallback (a lost one-shot costs at most
//! one tick of lateness). Resolution is whatever the one-shot gives: tens of
//! microseconds on hardware/KVM, a little coarser under TCG.
//!
//! Callback context, chosen per timer:
//! * [`TimerMode::Irq`] — runs in hard-IRQ context inside the timer
//!   interrupt, IRQs masked. Must be short and must not sleep (ksync wakes,
//!   `KSemaphore::up`, `WorkQueue::queue` are fine).
//! * [`TimerMode::Worker`] — the expiry queues the timer's work item on the
//!   system work queue; the callback runs there in task context and may sleep.
//!
//! A periodic timer is re-armed (`expires += period`, skipping missed periods
//! rather than bursting) before its callback runs; a callback that is still
//! running when the next period is due is not run concurrently — that period
//! is skipped. [`HrTimer::cancel`] removes a pending timer; `cancel_sync`
//! additionally waits for a running callback to finish. Dropping an `HrTimer`
//! does `cancel_sync`.
//!
//! Capacity: `MAX_ACTIVE` armed timers system-wide (start fails beyond that).

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::ksync::KSpinLock;
use crate::workqueue::Work;

pub type TimerFn = extern "C" fn(ctx: usize);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TimerMode {
    Irq = 0,
    Worker = 1,
}

pub const MAX_ACTIVE: usize = 128;

pub struct HrTimer {
    func: TimerFn,
    ctx: usize,
    mode: TimerMode,
    expires: AtomicU64,
    period: AtomicU64,
    armed: AtomicBool,
    running: AtomicBool,
    fired: AtomicU64,
    work: Work,
}
unsafe impl Sync for HrTimer {}
unsafe impl Send for HrTimer {}

struct Active {
    t: [*const HrTimer; MAX_ACTIVE],
    n: usize,
}
unsafe impl Send for Active {}

static ACTIVE: KSpinLock<Active> = KSpinLock::new(Active { t: [core::ptr::null(); MAX_ACTIVE], n: 0 });
/// Earliest armed expiry (`u64::MAX` = none). Exact under ACTIVE, a hint
/// outside it.
static NEXT: AtomicU64 = AtomicU64::new(u64::MAX);

extern "C" fn worker_trampoline(ctx: usize) {
    let t = unsafe { &*(ctx as *const HrTimer) };
    (t.func)(t.ctx);
    t.running.store(false, Ordering::Release);
}

impl HrTimer {
    /// A new, idle timer. Boxed so its address is stable while armed.
    pub fn new(func: TimerFn, ctx: usize, mode: TimerMode) -> Box<HrTimer> {
        let b = Box::new(HrTimer {
            func, ctx, mode,
            expires: AtomicU64::new(u64::MAX),
            period: AtomicU64::new(0),
            armed: AtomicBool::new(false),
            running: AtomicBool::new(false),
            fired: AtomicU64::new(0),
            work: Work::new(worker_trampoline, 0),
        });
        let p = &*b as *const HrTimer as usize;
        b.work.set_ctx(p);
        b
    }

    /// Arm (or re-arm) to fire at absolute `monotonic_ns()` `expires_ns`, then
    /// every `period_ns` if non-zero. Any context. False if the table is full.
    pub fn start_at(&self, expires_ns: u64, period_ns: u64) -> bool {
        self.period.store(period_ns, Ordering::Release);
        let mut a = ACTIVE.lock();
        self.expires.store(expires_ns, Ordering::Release);
        let me = self as *const HrTimer;
        if !self.armed.load(Ordering::Acquire) {
            if a.n == MAX_ACTIVE { return false; }
            let n = a.n;
            a.t[n] = me;
            a.n += 1;
            self.armed.store(true, Ordering::Release);
        }
        let next = recompute(&a);
        NEXT.store(next, Ordering::Release);
        drop(a);
        unsafe { crate::arch_timer_arm_deadline(expires_ns); }
        true
    }

    /// Arm to fire `delay_ns` from now, then every `period_ns` if non-zero.
    pub fn start(&self, delay_ns: u64, period_ns: u64) -> bool {
        self.start_at(crate::monotonic_ns().saturating_add(delay_ns), period_ns)
    }

    /// Disarm. Any context. Returns true if it was armed. A callback already
    /// running keeps running (see `cancel_sync`).
    pub fn cancel(&self) -> bool {
        let mut a = ACTIVE.lock();
        let me = self as *const HrTimer;
        let was = self.armed.swap(false, Ordering::AcqRel);
        if was {
            let n = a.n;
            for i in 0..n {
                if a.t[i] == me {
                    a.t[i] = a.t[n - 1];
                    a.t[n - 1] = core::ptr::null();
                    a.n -= 1;
                    break;
                }
            }
            let next = recompute(&a);
            NEXT.store(next, Ordering::Release);
        }
        was
    }

    /// Disarm and wait until no callback of this timer is running (worker
    /// mode: also drops a queued, not yet started callback). Must not be
    /// called from the timer's own callback.
    pub fn cancel_sync(&self) -> bool {
        let was = self.cancel();
        if self.mode == TimerMode::Worker {
            if let Some(q) = crate::workqueue::system() {
                if q.cancel(&self.work) { self.running.store(false, Ordering::Release); }
            }
        }
        while self.running.load(Ordering::Acquire) {
            if crate::irqctx::can_sleep() { crate::kthread::sleep_ns(100_000); }
            else { core::hint::spin_loop(); }
        }
        was
    }

    pub fn is_armed(&self) -> bool { self.armed.load(Ordering::Acquire) }
    /// Number of times the callback has been started.
    pub fn fired(&self) -> u64 { self.fired.load(Ordering::Acquire) }
    /// Absolute expiry it is armed for.
    pub fn expires(&self) -> u64 { self.expires.load(Ordering::Acquire) }
}

impl Drop for HrTimer {
    fn drop(&mut self) { self.cancel_sync(); }
}

fn recompute(a: &Active) -> u64 {
    let mut m = u64::MAX;
    for i in 0..a.n {
        let e = unsafe { (*a.t[i]).expires.load(Ordering::Relaxed) };
        if e < m { m = e; }
    }
    m
}

/// Run every due timer; returns the next expiry (`u64::MAX` = none). Called
/// from the one-shot deadline interrupt and from every CPU's tick (IRQ
/// context). One relaxed load when nothing is due.
pub fn service(now: u64) -> u64 {
    let hint = NEXT.load(Ordering::Acquire);
    if hint > now { return hint; }
    let mut due: [*const HrTimer; 32] = [core::ptr::null(); 32];
    let mut nd = 0;
    let next;
    {
        // Contended (another CPU is arming/servicing): retry shortly rather than
        // re-arming the one-shot for an instant already past.
        let mut a = match ACTIVE.try_lock() { Some(a) => a, None => return now.saturating_add(50_000) };
        let mut i = 0;
        while i < a.n && nd < due.len() {
            let t = unsafe { &*a.t[i] };
            if t.expires.load(Ordering::Relaxed) > now { i += 1; continue; }
            if t.running.load(Ordering::Acquire) {
                // Previous callback still running: skip this period.
                let p = t.period.load(Ordering::Relaxed);
                if p != 0 {
                    t.expires.store(now.saturating_add(p), Ordering::Relaxed);
                    i += 1;
                } else {
                    // One-shot re-armed while its callback runs: run it after.
                    i += 1;
                }
                continue;
            }
            t.running.store(true, Ordering::Release);
            t.fired.fetch_add(1, Ordering::AcqRel);
            due[nd] = a.t[i];
            nd += 1;
            let p = t.period.load(Ordering::Relaxed);
            if p != 0 {
                let mut e = t.expires.load(Ordering::Relaxed).saturating_add(p);
                if e <= now { e = now.saturating_add(p); }
                t.expires.store(e, Ordering::Relaxed);
                i += 1;
            } else {
                t.armed.store(false, Ordering::Release);
                let n = a.n;
                a.t[i] = a.t[n - 1];
                a.t[n - 1] = core::ptr::null();
                a.n -= 1;
            }
        }
        next = recompute(&a);
        NEXT.store(next, Ordering::Release);
    }
    for &tp in &due[..nd] {
        let t = unsafe { &*tp };
        match t.mode {
            TimerMode::Irq => {
                (t.func)(t.ctx);
                t.running.store(false, Ordering::Release);
            }
            TimerMode::Worker => {
                let queued = match crate::workqueue::system() {
                    Some(q) => unsafe { q.queue_raw(&t.work) },
                    None => false,
                };
                if !queued { t.running.store(false, Ordering::Release); }
            }
        }
    }
    if nd > 0 && next != u64::MAX {
        unsafe { crate::arch_timer_arm_deadline(next); }
    }
    next
}

// ── C ABI ────────────────────────────────────────────────────────────────────

/// Create a timer (task context). `mode`: 0 = IRQ callback, 1 = worker.
#[no_mangle]
pub extern "C" fn kfacil_hrtimer_create(func: TimerFn, ctx: usize, mode: u32) -> *mut HrTimer {
    let m = if mode == 1 { TimerMode::Worker } else { TimerMode::Irq };
    Box::into_raw(HrTimer::new(func, ctx, m))
}
/// Cancel synchronously and free (task context).
#[no_mangle]
pub unsafe extern "C" fn kfacil_hrtimer_destroy(t: *mut HrTimer) { if !t.is_null() { drop(Box::from_raw(t)); } }
/// Arm: first expiry `delay_ns` from now, then every `period_ns` (0 = one-shot).
#[no_mangle]
pub unsafe extern "C" fn kfacil_hrtimer_start(t: *mut HrTimer, delay_ns: u64, period_ns: u64) -> bool {
    (*t).start(delay_ns, period_ns)
}
#[no_mangle]
pub unsafe extern "C" fn kfacil_hrtimer_cancel(t: *mut HrTimer) -> bool { (*t).cancel() }
