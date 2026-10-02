//! `leandros-sysbus portal <settings|screenshot|open-file|all> [timeout_s]`:
//! a client of org.freedesktop.portal.Desktop (ports/portal), calling it the
//! way libcosmic/ashpd do, for in-guest verification.
//!
//!   settings    Settings.ReadOne color-scheme / accent-color / contrast, and
//!               ReadAll(["org.freedesktop.appearance"])
//!   screenshot  Screenshot.Screenshot(interactive=false); waits for the
//!               Request::Response, then checks the returned file is a PNG
//!               and prints its size
//!   open-file   FileChooser.OpenFile; waits (default 300 s) for the user to
//!               pick a file in the dialog and prints the returned URIs
//!
//! One `PASS`/`FAIL` line per check and a `PORTAL_PROBE pass=N fail=M` summary.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use zbus::zvariant::{OwnedValue, Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";

fn fail(msg: impl Into<String>) -> zbus::Error {
    zbus::Error::Failure(msg.into())
}

async fn read_one(c: &zbus::Connection, ns: &str, key: &str) -> zbus::Result<OwnedValue> {
    let reply = c
        .call_method(Some(DEST), PATH, Some("org.freedesktop.portal.Settings"), "ReadOne", &(ns, key))
        .await?;
    reply.body().deserialize::<OwnedValue>()
}

/// Call a request-returning portal method and wait for its Response signal.
/// Subscribes BEFORE the call, on the request path the spec derives from our
/// unique name and handle_token, so a fast backend cannot win the race.
async fn request(
    c: &zbus::Connection,
    iface: &str,
    method: &str,
    token: &str,
    body: impl serde::Serialize + zbus::zvariant::DynamicType,
    timeout: Duration,
) -> zbus::Result<(u32, HashMap<String, OwnedValue>)> {
    let unique = c.unique_name().ok_or_else(|| fail("no unique name"))?;
    let sender = unique.as_str().trim_start_matches(':').replace('.', "_");
    let req_path = format!("{PATH}/request/{sender}/{token}");
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")?
        .member("Response")?
        .path(req_path.clone())?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, c, Some(4)).await?;
    let reply = c.call_method(Some(DEST), PATH, Some(iface), method, &body).await?;
    let handle: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
    if handle.as_str() != req_path {
        return Err(fail(format!("handle {} != expected {req_path}", handle.as_str())));
    }
    let msg = tokio::time::timeout(timeout, stream.next())
        .await
        .map_err(|_| fail(format!("no Response within {} s", timeout.as_secs())))?
        .ok_or_else(|| fail("signal stream ended"))??;
    msg.body().deserialize::<(u32, HashMap<String, OwnedValue>)>()
}

fn uri_list(r: &HashMap<String, OwnedValue>) -> Vec<String> {
    r.get("uris")
        .cloned()
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default()
}

fn uri_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // percent-decoding: enough for paths the portal hands out
    let mut out = Vec::new();
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

pub async fn run(conn: zbus::Connection, what: &str, timeout_s: u64) -> (u32, u32) {
    let (mut pass, mut failn) = (0u32, 0u32);
    macro_rules! check {
        ($name:expr, $fut:expr) => {{
            let t = Instant::now();
            match $fut.await {
                Ok(v) => {
                    pass += 1;
                    println!("PASS {:<40} {:>6} ms  {}", $name, t.elapsed().as_millis(), v);
                }
                Err(e) => {
                    failn += 1;
                    println!("FAIL {:<40} {:>6} ms  {}", $name, t.elapsed().as_millis(), e);
                }
            }
        }};
    }
    let c = &conn;
    let all = what == "all";

    if all || what == "settings" {
        check!("Settings.ReadOne color-scheme", async {
            let v = read_one(c, "org.freedesktop.appearance", "color-scheme").await?;
            let n: u32 = v.try_into()?;
            let name = match n { 0 => "no-preference", 1 => "dark", 2 => "light", _ => "?" };
            Ok::<_, zbus::Error>(format!("{n} ({name})"))
        });
        check!("Settings.ReadOne accent-color", async {
            let v = read_one(c, "org.freedesktop.appearance", "accent-color").await?;
            let (r, g, b): (f64, f64, f64) = v.try_into()?;
            Ok::<_, zbus::Error>(format!("({r:.3}, {g:.3}, {b:.3})"))
        });
        check!("Settings.ReadOne contrast", async {
            let v = read_one(c, "org.freedesktop.appearance", "contrast").await?;
            let n: u32 = v.try_into()?;
            Ok::<_, zbus::Error>(format!("{n}"))
        });
        check!("Settings.ReadAll appearance", async {
            let reply = c
                .call_method(Some(DEST), PATH, Some("org.freedesktop.portal.Settings"), "ReadAll",
                    &(vec!["org.freedesktop.appearance"],))
                .await?;
            let m: HashMap<String, HashMap<String, OwnedValue>> = reply.body().deserialize()?;
            let keys: Vec<String> = m.values().flat_map(|k| k.keys().cloned()).collect();
            Ok::<_, zbus::Error>(format!("{} namespace(s), keys {keys:?}", m.len()))
        });
    }

    if all || what == "screenshot" {
        check!("Screenshot.Screenshot (non-interactive)", async {
            let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
            opts.insert("handle_token", "leandros_shot".into());
            opts.insert("interactive", false.into());
            opts.insert("modal", false.into());
            let (code, r) = request(c, "org.freedesktop.portal.Screenshot", "Screenshot", "leandros_shot",
                ("", opts), Duration::from_secs(timeout_s)).await?;
            if code != 0 {
                return Err(fail(format!("response {code} {r:?}")));
            }
            let uri: String = r.get("uri").cloned().ok_or_else(|| fail("no uri"))?.try_into()?;
            let path = uri_path(&uri).ok_or_else(|| fail(format!("not a file uri: {uri}")))?;
            let data = std::fs::read(&path).map_err(|e| fail(format!("{path}: {e}")))?;
            if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" {
                return Err(fail(format!("{path}: not a PNG ({} bytes)", data.len())));
            }
            let w = u32::from_be_bytes(data[16..20].try_into().unwrap());
            let h = u32::from_be_bytes(data[20..24].try_into().unwrap());
            Ok(format!("{uri} PNG {w}x{h}, {} bytes", data.len()))
        });
    }

    if what == "open-file" {
        let t = if timeout_s == 30 { 300 } else { timeout_s };
        check!("FileChooser.OpenFile", async {
            let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
            opts.insert("handle_token", "leandros_open".into());
            opts.insert("modal", false.into());
            println!("WAITING for a file to be picked in the dialog (up to {t} s)");
            let (code, r) = request(c, "org.freedesktop.portal.FileChooser", "OpenFile", "leandros_open",
                ("", "LeandrOS portal probe", opts), Duration::from_secs(t)).await?;
            let uris = uri_list(&r);
            match code {
                0 if !uris.is_empty() => {
                    let p = uri_path(&uris[0]).unwrap_or_default();
                    let ok = std::path::Path::new(&p).exists();
                    Ok(format!("uris={uris:?} exists={ok}"))
                }
                _ => Err(fail(format!("response {code} uris={uris:?}"))),
            }
        });
    }

    println!("PORTAL_PROBE pass={pass} fail={failn}");
    (pass, failn)
}
