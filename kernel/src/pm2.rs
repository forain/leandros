//! pm2: the one place that orders every device's suspend / resume / shutdown / kexec hook.
//!
//! Each driver is a row of `BUILTIN` in a phase (`kmod_abi::pm_phase`); modules add rows at run
//! time through `kapi_pm_register2` (`kmod::exports::pm`). Per operation the phases run in a fixed
//! order below, builtin rows first, then module rows in registration order. `pm.rs` (S3 / freeze)
//! and `power.rs` (reboot, kexec) only call the functions here, so a driver that becomes a module
//! changes its own row and nothing else.
use alloc::string::String;
use alloc::vec::Vec;
use kmod_abi::pm_phase as ph;
use crate::kmod::exports::{pm_mods, pm_run, ModPm, PmOp};

pub type Hook = fn(u32) -> Result<String, String>;

/// Result of a phase-1 shutdown hook: a log line and a PCI function whose bus mastering is cleared later.
pub struct ShutOut { pub line: Option<String>, pub bm: Option<drivers::pci::PciFunction> }

pub struct Dev {
    pub name: &'static str,
    pub phase: u32,
    pub suspend: Option<Hook>,
    pub resume: Option<Hook>,
    /// Device quiesce before the log's final flush (arg: hda_crst).
    pub shutdown: Option<fn(bool) -> ShutOut>,
    /// After the filesystems are synced (NVMe CC.SHN).
    pub block_shutdown: Option<fn()>,
    /// kexec only, interrupts off, after the last disk write.
    pub kexec: Option<fn() -> Vec<String>>,
}

const NONE: Dev = Dev { name: "", phase: 0, suspend: None, resume: None, shutdown: None, block_shutdown: None, kexec: None };

/// Suspend order; resume runs it backwards. PCI is last: it runs after every driver quiesced.
const SUSPEND_ORDER: [u32; 7] = [ph::INPUT, ph::GPU, ph::NET, ph::WIFI, ph::BLOCK, ph::USB, ph::PCI];
const SHUTDOWN_ORDER: [u32; 3] = [ph::SOUND, ph::NET, ph::WIFI];
const KEXEC_ORDER: [u32; 5] = [ph::CONSOLE, ph::BLOCK, ph::GPU, ph::NET, ph::WIFI];

fn audio_shutdown(hda_crst: bool) -> ShutOut {
    let (audio, f) = pipewire_server::shutdown(hda_crst);
    ShutOut { line: Some(alloc::format!("audio: {}", audio)), bm: f }
}
fn e1000e_shutdown(_: bool) -> ShutOut { ShutOut { line: drivers::e1000e::shutdown(), bm: None } }
#[cfg(all(target_arch = "aarch64", feature = "rpi5", feature = "drv-brcmf"))]
fn brcmf_shutdown(_: bool) -> ShutOut { ShutOut { line: drivers::brcmf_boot::shutdown(), bm: None } }
fn console_kexec() -> Vec<String> { drivers::gpu_kms::console_restore(); Vec::new() }
fn opt(s: Option<String>) -> Vec<String> { s.into_iter().collect() }
fn r8169_kexec() -> Vec<String> { opt(drivers::r8169::shutdown()) }
fn iwl_kexec() -> Vec<String> { opt(drivers::iwlwifi::kexec_stop()) }

static BUILTIN: &[Dev] = &[
    #[cfg(target_arch = "x86_64")]
    Dev { name: "i8042", phase: ph::INPUT, suspend: Some(crate::pm::ps2_suspend), resume: Some(crate::pm::ps2_resume), ..NONE },
    #[cfg(not(target_arch = "x86_64"))]
    Dev { name: "i8042", phase: ph::INPUT, suspend: Some(crate::pm::none), resume: Some(crate::pm::none), ..NONE },
    Dev { name: "i915", phase: ph::GPU, suspend: Some(crate::pm::i915_suspend), resume: Some(crate::pm::i915_resume),
          kexec: Some(drivers::i915::kexec_quiesce), ..NONE },
    Dev { name: "console", phase: ph::CONSOLE, kexec: Some(console_kexec), ..NONE },
    Dev { name: "audio", phase: ph::SOUND, shutdown: Some(audio_shutdown), ..NONE },
    Dev { name: "e1000e", phase: ph::NET, suspend: Some(crate::pm::e1000e_suspend), resume: Some(crate::pm::e1000e_resume),
          shutdown: Some(e1000e_shutdown), ..NONE },
    Dev { name: "r8169", phase: ph::NET, kexec: Some(r8169_kexec), ..NONE },
    Dev { name: "iwlwifi", phase: ph::WIFI, kexec: Some(iwl_kexec), ..NONE },
    #[cfg(all(target_arch = "aarch64", feature = "rpi5", feature = "drv-brcmf"))]
    Dev { name: "brcmf", phase: ph::WIFI, shutdown: Some(brcmf_shutdown), ..NONE },
    Dev { name: "block", phase: ph::BLOCK, suspend: Some(crate::pm::blk_suspend), resume: Some(crate::pm::blk_resume),
          block_shutdown: Some(drivers::nvme::shutdown_all), kexec: Some(drivers::nvme::kexec_disable), ..NONE },
    Dev { name: "xhci", phase: ph::USB, suspend: Some(crate::pm::xhci_suspend), resume: Some(crate::pm::xhci_resume), ..NONE },
    Dev { name: "pci", phase: ph::PCI, suspend: Some(crate::pm::pci_suspend), resume: Some(crate::pm::pci_resume), ..NONE },
];

/// One suspendable device in the order it ran.
pub struct Step { pub name: String, dev: Option<&'static Dev>, module: Option<ModPm> }

impl Step {
    pub fn suspend(&self, flags: u32) -> Result<String, String> {
        if let Some(d) = self.dev { return d.suspend.map_or(Ok(String::from("no hook")), |f| f(flags)); }
        self.module.as_ref().and_then(|m| pm_run(m, PmOp::Suspend)).unwrap_or_else(|| Ok(String::from("no hook")))
    }
    pub fn resume(&self, flags: u32) -> Result<String, String> {
        if let Some(d) = self.dev { return d.resume.map_or(Ok(String::from("no hook")), |f| f(flags)); }
        self.module.as_ref().and_then(|m| pm_run(m, PmOp::Resume)).unwrap_or_else(|| Ok(String::from("no hook")))
    }
}

/// Every device with a suspend or resume hook, in suspend order (resume runs it backwards).
pub fn suspend_steps() -> Vec<Step> {
    let mut v = Vec::new();
    for &p in SUSPEND_ORDER.iter() {
        for d in BUILTIN.iter().filter(|d| d.phase == p && (d.suspend.is_some() || d.resume.is_some())) {
            v.push(Step { name: String::from(d.name), dev: Some(d), module: None });
        }
        for m in pm_mods(p).into_iter().filter(|m| m.ops.suspend.is_some() || m.ops.resume.is_some()) {
            let n = crate::kmod::registry::name_of(m.owner);
            v.push(Step { name: String::from(crate::kmod::name_str(&n)), dev: None, module: Some(m) });
        }
    }
    v
}

fn module_line(m: &ModPm, op: PmOp, out: &mut Vec<String>) {
    match pm_run(m, op) {
        Some(Ok(s)) if !s.is_empty() => out.push(s),
        Some(Err(s)) => out.push(s),
        _ => {}
    }
}

/// Phase 1 of the reboot quiesce: SOUND, NET, WIFI. Returns the log lines and the PCI functions
/// whose bus mastering phase 2 clears.
pub fn shutdown_phase1(hda_crst: bool) -> (Vec<String>, Vec<drivers::pci::PciFunction>) {
    let (mut lines, mut bm) = (Vec::new(), Vec::new());
    for &p in SHUTDOWN_ORDER.iter() {
        for d in BUILTIN.iter().filter(|d| d.phase == p) {
            if let Some(f) = d.shutdown {
                let o = f(hda_crst);
                if let Some(l) = o.line { lines.push(l); }
                if let Some(b) = o.bm { bm.push(b); }
            }
        }
        for m in pm_mods(p) { module_line(&m, PmOp::Shutdown, &mut lines); }
    }
    (lines, bm)
}

/// After the filesystems are synced: the block phase (NVMe CC.SHN, and module block shutdown).
pub fn block_shutdown() -> Vec<String> {
    let mut lines = Vec::new();
    for d in BUILTIN.iter().filter(|d| d.phase == ph::BLOCK) { if let Some(f) = d.block_shutdown { f(); } }
    for m in pm_mods(ph::BLOCK) { module_line(&m, PmOp::Shutdown, &mut lines); }
    lines
}

/// kexec only: CONSOLE, BLOCK, GPU, NET, WIFI.
pub fn kexec_quiesce() -> Vec<String> {
    let mut lines = Vec::new();
    for &p in KEXEC_ORDER.iter() {
        for d in BUILTIN.iter().filter(|d| d.phase == p) { if let Some(f) = d.kexec { lines.extend(f()); } }
        for m in pm_mods(p) { module_line(&m, PmOp::KexecQuiesce, &mut lines); }
    }
    lines
}
