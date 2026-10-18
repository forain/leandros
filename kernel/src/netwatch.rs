//! `netwatch=<seconds>` (default off): reboot into the fallback boot entry
//! when no network interface has an IPv4 address by the deadline.
//!
//! Purpose: a one-shot trial kernel (armed by espctl; the next boot falls back
//! to the good entry) that breaks the only network path of a remote machine
//! must take the machine back to the good kernel by itself.
//!
//! `netwatch.loss=<seconds>` (also default off): after an address was seen at
//! least once, reboot when every non-loopback interface has been without an
//! address for that long, continuously.
//!
//! The reboot is the reboot(2) restart path (filesystems synced, klog flushed
//! to the log partition, device quiesce), so the line below is in the previous
//! boot's log on the next boot (`espctl klog`, `remote.py logs`).

use crate::{boot_cmdline, serial_print_str};

fn knob(name: &str) -> Option<u64> {
    boot_cmdline()?.split_ascii_whitespace()
        .find_map(|t| t.strip_prefix(name)?.strip_prefix('='))?
        .parse::<u64>().ok().filter(|&v| v > 0)
}

fn has_address() -> bool {
    net_server::any_ipv4_address()
}

fn watch() -> ! {
    let boot_s = knob("netwatch");
    let loss_s = knob("netwatch.loss");
    let mut had = false;
    let mut lost_since: Option<u64> = None;
    loop {
        let now = sched::monotonic_ns();
        let up = has_address();
        if up { had = true; lost_since = None; }
        else if had && lost_since.is_none() { lost_since = Some(now); }
        if !had {
            if let Some(s) = boot_s {
                if now >= s * 1_000_000_000 {
                    fire(&alloc::format!(
                        "[netwatch] no network address after {} s \u{2014} rebooting to the fallback entry\n", s));
                }
            }
        }
        if let (Some(s), Some(t0)) = (loss_s, lost_since) {
            if now.saturating_sub(t0) >= s * 1_000_000_000 {
                fire(&alloc::format!(
                    "[netwatch] network address lost for {} s \u{2014} rebooting to the fallback entry\n", s));
            }
        }
        sched::kthread::sleep_ns(1_000_000_000);
    }
}

fn fire(msg: &str) -> ! {
    serial_print_str(msg);
    crate::power::restart_now()
}

/// Start the watcher thread when a knob is set.
pub fn init() {
    if knob("netwatch").is_none() && knob("netwatch.loss").is_none() { return; }
    serial_print_str("[netwatch] armed\n");
    if sched::spawn(watch, 0).map(|p| sched::set_comm(p, b"netwatch")).is_none() {
        serial_print_str("[netwatch] could not start the watcher task\n");
    }
}
