//! Idle-cost census: how busy each CPU is, and which threads wake how often.
//!
//! WHY IT EXISTS. "The desktop is idle" should mean the guest is idle: every
//! CPU in `hlt`/`wfi` and no thread woken except for real work. Measuring it
//! needs two numbers the kernel did not keep: time spent parked in the idle
//! instruction per CPU, and dispatches (context switches in) per thread. On a
//! quiet system a dispatch is a wakeup, so the second is the per-thread
//! wakeup rate; the syscall the thread was parked in names what woke it.
//!
//! Output: every `PERIOD_NS` the BSP tick prints one `[IDLESTAT]` line (per-CPU
//! busy permille over the window) and up to `TOP` `[WAKE]` lines, busiest
//! first: `pid tgid disp=<dispatches/window> sc=<syscall it resumed in, most
//! recent> cpu_us=<CPU time over the window>`. Compile-time gated (`ENABLED`),
//! like `pcsample`: off, every hook is a constant-false branch.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

pub const ENABLED: bool = false;

const SLOTS: usize = 1024;
const TOP: usize = 16;
const PERIOD_NS: u64 = 10_000_000_000;

/// Dispatches in the current window, by `pid & 1023`.
static DISP: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
/// The pid the slot currently counts (a different pid resets it).
static PID: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
static TGID: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
/// Syscall the thread was parked in at its last dispatch (u32::MAX = none).
static RESUME_SC: [AtomicU32; SLOTS] = [const { AtomicU32::new(u32::MAX) }; SLOTS];
/// `Task::cpu_ns` at the start of the window.
static CPU0: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// Nanoseconds spent in the idle instruction, per CPU, cumulative.
static IDLE_NS: [AtomicU64; crate::MAX_CPUS] = [const { AtomicU64::new(0) }; crate::MAX_CPUS];
static IDLE0: [AtomicU64; crate::MAX_CPUS] = [const { AtomicU64::new(0) }; crate::MAX_CPUS];
static WIN_START: AtomicU64 = AtomicU64::new(0);

/// Poll-channel wakes by call site: (file ptr, line, calls, tasks woken).
/// Broadcast (`POLL_TAG_ALL`) wakes and timerfd deadline wakes are what the
/// idle census needs to attribute; a small open-addressed table is enough.
const SITES: usize = 48;
static SITE_FILE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_LINE: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];
static SITE_LEN: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];
static SITE_CALLS: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];
static SITE_WOKEN: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];
static SITE_TAGGED: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];

/// Record a poll-channel wake from `loc` that made `woken` tasks Ready.
#[inline]
pub fn note_wake(loc: &'static core::panic::Location<'static>, broadcast: bool, woken: usize) {
    if !ENABLED { return; }
    let f = loc.file().as_ptr() as u64;
    let l = loc.line();
    let h = ((f >> 3) as usize ^ (l as usize).wrapping_mul(31)) % SITES;
    for k in 0..SITES {
        let i = (h + k) % SITES;
        let cur = SITE_FILE[i].load(Relaxed);
        if cur == 0 {
            if SITE_FILE[i].compare_exchange(0, f, Relaxed, Relaxed).is_ok() || SITE_FILE[i].load(Relaxed) == f {
                if SITE_LINE[i].load(Relaxed) == 0 {
                    SITE_LEN[i].store(loc.file().len() as u32, Relaxed);
                    SITE_LINE[i].store(l, Relaxed);
                }
            }
        }
        if SITE_FILE[i].load(Relaxed) == f && SITE_LINE[i].load(Relaxed) == l {
            SITE_CALLS[i].fetch_add(1, Relaxed);
            SITE_WOKEN[i].fetch_add(woken as u32, Relaxed);
            if !broadcast { SITE_TAGGED[i].fetch_add(1, Relaxed); }
            return;
        }
    }
}

/// Deadline-service wakes by reason: own deadline due; timerfd expiry
/// matched a broadcast (`POLL_TAG_ALL`) mask; matched a narrow mask.
static DL_OWN: AtomicU32 = AtomicU32::new(0);
static DL_TFD_ALL: AtomicU32 = AtomicU32::new(0);
static DL_TFD_NARROW: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn note_deadline_wake(own_deadline: bool, broadcast_mask: bool) {
    if !ENABLED { return; }
    if own_deadline { DL_OWN.fetch_add(1, Relaxed); }
    else if broadcast_mask { DL_TFD_ALL.fetch_add(1, Relaxed); }
    else { DL_TFD_NARROW.fetch_add(1, Relaxed); }
}

/// Dispatcher hook: `pid` (thread group `tgid`) is about to run.
#[inline]
pub fn on_dispatch(pid: u32, tgid: u32, resume_sc: u32) {
    if !ENABLED { return; }
    let i = pid as usize & (SLOTS - 1);
    if PID[i].load(Relaxed) != pid {
        PID[i].store(pid, Relaxed);
        DISP[i].store(0, Relaxed);
        CPU0[i].store(0, Relaxed);
    }
    TGID[i].store(tgid, Relaxed);
    RESUME_SC[i].store(resume_sc, Relaxed);
    DISP[i].fetch_add(1, Relaxed);
}

/// Idle-loop hook: this CPU was parked for `ns`.
#[inline]
pub fn on_idle(cpu: usize, ns: u64) {
    if !ENABLED { return; }
    IDLE_NS[cpu.min(crate::MAX_CPUS - 1)].fetch_add(ns, Relaxed);
}

/// BSP tick hook (IRQ context: try-lock only, raw UART output).
pub fn tick(now: u64) {
    if !ENABLED { return; }
    let start = WIN_START.load(Relaxed);
    if start == 0 { WIN_START.store(now, Relaxed); return; }
    let win = now.saturating_sub(start);
    if win < PERIOD_NS { return; }
    let rq = match crate::RUN_QUEUE.try_lock() { Some(r) => r, None => return };
    WIN_START.store(now, Relaxed);

    g::s("[IDLESTAT] win_ms="); g::d((win / 1_000_000) as usize);
    let ncpu = crate::active_cpu_count().min(crate::MAX_CPUS);
    let mut busy_sum = 0u64;
    for c in 0..ncpu {
        let tot = IDLE_NS[c].load(Relaxed);
        let idle = tot.saturating_sub(IDLE0[c].swap(tot, Relaxed)).min(win);
        let busy_pm = (win - idle) * 1000 / win.max(1);
        busy_sum += busy_pm;
        g::s(" cpu"); g::d(c); g::s("="); g::d(busy_pm as usize);
    }
    g::s(" total_pm="); g::d((busy_sum / ncpu.max(1) as u64) as usize);

    // CPU time per thread over the window, from the tasks still alive.
    let mut cpu_us = [0u32; SLOTS];
    let mut total_disp = 0u64;
    for i in 0..crate::runqueue::MAX_TASKS {
        let t = match rq.get(i) { Some(t) => t, None => continue };
        let s = t.pid as usize & (SLOTS - 1);
        if PID[s].load(Relaxed) != t.pid {
            PID[s].store(t.pid, Relaxed);
            TGID[s].store(t.tgid, Relaxed);
            DISP[s].store(0, Relaxed);
            CPU0[s].store(t.cpu_ns, Relaxed);
            continue;
        }
        let c0 = CPU0[s].swap(t.cpu_ns, Relaxed);
        cpu_us[s] = (t.cpu_ns.saturating_sub(c0) / 1000).min(u32::MAX as u64) as u32;
    }
    drop(rq);
    let mut disp = [0u32; SLOTS];
    for s in 0..SLOTS {
        disp[s] = DISP[s].swap(0, Relaxed);
        total_disp += disp[s] as u64;
    }
    g::s(" disp_per_s="); g::d((total_disp * 1_000_000_000 / win.max(1)) as usize);
    g::s(" dl_own="); g::d(DL_OWN.swap(0, Relaxed) as usize);
    g::s(" dl_tfd_all="); g::d(DL_TFD_ALL.swap(0, Relaxed) as usize);
    g::s(" dl_tfd_narrow="); g::d(DL_TFD_NARROW.swap(0, Relaxed) as usize);
    g::nl();
    for i in 0..SITES {
        let calls = SITE_CALLS[i].swap(0, Relaxed);
        let woken = SITE_WOKEN[i].swap(0, Relaxed);
        let tagged = SITE_TAGGED[i].swap(0, Relaxed);
        if calls == 0 { continue; }
        let f = SITE_FILE[i].load(Relaxed) as *const u8;
        g::s("[WAKESITE] ");
        let n = SITE_LEN[i].load(Relaxed) as usize;
        g::bytes(unsafe { core::slice::from_raw_parts(f, n) });
        g::s(":"); g::d(SITE_LINE[i].load(Relaxed) as usize);
        g::s(" calls="); g::d(calls as usize);
        g::s(" tagged="); g::d(tagged as usize);
        g::s(" woken="); g::d(woken as usize);
        g::nl();
    }
    let mut taken = [false; SLOTS];
    for _ in 0..TOP {
        let mut best = usize::MAX; let mut bv = 0u64;
        for s in 0..SLOTS {
            if taken[s] { continue; }
            // Rank by dispatches, CPU time breaking ties (a spinner that is
            // never descheduled has few dispatches and a lot of CPU).
            let v = (disp[s] as u64) * 1000 + (cpu_us[s] as u64 / 1000);
            if v > bv { bv = v; best = s; }
        }
        if best == usize::MAX { break; }
        taken[best] = true;
        g::s("[WAKE] pid="); g::d(PID[best].load(Relaxed) as usize);
        g::s(" tgid="); g::d(TGID[best].load(Relaxed) as usize);
        g::s(" disp="); g::d(disp[best] as usize);
        let sc = RESUME_SC[best].load(Relaxed);
        g::s(" sc="); if sc == u32::MAX { g::s("-"); } else { g::d(sc as usize); }
        g::s(" cpu_us="); g::d(cpu_us[best] as usize);
        g::nl();
    }
}

/// Raw-UART printers (IRQ context: no locks), decimal for readability.
mod g {
    extern "C" { fn arch_serial_putc(c: u8); }
    pub fn s(m: &str) { for &b in m.as_bytes() { unsafe { arch_serial_putc(b) } } }
    pub fn d(mut n: usize) {
        let mut buf = [0u8; 20];
        let mut i = buf.len();
        loop { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
        for &b in &buf[i..] { unsafe { arch_serial_putc(b) } }
    }
    pub fn nl() { s("\n"); }
    pub fn bytes(b: &[u8]) { for &c in b { unsafe { arch_serial_putc(c) } } }
}
