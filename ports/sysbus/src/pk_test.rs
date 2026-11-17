//! `leandros-sysbus pk-test`: the PackageKit facade end to end. Run as root
//! under `dbus-run-session` (DBUS_SYSTEM_BUS_ADDRESS falls back to the
//! session bus). Needs the network: it installs and removes htop. One
//! PASS/FAIL line per check, `PK_TEST pass=N fail=M` last.

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use crate::pkcon::{self, Call, TxResult, FILTER_ARCH, FILTER_NEWEST, FILTER_NONE, FILTER_NOT_INSTALLED};
use crate::packagekit::{HELPER, HELPER_EXIT_DENIED};

const T: Duration = Duration::from_secs(120);
const LONG: Duration = Duration::from_secs(600);

fn ok_tx(r: TxResult) -> Result<TxResult, String> {
    match r.error {
        Some((c, m)) => Err(format!("ErrorCode {} ({}): {m}", pkcon::error_name(c), c)),
        None if r.exit != 1 => Err(format!("Finished exit={}", r.exit)),
        None => Ok(r),
    }
}

async fn tx(c: &zbus::Connection, call: Call, t: Duration) -> Result<TxResult, String> {
    ok_tx(pkcon::transaction(c, call, t).await.map_err(|e| e.to_string())?)
}

fn names(r: &TxResult, info: u32) -> Vec<String> {
    r.packages.iter().filter(|p| p.0 == info).map(|p| pkcon::pretty_id(&p.1).0).collect()
}

async fn is_installed(c: &zbus::Connection, name: &str) -> Result<bool, String> {
    match pkcon::transaction(c, Call::Resolve(FILTER_NONE, vec![name.into()]), T).await.map_err(|e| e.to_string())? {
        r if r.error.is_some() => Ok(false),
        r => Ok(r.packages.iter().any(|p| p.0 == 1)),
    }
}

/// Run the helper as an unprivileged uid with no groups; returns its exit status.
fn helper_as(uid: u32, args: &[&str]) -> Result<i32, String> {
    let mut cmd = Command::new(HELPER);
    cmd.args(args);
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(uid) != 0 || libc::setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let st = cmd.output().map_err(|e| format!("{e}"))?;
    Ok(st.status.code().unwrap_or(-1))
}

pub async fn run(c: zbus::Connection) -> (u32, u32) {
    let (mut pass, mut fail) = (0u32, 0u32);
    macro_rules! check {
        ($name:expr, $fut:expr) => {{
            match $fut.await {
                Ok(v) => {
                    pass += 1;
                    println!("PASS {:<36} {}", $name, v);
                }
                Err(e) => {
                    fail += 1;
                    println!("FAIL {:<36} {}", $name, e);
                }
            }
        }};
    }
    let c = &c;

    check!("activation + root properties", async {
        let p = zbus::fdo::PropertiesProxy::builder(c)
            .destination("org.freedesktop.PackageKit".to_owned())?
            .path("/org/freedesktop/PackageKit".to_owned())?
            .build()
            .await?;
        let m = tokio::time::timeout(
            Duration::from_secs(60),
            p.get_all(zbus::names::InterfaceName::try_from("org.freedesktop.PackageKit".to_owned())?),
        )
        .await
        .map_err(|_| "timeout activating org.freedesktop.PackageKit".to_string())??;
        let backend: String = m.get("BackendName").and_then(|v| v.downcast_ref::<String>().ok()).ok_or("no BackendName")?;
        if backend != "apk" {
            return Err(format!("BackendName={backend}").into());
        }
        Ok::<_, Box<dyn std::error::Error>>(format!("backend={backend} props={}", m.len()))
    });
    check!("resolve musl -> installed", async {
        let r = tx(c, Call::Resolve(FILTER_NONE, vec!["musl".into()]), T).await?;
        let n = names(&r, 1);
        if n.is_empty() {
            return Err(format!("musl not installed: {:?}", r.packages));
        }
        Ok(n.join(","))
    });
    check!("refresh cache", async {
        tx(c, Call::RefreshCache(true), LONG).await.map(|_| "ok".to_string())
    });
    check!("search name htop -> available", async {
        let r = tx(c, Call::SearchNames(FILTER_NONE, vec!["htop".into()]), T).await?;
        let all: Vec<String> = r.packages.iter().map(|p| p.1.clone()).collect();
        if r.packages.iter().any(|p| pkcon::pretty_id(&p.1).0.starts_with("htop-")) {
            Ok(format!("{} hits", all.len()))
        } else {
            Err(format!("htop not found in {} hits", all.len()))
        }
    });
    check!("install htop", async {
        let ids: Vec<String> = tx(c, Call::Resolve(FILTER_NOT_INSTALLED | FILTER_NEWEST | FILTER_ARCH, vec!["htop".into()]), T)
            .await?
            .packages
            .into_iter()
            .map(|p| p.1)
            .collect();
        if ids.is_empty() {
            return Err("htop did not resolve to an available package".to_string());
        }
        tx(c, Call::InstallPackages(ids.clone()), LONG).await?;
        if !std::path::Path::new("/usr/bin/htop").exists() {
            return Err("/usr/bin/htop missing after install".to_string());
        }
        if !is_installed(c, "htop").await? {
            return Err("resolve htop does not say installed".to_string());
        }
        Ok(ids.join(","))
    });
    check!("get-packages installed has htop", async {
        let r = tx(c, Call::GetPackages(1 << 2), T).await?;
        if names(&r, 1).iter().any(|n| n.starts_with("htop-")) {
            Ok(format!("{} installed", r.packages.len()))
        } else {
            Err("htop missing from the installed list".to_string())
        }
    });
    check!("remove htop", async {
        let r = tx(c, Call::Resolve(1 << 2, vec!["htop".into()]), T).await?;
        let ids: Vec<String> = r.packages.into_iter().map(|p| p.1).collect();
        if ids.is_empty() {
            return Err("htop did not resolve to an installed package".to_string());
        }
        tx(c, Call::RemovePackages(ids), LONG).await?;
        if std::path::Path::new("/usr/bin/htop").exists() {
            return Err("/usr/bin/htop still present after remove".to_string());
        }
        if is_installed(c, "htop").await? {
            return Err("resolve htop still says installed".to_string());
        }
        Ok("gone".to_string())
    });
    check!("unprivileged refused (exit 77)", async {
        let rc = helper_as(65534, &["add", "htop"])?;
        if rc == HELPER_EXIT_DENIED {
            Ok(format!("exit {rc}"))
        } else {
            Err(format!("helper exit {rc}, wanted {HELPER_EXIT_DENIED}"))
        }
    });
    check!("base removal refused", async {
        let rc = helper_as(0, &["del", "musl"])?;
        if rc == 0 { Err("helper del musl exited 0".to_string()) } else { Ok(format!("exit {rc}")) }
    });
    check!("InstallFiles not-supported", async {
        let r = pkcon::transaction(c, Call::InstallFiles(vec!["/tmp/x.apk".into()]), T).await.map_err(|e| e.to_string())?;
        match r.error {
            Some((3, _)) if r.exit == 2 => Ok("ErrorCode not-supported, Finished failed".to_string()),
            other => Err(format!("error={other:?} exit={}", r.exit)),
        }
    });

    println!("PK_TEST pass={pass} fail={fail}");
    (pass, fail)
}
