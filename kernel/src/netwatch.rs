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
//!
//! Trial watch (only when booted as `leandros.slot=trial`, i.e. the one-shot
//! entry espctl arms, and some `netwatch` knob is set): keep checking that the
//! data path works for the whole trial, not just at boot. Every
//! `netwatch.every=` (10 s) an ICMP echo goes to `netwatch.host=a.b.c.d`
//! (default: the default gateway, which must answer ARP first). When no reply
//! came for `netwatch.reach=` (300 s; 0 = off) AND the last `netwatch.fails=`
//! (3) probes all failed, it reboots: the trial already reached init, so
//! `espctl boot` made the fallback (good) entry the default and the reboot
//! lands there. A Wi-Fi blip shorter than `reach` never fires.
//! `netwatch.trial_max=` (minutes, default off) reboots an un-promoted trial
//! after that long regardless. Durations take an `s`/`m`/`h` suffix.
//!
//! `espctl promote` disarms everything for the rest of the boot
//! (`NETCTL_NETWATCH_DISARM`), so a promoted kernel never reboots itself.

use crate::{boot_cmdline, serial_print_str};

fn knob(name: &str) -> Option<u64> {
    boot_cmdline()?.split_ascii_whitespace()
        .find_map(|t| t.strip_prefix(name)?.strip_prefix('='))?
        .parse::<u64>().ok().filter(|&v| v > 0)
}

fn has_address() -> bool {
    net_server::any_ipv4_address()
}

fn raw(name: &str) -> Option<&'static str> {
    boot_cmdline()?.split_ascii_whitespace().find_map(|t| t.strip_prefix(name)?.strip_prefix('='))
}

/// A duration knob in seconds; a bare number is in `unit` seconds. `Some(0)` = off.
fn dur(name: &str, unit: u64) -> Option<u64> {
    let v = raw(name)?;
    let (n, u) = match v.as_bytes().last()? {
        b's' => (&v[..v.len() - 1], 1),
        b'm' => (&v[..v.len() - 1], 60),
        b'h' => (&v[..v.len() - 1], 3600),
        _ => (v, unit),
    };
    n.parse::<u64>().ok().map(|x| x.saturating_mul(u))
}

fn ipv4(v: &str) -> Option<[u8; 4]> {
    let mut o = [0u8; 4];
    let mut it = v.split('.');
    for b in o.iter_mut() { *b = it.next()?.parse().ok()?; }
    if it.next().is_some() { return None; }
    Some(o)
}

fn is_trial() -> bool { raw("leandros.slot") == Some("trial") }

fn any_knob() -> bool {
    boot_cmdline().map_or(false, |c| c.split_ascii_whitespace().any(|t| t.starts_with("netwatch=") || t.starts_with("netwatch.")))
}

struct Trial { reach_s: u64, every_s: u64, fails: u32, host: Option<[u8; 4]>, max_s: u64 }

fn trial_cfg() -> Option<Trial> {
    if !is_trial() || !any_knob() { return None; }
    let t = Trial {
        reach_s: dur("netwatch.reach", 1).unwrap_or(300),
        every_s: dur("netwatch.every", 1).filter(|&v| v > 0).unwrap_or(10),
        fails: raw("netwatch.fails").and_then(|v| v.parse().ok()).filter(|&v| v > 0).unwrap_or(3),
        host: raw("netwatch.host").and_then(ipv4),
        max_s: dur("netwatch.trial_max", 60).unwrap_or(0),
    };
    if t.reach_s == 0 && t.max_s == 0 { None } else { Some(t) }
}

fn ip_str(a: Option<[u8; 4]>) -> alloc::string::String {
    match a { Some(a) => alloc::format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]), None => "none".into() }
}

const S: u64 = 1_000_000_000;

fn watch() -> ! {
    let boot_s = knob("netwatch");
    let loss_s = knob("netwatch.loss");
    let trial = trial_cfg();
    let mut had = false;
    let mut lost_since: Option<u64> = None;
    let start = sched::monotonic_ns();
    let mut last_ok = start;
    let mut next_probe = start;
    let mut failed: u32 = 0;
    let mut reach_lost = false;
    loop {
        if net_server::netwatch::disarmed() {
            serial_print_str("[netwatch] disarmed (promoted): the watchdog is off for this boot\n");
            loop { sched::kthread::sleep_ns(3600 * S); }
        }
        let now = sched::monotonic_ns();
        let up = has_address();
        if up { had = true; lost_since = None; }
        else if had && lost_since.is_none() { lost_since = Some(now); }
        if !had {
            if let Some(s) = boot_s {
                if now >= s * S {
                    fire(&alloc::format!(
                        "[netwatch] no network address after {} s \u{2014} rebooting to the fallback entry\n", s));
                }
            }
        }
        if let (Some(s), Some(t0)) = (loss_s, lost_since) {
            if now.saturating_sub(t0) >= s * S {
                fire(&alloc::format!(
                    "[netwatch] network address lost for {} s \u{2014} rebooting to the fallback entry\n", s));
            }
        }
        if let Some(t) = &trial {
            if t.max_s > 0 && now >= t.max_s * S {
                fire(&alloc::format!(
                    "[netwatch] trial not promoted within {} s (netwatch.trial_max) \u{2014} rebooting to the fallback entry\n", t.max_s));
            }
            if t.reach_s > 0 && now >= next_probe {
                next_probe = now + t.every_s * S;
                let dest = t.host.or_else(net_server::netwatch::default_gateway);
                let ok = dest.map_or(false, net_server::netwatch::probe);
                if ok {
                    if reach_lost {
                        serial_print_str(&alloc::format!(
                            "[netwatch] {} answers again after {} s\n", ip_str(dest), (now - last_ok) / S));
                    }
                    last_ok = now; failed = 0; reach_lost = false;
                } else {
                    failed = failed.saturating_add(1);
                    let gap = (now - last_ok) / S;
                    if !reach_lost && failed >= t.fails {
                        reach_lost = true;
                        serial_print_str(&alloc::format!(
                            "[netwatch] {} not answering ({} probes, {} s); reboot at {} s\n", ip_str(dest), failed, gap, t.reach_s));
                    }
                    if failed >= t.fails && gap >= t.reach_s {
                        fire(&alloc::format!(
                            "[netwatch] {} unreachable for {} s ({} probes failed) \u{2014} rebooting to the fallback entry\n",
                            ip_str(dest), gap, failed));
                    }
                }
            }
        }
        sched::kthread::sleep_ns(S);
    }
}

fn fire(msg: &str) -> ! {
    serial_print_str(msg);
    crate::power::restart_now()
}

/// Start the watcher thread when a knob is set.
pub fn init() {
    let trial = trial_cfg();
    if knob("netwatch").is_none() && knob("netwatch.loss").is_none() && trial.is_none() { return; }
    serial_print_str("[netwatch] armed\n");
    if let Some(t) = &trial {
        serial_print_str(&alloc::format!(
            "[netwatch] trial watch: reach={} s every={} s fails={} host={} trial_max={} s\n",
            t.reach_s, t.every_s, t.fails, t.host.map_or(alloc::string::String::from("gateway"), |h| ip_str(Some(h))), t.max_s));
    }
    if sched::spawn(watch, 0).map(|p| sched::set_comm(p, b"netwatch")).is_none() {
        serial_print_str("[netwatch] could not start the watcher task\n");
    }
}
