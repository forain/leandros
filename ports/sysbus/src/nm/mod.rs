//! org.freedesktop.NetworkManager for LeandrOS: a stateless façade over the
//! kernel's `/dev/netctl` (lib/netctl-abi), shaped for the clients the session
//! runs — the pinned COSMIC network applet, which speaks NM only through
//! nmrs 3.4.0, and cosmic-settings' subscription crate.
//!
//! The kernel owns link policy (scanning, association, the 4-way handshake,
//! DHCP, autoconnect) and authorization (O_RDWR on /dev/netctl needs group
//! netdev). This process runs as the session user, opens the node with the
//! user's own credentials, and only translates: kernel events become NM
//! objects, properties and signals; NM method calls become netctl ioctls and
//! profile-slot writes. It is not a trust boundary.
//!
//! Layout:
//!   backend.rs   the Backend trait, the profile-slot store, the null backend
//!   netctl.rs    the real backend: ioctls + the 64-byte event stream
//!   mock.rs      a scripted kernel (APs, right/wrong key, wired eth0, timing)
//!   model.rs     NM object model derived from kernel state, lifetimes, signals
//!   engine.rs    the single task that owns the backend and the model
//!   settings.rs  NM settings dicts <-> netctl profiles
//!   dbus.rs      the zbus interfaces (thin views of the model)
//!
//! Hard rules this file set exists to keep (each one blanks, hangs or panics
//! the applet when broken; see artifacts/notes/lane-nmshim-2026-10-15.md):
//! GetAll answers on every interface; Wwan*/WirelessHardwareEnabled exist;
//! every listed AP answers its properties and stays exported >= 30 s after it
//! is delisted; HwAddress is always a MAC; Device and ActiveConnection emit
//! explicit StateChanged signals as well as PropertiesChanged; ACs carry
//! Id = SSID, Uuid, State, Devices, SpecificObject; the shim makes the uuids;
//! Settings.Connection.Update never errors; RequestScan always succeeds;
//! LastScan is never -1; Connectivity is never 2; strength updates are
//! throttled; a wrong key ends in device 120 reason 7 and AC 4 reason 9; eth0
//! has a synthetic "Wired connection 1" and an active connection; secret agents
//! are accepted and ignored.

mod backend;
mod dbus;
mod engine;
mod mock;
mod model;
mod netctl;
mod settings;

use std::sync::Arc;

pub(crate) use dbus::NmError;

pub const NAME: &str = "org.freedesktop.NetworkManager";
pub const NM_PATH: &str = "/org/freedesktop/NetworkManager";
pub const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
pub const AGENT_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager/AgentManager";

pub(crate) fn log(msg: std::fmt::Arguments<'_>) {
    crate::log("networkmanager", msg);
}

/// Object paths. Numbers are per-shim, monotonic, never reused.
pub(crate) mod path {
    use zbus::zvariant::OwnedObjectPath;

    fn mk(s: String) -> OwnedObjectPath {
        OwnedObjectPath::try_from(s).expect("valid object path")
    }
    pub fn root() -> OwnedObjectPath {
        mk("/".into())
    }
    pub fn dev(ifindex: u32) -> OwnedObjectPath {
        mk(format!("{}/Devices/{ifindex}", super::NM_PATH))
    }
    pub fn ap(n: u32) -> OwnedObjectPath {
        mk(format!("{}/AccessPoint/{n}", super::NM_PATH))
    }
    pub fn ac(n: u32) -> OwnedObjectPath {
        mk(format!("{}/ActiveConnection/{n}", super::NM_PATH))
    }
    pub fn ip4(n: u32) -> OwnedObjectPath {
        mk(format!("{}/IP4Config/{n}", super::NM_PATH))
    }
    pub fn conn(n: u32) -> OwnedObjectPath {
        mk(format!("{}/{n}", super::SETTINGS_PATH))
    }
    /// The trailing number of `<prefix>/<n>`, if `p` has that prefix.
    pub fn num(p: &str, prefix: &str) -> Option<u32> {
        p.strip_prefix(prefix)?.strip_prefix('/')?.parse().ok()
    }
    pub fn dev_num(p: &str) -> Option<u32> {
        num(p, &format!("{}/Devices", super::NM_PATH))
    }
    pub fn ap_num(p: &str) -> Option<u32> {
        num(p, &format!("{}/AccessPoint", super::NM_PATH))
    }
    pub fn ac_num(p: &str) -> Option<u32> {
        num(p, &format!("{}/ActiveConnection", super::NM_PATH))
    }
    pub fn conn_num(p: &str) -> Option<u32> {
        num(p, super::SETTINGS_PATH)
    }
}

/// Start the service: pick a backend, export the static objects, claim the
/// name, and spawn the engine. Returns the connection; main() serves until the
/// bus goes away.
pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    let (backend, events) = backend::open();
    log(format_args!(
        "backend {} ({})",
        backend.name(),
        if backend.writable() { "read-write: may manage" } else { "read-only: not in group netdev" }
    ));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(model::Shared::new(backend.writable(), backend.features(), tx));
    let conn = builder
        .serve_at(NM_PATH, dbus::Root(shared.clone()))?
        .serve_at(SETTINGS_PATH, dbus::SettingsIface(shared.clone()))?
        .serve_at(AGENT_MANAGER_PATH, dbus::AgentManager)?
        .serve_at("/org/freedesktop", zbus::fdo::ObjectManager)?
        .build()
        .await?;
    // Objects first, then the name: a client that sees the name owner can
    // already read everything (the engine exports devices before it requests
    // the name, see Engine::run).
    let eng = engine::Engine::new(conn.clone(), shared, backend, events, rx);
    tokio::spawn(eng.run());
    Ok(conn)
}
