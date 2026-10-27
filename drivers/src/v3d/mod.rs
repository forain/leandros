//! Broadcom V3D 7.1 GPU on the Raspberry Pi 5 (BCM2712).
//!
//! Phase A (this file, `regs`, `power`): map the blocks, power the GRAFX_V3D
//! domain, turn the firmware clock on, bring the SMS out of reset, identify
//! the part, and print one `[v3d] ...` line. Phase B (`mmu`, `irq`) builds on
//! the [`V3d`] it leaves in [`DEVICE`]. Design and phase plan:
//! artifacts/notes/lane-piv3d-design-2026-10-26.md.
//!
//! Inert on failure: any failing step logs one `[v3d] fail ...` line, leaves
//! [`DEVICE`] empty and the DRM backend on its stub path. The probe is opt-in:
//! it runs only with `v3d.probe=1` on the kernel command line.
pub mod regs;
pub mod power;
pub mod mmu;
pub mod irq;
pub mod queue;
pub mod submit;
pub mod selftest_cl;

pub use regs::{V3d, V3dRegs};

use core::sync::atomic::{AtomicU32, Ordering};

/// Probe not run (every non-rpi5 build stays here).
pub const PROBE_NONE: u32 = 0;
/// Powered and identified; [`DEVICE`] holds the block.
pub const PROBE_UP: u32 = 1;
/// Skipped: opt-in only (`v3d.probe=1`) until the probe is proven on a Pi 5.
pub const PROBE_SKIPPED: u32 = 2;
/// A region could not be mapped.
pub const PROBE_NOMAP: u32 = 3;
/// Failed: `PROBE_FAILED_BASE + power::Step as u32`.
pub const PROBE_FAILED_BASE: u32 = 0x100;
/// An SError (async external abort) was pending after the probe touched the hardware;
/// the block is treated as absent.
pub const PROBE_SERROR: u32 = 4;

/// `v3d.probe=1` opts in. The default is off: the first Pi 5 boot of the probe
/// (2026-10-26) ended in an SError at the first EL0 entry.
pub fn probe_enabled(cmdline: &str) -> bool {
    cmdline.split_ascii_whitespace().any(|t| t == "v3d.probe=1")
}

/// After-probe SError check, provided by the arch layer: `Some(syndrome)` if an SError
/// was pending (it has been consumed), `None` if clean.
pub type SerrorCheck = fn() -> Option<u64>;

pub fn serror_line(stage: &str, syndrome: u64) -> alloc::string::String {
    alloc::format!("[v3d] fail serror stage={} syn=0x{:016x} (block left unused)\n", stage, syndrome)
}

/// Outcome of the boot probe, for debug readers and later phases.
pub static PROBE_STATE: AtomicU32 = AtomicU32::new(PROBE_NONE);

pub fn probe_state() -> u32 {
    PROBE_STATE.load(Ordering::Relaxed)
}

/// Formats the success line (also used by host tests to pin the format).
pub fn ident_line(r: &power::ProbeReport) -> alloc::string::String {
    let sms = match r.sms_state {
        regs::SMS_STATE_IDLE => alloc::string::String::from("idle"),
        s => alloc::format!("0x{:x}", s),
    };
    alloc::format!(
        "[v3d] ident ver={} rev={} cores={} qpus={} va={} pa={} clk={}MHz sms={}\n",
        r.ver, r.rev, r.cores, r.qpus, r.va_width, r.pa_width, r.clk_hz / 1_000_000, sms
    )
}

/// Formats the one failure line.
pub fn fail_line(e: power::PowerError, r: &power::ProbeReport) -> alloc::string::String {
    alloc::format!(
        "[v3d] fail {} {} val=0x{:08x} grafx=0x{:08x}->0x{:08x} pm=0x{:08x}->0x{:08x} clk={}MHz\n",
        e.step().name(), e.kind(), e.value(), r.grafx_before, r.grafx_after, r.pm_before, r.pm_after, r.clk_hz / 1_000_000
    )
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::{probe_boot, MmioRegs, DEVICE, REPORT};

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::power::{self, PowerEnv, ProbeReport};
    use super::regs::*;
    use super::*;
    use crate::pci::serial_debug;
    use leandros_lib::mmio::{MmioRd, MmioWr};
    use mm::kva::{ioremap, CacheMode};
    use spin::Mutex;

    /// MMIO implementation of [`V3dRegs`] over ioremapped (Device) windows.
    pub struct MmioRegs {
        pub hub: usize,
        pub core: usize,
        pub sms: usize,
    }

    impl V3dRegs for MmioRegs {
        fn hub_r(&self, off: u32) -> u32 { unsafe { ((self.hub + off as usize) as *const u32).mmio_read() } }
        fn hub_w(&self, off: u32, v: u32) { unsafe { ((self.hub + off as usize) as *mut u32).mmio_write(v) } }
        fn core_r(&self, off: u32) -> u32 { unsafe { ((self.core + off as usize) as *const u32).mmio_read() } }
        fn core_w(&self, off: u32, v: u32) { unsafe { ((self.core + off as usize) as *mut u32).mmio_write(v) } }
        fn sms_r(&self, off: u32) -> u32 { unsafe { ((self.sms + off as usize) as *const u32).mmio_read() } }
        fn sms_w(&self, off: u32, v: u32) { unsafe { ((self.sms + off as usize) as *mut u32).mmio_write(v) } }
    }

    /// The powered V3D, once the probe succeeded (phase B takes it from here).
    pub static DEVICE: Mutex<Option<V3d<MmioRegs>>> = Mutex::new(None);
    /// The identity read at probe time (GET_PARAM answers from it later).
    pub static REPORT: Mutex<Option<ProbeReport>> = Mutex::new(None);

    struct Env {
        pm: usize,
        hhdm: usize,
    }

    impl Env {
        /// One single-tag property call; returns the tag's value words.
        fn tag(&mut self, tag: u32, vals: &[u32]) -> Option<[u32; 3]> {
            let mut msg = [0u32; 9];
            let n = vals.len().min(3);
            let words = 5 + n + 1;
            msg[0] = (words * 4) as u32;
            msg[2] = tag;
            msg[3] = (n * 4) as u32;
            msg[5..5 + n].copy_from_slice(&vals[..n]);
            let r = unsafe { crate::rpi_mailbox::property_call(self.hhdm, &msg[..words]) }.ok()?;
            // Tag response bit set by the firmware when it handled the tag.
            if r[4] & 0x8000_0000 == 0 {
                return None;
            }
            let mut out = [0u32; 3];
            out[..n].copy_from_slice(&r[5..5 + n]);
            Some(out)
        }
    }

    impl PowerEnv for Env {
        fn pm_r(&mut self, off: u32) -> u32 { unsafe { ((self.pm + off as usize) as *const u32).mmio_read() } }
        fn pm_w(&mut self, off: u32, v: u32) { unsafe { ((self.pm + off as usize) as *mut u32).mmio_write(v) } }
        fn clk_set_state(&mut self, id: u32, on: bool) -> Option<u32> {
            self.tag(TAG_SET_CLOCK_STATE, &[id, on as u32]).map(|v| v[1])
        }
        fn clk_get_max_rate(&mut self, id: u32) -> Option<u32> {
            self.tag(TAG_GET_MAX_CLOCK_RATE, &[id, 0]).map(|v| v[1])
        }
        fn clk_set_rate(&mut self, id: u32, hz: u32) -> Option<u32> {
            self.tag(TAG_SET_CLOCK_RATE, &[id, hz, 0]).map(|v| v[1])
        }
        fn delay_us(&mut self, us: u32) {
            let end = sched::monotonic_ns() + us as u64 * 1000;
            while sched::monotonic_ns() < end {
                core::hint::spin_loop();
            }
        }
        fn now_us(&mut self) -> u64 { sched::monotonic_ns() / 1000 }
    }

    /// Boot-time probe (task context, after kva is up). Never panics or hangs:
    /// every outcome is one `[v3d]` line and a [`PROBE_STATE`] value.
    pub fn probe_boot(cmdline: &str, hhdm_offset: u64, serror_check: SerrorCheck) {
        if !probe_enabled(cmdline) {
            serial_debug("[v3d] skipped (default off; v3d.probe=1 enables)\n");
            PROBE_STATE.store(PROBE_SKIPPED, Ordering::Relaxed);
            return;
        }
        let map = |phys: u64, len: usize| ioremap(phys, len, CacheMode::Uncached);
        let (hub, core, sms, pm, mbox) = match (
            map(HUB_PHYS, HUB_SIZE),
            map(CORE0_PHYS, CORE0_SIZE),
            map(SMS_PHYS, SMS_SIZE),
            map(PM_PHYS, PM_SIZE),
            map(crate::rpi_mailbox::MBOX_BASE_PHYS as u64, 0x40),
        ) {
            (Some(a), Some(b), Some(c), Some(d), Some(e)) => (a, b, c, d, e),
            _ => {
                serial_debug("[v3d] fail map ioremap\n");
                PROBE_STATE.store(PROBE_NOMAP, Ordering::Relaxed);
                return;
            }
        };
        // The HHDM does not cover the mailbox registers this late in boot.
        crate::rpi_mailbox::set_mbox_virt(mbox);
        let mut env = Env { pm, hhdm: hhdm_offset as usize };
        let t0 = sched::monotonic_ns();
        let res = power::power_on(MmioRegs { hub, core, sms }, &mut env);
        // Bus errors from the accesses above are asynchronous: catch them here,
        // attributed to the probe, instead of at the next EL0 entry.
        if let Some(syn) = serror_check() {
            if let Err((e, rep)) = &res { serial_debug(&fail_line(*e, rep)); }
            serial_debug(&serror_line("power", syn));
            PROBE_STATE.store(PROBE_SERROR, Ordering::Relaxed);
            return;
        }
        match res {
            Ok((dev, rep)) => {
                serial_debug(&ident_line(&rep));
                serial_debug(&alloc::format!(
                    "[v3d] regs hub_ident=0x{:08x},0x{:08x},0x{:08x},0x{:08x} core_ident=0x{:08x},0x{:08x},0x{:08x} mmu_debug=0x{:08x} grafx=0x{:08x}->0x{:08x} pm=0x{:08x}->0x{:08x} t={}us\n",
                    rep.hub_ident0, rep.hub_ident1, rep.hub_ident2, rep.hub_ident3,
                    rep.core_ident0, rep.core_ident1, rep.core_ident2, rep.mmu_debug,
                    rep.grafx_before, rep.grafx_after, rep.pm_before, rep.pm_after, (sched::monotonic_ns() - t0) / 1000
                ));
                *REPORT.lock() = Some(rep);
                // Phase B (pv3d-mmu): MMU + IRQ lines, only after a successful probe. The
                // register block is copied into a leaked 'static so the IRQ handlers and the
                // CREATE_BO path can reach it without taking DEVICE.
                let (ver, va_width) = (dev.ver, dev.va_width);
                let regs: &'static MmioRegs = alloc::boxed::Box::leak(alloc::boxed::Box::new(
                    MmioRegs { hub: dev.regs.hub, core: dev.regs.core, sms: dev.regs.sms }));
                *DEVICE.lock() = Some(dev);
                PROBE_STATE.store(PROBE_UP, Ordering::Relaxed);
                // Phase C (pv3d-cl): the CL queue + boot self-test, only once the MMU is up.
                let mmu_up = super::mmu::after_probe(regs, ver, va_width);
                if mmu_up {
                    super::submit::after_mmu(regs, ver, cmdline);
                }
                // Phase F (pv3d-mesa): the hardware is real and usable, so card0 and
                // renderD128 answer as `v3d` from now on; no client cap or env needed.
                if let Some(syn) = serror_check() {
                    serial_debug(&serror_line("mmu-cl", syn));
                    *DEVICE.lock() = None;
                    PROBE_STATE.store(PROBE_SERROR, Ordering::Relaxed);
                    return;
                }
                if super::submit::auto_arm_decision(mmu_up, cmdline) {
                    crate::drm_device_interface::v3d_arm_hw();
                    serial_debug("[v3d] armed: card0+renderD128 report v3d (auto, probe ok)\n");
                } else {
                    serial_debug(&alloc::format!("[v3d] not armed mmu={} (v3d.arm=0 or MMU down)\n",
                        if mmu_up { "up" } else { "down" }));
                }
            }
            Err((e, rep)) => {
                serial_debug(&fail_line(e, &rep));
                PROBE_STATE.store(PROBE_FAILED_BASE + e.step() as u32, Ordering::Relaxed);
            }
        }
    }
}
