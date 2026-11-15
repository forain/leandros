//! org.freedesktop.timedate1: the clock and timezone service behind COSMIC
//! Settings' "Date & time" page.
//!
//! Read side: Timezone (the /etc/timezone name, else the /etc/localtime
//! symlink target under zoneinfo, else "UTC"), LocalRTC (always false: the
//! RTC is UTC), CanNTP/NTP (false: LeandrOS has no time-sync daemon),
//! NTPSynchronized (false), TimeUSec, RTCTimeUSec (the same wall clock).
//!
//! Write side. Like locale1, the service runs as the session user, so
//! SetTimezone writes /etc/timezone and the /etc/localtime symlink only when
//! that user may, and otherwise fails with AccessDenied. A name must exist
//! under /usr/share/zoneinfo, except "UTC"/"Etc/UTC" which are always valid
//! (the image ships no tzdata package; /etc/localtime is a UTC TZif file).
//! SetLocalRTC(true), SetNTP(true) and SetTime are NotSupported.

use std::sync::Mutex;

const NAME: &str = "org.freedesktop.timedate1";
const PATH: &str = "/org/freedesktop/timedate1";
const ZONEINFO: &str = "/usr/share/zoneinfo";
const TIMEZONE_FILE: &str = "/etc/timezone";
const LOCALTIME: &str = "/etc/localtime";

fn log(msg: std::fmt::Arguments<'_>) {
    crate::log("timedate1", msg);
}

fn current_timezone() -> String {
    if let Ok(t) = std::fs::read_to_string(TIMEZONE_FILE) {
        let t = t.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(target) = std::fs::read_link(LOCALTIME) {
        if let Some(rest) = target.to_string_lossy().split("zoneinfo/").nth(1) {
            return rest.to_string();
        }
    }
    "UTC".to_string()
}

/// A zone name is acceptable if it is relative, has no `..`, and names a file
/// under zoneinfo (UTC always passes).
fn valid_zone(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.split('/').any(|c| c.is_empty() || c == "..") {
        return false;
    }
    if name == "UTC" || name == "Etc/UTC" {
        return true;
    }
    std::path::Path::new(ZONEINFO).join(name).is_file()
}

fn list_zones() -> Vec<String> {
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            // Skip the tzdata bookkeeping and the non-zone trees.
            if n.starts_with('.') || n.contains('.') || matches!(n.as_str(), "posix" | "right" | "posixrules" | "localtime" | "Etc") {
                continue;
            }
            let full = if prefix.is_empty() { n.clone() } else { format!("{prefix}/{n}") };
            let p = e.path();
            if p.is_dir() {
                walk(&p, &full, out);
            } else {
                out.push(full);
            }
        }
    }
    let mut v = Vec::new();
    walk(std::path::Path::new(ZONEINFO), "", &mut v);
    if !v.iter().any(|z| z == "UTC") {
        v.push("UTC".into());
    }
    v.sort();
    v
}

fn wall_usec() -> u64 {
    crate::now_usec().0
}

struct TimeDate {
    tz: Mutex<String>,
}

fn io_err(what: &str, e: std::io::Error) -> zbus::fdo::Error {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            zbus::fdo::Error::AccessDenied(format!("{what}: {e} (the system timezone belongs to root)"))
        }
        _ => zbus::fdo::Error::Failed(format!("{what}: {e}")),
    }
}

#[zbus::interface(name = "org.freedesktop.timedate1")]
impl TimeDate {
    async fn set_timezone(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        timezone: String,
        _interactive: bool,
    ) -> zbus::fdo::Result<()> {
        if !valid_zone(&timezone) {
            return Err(zbus::fdo::Error::InvalidArgs(format!("invalid or unknown timezone '{timezone}'")));
        }
        let text = format!("{timezone}\n");
        let tmp = format!("{TIMEZONE_FILE}.tmp-{}", std::process::id());
        let res = std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, TIMEZONE_FILE));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            log(format_args!("SetTimezone {timezone}: {TIMEZONE_FILE}: {e}"));
            return Err(io_err(&format!("cannot write {TIMEZONE_FILE}"), e));
        }
        // The symlink only when the zone data exists; the shipped UTC file
        // stays otherwise.
        let src = std::path::Path::new(ZONEINFO).join(&timezone);
        if src.is_file() {
            let ltmp = format!("{LOCALTIME}.tmp-{}", std::process::id());
            let _ = std::fs::remove_file(&ltmp);
            let res = std::os::unix::fs::symlink(&src, &ltmp).and_then(|_| std::fs::rename(&ltmp, LOCALTIME));
            if let Err(e) = res {
                let _ = std::fs::remove_file(&ltmp);
                log(format_args!("SetTimezone {timezone}: {LOCALTIME}: {e}"));
                return Err(io_err(&format!("cannot update {LOCALTIME}"), e));
            }
        }
        let changed = {
            let mut cur = self.tz.lock().unwrap();
            let c = *cur != timezone;
            *cur = timezone;
            c
        };
        if changed {
            self.timezone_changed(&emitter).await?;
        }
        Ok(())
    }

    fn set_local_rtc(&self, local: bool, _fix_system: bool, _interactive: bool) -> zbus::fdo::Result<()> {
        if local {
            return Err(zbus::fdo::Error::NotSupported("the RTC is kept in UTC".into()));
        }
        Ok(())
    }

    fn set_ntp(&self, use_ntp: bool, _interactive: bool) -> zbus::fdo::Result<()> {
        if use_ntp {
            return Err(zbus::fdo::Error::NotSupported("LeandrOS has no time synchronisation service".into()));
        }
        Ok(())
    }

    fn set_time(&self, _usec_utc: i64, _relative: bool, _interactive: bool) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported("setting the clock is not available to the session".into()))
    }

    fn list_timezones(&self) -> Vec<String> {
        list_zones()
    }

    #[zbus(property)]
    fn timezone(&self) -> String {
        self.tz.lock().unwrap().clone()
    }
    #[zbus(property, name = "LocalRTC")]
    fn local_rtc(&self) -> bool {
        false
    }
    #[zbus(property, name = "CanNTP")]
    fn can_ntp(&self) -> bool {
        false
    }
    #[zbus(property, name = "NTP")]
    fn ntp(&self) -> bool {
        false
    }
    #[zbus(property, name = "NTPSynchronized")]
    fn ntp_synchronized(&self) -> bool {
        false
    }
    #[zbus(property, name = "TimeUSec")]
    fn time_usec(&self) -> u64 {
        wall_usec()
    }
    #[zbus(property, name = "RTCTimeUSec")]
    fn rtc_time_usec(&self) -> u64 {
        wall_usec()
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    builder
        .serve_at(PATH, TimeDate { tz: Mutex::new(current_timezone()) })?
        .name(NAME)?
        .build()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zone_names() {
        assert!(valid_zone("UTC"));
        assert!(!valid_zone("../etc/passwd"));
        assert!(!valid_zone("/etc/passwd"));
        assert!(!valid_zone(""));
    }
}
