//! V3D 7.1 power-up and identification (phase A, lane pv3d-power).
//!
//! The sequence Linux runs on a Pi 5, in order:
//! 1. PM domain GRAFX_V3D: `PM[0x304] = PASSWORD | (PM[0x304] | V3DRSTN)` with
//!    1 us settle on either side (bcm2835-power `asb_power_on`, no ASB on 2712).
//! 2. Firmware clock 5: SET_CLOCK_STATE on, GET_MAX_CLOCK_RATE, SET_CLOCK_RATE max.
//! 3. SMS: `TEE_CS = CLEAR_POWER_OFF`, poll state IDLE; `REE_CS = state 4`
//!    (reset), poll until neither ISOLATING_FOR_RESET nor RESETTING.
//! 4. `L2TFLSTA = 0, L2TFLEND = ~0`.
//! 5. Identity: CTL_IDENT0 magic, HUB_IDENT1 ver/cores, HUB_IDENT3 rev,
//!    CTL_IDENT1 QPUs, MMU_DEBUG_INFO VA/PA widths.
//!
//! No V3D register is touched before steps 1 and 2 succeed (an unclocked block
//! may abort the access). Every poll is bounded at [`POLL_TIMEOUT_US`] by the
//! environment clock and by an iteration cap, so a frozen clock still ends.
//! Any failure returns a [`PowerError`] and the caller stays inert.

use super::regs::*;

/// Everything outside the V3D register blocks the power-up needs.
pub trait PowerEnv {
    fn pm_r(&mut self, off: u32) -> u32;
    fn pm_w(&mut self, off: u32, v: u32);
    /// SET_CLOCK_STATE; returns the firmware's reply state word.
    fn clk_set_state(&mut self, id: u32, on: bool) -> Option<u32>;
    /// GET_MAX_CLOCK_RATE in Hz.
    fn clk_get_max_rate(&mut self, id: u32) -> Option<u32>;
    /// SET_CLOCK_RATE; returns the rate the firmware actually set, in Hz.
    fn clk_set_rate(&mut self, id: u32, hz: u32) -> Option<u32>;
    fn delay_us(&mut self, us: u32);
    fn now_us(&mut self) -> u64;
}

pub const POLL_TIMEOUT_US: u64 = 100_000;
/// Hard cap on poll iterations (each sleeps 1 us), independent of the clock.
pub const POLL_MAX_ITERS: u32 = 200_000;
/// The only version this driver supports.
pub const SUPPORTED_VER: u32 = 71;

/// SET_CLOCK_STATE reply bits.
pub const CLK_STATE_ON: u32 = bit(0);
pub const CLK_STATE_NOT_EXIST: u32 = bit(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The PM write did not take (V3DRSTN still clear on readback).
    Pm,
    ClkState,
    ClkRate,
    SmsPowerOn,
    SmsReset,
    /// CTL_IDENT0 does not spell "V3D" (block unclocked or not there).
    Ident,
    Version,
    Cores,
    /// GRAFX island power-up (PM_GRAFX POWUP/POWOK, memory repair).
    Grafx,
    /// Re-read of PM right before the first V3D access did not show the island on.
    PmConfirm,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::Pm => "pm",
            Step::ClkState => "clk-state",
            Step::ClkRate => "clk-rate",
            Step::SmsPowerOn => "sms-power",
            Step::SmsReset => "sms-reset",
            Step::Ident => "ident",
            Step::Version => "ver",
            Step::Cores => "cores",
            Step::Grafx => "grafx",
            Step::PmConfirm => "pm-confirm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerError {
    /// A bounded poll expired; `last` is the last register value read.
    Timeout { step: Step, last: u32 },
    /// The firmware refused a clock tag (no reply or a bad reply).
    Firmware { step: Step, value: u32 },
    /// A value read back is not what a BCM2712 V3D 7.1 reports.
    Mismatch { step: Step, value: u32 },
}

impl PowerError {
    pub fn step(self) -> Step {
        match self {
            PowerError::Timeout { step, .. } | PowerError::Firmware { step, .. } | PowerError::Mismatch { step, .. } => step,
        }
    }
    pub fn value(self) -> u32 {
        match self {
            PowerError::Timeout { last, .. } => last,
            PowerError::Firmware { value, .. } | PowerError::Mismatch { value, .. } => value,
        }
    }
    pub fn kind(self) -> &'static str {
        match self {
            PowerError::Timeout { .. } => "timeout",
            PowerError::Firmware { .. } => "firmware",
            PowerError::Mismatch { .. } => "mismatch",
        }
    }
}

/// What the probe learned, for the `[v3d] ident ...` line and GET_PARAM later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeReport {
    pub grafx_before: u32,
    pub grafx_after: u32,
    pub pm_before: u32,
    pub pm_after: u32,
    pub clk_hz: u32,
    pub hub_ident0: u32,
    pub hub_ident1: u32,
    pub hub_ident2: u32,
    pub hub_ident3: u32,
    pub core_ident0: u32,
    pub core_ident1: u32,
    pub core_ident2: u32,
    pub mmu_debug: u32,
    pub ver: u32,
    pub rev: u32,
    pub cores: u32,
    pub qpus: u32,
    pub va_width: u32,
    pub pa_width: u32,
    /// SMS_REE_CS state after the reset (0 = idle).
    pub sms_state: u32,
}

/// Polls `read` until `done(value)`, bounded by time and iterations.
fn poll<E: PowerEnv>(env: &mut E, step: Step, mut read: impl FnMut() -> u32, done: impl Fn(u32) -> bool) -> Result<u32, PowerError> {
    let start = env.now_us();
    let mut last = 0;
    for _ in 0..POLL_MAX_ITERS {
        last = read();
        if done(last) {
            return Ok(last);
        }
        if env.now_us().wrapping_sub(start) >= POLL_TIMEOUT_US {
            break;
        }
        env.delay_us(1);
    }
    Err(PowerError::Timeout { step, last })
}

/// Step 1: release GRAFX_V3D from reset.
/// Per-inrush-step POWOK wait (Linux: 3 us) and the memory-repair wait (Linux: 1 us),
/// both widened for a slow `now_us`.
pub const POWOK_TIMEOUT_US: u64 = 20;
pub const MRDONE_TIMEOUT_US: u64 = 200;

fn pm_set<E: PowerEnv>(env: &mut E, off: u32, set: u32, clear: u32) {
    let v = env.pm_r(off) & 0x00ff_ffff;
    env.pm_w(off, PM_PASSWORD | ((v & !clear) | set));
}

/// GRAFX island power-up, Linux `bcm2835_power_power_on(PM_GRAFX)` (the parent domain of
/// GRAFX_V3D): left alone if the firmware already set POWUP; otherwise POWUP with the
/// inrush current ramped 3.5 -> 20 mA until POWOK, then ISPOW, MEMREP/MRDONE, ISFUNC.
/// Every failure undoes what it set, as Linux does.
pub fn grafx_power_on<E: PowerEnv>(env: &mut E, rep: &mut ProbeReport) -> Result<(), PowerError> {
    let before = env.pm_r(PM_GRAFX);
    rep.grafx_before = before;
    rep.grafx_after = before;
    if before & PM_POWUP != 0 {
        return Ok(());
    }
    let mut powok = Err(before);
    for inrush in 0..=PM_INRUSH_MAX {
        pm_set(env, PM_GRAFX, (inrush << PM_INRUSH_SHIFT) | PM_POWUP, PM_INRUSH_MASK);
        powok = poll_pm(env, PM_GRAFX, PM_POWOK, POWOK_TIMEOUT_US);
        if powok.is_ok() {
            break;
        }
    }
    if let Err(last) = powok {
        pm_set(env, PM_GRAFX, 0, PM_POWUP | PM_INRUSH_MASK);
        rep.grafx_after = env.pm_r(PM_GRAFX);
        return Err(PowerError::Timeout { step: Step::Grafx, last });
    }
    pm_set(env, PM_GRAFX, PM_ISPOW, 0);
    pm_set(env, PM_GRAFX, PM_MEMREP, 0);
    if let Err(last) = poll_pm(env, PM_GRAFX, PM_MRDONE, MRDONE_TIMEOUT_US) {
        pm_set(env, PM_GRAFX, 0, PM_ISPOW);
        pm_set(env, PM_GRAFX, 0, PM_POWUP | PM_INRUSH_MASK);
        rep.grafx_after = env.pm_r(PM_GRAFX);
        return Err(PowerError::Timeout { step: Step::Grafx, last });
    }
    pm_set(env, PM_GRAFX, PM_ISFUNC, 0);
    rep.grafx_after = env.pm_r(PM_GRAFX);
    Ok(())
}

/// Polls a PM register for `mask`, bounded by `timeout_us` of clock and of 1 us waits.
fn poll_pm<E: PowerEnv>(env: &mut E, off: u32, mask: u32, timeout_us: u64) -> Result<u32, u32> {
    let start = env.now_us();
    let mut last = 0;
    // Each miss waits 1 us, so `timeout_us + 1` reads also bound a frozen clock.
    for _ in 0..=timeout_us {
        last = env.pm_r(off);
        if last & mask != 0 {
            return Ok(last);
        }
        if env.now_us().wrapping_sub(start) >= timeout_us {
            break;
        }
        env.delay_us(1);
    }
    Err(last)
}

/// The gate in front of the first V3D access: re-read both PM registers after the
/// clock is on and require the island fully on (POWUP, POWOK, ISPOW, ISFUNC) and
/// V3D out of reset. Touching SMS/hub/core of an unpowered island is an external
/// abort (an SError at the next EL0 entry on the Pi 5), so this fails cleanly instead.
pub fn pm_confirm<E: PowerEnv>(env: &mut E, rep: &mut ProbeReport) -> Result<(), PowerError> {
    let g = env.pm_r(PM_GRAFX);
    let v = env.pm_r(PM_GRAFX_2712);
    rep.grafx_after = g;
    rep.pm_after = v;
    if g & PM_GRAFX_ON != PM_GRAFX_ON {
        return Err(PowerError::Mismatch { step: Step::PmConfirm, value: g });
    }
    if v & PM_V3DRSTN == 0 {
        return Err(PowerError::Mismatch { step: Step::PmConfirm, value: v });
    }
    Ok(())
}

pub fn pm_power_on<E: PowerEnv>(env: &mut E, rep: &mut ProbeReport) -> Result<(), PowerError> {
    let before = env.pm_r(PM_GRAFX_2712);
    rep.pm_before = before;
    // asb_power_on: (domain clock off) wait 32 clocks, deassert, (clock on).
    env.delay_us(1);
    env.pm_w(PM_GRAFX_2712, PM_PASSWORD | ((before & !0xff00_0000) | PM_V3DRSTN));
    env.delay_us(1);
    let after = env.pm_r(PM_GRAFX_2712);
    rep.pm_after = after;
    if after & PM_V3DRSTN == 0 {
        return Err(PowerError::Mismatch { step: Step::Pm, value: after });
    }
    Ok(())
}

/// Step 2: firmware clock on at its maximum rate.
pub fn clock_on<E: PowerEnv>(env: &mut E, rep: &mut ProbeReport) -> Result<(), PowerError> {
    let st = env.clk_set_state(FW_CLK_V3D, true).ok_or(PowerError::Firmware { step: Step::ClkState, value: 0 })?;
    if st & CLK_STATE_NOT_EXIST != 0 || st & CLK_STATE_ON == 0 {
        return Err(PowerError::Firmware { step: Step::ClkState, value: st });
    }
    let max = env.clk_get_max_rate(FW_CLK_V3D).ok_or(PowerError::Firmware { step: Step::ClkRate, value: 0 })?;
    if max == 0 {
        return Err(PowerError::Firmware { step: Step::ClkRate, value: 0 });
    }
    let hz = env.clk_set_rate(FW_CLK_V3D, max).ok_or(PowerError::Firmware { step: Step::ClkRate, value: max })?;
    if hz == 0 {
        return Err(PowerError::Firmware { step: Step::ClkRate, value: max });
    }
    rep.clk_hz = hz;
    Ok(())
}

/// Step 3a: SMS power on (v3d_resume_sms).
pub fn sms_power_on<R: V3dRegs, E: PowerEnv>(regs: &R, env: &mut E) -> Result<(), PowerError> {
    regs.sms_w(SMS_TEE_CS, SMS_CLEAR_POWER_OFF);
    poll(env, Step::SmsPowerOn, || regs.sms_r(SMS_TEE_CS), |v| sms_state(v) == SMS_STATE_IDLE).map(|_| ())
}

/// Step 3b: SMS reset (v3d_reset_sms). Returns the final REE_CS value.
pub fn sms_reset<R: V3dRegs, E: PowerEnv>(regs: &R, env: &mut E) -> Result<u32, PowerError> {
    regs.sms_w(SMS_REE_CS, SMS_STATE_RESET_REQ);
    poll(env, Step::SmsReset, || regs.sms_r(SMS_REE_CS), |v| {
        let s = sms_state(v);
        s != SMS_STATE_ISOLATING_FOR_RESET && s != SMS_STATE_RESETTING
    })
}

/// Steps 4 and 5 on a powered, clocked, reset block.
pub fn identify<R: V3dRegs>(regs: &R, rep: &mut ProbeReport) -> Result<(), PowerError> {
    rep.core_ident0 = regs.core_r(CTL_IDENT0);
    if rep.core_ident0 & 0x00ff_ffff != CTL_IDENT0_MAGIC {
        return Err(PowerError::Mismatch { step: Step::Ident, value: rep.core_ident0 });
    }
    regs.core_w(CTL_L2TFLSTA, 0);
    regs.core_w(CTL_L2TFLEND, !0);
    rep.mmu_debug = regs.hub_r(MMU_DEBUG_INFO);
    rep.va_width = mmu_va_width(rep.mmu_debug);
    rep.pa_width = mmu_pa_width(rep.mmu_debug);
    rep.hub_ident0 = regs.hub_r(HUB_IDENT0);
    rep.hub_ident1 = regs.hub_r(HUB_IDENT1);
    rep.hub_ident2 = regs.hub_r(HUB_IDENT2);
    rep.hub_ident3 = regs.hub_r(HUB_IDENT3);
    rep.core_ident1 = regs.core_r(CTL_IDENT1);
    rep.core_ident2 = regs.core_r(CTL_IDENT2);
    rep.ver = hub_ident1_tver(rep.hub_ident1) * 10 + hub_ident1_rev(rep.hub_ident1);
    rep.cores = hub_ident1_ncores(rep.hub_ident1);
    rep.rev = hub_ident3_iprev(rep.hub_ident3);
    rep.qpus = ctl_ident1_qpus(rep.core_ident1);
    if rep.ver != SUPPORTED_VER {
        return Err(PowerError::Mismatch { step: Step::Version, value: rep.hub_ident1 });
    }
    if rep.cores != 1 {
        return Err(PowerError::Mismatch { step: Step::Cores, value: rep.hub_ident1 });
    }
    Ok(())
}

/// The whole phase-A bring-up. On success the block is powered, clocked,
/// reset and identified; on failure nothing past the failing step ran.
pub fn power_on<R: V3dRegs, E: PowerEnv>(regs: R, env: &mut E) -> Result<(V3d<R>, ProbeReport), (PowerError, ProbeReport)> {
    let mut rep = ProbeReport::default();
    if let Err(e) = grafx_power_on(env, &mut rep) { return Err((e, rep)); }
    if let Err(e) = pm_power_on(env, &mut rep) { return Err((e, rep)); }
    if let Err(e) = clock_on(env, &mut rep) { return Err((e, rep)); }
    if let Err(e) = pm_confirm(env, &mut rep) { return Err((e, rep)); }
    if let Err(e) = sms_power_on(&regs, env) { return Err((e, rep)); }
    match sms_reset(&regs, env) {
        Ok(v) => rep.sms_state = sms_state(v),
        Err(e) => return Err((e, rep)),
    }
    if let Err(e) = identify(&regs, &mut rep) { return Err((e, rep)); }
    let (ver, va_width) = (rep.ver, rep.va_width);
    Ok((V3d { regs, ver, va_width }, rep))
}
