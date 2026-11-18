//! org.freedesktop.PackageKit: a PackageKit facade over apk, the backend of
//! cosmic-store (its `packagekit-zbus` client, `Connection::system()`).
//!
//! Alpine ships no PackageKit and PackageKit has no apk backend, so this
//! service speaks just enough of the D-Bus API (root object, Transaction
//! objects, the signals clients deserialize) on top of the real package
//! manager.
//!
//! * Read operations (Resolve, GetPackages, GetUpdates, SearchNames,
//!   GetDetails, WhatProvides) run `/sbin/apk.static --no-network ...` as the
//!   service user and never write anything. package_id is
//!   `name;version;arch;installed:alpine` or `...;available:alpine`.
//! * Write operations (RefreshCache, InstallPackages, RemovePackages,
//!   UpdatePackages) exec the set-user-ID-root `/usr/libexec/leandros-pk-helper`
//!   (src/bin/leandros-pk-helper.rs): it authorises the real uid (root or the
//!   wheel group), validates names and runs /usr/sbin/apk. A refusal (exit
//!   77) becomes ErrorCode not-authorized.
//! * InstallFiles and GetDetailsLocal answer ErrorCode not-supported and
//!   Finished(failed). Nothing pretends to succeed.
//!
//! Ordering. PackageKit clients subscribe to a transaction's signals only
//! after the method call returned, so every transaction replies first and
//! works in a spawned task, which waits 500 ms before its first signal,
//! then emits Package/Details/ItemProgress, ErrorCode, Finished, Destroy,
//! and removes the object a few seconds later.
//!
//! One stderr line per transaction (role, args, result) is the audit log.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::zvariant::{OwnedObjectPath, Value};

const NAME: &str = "org.freedesktop.PackageKit";
const ROOT_PATH: &str = "/org/freedesktop/PackageKit";
const TX_IFACE: &str = "org.freedesktop.PackageKit.Transaction";
pub(crate) const APK: &str = "/sbin/apk.static";
pub(crate) const HELPER: &str = "/usr/libexec/leandros-pk-helper";

// PkRoleEnum
const ROLE_DEPENDS_ON: u32 = 2;
const ROLE_GET_DETAILS: u32 = 3;
const ROLE_GET_FILES: u32 = 4;
const ROLE_GET_PACKAGES: u32 = 5;
const ROLE_GET_REPO_LIST: u32 = 6;
const ROLE_REQUIRED_BY: u32 = 7;
const ROLE_GET_UPDATE_DETAIL: u32 = 8;
const ROLE_GET_UPDATES: u32 = 9;
const ROLE_INSTALL_FILES: u32 = 10;
const ROLE_INSTALL_PACKAGES: u32 = 11;
const ROLE_REFRESH_CACHE: u32 = 13;
const ROLE_REMOVE_PACKAGES: u32 = 14;
const ROLE_RESOLVE: u32 = 17;
const ROLE_SEARCH_DETAILS: u32 = 18;
const ROLE_SEARCH_FILE: u32 = 19;
const ROLE_SEARCH_GROUP: u32 = 20;
const ROLE_SEARCH_NAME: u32 = 21;
const ROLE_UPDATE_PACKAGES: u32 = 22;
const ROLE_WHAT_PROVIDES: u32 = 23;
const ROLE_DOWNLOAD_PACKAGES: u32 = 25;
const ROLE_GET_DISTRO_UPGRADES: u32 = 26;
const ROLE_GET_CATEGORIES: u32 = 27;
const ROLE_REPAIR_SYSTEM: u32 = 29;
const ROLE_GET_DETAILS_LOCAL: u32 = 30;
const ROLE_GET_FILES_LOCAL: u32 = 31;
const ROLE_UPGRADE_SYSTEM: u32 = 33;

// PkStatusEnum
const STATUS_WAIT: u32 = 1;
const STATUS_RUNNING: u32 = 3;
const STATUS_QUERY: u32 = 4;
const STATUS_REMOVE: u32 = 6;
const STATUS_REFRESH_CACHE: u32 = 7;
const STATUS_INSTALL: u32 = 9;
const STATUS_UPDATE: u32 = 10;
const STATUS_FINISHED: u32 = 18;

// PkErrorEnum
const ERR_NOT_SUPPORTED: u32 = 3;
const ERR_INTERNAL: u32 = 4;
const ERR_ID_INVALID: u32 = 6;
const ERR_NOT_FOUND: u32 = 8;
const ERR_TRANSACTION: u32 = 16;
const ERR_NOT_AUTHORIZED: u32 = 48;

// PkExitEnum
const EXIT_SUCCESS: u32 = 1;
const EXIT_FAILED: u32 = 2;

// PkInfoEnum
const INFO_INSTALLED: u32 = 1;
const INFO_AVAILABLE: u32 = 2;
const INFO_NORMAL: u32 = 5;

// PkFilterEnum bits
const FILTER_NONE: u64 = 1 << 1;
const FILTER_INSTALLED: u64 = 1 << 2;
const FILTER_NOT_INSTALLED: u64 = 1 << 3;
const FILTER_NEWEST: u64 = 1 << 16;
const FILTER_ARCH: u64 = 1 << 18;

/// The helper's "not authorised" exit status.
pub(crate) const HELPER_EXIT_DENIED: i32 = 77;

fn log(msg: std::fmt::Arguments<'_>) {
    crate::log("packagekit", msg);
}

static NEXT_TX: AtomicU32 = AtomicU32::new(1);
static TX_LIST: Mutex<Vec<String>> = Mutex::new(Vec::new());

// ---------------------------------------------------------------- apk side

/// `[a-z0-9][a-z0-9._+-]*`, at most 128: the same rule the helper enforces.
pub(crate) fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'+' | b'-'))
}

pub(crate) fn arch() -> String {
    std::fs::read_to_string("/etc/apk/arch")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

/// "htop-3.4.1-r0" -> ("htop", "3.4.1-r0").
pub(crate) fn split_name_ver(s: &str) -> Option<(String, String)> {
    // Alpine versions end in -r<digits>; the name ends at the dash before the version.
    let mut end = s.len();
    if let Some(i) = s.rfind("-r") {
        let tail = &s[i + 2..];
        if !tail.is_empty() && tail.bytes().all(|c| c.is_ascii_digit()) {
            end = i;
        }
    }
    let j = s[..end].rfind('-')?;
    if j == 0 || !s[j + 1..].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((s[..j].to_string(), s[j + 1..].to_string()))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pkg {
    pub name: String,
    pub ver: String,
    pub arch: String,
    pub installed: bool,
}

impl Pkg {
    pub(crate) fn id(&self) -> String {
        format!("{};{};{};{}:alpine", self.name, self.ver, self.arch, if self.installed { "installed" } else { "available" })
    }
}

/// One line of `apk list`: `name-ver arch {origin} (license) [installed]`.
pub(crate) fn parse_list_line(line: &str) -> Option<Pkg> {
    let mut it = line.split_whitespace();
    let (name, ver) = split_name_ver(it.next()?)?;
    let arch = it.next()?.to_string();
    if arch.starts_with('{') || arch.starts_with('(') || arch.starts_with('[') {
        return None;
    }
    Some(Pkg { name, ver, arch, installed: line.contains("[installed]") })
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_apk_blocking(args: Vec<String>) -> Result<Out, String> {
    let o = Command::new(APK)
        .arg("--no-network")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {APK}: {e}"))?;
    Ok(Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    })
}

async fn run_apk(args: &[&str]) -> Result<Out, (u32, String)> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    match tokio::task::spawn_blocking(move || run_apk_blocking(args)).await {
        Ok(Ok(o)) => Ok(o),
        Ok(Err(e)) => Err((ERR_INTERNAL, e)),
        Err(e) => Err((ERR_INTERNAL, format!("apk task: {e}"))),
    }
}

/// `apk list [args]`, parsed, de-duplicated by id.
async fn apk_list(args: &[&str]) -> Result<Vec<Pkg>, (u32, String)> {
    let mut a = vec!["list"];
    a.extend_from_slice(args);
    let o = run_apk(&a).await?;
    if o.code != 0 && o.stdout.is_empty() {
        return Err((ERR_INTERNAL, format!("apk list failed ({}): {}", o.code, tail_lines(&o.stderr, 2))));
    }
    let mut seen = std::collections::HashSet::new();
    let mut v = Vec::new();
    for l in o.stdout.lines() {
        if let Some(p) = parse_list_line(l) {
            if seen.insert(p.id()) {
                v.push(p);
            }
        }
    }
    Ok(v)
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("; ")
}

fn filter_ok(filter: u64, installed: bool) -> bool {
    if filter & FILTER_INSTALLED != 0 && !installed {
        return false;
    }
    if filter & FILTER_NOT_INSTALLED != 0 && installed {
        return false;
    }
    true
}

fn name_of_id(id: &str) -> Result<String, (u32, String)> {
    let n = id.split(';').next().unwrap_or("");
    if valid_name(n) {
        Ok(n.to_string())
    } else {
        Err((ERR_ID_INVALID, format!("invalid package id '{id}'")))
    }
}

/// `apk info -d -w` output: "name-ver description:\n<text>\n\n" blocks.
fn parse_info(out: &str) -> HashMap<String, (String, String)> {
    let mut m: HashMap<String, (String, String)> = HashMap::new();
    let mut cur: Option<(String, String)> = None; // (name, section)
    for l in out.lines() {
        if let Some(h) = l.strip_suffix(" description:").map(|h| (h, "d")).or_else(|| l.strip_suffix(" webpage:").map(|h| (h, "w"))) {
            if let Some((n, _)) = split_name_ver(h.0.trim()) {
                cur = Some((n, h.1.to_string()));
                continue;
            }
        }
        if l.trim().is_empty() {
            cur = None;
            continue;
        }
        if let Some((n, sec)) = &cur {
            let e = m.entry(n.clone()).or_default();
            let slot = if sec == "d" { &mut e.0 } else { &mut e.1 };
            if !slot.is_empty() {
                slot.push('\n');
            }
            slot.push_str(l.trim());
        }
    }
    m
}

// ---------------------------------------------------------------- transactions

#[derive(Debug, Clone)]
enum Op {
    Refresh,
    GetPackages(u64),
    GetUpdates,
    Resolve(u64, Vec<String>),
    SearchNames(u64, Vec<String>),
    GetDetails(Vec<String>),
    /// Succeeds with no results (queries apk cannot answer).
    Empty,
    Install(Vec<String>),
    Remove(Vec<String>),
    Update(Vec<String>),
    Unsupported(&'static str),
}

struct TxState {
    role: u32,
    status: u32,
    percentage: u32,
    last_package: String,
    started: Option<Instant>,
    used: bool,
    flags: u64,
    uid: u32,
}

struct Tx {
    st: Arc<Mutex<TxState>>,
    path: String,
    n: u32,
}

#[derive(Clone)]
struct Ctx {
    conn: zbus::Connection,
    path: String,
    st: Arc<Mutex<TxState>>,
}

impl Ctx {
    async fn emit<B>(&self, member: &str, body: &B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        if let Err(e) = self.conn.emit_signal(None::<&str>, self.path.as_str(), TX_IFACE, member, body).await {
            log(format_args!("emit {member} on {}: {e}", self.path));
        }
    }

    async fn package(&self, p: &Pkg, info: Option<u32>, summary: &str) {
        let info = info.unwrap_or(if p.installed { INFO_INSTALLED } else { INFO_AVAILABLE });
        self.emit("Package", &(info, p.id().as_str(), summary)).await;
    }

    fn set_status(&self, status: u32) {
        self.st.lock().unwrap().status = status;
    }

    fn set_pct(&self, pct: u32) {
        self.st.lock().unwrap().percentage = pct.min(100);
    }
}

fn role_name(role: u32) -> &'static str {
    match role {
        ROLE_GET_DETAILS => "get-details",
        ROLE_GET_PACKAGES => "get-packages",
        ROLE_GET_UPDATES => "get-updates",
        ROLE_INSTALL_FILES => "install-files",
        ROLE_INSTALL_PACKAGES => "install-packages",
        ROLE_REFRESH_CACHE => "refresh-cache",
        ROLE_REMOVE_PACKAGES => "remove-packages",
        ROLE_RESOLVE => "resolve",
        ROLE_SEARCH_NAME => "search-name",
        ROLE_UPDATE_PACKAGES => "update-packages",
        ROLE_WHAT_PROVIDES => "what-provides",
        ROLE_GET_DETAILS_LOCAL => "get-details-local",
        _ => "other",
    }
}

async fn do_op(ctx: &Ctx, op: &Op) -> Result<(), (u32, String)> {
    match op {
        Op::Empty => Ok(()),
        Op::Unsupported(what) => Err((ERR_NOT_SUPPORTED, format!("{what} is not supported by the apk backend"))),
        Op::Refresh => {
            ctx.set_status(STATUS_REFRESH_CACHE);
            helper(ctx, "update", &[], STATUS_REFRESH_CACHE).await
        }
        Op::GetPackages(filter) => {
            ctx.set_status(STATUS_QUERY);
            let pk = if filter & FILTER_INSTALLED != 0 { apk_list(&["--installed"]).await? } else { apk_list(&[]).await? };
            for p in pk.iter().filter(|p| filter_ok(*filter, p.installed)) {
                ctx.package(p, None, "").await;
            }
            Ok(())
        }
        Op::GetUpdates => {
            ctx.set_status(STATUS_QUERY);
            for p in apk_list(&["--upgradable"]).await? {
                // The row is the newer, not yet installed version.
                ctx.package(&Pkg { installed: false, ..p }, Some(INFO_NORMAL), "").await;
            }
            Ok(())
        }
        Op::Resolve(filter, names) => {
            ctx.set_status(STATUS_QUERY);
            let mut found = false;
            for n in names.iter().filter(|n| valid_name(n)) {
                for p in apk_list(&[n.as_str()]).await?.into_iter().filter(|p| p.name == *n && filter_ok(*filter, p.installed)) {
                    found = true;
                    ctx.package(&p, None, "").await;
                }
            }
            if !found {
                // PackageKit reports package-not-found for a resolve with no hit.
                return Err((ERR_NOT_FOUND, format!("could not find package(s) {}", names.join(", "))));
            }
            Ok(())
        }
        Op::SearchNames(filter, terms) => {
            ctx.set_status(STATUS_QUERY);
            let all = apk_list(&[]).await?;
            let mut seen = std::collections::HashSet::new();
            for t in terms {
                let t = t.to_lowercase();
                for p in all.iter().filter(|p| p.name.contains(&t) && filter_ok(*filter, p.installed)) {
                    if seen.insert(p.id()) {
                        ctx.package(p, None, "").await;
                    }
                }
            }
            Ok(())
        }
        Op::GetDetails(ids) => {
            ctx.set_status(STATUS_QUERY);
            let names: Vec<String> = ids.iter().map(|i| name_of_id(i)).collect::<Result<_, _>>()?;
            let mut args = vec!["info", "-d", "-w"];
            args.extend(names.iter().map(String::as_str));
            let info = match run_apk(&args).await {
                Ok(o) => parse_info(&o.stdout),
                Err(_) => HashMap::new(),
            };
            for (id, n) in ids.iter().zip(&names) {
                let (desc, url) = info.get(n).cloned().unwrap_or_default();
                let summary = desc.lines().next().unwrap_or("").to_string();
                let mut d: HashMap<&str, Value<'_>> = HashMap::new();
                d.insert("package-id", Value::from(id.as_str()));
                d.insert("summary", Value::from(summary.as_str()));
                d.insert("description", Value::from(desc.as_str()));
                d.insert("url", Value::from(url.as_str()));
                ctx.emit("Details", &(d,)).await;
            }
            Ok(())
        }
        Op::Install(ids) => write_op(ctx, "add", ids, STATUS_INSTALL).await,
        Op::Remove(ids) => write_op(ctx, "del", ids, STATUS_REMOVE).await,
        Op::Update(ids) => write_op(ctx, "upgrade", ids, STATUS_UPDATE).await,
    }
}

async fn write_op(ctx: &Ctx, verb: &str, ids: &[String], status: u32) -> Result<(), (u32, String)> {
    let names: Vec<String> = ids.iter().map(|i| name_of_id(i)).collect::<Result<_, _>>()?;
    if names.is_empty() && verb != "upgrade" {
        return Err((ERR_ID_INVALID, "no package ids".into()));
    }
    ctx.set_status(status);
    helper(ctx, verb, &names, status).await
}

/// Run the setuid helper, streaming apk's "(i/n) Installing pkg (ver)" lines
/// as ItemProgress and Percentage (best effort), and map its exit status.
async fn helper(ctx: &Ctx, verb: &str, names: &[String], default_status: u32) -> Result<(), (u32, String)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (verb_s, names_v) = (verb.to_string(), names.to_vec());
    let task = tokio::task::spawn_blocking(move || -> Result<(i32, String, String), String> {
        let mut child = Command::new(HELPER)
            .arg(&verb_s)
            .args(&names_v)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run {HELPER}: {e}"))?;
        let mut err = child.stderr.take().unwrap();
        let t = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let mut all = String::new();
        for l in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
            all.push_str(&l);
            all.push('\n');
            let _ = tx.send(l);
        }
        let st = child.wait().map_err(|e| e.to_string())?;
        Ok((st.code().unwrap_or(-1), all, t.join().unwrap_or_default()))
    });
    let arch = arch();
    while let Some(l) = rx.recv().await {
        // "(3/10) Installing foo (1.2-r0)"
        let Some(rest) = l.strip_prefix('(') else { continue };
        let Some((frac, rest)) = rest.split_once(") ") else { continue };
        let Some((i, n)) = frac.split_once('/') else { continue };
        let (Ok(i), Ok(n)) = (i.parse::<u32>(), n.parse::<u32>()) else { continue };
        let mut w = rest.split_whitespace();
        let (Some(what), Some(pkg)) = (w.next(), w.next()) else { continue };
        let ver = w.next().map(|v| v.trim_matches(|c| c == '(' || c == ')')).unwrap_or("");
        let status = match what {
            "Purging" => STATUS_REMOVE,
            "Upgrading" | "Downgrading" => STATUS_UPDATE,
            "Installing" | "Reinstalling" => STATUS_INSTALL,
            _ => default_status,
        };
        let id = format!("{pkg};{ver};{arch};{}:alpine", if status == STATUS_REMOVE { "installed" } else { "available" });
        ctx.set_status(status);
        ctx.st.lock().unwrap().last_package = id.clone();
        ctx.set_pct(i.saturating_sub(1) * 100 / n.max(1));
        ctx.emit("ItemProgress", &(id.as_str(), status, 0u32)).await;
        ctx.set_pct(i * 100 / n.max(1));
        ctx.emit("ItemProgress", &(id.as_str(), status, 100u32)).await;
    }
    let (code, stdout, stderr) = match task.await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err((ERR_INTERNAL, e)),
        Err(e) => return Err((ERR_INTERNAL, format!("helper task: {e}"))),
    };
    if code == 0 {
        ctx.set_pct(100);
        return Ok(());
    }
    let msg = {
        let e = tail_lines(&stderr, 3);
        if e.is_empty() { tail_lines(&stdout, 3) } else { e }
    };
    if code == HELPER_EXIT_DENIED {
        return Err((ERR_NOT_AUTHORIZED, msg));
    }
    let low = format!("{stdout}\n{stderr}").to_lowercase();
    let pk = if low.contains("no such package") || low.contains("unable to select") { ERR_NOT_FOUND } else { ERR_TRANSACTION };
    Err((pk, format!("{verb} failed ({code}): {msg}")))
}

async fn run_tx(ctx: Ctx, n: u32, role: u32, op: Op) {
    let t0 = Instant::now();
    ctx.st.lock().unwrap().started = Some(t0);
    ctx.set_status(STATUS_WAIT);
    // The client subscribes to our signals after our reply; give it time.
    tokio::time::sleep(Duration::from_millis(500)).await;
    ctx.set_status(STATUS_RUNNING);
    let res = do_op(&ctx, &op).await;
    let args = format!("{op:?}");
    let args = if args.len() > 200 { format!("{}...", &args[..200]) } else { args };
    let exit = match &res {
        Ok(()) => {
            log(format_args!("tx {n} role={} args={args} result=ok", role_name(role)));
            EXIT_SUCCESS
        }
        Err((code, msg)) => {
            log(format_args!("tx {n} role={} args={args} result=error code={code} msg={msg}", role_name(role)));
            ctx.emit("ErrorCode", &(*code, msg.as_str())).await;
            EXIT_FAILED
        }
    };
    ctx.set_status(STATUS_FINISHED);
    ctx.emit("Finished", &(exit, t0.elapsed().as_millis() as u32)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    ctx.emit("Destroy", &()).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let _ = ctx.conn.object_server().remove::<Tx, _>(ctx.path.as_str()).await;
    TX_LIST.lock().unwrap().retain(|p| *p != ctx.path);
}

impl Tx {
    fn start(&self, conn: &zbus::Connection, role: u32, op: Op) -> zbus::fdo::Result<()> {
        {
            let mut st = self.st.lock().unwrap();
            if st.used {
                return Err(zbus::fdo::Error::Failed("transaction already used".into()));
            }
            st.used = true;
            st.role = role;
        }
        let ctx = Ctx { conn: conn.clone(), path: self.path.clone(), st: self.st.clone() };
        tokio::spawn(run_tx(ctx, self.n, role, op));
        Ok(())
    }
}

#[zbus::interface(name = "org.freedesktop.PackageKit.Transaction")]
impl Tx {
    fn set_hints(&self, _hints: Vec<String>) {}
    fn cancel(&self) {}
    async fn refresh_cache(&self, #[zbus(connection)] c: &zbus::Connection, _force: bool) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_REFRESH_CACHE, Op::Refresh)
    }
    async fn get_packages(&self, #[zbus(connection)] c: &zbus::Connection, filter: u64) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_PACKAGES, Op::GetPackages(filter))
    }
    async fn get_updates(&self, #[zbus(connection)] c: &zbus::Connection, _filter: u64) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_UPDATES, Op::GetUpdates)
    }
    async fn resolve(&self, #[zbus(connection)] c: &zbus::Connection, filter: u64, packages: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_RESOLVE, Op::Resolve(filter, packages))
    }
    async fn search_names(&self, #[zbus(connection)] c: &zbus::Connection, filter: u64, values: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_SEARCH_NAME, Op::SearchNames(filter, values))
    }
    async fn get_details(&self, #[zbus(connection)] c: &zbus::Connection, package_ids: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_DETAILS, Op::GetDetails(package_ids))
    }
    async fn what_provides(&self, #[zbus(connection)] c: &zbus::Connection, _filter: u64, _values: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_WHAT_PROVIDES, Op::Empty)
    }
    async fn install_packages(&self, #[zbus(connection)] c: &zbus::Connection, flags: u64, package_ids: Vec<String>) -> zbus::fdo::Result<()> {
        self.st.lock().unwrap().flags = flags;
        self.start(c, ROLE_INSTALL_PACKAGES, Op::Install(package_ids))
    }
    async fn remove_packages(
        &self,
        #[zbus(connection)] c: &zbus::Connection,
        flags: u64,
        package_ids: Vec<String>,
        _allow_deps: bool,
        _autoremove: bool,
    ) -> zbus::fdo::Result<()> {
        self.st.lock().unwrap().flags = flags;
        self.start(c, ROLE_REMOVE_PACKAGES, Op::Remove(package_ids))
    }
    async fn update_packages(&self, #[zbus(connection)] c: &zbus::Connection, flags: u64, package_ids: Vec<String>) -> zbus::fdo::Result<()> {
        self.st.lock().unwrap().flags = flags;
        self.start(c, ROLE_UPDATE_PACKAGES, Op::Update(package_ids))
    }
    async fn install_files(&self, #[zbus(connection)] c: &zbus::Connection, _flags: u64, _full_paths: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_INSTALL_FILES, Op::Unsupported("InstallFiles"))
    }
    async fn get_details_local(&self, #[zbus(connection)] c: &zbus::Connection, _files: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_DETAILS_LOCAL, Op::Unsupported("GetDetailsLocal"))
    }
    // Queries apk cannot answer: an honest empty result.
    async fn search_details(&self, #[zbus(connection)] c: &zbus::Connection, _filter: u64, _values: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_SEARCH_DETAILS, Op::Empty)
    }
    async fn search_files(&self, #[zbus(connection)] c: &zbus::Connection, _filter: u64, _values: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_SEARCH_FILE, Op::Empty)
    }
    async fn search_groups(&self, #[zbus(connection)] c: &zbus::Connection, _filter: u64, _values: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_SEARCH_GROUP, Op::Empty)
    }
    async fn depends_on(&self, #[zbus(connection)] c: &zbus::Connection, _filter: u64, _ids: Vec<String>, _recursive: bool) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_DEPENDS_ON, Op::Empty)
    }
    async fn required_by(&self, #[zbus(connection)] c: &zbus::Connection, _filter: u64, _ids: Vec<String>, _recursive: bool) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_REQUIRED_BY, Op::Empty)
    }
    async fn get_files(&self, #[zbus(connection)] c: &zbus::Connection, _ids: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_FILES, Op::Empty)
    }
    async fn get_files_local(&self, #[zbus(connection)] c: &zbus::Connection, _files: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_FILES_LOCAL, Op::Empty)
    }
    async fn get_update_detail(&self, #[zbus(connection)] c: &zbus::Connection, _ids: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_UPDATE_DETAIL, Op::Empty)
    }
    async fn get_repo_list(&self, #[zbus(connection)] c: &zbus::Connection, _filter: u64) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_REPO_LIST, Op::Empty)
    }
    async fn get_categories(&self, #[zbus(connection)] c: &zbus::Connection) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_CATEGORIES, Op::Empty)
    }
    async fn get_distro_upgrades(&self, #[zbus(connection)] c: &zbus::Connection) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_GET_DISTRO_UPGRADES, Op::Empty)
    }
    async fn download_packages(&self, #[zbus(connection)] c: &zbus::Connection, _store: bool, _ids: Vec<String>) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_DOWNLOAD_PACKAGES, Op::Unsupported("DownloadPackages"))
    }
    async fn repair_system(&self, #[zbus(connection)] c: &zbus::Connection, _flags: u64) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_REPAIR_SYSTEM, Op::Unsupported("RepairSystem"))
    }
    async fn upgrade_system(&self, #[zbus(connection)] c: &zbus::Connection, _flags: u64, _distro: String, _kind: u32) -> zbus::fdo::Result<()> {
        self.start(c, ROLE_UPGRADE_SYSTEM, Op::Unsupported("UpgradeSystem"))
    }

    #[zbus(property)]
    fn role(&self) -> u32 {
        self.st.lock().unwrap().role
    }
    #[zbus(property)]
    fn status(&self) -> u32 {
        self.st.lock().unwrap().status
    }
    #[zbus(property)]
    fn percentage(&self) -> u32 {
        self.st.lock().unwrap().percentage
    }
    #[zbus(property)]
    fn allow_cancel(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn caller_active(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn download_size_remaining(&self) -> u64 {
        0
    }
    #[zbus(property)]
    fn elapsed_time(&self) -> u32 {
        self.st.lock().unwrap().started.map(|t| t.elapsed().as_secs() as u32).unwrap_or(0)
    }
    #[zbus(property)]
    fn last_package(&self) -> String {
        self.st.lock().unwrap().last_package.clone()
    }
    #[zbus(property)]
    fn remaining_time(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn speed(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn sender(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn transaction_flags(&self) -> u64 {
        self.st.lock().unwrap().flags
    }
    #[zbus(property)]
    fn uid(&self) -> u32 {
        self.st.lock().unwrap().uid
    }
}

// ---------------------------------------------------------------- root object

struct Root {
    distro: String,
}

#[zbus::interface(name = "org.freedesktop.PackageKit")]
impl Root {
    async fn create_transaction(&self, #[zbus(connection)] conn: &zbus::Connection) -> zbus::fdo::Result<OwnedObjectPath> {
        let n = NEXT_TX.fetch_add(1, Ordering::Relaxed);
        let path = format!("{ROOT_PATH}/Transactions/{n}");
        // SAFETY: plain getter.
        let uid = unsafe { libc::getuid() };
        let st = Arc::new(Mutex::new(TxState {
            role: 0,
            status: STATUS_WAIT,
            percentage: 0,
            last_package: String::new(),
            started: None,
            used: false,
            flags: 0,
            uid,
        }));
        conn.object_server().at(path.as_str(), Tx { st, path: path.clone(), n }).await?;
        TX_LIST.lock().unwrap().push(path.clone());
        OwnedObjectPath::try_from(path).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    fn get_transaction_list(&self) -> Vec<OwnedObjectPath> {
        TX_LIST.lock().unwrap().iter().filter_map(|p| OwnedObjectPath::try_from(p.as_str()).ok()).collect()
    }
    fn get_daemon_state(&self) -> String {
        "leandros-sysbus packagekit (apk backend)".into()
    }
    fn can_authorize(&self, _action_id: String) -> u32 {
        1 // yes: the helper decides by group
    }
    fn get_time_since_action(&self, _role: u32) -> u32 {
        0
    }
    fn state_has_changed(&self, _reason: String) {}
    fn suggest_daemon_quit(&self) {}
    fn set_proxy(&self, _http: String, _https: String, _ftp: String, _socks: String, _no_proxy: String, _pac: String) {}

    #[zbus(property)]
    fn version_major(&self) -> u32 {
        1
    }
    #[zbus(property)]
    fn version_minor(&self) -> u32 {
        2
    }
    #[zbus(property)]
    fn version_micro(&self) -> u32 {
        8
    }
    #[zbus(property)]
    fn backend_name(&self) -> String {
        "apk".into()
    }
    #[zbus(property)]
    fn backend_description(&self) -> String {
        "Alpine apk (LeandrOS PackageKit facade)".into()
    }
    #[zbus(property)]
    fn backend_author(&self) -> String {
        "LeandrOS".into()
    }
    #[zbus(property)]
    fn roles(&self) -> u64 {
        [
            ROLE_GET_DETAILS,
            ROLE_GET_PACKAGES,
            ROLE_GET_UPDATES,
            ROLE_INSTALL_PACKAGES,
            ROLE_REFRESH_CACHE,
            ROLE_REMOVE_PACKAGES,
            ROLE_RESOLVE,
            ROLE_SEARCH_NAME,
            ROLE_UPDATE_PACKAGES,
            ROLE_WHAT_PROVIDES,
        ]
        .iter()
        .fold(0u64, |a, r| a | (1u64 << r))
    }
    #[zbus(property)]
    fn filters(&self) -> u64 {
        FILTER_NONE | FILTER_INSTALLED | FILTER_NOT_INSTALLED | FILTER_NEWEST | FILTER_ARCH
    }
    #[zbus(property)]
    fn groups(&self) -> u64 {
        0
    }
    #[zbus(property)]
    fn mime_types(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn locked(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn network_state(&self) -> u32 {
        2 // online
    }
    #[zbus(property)]
    fn distro_id(&self) -> String {
        self.distro.clone()
    }
}

fn distro_id() -> String {
    let ver = std::fs::read_to_string("/etc/alpine-release").map(|s| s.trim().to_string()).unwrap_or_else(|_| "unknown".into());
    format!("alpine;{ver};{}", arch())
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    builder.serve_at(ROOT_PATH, Root { distro: distro_id() })?.name(NAME)?.build().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_and_lines() {
        assert_eq!(split_name_ver("htop-3.4.1-r0"), Some(("htop".into(), "3.4.1-r0".into())));
        assert_eq!(split_name_ver("libstdc++-14.2.0-r6"), Some(("libstdc++".into(), "14.2.0-r6".into())));
        let p = parse_list_line("htop-3.4.1-r0 aarch64 {htop} (GPL-2.0-or-later) [installed]").unwrap();
        assert!(p.installed && p.name == "htop" && p.arch == "aarch64");
        assert_eq!(p.id(), "htop;3.4.1-r0;aarch64;installed:alpine");
        assert!(valid_name("a.b_c+d-e") && !valid_name("-x") && !valid_name("A"));
    }
}
