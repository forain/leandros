//! V3D 7.1 control-list queue (openbugs #98 phase C): one bin (CT0) + render (CT1) pipeline, one job
//! in flight, strictly in submission order, so fences retire in order and the global fence
//! watermark (`syncobj_retire_fence`) stays correct.
//!
//! Pure logic over the `V3dRegs` seam; the kernel glue (`submit.rs`, `aarch64 + rpi5`) owns the
//! lock, the fence ids and the BO references. Nothing here allocates while servicing an interrupt:
//! `push` reserves every vector the job can grow into, completed jobs move into a pre-reserved
//! `retired` ring and are dropped (BO refs released) later in task context by `take_retired`.
//!
//! Register sequences follow Linux `v3d_sched.c` / `v3d_gem.c` / `v3d_irq.c`:
//! - bin: invalidate caches; `PTB_BPOS = 0`; `CT0QMA/QMS` if qma; `CT0QTS = qts|EN` if qts;
//!   `CT0QBA`; `CT0QEA` starts it. Done = FLDONE.
//! - render: invalidate caches; `CT1QBA`; `CT1QEA` starts it. Done = FRDONE. Waits for bin.
//! - OUTOMEM: hand the binner a 256 KiB overflow chunk (`PTB_BPOA`, `PTB_BPOS`); the chunk stays
//!   attached to the job until render is done, then goes back to the pool.
//! - FLUSH_CACHE: the clean sequence (TMUWCF, then L2T clean) on the CPU before the fence signals.
//! - hang: every `HANG_TIMEOUT_NS` the running CT's CA/RA must have moved, else reset (GMP stop,
//!   SMS reset, re-init) and the job's fence signals with the job marked failed.
use super::regs::*;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

pub const HANG_TIMEOUT_NS: u64 = 500_000_000;
pub const OVERFLOW_BYTES: u32 = 256 * 1024;
/// Jobs queued + retired-but-not-reaped; the submit path waits for space beyond this.
pub const MAX_JOBS: usize = 64;
/// Overflow chunks one job may take before the hang timer has to resolve it.
pub const MAX_OVERFLOW_PER_JOB: usize = 16;
pub const MAX_CHUNKS: usize = 64;
/// Bounded register polls (cache clean, GMP drain, SMS reset).
pub const POLL_TIMEOUT_NS: u64 = 100_000_000;
pub const POLL_MAX_ITERS: u32 = 200_000;

/// `SLCACTL`: TVCCS | TDCCS | UCC | ICC, every slice.
pub const SLCACTL_INVALIDATE_ALL: u32 = 0x0F0F_0F0F;
/// Linux v3d_regs.h `V3D_GMP_STATUS_*`: outstanding reads/writes and config busy.
pub const GMP_STATUS_RD_COUNT_MASK: u32 = 0x007f_0000;
pub const GMP_STATUS_WR_COUNT_MASK: u32 = 0x7f00_0000;
pub const GMP_STATUS_CFG_BUSY: u32 = 1 << 3;
pub const GMP_STATUS_BUSY: u32 = GMP_STATUS_RD_COUNT_MASK | GMP_STATUS_WR_COUNT_MASK | GMP_STATUS_CFG_BUSY;

/// The environment the queue runs in (kernel glue or a host test).
pub trait QueueEnv {
    fn now_ns(&self) -> u64;
    /// True once a dependency fence has signalled.
    fn fence_done(&self, fence: u64) -> bool;
    /// Signals the job's fence (in order).
    fn signal(&self, fence: u64);
    /// After a reset: MMU page table + enable, interrupt masks. False = could not re-init.
    fn reinit(&self, regs: &dyn V3dRegs) -> bool;
}

#[derive(Debug, Default)]
pub struct ClJob {
    pub fence: u64,
    pub bcl_start: u32,
    pub bcl_end: u32,
    pub rcl_start: u32,
    pub rcl_end: u32,
    pub qma: u32,
    pub qms: u32,
    pub qts: u32,
    pub bin_deps: Vec<u64>,
    pub render_deps: Vec<u64>,
    pub flush_cache: bool,
    /// Opaque BO object references, released in task context after the fence.
    pub bos: Vec<u32>,
    /// Overflow chunk VAs handed to the binner for this job.
    pub overflow: Vec<u32>,
    /// False when the job was killed by a reset (or a cache clean timed out).
    pub ok: bool,
}

impl ClJob {
    pub fn has_bin(&self) -> bool { self.bcl_start != self.bcl_end }
    pub fn has_render(&self) -> bool { self.rcl_start != self.rcl_end }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Bin,
    /// Bin done (or absent); render waits for its dependencies.
    WaitRender,
    Render,
}

struct Running {
    job: ClJob,
    phase: Phase,
    deadline: u64,
    ca: u32,
    ra: u32,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub submitted: u64,
    pub completed: u64,
    pub bin_done: u64,
    pub render_done: u64,
    pub outomem: u64,
    pub overflow_fail: u64,
    pub timeouts: u64,
    pub resets: u64,
    pub reset_fail: u64,
    pub clean_fail: u64,
    pub stray_irq: u64,
}

pub struct ClQueue {
    pending: VecDeque<ClJob>,
    run: Option<Running>,
    retired: VecDeque<ClJob>,
    chunks: Vec<u32>,
    pub stats: Stats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    Full,
}

/// Polls `f` until it returns true, bounded by time and by an iteration cap (a frozen clock ends).
pub fn poll_until(env: &dyn QueueEnv, mut f: impl FnMut() -> bool) -> bool {
    let t0 = env.now_ns();
    let mut i = 0u32;
    loop {
        if f() { return true; }
        i += 1;
        if i >= POLL_MAX_ITERS || env.now_ns().wrapping_sub(t0) >= POLL_TIMEOUT_NS { return f(); }
        core::hint::spin_loop();
    }
}

/// `v3d_invalidate_caches` on 7.1 (L3 and L2C are no-ops): L2T flush, then the slice caches.
pub fn invalidate_caches(regs: &dyn V3dRegs) {
    regs.core_w(CTL_L2TCACTL, L2TCACTL_L2TFLS | (L2TCACTL_FLM_FLUSH << L2TCACTL_FLM_SHIFT));
    regs.core_w(CTL_SLCACTL, SLCACTL_INVALIDATE_ALL);
}

/// `v3d_clean_caches`: TMU write-combiner flush, then an L2T clean. False on a timeout.
pub fn clean_caches(regs: &dyn V3dRegs, env: &dyn QueueEnv) -> bool {
    regs.core_w(CTL_L2TCACTL, L2TCACTL_TMUWCF);
    let a = poll_until(env, || regs.core_r(CTL_L2TCACTL) & L2TCACTL_TMUWCF == 0);
    regs.core_w(CTL_L2TCACTL, L2TCACTL_L2TFLS | (L2TCACTL_FLM_CLEAN << L2TCACTL_FLM_SHIFT));
    let b = poll_until(env, || regs.core_r(CTL_L2TCACTL) & L2TCACTL_L2TFLS == 0);
    a && b
}

/// GPU reset after a hang (Linux `v3d_reset`, 7.1 flavour): stop the GMP and let outstanding AXI
/// traffic drain, SMS reset, then the core's L2T range and the caller's MMU/IRQ re-init.
/// Returns false if any bounded step timed out (the block is then best-effort).
pub fn reset_gpu(regs: &dyn V3dRegs, env: &dyn QueueEnv) -> bool {
    regs.hub_w(GMP_CFG, GMP_CFG_STOP_REQ);
    let gmp = poll_until(env, || regs.hub_r(GMP_STATUS) & GMP_STATUS_BUSY == 0);
    regs.sms_w(SMS_REE_CS, SMS_STATE_RESET_REQ);
    let sms = poll_until(env, || {
        let s = sms_state(regs.sms_r(SMS_REE_CS));
        s != SMS_STATE_ISOLATING_FOR_RESET && s != SMS_STATE_RESETTING
    });
    regs.core_w(CTL_L2TFLSTA, 0);
    regs.core_w(CTL_L2TFLEND, !0);
    let re = env.reinit(regs);
    gmp && sms && re
}

impl ClQueue {
    pub const fn new() -> Self {
        ClQueue { pending: VecDeque::new(), run: None, retired: VecDeque::new(), chunks: Vec::new(), stats: Stats {
            submitted: 0, completed: 0, bin_done: 0, render_done: 0, outomem: 0, overflow_fail: 0,
            timeouts: 0, resets: 0, reset_fail: 0, clean_fail: 0, stray_irq: 0 } }
    }

    /// Task context, once: reserves the rings so interrupt-time work never allocates.
    pub fn reserve(&mut self) {
        self.pending.reserve(MAX_JOBS);
        self.retired.reserve(MAX_JOBS + 1);
        self.chunks.reserve(MAX_CHUNKS);
    }

    pub fn has_space(&self) -> bool {
        self.pending.len() + self.retired.len() + (self.run.is_some() as usize) < MAX_JOBS
    }
    pub fn idle(&self) -> bool { self.run.is_none() && self.pending.is_empty() }
    pub fn phase(&self) -> Option<Phase> { self.run.as_ref().map(|r| r.phase) }
    pub fn free_chunks(&self) -> usize { self.chunks.len() }

    /// Task context: adds an overflow chunk (already GPU-mapped at `va`) to the pool.
    pub fn add_chunk(&mut self, va: u32) -> bool {
        if self.chunks.len() >= MAX_CHUNKS { return false; }
        self.chunks.push(va);
        true
    }

    /// Task context: queues a job (call `service` afterwards to start it).
    pub fn push(&mut self, mut job: ClJob) -> Result<(), PushError> {
        if !self.has_space() { return Err(PushError::Full); }
        job.overflow.reserve(MAX_OVERFLOW_PER_JOB);
        job.ok = true;
        self.pending.push_back(job);
        self.stats.submitted += 1;
        Ok(())
    }

    /// Task context: one completed job whose BO references can now be released.
    pub fn take_retired(&mut self) -> Option<ClJob> { self.retired.pop_front() }

    /// Interrupt bits (core `INT_STS`, already acked) and/or a plain kick: advance the pipeline.
    pub fn service(&mut self, regs: &dyn V3dRegs, env: &dyn QueueEnv, bits: u32) {
        if bits & INT_OUTOMEM != 0 { self.on_outomem(regs); }
        if bits & INT_FLDONE != 0 {
            match self.run.as_mut() {
                Some(r) if r.phase == Phase::Bin => { r.phase = Phase::WaitRender; self.stats.bin_done += 1; }
                _ => self.stats.stray_irq += 1,
            }
        }
        if bits & INT_FRDONE != 0 {
            match self.run.as_ref() {
                Some(r) if r.phase == Phase::Render => { self.stats.render_done += 1; self.finish(regs, env, true); }
                _ => self.stats.stray_irq += 1,
            }
        }
        self.pump(regs, env);
    }

    /// Periodic (tick): the hang check, plus a dependency re-check for a waiting job.
    pub fn tick(&mut self, regs: &dyn V3dRegs, env: &dyn QueueEnv) {
        let now = env.now_ns();
        let hung = match self.run.as_mut() {
            Some(r) if r.phase != Phase::WaitRender && now >= r.deadline => {
                let (ca, ra) = if r.phase == Phase::Bin { (CLE_CT0CA, CLE_CT0RA) } else { (CLE_CT1CA, CLE_CT1RA) };
                let (ca, ra) = (regs.core_r(ca), regs.core_r(ra));
                if ca != r.ca || ra != r.ra {
                    r.ca = ca;
                    r.ra = ra;
                    r.deadline = now + HANG_TIMEOUT_NS;
                    false
                } else {
                    true
                }
            }
            _ => false,
        };
        if hung {
            self.stats.timeouts += 1;
            self.reset(regs, env);
        }
        self.pump(regs, env);
    }

    /// Kills the running job (fence signalled, `ok = false`) and resets the GPU.
    pub fn reset(&mut self, regs: &dyn V3dRegs, env: &dyn QueueEnv) {
        if !reset_gpu(regs, env) { self.stats.reset_fail += 1; }
        self.stats.resets += 1;
        if self.run.is_some() { self.finish(regs, env, false); }
    }

    fn on_outomem(&mut self, regs: &dyn V3dRegs) {
        self.stats.outomem += 1;
        let r = match self.run.as_mut() {
            Some(r) if r.phase == Phase::Bin => r,
            _ => { self.stats.stray_irq += 1; return; }
        };
        // Capacity was reserved at push: never grow (allocate) here.
        if r.job.overflow.len() >= r.job.overflow.capacity() {
            self.stats.overflow_fail += 1;
            return;
        }
        match self.chunks.pop() {
            Some(va) => {
                r.job.overflow.push(va);
                regs.core_w(PTB_BPOA, va);
                regs.core_w(PTB_BPOS, OVERFLOW_BYTES);
            }
            // Nothing to give: the binner stalls and the hang timer resets it.
            None => self.stats.overflow_fail += 1,
        }
    }

    fn deps_done(env: &dyn QueueEnv, deps: &[u64]) -> bool { deps.iter().all(|&f| env.fence_done(f)) }

    fn arm(&mut self, regs: &dyn V3dRegs, env: &dyn QueueEnv, phase: Phase) {
        let r = self.run.as_mut().unwrap();
        let j = &r.job;
        invalidate_caches(regs);
        if phase == Phase::Bin {
            // Drop the previous job's overflow so the binner cannot reuse it.
            regs.core_w(PTB_BPOS, 0);
            if j.qma != 0 {
                regs.core_w(CLE_CT0QMA, j.qma);
                regs.core_w(CLE_CT0QMS, j.qms);
            }
            if j.qts != 0 { regs.core_w(CLE_CT0QTS, CLE_CT0QTS_ENABLE | j.qts); }
            regs.core_w(CLE_CT0QBA, j.bcl_start);
            regs.core_w(CLE_CT0QEA, j.bcl_end);
        } else {
            regs.core_w(CLE_CT1QBA, j.rcl_start);
            regs.core_w(CLE_CT1QEA, j.rcl_end);
        }
        r.phase = phase;
        r.deadline = env.now_ns() + HANG_TIMEOUT_NS;
        r.ca = 0;
        r.ra = 0;
    }

    fn pump(&mut self, regs: &dyn V3dRegs, env: &dyn QueueEnv) {
        loop {
            match self.run.as_ref().map(|r| r.phase) {
                Some(Phase::WaitRender) => {
                    let r = self.run.as_ref().unwrap();
                    // With no bin job the in_sync_bcl dependencies gate the render job instead.
                    let ready = Self::deps_done(env, &r.job.render_deps)
                        && (r.job.has_bin() || Self::deps_done(env, &r.job.bin_deps));
                    if !ready { return; }
                    if !r.job.has_render() {
                        self.finish(regs, env, true);
                        continue;
                    }
                    self.arm(regs, env, Phase::Render);
                    return;
                }
                Some(_) => return,
                None => {
                    let head = match self.pending.front() { Some(j) => j, None => return };
                    if head.has_bin() && !Self::deps_done(env, &head.bin_deps) { return; }
                    let job = self.pending.pop_front().unwrap();
                    let bin = job.has_bin();
                    self.run = Some(Running { job, phase: Phase::WaitRender, deadline: 0, ca: 0, ra: 0 });
                    if bin {
                        self.arm(regs, env, Phase::Bin);
                        return;
                    }
                }
            }
        }
    }

    fn finish(&mut self, regs: &dyn V3dRegs, env: &dyn QueueEnv, ok: bool) {
        let mut r = self.run.take().unwrap();
        if ok && r.job.flush_cache && !clean_caches(regs, env) {
            self.stats.clean_fail += 1;
            r.job.ok = false;
        }
        if !ok { r.job.ok = false; }
        // Overflow chunks go back to the pool (capacity reserved: no allocation).
        while let Some(va) = r.job.overflow.pop() {
            if self.chunks.len() < self.chunks.capacity() { self.chunks.push(va); }
        }
        let fence = r.job.fence;
        self.stats.completed += 1;
        if self.retired.len() < self.retired.capacity() {
            self.retired.push_back(r.job);
        } else {
            // Unreachable with `has_space`; leak rather than allocate or free in IRQ context.
            core::mem::forget(r.job);
        }
        env.signal(fence);
    }
}
