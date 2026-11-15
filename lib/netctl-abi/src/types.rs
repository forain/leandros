//! Command structs, state records and their enumerations.
//!
//! State records ([`NcIface`], [`NcIpv4`], [`NcWifiState`], [`NcBss`],
//! [`NcProfileMeta`], …) are used twice, identically: as the elements the
//! `NETCTL_GET_*` ioctls return, and as event payloads. Their sizes are
//! multiples of [`crate::EVENT_PAYLOAD_SIZE`] (48) so an event carries one
//! record split across whole 64-byte records (see [`crate::event`]).
//!
//! Offsets in the field comments are normative (the tests pin them).

use crate::{Pod, IFNAMSIZ, PROFILE_ID_MAX, SCAN_MAX_SSIDS, SECRET_MAX, SSID_MAX};

macro_rules! pod {
    ($t:ident, $size:expr) => {
        // SAFETY: integer-only #[repr(C)] struct with explicit padding; the
        // size assertion below and the offset tests catch layout mistakes.
        unsafe impl Pod for $t {}
        const _: () = assert!(core::mem::size_of::<$t>() == $size);
        impl Default for $t {
            fn default() -> Self { <$t as Pod>::zeroed() }
        }
        impl $t {
            /// Size of this struct in ABI version 1.
            pub const SIZE: usize = $size;
        }
    };
}

/// Command structs carry their own size first; `new()` fills it in.
macro_rules! cmd {
    ($t:ident, $size:expr) => {
        pod!($t, $size);
        impl $t {
            /// A zeroed command with `size` set.
            pub fn new() -> Self {
                let mut v = <$t as Pod>::zeroed();
                v.size = $size as u32;
                v
            }
        }
    };
}

// ── Enumerations (u8 on the wire) ──────────────────────────────────────────

/// Interface kind (`NcIface.kind`, profile `kind`).
pub mod if_kind {
    pub const UNKNOWN: u8 = 0;
    pub const LOOPBACK: u8 = 1;
    pub const ETHERNET: u8 = 2;
    pub const WIFI: u8 = 3;
}

/// Operational state (`NcIface.oper`).
pub mod oper {
    pub const UNKNOWN: u8 = 0;
    /// Administratively down, or no device behind it.
    pub const DOWN: u8 = 1;
    /// Admin up but no carrier (cable out; Wi-Fi not associated).
    pub const LOWER_DOWN: u8 = 2;
    /// Link layer coming up (Wi-Fi authenticating/associating/4-way).
    pub const DORMANT: u8 = 3;
    /// Frames flow.
    pub const UP: u8 = 4;
}

/// `NcIface.flags`.
pub mod iface_flags {
    /// Administratively up ("managed"); cleared by `DISCONNECT` on wired.
    pub const ADMIN_UP: u32 = 1 << 0;
    /// Carrier / associated.
    pub const CARRIER: u32 = 1 << 1;
    /// Wi-Fi: soft radio on (`SET_RADIO`). Always set for wired.
    pub const RADIO_ON: u32 = 1 << 2;
    /// Wi-Fi: no hardware kill switch engaged. Always set for wired.
    pub const RADIO_HW_ON: u32 = 1 << 3;
    /// Wi-Fi: firmware up and the backend accepts commands.
    pub const READY: u32 = 1 << 4;
}

/// Wi-Fi hardware/driver capabilities (`NcIface.wifi_caps`,
/// `NcVersion.features` uses its own bits).
pub mod wifi_caps {
    pub const PSK: u32 = 1 << 0;
    pub const PSK_SHA256: u32 = 1 << 1;
    pub const SAE: u32 = 1 << 2;
    pub const SAE_H2E: u32 = 1 << 3;
    pub const PMF: u32 = 1 << 4;
    pub const BAND_2GHZ: u32 = 1 << 5;
    pub const BAND_5GHZ: u32 = 1 << 6;
    pub const BAND_6GHZ: u32 = 1 << 7;
    /// Can scan while associated.
    pub const SCAN_CONNECTED: u32 = 1 << 8;
    /// Directed (hidden-SSID) probes.
    pub const DIRECTED_SCAN: u32 = 1 << 9;
    /// Can reassociate to another BSS of the same ESS without leaving first
    /// (the connection manager then roams on its own).
    pub const ROAM: u32 = 1 << 10;
}

/// Authentication and key management, as an enumeration (profiles, connect,
/// negotiated AKM in `NcWifiState`). AKM-agnostic on purpose: SAE slots in
/// without an ABI change.
pub mod akm {
    /// Profiles/connect: "wpa-psk" in NM terms — SAE if both sides can,
    /// else PSK-SHA256, else PSK; the AP's RSN decides. State: not associated.
    pub const AUTO: u8 = 0;
    /// Open network, no encryption.
    pub const NONE: u8 = 1;
    /// WPA2-Personal, AKM 00-0f-ac:2.
    pub const PSK: u8 = 2;
    /// WPA2-Personal with SHA-256, AKM 00-0f-ac:6.
    pub const PSK_SHA256: u8 = 3;
    /// WPA3-Personal, AKM 00-0f-ac:8 (deferred: answered EOPNOTSUPP or
    /// `reason::UNSUPPORTED_SECURITY` until the SAE lane lands).
    pub const SAE: u8 = 4;
    /// Highest value this ABI version defines.
    pub const MAX: u8 = SAE;
}

/// PMF (802.11w) policy, NM's `802-11-wireless-security.pmf` enumeration.
pub mod pmf {
    pub const DEFAULT: u8 = 0;
    pub const DISABLE: u8 = 1;
    pub const OPTIONAL: u8 = 2;
    pub const REQUIRED: u8 = 3;
}

/// Wi-Fi link state (`NcWifiState.state`). One machine per Wi-Fi interface.
pub mod wifi_state {
    /// No firmware / backend not ready / device gone.
    pub const UNAVAILABLE: u8 = 0;
    /// Soft or hardware radio off (`reason` says which: RADIO_OFF / RFKILL).
    pub const RADIO_OFF: u8 = 1;
    /// Idle; may be scanning (flag SCANNING).
    pub const DISCONNECTED: u8 = 2;
    /// A connect was accepted (explicit or autoconnect); target chosen.
    pub const CONNECTING: u8 = 3;
    /// 802.11 authentication (Open System or SAE).
    pub const AUTHENTICATING: u8 = 4;
    pub const ASSOCIATING: u8 = 5;
    /// EAPOL 4-way handshake.
    pub const HANDSHAKE: u8 = 6;
    /// Keys installed, data path open. IP state is separate (`NcIpv4`).
    pub const CONNECTED: u8 = 7;
    pub const DISCONNECTING: u8 = 8;
    /// The attempt failed (`reason` says why). Transient: always followed by
    /// DISCONNECTED (or CONNECTING when autoconnect moves on).
    pub const FAILED: u8 = 9;
}

/// Why the Wi-Fi state changed (`NcWifiState.reason`). Set on FAILED and on
/// the DISCONNECTED / RADIO_OFF that follows; NONE otherwise.
pub mod reason {
    pub const NONE: u8 = 0;
    /// 4-way handshake MIC failure / SAE confirm mismatch: the secret is wrong.
    pub const WRONG_KEY: u8 = 1;
    /// The profile has no secret but the network needs one.
    pub const NO_SECRET: u8 = 2;
    /// No matching AP in range (after a fresh scan).
    pub const NO_AP: u8 = 3;
    /// The AP refused authentication (`ieee_status` has the status code).
    pub const AUTH_REJECTED: u8 = 4;
    /// The AP refused association (`ieee_status` has the status code).
    pub const ASSOC_REJECTED: u8 = 5;
    /// A step timed out (no auth/assoc response, no EAPOL message).
    pub const TIMEOUT: u8 = 6;
    /// Firmware error or restart during the attempt.
    pub const FW_ERROR: u8 = 7;
    /// The AP's security cannot be met (SAE-only AP before WPA3, TKIP, EAP…).
    pub const UNSUPPORTED_SECURITY: u8 = 8;
    /// The AP deauthenticated/disassociated us (`ieee_reason` has the code).
    pub const DEAUTH_BY_AP: u8 = 9;
    pub const BEACON_LOSS: u8 = 10;
    /// `DISCONNECT` from a client.
    pub const USER: u8 = 11;
    /// Soft radio off (`SET_RADIO`).
    pub const RADIO_OFF: u8 = 12;
    /// Hardware kill switch.
    pub const RFKILL: u8 = 13;
    /// A newer `CONNECT` replaced this attempt.
    pub const SUPERSEDED: u8 = 14;
}

/// `NcWifiState.flags`.
pub mod wifi_flags {
    /// A scan is in progress on this interface.
    pub const SCANNING: u32 = 1 << 0;
    /// Autoconnect is suppressed (after `DISCONNECT`, until the next CONNECT
    /// or RELOAD_PROFILES).
    pub const AUTOCONNECT_BLOCKED: u32 = 1 << 1;
    /// The current attempt was started by autoconnect, not by a client.
    pub const AUTO: u32 = 1 << 2;
    /// PMF is in use on the current association.
    pub const PMF_ACTIVE: u32 = 1 << 3;
}

/// IPv4 configuration state (`NcIpv4.state`).
pub mod ip_state {
    pub const NONE: u8 = 0;
    /// DHCP discovering/requesting.
    pub const REQUESTING: u8 = 1;
    /// DHCP lease bound.
    pub const BOUND: u8 = 2;
    /// Static configuration (loopback; manual profiles later).
    pub const STATIC: u8 = 3;
}

/// IPv4 method (`NcIpv4.method`).
pub mod ip_method {
    pub const NONE: u8 = 0;
    pub const AUTO: u8 = 1;
    pub const MANUAL: u8 = 2;
    pub const DISABLED: u8 = 3;
    pub const LOOPBACK: u8 = 4;
}

/// `NcIpv4.flags`.
pub mod ip_flags {
    /// `gw` is valid.
    pub const HAS_GW: u8 = 1 << 0;
    /// This interface's default route is the selected one (lowest metric
    /// among bound interfaces): the "primary" connection.
    pub const DEFAULT_ROUTE: u8 = 1 << 1;
}

/// NetworkManager's `NM80211ApFlags` (`NcBss.nm_flags`), bit-identical.
pub mod nm_ap_flags {
    pub const NONE: u32 = 0x0;
    pub const PRIVACY: u32 = 0x1;
    pub const WPS: u32 = 0x2;
    pub const WPS_PBC: u32 = 0x4;
    pub const WPS_PIN: u32 = 0x8;
}

/// NetworkManager's `NM80211ApSecurityFlags` (`NcBss.wpa_flags` from the
/// WPA1 IE, `NcBss.rsn_flags` from the RSN IE), bit-identical so a shim
/// passes them through. Mapping from AKM suites follows NM: 1/3/5 → 802_1X,
/// 2/4/6 → PSK, 8/9/24/25 → SAE, 18 → OWE, 12/13 → EAP_SUITE_B_192.
pub mod nm_sec {
    pub const NONE: u32 = 0x0;
    pub const PAIR_WEP40: u32 = 0x1;
    pub const PAIR_WEP104: u32 = 0x2;
    pub const PAIR_TKIP: u32 = 0x4;
    pub const PAIR_CCMP: u32 = 0x8;
    pub const GROUP_WEP40: u32 = 0x10;
    pub const GROUP_WEP104: u32 = 0x20;
    pub const GROUP_TKIP: u32 = 0x40;
    pub const GROUP_CCMP: u32 = 0x80;
    pub const KEY_MGMT_PSK: u32 = 0x100;
    pub const KEY_MGMT_802_1X: u32 = 0x200;
    pub const KEY_MGMT_SAE: u32 = 0x400;
    pub const KEY_MGMT_OWE: u32 = 0x800;
    pub const KEY_MGMT_OWE_TM: u32 = 0x1000;
    pub const KEY_MGMT_EAP_SUITE_B_192: u32 = 0x2000;
}

/// Detail NM's flags cannot express (`NcBss.ext`).
pub mod bss_ext {
    /// AKM 00-0f-ac:2 offered (NM folds it into KEY_MGMT_PSK).
    pub const AKM_PSK: u32 = 1 << 0;
    /// AKM 00-0f-ac:6 offered (NM folds it into KEY_MGMT_PSK).
    pub const AKM_PSK_SHA256: u32 = 1 << 1;
    /// AKM 00-0f-ac:8 offered.
    pub const AKM_SAE: u32 = 1 << 2;
    /// AKM 00-0f-ac:24 (SAE-EXT-KEY) offered.
    pub const AKM_SAE_EXT: u32 = 1 << 3;
    pub const AKM_FT_PSK: u32 = 1 << 4;
    pub const AKM_FT_SAE: u32 = 1 << 5;
    pub const AKM_8021X: u32 = 1 << 6;
    pub const AKM_OWE: u32 = 1 << 7;
    /// RSN capabilities: management frame protection capable / required.
    pub const MFPC: u32 = 1 << 8;
    pub const MFPR: u32 = 1 << 9;
    /// RSNXE bit 5: SAE hash-to-element supported.
    pub const SAE_H2E: u32 = 1 << 10;
    /// BSS membership selector 123: H2E only.
    pub const SAE_H2E_ONLY: u32 = 1 << 11;
    /// WPA3 transition: both PSK-family and SAE AKMs offered.
    pub const TRANSITION: u32 = 1 << 12;
    /// WPA1 IE present.
    pub const WPA1: u32 = 1 << 13;
    /// RSN IE present.
    pub const RSN: u32 = 1 << 14;
    /// Beacon hides the SSID (empty or all-zero); `ssid` may come from a
    /// directed probe response.
    pub const HIDDEN: u32 = 1 << 15;
    pub const HT: u32 = 1 << 16;
    pub const VHT: u32 = 1 << 17;
    pub const HE: u32 = 1 << 18;
    /// This interface can join the BSS now with its current capabilities
    /// (open, or PSK/PSK-SHA256 with CCMP; SAE once supported). Clients hide
    /// or grey out BSSs without it (e.g. SAE-only APs before WPA3).
    pub const JOINABLE: u32 = 1 << 31;
}

/// Band (`NcBss.band`).
pub mod band {
    pub const GHZ_2_4: u8 = 0;
    pub const GHZ_5: u8 = 1;
    pub const GHZ_6: u8 = 2;
}

/// Scan outcome (`NcScanInfo.status`).
pub mod scan_status {
    pub const OK: u8 = 0;
    pub const FAILED: u8 = 1;
    pub const ABORTED: u8 = 2;
}

/// `NcScanInfo.flags`.
pub mod scan_flags {
    /// Answered from the cache (request within SCAN_COALESCE_MS).
    pub const COALESCED: u8 = 1 << 0;
    /// Included directed probes.
    pub const DIRECTED: u8 = 1 << 1;
    /// Started by the kernel (periodic/autoconnect), not by a client.
    pub const BACKGROUND: u8 = 1 << 2;
}

/// `NcProfileMeta.flags` / profile entry flags.
pub mod profile_flags {
    /// The SSID is hidden: scan for it with directed probes.
    pub const HIDDEN: u8 = 1 << 0;
    /// (meta only) A secret is stored. The secret itself never leaves the
    /// kernel through /dev/netctl.
    pub const HAS_SECRET: u8 = 1 << 1;
    /// (meta only) Synthesised from /etc/leandros/wifi.conf or the kernel
    /// command line; not in the slot files, cannot be edited through them.
    pub const LEGACY: u8 = 1 << 2;
    /// (meta only) Autoconnect skips it after a WRONG_KEY failure until the
    /// next RELOAD_PROFILES or an explicit CONNECT.
    pub const BLOCKED_WRONG_KEY: u8 = 1 << 3;
    /// Bits a slot file may set (the rest are kernel-reported only).
    pub const STORED_MASK: u8 = HIDDEN;
}

/// Snapshot reason (`NcSnapshotInfo.reason`).
pub mod snapshot_reason {
    /// Every open(2) starts with one.
    pub const OPEN: u32 = 1;
    /// After `RESYNC` (queue overflow).
    pub const RESYNC: u32 = 2;
    /// `NETCTL_REQUEST_SNAPSHOT`.
    pub const REQUESTED: u32 = 3;
}

/// Where the loaded profiles came from (`NcProfilesInfo.source`, `NcReload`).
pub mod profiles_source {
    /// No valid slot (only legacy profiles, if any).
    pub const NONE: u8 = 0;
    pub const SLOT_A: u8 = 1;
    pub const SLOT_B: u8 = 2;
}

/// `NcProfilesInfo.status` / `NcReload.status`.
pub mod profiles_status {
    pub const OK: u8 = 0;
    /// Neither slot exists.
    pub const NO_FILE: u8 = 1;
    /// A slot exists but none is valid (torn/corrupt): see source=NONE.
    pub const CORRUPT: u8 = 2;
    /// I/O error reading the slots.
    pub const IO_ERROR: u8 = 3;
}

/// `NcVersion.features`.
pub mod features {
    /// At least one Wi-Fi backend is compiled in.
    pub const WIFI: u32 = 1 << 0;
    /// SAE / WPA3-Personal is supported by this kernel.
    pub const SAE: u32 = 1 << 1;
    /// PMF is supported.
    pub const PMF: u32 = 1 << 2;
    /// `DISCONNECT`/`CONNECT` on wired interfaces (admin down/up).
    pub const WIRED_ADMIN: u32 = 1 << 3;
}

/// `NcConnect.flags`.
pub mod connect_flags {
    /// Use the inline `ssid`/`akm`/`pmf`/`secret` instead of a loaded profile.
    /// `profile_uuid` (optional) is then only a label echoed in `NcWifiState`.
    pub const INLINE: u32 = 1 << 0;
    /// The SSID is hidden: probe for it directly.
    pub const HIDDEN: u32 = 1 << 1;
    /// `bssid` is valid: join exactly that BSS.
    pub const BSSID: u32 = 1 << 2;
}

/// `NcIfReq.flags` for `DISCONNECT`.
pub mod disconnect_flags {
    /// Do not suppress autoconnect afterwards (NM semantics are to suppress
    /// it until the next CONNECT/RELOAD; this opts out).
    pub const KEEP_AUTOCONNECT: u32 = 1 << 0;
}

/// `NcScan.flags`.
pub mod scan_req_flags {
    /// Bypass coalescing: always run a fresh scan (rate-limited by the kernel
    /// to one per 2 s).
    pub const FORCE: u32 = 1 << 0;
    /// The flags of an explicit user scan (`netctl scan`): always fresh, also
    /// while associated (a background scan on the firmware's channel set).
    pub const USER: u32 = FORCE;
}

// ── Commands ────────────────────────────────────────────────────────────────

/// `NETCTL_GET_VERSION` (out only).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcVersion {
    /// 0: caller's size of this struct (in), kernel's (out).
    pub size: u32,
    /// 4: [`crate::NETCTL_ABI_VERSION`].
    pub abi_version: u32,
    /// 8: bytes per event record (64).
    pub event_size: u32,
    /// 12: event queue capacity per open, in records.
    pub queue_records: u32,
    /// 16
    pub max_ifaces: u16,
    /// 18
    pub max_bss: u16,
    /// 20
    pub max_profiles: u16,
    /// 22
    pub _pad: u16,
    /// 24: [`features`] bits.
    pub features: u32,
    /// 28
    pub _rsv: u32,
}
cmd!(NcVersion, 32);

/// Every `NETCTL_GET_*` list query. Return value: number of elements written
/// (>= 0) or -errno. `count` reports how many exist; when `count > cap`, the
/// first `cap` were written and the caller retries with a bigger buffer.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcQuery {
    /// 0
    pub size: u32,
    /// 4: restrict to one interface; 0 = all (ignored by GET_PROFILES).
    pub ifindex: u32,
    /// 8: in: the caller's element stride (0 = the kernel's own); must be at
    /// least the ABI-1 element size. out: the kernel's element size. The
    /// kernel writes min(stride, its size) bytes per element and zero-fills
    /// the rest of each stride.
    pub elem_size: u32,
    /// 12: in: elements the buffer holds.
    pub cap: u32,
    /// 16: out: elements available.
    pub count: u32,
    /// 20: out: generation of the list (`scan_gen` for GET_BSS, profiles
    /// `gen` for GET_PROFILES, 0 otherwise).
    pub gen: u32,
    /// 24: user pointer to `cap * stride` bytes.
    pub buf: u64,
}
cmd!(NcQuery, 32);

/// One SSID for a directed probe.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcSsid {
    /// 0
    pub len: u8,
    /// 1
    pub bytes: [u8; SSID_MAX],
    /// 33
    pub _pad: [u8; 3],
}
pod!(NcSsid, 36);

impl NcSsid {
    pub fn from_bytes(s: &[u8]) -> Option<Self> {
        if s.len() > SSID_MAX { return None; }
        let mut v = Self::default();
        v.len = s.len() as u8;
        v.bytes[..s.len()].copy_from_slice(s);
        Some(v)
    }
    pub fn as_slice(&self) -> &[u8] { &self.bytes[..(self.len as usize).min(SSID_MAX)] }
}

/// `NETCTL_SCAN`. Returns 0 once the scan is started, joined (one already
/// running) or coalesced; a `SCAN_DONE` event always follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcScan {
    /// 0
    pub size: u32,
    /// 4: Wi-Fi ifindex; 0 = every Wi-Fi interface.
    pub ifindex: u32,
    /// 8: [`scan_req_flags`].
    pub flags: u32,
    /// 12: directed probes in `ssids` (0..=4); a wildcard probe always runs.
    pub n_ssids: u32,
    /// 16
    pub ssids: [NcSsid; SCAN_MAX_SSIDS],
}
cmd!(NcScan, 160);

/// `NETCTL_CONNECT`. Asynchronous: returns 0 when accepted and fills
/// `attempt`; progress and outcome arrive as `WIFI_STATE` events carrying the
/// same attempt id. On a wired ifindex: admin up (resume DHCP); every other
/// field is ignored.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcConnect {
    /// 0
    pub size: u32,
    /// 4: target ifindex; 0 = the first Wi-Fi interface.
    pub ifindex: u32,
    /// 8: [`connect_flags`].
    pub flags: u32,
    /// 12: out: attempt id (non-zero, unique per boot).
    pub attempt: u32,
    /// 16: profile to activate (without INLINE: must be loaded, else ENOENT);
    /// with INLINE: optional label, all-zero = none.
    pub profile_uuid: [u8; 16],
    /// 32: with flag BSSID: the BSS to join.
    pub bssid: [u8; 6],
    /// 38: INLINE: SSID length (1..=32).
    pub ssid_len: u8,
    /// 39: INLINE: [`akm`] (AUTO, NONE, PSK, PSK_SHA256, SAE).
    pub akm: u8,
    /// 40: INLINE: [`pmf`].
    pub pmf: u8,
    /// 41
    pub _pad: u8,
    /// 42: INLINE: secret length (0..=128).
    pub secret_len: u16,
    /// 44: INLINE: SSID bytes.
    pub ssid: [u8; SSID_MAX],
    /// 76
    pub _pad2: u32,
    /// 80: INLINE: passphrase (8..=63), 64 hex digits (raw PSK) or SAE
    /// password (<=128). Never logged; the kernel wipes its copy after use.
    pub secret: [u8; SECRET_MAX],
}
cmd!(NcConnect, 208);

impl NcConnect {
    pub fn ssid(&self) -> &[u8] { &self.ssid[..(self.ssid_len as usize).min(SSID_MAX)] }
    pub fn secret(&self) -> &[u8] { &self.secret[..(self.secret_len as usize).min(SECRET_MAX)] }
    /// Overwrite the secret bytes (callers do this after the ioctl).
    pub fn wipe_secret(&mut self) {
        for b in self.secret.iter_mut() {
            // SAFETY: plain byte store; volatile so it is not elided.
            unsafe { core::ptr::write_volatile(b, 0) };
        }
        self.secret_len = 0;
    }
}

/// `NETCTL_DISCONNECT`. Wi-Fi: leave the network (deauth), reach
/// DISCONNECTED within DISCONNECT_DEADLINE_MS, suppress autoconnect unless
/// KEEP_AUTOCONNECT. Wired: admin down — stop DHCP, drop the address and
/// routes. Returns 0 also when already disconnected.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcIfReq {
    /// 0
    pub size: u32,
    /// 4: ifindex; 0 = the first Wi-Fi interface.
    pub ifindex: u32,
    /// 8: [`disconnect_flags`].
    pub flags: u32,
    /// 12
    pub _rsv: u32,
}
cmd!(NcIfReq, 16);

/// `NETCTL_SET_METRIC`: set an interface's route metric (lower wins the
/// default route). `metric == 0` restores the built-in value (wired 100,
/// Wi-Fi 600, or the `net.metric.<ifname>=` boot knob).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcMetric {
    /// 0
    pub size: u32,
    /// 4: ifindex (>= 2).
    pub ifindex: u32,
    /// 8: new metric, 0 = restore the default.
    pub metric: u32,
    /// 12
    pub _rsv: u32,
}
cmd!(NcMetric, 16);

/// `NETCTL_SET_RADIO`: soft rfkill. Off = disconnect (reason RADIO_OFF),
/// stop scanning, drop the BSS cache (BSS_GONE for each), firmware idle.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcRadio {
    /// 0
    pub size: u32,
    /// 4: Wi-Fi ifindex; 0 = every Wi-Fi interface.
    pub ifindex: u32,
    /// 8: 1 = on, 0 = off.
    pub on: u32,
    /// 12
    pub _rsv: u32,
}
cmd!(NcRadio, 16);

/// `NETCTL_RELOAD_PROFILES`. Synchronous: when it returns 0 the new set is
/// loaded, CONNECT by uuid sees it, and PROFILE…PROFILES_END events have been
/// queued to every open. A missing or corrupt file is not an error (status).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcReload {
    /// 0
    pub size: u32,
    /// 4: out: new profiles generation.
    pub gen: u32,
    /// 8: out: profiles loaded (stored + legacy).
    pub count: u32,
    /// 12: out: [`profiles_source`].
    pub source: u8,
    /// 13: out: [`profiles_status`].
    pub status: u8,
    /// 14
    pub _pad: u16,
}
cmd!(NcReload, 16);

// ── State records (ioctl elements and event payloads) ──────────────────────

/// One network interface. Event kinds IFACE_ADDED / IFACE_CHANGED /
/// IFACE_REMOVED; `NETCTL_GET_IFACES` element. 96 bytes = 2 event parts.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcIface {
    /// 0
    pub ifindex: u32,
    /// 4: [`if_kind`].
    pub kind: u8,
    /// 5: [`oper`].
    pub oper: u8,
    /// 6
    pub _pad: u16,
    /// 8: "lo", "eth0", "wifi0", … NUL-padded.
    pub name: [u8; IFNAMSIZ],
    /// 24: current MAC (all-zero only for lo).
    pub mac: [u8; 6],
    /// 30
    pub mtu: u16,
    /// 32: [`iface_flags`].
    pub flags: u32,
    /// 36: link speed in Mbit/s, 0 = unknown.
    pub speed_mbps: u32,
    /// 40: default-route metric (METRIC_WIRED / METRIC_WIFI unless overridden).
    pub metric: u32,
    /// 44: [`wifi_caps`] (0 for non-Wi-Fi).
    pub wifi_caps: u32,
    /// 48: permanent (burned-in) MAC.
    pub perm_mac: [u8; 6],
    /// 54
    pub _pad2: u16,
    /// 56: bumped at every link-up transition.
    pub link_gen: u32,
    /// 60
    pub _pad3: u32,
    /// 64: driver name ("virtio-net", "e1000e", "r8169", "iwlwifi", "wifi_sim").
    pub driver: [u8; 16],
    /// 80
    pub _rsv: [u8; 16],
}
pod!(NcIface, 96);

impl NcIface {
    pub fn name(&self) -> &[u8] { crate::util::cstr(&self.name) }
    pub fn driver(&self) -> &[u8] { crate::util::cstr(&self.driver) }
}

/// IPv4 configuration of one interface. Event kind IPV4; `NETCTL_GET_IPV4`
/// element. 48 bytes = 1 part.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcIpv4 {
    /// 0
    pub ifindex: u32,
    /// 4: [`ip_state`].
    pub state: u8,
    /// 5: prefix length (0..=32).
    pub prefix: u8,
    /// 6: valid entries in `dns` (0..=3).
    pub n_dns: u8,
    /// 7: [`ip_flags`].
    pub flags: u8,
    /// 8: address, network byte order (a.b.c.d = [a, b, c, d]).
    pub addr: [u8; 4],
    /// 12: gateway (valid with HAS_GW).
    pub gw: [u8; 4],
    /// 16: DNS servers learnt on this interface.
    pub dns: [[u8; 4]; 3],
    /// 28: DHCP server.
    pub server: [u8; 4],
    /// 32: route metric.
    pub metric: u32,
    /// 36: seconds left on the lease when emitted; 0 = n/a.
    pub lease_s: u32,
    /// 40: [`ip_method`].
    pub method: u8,
    /// 41
    pub _pad: [u8; 3],
    /// 44
    pub _rsv: u32,
}
pod!(NcIpv4, 48);

/// Wi-Fi link state of one interface. Event kind WIFI_STATE;
/// `NETCTL_GET_WIFI` element. 96 bytes = 2 parts.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcWifiState {
    /// 0
    pub ifindex: u32,
    /// 4: [`wifi_state`].
    pub state: u8,
    /// 5: [`reason`].
    pub reason: u8,
    /// 6: negotiated [`akm`] (AUTO = not associated).
    pub akm: u8,
    /// 7: 1 when PMF is negotiated.
    pub pmf: u8,
    /// 8: last 802.11 status code from the AP (auth/assoc response), 0 = none.
    pub ieee_status: u16,
    /// 10: last 802.11 reason code (deauth/disassoc), 0 = none.
    pub ieee_reason: u16,
    /// 12: attempt id (from NETCTL_CONNECT, or kernel-assigned for
    /// autoconnect); 0 when idle.
    pub attempt: u32,
    /// 16: profile in use (all-zero for an unlabelled inline connect).
    pub profile_uuid: [u8; 16],
    /// 32: target / current BSS.
    pub bssid: [u8; 6],
    /// 38
    pub freq_mhz: u16,
    /// 40: dBm, 0 = unknown.
    pub signal_dbm: i16,
    /// 42
    pub ssid_len: u8,
    /// 43
    pub _pad: u8,
    /// 44
    pub ssid: [u8; SSID_MAX],
    /// 76: TX bitrate, kbit/s.
    pub tx_rate_kbps: u32,
    /// 80: boot-time milliseconds of the last `state` change.
    pub since_ms: u32,
    /// 84: [`wifi_flags`].
    pub flags: u32,
    /// 88
    pub _rsv: [u8; 8],
}
pod!(NcWifiState, 96);

impl NcWifiState {
    pub fn ssid(&self) -> &[u8] { &self.ssid[..(self.ssid_len as usize).min(SSID_MAX)] }
}

/// Signal update while connected (event WIFI_SIGNAL; at most every 2 s and
/// only on a change of >= 3 dB). 48 bytes = 1 part.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcWifiSignal {
    /// 0
    pub ifindex: u32,
    /// 4
    pub signal_dbm: i16,
    /// 6: [`crate::util::quality_from_dbm`].
    pub quality: u8,
    /// 7
    pub _pad: u8,
    /// 8
    pub tx_rate_kbps: u32,
    /// 12
    pub bssid: [u8; 6],
    /// 18
    pub _pad2: u16,
    /// 20
    pub _rsv: [u8; 28],
}
pod!(NcWifiSignal, 48);

/// Scan start/completion (events SCAN_STARTED / SCAN_DONE). 48 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcScanInfo {
    /// 0
    pub ifindex: u32,
    /// 4: [`scan_status`] (SCAN_DONE only).
    pub status: u8,
    /// 5: [`scan_flags`].
    pub flags: u8,
    /// 6
    pub _pad: u16,
    /// 8: BSS entries cached after this scan.
    pub n_bss: u32,
    /// 12: bumped per completed (non-coalesced) scan.
    pub scan_gen: u32,
    /// 16: boot-time milliseconds of the event (NM `LastScan` uses it).
    pub time_ms: u64,
    /// 24
    pub _rsv: [u8; 24],
}
pod!(NcScanInfo, 48);

/// One BSS from the scan cache. Event kinds BSS (new or changed) and
/// BSS_GONE (aged out / radio off); `NETCTL_GET_BSS` element. 96 bytes =
/// 2 parts. Key: (ifindex, bssid).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcBss {
    /// 0
    pub ifindex: u32,
    /// 4
    pub bssid: [u8; 6],
    /// 10
    pub freq_mhz: u16,
    /// 12: dBm.
    pub signal_dbm: i16,
    /// 14: 0..=100, [`crate::util::quality_from_dbm`] (NM's `Strength`).
    pub quality: u8,
    /// 15: 0..=32; 0 for a hidden SSID not yet resolved.
    pub ssid_len: u8,
    /// 16
    pub ssid: [u8; SSID_MAX],
    /// 48: [`nm_ap_flags`].
    pub nm_flags: u32,
    /// 52: [`nm_sec`] from the WPA1 IE.
    pub wpa_flags: u32,
    /// 56: [`nm_sec`] from the RSN IE.
    pub rsn_flags: u32,
    /// 60: [`bss_ext`].
    pub ext: u32,
    /// 64: raw RSN capabilities field.
    pub rsn_caps: u16,
    /// 66
    pub channel: u8,
    /// 67: [`band`].
    pub band: u8,
    /// 68: 802.11 capability information.
    pub capability: u16,
    /// 70: TU.
    pub beacon_int: u16,
    /// 72: ms since last seen, when emitted.
    pub age_ms: u32,
    /// 76: highest supported rate, kbit/s.
    pub max_rate_kbps: u32,
    /// 80: boot-time ms when last seen (NM `LastSeen` is in seconds).
    pub last_seen_ms: u32,
    /// 84
    pub _rsv: [u8; 12],
}
pod!(NcBss, 96);

impl NcBss {
    pub fn ssid(&self) -> &[u8] { &self.ssid[..(self.ssid_len as usize).min(SSID_MAX)] }
}

/// Secret-free metadata of one profile. Event kind PROFILE;
/// `NETCTL_GET_PROFILES` element. 192 bytes = 4 parts.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcProfileMeta {
    /// 0
    pub uuid: [u8; 16],
    /// 16: [`if_kind`] (WIFI; ETHERNET reserved).
    pub kind: u8,
    /// 17: [`akm`].
    pub akm: u8,
    /// 18: 0/1.
    pub autoconnect: u8,
    /// 19: [`profile_flags`].
    pub flags: u8,
    /// 20: autoconnect priority, higher first.
    pub priority: i32,
    /// 24: profiles generation this record belongs to.
    pub gen: u32,
    /// 28
    pub ssid_len: u8,
    /// 29
    pub id_len: u8,
    /// 30: [`pmf`].
    pub pmf: u8,
    /// 31
    pub _pad: u8,
    /// 32
    pub ssid: [u8; SSID_MAX],
    /// 64: display name, UTF-8.
    pub id: [u8; PROFILE_ID_MAX],
    /// 128: bind to this interface name; empty = any.
    pub iface: [u8; IFNAMSIZ],
    /// 144: last successful connection, seconds since the epoch (0 = never),
    /// as stored by the writer.
    pub timestamp_s: u64,
    /// 152
    pub _rsv: [u8; 40],
}
pod!(NcProfileMeta, 192);

impl NcProfileMeta {
    pub fn ssid(&self) -> &[u8] { &self.ssid[..(self.ssid_len as usize).min(SSID_MAX)] }
    pub fn id(&self) -> &[u8] { &self.id[..(self.id_len as usize).min(PROFILE_ID_MAX)] }
    pub fn iface(&self) -> &[u8] { crate::util::cstr(&self.iface) }
}

/// Snapshot framing (events SNAPSHOT_BEGIN / SNAPSHOT_END). 48 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcSnapshotInfo {
    /// 0: [`snapshot_reason`].
    pub reason: u32,
    /// 4: [`crate::NETCTL_ABI_VERSION`].
    pub abi_version: u32,
    /// 8: interfaces in this snapshot.
    pub n_ifaces: u32,
    /// 12: BSS records in this snapshot.
    pub n_bss: u32,
    /// 16: PROFILE records in this snapshot.
    pub n_profiles: u32,
    /// 20: profiles generation.
    pub profiles_gen: u32,
    /// 24: 1 if this open may issue mutating ioctls (root or netdev at open).
    pub writable: u32,
    /// 28
    pub _rsv: [u8; 20],
}
pod!(NcSnapshotInfo, 48);

/// Event RESYNC: the queue of this open overflowed; everything queued was
/// discarded and a full snapshot follows. 48 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcResync {
    /// 0: logical events dropped (saturating).
    pub dropped: u32,
    /// 4
    pub _rsv: [u8; 44],
}
pod!(NcResync, 48);

/// Event PROFILES_END: closes a PROFILE batch. Clients replace their whole
/// profile list with the PROFILE records of generation `gen`. 48 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcProfilesInfo {
    /// 0
    pub gen: u32,
    /// 4: profiles in this generation.
    pub count: u32,
    /// 8: [`profiles_source`].
    pub source: u8,
    /// 9: [`profiles_status`].
    pub status: u8,
    /// 10: how many of `count` are LEGACY.
    pub n_legacy: u8,
    /// 11
    pub _pad: u8,
    /// 12: global flags of the loaded slot
    /// ([`crate::profiles::global_flags`]).
    pub global_flags: u32,
    /// 16: `seq` of the loaded slot (0 when none).
    pub seq: u64,
    /// 24
    pub _rsv: [u8; 24],
}
pod!(NcProfilesInfo, 48);
