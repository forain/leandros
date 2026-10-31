//! Pi 5 tryboot deadline. Booted with `leandros.tryboot=1` (the firmware read
//! `tryboot.txt`, whose `cmdline_try.txt` carries the token), a kthread reboots
//! plainly after `tryboot.deadline=<seconds>` (default 900) unless the trial is
//! confirmed with `reboot(LEANDROS_REBOOT_CMD_TRYBOOT_CONFIRM)` (`reboot
//! tryboot-commit`). A plain reboot consumes no tryboot flag, so the firmware
//! then reads `config.txt` and boots the good slot. Only the token is checked;
//! the command line is never printed.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::{boot_cmdline, serial_print_str};

const S: u64 = 1_000_000_000;
const DEFAULT_DEADLINE_S: u64 = 900;

static CONFIRMED: AtomicBool = AtomicBool::new(false);
/// This boot carries the `leandros.tryboot=1` token.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// A tryboot reset was armed (RESTART2 "tryboot"); do not clear the flag.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Lease end (monotonic ns) of a commit in progress; 0 = not held.
static HOLD_UNTIL: AtomicU64 = AtomicU64::new(0);
const HOLD_LEASE_S: u64 = 300;

pub fn note_armed() { ARMED.store(true, Ordering::Release); }

/// Called by every plain restart: in a trial boot, clear the one-shot flag.
pub fn before_plain_reset() {
    if ACTIVE.load(Ordering::Acquire) && !ARMED.load(Ordering::Acquire) {
        crate::power::clear_reboot_flag();
    }
}

/// A commit is writing the boot FAT: the deadline waits (up to the lease).
pub fn hold(on: bool) {
    HOLD_UNTIL.store(if on { sched::monotonic_ns() + HOLD_LEASE_S * S } else { 0 }, Ordering::Release);
    serial_print_str(if on { "[tryboot] hold: commit in progress\n" } else { "[tryboot] hold released\n" });
}

/// Cancel the deadline for this boot.
pub fn confirm() {
    if !CONFIRMED.swap(true, Ordering::AcqRel) {
        serial_print_str("[tryboot] confirmed: deadline disarmed\n");
    }
}

fn watch() -> ! {
    let secs = boot_cmdline()
        .and_then(|c| c.split_ascii_whitespace().find_map(|t| t.strip_prefix("tryboot.deadline=")))
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_DEADLINE_S);
    loop {
        if CONFIRMED.load(Ordering::Acquire) {
            loop { sched::kthread::sleep_ns(3600 * S); }
        }
        let now = sched::monotonic_ns();
        if now >= secs * S && now >= HOLD_UNTIL.load(Ordering::Acquire) {
            serial_print_str("[tryboot] deadline expired without confirm: plain reboot to the good slot\n");
            crate::power::restart_now(); // clears the flag first (before_plain_reset)
        }
        sched::kthread::sleep_ns(S);
    }
}

pub fn init() {
    let on = boot_cmdline().map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "leandros.tryboot=1"));
    if !on { return; }
    ACTIVE.store(true, Ordering::Release);
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    drivers::rpi_misc::set_trial_boot(true);
    serial_print_str("[tryboot] trial boot: deadline armed\n");
    if sched::spawn(watch, 0).map(|p| sched::set_comm(p, b"tryboot")).is_none() {
        serial_print_str("[tryboot] could not start the deadline task\n");
    }
}
