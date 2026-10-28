//! Panel backlight through the SPT/KBL PCH PWM (BLC_PWM_PCH_CTL1/CTL2):
//! CTL2 = period << 16 | duty, CTL1 bit 31 = PWM enabled. Only answers once
//! a bound i915 has mapped its registers (and not in fake mode).

use super::hw::reg;
use super::{kms, Mmio};

fn mmio() -> Option<Mmio> {
    let m = kms::mmio();
    if m.va == 0 || kms::FAKE.load(core::sync::atomic::Ordering::Relaxed) { return None; }
    Some(m)
}

/// `(duty, period)` of the PCH backlight PWM, if it is enabled with a period.
pub fn get() -> Option<(u32, u32)> {
    let m = mmio()?;
    if m.r(reg::BLC_PWM_PCH_CTL1) >> 31 == 0 { return None; }
    let c2 = m.r(reg::BLC_PWM_PCH_CTL2);
    let period = c2 >> 16;
    if period == 0 { return None; }
    Some((c2 & 0xFFFF, period))
}

/// Set the PWM duty (clamped to the period); false when no backlight is present.
pub fn set(duty: u32) -> bool {
    let Some(m) = mmio() else { return false };
    let Some((_, period)) = get() else { return false };
    let d = duty.min(period);
    m.w(reg::BLC_PWM_PCH_CTL2, (period << 16) | d);
    true
}
