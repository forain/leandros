//! Holder/waiter bookkeeping for the kernel's hot global spinlocks.
//!
//! Every kernel `spin::Mutex` is an IRQ-off spinlock (syscalls run with IRQs
//! masked), so a CPU that deadlocks on one goes silent: no tick, no output,
//! no panic. The per-CPU tick watchdog in `lib.rs` notices the silence from a
//! CPU that is still alive; this module lets it also *name the lock*. A
//! [`TrackedMutex`] is a drop-in `spin::Mutex` — same `lock()` / `try_lock()`
//! shape, same guard semantics — that publishes which CPU holds it and which
//! lock each CPU is currently spinning for. The watchdog line then reads
//! "cpu1 waits for RUN_QUEUE held by cpu0; cpu0 waits for PIPEWIRE held by
//! cpu1", which is the whole diagnosis.
//!
//! Cost on the uncontended path: one `try_lock` (the same CAS `lock()` would
//! do first) plus one relaxed byte store on acquire and one on release. The
//! contended path adds two relaxed stores around the spin. Only the handful of
//! locks named in [`NAMES`] are tracked; everything else stays a plain
//! `spin::Mutex`.

use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU8, Ordering};

/// Lock ids. Index into [`NAMES`]; 0 is "none".
pub const L_RUN_QUEUE: u8 = 1;
pub const L_PIPEWIRE: u8 = 2;
pub const L_FD_TABLES: u8 = 3;
pub const L_PIPE_RINGS: u8 = 4;
pub const L_VIRTIO_GPU: u8 = 5;
pub const L_PORT_TABLE: u8 = 6;
pub const L_EPOLL: u8 = 7;
/// The per-address-space `busy` flag (`lock_leader_address_space`): not a
/// Mutex, so it reports through [`note_wait`] / [`note_hold`] by hand.
pub const L_AS_BUSY: u8 = 8;
pub const N_LOCKS: usize = 9;

pub static NAMES: [&str; N_LOCKS] = [
    "-", "RUN_QUEUE", "PIPEWIRE_STATE", "FD_TABLES", "PIPE_RINGS", "VIRTIO_GPU", "PORT_TABLE", "EPOLL",
    "ADDRSPACE_BUSY",
];

/// `HOLDER[id]` = cpu + 1 of the current holder, 0 = free.
static HOLDER: [AtomicU8; N_LOCKS] = [const { AtomicU8::new(0) }; N_LOCKS];
/// `WANT[cpu]` = lock id this CPU is spinning for right now, 0 = none.
static WANT: [AtomicU8; super::MAX_CPUS] = [const { AtomicU8::new(0) }; super::MAX_CPUS];

#[inline(always)]
fn me() -> usize {
    (unsafe { super::cpu_id() }).min(super::MAX_CPUS - 1)
}

/// Which CPU holds `id` (None = free). For the watchdog.
pub fn holder(id: u8) -> Option<usize> {
    let h = HOLDER[(id as usize).min(N_LOCKS - 1)].load(Ordering::Relaxed);
    if h == 0 { None } else { Some(h as usize - 1) }
}

/// Which lock `cpu` is spinning for (0 = none). For the watchdog.
pub fn wanted_by(cpu: usize) -> u8 {
    WANT[cpu.min(super::MAX_CPUS - 1)].load(Ordering::Relaxed)
}

/// Hand-rolled locks: say this CPU is spinning for `id` (0 = done waiting).
#[inline]
pub fn note_wait(id: u8) {
    WANT[me()].store(id, Ordering::Relaxed);
}

/// Hand-rolled locks: this CPU took (`held`) or released `id`. Several CPUs
/// can hold distinct address spaces at once; the slot records the most recent
/// taker, which is still the right CPU to look at when everything is stuck.
#[inline]
pub fn note_hold(id: u8, held: bool) {
    let cpu = me() as u8 + 1;
    let slot = &HOLDER[(id as usize).min(N_LOCKS - 1)];
    if held {
        slot.store(cpu, Ordering::Relaxed);
    } else {
        let _ = slot.compare_exchange(cpu, 0, Ordering::Relaxed, Ordering::Relaxed);
    }
}

pub fn name(id: u8) -> &'static str {
    NAMES[(id as usize).min(N_LOCKS - 1)]
}

pub struct TrackedMutex<T> {
    id: u8,
    inner: spin::Mutex<T>,
}

pub struct TrackedGuard<'a, T> {
    id: u8,
    guard: spin::MutexGuard<'a, T>,
}

impl<T> TrackedMutex<T> {
    pub const fn new(id: u8, value: T) -> Self {
        Self { id, inner: spin::Mutex::new(value) }
    }

    #[inline]
    pub fn lock(&self) -> TrackedGuard<'_, T> {
        let cpu = me();
        if let Some(guard) = self.inner.try_lock() {
            HOLDER[self.id as usize].store(cpu as u8 + 1, Ordering::Relaxed);
            return TrackedGuard { id: self.id, guard };
        }
        WANT[cpu].store(self.id, Ordering::Relaxed);
        let guard = self.inner.lock();
        WANT[cpu].store(0, Ordering::Relaxed);
        HOLDER[self.id as usize].store(cpu as u8 + 1, Ordering::Relaxed);
        TrackedGuard { id: self.id, guard }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<TrackedGuard<'_, T>> {
        let guard = self.inner.try_lock()?;
        HOLDER[self.id as usize].store(me() as u8 + 1, Ordering::Relaxed);
        Some(TrackedGuard { id: self.id, guard })
    }

    #[inline]
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }
}

impl<'a, T> Drop for TrackedGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        // Clear the holder BEFORE the inner guard releases the lock (fields
        // drop after this body), so a new holder's store can never be wiped
        // by a late clear from the previous one.
        HOLDER[self.id as usize].store(0, Ordering::Relaxed);
    }
}

impl<'a, T> Deref for TrackedGuard<'a, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T { &self.guard }
}

impl<'a, T> DerefMut for TrackedGuard<'a, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T { &mut self.guard }
}
