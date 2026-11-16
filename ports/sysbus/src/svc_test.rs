//! `leandros-sysbus svc-test`: bus-level checks of the session services a
//! COSMIC desktop reaches for beyond login1 & co: the xdg-desktop-portal
//! frontend (activation, Screenshot/FileChooser interfaces, the cosmic
//! backend registered as activatable), timedate1, SwitcherooControl and the
//! a11y bus. Needs no compositor, so it runs under `dbus-run-session` in the
//! regress suite. One PASS/FAIL line per check, `SVC_TEST pass=N fail=M` last.

use std::collections::HashMap;
use std::time::Duration;

use zbus::zvariant::OwnedValue;

type R<T> = zbus::Result<T>;

fn fail(m: impl Into<String>) -> zbus::Error {
    zbus::Error::Failure(m.into())
}

async fn timed<T>(f: impl std::future::Future<Output = R<T>>) -> R<T> {
    tokio::time::timeout(Duration::from_secs(60), f).await.unwrap_or_else(|_| Err(fail("timeout (60 s)")))
}

async fn get_all(c: &zbus::Connection, dest: &str, path: &str, iface: &str) -> R<HashMap<String, OwnedValue>> {
    timed(async {
        let p = zbus::fdo::PropertiesProxy::builder(c).destination(dest.to_owned())?.path(path.to_owned())?.build().await?;
        Ok(p.get_all(zbus::names::InterfaceName::try_from(iface.to_owned())?).await?)
    })
    .await
}

async fn call<B, T>(c: &zbus::Connection, dest: &str, path: &str, iface: &str, m: &str, body: &B) -> R<T>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    T: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    timed(async { c.call_method(Some(dest), path, Some(iface), m, body).await?.body().deserialize::<T>() }).await
}

fn error_name(r: &R<()>) -> String {
    match r {
        Err(zbus::Error::MethodError(n, _, _)) => n.to_string(),
        Err(e) => format!("{e}"),
        Ok(()) => "ok".into(),
    }
}

pub async fn run(c: zbus::Connection, with_portal: bool) -> (u32, u32) {
    let (mut pass, mut fail_n) = (0u32, 0u32);
    macro_rules! check {
        ($name:expr, $fut:expr) => {{
            match $fut.await {
                Ok(v) => {
                    pass += 1;
                    println!("PASS {:<36} {}", $name, v);
                }
                Err(e) => {
                    fail_n += 1;
                    println!("FAIL {:<36} {}", $name, e);
                }
            }
        }};
    }
    let c = &c;

    // --- xdg-desktop-portal (ports/portal) ---
    // The frontend waits for its cosmic backend (a Wayland client) at start-up,
    // so this only works inside a COSMIC session: `svc-test portal`.
    if with_portal {
    const P: &str = "org.freedesktop.portal.Desktop";
    const PP: &str = "/org/freedesktop/portal/desktop";
    check!("portal Screenshot interface", async {
        let m = get_all(c, P, PP, "org.freedesktop.portal.Screenshot").await?;
        let v: u32 = m.get("version").and_then(|v| v.downcast_ref::<u32>().ok()).ok_or_else(|| fail("no version"))?;
        Ok::<_, zbus::Error>(format!("version={v}"))
    });
    check!("portal FileChooser interface", async {
        let m = get_all(c, P, PP, "org.freedesktop.portal.FileChooser").await?;
        let v: u32 = m.get("version").and_then(|v| v.downcast_ref::<u32>().ok()).ok_or_else(|| fail("no version"))?;
        Ok::<_, zbus::Error>(format!("version={v}"))
    });
    check!("portal cosmic backend activatable", async {
        let names: Vec<String> =
            call(c, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListActivatableNames", &()).await?;
        if names.iter().any(|n| n == "org.freedesktop.impl.portal.desktop.cosmic") {
            Ok(format!("{} activatable names", names.len()))
        } else {
            Err(fail("org.freedesktop.impl.portal.desktop.cosmic not activatable"))
        }
    });

    }

    // --- timedate1 ---
    const T: &str = "org.freedesktop.timedate1";
    const TP: &str = "/org/freedesktop/timedate1";
    check!("timedate1 properties", async {
        let m = get_all(c, T, TP, T).await?;
        let tz: String = m.get("Timezone").and_then(|v| v.downcast_ref::<&str>().ok().map(|s| s.to_string())).ok_or_else(|| fail("no Timezone"))?;
        let rtc: bool = m.get("LocalRTC").and_then(|v| v.downcast_ref::<bool>().ok()).ok_or_else(|| fail("no LocalRTC"))?;
        let ntp: bool = m.get("CanNTP").and_then(|v| v.downcast_ref::<bool>().ok()).ok_or_else(|| fail("no CanNTP"))?;
        let t: u64 = m.get("TimeUSec").and_then(|v| v.downcast_ref::<u64>().ok()).ok_or_else(|| fail("no TimeUSec"))?;
        if rtc || ntp || t == 0 || tz.is_empty() {
            return Err(fail(format!("tz={tz:?} LocalRTC={rtc} CanNTP={ntp} TimeUSec={t}")));
        }
        Ok(format!("Timezone={tz} TimeUSec={t}"))
    });
    check!("timedate1 ListTimezones has UTC", async {
        let z: Vec<String> = call(c, T, TP, T, "ListTimezones", &()).await?;
        if z.iter().any(|s| s == "UTC") { Ok(format!("{} zones", z.len())) } else { Err(fail("no UTC")) }
    });
    check!("timedate1 SetTimezone bad -> InvalidArgs", async {
        let r = call::<_, ()>(c, T, TP, T, "SetTimezone", &("Nowhere/Bogus", false)).await;
        let n = error_name(&r);
        if n == "org.freedesktop.DBus.Error.InvalidArgs" { Ok(n) } else { Err(fail(n)) }
    });
    check!("timedate1 SetTimezone UTC", async {
        let r = call::<_, ()>(c, T, TP, T, "SetTimezone", &("UTC", false)).await;
        let n = error_name(&r);
        // Root writes /etc/timezone; a session user gets AccessDenied.
        if n == "ok" || n == "org.freedesktop.DBus.Error.AccessDenied" { Ok(n) } else { Err(fail(n)) }
    });
    check!("timedate1 SetTimezone America/Sao_Paulo -> /etc/localtime + date", async {
        let r = call::<_, ()>(c, T, TP, T, "SetTimezone", &("America/Sao_Paulo", false)).await;
        let n = error_name(&r);
        if n == "org.freedesktop.DBus.Error.AccessDenied" { return Ok(n); }
        if n != "ok" { return Err(fail(n)); }
        let link = std::fs::read_link("/etc/localtime").map_err(|e| fail(format!("readlink: {e}")))?;
        let out = std::process::Command::new("date").arg("+%Z").env_remove("TZ").output().map_err(|e| fail(format!("date: {e}")))?;
        let abbr = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let m = get_all(c, T, TP, T).await?;
        let tz: String = m.get("Timezone").and_then(|v| v.downcast_ref::<&str>().ok().map(|s| s.to_string())).unwrap_or_default();
        // restore before judging so a failure does not leave the image zone changed
        let _ = call::<_, ()>(c, T, TP, T, "SetTimezone", &("UTC", false)).await;
        if link.to_string_lossy() != "/usr/share/zoneinfo/America/Sao_Paulo" { return Err(fail(format!("link={}", link.display()))); }
        if abbr != "-03" { return Err(fail(format!("date +%Z={abbr:?}"))); }
        if tz != "America/Sao_Paulo" { return Err(fail(format!("Timezone={tz}"))); }
        Ok(format!("link={} date={abbr}", link.display()))
    });
    check!("timedate1 SetNTP(true) -> NotSupported", async {
        let r = call::<_, ()>(c, T, TP, T, "SetNTP", &(true, false)).await;
        let n = error_name(&r);
        if n == "org.freedesktop.DBus.Error.NotSupported" { Ok(n) } else { Err(fail(n)) }
    });

    // --- SwitcherooControl ---
    const S: &str = "net.hadess.SwitcherooControl";
    check!("SwitcherooControl properties", async {
        let m = get_all(c, S, "/net/hadess/SwitcherooControl", S).await?;
        let dual: bool = m.get("HasDualGpu").and_then(|v| v.downcast_ref::<bool>().ok()).ok_or_else(|| fail("no HasDualGpu"))?;
        let n: u32 = m.get("NumGPUs").and_then(|v| v.downcast_ref::<u32>().ok()).ok_or_else(|| fail("no NumGPUs"))?;
        let gpus = m.get("GPUs").ok_or_else(|| fail("no GPUs"))?;
        let list: Vec<HashMap<String, OwnedValue>> = gpus.try_clone()?.try_into().map_err(|e| fail(format!("GPUs type: {e}")))?;
        if dual || list.len() as u32 != n {
            return Err(fail(format!("HasDualGpu={dual} NumGPUs={n} GPUs.len={}", list.len())));
        }
        Ok(format!("HasDualGpu=false NumGPUs={n}"))
    });

    // --- a11y bus ---
    check!("a11y GetAddress", async {
        let a: String = call(c, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", &()).await?;
        if a.starts_with("unix:") { Ok(a) } else { Err(fail(format!("address {a:?}"))) }
    });
    check!("a11y Status IsEnabled=false", async {
        let m = get_all(c, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status").await?;
        let e: bool = m.get("IsEnabled").and_then(|v| v.downcast_ref::<bool>().ok()).ok_or_else(|| fail("no IsEnabled"))?;
        if !e { Ok("IsEnabled=false".to_string()) } else { Err(fail("IsEnabled=true")) }
    });

    println!("SVC_TEST pass={pass} fail={fail_n}");
    (pass, fail_n)
}
