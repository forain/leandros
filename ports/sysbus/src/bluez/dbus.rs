//! The org.bluez interfaces. Getters read the shared model; methods do the
//! btd request themselves (each on its own btd.sock connection), update the
//! model and kick the engine, which publishes the PropertiesChanged diff.

use std::collections::HashMap;

use bt_host::api::{op, status, Msg};
use bt_host::Addr;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::{log, Sh, ADAPTER_PATH};

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
pub enum BzError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    NotReady(String),
    InProgress(String),
    DoesNotExist(String),
    NotSupported(String),
    AuthenticationFailed(String),
    AuthenticationTimeout(String),
    AuthenticationRejected(String),
    ConnectionAttemptFailed(String),
}

fn status_err(s: u8) -> BzError {
    let n = status::name(s).to_string();
    match s {
        status::NOT_READY => BzError::NotReady(n),
        status::PAGE_TIMEOUT => BzError::ConnectionAttemptFailed("Page Timeout".into()),
        status::AUTH_FAILED => BzError::AuthenticationFailed(n),
        status::SECURITY_BLOCK => BzError::AuthenticationRejected(n),
        status::BUSY => BzError::InProgress(n),
        status::TIMEOUT => BzError::AuthenticationTimeout(n),
        _ => BzError::Failed(n),
    }
}

fn io_err(e: std::io::Error) -> BzError {
    BzError::NotReady(format!("btd: {e}"))
}

pub struct Adapter(pub Sh);

#[zbus::interface(name = "org.bluez.Adapter1")]
impl Adapter {
    async fn start_discovery(&self) -> Result<(), BzError> {
        let m = self.0.lock();
        if !m.ready || !m.powered {
            return Err(BzError::NotReady("Resource Not Ready".into()));
        }
        drop(m);
        super::start_scan(&self.0);
        Ok(())
    }
    async fn stop_discovery(&self) -> Result<(), BzError> {
        self.0.stop_scan();
        Ok(())
    }
    fn set_discovery_filter(&self, _filter: HashMap<String, OwnedValue>) {}
    fn get_discovery_filters(&self) -> Vec<String> {
        ["UUIDs", "RSSI", "Pathloss", "Transport", "DuplicateData", "Discoverable", "Pattern"].map(String::from).to_vec()
    }
    async fn remove_device(&self, device: OwnedObjectPath) -> Result<(), BzError> {
        let Some(a) = super::path_addr(device.as_str()) else {
            return Err(BzError::DoesNotExist("Does Not Exist".into()));
        };
        let paired = match self.0.lock().devs.get(&a.0) {
            Some(d) => d.paired,
            None => return Err(BzError::DoesNotExist("Does Not Exist".into())),
        };
        self.0.drop_held(&a);
        let _ = super::btd_op(Msg::Disconnect { addr: a }, op::DISCONNECT, 5).await;
        if paired {
            let s = super::btd_op(Msg::Remove { addr: a }, op::REMOVE, 5).await.map_err(io_err)?;
            if s != status::OK {
                return Err(status_err(s));
            }
        }
        let mut m = self.0.lock();
        m.devs.remove(&a.0);
        m.hidden.insert(a.0);
        drop(m);
        log(format_args!("removed {a}"));
        self.0.kick();
        Ok(())
    }

    #[zbus(property)]
    fn address(&self) -> String {
        self.0.lock().addr.to_string()
    }
    #[zbus(property)]
    fn address_type(&self) -> String {
        "public".into()
    }
    #[zbus(property)]
    fn name(&self) -> String {
        self.0.lock().name.clone()
    }
    #[zbus(property)]
    fn alias(&self) -> String {
        let m = self.0.lock();
        m.alias.clone().unwrap_or_else(|| m.name.clone())
    }
    #[zbus(property)]
    fn set_alias(&self, v: String) {
        self.0.lock().alias = if v.is_empty() { None } else { Some(v) };
        self.0.kick();
    }
    #[zbus(property)]
    fn class(&self) -> u32 {
        0x00010c
    }
    #[zbus(property)]
    fn powered(&self) -> bool {
        self.0.lock().powered
    }
    /// Soft power: btd keeps the controller up; off stops discovery and
    /// drops the links this shim holds, and refuses new discovery.
    #[zbus(property)]
    fn set_powered(&self, v: bool) {
        self.0.lock().powered = v;
        if !v {
            self.0.stop_scan();
            self.0.held.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        log(format_args!("Powered = {v}"));
        self.0.kick();
    }
    #[zbus(property)]
    fn power_state(&self) -> String {
        if self.0.lock().powered { "on" } else { "off" }.into()
    }
    #[zbus(property)]
    fn discoverable(&self) -> bool {
        self.0.lock().discoverable
    }
    #[zbus(property)]
    fn set_discoverable(&self, v: bool) {
        self.0.lock().discoverable = v;
        self.0.kick();
    }
    #[zbus(property)]
    fn discoverable_timeout(&self) -> u32 {
        180
    }
    #[zbus(property)]
    fn set_discoverable_timeout(&self, _v: u32) {}
    #[zbus(property)]
    fn pairable(&self) -> bool {
        self.0.lock().pairable
    }
    #[zbus(property)]
    fn set_pairable(&self, v: bool) {
        self.0.lock().pairable = v;
        self.0.kick();
    }
    #[zbus(property)]
    fn pairable_timeout(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn set_pairable_timeout(&self, _v: u32) {}
    #[zbus(property)]
    fn discovering(&self) -> bool {
        self.0.lock().discovering
    }
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn modalias(&self) -> String {
        "usb:v1D6Bp0246d0548".into()
    }
    #[zbus(property)]
    fn roles(&self) -> Vec<String> {
        vec!["central".into()]
    }
    #[zbus(property)]
    fn experimental_features(&self) -> Vec<String> {
        Vec::new()
    }
}

pub struct Device {
    pub sh: Sh,
    pub addr: Addr,
}

impl Device {
    fn get<R: Default>(&self, f: impl FnOnce(&super::Dev) -> R) -> R {
        self.sh.lock().devs.get(&self.addr.0).map(f).unwrap_or_default()
    }
}

#[zbus::interface(name = "org.bluez.Device1")]
impl Device {
    async fn pair(&self) -> Result<(), BzError> {
        let a = self.addr;
        if self.get(|d| d.paired) {
            return Err(BzError::Failed("Already Exists".into()));
        }
        log(format_args!("pair {a}"));
        let s = super::btd_op(Msg::Pair { addr: a }, op::PAIR, 60).await.map_err(io_err)?;
        if s != status::OK {
            log(format_args!("pair {a}: {}", status::name(s)));
            return Err(status_err(s));
        }
        if let Some(d) = self.sh.lock().devs.get_mut(&a.0) {
            d.paired = true;
            d.trusted = true;
        }
        self.sh.kick();
        Ok(())
    }
    fn cancel_pairing(&self) {}
    async fn connect(&self) -> Result<(), BzError> {
        let a = self.addr;
        let (ready, powered, connected, hid, le) = {
            let m = self.sh.lock();
            let d = m.devs.get(&a.0);
            (m.ready, m.powered, d.map(|d| d.connected).unwrap_or(false), d.map(|d| d.is_hid()).unwrap_or(false), d.map(|d| d.le).unwrap_or(false))
        };
        if !ready || !powered {
            return Err(BzError::NotReady("Resource Not Ready".into()));
        }
        if le {
            return Err(BzError::NotSupported("LE connections are not supported yet".into()));
        }
        if connected && (hid || self.sh.held.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&a.0)) {
            return Ok(());
        }
        log(format_args!("connect {a} ({})", if hid { "HID" } else { "generic" }));
        if hid && super::hid_hook(&a) {
            if super::wait_connected(&self.sh, &a, true, 15).await {
                self.sh.kick();
                return Ok(());
            }
            return Err(BzError::Failed("br-connection-page-timeout".into()));
        }
        super::hold_link(&self.sh, a).await.map_err(status_err)?;
        if let Some(d) = self.sh.lock().devs.get_mut(&a.0) {
            d.connected = true;
        }
        self.sh.kick();
        Ok(())
    }
    async fn disconnect(&self) -> Result<(), BzError> {
        let a = self.addr;
        log(format_args!("disconnect {a}"));
        self.sh.drop_held(&a);
        let s = super::btd_op(Msg::Disconnect { addr: a }, op::DISCONNECT, 5).await.map_err(io_err)?;
        if s != status::OK && s != status::BAD_REQUEST {
            log(format_args!("disconnect {a}: {}", status::name(s)));
        }
        if let Some(d) = self.sh.lock().devs.get_mut(&a.0) {
            d.connected = false;
        }
        self.sh.kick();
        Ok(())
    }
    async fn connect_profile(&self, _uuid: String) -> Result<(), BzError> {
        self.connect().await
    }
    async fn disconnect_profile(&self, _uuid: String) -> Result<(), BzError> {
        self.disconnect().await
    }

    #[zbus(property)]
    fn address(&self) -> String {
        self.addr.to_string()
    }
    #[zbus(property)]
    fn address_type(&self) -> String {
        if self.get(|d| d.random) { "random" } else { "public" }.into()
    }
    #[zbus(property)]
    fn name(&self) -> String {
        self.get(|d| d.alias())
    }
    #[zbus(property)]
    fn alias(&self) -> String {
        self.get(|d| d.alias())
    }
    #[zbus(property)]
    fn set_alias(&self, v: String) {
        if let Some(d) = self.sh.lock().devs.get_mut(&self.addr.0) {
            d.alias = if v.is_empty() { None } else { Some(v) };
        }
        self.sh.kick();
    }
    #[zbus(property)]
    fn class(&self) -> u32 {
        self.get(|d| d.class)
    }
    #[zbus(property)]
    fn icon(&self) -> String {
        self.get(|d| d.icon().to_string())
    }
    #[zbus(property)]
    fn paired(&self) -> bool {
        self.get(|d| d.paired)
    }
    #[zbus(property)]
    fn bonded(&self) -> bool {
        self.get(|d| d.paired)
    }
    #[zbus(property)]
    fn trusted(&self) -> bool {
        self.get(|d| d.trusted)
    }
    #[zbus(property)]
    fn set_trusted(&self, v: bool) {
        if let Some(d) = self.sh.lock().devs.get_mut(&self.addr.0) {
            d.trusted = v;
        }
        self.sh.kick();
    }
    #[zbus(property)]
    fn blocked(&self) -> bool {
        self.get(|d| d.blocked)
    }
    #[zbus(property)]
    fn set_blocked(&self, v: bool) {
        if let Some(d) = self.sh.lock().devs.get_mut(&self.addr.0) {
            d.blocked = v;
        }
        self.sh.kick();
    }
    #[zbus(property)]
    fn wake_allowed(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.get(|d| d.connected)
    }
    #[zbus(property)]
    fn services_resolved(&self) -> bool {
        self.get(|d| d.connected)
    }
    #[zbus(property)]
    fn legacy_pairing(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn adapter(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(ADAPTER_PATH).expect("valid path")
    }
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> Vec<String> {
        self.get(|d| d.uuids())
    }
    /// BlueZ omits RSSI outside discovery; here it reads 0 then.
    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> i16 {
        self.get(|d| d.rssi.unwrap_or(0))
    }
}

/// Agents are accepted and never called: btd pairs with SSP just-works
/// (NoInputNoOutput, numeric comparison auto-accepted).
pub struct AgentManager;

#[zbus::interface(name = "org.bluez.AgentManager1")]
impl AgentManager {
    fn register_agent(&self, agent: OwnedObjectPath, capability: String) {
        log(format_args!("agent {} ({capability}) registered", agent.as_str()));
    }
    fn unregister_agent(&self, _agent: OwnedObjectPath) {}
    fn request_default_agent(&self, _agent: OwnedObjectPath) {}
}

pub struct ProfileManager;

#[zbus::interface(name = "org.bluez.ProfileManager1")]
impl ProfileManager {
    fn register_profile(&self, _profile: OwnedObjectPath, uuid: String, _options: HashMap<String, OwnedValue>) {
        log(format_args!("profile {uuid} registered (not routed)"));
    }
    fn unregister_profile(&self, _profile: OwnedObjectPath) {}
}
