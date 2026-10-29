//! kapi exports: power management
use super::*;

struct PmHooks { suspend: Option<KPmFn>, resume: Option<KPmFn>, shutdown: Option<KPmFn>, ctx: usize }
static PM_HOOKS: spin::Mutex<Option<PmHooks>> = spin::Mutex::new(None);

/// Register the NIC's power-management hooks (one set; the builtin e1000e is then a stub).
/// 0 ok, -16 already registered.
#[no_mangle]
pub extern "C" fn kapi_pm_register(_owner: KmodId, suspend: Option<KPmFn>, resume: Option<KPmFn>,
                                   shutdown: Option<KPmFn>, ctx: usize) -> i32 {
    let mut g = PM_HOOKS.lock();
    if g.is_some() { return -16; }
    *g = Some(PmHooks { suspend, resume, shutdown, ctx });
    0
}

fn run_pm(f: Option<KPmFn>, ctx: usize) -> Option<(i32, String)> {
    let f = f?;
    let mut buf = [0u8; 256];
    let r = f(ctx, buf.as_mut_ptr(), buf.len());
    let n = (r.unsigned_abs() as usize).min(buf.len());
    Some((r, String::from(String::from_utf8_lossy(&buf[..n]))))
}

fn pm_result(h: Option<PmHooks>, pick: fn(&PmHooks) -> Option<KPmFn>) -> Option<Result<String, String>> {
    let h = h?;
    let (r, m) = run_pm(pick(&h), h.ctx)?;
    Some(if r >= 0 { Ok(m) } else { Err(m) })
}
fn pm_hooks() -> Option<PmHooks> { *PM_HOOKS.lock() }

/// `Some` only when a module registered hooks; pm.rs falls back to the builtin otherwise.
pub fn pm_suspend() -> Option<Result<String, String>> { pm_result(pm_hooks(), |h| h.suspend) }
pub fn pm_resume() -> Option<Result<String, String>> { pm_result(pm_hooks(), |h| h.resume) }
pub fn pm_shutdown() -> Option<String> {
    match pm_result(pm_hooks(), |h| h.shutdown)? { Ok(m) if !m.is_empty() => Some(m), Err(m) => Some(m), _ => None }
}

