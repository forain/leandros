//! nmtest: drive org.freedesktop.NetworkManager the way the COSMIC network
//! applet does, through nmrs 3.4.0 (the exact library cosmic-applets e544cb5
//! links), and check every step. Written against the LeandrOS shim's mock
//! backend (ports/sysbus/src/nm/mock.rs), whose simulated networks are:
//!
//!   SimHome-WPA2  two BSSIDs, WPA2-PSK, password "correct-horse"
//!   SimMixed      WPA2/WPA3 transition (must show as plain PSK)
//!   SimCafe-WPA3  SAE only (must be hidden: no WPA3 yet)
//!   SimOpen       open
//!   SimFlaky      comes and goes between scans
//!   eth0          wired, DHCP bound, "Wired connection 1"
//!
//! (These are fixtures of a simulated network, not real credentials.)
//!
//! Usage:
//!   nmtest [--as USER [--no-netdev]] [--user-bus] [--spawn [SHIM]]
//!          [--real [--after-reboot] [--cleanup]] [--ro] [--quick]
//!
//!   --spawn   start SHIM (default /usr/libexec/leandros-sysbus) as
//!             `networkmanager` with the mock backend and a fresh profile
//!             directory (with --real: the /dev/netctl backend), instead of
//!             using whatever owns the name
//!   --ro      read-only checks instead of the connect flow (mock: the
//!             spawned shim plays a user outside group netdev; real: run as
//!             such a user, see --no-netdev)
//!   --quick   skip the 25 s snapshot-storm check (mock)
//!
//! THE REAL STACK (`--real`): no mock anywhere. The shim's /dev/netctl backend
//! drives the kernel's wifi_sim (drivers/src/wifi_sim.rs; boot with the sim
//! NIC, LEANDROS_NIC2=wifisim) through the same wifi_mgr state machine
//! iwlwifi runs with iwlwifi.mgr=1. Its default APs:
//!
//!   SimHome    WPA2-PSK  "simhome-pass"   2.4 GHz   (right/wrong key, saved)
//!   SimCafe    WPA2-PSK                   5 GHz
//!   SimWPA3    SAE only                   (real stack: listed, WPA3-Personal)
//!   SimMixed   PSK+SAE transition         (must show as plain PSK)
//!   SimOpen    open
//!   SimHidden  hidden SSID                (not exported)
//!   SimFull    refuses association        (fails, never hangs)
//!   SimTkip    WPA/WPA2 mixed, TKIP group + CCMP pairwise (joinable)
//!   SimTkipOnly pure TKIP (must be hidden: not JOINABLE)
//!
//! (Fixtures of the simulator, not real credentials.) The run ends with
//! SimHome saved and connected, so a reboot checks autoconnect:
//!   --after-reboot  only check that wifi0 came up on SimHome by itself from
//!                   the saved profile (/etc/leandros/network/profiles.{a,b})
//!   --cleanup       forget SimHome at the end instead of keeping it
//!
//! Identity (from a root serial shell): `--as USER` drops to USER with
//! USER's /etc/group memberships (initgroups) before anything else runs;
//! `--no-netdev` also drops group netdev (108): a user who may look but not
//! manage. `--user-bus` talks to USER's COSMIC session bus
//! (/run/user/<uid>/bus) and so to the busd-activated shim of that session.
//! `--as USER [--no-netdev] -- CMD ARGS...` runs CMD as that user instead:
//!   nmtest --as leandro -- /usr/bin/dbus-run-session -- /bin/nmtest --spawn --real
//!   nmtest --as leandro --user-bus --real          (inside leandro's COSMIC session)
//!
//! The bus: nmrs uses the system bus. LeandrOS has none; the session bus
//! stands in (DBUS_SYSTEM_BUS_ADDRESS is set to it when unset), as in
//! start-cosmic-leandros. In-guest from a serial shell:
//!   dbus-run-session -- /bin/nmtest --spawn
//!
//! Output: one `PASS`/`FAIL` line per check, then `NMTEST pass=N fail=M`.
//! Exit status 0 iff no check failed.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use nmrs::agent::{SecretAgent, SecretAgentCapabilities};
use nmrs::{ActiveConnection, ActiveConnectionState, ConnectionError, NetworkManager, WifiSecurity};
use zbus::zvariant::OwnedValue;

const NM: &str = "org.freedesktop.NetworkManager";
const RIGHT: &str = "correct-horse";
const WRONG: &str = "wrong-password-1";

// wifi_sim's default APs (drivers/src/wifi_sim.rs).
const SIM_HOME: &str = "SimHome";
const SIM_HOME_KEY: &str = "simhome-pass";
const SIM_WRONG: &str = "not-the-simhome-key";
const GID_NETDEV: libc::gid_t = 108;

/// The fixtures a run talks about: the mock's or wifi_sim's.
struct Fx {
    home: &'static str,
    right: &'static str,
}

/// `--as USER [--no-netdev]`: become USER (uid, gid, /etc/group memberships,
/// HOME/USER/LOGNAME), optionally without group netdev. Runs before the
/// tokio runtime exists, while the process has one thread. Returns the uid.
fn become_user(name: &str, no_netdev: bool) -> Result<u32, String> {
    let cname = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
    // SAFETY: getpwnam returns a pointer into static storage or NULL; we copy
    // what we need before any other passwd call.
    let pw = unsafe { libc::getpwnam(cname.as_ptr()) };
    if pw.is_null() {
        return Err(format!("no user {name:?} in /etc/passwd"));
    }
    let (uid, gid, home) = unsafe {
        let p = &*pw;
        (p.pw_uid, p.pw_gid, std::ffi::CStr::from_ptr(p.pw_dir).to_string_lossy().into_owned())
    };
    // The user's runtime directory (dbus-run-session puts busd's readiness
    // file there), made as root while we still can, as a login would.
    let runtime = format!("/run/user/{uid}");
    if std::fs::metadata(&runtime).is_err() {
        std::fs::create_dir_all(&runtime).map_err(|e| format!("{runtime}: {e}"))?;
        let c = std::ffi::CString::new(runtime.clone()).map_err(|e| e.to_string())?;
        // SAFETY: valid NUL-terminated path.
        unsafe {
            libc::chown(c.as_ptr(), uid, gid);
            libc::chmod(c.as_ptr(), 0o700);
        }
    }
    // SAFETY: plain libc calls with valid arguments; single-threaded here.
    unsafe {
        if libc::initgroups(cname.as_ptr(), gid) != 0 {
            return Err(format!("initgroups: {}", std::io::Error::last_os_error()));
        }
        if no_netdev {
            let mut g = vec![0 as libc::gid_t; 64];
            let n = libc::getgroups(g.len() as libc::c_int, g.as_mut_ptr());
            if n < 0 {
                return Err(format!("getgroups: {}", std::io::Error::last_os_error()));
            }
            g.truncate(n as usize);
            g.retain(|&x| x != GID_NETDEV);
            if libc::setgroups(g.len() as _, g.as_ptr()) != 0 {
                return Err(format!("setgroups: {}", std::io::Error::last_os_error()));
            }
        }
        if libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
            return Err(format!("setgid/setuid: {}", std::io::Error::last_os_error()));
        }
        if libc::setuid(0) == 0 {
            return Err("still able to become root".into());
        }
        std::env::set_var("HOME", &home);
        std::env::set_var("USER", name);
        std::env::set_var("LOGNAME", name);
        // Root's values would point the user's tools at root's directories.
        std::env::set_var("XDG_RUNTIME_DIR", &runtime);
        std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
        std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS");
    }
    // The caller's cwd (root's home) is not ours to search.
    let _ = std::env::set_current_dir(&home).or_else(|_| std::env::set_current_dir("/tmp"));
    Ok(uid)
}

fn groups_now() -> Vec<u32> {
    let mut g = vec![0 as libc::gid_t; 64];
    // SAFETY: g has room for 64 entries.
    let n = unsafe { libc::getgroups(g.len() as libc::c_int, g.as_mut_ptr()) };
    g.truncate(n.max(0) as usize);
    g
}

struct T {
    pass: u32,
    fail: u32,
}

impl T {
    fn report(&mut self, name: &str, t: Instant, r: Result<String, String>) {
        let ms = t.elapsed().as_millis();
        step(&format!("after {name}"));
        match r {
            Ok(v) => {
                self.pass += 1;
                println!("PASS {name:<44} {ms:>6} ms  {v}");
            }
            Err(e) => {
                self.fail += 1;
                println!("FAIL {name:<44} {ms:>6} ms  {e}");
            }
        }
    }
}

macro_rules! check {
    ($t:expr, $name:expr, $body:expr) => {{
        step($name);
        let t0 = Instant::now();
        let r: Result<String, String> = async { $body }.await;
        $t.report($name, t0, r);
    }};
}

fn ensure(cond: bool, msg: impl Into<String>) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg.into()) }
}

async fn prop(conn: &zbus::Connection, path: &str, iface: &str, name: &str) -> Result<OwnedValue, String> {
    let p = zbus::fdo::PropertiesProxy::builder(conn)
        .destination(NM)
        .map_err(|e| e.to_string())?
        .path(path.to_owned())
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let iface = zbus::names::InterfaceName::try_from(iface.to_owned()).map_err(|e| e.to_string())?;
    p.get(iface, name).await.map_err(|e| e.to_string())
}

async fn device_path(conn: &zbus::Connection, ifname: &str) -> Result<String, String> {
    let r = conn
        .call_method(Some(NM), "/org/freedesktop/NetworkManager", Some(NM), "GetDeviceByIpIface", &(ifname,))
        .await
        .map_err(|e| e.to_string())?;
    let p: zbus::zvariant::OwnedObjectPath = r.body().deserialize().map_err(|e| e.to_string())?;
    Ok(p.to_string())
}

async fn dev_state(conn: &zbus::Connection, dev: &str) -> Result<u32, String> {
    let v = prop(conn, dev, "org.freedesktop.NetworkManager.Device", "State").await?;
    u32::try_from(v).map_err(|e| e.to_string())
}

/// Poll until `f` is true (or time out), returning how long it took.
async fn wait_until<F, Fut>(timeout: Duration, mut f: F) -> Result<Duration, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        if f().await {
            return Ok(t0.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("timed out after {timeout:?}"))
}

fn strongest_bssid(snap: &nmrs::NetworkSnapshot, ssid: &str) -> Option<String> {
    snap.access_points.iter().filter(|a| a.ssid == ssid).max_by_key(|a| a.strength).map(|a| a.bssid.clone())
}

fn active_wifi(snap: &nmrs::NetworkSnapshot) -> Option<&nmrs::ActiveWifiConnection> {
    snap.active_connections.iter().find_map(|c| match c {
        ActiveConnection::Wifi(w) => Some(w),
        _ => None,
    })
}

fn saved_wifi_count(snap: &nmrs::NetworkSnapshot, ssid: &str) -> usize {
    snap.saved_connections
        .iter()
        .filter(|c| matches!(&c.summary, nmrs::SettingsSummary::Wifi { ssid: s, .. } if s == ssid))
        .count()
}


// ---- Stall diagnostics (lane nmflake) ---------------------------------
// `nmtest --spawn --quick` hung once past the 900 s regress timeout with no
// record of where. Every check and every teardown phase names itself here;
// a plain OS thread (independent of the tokio runtime, which may be the thing
// that is stuck) prints, when one phase has made no progress for
// NMTEST_STALL_SECS (default 120 s; again every period while it lasts), the
// phase, and every process with its threads' scheduler states and the
// kernel's wait-channel field from /proc. It only reports: the test's
// behaviour and verdicts are unchanged.
static STEP: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn step(name: &str) {
    if let Ok(mut s) = STEP.lock() {
        *s = Some((name.to_string(), Instant::now()));
    }
}

fn proc_states(pid: &str) -> String {
    // stat: "pid (comm) S ppid ..." -- comm may contain spaces/parens.
    let rd = |p: String| std::fs::read_to_string(p).unwrap_or_default();
    let stat = rd(format!("/proc/{pid}/stat"));
    let (comm, rest) = match (stat.find('('), stat.rfind(')')) {
        (Some(a), Some(b)) if b > a => (&stat[a + 1..b], stat[b + 1..].trim()),
        _ => ("?", ""),
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After comm: [0]=state [1]=ppid ... [32]=wchan (Linux field 35).
    let g = |i: usize| f.get(i).copied().unwrap_or("?");
    let mut thr = Vec::new();
    if let Ok(d) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for e in d.flatten() {
            let tid = e.file_name().to_string_lossy().into_owned();
            let ts = rd(format!("/proc/{pid}/task/{tid}/stat"));
            let st = ts.rfind(')').and_then(|b| ts[b + 1..].split_whitespace().next().map(str::to_string));
            thr.push(format!("{tid}:{}", st.unwrap_or_else(|| "?".into())));
        }
    }
    let cmd = rd(format!("/proc/{pid}/cmdline")).replace('\0', " ");
    format!("pid {pid} ppid {} {} wchan {} comm={comm} threads=[{}] cmd={}", g(1), g(0), g(32), thr.join(","), cmd.trim())
}

fn stall_watchdog() {
    let period = std::env::var("NMTEST_STALL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120u64);
    let _ = std::thread::Builder::new().name("nmtest-watchdog".into()).spawn(move || {
        let mut last_dump: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let Some((name, since)) = STEP.lock().ok().and_then(|s| s.clone()) else { continue };
            let stalled = since.elapsed().as_secs();
            if stalled < period || last_dump.is_some_and(|d| d.elapsed().as_secs() < period) {
                continue;
            }
            last_dump = Some(Instant::now());
            let mut out = format!("NMTEST-STALL step={name:?} no progress for {stalled} s; processes:\n");
            if let Ok(d) = std::fs::read_dir("/proc") {
                let mut pids: Vec<u64> = d.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
                pids.sort_unstable();
                for p in pids {
                    out.push_str("NMTEST-STALL   ");
                    out.push_str(&proc_states(&p.to_string()));
                    out.push('\n');
                }
            }
            use std::io::Write;
            let mut so = std::io::stdout().lock();
            let _ = so.write_all(out.as_bytes());
            let _ = so.flush();
        }
    });
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--as") {
        let Some(name) = args.get(i + 1) else {
            eprintln!("nmtest: --as needs a user name");
            return std::process::ExitCode::from(2);
        };
        match become_user(name, args.iter().any(|a| a == "--no-netdev")) {
            Ok(uid) => {
                if args.iter().any(|a| a == "--user-bus") {
                    // SAFETY: still single-threaded (no runtime yet).
                    unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", format!("unix:path=/run/user/{uid}/bus")) };
                }
                // SAFETY: getuid/geteuid have no preconditions.
                println!("nmtest: running as {name} uid {} euid {} groups {:?}", unsafe { libc::getuid() }, unsafe { libc::geteuid() }, groups_now());
                // `--as USER -- CMD ARGS...`: run CMD as that user instead
                // (e.g. dbus-run-session, so the bus is the user's too).
                if let Some(k) = args.iter().position(|a| a == "--") {
                    use std::os::unix::process::CommandExt;
                    let Some(cmd) = args.get(k + 1) else {
                        eprintln!("nmtest: nothing after --");
                        return std::process::ExitCode::from(2);
                    };
                    let e = std::process::Command::new(cmd).args(&args[k + 2..]).exec();
                    eprintln!("nmtest: exec {cmd}: {e}");
                    return std::process::ExitCode::from(2);
                }
            }
            Err(e) => {
                eprintln!("nmtest: --as {name}: {e}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    stall_watchdog();
    step("runtime start");
    match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => {
            let r = rt.block_on(amain(args));
            step("runtime shutdown");
            drop(rt);
            step("exit");
            r
        }
        Err(e) => {
            eprintln!("nmtest: runtime: {e}");
            std::process::ExitCode::from(2)
        }
    }
}

async fn amain(args: Vec<String>) -> std::process::ExitCode {
    let spawn = args.iter().position(|a| a == "--spawn").map(|i| {
        args.get(i + 1).filter(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| "/usr/libexec/leandros-sysbus".into())
    });
    let ro = args.iter().any(|a| a == "--ro");
    let quick = args.iter().any(|a| a == "--quick");
    let real = args.iter().any(|a| a == "--real");

    if std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS").is_none() {
        match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
            // SAFETY: single-threaded at this point in practice (before any
            // task touches the environment); nmrs reads it in Connection::system().
            Ok(a) => unsafe { std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", a) },
            Err(_) => {
                eprintln!("nmtest: no D-Bus bus; run under dbus-run-session or a COSMIC session");
                return std::process::ExitCode::from(2);
            }
        }
    }
    step("bus connect");
    let bus = match zbus::Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nmtest: bus: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    let mut child = None;
    if let Some(shim) = &spawn {
        let dir = format!("/tmp/nmtest-{}", std::process::id());
        let _ = std::fs::remove_dir_all(&dir);
        let mut cmd = std::process::Command::new(shim);
        cmd.arg("networkmanager");
        if real {
            // The kernel's /dev/netctl, opened with this process's identity.
            cmd.env("LEANDROS_NM_BACKEND", "netctl").env_remove("LEANDROS_NM_MOCK");
        } else {
            cmd.env("LEANDROS_NM_BACKEND", "mock").env("LEANDROS_NM_MOCK_DIR", &dir);
            if ro {
                cmd.env("LEANDROS_NM_MOCK_RO", "1");
            }
        }
        step("spawn shim");
        match cmd.spawn() {
            Ok(c) => child = Some(c),
            Err(e) => {
                eprintln!("nmtest: spawn {shim}: {e}");
                return std::process::ExitCode::from(2);
            }
        }
        step("wait for shim name");
        let dbus = zbus::fdo::DBusProxy::new(&bus).await.expect("DBus proxy");
        let name = zbus::names::BusName::try_from(NM).expect("name");
        let ok = wait_until(Duration::from_secs(15), || {
            let dbus = &dbus;
            let name = name.clone();
            async move { dbus.name_has_owner(name).await.unwrap_or(false) }
        })
        .await;
        if let Err(e) = ok {
            eprintln!("nmtest: shim never owned {NM}: {e}");
            if let Some(mut c) = child {
                let _ = c.kill();
            }
            return std::process::ExitCode::from(2);
        }
    }

    let mut t = T { pass: 0, fail: 0 };
    let fx = if real { Fx { home: SIM_HOME, right: SIM_HOME_KEY } } else { Fx { home: "SimHome-WPA2", right: RIGHT } };
    if ro {
        read_only(&mut t, &bus, &fx).await;
    } else if real && args.iter().any(|a| a == "--after-reboot") {
        real_after_reboot(&mut t).await;
    } else if real {
        real_stack(&mut t, &bus, args.iter().any(|a| a == "--cleanup")).await;
    } else {
        full(&mut t, &bus, quick).await;
    }
    println!("NMTEST pass={} fail={}", t.pass, t.fail);
    if let Some(mut c) = child {
        step("teardown: kill shim");
        let _ = c.kill();
        step("teardown: wait for shim");
        let _ = c.wait();
    }
    step("teardown: drop bus");
    drop(bus);
    if t.fail == 0 { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::from(1) }
}

async fn full(t: &mut T, bus: &zbus::Connection, quick: bool) {
    let nm = match NetworkManager::new().await {
        Ok(nm) => nm,
        Err(e) => {
            t.report("NetworkManager::new", Instant::now(), Err(e.to_string()));
            return;
        }
    };

    // Applet start-up: event stream, secret agent, snapshot.
    let events = Arc::new(Mutex::new(0u32));
    let mut ev_task = None;
    check!(t, "network_events() subscribe", {
        let mut s = nm.network_events().await.map_err(|e| e.to_string())?;
        let n = events.clone();
        ev_task = Some(tokio::spawn(async move {
            while let Some(e) = s.next().await {
                if e.is_ok() {
                    *n.lock().unwrap() += 1;
                }
            }
        }));
        Ok("stream open".to_string())
    });
    let mut _agent = None;
    check!(t, "SecretAgent register (VPN_HINTS)", {
        let (h, rx) = SecretAgent::builder()
            .with_identifier("org.leandros.nmtest.SecretAgent")
            .with_object_path("/org/freedesktop/NetworkManager/SecretAgent")
            .with_capabilities(SecretAgentCapabilities::VPN_HINTS)
            .register()
            .await
            .map_err(|e| e.to_string())?;
        _agent = Some((h, rx));
        Ok("accepted".to_string())
    });

    let mut home_bssid = String::new();
    check!(t, "snapshot: radios, devices, wired AC", {
        let s = nm.snapshot().await.map_err(|e| format!("snapshot: {e}"))?;
        ensure(s.wifi.enabled && s.wifi.present && s.wifi.hardware_enabled, format!("wifi radio {:?}", s.wifi))?;
        ensure(!s.wwan.present, "wwan should be absent")?;
        ensure(s.wifi_devices.iter().any(|d| d.interface == "wifi0"), "no wifi0 device")?;
        ensure(s.wired_devices.iter().any(|d| d.interface == "eth0"), "no eth0 device")?;
        let wired = s.active_connections.iter().find_map(|c| match c {
            ActiveConnection::Wired(w) => Some(w),
            _ => None,
        });
        let w = wired.ok_or("no wired active connection")?;
        ensure(w.id == "Wired connection 1", format!("wired AC id {:?}", w.id))?;
        ensure(w.state == ActiveConnectionState::Activated, format!("wired AC state {:?}", w.state))?;
        ensure(w.ip4_address.as_deref() == Some("10.0.2.15/24"), format!("wired ip {:?}", w.ip4_address))?;
        ensure(saved_wifi_count(&s, "SimHome-WPA2") == 0, "a saved SimHome-WPA2 before any connect")?;
        Ok(format!("{} APs, {} active, connectivity {:?}", s.access_points.len(), s.active_connections.len(), s.connectivity.state))
    });

    check!(t, "AP list: hidden SAE-only, transition as PSK", {
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let names: Vec<&str> = s.access_points.iter().map(|a| a.ssid.as_str()).collect();
        for want in ["SimHome-WPA2", "SimMixed", "SimOpen"] {
            ensure(names.contains(&want), format!("{want} missing from {names:?}"))?;
        }
        ensure(!names.contains(&"SimCafe-WPA3"), "SAE-only SimCafe-WPA3 is listed")?;
        ensure(s.access_points.iter().filter(|a| a.ssid == "SimHome-WPA2").count() == 2, "SimHome-WPA2 should have 2 BSSIDs")?;
        let mixed = s.access_points.iter().find(|a| a.ssid == "SimMixed").ok_or("no SimMixed")?;
        ensure(mixed.security.psk && !mixed.security.sae, format!("SimMixed security {:?}", mixed.security))?;
        let open = s.access_points.iter().find(|a| a.ssid == "SimOpen").ok_or("no SimOpen")?;
        ensure(open.security.is_open(), "SimOpen is not open")?;
        for a in &s.access_points {
            ensure(a.bssid.len() == 17 && a.bssid.matches(':').count() == 5, format!("bad BSSID {:?}", a.bssid))?;
            ensure(a.frequency_mhz > 2000, format!("bad frequency {}", a.frequency_mhz))?;
        }
        let summary = s.applet_summary();
        ensure(!summary.wifi_groups.is_empty(), "applet_summary has no Wi-Fi groups")?;
        home_bssid = strongest_bssid(&s, "SimHome-WPA2").ok_or("no SimHome-WPA2 BSSID")?;
        Ok(format!("{names:?}"))
    });

    let wifi0 = device_path(bus, "wifi0").await.unwrap_or_default();
    let eth0 = device_path(bus, "eth0").await.unwrap_or_default();

    check!(t, "RequestScan + LastScan advances (never -1)", {
        let before = i64::try_from(prop(bus, &wifi0, "org.freedesktop.NetworkManager.Device.Wireless", "LastScan").await?)
            .map_err(|e| e.to_string())?;
        ensure(before != -1, "LastScan is -1")?;
        nm.scan_networks(Some("wifi0")).await.map_err(|e| e.to_string())?;
        let took = wait_until(Duration::from_secs(5), || async {
            prop(bus, &wifi0, "org.freedesktop.NetworkManager.Device.Wireless", "LastScan")
                .await
                .ok()
                .and_then(|v| i64::try_from(v).ok())
                .is_some_and(|v| v > before)
        })
        .await;
        match took {
            // Within 10 s of the previous scan the kernel answers from its
            // cache with a fresh SCAN_DONE: LastScan still moves.
            Ok(d) => Ok(format!("LastScan {before} -> advanced in {} ms", d.as_millis())),
            Err(e) => Err(format!("LastScan stuck at {before}: {e}")),
        }
    });

    // Watch wifi0's StateChanged(new, old, reason) signals from here on.
    let dev_sigs: Arc<Mutex<Vec<(u32, u32, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let sig_task = {
        let sigs = dev_sigs.clone();
        let proxy = zbus::Proxy::new(bus, NM, wifi0.clone(), "org.freedesktop.NetworkManager.Device").await;
        match proxy {
            Ok(p) => match p.receive_signal("StateChanged").await {
                Ok(mut st) => Some(tokio::spawn(async move {
                    while let Some(m) = st.next().await {
                        if let Ok(a) = m.body().deserialize::<(u32, u32, u32)>() {
                            sigs.lock().unwrap().push(a);
                        }
                    }
                })),
                Err(_) => None,
            },
            Err(_) => None,
        }
    };

    check!(t, "connect wrong key -> AuthFailed, dev 120/7", {
        let t0 = Instant::now();
        let r = nm
            .connect_to_bssid("SimHome-WPA2", Some(&home_bssid), Some("wifi0"), WifiSecurity::WpaPsk { psk: WRONG.into() })
            .await;
        let took = t0.elapsed();
        match r {
            Err(ConnectionError::AuthFailed) => {}
            Err(e) => return Err(format!("expected AuthFailed, got {e}")),
            Ok(()) => return Err("connected with a wrong key".into()),
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let sigs = dev_sigs.lock().unwrap().clone();
        ensure(sigs.iter().any(|s| s.0 == 120 && s.2 == 7), format!("no StateChanged(120, _, 7) in {sigs:?}"))?;
        let st = dev_state(bus, &wifi0).await?;
        ensure(st == 30 || st == 120, format!("device state {st} after the failure"))?;
        Ok(format!("AuthFailed after {} ms; signals {:?}", took.as_millis(), sigs))
    });

    check!(t, "connect right key -> activated + IPv4", {
        let t0 = Instant::now();
        nm.connect_to_bssid("SimHome-WPA2", Some(&home_bssid), Some("wifi0"), WifiSecurity::WpaPsk { psk: RIGHT.into() })
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let took = t0.elapsed();
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let w = active_wifi(&s).ok_or("no Wi-Fi active connection")?;
        ensure(w.ssid == "SimHome-WPA2" && w.id == "SimHome-WPA2", format!("AC id/ssid {:?}/{:?}", w.id, w.ssid))?;
        ensure(w.state == ActiveConnectionState::Activated, format!("AC state {:?}", w.state))?;
        ensure(w.bssid.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(&home_bssid)), format!("AC bssid {:?}", w.bssid))?;
        let ip = w.ip4_address.clone().ok_or("no IPv4 on the Wi-Fi AC")?;
        ensure(s.access_points.iter().any(|a| a.is_active && a.ssid == "SimHome-WPA2"), "no AP marked active")?;
        ensure(saved_wifi_count(&s, "SimHome-WPA2") == 1, format!("{} saved SimHome-WPA2 profiles (want 1: dedupe)", saved_wifi_count(&s, "SimHome-WPA2")))?;
        ensure(dev_state(bus, &wifi0).await? == 100, "device not 100")?;
        Ok(format!("connected in {} ms, {ip}", took.as_millis()))
    });

    check!(t, "is_connected / current_ssid", {
        let c = nm.is_connected("SimHome-WPA2").await.map_err(|e| e.to_string())?;
        ensure(c, "is_connected(SimHome-WPA2) = false")?;
        let cur = nm.current_ssid().await;
        ensure(cur.as_deref() == Some("SimHome-WPA2"), format!("current_ssid {cur:?}"))?;
        Ok("SimHome-WPA2".into())
    });

    check!(t, "disconnect -> 30, no Wi-Fi AC", {
        nm.disconnect(Some("wifi0")).await.map_err(|e| e.to_string())?;
        let st = dev_state(bus, &wifi0).await?;
        ensure(st == 30, format!("device state {st}"))?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(active_wifi(&s).is_none(), "Wi-Fi AC still listed")?;
        Ok("disconnected".into())
    });

    check!(t, "reconnect saved profile (no password)", {
        // nmrs "UseSaved": ActivateConnection(saved, dev, ap).
        nm.connect_to_bssid("SimHome-WPA2", Some(&home_bssid), Some("wifi0"), WifiSecurity::WpaPsk { psk: String::new() })
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let w = active_wifi(&s).ok_or("no Wi-Fi AC")?;
        ensure(w.state == ActiveConnectionState::Activated, format!("AC state {:?}", w.state))?;
        nm.disconnect(Some("wifi0")).await.map_err(|e| e.to_string())?;
        Ok("activated from the saved profile, then disconnected".into())
    });

    check!(t, "open network connect + disconnect", {
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let b = strongest_bssid(&s, "SimOpen").ok_or("no SimOpen")?;
        nm.connect_to_bssid("SimOpen", Some(&b), Some("wifi0"), WifiSecurity::Open).await.map_err(|e| format!("connect: {e}"))?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(active_wifi(&s).is_some_and(|w| w.ssid == "SimOpen"), "SimOpen not active")?;
        nm.disconnect(Some("wifi0")).await.map_err(|e| e.to_string())?;
        Ok("ok".into())
    });

    check!(t, "Settings: GetSettings has no secrets", {
        let r = bus
            .call_method(Some(NM), "/org/freedesktop/NetworkManager/Settings", Some("org.freedesktop.NetworkManager.Settings"), "ListConnections", &())
            .await
            .map_err(|e| e.to_string())?;
        let paths: Vec<zbus::zvariant::OwnedObjectPath> = r.body().deserialize().map_err(|e| e.to_string())?;
        let mut n = 0;
        for p in &paths {
            let r = bus
                .call_method(Some(NM), p.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "GetSettings", &())
                .await
                .map_err(|e| e.to_string())?;
            let s: std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>> =
                r.body().deserialize().map_err(|e| e.to_string())?;
            ensure(s.get("connection").is_some_and(|c| c.contains_key("uuid")), format!("{p}: no uuid"))?;
            ensure(!s.values().any(|sec| sec.contains_key("psk")), format!("{p}: psk in GetSettings"))?;
            n += 1;
        }
        Ok(format!("{n} connections"))
    });

    check!(t, "Settings.Connection.Update never fails", {
        let r = bus
            .call_method(Some(NM), "/org/freedesktop/NetworkManager/Settings", Some("org.freedesktop.NetworkManager.Settings"), "ListConnections", &())
            .await
            .map_err(|e| e.to_string())?;
        let paths: Vec<zbus::zvariant::OwnedObjectPath> = r.body().deserialize().map_err(|e| e.to_string())?;
        for p in &paths {
            let r = bus
                .call_method(Some(NM), p.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "GetSettings", &())
                .await
                .map_err(|e| e.to_string())?;
            let s: std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>> =
                r.body().deserialize().map_err(|e| e.to_string())?;
            bus.call_method(Some(NM), p.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "Update", &(s,))
                .await
                .map_err(|e| format!("{p}: Update: {e}"))?;
        }
        Ok(format!("{} updated", paths.len()))
    });

    check!(t, "wired: Device.Disconnect then connect_wired", {
        bus.call_method(Some(NM), eth0.as_str(), Some("org.freedesktop.NetworkManager.Device"), "Disconnect", &())
            .await
            .map_err(|e| format!("Disconnect: {e}"))?;
        wait_until(Duration::from_secs(5), || async { dev_state(bus, &eth0).await.is_ok_and(|s| s == 30) }).await?;
        nm.connect_wired().await.map_err(|e| format!("connect_wired: {e}"))?;
        let st = dev_state(bus, &eth0).await?;
        ensure(st == 100, format!("eth0 state {st}"))?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(
            s.active_connections.iter().any(|c| matches!(c, ActiveConnection::Wired(w) if w.id == "Wired connection 1")),
            "wired AC not back",
        )?;
        Ok("eth0 30 -> 100".into())
    });

    check!(t, "Wi-Fi off: no APs; on: APs back", {
        nm.set_wireless_enabled(false).await.map_err(|e| e.to_string())?;
        wait_until(Duration::from_secs(5), || async {
            nm.snapshot().await.is_ok_and(|s| !s.wifi.enabled && s.access_points.is_empty())
        })
        .await
        .map_err(|e| format!("off: {e}"))?;
        ensure(dev_state(bus, &wifi0).await? == 20, "wifi0 not 20 with the radio off")?;
        nm.set_wireless_enabled(true).await.map_err(|e| e.to_string())?;
        let took = wait_until(Duration::from_secs(8), || async {
            nm.snapshot().await.is_ok_and(|s| s.wifi.enabled && s.access_points.iter().any(|a| a.ssid == "SimHome-WPA2"))
        })
        .await
        .map_err(|e| format!("on: {e}"))?;
        Ok(format!("APs back {} ms after radio on", took.as_millis()))
    });

    if !quick {
        check!(t, "snapshot storm during AP churn (25 s)", {
            let t0 = Instant::now();
            let (mut ok, mut bad, mut last_err) = (0u32, 0u32, String::new());
            let mut next_scan = Instant::now();
            let mut seen_flaky = (false, false);
            while t0.elapsed() < Duration::from_secs(25) {
                if Instant::now() >= next_scan {
                    let _ = nm.scan_networks(Some("wifi0")).await;
                    next_scan = Instant::now() + Duration::from_millis(10_500);
                }
                match nm.snapshot().await {
                    Ok(s) => {
                        ok += 1;
                        if s.access_points.iter().any(|a| a.ssid == "SimFlaky") {
                            seen_flaky.0 = true;
                        } else {
                            seen_flaky.1 = true;
                        }
                    }
                    Err(e) => {
                        bad += 1;
                        last_err = e.to_string();
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            ensure(bad == 0, format!("{bad} of {} snapshots failed; last: {last_err}", ok + bad))?;
            Ok(format!("{ok} snapshots ok; SimFlaky seen present={} absent={}", seen_flaky.0, seen_flaky.1))
        });
    }

    check!(t, "forget -> profile gone", {
        nm.forget("SimHome-WPA2").await.map_err(|e| e.to_string())?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(saved_wifi_count(&s, "SimHome-WPA2") == 0, "SimHome-WPA2 still saved")?;
        Ok(format!("{} saved connections left", s.saved_connections.len()))
    });

    check!(t, "Connectivity is never 2 (portal)", {
        let v = u32::try_from(prop(bus, "/org/freedesktop/NetworkManager", NM, "Connectivity").await?).map_err(|e| e.to_string())?;
        ensure(v != 2, "Connectivity = 2")?;
        let st = u32::try_from(prop(bus, "/org/freedesktop/NetworkManager", NM, "State").await?).map_err(|e| e.to_string())?;
        Ok(format!("Connectivity={v} State={st}"))
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    check!(t, "network_events delivered", {
        let n = *events.lock().unwrap();
        ensure(n > 0, "no NetworkEvent at all")?;
        Ok(format!("{n} events"))
    });
    if let Some(h) = ev_task {
        h.abort();
    }
    if let Some(h) = sig_task {
        h.abort();
    }
}

/// A user outside group netdev: everything is visible, nothing changes, and
/// nothing errors in a way that panics or blanks a client.
async fn read_only(t: &mut T, bus: &zbus::Connection, fx: &Fx) {
    let nm = match NetworkManager::new().await {
        Ok(nm) => nm,
        Err(e) => {
            t.report("NetworkManager::new", Instant::now(), Err(e.to_string()));
            return;
        }
    };
    check!(t, "ro: snapshot lists APs", {
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(s.access_points.iter().any(|a| a.ssid == fx.home), format!("{} not listed", fx.home))?;
        Ok(format!("{} APs", s.access_points.len()))
    });
    check!(t, "ro: GetPermissions says no", {
        let r = bus
            .call_method(Some(NM), "/org/freedesktop/NetworkManager", Some(NM), "GetPermissions", &())
            .await
            .map_err(|e| e.to_string())?;
        let p: std::collections::HashMap<String, String> = r.body().deserialize().map_err(|e| e.to_string())?;
        let v = p.get("org.freedesktop.NetworkManager.network-control").cloned().unwrap_or_default();
        ensure(v == "no", format!("network-control = {v:?}"))?;
        Ok("network-control=no".into())
    });
    check!(t, "ro: RequestScan still succeeds", {
        nm.scan_networks(Some("wifi0")).await.map_err(|e| e.to_string())?;
        Ok("ok".into())
    });
    check!(t, "ro: connect is refused (PermissionDenied)", {
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let b = strongest_bssid(&s, fx.home).ok_or("no AP")?;
        match nm.connect_to_bssid(fx.home, Some(&b), Some("wifi0"), WifiSecurity::WpaPsk { psk: fx.right.into() }).await {
            Ok(()) => Err("a read-only user connected".into()),
            Err(e) => {
                let msg = e.to_string();
                ensure(msg.contains("PermissionDenied") || msg.contains("netdev"), format!("unexpected error {msg}"))?;
                Ok(msg)
            }
        }
    });
    check!(t, "ro: Update succeeds as an unsaved overlay", {
        let r = bus
            .call_method(Some(NM), "/org/freedesktop/NetworkManager/Settings", Some("org.freedesktop.NetworkManager.Settings"), "ListConnections", &())
            .await
            .map_err(|e| e.to_string())?;
        let paths: Vec<zbus::zvariant::OwnedObjectPath> = r.body().deserialize().map_err(|e| e.to_string())?;
        let p = paths.first().ok_or("no connection to update")?;
        let r = bus
            .call_method(Some(NM), p.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "GetSettings", &())
            .await
            .map_err(|e| e.to_string())?;
        let s: std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>> =
            r.body().deserialize().map_err(|e| e.to_string())?;
        bus.call_method(Some(NM), p.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "Update", &(s,))
            .await
            .map_err(|e| format!("Update: {e}"))?;
        let u = bool::try_from(prop(bus, p.as_str(), "org.freedesktop.NetworkManager.Settings.Connection", "Unsaved").await?)
            .map_err(|e| e.to_string())?;
        ensure(u, "Unsaved is false after a read-only Update")?;
        Ok(format!("{p} Unsaved=true"))
    });
    check!(t, "ro: Wi-Fi toggle refused, state unchanged", {
        let r = nm.set_wireless_enabled(false).await;
        ensure(r.is_err(), "WirelessEnabled=false was accepted")?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(s.wifi.enabled, "radio went off")?;
        Ok(format!("{}", r.err().map(|e| e.to_string()).unwrap_or_default()))
    });
}

/// The slot files a netdev member can read (root:netdev 0660): which one
/// holds `ssid` (the shim writes the other slot with seq + 1 each time).
fn slot_with(ssid: &str) -> Result<String, String> {
    let mut found = Vec::new();
    for slot in ["a", "b"] {
        let p = format!("/etc/leandros/network/profiles.{slot}");
        let b = std::fs::read(&p).map_err(|e| format!("{p}: {e}"))?;
        if b.windows(ssid.len()).any(|w| w == ssid.as_bytes()) {
            found.push(format!("profiles.{slot} ({} bytes)", b.len()));
        }
    }
    if found.is_empty() { Err(format!("{ssid} in neither slot")) } else { Ok(found.join(", ")) }
}

/// The real stack: shim -> /dev/netctl -> wifi_mgr -> wifi_sim, as the
/// applet drives it. Ends with SimHome saved (autoconnect) and connected.
async fn real_stack(t: &mut T, bus: &zbus::Connection, cleanup: bool) {
    let nm = match NetworkManager::new().await {
        Ok(nm) => nm,
        Err(e) => {
            t.report("NetworkManager::new", Instant::now(), Err(e.to_string()));
            return;
        }
    };
    let events = Arc::new(Mutex::new(0u32));
    let mut ev_task = None;
    check!(t, "network_events() subscribe", {
        let mut s = nm.network_events().await.map_err(|e| e.to_string())?;
        let n = events.clone();
        ev_task = Some(tokio::spawn(async move {
            while let Some(e) = s.next().await {
                if e.is_ok() {
                    *n.lock().unwrap() += 1;
                }
            }
        }));
        Ok("stream open".to_string())
    });

    check!(t, "GetPermissions: network-control yes", {
        let r = bus
            .call_method(Some(NM), "/org/freedesktop/NetworkManager", Some(NM), "GetPermissions", &())
            .await
            .map_err(|e| e.to_string())?;
        let p: std::collections::HashMap<String, String> = r.body().deserialize().map_err(|e| e.to_string())?;
        let v = p.get("org.freedesktop.NetworkManager.network-control").cloned().unwrap_or_default();
        ensure(v == "yes", format!("network-control = {v:?} (is this user in netdev?)"))?;
        Ok("network-control=yes".into())
    });

    // A previous run (or a reboot test) may have left SimHome saved and
    // connected: start from a clean slate.
    {
        let _ = nm.disconnect(Some("wifi0")).await;
        let _ = nm.forget(SIM_HOME).await;
    }

    check!(t, "snapshot: wifi0 + eth0, wired AC with IPv4", {
        let s = nm.snapshot().await.map_err(|e| format!("snapshot: {e}"))?;
        ensure(s.wifi.enabled && s.wifi.present && s.wifi.hardware_enabled, format!("wifi radio {:?}", s.wifi))?;
        ensure(s.wifi_devices.iter().any(|d| d.interface == "wifi0"), "no wifi0 device")?;
        ensure(s.wired_devices.iter().any(|d| d.interface == "eth0"), "no eth0 device")?;
        let w = s
            .active_connections
            .iter()
            .find_map(|c| match c {
                ActiveConnection::Wired(w) => Some(w),
                _ => None,
            })
            .ok_or("no wired active connection")?;
        ensure(w.state == ActiveConnectionState::Activated, format!("wired AC state {:?}", w.state))?;
        let ip = w.ip4_address.clone().ok_or("wired AC has no IPv4")?;
        Ok(format!("eth0 {ip}; {} APs", s.access_points.len()))
    });

    let wifi0 = device_path(bus, "wifi0").await.unwrap_or_default();
    let eth0 = device_path(bus, "eth0").await.unwrap_or_default();

    check!(t, "RequestScan + LastScan advances", {
        let before = i64::try_from(prop(bus, &wifi0, "org.freedesktop.NetworkManager.Device.Wireless", "LastScan").await?)
            .map_err(|e| e.to_string())?;
        ensure(before != -1, "LastScan is -1")?;
        nm.scan_networks(Some("wifi0")).await.map_err(|e| e.to_string())?;
        let d = wait_until(Duration::from_secs(8), || async {
            prop(bus, &wifi0, "org.freedesktop.NetworkManager.Device.Wireless", "LastScan")
                .await
                .ok()
                .and_then(|v| i64::try_from(v).ok())
                .is_some_and(|v| v > before)
        })
        .await?;
        Ok(format!("LastScan {before} -> advanced in {} ms", d.as_millis()))
    });

    let mut home_bssid = String::new();
    check!(t, "AP list: sim APs, WPA3 shown, hidden not shown", {
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let names: Vec<&str> = s.access_points.iter().map(|a| a.ssid.as_str()).collect();
        for want in [SIM_HOME, "SimCafe", "SimMixed", "SimOpen"] {
            ensure(names.contains(&want), format!("{want} missing from {names:?}"))?;
        }
        ensure(names.contains(&"SimWPA3"), format!("SAE-only SimWPA3 (joinable since the SAE lane) missing from {names:?}"))?;
        let wpa3 = s.access_points.iter().find(|a| a.ssid == "SimWPA3").ok_or("no SimWPA3")?;
        ensure(wpa3.security.sae, format!("SimWPA3 security {:?}", wpa3.security))?;
        ensure(names.contains(&"SimTkip"), format!("SimTkip (mixed mode, TKIP group + CCMP pairwise, joinable) missing from {names:?}"))?;
        ensure(!names.contains(&"SimTkipOnly"), "SimTkipOnly (pure TKIP, not joinable) is listed")?;
        ensure(!names.contains(&"SimHidden"), "hidden SimHidden is listed")?;
        ensure(!names.iter().any(|n| n.is_empty()), "an AP with an empty SSID is listed")?;
        let mixed = s.access_points.iter().find(|a| a.ssid == "SimMixed").ok_or("no SimMixed")?;
        ensure(mixed.security.psk || mixed.security.sae, format!("SimMixed security {:?}", mixed.security))?;
        let open = s.access_points.iter().find(|a| a.ssid == "SimOpen").ok_or("no SimOpen")?;
        ensure(open.security.is_open(), "SimOpen is not open")?;
        let cafe = s.access_points.iter().find(|a| a.ssid == "SimCafe").ok_or("no SimCafe")?;
        ensure(cafe.frequency_mhz > 5000, format!("SimCafe at {} MHz (want 5 GHz)", cafe.frequency_mhz))?;
        for a in &s.access_points {
            ensure(a.bssid.len() == 17 && a.bssid.matches(':').count() == 5, format!("bad BSSID {:?}", a.bssid))?;
        }
        ensure(!s.applet_summary().wifi_groups.is_empty(), "applet_summary has no Wi-Fi groups")?;
        home_bssid = strongest_bssid(&s, SIM_HOME).ok_or("no SimHome BSSID")?;
        Ok(format!("{names:?}"))
    });

    let dev_sigs: Arc<Mutex<Vec<(u32, u32, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let sig_task = {
        let sigs = dev_sigs.clone();
        match zbus::Proxy::new(bus, NM, wifi0.clone(), "org.freedesktop.NetworkManager.Device").await {
            Ok(p) => match p.receive_signal("StateChanged").await {
                Ok(mut st) => Some(tokio::spawn(async move {
                    while let Some(m) = st.next().await {
                        if let Ok(a) = m.body().deserialize::<(u32, u32, u32)>() {
                            sigs.lock().unwrap().push(a);
                        }
                    }
                })),
                Err(_) => None,
            },
            Err(_) => None,
        }
    };

    check!(t, "wrong key -> AuthFailed, dev 120/7", {
        let t0 = Instant::now();
        let r = nm
            .connect_to_bssid(SIM_HOME, Some(&home_bssid), Some("wifi0"), WifiSecurity::WpaPsk { psk: SIM_WRONG.into() })
            .await;
        let took = t0.elapsed();
        match r {
            Err(ConnectionError::AuthFailed) => {}
            Err(e) => return Err(format!("expected AuthFailed, got {e}")),
            Ok(()) => return Err("connected with a wrong key".into()),
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let sigs = dev_sigs.lock().unwrap().clone();
        ensure(sigs.iter().any(|s| s.0 == 120 && s.2 == 7), format!("no StateChanged(120, _, 7) in {sigs:?}"))?;
        Ok(format!("AuthFailed after {} ms", took.as_millis()))
    });

    check!(t, "right key -> activated, IPv4 on 10.0.3/24", {
        let t0 = Instant::now();
        nm.connect_to_bssid(SIM_HOME, Some(&home_bssid), Some("wifi0"), WifiSecurity::WpaPsk { psk: SIM_HOME_KEY.into() })
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let took = t0.elapsed();
        // DHCP over the sim's data path follows the association.
        let ip = {
            let mut ip = None;
            let t1 = Instant::now();
            while t1.elapsed() < Duration::from_secs(20) {
                let s = nm.snapshot().await.map_err(|e| e.to_string())?;
                if let Some(a) = active_wifi(&s).and_then(|w| w.ip4_address.clone()) {
                    ip = Some(a);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            ip.ok_or("no IPv4 on the Wi-Fi AC after 20 s")?
        };
        ensure(ip.starts_with("10.0.3."), format!("wifi0 address {ip}"))?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let w = active_wifi(&s).ok_or("no Wi-Fi active connection")?;
        ensure(w.ssid == SIM_HOME && w.id == SIM_HOME, format!("AC id/ssid {:?}/{:?}", w.id, w.ssid))?;
        ensure(w.state == ActiveConnectionState::Activated, format!("AC state {:?}", w.state))?;
        ensure(saved_wifi_count(&s, SIM_HOME) == 1, format!("{} saved SimHome profiles (want 1)", saved_wifi_count(&s, SIM_HOME)))?;
        ensure(dev_state(bus, &wifi0).await? == 100, "device not 100")?;
        Ok(format!("connected in {} ms, {ip}", took.as_millis()))
    });

    check!(t, "saved profile is in a profile slot", slot_with(SIM_HOME));

    check!(t, "Settings: GetSettings has no secrets", {
        let r = bus
            .call_method(Some(NM), "/org/freedesktop/NetworkManager/Settings", Some("org.freedesktop.NetworkManager.Settings"), "ListConnections", &())
            .await
            .map_err(|e| e.to_string())?;
        let paths: Vec<zbus::zvariant::OwnedObjectPath> = r.body().deserialize().map_err(|e| e.to_string())?;
        for p in &paths {
            let r = bus
                .call_method(Some(NM), p.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "GetSettings", &())
                .await
                .map_err(|e| e.to_string())?;
            let s: std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>> =
                r.body().deserialize().map_err(|e| e.to_string())?;
            ensure(!s.values().any(|sec| sec.contains_key("psk")), format!("{p}: psk in GetSettings"))?;
        }
        Ok(format!("{} connections", paths.len()))
    });

    check!(t, "disconnect -> 30 in < 10 s", {
        let t0 = Instant::now();
        nm.disconnect(Some("wifi0")).await.map_err(|e| e.to_string())?;
        let d = wait_until(Duration::from_secs(10), || async { dev_state(bus, &wifi0).await.is_ok_and(|s| s == 30) }).await?;
        let took = t0.elapsed();
        ensure(took < Duration::from_secs(10), format!("took {took:?}"))?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(active_wifi(&s).is_none(), "Wi-Fi AC still listed")?;
        Ok(format!("disconnected in {} ms (state 30 seen after {} ms)", took.as_millis(), d.as_millis()))
    });

    check!(t, "no autoconnect after a user disconnect (5 s)", {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let st = dev_state(bus, &wifi0).await?;
        ensure(st == 30, format!("wifi0 state {st}: autoconnect undid the user's disconnect"))?;
        Ok("stayed disconnected".into())
    });

    check!(t, "reconnect from the saved profile (no password)", {
        let t0 = Instant::now();
        nm.connect_to_bssid(SIM_HOME, Some(&home_bssid), Some("wifi0"), WifiSecurity::WpaPsk { psk: String::new() })
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let w = active_wifi(&s).ok_or("no Wi-Fi AC")?;
        ensure(w.state == ActiveConnectionState::Activated, format!("AC state {:?}", w.state))?;
        Ok(format!("activated in {} ms", t0.elapsed().as_millis()))
    });

    check!(t, "switch to the open network SimOpen", {
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let b = strongest_bssid(&s, "SimOpen").ok_or("no SimOpen")?;
        nm.connect_to_bssid("SimOpen", Some(&b), Some("wifi0"), WifiSecurity::Open).await.map_err(|e| format!("connect: {e}"))?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(active_wifi(&s).is_some_and(|w| w.ssid == "SimOpen"), "SimOpen not active")?;
        nm.disconnect(Some("wifi0")).await.map_err(|e| e.to_string())?;
        let _ = nm.forget("SimOpen").await;
        Ok("ok".into())
    });

    check!(t, "AP that refuses association fails, no hang", {
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let Some(b) = strongest_bssid(&s, "SimFull") else { return Ok("SimFull not listed (skipped)".to_string()) };
        let t0 = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(60),
            nm.connect_to_bssid("SimFull", Some(&b), Some("wifi0"), WifiSecurity::WpaPsk { psk: "full-pass-123".into() }),
        )
        .await
        .map_err(|_| "connect did not return within 60 s".to_string())?;
        let _ = nm.forget("SimFull").await;
        match r {
            Ok(()) => Err("connected to an AP that refuses association".into()),
            Err(e) => {
                let m = e.to_string();
                if m.contains("unknown") {
                    Err(format!("refusal reported as '{m}' (Device.StateReason lost)"))
                } else {
                    Ok(format!("{m} after {} ms", t0.elapsed().as_millis()))
                }
            }
        }
    });

    check!(t, "wired: Device.Disconnect then connect_wired (admin)", {
        bus.call_method(Some(NM), eth0.as_str(), Some("org.freedesktop.NetworkManager.Device"), "Disconnect", &())
            .await
            .map_err(|e| format!("Disconnect: {e}"))?;
        let d = wait_until(Duration::from_secs(5), || async { dev_state(bus, &eth0).await.is_ok_and(|s| s == 30) }).await?;
        let t1 = Instant::now();
        nm.connect_wired().await.map_err(|e| format!("connect_wired: {e}"))?;
        wait_until(Duration::from_secs(20), || async { dev_state(bus, &eth0).await.is_ok_and(|s| s == 100) }).await?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        let w = s
            .active_connections
            .iter()
            .find_map(|c| match c {
                ActiveConnection::Wired(w) => Some(w),
                _ => None,
            })
            .ok_or("wired AC not back")?;
        Ok(format!("eth0 down in {} ms, back to 100 in {} ms ({:?})", d.as_millis(), t1.elapsed().as_millis(), w.ip4_address))
    });

    check!(t, "Wi-Fi off: no APs; on: APs back", {
        nm.set_wireless_enabled(false).await.map_err(|e| e.to_string())?;
        wait_until(Duration::from_secs(5), || async { nm.snapshot().await.is_ok_and(|s| !s.wifi.enabled && s.access_points.is_empty()) })
            .await
            .map_err(|e| format!("off: {e}"))?;
        ensure(dev_state(bus, &wifi0).await? == 20, "wifi0 not 20 with the radio off")?;
        nm.set_wireless_enabled(true).await.map_err(|e| e.to_string())?;
        let took = wait_until(Duration::from_secs(15), || async {
            nm.snapshot().await.is_ok_and(|s| s.wifi.enabled && s.access_points.iter().any(|a| a.ssid == SIM_HOME))
        })
        .await
        .map_err(|e| format!("on: {e}"))?;
        Ok(format!("APs back {} ms after radio on", took.as_millis()))
    });

    // Leave SimHome connected (and saved, autoconnect) for the reboot check.
    check!(t, "final: SimHome connected (radio on autoconnects or reconnect)", {
        let ok = wait_until(Duration::from_secs(10), || async {
            nm.snapshot().await.ok().and_then(|s| active_wifi(&s).map(|w| w.ssid == SIM_HOME && w.state == ActiveConnectionState::Activated)).unwrap_or(false)
        })
        .await;
        let how = if ok.is_ok() {
            "autoconnected after radio on"
        } else {
            nm.connect(SIM_HOME, Some("wifi0"), WifiSecurity::WpaPsk { psk: String::new() }).await.map_err(|e| format!("connect: {e}"))?;
            "reconnected"
        };
        Ok(how.into())
    });

    if cleanup {
        check!(t, "forget -> profile gone", {
            nm.forget(SIM_HOME).await.map_err(|e| e.to_string())?;
            let s = nm.snapshot().await.map_err(|e| e.to_string())?;
            ensure(saved_wifi_count(&s, SIM_HOME) == 0, "SimHome still saved")?;
            Ok(format!("{} saved connections left", s.saved_connections.len()))
        });
    }

    check!(t, "Connectivity is never 2 (portal)", {
        let v = u32::try_from(prop(bus, "/org/freedesktop/NetworkManager", NM, "Connectivity").await?).map_err(|e| e.to_string())?;
        ensure(v != 2, "Connectivity = 2")?;
        Ok(format!("Connectivity={v}"))
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    check!(t, "network_events delivered", {
        let n = *events.lock().unwrap();
        ensure(n > 0, "no NetworkEvent at all")?;
        Ok(format!("{n} events"))
    });
    if let Some(h) = ev_task {
        h.abort();
    }
    if let Some(h) = sig_task {
        h.abort();
    }
}

/// After a reboot with SimHome saved: the kernel autoconnected from the
/// profile slots before anyone asked, and the shim shows it.
async fn real_after_reboot(t: &mut T) {
    let nm = match NetworkManager::new().await {
        Ok(nm) => nm,
        Err(e) => {
            t.report("NetworkManager::new", Instant::now(), Err(e.to_string()));
            return;
        }
    };
    check!(t, "after reboot: SimHome still saved", {
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        ensure(saved_wifi_count(&s, SIM_HOME) == 1, format!("{} saved SimHome profiles", saved_wifi_count(&s, SIM_HOME)))?;
        slot_with(SIM_HOME)
    });
    check!(t, "after reboot: autoconnected to SimHome with IPv4", {
        let d = wait_until(Duration::from_secs(60), || async {
            nm.snapshot()
                .await
                .ok()
                .and_then(|s| {
                    active_wifi(&s).map(|w| w.ssid == SIM_HOME && w.state == ActiveConnectionState::Activated && w.ip4_address.is_some())
                })
                .unwrap_or(false)
        })
        .await?;
        let s = nm.snapshot().await.map_err(|e| e.to_string())?;
        Ok(format!("active after {} ms of waiting: {:?}", d.as_millis(), active_wifi(&s).and_then(|w| w.ip4_address.clone())))
    });
    check!(t, "after reboot: SimHome connection.timestamp persisted", {
        let bus = zbus::Connection::system().await.map_err(|e| e.to_string())?;
        let r = bus
            .call_method(Some(NM), "/org/freedesktop/NetworkManager/Settings", Some("org.freedesktop.NetworkManager.Settings"), "ListConnections", &())
            .await
            .map_err(|e| e.to_string())?;
        let paths: Vec<zbus::zvariant::OwnedObjectPath> = r.body().deserialize().map_err(|e| e.to_string())?;
        for p in &paths {
            let r = bus
                .call_method(Some(NM), p.as_str(), Some("org.freedesktop.NetworkManager.Settings.Connection"), "GetSettings", &())
                .await
                .map_err(|e| e.to_string())?;
            let s: std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>> =
                r.body().deserialize().map_err(|e| e.to_string())?;
            let Some(c) = s.get("connection") else { continue };
            let id = c.get("id").and_then(|v| String::try_from(v.try_clone().ok()?).ok());
            if id.as_deref() != Some(SIM_HOME) {
                continue;
            }
            let ts = c.get("timestamp").and_then(|v| u64::try_from(v.try_clone().ok()?).ok()).unwrap_or(0);
            ensure(ts > 0, "connection.timestamp is 0 (never persisted)")?;
            return Ok(format!("{p} timestamp={ts}"));
        }
        Err(format!("no connection with id {SIM_HOME}"))
    });
}
