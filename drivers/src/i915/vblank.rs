//! Real vblank for pipe A and asynchronous flips (lane kmscursor).
//!
//! Before this, `kms::present` ran inside the atomic-commit ioctl: wait the
//! render fence (≤ 1 s), clflush, write PLANE_SURF, then poll PLANE_SURFLIVE
//! with msleep(1) until the latch (≤ a frame); the DRM event then came from a
//! 60 Hz hrtimer unrelated to the panel's vblank. The compositor thread sat
//! in the ioctl for most of a frame and its frame rate quantised to 30/20/15.
//!
//! Now (default; `i915.vblank=0` restores the old path):
//!
//! * `queue` (the ioctl) validates the flip and, when the render fence has
//!   already signalled and no clflush is needed, ARMS it at once (the plane
//!   registers, PLANE_SURF last — a few MMIO writes) and returns. Otherwise
//!   the `i915-flip` kthread waits the fence, clflushes and arms it.
//! * Completion: the display interrupt (MSI; GEN8_MASTER_IRQ → DE pipe A IIR
//!   VBLANK / PLANE1_FLIP_DONE, as Linux's gen8_de_irq_handler) checks
//!   PIPE_FRMCOUNT moved past the frame the flip was armed in and
//!   PLANE_SURFLIVE shows the new surface, then completes the commit's DRM
//!   event with the hardware frame counter and the vblank-start time
//!   (interrupt time minus the scan lines since vblank start, from PIPEDSL).
//!   The vblank interrupt is unmasked (IMR) only while something is pending.
//! * A backstop hrtimer runs the same check one frame + 3 ms after arming
//!   and every 2 ms after that, so a silent interrupt (MSI not delivered)
//!   degrades to polling, never to a stall. On the fake device (RAM
//!   registers) it is the vblank itself: a 60 Hz software frame counter.
//! * One commit in flight: `busy` makes the DRM layer answer EBUSY to a
//!   nonblocking commit (Linux's semantics) and wait for a blocking one.
//!
//! Console hand-off: `console_gen_bump` (from `kms::console_restore`, lock-
//! free) makes a flip still waiting for its fence be dropped instead of
//! armed, and completes an armed one at the next check (its event still goes
//! out; the client must not stall).
//!
//! IRQ ownership: this module requests the device's MSI and owns
//! GEN8_MASTER_IRQ. GT/PCU banks are only acknowledged here (their sources
//! stay masked: execution completion is polled by `exec`); a later GT/PM
//! interrupt user hooks into `isr` (`GT bank` comment) rather than requesting
//! the MSI a second time.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use sched::ksync::{KSemaphore, KSpinLock};

use crate::gpu_kms::KmsFlip;
use crate::DriverError;

use super::hw::reg;
use super::kms::{self, FlipRegs};
use super::{ilog, log, opt, Device};

const MASTER_IRQ: u32 = reg::GEN8_MASTER_IRQ;
const MASTER_IRQ_CONTROL: u32 = 1 << 31;
/// GEN8_MASTER_IRQ bank bits (gen8/9).
const MASTER_GT: [(u32, u32); 4] = [(0x3, 0x44308), (0xC, 0x44318), (0x30, 0x44328), (0x40, 0x44338)];
const MASTER_DE_PIPE_A: u32 = 1 << 16;
const MASTER_DE_PIPE_B: u32 = 1 << 17;
const MASTER_DE_PIPE_C: u32 = 1 << 18;
const MASTER_DE_PORT: u32 = 1 << 20;
const MASTER_DE_MISC: u32 = 1 << 22;
const MASTER_DE_PCH: u32 = 1 << 23;
const MASTER_PCU: u32 = 1 << 30;
/// GEN8_DE_PIPE_{IMR,IIR,IER}(pipe) = 0x44404/8/C + 0x10 * pipe.
const DE_PIPE_IMR: u32 = 0x44404;
const DE_PIPE_IIR: u32 = 0x44408;
const DE_PIPE_IER: u32 = 0x4440C;
const DE_PORT_IIR: u32 = 0x44448;
const DE_MISC_IIR: u32 = 0x44468;
const PCU_IIR: u32 = 0x444E8;
const SDEIIR: u32 = 0xC4008;
const SDEIMR: u32 = 0xC4004;
const SDEIER: u32 = 0xC400C;
const PCH_PORT_HOTPLUG: u32 = 0xC4030;
/// SDE hotplug bits for DDI B/C/D (SPT): 21, 22, 23.
const SDE_HPD_BITS: u32 = 7 << 21;
/// GEN8_PIPE_VBLANK, GEN9_PIPE_PLANE1_FLIP_DONE, GEN8_PIPE_FIFO_UNDERRUN.
const PIPE_VBLANK: u32 = 1 << 0;
const PIPE_PLANE1_FLIP_DONE: u32 = 1 << 3;
const PIPE_FIFO_UNDERRUN: u32 = 1 << 31;
const OURS: u32 = PIPE_VBLANK | PIPE_PLANE1_FLIP_DONE;

const FRAME_NS: u64 = 16_666_667;
const EBUSY: i32 = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Waiting for its render fence (the worker's). `taken`: the worker is on it.
    Queued { job: u64, taken: bool },
    /// Registers written in hardware frame `frame`; waiting for the latch.
    Armed { frame: u32 },
}

#[derive(Clone, Copy)]
struct Pending {
    /// None: an event-only commit (cursor move, nothing changed).
    regs: Option<FlipRegs>,
    /// DRM event cookies to complete (0 = none).
    ev: [u32; 2],
    stage: Stage,
    /// `GEN` when queued: a console restore since means "drop it".
    gen: u32,
    /// The token that was on screen before an armed flip (still scanned
    /// until the latch).
    prev: u32,
    t_queue: u64,
    t_arm: u64,
}

static STATE: KSpinLock<Option<Pending>> = KSpinLock::new(None);
static GEN: AtomicU32 = AtomicU32::new(0);
static JOB: AtomicU64 = AtomicU64::new(0);
static WORK: KSemaphore = KSemaphore::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);
/// The MSI is installed and GEN8_MASTER_IRQ enabled.
static IRQ_ON: AtomicBool = AtomicBool::new(false);
static FAKE_FRAME: AtomicU32 = AtomicU32::new(0);
static TIMER: AtomicPtr<sched::hrtimer::HrTimer> = AtomicPtr::new(core::ptr::null_mut());

// Counters (the periodic `kms: vblank` line).
static IRQS: AtomicU64 = AtomicU64::new(0);
static IRQ_VBL: AtomicU64 = AtomicU64::new(0);
static IRQ_IDLE_STREAK: AtomicU32 = AtomicU32::new(0);
static DONE_IRQ: AtomicU64 = AtomicU64::new(0);
static DONE_POLL: AtomicU64 = AtomicU64::new(0);
static ARMED_DIRECT: AtomicU64 = AtomicU64::new(0);
static ARMED_WORKER: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static FENCE_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static UNLATCHED: AtomicU64 = AtomicU64::new(0);
static EVENT_ONLY: AtomicU64 = AtomicU64::new(0);
static Q2D_NS: AtomicU64 = AtomicU64::new(0);
static Q2D_MAX: AtomicU64 = AtomicU64::new(0);
static A2D_NS: AtomicU64 = AtomicU64::new(0);
static A2D_MAX: AtomicU64 = AtomicU64::new(0);
static NDONE: AtomicU64 = AtomicU64::new(0);

fn fake() -> bool { kms::FAKE.load(Ordering::Relaxed) }
fn now() -> u64 { super::now_ns() }

fn frame_now() -> u32 {
    if fake() { return FAKE_FRAME.load(Ordering::Acquire); }
    kms::mmio().r(reg::pipe(reg::PIPE_FRMCOUNT_A, 0))
}

/// Monotonic time of the latest vblank start: `now` minus the scan lines
/// since then (PIPEDSL), when the timings are known.
fn vblank_ts(now_ns: u64) -> u64 {
    if fake() { return now_ns; }
    let (vbs, vt, ln) = (kms::VBL_START.load(Ordering::Relaxed), kms::VTOTAL.load(Ordering::Relaxed), kms::LINE_NS.load(Ordering::Relaxed));
    if vbs == 0 || vt == 0 || ln == 0 { return now_ns; }
    let dsl = kms::mmio().r(reg::PIPEDSL_A) & 0x1FFF;
    let since = (dsl + vt - vbs.min(vt)) % vt;
    now_ns.saturating_sub(since as u64 * ln as u64)
}

/// Unmask / mask pipe A's vblank + flip-done in the IMR (IER is set once at
/// init). Under `STATE`.
fn vblank_irq(on: bool) {
    if !IRQ_ON.load(Ordering::Relaxed) || fake() { return; }
    let m = kms::mmio();
    let imr = m.r(DE_PIPE_IMR);
    let want = if on { imr & !OURS } else { imr | OURS };
    if want != imr { m.w(DE_PIPE_IMR, want); }
}

fn timer() -> Option<&'static sched::hrtimer::HrTimer> {
    let p = TIMER.load(Ordering::Acquire);
    unsafe { p.as_ref() }
}

/// (Re)arm the backstop `after_ns` from now (never later than it already is).
fn backstop(after_ns: u64) {
    if let Some(t) = timer() {
        let due = now() + after_ns;
        if t.is_armed() && t.expires() <= due { return; }
        let _ = t.start_at(due, 0);
    }
}

extern "C" fn backstop_fire(_ctx: usize) {
    if fake() { FAKE_FRAME.fetch_add(1, Ordering::AcqRel); }
    if !check(false) { return; }
    if fake() { backstop(FRAME_NS); return; }
    // Still pending: poll every 2 ms, but not before one frame + 3 ms after
    // the arm (an early backstop left from the previous flip must not turn
    // into polling while the interrupt is about to complete this one).
    let t = now();
    let due = ARM_NS.load(Ordering::Relaxed).saturating_add(FRAME_NS + 3_000_000).max(t + 2_000_000);
    if let Some(tm) = timer() { let _ = tm.start_at(due, 0); }
}

/// When the pending commit was armed (the backstop's reference).
static ARM_NS: AtomicU64 = AtomicU64::new(0);

/// Complete what has latched. Any context (hard IRQ included). Returns true
/// while something is still pending.
fn check(from_irq: bool) -> bool {
    let (p, seq, ts) = {
        let mut st = STATE.lock();
        let Some(p) = *st else { vblank_irq(false); return false };
        let Stage::Armed { frame } = p.stage else { return true };
        let f = frame_now();
        let gen_moved = p.gen != GEN.load(Ordering::Acquire);
        let latched = match p.regs {
            Some(r) if !fake() => kms::mmio().r(kms::preg(kms::PLANE_SURFLIVE_1_A)) & !0xFFF == r.surf & !0xFFF,
            _ => true,
        };
        let moved = f != frame;
        // Two frames on without the surface showing: something else wrote
        // the plane (a console restore that raced the arm); do not stall.
        let stuck = f.wrapping_sub(frame) >= 3;
        if !(gen_moved || (moved && latched) || stuck) { return true; }
        if stuck && !latched && !gen_moved { UNLATCHED.fetch_add(1, Ordering::Relaxed); }
        *st = None;
        vblank_irq(false);
        let t = now();
        let a2d = t.saturating_sub(p.t_arm);
        let q2d = t.saturating_sub(p.t_queue);
        A2D_NS.fetch_add(a2d, Ordering::Relaxed);
        A2D_MAX.fetch_max(a2d, Ordering::Relaxed);
        Q2D_NS.fetch_add(q2d, Ordering::Relaxed);
        Q2D_MAX.fetch_max(q2d, Ordering::Relaxed);
        NDONE.fetch_add(1, Ordering::Relaxed);
        if from_irq { DONE_IRQ.fetch_add(1, Ordering::Relaxed); } else { DONE_POLL.fetch_add(1, Ordering::Relaxed); }
        (p, f, vblank_ts(t))
    };
    for &e in &p.ev { if e != 0 { crate::drm_device_interface::ext_event_complete(e, seq, ts); } }
    false
}

/// Write the flip now and record it armed. Under `STATE`.
fn arm_locked(st: &mut Option<Pending>, mut p: Pending) -> kms::Armed {
    let r = p.regs.expect("arm of an event-only commit");
    let a = kms::arm(&r, false);
    p.prev = a.prev;
    p.stage = Stage::Armed { frame: frame_now() };
    p.t_arm = now();
    ARM_NS.store(p.t_arm, Ordering::Relaxed);
    *st = Some(p);
    vblank_irq(true);
    a
}

/// Complete the events of a commit that will never latch (dropped).
fn complete_now(p: &Pending) {
    let t = now();
    let f = frame_now();
    for &e in &p.ev { if e != 0 { crate::drm_device_interface::ext_event_complete(e, f, t); } }
}

// ── KmsFlip hooks ───────────────────────────────────────────────────────────

fn queue(token: u32, src_x: u32, src_y: u32, ev: u32) -> Result<(), DriverError> {
    let r = kms::prepare(token, src_x, src_y)?;
    let t0 = now();
    let ready = kms::fence_ready(&r) && !kms::needs_flush(&r);
    let mut p = Pending {
        regs: Some(r), ev: [ev, 0], stage: Stage::Queued { job: 0, taken: false },
        gen: GEN.load(Ordering::Acquire), prev: 0, t_queue: t0, t_arm: t0,
    };
    let armed = {
        let mut st = STATE.lock();
        if st.is_some() { return Err(DriverError::Errno(EBUSY)); }
        if ready {
            Some(arm_locked(&mut st, p))
        } else {
            p.stage = Stage::Queued { job: JOB.fetch_add(1, Ordering::Relaxed) + 1, taken: false };
            *st = Some(p);
            None
        }
    };
    match armed {
        Some(a) => {
            ARMED_DIRECT.fetch_add(1, Ordering::Relaxed);
            backstop(FRAME_NS + 3_000_000);
            kms::after_arm(&r, &a, (t0, t0, t0));
        }
        None => WORK.up(),
    }
    Ok(())
}

fn event(ev: u32) {
    EVENT_ONLY.fetch_add(1, Ordering::Relaxed);
    {
        let mut st = STATE.lock();
        match st.as_mut() {
            // A commit while one is pending (the DRM layer refuses that with
            // EBUSY, so only a race): ride along with it.
            Some(p) => {
                if p.ev[1] == 0 { p.ev[1] = ev; return; }
            }
            None => {
                let t = now();
                ARM_NS.store(t, Ordering::Relaxed);
                *st = Some(Pending {
                    regs: None, ev: [ev, 0], stage: Stage::Armed { frame: frame_now() },
                    gen: GEN.load(Ordering::Acquire), prev: 0, t_queue: t, t_arm: t,
                });
                vblank_irq(true);
                drop(st);
                backstop(FRAME_NS + 3_000_000);
                return;
            }
        }
    }
    // No room: complete it now rather than lose it.
    let t = now();
    crate::drm_device_interface::ext_event_complete(ev, frame_now(), t);
}

fn busy() -> bool { STATE.lock().is_some() }

fn wait_idle(timeout_ns: u64) -> bool {
    let t0 = now();
    loop {
        if !busy() { return true; }
        if now().wrapping_sub(t0) >= timeout_ns { return false; }
        // Polling keeps a missed completion from stalling a blocking commit.
        let _ = check(false);
        sched::kthread::sleep_ns(500_000);
    }
}

/// `kms::present` (synchronous) before it writes the plane: no-op unless the
/// real-vblank path is on.
pub(super) fn wait_idle_quiet(timeout_ns: u64) {
    if ENABLED.load(Ordering::Relaxed) { let _ = wait_idle(timeout_ns); }
}

static FLIP: KmsFlip = KmsFlip { queue, event, busy, wait_idle };

// ── console / framebuffer lifetime ──────────────────────────────────────────

/// The console took the scanout back (`kms::console_restore`). Lock-free.
pub(super) fn console_gen_bump() {
    GEN.fetch_add(1, Ordering::AcqRel);
}

/// Framebuffer `token` is going away: drop a flip of it still waiting for
/// its fence; wait out an armed flip that shows it or moves away from it.
pub(super) fn quiesce(token: u32) {
    if !ENABLED.load(Ordering::Relaxed) { return; }
    let (dropped, wait) = {
        let mut st = STATE.lock();
        match *st {
            Some(p) if p.regs.map_or(false, |r| r.token == token) && matches!(p.stage, Stage::Queued { .. }) => {
                *st = None;
                (Some(p), false)
            }
            Some(p) if matches!(p.stage, Stage::Armed { .. }) && (p.prev == token || p.regs.map_or(false, |r| r.token == token)) => (None, true),
            _ => (None, false),
        }
    };
    if let Some(p) = dropped {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        complete_now(&p);
    }
    if wait { let _ = wait_idle(100_000_000); }
}

// ── the worker: fence wait, clflush, arm ────────────────────────────────────

fn worker() {
    let mut last_line = now();
    let mut last_done = 0u64;
    loop {
        let _ = WORK.down_timeout(1_000_000_000);
        let job = {
            let mut st = STATE.lock();
            match st.as_mut() {
                Some(p) => match p.stage {
                    Stage::Queued { job, taken: false } => {
                        p.stage = Stage::Queued { job, taken: true };
                        Some((job, p.regs.expect("queued flip"), p.gen))
                    }
                    _ => None,
                },
                None => None,
            }
        };
        if let Some((job, r, gen)) = job {
            let t0 = now();
            let ok = kms::fence_wait(&r, 1_000_000_000);
            let t1 = now();
            if ok && kms::needs_flush(&r) { kms::flush(&r); }
            let t2 = now();
            let mut drop_it: Option<Pending> = None;
            let armed = {
                let mut st = STATE.lock();
                match *st {
                    Some(p) if p.stage == (Stage::Queued { job, taken: true }) => {
                        if !ok || gen != GEN.load(Ordering::Acquire) {
                            *st = None;
                            drop_it = Some(p);
                            None
                        } else {
                            Some(arm_locked(&mut st, p))
                        }
                    }
                    // Dropped meanwhile (RMFB of it, console restore).
                    _ => None,
                }
            };
            if let Some(a) = armed {
                ARMED_WORKER.fetch_add(1, Ordering::Relaxed);
                backstop(FRAME_NS + 3_000_000);
                kms::after_arm(&r, &a, (t0, t1, t2));
            }
            if let Some(p) = drop_it {
                DROPPED.fetch_add(1, Ordering::Relaxed);
                if !ok {
                    if FENCE_TIMEOUTS.fetch_add(1, Ordering::Relaxed) < 8 {
                        ilog!("kms: vblank: fb {} rendering not done after 1 s; flip dropped (event sent)", r.token);
                    }
                }
                complete_now(&p);
            }
        }
        // The summary line: every 10 s while flips happen with kmsdebug,
        // every 60 s without.
        let n = NDONE.load(Ordering::Relaxed);
        let every = if kms::debug() { 10_000_000_000 } else { 60_000_000_000 };
        if n != last_done && now().wrapping_sub(last_line) >= every {
            stats_line(n - last_done);
            last_done = n;
            last_line = now();
        }
    }
}

fn stats_line(window: u64) {
    let n = NDONE.load(Ordering::Relaxed).max(1);
    let (ebusy, evpend) = crate::drm_device_interface::ext_event_stats();
    ilog!("kms: vblank: {} completions in window ({} total: {} on IRQ, {} by polling); armed {} in-ioctl / {} after fence; event-only {}; dropped {} (fence timeouts {}); unlatched {}; queue->event avg {} us max {} us, arm->event avg {} us max {} us; IRQs {} (vblank/flip {}) irq={}; EBUSY {}; DRM events pending {}; underruns {}",
          window, NDONE.load(Ordering::Relaxed), DONE_IRQ.load(Ordering::Relaxed), DONE_POLL.load(Ordering::Relaxed),
          ARMED_DIRECT.load(Ordering::Relaxed), ARMED_WORKER.load(Ordering::Relaxed), EVENT_ONLY.load(Ordering::Relaxed),
          DROPPED.load(Ordering::Relaxed), FENCE_TIMEOUTS.load(Ordering::Relaxed), UNLATCHED.load(Ordering::Relaxed),
          Q2D_NS.load(Ordering::Relaxed) / n / 1000, Q2D_MAX.swap(0, Ordering::Relaxed) / 1000,
          A2D_NS.load(Ordering::Relaxed) / n / 1000, A2D_MAX.swap(0, Ordering::Relaxed) / 1000,
          IRQS.load(Ordering::Relaxed), IRQ_VBL.load(Ordering::Relaxed), IRQ_ON.load(Ordering::Relaxed) as u8,
          ebusy, evpend, kms::UNDERRUNS.load(Ordering::Relaxed));
    let (sets, moves) = super::cursor::stats();
    if sets != 0 { ilog!("kms: cursor: {} images, {} moves", sets, moves); }
    if IRQ_ON.load(Ordering::Relaxed) && IRQ_VBL.load(Ordering::Relaxed) == 0 && NDONE.load(Ordering::Relaxed) >= 30 {
        static NOTED: AtomicBool = AtomicBool::new(false);
        if !NOTED.swap(true, Ordering::Relaxed) {
            log("kms: vblank: the display interrupt never arrived; flips complete by polling (2 ms granularity). i915.vblank=0 restores the synchronous path");
        }
    }
}

// ── the interrupt ───────────────────────────────────────────────────────────

/// gen8_irq_handler's shape: master off, banks acked (DE pipe A handled),
/// master back on (a bank that latched meanwhile raises a new MSI).
extern "C" fn isr(_ctx: usize) -> crate::irq::IrqReturn {
    let m = kms::mmio();
    IRQS.fetch_add(1, Ordering::Relaxed);
    let master = m.r(MASTER_IRQ) & !MASTER_IRQ_CONTROL;
    if master == 0 { return crate::irq::IrqReturn::Handled; }
    m.w(MASTER_IRQ, 0);
    // GT banks: nothing here unmasks GT sources (execution completion is
    // polled); acknowledge whatever latched so it cannot re-raise. A GT/PM
    // interrupt user dispatches from here.
    for &(bits, iir) in MASTER_GT.iter() {
        if master & bits != 0 {
            let v = m.r(iir);
            if v != 0 { m.w(iir, v); }
        }
    }
    let mut ours = false;
    let mut hpd_seen = false;
    if master & MASTER_DE_PIPE_A != 0 {
        let iir = m.r(DE_PIPE_IIR);
        if iir != 0 {
            m.w(DE_PIPE_IIR, iir);
            if iir & PIPE_FIFO_UNDERRUN != 0 { kms::UNDERRUNS.fetch_add(1, Ordering::Relaxed); }
            if iir & OURS != 0 { ours = true; }
        }
    }
    for &(bit, iir) in [(MASTER_DE_PIPE_B, DE_PIPE_IIR + 0x10), (MASTER_DE_PIPE_C, DE_PIPE_IIR + 0x20),
                        (MASTER_DE_PORT, DE_PORT_IIR), (MASTER_DE_MISC, DE_MISC_IIR),
                        (MASTER_DE_PCH, SDEIIR), (MASTER_PCU, PCU_IIR)].iter() {
        if master & bit != 0 {
            let v = m.r(iir);
            if v != 0 { m.w(iir, v); }
            if iir == SDEIIR && v & SDE_HPD_BITS != 0 {
                // Latch the per-port pulse status (write-1-to-clear), hand it to
                // the i915-hpd thread. No locks here.
                let st = m.r(PCH_PORT_HOTPLUG);
                m.w(PCH_PORT_HOTPLUG, st);
                super::outputs::hpd_irq(st);
                hpd_seen = true;
            }
        }
    }
    if ours {
        IRQ_VBL.fetch_add(1, Ordering::Relaxed);
        IRQ_IDLE_STREAK.store(0, Ordering::Relaxed);
        let _ = check(true);
    } else if hpd_seen {
        IRQ_IDLE_STREAK.store(0, Ordering::Relaxed);
    } else if IRQ_IDLE_STREAK.fetch_add(1, Ordering::Relaxed) > 100_000 {
        // An interrupt source we do not handle keeps firing: leave the
        // master off (the backstop polls) rather than storm.
        IRQ_ON.store(false, Ordering::Relaxed);
        return crate::irq::IrqReturn::Handled;
    }
    // After the kexec hook masked everything, leave the master off.
    if KEXEC_OFF.load(Ordering::Acquire) { return crate::irq::IrqReturn::Handled; }
    m.w(MASTER_IRQ, MASTER_IRQ_CONTROL);
    crate::irq::IrqReturn::Handled
}

/// Unmask the PCH hotplug interrupt (DDI B/C/D) once the display MSI is live.
/// Returns the readback `(SDEIMR, SDEIER)`, or None when the IRQ path is off.
pub(super) fn hpd_irq_enable(m: &super::Mmio) -> Option<(u32, u32)> {
    if !IRQ_ON.load(Ordering::Acquire) || KEXEC_OFF.load(Ordering::Acquire) { return None; }
    m.w(SDEIIR, SDE_HPD_BITS);
    m.w(SDEIMR, m.r(SDEIMR) & !SDE_HPD_BITS);
    m.w(SDEIER, m.r(SDEIER) | SDE_HPD_BITS);
    Some((m.r(SDEIMR), m.r(SDEIER)))
}

/// Set by [`kexec_irq_off`]: the ISR must not turn the master back on.
static KEXEC_OFF: AtomicBool = AtomicBool::new(false);

/// kexec hook: hand the display over with its interrupts off (Linux
/// `gen8_irq_reset` for the display banks): GEN8_MASTER_IRQ = 0, then for
/// DE pipe A (and B/C when their IER is set; the IER of a pipe whose power
/// well is down reads 0), DE port and DE misc: IMR all masked, IER 0, IIR
/// cleared (twice: the IIR is double-buffered). Only interrupt registers are
/// written, so the plane keeps scanning out the boot framebuffer. Lock-free,
/// no waits. `None` when the display MMIO was never mapped (or fake).
pub(super) fn kexec_irq_off() -> Option<alloc::string::String> {
    if fake() { return None; }
    let m = kms::mmio();
    if m.va == 0 { return None; }
    KEXEC_OFF.store(true, Ordering::Release);
    IRQ_ON.store(false, Ordering::Release);
    let master = m.r(MASTER_IRQ);
    m.w(MASTER_IRQ, 0);
    let _ = m.r(MASTER_IRQ);
    let ier_a = m.r(DE_PIPE_IER);
    let mut pipes = 0;
    for p in 0..3u32 {
        let o = 0x10 * p;
        if p != 0 && m.r(DE_PIPE_IER + o) == 0 { continue; }
        m.w(DE_PIPE_IMR + o, !0);
        m.w(DE_PIPE_IER + o, 0);
        m.w(DE_PIPE_IIR + o, !0);
        m.w(DE_PIPE_IIR + o, !0);
        pipes += 1;
    }
    m.w(SDEIMR, !0);
    m.w(SDEIER, 0);
    m.w(SDEIIR, !0);
    // DE port (IMR/IIR/IER 0x44444/8/C) and DE misc (0x44464/8/C).
    for iir in [DE_PORT_IIR, DE_MISC_IIR] {
        m.w(iir - 4, !0);
        m.w(iir + 4, 0);
        m.w(iir, !0);
        m.w(iir, !0);
    }
    Some(alloc::format!(
        "i915: display interrupts off (MASTER_IRQ 0x{:08x} -> 0x{:08x}, pipe A IER 0x{:08x} -> 0x{:08x}, {} pipe(s) + port/misc masked); scanout untouched",
        master, m.r(MASTER_IRQ), ier_a, m.r(DE_PIPE_IER), pipes))
}

// ── init ────────────────────────────────────────────────────────────────────

/// Register the real-vblank flip half (unless `i915.vblank=0`). Called from
/// `kms::init` after the GOP state was verified, before the backend registers.
pub(super) fn init(d: &Device) -> bool {
    if opt("i915.vblank") == Some(0) {
        log("kms: i915.vblank=0: synchronous flips, 60 Hz synthetic flip events");
        return false;
    }
    let t = alloc::boxed::Box::into_raw(sched::hrtimer::HrTimer::new(backstop_fire, 0, sched::hrtimer::TimerMode::Irq));
    TIMER.store(t, Ordering::Release);
    if sched::kthread::spawn("i915-flip", worker).is_none() {
        log("kms: vblank: could not start the flip worker; synchronous flips");
        return false;
    }
    let mut how = alloc::string::String::from("fake device: a 60 Hz software frame counter");
    if !d.fake {
        let m = &d.mmio;
        ilog!("kms: vblank: before: MASTER_IRQ=0x{:08x} DE pipe A IMR=0x{:08x} IIR=0x{:08x} IER=0x{:08x} FRMCOUNT={} PIPEDSL={}",
              m.r(MASTER_IRQ), m.r(DE_PIPE_IMR), m.r(DE_PIPE_IIR), m.r(DE_PIPE_IER),
              m.r(reg::pipe(reg::PIPE_FRMCOUNT_A, 0)), m.r(reg::PIPEDSL_A) & 0x1FFF);
        how = if opt("i915.vblank_irq") == Some(0) {
            alloc::string::String::from("i915.vblank_irq=0: completion by polling only")
        } else {
            match crate::irq::request_msi(d.func, isr, 0) {
                Ok(h) => {
                    core::mem::forget(h);
                    // Mask ours, clear stale latches, enable them in the IER
                    // (the IMR unmasks them while something is pending), and
                    // the master.
                    m.w(DE_PIPE_IMR, m.r(DE_PIPE_IMR) | OURS);
                    m.w(DE_PIPE_IIR, OURS);
                    m.w(DE_PIPE_IER, m.r(DE_PIPE_IER) | OURS);
                    IRQ_ON.store(true, Ordering::Release);
                    m.w(MASTER_IRQ, MASTER_IRQ_CONTROL);
                    alloc::format!("MSI, DE pipe A VBLANK + PLANE1_FLIP_DONE (IER now 0x{:08x}, MASTER_IRQ 0x{:08x})",
                                   m.r(DE_PIPE_IER), m.r(MASTER_IRQ))
                }
                Err(e) => alloc::format!("no MSI ({:?}): completion by polling", e),
            }
        };
    }
    ENABLED.store(true, Ordering::Release);
    crate::gpu_kms::register_flip(&FLIP);
    ilog!("kms: vblank: flips queued from the ioctl, armed after the render fence, completed at the latching vblank ({}); one commit in flight (EBUSY), i915.vblank=0 for the old synchronous path", how);
    true
}
