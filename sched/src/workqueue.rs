//! Work queues: deferred work run by a kernel worker thread (the driver
//! "bottom half"; RM's `rm_isr_bh` / `os_queue_work_item`).
//!
//! * [`Work`] is an intrusive item the caller owns (no allocation when it is
//!   queued, so queueing is safe from IRQ handlers). A `Work` is either idle
//!   or pending on exactly one queue; queueing a pending item is a no-op that
//!   returns false (Linux `queue_work` semantics).
//! * [`WorkQueue::queue_fn`] queues a boxed closure. It allocates, which in IRQ
//!   context goes to the `mm` atomic pool (see `mm::atomic_pool`), so it can
//!   fail there under pressure (returns false).
//! * Each queue has ONE worker thread and runs items in FIFO order, one at a
//!   time. Work functions run in task context with IRQs masked (like a
//!   syscall) and may sleep (ksync locks, `msleep`).
//! * [`WorkQueue::flush`] waits until everything queued before the call has
//!   finished running. Calling it from the queue's own worker would deadlock:
//!   it is detected, logged once, and returns immediately.
//!
//! `system()` is the shared queue (`[kworker]`), created by `init_system()`
//! during kernel init; drivers that need ordering independent from everyone
//! else create their own with `WorkQueue::new`.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use crate::ksync::{KSpinLock, KWaitQueue};
use crate::task::Pid;

/// Work function: receives the `ctx` the item was created with.
pub type WorkFn = extern "C" fn(ctx: usize);

/// An intrusive work item. Must stay at a fixed address while pending (use a
/// `static`, a `Box`, or an `Arc` kept alive by the owner).
pub struct Work {
    func: WorkFn,
    ctx: AtomicUsize,
    pending: AtomicBool,
    next: UnsafeCell<*const Work>,
}
unsafe impl Sync for Work {}
unsafe impl Send for Work {}

impl Work {
    pub const fn new(func: WorkFn, ctx: usize) -> Self {
        Self { func, ctx: AtomicUsize::new(ctx), pending: AtomicBool::new(false),
               next: UnsafeCell::new(core::ptr::null()) }
    }
    /// Change the context passed to the function (only while not pending).
    pub fn set_ctx(&self, ctx: usize) { self.ctx.store(ctx, Ordering::Release); }
    pub fn is_pending(&self) -> bool { self.pending.load(Ordering::Acquire) }
}

struct Fifo {
    head: *const Work,
    tail: *const Work,
}
unsafe impl Send for Fifo {}

pub struct WorkQueue {
    name: &'static str,
    list: KSpinLock<Fifo>,
    wq: KWaitQueue,
    /// Items queued / finished since creation (for `flush`).
    queued: AtomicU64,
    finished: AtomicU64,
    flush_wq: KWaitQueue,
    stop: AtomicBool,
    worker: core::sync::atomic::AtomicU32,
    running: AtomicBool,
}
unsafe impl Sync for WorkQueue {}
unsafe impl Send for WorkQueue {}

static FLUSH_SELF_WARNED: AtomicBool = AtomicBool::new(false);

impl WorkQueue {
    /// Create a queue and start its worker thread. Task or boot context.
    pub fn new(name: &'static str) -> Option<Arc<WorkQueue>> {
        let q = Arc::new(WorkQueue {
            name,
            list: KSpinLock::new(Fifo { head: core::ptr::null(), tail: core::ptr::null() }),
            wq: KWaitQueue::new(),
            queued: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            flush_wq: KWaitQueue::new(),
            stop: AtomicBool::new(false),
            worker: core::sync::atomic::AtomicU32::new(0),
            running: AtomicBool::new(true),
        });
        let q2 = q.clone();
        let t = crate::kthread::spawn(name, move || q2.worker_loop())?;
        q.worker.store(t.pid(), Ordering::Release);
        Some(q)
    }

    pub fn name(&self) -> &'static str { self.name }
    /// Pid of the worker thread.
    pub fn worker_pid(&self) -> Pid { self.worker.load(Ordering::Acquire) }

    /// Queue `w`. Any context (no allocation). False if it was already
    /// pending or the queue is shutting down.
    pub fn queue(&self, w: &'static Work) -> bool { unsafe { self.queue_raw(w) } }

    /// Queue a work item by pointer.
    ///
    /// # Safety
    /// `w` must stay valid and unmoved until its function has started (it is
    /// no longer touched once `pending` clears, just before the call).
    pub unsafe fn queue_raw(&self, w: *const Work) -> bool {
        if self.stop.load(Ordering::Acquire) { return false; }
        if (*w).pending.swap(true, Ordering::AcqRel) { return false; }
        {
            let mut l = self.list.lock();
            *(*w).next.get() = core::ptr::null();
            if l.tail.is_null() { l.head = w; } else { *(*l.tail).next.get() = w; }
            l.tail = w;
            self.queued.fetch_add(1, Ordering::AcqRel);
        }
        self.wq.wake_one();
        true
    }

    /// Queue a closure (allocates; see the module doc for IRQ context).
    pub fn queue_fn<F: FnOnce() + Send + 'static>(&self, f: F) -> bool {
        struct Boxed {
            work: Work,
            f: Option<Box<dyn FnOnce() + Send>>,
        }
        extern "C" fn run(ctx: usize) {
            // SAFETY: `ctx` is the leaked Box<Boxed> below; this runs once.
            let mut b: Box<Boxed> = unsafe { Box::from_raw(ctx as *mut Boxed) };
            if let Some(f) = b.f.take() { f(); }
        }
        let b = Box::new(Boxed { work: Work::new(run, 0), f: Some(Box::new(f)) });
        let raw = Box::into_raw(b);
        unsafe {
            (*raw).work.set_ctx(raw as usize);
            if self.queue_raw(&(*raw).work) { true } else { drop(Box::from_raw(raw)); false }
        }
    }

    /// Remove `w` if it is still pending (not yet started). Any context.
    /// Returns true if it was removed.
    pub fn cancel(&self, w: *const Work) -> bool {
        let mut l = self.list.lock();
        let mut prev: *const Work = core::ptr::null();
        let mut cur = l.head;
        unsafe {
            while !cur.is_null() {
                if cur == w {
                    let nxt = *(*cur).next.get();
                    if prev.is_null() { l.head = nxt; } else { *(*prev).next.get() = nxt; }
                    if l.tail == cur { l.tail = prev; }
                    (*cur).pending.store(false, Ordering::Release);
                    // Counts as finished so flushers do not wait for it.
                    self.finished.fetch_add(1, Ordering::AcqRel);
                    drop(l);
                    self.flush_wq.wake_all();
                    return true;
                }
                prev = cur;
                cur = *(*cur).next.get();
            }
        }
        false
    }

    /// Wait until every item queued before this call has run. Task context.
    #[track_caller]
    pub fn flush(&self) {
        if crate::current_pid() == self.worker_pid() {
            if !FLUSH_SELF_WARNED.swap(true, Ordering::Relaxed) {
                crate::irqctx::raw_log("[WQ] flush from the queue's own worker ignored (would deadlock)\n");
            }
            return;
        }
        let target = self.queued.load(Ordering::Acquire);
        self.flush_wq.wait_event(|| self.finished.load(Ordering::Acquire) >= target);
    }

    /// Items queued but not finished (snapshot).
    pub fn backlog(&self) -> u64 {
        self.queued.load(Ordering::Acquire).saturating_sub(self.finished.load(Ordering::Acquire))
    }

    /// Run everything still queued, then stop the worker and wait for it.
    /// Task context. Later `queue` calls return false.
    pub fn destroy(&self) {
        self.flush();
        self.stop.store(true, Ordering::Release);
        self.wq.wake_all();
        self.flush_wq.wait_event(|| !self.running.load(Ordering::Acquire));
    }

    fn pop(&self) -> Option<*const Work> {
        let mut l = self.list.lock();
        let w = l.head;
        if w.is_null() { return None; }
        unsafe {
            l.head = *(*w).next.get();
            if l.head.is_null() { l.tail = core::ptr::null(); }
        }
        Some(w)
    }

    fn worker_loop(&self) {
        loop {
            self.wq.wait_event(|| {
                self.stop.load(Ordering::Acquire) || !self.list.lock().head.is_null()
            });
            while let Some(w) = self.pop() {
                unsafe {
                    let func = (*w).func;
                    let ctx = (*w).ctx.load(Ordering::Acquire);
                    // Cleared before the call: the function may re-queue itself,
                    // and the item may be freed by its owner from here on.
                    (*w).pending.store(false, Ordering::Release);
                    func(ctx);
                }
                self.finished.fetch_add(1, Ordering::AcqRel);
                self.flush_wq.wake_all();
            }
            if self.stop.load(Ordering::Acquire) && self.list.lock().head.is_null() { break; }
        }
        self.running.store(false, Ordering::Release);
        self.flush_wq.wake_all();
    }
}

static SYSTEM: AtomicPtr<WorkQueue> = AtomicPtr::new(core::ptr::null_mut());

/// Create the system work queue (`[kworker]`). Kernel init, once.
pub fn init_system() -> bool {
    if !SYSTEM.load(Ordering::Acquire).is_null() { return true; }
    match WorkQueue::new("kworker") {
        Some(q) => { SYSTEM.store(Arc::into_raw(q) as *mut WorkQueue, Ordering::Release); true }
        None => false,
    }
}

/// The system work queue, if `init_system` ran.
pub fn system() -> Option<&'static WorkQueue> {
    let p = SYSTEM.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

static NO_SYSTEM_WARNED: AtomicBool = AtomicBool::new(false);

/// Queue `w` on the system queue. Any context.
pub fn queue_work(w: &'static Work) -> bool {
    match system() {
        Some(q) => q.queue(w),
        None => {
            if !NO_SYSTEM_WARNED.swap(true, Ordering::Relaxed) {
                crate::irqctx::raw_log("[WQ] queue_work before workqueue::init_system; dropped\n");
            }
            false
        }
    }
}

// ── C ABI ────────────────────────────────────────────────────────────────────

/// Allocate a work item (task context). Free with `kfacil_work_free` once it
/// is no longer pending.
#[no_mangle]
pub extern "C" fn kfacil_work_alloc(func: WorkFn, ctx: usize) -> *mut Work {
    Box::into_raw(Box::new(Work::new(func, ctx)))
}
#[no_mangle]
pub unsafe extern "C" fn kfacil_work_free(w: *mut Work) { if !w.is_null() { drop(Box::from_raw(w)); } }
/// Queue on the system queue. Any context.
#[no_mangle]
pub unsafe extern "C" fn kfacil_queue_work(w: *const Work) -> bool {
    match system() { Some(q) => q.queue_raw(w), None => false }
}
/// Wait for everything queued on the system queue so far. Task context.
#[no_mangle]
pub extern "C" fn kfacil_flush_system_wq() { if let Some(q) = system() { q.flush(); } }

/// One diagnostic line for the system queue (Ctrl-T dump hook; lock-free
/// reads, raw UART).
pub fn dump() {
    use crate::irqctx::{raw_log, raw_dec};
    let (c, w, d, dw) = crate::kwake_stats();
    raw_log("[KFACIL] kwake calls="); raw_dec(c); raw_log(" woke="); raw_dec(w);
    raw_log(" deferred="); raw_dec(d); raw_log(" deferred_woke="); raw_dec(dw);
    if let Some(q) = system() {
        raw_log(" | kworker pid="); raw_dec(q.worker_pid() as u64);
        raw_log(" queued="); raw_dec(q.queued.load(Ordering::Relaxed));
        raw_log(" finished="); raw_dec(q.finished.load(Ordering::Relaxed));
        raw_log(" head_nonnull="); raw_dec(match q.list.try_lock() { Some(l) => (!l.head.is_null()) as u64, None => 9 });
        raw_log(" wq_waiters="); raw_dec(q.wq.has_waiters() as u64);
        raw_log(" flush_waiters="); raw_dec(q.flush_wq.has_waiters() as u64);
    }
    raw_log("\n");
}
