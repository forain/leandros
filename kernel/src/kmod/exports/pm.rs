//! kapi exports: power management. Every registration is a per-owner record (`ModPm`) in one
//! list, noted in the registry as `RegKind::PM`, and removed again if the module's init fails.
//! Ordering across phases lives in `crate::pm2`; this file only stores and runs module hooks.
use super::*;
use alloc::string::String;
use kmod_abi::{pm_phase, KPmFn, KPmOps};

#[derive(Clone, Copy)]
pub struct ModPm { pub owner: KmodId, pub ops: KPmOps, pub ctx: usize }

pub(super) static PM_LIST: spin::Mutex<alloc::vec::Vec<ModPm>> = spin::Mutex::new(alloc::vec::Vec::new());

/// Which hook of a `KPmOps` to run.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PmOp { Suspend, Resume, Shutdown, KexecQuiesce }

pub(super) fn pick(o: &KPmOps, op: PmOp) -> Option<KPmFn> {
    match op { PmOp::Suspend => o.suspend, PmOp::Resume => o.resume, PmOp::Shutdown => o.shutdown,
               PmOp::KexecQuiesce => o.kexec_quiesce }
}

/// Register a hook table in `ops.phase`. 0 ok, -22 bad table. Several modules may register.
#[no_mangle]
pub extern "C" fn kapi_pm_register2(owner: KmodId, ops: *const KPmOps, ctx: usize) -> i32 {
    if ops.is_null() { return -22; }
    let o = unsafe { *ops };
    if (o.size as usize) < core::mem::size_of::<KPmOps>() || o.phase == 0 || o.phase > pm_phase::MAX { return -22; }
    PM_LIST.lock().push(ModPm { owner, ops: o, ctx });
    crate::kmod::registry::note_registration(owner, RegKind::PM, ctx as u64);
    0
}

/// Old single-set registration: the NET phase, any number of callers (it used to be one global).
#[no_mangle]
pub extern "C" fn kapi_pm_register(owner: KmodId, suspend: Option<KPmFn>, resume: Option<KPmFn>,
                                   shutdown: Option<KPmFn>, ctx: usize) -> i32 {
    let o = KPmOps { size: core::mem::size_of::<KPmOps>() as u32, phase: pm_phase::NET, suspend, resume,
                     freeze: None, thaw: None, shutdown, kexec_quiesce: None, resume_check: None };
    kapi_pm_register2(owner, &o, ctx)
}

/// Failed init: drop every hook table of `owner`.
pub fn pm_unregister_owner(owner: KmodId) { PM_LIST.lock().retain(|m| m.owner != owner); }

/// Unload step 0 (before `quiesce_pci`): run the module's own `shutdown` hooks while bus mastering
/// is still on, so a driver can stop its DMA engine in order (RX/TX off, ring flush, MAC reset).
/// Returns how many hooks ran.
pub fn pm_unload_quiesce(owner: KmodId) -> usize {
    let mods: alloc::vec::Vec<ModPm> = PM_LIST.lock().iter().filter(|m| m.owner == owner).copied().collect();
    let mut n = 0;
    for m in &mods {
        if let Some(r) = pm_run(m, PmOp::Shutdown) {
            n += 1;
            let line = match r { Ok(s) | Err(s) => s };
            if !line.is_empty() { drivers::pci::serial_debug(&alloc::format!("[kmod] unload quiesce: {}\n", line)); }
        }
    }
    n
}

/// Number of hook tables registered by `owner` (ktest).
pub fn pm_count(owner: KmodId) -> usize { PM_LIST.lock().iter().filter(|m| m.owner == owner).count() }

/// Snapshot of the module hook tables registered in `phase`, registration order.
pub fn pm_mods(phase: u32) -> alloc::vec::Vec<ModPm> {
    PM_LIST.lock().iter().filter(|m| m.ops.phase == phase).copied().collect()
}

pub(super) fn run_pm(f: Option<KPmFn>, ctx: usize) -> Option<(i32, String)> {
    let f = f?;
    let mut buf = [0u8; 256];
    let r = f(ctx, buf.as_mut_ptr(), buf.len());
    let n = (r.unsigned_abs() as usize).min(buf.len());
    Some((r, String::from(String::from_utf8_lossy(&buf[..n]))))
}

/// Run `op` of one module hook table. `None` when the table has no such hook.
pub fn pm_run(m: &ModPm, op: PmOp) -> Option<Result<String, String>> {
    let (r, msg) = run_pm(pick(&m.ops, op), m.ctx)?;
    Some(if r >= 0 { Ok(msg) } else { Err(msg) })
}
