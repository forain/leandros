//! Sleeping synchronisation for kernel code: mutex, rwlock, counting
//! semaphore, wait queue, completion, plus an IRQ-safe spinlock.
//!
//! All sleeping primitives park the task in the scheduler (`KWAIT_CHANNEL`,
//! woken by pid) instead of spinning. Context rules (see `irqctx` for the
//! detection mechanism):
//!
//! | operation | task | task, atomic¹ | hard IRQ | boot² |
//! |---|---|---|---|---|
//! | `KSpinLock::lock`, `RawSpinLock::lock_irqsave` | yes | yes (nests) | yes | yes |
//! | `KMutex::lock`, `KRwLock::read/write`, `KSemaphore::down*`, `KWaitQueue::wait_event*`, `KCompletion::wait*` | **sleeps** | logged once, spins | logged once, spins | spins |
//! | `try_lock`, `try_read`, `try_write`, `try_down`, `KCompletion::done` | yes | yes | yes | yes |
//! | `KMutex` unlock, `KRwLock` unlock | yes | yes | yes³ | yes |
//! | `KSemaphore::up`, `KWaitQueue::wake_one/wake_all`, `KCompletion::complete*` | yes | yes | **yes** | yes |
//!
//! ¹ holding a `KSpinLock`, a lockwatch-tracked lock, or preemption disabled.
//! ² `kernel_main` / `init_task_main`, before `sched::run`: there is no task
//!   to park, so waiting spins (opening IRQ windows when IRQs are masked).
//! ³ only after a successful `try_*` in that same IRQ handler.
//!
//! Wakers never take a lock that is held across a park: a waker takes the
//! primitive's own spinlock (IRQ-off, a few instructions), drops it, then
//! wakes the chosen pid through `kwake`, which only *try*-locks `RUN_QUEUE`
//! and defers to the next tick when that lock is contended. So every wake is
//! safe from interrupt context and never deadlocks.
//!
//! Hand-off semantics: `KSemaphore::up`, `KMutex` unlock and `KRwLock`
//! unlock transfer ownership directly to the first queued waiter (FIFO), so a
//! waiter that is woken always owns what it waited for; no thundering herd,
//! no barging past sleepers. A waiter that times out at the same instant it
//! is granted keeps the grant (reports success).
//!
//! Waits are uninterruptible: a signal makes the parked task runnable, the
//! wait loop sees nothing granted and parks again.
//!
//! Held-lock accounting (lane loginhang, 2026-10-10): every task's count of
//! sleeping locks it holds or is queued for (`KMutex`/`KRawMutex`, so
//! `F2FS_MOUNTS` and the USB disk locks, and `KRwLock`) is kept in
//! [`klocks_held`]. A task with a non-zero count is executing kernel code that
//! other tasks depend on — it owns a lock, or its `WaitNode` is linked into a
//! lock's queue from its kernel stack — and must not be reaped in place:
//! `exit_group`'s group kill defers such a sibling until it has left the
//! kernel. Before this, a thread parked in the USB poll loop with the
//! filesystem lock held was reaped off-CPU when its process exited (the
//! greeter at login), the lock stayed owned by a dead pid, and every later
//! filesystem access — logins, exec, the klog flush — slept forever.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use crate::irqctx;
use crate::task::Pid;

extern "C" {
    fn arch_interrupt_save() -> usize;
    fn arch_interrupt_restore(flags: usize);
}

/// IRQ-state cookie returned by [`irq_save`].
pub type IrqFlags = usize;

/// Mask interrupts on this CPU and return the previous state.
#[inline(always)]
pub fn irq_save() -> IrqFlags { unsafe { arch_interrupt_save() } }

/// Restore the interrupt state saved by [`irq_save`].
#[inline(always)]
pub fn irq_restore(f: IrqFlags) { unsafe { arch_interrupt_restore(f) } }

// ── RawSpinLock / KSpinLock ──────────────────────────────────────────────────

/// A bare IRQ-safe spinlock. `lock_irqsave` masks IRQs, raises this CPU's
/// atomic depth (so a sleep attempted inside is detected), and spins
/// answering TLB-shootdown requests (IRQs are masked, so the IPI cannot be
/// taken). Not recursive. `#[repr(C)]` and 4 bytes, so a C shim can embed it.
#[repr(C)]
pub struct RawSpinLock {
    locked: AtomicU32,
}

impl RawSpinLock {
    pub const fn new() -> Self { Self { locked: AtomicU32::new(0) } }

    /// Acquire with IRQs masked. Returns the IRQ state to pass to
    /// [`Self::unlock_irqrestore`].
    #[inline]
    pub fn lock_irqsave(&self) -> IrqFlags {
        let f = irq_save();
        irqctx::atomic_inc();
        self.lock_raw();
        f
    }

    /// Release and restore the IRQ state from [`Self::lock_irqsave`].
    #[inline]
    pub fn unlock_irqrestore(&self, f: IrqFlags) {
        self.unlock_raw();
        irqctx::atomic_dec();
        irq_restore(f);
    }

    /// Try to acquire (IRQs masked on success; on failure the IRQ state is
    /// unchanged).
    #[inline]
    pub fn try_lock_irqsave(&self) -> Option<IrqFlags> {
        let f = irq_save();
        if self.locked.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            irqctx::atomic_inc();
            Some(f)
        } else {
            irq_restore(f);
            None
        }
    }

    #[inline]
    fn lock_raw(&self) {
        loop {
            if self.locked.compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return;
            }
            while self.locked.load(Ordering::Relaxed) != 0 {
                mm::paging::tlb_service_pending();
                core::hint::spin_loop();
            }
        }
    }

    #[inline]
    fn unlock_raw(&self) { self.locked.store(0, Ordering::Release); }

    pub fn is_locked(&self) -> bool { self.locked.load(Ordering::Relaxed) != 0 }
}

/// A data-carrying IRQ-safe spinlock: usable from IRQ handlers and task
/// context alike; the guard keeps IRQs masked. Never sleep while holding it
/// (detected: see `irqctx`).
pub struct KSpinLock<T> {
    raw: RawSpinLock,
    data: UnsafeCell<T>,
}
unsafe impl<T: Send> Sync for KSpinLock<T> {}
unsafe impl<T: Send> Send for KSpinLock<T> {}

pub struct KSpinGuard<'a, T> {
    lock: &'a KSpinLock<T>,
    flags: IrqFlags,
}

impl<T> KSpinLock<T> {
    pub const fn new(v: T) -> Self { Self { raw: RawSpinLock::new(), data: UnsafeCell::new(v) } }
    #[inline]
    pub fn lock(&self) -> KSpinGuard<'_, T> {
        let flags = self.raw.lock_irqsave();
        KSpinGuard { lock: self, flags }
    }
    #[inline]
    pub fn try_lock(&self) -> Option<KSpinGuard<'_, T>> {
        self.raw.try_lock_irqsave().map(|flags| KSpinGuard { lock: self, flags })
    }
}
impl<'a, T> Deref for KSpinGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.lock.data.get() } }
}
impl<'a, T> DerefMut for KSpinGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.lock.data.get() } }
}
impl<'a, T> Drop for KSpinGuard<'a, T> {
    fn drop(&mut self) { self.lock.raw.unlock_irqrestore(self.flags); }
}

// ── held-lock accounting (lane loginhang) ─────────────────────────────────

/// Sleeping locks held (or waited for) per task, indexed `pid % KLOCK_SLOTS`.
/// Two live pids that share a slot add up: the count can only read too high
/// for a pid (a reap deferred while the other one holds a lock), never too low.
const KLOCK_SLOTS: usize = 4096;
static KLOCKS: [AtomicU32; KLOCK_SLOTS] = [const { AtomicU32::new(0) }; KLOCK_SLOTS];

#[inline]
fn klock_inc(pid: Pid) { KLOCKS[pid as usize % KLOCK_SLOTS].fetch_add(1, Ordering::Relaxed); }

#[inline]
fn klock_dec(pid: Pid) {
    let c = &KLOCKS[pid as usize % KLOCK_SLOTS];
    let _ = c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
}

/// Sleeping locks `pid` holds or is queued for (see the module comment). Non
/// zero means the task is inside kernel code that must run to completion
/// before the task can be reaped.
#[inline]
pub fn klocks_held(pid: Pid) -> u32 { KLOCKS[pid as usize % KLOCK_SLOTS].load(Ordering::Relaxed) }

/// The current task enters kernel code that others wait on and that must
/// run to completion (an f2fs miss read other requests share, lane nostall):
/// counted like a held sleeping lock, so a group kill defers the reap.
/// Pair with `must_finish_end`.
pub fn must_finish_begin() { klock_inc(crate::current_pid()) }

/// End of a `must_finish_begin` section.
pub fn must_finish_end() { klock_dec(crate::current_pid()) }

// ── waiter list (intrusive, nodes live on the waiters' stacks) ─────────────

const KIND_PLAIN: u8 = 0;
const KIND_READER: u8 = 1;
const KIND_WRITER: u8 = 2;

struct WaitNode {
    pid: Pid,
    kind: u8,
    /// `monotonic_ns` when the wait began (hung-task reports).
    t0: u64,
    /// Set by a waker under the list lock: ownership was handed over (locks,
    /// semaphores) or a wake was delivered (wait queues).
    granted: bool,
    linked: bool,
    next: *mut WaitNode,
    prev: *mut WaitNode,
}

impl WaitNode {
    fn new(kind: u8) -> Self {
        Self { pid: crate::current_pid(), kind, t0: crate::monotonic_ns(), granted: false, linked: false,
               next: core::ptr::null_mut(), prev: core::ptr::null_mut() }
    }
}

struct WaitList {
    head: *mut WaitNode,
    tail: *mut WaitNode,
}

impl WaitList {
    const fn new() -> Self { Self { head: core::ptr::null_mut(), tail: core::ptr::null_mut() } }
    fn is_empty(&self) -> bool { self.head.is_null() }
    unsafe fn push_back(&mut self, n: *mut WaitNode) {
        (*n).next = core::ptr::null_mut();
        (*n).prev = self.tail;
        if self.tail.is_null() { self.head = n; } else { (*self.tail).next = n; }
        self.tail = n;
        (*n).linked = true;
    }
    unsafe fn remove(&mut self, n: *mut WaitNode) {
        if !(*n).linked { return; }
        if (*n).prev.is_null() { self.head = (*n).next; } else { (*(*n).prev).next = (*n).next; }
        if (*n).next.is_null() { self.tail = (*n).prev; } else { (*(*n).next).prev = (*n).prev; }
        (*n).next = core::ptr::null_mut();
        (*n).prev = core::ptr::null_mut();
        (*n).linked = false;
    }
    unsafe fn pop_front(&mut self) -> Option<*mut WaitNode> {
        let n = self.head;
        if n.is_null() { return None; }
        self.remove(n);
        Some(n)
    }
    fn front_kind(&self) -> Option<u8> {
        if self.head.is_null() { None } else { Some(unsafe { (*self.head).kind }) }
    }
}

/// Absolute deadline from a relative timeout (`u64::MAX` = forever).
#[inline]
pub fn deadline_after(timeout_ns: u64) -> u64 {
    if timeout_ns == u64::MAX { u64::MAX } else { crate::monotonic_ns().saturating_add(timeout_ns) }
}

/// One iteration of a non-sleeping wait (IRQ, atomic or boot context).
#[inline]
fn relax() {
    if irqctx::in_interrupt() || irqctx::atomic_depth() != 0 {
        mm::paging::tlb_service_pending();
        core::hint::spin_loop();
    } else if crate::irqs_masked() {
        // Let the tick (and on the boot path, device completions) in, but
        // never let it switch tasks: this path runs exactly when the caller
        // may be holding a lock (a tracked one, or with preemption off).
        crate::preempt_disable();
        crate::irq_window();
        crate::preempt_enable_no_resched();
    } else {
        core::hint::spin_loop();
    }
}

/// Park the current task until `node.granted` (read under `lock`) or the
/// deadline. The node must already be linked (or granted). Returns whether it
/// was granted. On timeout the node is unlinked. Caller has IRQs masked.
fn park_until_granted(lock: &RawSpinLock, list: *mut WaitList, node: *mut WaitNode, deadline: u64) -> bool {
    loop {
        crate::kwait_prepare(deadline);
        lock.lock_raw();
        let g = unsafe { (*node).granted };
        if g {
            lock.unlock_raw();
            crate::kwait_cancel();
            return true;
        }
        if crate::monotonic_ns() >= deadline {
            unsafe { (*list).remove(node); }
            lock.unlock_raw();
            crate::kwait_cancel();
            return false;
        }
        lock.unlock_raw();
        crate::kwait_commit();
    }
}

// ── KSemaphore ───────────────────────────────────────────────────────────────

/// Counting semaphore. `down*` sleep (task context); `up` is callable from
/// any context, IRQ handlers included, and hands the unit straight to the
/// longest waiter.
pub struct KSemaphore {
    lock: RawSpinLock,
    count: UnsafeCell<i64>,
    waiters: UnsafeCell<WaitList>,
}
unsafe impl Sync for KSemaphore {}
unsafe impl Send for KSemaphore {}

impl KSemaphore {
    pub const fn new(count: i64) -> Self {
        Self { lock: RawSpinLock::new(), count: UnsafeCell::new(count), waiters: UnsafeCell::new(WaitList::new()) }
    }

    /// Take one unit without waiting.
    pub fn try_down(&self) -> bool {
        let f = self.lock.lock_irqsave();
        let c = unsafe { &mut *self.count.get() };
        let ok = *c > 0;
        if ok { *c -= 1; }
        self.lock.unlock_irqrestore(f);
        ok
    }

    /// Take one unit, sleeping as long as needed.
    #[track_caller]
    pub fn down(&self) { let _ = self.down_deadline(u64::MAX); }

    /// Take one unit, sleeping at most `timeout_ns`. Returns false on timeout.
    #[track_caller]
    pub fn down_timeout(&self, timeout_ns: u64) -> bool { self.down_deadline(deadline_after(timeout_ns)) }

    /// Take one unit, sleeping until the absolute `monotonic_ns()` deadline.
    #[track_caller]
    pub fn down_deadline(&self, deadline: u64) -> bool {
        if self.try_down() { return true; }
        if !irqctx::might_sleep() {
            loop {
                if self.try_down() { return true; }
                if crate::monotonic_ns() >= deadline { return false; }
                relax();
            }
        }
        let f = self.lock.lock_irqsave();
        let c = unsafe { &mut *self.count.get() };
        if *c > 0 {
            *c -= 1;
            self.lock.unlock_irqrestore(f);
            return true;
        }
        let mut node = WaitNode::new(KIND_PLAIN);
        let np: *mut WaitNode = &mut node;
        unsafe { (*self.waiters.get()).push_back(np); }
        // Drop the lock but keep IRQs masked until the park sequence is over.
        self.lock.unlock_raw();
        irqctx::atomic_dec();
        let ok = park_until_granted(&self.lock, self.waiters.get(), np, deadline);
        irq_restore(f);
        ok
    }

    /// Release one unit. Any context.
    pub fn up(&self) {
        let f = self.lock.lock_irqsave();
        let w = unsafe { (*self.waiters.get()).pop_front() };
        let pid = match w {
            Some(n) => unsafe { (*n).granted = true; (*n).pid },
            None => { unsafe { *self.count.get() += 1; } 0 }
        };
        self.lock.unlock_irqrestore(f);
        if pid != 0 { crate::kwake(pid); }
    }

    /// Current count (racy snapshot; negative never, waiters are separate).
    pub fn count(&self) -> i64 {
        let f = self.lock.lock_irqsave();
        let c = unsafe { *self.count.get() };
        self.lock.unlock_irqrestore(f);
        c
    }

    /// True if a task is queued waiting (racy snapshot).
    pub fn has_waiters(&self) -> bool {
        let f = self.lock.lock_irqsave();
        let w = unsafe { !(*self.waiters.get()).is_empty() };
        self.lock.unlock_irqrestore(f);
        w
    }
}

// ── KRawMutex / KMutex ───────────────────────────────────────────────────────

/// A sleeping mutex without data (for C callers and `KMutex<T>`). FIFO
/// hand-off on unlock. Not recursive: relocking from the owner is logged
/// once and then deadlocks that task only (as on Linux).
pub struct KRawMutex {
    lock: RawSpinLock,
    /// Owner pid + 1 (boot context owns as pid 0 -> 1); 0 = unlocked.
    owner: UnsafeCell<u32>,
    waiters: UnsafeCell<WaitList>,
}
unsafe impl Sync for KRawMutex {}
unsafe impl Send for KRawMutex {}

static RELOCK_WARNED: AtomicBool = AtomicBool::new(false);

impl KRawMutex {
    pub const fn new() -> Self {
        Self { lock: RawSpinLock::new(), owner: UnsafeCell::new(0), waiters: UnsafeCell::new(WaitList::new()) }
    }

    #[inline]
    fn me() -> u32 { crate::current_pid() + 1 }

    pub fn try_lock(&self) -> bool {
        let f = self.lock.lock_irqsave();
        let o = unsafe { &mut *self.owner.get() };
        let ok = *o == 0;
        if ok { *o = Self::me(); klock_inc(Self::me() - 1); }
        self.lock.unlock_irqrestore(f);
        ok
    }

    #[track_caller]
    pub fn lock(&self) { let _ = self.lock_deadline(u64::MAX); }

    /// Owner pid + 1 (0 = free): diagnostics (hung-task reports).
    pub fn owner_raw(&self) -> u32 { unsafe { core::ptr::read_volatile(self.owner.get()) } }

    /// `(waiters, oldest waiter's pid, when it began waiting)` — a snapshot
    /// for hung-task reports. Takes only this mutex's internal spinlock (a few
    /// instructions per waiter), never the mutex itself.
    pub fn waiters_snapshot(&self) -> (usize, Pid, u64) {
        let f = self.lock.lock_irqsave();
        let mut n = 0usize;
        let head = unsafe { (*self.waiters.get()).head };
        let (pid, t0) = if head.is_null() { (0, 0) } else { unsafe { ((*head).pid, (*head).t0) } };
        let mut p = head;
        while !p.is_null() && n < 4096 { n += 1; p = unsafe { (*p).next }; }
        self.lock.unlock_irqrestore(f);
        (n, pid, t0)
    }

    /// Lock, giving up at the absolute deadline. Returns false on timeout.
    #[track_caller]
    pub fn lock_deadline(&self, deadline: u64) -> bool {
        if self.try_lock() { return true; }
        if self.is_owned_by_me() && !RELOCK_WARNED.swap(true, Ordering::Relaxed) {
            let loc = core::panic::Location::caller();
            irqctx::raw_log("[KSYNC] KMutex relocked by its owner at ");
            irqctx::raw_log(loc.file()); irqctx::raw_log(":"); irqctx::raw_dec(loc.line() as u64);
            irqctx::raw_log("\n");
        }
        if !irqctx::might_sleep() {
            loop {
                if self.try_lock() { return true; }
                if crate::monotonic_ns() >= deadline { return false; }
                relax();
            }
        }
        let f = self.lock.lock_irqsave();
        let o = unsafe { &mut *self.owner.get() };
        if *o == 0 {
            *o = Self::me();
            klock_inc(Self::me() - 1);
            self.lock.unlock_irqrestore(f);
            return true;
        }
        let mut node = WaitNode::new(KIND_PLAIN);
        let np: *mut WaitNode = &mut node;
        // Counted from the moment the node is linked: a granted waiter keeps
        // the count as the owner; a timed-out one gives it back below.
        klock_inc(node.pid);
        unsafe { (*self.waiters.get()).push_back(np); }
        self.lock.unlock_raw();
        irqctx::atomic_dec();
        let ok = park_until_granted(&self.lock, self.waiters.get(), np, deadline);
        if !ok { klock_dec(node.pid); }
        irq_restore(f);
        ok
    }

    /// Unlock: hand the mutex to the first waiter, if any.
    ///
    /// # Safety contract (not enforced)
    /// Only the owner may unlock.
    pub fn unlock(&self) {
        let f = self.lock.lock_irqsave();
        // The count belongs to the recorded owner (it may differ from the
        // caller after a non-leader exec exchanged pids).
        let prev = unsafe { *self.owner.get() };
        if prev != 0 { klock_dec(prev - 1); }
        let w = unsafe { (*self.waiters.get()).pop_front() };
        let pid = match w {
            Some(n) => unsafe {
                (*n).granted = true;
                *self.owner.get() = (*n).pid + 1;
                (*n).pid
            },
            None => { unsafe { *self.owner.get() = 0; } 0 }
        };
        self.lock.unlock_irqrestore(f);
        if pid != 0 { crate::kwake(pid); }
    }

    pub fn is_locked(&self) -> bool { unsafe { core::ptr::read_volatile(self.owner.get()) != 0 } }

    /// True if the current task holds it (RM: `os_is_..._held`-style asserts).
    pub fn is_owned_by_me(&self) -> bool { unsafe { core::ptr::read_volatile(self.owner.get()) == Self::me() } }
}

/// A sleeping mutex protecting `T`.
pub struct KMutex<T> {
    raw: KRawMutex,
    data: UnsafeCell<T>,
}
unsafe impl<T: Send> Sync for KMutex<T> {}
unsafe impl<T: Send> Send for KMutex<T> {}

pub struct KMutexGuard<'a, T> { m: &'a KMutex<T> }

impl<T> KMutex<T> {
    pub const fn new(v: T) -> Self { Self { raw: KRawMutex::new(), data: UnsafeCell::new(v) } }
    #[track_caller]
    pub fn lock(&self) -> KMutexGuard<'_, T> { self.raw.lock(); KMutexGuard { m: self } }
    pub fn try_lock(&self) -> Option<KMutexGuard<'_, T>> {
        if self.raw.try_lock() { Some(KMutexGuard { m: self }) } else { None }
    }
    #[track_caller]
    pub fn lock_timeout(&self, timeout_ns: u64) -> Option<KMutexGuard<'_, T>> {
        if self.raw.lock_deadline(deadline_after(timeout_ns)) { Some(KMutexGuard { m: self }) } else { None }
    }
    pub fn is_locked(&self) -> bool { self.raw.is_locked() }
    /// Owner pid + 1 (0 = free), diagnostics.
    pub fn owner_raw(&self) -> u32 { self.raw.owner_raw() }
    /// See [`KRawMutex::waiters_snapshot`].
    pub fn waiters_snapshot(&self) -> (usize, Pid, u64) { self.raw.waiters_snapshot() }
}
impl<'a, T> Deref for KMutexGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.m.data.get() } }
}
impl<'a, T> DerefMut for KMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.m.data.get() } }
}
impl<'a, T> Drop for KMutexGuard<'a, T> {
    fn drop(&mut self) { self.m.raw.unlock(); }
}

// ── KRawRwLock / KRwLock ─────────────────────────────────────────────────────

/// Sleeping reader/writer lock without data. Writer-preferring: once a
/// writer queues, new readers queue behind it, so writers cannot starve.
/// Release hands over to the queue head: one writer, or every reader up to
/// the next queued writer.
pub struct KRawRwLock {
    lock: RawSpinLock,
    /// Readers holding it.
    readers: UnsafeCell<u32>,
    /// A writer holds it.
    writer: UnsafeCell<bool>,
    waiters: UnsafeCell<WaitList>,
}
unsafe impl Sync for KRawRwLock {}
unsafe impl Send for KRawRwLock {}

impl KRawRwLock {
    pub const fn new() -> Self {
        Self { lock: RawSpinLock::new(), readers: UnsafeCell::new(0), writer: UnsafeCell::new(false),
               waiters: UnsafeCell::new(WaitList::new()) }
    }

    pub fn try_read(&self) -> bool {
        let f = self.lock.lock_irqsave();
        let ok = unsafe { !*self.writer.get() && (*self.waiters.get()).is_empty() };
        if ok { unsafe { *self.readers.get() += 1; } klock_inc(crate::current_pid()); }
        self.lock.unlock_irqrestore(f);
        ok
    }

    pub fn try_write(&self) -> bool {
        let f = self.lock.lock_irqsave();
        let ok = unsafe { !*self.writer.get() && *self.readers.get() == 0 };
        if ok { unsafe { *self.writer.get() = true; } klock_inc(crate::current_pid()); }
        self.lock.unlock_irqrestore(f);
        ok
    }

    #[track_caller]
    pub fn read(&self) { self.acquire(KIND_READER) }
    #[track_caller]
    pub fn write(&self) { self.acquire(KIND_WRITER) }

    #[track_caller]
    fn acquire(&self, kind: u8) {
        let try_it = |s: &Self| if kind == KIND_READER { s.try_read() } else { s.try_write() };
        if try_it(self) { return; }
        if !irqctx::might_sleep() {
            while !try_it(self) { relax(); }
            return;
        }
        let f = self.lock.lock_irqsave();
        let free = unsafe {
            if kind == KIND_READER { !*self.writer.get() && (*self.waiters.get()).is_empty() }
            else { !*self.writer.get() && *self.readers.get() == 0 }
        };
        if free {
            unsafe { if kind == KIND_READER { *self.readers.get() += 1 } else { *self.writer.get() = true } }
            klock_inc(crate::current_pid());
            self.lock.unlock_irqrestore(f);
            return;
        }
        let mut node = WaitNode::new(kind);
        let np: *mut WaitNode = &mut node;
        klock_inc(node.pid);
        unsafe { (*self.waiters.get()).push_back(np); }
        self.lock.unlock_raw();
        irqctx::atomic_dec();
        let _ = park_until_granted(&self.lock, self.waiters.get(), np, u64::MAX);
        irq_restore(f);
    }

    pub fn read_unlock(&self) {
        klock_dec(crate::current_pid());
        let f = self.lock.lock_irqsave();
        unsafe {
            let r = &mut *self.readers.get();
            if *r > 0 { *r -= 1; }
        }
        self.grant_and_unlock(f);
    }

    pub fn write_unlock(&self) {
        klock_dec(crate::current_pid());
        let f = self.lock.lock_irqsave();
        unsafe { *self.writer.get() = false; }
        self.grant_and_unlock(f);
    }

    /// With the lock held: hand over to the queue head(s), drop the lock, wake.
    fn grant_and_unlock(&self, mut f: IrqFlags) {
        loop {
            let mut pids = [0 as Pid; 16];
            let mut n = 0;
            let mut more = false;
            unsafe {
                let list = &mut *self.waiters.get();
                let writer = &mut *self.writer.get();
                let readers = &mut *self.readers.get();
                if !*writer {
                    match list.front_kind() {
                        Some(KIND_WRITER) if *readers == 0 => {
                            let w = list.pop_front().unwrap();
                            (*w).granted = true;
                            *writer = true;
                            pids[0] = (*w).pid; n = 1;
                        }
                        Some(KIND_READER) => {
                            while n < pids.len() && list.front_kind() == Some(KIND_READER) {
                                let r = list.pop_front().unwrap();
                                (*r).granted = true;
                                *readers += 1;
                                pids[n] = (*r).pid; n += 1;
                            }
                            more = list.front_kind() == Some(KIND_READER);
                        }
                        _ => {}
                    }
                }
            }
            self.lock.unlock_irqrestore(f);
            for &p in &pids[..n] { crate::kwake(p); }
            if !more { return; }
            f = self.lock.lock_irqsave();
        }
    }

    /// Readers currently holding it (snapshot).
    pub fn readers(&self) -> u32 { unsafe { core::ptr::read_volatile(self.readers.get()) } }
    /// Whether a writer holds it (snapshot).
    pub fn write_locked(&self) -> bool { unsafe { core::ptr::read_volatile(self.writer.get()) } }
}

/// A sleeping reader/writer lock protecting `T`.
pub struct KRwLock<T> {
    raw: KRawRwLock,
    data: UnsafeCell<T>,
}
unsafe impl<T: Send + Sync> Sync for KRwLock<T> {}
unsafe impl<T: Send> Send for KRwLock<T> {}

pub struct KReadGuard<'a, T> { l: &'a KRwLock<T> }
pub struct KWriteGuard<'a, T> { l: &'a KRwLock<T> }

impl<T> KRwLock<T> {
    pub const fn new(v: T) -> Self { Self { raw: KRawRwLock::new(), data: UnsafeCell::new(v) } }
    #[track_caller]
    pub fn read(&self) -> KReadGuard<'_, T> { self.raw.read(); KReadGuard { l: self } }
    #[track_caller]
    pub fn write(&self) -> KWriteGuard<'_, T> { self.raw.write(); KWriteGuard { l: self } }
    pub fn try_read(&self) -> Option<KReadGuard<'_, T>> {
        if self.raw.try_read() { Some(KReadGuard { l: self }) } else { None }
    }
    pub fn try_write(&self) -> Option<KWriteGuard<'_, T>> {
        if self.raw.try_write() { Some(KWriteGuard { l: self }) } else { None }
    }
}
impl<'a, T> Deref for KReadGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.l.data.get() } }
}
impl<'a, T> Drop for KReadGuard<'a, T> { fn drop(&mut self) { self.l.raw.read_unlock(); } }
impl<'a, T> Deref for KWriteGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.l.data.get() } }
}
impl<'a, T> DerefMut for KWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.l.data.get() } }
}
impl<'a, T> Drop for KWriteGuard<'a, T> { fn drop(&mut self) { self.l.raw.write_unlock(); } }

// ── KWaitQueue ───────────────────────────────────────────────────────────────

/// A wait queue: tasks sleep until a condition holds; wakers (any context,
/// IRQ handlers included) call `wake_one` / `wake_all` after making the
/// condition true.
///
/// The condition is evaluated in the waiter's context, after it is
/// registered and marked Blocked, so a wake can never fall between the check
/// and the park. It must be cheap and must not sleep (it runs with IRQs
/// masked). `wake_one` wakes the longest waiter; if waiters on one queue wait
/// for *different* conditions, use `wake_all`.
pub struct KWaitQueue {
    lock: RawSpinLock,
    waiters: UnsafeCell<WaitList>,
}
unsafe impl Sync for KWaitQueue {}
unsafe impl Send for KWaitQueue {}

impl KWaitQueue {
    pub const fn new() -> Self { Self { lock: RawSpinLock::new(), waiters: UnsafeCell::new(WaitList::new()) } }

    /// Sleep until `cond()` is true.
    #[track_caller]
    pub fn wait_event<F: FnMut() -> bool>(&self, cond: F) { let _ = self.wait_event_deadline(cond, u64::MAX); }

    /// Sleep until `cond()` is true or `timeout_ns` elapses. Returns the final
    /// value of `cond()` (true = condition met).
    #[track_caller]
    pub fn wait_event_timeout<F: FnMut() -> bool>(&self, cond: F, timeout_ns: u64) -> bool {
        self.wait_event_deadline(cond, deadline_after(timeout_ns))
    }

    /// As [`Self::wait_event_timeout`] with an absolute `monotonic_ns()` deadline.
    #[track_caller]
    pub fn wait_event_deadline<F: FnMut() -> bool>(&self, mut cond: F, deadline: u64) -> bool {
        if cond() { return true; }
        if !irqctx::might_sleep() {
            loop {
                if cond() { return true; }
                if crate::monotonic_ns() >= deadline { return false; }
                relax();
            }
        }
        let f = irq_save();
        let mut node = WaitNode::new(KIND_PLAIN);
        let np: *mut WaitNode = &mut node;
        let list = self.waiters.get();
        let result = loop {
            self.lock.lock_raw();
            unsafe { if !(*np).linked { (*np).granted = false; (*list).push_back(np); } }
            self.lock.unlock_raw();
            crate::kwait_prepare(deadline);
            if cond() { crate::kwait_cancel(); break true; }
            if crate::monotonic_ns() >= deadline { crate::kwait_cancel(); break false; }
            // A waker may have taken this node off the list between the link
            // above and the park just published (its kwake then found the
            // task still running and did nothing). Parking unlinked would be
            // a lost wake: nobody can find us any more. Re-link and re-check.
            self.lock.lock_raw();
            let linked = unsafe { (*np).linked };
            self.lock.unlock_raw();
            if !linked { crate::kwait_cancel(); continue; }
            crate::kwait_commit();
        };
        self.lock.lock_raw();
        unsafe { (*list).remove(np); }
        self.lock.unlock_raw();
        irq_restore(f);
        result
    }

    /// Wake the longest-waiting task. Any context. Returns whether one was
    /// queued.
    pub fn wake_one(&self) -> bool {
        let f = self.lock.lock_irqsave();
        let pid = unsafe {
            match (*self.waiters.get()).pop_front() {
                Some(n) => { (*n).granted = true; (*n).pid }
                None => 0,
            }
        };
        self.lock.unlock_irqrestore(f);
        if pid != 0 { crate::kwake(pid); true } else { false }
    }

    /// Wake every queued task. Any context. Returns how many.
    pub fn wake_all(&self) -> usize {
        let mut total = 0;
        loop {
            let mut pids = [0 as Pid; 16];
            let mut n = 0;
            let f = self.lock.lock_irqsave();
            unsafe {
                let list = &mut *self.waiters.get();
                while n < pids.len() {
                    match list.pop_front() {
                        Some(w) => { (*w).granted = true; pids[n] = (*w).pid; n += 1; }
                        None => break,
                    }
                }
            }
            let more = unsafe { !(*self.waiters.get()).is_empty() };
            self.lock.unlock_irqrestore(f);
            for &p in &pids[..n] { crate::kwake(p); }
            total += n;
            if !more || n == 0 { return total; }
        }
    }

    /// True if any task is queued (snapshot).
    pub fn has_waiters(&self) -> bool {
        let f = self.lock.lock_irqsave();
        let w = unsafe { !(*self.waiters.get()).is_empty() };
        self.lock.unlock_irqrestore(f);
        w
    }
}

// ── KCompletion ──────────────────────────────────────────────────────────────

/// One-shot (or counted) event: `complete` from any context, `wait` sleeps
/// until it happened. `complete_all` latches it for every present and future
/// waiter until `reinit`.
pub struct KCompletion {
    done: AtomicU32,
    wq: KWaitQueue,
}

const COMPLETE_ALL: u32 = u32::MAX;

impl KCompletion {
    pub const fn new() -> Self { Self { done: AtomicU32::new(0), wq: KWaitQueue::new() } }

    /// Signal one waiter (or bank one for a future `wait`). Any context.
    pub fn complete(&self) {
        let _ = self.done.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |d| if d == COMPLETE_ALL { None } else { Some(d.saturating_add(1).min(COMPLETE_ALL - 1)) });
        self.wq.wake_all();
    }

    /// Release every waiter now and later. Any context.
    pub fn complete_all(&self) {
        self.done.store(COMPLETE_ALL, Ordering::Release);
        self.wq.wake_all();
    }

    fn try_take(&self) -> bool {
        self.done.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |d| if d == 0 { None } else if d == COMPLETE_ALL { Some(d) } else { Some(d - 1) }).is_ok()
    }

    /// True if a `wait` would return immediately (consumes nothing).
    pub fn done(&self) -> bool { self.done.load(Ordering::Acquire) != 0 }

    #[track_caller]
    pub fn wait(&self) { self.wq.wait_event(|| self.try_take()); }

    /// Returns false on timeout.
    #[track_caller]
    pub fn wait_timeout(&self, timeout_ns: u64) -> bool {
        self.wq.wait_event_timeout(|| self.try_take(), timeout_ns)
    }

    pub fn reinit(&self) { self.done.store(0, Ordering::Release); }
}

// ── C-ABI entry points for the RM shim ──────────────────────────────────────
//
// Opaque heap objects. Create/destroy are task context (they allocate); every
// other call follows the table at the top of this file. A NULL handle is
// ignored (`*_create` returns NULL on allocation failure).
pub mod ffi {
    use super::*;
    use alloc::boxed::Box;
    use core::ffi::c_void;

    #[no_mangle] pub extern "C" fn kfacil_spinlock_create() -> *mut RawSpinLock {
        Box::into_raw(Box::new(RawSpinLock::new()))
    }
    #[no_mangle] pub unsafe extern "C" fn kfacil_spinlock_destroy(l: *mut RawSpinLock) {
        if !l.is_null() { drop(Box::from_raw(l)); }
    }
    /// Returns the IRQ flags to hand back to `kfacil_spin_unlock_irqrestore`.
    #[no_mangle] pub unsafe extern "C" fn kfacil_spin_lock_irqsave(l: *mut RawSpinLock) -> usize {
        (*l).lock_irqsave()
    }
    #[no_mangle] pub unsafe extern "C" fn kfacil_spin_unlock_irqrestore(l: *mut RawSpinLock, flags: usize) {
        (*l).unlock_irqrestore(flags)
    }

    #[no_mangle] pub extern "C" fn kfacil_mutex_create() -> *mut KRawMutex { Box::into_raw(Box::new(KRawMutex::new())) }
    #[no_mangle] pub unsafe extern "C" fn kfacil_mutex_destroy(m: *mut KRawMutex) { if !m.is_null() { drop(Box::from_raw(m)); } }
    #[no_mangle] pub unsafe extern "C" fn kfacil_mutex_lock(m: *mut KRawMutex) { (*m).lock() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_mutex_trylock(m: *mut KRawMutex) -> bool { (*m).try_lock() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_mutex_unlock(m: *mut KRawMutex) { (*m).unlock() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_mutex_held_by_me(m: *mut KRawMutex) -> bool { (*m).is_owned_by_me() }

    #[no_mangle] pub extern "C" fn kfacil_rwlock_create() -> *mut KRawRwLock { Box::into_raw(Box::new(KRawRwLock::new())) }
    #[no_mangle] pub unsafe extern "C" fn kfacil_rwlock_destroy(l: *mut KRawRwLock) { if !l.is_null() { drop(Box::from_raw(l)); } }
    #[no_mangle] pub unsafe extern "C" fn kfacil_rwlock_read(l: *mut KRawRwLock) { (*l).read() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_rwlock_write(l: *mut KRawRwLock) { (*l).write() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_rwlock_try_read(l: *mut KRawRwLock) -> bool { (*l).try_read() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_rwlock_try_write(l: *mut KRawRwLock) -> bool { (*l).try_write() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_rwlock_read_unlock(l: *mut KRawRwLock) { (*l).read_unlock() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_rwlock_write_unlock(l: *mut KRawRwLock) { (*l).write_unlock() }

    #[no_mangle] pub extern "C" fn kfacil_sema_create(count: i64) -> *mut KSemaphore { Box::into_raw(Box::new(KSemaphore::new(count))) }
    #[no_mangle] pub unsafe extern "C" fn kfacil_sema_destroy(s: *mut KSemaphore) { if !s.is_null() { drop(Box::from_raw(s)); } }
    #[no_mangle] pub unsafe extern "C" fn kfacil_sema_down(s: *mut KSemaphore) { (*s).down() }
    /// Returns false on timeout. `timeout_ns == u64::MAX` waits forever.
    #[no_mangle] pub unsafe extern "C" fn kfacil_sema_down_timeout(s: *mut KSemaphore, timeout_ns: u64) -> bool { (*s).down_timeout(timeout_ns) }
    #[no_mangle] pub unsafe extern "C" fn kfacil_sema_try_down(s: *mut KSemaphore) -> bool { (*s).try_down() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_sema_up(s: *mut KSemaphore) { (*s).up() }

    #[no_mangle] pub extern "C" fn kfacil_waitq_create() -> *mut KWaitQueue { Box::into_raw(Box::new(KWaitQueue::new())) }
    #[no_mangle] pub unsafe extern "C" fn kfacil_waitq_destroy(q: *mut KWaitQueue) { if !q.is_null() { drop(Box::from_raw(q)); } }
    /// Sleep until `cond(arg)` returns true or the timeout passes; returns the
    /// final condition value. `cond` runs with IRQs masked and must not sleep.
    #[no_mangle] pub unsafe extern "C" fn kfacil_waitq_wait(
        q: *mut KWaitQueue, cond: extern "C" fn(*mut c_void) -> bool, arg: *mut c_void, timeout_ns: u64,
    ) -> bool {
        (*q).wait_event_timeout(|| cond(arg), timeout_ns)
    }
    #[no_mangle] pub unsafe extern "C" fn kfacil_waitq_wake_one(q: *mut KWaitQueue) -> bool { (*q).wake_one() }
    #[no_mangle] pub unsafe extern "C" fn kfacil_waitq_wake_all(q: *mut KWaitQueue) -> usize { (*q).wake_all() }

    #[no_mangle] pub extern "C" fn kfacil_in_irq() -> bool { irqctx::in_irq() }
    #[no_mangle] pub extern "C" fn kfacil_can_sleep() -> bool { irqctx::can_sleep() }
}
