//! leandros-sysbus: the minimal freedesktop system services a COSMIC session
//! asks for, implemented for LeandrOS.
//!
//! One binary, one service per process, chosen by argv[1]:
//!
//!   leandros-sysbus login1   org.freedesktop.login1   (sessions, seats, inhibitors)
//!   leandros-sysbus locale1  org.freedesktop.locale1  (/etc/locale.conf, X11 keymap)
//!   leandros-sysbus upower   org.freedesktop.UPower   (a machine with no battery)
//!   leandros-sysbus power-profiles  org.freedesktop.UPower.PowerProfiles +
//!                                    net.hadess.PowerProfiles (ppd, placeholder driver)
//!   leandros-sysbus probe    client: exercises all of them, prints PASS/FAIL
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

mod locale1;
mod login1;
mod power_profiles;
mod probe;
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

fn usage() -> ExitCode {
    eprintln!("usage: leandros-sysbus <login1|locale1|upower|power-profiles|probe>");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let Some(which) = std::env::args().nth(1) else {
        return usage();
    };
    let Some(address) = bus_address() else {
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
        if which == "probe" {
            let (_pass, fail) = probe::run(builder.build().await?).await;
            return Ok(Some(fail == 0));
        }
        let conn = match which.as_str() {
            "login1" => login1::serve(builder).await?,
            "locale1" => locale1::serve(builder).await?,
            "upower" => upower::serve(builder).await?,
            "power-profiles" => power_profiles::serve(builder).await?,
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
