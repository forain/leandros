//! `leandros-sysbus pkcon`: a small PackageKit command-line client (the
//! `pkcon` look-alike /usr/bin/pkcon execs it) and the transaction client
//! `pk-test` reuses. It talks to org.freedesktop.PackageKit like any
//! PackageKit client: CreateTransaction, subscribe to the transaction's
//! signals, call the method, collect signals until Finished.
//!
//!   pkcon search name <term>        pkcon resolve <names>
//!   pkcon install [-y] <names>      pkcon remove [-y] <names>
//!   pkcon get-packages [--filter installed]   pkcon get-updates
//!   pkcon refresh                   pkcon get-details <names>
//!
//! Exit status 0 on success, 1 on any ErrorCode or failure, 2 on usage.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const DEST: &str = "org.freedesktop.PackageKit";
const ROOT: &str = "/org/freedesktop/PackageKit";
const TX_IFACE: &str = "org.freedesktop.PackageKit.Transaction";

pub const FILTER_NONE: u64 = 1 << 1;
pub const FILTER_INSTALLED: u64 = 1 << 2;
pub const FILTER_NOT_INSTALLED: u64 = 1 << 3;
pub const FILTER_NEWEST: u64 = 1 << 16;
pub const FILTER_ARCH: u64 = 1 << 18;
const ONLY_TRUSTED: u64 = 1 << 1;

#[derive(Default, Debug)]
pub struct TxResult {
    /// (info, package_id, summary)
    pub packages: Vec<(u32, String, String)>,
    pub details: Vec<HashMap<String, OwnedValue>>,
    pub error: Option<(u32, String)>,
    pub exit: u32,
    pub progress: u32,
}

/// Which PackageKit call a transaction makes.
pub enum Call {
    RefreshCache(bool),
    GetPackages(u64),
    GetUpdates,
    Resolve(u64, Vec<String>),
    SearchNames(u64, Vec<String>),
    GetDetails(Vec<String>),
    InstallPackages(Vec<String>),
    RemovePackages(Vec<String>),
    InstallFiles(Vec<String>),
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

/// Run one transaction to Finished. Subscribes before calling, so no signal
/// is missed, and gives up after `timeout`.
pub async fn transaction(c: &zbus::Connection, call: Call, timeout: Duration) -> zbus::Result<TxResult> {
    let fail = |m: String| zbus::Error::Failure(m);
    let reply = c.call_method(Some(DEST), ROOT, Some(DEST), "CreateTransaction", &()).await?;
    let path: OwnedObjectPath = reply.body().deserialize()?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(TX_IFACE)?
        .path(path.as_str())?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, c, None).await?;
    let p = path.as_str();
    let hints: Vec<&str> = vec!["interactive=false"];
    c.call_method(Some(DEST), p, Some(TX_IFACE), "SetHints", &(hints,)).await?;
    match &call {
        Call::RefreshCache(force) => c.call_method(Some(DEST), p, Some(TX_IFACE), "RefreshCache", &(*force,)).await?,
        Call::GetPackages(f) => c.call_method(Some(DEST), p, Some(TX_IFACE), "GetPackages", &(*f,)).await?,
        Call::GetUpdates => c.call_method(Some(DEST), p, Some(TX_IFACE), "GetUpdates", &(FILTER_NONE,)).await?,
        Call::Resolve(f, n) => c.call_method(Some(DEST), p, Some(TX_IFACE), "Resolve", &(*f, refs(n))).await?,
        Call::SearchNames(f, n) => c.call_method(Some(DEST), p, Some(TX_IFACE), "SearchNames", &(*f, refs(n))).await?,
        Call::GetDetails(n) => c.call_method(Some(DEST), p, Some(TX_IFACE), "GetDetails", &(refs(n),)).await?,
        Call::InstallPackages(n) => c.call_method(Some(DEST), p, Some(TX_IFACE), "InstallPackages", &(ONLY_TRUSTED, refs(n))).await?,
        Call::InstallFiles(n) => c.call_method(Some(DEST), p, Some(TX_IFACE), "InstallFiles", &(0u64, refs(n))).await?,
        Call::RemovePackages(n) => c.call_method(Some(DEST), p, Some(TX_IFACE), "RemovePackages", &(0u64, refs(n), true, true)).await?,
    };
    let mut r = TxResult::default();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let msg = match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_) => return Err(fail(format!("transaction timed out after {} s", timeout.as_secs()))),
            Ok(None) => return Err(fail("bus connection closed".into())),
            Ok(Some(m)) => m?,
        };
        let Some(member) = msg.header().member().map(|m| m.to_string()) else { continue };
        let body = msg.body();
        match member.as_str() {
            "Package" => r.packages.push(body.deserialize::<(u32, String, String)>()?),
            "Details" => r.details.push(body.deserialize::<HashMap<String, OwnedValue>>()?),
            "ErrorCode" => r.error = Some(body.deserialize::<(u32, String)>()?),
            "ItemProgress" => r.progress = body.deserialize::<(String, u32, u32)>()?.2,
            "Finished" => {
                r.exit = body.deserialize::<(u32, u32)>()?.0;
                return Ok(r);
            }
            _ => {}
        }
    }
}

pub fn error_name(code: u32) -> &'static str {
    match code {
        3 => "not-supported",
        4 => "internal-error",
        6 => "package-id-invalid",
        8 => "package-not-found",
        9 => "package-already-installed",
        16 => "transaction-error",
        48 => "not-authorized",
        _ => "error",
    }
}

/// `htop;3.4.1-r0;aarch64;installed:alpine` -> ("htop-3.4.1-r0.aarch64", "alpine").
pub fn pretty_id(id: &str) -> (String, String) {
    let f: Vec<&str> = id.split(';').collect();
    if f.len() < 4 {
        return (id.to_string(), String::new());
    }
    (format!("{}-{}.{}", f[0], f[1], f[2]), f[3].rsplit(':').next().unwrap_or("").to_string())
}

fn label(info: u32) -> &'static str {
    match info {
        1 => "Installed",
        2 => "Available",
        5 => "Update",
        9 => "Installing",
        _ => "Package",
    }
}

pub fn print_packages(r: &TxResult) {
    for (info, id, _summary) in &r.packages {
        let (n, repo) = pretty_id(id);
        println!("{:<10} {n} ({repo})", label(*info));
    }
}

const LONG: Duration = Duration::from_secs(900);

async fn resolve_ids(c: &zbus::Connection, names: &[String], filter: u64) -> Result<Vec<String>, String> {
    let r = transaction(c, Call::Resolve(filter, names.to_vec()), Duration::from_secs(120)).await.map_err(|e| e.to_string())?;
    if let Some((code, msg)) = r.error {
        return Err(format!("{} ({}): {msg}", error_name(code), code));
    }
    Ok(r.packages.into_iter().map(|p| p.1).collect())
}

/// Reports an ErrorCode the way pkcon does and returns whether the
/// transaction succeeded.
fn report(r: &TxResult) -> bool {
    if let Some((code, msg)) = &r.error {
        eprintln!("Command failed: {msg} ({})", error_name(*code));
        return false;
    }
    r.exit == 1
}

fn usage() -> bool {
    eprintln!("usage: pkcon <search name TERM|resolve NAMES|install [-y] NAMES|remove [-y] NAMES|get-packages [--filter installed]|get-updates|refresh|get-details NAMES>");
    false
}

/// Returns Some(success), or None for a usage error.
pub async fn run(c: &zbus::Connection, args: Vec<String>) -> Option<bool> {
    let mut a: Vec<String> = args.into_iter().filter(|s| s != "-y" && s != "--noninteractive" && s != "-p").collect();
    if a.is_empty() {
        return None;
    }
    let cmd = a.remove(0);
    let ok = match cmd.as_str() {
        "search" => {
            if a.len() < 2 || a[0] != "name" {
                return None;
            }
            run_call(c, Call::SearchNames(FILTER_NONE, a[1..].to_vec())).await
        }
        "resolve" if !a.is_empty() => run_call(c, Call::Resolve(FILTER_NONE, a)).await,
        "get-packages" => {
            let installed = a.windows(2).any(|w| w[0] == "--filter" && w[1] == "installed");
            run_call(c, Call::GetPackages(if installed { FILTER_INSTALLED } else { FILTER_NONE })).await
        }
        "get-updates" => run_call(c, Call::GetUpdates).await,
        "refresh" => run_call(c, Call::RefreshCache(true)).await,
        "get-details" if !a.is_empty() => {
            let ids = match resolve_ids(c, &a, FILTER_NONE).await {
                Ok(i) if !i.is_empty() => i,
                Ok(_) => {
                    eprintln!("Command failed: no such package");
                    return Some(false);
                }
                Err(e) => {
                    eprintln!("Command failed: {e}");
                    return Some(false);
                }
            };
            match transaction(c, Call::GetDetails(ids), LONG).await {
                Ok(r) => {
                    for d in &r.details {
                        for k in ["package-id", "summary", "url", "description"] {
                            if let Some(v) = d.get(k) {
                                if let zbus::zvariant::Value::Str(s) = &**v {
                                    println!("{k}: {s}");
                                }
                            }
                        }
                        println!();
                    }
                    report(&r)
                }
                Err(e) => {
                    eprintln!("Command failed: {e}");
                    false
                }
            }
        }
        "install" | "remove" if !a.is_empty() => {
            let install = cmd == "install";
            let filter = if install { FILTER_NOT_INSTALLED | FILTER_NEWEST | FILTER_ARCH } else { FILTER_INSTALLED };
            let ids = match resolve_ids(c, &a, filter).await {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("Command failed: {e}");
                    return Some(false);
                }
            };
            if ids.is_empty() {
                // Resolved to nothing under the filter: already in the wanted state.
                println!("Nothing to do: {} already {}", a.join(" "), if install { "installed" } else { "removed" });
                return Some(true);
            }
            for id in &ids {
                let (n, repo) = pretty_id(id);
                println!("{:<10} {n} ({repo})", if install { "Installing" } else { "Removing" });
            }
            let call = if install { Call::InstallPackages(ids) } else { Call::RemovePackages(ids) };
            match transaction(c, call, LONG).await {
                Ok(r) => {
                    let ok = report(&r);
                    if ok {
                        println!("Finished");
                    }
                    ok
                }
                Err(e) => {
                    eprintln!("Command failed: {e}");
                    false
                }
            }
        }
        _ => return None,
    };
    Some(ok)
}

async fn run_call(c: &zbus::Connection, call: Call) -> bool {
    match transaction(c, call, LONG).await {
        Ok(r) => {
            print_packages(&r);
            report(&r)
        }
        Err(e) => {
            eprintln!("Command failed: {e}");
            false
        }
    }
}

/// Entry from main: usage errors return None.
pub async fn main_run(c: zbus::Connection, args: Vec<String>) -> Option<bool> {
    let r = run(&c, args).await;
    if r.is_none() {
        usage();
    }
    r
}
