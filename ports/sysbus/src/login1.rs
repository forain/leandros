//! org.freedesktop.login1 (systemd-logind's interface), describing the one
//! thing LeandrOS has: a single local graphical session on seat0, owned by the
//! user this service runs as (busd spawns it as the session user).
//!
//! Objects:
//!   /org/freedesktop/login1                     Manager
//!   /org/freedesktop/login1/session/c1          Session (also at .../auto and
//!   /org/freedesktop/login1/session/self         .../self, logind's aliases for
//!                                                "the caller's session")
//!   /org/freedesktop/login1/seat/seat0          Seat   (also .../auto, .../self)
//!   /org/freedesktop/login1/user/_<uid>         User   (also .../auto, .../self)
//!
//! Consumers in the COSMIC session:
//!   * cosmic-osd: Session.Id to register its polkit agent; Manager.PowerOff /
//!     Reboot / SetRebootToFirmwareSetup from the shutdown dialog.
//!   * cosmic-session / cosmic-comp: Manager.Inhibit(handle-power-key /
//!     handle-lid-switch, block) -- the returned fd is the inhibitor.
//!   * cosmic-settings-daemon: Session.SetBrightness, Manager.PrepareForSleep.
//!
//! What is real: sessions/seat/user, idle and locked hints, Lock/Unlock
//! signals, inhibitors (tracked until the client closes its fd, listed by
//! ListInhibitors and folded into BlockInhibited/DelayInhibited), and
//! SetBrightness on /sys/class/{backlight,leds}.
//!
//! PowerOff/Reboot/Halt: this process is not root, so it cannot call
//! reboot(2) itself. It forwards the request to init over `/run/user/initctl`
//! (userland/init), which authorises it from the socket's peer credentials --
//! root, or a process in a local session init supervises, which is logind's
//! default polkit policy (`allow_active`) -- and performs the orderly shutdown
//! (SIGTERM, SIGKILL, sync, remount read-only, reboot(2)). PrepareForShutdown
//! (true) is emitted first, as logind does. CanPowerOff/CanReboot/CanHalt
//! answer "yes" while init listens, "na" otherwise.
//!
//! Suspend/Hibernate/HybridSleep and RebootToFirmwareSetup answer
//! org.freedesktop.DBus.Error.NotSupported and their Can* answer "na" --
//! logind's own answer on a system that cannot do it.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

const NAME: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const SESSION_ID: &str = "c1";
const SESSION_PATH: &str = "/org/freedesktop/login1/session/c1";
const SEAT_PATH: &str = "/org/freedesktop/login1/seat/seat0";

fn log(msg: std::fmt::Arguments<'_>) {
    crate::log("login1", msg)
}

fn path(s: &str) -> OwnedObjectPath {
    ObjectPath::try_from(s.to_owned()).expect("valid object path").into()
}

fn not_supported(what: &str) -> zbus::fdo::Error {
    zbus::fdo::Error::NotSupported(format!("{what} is not available on LeandrOS"))
}

/// init's control socket (userland/init, "Shutdown and reboot").
const INITCTL: &str = "/run/user/initctl";

fn initctl_available() -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(INITCTL).map(|m| m.file_type().is_socket()).unwrap_or(false)
}

/// Ask init for `request` ("poweroff" / "reboot" / "halt"). Blocking, but
/// init answers within its supervisor tick (~250 ms).
fn initctl_request(request: &str) -> zbus::fdo::Result<()> {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(INITCTL)
        .map_err(|e| zbus::fdo::Error::Failed(format!("cannot reach init at {INITCTL}: {e}")))?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    s.write_all(format!("{request}\n").as_bytes())
        .map_err(|e| zbus::fdo::Error::Failed(format!("{INITCTL}: {e}")))?;
    let mut reply = String::new();
    let _ = s.read_to_string(&mut reply);
    match reply.trim() {
        "ok" => Ok(()),
        "denied" => Err(zbus::fdo::Error::AccessDenied(format!(
            "{request}: not authorised (the caller is not in a local session)"
        ))),
        other => Err(zbus::fdo::Error::Failed(format!("{request}: init answered {other:?}"))),
    }
}

#[derive(Clone)]
struct Inhibitor {
    id: u64,
    what: String,
    who: String,
    why: String,
    mode: String,
    uid: u32,
    pid: u32,
}

struct Shared {
    uid: u32,
    gid: u32,
    user_name: String,
    user_path: String,
    session_type: String,
    desktop: String,
    tty: String,
    vtnr: u32,
    leader: u32,
    created: (u64, u64),
    idle_hint: Mutex<(bool, u64, u64)>,
    locked_hint: Mutex<bool>,
    inhibitors: Mutex<Vec<Inhibitor>>,
    next_inhibitor: Mutex<u64>,
}

impl Shared {
    fn session_tuple(&self) -> (String, OwnedObjectPath) {
        (SESSION_ID.to_owned(), path(SESSION_PATH))
    }

    fn inhibited(&self, mode: &str) -> String {
        let mut whats: Vec<String> = Vec::new();
        for i in self.inhibitors.lock().unwrap().iter().filter(|i| i.mode == mode) {
            for w in i.what.split(':') {
                if !w.is_empty() && !whats.iter().any(|x| x == w) {
                    whats.push(w.to_owned());
                }
            }
        }
        whats.join(":")
    }
}

fn user_name_for(uid: u32) -> String {
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() > 2 && f[2].parse::<u32>().ok() == Some(uid) {
                return f[0].to_owned();
            }
        }
    }
    std::env::var("USER").unwrap_or_else(|_| format!("uid{uid}"))
}

async fn caller_creds(conn: &zbus::Connection, hdr: &Header<'_>) -> (u32, u32) {
    let Some(sender) = hdr.sender() else { return (0, 0) };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else { return (0, 0) };
    let bus_name: zbus::names::BusName<'_> = sender.clone().into();
    let uid = dbus.get_connection_unix_user(bus_name.clone()).await.unwrap_or(0);
    let pid = dbus.get_connection_unix_process_id(bus_name).await.unwrap_or(0);
    (uid, pid)
}

// ── Manager ──────────────────────────────────────────────────────────────────

struct Manager {
    s: Arc<Shared>,
}

impl Manager {
    /// PowerOff/Reboot/Halt: PrepareForShutdown(true), then init does it.
    /// On a refusal PrepareForShutdown(false) undoes the announcement.
    async fn shutdown(&self, method: &str, request: &'static str, emitter: &SignalEmitter<'_>) -> zbus::fdo::Result<()> {
        log(format_args!("{method} requested: forwarding to init ({INITCTL})"));
        let _ = Manager::prepare_for_shutdown(emitter, true).await;
        let r = tokio::task::spawn_blocking(move || initctl_request(request))
            .await
            .unwrap_or_else(|e| Err(zbus::fdo::Error::Failed(format!("{e}"))));
        match &r {
            Ok(()) => log(format_args!("{method}: init accepted; the system is going down")),
            Err(e) => {
                log(format_args!("{method}: {e}"));
                let _ = Manager::prepare_for_shutdown(emitter, false).await;
            }
        }
        r
    }
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    fn get_session(&self, session_id: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        match session_id {
            SESSION_ID | "auto" | "self" | "" => Ok(path(SESSION_PATH)),
            _ => Err(zbus::fdo::Error::Failed(format!("No session '{session_id}' known"))),
        }
    }

    #[zbus(name = "GetSessionByPID")]
    fn get_session_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        path(SESSION_PATH)
    }

    fn get_user(&self, uid: u32) -> zbus::fdo::Result<OwnedObjectPath> {
        if uid == self.s.uid {
            Ok(path(&self.s.user_path))
        } else {
            Err(zbus::fdo::Error::Failed(format!("User ID {uid} is not logged in")))
        }
    }

    #[zbus(name = "GetUserByPID")]
    fn get_user_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        path(&self.s.user_path)
    }

    fn get_seat(&self, seat_id: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        match seat_id {
            "seat0" | "auto" | "self" | "" => Ok(path(SEAT_PATH)),
            _ => Err(zbus::fdo::Error::Failed(format!("No seat '{seat_id}' known"))),
        }
    }

    fn list_sessions(&self) -> Vec<(String, u32, String, String, OwnedObjectPath)> {
        vec![(
            SESSION_ID.to_owned(),
            self.s.uid,
            self.s.user_name.clone(),
            "seat0".to_owned(),
            path(SESSION_PATH),
        )]
    }

    fn list_users(&self) -> Vec<(u32, String, OwnedObjectPath)> {
        vec![(self.s.uid, self.s.user_name.clone(), path(&self.s.user_path))]
    }

    fn list_seats(&self) -> Vec<(String, OwnedObjectPath)> {
        vec![("seat0".to_owned(), path(SEAT_PATH))]
    }

    fn list_inhibitors(&self) -> Vec<(String, String, String, String, u32, u32)> {
        self.s
            .inhibitors
            .lock()
            .unwrap()
            .iter()
            .map(|i| (i.what.clone(), i.who.clone(), i.why.clone(), i.mode.clone(), i.uid, i.pid))
            .collect()
    }

    /// Returns the write end of a pipe. The inhibitor lives until every copy
    /// of that fd is closed, which we observe as EOF on our read end.
    async fn inhibit(
        &self,
        what: String,
        who: String,
        why: String,
        mode: String,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        const KNOWN: &[&str] = &[
            "shutdown", "sleep", "idle", "handle-power-key", "handle-suspend-key",
            "handle-hibernate-key", "handle-lid-switch", "handle-reboot-key",
        ];
        if what.is_empty() || what.split(':').any(|w| !KNOWN.contains(&w)) {
            return Err(zbus::fdo::Error::InvalidArgs(format!("invalid what specification {what:?}")));
        }
        if mode != "block" && mode != "delay" && mode != "block-weak" {
            return Err(zbus::fdo::Error::InvalidArgs(format!("invalid mode {mode:?}")));
        }
        let mut fds = [0i32; 2];
        // SAFETY: valid two-int array; the fds are ours until wrapped below.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(zbus::fdo::Error::Failed(format!("pipe: {e}")));
        }
        for fd in fds {
            // Not inherited by anything this service might exec.
            // SAFETY: fd is a valid descriptor we own.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        // SAFETY: pipe2 just returned these two fresh descriptors.
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (uid, pid) = caller_creds(conn, &hdr).await;
        let id = {
            let mut n = self.s.next_inhibitor.lock().unwrap();
            *n += 1;
            *n
        };
        log(format_args!("Inhibit what={what} who={who:?} mode={mode} uid={uid} pid={pid} -> #{id}"));
        self.s.inhibitors.lock().unwrap().push(Inhibitor { id, what, who, why, mode, uid, pid });
        let shared = self.s.clone();
        tokio::task::spawn_blocking(move || {
            let mut f = std::fs::File::from(rd);
            let mut buf = [0u8; 64];
            loop {
                match std::io::Read::read(&mut f, &mut buf) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            shared.inhibitors.lock().unwrap().retain(|i| i.id != id);
            log(format_args!("inhibitor #{id} released"));
        });
        Ok(wr.into())
    }

    async fn power_off(
        &self,
        _interactive: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.shutdown("PowerOff", "poweroff", &emitter).await
    }
    async fn reboot(
        &self,
        _interactive: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.shutdown("Reboot", "reboot", &emitter).await
    }
    async fn halt(
        &self,
        _interactive: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.shutdown("Halt", "halt", &emitter).await
    }
    fn suspend(&self, _interactive: bool) -> zbus::fdo::Result<()> {
        Err(not_supported("Suspend"))
    }
    fn hibernate(&self, _interactive: bool) -> zbus::fdo::Result<()> {
        Err(not_supported("Hibernate"))
    }
    fn hybrid_sleep(&self, _interactive: bool) -> zbus::fdo::Result<()> {
        Err(not_supported("HybridSleep"))
    }
    fn suspend_then_hibernate(&self, _interactive: bool) -> zbus::fdo::Result<()> {
        Err(not_supported("SuspendThenHibernate"))
    }
    fn can_power_off(&self) -> &str {
        if initctl_available() { "yes" } else { "na" }
    }
    fn can_reboot(&self) -> &str {
        if initctl_available() { "yes" } else { "na" }
    }
    fn can_halt(&self) -> &str {
        if initctl_available() { "yes" } else { "na" }
    }
    fn can_suspend(&self) -> &str {
        "na"
    }
    fn can_hibernate(&self) -> &str {
        "na"
    }
    fn can_hybrid_sleep(&self) -> &str {
        "na"
    }
    fn can_suspend_then_hibernate(&self) -> &str {
        "na"
    }
    fn can_reboot_to_firmware_setup(&self) -> &str {
        "na"
    }
    fn set_reboot_to_firmware_setup(&self, _enable: bool) -> zbus::fdo::Result<()> {
        Err(not_supported("RebootToFirmwareSetup"))
    }

    async fn lock_session(
        &self,
        session_id: &str,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        self.get_session(session_id)?;
        emit_session_signal(conn, true).await;
        Ok(())
    }
    async fn unlock_session(
        &self,
        session_id: &str,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        self.get_session(session_id)?;
        emit_session_signal(conn, false).await;
        Ok(())
    }
    async fn lock_sessions(&self, #[zbus(connection)] conn: &zbus::Connection) {
        emit_session_signal(conn, true).await;
    }
    async fn unlock_sessions(&self, #[zbus(connection)] conn: &zbus::Connection) {
        emit_session_signal(conn, false).await;
    }
    fn activate_session(&self, session_id: &str) -> zbus::fdo::Result<()> {
        self.get_session(session_id).map(|_| ())
    }
    fn terminate_session(&self, _session_id: &str) -> zbus::fdo::Result<()> {
        Err(not_supported("TerminateSession"))
    }
    fn kill_session(&self, _session_id: &str, _who: &str, _signal: i32) -> zbus::fdo::Result<()> {
        Err(not_supported("KillSession"))
    }

    #[zbus(signal)]
    async fn session_new(e: &SignalEmitter<'_>, id: &str, path: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn session_removed(e: &SignalEmitter<'_>, id: &str, path: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn user_new(e: &SignalEmitter<'_>, uid: u32, path: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn user_removed(e: &SignalEmitter<'_>, uid: u32, path: ObjectPath<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn prepare_for_shutdown(e: &SignalEmitter<'_>, start: bool) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn prepare_for_sleep(e: &SignalEmitter<'_>, start: bool) -> zbus::Result<()>;

    #[zbus(property, name = "NAutoVTs")]
    fn n_auto_vts(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn kill_user_processes(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn idle_hint(&self) -> bool {
        self.s.idle_hint.lock().unwrap().0
    }
    #[zbus(property)]
    fn idle_since_hint(&self) -> u64 {
        self.s.idle_hint.lock().unwrap().1
    }
    #[zbus(property)]
    fn idle_since_hint_monotonic(&self) -> u64 {
        self.s.idle_hint.lock().unwrap().2
    }
    #[zbus(property)]
    fn block_inhibited(&self) -> String {
        self.s.inhibited("block")
    }
    #[zbus(property)]
    fn delay_inhibited(&self) -> String {
        self.s.inhibited("delay")
    }
    #[zbus(property, name = "InhibitDelayMaxUSec")]
    fn inhibit_delay_max_usec(&self) -> u64 {
        5_000_000
    }
    #[zbus(property)]
    fn inhibitors_max(&self) -> u64 {
        8192
    }
    #[zbus(property)]
    fn n_current_inhibitors(&self) -> u64 {
        self.s.inhibitors.lock().unwrap().len() as u64
    }
    #[zbus(property)]
    fn sessions_max(&self) -> u64 {
        8192
    }
    #[zbus(property)]
    fn n_current_sessions(&self) -> u64 {
        1
    }
    #[zbus(property)]
    fn handle_power_key(&self) -> &str {
        "ignore"
    }
    #[zbus(property)]
    fn handle_suspend_key(&self) -> &str {
        "ignore"
    }
    #[zbus(property)]
    fn handle_hibernate_key(&self) -> &str {
        "ignore"
    }
    #[zbus(property)]
    fn handle_lid_switch(&self) -> &str {
        "ignore"
    }
    #[zbus(property)]
    fn idle_action(&self) -> &str {
        "ignore"
    }
    #[zbus(property)]
    fn preparing_for_shutdown(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn preparing_for_sleep(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn docked(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn lid_closed(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn on_external_power(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn reboot_to_firmware_setup(&self) -> bool {
        false
    }
}

async fn emit_session_signal(conn: &zbus::Connection, lock: bool) {
    let member = if lock { "Lock" } else { "Unlock" };
    for p in [SESSION_PATH, "/org/freedesktop/login1/session/auto", "/org/freedesktop/login1/session/self"] {
        let _ = conn
            .emit_signal(None::<()>, p, "org.freedesktop.login1.Session", member, &())
            .await;
    }
}

// ── Session ──────────────────────────────────────────────────────────────────

struct Session {
    s: Arc<Shared>,
}

/// Write `value` to /sys/class/<subsystem>/<name>/brightness, the call logind
/// makes on an unprivileged session's behalf. Names are a single path
/// component, as logind requires.
fn set_brightness(subsystem: &str, name: &str, value: u32) -> zbus::fdo::Result<()> {
    if !matches!(subsystem, "backlight" | "leds") {
        return Err(zbus::fdo::Error::InvalidArgs(format!("subsystem {subsystem:?} not supported")));
    }
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(zbus::fdo::Error::InvalidArgs(format!("invalid device name {name:?}")));
    }
    let p = format!("/sys/class/{subsystem}/{name}/brightness");
    std::fs::write(&p, value.to_string())
        .map_err(|e| zbus::fdo::Error::Failed(format!("{p}: {e}")))
}

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Session {
    async fn lock(&self, #[zbus(connection)] conn: &zbus::Connection) {
        emit_session_signal(conn, true).await;
    }
    async fn unlock(&self, #[zbus(connection)] conn: &zbus::Connection) {
        emit_session_signal(conn, false).await;
    }
    fn activate(&self) {}
    fn terminate(&self) -> zbus::fdo::Result<()> {
        Err(not_supported("Session.Terminate"))
    }
    fn kill(&self, _who: &str, _signal: i32) -> zbus::fdo::Result<()> {
        Err(not_supported("Session.Kill"))
    }
    async fn set_idle_hint(
        &self,
        idle: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let changed = {
            let mut h = self.s.idle_hint.lock().unwrap();
            let changed = h.0 != idle;
            if changed {
                let (wall, mono) = crate::now_usec();
                *h = (idle, wall, mono);
            }
            changed
        };
        if changed {
            let _ = self.idle_hint_changed(&emitter).await;
        }
    }
    async fn set_locked_hint(
        &self,
        locked: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let changed = {
            let mut l = self.s.locked_hint.lock().unwrap();
            let changed = *l != locked;
            *l = locked;
            changed
        };
        if changed {
            let _ = self.locked_hint_changed(&emitter).await;
        }
    }
    fn set_type(&self, _type: &str) -> zbus::fdo::Result<()> {
        Err(not_supported("Session.SetType"))
    }
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::fdo::Result<()> {
        set_brightness(subsystem, name, brightness)
    }
    fn take_control(&self, _force: bool) -> zbus::fdo::Result<()> {
        Err(not_supported("Session.TakeControl"))
    }
    fn release_control(&self) {}
    fn take_device(&self, _major: u32, _minor: u32) -> zbus::fdo::Result<(zbus::zvariant::OwnedFd, bool)> {
        Err(not_supported("Session.TakeDevice"))
    }
    fn release_device(&self, _major: u32, _minor: u32) {}

    #[zbus(property)]
    fn id(&self) -> &str {
        SESSION_ID
    }
    #[zbus(property)]
    fn user(&self) -> (u32, OwnedObjectPath) {
        (self.s.uid, path(&self.s.user_path))
    }
    #[zbus(property)]
    fn name(&self) -> String {
        self.s.user_name.clone()
    }
    #[zbus(property)]
    fn timestamp(&self) -> u64 {
        self.s.created.0
    }
    #[zbus(property)]
    fn timestamp_monotonic(&self) -> u64 {
        self.s.created.1
    }
    #[zbus(property, name = "VTNr")]
    fn vtnr(&self) -> u32 {
        self.s.vtnr
    }
    #[zbus(property)]
    fn seat(&self) -> (String, OwnedObjectPath) {
        ("seat0".to_owned(), path(SEAT_PATH))
    }
    #[zbus(property, name = "TTY")]
    fn tty(&self) -> String {
        self.s.tty.clone()
    }
    #[zbus(property)]
    fn display(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn remote(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn remote_host(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn remote_user(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn service(&self) -> &str {
        "greetd"
    }
    #[zbus(property)]
    fn desktop(&self) -> String {
        self.s.desktop.clone()
    }
    #[zbus(property)]
    fn scope(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn leader(&self) -> u32 {
        self.s.leader
    }
    #[zbus(property)]
    fn audit(&self) -> u32 {
        0
    }
    #[zbus(property, name = "Type")]
    fn type_(&self) -> String {
        self.s.session_type.clone()
    }
    #[zbus(property)]
    fn class(&self) -> &str {
        "user"
    }
    #[zbus(property)]
    fn active(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn state(&self) -> &str {
        "active"
    }
    #[zbus(property)]
    fn idle_hint(&self) -> bool {
        self.s.idle_hint.lock().unwrap().0
    }
    #[zbus(property)]
    fn idle_since_hint(&self) -> u64 {
        self.s.idle_hint.lock().unwrap().1
    }
    #[zbus(property)]
    fn idle_since_hint_monotonic(&self) -> u64 {
        self.s.idle_hint.lock().unwrap().2
    }
    #[zbus(property)]
    fn locked_hint(&self) -> bool {
        *self.s.locked_hint.lock().unwrap()
    }
}

// ── Seat ─────────────────────────────────────────────────────────────────────

struct Seat {
    s: Arc<Shared>,
}

#[zbus::interface(name = "org.freedesktop.login1.Seat")]
impl Seat {
    fn switch_to(&self, _vtnr: u32) -> zbus::fdo::Result<()> {
        Err(not_supported("Seat.SwitchTo"))
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "seat0"
    }
    #[zbus(property)]
    fn active_session(&self) -> (String, OwnedObjectPath) {
        self.s.session_tuple()
    }
    #[zbus(property)]
    fn can_multi_session(&self) -> bool {
        false
    }
    #[zbus(property, name = "CanTTY")]
    fn can_tty(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_graphical(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn sessions(&self) -> Vec<(String, OwnedObjectPath)> {
        vec![self.s.session_tuple()]
    }
    #[zbus(property)]
    fn idle_hint(&self) -> bool {
        self.s.idle_hint.lock().unwrap().0
    }
}

// ── User ─────────────────────────────────────────────────────────────────────

struct User {
    s: Arc<Shared>,
}

#[zbus::interface(name = "org.freedesktop.login1.User")]
impl User {
    #[zbus(property, name = "UID")]
    fn uid(&self) -> u32 {
        self.s.uid
    }
    #[zbus(property, name = "GID")]
    fn gid(&self) -> u32 {
        self.s.gid
    }
    #[zbus(property)]
    fn name(&self) -> String {
        self.s.user_name.clone()
    }
    #[zbus(property)]
    fn timestamp(&self) -> u64 {
        self.s.created.0
    }
    #[zbus(property)]
    fn timestamp_monotonic(&self) -> u64 {
        self.s.created.1
    }
    #[zbus(property)]
    fn runtime_path(&self) -> String {
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", self.s.uid))
    }
    #[zbus(property)]
    fn service(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn slice(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn display(&self) -> (String, OwnedObjectPath) {
        self.s.session_tuple()
    }
    #[zbus(property)]
    fn state(&self) -> &str {
        "active"
    }
    #[zbus(property)]
    fn sessions(&self) -> Vec<(String, OwnedObjectPath)> {
        vec![self.s.session_tuple()]
    }
    #[zbus(property)]
    fn idle_hint(&self) -> bool {
        self.s.idle_hint.lock().unwrap().0
    }
    #[zbus(property)]
    fn linger(&self) -> bool {
        false
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    // SAFETY: plain libc getters.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let created = crate::now_usec();
    let tty = std::env::var("XDG_VTNR")
        .ok()
        .map(|n| format!("tty{n}"))
        .unwrap_or_default();
    let shared = Arc::new(Shared {
        uid,
        gid,
        user_name: user_name_for(uid),
        user_path: format!("/org/freedesktop/login1/user/_{uid}"),
        session_type: std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "wayland".into()),
        desktop: std::env::var("XDG_SESSION_DESKTOP")
            .or_else(|_| std::env::var("XDG_CURRENT_DESKTOP"))
            .unwrap_or_default(),
        vtnr: std::env::var("XDG_VTNR").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        tty,
        // getppid: busd, which dbus-run-session started inside the session.
        // SAFETY: plain libc getter.
        leader: unsafe { libc::getppid() } as u32,
        created,
        idle_hint: Mutex::new((false, created.0, created.1)),
        locked_hint: Mutex::new(false),
        inhibitors: Mutex::new(Vec::new()),
        next_inhibitor: Mutex::new(0),
    });
    let user_path = shared.user_path.clone();
    let mut b = builder.serve_at(MANAGER_PATH, Manager { s: shared.clone() })?;
    for p in [SESSION_PATH, "/org/freedesktop/login1/session/auto", "/org/freedesktop/login1/session/self"] {
        b = b.serve_at(p, Session { s: shared.clone() })?;
    }
    for p in [SEAT_PATH, "/org/freedesktop/login1/seat/auto", "/org/freedesktop/login1/seat/self"] {
        b = b.serve_at(p, Seat { s: shared.clone() })?;
    }
    for p in [user_path.as_str(), "/org/freedesktop/login1/user/auto", "/org/freedesktop/login1/user/self"] {
        b = b.serve_at(p.to_owned(), User { s: shared.clone() })?;
    }
    let conn = b.name(NAME)?.build().await?;
    log(format_args!("session {SESSION_ID} user {} ({uid})", shared.user_name));
    Ok(conn)
}
