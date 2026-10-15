//! The zbus interfaces. Each is a thin view: getters read the model under its
//! lock; anything that changes state is a command to the engine (which owns
//! the backend) answered over a oneshot. Signals are declared here for
//! introspection but emitted by the engine (engine.rs `emit`), and
//! PropertiesChanged comes from the engine's GetAll diff, never from here.

use std::collections::HashMap;
use std::sync::Arc;

use netctl_abi::{iface_flags, ip_state, nm_sec, wifi_caps};
use tokio::sync::oneshot;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::engine::Cmd;
use super::model::{self, ip_str, mac_str, nm, Model, Shared};
use super::path;
use super::settings::{self, ov, Settings};

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.NetworkManager")]
pub enum NmError {
    #[zbus(error)]
    ZBus(zbus::Error),
    PermissionDenied(String),
    UnknownConnection(String),
    UnknownDevice(String),
    ConnectionNotActive(String),
    Failed(String),
    #[zbus(name = "Settings.InvalidConnection")]
    InvalidConnection(String),
}

type Sh = Arc<Shared>;

async fn ask<T>(sh: &Sh, mk: impl FnOnce(oneshot::Sender<Result<T, NmError>>) -> Cmd) -> Result<T, NmError> {
    let (tx, rx) = oneshot::channel();
    sh.cmd.send(mk(tx)).map_err(|_| NmError::Failed("shim engine stopped".into()))?;
    rx.await.map_err(|_| NmError::Failed("shim engine dropped the request".into()))?
}

fn with<R>(sh: &Sh, f: impl FnOnce(&Model) -> R) -> R {
    f(&sh.lock())
}

// ── /org/freedesktop/NetworkManager ─────────────────────────────────────────

pub struct Root(pub Sh);

#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl Root {
    fn get_devices(&self) -> Vec<OwnedObjectPath> {
        with(&self.0, |m| m.device_paths())
    }
    fn get_all_devices(&self) -> Vec<OwnedObjectPath> {
        with(&self.0, |m| m.device_paths())
    }
    fn get_device_by_ip_iface(&self, iface: String) -> Result<OwnedObjectPath, NmError> {
        with(&self.0, |m| {
            m.ifaces
                .iter()
                .find(|(_, i)| i.name() == iface)
                .map(|(k, _)| path::dev(*k))
                .ok_or_else(|| NmError::UnknownDevice(format!("no device {iface}")))
        })
    }
    async fn activate_connection(
        &self,
        connection: OwnedObjectPath,
        device: OwnedObjectPath,
        specific_object: OwnedObjectPath,
    ) -> Result<OwnedObjectPath, NmError> {
        ask(&self.0, |reply| Cmd::Activate { conn: connection, dev: device, spec: specific_object, reply }).await
    }
    async fn add_and_activate_connection(
        &self,
        connection: Settings,
        device: OwnedObjectPath,
        specific_object: OwnedObjectPath,
    ) -> Result<(OwnedObjectPath, OwnedObjectPath), NmError> {
        ask(&self.0, |reply| Cmd::AddAndActivate { settings: connection, dev: device, spec: specific_object, reply }).await
    }
    async fn add_and_activate_connection2(
        &self,
        connection: Settings,
        device: OwnedObjectPath,
        specific_object: OwnedObjectPath,
        _options: HashMap<String, OwnedValue>,
    ) -> Result<(OwnedObjectPath, OwnedObjectPath, HashMap<String, OwnedValue>), NmError> {
        let (c, a) =
            ask(&self.0, |reply| Cmd::AddAndActivate { settings: connection, dev: device, spec: specific_object, reply }).await?;
        Ok((c, a, HashMap::new()))
    }
    async fn deactivate_connection(&self, active_connection: OwnedObjectPath) -> Result<(), NmError> {
        ask(&self.0, |reply| Cmd::Deactivate { ac: active_connection, reply }).await
    }
    fn sleep(&self, _sleep: bool) {}
    fn enable(&self, _enable: bool) {}
    fn reload(&self, _flags: u32) {}
    fn get_permissions(&self) -> HashMap<String, String> {
        let yes = if with(&self.0, |m| m.writable) { "yes" } else { "no" };
        [
            "enable-disable-network",
            "enable-disable-wifi",
            "enable-disable-wwan",
            "enable-disable-wimax",
            "sleep-wake",
            "network-control",
            "wifi.share.protected",
            "wifi.share.open",
            "settings.modify.system",
            "settings.modify.own",
            "settings.modify.hostname",
            "settings.modify.global-dns",
            "reload",
            "checkpoint-rollback",
            "enable-disable-statistics",
            "enable-disable-connectivity-check",
            "wifi.scan",
        ]
        .iter()
        .map(|a| (format!("org.freedesktop.NetworkManager.{a}"), yes.to_owned()))
        .collect()
    }
    fn get_logging(&self) -> (String, String) {
        ("INFO".into(), "PLATFORM,DEVICE,WIFI".into())
    }
    fn set_logging(&self, _level: String, _domains: String) {}
    fn check_connectivity(&self) -> u32 {
        with(&self.0, |m| m.connectivity())
    }
    fn state(&self) -> u32 {
        with(&self.0, |m| m.nm_state)
    }

    #[zbus(signal)]
    async fn check_permissions(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal, name = "StateChanged")]
    async fn state_changed_sig(emitter: &SignalEmitter<'_>, state: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn device_added(emitter: &SignalEmitter<'_>, device_path: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn device_removed(emitter: &SignalEmitter<'_>, device_path: OwnedObjectPath) -> zbus::Result<()>;

    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        with(&self.0, |m| m.device_paths())
    }
    #[zbus(property)]
    fn all_devices(&self) -> Vec<OwnedObjectPath> {
        with(&self.0, |m| m.device_paths())
    }
    #[zbus(property)]
    fn checkpoints(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }
    #[zbus(property)]
    fn networking_enabled(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        with(&self.0, |m| m.wireless_enabled())
    }
    #[zbus(property)]
    async fn set_wireless_enabled(&self, value: bool) -> zbus::fdo::Result<()> {
        ask(&self.0, |reply| Cmd::SetWireless { on: value, reply }).await.map_err(|e| match e {
            NmError::PermissionDenied(m) => zbus::fdo::Error::AccessDenied(m),
            other => zbus::fdo::Error::Failed(other.to_string()),
        })
    }
    #[zbus(property)]
    fn wireless_hardware_enabled(&self) -> bool {
        with(&self.0, |m| m.wireless_hw_enabled())
    }
    #[zbus(property)]
    fn wwan_enabled(&self) -> bool {
        with(&self.0, |m| m.wwan_enabled)
    }
    /// No modems: accepted and remembered (airplane mode sets it), no effect.
    #[zbus(property)]
    fn set_wwan_enabled(&self, value: bool) {
        self.0.lock().wwan_enabled = value;
    }
    #[zbus(property)]
    fn wwan_hardware_enabled(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn wimax_enabled(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn wimax_hardware_enabled(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn radio_flags(&self) -> u32 {
        // NM_RADIO_FLAG_WLAN_AVAILABLE when a Wi-Fi device exists.
        with(&self.0, |m| m.first_wifi().is_some() as u32)
    }
    #[zbus(property)]
    fn active_connections(&self) -> Vec<OwnedObjectPath> {
        with(&self.0, |m| m.live_acs())
    }
    #[zbus(property)]
    fn primary_connection(&self) -> OwnedObjectPath {
        with(&self.0, |m| m.primary_ac().map(path::ac).unwrap_or_else(path::root))
    }
    #[zbus(property)]
    fn primary_connection_type(&self) -> String {
        with(&self.0, |m| {
            m.primary_ac()
                .and_then(|n| m.acs.get(&n))
                .map(|a| if a.wifi { model::WIFI_TYPE } else { model::ETH_TYPE }.to_owned())
                .unwrap_or_default()
        })
    }
    #[zbus(property)]
    fn metered(&self) -> u32 {
        4 // NM_METERED_GUESS_NO
    }
    #[zbus(property)]
    fn activating_connection(&self) -> OwnedObjectPath {
        with(&self.0, |m| {
            m.acs.iter().find(|(_, a)| a.live() && a.state == nm::AC_ACTIVATING).map(|(k, _)| path::ac(*k)).unwrap_or_else(path::root)
        })
    }
    #[zbus(property)]
    fn startup(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn version(&self) -> String {
        "1.48.0".into()
    }
    #[zbus(property)]
    fn version_info(&self) -> Vec<u32> {
        vec![(1 << 16) | (48 << 8)]
    }
    #[zbus(property)]
    fn capabilities(&self) -> Vec<u32> {
        Vec::new()
    }
    #[zbus(property(emits_changed_signal = "false"), name = "State")]
    fn state_prop(&self) -> u32 {
        with(&self.0, |m| m.nm_state)
    }
    /// Never 2 (portal).
    #[zbus(property)]
    fn connectivity(&self) -> u32 {
        with(&self.0, |m| m.connectivity())
    }
    #[zbus(property)]
    fn connectivity_check_available(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn connectivity_check_enabled(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn connectivity_check_uri(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn global_dns_configuration(&self) -> HashMap<String, OwnedValue> {
        HashMap::new()
    }
}

// ── Devices ─────────────────────────────────────────────────────────────────

pub struct Device {
    pub sh: Sh,
    pub ifindex: u32,
}

impl Device {
    fn r<R: Default>(&self, f: impl FnOnce(&Model, &model::Iface) -> R) -> R {
        let m = self.sh.lock();
        m.ifaces.get(&self.ifindex).map(|i| f(&m, i)).unwrap_or_default()
    }
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.Device")]
impl Device {
    async fn disconnect(&self) -> Result<(), NmError> {
        let ifindex = self.ifindex;
        ask(&self.sh, |reply| Cmd::DevDisconnect { ifindex, reply }).await
    }
    fn reapply(&self, _connection: Settings, _version_id: u64, _flags: u32) -> Result<(), NmError> {
        Err(NmError::Failed("Reapply is not supported".into()))
    }
    fn get_applied_connection(&self, _flags: u32) -> Result<(Settings, u64), NmError> {
        let m = self.sh.lock();
        let s = m
            .ifaces
            .get(&self.ifindex)
            .and_then(|i| i.cur_ac)
            .and_then(|n| m.acs.get(&n))
            .and_then(|a| a.conn)
            .and_then(|c| m.conns.get(&c))
            .map(|c| c.settings.clone())
            .ok_or_else(|| NmError::ConnectionNotActive("device has no active connection".into()))?;
        Ok((s, 1))
    }
    fn delete(&self) -> Result<(), NmError> {
        Err(NmError::Failed("hardware devices cannot be deleted".into()))
    }

    #[zbus(signal, name = "StateChanged")]
    async fn state_changed_sig(emitter: &SignalEmitter<'_>, new_state: u32, old_state: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn udi(&self) -> String {
        format!("/sys/class/net/{}", self.r(|_, i| i.name()))
    }
    #[zbus(property)]
    fn path(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn interface(&self) -> String {
        self.r(|_, i| i.name())
    }
    #[zbus(property)]
    fn ip_interface(&self) -> String {
        self.r(|_, i| if i.dev_state >= nm::DEV_IP_CONFIG { i.name() } else { String::new() })
    }
    #[zbus(property)]
    fn driver(&self) -> String {
        self.r(|_, i| i.driver())
    }
    #[zbus(property)]
    fn driver_version(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn firmware_version(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn capabilities(&self) -> u32 {
        // NM_DEVICE_CAP_NM_SUPPORTED | CARRIER_DETECT
        3
    }
    #[zbus(property)]
    fn ip4_address(&self) -> u32 {
        self.r(|_, i| if i.bound() { u32::from_le_bytes(i.ip.addr) } else { 0 })
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        self.r(|_, i| i.dev_state)
    }
    #[zbus(property)]
    fn state_reason(&self) -> (u32, u32) {
        self.r(|_, i| (i.dev_state, i.dev_reason))
    }
    #[zbus(property)]
    fn active_connection(&self) -> OwnedObjectPath {
        self.r(|m, i| i.cur_ac.filter(|n| m.acs.contains_key(n)).map(path::ac)).unwrap_or_else(path::root)
    }
    #[zbus(property)]
    fn ip4_config(&self) -> OwnedObjectPath {
        self.r(|_, i| i.cur_ac.filter(|_| i.bound()).map(path::ip4)).unwrap_or_else(path::root)
    }
    #[zbus(property)]
    fn dhcp4_config(&self) -> OwnedObjectPath {
        path::root()
    }
    #[zbus(property)]
    fn ip6_config(&self) -> OwnedObjectPath {
        path::root()
    }
    #[zbus(property)]
    fn dhcp6_config(&self) -> OwnedObjectPath {
        path::root()
    }
    #[zbus(property)]
    fn managed(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn autoconnect(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn set_autoconnect(&self, _value: bool) {}
    #[zbus(property)]
    fn firmware_missing(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn nm_plugin_missing(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn device_type(&self) -> u32 {
        self.r(|_, i| if i.is_wifi() { nm::TYPE_WIFI } else { nm::TYPE_ETHERNET })
    }
    #[zbus(property)]
    fn available_connections(&self) -> Vec<OwnedObjectPath> {
        with(&self.sh, |m| m.available_conns(self.ifindex))
    }
    #[zbus(property)]
    fn physical_port_id(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn mtu(&self) -> u32 {
        self.r(|_, i| i.nc.mtu as u32)
    }
    #[zbus(property)]
    fn metered(&self) -> u32 {
        4
    }
    #[zbus(property)]
    fn lldp_neighbors(&self) -> Vec<HashMap<String, OwnedValue>> {
        Vec::new()
    }
    #[zbus(property)]
    fn real(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn ip4_connectivity(&self) -> u32 {
        self.r(|_, i| if i.has_gw() { nm::CONNECTIVITY_FULL } else { nm::CONNECTIVITY_NONE })
    }
    #[zbus(property)]
    fn ip6_connectivity(&self) -> u32 {
        nm::CONNECTIVITY_NONE
    }
    #[zbus(property)]
    fn interface_flags(&self) -> u32 {
        // IFF_UP 0x1, IFF_LOWER_UP 0x10000
        self.r(|_, i| {
            (if i.nc.flags & iface_flags::ADMIN_UP != 0 { 1 } else { 0 })
                | (if i.nc.flags & iface_flags::CARRIER != 0 { 0x10000 } else { 0 })
        })
    }
    #[zbus(property)]
    fn hw_address(&self) -> String {
        self.r(|_, i| mac_str(&i.nc.mac))
    }
    #[zbus(property)]
    fn ports(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }
}

pub struct Wireless {
    pub sh: Sh,
    pub ifindex: u32,
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl Wireless {
    fn get_access_points(&self) -> Vec<OwnedObjectPath> {
        with(&self.sh, |m| m.listed_aps(self.ifindex))
    }
    fn get_all_access_points(&self) -> Vec<OwnedObjectPath> {
        with(&self.sh, |m| m.listed_aps(self.ifindex))
    }
    /// Always succeeds (see Cmd::RequestScan).
    async fn request_scan(&self, _options: HashMap<String, OwnedValue>) {
        let (tx, rx) = oneshot::channel();
        if self.sh.cmd.send(Cmd::RequestScan { ifindex: self.ifindex, reply: tx }).is_ok() {
            let _ = rx.await;
        }
    }

    #[zbus(signal)]
    async fn access_point_added(emitter: &SignalEmitter<'_>, access_point: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn access_point_removed(emitter: &SignalEmitter<'_>, access_point: OwnedObjectPath) -> zbus::Result<()>;

    #[zbus(property)]
    fn hw_address(&self) -> String {
        with(&self.sh, |m| m.ifaces.get(&self.ifindex).map(|i| mac_str(&i.nc.mac)).unwrap_or_else(|| mac_str(&[0; 6])))
    }
    #[zbus(property)]
    fn perm_hw_address(&self) -> String {
        with(&self.sh, |m| m.ifaces.get(&self.ifindex).map(|i| mac_str(&i.nc.perm_mac)).unwrap_or_else(|| mac_str(&[0; 6])))
    }
    #[zbus(property)]
    fn mode(&self) -> u32 {
        2 // NM_802_11_MODE_INFRA
    }
    #[zbus(property)]
    fn bitrate(&self) -> u32 {
        with(&self.sh, |m| {
            m.ifaces
                .get(&self.ifindex)
                .and_then(|i| i.wifi)
                .filter(|w| w.state == netctl_abi::wifi_state::CONNECTED)
                .map(|w| w.tx_rate_kbps)
                .unwrap_or(0)
        })
    }
    #[zbus(property)]
    fn access_points(&self) -> Vec<OwnedObjectPath> {
        with(&self.sh, |m| m.listed_aps(self.ifindex))
    }
    #[zbus(property)]
    fn active_access_point(&self) -> OwnedObjectPath {
        with(&self.sh, |m| m.active_ap(self.ifindex))
    }
    #[zbus(property)]
    fn wireless_capabilities(&self) -> u32 {
        with(&self.sh, |m| {
            let caps = m.ifaces.get(&self.ifindex).map(|i| i.nc.wifi_caps).unwrap_or(0);
            // NM_WIFI_DEVICE_CAP_CIPHER_TKIP|CCMP|WPA|RSN = 0x4|0x8|0x10|0x20,
            // FREQ_VALID|2GHZ|5GHZ = 0x100|0x200|0x400
            let mut c = 0x8 | 0x10 | 0x20 | 0x100;
            if caps & wifi_caps::BAND_2GHZ != 0 {
                c |= 0x200;
            }
            if caps & wifi_caps::BAND_5GHZ != 0 {
                c |= 0x400;
            }
            c
        })
    }
    /// CLOCK_BOOTTIME ms of the last scan; never -1 (the subscription crate
    /// reads -1 as "scan failed" and drops every AP).
    #[zbus(property)]
    fn last_scan(&self) -> i64 {
        with(&self.sh, |m| m.ifaces.get(&self.ifindex).map(|i| i.last_scan).unwrap_or(0))
    }
}

pub struct Wired {
    pub sh: Sh,
    pub ifindex: u32,
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.Device.Wired")]
impl Wired {
    #[zbus(property)]
    fn hw_address(&self) -> String {
        with(&self.sh, |m| m.ifaces.get(&self.ifindex).map(|i| mac_str(&i.nc.mac)).unwrap_or_else(|| mac_str(&[0; 6])))
    }
    #[zbus(property)]
    fn perm_hw_address(&self) -> String {
        with(&self.sh, |m| m.ifaces.get(&self.ifindex).map(|i| mac_str(&i.nc.perm_mac)).unwrap_or_else(|| mac_str(&[0; 6])))
    }
    #[zbus(property)]
    fn speed(&self) -> u32 {
        with(&self.sh, |m| m.ifaces.get(&self.ifindex).map(|i| i.nc.speed_mbps).unwrap_or(0))
    }
    #[zbus(property)]
    fn s390_subchannels(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn carrier(&self) -> bool {
        with(&self.sh, |m| m.ifaces.get(&self.ifindex).is_some_and(|i| i.nc.flags & iface_flags::CARRIER != 0))
    }
}

// ── Access points ───────────────────────────────────────────────────────────

pub struct AccessPoint {
    pub sh: Sh,
    pub n: u32,
}

impl AccessPoint {
    fn r<R: Default>(&self, f: impl FnOnce(&Model, &model::Ap) -> R) -> R {
        let m = self.sh.lock();
        m.aps.get(&self.n).map(|a| f(&m, a)).unwrap_or_default()
    }
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
impl AccessPoint {
    #[zbus(property)]
    fn flags(&self) -> u32 {
        self.r(|_, a| a.bss.nm_flags)
    }
    #[zbus(property)]
    fn wpa_flags(&self) -> u32 {
        self.r(|_, a| a.bss.wpa_flags)
    }
    /// SAE is advertised only when the kernel can do it: a transition-mode
    /// AP is then plain PSK to the applet (WPA3 is deferred).
    #[zbus(property)]
    fn rsn_flags(&self) -> u32 {
        self.r(|m, a| {
            if m.features & netctl_abi::features::SAE == 0 && a.bss.rsn_flags & nm_sec::KEY_MGMT_PSK != 0 {
                a.bss.rsn_flags & !nm_sec::KEY_MGMT_SAE
            } else {
                a.bss.rsn_flags
            }
        })
    }
    #[zbus(property)]
    fn ssid(&self) -> Vec<u8> {
        self.r(|_, a| a.bss.ssid().to_vec())
    }
    #[zbus(property)]
    fn frequency(&self) -> u32 {
        self.r(|_, a| a.bss.freq_mhz as u32)
    }
    #[zbus(property)]
    fn hw_address(&self) -> String {
        let m = self.sh.lock();
        mac_str(&m.aps.get(&self.n).map(|a| a.bss.bssid).unwrap_or([0; 6]))
    }
    #[zbus(property)]
    fn mode(&self) -> u32 {
        2
    }
    #[zbus(property)]
    fn max_bitrate(&self) -> u32 {
        self.r(|_, a| a.bss.max_rate_kbps)
    }
    #[zbus(property)]
    fn bandwidth(&self) -> u32 {
        self.r(|_, a| if a.bss.freq_mhz > 5000 { 80 } else { 20 })
    }
    #[zbus(property)]
    fn strength(&self) -> u8 {
        self.r(|_, a| a.strength)
    }
    /// Seconds of CLOCK_BOOTTIME when last seen.
    #[zbus(property)]
    fn last_seen(&self) -> i32 {
        self.r(|_, a| (a.bss.last_seen_ms / 1000) as i32)
    }
}

// ── Active connections and their IP configuration ───────────────────────────

pub struct ActiveConn {
    pub sh: Sh,
    pub n: u32,
}

impl ActiveConn {
    fn r<R: Default>(&self, f: impl FnOnce(&Model, &model::Ac) -> R) -> R {
        let m = self.sh.lock();
        m.acs.get(&self.n).map(|a| f(&m, a)).unwrap_or_default()
    }
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.Connection.Active")]
impl ActiveConn {
    #[zbus(signal, name = "StateChanged")]
    async fn state_changed_sig(emitter: &SignalEmitter<'_>, state: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn connection(&self) -> OwnedObjectPath {
        self.r(|m, a| a.conn.filter(|c| m.conns.contains_key(c)).map(path::conn)).unwrap_or_else(path::root)
    }
    #[zbus(property)]
    fn specific_object(&self) -> OwnedObjectPath {
        self.r(|m, a| a.ap.filter(|n| m.aps.contains_key(n)).map(path::ap)).unwrap_or_else(path::root)
    }
    #[zbus(property)]
    fn id(&self) -> String {
        self.r(|_, a| a.id.clone())
    }
    #[zbus(property)]
    fn uuid(&self) -> String {
        self.r(|_, a| a.uuid.clone())
    }
    #[zbus(property, name = "Type")]
    fn type_(&self) -> String {
        self.r(|_, a| if a.wifi { model::WIFI_TYPE } else { model::ETH_TYPE }.to_owned())
    }
    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        self.r(|_, a| vec![path::dev(a.ifindex)])
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        self.r(|_, a| a.state)
    }
    #[zbus(property)]
    fn state_flags(&self) -> u32 {
        // LAYER2_READY 0x8 | IP4_READY 0x10 once activated.
        self.r(|_, a| if a.state == nm::AC_ACTIVATED { 0x8 | 0x10 } else { 0 })
    }
    #[zbus(property)]
    fn default(&self) -> bool {
        with(&self.sh, |m| m.primary_ac() == Some(self.n))
    }
    #[zbus(property)]
    fn ip4_config(&self) -> OwnedObjectPath {
        self.r(|m, a| {
            (a.live() && m.ifaces.get(&a.ifindex).is_some_and(|i| i.bound() && i.cur_ac == Some(self.n)))
                .then(|| path::ip4(self.n))
        })
        .unwrap_or_else(path::root)
    }
    #[zbus(property)]
    fn dhcp4_config(&self) -> OwnedObjectPath {
        path::root()
    }
    #[zbus(property)]
    fn default6(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn ip6_config(&self) -> OwnedObjectPath {
        path::root()
    }
    #[zbus(property)]
    fn dhcp6_config(&self) -> OwnedObjectPath {
        path::root()
    }
    #[zbus(property)]
    fn vpn(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn controller(&self) -> OwnedObjectPath {
        path::root()
    }
    #[zbus(property)]
    fn master(&self) -> OwnedObjectPath {
        path::root()
    }
}

pub struct Ip4Config {
    pub sh: Sh,
    /// The active connection this configuration belongs to.
    pub n: u32,
}

impl Ip4Config {
    fn ip(&self) -> Option<netctl_abi::NcIpv4> {
        let m = self.sh.lock();
        let a = m.acs.get(&self.n)?;
        let i = m.ifaces.get(&a.ifindex)?;
        (i.cur_ac == Some(self.n) && matches!(i.ip.state, ip_state::BOUND | ip_state::STATIC)).then_some(i.ip)
    }
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.IP4Config")]
impl Ip4Config {
    #[zbus(property)]
    fn address_data(&self) -> Vec<HashMap<String, OwnedValue>> {
        self.ip()
            .map(|ip| vec![model::dict(vec![("address", ov(ip_str(ip.addr))), ("prefix", ov(ip.prefix as u32))])])
            .unwrap_or_default()
    }
    #[zbus(property)]
    fn addresses(&self) -> Vec<Vec<u32>> {
        self.ip()
            .map(|ip| vec![vec![u32::from_le_bytes(ip.addr), ip.prefix as u32, u32::from_le_bytes(ip.gw)]])
            .unwrap_or_default()
    }
    #[zbus(property)]
    fn gateway(&self) -> String {
        self.ip().filter(|ip| ip.flags & netctl_abi::ip_flags::HAS_GW != 0).map(|ip| ip_str(ip.gw)).unwrap_or_default()
    }
    #[zbus(property)]
    fn route_data(&self) -> Vec<HashMap<String, OwnedValue>> {
        let Some(ip) = self.ip() else { return Vec::new() };
        let mut v = Vec::new();
        let p = ip.prefix.min(32);
        let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
        let net = (u32::from_be_bytes(ip.addr) & mask).to_be_bytes();
        v.push(model::dict(vec![("dest", ov(ip_str(net))), ("prefix", ov(p as u32)), ("metric", ov(ip.metric))]));
        if ip.flags & netctl_abi::ip_flags::HAS_GW != 0 {
            v.push(model::dict(vec![
                ("dest", ov("0.0.0.0")),
                ("prefix", ov(0u32)),
                ("next-hop", ov(ip_str(ip.gw))),
                ("metric", ov(ip.metric)),
            ]));
        }
        v
    }
    #[zbus(property)]
    fn routes(&self) -> Vec<Vec<u32>> {
        Vec::new()
    }
    #[zbus(property)]
    fn nameserver_data(&self) -> Vec<HashMap<String, OwnedValue>> {
        self.ip()
            .map(|ip| ip.dns[..(ip.n_dns as usize).min(3)].iter().map(|d| model::dict(vec![("address", ov(ip_str(*d)))])).collect())
            .unwrap_or_default()
    }
    #[zbus(property)]
    fn nameservers(&self) -> Vec<u32> {
        self.ip().map(|ip| ip.dns[..(ip.n_dns as usize).min(3)].iter().map(|d| u32::from_le_bytes(*d)).collect()).unwrap_or_default()
    }
    #[zbus(property)]
    fn domains(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn searches(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn dns_options(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn dns_priority(&self) -> i32 {
        100
    }
    #[zbus(property)]
    fn wins_server_data(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn wins_servers(&self) -> Vec<u32> {
        Vec::new()
    }
}

// ── Settings ────────────────────────────────────────────────────────────────

pub struct SettingsIface(pub Sh);

#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings")]
impl SettingsIface {
    fn list_connections(&self) -> Vec<OwnedObjectPath> {
        with(&self.0, |m| m.conn_paths())
    }
    fn get_connection_by_uuid(&self, uuid: String) -> Result<OwnedObjectPath, NmError> {
        with(&self.0, |m| m.conn_by_uuid.get(&uuid).map(|n| path::conn(*n)))
            .ok_or_else(|| NmError::InvalidConnection(format!("no connection with uuid {uuid}")))
    }
    async fn add_connection(&self, connection: Settings) -> Result<OwnedObjectPath, NmError> {
        ask(&self.0, |reply| Cmd::AddConnection { settings: connection, save: true, reply }).await
    }
    async fn add_connection_unsaved(&self, connection: Settings) -> Result<OwnedObjectPath, NmError> {
        ask(&self.0, |reply| Cmd::AddConnection { settings: connection, save: false, reply }).await
    }
    async fn add_connection2(
        &self,
        settings: Settings,
        flags: u32,
        _args: HashMap<String, OwnedValue>,
    ) -> Result<(OwnedObjectPath, HashMap<String, OwnedValue>), NmError> {
        // NM_SETTINGS_ADD_CONNECTION2_FLAG_IN_MEMORY = 0x2
        let save = flags & 0x2 == 0;
        let p = ask(&self.0, |reply| Cmd::AddConnection { settings, save, reply }).await?;
        Ok((p, HashMap::new()))
    }
    fn load_connections(&self, _filenames: Vec<String>) -> (bool, Vec<String>) {
        (true, Vec::new())
    }
    fn reload_connections(&self) -> bool {
        true
    }
    fn save_hostname(&self, _hostname: String) -> Result<(), NmError> {
        Err(NmError::PermissionDenied("the hostname is not managed here".into()))
    }

    #[zbus(signal)]
    async fn new_connection(emitter: &SignalEmitter<'_>, connection: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn connection_removed(emitter: &SignalEmitter<'_>, connection: OwnedObjectPath) -> zbus::Result<()>;

    #[zbus(property)]
    fn connections(&self) -> Vec<OwnedObjectPath> {
        with(&self.0, |m| m.conn_paths())
    }
    #[zbus(property)]
    fn hostname(&self) -> String {
        std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_owned()).unwrap_or_default()
    }
    #[zbus(property)]
    fn can_modify(&self) -> bool {
        with(&self.0, |m| m.writable)
    }
    #[zbus(property)]
    fn version_id(&self) -> u64 {
        with(&self.0, |m| m.profiles_gen as u64)
    }
}

pub struct ConnIface {
    pub sh: Sh,
    pub n: u32,
}

#[zbus::interface(name = "org.freedesktop.NetworkManager.Settings.Connection")]
impl ConnIface {
    fn get_settings(&self) -> Result<Settings, NmError> {
        with(&self.sh, |m| m.conns.get(&self.n).map(|c| settings::strip_secrets(&c.settings)))
            .ok_or_else(|| NmError::InvalidConnection("connection removed".into()))
    }
    /// Never fails (cosmic-settings unwraps it).
    async fn update(&self, properties: Settings) -> Result<(), NmError> {
        let n = self.n;
        ask(&self.sh, |reply| Cmd::Update { n, settings: properties, save: true, reply }).await
    }
    async fn update_unsaved(&self, properties: Settings) -> Result<(), NmError> {
        let n = self.n;
        ask(&self.sh, |reply| Cmd::Update { n, settings: properties, save: false, reply }).await
    }
    async fn update2(
        &self,
        settings: Settings,
        flags: u32,
        _args: HashMap<String, OwnedValue>,
    ) -> Result<HashMap<String, OwnedValue>, NmError> {
        let n = self.n;
        // NM_SETTINGS_UPDATE2_FLAG_IN_MEMORY* = 0x2|0x4|0x8
        let save = flags & 0xE == 0;
        ask(&self.sh, |reply| Cmd::Update { n, settings, save, reply }).await?;
        Ok(HashMap::new())
    }
    async fn delete(&self) -> Result<(), NmError> {
        let n = self.n;
        ask(&self.sh, |reply| Cmd::Delete { n, reply }).await
    }
    async fn get_secrets(&self, _setting_name: String) -> Result<Settings, NmError> {
        let n = self.n;
        ask(&self.sh, |reply| Cmd::GetSecrets { n, reply }).await
    }
    async fn clear_secrets(&self) -> Result<(), NmError> {
        let n = self.n;
        ask(&self.sh, |reply| Cmd::ClearSecrets { n, reply }).await
    }
    fn save(&self) {}

    #[zbus(signal)]
    async fn updated(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn removed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn unsaved(&self) -> bool {
        with(&self.sh, |m| m.conns.get(&self.n).is_some_and(|c| c.unsaved))
    }
    #[zbus(property)]
    fn flags(&self) -> u32 {
        // NM_SETTINGS_CONNECTION_FLAG_UNSAVED = 0x1
        self.unsaved() as u32
    }
    #[zbus(property)]
    fn filename(&self) -> String {
        with(&self.sh, |m| match m.conns.get(&self.n) {
            Some(c) if c.legacy() => netctl_abi::LEGACY_WIFI_CONF.to_owned(),
            Some(c) if matches!(c.src, model::ConnSrc::Stored { .. } | model::ConnSrc::Pending) => {
                netctl_abi::PROFILES_DIR.to_owned()
            }
            _ => String::new(),
        })
    }
    #[zbus(property)]
    fn version_id(&self) -> u64 {
        1
    }
}

// ── AgentManager ────────────────────────────────────────────────────────────

/// Secret agents register here (the applet does at start-up). Wi-Fi secrets
/// arrive inline with AddAndActivateConnection, so agents are accepted and
/// never called.
pub struct AgentManager;

#[zbus::interface(name = "org.freedesktop.NetworkManager.AgentManager")]
impl AgentManager {
    fn register(&self, _identifier: String) {}
    fn register_with_capabilities(&self, _identifier: String, _capabilities: u32) {}
    fn unregister(&self) {}
}
