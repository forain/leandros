//! Const-constructible sync primitives over the kapi mutex/semaphore/hrtimer handles (K2b), so a
//! module can keep them in `static`s like the built-in driver does. Handles are created lazily on
//! first use with a CAS: a loser of the race hands its own handle back (the kernel handle table is
//! permanent, so the loser's is simply never used; `HANDLE_LEAKS` counts them for ktest).
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kmod_abi::sys;

/// Handles created by a first-user that then lost the CAS.
pub static HANDLE_LEAKS: AtomicUsize = AtomicUsize::new(0);

/// Install `make()` into `slot` unless somebody else did first; returns the winning handle.
fn lazy(slot: &AtomicUsize, make: impl FnOnce() -> usize) -> usize {
    let h = slot.load(Ordering::Acquire);
    if h != 0 { return h; }
    let n = make();
    match slot.compare_exchange(0, n, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => n,
        Err(w) => { HANDLE_LEAKS.fetch_add(1, Ordering::Relaxed); w }
    }
}

/// Counting semaphore with a `const fn new`.
pub struct KSemaphore { h: AtomicUsize, initial: u32 }
impl KSemaphore {
    pub const fn new(initial: u32) -> Self { KSemaphore { h: AtomicUsize::new(0), initial } }
    pub fn handle(&self) -> usize { lazy(&self.h, || unsafe { sys::kapi_sem_new(self.initial) }) }
    pub fn up(&self) { unsafe { sys::kapi_sem_up(self.handle()) } }
    /// True when acquired within `ns`.
    pub fn down_timeout(&self, ns: u64) -> bool { unsafe { sys::kapi_sem_down_timeout(self.handle(), ns) } }
}

/// Sleeping mutex owning a `T`, with a `const fn new`.
pub struct KMutex<T> { h: AtomicUsize, name: &'static str, v: UnsafeCell<T> }
unsafe impl<T: Send> Sync for KMutex<T> {}
unsafe impl<T: Send> Send for KMutex<T> {}
pub struct KMutexGuard<'a, T> { m: &'a KMutex<T>, h: usize }
impl<T> KMutex<T> {
    pub const fn new(name: &'static str, v: T) -> Self { KMutex { h: AtomicUsize::new(0), name, v: UnsafeCell::new(v) } }
    pub fn handle(&self) -> usize { lazy(&self.h, || unsafe { sys::kapi_mutex_new(self.name.as_ptr(), self.name.len()) }) }
    pub fn lock(&self) -> KMutexGuard<'_, T> {
        let h = self.handle();
        unsafe { sys::kapi_mutex_lock(h) };
        KMutexGuard { m: self, h }
    }
    pub fn try_lock(&self) -> Option<KMutexGuard<'_, T>> {
        let h = self.handle();
        if unsafe { sys::kapi_mutex_trylock(h) } { Some(KMutexGuard { m: self, h }) } else { None }
    }
}
impl<T> Deref for KMutexGuard<'_, T> { type Target = T; fn deref(&self) -> &T { unsafe { &*self.m.v.get() } } }
impl<T> DerefMut for KMutexGuard<'_, T> { fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.m.v.get() } } }
impl<T> Drop for KMutexGuard<'_, T> { fn drop(&mut self) { unsafe { sys::kapi_mutex_unlock(self.h) } } }

/// Interrupt-safe spin lock: masks IRQs (`kapi_irq_save`), spins on an atomic, and accounts the
/// hold with `kapi_spin_held` so the scheduler knows not to sleep under it. Safe to take from an
/// ISR that can interrupt a holder on the same CPU, because the holder runs with IRQs masked.
pub struct KSpinLock<T> { locked: AtomicBool, v: UnsafeCell<T> }
unsafe impl<T: Send> Sync for KSpinLock<T> {}
unsafe impl<T: Send> Send for KSpinLock<T> {}
pub struct KSpinGuard<'a, T> { l: &'a KSpinLock<T>, flags: u64 }
impl<T> KSpinLock<T> {
    pub const fn new(v: T) -> Self { KSpinLock { locked: AtomicBool::new(false), v: UnsafeCell::new(v) } }
    pub fn lock(&self) -> KSpinGuard<'_, T> {
        let flags = unsafe { sys::kapi_irq_save() };
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() { core::hint::spin_loop(); }
        unsafe { sys::kapi_spin_held(true) };
        KSpinGuard { l: self, flags }
    }
    pub fn try_lock(&self) -> Option<KSpinGuard<'_, T>> {
        let flags = unsafe { sys::kapi_irq_save() };
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            unsafe { sys::kapi_spin_held(true) };
            Some(KSpinGuard { l: self, flags })
        } else { unsafe { sys::kapi_irq_restore(flags) }; None }
    }
}
impl<T> Deref for KSpinGuard<'_, T> { type Target = T; fn deref(&self) -> &T { unsafe { &*self.l.v.get() } } }
impl<T> DerefMut for KSpinGuard<'_, T> { fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.l.v.get() } } }
impl<T> Drop for KSpinGuard<'_, T> {
    fn drop(&mut self) {
        unsafe { sys::kapi_spin_held(false) };
        self.l.locked.store(false, Ordering::Release);
        unsafe { sys::kapi_irq_restore(self.flags) };
    }
}

/// One-shot high-resolution timer; the kernel timer is created lazily on first arm.
pub struct HrTimer { h: AtomicUsize, cb: extern "C" fn(usize), ctx: usize, irq_mode: bool }
impl HrTimer {
    pub const fn new(cb: extern "C" fn(usize), ctx: usize, irq_mode: bool) -> Self { HrTimer { h: AtomicUsize::new(0), cb, ctx, irq_mode } }
    pub fn handle(&self) -> usize { lazy(&self.h, || unsafe { sys::kapi_hrtimer_new(Some(self.cb), self.ctx, self.irq_mode) }) }
    pub fn arm(&self, ns: u64) { unsafe { sys::kapi_hrtimer_arm(self.handle(), ns) } }
    /// Cancel a pending expiry (no-op if the timer was never armed).
    pub fn cancel(&self) { let h = self.h.load(Ordering::Acquire); if h != 0 { unsafe { sys::kapi_hrtimer_cancel(h) } } }
}
