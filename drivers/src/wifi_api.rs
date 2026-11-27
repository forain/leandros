//! The Wi-Fi control seam: what `/dev/netctl` (servers/net/src/netctl.rs,
//! lane L2) needs from a Wi-Fi backend (`iwlwifi`, and the simulated
//! `wifi_sim` for QEMU tests, lane L3), and what a backend gets back.
//!
//! L0 status: types and signatures only. Nothing registers a backend or
//! installs a sink yet, so every function here is inert and the running
//! system behaves exactly as before. The wire types are the ABI records of
//! `netctl-abi` (docs/servers/netctl.md), so a backend reports state in the
//! same shape netctl forwards to userland.
//!
//! ## Directions
//!
//! * netctl → backend: [`WifiBackend::submit`] queues a [`WifiCmd`]; it never
//!   blocks (it runs on an ioctl caller's thread). Reads of current state go
//!   through [`WifiBackend::state`] / [`WifiBackend::bss_snapshot`] /
//!   [`WifiBackend::last_scan`] (snapshots and `NETCTL_GET_*`).
//! * backend → netctl: [`emit`] a [`WifiEvent`] for every change, in the
//!   order it happened. netctl turns each into one event record per open.
//! * profiles: netctl parses the slot files (`netctl_abi::profiles`), merges
//!   the legacy wifi.conf / `wifi.ssid=` profile, and publishes the result
//!   with [`set_profiles`] before submitting [`WifiCmd::ProfilesChanged`].
//!   Backends read them with [`with_profiles`] for autoconnect and for
//!   `Connect` by uuid (netctl resolves the uuid into [`ConnectParams`]
//!   already, so a backend only needs the list for autoconnect).
//!
//! ## Lock order
//!
//! The event sink takes only netctl's own leaf lock (the per-open queues)
//! and never calls back into a backend, so a backend may call [`emit`] while
//! holding its own state lock. Conversely netctl must not hold its queue lock
//! while calling any [`WifiBackend`] method.
//!
//! ## Backend obligations (normative, from docs/servers/netctl.md)
//!
//! * `Disconnect` reaches `wifi_state::DISCONNECTED` within
//!   `DISCONNECT_DEADLINE_MS` (send deauth, report, then any firmware
//!   restart in the background).
//! * A scan never flushes BSSs still within `BSS_AGE_OUT_MS`; aged-out
//!   entries are reported with `WifiEvent::BssGone`. A `Scan` within
//!   `SCAN_COALESCE_MS` of the last one may complete at once with the cache
//!   (`scan_flags::COALESCED`). Every accepted `Scan` ends in `ScanDone`.
//! * FAILED is always followed by DISCONNECTED (or a new CONNECTING).
//! * Secrets are never logged; [`Secret`] wipes itself on drop.
//!
//! ## The netif registry seam (lane L1, `drivers/src/netdev.rs`)
//!
//! Names and types only; L1 owns the definitions. A Wi-Fi backend registers
//! its data path there first, then its control side here with the ifindex
//! it got:
//!
//! ```text
//! pub enum Kind { Ethernet, Wifi }                    // netctl_abi::if_kind 2 / 3
//! pub struct NetifOps {                               // plain fn table, no dyn on the hot path
//!     send:          fn(idx: u8, frame: &[u8]) -> bool,
//!     recv:          fn(idx: u8, buf: &mut [u8]) -> Option<usize>,
//!     rx_rearm:      fn(idx: u8) -> bool,
//!     rx_irq_driven: fn(idx: u8) -> bool,
//!     mac:           fn(idx: u8) -> Option<[u8; 6]>,
//!     link:          fn(idx: u8) -> (bool /*up*/, u32 /*Mbit/s*/, bool /*full duplex*/),
//!     link_gen:      fn(idx: u8) -> u32,
//!     info:          fn(idx: u8, out: &mut String),   // /proc/net/nic section body
//!     set_admin:     fn(idx: u8, up: bool),
//! }
//! pub fn register(kind: Kind, ops: &'static NetifOps, idx: u8, driver: &'static str) -> u32; // ifindex
//! pub fn get(ifindex: u32) -> Option<Netif>;          // Netif { ifindex, name: [u8; 16], kind, ops, idx, driver }
//! pub fn for_each(f: impl FnMut(&Netif));
//! // ifindex 1 = lo (owned by the net server); wired NICs register first
//! // (virtio-net devices, r8169, e1000e) as eth0, eth1, …; then Wi-Fi as
//! // wifi0, …. NETD_TAG / kick / kick_from_irq / take_kick / kicked stay.
//! ```
//!
//! iwlwifi: `let ix = netdev::register(Kind::Wifi, &IWL_OPS, 0, "iwlwifi");
//! wifi_api::register(ix, &IWL_BACKEND);` at probe time (before firmware
//! load, so the stack exists early; the MAC is updated through `mac`).

use alloc::vec::Vec;
use netctl_abi::profiles::Profile;
use netctl_abi::{NcBss, NcScanInfo, NcSsid, NcWifiSignal, NcWifiState, SECRET_MAX};
use spin::Mutex;

/// Interface index as `/dev/netctl` reports it.
pub type IfIndex = u32;

/// Most Wi-Fi interfaces the seam tracks.
pub const MAX_WIFI: usize = 2;

/// A Wi-Fi secret (passphrase, 64-hex PSK or SAE password). Wiped on drop,
/// redacted in `Debug`.
#[derive(Clone)]
pub struct Secret {
    len: u16,
    bytes: [u8; SECRET_MAX],
}

impl Secret {
    pub const fn empty() -> Self { Secret { len: 0, bytes: [0; SECRET_MAX] } }
    /// `None` when longer than `SECRET_MAX`.
    pub fn new(s: &[u8]) -> Option<Self> {
        if s.len() > SECRET_MAX { return None; }
        let mut v = Self::empty();
        v.bytes[..s.len()].copy_from_slice(s);
        v.len = s.len() as u16;
        Some(v)
    }
    pub fn as_bytes(&self) -> &[u8] { &self.bytes[..self.len as usize] }
    pub fn is_empty(&self) -> bool { self.len == 0 }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for b in self.bytes.iter_mut() {
            // SAFETY: plain byte store; volatile so it is not elided.
            unsafe { core::ptr::write_volatile(b, 0) };
        }
    }
}

impl core::fmt::Debug for Secret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Secret(<{} bytes>)", self.len)
    }
}

/// A fully resolved connect request (netctl already looked up the profile
/// for `NETCTL_CONNECT` by uuid, or took the inline fields).
#[derive(Clone, Debug)]
pub struct ConnectParams {
    /// Attempt id to put in every `NcWifiState` of this attempt (non-zero).
    pub attempt: u32,
    /// Profile label (all-zero for an unlabelled inline connect).
    pub profile_uuid: [u8; 16],
    pub ssid: NcSsid,
    /// Join exactly this BSS.
    pub bssid: Option<[u8; 6]>,
    /// `netctl_abi::akm`.
    pub akm: u8,
    /// `netctl_abi::pmf`.
    pub pmf: u8,
    /// Probe for the SSID directly.
    pub hidden: bool,
    pub secret: Secret,
}

/// Commands netctl submits.
#[derive(Clone, Debug)]
pub enum WifiCmd {
    /// Scan (wildcard plus up to `SCAN_MAX_SSIDS` directed probes).
    Scan { ssids: Vec<NcSsid>, force: bool },
    /// Connect; preempts autoconnect and any attempt in progress (the old
    /// one ends FAILED/SUPERSEDED).
    Connect(ConnectParams),
    /// Leave the network; `block_autoconnect` suppresses autoconnect until
    /// the next Connect or ProfilesChanged.
    Disconnect { block_autoconnect: bool },
    /// Soft radio on/off.
    SetRadio(bool),
    /// [`set_profiles`] published a new list: re-evaluate autoconnect, clear
    /// wrong-key blocks and the disconnect block.
    ProfilesChanged,
}

/// Changes a backend reports.
#[derive(Clone, Copy, Debug)]
pub enum WifiEvent {
    State(NcWifiState),
    Signal(NcWifiSignal),
    ScanStarted(NcScanInfo),
    ScanDone(NcScanInfo),
    Bss(NcBss),
    BssGone(NcBss),
}

/// One Wi-Fi backend (one interface).
pub trait WifiBackend: Sync {
    /// Driver name for `NcIface.driver` ("iwlwifi", "wifi_sim").
    fn driver(&self) -> &'static str;
    /// `netctl_abi::wifi_caps` bits.
    fn caps(&self) -> u32;
    /// Current MAC, `None` before the firmware reported it.
    fn mac(&self) -> Option<[u8; 6]>;
    /// Queue a command. Must not block. `Err(errno)` (positive errno:
    /// ENETDOWN radio off / not ready, EOPNOTSUPP unsupported AKM, EINVAL).
    fn submit(&self, cmd: WifiCmd) -> Result<(), i32>;
    /// Current link state (ifindex filled in).
    fn state(&self) -> NcWifiState;
    /// Append every cached BSS to `out`.
    fn bss_snapshot(&self, out: &mut Vec<NcBss>);
    /// The last completed scan, `None` before the first.
    fn last_scan(&self) -> Option<NcScanInfo>;
}

#[derive(Clone, Copy)]
struct Entry {
    ifindex: IfIndex,
    backend: &'static dyn WifiBackend,
}

static BACKENDS: Mutex<[Option<Entry>; MAX_WIFI]> = Mutex::new([None; MAX_WIFI]);

/// Register a backend for `ifindex` (from the netif registry). False when
/// the table is full or the ifindex is taken.
pub fn register(ifindex: IfIndex, backend: &'static dyn WifiBackend) -> bool {
    let ok = {
        let mut t = BACKENDS.lock();
        if t.iter().flatten().any(|e| e.ifindex == ifindex) { return false; }
        match t.iter_mut().find(|e| e.is_none()) {
            Some(slot) => { *slot = Some(Entry { ifindex, backend }); true }
            None => false,
        }
    };
    if ok {
        let f = REGISTER_HOOK.load(core::sync::atomic::Ordering::Acquire);
        if f != 0 { unsafe { core::mem::transmute::<usize, fn()>(f)() } }
    }
    ok
}

/// Drop the backend of `ifindex` (its netif was unregistered). False when none was registered.
pub fn unregister(ifindex: IfIndex) -> bool {
    let mut t = BACKENDS.lock();
    let mut hit = false;
    for e in t.iter_mut() {
        if e.as_ref().map_or(false, |x| x.ifindex == ifindex) { *e = None; hit = true; }
    }
    hit
}

static REGISTER_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Called (no lock held) after each successful [`register`]: lets the
/// netctl model pick a new backend up without polling for it.
pub fn set_register_hook(f: fn()) { REGISTER_HOOK.store(f as usize, core::sync::atomic::Ordering::Release); }

/// The backend of `ifindex`.
pub fn backend(ifindex: IfIndex) -> Option<&'static dyn WifiBackend> {
    BACKENDS.lock().iter().flatten().find(|e| e.ifindex == ifindex).map(|e| e.backend)
}

/// The first registered backend (ifindex 0 in commands means this one).
pub fn first() -> Option<(IfIndex, &'static dyn WifiBackend)> {
    BACKENDS.lock().iter().flatten().min_by_key(|e| e.ifindex).map(|e| (e.ifindex, e.backend))
}

/// Every registered backend, ifindex ascending. The table lock is not held
/// while `f` runs.
pub fn for_each(mut f: impl FnMut(IfIndex, &'static dyn WifiBackend)) {
    let mut list: [Option<Entry>; MAX_WIFI] = *BACKENDS.lock();
    list.sort_by_key(|e| e.map(|e| e.ifindex).unwrap_or(u32::MAX));
    for e in list.iter().flatten() { f(e.ifindex, e.backend); }
}

/// Where backend events go (installed by netctl).
pub type EventSink = fn(IfIndex, &WifiEvent);

static SINK: Mutex<Option<EventSink>> = Mutex::new(None);

/// Install the event sink (netctl, once at init).
pub fn set_event_sink(sink: EventSink) { *SINK.lock() = Some(sink); }

/// Report a change. Dropped while no sink is installed (before netctl is
/// up, the snapshot taken at open covers it).
pub fn emit(ifindex: IfIndex, ev: WifiEvent) {
    let sink = *SINK.lock();
    if let Some(f) = sink { f(ifindex, &ev); }
}

/// The loaded profiles (secrets included) and their generation.
static PROFILES: Mutex<(u32, Vec<Profile>)> = Mutex::new((0, Vec::new()));

/// Publish a new profile list (netctl, after each reload). Replaces the old
/// list, whose secrets are wiped as it drops.
pub fn set_profiles(gen: u32, list: Vec<Profile>) {
    let old = core::mem::replace(&mut *PROFILES.lock(), (gen, list));
    drop(old);
}

/// Run `f` on the current profiles. Do not block or emit inside `f`.
pub fn with_profiles<R>(f: impl FnOnce(u32, &[Profile]) -> R) -> R {
    let g = PROFILES.lock();
    f(g.0, &g.1)
}
