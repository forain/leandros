//! kexec hook: unload GSP-RM before the jump so the next kernel can boot it
//! again (without it, OpenRM's kgspBootstrap stops at "unexpected WPR2
//! already up, cannot proceed with booting GSP"). Ordering, once-only guard
//! and the bounded wait live in `kexec_sm` (host-tested); this file binds
//! them to the RM entry points, a kthread, the clock and BAR0.
//!
//! Knobs:
//!   nvidia.kexec_unload=1|0      (default 0 until validated on the 3070,
//!                                  see artifacts/notes/lane-gspkexec-*.md)
//!   nvidia.kexec_timeout_ms=N    (default 10000) worker deadline
//! A timeout, a failed worker start or WPR2 still up afterwards ask devreset
//! to reset the GPU function even when it owns the boot framebuffer.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};

use crate::glue::{self, NvState, NV_GPU_BAR_INDEX_REGS};
use crate::kexec_sm::{self, Cfg, Host, Outcome, Phase, Rm};

/// Set by device.rs once rm_init_adapter succeeded on real hardware.
pub(crate) static NV: AtomicPtr<NvState> = AtomicPtr::new(core::ptr::null_mut());
pub(crate) static GSP_UP: AtomicBool = AtomicBool::new(false);
static BDF: AtomicU32Bdf = AtomicU32Bdf::new();
static ONCE: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicU8 = AtomicU8::new(Phase::Idle as u8);

/// NV_PFB_PRI_MMU_WPR2_ADDR_HI (Turing+ dev_fb.h), VAL = bits 31:4.
const WPR2_ADDR_HI: u64 = 0x001F_A828;

struct AtomicU32Bdf(core::sync::atomic::AtomicU32);
impl AtomicU32Bdf {
    const fn new() -> Self { AtomicU32Bdf(core::sync::atomic::AtomicU32::new(u32::MAX)) }
}

pub(crate) fn gsp_started(nv: *mut NvState, bus: u8, dev: u8, func: u8) {
    NV.store(nv, Ordering::Release);
    BDF.0.store(((bus as u32) << 16) | ((dev as u32) << 8) | func as u32, Ordering::Release);
    GSP_UP.store(true, Ordering::Release);
}

struct KRm(*mut NvState);

impl Rm for KRm {
    fn disable_adapter(&mut self) { unsafe { crate::rm::rm_disable_adapter(core::ptr::null_mut(), self.0) } }
    fn stop_irq(&mut self) { crate::device::kexec_stop_irq() }
    fn shutdown_adapter(&mut self) { unsafe { crate::rm::rm_shutdown_adapter(core::ptr::null_mut(), self.0) } }
    fn set_phase(&mut self, p: Phase) { PHASE.store(p as u8, Ordering::Release) }
}

struct KHost(*mut NvState);

impl Host for KHost {
    fn start_worker(&mut self) -> bool {
        let nv = self.0 as usize;
        sched::kthread::spawn("nvrm-unload", move || kexec_sm::worker(&mut KRm(nv as *mut NvState))).is_some()
    }
    fn phase(&self) -> Phase { Phase::from_u8(PHASE.load(Ordering::Acquire)) }
    fn now_ms(&self) -> u64 { sched::monotonic_ns() / 1_000_000 }
    fn sleep_ms(&mut self, ms: u64) { sched::kthread::msleep(ms) }
    fn wpr2_up(&mut self) -> Option<bool> {
        let (mut base, mut size) = (0u64, 0u64);
        unsafe { glue::lnv_state_bar(self.0, NV_GPU_BAR_INDEX_REGS, &mut base, &mut size) };
        if base == 0 || size <= WPR2_ADDR_HI + 4 { return None; }
        let va = mm::kva::ioremap(base + WPR2_ADDR_HI, 4, mm::kva::CacheMode::Uncached)?;
        let v = unsafe { core::ptr::read_volatile(va as *const u32) };
        // All-ones: the function no longer decodes BAR0 (fell off the bus).
        if v == u32::MAX { return None; }
        Some(v >> 4 != 0)
    }
}

/// What the kernel's kexec path gets back: log lines, and the function to
/// reset even if it owns the boot framebuffer.
pub struct KexecUnload {
    pub lines: Vec<String>,
    pub force_reset: Option<(u8, u8, u8)>,
}

/// Task context, interrupts and scheduler on, bus mastering still on (GSP's
/// RPC queues live in system memory): call before power.rs quiesce_phase1.
pub fn kexec_unload() -> KexecUnload {
    let enabled = matches!(crate::opt_str("nvidia.kexec_unload"), Some("1") | Some("on") | Some("yes"));
    let timeout_ms = crate::opt_str("nvidia.kexec_timeout_ms").and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let nv = NV.load(Ordering::Acquire);
    let cfg = Cfg { enabled, gsp_up: GSP_UP.load(Ordering::Acquire) && !nv.is_null(), timeout_ms };
    let t0 = sched::monotonic_ns();
    let o = kexec_sm::run(&ONCE, &cfg, &mut KHost(nv));
    let ms = (sched::monotonic_ns() - t0) / 1_000_000;
    let b = BDF.0.load(Ordering::Acquire);
    let bdf = (b != u32::MAX).then(|| ((b >> 16) as u8, (b >> 8) as u8, b as u8));
    let msg = match o {
        Outcome::Disabled if cfg.gsp_up => String::from("nvidia: GSP-RM left running (nvidia.kexec_unload=0); the next kernel may not boot GSP"),
        Outcome::Disabled | Outcome::NotRunning => return KexecUnload { lines: Vec::new(), force_reset: None },
        Outcome::AlreadyRan => String::from("nvidia: GSP unload already ran, not repeated"),
        Outcome::Unloaded { wpr2_checked: true } => format!("nvidia: GSP-RM unloaded in {} ms, WPR2 down", ms),
        Outcome::Unloaded { wpr2_checked: false } => format!("nvidia: GSP-RM unloaded in {} ms (WPR2 unreadable)", ms),
        Outcome::WorkerFailed => String::from("nvidia: GSP unload worker could not start; falling back to FLR"),
        Outcome::Timeout(p) => format!("nvidia: GSP unload TIMED OUT after {} ms in {}; falling back to FLR", ms, p.name()),
        Outcome::Wpr2StillUp => format!("nvidia: GSP-RM unload finished in {} ms but WPR2 is still up; falling back to FLR", ms),
    };
    crate::nvlog!("[nvrm] {}\n", msg);
    let force_reset = if o.wants_flr() { bdf } else { None };
    let mut lines = alloc::vec![msg];
    if let Some((b, d, f)) = force_reset {
        lines.push(format!("nvidia: devreset will reset {:02x}:{:02x}.{} even if it owns the boot framebuffer", b, d, f));
    }
    KexecUnload { lines, force_reset }
}
