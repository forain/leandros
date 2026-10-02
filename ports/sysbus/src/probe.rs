//! `leandros-sysbus probe`: exercise every service the way the COSMIC
//! components do (property caching via GetAll, the calls they make) and
//! print one line per check, `PASS`/`FAIL`, then a summary. Used for
//! in-guest verification; also runs on a host bus.

use std::collections::HashMap;
use std::time::Instant;

use zbus::zvariant::OwnedValue;

async fn get_all(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    iface: &str,
) -> zbus::Result<HashMap<String, OwnedValue>> {
    let p = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(dest.to_owned())?
        .path(path.to_owned())?
        .build()
        .await?;
    let iface = zbus::names::InterfaceName::try_from(iface.to_owned())?;
    Ok(p.get_all(iface).await?)
}

async fn call<B, R>(conn: &zbus::Connection, dest: &str, path: &str, iface: &str, method: &str, body: &B) -> zbus::Result<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn
        .call_method(Some(dest), path, Some(iface), method, body)
        .await?;
    reply.body().deserialize::<R>()
}

pub async fn run(conn: zbus::Connection) -> (u32, u32) {
    let (mut pass, mut fail) = (0u32, 0u32);
    macro_rules! check {
        ($name:expr, $fut:expr) => {{
            let t = Instant::now();
            match $fut.await {
                Ok(v) => {
                    pass += 1;
                    println!("PASS {:<40} {:>5} ms  {}", $name, t.elapsed().as_millis(), v);
                }
                Err(e) => {
                    fail += 1;
                    println!("FAIL {:<40} {:>5} ms  {}", $name, t.elapsed().as_millis(), e);
                }
            }
        }};
    }
    let c = &conn;

    // login1 -- first call activates the service.
    check!("login1 Manager.GetAll", async {
        get_all(c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager")
            .await
            .map(|m| format!("{} props, BlockInhibited={:?}", m.len(), m.get("BlockInhibited")))
    });
    check!("login1 Session(auto).Id", async {
        get_all(c, "org.freedesktop.login1", "/org/freedesktop/login1/session/auto", "org.freedesktop.login1.Session")
            .await
            .and_then(|m| {
                let id: String = m.get("Id").cloned().ok_or(zbus::Error::Failure("no Id".into()))?.try_into()?;
                let active: bool = m.get("Active").cloned().ok_or(zbus::Error::Failure("no Active".into()))?.try_into()?;
                Ok(format!("Id={id} Active={active} ({} props)", m.len()))
            })
    });
    check!("login1 ListSessions", async {
        call::<_, Vec<(String, u32, String, String, zbus::zvariant::OwnedObjectPath)>>(
            c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "ListSessions", &(),
        )
        .await
        .map(|v| format!("{v:?}"))
    });
    // Hold an inhibitor the way cosmic-session does, list it, drop it.
    check!("login1 Inhibit+ListInhibitors+release", async {
        let fd: zbus::zvariant::OwnedFd = call(
            c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "Inhibit",
            &("handle-power-key", "probe", "probe", "block"),
        )
        .await?;
        let listed: Vec<(String, String, String, String, u32, u32)> = call(
            c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "ListInhibitors", &(),
        )
        .await?;
        drop(fd);
        let mut after = listed.len();
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let v: Vec<(String, String, String, String, u32, u32)> = call(
                c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "ListInhibitors", &(),
            )
            .await?;
            after = v.iter().filter(|i| i.1 == "probe").count();
            if after == 0 {
                break;
            }
        }
        if listed.iter().any(|i| i.1 == "probe") && after == 0 {
            Ok(format!("held {} -> released", listed.len()))
        } else {
            Err(zbus::Error::Failure(format!("listed={listed:?} still_after_close={after}")))
        }
    });
    // Not PowerOff itself: that now really powers the machine off.
    check!("login1 CanPowerOff/CanReboot -> yes", async {
        let a: String = call(c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "CanPowerOff", &()).await?;
        let b: String = call(c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "CanReboot", &()).await?;
        if a == "yes" && b == "yes" { Ok(format!("{a}/{b}")) } else { Err(zbus::Error::Failure(format!("CanPowerOff={a} CanReboot={b}"))) }
    });
    check!("login1 Suspend -> NotSupported", async {
        match call::<_, ()>(c, "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "Suspend", &(false,)).await {
            Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.NotSupported" => Ok("NotSupported".to_string()),
            other => Err(zbus::Error::Failure(format!("unexpected {other:?}"))),
        }
    });

    // locale1
    check!("locale1 GetAll", async {
        get_all(c, "org.freedesktop.locale1", "/org/freedesktop/locale1", "org.freedesktop.locale1")
            .await
            .and_then(|m| {
                let loc: Vec<String> = m.get("Locale").cloned().ok_or(zbus::Error::Failure("no Locale".into()))?.try_into()?;
                Ok(format!("Locale={loc:?} ({} props)", m.len()))
            })
    });
    check!("locale1 SetX11Keyboard round trip", async {
        let before: HashMap<String, OwnedValue> =
            get_all(c, "org.freedesktop.locale1", "/org/freedesktop/locale1", "org.freedesktop.locale1").await?;
        let layout: String = before.get("X11Layout").cloned().map(|v| v.try_into()).transpose()?.unwrap_or_default();
        call::<_, ()>(c, "org.freedesktop.locale1", "/org/freedesktop/locale1", "org.freedesktop.locale1", "SetX11Keyboard",
            &("us", "", "", "", false, false)).await?;
        let mid: HashMap<String, OwnedValue> =
            get_all(c, "org.freedesktop.locale1", "/org/freedesktop/locale1", "org.freedesktop.locale1").await?;
        let got: String = mid.get("X11Layout").cloned().map(|v| v.try_into()).transpose()?.unwrap_or_default();
        // restore
        call::<_, ()>(c, "org.freedesktop.locale1", "/org/freedesktop/locale1", "org.freedesktop.locale1", "SetX11Keyboard",
            &(layout.as_str(), "", "", "", false, false)).await?;
        if got == "us" { Ok(format!("X11Layout {layout:?} -> \"us\" -> restored")) } else { Err(zbus::Error::Failure(format!("got {got:?}"))) }
    });

    // UPower
    check!("UPower GetAll", async {
        get_all(c, "org.freedesktop.UPower", "/org/freedesktop/UPower", "org.freedesktop.UPower")
            .await
            .and_then(|m| {
                let ob: bool = m.get("OnBattery").cloned().ok_or(zbus::Error::Failure("no OnBattery".into()))?.try_into()?;
                Ok(format!("OnBattery={ob} ({} props)", m.len()))
            })
    });
    check!("UPower DisplayDevice GetAll", async {
        get_all(c, "org.freedesktop.UPower", "/org/freedesktop/UPower/devices/DisplayDevice", "org.freedesktop.UPower.Device")
            .await
            .and_then(|m| {
                let p: bool = m.get("IsPresent").cloned().ok_or(zbus::Error::Failure("no IsPresent".into()))?.try_into()?;
                Ok(format!("IsPresent={p} ({} props)", m.len()))
            })
    });
    check!("UPower KbdBacklight.GetMaxBrightness", async {
        call::<_, i32>(c, "org.freedesktop.UPower", "/org/freedesktop/UPower/KbdBacklight", "org.freedesktop.UPower.KbdBacklight", "GetMaxBrightness", &())
            .await
            .map(|v| format!("{v}"))
    });

    // power-profiles-daemon
    check!("PowerProfiles GetAll + set power-saver", async {
        let m = get_all(c, "org.freedesktop.UPower.PowerProfiles", "/org/freedesktop/UPower/PowerProfiles",
            "org.freedesktop.UPower.PowerProfiles").await?;
        let active: String = m.get("ActiveProfile").cloned().ok_or(zbus::Error::Failure("no ActiveProfile".into()))?.try_into()?;
        let p = zbus::fdo::PropertiesProxy::builder(c)
            .destination("org.freedesktop.UPower.PowerProfiles")?
            .path("/org/freedesktop/UPower/PowerProfiles")?
            .build().await?;
        let iface = zbus::names::InterfaceName::from_static_str_unchecked("org.freedesktop.UPower.PowerProfiles");
        p.set(iface.clone(), "ActiveProfile", zbus::zvariant::Value::from("power-saver")).await?;
        let now: String = p.get(iface.clone(), "ActiveProfile").await?.try_into()?;
        p.set(iface, "ActiveProfile", zbus::zvariant::Value::from(active.as_str())).await?;
        if now == "power-saver" { Ok(format!("{active} -> power-saver -> {active}")) } else { Err(zbus::Error::Failure(format!("got {now}"))) }
    });
    check!("net.hadess.PowerProfiles (same process)", async {
        get_all(c, "net.hadess.PowerProfiles", "/net/hadess/PowerProfiles", "net.hadess.PowerProfiles")
            .await
            .map(|m| format!("{} props", m.len()))
    });

    // PolicyKit1: cosmic-osd's registration, and a non-root check is refused.
    check!("PolicyKit1 Register + CheckAuthorization", async {
        let mut d: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        d.insert("session-id", "c1".into());
        call::<_, ()>(c, "org.freedesktop.PolicyKit1", "/org/freedesktop/PolicyKit1/Authority",
            "org.freedesktop.PolicyKit1.Authority", "RegisterAuthenticationAgent",
            &(("unix-session", d), "en_US", "/org/leandros/ProbeAgent")).await?;
        let mut u: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        u.insert("uid", 1000u32.into());
        let (ok, challenge, _): (bool, bool, HashMap<String, String>) = call(c, "org.freedesktop.PolicyKit1",
            "/org/freedesktop/PolicyKit1/Authority", "org.freedesktop.PolicyKit1.Authority", "CheckAuthorization",
            &(("unix-user", u), "org.freedesktop.login1.power-off", HashMap::<&str, &str>::new(), 0u32, "")).await?;
        if !ok && !challenge { Ok("registered; uid 1000 not authorized".to_string()) }
        else { Err(zbus::Error::Failure(format!("uid 1000 authorized={ok} challenge={challenge}"))) }
    });

    println!("SYSBUS_PROBE pass={pass} fail={fail}");
    (pass, fail)
}
