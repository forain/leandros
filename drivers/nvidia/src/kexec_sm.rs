//! NVIDIA kexec unload: the ordering, the once-only guard and the bounded
//! wait, with every RM and hardware effect behind `Rm` / `Host` so the host
//! test (drivers/nvidia/kexec-tests) drives it with a fake transport. No
//! dependencies beyond `core`.
//!
//! The sequence is Linux's (kernel-open/nvidia/nv.c `nv_shutdown_adapter`,
//! OpenRM 570.86.16), which nouveau's `r535_gsp_fini(gsp, suspend=false)`
//! mirrors step for step:
//!   1. `rm_disable_adapter`: GPU interrupts off (intrSetIntrEn DISABLED),
//!      channels and engines stopped.
//!   2. Bottom half stopped, MSI freed (from here RM polls).
//!   3. `rm_shutdown_adapter`: gpuStateUnload + gpuStateDestroy, whose
//!      `kgspUnloadRm` sends NV_RM_RPC_UNLOADING_GUEST_DRIVER (power level 0,
//!      not a PM transition), waits in kgspWaitForProcessorSuspend for GSP
//!      to halt, then kgspTeardown: FWSEC-SB and booter unload, which takes
//!      WPR2 down. (nouveau: rpc_unloading_guest_driver, the falcon 0x040
//!      bit-31 wait, falcon reset, fwsec_sb, booter_unload.)
//! Steps 1-3 run on a worker thread; the kexec path waits for it with a
//! deadline and then reads WPR2 back. Anything but "worker done, WPR2 down"
//! asks devreset to reset (FLR) the GPU, so kexec never hangs on the GPU and
//! the next kernel's `kgspIsWpr2Up` check ("unexpected WPR2 already up,
//! cannot proceed with booting GSP") has a chance to pass.

use core::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Phase {
    Idle = 0,
    Disabling = 1,
    StoppingIrq = 2,
    Unloading = 3,
    Done = 4,
}

impl Phase {
    pub fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Disabling,
            2 => Phase::StoppingIrq,
            3 => Phase::Unloading,
            4 => Phase::Done,
            _ => Phase::Idle,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Phase::Idle => "worker not started",
            Phase::Disabling => "rm_disable_adapter",
            Phase::StoppingIrq => "bottom half / MSI teardown",
            Phase::Unloading => "rm_shutdown_adapter (GSP unload RPC, halt wait, booter unload)",
            Phase::Done => "done",
        }
    }
}

/// Worker side: the RM calls, in Linux's order.
pub trait Rm {
    fn disable_adapter(&mut self);
    fn stop_irq(&mut self);
    fn shutdown_adapter(&mut self);
    fn set_phase(&mut self, p: Phase);
}

/// The worker thread's body. Publishes each phase before entering it, so a
/// timeout names the call that hung.
pub fn worker<R: Rm>(rm: &mut R) {
    rm.set_phase(Phase::Disabling);
    rm.disable_adapter();
    rm.set_phase(Phase::StoppingIrq);
    rm.stop_irq();
    rm.set_phase(Phase::Unloading);
    rm.shutdown_adapter();
    rm.set_phase(Phase::Done);
}

/// kexec side.
pub trait Host {
    /// Start `worker` on its own thread; false if it could not be started.
    fn start_worker(&mut self) -> bool;
    fn phase(&self) -> Phase;
    fn now_ms(&self) -> u64;
    fn sleep_ms(&mut self, ms: u64);
    /// NV_PFB_PRI_MMU_WPR2_ADDR_HI_VAL != 0; None when BAR0 is not readable.
    fn wpr2_up(&mut self) -> Option<bool>;
}

pub struct Cfg {
    /// `nvidia.kexec_unload`.
    pub enabled: bool,
    /// rm_init_adapter succeeded (GSP-RM reached INIT_DONE).
    pub gsp_up: bool,
    /// `nvidia.kexec_timeout_ms`.
    pub timeout_ms: u64,
}

pub const POLL_MS: u64 = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Knob off: nothing done, nothing requested (the previous behaviour).
    Disabled,
    /// GSP never came up: nothing to unload.
    NotRunning,
    /// A second call: GSP is never unloaded (and its memory freed) twice.
    AlreadyRan,
    /// Worker finished; `wpr2_checked` false when BAR0 could not be read.
    Unloaded { wpr2_checked: bool },
    /// The worker thread could not be started.
    WorkerFailed,
    /// The worker did not finish in time; the phase it was stuck in.
    Timeout(Phase),
    /// The worker finished but booter unload left WPR2 up.
    Wpr2StillUp,
}

impl Outcome {
    /// Should devreset reset the GPU function even when it owns the boot
    /// framebuffer?
    pub fn wants_flr(self) -> bool {
        matches!(self, Outcome::WorkerFailed | Outcome::Timeout(_) | Outcome::Wpr2StillUp)
    }
}

/// The kexec hook. `once` is the driver's static guard.
pub fn run<H: Host>(once: &AtomicBool, cfg: &Cfg, h: &mut H) -> Outcome {
    if !cfg.enabled {
        return Outcome::Disabled;
    }
    if !cfg.gsp_up {
        return Outcome::NotRunning;
    }
    if once.swap(true, Ordering::AcqRel) {
        return Outcome::AlreadyRan;
    }
    if !h.start_worker() {
        return Outcome::WorkerFailed;
    }
    let t0 = h.now_ms();
    while h.phase() != Phase::Done {
        if h.now_ms().saturating_sub(t0) >= cfg.timeout_ms {
            return Outcome::Timeout(h.phase());
        }
        h.sleep_ms(POLL_MS);
    }
    match h.wpr2_up() {
        Some(true) => Outcome::Wpr2StillUp,
        Some(false) => Outcome::Unloaded { wpr2_checked: true },
        None => Outcome::Unloaded { wpr2_checked: false },
    }
}
