//! Locks, semaphores, wait queues and context queries, on the lane kfacil
//! primitives (`sched::ksync`, `sched::irqctx`).
//!
//! Semantics follow kernel-open/nvidia/os-interface.c:
//!   - spinlocks are IRQ-saving (`spin_lock_irqsave`); the returned flags go
//!     back to `os_release_spinlock`.
//!   - mutex / semaphore / rwlock acquisition fails with
//!     NV_ERR_INVALID_REQUEST where sleeping is not allowed (hard IRQ or a
//!     spinlock held). Boot context (no current task) is let through: the
//!     ksync primitives spin there.
//!   - a wait queue is a completion: `os_wake_up` is `complete_all`.
//!
//! Bounded waits: a contended mutex / semaphore is waited for in 5 s slices;
//! every expired slice logs one line naming the lock and the waiting time,
//! and is an abort point for the bring-up watchdog (`device::abort_point`).

use core::ffi::c_void;

use sched::ksync::{KCompletion, KRawMutex, KRawRwLock, KSemaphore, RawSpinLock};

use crate::{nvlog, NV_ERR_INVALID_REQUEST, NV_ERR_NO_MEMORY, NV_ERR_TIMEOUT_RETRY, NV_OK};

const SLICE_NS: u64 = 5_000_000_000;

fn may_block() -> bool {
    !sched::irqctx::in_irq() && sched::irqctx::atomic_depth() == 0
}

fn boxed<T>(v: T) -> *mut T {
    alloc::boxed::Box::into_raw(alloc::boxed::Box::new(v))
}

unsafe fn unbox<T>(p: *mut c_void) {
    if !p.is_null() {
        drop(alloc::boxed::Box::from_raw(p as *mut T));
    }
}

fn slow_wait(what: &str, p: *mut c_void, waited_s: u64) {
    nvlog!("[nvrm] +{}ms {} {:p}: still waiting after {} s (pid {})\n",
           crate::ms(), what, p, waited_s, sched::current_pid());
    crate::device::abort_point(what);
}

// -------------------------------------------------------------- spinlock --

#[no_mangle]
pub unsafe extern "C" fn os_alloc_spinlock(lock: *mut *mut c_void) -> u32 {
    cb!("os_alloc_spinlock");
    *lock = boxed(RawSpinLock::new()) as *mut c_void;
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_free_spinlock(lock: *mut c_void) {
    cb!("os_free_spinlock");
    unbox::<RawSpinLock>(lock);
}

/// C: `NvU64 os_acquire_spinlock(void *)` -- returns the saved IRQ state.
#[no_mangle]
pub unsafe extern "C" fn os_acquire_spinlock(lock: *mut c_void) -> u64 {
    cb!("os_acquire_spinlock");
    (*(lock as *const RawSpinLock)).lock_irqsave() as u64
}

#[no_mangle]
pub unsafe extern "C" fn os_release_spinlock(lock: *mut c_void, old_irql: u64) {
    cb!("os_release_spinlock");
    (*(lock as *const RawSpinLock)).unlock_irqrestore(old_irql as _);
}

// ----------------------------------------------------------------- mutex --

#[no_mangle]
pub unsafe extern "C" fn os_alloc_mutex(m: *mut *mut c_void) -> u32 {
    cb!("os_alloc_mutex");
    *m = boxed(KRawMutex::new()) as *mut c_void;
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_free_mutex(m: *mut c_void) {
    cb!("os_free_mutex");
    unbox::<KRawMutex>(m);
}

#[no_mangle]
pub unsafe extern "C" fn os_acquire_mutex(m: *mut c_void) -> u32 {
    cb!("os_acquire_mutex");
    if !may_block() {
        return NV_ERR_INVALID_REQUEST;
    }
    let mx = &*(m as *const KRawMutex);
    if mx.try_lock() {
        return NV_OK;
    }
    let mut waited = 0;
    while !mx.lock_deadline(sched::ksync::deadline_after(SLICE_NS)) {
        waited += 5;
        slow_wait("os_acquire_mutex", m, waited);
    }
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_cond_acquire_mutex(m: *mut c_void) -> u32 {
    cb!("os_cond_acquire_mutex");
    if (*(m as *const KRawMutex)).try_lock() { NV_OK } else { NV_ERR_TIMEOUT_RETRY }
}

#[no_mangle]
pub unsafe extern "C" fn os_release_mutex(m: *mut c_void) {
    cb!("os_release_mutex");
    (*(m as *const KRawMutex)).unlock();
}

// ------------------------------------------------------------- semaphore --

#[no_mangle]
pub unsafe extern "C" fn os_alloc_semaphore(initial: u32) -> *mut c_void {
    cb!("os_alloc_semaphore");
    boxed(KSemaphore::new(initial as i64)) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_free_semaphore(s: *mut c_void) {
    cb!("os_free_semaphore");
    unbox::<KSemaphore>(s);
}

#[no_mangle]
pub unsafe extern "C" fn os_acquire_semaphore(s: *mut c_void) -> u32 {
    cb!("os_acquire_semaphore");
    if !may_block() {
        return NV_ERR_INVALID_REQUEST;
    }
    let sem = &*(s as *const KSemaphore);
    if sem.try_down() {
        return NV_OK;
    }
    let mut waited = 0;
    while !sem.down_timeout(SLICE_NS) {
        waited += 5;
        slow_wait("os_acquire_semaphore", s, waited);
    }
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_cond_acquire_semaphore(s: *mut c_void) -> u32 {
    cb!("os_cond_acquire_semaphore");
    if (*(s as *const KSemaphore)).try_down() { NV_OK } else { NV_ERR_TIMEOUT_RETRY }
}

/// Any context, including the ISR.
#[no_mangle]
pub unsafe extern "C" fn os_release_semaphore(s: *mut c_void) -> u32 {
    cb!("os_release_semaphore");
    (*(s as *const KSemaphore)).up();
    NV_OK
}

/// Linux: `NV_MAY_SLEEP()` = !irqs_disabled() && !in_interrupt(). Our task
/// context runs with IRQs masked yet may sleep, so ask irqctx instead.
#[no_mangle]
pub unsafe extern "C" fn os_semaphore_may_sleep() -> u8 {
    cb!("os_semaphore_may_sleep");
    sched::irqctx::can_sleep() as u8
}

/// Haiku/Linux: "in an interrupt handler".
#[no_mangle]
pub unsafe extern "C" fn os_is_isr() -> u8 {
    cb!("os_is_isr");
    sched::irqctx::in_irq() as u8
}

// ---------------------------------------------------------------- rwlock --

#[no_mangle]
pub unsafe extern "C" fn os_alloc_rwlock() -> *mut c_void {
    cb!("os_alloc_rwlock");
    boxed(KRawRwLock::new()) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_free_rwlock(l: *mut c_void) {
    cb!("os_free_rwlock");
    unbox::<KRawRwLock>(l);
}

#[no_mangle]
pub unsafe extern "C" fn os_acquire_rwlock_read(l: *mut c_void) -> u32 {
    cb!("os_acquire_rwlock_read");
    if !may_block() {
        return NV_ERR_INVALID_REQUEST;
    }
    (*(l as *const KRawRwLock)).read();
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_acquire_rwlock_write(l: *mut c_void) -> u32 {
    cb!("os_acquire_rwlock_write");
    if !may_block() {
        return NV_ERR_INVALID_REQUEST;
    }
    (*(l as *const KRawRwLock)).write();
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_cond_acquire_rwlock_read(l: *mut c_void) -> u32 {
    cb!("os_cond_acquire_rwlock_read");
    if (*(l as *const KRawRwLock)).try_read() { NV_OK } else { NV_ERR_TIMEOUT_RETRY }
}

#[no_mangle]
pub unsafe extern "C" fn os_cond_acquire_rwlock_write(l: *mut c_void) -> u32 {
    cb!("os_cond_acquire_rwlock_write");
    if (*(l as *const KRawRwLock)).try_write() { NV_OK } else { NV_ERR_TIMEOUT_RETRY }
}

#[no_mangle]
pub unsafe extern "C" fn os_release_rwlock_read(l: *mut c_void) {
    cb!("os_release_rwlock_read");
    (*(l as *const KRawRwLock)).read_unlock();
}

#[no_mangle]
pub unsafe extern "C" fn os_release_rwlock_write(l: *mut c_void) {
    cb!("os_release_rwlock_write");
    (*(l as *const KRawRwLock)).write_unlock();
}

// ------------------------------------------------------------ wait queue --

#[no_mangle]
pub unsafe extern "C" fn os_alloc_wait_queue(wq: *mut *mut c_void) -> u32 {
    cb!("os_alloc_wait_queue");
    if wq.is_null() {
        return NV_ERR_NO_MEMORY;
    }
    *wq = boxed(KCompletion::new()) as *mut c_void;
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_free_wait_queue(wq: *mut c_void) {
    cb!("os_free_wait_queue");
    unbox::<KCompletion>(wq);
}

unsafe fn wq_wait(wq: *mut c_void, what: &str) {
    let c = &*(wq as *const KCompletion);
    let mut waited = 0;
    while !c.wait_timeout(SLICE_NS) {
        waited += 5;
        slow_wait(what, wq, waited);
    }
}

#[no_mangle]
pub unsafe extern "C" fn os_wait_uninterruptible(wq: *mut c_void) {
    cb!("os_wait_uninterruptible");
    wq_wait(wq, "os_wait_uninterruptible");
}

#[no_mangle]
pub unsafe extern "C" fn os_wait_interruptible(wq: *mut c_void) {
    cb!("os_wait_interruptible");
    wq_wait(wq, "os_wait_interruptible");
}

/// `complete_all`: any context.
#[no_mangle]
pub unsafe extern "C" fn os_wake_up(wq: *mut c_void) {
    cb!("os_wake_up");
    (*(wq as *const KCompletion)).complete_all();
}
