//! The Wi-Fi connection manager behind the `/dev/netctl` seam
//! (`wifi_api`): one command-driven state machine per Wi-Fi interface,
//! shared by the real driver (`iwlwifi`, with `iwlwifi.mgr=1`) and the
//! simulated one (`wifi_sim`), so the QEMU tests exercise the same policy
//! code the laptop runs.
//!
//! The split:
//! * [`Mgr`] is the `WifiBackend`: a command queue (`submit` never blocks),
//!   the published `NcWifiState`, the BSS cache with aging, the last scan,
//!   and the autoconnect bookkeeping (wrong-key blocks, back-off, the
//!   disconnect block, the last good profile). Every change is emitted as a
//!   `WifiEvent` and also kept in a small ring ([`Mgr::events_since`]) for
//!   the self-test and for debugging.
//! * [`Radio`] is what a driver provides: scan, connect (auth, assoc, 4-way),
//!   watch the link, leave, and the slow clean-up after a failure.
//! * [`run`] is the driver thread's loop once the firmware is up.
//!
//! Policy (docs/servers/netctl.md, "Wi-Fi state machine"):
//! * `Disconnect` reaches DISCONNECTED within `DISCONNECT_DEADLINE_MS`: the
//!   radio's fast `leave` (deauth, data path closed) runs first, the event
//!   goes out, and only then the slow `settle` (firmware clean-up).
//! * A scan never flushes a BSS seen within the age-out window
//!   (`BSS_AGE_OUT_MS`); entries are aged only when a scan completes, so a
//!   radio that cannot scan while associated keeps its list.
//! * Requests within `SCAN_COALESCE_MS` of the last scan (2 s with FORCE)
//!   complete at once from the cache with `scan_flags::COALESCED`.
//! * Autoconnect (NetworkManager's rules): only while disconnected — a
//!   working connection is never left for autoconnect reasons. Among the
//!   profiles with autoconnect on: highest priority (0 unless set, the legacy
//!   wifi.conf / `wifi.ssid=` network included), then the most recently
//!   connected (this boot, then the stored `timestamp_s`), then the legacy
//!   network (configured for this machine), then the signal — except that a
//!   usable 5 GHz BSS goes before the stored timestamp, the legacy flag and
//!   the signal ([`auto_key`], `wifi.prefer5=0` turns that off). So after a
//!   failed activation (the user's, or one autoconnect tried) the manager
//!   goes back to the network it was on last. Wrong-key failures block the
//!   profile until the next `ProfilesChanged` or explicit `Connect`.
//!   Failures of one access point (auth/association refused, deauthenticated
//!   during the handshake, no answer, a link that dropped within
//!   `FLAP_MS`) back that BSS off for 15 s, 60 s, 120 s, then 300 s, so a
//!   flaky AP cannot make autoconnect ping-pong; other failures back the
//!   profile off 10 s, 30 s, 60 s.
//! * A BSS that cannot be joined (`JOINABLE` clear) has its RSN element
//!   logged once (AKMs, ciphers, MFPC/MFPR) with the exact cause
//!   ([`join_cause`]), and a connection to it fails UNSUPPORTED_SECURITY
//!   with that cause as its message.
//! * AKM selection is one function ([`select_akm`]) driven by the radio's
//!   `wifi_caps`; SAE slots in by adding the cap and the radio's SAE path.

#![allow(dead_code)]

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;

use netctl_abi::profiles::Profile;
use netctl_abi::util::{channel_to_freq, quality_from_dbm, uuid_is_nil};
use netctl_abi::{
    akm, band, bss_ext, if_kind, nm_ap_flags, nm_sec, reason, scan_flags, scan_status, wifi_caps, wifi_flags,
    wifi_state, NcBss, NcScanInfo, NcSsid, NcWifiSignal, NcWifiState, BSS_AGE_OUT_MS, SCAN_COALESCE_MS, SSID_MAX,
};

use crate::wifi_api::{ConnectParams, IfIndex, Secret, WifiBackend, WifiCmd, WifiEvent};
use crate::wifi_frame as wf;

// ── RSN suite selectors (OUI 00-0f-ac || type, big-endian) ─────────────────

pub const SUITE_TKIP: u32 = 0x000f_ac02;
pub const SUITE_CCMP: u32 = 0x000f_ac04;
pub const SUITE_WEP40: u32 = 0x000f_ac01;
pub const SUITE_WEP104: u32 = 0x000f_ac05;
pub const AKM_8021X: u32 = 0x000f_ac01;
pub const AKM_PSK: u32 = 0x000f_ac02;
pub const AKM_FT_8021X: u32 = 0x000f_ac03;
pub const AKM_FT_PSK: u32 = 0x000f_ac04;
pub const AKM_8021X_SHA256: u32 = 0x000f_ac05;
pub const AKM_PSK_SHA256: u32 = 0x000f_ac06;
pub const AKM_SAE: u32 = 0x000f_ac08;
pub const AKM_FT_SAE: u32 = 0x000f_ac09;
pub const AKM_SUITE_B_192: u32 = 0x000f_ac0c;
pub const AKM_SUITE_B: u32 = 0x000f_ac0b;
pub const AKM_OWE: u32 = 0x000f_ac12;
pub const AKM_SAE_EXT: u32 = 0x000f_ac18;
pub const AKM_FT_SAE_EXT: u32 = 0x000f_ac19;

/// RSN capabilities bits.
pub const RSN_CAP_MFPR: u16 = 1 << 6;
pub const RSN_CAP_MFPC: u16 = 1 << 7;

/// 802.11 reason codes the failure classification looks at.
pub const IEEE_REASON_UNSPECIFIED: u16 = 1;
pub const IEEE_REASON_PREV_AUTH_INVALID: u16 = 2;
pub const IEEE_REASON_LEAVING: u16 = 3;
pub const IEEE_REASON_MIC_FAILURE: u16 = 14;
pub const IEEE_REASON_4WAY_TIMEOUT: u16 = 15;
pub const IEEE_REASON_8021X_FAILED: u16 = 23;

// ── timing ──────────────────────────────────────────────────────────────────

/// Periodic background scan while disconnected and the radio is on.
pub const IDLE_SCAN_MS: u64 = 30_000;
/// Background scan while connected (only radios with SCAN_CONNECTED).
pub const CONNECTED_SCAN_MS: u64 = 120_000;
/// FORCE still runs at most one fresh scan per this interval.
pub const FORCE_MIN_MS: u64 = 2_000;
/// Autoconnect uses the cache when it is this fresh, else scans first.
pub const AUTO_CACHE_MS: u64 = 10_000;
/// Back-off after a non-wrong-key failure, by consecutive failures.
pub const BACKOFF_MS: [u64; 3] = [10_000, 30_000, 60_000];
/// Back-off of one BSS after it failed us, by consecutive failures.
pub const BSS_BACKOFF_MS: [u64; 4] = [15_000, 60_000, 120_000, 300_000];
/// Autoconnect pause after a failed user-requested connection.
const USER_FAIL_AUTO_DELAY_MS: u64 = 2_000;
/// A link the AP ended within this long after CONNECTED is a flap; the
/// second flap of a BSS within `FLAP_WINDOW_MS` counts as a failure of that
/// BSS (it backs off). The first is a link that was lost: reconnect at once.
pub const FLAP_MS: u64 = 30_000;
pub const FLAP_WINDOW_MS: u64 = 120_000;
/// Signal events: at most this often, and only on a change of >= 3 dB.
pub const SIGNAL_MIN_MS: u64 = 2_000;
pub use crate::wifi_pref::{auto_key, set_knobs, PREFER_5GHZ, PREFER_5GHZ_MIN_DBM};
/// Kernel-assigned attempt ids (autoconnect) have this bit set; ids from
/// `/dev/netctl` (CONNECT) are expected below it.
pub const AUTO_ATTEMPT_BIT: u32 = 0x8000_0000;
/// Events kept in the ring for the self-test and /proc.
const EVENT_RING: usize = 256;
/// Commands queued at most (Scan requests are merged).
const CMD_QUEUE_MAX: usize = 32;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }

/// Group-key state and TKIP receive counters, shared by the Intel driver and
/// the simulated radio; shown by /proc/net/wifi (`# Group key`).
pub mod gk {
    use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static TKIP: AtomicU64 = AtomicU64::new(0);
    static KEY_ID: AtomicU64 = AtomicU64::new(0);
    static INSTALLS: AtomicU64 = AtomicU64::new(0);
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    /// 0 fw-decrypted, 1 host-decrypted, 2 Michael failures, 3 replay drops, 4 undecryptable.
    static RX: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
    pub const FW: usize = 0;
    pub const SW: usize = 1;
    pub const MIC: usize = 2;
    pub const REPLAY: usize = 3;
    pub const UNDECRYPTED: usize = 4;
    pub fn note_install(key_id: u8, tkip: bool) {
        TKIP.store(tkip as u64 + 1, Relaxed);
        KEY_ID.store(key_id as u64, Relaxed);
        LAST_MS.store(super::now_ms().max(1), Relaxed);
        INSTALLS.fetch_add(1, Relaxed);
    }
    pub fn note_rx(kind: usize) { RX[kind].fetch_add(1, Relaxed); }
    pub fn installs() -> u64 { INSTALLS.load(Relaxed) }
    pub fn key_id() -> u64 { KEY_ID.load(Relaxed) }
    pub fn rx(kind: usize) -> u64 { RX[kind].load(Relaxed) }
    pub fn lines(out: &mut alloc::string::String) {
        use core::fmt::Write;
        let _ = writeln!(out);
        let _ = writeln!(out, "# Group key (since boot)");
        let c = match TKIP.load(Relaxed) { 1 => "ccmp", 2 => "tkip", _ => "none" };
        let last = LAST_MS.load(Relaxed);
        let _ = writeln!(out, "gtk: cipher {} key_index {} installs {} last_install {}", c, KEY_ID.load(Relaxed), INSTALLS.load(Relaxed),
                         if last == 0 { alloc::string::String::from("never") } else { alloc::format!("{} s ago", super::now_ms().saturating_sub(last) / 1000) });
        let _ = writeln!(out, "gtk_rx: tkip_fw {} tkip_sw {} michael_fail {} replay_drop {} undecryptable {}",
                         rx(FW), rx(SW), rx(MIC), rx(REPLAY), rx(UNDECRYPTED));
    }
}
fn msleep(ms: u64) { sched::kthread::msleep(ms) }

// ── what a radio reports ────────────────────────────────────────────────────

/// One BSS seen by a scan.
#[derive(Clone, Debug)]
pub struct ScanHit {
    pub bss: wf::Bss,
    pub rssi: i8,
}

/// Why an attempt or a link ended.
#[derive(Clone, Debug)]
pub struct Fail {
    /// `netctl_abi::reason`.
    pub reason: u8,
    /// 802.11 status code (auth/assoc response), 0 = none.
    pub ieee_status: u16,
    /// 802.11 reason code (deauth/disassoc), 0 = none.
    pub ieee_reason: u16,
    /// The firmware asserted (the radio must be restarted).
    pub fw_assert: bool,
    pub msg: String,
}

impl Fail {
    pub fn new(reason: u8, msg: impl Into<String>) -> Fail {
        Fail { reason, ieee_status: 0, ieee_reason: 0, fw_assert: false, msg: msg.into() }
    }
    pub fn status(mut self, s: u16) -> Fail { self.ieee_status = s; self }
    pub fn ieee_reason(mut self, r: u16) -> Fail { self.ieee_reason = r; self }
    pub fn fw_assert(msg: impl Into<String>) -> Fail {
        Fail { reason: reason::FW_ERROR, ieee_status: 0, ieee_reason: 0, fw_assert: true, msg: msg.into() }
    }
    pub fn superseded() -> Fail { Fail::new(reason::SUPERSEDED, "preempted by a newer command") }
}

/// The 4-way handshake ended without keys: decide between a wrong secret
/// and a plain timeout. The pattern of a wrong passphrase on WPA2-PSK is
/// that the AP sent message 1, got our message 2, could not verify its MIC
/// (the PMK differs) and so never sends message 3: it retransmits message 1
/// and finally deauthenticates with reason 15 (4-way handshake timeout),
/// some APs with 14 (MIC failure) or 23 (802.1X failed), others just go
/// quiet. Without a message 1 at all it is a timeout, not the key.
pub fn handshake_failure(msg2_sent: bool, deauth_reason: Option<u16>) -> Fail {
    match deauth_reason {
        Some(r) if msg2_sent && matches!(r, IEEE_REASON_4WAY_TIMEOUT | IEEE_REASON_MIC_FAILURE | IEEE_REASON_8021X_FAILED
            | IEEE_REASON_PREV_AUTH_INVALID | IEEE_REASON_UNSPECIFIED) =>
            Fail::new(reason::WRONG_KEY, alloc::format!("the AP ended the handshake (reason {}): wrong passphrase", r)).ieee_reason(r),
        Some(r) => Fail::new(reason::DEAUTH_BY_AP, alloc::format!("deauthenticated during the handshake (reason {})", r)).ieee_reason(r),
        None if msg2_sent => Fail::new(reason::WRONG_KEY, "4-way handshake timed out after message 2: wrong passphrase"),
        None => Fail::new(reason::TIMEOUT, "no EAPOL message 1 from the AP"),
    }
}

/// Context a radio's `connect` gets: progress reports and preemption.
pub struct ConnCtx<'a> {
    mgr: &'a Mgr,
    deadline_ms: u64,
}

impl ConnCtx<'_> {
    /// Report the next step (AUTHENTICATING, ASSOCIATING, HANDSHAKE).
    pub fn step(&self, state: u8) { self.mgr.set_step(state); }
    /// A newer command (Connect, Disconnect, radio off) is waiting, or the
    /// attempt ran past `CONNECT_DEADLINE_MS`: stop and return
    /// [`Fail::superseded`] (or a TIMEOUT).
    pub fn aborted(&self) -> bool { self.mgr.preempt_pending() }
    /// Milliseconds left of the attempt's overall deadline.
    pub fn left_ms(&self) -> u64 { self.deadline_ms.saturating_sub(now_ms()) }
    /// Sleep up to `ms`, waking early when preempted. False when preempted.
    pub fn sleep(&self, ms: u64) -> bool {
        let t0 = now_ms();
        while now_ms() - t0 < ms {
            if self.aborted() { return false; }
            msleep((ms - (now_ms() - t0)).min(20).max(1));
        }
        !self.aborted()
    }
}

/// What a Wi-Fi driver provides to [`run`].
pub trait Radio {
    /// `netctl_abi::wifi_caps`.
    fn caps(&self) -> u32;
    /// Scan: wildcard plus directed probes for `ssids`. `connected` is true
    /// when associated (only called then if caps has SCAN_CONNECTED).
    fn scan(&mut self, ssids: &[Vec<u8>], connected: bool) -> Result<Vec<ScanHit>, Fail>;
    /// Authenticate, associate and (unless `akm == NONE`) run the 4-way
    /// handshake with `secret`; on success the data path is open.
    fn connect(&mut self, hit: &ScanHit, ssid: &[u8], akm: u8, secret: &[u8], cx: &ConnCtx) -> Result<(), Fail>;
    /// Associated: service the link for up to `budget_ms`. `Some` when it
    /// went away (deauth by AP, beacon loss, firmware error).
    fn poll_connected(&mut self, budget_ms: u64) -> Option<Fail>;
    /// Fast part of leaving (a few ms): close the data path, send a
    /// deauthentication when associated (`deauth`).
    fn leave(&mut self, deauth: bool);
    /// Slow part, after the state was reported: put the firmware back into a
    /// clean unassociated state (a restart after `after.fw_assert`). False
    /// when the device is gone for good.
    fn settle(&mut self, after: Option<&Fail>) -> bool;
    /// Idle: service the device for up to `ms`.
    fn idle(&mut self, ms: u64);
    /// Soft radio switch.
    fn set_radio(&mut self, on: bool) -> bool;
    /// Hardware kill switch engaged.
    fn hw_rfkill(&mut self) -> bool { false }
    /// The kill switch was released: bring the radio back (false = dead).
    fn rfkill_released(&mut self) -> bool { self.settle(None) }
    /// Current signal of the associated BSS.
    fn signal(&mut self) -> Option<i8> { None }
    /// TX rate in kbit/s, 0 = unknown.
    fn tx_rate_kbps(&mut self) -> u32 { 0 }
}

// ── BSS → NcBss ─────────────────────────────────────────────────────────────

fn akm_ext(s: u32) -> u32 {
    match s {
        AKM_PSK => bss_ext::AKM_PSK,
        AKM_PSK_SHA256 => bss_ext::AKM_PSK_SHA256,
        AKM_SAE => bss_ext::AKM_SAE,
        AKM_SAE_EXT => bss_ext::AKM_SAE_EXT,
        AKM_FT_PSK => bss_ext::AKM_FT_PSK,
        AKM_FT_SAE | AKM_FT_SAE_EXT => bss_ext::AKM_FT_SAE,
        AKM_8021X | AKM_FT_8021X | AKM_8021X_SHA256 | AKM_SUITE_B | AKM_SUITE_B_192 => bss_ext::AKM_8021X,
        AKM_OWE => bss_ext::AKM_OWE,
        _ => 0,
    }
}

/// NetworkManager's mapping of AKM suites to `NM80211ApSecurityFlags`.
fn akm_nm(s: u32) -> u32 {
    match s {
        AKM_8021X | AKM_FT_8021X | AKM_8021X_SHA256 => nm_sec::KEY_MGMT_802_1X,
        AKM_PSK | AKM_FT_PSK | AKM_PSK_SHA256 => nm_sec::KEY_MGMT_PSK,
        AKM_SAE | AKM_FT_SAE | AKM_SAE_EXT | AKM_FT_SAE_EXT => nm_sec::KEY_MGMT_SAE,
        AKM_OWE => nm_sec::KEY_MGMT_OWE,
        AKM_SUITE_B | AKM_SUITE_B_192 => nm_sec::KEY_MGMT_EAP_SUITE_B_192,
        _ => 0,
    }
}

fn pair_nm(s: u32) -> u32 {
    match s { SUITE_CCMP => nm_sec::PAIR_CCMP, SUITE_TKIP => nm_sec::PAIR_TKIP,
              SUITE_WEP40 => nm_sec::PAIR_WEP40, SUITE_WEP104 => nm_sec::PAIR_WEP104, _ => 0 }
}

fn group_nm(s: u32) -> u32 {
    match s { SUITE_CCMP => nm_sec::GROUP_CCMP, SUITE_TKIP => nm_sec::GROUP_TKIP,
              SUITE_WEP40 => nm_sec::GROUP_WEP40, SUITE_WEP104 => nm_sec::GROUP_WEP104, _ => 0 }
}

/// The AKM to use against a BSS (`ext` = its `bss_ext` bits, `rsn_flags` =
/// its NM security flags from the RSN IE, `privacy` = capability bit) for a
/// requested `want` (`netctl_abi::akm`), given the radio's `caps`. `Err` is
/// the failure reason. Order for AUTO: SAE > PSK-SHA256 > PSK, each only
/// when both sides can do it. This is the one place WPA3 changes: add
/// `wifi_caps::SAE` to the radio and its SAE authentication path.
pub fn select_akm(want: u8, ext: u32, rsn_flags: u32, privacy: bool, caps: u32) -> Result<u8, u8> {
    let rsn = ext & bss_ext::RSN != 0;
    let open = !rsn && ext & bss_ext::WPA1 == 0 && !privacy;
    if want == akm::NONE {
        return if open { Ok(akm::NONE) } else { Err(reason::UNSUPPORTED_SECURITY) };
    }
    if open || !rsn {
        // An open network for a secured profile, or WPA1/WEP-only.
        return Err(reason::UNSUPPORTED_SECURITY);
    }
    // Pairwise CCMP, and a CCMP or TKIP group key (WPA/WPA2 mixed mode:
    // the group key is only ever received), is what the data path can do.
    let ccmp = rsn_flags & nm_sec::PAIR_CCMP != 0 && rsn_flags & (nm_sec::GROUP_CCMP | nm_sec::GROUP_TKIP) != 0;
    let mfpr = ext & bss_ext::MFPR != 0;
    let pmf_ok = !mfpr || caps & wifi_caps::PMF != 0;
    let can = |a: u8| -> bool {
        match a {
            akm::SAE => caps & wifi_caps::SAE != 0 && ext & bss_ext::AKM_SAE != 0 && caps & wifi_caps::PMF != 0,
            akm::PSK_SHA256 => caps & wifi_caps::PSK_SHA256 != 0 && ext & bss_ext::AKM_PSK_SHA256 != 0 && ccmp && pmf_ok,
            akm::PSK => caps & wifi_caps::PSK != 0 && ext & bss_ext::AKM_PSK != 0 && ccmp && pmf_ok,
            _ => false,
        }
    };
    let order: &[u8] = match want {
        akm::AUTO => &[akm::SAE, akm::PSK_SHA256, akm::PSK],
        akm::PSK => &[akm::PSK],
        akm::PSK_SHA256 => &[akm::PSK_SHA256],
        akm::SAE => &[akm::SAE],
        _ => &[],
    };
    order.iter().copied().find(|&a| can(a)).ok_or(reason::UNSUPPORTED_SECURITY)
}

fn suite_name(s: u32) -> String {
    match s {
        SUITE_WEP40 => "WEP-40".into(), SUITE_TKIP => "TKIP".into(), SUITE_CCMP => "CCMP".into(),
        SUITE_WEP104 => "WEP-104".into(), 0x000f_ac06 => "BIP-CMAC-128".into(), 0x000f_ac07 => "group-not-allowed".into(),
        0x000f_ac08 => "GCMP-128".into(), 0x000f_ac09 => "GCMP-256".into(), 0x000f_ac0a => "CCMP-256".into(),
        _ => alloc::format!("{:06x}:{}", s >> 8, s & 0xff),
    }
}

fn akm_suite_name(s: u32) -> String {
    match s {
        AKM_8021X => "802.1X".into(), AKM_PSK => "PSK".into(), AKM_FT_8021X => "FT-802.1X".into(),
        AKM_FT_PSK => "FT-PSK".into(), AKM_8021X_SHA256 => "802.1X-SHA256".into(), AKM_PSK_SHA256 => "PSK-SHA256".into(),
        AKM_SAE => "SAE".into(), AKM_FT_SAE => "FT-SAE".into(), AKM_SUITE_B => "SuiteB".into(),
        AKM_SUITE_B_192 => "SuiteB-192".into(), AKM_OWE => "OWE".into(), AKM_SAE_EXT => "SAE-EXT".into(),
        AKM_FT_SAE_EXT => "FT-SAE-EXT".into(),
        _ => alloc::format!("{:06x}:{}", s >> 8, s & 0xff),
    }
}

/// The RSN element as one log fragment: version, group and pairwise ciphers,
/// AKMs, capabilities with MFPC/MFPR (and whether a WPA1 element is there).
pub fn rsn_details(b: &wf::Bss) -> String {
    match b.rsn.as_ref() {
        Some(r) => alloc::format!(
            "RSN v{} group {} pairwise {} akm {} caps 0x{:04x} (MFPC {} MFPR {}){}",
            r.version, suite_name(r.group),
            r.pairwise.iter().map(|&x| suite_name(x)).collect::<Vec<_>>().join(","),
            r.akm.iter().map(|&x| akm_suite_name(x)).collect::<Vec<_>>().join(","),
            r.caps, (r.caps & RSN_CAP_MFPC != 0) as u8, (r.caps & RSN_CAP_MFPR != 0) as u8,
            if b.wpa1 { " +WPA1 element" } else { "" }),
        None if b.wpa1 => String::from("WPA1 element only, no RSN"),
        None if b.capability & wf::CAP_PRIVACY != 0 => String::from("privacy bit, no RSN/WPA element (WEP)"),
        None => String::from("open (no RSN element)"),
    }
}

/// Why `want` (`netctl_abi::akm`) cannot join `b` with the radio's `caps`,
/// as a sentence for logs and failure messages; `None` when it can. The
/// checks mirror [`select_akm`], most fundamental first, so the cause named
/// is the one to fix (or the feature still missing here).
pub fn join_cause(want: u8, b: &wf::Bss, caps: u32) -> Option<String> {
    let privacy = b.capability & wf::CAP_PRIVACY != 0;
    let nc = bss_to_nc(0, b, b.rssi, caps, 0);
    if select_akm(want, nc.ext, nc.rsn_flags, privacy, caps).is_ok() { return None; }
    let r = match b.rsn.as_ref() {
        Some(r) => r,
        None => {
            return Some(String::from(if want == akm::NONE {
                "the network is secured (WPA/WEP) but the connection has no passphrase"
            } else if b.wpa1 {
                "the network offers WPA (version 1) only, which is not supported (WPA2 or later is)"
            } else if privacy {
                "the network uses WEP, which is not supported"
            } else {
                "the network is open but the connection is set up with a passphrase"
            }));
        }
    };
    if want == akm::NONE {
        return Some(String::from("the network is WPA2/WPA3-secured but the connection has no passphrase"));
    }
    let has = |a: u32| r.akm.contains(&a);
    let psk = has(AKM_PSK) && caps & wifi_caps::PSK != 0;
    let sha = has(AKM_PSK_SHA256) && caps & wifi_caps::PSK_SHA256 != 0;
    let sae = (has(AKM_SAE) || has(AKM_SAE_EXT)) && caps & wifi_caps::SAE != 0;
    let mfpr = r.caps & RSN_CAP_MFPR != 0;
    if !psk && !sha && !sae {
        let what = if has(AKM_PSK_SHA256) {
            "offers only PSK-SHA256 key management (AKM 6), which is not supported yet"
        } else if has(AKM_FT_PSK) {
            "offers only FT-PSK key management (fast transition, AKM 4), which is not supported yet"
        } else if has(AKM_SAE) || has(AKM_SAE_EXT) || has(AKM_FT_SAE) || has(AKM_FT_SAE_EXT) {
            "is WPA3-Personal (SAE) only, which is not supported yet"
        } else if has(AKM_8021X) || has(AKM_FT_8021X) || has(AKM_8021X_SHA256) || has(AKM_SUITE_B) || has(AKM_SUITE_B_192) {
            "is WPA2/WPA3-Enterprise (802.1X), which is not supported"
        } else if has(AKM_OWE) {
            "uses OWE (Enhanced Open), which is not supported"
        } else {
            "offers no key management this interface can do"
        };
        return Some(alloc::format!("the network {}", what));
    }
    if mfpr && caps & wifi_caps::PMF == 0 {
        return Some(String::from("the network requires management frame protection (802.11w, MFPR set), which is not supported yet"));
    }
    if !r.pairwise.contains(&SUITE_CCMP) {
        return Some(alloc::format!("the network offers no CCMP pairwise cipher (pairwise {}); only CCMP is supported",
                                   r.pairwise.iter().map(|&x| suite_name(x)).collect::<Vec<_>>().join(",")));
    }
    if r.group != SUITE_CCMP && r.group != SUITE_TKIP {
        return Some(alloc::format!("the network uses the {} group cipher; only CCMP and TKIP (receive) are supported", suite_name(r.group)));
    }
    Some(String::from("the network's security settings are not supported"))
}

/// The cache record of a BSS.
pub fn bss_to_nc(ifindex: u32, b: &wf::Bss, rssi: i8, caps: u32, now: u64) -> NcBss {
    let mut n = NcBss::default();
    n.ifindex = ifindex;
    n.bssid = b.bssid;
    let bnd = if b.channel > 14 { band::GHZ_5 } else { band::GHZ_2_4 };
    n.band = bnd;
    n.channel = b.channel;
    n.freq_mhz = channel_to_freq(b.channel, bnd);
    n.signal_dbm = rssi as i16;
    n.quality = quality_from_dbm(rssi as i32);
    let sl = b.ssid.len().min(SSID_MAX);
    // A hidden SSID is sent as an empty or all-zero element.
    let hidden = sl == 0 || b.ssid.iter().all(|&c| c == 0);
    if !hidden {
        n.ssid_len = sl as u8;
        n.ssid[..sl].copy_from_slice(&b.ssid[..sl]);
    }
    let privacy = b.capability & wf::CAP_PRIVACY != 0;
    n.nm_flags = if privacy { nm_ap_flags::PRIVACY } else { nm_ap_flags::NONE };
    let mut ext = 0u32;
    if hidden { ext |= bss_ext::HIDDEN; }
    if b.ht { ext |= bss_ext::HT; }
    if b.vht { ext |= bss_ext::VHT; }
    if b.wpa1 {
        ext |= bss_ext::WPA1;
        // The WPA1 IE's suites are not kept by the parser; NM shows WPA1 as
        // PSK/TKIP in practice.
        n.wpa_flags = nm_sec::KEY_MGMT_PSK | nm_sec::PAIR_TKIP | nm_sec::GROUP_TKIP;
    }
    if let Some(r) = b.rsn.as_ref() {
        ext |= bss_ext::RSN;
        let mut f = group_nm(r.group);
        for &p in r.pairwise.iter() { f |= pair_nm(p); }
        let mut psk_family = false;
        let mut sae_family = false;
        for &a in r.akm.iter() {
            f |= akm_nm(a);
            ext |= akm_ext(a);
            psk_family |= matches!(a, AKM_PSK | AKM_PSK_SHA256 | AKM_FT_PSK);
            sae_family |= matches!(a, AKM_SAE | AKM_SAE_EXT | AKM_FT_SAE | AKM_FT_SAE_EXT);
        }
        if psk_family && sae_family { ext |= bss_ext::TRANSITION; }
        if r.caps & RSN_CAP_MFPC != 0 { ext |= bss_ext::MFPC; }
        if r.caps & RSN_CAP_MFPR != 0 { ext |= bss_ext::MFPR; }
        n.rsn_flags = f;
        n.rsn_caps = r.caps;
    }
    let joinable = select_akm(akm::AUTO, ext, n.rsn_flags, privacy, caps).is_ok()
        || select_akm(akm::NONE, ext, n.rsn_flags, privacy, caps).is_ok();
    if joinable { ext |= bss_ext::JOINABLE; }
    n.ext = ext;
    n.capability = b.capability;
    n.beacon_int = b.beacon_interval;
    let max_rate = b.rates.iter().map(|r| (r & 0x7f) as u32 * 500).max().unwrap_or(0);
    n.max_rate_kbps = if b.vht { max_rate.max(433_300) } else if b.ht { max_rate.max(72_200) } else { max_rate };
    n.last_seen_ms = now as u32;
    n
}

/// The kernel's legacy profile (wifi.conf / `wifi.ssid=`): its uuid is
/// derived from its SSID in the legacy domain.
fn is_legacy(p: &Profile) -> bool {
    p.uuid == netctl_abi::util::derive_uuid(netctl_abi::util::UUID_DOMAIN_LEGACY, p.ssid())
}

/// The PMK for WPA2-PSK: 64 hex digits are the PMK itself, anything else is
/// a passphrase (PBKDF2-SHA1, 4096 rounds over the SSID).
pub fn pmk_for(ssid: &[u8], psk: &[u8]) -> [u8; 32] {
    if psk.len() == 64 && psk.iter().all(|c| c.is_ascii_hexdigit()) {
        let mut pmk = [0u8; 32];
        let h = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
        for i in 0..32 { pmk[i] = (h(psk[2 * i]) << 4) | h(psk[2 * i + 1]); }
        return pmk;
    }
    crate::wifi_crypto::wpa_psk(psk, ssid)
}

// ── the manager ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Entry {
    hit: ScanHit,
    nc: NcBss,
    seen_ms: u64,
}

struct Backoff {
    uuid: [u8; 16],
    fails: u32,
    next_ms: u64,
}

/// A BSS that failed us (see `BSS_BACKOFF_MS`).
struct BssBackoff {
    bssid: [u8; 6],
    fails: u32,
    next_ms: u64,
}

struct St {
    ws: NcWifiState,
    bss: Vec<Entry>,
    last_scan: Option<NcScanInfo>,
    last_scan_ms: u64,
    scan_gen: u32,
    radio_on: bool,
    caps: u32,
    age_out_ms: u64,
    auto_blocked: bool,
    wrong_key: Vec<[u8; 16]>,
    backoff: Vec<Backoff>,
    bss_backoff: Vec<BssBackoff>,
    /// (profile, `now_ms` of its last successful connection this boot).
    used: Vec<([u8; 16], u64)>,
    /// BSSs whose RSN details were logged as not joinable, with the `ext`
    /// they were logged with (logged again only when that changes).
    rejected_logged: Vec<([u8; 6], u32)>,
    /// (BSS, `now_ms`) of the last short-lived link per BSS (`FLAP_MS`).
    flaps: Vec<([u8; 6], u64)>,
    /// Profiles used while netctl never published any (generation 0):
    /// the legacy wifi.conf / `wifi.ssid=` network.
    fallback: Vec<Profile>,
    /// Autoconnect may run (not before the radio is up).
    next_auto_ms: u64,
    next_attempt: u32,
    last_signal: (u64, i16),
}

/// One Wi-Fi interface's manager; a `static` in its driver.
pub struct Mgr {
    name: &'static str,
    prefix: &'static str,
    mac_fn: fn() -> Option<[u8; 6]>,
    ifindex: AtomicU32,
    cmds: Mutex<VecDeque<WifiCmd>>,
    /// The radio thread's wake-up (`set_waker`): a queued command ends a
    /// long park instead of waiting for it.
    waker: AtomicUsize,
    st: Mutex<St>,
    events: Mutex<(u64, VecDeque<(u64, u64, WifiEvent)>)>,
    ifname: Mutex<[u8; 16]>,
}

impl Mgr {
    pub const fn new(name: &'static str, prefix: &'static str, mac_fn: fn() -> Option<[u8; 6]>) -> Mgr {
        Mgr {
            name, prefix, mac_fn,
            ifindex: AtomicU32::new(0),
            cmds: Mutex::new(VecDeque::new()),
            waker: AtomicUsize::new(0),
            st: Mutex::new(St {
                ws: NcWifiState {
                    ifindex: 0, state: wifi_state::UNAVAILABLE, reason: reason::NONE, akm: akm::AUTO, pmf: 0,
                    ieee_status: 0, ieee_reason: 0, attempt: 0, profile_uuid: [0; 16], bssid: [0; 6], freq_mhz: 0,
                    signal_dbm: 0, ssid_len: 0, _pad: 0, ssid: [0; SSID_MAX], tx_rate_kbps: 0, since_ms: 0, flags: 0,
                    _rsv: [0; 8],
                },
                bss: Vec::new(), last_scan: None, last_scan_ms: 0, scan_gen: 0, radio_on: true, caps: 0,
                age_out_ms: BSS_AGE_OUT_MS as u64, auto_blocked: false, wrong_key: Vec::new(), backoff: Vec::new(),
                bss_backoff: Vec::new(), used: Vec::new(), rejected_logged: Vec::new(), flaps: Vec::new(),
                fallback: Vec::new(), next_auto_ms: 0, next_attempt: 1, last_signal: (0, 0),
            }),
            events: Mutex::new((0, VecDeque::new())),
            ifname: Mutex::new(*b"wifi0\0\0\0\0\0\0\0\0\0\0\0"),
        }
    }

    fn log(&self, msg: &str) {
        let mut s = String::with_capacity(msg.len() + 16);
        s.push('[');
        s.push_str(self.prefix);
        s.push_str("] ");
        s.push_str(msg);
        s.push('\n');
        // SAFETY: the kernel's raw serial writer takes a pointer and length.
        unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
    }

    /// The ifindex events are tagged with (0 before [`Mgr::attach`]).
    pub fn ifindex(&self) -> u32 { self.ifindex.load(Ordering::Relaxed) }

    /// Register with the netctl seam under `ifindex` (interface `name`).
    pub fn attach(&'static self, ifindex: IfIndex, name: &str) -> bool {
        self.ifindex.store(ifindex, Ordering::SeqCst);
        {
            let mut n = self.ifname.lock();
            *n = [0; 16];
            let b = name.as_bytes();
            let l = b.len().min(15);
            n[..l].copy_from_slice(&b[..l]);
        }
        self.st.lock().ws.ifindex = ifindex;
        let ok = crate::wifi_api::register(ifindex, self);
        self.log(&alloc::format!("{} attached to the netctl seam as ifindex {} ({})", name, ifindex,
                                 if ok { "ok" } else { "REFUSED: ifindex taken or table full" }));
        ok
    }

    pub fn set_caps(&self, caps: u32) { self.st.lock().caps = caps; }

    /// Override the BSS age-out (self-tests only; the ABI value otherwise).
    pub fn set_age_out_ms(&self, ms: u64) { self.st.lock().age_out_ms = ms; }

    /// The legacy network to autoconnect to while netctl published nothing.
    pub fn set_fallback_profiles(&self, list: Vec<Profile>) { self.st.lock().fallback = list; }

    // ── events ──

    fn emit(&self, ev: WifiEvent) {
        let t = now_ms();
        {
            let mut e = self.events.lock();
            e.0 += 1;
            let seq = e.0;
            if e.1.len() >= EVENT_RING { e.1.pop_front(); }
            e.1.push_back((seq, t, ev));
        }
        crate::wifi_api::emit(self.ifindex(), ev);
    }

    /// Sequence number of the newest event (for [`Mgr::events_since`]).
    pub fn event_seq(&self) -> u64 { self.events.lock().0 }

    /// Events after `seq` still in the ring: (seq, time ms, event).
    pub fn events_since(&self, seq: u64) -> Vec<(u64, u64, WifiEvent)> {
        self.events.lock().1.iter().filter(|e| e.0 > seq).copied().collect()
    }

    fn publish(&self) {
        let ws = self.st.lock().ws;
        self.emit(WifiEvent::State(ws));
    }

    /// Change the state (and reason); clears link details when leaving a
    /// connection. Emits WIFI_STATE.
    fn set_state(&self, state: u8, why: u8) {
        {
            let mut st = self.st.lock();
            st.ws.state = state;
            st.ws.reason = why;
            st.ws.since_ms = now_ms() as u32;
            if matches!(state, wifi_state::DISCONNECTED | wifi_state::RADIO_OFF | wifi_state::UNAVAILABLE) {
                st.ws.akm = akm::AUTO;
                st.ws.pmf = 0;
                st.ws.attempt = 0;
                st.ws.tx_rate_kbps = 0;
                st.ws.flags &= !(wifi_flags::AUTO | wifi_flags::PMF_ACTIVE);
                if state != wifi_state::DISCONNECTED || why == reason::NONE {
                    st.ws.ieee_status = 0;
                    st.ws.ieee_reason = 0;
                }
                // Keep ssid/bssid of the failed or left network on the
                // DISCONNECTED that follows (clients show "failed to join X").
            }
            if st.auto_blocked { st.ws.flags |= wifi_flags::AUTOCONNECT_BLOCKED; } else { st.ws.flags &= !wifi_flags::AUTOCONNECT_BLOCKED; }
        }
        self.publish();
    }

    /// Re-derive AUTOCONNECT_BLOCKED and publish when it changed.
    fn refresh_flags(&self) {
        let changed = {
            let mut st = self.st.lock();
            let f = if st.auto_blocked { st.ws.flags | wifi_flags::AUTOCONNECT_BLOCKED } else { st.ws.flags & !wifi_flags::AUTOCONNECT_BLOCKED };
            let c = f != st.ws.flags;
            st.ws.flags = f;
            c
        };
        if changed { self.publish(); }
    }

    fn set_step(&self, state: u8) {
        if self.st.lock().ws.state == state { return; }
        self.set_state(state, reason::NONE);
    }

    fn set_scanning(&self, on: bool) {
        let changed = {
            let mut st = self.st.lock();
            let was = st.ws.flags & wifi_flags::SCANNING != 0;
            if on { st.ws.flags |= wifi_flags::SCANNING; } else { st.ws.flags &= !wifi_flags::SCANNING; }
            was != on
        };
        if changed { self.publish(); }
    }

    // ── commands ──

    /// Called after every queued command (the radio thread may be parked).
    pub fn set_waker(&self, f: fn()) { self.waker.store(f as usize, Ordering::Release); }

    fn pop_cmd(&self) -> Option<WifiCmd> { self.cmds.lock().pop_front() }

    /// A queued command that ends a connection attempt.
    fn preempt_pending(&self) -> bool {
        self.cmds.lock().iter().any(|c| matches!(c, WifiCmd::Connect(_) | WifiCmd::Disconnect { .. } | WifiCmd::SetRadio(false)))
    }

    fn state_now(&self) -> u8 { self.st.lock().ws.state }

    /// Status line for /proc/net/nic.
    pub fn info_lines(&self, out: &mut String) {
        use core::fmt::Write;
        let st = self.st.lock();
        let _ = writeln!(out, "manager: on (netctl seam, ifindex {})", self.ifindex());
        let _ = writeln!(out, "wifi_state: {}{}{}", state_name(st.ws.state),
                         if st.ws.reason != reason::NONE { alloc::format!(" (reason {})", reason_name(st.ws.reason)) } else { String::new() },
                         if st.auto_blocked { ", autoconnect blocked" } else { "" });
        if st.ws.ssid_len > 0 && st.ws.state != wifi_state::DISCONNECTED {
            let _ = writeln!(out, "wifi_target: '{}' {}", wf::ssid_display(st.ws.ssid()), fmt_mac(&st.ws.bssid));
        }
        let _ = writeln!(out, "wifi_bss_cached: {} scan_gen: {}", st.bss.len(), st.scan_gen);
    }

    /// The BSS cache as the /proc/net/wifi table.
    pub fn scan_lines(&self, title: &str, out: &mut String) {
        use core::fmt::Write;
        let st = self.st.lock();
        let now = now_ms();
        let _ = writeln!(out, "# {} — {}; last scan {} s ago; {} BSSs cached", title, state_name(st.ws.state),
                         if st.last_scan_ms == 0 { 0 } else { (now - st.last_scan_ms) / 1000 }, st.bss.len());
        let _ = writeln!(out, "{:<17} {:>4} {:>6}  {:<14} {:<4} {:>5} {}", "BSSID", "CH", "SIGNAL", "SECURITY", "JOIN", "AGE", "SSID");
        let mut v: Vec<&Entry> = st.bss.iter().collect();
        v.sort_by(|a, b| b.hit.rssi.cmp(&a.hit.rssi));
        for e in v {
            let _ = writeln!(out, "{:<17} {:>4} {:>6}  {:<14} {:<4} {:>4}s {}", fmt_mac(&e.nc.bssid), e.nc.channel, e.hit.rssi,
                             wf::security_summary(&e.hit.bss), if e.nc.ext & bss_ext::JOINABLE != 0 { "yes" } else { "no" },
                             (now - e.seen_ms) / 1000, wf::ssid_display(&e.hit.bss.ssid));
        }
        drop(st);
        gk::lines(out);
    }

    // ── state machine pieces used by `run` ──

    fn new_attempt_id(&self) -> u32 {
        let mut st = self.st.lock();
        let id = st.next_attempt;
        st.next_attempt = st.next_attempt.wrapping_add(1).max(1) & !AUTO_ATTEMPT_BIT;
        id | AUTO_ATTEMPT_BIT
    }

    /// Merge scan results, age the cache, emit BSS / BSS_GONE / SCAN_DONE.
    fn finish_scan(&self, hits: Vec<ScanHit>, flags: u8, keep: Option<[u8; 6]>) -> NcScanInfo {
        let now = now_ms();
        let ix = self.ifindex();
        let mut evs: Vec<WifiEvent> = Vec::new();
        let mut rejected: Vec<String> = Vec::new();
        let info = {
            let mut st = self.st.lock();
            let caps = st.caps;
            for h in hits {
                let mut nc = bss_to_nc(ix, &h.bss, h.rssi, caps, now);
                // A secured BSS we cannot join: say exactly why, once.
                let secured = nc.ext & (bss_ext::RSN | bss_ext::WPA1) != 0 || nc.nm_flags & nm_ap_flags::PRIVACY != 0;
                if secured && nc.ext & bss_ext::JOINABLE == 0
                    && !st.rejected_logged.iter().any(|(b, x)| *b == h.bss.bssid && *x == nc.ext)
                {
                    st.rejected_logged.retain(|(b, _)| *b != h.bss.bssid);
                    if st.rejected_logged.len() < 2 * netctl_abi::MAX_BSS { st.rejected_logged.push((h.bss.bssid, nc.ext)); }
                    rejected.push(alloc::format!("not joinable: '{}' {} ch {}: {}: {}", wf::ssid_display(&h.bss.ssid),
                        fmt_mac(&h.bss.bssid), h.bss.channel, rsn_details(&h.bss),
                        join_cause(akm::AUTO, &h.bss, caps).unwrap_or_else(|| String::from("?"))));
                }
                match st.bss.iter_mut().find(|e| e.nc.bssid == h.bss.bssid) {
                    Some(e) => {
                        // Keep an SSID a directed probe revealed for a hidden network.
                        let mut hit = h;
                        if nc.ssid_len == 0 && e.nc.ssid_len > 0 {
                            nc.ssid = e.nc.ssid;
                            nc.ssid_len = e.nc.ssid_len;
                            hit.bss.ssid = e.hit.bss.ssid.clone();
                        }
                        let changed = nc.ssid != e.nc.ssid || nc.ext != e.nc.ext || nc.rsn_flags != e.nc.rsn_flags
                            || nc.channel != e.nc.channel || (nc.signal_dbm - e.nc.signal_dbm).abs() >= 3;
                        e.hit = hit;
                        e.nc = nc;
                        e.seen_ms = now;
                        if changed { evs.push(WifiEvent::Bss(nc)); }
                    }
                    None => {
                        if st.bss.len() < netctl_abi::MAX_BSS {
                            st.bss.push(Entry { hit: h, nc, seen_ms: now });
                            evs.push(WifiEvent::Bss(nc));
                        }
                    }
                }
            }
            let age = st.age_out_ms;
            let mut i = 0;
            while i < st.bss.len() {
                let e = &st.bss[i];
                if Some(e.nc.bssid) != keep && now - e.seen_ms > age {
                    let mut gone = e.nc;
                    gone.age_ms = (now - e.seen_ms) as u32;
                    evs.push(WifiEvent::BssGone(gone));
                    st.bss.remove(i);
                } else {
                    i += 1;
                }
            }
            st.scan_gen = st.scan_gen.wrapping_add(1);
            st.last_scan_ms = now;
            let mut info = NcScanInfo::default();
            info.ifindex = ix;
            info.status = scan_status::OK;
            info.flags = flags;
            info.n_bss = st.bss.len() as u32;
            info.scan_gen = st.scan_gen;
            info.time_ms = now;
            st.last_scan = Some(info);
            info
        };
        for r in rejected { self.log(&r); }
        for e in evs { self.emit(e); }
        self.emit(WifiEvent::ScanDone(info));
        info
    }

    fn coalesced_done(&self, flags: u8) {
        let info = {
            let mut st = self.st.lock();
            let mut info = st.last_scan.unwrap_or_default();
            info.ifindex = self.ifindex();
            info.status = scan_status::OK;
            info.flags = flags | scan_flags::COALESCED;
            info.n_bss = st.bss.len() as u32;
            info.scan_gen = st.scan_gen;
            info.time_ms = now_ms();
            st.last_scan = Some(info);
            info
        };
        self.emit(WifiEvent::ScanDone(info));
    }

    fn failed_scan(&self, flags: u8, status: u8) {
        let mut info = NcScanInfo::default();
        info.ifindex = self.ifindex();
        info.status = status;
        info.flags = flags;
        {
            let st = self.st.lock();
            info.n_bss = st.bss.len() as u32;
            info.scan_gen = st.scan_gen;
        }
        info.time_ms = now_ms();
        self.emit(WifiEvent::ScanDone(info));
    }

    /// Drop the cache (radio off): BSS_GONE for each.
    fn flush_bss(&self) {
        let gone: Vec<NcBss> = {
            let mut st = self.st.lock();
            let v = st.bss.iter().map(|e| e.nc).collect();
            st.bss.clear();
            v
        };
        for g in gone { self.emit(WifiEvent::BssGone(g)); }
    }

    /// The current profile list: netctl's, or the fallback before netctl
    /// published one.
    fn profiles(&self) -> Vec<Profile> {
        let (gen, list) = crate::wifi_api::with_profiles(|g, l| (g, if g == 0 { Vec::new() } else { l.to_vec() }));
        if gen == 0 { self.st.lock().fallback.clone() } else { list }
    }

    /// The best autoconnect candidate in the cache: (profile, bssid, akm).
    fn pick_auto(&self) -> Option<(Profile, [u8; 6], u8)> {
        let profiles = self.profiles();
        let now = now_ms();
        let ifname = *self.ifname.lock();
        let st = self.st.lock();
        // Candidates: (profile, BSS, AKM, auto_key inputs), then the strongest
        // 2.4 GHz one (the 5 GHz preference is relative to it), then the key.
        let mut cands: Vec<(usize, [u8; 6], u8, i32, u64, u64, bool, u8, i16)> = Vec::new();
        for (pi, p) in profiles.iter().enumerate() {
            if p.kind != if_kind::WIFI || !p.autoconnect { continue; }
            let pif = p.iface();
            if !pif.is_empty() && pif != netctl_abi::util::cstr(&ifname) { continue; }
            if st.wrong_key.contains(&p.uuid) { continue; }
            if st.backoff.iter().any(|b| b.uuid == p.uuid && now < b.next_ms) { continue; }
            if p.akm != akm::NONE && p.secret().is_empty() { continue; }
            // A connection this boot beats any stored timestamp (seconds of
            // an earlier boot); the newest wins either way.
            let (boot_ms, stored_s) = match st.used.iter().find(|u| u.0 == p.uuid) {
                Some(&(_, t)) => (t.max(1), 0),
                None => (0, p.timestamp_s),
            };
            let legacy = is_legacy(p);
            for e in st.bss.iter() {
                if e.hit.bss.ssid != p.ssid() { continue; }
                if now - e.seen_ms > AUTO_CACHE_MS.max(IDLE_SCAN_MS + 5_000) { continue; }
                if st.bss_backoff.iter().any(|b| b.bssid == e.nc.bssid && now < b.next_ms) { continue; }
                let a = match select_akm(p.akm, e.nc.ext, e.nc.rsn_flags, e.nc.nm_flags & nm_ap_flags::PRIVACY != 0, st.caps) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                cands.push((pi, e.nc.bssid, a, p.priority, boot_ms, stored_s, legacy, e.nc.channel, e.nc.signal_dbm));
            }
        }
        let best24 = cands.iter().filter(|c| c.7 <= 14).map(|c| c.8).max();
        let mut best: Option<((i64, u64, bool, u64, bool, i16), usize, [u8; 6], u8)> = None;
        for &(pi, bssid, a, prio, boot_ms, stored_s, legacy, ch, sig) in cands.iter() {
            let k = auto_key(prio, boot_ms, stored_s, legacy, ch, sig, best24);
            if best.as_ref().map_or(true, |b| k > b.0) { best = Some((k, pi, bssid, a)); }
        }
        best.map(|b| (profiles[b.1].clone(), b.2, b.3))
    }

    /// How long ago (ms) profile `uuid` last connected this boot.
    fn used_ago_ms(&self, uuid: &[u8; 16]) -> Option<u64> {
        self.st.lock().used.iter().find(|u| u.0 == *uuid).map(|u| now_ms().saturating_sub(u.1))
    }

    fn has_auto_profiles(&self) -> bool {
        let st_blocked = self.st.lock().auto_blocked;
        !st_blocked && self.profiles().iter().any(|p| p.kind == if_kind::WIFI && p.autoconnect)
    }

    /// One failure of the access point `bssid` (see `BSS_BACKOFF_MS`).
    fn note_bss_failure(&self, bssid: [u8; 6]) {
        let now = now_ms();
        let (fails, ms) = {
            let mut st = self.st.lock();
            match st.bss_backoff.iter_mut().find(|b| b.bssid == bssid) {
                Some(b) => {
                    b.fails += 1;
                    let ms = BSS_BACKOFF_MS[(b.fails as usize - 1).min(BSS_BACKOFF_MS.len() - 1)];
                    b.next_ms = now + ms;
                    (b.fails, ms)
                }
                None => {
                    if st.bss_backoff.len() >= netctl_abi::MAX_BSS { st.bss_backoff.remove(0); }
                    st.bss_backoff.push(BssBackoff { bssid, fails: 1, next_ms: now + BSS_BACKOFF_MS[0] });
                    (1, BSS_BACKOFF_MS[0])
                }
            }
        };
        self.log(&alloc::format!("{}: failure {} in a row, autoconnect skips it for {} s", fmt_mac(&bssid), fails, ms / 1000));
    }

    /// The AP ended a link soon after accepting us (see `FLAP_MS`).
    fn note_bss_flap(&self, bssid: [u8; 6]) {
        let now = now_ms();
        let again = {
            let mut st = self.st.lock();
            let again = st.flaps.iter().any(|f| f.0 == bssid && now - f.1 < FLAP_WINDOW_MS);
            st.flaps.retain(|f| f.0 != bssid && now - f.1 < FLAP_WINDOW_MS);
            if !again { st.flaps.push((bssid, now)); }
            again
        };
        if again { self.note_bss_failure(bssid); }
    }

    fn note_failure(&self, uuid: &[u8; 16], bssid: Option<[u8; 6]>, f: &Fail) {
        if f.reason == reason::SUPERSEDED { return; }
        // A failure of that access point rather than of the network: back the
        // BSS off, so autoconnect moves on (another BSS of the same network
        // included) instead of hammering it.
        let bss_fault = matches!(f.reason, reason::AUTH_REJECTED | reason::ASSOC_REJECTED | reason::DEAUTH_BY_AP
                                         | reason::TIMEOUT | reason::BEACON_LOSS);
        if let (true, Some(b)) = (bss_fault, bssid) {
            self.note_bss_failure(b);
        }
        if uuid_is_nil(uuid) { return; }
        let mut st = self.st.lock();
        if f.reason == reason::WRONG_KEY || f.reason == reason::NO_SECRET {
            if !st.wrong_key.contains(uuid) { st.wrong_key.push(*uuid); }
            return;
        }
        if bss_fault && bssid.is_some() { return; }
        let now = now_ms();
        match st.backoff.iter_mut().find(|b| b.uuid == *uuid) {
            Some(b) => {
                b.fails += 1;
                b.next_ms = now + BACKOFF_MS[(b.fails as usize - 1).min(BACKOFF_MS.len() - 1)];
            }
            None => st.backoff.push(Backoff { uuid: *uuid, fails: 1, next_ms: now + BACKOFF_MS[0] }),
        }
    }

    fn note_success(&self, uuid: &[u8; 16], bssid: [u8; 6]) {
        let now = now_ms();
        let mut st = self.st.lock();
        st.backoff.retain(|b| b.uuid != *uuid);
        st.bss_backoff.retain(|b| b.bssid != bssid);
        if !uuid_is_nil(uuid) {
            st.used.retain(|u| u.0 != *uuid);
            if st.used.len() >= netctl_abi::MAX_PROFILES { st.used.remove(0); }
            st.used.push((*uuid, now));
        }
    }
}

impl WifiBackend for Mgr {
    fn driver(&self) -> &'static str { self.name }
    fn caps(&self) -> u32 { self.st.lock().caps }
    fn mac(&self) -> Option<[u8; 6]> { (self.mac_fn)() }

    fn submit(&self, cmd: WifiCmd) -> Result<(), i32> {
        use netctl_abi::errno;
        let (state, radio, caps) = { let st = self.st.lock(); (st.ws.state, st.radio_on, st.caps) };
        match &cmd {
            WifiCmd::Scan { .. } | WifiCmd::Connect(_) if state == wifi_state::UNAVAILABLE => return Err(errno::ENETDOWN),
            WifiCmd::Scan { .. } | WifiCmd::Connect(_) if !radio || state == wifi_state::RADIO_OFF => return Err(errno::ENETDOWN),
            WifiCmd::Connect(p) => {
                if p.ssid.len == 0 || p.ssid.len as usize > SSID_MAX { return Err(errno::EINVAL); }
                if p.akm > akm::MAX { return Err(errno::EINVAL); }
                if p.akm == akm::SAE && caps & wifi_caps::SAE == 0 { return Err(errno::EOPNOTSUPP); }
                if p.akm == akm::PSK_SHA256 && caps & wifi_caps::PSK_SHA256 == 0 { return Err(errno::EOPNOTSUPP); }
                if !netctl_abi::util::secret_valid_for(p.akm, p.secret.as_bytes()) { return Err(errno::EINVAL); }
            }
            _ => {}
        }
        let mut q = self.cmds.lock();
        if let WifiCmd::Scan { ssids, force } = &cmd {
            // One queued scan is enough: merge the probes into it.
            if let Some(WifiCmd::Scan { ssids: qs, force: qf }) = q.iter_mut().find(|c| matches!(c, WifiCmd::Scan { .. })) {
                for s in ssids.iter() {
                    if qs.len() < netctl_abi::SCAN_MAX_SSIDS && !qs.iter().any(|x| x.as_slice() == s.as_slice()) { qs.push(*s); }
                }
                *qf |= *force;
                return Ok(());
            }
        }
        if q.len() >= CMD_QUEUE_MAX { return Err(11 /* EAGAIN */); }
        q.push_back(cmd);
        drop(q);
        let w = self.waker.load(Ordering::Acquire);
        if w != 0 {
            // SAFETY: only `set_waker` stores here, and it stores a `fn()`.
            let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(w) };
            f();
        }
        Ok(())
    }

    fn state(&self) -> NcWifiState {
        let mut ws = self.st.lock().ws;
        ws.ifindex = self.ifindex();
        ws
    }

    fn bss_snapshot(&self, out: &mut Vec<NcBss>) {
        let now = now_ms();
        let st = self.st.lock();
        for e in st.bss.iter() {
            let mut n = e.nc;
            n.ifindex = self.ifindex();
            n.age_ms = (now - e.seen_ms) as u32;
            out.push(n);
        }
    }

    fn last_scan(&self) -> Option<NcScanInfo> { self.st.lock().last_scan }
}

pub fn state_name(s: u8) -> &'static str {
    match s {
        wifi_state::UNAVAILABLE => "unavailable", wifi_state::RADIO_OFF => "radio-off",
        wifi_state::DISCONNECTED => "disconnected", wifi_state::CONNECTING => "connecting",
        wifi_state::AUTHENTICATING => "authenticating", wifi_state::ASSOCIATING => "associating",
        wifi_state::HANDSHAKE => "handshake", wifi_state::CONNECTED => "connected",
        wifi_state::DISCONNECTING => "disconnecting", wifi_state::FAILED => "failed", _ => "?",
    }
}

pub fn reason_name(r: u8) -> &'static str {
    match r {
        reason::NONE => "none", reason::WRONG_KEY => "wrong-key", reason::NO_SECRET => "no-secret",
        reason::NO_AP => "no-ap", reason::AUTH_REJECTED => "auth-rejected", reason::ASSOC_REJECTED => "assoc-rejected",
        reason::TIMEOUT => "timeout", reason::FW_ERROR => "fw-error", reason::UNSUPPORTED_SECURITY => "unsupported-security",
        reason::DEAUTH_BY_AP => "deauth-by-ap", reason::BEACON_LOSS => "beacon-loss", reason::USER => "user",
        reason::RADIO_OFF => "radio-off", reason::RFKILL => "rfkill", reason::SUPERSEDED => "superseded", _ => "?",
    }
}

pub fn fmt_mac(m: &[u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

// ── the loop ────────────────────────────────────────────────────────────────

/// What ended a connected session.
enum After {
    /// Back to the top of the loop (state already reported).
    Idle,
    /// The device is gone for good.
    Dead,
    /// A new connection request replaced the session.
    Next(Req),
}

/// Run an attempt and every attempt that supersedes it.
fn drive<R: Radio>(mgr: &Mgr, r: &mut R, req: Req) -> After {
    let mut a = attempt(mgr, r, req);
    while let After::Next(req) = a { a = attempt(mgr, r, req); }
    a
}

/// The connection request the loop works on.
struct Req {
    attempt: u32,
    uuid: [u8; 16],
    ssid: Vec<u8>,
    bssid: Option<[u8; 6]>,
    akm: u8,
    hidden: bool,
    secret: Secret,
    auto: bool,
}

impl Req {
    fn from_params(p: ConnectParams) -> Req {
        Req { attempt: p.attempt, uuid: p.profile_uuid, ssid: p.ssid.as_slice().to_vec(), bssid: p.bssid, akm: p.akm,
              hidden: p.hidden, secret: p.secret, auto: false }
    }
}

/// The driver thread's loop once the radio is up (never returns while the
/// device lives). `mgr` must be the radio's own manager.
pub fn run<R: Radio>(mgr: &'static Mgr, r: &mut R) {
    mgr.set_caps(r.caps());
    {
        let mut st = mgr.st.lock();
        st.next_auto_ms = 0;
    }
    mgr.set_state(wifi_state::DISCONNECTED, reason::NONE);
    mgr.log("manager running: commands from /dev/netctl, autoconnect from profiles");
    // Kick off the first scan at once (the applet wants a list; autoconnect
    // wants candidates).
    let mut want_scan = true;
    let mut rfkill = false;
    loop {
        // Hardware kill switch.
        let hw = r.hw_rfkill();
        if hw != rfkill {
            rfkill = hw;
            if hw {
                mgr.log("hardware RF-kill engaged");
                mgr.flush_bss();
                mgr.set_state(wifi_state::RADIO_OFF, reason::RFKILL);
            } else {
                mgr.log("hardware RF-kill released");
                if !r.rfkill_released() { mgr.set_state(wifi_state::UNAVAILABLE, reason::FW_ERROR); return; }
                if mgr.st.lock().radio_on { mgr.set_state(wifi_state::DISCONNECTED, reason::NONE); want_scan = true; }
            }
        }
        // Commands.
        if let Some(cmd) = mgr.pop_cmd() {
            match cmd {
                WifiCmd::Scan { ssids, force } => {
                    let ss: Vec<Vec<u8>> = ssids.iter().map(|s| s.as_slice().to_vec()).collect();
                    if !rfkill && mgr.st.lock().radio_on { do_scan(mgr, r, &ss, force, false, None); }
                    else { mgr.failed_scan(0, scan_status::ABORTED); }
                }
                WifiCmd::Connect(p) => {
                    {
                        let mut st = mgr.st.lock();
                        st.auto_blocked = false;
                        st.wrong_key.retain(|u| *u != p.profile_uuid);
                        st.backoff.retain(|b| b.uuid != p.profile_uuid);
                    }
                    if rfkill || !mgr.st.lock().radio_on { continue; }
                    match drive(mgr, r, Req::from_params(p)) { After::Dead => return, _ => {} }
                }
                WifiCmd::Disconnect { block_autoconnect } => {
                    mgr.st.lock().auto_blocked = block_autoconnect;
                    // Already disconnected: confirm (nmrs waits for the state).
                    if mgr.state_now() == wifi_state::DISCONNECTED { mgr.set_state(wifi_state::DISCONNECTED, reason::USER); }
                }
                WifiCmd::SetRadio(on) => {
                    let was = mgr.st.lock().radio_on;
                    mgr.st.lock().radio_on = on;
                    if on != was {
                        if !on {
                            r.set_radio(false);
                            mgr.flush_bss();
                            mgr.set_state(wifi_state::RADIO_OFF, reason::RADIO_OFF);
                        } else {
                            if !r.set_radio(true) { mgr.set_state(wifi_state::UNAVAILABLE, reason::FW_ERROR); return; }
                            if !rfkill { mgr.set_state(wifi_state::DISCONNECTED, reason::NONE); }
                            mgr.st.lock().next_auto_ms = 0;
                            want_scan = true;
                        }
                    }
                }
                WifiCmd::ProfilesChanged => {
                    {
                        let mut st = mgr.st.lock();
                        st.wrong_key.clear();
                        st.backoff.clear();
                        st.auto_blocked = false;
                        st.next_auto_ms = 0;
                    }
                    mgr.refresh_flags();
                }
            }
            continue;
        }
        let (radio_on, last_scan_ms, next_auto, blocked) = {
            let st = mgr.st.lock();
            (st.radio_on, st.last_scan_ms, st.next_auto_ms, st.auto_blocked)
        };
        if !radio_on || rfkill { r.idle(50); continue; }
        let now = now_ms();
        // Periodic background scan.
        if want_scan || now - last_scan_ms >= IDLE_SCAN_MS {
            want_scan = false;
            do_scan(mgr, r, &hidden_ssids(mgr), true, true, None);
            mgr.st.lock().next_auto_ms = 0;
            continue;
        }
        // Autoconnect.
        if !blocked && now >= next_auto {
            mgr.st.lock().next_auto_ms = now + 1_000;
            if let Some((p, bssid, _a)) = mgr.pick_auto() {
                let req = Req {
                    attempt: mgr.new_attempt_id(), uuid: p.uuid, ssid: p.ssid().to_vec(), bssid: Some(bssid),
                    akm: p.akm, hidden: p.hidden(), secret: Secret::new(p.secret()).unwrap_or(Secret::empty()), auto: true,
                };
                let why = match mgr.used_ago_ms(&p.uuid) {
                    Some(ms) => alloc::format!("last connected {} s ago", ms / 1000),
                    None if p.timestamp_s != 0 => alloc::format!("stored timestamp {}", p.timestamp_s),
                    None if is_legacy(&p) => String::from("the legacy network"),
                    None => String::from("not connected before"),
                };
                mgr.log(&alloc::format!("autoconnect: '{}' (priority {}) via {} ({})", wf::ssid_display(p.ssid()), p.priority,
                                        fmt_mac(&bssid), why));
                match drive(mgr, r, req) { After::Dead => return, _ => {} }
                continue;
            }
        }
        r.idle(50);
    }
}

/// Directed probes for background scans: the SSIDs of the autoconnect
/// profiles, hidden ones first, then by priority (the pre-manager driver
/// always probed for its configured network, which a hidden one needs).
fn hidden_ssids(mgr: &Mgr) -> Vec<Vec<u8>> {
    let mut v: Vec<(bool, i32, Vec<u8>)> = mgr.profiles().iter().filter(|p| p.autoconnect && p.kind == if_kind::WIFI)
        .map(|p| (!p.hidden(), -p.priority, p.ssid().to_vec())).collect();
    v.sort();
    v.dedup_by(|a, b| a.2 == b.2);
    v.into_iter().take(netctl_abi::SCAN_MAX_SSIDS).map(|x| x.2).collect()
}

/// One scan request: coalesce, or run it and merge.
fn do_scan<R: Radio>(mgr: &Mgr, r: &mut R, ssids: &[Vec<u8>], force: bool, background: bool, connected_bssid: Option<[u8; 6]>) {
    let mut flags = 0u8;
    if background { flags |= scan_flags::BACKGROUND; }
    if !ssids.is_empty() { flags |= scan_flags::DIRECTED; }
    let (since, have) = { let st = mgr.st.lock(); (now_ms() - st.last_scan_ms, st.last_scan.is_some()) };
    if have && !background && (since < FORCE_MIN_MS || (!force && since < SCAN_COALESCE_MS as u64)) {
        mgr.coalesced_done(flags);
        return;
    }
    let connected = connected_bssid.is_some();
    if connected && r.caps() & wifi_caps::SCAN_CONNECTED == 0 {
        // The firmware cannot scan while associated (or it is not enabled):
        // answer from the cache rather than failing the request.
        mgr.coalesced_done(flags);
        return;
    }
    let mut info = NcScanInfo::default();
    info.ifindex = mgr.ifindex();
    info.flags = flags;
    info.time_ms = now_ms();
    mgr.emit(WifiEvent::ScanStarted(info));
    mgr.set_scanning(true);
    let res = r.scan(ssids, connected);
    mgr.set_scanning(false);
    match res {
        Ok(hits) => { mgr.finish_scan(hits, flags, connected_bssid); }
        Err(f) => {
            mgr.log(&alloc::format!("scan failed: {}", f.msg));
            mgr.failed_scan(flags, scan_status::FAILED);
            if f.fw_assert && !connected {
                if !r.settle(Some(&f)) { mgr.set_state(wifi_state::UNAVAILABLE, reason::FW_ERROR); }
            }
        }
    }
}

/// FAILED(f), then DISCONNECTED(f), then the slow clean-up.
fn fail_attempt<R: Radio>(mgr: &Mgr, r: &mut R, req: &Req, f: &Fail, touched_radio: bool) -> After {
    mgr.log(&alloc::format!("connection to '{}' failed: {} ({})", wf::ssid_display(&req.ssid), reason_name(f.reason), f.msg));
    {
        let mut st = mgr.st.lock();
        st.ws.ieee_status = f.ieee_status;
        st.ws.ieee_reason = f.ieee_reason;
    }
    let bssid = if touched_radio { Some(mgr.st.lock().ws.bssid) } else { None };
    mgr.note_failure(&req.uuid, bssid, f);
    if touched_radio { r.leave(true); }
    mgr.set_state(wifi_state::FAILED, f.reason);
    if !req.auto && f.reason != reason::SUPERSEDED {
        // A failed user request: autoconnect elsewhere waits a moment, so the
        // failure stays on the device (Device.StateReason, which clients read
        // after the connection dies) instead of being replaced at once by the
        // next attempt's CONNECTING.
        let mut st = mgr.st.lock();
        st.next_auto_ms = st.next_auto_ms.max(now_ms() + USER_FAIL_AUTO_DELAY_MS);
    }
    let radio_off = !mgr.st.lock().radio_on;
    mgr.set_state(if radio_off { wifi_state::RADIO_OFF } else { wifi_state::DISCONNECTED },
                  if radio_off && f.reason == reason::SUPERSEDED { reason::RADIO_OFF } else { f.reason });
    if touched_radio && !r.settle(Some(f)) {
        mgr.set_state(wifi_state::UNAVAILABLE, reason::FW_ERROR);
        return After::Dead;
    }
    if radio_off { r.set_radio(false); }
    After::Idle
}

/// Find the target BSS (cache, else a fresh or directed scan).
fn find_target<R: Radio>(mgr: &Mgr, r: &mut R, req: &Req) -> Option<(ScanHit, NcBss)> {
    let look = |mgr: &Mgr| -> Option<(ScanHit, NcBss)> {
        let st = mgr.st.lock();
        let now = now_ms();
        let mut best: Option<&Entry> = None;
        for e in st.bss.iter() {
            if e.hit.bss.ssid != req.ssid { continue; }
            if let Some(b) = req.bssid { if e.nc.bssid != b { continue; } }
            if now - e.seen_ms > st.age_out_ms { continue; }
            // Prefer BSSs we can join, then signal.
            let j = e.nc.ext & bss_ext::JOINABLE != 0;
            let better = match best {
                None => true,
                Some(b) => (j, e.hit.rssi) > (b.nc.ext & bss_ext::JOINABLE != 0, b.hit.rssi),
            };
            if better { best = Some(e); }
        }
        best.map(|e| (e.hit.clone(), e.nc))
    };
    let fresh = now_ms() - mgr.st.lock().last_scan_ms < AUTO_CACHE_MS;
    if let Some(t) = look(mgr) { if fresh || !req.hidden { return Some(t); } }
    let ssids: Vec<Vec<u8>> = if req.hidden { alloc::vec![req.ssid.clone()] } else { Vec::new() };
    do_scan(mgr, r, &ssids, true, true, None);
    look(mgr)
}

/// One connection attempt, then (on success) the connected session.
fn attempt<R: Radio>(mgr: &Mgr, r: &mut R, req: Req) -> After {
    let t0 = now_ms();
    {
        let mut st = mgr.st.lock();
        st.ws.attempt = req.attempt;
        st.ws.profile_uuid = req.uuid;
        let n = req.ssid.len().min(SSID_MAX);
        st.ws.ssid = [0; SSID_MAX];
        st.ws.ssid[..n].copy_from_slice(&req.ssid[..n]);
        st.ws.ssid_len = n as u8;
        st.ws.bssid = req.bssid.unwrap_or([0; 6]);
        st.ws.freq_mhz = 0;
        st.ws.signal_dbm = 0;
        st.ws.ieee_status = 0;
        st.ws.ieee_reason = 0;
        if req.auto { st.ws.flags |= wifi_flags::AUTO; } else { st.ws.flags &= !wifi_flags::AUTO; }
    }
    mgr.set_state(wifi_state::CONNECTING, reason::NONE);
    if mgr.preempt_pending() { return fail_attempt(mgr, r, &req, &Fail::superseded(), false); }
    let (hit, nc) = match find_target(mgr, r, &req) {
        Some(t) => t,
        None => return fail_attempt(mgr, r, &req, &Fail::new(reason::NO_AP, "not found in a fresh scan"), false),
    };
    let caps = mgr.st.lock().caps;
    let a = match select_akm(req.akm, nc.ext, nc.rsn_flags, nc.nm_flags & nm_ap_flags::PRIVACY != 0, caps) {
        Ok(a) => a,
        Err(why) => {
            let cause = join_cause(req.akm, &hit.bss, caps).unwrap_or_else(|| String::from("the security settings are not supported"));
            mgr.log(&alloc::format!("'{}' {}: {}", wf::ssid_display(&req.ssid), fmt_mac(&nc.bssid), rsn_details(&hit.bss)));
            return fail_attempt(mgr, r, &req, &Fail::new(why, cause), false);
        }
    };
    if a != akm::NONE && req.secret.is_empty() {
        return fail_attempt(mgr, r, &req, &Fail::new(reason::NO_SECRET, "the network needs a passphrase"), false);
    }
    {
        let mut st = mgr.st.lock();
        st.ws.bssid = nc.bssid;
        st.ws.freq_mhz = nc.freq_mhz;
        st.ws.signal_dbm = nc.signal_dbm;
    }
    mgr.log(&alloc::format!("connecting to '{}' {} ch {} ({} dBm, akm {}){}", wf::ssid_display(&req.ssid), fmt_mac(&nc.bssid),
                            nc.channel, nc.signal_dbm, akm_name(a), if req.auto { " [auto]" } else { "" }));
    let cx = ConnCtx { mgr, deadline_ms: t0 + netctl_abi::CONNECT_DEADLINE_MS as u64 };
    mgr.set_state(wifi_state::AUTHENTICATING, reason::NONE);
    let res = r.connect(&hit, &req.ssid, a, req.secret.as_bytes(), &cx);
    if let Err(f) = res {
        let f = if f.reason == reason::SUPERSEDED && !mgr.preempt_pending() && cx.left_ms() == 0 {
            Fail::new(reason::TIMEOUT, "connect deadline passed")
        } else { f };
        return fail_attempt(mgr, r, &req, &f, true);
    }
    // Connected.
    mgr.note_success(&req.uuid, nc.bssid);
    {
        let mut st = mgr.st.lock();
        st.ws.akm = a;
        st.ws.tx_rate_kbps = r.tx_rate_kbps();
        st.last_signal = (now_ms(), nc.signal_dbm);
    }
    mgr.set_state(wifi_state::CONNECTED, reason::NONE);
    mgr.log(&alloc::format!("connected to '{}' in {} ms", wf::ssid_display(&req.ssid), now_ms() - t0));
    connected(mgr, r, &req, nc.bssid)
}

fn akm_name(a: u8) -> &'static str {
    match a { akm::NONE => "open", akm::PSK => "psk", akm::PSK_SHA256 => "psk-sha256", akm::SAE => "sae", _ => "auto" }
}

/// Associated: watch the link and serve commands until it ends.
fn connected<R: Radio>(mgr: &Mgr, r: &mut R, req: &Req, bssid: [u8; 6]) -> After {
    let mut last_bg = now_ms();
    let since = now_ms();
    loop {
        if let Some(f) = r.poll_connected(50) {
            mgr.log(&alloc::format!("link to '{}' lost: {} ({})", wf::ssid_display(&req.ssid), reason_name(f.reason), f.msg));
            // An AP that drops us right after accepting us would otherwise be
            // rejoined at once, over and over.
            if now_ms() - since < FLAP_MS && !f.fw_assert { mgr.note_bss_flap(bssid); }
            r.leave(false);
            {
                let mut st = mgr.st.lock();
                st.ws.ieee_reason = f.ieee_reason;
                // Reconnect at once (no back-off for a link that was good).
                st.next_auto_ms = 0;
            }
            mgr.set_state(wifi_state::DISCONNECTED, f.reason);
            if !r.settle(Some(&f)) { mgr.set_state(wifi_state::UNAVAILABLE, reason::FW_ERROR); return After::Dead; }
            return After::Idle;
        }
        // Signal reports.
        let now = now_ms();
        let (t, last) = mgr.st.lock().last_signal;
        if now - t >= SIGNAL_MIN_MS {
            if let Some(s) = r.signal() {
                let s = s as i16;
                mgr.st.lock().last_signal = (now, if (s - last).abs() >= 3 { s } else { last });
                if (s - last).abs() >= 3 {
                    let mut sig = NcWifiSignal::default();
                    sig.ifindex = mgr.ifindex();
                    sig.signal_dbm = s;
                    sig.quality = quality_from_dbm(s as i32);
                    sig.tx_rate_kbps = r.tx_rate_kbps();
                    sig.bssid = bssid;
                    mgr.st.lock().ws.signal_dbm = s;
                    mgr.emit(WifiEvent::Signal(sig));
                }
            }
        }
        // Keep the current BSS fresh in the cache.
        {
            let mut st = mgr.st.lock();
            if let Some(e) = st.bss.iter_mut().find(|e| e.nc.bssid == bssid) { e.seen_ms = now; }
        }
        let cmd = match mgr.pop_cmd() {
            Some(c) => c,
            None => {
                if r.caps() & wifi_caps::SCAN_CONNECTED != 0 && now - last_bg >= CONNECTED_SCAN_MS {
                    last_bg = now;
                    do_scan(mgr, r, &[], true, true, Some(bssid));
                }
                continue;
            }
        };
        match cmd {
            WifiCmd::Scan { ssids, force } => {
                let ss: Vec<Vec<u8>> = ssids.iter().map(|s| s.as_slice().to_vec()).collect();
                do_scan(mgr, r, &ss, force, false, Some(bssid));
            }
            WifiCmd::ProfilesChanged => {
                {
                    let mut st = mgr.st.lock();
                    st.wrong_key.clear();
                    st.backoff.clear();
                    st.auto_blocked = false;
                }
                mgr.refresh_flags();
            }
            WifiCmd::Disconnect { block_autoconnect } => {
                let t0 = now_ms();
                mgr.st.lock().auto_blocked = block_autoconnect;
                mgr.set_state(wifi_state::DISCONNECTING, reason::USER);
                r.leave(true);
                mgr.set_state(wifi_state::DISCONNECTED, reason::USER);
                mgr.log(&alloc::format!("disconnected from '{}' on request in {} ms", wf::ssid_display(&req.ssid), now_ms() - t0));
                if !r.settle(None) { mgr.set_state(wifi_state::UNAVAILABLE, reason::FW_ERROR); return After::Dead; }
                return After::Idle;
            }
            WifiCmd::SetRadio(on) => {
                if on { continue; }
                mgr.st.lock().radio_on = false;
                mgr.set_state(wifi_state::DISCONNECTING, reason::RADIO_OFF);
                r.leave(true);
                let ok = r.settle(None);
                r.set_radio(false);
                mgr.flush_bss();
                mgr.set_state(wifi_state::RADIO_OFF, reason::RADIO_OFF);
                if !ok { mgr.set_state(wifi_state::UNAVAILABLE, reason::FW_ERROR); return After::Dead; }
                return After::Idle;
            }
            WifiCmd::Connect(p) => {
                // A new activation replaces this one.
                mgr.set_state(wifi_state::DISCONNECTING, reason::SUPERSEDED);
                r.leave(true);
                mgr.set_state(wifi_state::DISCONNECTED, reason::SUPERSEDED);
                if !r.settle(None) { mgr.set_state(wifi_state::UNAVAILABLE, reason::FW_ERROR); return After::Dead; }
                {
                    let mut st = mgr.st.lock();
                    st.auto_blocked = false;
                    st.wrong_key.retain(|u| *u != p.profile_uuid);
                    st.backoff.retain(|b| b.uuid != p.profile_uuid);
                }
                return After::Next(Req::from_params(p));
            }
        }
    }
}

/// `NcSsid` from bytes (for callers building commands).
pub fn ssid(s: &[u8]) -> NcSsid { NcSsid::from_bytes(s).unwrap_or_default() }
