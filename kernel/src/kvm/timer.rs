//! Guest timers (kvm-plan section 3 item 5). The guest owns the EL1 virtual timer
//! while it runs (CNTV_*_EL02 under E2H); the physical timer is trapped and kept in
//! software (`El1Regs::cntp_*`). The host moves off PPI 27 (kvm-vhe), so PPI 27 is the
//! guest's: it is enabled at this CPU's redistributor only while the guest runs.
use ::kvm::hw::VcpuHw;

pub const CTL_ENABLE: u64 = 1;
pub const CTL_IMASK: u64 = 2;

macro_rules! rd {
    ($name:literal) => {{
        let v: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)); }
        v
    }};
}
macro_rules! wr {
    ($name:literal, $v:expr) => {
        unsafe { core::arch::asm!(concat!("msr ", $name, ", {}"), in(reg) ($v) as u64, options(nomem, nostack)); }
    };
}

/// Physical count (CNTPCT_EL0 at EL2 is the physical counter).
#[inline] pub fn cntpct() -> u64 { rd!("cntpct_el0") }

fn expired(ctl: u64, cval: u64, now: u64) -> bool { ctl & CTL_ENABLE != 0 && ctl & CTL_IMASK == 0 && now >= cval }

/// Output level of the guest virtual timer.
pub fn vtimer_level(hw: &VcpuHw) -> bool { expired(hw.el1.cntv_ctl, hw.el1.cntv_cval, cntpct().wrapping_sub(hw.cntvoff)) }
/// Output level of the (software) guest physical timer.
pub fn ptimer_level(hw: &VcpuHw) -> bool { expired(hw.el1.cntp_ctl, hw.el1.cntp_cval, cntpct()) }

/// Absolute `monotonic_ns` instant at which a WFI must wake for a timer, or `u64::MAX`.
pub fn wfi_deadline_ns(hw: &VcpuHw) -> u64 {
    let now = cntpct();
    let f = arch_aarch64::timer::freq().max(1);
    let mut best = u64::MAX;
    for (ctl, cval, off) in [(hw.el1.cntv_ctl, hw.el1.cntv_cval, hw.cntvoff), (hw.el1.cntp_ctl, hw.el1.cntp_cval, 0)] {
        if ctl & CTL_ENABLE == 0 || ctl & CTL_IMASK != 0 { continue; }
        let left = cval.saturating_sub(now.wrapping_sub(off));
        let ns = (left as u128 * 1_000_000_000 / f as u128).min(u64::MAX as u128 / 4) as u64;
        best = best.min(sched::monotonic_ns().saturating_add(ns).saturating_add(1000));
    }
    best
}

/// Program the guest virtual timer into hardware. `forced_mask`: the level is already
/// delivered (to userspace or the vGIC), so keep the hardware output masked until the
/// guest touches the timer; PPI 27 is enabled so an expiry during the run exits.
pub fn load(hw: &VcpuHw, forced_mask: bool) {
    wr!("S3_5_C14_C3_2", hw.el1.cntv_cval); // CNTV_CVAL_EL02
    wr!("S3_5_C14_C3_1", hw.el1.cntv_ctl | if forced_mask { CTL_IMASK } else { 0 }); // CNTV_CTL_EL02
    unsafe { core::arch::asm!("isb", options(nomem, nostack)); }
    arch_aarch64::gic::ppi_set_enabled(arch_aarch64::gic::ppi_virt_timer_guest(), true);
}

/// Save the guest virtual timer, then switch the hardware timer off.
pub fn save(hw: &mut VcpuHw, forced_mask: bool) {
    let orig = hw.el1.cntv_ctl;
    let mut ctl = rd!("S3_5_C14_C3_1") & 3; // drop ISTATUS
    if forced_mask && orig & CTL_IMASK == 0 { ctl &= !CTL_IMASK; } // the mask was ours
    hw.el1.cntv_ctl = ctl;
    hw.el1.cntv_cval = rd!("S3_5_C14_C3_2");
    wr!("S3_5_C14_C3_1", 0u64);
    arch_aarch64::gic::ppi_set_enabled(arch_aarch64::gic::ppi_virt_timer_guest(), false);
}
