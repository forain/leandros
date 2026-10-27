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
/// The f2fs mount table (`sched::iolock::IoMutex`), held across block I/O.
pub const L_F2FS: u8 = 9;
/// The per-process fd tables (one lock per process, one class here); the
/// tgid → table map is `L_FD_TABLES`.
pub const L_FD_TABLE: u8 = 10;
/// The per-CPU run-queue locks `cpuq::RQ[c]` (one class; lane percpu). Below
/// RUN_QUEUE in the lock order: an RQ holder never takes RUN_QUEUE
/// (asserted in `TrackedMutex::lock`/`try_lock`).
pub const L_RQ: u8 = 11;
/// The AF_UNIX connection table (`servers/net` `UNIX_CONNS`; lane ipcfast).
pub const L_UNIX_CONNS: u8 = 12;
/// The per-process socket tables (`servers/net` `SOCK_TABLES`; lane ipcfast).
pub const L_SOCK_TABLES: u8 = 13;
pub const N_LOCKS: usize = 14;

pub static NAMES: [&str; N_LOCKS] = [
    "-", "RUN_QUEUE", "PIPEWIRE_STATE", "FD_TABLES", "PIPE_RINGS", "VIRTIO_GPU", "PORT_TABLE", "EPOLL",
    "ADDRSPACE_BUSY", "F2FS_MOUNTS", "FD_TABLE", "RQ", "UNIX_CONNS", "SOCK_TABLES",
];

/// Lock order RUN_QUEUE (TASKS) → RQ: taking TASKS while this CPU holds a
/// run-queue lock is a deadlock waiting to happen.
#[inline]
#[track_caller]
fn assert_no_rq(id: u8) {
    if id == L_RUN_QUEUE && super::cpuq::held_here() != 0 {
        panic!("[SCHED] RUN_QUEUE taken while holding an RQ lock at {}", core::panic::Location::caller());
    }
}

// ── Per-lock contention counters (`/proc/lockstat`) ─────────────────────────
//
// For every tracked lock id: acquisitions through `lock()`, how many of them
// found the lock held, and the nanoseconds spent spinning for it. Per CPU, so
// counting a hot lock's acquisitions does not itself bounce a cache line
// between CPUs; summed when read. The contended path already reads the clock
// on the profiled lock only, so this adds one clock read pair to a contended
// acquire (which is spinning anyway) and one CPU-local add to every acquire.
static ACQ:     [[AtomicU64; N_LOCKS]; super::MAX_CPUS] = [const { [const { AtomicU64::new(0) }; N_LOCKS] }; super::MAX_CPUS];
static CONT:    [[AtomicU64; N_LOCKS]; super::MAX_CPUS] = [const { [const { AtomicU64::new(0) }; N_LOCKS] }; super::MAX_CPUS];
static CONT_NS: [[AtomicU64; N_LOCKS]; super::MAX_CPUS] = [const { [const { AtomicU64::new(0) }; N_LOCKS] }; super::MAX_CPUS];

/// `(acquisitions, contended acquisitions, ns spent spinning)` of lock `id`
/// since boot, summed over CPUs.
pub fn lock_stats(id: u8) -> (u64, u64, u64) {
    let i = (id as usize).min(N_LOCKS - 1);
    let mut r = (0, 0, 0);
    for c in 0..super::MAX_CPUS {
        r.0 += ACQ[c][i].load(Ordering::Relaxed);
        r.1 += CONT[c][i].load(Ordering::Relaxed);
        r.2 += CONT_NS[c][i].load(Ordering::Relaxed);
    }
    r
}

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
/// Contended acquires (fast try_lock failed) and total ns spent spinning.
static SITE_CWAITS:   [AtomicU32; PROFILE_SITES]   = [const { AtomicU32::new(0) }; PROFILE_SITES];
static SITE_CWAIT_NS: [AtomicU64; PROFILE_SITES]   = [const { AtomicU64::new(0) }; PROFILE_SITES];
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
/// Ready tasks `RunQueue::pick_next`'s once-per-tick full scan found with
/// their `maybe_ready` bit clear — must stay 0 (a non-zero count is a `&mut
/// Task` path that bypasses the hint; each costs that task ≤ 1 tick).
static READY_HINT_MISS: AtomicU32 = AtomicU32::new(0);
static PICK_N: AtomicU32 = AtomicU32::new(0);
static PICK_FULL: AtomicU32 = AtomicU32::new(0);
static PICK_VISITED: AtomicU64 = AtomicU64::new(0);
static PICK_NS: AtomicU64 = AtomicU64::new(0);
/// `pick_next` census (HOLD_PROFILE only): picks, full scans, tasks visited, ns.
#[inline]
pub fn note_pick(full: bool, visited: u32, ns: u64) {
    if !HOLD_PROFILE { return; }
    PICK_N.fetch_add(1, Ordering::Relaxed);
    if full { PICK_FULL.fetch_add(1, Ordering::Relaxed); }
    PICK_VISITED.fetch_add(visited as u64, Ordering::Relaxed);
    PICK_NS.fetch_add(ns, Ordering::Relaxed);
}
#[inline]
pub fn pick_clock() -> u64 { if HOLD_PROFILE { now_ns() } else { 0 } }
/// Address-space acquisition census (HOLD_PROFILE only): every
/// `lock_leader_address_space` success — how long getting it took (RUN_QUEUE
/// wait + `busy` spin; `contended` = it had to spin on `busy`) and how long
/// `busy` was then held.
static AS_ACQ: AtomicU32 = AtomicU32::new(0);
static AS_CONT: AtomicU32 = AtomicU32::new(0);
static AS_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static AS_WAIT_MAX: AtomicU32 = AtomicU32::new(0);
static AS_HOLD_NS: AtomicU64 = AtomicU64::new(0);
static AS_HOLD_MAX: AtomicU32 = AtomicU32::new(0);
static AS_SINCE: [AtomicU64; super::MAX_CPUS] = [const { AtomicU64::new(0) }; super::MAX_CPUS];
#[inline]
pub fn as_clock() -> u64 { if HOLD_PROFILE { now_ns() } else { 0 } }
#[inline]
pub fn note_as_acquired(t0: u64, contended: bool) {
    if !HOLD_PROFILE { return; }
    let t = now_ns();
    let w = t.saturating_sub(t0);
    AS_ACQ.fetch_add(1, Ordering::Relaxed);
    if contended { AS_CONT.fetch_add(1, Ordering::Relaxed); }
    AS_WAIT_NS.fetch_add(w, Ordering::Relaxed);
    AS_WAIT_MAX.fetch_max(w.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    AS_SINCE[me()].store(t, Ordering::Relaxed);
}
#[inline]
pub fn note_as_released() {
    if !HOLD_PROFILE { return; }
    let h = now_ns().saturating_sub(AS_SINCE[me()].load(Ordering::Relaxed));
    AS_HOLD_NS.fetch_add(h, Ordering::Relaxed);
    AS_HOLD_MAX.fetch_max(h.min(u32::MAX as u64) as u32, Ordering::Relaxed);
}
pub fn note_ready_hint_miss() { READY_HINT_MISS.fetch_add(1, Ordering::Relaxed); }
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

// ── Always-on RUN_QUEUE hold census (`/proc/rqhold`, lane deskbusy2) ────────
//
// Per acquire site (the `#[track_caller]` location of `RUN_QUEUE.lock()` /
// `try_lock*`): holds and total ns held. One clock read and three relaxed
// adds per release plus a short hash probe — affordable always, unlike
// `HOLD_PROFILE`'s linear site scan. Answers "how much of the lock's duty
// cycle is wake scans vs parks vs picks".
const HSITES: usize = 512;
static HS_LOC: [AtomicUsize; HSITES] = [const { AtomicUsize::new(0) }; HSITES];
static HS_N:   [AtomicU64; HSITES] = [const { AtomicU64::new(0) }; HSITES];
static HS_NS:  [AtomicU64; HSITES] = [const { AtomicU64::new(0) }; HSITES];
static HS_MAX: [AtomicU64; HSITES] = [const { AtomicU64::new(0) }; HSITES];
pub static HS_OVERFLOW: AtomicU64 = AtomicU64::new(0);

static HS_SPIN: [AtomicU64; HSITES] = [const { AtomicU64::new(0) }; HSITES];

#[inline]
fn hs_slot(loc: usize) -> Option<usize> {
    if loc == 0 { return None; }
    let mut h = (loc.wrapping_mul(0x9E37_79B9_7F4A_7C15usize) >> 7) % HSITES;
    for _ in 0..16 {
        let cur = HS_LOC[h].load(Ordering::Relaxed);
        if cur == loc || (cur == 0 && HS_LOC[h].compare_exchange(0, loc, Ordering::Relaxed, Ordering::Relaxed)
            .map_or_else(|o| o == loc, |_| true)) {
            return Some(h);
        }
        h = (h + 1) % HSITES;
    }
    HS_OVERFLOW.fetch_add(1, Ordering::Relaxed);
    None
}

#[inline]
fn hold_census(loc: usize, held: u64) {
    if let Some(h) = hs_slot(loc) {
        HS_N[h].fetch_add(1, Ordering::Relaxed);
        HS_NS[h].fetch_add(held, Ordering::Relaxed);
        HS_MAX[h].fetch_max(held, Ordering::Relaxed);
    }
}

/// A contended RUN_QUEUE acquire at `loc` spun `ns` first.
#[inline]
fn spin_census(loc: usize, ns: u64) {
    if let Some(h) = hs_slot(loc) { HS_SPIN[h].fetch_add(ns, Ordering::Relaxed); }
}

/// Every RUN_QUEUE acquire site seen: `(file, line, holds, ns held, max ns,
/// ns spun waiting for the lock)`.
pub fn for_each_hold_site(mut f: impl FnMut(&'static str, u32, u64, u64, u64, u64)) {
    for i in 0..HSITES {
        let key = HS_LOC[i].load(Ordering::Relaxed);
        if key == 0 { continue; }
        let loc = unsafe { &*(key as *const core::panic::Location<'static>) };
        f(loc.file(), loc.line(), HS_N[i].load(Ordering::Relaxed),
          HS_NS[i].load(Ordering::Relaxed), HS_MAX[i].load(Ordering::Relaxed),
          HS_SPIN[i].load(Ordering::Relaxed));
    }
}

// ── Per-lock-instance rows (`/proc/rqhold` `# lock` lines, lane percpu) ────
//
// The per-site census above answers "who holds RUN_QUEUE"; these rows answer
// "which lock": row 0 is TASKS (today's RUN_QUEUE), row 1 + c is CPU c's run
// queue lock once the per-CPU queues get their own locks. Holds, ns held,
// contended acquires and ns spun, cumulative since boot.
pub const ROW_TASKS: usize = 0;
pub const NROWS: usize = 1 + super::MAX_CPUS;
#[inline]
pub const fn row_rq(cpu: usize) -> usize { 1 + cpu }
static ROW_N:    [AtomicU64; NROWS] = [const { AtomicU64::new(0) }; NROWS];
static ROW_NS:   [AtomicU64; NROWS] = [const { AtomicU64::new(0) }; NROWS];
static ROW_CONT: [AtomicU64; NROWS] = [const { AtomicU64::new(0) }; NROWS];
static ROW_SPIN: [AtomicU64; NROWS] = [const { AtomicU64::new(0) }; NROWS];

/// One release of the lock of `row`, held `ns`.
#[inline]
pub fn row_note_hold(row: usize, ns: u64) {
    if row < NROWS {
        ROW_N[row].fetch_add(1, Ordering::Relaxed);
        ROW_NS[row].fetch_add(ns, Ordering::Relaxed);
    }
}

/// One contended acquire of the lock of `row` that spun `ns` first.
#[inline]
pub fn row_note_spin(row: usize, ns: u64) {
    if row < NROWS {
        ROW_CONT[row].fetch_add(1, Ordering::Relaxed);
        ROW_SPIN[row].fetch_add(ns, Ordering::Relaxed);
    }
}

/// Every row with at least one hold: `(name, index, holds, ns held,
/// contended acquires, ns spun)`; name is `"TASKS"` (index 0) or `"RQ"`
/// (index = cpu).
pub fn for_each_lock_row(mut f: impl FnMut(&'static str, usize, u64, u64, u64, u64)) {
    for r in 0..NROWS {
        let n = ROW_N[r].load(Ordering::Relaxed);
        if n == 0 && r != ROW_TASKS { continue; }
        let (name, idx) = if r == ROW_TASKS { ("TASKS", 0) } else { ("RQ", r - 1) };
        f(name, idx, n, ROW_NS[r].load(Ordering::Relaxed),
          ROW_CONT[r].load(Ordering::Relaxed), ROW_SPIN[r].load(Ordering::Relaxed));
    }
}

#[inline]
fn on_release(id: u8, site1: u32, since: u64) {
    if id == L_RUN_QUEUE && since != 0 {
        let held = now_ns().saturating_sub(since);
        hold_census(CUR_LOC[id as usize].load(Ordering::Relaxed), held);
        row_note_hold(ROW_TASKS, held);
    }
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
    s(" | ready_hint_miss="); n(READY_HINT_MISS.load(Ordering::Relaxed) as u64);
    s(" | picks="); n(PICK_N.load(Ordering::Relaxed) as u64);
    s(" full="); n(PICK_FULL.load(Ordering::Relaxed) as u64);
    s(" visited="); n(PICK_VISITED.load(Ordering::Relaxed));
    s(" pick_ns="); n(PICK_NS.load(Ordering::Relaxed));
    s(" | as acq="); n(AS_ACQ.load(Ordering::Relaxed) as u64);
    s(" cont="); n(AS_CONT.load(Ordering::Relaxed) as u64);
    s(" wait_us="); n(AS_WAIT_NS.load(Ordering::Relaxed) / 1000);
    s(" wait_max="); n(AS_WAIT_MAX.load(Ordering::Relaxed) as u64);
    s("ns hold_us="); n(AS_HOLD_NS.load(Ordering::Relaxed) / 1000);
    s(" hold_max="); n(AS_HOLD_MAX.load(Ordering::Relaxed) as u64);
    s("ns | age-at-fail:");
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
        let cw = SITE_CWAITS[i].load(Ordering::Relaxed);
        if cw > 0 {
            s(" | contended="); n(cw as u64);
            s(" waited_us="); n(SITE_CWAIT_NS[i].load(Ordering::Relaxed) / 1000);
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

/// Address-space `busy` flags this CPU holds. `HOLDER` keeps one CPU per
/// lock id, and a second CPU taking another process's flag overwrites it, so
/// it cannot answer "may this CPU sleep?" for `L_AS_BUSY`; this count can.
static AS_HELD: [core::sync::atomic::AtomicU32; super::MAX_CPUS] =
    [const { core::sync::atomic::AtomicU32::new(0) }; super::MAX_CPUS];

/// True while this CPU holds some address space's `busy` flag.
pub fn as_busy_held_here() -> bool { AS_HELD[me()].load(Ordering::Relaxed) != 0 }

/// Hand-rolled locks: this CPU took (`held`) or released `id`. Several CPUs
/// can hold distinct address spaces at once; the slot records the most recent
/// taker, which is still the right CPU to look at when everything is stuck.
#[inline]
pub fn note_hold(id: u8, held: bool) {
    if id == L_AS_BUSY {
        let c = &AS_HELD[me()];
        if held { c.fetch_add(1, Ordering::Relaxed); }
        else { let v = c.load(Ordering::Relaxed); if v != 0 { c.store(v - 1, Ordering::Relaxed); } }
    }
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

/// `ID` (optional) carries the lock id in the TYPE instead of the value, so a
/// static whose payload is all zero stays all zero and lands in .bss rather
/// than the kernel image (a 16 MB table in .data is 16 MB of image to load).
/// With `ID == 0` the runtime `id` field is used, as before.
/// Per CPU and lock id: the `spin::Mutex` this CPU last took under that id
/// and a type-erased force-unlock for it, so a context that can never drop
/// its guards (a task killed by the EL1 user-pointer fault path, which does
/// not unwind) can still release what it holds: see `force_release_held`.
static OWN_PTR: [[AtomicUsize; N_LOCKS]; super::MAX_CPUS] = [const { [const { AtomicUsize::new(0) }; N_LOCKS] }; super::MAX_CPUS];
static OWN_FN:  [[AtomicUsize; N_LOCKS]; super::MAX_CPUS] = [const { [const { AtomicUsize::new(0) }; N_LOCKS] }; super::MAX_CPUS];

unsafe fn force_unlock_erased<T>(p: usize) {
    (*(p as *const spin::Mutex<T>)).force_unlock();
}

#[inline(always)]
fn note_owner<T>(cpu: usize, id: u8, m: &spin::Mutex<T>) {
    if cpu < super::MAX_CPUS && (id as usize) < N_LOCKS {
        OWN_PTR[cpu][id as usize].store(m as *const _ as usize, Ordering::Relaxed);
        OWN_FN[cpu][id as usize].store(force_unlock_erased::<T> as unsafe fn(usize) as usize, Ordering::Relaxed);
    }
}

/// Force-release every tracked lock this CPU holds, for a kernel context that
/// will never run its guards' drops: the EL1 fault path kills the current
/// task (`exit_group_signal`) from inside a syscall without unwinding, and
/// the exit path takes these same locks again -- a self-deadlock (Pi 5,
/// a9a61b2b: a fault under EPOLL wedged all four CPUs). The protected data
/// may be mid-update; that is still better than a machine-wide wedge.
/// Returns the bitmask of lock ids released. Never call it from a context
/// that still owns a live guard it will drop.
pub fn force_release_held() -> u32 {
    let cpu = me();
    if cpu >= super::MAX_CPUS { return 0; }
    let mut released = 0u32;
    for id in 1..N_LOCKS {
        if HOLDER[id].load(Ordering::Relaxed) as usize != cpu + 1 { continue; }
        let p = OWN_PTR[cpu][id].load(Ordering::Relaxed);
        let f = OWN_FN[cpu][id].load(Ordering::Relaxed);
        if p == 0 || f == 0 { continue; }
        HOLDER[id].store(0, Ordering::Relaxed);
        // SAFETY: `f` is `force_unlock_erased::<T>` for the `spin::Mutex<T>`
        // at `p` (a static), recorded by this CPU when it took the lock.
        unsafe {
            let f: unsafe fn(usize) = core::mem::transmute(f);
            f(p);
        }
        released |= 1 << id;
    }
    released
}

pub struct TrackedMutex<T, const ID: u8 = 0> {
    id: u8,
    inner: spin::Mutex<T>,
}

pub struct TrackedGuard<'a, T> {
    id: u8,
    site1: u32,
    since: u64,
    lock: &'a spin::Mutex<T>,
    guard: spin::MutexGuard<'a, T>,
}

impl<T, const ID: u8> TrackedMutex<T, ID> {
    pub const fn new(id: u8, value: T) -> Self {
        Self { id, inner: spin::Mutex::new(value) }
    }

    /// Construct with the id taken from the `ID` type parameter only.
    pub const fn new_typed(value: T) -> Self {
        Self { id: 0, inner: spin::Mutex::new(value) }
    }

    #[inline(always)]
    fn lock_id(&self) -> u8 { if ID != 0 { ID } else { self.id } }

    #[inline]
    #[track_caller]
    pub fn lock(&self) -> TrackedGuard<'_, T> {
        let cpu = me();
        let loc = core::panic::Location::caller();
        assert_no_rq(self.lock_id());
        let id = self.lock_id() as usize;
        ACQ[cpu][id].fetch_add(1, Ordering::Relaxed);
        if let Some(guard) = self.inner.try_lock() {
            HOLDER[self.lock_id() as usize].store(cpu as u8 + 1, Ordering::Relaxed);
            note_owner(cpu, self.lock_id(), &self.inner);
            let (site1, since) = on_acquire(self.lock_id(), loc);
            return TrackedGuard { id: self.lock_id(), site1, since, lock: &self.inner, guard };
        }
        WANT[cpu].store(self.lock_id(), Ordering::Relaxed);
        let w0 = now_ns();
        // Spin with try_lock rather than `inner.lock()` so a CPU waiting here
        // with IRQs masked still answers TLB-shootdown requests: the holder
        // (or a third CPU) may be spinning for this CPU's acknowledgement.
        let mut sp = crate::wedge::Spin::new();
        let guard = loop {
            if !self.inner.is_locked() {
                if let Some(g) = self.inner.try_lock() { break g; }
            }
            mm::paging::tlb_service_pending();
            sp.tick(name(self.lock_id()));
            core::hint::spin_loop();
        };
        WANT[cpu].store(0, Ordering::Relaxed);
        CONT[cpu][id].fetch_add(1, Ordering::Relaxed);
        CONT_NS[cpu][id].fetch_add(now_ns().saturating_sub(w0), Ordering::Relaxed);
        HOLDER[self.lock_id() as usize].store(cpu as u8 + 1, Ordering::Relaxed);
        note_owner(cpu, self.lock_id(), &self.inner);
        let (site1, since) = on_acquire(self.lock_id(), loc);
        if id == L_RUN_QUEUE as usize {
            spin_census(loc as *const _ as usize, since.saturating_sub(w0));
            row_note_spin(ROW_TASKS, since.saturating_sub(w0));
        }
        if HOLD_PROFILE && site1 != 0 {
            SITE_CWAITS[site1 as usize - 1].fetch_add(1, Ordering::Relaxed);
            SITE_CWAIT_NS[site1 as usize - 1].fetch_add(since.saturating_sub(w0), Ordering::Relaxed);
        }
        TrackedGuard { id: self.lock_id(), site1, since, lock: &self.inner, guard }
    }

    #[inline]
    #[track_caller]
    pub fn try_lock(&self) -> Option<TrackedGuard<'_, T>> {
        assert_no_rq(self.lock_id());
        let guard = self.inner.try_lock()?;
        let cpu = me();
        HOLDER[self.lock_id() as usize].store(cpu as u8 + 1, Ordering::Relaxed);
        note_owner(cpu, self.lock_id(), &self.inner);
        let (site1, since) = on_acquire(self.lock_id(), core::panic::Location::caller());
        Some(TrackedGuard { id: self.lock_id(), site1, since, lock: &self.inner, guard })
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
                SPIN_HITS[self.lock_id() as usize].fetch_add(1, Ordering::Relaxed);
                SPIN_MAX_NS[self.lock_id() as usize].fetch_max(waited.min(u32::MAX as u64) as u32, Ordering::Relaxed);
                return Some(g);
            }
            if now_ns().saturating_sub(t0) >= max_ns {
                SPIN_GAVE_UP[self.lock_id() as usize].fetch_add(1, Ordering::Relaxed);
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
        // Lane percpu: every wake made in this TASKS hold is placed on a CPU
        // queue before the hold ends (see `RunQueue::flush_woken`), whatever
        // path made it — a wake is never left for a pick to find.
        if self.id == L_RUN_QUEUE && super::slot::woken_any() {
            // SAFETY: the only TrackedMutex with id L_RUN_QUEUE is
            // `RUN_QUEUE: TrackedMutex<RunQueue>`.
            let rq = (&mut *self.guard) as *mut T as *mut super::runqueue::RunQueue;
            unsafe { (*rq).flush_woken(); }
        }
        // Clear the holder BEFORE the inner guard releases the lock (fields
        // drop after this body), so a new holder's store can never be wiped
        // by a late clear from the previous one.
        on_release(self.id, self.site1, self.since);
        HOLDER[self.id as usize].store(0, Ordering::Relaxed);
    }
}

/// Spin for `m` with IRQs masked, answering TLB shootdowns (see
/// `TrackedMutex::lock`), and charge lock `id`'s contention counters.
fn spin_acquire<G>(id: u8, mut try_once: impl FnMut() -> Option<G>, busy: impl Fn() -> bool) -> G {
    let cpu = me();
    let i = id as usize;
    ACQ[cpu][i].fetch_add(1, Ordering::Relaxed);
    if let Some(g) = try_once() { return g; }
    WANT[cpu].store(id, Ordering::Relaxed);
    let w0 = now_ns();
    let g = loop {
        if !busy() { if let Some(g) = try_once() { break g; } }
        mm::paging::tlb_service_pending();
        core::hint::spin_loop();
    };
    WANT[cpu].store(0, Ordering::Relaxed);
    CONT[cpu][i].fetch_add(1, Ordering::Relaxed);
    CONT_NS[cpu][i].fetch_add(now_ns().saturating_sub(w0), Ordering::Relaxed);
    g
}

impl<'a, T> TrackedGuard<'a, T> {
    /// Run `f` with the lock released, then take it again (lane ipcfast: the
    /// AF_UNIX user copies run here instead of under UNIX_CONNS). The guard
    /// stays valid across the call, but everything read through it before
    /// may have changed: callers re-validate after it returns. Never for
    /// RUN_QUEUE (its drop flushes wakes made in the hold).
    #[track_caller]
    pub fn unlocked<R>(&mut self, f: impl FnOnce() -> R) -> R {
        assert!(self.id != L_RUN_QUEUE);
        on_release(self.id, self.site1, self.since);
        HOLDER[self.id as usize].store(0, Ordering::Relaxed);
        // SAFETY: this guard owns the lock; it is re-taken (and the new
        // guard forgotten) before the old guard is used or dropped again.
        unsafe { self.lock.force_unlock(); }
        let r = f();
        let lock = self.lock;
        core::mem::forget(spin_acquire(self.id, || lock.try_lock(), || lock.is_locked()));
        HOLDER[self.id as usize].store(me() as u8 + 1, Ordering::Relaxed);
        note_owner(me(), self.id, lock);
        let (site1, since) = on_acquire(self.id, core::panic::Location::caller());
        self.site1 = site1;
        self.since = since;
        r
    }
}

/// A reader-writer spinlock with the [`TrackedMutex`] contention counters
/// and TLB-shootdown-safe spinning. `lock()` is the exclusive side, so a
/// `TrackedMutex` converts by changing only its type; hot read-only paths
/// call `read()` and no longer serialize each other (lane ipcfast:
/// SOCK_TABLES is looked up ~5 times per AF_UNIX sendmsg/recvmsg).
pub struct TrackedRwLock<T> {
    id: u8,
    inner: spin::RwLock<T>,
}

impl<T> TrackedRwLock<T> {
    pub const fn new(id: u8, value: T) -> Self { Self { id, inner: spin::RwLock::new(value) } }

    #[inline]
    pub fn lock(&self) -> spin::RwLockWriteGuard<'_, T> {
        spin_acquire(self.id, || self.inner.try_write(), || self.inner.writer_count() != 0 || self.inner.reader_count() != 0)
    }

    #[inline]
    pub fn read(&self) -> spin::RwLockReadGuard<'_, T> {
        spin_acquire(self.id, || self.inner.try_read(), || self.inner.writer_count() != 0)
    }

    #[inline]
    pub fn try_lock(&self) -> Option<spin::RwLockWriteGuard<'_, T>> {
        let g = self.inner.try_write()?;
        ACQ[me()][self.id as usize].fetch_add(1, Ordering::Relaxed);
        Some(g)
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
