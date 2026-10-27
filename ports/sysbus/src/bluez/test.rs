//! `leandros-sysbus bluez-test`: end-to-end test of the org.bluez shim the
//! way the COSMIC applet and Settings drive it. Starts `bttest serve` (real
//! btd on the scripted controller with the test pad behind it) and
//! `leandros-sysbus bluez` against it, then as a D-Bus client: the adapter is
//! exported, an agent registers, discovery finds the pad (InterfacesAdded),
//! Pair stores the link key, Connect/Disconnect flip Connected (with
//! PropertiesChanged), RemoveDevice drops the object (InterfacesRemoved).
//! Run under `dbus-run-session`. Output: "<case>: PASS|FAIL [detail]", then
//! "--- blueztest done: N failure(s)".

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const SOCK: &str = "/tmp/blueztest.sock";
const KEYS: &str = "/tmp/blueztest.keys";
const ADAPTER: &str = "/org/bluez/hci0";
const PAD: &str = "/org/bluez/hci0/dev_06_05_04_03_02_01";

struct T {
    pass: u32,
    fail: u32,
}

impl T {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            println!("{name}: PASS");
            self.pass += 1;
        } else {
            println!("{name}: FAIL {detail}");
            self.fail += 1;
        }
    }
}

async fn call<B>(c: &zbus::Connection, path: &str, iface: &str, m: &str, body: &B) -> zbus::Result<zbus::Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let fut = c.call_method(Some("org.bluez"), path, Some(iface), m, body);
    tokio::time::timeout(Duration::from_secs(70), fut).await.unwrap_or_else(|_| Err(zbus::Error::Failure("timeout".into())))
}

async fn get(c: &zbus::Connection, path: &str, iface: &str, prop: &str) -> Option<OwnedValue> {
    let r = call(c, path, "org.freedesktop.DBus.Properties", "Get", &(iface, prop)).await.ok()?;
    r.body().deserialize::<OwnedValue>().ok()
}

async fn get_bool(c: &zbus::Connection, path: &str, iface: &str, prop: &str) -> Option<bool> {
    get(c, path, iface, prop).await.and_then(|v| bool::try_from(v).ok())
}

type Managed = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

async fn managed(c: &zbus::Connection) -> Option<Managed> {
    let r = call(c, "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", &()).await.ok()?;
    r.body().deserialize::<Managed>().ok()
}

fn s(v: Option<&OwnedValue>) -> String {
    v.and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default()
}

/// Waits up to `secs` for a PropertiesChanged on `path` with `prop` == `want`.
async fn wait_prop(st: &mut zbus::MessageStream, path: &str, prop: &str, want: bool, secs: u64) -> bool {
    let t0 = Instant::now();
    while let Some(left) = Duration::from_secs(secs).checked_sub(t0.elapsed()) {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(left, st.next()).await else { return false };
        let h = msg.header();
        if h.member().map(|m| m.as_str()) != Some("PropertiesChanged") || h.path().map(|p| p.as_str()) != Some(path) {
            continue;
        }
        if let Ok((_i, ch, _inv)) = msg.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>() {
            if ch.get(prop).and_then(|v| bool::try_from(v.try_clone().ok()?).ok()) == Some(want) {
                return true;
            }
        }
    }
    false
}

pub async fn run(c: zbus::Connection) -> (u32, u32) {
    let mut t = T { pass: 0, fail: 0 };
    let _ = std::fs::remove_file(SOCK);
    let mut serve = match Command::new("/bin/bttest").args(["serve", SOCK, KEYS]).stdin(Stdio::piped()).spawn() {
        Ok(p) => p,
        Err(e) => {
            t.check("spawn_bttest_serve", false, e);
            return (t.pass, t.fail);
        }
    };
    let t0 = Instant::now();
    while !std::path::Path::new(SOCK).exists() && t0.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| "/usr/libexec/leandros-sysbus".into());
    let mut shim = match Command::new(exe).arg("bluez").env("LEANDROS_BTD_SOCK", SOCK).spawn() {
        Ok(p) => p,
        Err(e) => {
            t.check("spawn_shim", false, e);
            return (t.pass, t.fail);
        }
    };

    // Every signal from org.bluez, subscribed before anything happens.
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender("org.bluez").unwrap().build();
    let mut sig = match zbus::MessageStream::for_match_rule(rule, &c, Some(256)).await {
        Ok(s) => s,
        Err(e) => {
            t.check("subscribe", false, e);
            return (t.pass, t.fail);
        }
    };

    // Wait for our shim to own the name before the first call to it: a call
    // to an unowned org.bluez would make busd activate a second instance
    // (from org.bluez.service, without LEANDROS_BTD_SOCK).
    let t0 = Instant::now();
    if let Ok(dbus) = zbus::fdo::DBusProxy::new(&c).await {
        let name = zbus::names::BusName::try_from("org.bluez").unwrap();
        while t0.elapsed() < Duration::from_secs(20) && !dbus.name_has_owner(name.clone()).await.unwrap_or(false) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let mut objs = None;
    while t0.elapsed() < Duration::from_secs(20) {
        if let Some(m) = managed(&c).await {
            if m.keys().any(|p| p.as_str() == ADAPTER) {
                objs = Some(m);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let ad = objs.as_ref().and_then(|m| m.iter().find(|(p, _)| p.as_str() == ADAPTER)).and_then(|(_, i)| i.get("org.bluez.Adapter1"));
    t.check("adapter_exported", ad.is_some(), "no /org/bluez/hci0 Adapter1 within 20 s");
    if let Some(ad) = ad {
        let ok = s(ad.get("Address")) == "11:22:33:44:55:66"
            && ad.get("Powered").and_then(|v| bool::try_from(v.try_clone().ok()?).ok()) == Some(true)
            && ["Alias", "Discoverable", "Discovering", "Pairable"].iter().all(|k| ad.contains_key(*k));
        t.check("adapter_props", ok, format!("{ad:?}"));
    }

    let agent = ObjectPath::try_from("/org/bluez/blueztest/agent").unwrap();
    let r1 = call(&c, "/org/bluez", "org.bluez.AgentManager1", "RegisterAgent", &(&agent, "KeyboardDisplay")).await;
    let r2 = call(&c, "/org/bluez", "org.bluez.AgentManager1", "RequestDefaultAgent", &(&agent,)).await;
    t.check("agent_registered", r1.is_ok() && r2.is_ok(), format!("{r1:?} {r2:?}"));

    // The applet's sequence: SetDiscoveryFilter, StartDiscovery.
    let mut filter: HashMap<&str, Value<'_>> = HashMap::new();
    filter.insert("UUIDs", Value::from(Vec::<String>::new()));
    filter.insert("Transport", Value::from("auto"));
    let r = call(&c, ADAPTER, "org.bluez.Adapter1", "SetDiscoveryFilter", &(filter,)).await;
    t.check("set_discovery_filter", r.is_ok(), format!("{r:?}"));
    let r = call(&c, ADAPTER, "org.bluez.Adapter1", "StartDiscovery", &()).await;
    t.check("start_discovery", r.is_ok(), format!("{r:?}"));

    let t0 = Instant::now();
    let (mut found, mut disc) = (None, false);
    while t0.elapsed() < Duration::from_secs(20) && (found.is_none() || !disc) {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(1), sig.next()).await else { continue };
        let h = msg.header();
        match h.member().map(|m| m.as_str()) {
            Some("InterfacesAdded") => {
                type Added = (OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>);
                if let Ok((p, ifs)) = msg.body().deserialize::<Added>() {
                    if p.as_str() == PAD {
                        if let Some(d) = ifs.get("org.bluez.Device1") {
                            found = Some((s(d.get("Name")), s(d.get("Icon")), s(d.get("Address"))));
                        }
                    }
                }
            }
            Some("PropertiesChanged") if h.path().map(|p| p.as_str()) == Some(ADAPTER) => {
                if let Ok((_i, ch, _)) = msg.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>() {
                    disc |= ch.get("Discovering").and_then(|v| bool::try_from(v.try_clone().ok()?).ok()) == Some(true);
                }
            }
            _ => {}
        }
    }
    t.check("discovering_signalled", disc, "no PropertiesChanged Discovering=true");
    let ok = found.as_ref().map(|(n, i, a)| n == "LeandrOS Test Pad" && i == "input-gaming" && a == "06:05:04:03:02:01");
    t.check("device_added", ok == Some(true), format!("{found:?}"));

    let r = call(&c, ADAPTER, "org.bluez.Adapter1", "StopDiscovery", &()).await;
    let off = wait_prop(&mut sig, ADAPTER, "Discovering", false, 10).await;
    t.check("stop_discovery", r.is_ok() && off, format!("{r:?} discovering_off={off}"));

    let r = call(&c, PAD, "org.bluez.Device1", "Pair", &()).await;
    let sig_ok = wait_prop(&mut sig, PAD, "Paired", true, 5).await;
    let key = std::fs::read_to_string(KEYS).unwrap_or_default().contains("06:05:04:03:02:01");
    t.check("pair", r.is_ok() && sig_ok && key, format!("{r:?} signal={sig_ok} key={key}"));
    let r = call(&c, PAD, "org.freedesktop.DBus.Properties", "Set", &("org.bluez.Device1", "Trusted", Value::from(true))).await;
    t.check("set_trusted", r.is_ok() && get_bool(&c, PAD, "org.bluez.Device1", "Trusted").await == Some(true), format!("{r:?}"));

    let r = call(&c, PAD, "org.bluez.Device1", "Connect", &()).await;
    let now = get_bool(&c, PAD, "org.bluez.Device1", "Connected").await;
    t.check("connect", r.is_ok() && now == Some(true), format!("{r:?} Connected={now:?}"));
    // Still up after btd's 5 s idle-link timeout: the shim holds a channel.
    tokio::time::sleep(Duration::from_secs(7)).await;
    let now = get_bool(&c, PAD, "org.bluez.Device1", "Connected").await;
    t.check("connect_held", now == Some(true), format!("Connected={now:?}"));

    let r = call(&c, PAD, "org.bluez.Device1", "Disconnect", &()).await;
    let sig_ok = wait_prop(&mut sig, PAD, "Connected", false, 5).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let now = get_bool(&c, PAD, "org.bluez.Device1", "Connected").await;
    t.check("disconnect", r.is_ok() && sig_ok && now == Some(false), format!("{r:?} signal={sig_ok} Connected={now:?}"));

    let pad = ObjectPath::try_from(PAD).unwrap();
    let r = call(&c, ADAPTER, "org.bluez.Adapter1", "RemoveDevice", &(&pad,)).await;
    let t0 = Instant::now();
    let mut removed = false;
    while !removed && t0.elapsed() < Duration::from_secs(5) {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(1), sig.next()).await else { continue };
        if msg.header().member().map(|m| m.as_str()) == Some("InterfacesRemoved") {
            removed |= msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>().map(|(p, _)| p.as_str() == PAD).unwrap_or(false);
        }
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let gone = managed(&c).await.map(|m| !m.keys().any(|p| p.as_str() == PAD)).unwrap_or(false);
    let key = std::fs::read_to_string(KEYS).unwrap_or_default().contains("06:05:04:03:02:01");
    t.check("remove_device", r.is_ok() && removed && gone && !key, format!("{r:?} signal={removed} gone={gone} key={key}"));

    let _ = shim.kill();
    let _ = shim.wait();
    drop(serve.stdin.take());
    let _ = serve.wait();
    let _ = std::fs::remove_file(KEYS);
    println!("--- blueztest done: {} failure(s)", t.fail);
    (t.pass, t.fail)
}
