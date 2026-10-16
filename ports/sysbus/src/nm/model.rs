//! The NM object model, derived from the kernel's netctl state.
//!
//! The kernel is the source of truth: every event updates the raw records
//! (NcIface / NcIpv4 / NcWifiState / NcBss / NcProfileMeta), and the NM view
//! (device states, active connections, AP list, global state) is re-derived
//! from them. What the kernel cannot know lives here too: object numbering,
//! AP/AC lifetimes after the kernel forgot them (clients still hold their
//! paths), the 0.5 s FAILED hold, strength hysteresis, unsaved overlays.
//!
//! Explicit signals (StateChanged & co) are queued in `signals` while the
//! model changes; the engine emits them after exporting new objects and after
//! the PropertiesChanged diffs, so a client never hears of a path it cannot
//! read yet.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use netctl_abi::util::{derive_uuid, uuid_to_str, UUID_DOMAIN_WIRED};
use netctl_abi::*;
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::engine::Cmd;
use super::path;
use super::settings::{self, Settings};

/// AP objects stay exported this long after they leave AccessPoints.
pub const AP_LINGER: Duration = Duration::from_secs(30);
/// AC objects stay exported this long after reaching DEACTIVATED.
pub const AC_LINGER: Duration = Duration::from_secs(10);
/// Device FAILED (120) is shown at least this long, as NM does.
pub const FAILED_HOLD: Duration = Duration::from_millis(500);
/// An activation the shim started but the kernel has not reported yet.
pub const PENDING_GRACE: Duration = Duration::from_secs(15);
/// Strength changes smaller than this are not published (snapshot storms).
pub const STRENGTH_STEP: u8 = 5;

// NM enums (libnm NMDeviceState, NMActiveConnectionState, NMState, ...).
pub mod nm {
    pub const DEV_UNAVAILABLE: u32 = 20;
    pub const DEV_DISCONNECTED: u32 = 30;
    pub const DEV_PREPARE: u32 = 40;
    pub const DEV_CONFIG: u32 = 50;
    pub const DEV_IP_CONFIG: u32 = 70;
    pub const DEV_ACTIVATED: u32 = 100;
    pub const DEV_DEACTIVATING: u32 = 110;
    pub const DEV_FAILED: u32 = 120;

    pub const AC_ACTIVATING: u32 = 1;
    pub const AC_ACTIVATED: u32 = 2;
    pub const AC_DEACTIVATING: u32 = 3;
    pub const AC_DEACTIVATED: u32 = 4;

    // NMActiveConnectionStateReason
    pub const ACR_UNKNOWN: u32 = 0;
    pub const ACR_USER_DISCONNECTED: u32 = 2;
    pub const ACR_DEVICE_DISCONNECTED: u32 = 3;
    pub const ACR_NO_SECRETS: u32 = 9;

    // NMDeviceStateReason
    pub const DR_NONE: u32 = 0;
    pub const DR_UNKNOWN: u32 = 1;
    pub const DR_NO_SECRETS: u32 = 7;
    pub const DR_SUPPLICANT_DISCONNECT: u32 = 8;
    pub const DR_SUPPLICANT_TIMEOUT: u32 = 11;
    pub const DR_USER_REQUESTED: u32 = 39;
    pub const DR_CARRIER: u32 = 40;
    pub const DR_RFKILL: u32 = 50;
    pub const DR_SSID_NOT_FOUND: u32 = 53;

    pub const STATE_DISCONNECTED: u32 = 20;
    pub const STATE_CONNECTING: u32 = 40;
    pub const STATE_CONNECTED_SITE: u32 = 60;
    pub const STATE_CONNECTED_GLOBAL: u32 = 70;

    pub const CONNECTIVITY_NONE: u32 = 1;
    pub const CONNECTIVITY_FULL: u32 = 4;

    pub const TYPE_ETHERNET: u32 = 1;
    pub const TYPE_WIFI: u32 = 2;
}

pub const WIFI_TYPE: &str = "802-11-wireless";
pub const ETH_TYPE: &str = "802-3-ethernet";

/// Device reason for a kernel Wi-Fi reason.
pub fn dev_reason(r: u8) -> u32 {
    match r {
        reason::NONE | reason::SUPERSEDED => nm::DR_NONE,
        reason::WRONG_KEY | reason::NO_SECRET => nm::DR_NO_SECRETS,
        reason::NO_AP => nm::DR_SSID_NOT_FOUND,
        reason::AUTH_REJECTED | reason::ASSOC_REJECTED | reason::DEAUTH_BY_AP | reason::BEACON_LOSS => {
            nm::DR_SUPPLICANT_DISCONNECT
        }
        reason::TIMEOUT => nm::DR_SUPPLICANT_TIMEOUT,
        // docs/servers/netctl.md maps it to 1 (unknown): NM has no reason
        // for "this security cannot be done here".
        reason::UNSUPPORTED_SECURITY => nm::DR_UNKNOWN,
        reason::USER => nm::DR_USER_REQUESTED,
        reason::RADIO_OFF | reason::RFKILL => nm::DR_RFKILL,
        _ => nm::DR_UNKNOWN,
    }
}

/// Active-connection reason for a kernel Wi-Fi reason. Anything that is not
/// "wrong secret" or "the user asked" is DEVICE_DISCONNECTED, and clients
/// (nmrs) then read Device.StateReason for the detail.
pub fn ac_reason(r: u8) -> u32 {
    match r {
        reason::WRONG_KEY | reason::NO_SECRET => nm::ACR_NO_SECRETS,
        reason::USER => nm::ACR_USER_DISCONNECTED,
        _ => nm::ACR_DEVICE_DISCONNECTED,
    }
}

pub fn mac_str(m: &[u8; 6]) -> String {
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", m[0], m[1], m[2], m[3], m[4], m[5])
}

pub fn ip_str(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

pub fn uuid_string(u: &[u8; 16]) -> String {
    String::from_utf8_lossy(&uuid_to_str(u)).into_owned()
}

/// Explicit (non-PropertiesChanged) signals, emitted by the engine.
#[derive(Debug, Clone)]
pub enum Sig {
    NmStateChanged(u32),
    DeviceAdded(u32),
    DeviceRemoved(u32),
    DevStateChanged { ifindex: u32, new: u32, old: u32, reason: u32 },
    ApAdded { ifindex: u32, ap: u32 },
    ApRemoved { ifindex: u32, ap: u32 },
    AcStateChanged { ac: u32, state: u32, reason: u32 },
    NewConnection(u32),
    ConnectionRemoved(u32),
    ConnUpdated(u32),
    ConnRemoved(u32),
}

pub struct Iface {
    pub nc: NcIface,
    pub ip: NcIpv4,
    pub wifi: Option<NcWifiState>,
    /// Seen in the current snapshot (RESYNC bookkeeping).
    pub seen: bool,
    pub dev_state: u32,
    pub dev_reason: u32,
    /// FAILED shown until this instant, with this device reason.
    pub fail_hold: Option<(Instant, u32)>,
    pub cur_ac: Option<u32>,
    /// CLOCK_BOOTTIME ms of the last SCAN_DONE; 0 before the first.
    pub last_scan: i64,
}

impl Iface {
    pub fn is_wifi(&self) -> bool {
        self.nc.kind == if_kind::WIFI
    }
    pub fn name(&self) -> String {
        String::from_utf8_lossy(self.nc.name()).into_owned()
    }
    pub fn driver(&self) -> String {
        String::from_utf8_lossy(self.nc.driver()).into_owned()
    }
    pub fn bound(&self) -> bool {
        matches!(self.ip.state, ip_state::BOUND | ip_state::STATIC)
    }
    pub fn has_gw(&self) -> bool {
        self.bound() && self.ip.flags & ip_flags::HAS_GW != 0
    }
    fn wifi_state(&self) -> u8 {
        self.wifi.map(|w| w.state).unwrap_or(wifi_state::UNAVAILABLE)
    }
}

pub struct Ap {
    pub ifindex: u32,
    pub bss: NcBss,
    /// In the device's AccessPoints list.
    pub listed: bool,
    /// The kernel aged it out; delisted as soon as it is not pinned.
    pub gone: bool,
    /// Published strength (hysteresis of STRENGTH_STEP).
    pub strength: u8,
    /// Delisted: unexport at this instant.
    pub unexport_at: Option<Instant>,
    pub seen: bool,
}

pub struct Ac {
    pub ifindex: u32,
    pub wifi: bool,
    pub attempt: u32,
    pub uuid: String,
    pub id: String,
    pub conn: Option<u32>,
    pub ap: Option<u32>,
    pub state: u32,
    pub reason: u32,
    /// Started by the shim and not yet reported active by the kernel.
    pub pending_until: Option<Instant>,
    /// DEACTIVATED: unexport at this instant.
    pub dead_at: Option<Instant>,
}

impl Ac {
    pub fn live(&self) -> bool {
        self.dead_at.is_none()
    }
}

pub enum ConnSrc {
    /// From the kernel's profile list (PROFILE events), this generation.
    Stored { meta: NcProfileMeta },
    /// Written by this shim, not yet confirmed by a PROFILE event.
    Pending,
    /// In memory only (AddConnectionUnsaved, a read-only user's Update, a
    /// failed write). Never activatable without a secret.
    Memory,
    /// The synthetic "Wired connection N" of an ethernet interface.
    Wired { ifindex: u32 },
}

pub struct Conn {
    pub uuid: String,
    pub src: ConnSrc,
    /// Secret-free settings as GetSettings returns them.
    pub settings: Settings,
    pub unsaved: bool,
    /// The SSID (Wi-Fi) for dedupe; empty for wired.
    pub ssid: Vec<u8>,
}

impl Conn {
    pub fn is_wifi(&self) -> bool {
        !matches!(self.src, ConnSrc::Wired { .. })
    }
    pub fn legacy(&self) -> bool {
        matches!(&self.src, ConnSrc::Stored { meta } if meta.flags & profile_flags::LEGACY != 0)
    }
    pub fn id(&self) -> String {
        settings::get_str(&self.settings, "connection", "id").unwrap_or_default()
    }
}

pub struct Model {
    pub writable: bool,
    pub features: u32,
    pub ifaces: BTreeMap<u32, Iface>,
    pub aps: BTreeMap<u32, Ap>,
    pub ap_by_key: HashMap<(u32, [u8; 6]), u32>,
    pub acs: BTreeMap<u32, Ac>,
    pub conns: BTreeMap<u32, Conn>,
    pub conn_by_uuid: HashMap<String, u32>,
    pub profiles_gen: u32,
    pub in_snapshot: bool,
    pub snapshot_done: bool,
    pub wireless_enabled_wanted: bool,
    pub wwan_enabled: bool,
    pub nm_state: u32,
    pub signals: Vec<Sig>,
    next_n: u32,
}

/// Shared between the engine and the D-Bus interface views.
pub struct Shared {
    pub m: Mutex<Model>,
    pub cmd: UnboundedSender<Cmd>,
}

impl Shared {
    pub fn new(writable: bool, features: u32, cmd: UnboundedSender<Cmd>) -> Self {
        Shared { m: Mutex::new(Model::new(writable, features)), cmd }
    }
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Model> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Model {
    fn new(writable: bool, features: u32) -> Self {
        Model {
            writable,
            features,
            ifaces: BTreeMap::new(),
            aps: BTreeMap::new(),
            ap_by_key: HashMap::new(),
            acs: BTreeMap::new(),
            conns: BTreeMap::new(),
            conn_by_uuid: HashMap::new(),
            profiles_gen: 0,
            in_snapshot: false,
            snapshot_done: false,
            wireless_enabled_wanted: true,
            wwan_enabled: true,
            nm_state: nm::STATE_DISCONNECTED,
            signals: Vec::new(),
            next_n: 1,
        }
    }

    pub fn next(&mut self) -> u32 {
        let n = self.next_n;
        self.next_n += 1;
        n
    }

    // ── kernel events ─────────────────────────────────────────────────────

    pub fn apply(&mut self, ev: Event, now: Instant) {
        match ev {
            Event::SnapshotBegin(_) | Event::Resync(_) => {
                self.in_snapshot = true;
                for i in self.ifaces.values_mut() {
                    i.seen = false;
                }
                for a in self.aps.values_mut() {
                    a.seen = false;
                }
            }
            Event::SnapshotEnd(_) => {
                self.in_snapshot = false;
                self.snapshot_done = true;
                let gone: Vec<u32> = self.ifaces.iter().filter(|(_, i)| !i.seen).map(|(k, _)| *k).collect();
                for ifindex in gone {
                    self.remove_iface(ifindex, now);
                }
                let gone: Vec<u32> = self.aps.iter().filter(|(_, a)| !a.seen && a.listed).map(|(k, _)| *k).collect();
                for n in gone {
                    if let Some(a) = self.aps.get_mut(&n) {
                        a.gone = true;
                    }
                }
            }
            Event::IfaceAdded(nc) | Event::IfaceChanged(nc) => self.upsert_iface(nc),
            Event::IfaceRemoved(nc) => self.remove_iface(nc.ifindex, now),
            Event::Ipv4(ip) => {
                if let Some(i) = self.ifaces.get_mut(&ip.ifindex) {
                    i.ip = ip;
                }
            }
            Event::WifiState(ws) => {
                if let Some(i) = self.ifaces.get_mut(&ws.ifindex) {
                    // A superseded attempt is not a failure the user should
                    // see: a newer CONNECT replaced it.
                    if ws.state == wifi_state::FAILED && ws.reason != reason::SUPERSEDED {
                        i.fail_hold = Some((now + FAILED_HOLD, dev_reason(ws.reason)));
                    } else if matches!(ws.state, wifi_state::CONNECTING..=wifi_state::CONNECTED) {
                        i.fail_hold = None;
                    }
                    i.wifi = Some(ws);
                    // FAILED ends the attempt it names even while the AC is
                    // still "pending" (reconcile keeps pending ACs alive
                    // through the idle state that precedes CONNECTING).
                    if ws.state == wifi_state::FAILED {
                        self.fail_attempt(ws.ifindex, ws.attempt, ws.reason, now);
                    }
                }
            }
            Event::WifiSignal(sig) => {
                if let Some(i) = self.ifaces.get_mut(&sig.ifindex) {
                    if let Some(w) = i.wifi.as_mut() {
                        w.signal_dbm = sig.signal_dbm;
                    }
                }
                if let Some(&n) = self.ap_by_key.get(&(sig.ifindex, sig.bssid)) {
                    if let Some(a) = self.aps.get_mut(&n) {
                        a.bss.signal_dbm = sig.signal_dbm;
                        a.bss.quality = sig.quality;
                        a.publish_strength();
                    }
                }
            }
            Event::ScanStarted(_) => {}
            Event::ScanDone(s) => {
                if let Some(i) = self.ifaces.get_mut(&s.ifindex) {
                    i.last_scan = (s.time_ms as i64).max(1);
                }
            }
            Event::Bss(b) => self.upsert_bss(b, now),
            Event::BssGone(b) => {
                if let Some(&n) = self.ap_by_key.get(&(b.ifindex, b.bssid)) {
                    if let Some(a) = self.aps.get_mut(&n) {
                        a.gone = true;
                    }
                }
            }
            Event::Profile(meta) => self.upsert_profile(meta),
            Event::ProfilesEnd(info) => self.end_profiles(info.gen),
            Event::Unknown { .. } => {}
        }
    }

    fn upsert_iface(&mut self, nc: NcIface) {
        if nc.kind != if_kind::ETHERNET && nc.kind != if_kind::WIFI {
            return;
        }
        let ifindex = nc.ifindex;
        let new = !self.ifaces.contains_key(&ifindex);
        let i = self.ifaces.entry(ifindex).or_insert_with(|| Iface {
            nc,
            ip: NcIpv4 { ifindex, ..Default::default() },
            wifi: None,
            seen: true,
            dev_state: nm::DEV_UNAVAILABLE,
            dev_reason: nm::DR_NONE,
            fail_hold: None,
            cur_ac: None,
            last_scan: 0,
        });
        i.nc = nc;
        i.seen = true;
        if new {
            if nc.kind == if_kind::ETHERNET {
                self.add_wired_conn(ifindex);
            }
            self.signals.push(Sig::DeviceAdded(ifindex));
        }
    }

    fn remove_iface(&mut self, ifindex: u32, now: Instant) {
        if let Some(cur) = self.ifaces.get(&ifindex).and_then(|i| i.cur_ac) {
            self.finish_ac(cur, nm::ACR_DEVICE_DISCONNECTED, now);
        }
        let aps: Vec<u32> = self.aps.iter().filter(|(_, a)| a.ifindex == ifindex).map(|(k, _)| *k).collect();
        for n in aps {
            self.delist_ap(n, now);
        }
        if self.ifaces.remove(&ifindex).is_some() {
            self.signals.push(Sig::DeviceRemoved(ifindex));
        }
        let wired: Vec<u32> = self
            .conns
            .iter()
            .filter(|(_, c)| matches!(c.src, ConnSrc::Wired { ifindex: i } if i == ifindex))
            .map(|(k, _)| *k)
            .collect();
        for n in wired {
            self.remove_conn(n);
        }
    }

    /// The SSIDs the applet can act on: BSSs the kernel can join
    /// (`bss_ext::JOINABLE`), so the applet never offers a password box that
    /// cannot work. That hides SAE-only networks until WPA3 lands, and also
    /// the WPA2 variants the station cannot do yet — a TKIP group cipher
    /// (WPA/WPA2 mixed mode), PSK-SHA256 or FT-PSK as the only AKM, MFP
    /// required — which used to be listed (the PSK AKM family made them look
    /// fine) and then failed UNSUPPORTED_SECURITY. The kernel logs why each
    /// is not joinable. Hidden SSIDs are left out (no way to show them).
    fn bss_exported(&self, b: &NcBss) -> bool {
        if b.ssid_len == 0 || b.ssid().iter().all(|&c| c == 0) {
            return false;
        }
        b.ext & bss_ext::JOINABLE != 0
    }

    fn upsert_bss(&mut self, b: NcBss, now: Instant) {
        if !self.ifaces.contains_key(&b.ifindex) {
            return;
        }
        let exported = self.bss_exported(&b);
        let key = (b.ifindex, b.bssid);
        match self.ap_by_key.get(&key).copied() {
            Some(n) => {
                let Some(a) = self.aps.get_mut(&n) else { return };
                a.bss = b;
                a.seen = true;
                if !exported {
                    a.gone = true;
                    return;
                }
                a.gone = false;
                a.publish_strength();
                if !a.listed {
                    a.listed = true;
                    a.unexport_at = None;
                    self.signals.push(Sig::ApAdded { ifindex: b.ifindex, ap: n });
                }
            }
            None => {
                if !exported {
                    return;
                }
                let n = self.next();
                self.aps.insert(
                    n,
                    Ap { ifindex: b.ifindex, bss: b, listed: true, gone: false, strength: b.quality, unexport_at: None, seen: true },
                );
                self.ap_by_key.insert(key, n);
                self.signals.push(Sig::ApAdded { ifindex: b.ifindex, ap: n });
            }
        }
        let _ = now;
    }

    fn delist_ap(&mut self, n: u32, now: Instant) {
        if let Some(a) = self.aps.get_mut(&n) {
            if a.listed {
                a.listed = false;
                a.unexport_at = Some(now + AP_LINGER);
                self.signals.push(Sig::ApRemoved { ifindex: a.ifindex, ap: n });
            }
        }
    }

    pub fn ap_pinned(&self, n: u32) -> bool {
        self.acs.values().any(|ac| ac.live() && ac.ap == Some(n))
    }

    // ── profiles ──────────────────────────────────────────────────────────

    fn add_wired_conn(&mut self, ifindex: u32) {
        let Some(i) = self.ifaces.get(&ifindex) else { return };
        let uuid = uuid_string(&derive_uuid(UUID_DOMAIN_WIRED, &i.nc.perm_mac));
        if self.conn_by_uuid.contains_key(&uuid) {
            return;
        }
        let k = 1 + self.conns.values().filter(|c| matches!(c.src, ConnSrc::Wired { .. })).count();
        let s = settings::wired_settings(&format!("Wired connection {k}"), &uuid, &i.name());
        let n = self.next();
        self.conns.insert(n, Conn { uuid: uuid.clone(), src: ConnSrc::Wired { ifindex }, settings: s, unsaved: false, ssid: Vec::new() });
        self.conn_by_uuid.insert(uuid, n);
        self.signals.push(Sig::NewConnection(n));
    }

    fn upsert_profile(&mut self, meta: NcProfileMeta) {
        if meta.kind != if_kind::WIFI {
            return;
        }
        let uuid = uuid_string(&meta.uuid);
        let s = settings::settings_from_meta(&meta, &uuid);
        match self.conn_by_uuid.get(&uuid).copied() {
            Some(n) => {
                let c = self.conns.get_mut(&n).expect("conn index");
                let changed = c.settings != s || !matches!(c.src, ConnSrc::Stored { .. });
                // A read-only user's overlay survives kernel refreshes.
                if !(c.unsaved && matches!(c.src, ConnSrc::Stored { .. })) {
                    c.settings = s;
                    c.unsaved = false;
                }
                c.src = ConnSrc::Stored { meta };
                c.ssid = meta.ssid().to_vec();
                if changed {
                    self.signals.push(Sig::ConnUpdated(n));
                }
            }
            None => {
                let n = self.next();
                self.conns.insert(n, Conn { uuid: uuid.clone(), src: ConnSrc::Stored { meta }, settings: s, unsaved: false, ssid: meta.ssid().to_vec() });
                self.conn_by_uuid.insert(uuid, n);
                self.signals.push(Sig::NewConnection(n));
            }
        }
    }

    fn end_profiles(&mut self, gen: u32) {
        self.profiles_gen = gen;
        let stale: Vec<u32> = self
            .conns
            .iter()
            .filter(|(_, c)| matches!(&c.src, ConnSrc::Stored { meta } if meta.gen != gen))
            .map(|(k, _)| *k)
            .collect();
        for n in stale {
            self.remove_conn(n);
        }
    }

    pub fn remove_conn(&mut self, n: u32) {
        if let Some(c) = self.conns.remove(&n) {
            self.conn_by_uuid.remove(&c.uuid);
            self.signals.push(Sig::ConnRemoved(n));
            self.signals.push(Sig::ConnectionRemoved(n));
        }
    }

    /// Insert or replace a connection created by this shim (pending or
    /// memory-only), keeping its number if the uuid is known.
    pub fn put_conn(&mut self, uuid: &str, src: ConnSrc, s: Settings, unsaved: bool, ssid: Vec<u8>) -> u32 {
        match self.conn_by_uuid.get(uuid).copied() {
            Some(n) => {
                let c = self.conns.get_mut(&n).expect("conn index");
                c.settings = s;
                c.unsaved = unsaved;
                c.ssid = ssid;
                if !matches!(c.src, ConnSrc::Stored { .. } | ConnSrc::Wired { .. }) || matches!(src, ConnSrc::Memory) {
                    c.src = src;
                }
                self.signals.push(Sig::ConnUpdated(n));
                n
            }
            None => {
                let n = self.next();
                self.conns.insert(n, Conn { uuid: uuid.to_owned(), src, settings: s, unsaved, ssid });
                self.conn_by_uuid.insert(uuid.to_owned(), n);
                self.signals.push(Sig::NewConnection(n));
                n
            }
        }
    }

    pub fn wifi_conn_by_ssid(&self, ssid: &[u8]) -> Option<u32> {
        self.conns.iter().find(|(_, c)| c.is_wifi() && !c.legacy() && c.ssid == ssid).map(|(k, _)| *k)
    }

    // ── active connections ────────────────────────────────────────────────

    /// A new AC on `ifindex`, replacing the current one.
    pub fn new_ac(&mut self, ifindex: u32, attempt: u32, conn: Option<u32>, ap: Option<u32>, pending: bool, now: Instant) -> u32 {
        if let Some(old) = self.ifaces.get(&ifindex).and_then(|i| i.cur_ac) {
            self.finish_ac(old, nm::ACR_DEVICE_DISCONNECTED, now);
        }
        let wifi = self.ifaces.get(&ifindex).is_some_and(|i| i.is_wifi());
        let (uuid, id) = match conn.and_then(|n| self.conns.get(&n)) {
            Some(c) => (c.uuid.clone(), c.id()),
            None => {
                // No profile known (an unlabelled kernel connect): the SSID
                // is the id, as clients match AC.Id against SSIDs.
                let ssid = self
                    .ifaces
                    .get(&ifindex)
                    .and_then(|i| i.wifi)
                    .map(|w| String::from_utf8_lossy(w.ssid()).into_owned())
                    .unwrap_or_default();
                (uuid_string(&derive_uuid(b"leandros-nmshim-adhoc", ssid.as_bytes())), ssid)
            }
        };
        let n = self.next();
        self.acs.insert(
            n,
            Ac {
                ifindex,
                wifi,
                attempt,
                uuid,
                id,
                conn,
                ap,
                state: nm::AC_ACTIVATING,
                reason: nm::ACR_UNKNOWN,
                pending_until: pending.then_some(now + PENDING_GRACE),
                dead_at: None,
            },
        );
        if let Some(i) = self.ifaces.get_mut(&ifindex) {
            i.cur_ac = Some(n);
            if pending {
                i.fail_hold = None;
            }
        }
        n
    }

    pub fn set_ac_state(&mut self, n: u32, state: u32, reason: u32) {
        if let Some(ac) = self.acs.get_mut(&n) {
            if ac.state != state {
                ac.state = state;
                ac.reason = reason;
                self.signals.push(Sig::AcStateChanged { ac: n, state, reason });
            }
        }
    }

    pub fn finish_ac(&mut self, n: u32, reason: u32, now: Instant) {
        let Some(ac) = self.acs.get(&n) else { return };
        if !ac.live() {
            return;
        }
        let ifindex = ac.ifindex;
        self.set_ac_state(n, nm::AC_DEACTIVATED, reason);
        if let Some(ac) = self.acs.get_mut(&n) {
            ac.dead_at = Some(now + AC_LINGER);
            ac.pending_until = None;
        }
        if let Some(i) = self.ifaces.get_mut(&ifindex) {
            if i.cur_ac == Some(n) {
                i.cur_ac = None;
            }
        }
    }

    /// The kernel ended attempt `attempt` with `why`.
    fn fail_attempt(&mut self, ifindex: u32, attempt: u32, why: u8, now: Instant) {
        let Some(cur) = self.ifaces.get(&ifindex).and_then(|i| i.cur_ac) else { return };
        let matches = self.acs.get(&cur).is_some_and(|ac| attempt == 0 || ac.attempt == attempt);
        if matches {
            self.finish_ac(cur, ac_reason(why), now);
        }
    }

    fn ap_for(&mut self, ifindex: u32, bssid: [u8; 6], ws: Option<&NcWifiState>) -> Option<u32> {
        if bssid == [0; 6] {
            return None;
        }
        if let Some(&n) = self.ap_by_key.get(&(ifindex, bssid)) {
            return Some(n);
        }
        // Associated to a BSS the scan list does not have (hidden, filtered):
        // make an AP object for it so ActiveAccessPoint is a live path.
        let ws = ws?;
        let mut b = NcBss { ifindex, bssid, freq_mhz: ws.freq_mhz, signal_dbm: ws.signal_dbm, ..Default::default() };
        b.quality = util::quality_from_dbm(ws.signal_dbm as i32);
        b.ssid_len = ws.ssid_len;
        b.ssid = ws.ssid;
        let n = self.next();
        self.aps.insert(n, Ap { ifindex, bss: b, listed: true, gone: true, strength: b.quality, unexport_at: None, seen: true });
        self.ap_by_key.insert((ifindex, bssid), n);
        self.signals.push(Sig::ApAdded { ifindex, ap: n });
        Some(n)
    }

    /// Bring every interface's AC and device state in line with the kernel.
    pub fn reconcile(&mut self, now: Instant) {
        let ifs: Vec<u32> = self.ifaces.keys().copied().collect();
        for ifindex in ifs {
            if self.ifaces[&ifindex].is_wifi() {
                self.reconcile_wifi(ifindex, now);
            } else {
                self.reconcile_wired(ifindex, now);
            }
            self.derive_dev_state(ifindex, now);
        }
        // AP lifetimes.
        let ns: Vec<u32> = self.aps.keys().copied().collect();
        for n in ns {
            let (gone, listed, due, radio_off) = {
                let a = &self.aps[&n];
                let radio_off = self.ifaces.get(&a.ifindex).is_none_or(|i| i.nc.flags & iface_flags::RADIO_ON == 0);
                (a.gone, a.listed, a.unexport_at.is_some_and(|t| now >= t), radio_off)
            };
            if listed && (gone || radio_off) && !self.ap_pinned(n) {
                self.delist_ap(n, now);
            }
            if !listed && due && !self.ap_pinned(n) {
                let a = self.aps.remove(&n).expect("ap");
                if self.ap_by_key.get(&(a.ifindex, a.bss.bssid)) == Some(&n) {
                    self.ap_by_key.remove(&(a.ifindex, a.bss.bssid));
                }
            }
        }
        // AC lifetimes.
        let dead: Vec<u32> = self.acs.iter().filter(|(_, a)| a.dead_at.is_some_and(|t| now >= t)).map(|(k, _)| *k).collect();
        for n in dead {
            self.acs.remove(&n);
        }
        let st = self.global_state();
        if st != self.nm_state {
            self.nm_state = st;
            self.signals.push(Sig::NmStateChanged(st));
        }
    }

    fn reconcile_wifi(&mut self, ifindex: u32, now: Instant) {
        let i = &self.ifaces[&ifindex];
        let ws = i.wifi;
        let bound = i.bound();
        let cur = i.cur_ac;
        let active = ws.filter(|w| matches!(w.state, wifi_state::CONNECTING..=wifi_state::CONNECTED | wifi_state::DISCONNECTING));
        match (cur, active) {
            (Some(n), Some(w)) => {
                let (ac_att, pending) = self
                    .acs
                    .get(&n)
                    .map(|ac| (ac.attempt, ac.pending_until.is_some_and(|t| now < t)))
                    .unwrap_or((0, false));
                if pending && ac_att != 0 && w.attempt < ac_att {
                    // Still the state of an older attempt; ours (newer ids
                    // are larger) has not been reported yet.
                    self.set_ac_state(n, nm::AC_ACTIVATING, nm::ACR_UNKNOWN);
                    return;
                }
                let same = ac_att == w.attempt || ac_att == 0;
                if !same {
                    // The kernel moved on to another attempt (autoconnect, or
                    // another session's shim): converge on it.
                    let conn = self.conn_by_uuid.get(&uuid_string(&w.profile_uuid)).copied();
                    let ap = self.ap_for(ifindex, w.bssid, Some(&w));
                    self.new_ac(ifindex, w.attempt, conn, ap, false, now);
                } else if let Some(ac) = self.acs.get_mut(&n) {
                    ac.attempt = w.attempt;
                    ac.pending_until = None;
                }
                let n = self.ifaces[&ifindex].cur_ac.expect("cur ac");
                if self.acs.get(&n).is_some_and(|ac| ac.ap.is_none()) || w.bssid != [0; 6] {
                    let ap = self.ap_for(ifindex, w.bssid, Some(&w));
                    if let (Some(ap), Some(ac)) = (ap, self.acs.get_mut(&n)) {
                        ac.ap = Some(ap);
                    }
                }
                let st = match w.state {
                    wifi_state::DISCONNECTING => nm::AC_DEACTIVATING,
                    wifi_state::CONNECTED if bound => nm::AC_ACTIVATED,
                    _ => nm::AC_ACTIVATING,
                };
                self.set_ac_state(n, st, nm::ACR_UNKNOWN);
            }
            (None, Some(w)) => {
                let conn = self.conn_by_uuid.get(&uuid_string(&w.profile_uuid)).copied();
                let ap = self.ap_for(ifindex, w.bssid, Some(&w));
                let n = self.new_ac(ifindex, w.attempt, conn, ap, false, now);
                let st = if w.state == wifi_state::CONNECTED && bound { nm::AC_ACTIVATED } else { nm::AC_ACTIVATING };
                self.set_ac_state(n, st, nm::ACR_UNKNOWN);
            }
            (Some(n), None) => {
                let pending = self.acs.get(&n).and_then(|ac| ac.pending_until).is_some_and(|t| now < t);
                if !pending {
                    let r = ws.map(|w| ac_reason(w.reason)).unwrap_or(nm::ACR_DEVICE_DISCONNECTED);
                    self.finish_ac(n, r, now);
                }
            }
            (None, None) => {}
        }
    }

    fn reconcile_wired(&mut self, ifindex: u32, now: Instant) {
        let i = &self.ifaces[&ifindex];
        let up = i.nc.flags & iface_flags::ADMIN_UP != 0;
        let active = up && i.nc.flags & iface_flags::CARRIER != 0 && i.ip.state != ip_state::NONE;
        let bound = i.bound();
        match (i.cur_ac, active) {
            (None, true) => {
                let conn = self.conns.iter().find(|(_, c)| matches!(c.src, ConnSrc::Wired { ifindex: x } if x == ifindex)).map(|(k, _)| *k);
                let n = self.new_ac(ifindex, 0, conn, None, false, now);
                self.set_ac_state(n, if bound { nm::AC_ACTIVATED } else { nm::AC_ACTIVATING }, nm::ACR_UNKNOWN);
            }
            (Some(n), true) => {
                if let Some(ac) = self.acs.get_mut(&n) {
                    ac.pending_until = None;
                }
                self.set_ac_state(n, if bound { nm::AC_ACTIVATED } else { nm::AC_ACTIVATING }, nm::ACR_UNKNOWN);
            }
            (Some(n), false) => {
                let pending = self.acs.get(&n).and_then(|ac| ac.pending_until).is_some_and(|t| now < t);
                if !pending {
                    let r = if up { nm::ACR_DEVICE_DISCONNECTED } else { nm::ACR_USER_DISCONNECTED };
                    self.finish_ac(n, r, now);
                }
            }
            (None, false) => {}
        }
    }

    fn derive_dev_state(&mut self, ifindex: u32, now: Instant) {
        let i = self.ifaces.get_mut(&ifindex).expect("iface");
        let pending_ac = i.cur_ac.is_some();
        let (st, why) = if i.nc.kind == if_kind::WIFI {
            let ws = i.wifi_state();
            let r = i.wifi.map(|w| dev_reason(w.reason)).unwrap_or(nm::DR_NONE);
            let hold = i.fail_hold.filter(|(t, _)| now < *t);
            if hold.is_none() {
                i.fail_hold = None;
            }
            if i.nc.flags & iface_flags::READY == 0 && ws == wifi_state::UNAVAILABLE {
                (nm::DEV_UNAVAILABLE, nm::DR_NONE)
            } else {
                match ws {
                    wifi_state::UNAVAILABLE | wifi_state::RADIO_OFF => (nm::DEV_UNAVAILABLE, r),
                    wifi_state::FAILED if i.wifi.is_some_and(|w| w.reason == reason::SUPERSEDED) => {
                        (nm::DEV_PREPARE, nm::DR_NONE)
                    }
                    wifi_state::FAILED => (nm::DEV_FAILED, r),
                    wifi_state::DISCONNECTED => match hold {
                        Some((_, hr)) => (nm::DEV_FAILED, hr),
                        None if pending_ac => (nm::DEV_PREPARE, nm::DR_NONE),
                        None => (nm::DEV_DISCONNECTED, r),
                    },
                    wifi_state::CONNECTING => (nm::DEV_PREPARE, nm::DR_NONE),
                    wifi_state::AUTHENTICATING | wifi_state::ASSOCIATING | wifi_state::HANDSHAKE => {
                        (nm::DEV_CONFIG, nm::DR_NONE)
                    }
                    wifi_state::CONNECTED if i.bound() => (nm::DEV_ACTIVATED, nm::DR_NONE),
                    wifi_state::CONNECTED => (nm::DEV_IP_CONFIG, nm::DR_NONE),
                    wifi_state::DISCONNECTING => (nm::DEV_DEACTIVATING, r),
                    _ => (nm::DEV_UNAVAILABLE, nm::DR_UNKNOWN),
                }
            }
        } else if i.nc.flags & iface_flags::ADMIN_UP == 0 {
            if pending_ac { (nm::DEV_PREPARE, nm::DR_NONE) } else { (nm::DEV_DISCONNECTED, nm::DR_USER_REQUESTED) }
        } else if i.nc.flags & iface_flags::CARRIER == 0 {
            (nm::DEV_UNAVAILABLE, nm::DR_CARRIER)
        } else if i.bound() {
            (nm::DEV_ACTIVATED, nm::DR_NONE)
        } else {
            (nm::DEV_IP_CONFIG, nm::DR_NONE)
        };
        if st != i.dev_state || why != i.dev_reason {
            let old = i.dev_state;
            i.dev_state = st;
            i.dev_reason = why;
            if st != old {
                self.signals.push(Sig::DevStateChanged { ifindex, new: st, old, reason: why });
            }
        }
    }

    /// Next instant at which reconcile() would change something by itself.
    pub fn next_deadline(&self) -> Option<Instant> {
        let a = self.ifaces.values().filter_map(|i| i.fail_hold.map(|(t, _)| t));
        let b = self.aps.values().filter_map(|a| a.unexport_at);
        let c = self.acs.values().filter_map(|a| a.dead_at.or(a.pending_until));
        a.chain(b).chain(c).min()
    }

    // ── derived global properties ─────────────────────────────────────────

    /// The interface that holds the default route.
    pub fn primary_iface(&self) -> Option<u32> {
        let cands = self.ifaces.iter().filter(|(_, i)| i.has_gw());
        cands
            .clone()
            .find(|(_, i)| i.ip.flags & ip_flags::DEFAULT_ROUTE != 0)
            .or_else(|| cands.min_by_key(|(_, i)| i.ip.metric))
            .map(|(k, _)| *k)
    }

    pub fn primary_ac(&self) -> Option<u32> {
        let p = self.primary_iface()?;
        self.ifaces.get(&p)?.cur_ac.filter(|n| self.acs.get(n).is_some_and(|a| a.state == nm::AC_ACTIVATED))
    }

    /// Never 2 (portal): the applet would spawn a browser on every refresh.
    pub fn connectivity(&self) -> u32 {
        if self.ifaces.values().any(|i| i.has_gw() && i.dev_state == nm::DEV_ACTIVATED) {
            nm::CONNECTIVITY_FULL
        } else {
            nm::CONNECTIVITY_NONE
        }
    }

    fn global_state(&self) -> u32 {
        if self.connectivity() == nm::CONNECTIVITY_FULL {
            nm::STATE_CONNECTED_GLOBAL
        } else if self.ifaces.values().any(|i| i.dev_state == nm::DEV_ACTIVATED) {
            nm::STATE_CONNECTED_SITE
        } else if self.ifaces.values().any(|i| (nm::DEV_PREPARE..nm::DEV_ACTIVATED).contains(&i.dev_state)) {
            nm::STATE_CONNECTING
        } else {
            nm::STATE_DISCONNECTED
        }
    }

    pub fn wireless_enabled(&self) -> bool {
        let mut wifi = self.ifaces.values().filter(|i| i.is_wifi()).peekable();
        if wifi.peek().is_none() {
            return self.wireless_enabled_wanted;
        }
        wifi.any(|i| i.nc.flags & iface_flags::RADIO_ON != 0)
    }

    pub fn wireless_hw_enabled(&self) -> bool {
        self.ifaces.values().filter(|i| i.is_wifi()).all(|i| i.nc.flags & iface_flags::RADIO_HW_ON != 0)
    }

    pub fn device_paths(&self) -> Vec<OwnedObjectPath> {
        self.ifaces.keys().map(|&k| path::dev(k)).collect()
    }

    pub fn live_acs(&self) -> Vec<OwnedObjectPath> {
        self.acs.iter().filter(|(_, a)| a.live()).map(|(&k, _)| path::ac(k)).collect()
    }

    pub fn listed_aps(&self, ifindex: u32) -> Vec<OwnedObjectPath> {
        self.aps.iter().filter(|(_, a)| a.ifindex == ifindex && a.listed).map(|(&k, _)| path::ap(k)).collect()
    }

    pub fn active_ap(&self, ifindex: u32) -> OwnedObjectPath {
        self.ifaces
            .get(&ifindex)
            .and_then(|i| i.cur_ac)
            .and_then(|n| self.acs.get(&n))
            .and_then(|ac| ac.ap)
            .filter(|n| self.aps.contains_key(n))
            .map(path::ap)
            .unwrap_or_else(path::root)
    }

    pub fn first_wifi(&self) -> Option<u32> {
        self.ifaces.iter().find(|(_, i)| i.is_wifi()).map(|(k, _)| *k)
    }

    /// Connections usable on `ifindex` (Device.AvailableConnections).
    pub fn available_conns(&self, ifindex: u32) -> Vec<OwnedObjectPath> {
        let Some(i) = self.ifaces.get(&ifindex) else { return Vec::new() };
        let wifi = i.is_wifi();
        let visible: Vec<&[u8]> = self.aps.values().filter(|a| a.ifindex == ifindex && a.listed).map(|a| a.bss.ssid()).collect();
        self.conns
            .iter()
            .filter(|(_, c)| match c.src {
                ConnSrc::Wired { ifindex: x } => x == ifindex,
                _ => wifi && visible.contains(&c.ssid.as_slice()),
            })
            .map(|(&k, _)| path::conn(k))
            .collect()
    }

    pub fn conn_paths(&self) -> Vec<OwnedObjectPath> {
        self.conns.keys().map(|&k| path::conn(k)).collect()
    }
}

impl Ap {
    fn publish_strength(&mut self) {
        let q = self.bss.quality.min(100);
        if q.abs_diff(self.strength) >= STRENGTH_STEP || (q != self.strength && (q == 0 || q == 100)) {
            self.strength = q;
        }
    }
}

/// `a{sv}` helper for IP4Config dicts.
pub fn dict(pairs: Vec<(&str, OwnedValue)>) -> HashMap<String, OwnedValue> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}
