//! leandros-sysbus: the minimal freedesktop system services a COSMIC session
//! asks for, implemented for LeandrOS.
//!
//! One binary, one service per process, chosen by argv[1]:
//!
//!   leandros-sysbus login1   org.freedesktop.login1   (sessions, seats, inhibitors)
//!   leandros-sysbus locale1  org.freedesktop.locale1  (/etc/locale.conf, X11 keymap)
//!   leandros-sysbus upower   org.freedesktop.UPower   (power_supply class, or no battery)
//!   leandros-sysbus power-profiles  org.freedesktop.UPower.PowerProfiles +
//!                                    net.hadess.PowerProfiles (ppd, placeholder driver)
//!   leandros-sysbus polkit   org.freedesktop.PolicyKit1 (agent registry; only root is authorized)
//!   leandros-sysbus rtkit    org.freedesktop.RealtimeKit1 (grants nothing; NotSupported)
//!   leandros-sysbus timedate1  org.freedesktop.timedate1 (Timezone, SetTimezone, no NTP)
//!   leandros-sysbus switcheroo net.hadess.SwitcherooControl (GPUs from /sys/class/drm)
//!   leandros-sysbus a11y     org.a11y.Bus (GetAddress -> the session bus)
//!   leandros-sysbus svc-test client: portal/timedate1/switcheroo/a11y activation checks
//!   leandros-sysbus networkmanager  org.freedesktop.NetworkManager over /dev/netctl
//!                            (or a scripted mock: LEANDROS_NM_MOCK, see nm/mock.rs)
//!   leandros-sysbus probe    client: exercises all of them, prints PASS/FAIL
//!   leandros-sysbus portal <settings|screenshot|open-file|all> [timeout_s]
//!                            client of org.freedesktop.portal.Desktop (ports/portal)
//!
//! Each is started by busd's D-Bus activation from a `.service` file in
//! /usr/share/dbus-1/services (ports/dbus/session-pkg/services), the first
//! time any client addresses the name.
//!
//! Which bus. LeandrOS runs no system broker: start-cosmic-leandros aliases
//! DBUS_SYSTEM_BUS_ADDRESS to the session bus, so `Connection::system()` in a
//! COSMIC component reaches the session busd, and these services claim their
//! names there. We connect to DBUS_SYSTEM_BUS_ADDRESS first (that is where the
//! clients look), then DBUS_SESSION_BUS_ADDRESS.
//!
//! Privilege. busd spawns activated services as its own user, i.e. the session
//! user. The services therefore never claim more than that user can do:
//! anything that needs root (powering off, writing /etc as uid 1000) is
//! reported as an error instead of pretending to succeed. See each module.

mod a11y;
mod bluez;
mod locale1;
mod login1;
mod nm;
mod polkit;
mod power_profiles;
mod portal_probe;
mod probe;
mod rtkit;
mod svc_test;
mod switcheroo;
mod timedate1;
mod upower;

use std::process::ExitCode;

fn bus_address() -> Option<String> {
    ["DBUS_SYSTEM_BUS_ADDRESS", "DBUS_SESSION_BUS_ADDRESS"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

pub(crate) fn log(service: &str, msg: std::fmt::Arguments<'_>) {
    eprintln!("leandros-sysbus[{service}]: {msg}");
}

/// Every subcommand this binary implements (bus services and clients), as a
/// marker the image build reads out of the staged binary
/// (scripts/mkfs-f2fs-populated.py) and compares with THIS line in the tree
/// being packed: any difference means the staged binary was built from
/// another tree and mkfs refuses the image. ~/code/leandros-artifacts is
/// shared by every worktree, so a sysbus rebuilt by a lane on an older tree
/// would otherwise silently drop e.g. `bluez` or `power`. Add every new
/// subcommand here.
const SERVICES: &str = "leandros-sysbus-services:login1,locale1,upower,power-profiles,polkit,rtkit,networkmanager,bluez,bluez-test,probe,power,portal,timedate1,switcheroo,a11y,svc-test;";

fn usage() -> ExitCode {
    eprintln!("usage: leandros-sysbus <login1|locale1|upower|power-profiles|polkit|rtkit|timedate1|switcheroo|a11y|svc-test|networkmanager|bluez|bluez-test|probe|power <PowerOff|Reboot|Halt|CanPowerOff|CanReboot|CanHalt>|portal [settings|screenshot|open-file|all] [timeout_s]>");
    eprintln!("{SERVICES}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let Some(which) = std::env::args().nth(1) else {
        return usage();
    };
    // The portal lives on the session bus proper.
    let address = if which == "portal" {
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().filter(|v| !v.is_empty()).or_else(bus_address)
    } else {
        bus_address()
    };
    let Some(address) = address else {
        eprintln!("leandros-sysbus: no DBUS_SYSTEM_BUS_ADDRESS or DBUS_SESSION_BUS_ADDRESS");
        return ExitCode::from(1);
    };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("leandros-sysbus: tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let result = rt.block_on(async {
        let builder = zbus::connection::Builder::address(address.as_str())?;
        if which == "portal" {
            let what = std::env::args().nth(2).unwrap_or_else(|| "all".into());
            let t = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(30);
            let (_pass, fail) = portal_probe::run(builder.build().await?, &what, t).await;
            return Ok(Some(fail == 0));
        }
        if which == "power" {
            // Client: `leandros-sysbus power <PowerOff|Reboot|Halt|CanPowerOff|CanReboot|CanHalt>`
            // calls login1 on the system bus the way systemctl/loginctl do;
            // /bin/poweroff, reboot, halt and systemctl use it for a non-root
            // caller. Can* print logind's answer ("yes", "na", ...).
            let method = std::env::args().nth(2).unwrap_or_default();
            if !matches!(
                method.as_str(),
                "PowerOff" | "Reboot" | "Halt" | "CanPowerOff" | "CanReboot" | "CanHalt"
            ) {
                return Ok(None);
            }
            let conn = builder.build().await?;
            let (dest, path, iface) =
                (Some("org.freedesktop.login1"), "/org/freedesktop/login1", Some("org.freedesktop.login1.Manager"));
            if method.starts_with("Can") {
                let reply = conn.call_method(dest, path, iface, method.as_str(), &()).await?;
                let answer: String = reply.body().deserialize()?;
                println!("{answer}");
                return Ok(Some(answer == "yes"));
            }
            conn.call_method(dest, path, iface, method.as_str(), &(false,)).await?;
            return Ok(Some(true));
        }
        if which == "bluez-test" {
            let (_pass, fail) = bluez::test::run(builder.build().await?).await;
            return Ok(Some(fail == 0));
        }
        if which == "svc-test" {
            let (_pass, fail) = svc_test::run(builder.build().await?).await;
            return Ok(Some(fail == 0));
        }
        if which == "probe" {
            let (_pass, fail) = probe::run(builder.build().await?).await;
            return Ok(Some(fail == 0));
        }
        let conn = match which.as_str() {
            "login1" => login1::serve(builder).await?,
            "locale1" => locale1::serve(builder).await?,
            "upower" => upower::serve(builder).await?,
            "power-profiles" => power_profiles::serve(builder).await?,
            "polkit" => polkit::serve(builder).await?,
            "rtkit" => rtkit::serve(builder).await?,
            "timedate1" => timedate1::serve(builder).await?,
            "switcheroo" => switcheroo::serve(builder).await?,
            "a11y" => a11y::serve(builder, &address).await?,
            "networkmanager" => nm::serve(builder).await?,
            "bluez" => bluez::serve(builder).await?,
            _ => return Ok::<_, zbus::Error>(None),
        };
        log(&which, format_args!("serving on {address}"));
        // Serve until the bus goes away. busd dies with the session, and a
        // service that outlived it would leak one process per login.
        let mut stream = zbus::MessageStream::from(&conn);
        while let Some(msg) = futures_util::StreamExt::next(&mut stream).await {
            if let Err(e) = msg {
                log(&which, format_args!("bus connection lost ({e}); exiting"));
                break;
            }
        }
        Ok(Some(true))
    });
    match result {
        Ok(Some(true)) => ExitCode::SUCCESS,
        Ok(Some(false)) => ExitCode::from(1),
        Ok(None) => usage(),
        Err(e) => {
            eprintln!("leandros-sysbus[{which}]: {e}");
            ExitCode::from(1)
        }
    }
}

/// Microseconds since the Unix epoch and on CLOCK_MONOTONIC, the pair logind
/// and upower report for every timestamp property.
pub(crate) fn now_usec() -> (u64, u64) {
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let mono = ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000;
    (wall, mono)
}
