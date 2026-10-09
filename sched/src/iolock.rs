//! A fair, tick-friendly spinlock for locks that are held across slow
//! synchronous device I/O (lane nvstall, 2026-10-09).
//!
//! WHY. Every kernel `spin::Mutex` is an IRQ-off spinlock (syscalls and page
//! faults run with IRQs masked). That is fine for a lock held for
//! microseconds, and wrong for `F2FS_MOUNTS`, which is held for a whole file
//! read — on the real PC a read from a USB 2.0 stick, milliseconds per
//! command. The holder already keeps its CPU alive (the xHCI poll loop opens
//! `irq_window`s under `preempt_disable`), but every *waiter* spun with
//! interrupts masked: no timer tick on that CPU for as long as the queue in
//! front of it took. During the COSMIC session start on the PC (boot6) that
//! was the 61 `[WDOG] cpuN took no timer tick for ~2 s` reports, two of 12 s
//! (and a 20 s `mmap` in cosmic-comp). When the stalled CPU was the BSP, the
//! 100 Hz tick hooks stopped with it — DRM flip events, sync-file
//! signalling, USB HID polling, audio, poll deadlines — so the desktop froze
//! along with the loader. `spin::Mutex` is also unfair: a CPU could lose the
//! race for the lock again and again (the 12-20 s waits).
//!
//! WHAT. A ticket lock (FIFO: a waiter is served after exactly the holders
//! queued before it) whose waiters
//!   * open an `irq_window` every ~1k spins when they run with IRQs masked,
//!     under `preempt_disable` so the window takes the tick, tick hooks and
//!     device interrupts but never switches tasks (this CPU may hold other
//!     spinlocks — the same contract as the USB/virtio poll windows);
//!   * service TLB-shootdown requests while they spin (a holder may be
//!     waiting for this CPU's acknowledgement);
//!   * publish themselves to `lockwatch`, so a `[WDOG]` line still names the
//!     lock and its holder if something else stalls the CPU.
//! A task that takes the lock with IRQs *enabled* runs with preemption
//! disabled from the moment it takes a ticket until it releases the lock: a
//! ticket holder that got switched out would stall every CPU queued behind it.
//!
//! DIAGNOSTICS. Waits, total and maximum wait, and maximum hold time are
//! counted per lock; a wait of `LONG_WAIT_NS` or more prints one
//! `[LOCKWAIT]` line on the raw UART (at most one per second), naming the
//! waiter, its syscall, the holder and how long the holder has had it.
//!
//! NOT for IRQ context (there is no try_lock): an interrupt handler that
//! spun on a ticket would deadlock against its own CPU.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// A wait this long is reported as `[LOCKWAIT]`.
pub const LONG_WAIT_NS: u64 = 500_000_000;
/// Spins between two interrupt windows (~40 us of `pause` on a 2017 core).
const WINDOW_SPINS: u32 = 1024;

pub struct IoMutex<T> {
    next: AtomicU32,
    serving: AtomicU32,
    /// `lockwatch` id (0 = untracked).
    id: u8,
    name: &'static str,
    /// Holder's pid and the `monotonic_ns` it took the lock at (diagnostics).
    holder_pid: AtomicU32,
    hold_since: AtomicU64,
    waits: AtomicU64,
    wait_ns: AtomicU64,
    wait_max_ns: AtomicU64,
    hold_max_ns: AtomicU64,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for IoMutex<T> {}
unsafe impl<T: Send> Send for IoMutex<T> {}

pub struct IoGuard<'a, T> {
    m: &'a IoMutex<T>,
    /// This guard disabled preemption (taken with IRQs enabled).
    preempt: bool,
}

/// `(waits, total wait ns, max wait ns, max hold ns)`.
pub type IoLockStats = (u64, u64, u64, u64);

static LAST_REPORT_NS: AtomicU64 = AtomicU64::new(0);
static SUPPRESSED: AtomicU32 = AtomicU32::new(0);

impl<T> IoMutex<T> {
    pub const fn new(id: u8, name: &'static str, value: T) -> Self {
        Self {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
            id,
            name,
            holder_pid: AtomicU32::new(0),
            hold_since: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
            wait_max_ns: AtomicU64::new(0),
            hold_max_ns: AtomicU64::new(0),
            data: UnsafeCell::new(value),
        }
    }

    #[inline]
    pub fn lock(&self) -> IoGuard<'_, T> {
        let preempt = !crate::irqs_masked() && crate::current_pid() != 0;
        if preempt { crate::preempt_disable(); }
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        if self.serving.load(Ordering::Acquire) != ticket {
            self.wait(ticket);
        }
        let now = crate::monotonic_ns();
        self.hold_since.store(now, Ordering::Relaxed);
        self.holder_pid.store(crate::current_pid(), Ordering::Relaxed);
        if self.id != 0 { crate::lockwatch::note_hold(self.id, true); }
        IoGuard { m: self, preempt }
    }

    #[cold]
    #[inline(never)]
    fn wait(&self, ticket: u32) {
        if self.id != 0 { crate::lockwatch::note_wait(self.id); }
        let t0 = crate::monotonic_ns();
        // IRQs masked in task context (syscall, page fault): keep the CPU's
        // tick alive. Boot (pid 0) and IRQs-on callers just spin (their
        // interrupts are already open).
        let window = crate::irqs_masked() && crate::current_pid() != 0;
        if window { crate::preempt_disable(); }
        let mut spins = 0u32;
        while self.serving.load(Ordering::Acquire) != ticket {
            mm::paging::tlb_service_pending();
            spins = spins.wrapping_add(1);
            if window && spins % WINDOW_SPINS == 0 {
                crate::irq_window();
            }
            core::hint::spin_loop();
        }
        if window { crate::preempt_enable_no_resched(); }
        if self.id != 0 { crate::lockwatch::note_wait(0); }
        let now = crate::monotonic_ns();
        let waited = now.saturating_sub(t0);
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.wait_ns.fetch_add(waited, Ordering::Relaxed);
        self.wait_max_ns.fetch_max(waited, Ordering::Relaxed);
        if waited >= LONG_WAIT_NS {
            self.report_long_wait(waited, now);
        }
    }

    /// `[LOCKWAIT]` line (raw UART, no lock, no allocation). The previous
    /// holder is gone by now; what is printed is who waited, for how long,
    /// and how many tickets were still queued behind it.
    fn report_long_wait(&self, waited: u64, now: u64) {
        let last = LAST_REPORT_NS.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < 1_000_000_000 {
            SUPPRESSED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        LAST_REPORT_NS.store(now, Ordering::Relaxed);
        extern "C" { fn arch_serial_putc(c: u8); }
        fn s(m: &str) { for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } } }
        fn n(v: u64) {
            if v == 0 { unsafe { arch_serial_putc(b'0') }; return; }
            let mut buf = [0u8; 20]; let mut i = 0; let mut x = v;
            while x > 0 { buf[i] = b'0' + (x % 10) as u8; x /= 10; i += 1; }
            while i > 0 { i -= 1; unsafe { arch_serial_putc(buf[i]) } }
        }
        let cpu = unsafe { crate::cpu_id() };
        let queued = self.next.load(Ordering::Relaxed).wrapping_sub(self.serving.load(Ordering::Relaxed));
        s("\n[LOCKWAIT] "); s(self.name); s(": cpu"); n(cpu as u64);
        s(" pid "); n(crate::current_pid() as u64);
        let sc = crate::syscall_on_cpu(cpu);
        if sc == crate::NO_SYSCALL { s(" (fault/kernel)"); } else { s(" (syscall "); n(sc as u64); s(")"); }
        s(" waited "); n(waited / 1_000_000); s(" ms with IRQ windows open; ");
        n(queued.saturating_sub(1) as u64); s(" more queued; waits "); n(self.waits.load(Ordering::Relaxed));
        s(", avg wait us "); n(self.wait_ns.load(Ordering::Relaxed) / self.waits.load(Ordering::Relaxed).max(1) / 1000);
        s(", max hold ms "); n(self.hold_max_ns.load(Ordering::Relaxed) / 1_000_000);
        let sup = SUPPRESSED.swap(0, Ordering::Relaxed);
        if sup != 0 { s(" ("); n(sup as u64); s(" similar suppressed)"); }
        s("\n");
    }

    /// Counters for summaries and tests.
    pub fn stats(&self) -> IoLockStats {
        (self.waits.load(Ordering::Relaxed), self.wait_ns.load(Ordering::Relaxed),
         self.wait_max_ns.load(Ordering::Relaxed), self.hold_max_ns.load(Ordering::Relaxed))
    }

    pub fn is_locked(&self) -> bool {
        self.next.load(Ordering::Relaxed) != self.serving.load(Ordering::Relaxed)
    }

    /// Holder pid and how long it has held the lock (0, 0 when free).
    pub fn holder(&self) -> (u32, u64) {
        if !self.is_locked() { return (0, 0); }
        (self.holder_pid.load(Ordering::Relaxed),
         crate::monotonic_ns().saturating_sub(self.hold_since.load(Ordering::Relaxed)))
    }
}

impl<'a, T> Drop for IoGuard<'a, T> {
    #[inline]
    fn drop(&mut self) {
        let m = self.m;
        let held = crate::monotonic_ns().saturating_sub(m.hold_since.load(Ordering::Relaxed));
        m.hold_max_ns.fetch_max(held, Ordering::Relaxed);
        if m.id != 0 { crate::lockwatch::note_hold(m.id, false); }
        // Only the holder advances `serving`.
        let s = m.serving.load(Ordering::Relaxed);
        m.serving.store(s.wrapping_add(1), Ordering::Release);
        if self.preempt { crate::preempt_enable(); }
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
