//! CYW43455 power sequencing policy (pure; host-tested with the mock bus).
//!
//! The chip keeps running across a kexec. When the old kernel's brcmf had
//! firmware up (associated), a short WL_REG_ON low pulse (the old 20 ms) did
//! not bring it back to power-on state: the new kernel attached, then the
//! post-download HT clock request timed out. Linux sequences the WL_REG_ON
//! regulator with the mmc-pwrseq style delays (~100 ms off, 150 ms
//! `startup-delay-us` on); a stale chip gets one more, much longer cycle.
//!
//! Both sides of a kexec use this:
//! - old kernel: `brcmf_boot::shutdown` (reboot/kexec quiesce phase 1)
//!   disassociates, WLC_DOWN, then holds WL_REG_ON low across the jump;
//! - new kernel: every probe holds WL_REG_ON low for at least
//!   [`WL_OFF_MIN_US`], and a chip/firmware failure auto-retries once with
//!   [`WL_OFF_RETRY_US`] (so a fixed kernel comes up even when kexec'd from
//!   an unfixed one).

/// Floor for any WL_REG_ON low time (the `wloff_ms=` retry knob included).
pub const WL_OFF_MIN_US: u32 = 100_000;
/// WL_REG_ON low time of the boot probe and of a plain `brcmfctl retry`.
pub const WL_OFF_BOOT_US: u32 = 200_000;
/// WL_REG_ON low time of the one automatic retry after a chip failure.
pub const WL_OFF_RETRY_US: u32 = 1_000_000;
/// Automatic retries per boot (manual `brcmfctl retry` is not counted).
pub const AUTO_RETRIES: u32 = 1;

/// The WL_REG_ON low time actually applied for a requested one.
pub fn off_us(requested_us: u32) -> u32 { requested_us.max(WL_OFF_MIN_US) }

/// WL_REG_ON low time for the next automatic retry after a bring-up failure
/// whose `[brcmf] fail ...` text is `fail`, or None for no retry. Only
/// chip-state failures (attach, firmware download/boot incl. the HT and ALP
/// clock timeouts, a firmware that does not answer clm/ver) are retried: a missing file or a bad knob would fail the
/// same way again.
pub fn auto_retry_off_us(fail: &str, done: u32) -> Option<u32> {
    if done >= AUTO_RETRIES { return None; }
    let hw = ["chip ", "fw ", "clm ", "ver "].iter().any(|p| fail.starts_with(p));
    hw.then_some(WL_OFF_RETRY_US)
}
