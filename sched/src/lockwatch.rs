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
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

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

// ── RUN_QUEUE hold profile ──────────────────────────────────────────────────
//
// Who holds RUN_QUEUE, from where, for how long. Every acquire of RUN_QUEUE
// records the caller's `Location` (via `#[track_caller]`) and the monotonic
// time it took the lock; a tick-context lock attempt that fails charges the
// failure to the site holding the lock at that instant, with the age of that
// hold and the holder's pid/syscall. When `HOLD_PROFILE` is on, every release
// also feeds a per-site hold-count/duration histogram. Read with Ctrl-T
// (`dump_tasks` prints a `[RQPROF]` block) — see `dump_profile`.
//
// Cost with `HOLD_PROFILE` off (the default): one counter read and two
// relaxed stores per RUN_QUEUE acquire, nothing on release. With it on: a
// ≤`PROFILE_SITES`-entry scan per acquire plus a counter read and four RMWs
// per release — flip it for a profiling build only.
//
// What it found (2026-09-18, greeter idle, aarch64/HVF): no long holder at
// all — 99.9 % of holds were < 1 µs — but ~760 k acquisitions/s, four per
// synchronous server call (reply-port lookup, park-prepare, wake-scan of the
// whole queue, park-cancel) at ~170 k calls/s from the compositor, for a
// ~9 % duty cycle that the tick's one-shot try_lock lost 8 % of the time.
pub const HOLD_PROFILE: bool = false;
pub const PROFILE_SITES: usize = 64;
const NBUCKETS: usize = 7; // <1µs <4µs <16µs <64µs <256µs <1ms ≥1ms
const BUCKET_NS: [u64; NBUCKETS - 1] = [1_000, 4_000, 16_000, 64_000, 256_000, 1_000_000];

static SITE_LOC:      [AtomicUsize; PROFILE_SITES] = [const { AtomicUsize::new(0) }; PROFILE_SITES];
static SITE_HOLDS:    [AtomicU32; PROFILE_SITES]   = [const { AtomicU32::new(0) }; PROFILE_SITES];
static SITE_NS:       [AtomicU64; PROFILE_SITES]   = [const { AtomicU64::new(0) }; PROFILE_SITES];
static SITE_MAX_NS:   [AtomicU32; PROFILE_SITES]   = [const { AtomicU32::new(0) }; PROFILE_SITES];
static SITE_HIST:     [[AtomicU32; NBUCKETS]; PROFILE_SITES] =
    [const { [const { AtomicU32::new(0) }; NBUCKETS] }; PROFILE_SITES];
/// Tick try_lock failures charged to this site, and the age of the hold then.
static SITE_TFAIL:    [AtomicU32; PROFILE_SITES]   = [const { AtomicU32::new(0) }; PROFILE_SITES];
static SITE_TFAIL_NS: [AtomicU64; PROFILE_SITES]   = [const { AtomicU64::new(0) }; PROFILE_SITES];
static SITE_TFAIL_MAX:[AtomicU32; PROFILE_SITES]   = [const { AtomicU32::new(0) }; PROFILE_SITES];
/// pid / syscall of the holder at the most recent charged failure.
static SITE_TFAIL_PID:[AtomicU32; PROFILE_SITES]   = [const { AtomicU32::new(0) }; PROFILE_SITES];
static SITE_TFAIL_SC: [AtomicU32; PROFILE_SITES]   = [const { AtomicU32::new(0) }; PROFILE_SITES];
/// Current holder's acquire site (`Location` pointer, 0 = unknown) and
/// acquire time, per lock. Always maintained for RUN_QUEUE.
static CUR_LOC:   [AtomicUsize; N_LOCKS] = [const { AtomicUsize::new(0) }; N_LOCKS];
static CUR_SINCE: [AtomicU64; N_LOCKS] = [const { AtomicU64::new(0) }; N_LOCKS];
/// Tick-context try_lock attempts / failures / failures where the holder was
/// this very CPU (an `irq_window` under the lock) / failures with no holder
/// recorded (lost the race between the CAS and the bookkeeping store).
static TICK_TRY:      AtomicU32 = AtomicU32::new(0);
static TICK_FAIL:     AtomicU32 = AtomicU32::new(0);
static TICK_FAIL_SELF:AtomicU32 = AtomicU32::new(0);
static TICK_FAIL_FREE:AtomicU32 = AtomicU32::new(0);
/// `try_wake_poll_tagged` attempts / failures from any context (vfs pipe
/// edges, DRM IRQs, the tick's deferred-wake pass).
static WAKE_TRY:  AtomicU32 = AtomicU32::new(0);
static WAKE_FAIL: AtomicU32 = AtomicU32::new(0);
/// `try_lock_spin` outcomes per lock: acquisitions that needed a spin, the
/// longest such spin, and spins that hit the bound.
static SPIN_HITS:    [AtomicU32; N_LOCKS] = [const { AtomicU32::new(0) }; N_LOCKS];
static SPIN_MAX_NS:  [AtomicU32; N_LOCKS] = [const { AtomicU32::new(0) }; N_LOCKS];
static SPIN_GAVE_UP: [AtomicU32; N_LOCKS] = [const { AtomicU32::new(0) }; N_LOCKS];
/// Hold-age histogram at tick failure (same buckets).
static TFAIL_HIST: [AtomicU32; NBUCKETS] = [const { AtomicU32::new(0) }; NBUCKETS];

extern "C" { fn arch_monotonic_ns() -> u64; }

#[inline(always)]
fn now_ns() -> u64 { unsafe { arch_monotonic_ns() } }

fn bucket(ns: u64) -> usize {
    let mut b = 0;
    while b < NBUCKETS - 1 && ns >= BUCKET_NS[b] { b += 1; }
    b
}

/// Slot for `loc` in the site table (inserting it if new); `PROFILE_SITES`
/// when the table is full.
fn site_slot(loc: &'static core::panic::Location<'static>) -> usize {
    let key = loc as *const _ as usize;
    for i in 0..PROFILE_SITES {
        let cur = SITE_LOC[i].load(Ordering::Relaxed);
        if cur == key { return i; }
        if cur == 0 {
            match SITE_LOC[i].compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return i,
                Err(other) => if other == key { return i; },
            }
        }
    }
    PROFILE_SITES
}

/// Bookkeeping on acquire: who/where/when. Returns (site+1, since) for the
/// guard.
#[inline]
fn on_acquire(id: u8, loc: &'static core::panic::Location<'static>) -> (u32, u64) {
    if id != L_RUN_QUEUE { return (0, 0); }
    let t = now_ns();
    CUR_LOC[id as usize].store(loc as *const _ as usize, Ordering::Relaxed);
    CUR_SINCE[id as usize].store(t, Ordering::Relaxed);
    if !HOLD_PROFILE { return (0, t); }
    let slot = site_slot(loc);
    (if slot < PROFILE_SITES { slot as u32 + 1 } else { 0 }, t)
}

#[inline]
fn on_release(id: u8, site1: u32, since: u64) {
    if !HOLD_PROFILE || id != L_RUN_QUEUE || site1 == 0 { return; }
    let held = now_ns().saturating_sub(since);
    let i = site1 as usize - 1;
    SITE_HOLDS[i].fetch_add(1, Ordering::Relaxed);
    SITE_NS[i].fetch_add(held, Ordering::Relaxed);
    SITE_MAX_NS[i].fetch_max(held.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    SITE_HIST[i][bucket(held)].fetch_add(1, Ordering::Relaxed);
}

/// A tick-context `try_lock` on `id` (RUN_QUEUE) was attempted and, if
/// `!ok`, failed: charge the failure to the current holder.
pub fn note_tick_try(id: u8, ok: bool) {
    if id != L_RUN_QUEUE { return; }
    TICK_TRY.fetch_add(1, Ordering::Relaxed);
    if ok { return; }
    TICK_FAIL.fetch_add(1, Ordering::Relaxed);
    let h = HOLDER[id as usize].load(Ordering::Relaxed);
    if h == 0 { TICK_FAIL_FREE.fetch_add(1, Ordering::Relaxed); return; }
    let hcpu = h as usize - 1;
    if hcpu == me() { TICK_FAIL_SELF.fetch_add(1, Ordering::Relaxed); }
    let age = now_ns().saturating_sub(CUR_SINCE[id as usize].load(Ordering::Relaxed));
    TFAIL_HIST[bucket(age)].fetch_add(1, Ordering::Relaxed);
    let key = CUR_LOC[id as usize].load(Ordering::Relaxed);
    if key == 0 { return; }
    let i = site_slot(unsafe { &*(key as *const core::panic::Location<'static>) });
    if i >= PROFILE_SITES { return; }
    SITE_TFAIL[i].fetch_add(1, Ordering::Relaxed);
    SITE_TFAIL_NS[i].fetch_add(age, Ordering::Relaxed);
    SITE_TFAIL_MAX[i].fetch_max(age.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    SITE_TFAIL_PID[i].store(super::pid_on_cpu(hcpu), Ordering::Relaxed);
    SITE_TFAIL_SC[i].store(super::syscall_on_cpu(hcpu), Ordering::Relaxed);
}

pub fn note_wake_try(ok: bool) {
    WAKE_TRY.fetch_add(1, Ordering::Relaxed);
    if !ok { WAKE_FAIL.fetch_add(1, Ordering::Relaxed); }
}

/// Tick try_lock (attempts, failures) so far — for tests and the report.
pub fn tick_try_stats() -> (u32, u32) {
    (TICK_TRY.load(Ordering::Relaxed), TICK_FAIL.load(Ordering::Relaxed))
}

/// Print the RUN_QUEUE profile on the raw UART. IRQ-safe: atomics only.
pub fn dump_profile() {
    extern "C" { fn arch_serial_putc(c: u8); }
    fn s(m: &str) { for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } } }
    fn n(v: u64) {
        if v == 0 { unsafe { arch_serial_putc(b'0') }; return; }
        let mut buf = [0u8; 20]; let mut i = 0; let mut x = v;
        while x > 0 { buf[i] = b'0' + (x % 10) as u8; x /= 10; i += 1; }
        while i > 0 { i -= 1; unsafe { arch_serial_putc(buf[i]) } }
    }
    fn hex(v: u64) { s("0x"); let d = b"0123456789abcdef"; let mut st = false;
        for i in (0..16).rev() { let c = ((v >> (i * 4)) & 0xF) as usize;
            if c != 0 || st || i == 0 { st = true; unsafe { arch_serial_putc(d[c]) } } } }
    let (tr, tf) = tick_try_stats();
    s("[RQPROF] tick try_lock: attempts="); n(tr as u64); s(" failed="); n(tf as u64);
    s(" self="); n(TICK_FAIL_SELF.load(Ordering::Relaxed) as u64);
    s(" free="); n(TICK_FAIL_FREE.load(Ordering::Relaxed) as u64);
    s(" | spun="); n(SPIN_HITS[L_RUN_QUEUE as usize].load(Ordering::Relaxed) as u64);
    s(" max="); n(SPIN_MAX_NS[L_RUN_QUEUE as usize].load(Ordering::Relaxed) as u64);
    s("ns gave_up="); n(SPIN_GAVE_UP[L_RUN_QUEUE as usize].load(Ordering::Relaxed) as u64);
    s(" | wake try="); n(WAKE_TRY.load(Ordering::Relaxed) as u64);
    s(" failed="); n(WAKE_FAIL.load(Ordering::Relaxed) as u64);
    s(" | age-at-fail:");
    for b in 0..NBUCKETS { s(" "); n(TFAIL_HIST[b].load(Ordering::Relaxed) as u64); }
    s("  (buckets <1u <4u <16u <64u <256u <1m >=1m)
");
    for i in 0..PROFILE_SITES {
        let key = SITE_LOC[i].load(Ordering::Relaxed);
        if key == 0 { break; }
        let loc = unsafe { &*(key as *const core::panic::Location<'static>) };
        let holds = SITE_HOLDS[i].load(Ordering::Relaxed);
        let tf = SITE_TFAIL[i].load(Ordering::Relaxed);
        if holds == 0 && tf == 0 { continue; }
        s("[RQPROF] ");
        let f = loc.file(); let base = f.rsplit('/').next().unwrap_or(f);
        s(base); s(":"); n(loc.line() as u64);
        s(" holds="); n(holds as u64);
        if holds > 0 {
            s(" avg="); n(SITE_NS[i].load(Ordering::Relaxed) / holds as u64);
            s("ns max="); n(SITE_MAX_NS[i].load(Ordering::Relaxed) as u64); s("ns hist:");
            for b in 0..NBUCKETS { s(" "); n(SITE_HIST[i][b].load(Ordering::Relaxed) as u64); }
        }
        if tf > 0 {
            s(" | tickfail="); n(tf as u64);
            s(" age avg="); n(SITE_TFAIL_NS[i].load(Ordering::Relaxed) / tf as u64);
            s("ns max="); n(SITE_TFAIL_MAX[i].load(Ordering::Relaxed) as u64);
            s("ns last pid="); n(SITE_TFAIL_PID[i].load(Ordering::Relaxed) as u64);
            s(" sc="); hex(SITE_TFAIL_SC[i].load(Ordering::Relaxed) as u64);
        }
        s("\n");
    }
}

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
    site1: u32,
    since: u64,
    guard: spin::MutexGuard<'a, T>,
}

impl<T> TrackedMutex<T> {
    pub const fn new(id: u8, value: T) -> Self {
        Self { id, inner: spin::Mutex::new(value) }
    }

    #[inline]
    #[track_caller]
    pub fn lock(&self) -> TrackedGuard<'_, T> {
        let cpu = me();
        let loc = core::panic::Location::caller();
        if let Some(guard) = self.inner.try_lock() {
            HOLDER[self.id as usize].store(cpu as u8 + 1, Ordering::Relaxed);
            let (site1, since) = on_acquire(self.id, loc);
            return TrackedGuard { id: self.id, site1, since, guard };
        }
        WANT[cpu].store(self.id, Ordering::Relaxed);
        let guard = self.inner.lock();
        WANT[cpu].store(0, Ordering::Relaxed);
        HOLDER[self.id as usize].store(cpu as u8 + 1, Ordering::Relaxed);
        let (site1, since) = on_acquire(self.id, loc);
        TrackedGuard { id: self.id, site1, since, guard }
    }

    #[inline]
    #[track_caller]
    pub fn try_lock(&self) -> Option<TrackedGuard<'_, T>> {
        let guard = self.inner.try_lock()?;
        HOLDER[self.id as usize].store(me() as u8 + 1, Ordering::Relaxed);
        let (site1, since) = on_acquire(self.id, core::panic::Location::caller());
        Some(TrackedGuard { id: self.id, site1, since, guard })
    }

    /// `try_lock` that keeps trying for up to `max_ns` before giving up.
    ///
    /// For IRQ-context callers (the poll-deadline tick) that must never block
    /// indefinitely but can afford a bounded wait: the holds they collide
    /// with are sub-microsecond (99.98 % of RUN_QUEUE holds are < 4 µs), so a
    /// one-shot `try_lock` that fails defers real work — a timed poll wake —
    /// by a whole 10 ms tick to avoid a wait of a few hundred ns. The bound
    /// keeps every deadlock-freedom argument of `try_lock`: the worst case is
    /// `max_ns` of spinning, then the same deferral as before.
    #[inline]
    #[track_caller]
    pub fn try_lock_spin(&self, max_ns: u64) -> Option<TrackedGuard<'_, T>> {
        if let Some(g) = self.try_lock() { return Some(g); }
        let t0 = now_ns();
        loop {
            core::hint::spin_loop();
            if let Some(g) = self.try_lock() {
                let waited = now_ns().saturating_sub(t0);
                SPIN_HITS[self.id as usize].fetch_add(1, Ordering::Relaxed);
                SPIN_MAX_NS[self.id as usize].fetch_max(waited.min(u32::MAX as u64) as u32, Ordering::Relaxed);
                return Some(g);
            }
            if now_ns().saturating_sub(t0) >= max_ns {
                SPIN_GAVE_UP[self.id as usize].fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
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
        on_release(self.id, self.site1, self.since);
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
