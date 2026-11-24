//! `leandros-sysbus activation-test` and its helper `act-claim`: bus activation
//! against the reference dbus-daemon contract, independent of any portal.
//!
//! The test starts its own busd on a private socket with a private service
//! directory, then calls org.freedesktop.DBus.Peer.Ping on four names:
//!   * slow:  the service claims its name only after 40 s. dbus-daemon waits
//!     (service_start_timeout, 120 s on a session bus) and the call succeeds;
//!     busd with a fixed 25 s wait failed it with ServiceUnknown ("not
//!     provided by any .service files") although the file was there.
//!   * exit:  the service exits (status 3) without claiming: fails at once with
//!     Spawn.ChildExited ("exited with status 3").
//!   * never: the service stays alive and never claims: fails at the cap with
//!     org.freedesktop.DBus.Error.TimedOut ("timed out"), not ServiceUnknown.
//!   * nofile: no .service file: ServiceUnknown ("not provided by any
//!     .service files"), promptly.
//! One PASS/FAIL line per check, `ACT_TEST pass=N fail=M` last. Runs ~2 min.

use std::time::{Duration, Instant};

const SLOW_S: u64 = 40;
const CAP_S: u64 = 120;

struct Obj;

#[zbus::interface(name = "org.leandros.ActTest")]
impl Obj {
    fn hello(&self) -> bool {
        true
    }
}

pub(crate) async fn claim(address: &str, name: &str, mode: &str) -> bool {
    if mode == "exit" {
        std::process::exit(3);
    }
    if mode == "exit0" {
        std::process::exit(0);
    }
    if mode == "sig" {
        unsafe { libc::raise(libc::SIGKILL) };
    }
    if let Some(d) = mode.strip_prefix("daemon:") {
        // "Daemonize": a detached grandchild claims the name after d seconds, this process exits 0.
        let me = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let _ = std::process::Command::new(me).args(["act-claim", name, &format!("late:{d}")]).spawn();
        std::process::exit(0);
    }
    let detached = mode.starts_with("late:");
    let parent = unsafe { libc::getppid() };
    let alive = move || detached || unsafe { libc::getppid() } == parent;
    let delay: u64 = mode.strip_prefix("claim:").or(mode.strip_prefix("late:")).and_then(|d| d.parse().ok()).unwrap_or(u64::MAX);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(delay.min(CAP_S + 60)) {
        if !alive() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if delay == u64::MAX {
        return true; // "never": stay alive without claiming until busd goes away or the cap passes
    }
    let conn = match zbus::connection::Builder::address(address).and_then(|b| b.name(name.to_owned())).and_then(|b| b.serve_at("/", Obj)) {
        Ok(b) => match b.build().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("act-claim {name}: {e}");
                return false;
            }
        },
        Err(e) => {
            eprintln!("act-claim {name}: {e}");
            return false;
        }
    };
    let held = Instant::now();
    while alive() && !(detached && held.elapsed() > Duration::from_secs(30)) {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    drop(conn);
    true
}

async fn ping(addr: &str, name: &str) -> (Result<(), (String, String)>, Duration) {
    let t0 = Instant::now();
    let r = async {
        let c = zbus::connection::Builder::address(addr).map_err(|e| ("connect".to_string(), e.to_string()))?
            .build().await.map_err(|e| ("connect".to_string(), e.to_string()))?;
        match c.call_method(Some(name.to_owned()), "/", Some("org.freedesktop.DBus.Peer"), "Ping", &()).await {
            Ok(_) => Ok(()),
            Err(zbus::Error::MethodError(n, m, _)) => Err((n.to_string(), m.unwrap_or_default())),
            Err(e) => Err(("other".to_string(), e.to_string())),
        }
    }
    .await;
    eprintln!("act-test: {name} -> {r:?} after {:.1}s", t0.elapsed().as_secs_f32());
    (r, t0.elapsed())
}

pub(crate) async fn run() -> (u32, u32) {
    let (mut pass, mut fail) = (0u32, 0u32);
    let mut check = |name: &str, ok: bool, detail: String| {
        if ok { pass += 1 } else { fail += 1 }
        println!("{} {name}: {detail}", if ok { "PASS" } else { "FAIL" });
    };
    let me = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "/usr/libexec/leandros-sysbus".into());
    let dir = format!("/tmp/acttest-{}", std::process::id());
    let _ = std::fs::remove_dir_all(&dir);
    if std::fs::create_dir_all(format!("{dir}/services")).is_err() {
        check("setup", false, format!("cannot create {dir}"));
        return (pass, fail);
    }
    let sock = format!("{dir}/bus");
    let addr = format!("unix:path={sock}");
    let svc = |n: &str, mode: &str| {
        let _ = std::fs::write(
            format!("{dir}/services/{n}.service"),
            format!("[D-BUS Service]\nName=org.leandros.ActTest.{n}\nExec={me} act-claim org.leandros.ActTest.{n} {mode}\n"),
        );
    };
    svc("Slow", &format!("claim:{SLOW_S}"));
    svc("Exit", "exit");
    svc("Never", "never");
    svc("Sig", "sig");
    svc("ExitZero", "exit0");
    svc("Daemon", "daemon:3");
    let _ = std::fs::write(
        format!("{dir}/session.conf"),
        format!("<busconfig><type>session</type><auth>EXTERNAL</auth><listen>{addr}</listen><servicedir>{dir}/services</servicedir></busconfig>\n"),
    );
    // ACT_REFERENCE=1: run the same cases against the reference dbus-daemon (evidence for the contract).
    let reference = std::env::var("ACT_REFERENCE").is_ok();
    let busd = if reference {
        "/usr/bin/dbus-daemon".to_string()
    } else {
        std::env::var("BUSD_BIN").unwrap_or_else(|_| "/usr/libexec/busd".into())
    };
    if reference {
        let _ = std::fs::write(
            format!("{dir}/session.conf"),
            format!("<busconfig><type>session</type><auth>EXTERNAL</auth><listen>{addr}</listen><servicedir>{dir}/services</servicedir><limit name=\"service_start_timeout\">{}</limit><policy context=\"default\"><allow send_type=\"method_call\"/><allow send_type=\"method_return\"/><allow send_type=\"error\"/><allow send_type=\"signal\"/><allow receive_type=\"method_call\"/><allow receive_type=\"method_return\"/><allow receive_type=\"error\"/><allow receive_type=\"signal\"/><allow own=\"*\"/></policy></busconfig>\n", CAP_S * 1000),
        );
    }
    let mut cmd = std::process::Command::new(&busd);
    if reference {
        cmd.args(["--nofork", "--config-file", &format!("{dir}/session.conf")]);
    } else {
        cmd.args(["--config", &format!("{dir}/session.conf"), "--address", &addr]);
    }
    let mut child = match cmd
        .env("DBUS_SESSION_BUS_ADDRESS", &addr)
        .env("DBUS_STARTER_ADDRESS", &addr)
        .env_remove("DBUS_SYSTEM_BUS_ADDRESS")
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            check("busd_start", false, format!("{busd}: {e}"));
            return (pass, fail);
        }
    };
    let t0 = Instant::now();
    while std::os::unix::net::UnixStream::connect(&sock).is_err() && t0.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let n = |s: &str| format!("org.leandros.ActTest.{s}");
    let ((slow, exit, never, nofile), (sig, exitzero, daemon)) = futures_util::future::join(
        futures_util::future::join4(
            ping(&addr, &n("Slow")),
            ping(&addr, &n("Exit")),
            ping(&addr, &n("Never")),
            ping(&addr, &n("NoFile")),
        ),
        futures_util::future::join3(ping(&addr, &n("Sig")), ping(&addr, &n("ExitZero")), ping(&addr, &n("Daemon"))),
    )
    .await;
    let secs = |d: Duration| d.as_secs_f32();

    let (r, d) = slow;
    check(
        "slow_service_waited_for",
        r.is_ok() && d >= Duration::from_secs(SLOW_S - 2) && d < Duration::from_secs(CAP_S),
        format!("{r:?} after {:.1}s (service claims at {SLOW_S}s)", secs(d)),
    );
    let (r, d) = exit;
    check(
        "exit_unclaimed_fails_promptly",
        matches!(&r, Err((en, m)) if en == "org.freedesktop.DBus.Error.Spawn.ChildExited" && m.contains("exited with status 3"))
            && d < Duration::from_secs(10),
        format!("{r:?} after {:.1}s", secs(d)),
    );
    let (r, d) = never;
    check(
        "never_claims_times_out_at_cap",
        matches!(&r, Err((en, m)) if en == "org.freedesktop.DBus.Error.TimedOut" && m.contains("timed out"))
            && d >= Duration::from_secs(CAP_S - 5)
            && d < Duration::from_secs(CAP_S + 20),
        format!("{r:?} after {:.1}s (cap {CAP_S}s)", secs(d)),
    );
    let (r, d) = sig;
    check(
        "signal_unclaimed_fails_promptly",
        matches!(&r, Err((en, m)) if en == "org.freedesktop.DBus.Error.Spawn.ChildSignaled" && m.contains("received signal 9"))
            && d < Duration::from_secs(10),
        format!("{r:?} after {:.1}s", secs(d)),
    );
    // dbus-daemon ignores exit status 0 (a program may daemonize): it keeps waiting.
    let (r, d) = daemon;
    check(
        "daemonizing_service_exit0_then_claim_succeeds",
        r.is_ok() && d >= Duration::from_secs(2) && d < Duration::from_secs(30),
        format!("{r:?} after {:.1}s (parent exits 0, grandchild claims at 3s)", secs(d)),
    );
    let (r, d) = exitzero;
    check(
        "exit0_never_claims_waits_to_cap",
        matches!(&r, Err((en, m)) if en == "org.freedesktop.DBus.Error.TimedOut" && m.contains("timed out"))
            && d >= Duration::from_secs(CAP_S - 5)
            && d < Duration::from_secs(CAP_S + 20),
        format!("{r:?} after {:.1}s", secs(d)),
    );
    let (r, d) = nofile;
    check(
        "no_service_file_is_service_unknown",
        matches!(&r, Err((en, m)) if en == "org.freedesktop.DBus.Error.ServiceUnknown" && m.contains("not provided by any .service files"))
            && d < Duration::from_secs(5),
        format!("{r:?} after {:.1}s", secs(d)),
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    println!("ACT_TEST pass={pass} fail={fail}");
    (pass, fail)
}
