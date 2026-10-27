//! FullMAC control: the init sequence, escan, WPA2-PSK join through the
//! firmware supplicant, link tracking, disconnect and reconnect. Written
//! only against the frozen [`Fwil`] / [`RxSink`] seam, so it runs the same
//! over the real BCDC/SDPCM stack and over the host tests' mock.
//!
//! The firmware does the MLME. The 4-way handshake runs in the firmware
//! ("sup_wpa" 1, when the firmware has `idsup`) or on the host
//! ([`crate::wifi_crypto::Supplicant`] over EAPOL data frames, keys installed
//! with "wsec_key"); see [`JoinOpts`]. The host configures security, asks to
//! join, and follows the events:
//!
//! * join done = `LINK` with the link flag, plus (keyed networks) `PSK_SUP`
//!   with status FWSUP_COMPLETED;
//! * join failed = `SET_SSID` with a non-success status, a `PSK_SUP` failure,
//!   or a deauth/disassoc/link-down before both of the above;
//! * link lost = `DEAUTH(_IND)`, `DISASSOC(_IND)` or `LINK` without the flag.
//!
//! Every wait is bounded by the caller's timeout and services the bus
//! through [`Fwil::poll`]; nothing here can hang.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::event::{self, ev, status, EventMask};
use super::fwil::{FwError, Fwil, RxSink};

/// WLC ioctl numbers (`BRCMF_C_*`).
pub mod cmd {
    pub const UP: u32 = 2;
    pub const DOWN: u32 = 3;
    pub const GET_RATE: u32 = 12;
    pub const SET_INFRA: u32 = 20;
    pub const SET_AUTH: u32 = 22;
    pub const GET_BSSID: u32 = 23;
    pub const SET_SSID: u32 = 26;
    pub const SET_PASSIVE_SCAN: u32 = 49;
    pub const DISASSOC: u32 = 52;
    pub const GET_RSSI: u32 = 127;
    pub const SET_WSEC: u32 = 134;
    pub const SET_WPA_AUTH: u32 = 165;
    pub const SET_WSEC_PMK: u32 = 268;
}

pub const WSEC_NONE: u32 = 0;
pub const WSEC_AES: u32 = 0x4;
pub const WPA_AUTH_DISABLED: u32 = 0;
pub const WPA2_AUTH_PSK: u32 = 0x80;
/// AKM 6 (PSK-SHA256), Linux brcmfmac `WPA2_AUTH_PSK_SHA256`.
pub const WPA2_AUTH_PSK_SHA256: u32 = 0x8000;
pub const WSEC_TKIP: u32 = 0x2;
/// `brcmf_wsec_pmk_le.flags`: the key is an ASCII passphrase (the firmware
/// runs PBKDF2 itself).
pub const WSEC_PASSPHRASE: u16 = 1;
pub const PMK_STRUCT_LEN: usize = 70;
/// `brcmf_wsec_key_le` (164 bytes) and its constants.
pub const WSEC_KEY_LEN: usize = 164;
pub const CRYPTO_ALGO_TKIP: u32 = 2;
pub const CRYPTO_ALGO_AES_CCM: u32 = 4;
pub const BRCMF_PRIMARY_KEY: u32 = 1 << 1;
const ETH_P_PAE: [u8; 2] = [0x88, 0x8e];

/// The netctl ifindex wifi0 was registered under (0 before attach). Only for
/// the handshake log line: the EAPOL frames never pass through the IP stack
/// or any netif queue, they go straight to the firmware (BDC ifidx 0) with the
/// firmware's own MAC as source, so a second NIC cannot reroute them.
pub static NETCTL_IFINDEX: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub const ESCAN_REQ_VERSION: u32 = 1;
pub const ESCAN_ACTION_START: u16 = 1;
pub const ESCAN_ACTION_ABORT: u16 = 3;
pub const BSS_INFO_MIN: usize = 128;
/// 802.11 reason "leaving" for our own disassociation.
pub const REASON_LEAVING: u32 = 3;
/// Received data frames held before the net server takes them.
pub const RX_QUEUE_MAX: usize = 256;

/// What the driver needs from its environment (kernel or test).
#[derive(Clone, Copy)]
pub struct Env {
    pub now_ms: fn() -> u64,
    pub sleep_ms: fn(u64),
    /// One line, no `[brcmf]` prefix, no newline. Never given secrets.
    pub log: fn(&str),
    /// Fill with random bytes (the host supplicant's SNonce).
    pub random: fn(&mut [u8]),
}

fn fmt_mac6(m: &[u8; 6]) -> alloc::string::String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

fn le16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn le32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }

// ── scan results ───────────────────────────────────────────────────────────

/// One BSS from an `ESCAN_RESULT` (`brcmf_bss_info_le`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BssEntry {
    pub bssid: [u8; 6],
    pub ssid: Vec<u8>,
    pub channel: u8,
    pub chanspec: u16,
    pub rssi: i8,
    pub capability: u16,
    pub beacon_period: u16,
    pub ies: Vec<u8>,
}

impl BssEntry {
    /// The BSS as a beacon MPDU (header + fixed fields + IEs), so the shared
    /// 802.11 parser (`wifi_frame::parse_beacon`) reads its RSN, rates, HT.
    pub fn beacon_frame(&self) -> Vec<u8> {
        let mut f = Vec::with_capacity(36 + self.ies.len());
        f.extend_from_slice(&[0x80, 0x00, 0, 0]);
        f.extend_from_slice(&[0xff; 6]);
        f.extend_from_slice(&self.bssid);
        f.extend_from_slice(&self.bssid);
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&[0; 8]);
        f.extend_from_slice(&self.beacon_period.to_le_bytes());
        f.extend_from_slice(&self.capability.to_le_bytes());
        f.extend_from_slice(&self.ies);
        f
    }

    /// Encode as `brcmf_bss_info_le` (tests and mocks).
    pub fn to_bss_info(&self) -> Vec<u8> {
        let mut b = alloc::vec![0u8; BSS_INFO_MIN];
        let len = BSS_INFO_MIN + self.ies.len();
        b[0..4].copy_from_slice(&109u32.to_le_bytes());
        b[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        b[8..14].copy_from_slice(&self.bssid);
        b[14..16].copy_from_slice(&self.beacon_period.to_le_bytes());
        b[16..18].copy_from_slice(&self.capability.to_le_bytes());
        let sl = self.ssid.len().min(32);
        b[18] = sl as u8;
        b[19..19 + sl].copy_from_slice(&self.ssid[..sl]);
        b[72..74].copy_from_slice(&self.chanspec.to_le_bytes());
        b[78..80].copy_from_slice(&(self.rssi as i16).to_le_bytes());
        b[88] = self.channel;
        b[116..118].copy_from_slice(&(BSS_INFO_MIN as u16).to_le_bytes());
        b[120..124].copy_from_slice(&(self.ies.len() as u32).to_le_bytes());
        b.extend_from_slice(&self.ies);
        b
    }
}

/// Parse one `brcmf_bss_info_le` record.
pub fn parse_bss_info(r: &[u8]) -> Option<BssEntry> {
    if r.len() < BSS_INFO_MIN { return None; }
    let len = (le32(r, 4) as usize).min(r.len());
    let mut bssid = [0u8; 6];
    bssid.copy_from_slice(&r[8..14]);
    let sl = (r[18] as usize).min(32);
    let chanspec = le16(r, 72);
    let ctl = r[88];
    let ie_off = le16(r, 116) as usize;
    let ie_len = le32(r, 120) as usize;
    let ies = if ie_off >= BSS_INFO_MIN && ie_off.checked_add(ie_len).map_or(false, |e| e <= len) {
        r[ie_off..ie_off + ie_len].to_vec()
    } else {
        Vec::new()
    };
    let rssi = (le16(r, 78) as i16).clamp(-128, 127) as i8;
    Some(BssEntry {
        bssid,
        ssid: r[19..19 + sl].to_vec(),
        channel: if ctl != 0 { ctl } else { (chanspec & 0xff) as u8 },
        chanspec,
        rssi,
        capability: le16(r, 16),
        beacon_period: le16(r, 14),
        ies,
    })
}

/// Parse the data of an `ESCAN_RESULT` event (`brcmf_escan_result_le`):
/// (sync id, the BSSs in it).
pub fn parse_escan(d: &[u8]) -> Option<(u16, Vec<BssEntry>)> {
    if d.len() < 12 { return None; }
    let sync = le16(d, 8);
    let count = le16(d, 10) as usize;
    let mut out = Vec::new();
    let mut off = 12;
    for _ in 0..count {
        let r = match d.get(off..) { Some(r) if r.len() >= BSS_INFO_MIN => r, _ => break };
        let len = le32(r, 4) as usize;
        if len < BSS_INFO_MIN { break; }
        if let Some(b) = parse_bss_info(&r[..len.min(r.len())]) { out.push(b); }
        off += len;
    }
    Some((sync, out))
}

/// Encode an `ESCAN_RESULT` payload (tests and mocks).
pub fn build_escan(sync: u16, bss: &[BssEntry]) -> Vec<u8> {
    let mut recs = Vec::new();
    for b in bss { recs.extend_from_slice(&b.to_bss_info()); }
    let mut d = Vec::new();
    d.extend_from_slice(&((12 + recs.len()) as u32).to_le_bytes());
    d.extend_from_slice(&109u32.to_le_bytes());
    d.extend_from_slice(&sync.to_le_bytes());
    d.extend_from_slice(&(bss.len() as u16).to_le_bytes());
    d.extend_from_slice(&recs);
    d
}

/// Add a BSS to a result list: one entry per BSSID, the strongest sighting
/// wins, and a hidden-SSID sighting never erases a known SSID.
pub fn merge_bss(list: &mut Vec<BssEntry>, b: BssEntry) {
    match list.iter_mut().find(|x| x.bssid == b.bssid) {
        Some(x) => {
            let keep_ssid = if b.ssid.is_empty() { Some(core::mem::take(&mut x.ssid)) } else { None };
            if b.rssi > x.rssi || (x.ies.is_empty() && !b.ies.is_empty()) {
                *x = b;
            }
            if let Some(s) = keep_ssid { if x.ssid.is_empty() { x.ssid = s; } }
        }
        None => list.push(b),
    }
}

// ── request encoders ───────────────────────────────────────────────────────

/// `brcmf_escan_params_le` (version 1): wildcard scan, plus a wildcard and
/// one directed entry per SSID when `ssids` is non-empty; all channels.
pub fn escan_params(sync_id: u16, action: u16, ssids: &[&[u8]]) -> Vec<u8> {
    let mut p = alloc::vec![0u8; 8 + 64];
    p[0..4].copy_from_slice(&ESCAN_REQ_VERSION.to_le_bytes());
    p[4..6].copy_from_slice(&action.to_le_bytes());
    p[6..8].copy_from_slice(&sync_id.to_le_bytes());
    let s = &mut p[8..];
    s[36..42].copy_from_slice(&[0xff; 6]);
    s[42] = 2; // DOT11_BSSTYPE_ANY
    s[43] = 0; // active
    for o in [44, 48, 52, 56] { s[o..o + 4].copy_from_slice(&(-1i32).to_le_bytes()); }
    if ssids.is_empty() { return p; }
    let n = ssids.len() + 1;
    s[60..64].copy_from_slice(&((n as u32) << 16).to_le_bytes());
    let push = |p: &mut Vec<u8>, ssid: &[u8]| {
        let l = ssid.len().min(32);
        p.extend_from_slice(&(l as u32).to_le_bytes());
        let mut b = [0u8; 32];
        b[..l].copy_from_slice(&ssid[..l]);
        p.extend_from_slice(&b);
    };
    push(&mut p, b"");
    for s in ssids { push(&mut p, s); }
    p
}

/// d11ac 20 MHz chanspec of a channel number.
pub fn chanspec20(ch: u8) -> u16 {
    let band = if ch <= 14 { 0x0000 } else { 0xc000 };
    ch as u16 | 0x1000 | band
}

fn ssid_le(out: &mut Vec<u8>, ssid: &[u8]) {
    let l = ssid.len().min(32);
    out.extend_from_slice(&(l as u32).to_le_bytes());
    let mut b = [0u8; 32];
    b[..l].copy_from_slice(&ssid[..l]);
    out.extend_from_slice(&b);
}

fn assoc_le(out: &mut Vec<u8>, bssid: Option<[u8; 6]>, channel: u8) {
    out.extend_from_slice(&bssid.unwrap_or([0xff; 6]));
    out.extend_from_slice(&[0, 0]);
    let n: u32 = if channel != 0 { 1 } else { 0 };
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&(if channel != 0 { chanspec20(channel) } else { 0 }).to_le_bytes());
    out.extend_from_slice(&[0, 0]);
}

/// `brcmf_ext_join_params_le` for the "join" iovar (72 bytes).
pub fn ext_join_params(ssid: &[u8], bssid: Option<[u8; 6]>, channel: u8) -> Vec<u8> {
    let mut p = Vec::with_capacity(72);
    ssid_le(&mut p, ssid);
    p.extend_from_slice(&[0xff, 0, 0, 0]); // scan_type -1 (default)
    let (np, act, pas) = if channel != 0 { (8i32, 320i32, 400i32) } else { (-1, -1, -1) };
    for v in [np, act, pas, -1] { p.extend_from_slice(&v.to_le_bytes()); }
    assoc_le(&mut p, bssid, channel);
    p
}

/// `brcmf_join_params` for the WLC_SET_SSID fallback (52 bytes).
pub fn join_params(ssid: &[u8], bssid: Option<[u8; 6]>, channel: u8) -> Vec<u8> {
    let mut p = Vec::with_capacity(52);
    ssid_le(&mut p, ssid);
    assoc_le(&mut p, bssid, channel);
    p
}

/// `brcmf_wsec_pmk_le` (70 bytes) for WLC_SET_WSEC_PMK in the given form:
/// * `Raw`: the 32-byte PSK, flags 0 (Linux `brcmf_set_pmk` since 4.18);
/// * `Hex`: the PSK as 64 hex digits with WSEC_PASSPHRASE (Linux 4.13..4.17);
/// * `Passphrase`: the ASCII passphrase with WSEC_PASSPHRASE (the firmware
///   runs PBKDF2; Linux uses this form only for SAE).
/// None when the form needs a passphrase the key does not have.
pub fn wsec_pmk(key: &Key, ssid: &[u8], form: PmkForm) -> Option<[u8; PMK_STRUCT_LEN]> {
    let mut p = [0u8; PMK_STRUCT_LEN];
    match (key, form) {
        (Key::Open, _) => {}
        (Key::Passphrase(s), PmkForm::Passphrase) => {
            let l = s.len().min(64);
            p[0..2].copy_from_slice(&(l as u16).to_le_bytes());
            p[2..4].copy_from_slice(&WSEC_PASSPHRASE.to_le_bytes());
            p[4..4 + l].copy_from_slice(&s[..l]);
        }
        (Key::Pmk(_), PmkForm::Passphrase) => return None,
        (k, f) => {
            let mut psk = k.psk(ssid)?;
            if f == PmkForm::Hex {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                p[0..2].copy_from_slice(&64u16.to_le_bytes());
                p[2..4].copy_from_slice(&WSEC_PASSPHRASE.to_le_bytes());
                for (i, b) in psk.iter().enumerate() {
                    p[4 + 2 * i] = HEX[(b >> 4) as usize];
                    p[5 + 2 * i] = HEX[(b & 15) as usize];
                }
            } else {
                p[0..2].copy_from_slice(&32u16.to_le_bytes());
                p[4..36].copy_from_slice(&psk);
            }
            for b in psk.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) } }
        }
    }
    Some(p)
}

/// `brcmf_wsec_key_le` for the "wsec_key" iovar: a pairwise key (`ea` = the
/// AP, flags 0) or a group key (`ea` zero, BRCMF_PRIMARY_KEY), as Linux
/// `brcmf_add_keyext` / `brcmf_cfg80211_add_key` send them. A 32-byte TKIP
/// key has its MIC halves swapped (station mode), as brcmfmac does.
pub fn wsec_key(index: u32, key: &[u8], ea: Option<[u8; 6]>) -> [u8; WSEC_KEY_LEN] {
    let mut p = [0u8; WSEC_KEY_LEN];
    let l = key.len().min(32);
    p[0..4].copy_from_slice(&index.to_le_bytes());
    p[4..8].copy_from_slice(&(l as u32).to_le_bytes());
    p[8..8 + l].copy_from_slice(&key[..l]);
    let algo = if l == 32 {
        p[24..32].copy_from_slice(&key[24..32]);
        p[32..40].copy_from_slice(&key[16..24]);
        CRYPTO_ALGO_TKIP
    } else { CRYPTO_ALGO_AES_CCM };
    p[112..116].copy_from_slice(&algo.to_le_bytes());
    let flags = if ea.is_some() { 0 } else { BRCMF_PRIMARY_KEY };
    p[116..120].copy_from_slice(&flags.to_le_bytes());
    if let Some(ea) = ea { p[156..162].copy_from_slice(&ea); }
    p
}

// ── join options (live-tunable over /etc/brcmf.ctl) ────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinMethod { Auto, JoinIovar, SetSsid }
/// Who runs the 4-way handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupMode { Auto, Firmware, Host }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmkForm { Auto, Raw, Hex, Passphrase }
/// How auth / wsec / wpa_auth are set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecPath { Auto, Ioctl, Iovar }

/// Join knobs. `Auto` = what Linux brcmfmac does, with logged fallbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinOpts {
    pub method: JoinMethod,
    pub sup: SupMode,
    pub pmk: PmkForm,
    pub sec: SecPath,
    /// None = set "mfp" 0 only if the firmware advertises `mfp`.
    pub mfp: Option<u32>,
    /// Set "wpaie" (our RSN IE) before joining.
    pub wpaie: bool,
    /// AKM suite type we advertise: None = auto (6 if the AP offers it, else 2).
    pub akm: Option<u8>,
    /// RSN capability MFPC / MFPR we advertise: None = auto (MFPC if the AP
    /// has MFPC and the firmware has `mfp`, or with AKM 6; MFPR 0).
    pub mfpc: Option<bool>,
    pub mfpr: Option<bool>,
    /// RSN IE put in message 2: the firmware's association request IE
    /// ("assoc_req_ies") when it reports one (auto/fw), or the IE we built.
    pub msg2ie: Msg2Ie,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg2Ie { Auto, Ours, Fw }

/// The AP's RSN element from the scan, decoded (802.11-2016 9.4.2.25).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApRsn {
    pub raw: Vec<u8>,
    pub group: u32,
    pub pairwise: Vec<u32>,
    pub akm: Vec<u32>,
    pub caps: u16,
    pub pmkids: usize,
    pub group_mgmt: Option<u32>,
}

impl ApRsn {
    pub fn mfpc(&self) -> bool { self.caps & crate::wifi_crypto::RSN_CAP_MFPC != 0 }
    pub fn mfpr(&self) -> bool { self.caps & crate::wifi_crypto::RSN_CAP_MFPR != 0 }
    pub fn has_akm(&self, t: u8) -> bool { self.akm.contains(&(0x000f_ac00 | t as u32)) }
    pub fn describe(&self) -> alloc::string::String {
        let suite = |v: u32| if v >> 8 == 0x000fac { alloc::format!("{}", v & 0xff) } else { alloc::format!("{:08x}", v) };
        let list = |v: &[u32]| v.iter().map(|x| suite(*x)).collect::<Vec<_>>().join(",");
        alloc::format!("group={} pairwise={} akm={} caps=0x{:04x} mfpc={} mfpr={} pmkids={} group_mgmt={} ie={}",
            suite(self.group), list(&self.pairwise), list(&self.akm), self.caps, self.mfpc() as u8, self.mfpr() as u8,
            self.pmkids, self.group_mgmt.map(suite).unwrap_or_else(|| "absent(6)".into()), hex(&self.raw))
    }
}

pub fn hex(b: &[u8]) -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::with_capacity(b.len() * 2);
    for x in b { let _ = write!(s, "{:02x}", x); }
    s
}

/// Find the RSN element (id 48) in an IE list; returns the whole element.
pub fn find_rsn_ie(ies: &[u8]) -> Option<&[u8]> {
    let mut p = 0usize;
    while p + 2 <= ies.len() {
        let end = p + 2 + ies[p + 1] as usize;
        if end > ies.len() { return None; }
        if ies[p] == 48 { return Some(&ies[p..end]); }
        p = end;
    }
    None
}

/// Decode a whole RSN element (id, len, body).
pub fn parse_ap_rsn(ie: &[u8]) -> Option<ApRsn> {
    if ie.len() < 4 || ie[0] != 48 { return None; }
    let d = &ie[2..];
    let be32 = |o: usize| d.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let le16o = |o: usize| d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let mut r = ApRsn { raw: ie.to_vec(), group: 0x000f_ac04, pairwise: alloc::vec![0x000f_ac04], akm: alloc::vec![0x000f_ac01],
                        caps: 0, pmkids: 0, group_mgmt: None };
    let mut o = 2;
    if o < d.len() { r.group = be32(o)?; o += 4; }
    let list = |o: &mut usize| -> Option<Vec<u32>> {
        let n = le16o(*o)? as usize; *o += 2;
        let mut v = Vec::new();
        for _ in 0..n { v.push(be32(*o)?); *o += 4; }
        Some(v)
    };
    if o < d.len() { r.pairwise = list(&mut o)?; }
    if o < d.len() { r.akm = list(&mut o)?; }
    if o < d.len() { r.caps = le16o(o)?; o += 2; }
    if o < d.len() { r.pmkids = le16o(o)? as usize; o += 2 + 16 * r.pmkids; }
    if o < d.len() { r.group_mgmt = be32(o); }
    Some(r)
}

impl Default for JoinOpts {
    fn default() -> Self {
        JoinOpts { method: JoinMethod::Auto, sup: SupMode::Auto, pmk: PmkForm::Auto, sec: SecPath::Auto, mfp: None, wpaie: true,
                   akm: None, mfpc: None, mfpr: None, msg2ie: Msg2Ie::Auto }
    }
}

impl JoinOpts {
    /// Apply knob words (`method= supwpa= pmk= sec= mfp= wpaie=`); `reset`
    /// starts from the defaults. Unknown words are returned.
    pub fn apply<'a>(&mut self, words: &[&'a str]) -> Vec<&'a str> {
        let mut bad = Vec::new();
        for w in words {
            let (k, v) = match w.split_once('=') { Some(kv) => kv, None => (*w, "") };
            let ok = match (k, v) {
                ("reset", _) => { *self = JoinOpts::default(); true }
                ("method", "auto") => { self.method = JoinMethod::Auto; true }
                ("method", "joiniovar") | ("method", "join") => { self.method = JoinMethod::JoinIovar; true }
                ("method", "setssid") => { self.method = JoinMethod::SetSsid; true }
                ("supwpa", "auto") => { self.sup = SupMode::Auto; true }
                ("supwpa", "1") | ("sup", "fw") => { self.sup = SupMode::Firmware; true }
                ("supwpa", "0") | ("sup", "host") => { self.sup = SupMode::Host; true }
                ("pmk", "auto") => { self.pmk = PmkForm::Auto; true }
                ("pmk", "raw") => { self.pmk = PmkForm::Raw; true }
                ("pmk", "hex") => { self.pmk = PmkForm::Hex; true }
                ("pmk", "passphrase") => { self.pmk = PmkForm::Passphrase; true }
                ("sec", "auto") => { self.sec = SecPath::Auto; true }
                ("sec", "ioctl") => { self.sec = SecPath::Ioctl; true }
                ("sec", "iovar") => { self.sec = SecPath::Iovar; true }
                ("mfp", "auto") => { self.mfp = None; true }
                ("mfp", n) => match n.parse::<u32>() { Ok(n) if n <= 2 => { self.mfp = Some(n); true } _ => false },
                ("wpaie", "1") => { self.wpaie = true; true }
                ("wpaie", "0") => { self.wpaie = false; true }
                ("akm", "auto") => { self.akm = None; true }
                ("akm", "2") => { self.akm = Some(2); true }
                ("akm", "6") => { self.akm = Some(6); true }
                ("mfpc", "auto") => { self.mfpc = None; true }
                ("mfpc", "0") => { self.mfpc = Some(false); true }
                ("mfpc", "1") => { self.mfpc = Some(true); true }
                ("mfpr", "auto") => { self.mfpr = None; true }
                ("mfpr", "0") => { self.mfpr = Some(false); true }
                ("mfpr", "1") => { self.mfpr = Some(true); true }
                ("msg2ie", "auto") => { self.msg2ie = Msg2Ie::Auto; true }
                ("msg2ie", "ours") => { self.msg2ie = Msg2Ie::Ours; true }
                ("msg2ie", "fw") => { self.msg2ie = Msg2Ie::Fw; true }
                _ => false,
            };
            if !ok { bad.push(*w); }
        }
        bad
    }

    pub fn describe(&self) -> alloc::string::String {
        let m = match self.method { JoinMethod::Auto => "auto", JoinMethod::JoinIovar => "joiniovar", JoinMethod::SetSsid => "setssid" };
        let s = match self.sup { SupMode::Auto => "auto", SupMode::Firmware => "1", SupMode::Host => "0" };
        let p = match self.pmk { PmkForm::Auto => "auto", PmkForm::Raw => "raw", PmkForm::Hex => "hex", PmkForm::Passphrase => "passphrase" };
        let c = match self.sec { SecPath::Auto => "auto", SecPath::Ioctl => "ioctl", SecPath::Iovar => "iovar" };
        let f = match self.mfp { None => alloc::string::String::from("auto"), Some(n) => alloc::format!("{}", n) };
        let ob = |v: Option<bool>| match v { None => "auto", Some(true) => "1", Some(false) => "0" };
        let a = match self.akm { None => alloc::string::String::from("auto"), Some(n) => alloc::format!("{}", n) };
        let i = match self.msg2ie { Msg2Ie::Auto => "auto", Msg2Ie::Ours => "ours", Msg2Ie::Fw => "fw" };
        alloc::format!("method={} supwpa={} pmk={} sec={} mfp={} wpaie={} akm={} mfpc={} mfpr={} msg2ie={}",
                       m, s, p, c, f, self.wpaie as u8, a, ob(self.mfpc), ob(self.mfpr), i)
    }
}

/// Host 4-way handshake state for one join (sup_wpa 0).
struct HostSup {
    pmk: [u8; 32],
    rsn_ie: Vec<u8>,
    akm: u8,
    group_tkip: bool,
    sup: Option<crate::wifi_crypto::Supplicant>,
    ap: [u8; 6],
}

impl Drop for HostSup {
    fn drop(&mut self) { for b in self.pmk.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) } } }
}

/// `brcmf_fil_country_le`: abbrev, rev -1 (the firmware's default
/// revision for that country), ccode.
pub fn country_params(cc: [u8; 2]) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[0..2].copy_from_slice(&cc);
    p[4..8].copy_from_slice(&(-1i32).to_le_bytes());
    p[8..10].copy_from_slice(&cc);
    p
}

/// Country from the command line (`brcmf.country=XX`, else the generic
/// `wifi.country=XX`), default "PT". Two ASCII letters, upper-cased.
pub fn country_from_cmdline(cmdline: &str) -> [u8; 2] {
    let val = |k: &str| cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix(k).and_then(|r| r.strip_prefix('='))).last();
    let v = val("brcmf.country").or_else(|| val("wifi.country"));
    match v.map(|s| s.as_bytes()) {
        Some(&[a, b]) if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() => [a.to_ascii_uppercase(), b.to_ascii_uppercase()],
        _ => *b"PT",
    }
}

// ── keys and join requests ─────────────────────────────────────────────────

/// What the firmware supplicant gets. Wiped on drop; never logged.
#[derive(Clone, PartialEq, Eq)]
pub enum Key {
    Open,
    /// 8..63 ASCII characters; PBKDF2 runs in the firmware.
    Passphrase(Vec<u8>),
    /// A precomputed PMK (also how a 64-hex-digit PSK arrives).
    Pmk([u8; 32]),
}

impl Key {
    /// The 32-byte PSK (PBKDF2 of a passphrase over the SSID); None if open.
    pub fn psk(&self, ssid: &[u8]) -> Option<[u8; 32]> {
        match self {
            Key::Open => None,
            Key::Passphrase(p) => Some(crate::wifi_crypto::wpa_psk(p, ssid)),
            Key::Pmk(k) => Some(*k),
        }
    }

    /// Shape of the secret for diagnostics: kind and length, and whether it
    /// has leading/trailing whitespace (a classic provisioning slip). Never the value.
    pub fn describe_shape(&self) -> alloc::string::String {
        match self {
            Key::Open => "open".into(),
            Key::Pmk(_) => "pmk (64 hex digits)".into(),
            Key::Passphrase(p) => {
                let ws = |c: Option<&u8>| c.is_some_and(|c| c.is_ascii_whitespace() || *c < 0x20);
                alloc::format!("passphrase len={} edge_whitespace={} non_ascii={}", p.len(),
                               (ws(p.first()) || ws(p.last())) as u8, p.iter().any(|c| *c >= 0x7f || *c < 0x20) as u8)
            }
        }
    }

    /// From a netctl secret: empty = open, 64 hex digits = PMK, else passphrase.
    pub fn from_secret(s: &[u8]) -> Key {
        if s.is_empty() { return Key::Open; }
        if s.len() == 64 && s.iter().all(|c| c.is_ascii_hexdigit()) {
            let h = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            let mut k = [0u8; 32];
            for i in 0..32 { k[i] = (h(s[2 * i]) << 4) | h(s[2 * i + 1]); }
            return Key::Pmk(k);
        }
        Key::Passphrase(s.to_vec())
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        match self {
            Key::Open => {}
            Key::Passphrase(v) => for b in v.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) } },
            Key::Pmk(k) => for b in k.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) } },
        }
    }
}

impl core::fmt::Debug for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self { Key::Open => "Open", Key::Passphrase(_) => "Passphrase(<redacted>)", Key::Pmk(_) => "Pmk(<redacted>)" })
    }
}

#[derive(Clone, Debug)]
pub struct JoinReq {
    pub ssid: Vec<u8>,
    pub bssid: Option<[u8; 6]>,
    /// 0 = let the firmware scan for it.
    pub channel: u8,
    pub key: Key,
}

/// Why a join did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinFail {
    Fw(FwError),
    /// `SET_SSID` status NO_NETWORKS: the SSID was not found.
    NoNetwork,
    /// `SET_SSID` failed (auth/assoc refused, timeout) with this event status.
    Assoc { status: u32, reason: u32 },
    /// The handshake did not complete: `msg2` when we were associated (the
    /// AP had started it), `reason` the AP's deauth/disassoc reason.
    Handshake { msg2: bool, reason: Option<u16> },
    /// No outcome within the timeout, before association.
    Timeout,
    /// The caller preempted the attempt.
    Aborted,
}

/// How an established link ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Down {
    /// Deauthenticated (`by_ap` = the _IND form, sent by the AP).
    Deauth { reason: u16, by_ap: bool },
    Disassoc { reason: u16, by_ap: bool },
    /// LINK event without the link flag (beacon loss and similar).
    LinkLost { reason: u32 },
    /// The firmware supplicant failed (e.g. a group rekey).
    SupFailed { status: u32, reason: u32 },
    JoinFailed { status: u32, reason: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step { Associating, Handshake }

/// Progress and preemption for [`Brcmf::join`].
pub trait JoinObserver {
    fn aborted(&self) -> bool { false }
    fn step(&mut self, _s: Step) {}
}

pub struct NoObserver;
impl JoinObserver for NoObserver {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link { Down, Joining, Up }

// ── the event/data sink ────────────────────────────────────────────────────

/// Everything the firmware sends up, folded into state. This is the
/// [`RxSink`] the driver passes to [`Fwil::poll`].
pub struct Rx {
    escan_sync: u16,
    escan_active: bool,
    pub escan_done: Option<u32>,
    pub results: Vec<BssEntry>,
    joining: bool,
    pub assoc_status: Option<u32>,
    pub link_up: bool,
    /// Associated at least once during the current attempt.
    pub link_seen: bool,
    pub keyed: bool,
    pub sup_done: bool,
    /// Furthest supplicant state seen in this attempt.
    pub sup_max: u32,
    pub down: Option<Down>,
    pub bssid: [u8; 6],
    pub rx: VecDeque<Vec<u8>>,
    pub rx_dropped: u64,
    pub rx_frames: u64,
    pub events: u64,
    pub bad_events: u64,
    pub if_events: u32,
    /// The last 16 events (type, status, reason).
    pub recent: VecDeque<(u32, u32, u32)>,
}

impl Rx {
    pub fn new() -> Rx {
        Rx {
            escan_sync: 0, escan_active: false, escan_done: None, results: Vec::new(),
            joining: false, assoc_status: None, link_up: false, link_seen: false, keyed: false, sup_done: false,
            sup_max: 0, down: None, bssid: [0; 6], rx: VecDeque::new(), rx_dropped: 0, rx_frames: 0,
            events: 0, bad_events: 0, if_events: 0, recent: VecDeque::new(),
        }
    }

    fn begin_join(&mut self, keyed: bool) {
        self.joining = true;
        self.assoc_status = None;
        self.link_up = false;
        self.link_seen = false;
        self.keyed = keyed;
        self.sup_done = false;
        self.sup_max = 0;
        self.down = None;
    }

    /// The join finished: link up and, if keyed, the supplicant done.
    pub fn joined(&self) -> bool { self.link_up && (!self.keyed || self.sup_done) }
}

impl Default for Rx {
    fn default() -> Self { Rx::new() }
}

impl RxSink for Rx {
    fn on_event(&mut self, frame: &[u8]) {
        let e = match event::parse(frame) { Some(e) => e, None => { self.bad_events += 1; return; } };
        self.events += 1;
        if self.recent.len() == 16 { self.recent.pop_front(); }
        self.recent.push_back((e.event_type, e.status, e.reason));
        if e.event_type == ev::IF { self.if_events += 1; return; }
        if e.ifidx != 0 { return; }
        match e.event_type {
            ev::ESCAN_RESULT => {
                if !self.escan_active { return; }
                if e.status == status::PARTIAL {
                    if let Some((sync, list)) = parse_escan(e.data) {
                        if sync != self.escan_sync { return; }
                        for b in list { merge_bss(&mut self.results, b); }
                    }
                } else {
                    self.escan_done = Some(e.status);
                    self.escan_active = false;
                }
            }
            ev::SET_SSID => {
                self.assoc_status = Some(e.status);
                if e.status != status::SUCCESS && self.joining && self.down.is_none() {
                    self.down = Some(Down::JoinFailed { status: e.status, reason: e.reason });
                }
            }
            ev::LINK => {
                if e.flags & event::FLAG_LINK != 0 {
                    self.link_up = true;
                    self.link_seen = true;
                    self.bssid = e.addr;
                } else {
                    let was = self.link_up || self.joining;
                    self.link_up = false;
                    if was && self.down.is_none() { self.down = Some(Down::LinkLost { reason: e.reason }); }
                }
            }
            ev::ROAM => {
                if e.status == status::SUCCESS && e.addr != [0; 6] { self.bssid = e.addr; }
            }
            ev::PSK_SUP => {
                match e.status {
                    status::FWSUP_COMPLETED => { self.sup_done = true; self.sup_max = e.status; }
                    status::FWSUP_WAIT_M1 | status::FWSUP_PREP_M2 | status::FWSUP_WAIT_M3 | status::FWSUP_PREP_M4
                    | status::FWSUP_WAIT_G1 | status::FWSUP_PREP_G2 if e.reason == 0 => {
                        if !self.sup_done { self.sup_max = self.sup_max.max(e.status); }
                    }
                    _ => {
                        if e.status != status::FWSUP_TIMEOUT && e.status != status::FAIL { self.sup_max = self.sup_max.max(e.status); }
                        if (self.joining || self.link_up) && self.down.is_none() {
                            self.down = Some(Down::SupFailed { status: e.status, reason: e.reason });
                        }
                        self.link_up = false;
                    }
                }
            }
            ev::DEAUTH | ev::DEAUTH_IND | ev::DISASSOC | ev::DISASSOC_IND => {
                let by_ap = matches!(e.event_type, ev::DEAUTH_IND | ev::DISASSOC_IND);
                let r = e.reason as u16;
                let d = if matches!(e.event_type, ev::DEAUTH | ev::DEAUTH_IND) {
                    Down::Deauth { reason: r, by_ap }
                } else {
                    Down::Disassoc { reason: r, by_ap }
                };
                if (self.link_up || self.joining) && self.down.is_none() { self.down = Some(d); }
                self.link_up = false;
            }
            _ => {}
        }
    }

    fn on_data(&mut self, frame: &[u8]) {
        self.rx_frames += 1;
        if self.rx.len() >= RX_QUEUE_MAX {
            self.rx.pop_front();
            self.rx_dropped += 1;
        }
        self.rx.push_back(frame.to_vec());
    }
}

// ── the driver object ──────────────────────────────────────────────────────

enum Wait { Done, Timeout, Aborted }

/// Counters for /proc/net/nic and the boot line.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub scans: u32,
    pub joins: u32,
    pub join_fails: u32,
    pub link_downs: u32,
    pub tx_frames: u64,
    pub tx_errors: u64,
    pub eapol_rx: u32,
    pub eapol_tx: u32,
}

/// One CYW43455 FullMAC interface over a [`Fwil`].
pub struct Brcmf<F: Fwil> {
    pub fw: F,
    pub rx: Rx,
    env: Env,
    pub mac: [u8; 6],
    pub country: [u8; 2],
    connected: bool,
    radio_on: bool,
    sync_id: u16,
    last: Option<JoinReq>,
    pub stats: Stats,
    /// Join knobs (persist across joins; set by `join <knobs>` on the ctl).
    pub opts: JoinOpts,
    /// The firmware's "cap" string, if it answered.
    pub caps: Option<alloc::string::String>,
    /// The last firmware command that refused during a join, with its error.
    pub fail_cmd: alloc::string::String,
    /// Who ran the last keyed join's handshake ("firmware" / "host").
    pub sup_used: &'static str,
    host: Option<HostSup>,
    /// BSSs from the last scan (for the AP's RSN element at join time).
    pub last_bss: Vec<BssEntry>,
    /// RSN/EAPOL diagnostics for /proc/net/nic (never key material).
    pub rsn: RsnDiag,
}

/// What the last keyed join negotiated, for /proc/net/nic. Holds element
/// bytes and EAPOL header fields only; never keys, PMK, PTK or the passphrase.
#[derive(Clone, Debug, Default)]
pub struct RsnDiag {
    pub ap: alloc::string::String,
    pub chosen: alloc::string::String,
    pub assoc_ours: alloc::string::String,
    pub assoc_fw: alloc::string::String,
    pub msg2_ie: alloc::string::String,
    pub msg1: alloc::string::String,
    pub last: alloc::string::String,
    pub secret: alloc::string::String,
    /// Host supplicant, this join: EAPOL frames we sent (message 2 first).
    pub eapol_sent: u32,
    /// Host supplicant, this join: EAPOL frames received.
    pub eapol_rx_join: u32,
    /// Host supplicant, this join: message 1 frames received after our
    /// message 2 (the AP retransmitting message 1 = it could not verify the
    /// message 2 MIC, the signature of a wrong PMK).
    pub msg1_after_msg2: u32,
}

impl RsnDiag {
    pub fn lines(&self) -> [(&'static str, &alloc::string::String); 8] {
        [("rsn_ap", &self.ap), ("rsn_chosen", &self.chosen), ("rsn_assoc_ours", &self.assoc_ours),
         ("rsn_assoc_fw", &self.assoc_fw), ("rsn_msg2_ie", &self.msg2_ie), ("eapol_msg1", &self.msg1),
         ("eapol_last", &self.last), ("secret", &self.secret)]
    }
}

impl<F: Fwil> Brcmf<F> {
    pub fn new(fw: F, env: Env) -> Self {
        Brcmf { fw, rx: Rx::new(), env, mac: [0; 6], country: *b"PT", connected: false, radio_on: false,
                sync_id: 0, last: None, stats: Stats::default(), opts: JoinOpts::default(), caps: None,
                fail_cmd: alloc::string::String::new(), sup_used: "-", host: None,
                last_bss: Vec::new(), rsn: RsnDiag::default() }
    }

    fn log(&self, s: &str) { (self.env.log)(s) }

    fn set_u32(&mut self, c: u32, v: u32) -> Result<(), FwError> { self.fw.cmd_set(c, &v.to_le_bytes()) }
    fn iovar_u32(&mut self, name: &str, v: u32) -> Result<(), FwError> { self.fw.iovar_set(name, &v.to_le_bytes()) }

    /// The init sequence: MAC, country, mpc 0, txglom off, event mask, UP.
    /// Only the MAC read, the event mask and UP are fatal.
    /// Gives the bus back (for a full re-probe).
    pub fn into_fw(self) -> F { self.fw }

    pub fn init(&mut self, country: [u8; 2]) -> Result<[u8; 6], FwError> {
        let mut b = [0u8; 6];
        let n = self.fw.iovar_get("cur_etheraddr", &mut b)?;
        if n < 6 { return Err(FwError::TooLong); }
        self.mac = b;
        self.country = country;
        let mut cap = [0u8; 512];
        match self.fw.iovar_get("cap", &mut cap) {
            Ok(n) => {
                let n = cap[..n.min(cap.len())].iter().position(|&c| c == 0).unwrap_or(n.min(cap.len()));
                let c = alloc::string::String::from_utf8_lossy(&cap[..n]).trim().into();
                self.log(&alloc::format!("caps: {}", c));
                self.caps = Some(c);
            }
            Err(e) => self.log(&alloc::format!("cap refused: {:?}", e)),
        }
        if let Err(e) = self.fw.iovar_set("country", &country_params(country)) {
            self.log(&alloc::format!("country {}{} refused: {:?}; firmware default kept", country[0] as char, country[1] as char, e));
        }
        if let Err(e) = self.iovar_u32("mpc", 0) { self.log(&alloc::format!("mpc 0 refused: {:?}", e)); }
        if let Err(e) = self.iovar_u32("bus:txglom", 0) { self.log(&alloc::format!("bus:txglom 0 refused: {:?}", e)); }
        self.fw.iovar_set("event_msgs", EventMask::station().bytes())?;
        self.set_u32(cmd::UP, 1)?;
        self.radio_on = true;
        if let Err(e) = self.set_u32(cmd::SET_INFRA, 1) { self.log(&alloc::format!("infra 1 refused: {:?}", e)); }
        Ok(self.mac)
    }

    pub fn link(&self) -> Link {
        if self.connected && self.rx.link_up { Link::Up } else if self.rx.joining { Link::Joining } else { Link::Down }
    }
    pub fn connected(&self) -> bool { self.connected }
    pub fn bssid(&self) -> Option<[u8; 6]> { if self.connected { Some(self.rx.bssid) } else { None } }
    pub fn last_join(&self) -> Option<&JoinReq> { self.last.as_ref() }

    /// Service the bus once.
    pub fn pump(&mut self, budget_us: u32) -> Result<(), FwError> { self.fw.poll(&mut self.rx, budget_us) }

    fn wait(&mut self, timeout_ms: u64, obs: &mut dyn JoinObserver, done: fn(&Rx) -> bool,
            mut on_poll: impl FnMut(&Rx, &mut dyn JoinObserver)) -> Result<Wait, FwError> {
        let t0 = (self.env.now_ms)();
        loop {
            let before = self.rx.events;
            self.fw.poll(&mut self.rx, 2_000)?;
            self.host_eapol();
            on_poll(&self.rx, obs);
            if done(&self.rx) { return Ok(Wait::Done); }
            if obs.aborted() { return Ok(Wait::Aborted); }
            if (self.env.now_ms)().saturating_sub(t0) >= timeout_ms { return Ok(Wait::Timeout); }
            if self.rx.events == before { (self.env.sleep_ms)(1); }
        }
    }

    /// Escan: wildcard plus directed probes for `ssids`. Returns the BSSs
    /// (one per BSSID, strongest first). A scan that times out is aborted and
    /// returns what it collected.
    pub fn scan(&mut self, ssids: &[&[u8]], timeout_ms: u64, obs: &mut dyn JoinObserver) -> Result<Vec<BssEntry>, FwError> {
        self.sync_id = self.sync_id.wrapping_add(1).max(1);
        self.rx.escan_sync = self.sync_id;
        self.rx.escan_active = true;
        self.rx.escan_done = None;
        self.rx.results.clear();
        let _ = self.set_u32(cmd::SET_PASSIVE_SCAN, 0);
        if let Err(e) = self.fw.iovar_set("escan", &escan_params(self.sync_id, ESCAN_ACTION_START, ssids)) {
            self.rx.escan_active = false;
            return Err(e);
        }
        self.stats.scans += 1;
        let w = self.wait(timeout_ms, obs, |r| r.escan_done.is_some(), |_, _| {})?;
        if !matches!(w, Wait::Done) {
            let _ = self.fw.iovar_set("escan", &escan_params(self.sync_id, ESCAN_ACTION_ABORT, &[]));
            self.rx.escan_active = false;
            if matches!(w, Wait::Timeout) && self.rx.results.is_empty() { return Err(FwError::Timeout); }
        }
        let mut v = core::mem::take(&mut self.rx.results);
        v.sort_by(|a, b| b.rssi.cmp(&a.rssi));
        self.last_bss = v.clone();
        Ok(v)
    }

    /// The firmware supports its own supplicant (`idsup` in "cap").
    pub fn has_cap(&self, c: &str) -> bool {
        self.caps.as_deref().map_or(false, |s| s.split_ascii_whitespace().any(|w| w == c))
    }

    /// One join step: log and remember the exact command when it is refused.
    fn step(&mut self, what: &str, r: Result<(), FwError>) -> Result<(), FwError> {
        if let Err(e) = r {
            self.fail_cmd = alloc::format!("{} -> {:?}", what, e);
            self.log(&alloc::format!("join: {} refused: {:?}", what, e));
        }
        r
    }

    /// auth / wsec / wpa_auth through the WLC ioctl or the iovar Linux uses
    /// ("auth", "wsec", "wpa_auth"; bsscfg 0 = no prefix).
    fn set_sec(&mut self, ioctl: u32, ioctl_name: &str, iovar: &str, v: u32) -> Result<(), FwError> {
        let path = self.opts.sec;
        if path != SecPath::Iovar {
            let r = self.set_u32(ioctl, v);
            let r = self.step(&alloc::format!("{} {}", ioctl_name, v), r);
            if r.is_ok() || path == SecPath::Ioctl { return r; }
        }
        let r = self.iovar_u32(iovar, v);
        self.step(&alloc::format!("iovar {}={}", iovar, v), r)
    }

    /// Security for a join. Returns true when the host runs the handshake.
    /// The target AP's RSN element from the last scan (by BSSID, else the
    /// strongest BSS with the SSID).
    fn ap_rsn(&self, req: &JoinReq) -> Option<ApRsn> {
        let b = match req.bssid {
            Some(id) => self.last_bss.iter().find(|b| b.bssid == id),
            None => self.last_bss.iter().find(|b| b.ssid == req.ssid),
        }?;
        parse_ap_rsn(find_rsn_ie(&b.ies)?)
    }

    /// The RSN IE we advertise, the AKM, the "mfp" iovar value (if any) and
    /// whether the group cipher is TKIP. Knobs override each auto choice.
    fn choose_rsn(&mut self, ap: Option<&ApRsn>) -> (Vec<u8>, u8, Option<u32>, bool) {
        use crate::wifi_crypto::{rsn_ie_psk_akm, RSN_CAP_MFPC, RSN_CAP_MFPR};
        let o = self.opts;
        let fw_mfp = self.has_cap("mfp");
        let akm = o.akm.unwrap_or(match ap { Some(a) if a.has_akm(6) => 6, _ => 2 });
        // Legacy mfp=N knob maps onto the IE bits unless mfpc/mfpr are set.
        let mfpc = o.mfpc.or(o.mfp.map(|m| m >= 1)).unwrap_or(akm == 6 || (fw_mfp && ap.is_some_and(|a| a.mfpc())));
        let mfpr = o.mfpr.or(o.mfp.map(|m| m == 2)).unwrap_or(false);
        let mfpc = mfpc || mfpr;
        let group_tkip = ap.is_some_and(|a| a.group == 0x000f_ac02);
        let group = if group_tkip { 0x000f_ac02 } else { 0x000f_ac04 };
        let caps = if mfpc { RSN_CAP_MFPC } else { 0 } | if mfpr { RSN_CAP_MFPR } else { 0 };
        let ie = rsn_ie_psk_akm(group, akm, caps).to_vec();
        let mfp = match o.mfp {
            Some(m) => Some(m),
            None if fw_mfp || mfpc => Some(if mfpr { 2 } else if mfpc { 1 } else { 0 }),
            None => None,
        };
        if let Some(a) = ap {
            if !a.has_akm(akm) { self.log(&alloc::format!("join: AKM {} not offered by the AP (akm list {:?})", akm, a.akm)); }
            if a.mfpr() && !mfpc { self.log("join: the AP requires MFP (MFPR) but MFPC is off"); }
            if mfpc && a.group_mgmt.is_some_and(|g| g != 0x000f_ac06) { self.log("join: AP group management cipher is not BIP-CMAC-128"); }
        }
        self.rsn.chosen = alloc::format!("akm={} mfpc={} mfpr={} mfp_iovar={} group={} wpa_auth=0x{:x}",
            akm, mfpc as u8, mfpr as u8, mfp.map(|m| alloc::format!("{}", m)).unwrap_or_else(|| "unset".into()),
            if group_tkip { "tkip" } else { "ccmp" }, if akm == 6 { WPA2_AUTH_PSK_SHA256 } else { WPA2_AUTH_PSK });
        (ie, akm, mfp, group_tkip)
    }

    /// The RSN IE of the association request the firmware sent ("assoc_info"
    /// lengths, then "assoc_req_ies"), as Linux brcmf_get_assoc_ies reads it.
    fn assoc_req_rsn(&mut self) -> Option<Vec<u8>> {
        let mut li = [0u8; 8];
        let n = self.fw.iovar_get("assoc_info", &mut li).ok()?;
        if n < 4 { return None; }
        let req_len = (le32(&li, 0) as usize).min(512);
        if req_len == 0 { return None; }
        let mut buf = alloc::vec![0u8; 512];
        let n = self.fw.iovar_get("assoc_req_ies", &mut buf).ok()?.min(512);
        let b = &buf[..n.max(req_len).min(512)];
        // Usually the IEs start at 0 (SSID first); some builds keep the fixed
        // fields (capability + listen interval, + current AP for reassoc).
        for off in [0usize, 4, 10] {
            if b.get(off) == Some(&0) {
                if let Some(ie) = find_rsn_ie(&b[off..]) { return Some(ie.to_vec()); }
            }
        }
        None
    }

    fn configure_security(&mut self, req: &JoinReq) -> Result<bool, FwError> {
        let key = &req.key;
        let o = self.opts;
        if *key == Key::Open {
            self.set_sec(cmd::SET_AUTH, "WLC_SET_AUTH", "auth", 0)?;
            self.set_sec(cmd::SET_WSEC, "WLC_SET_WSEC", "wsec", WSEC_NONE)?;
            self.set_sec(cmd::SET_WPA_AUTH, "WLC_SET_WPA_AUTH", "wpa_auth", WPA_AUTH_DISABLED)?;
            let _ = self.iovar_u32("sup_wpa", 0);
            return Ok(false);
        }
        // Linux brcmf_cfg80211_connect order: wpaie, wpa_auth, auth, wsec,
        // key mgmt (+ mfp), sup_wpa, PMK, join.
        // The AP's RSN element (scan) decides the AKM and MFP bits we advertise.
        let ap = self.ap_rsn(req);
        self.rsn = RsnDiag::default();
        self.rsn.ap = ap.as_ref().map(|a| a.describe()).unwrap_or_else(|| "unknown (BSS not in the last scan)".into());
        self.rsn.secret = key.describe_shape();
        let (ie, akm, mfp, group_tkip) = self.choose_rsn(ap.as_ref());
        self.rsn.assoc_ours = hex(&ie);
        if o.wpaie {
            let r = self.fw.iovar_set("wpaie", &ie);
            let _ = self.step(&alloc::format!("iovar wpaie (RSN PSK AKM {})", akm), r); // optional
        }
        let wpa_auth = if akm == 6 { WPA2_AUTH_PSK_SHA256 } else { WPA2_AUTH_PSK };
        self.set_sec(cmd::SET_WPA_AUTH, "WLC_SET_WPA_AUTH", "wpa_auth", wpa_auth)?;
        self.set_sec(cmd::SET_AUTH, "WLC_SET_AUTH", "auth", 0)?;
        self.set_sec(cmd::SET_WSEC, "WLC_SET_WSEC", "wsec", if group_tkip { WSEC_AES | WSEC_TKIP } else { WSEC_AES })?;
        if let Some(m) = mfp {
            let r = self.iovar_u32("mfp", m);
            let r = self.step(&alloc::format!("iovar mfp={}", m), r);
            if r.is_err() && (o.mfp.is_some() || o.mfpc.is_some() || o.mfpr.is_some()) { return r.map(|_| false); }
        }
        let host = match o.sup {
            SupMode::Host => true,
            SupMode::Firmware => {
                let r = self.iovar_u32("sup_wpa", 1);
                self.step("iovar sup_wpa=1", r)?;
                false
            }
            SupMode::Auto => {
                let r = self.iovar_u32("sup_wpa", 1);
                if self.step("iovar sup_wpa=1", r).is_err() {
                    self.log(&alloc::format!("join: no firmware supplicant (idsup {}); running the 4-way handshake on the host",
                                             if self.has_cap("idsup") { "advertised" } else { "not in caps" }));
                    true
                } else { false }
            }
        };
        let Some(pmk) = key.psk(&req.ssid) else { return Ok(false) };
        if host {
            let _ = self.iovar_u32("sup_wpa", 0);
            self.host = Some(HostSup { pmk, rsn_ie: ie, akm, group_tkip, sup: None, ap: req.bssid.unwrap_or([0; 6]) });
            self.sup_used = "host";
            return Ok(true);
        }
        let mut pmk = pmk;
        for b in pmk.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) } }
        self.sup_used = "firmware";
        let forms: &[PmkForm] = match o.pmk {
            PmkForm::Auto => &[PmkForm::Raw, PmkForm::Passphrase, PmkForm::Hex],
            PmkForm::Raw => &[PmkForm::Raw],
            PmkForm::Hex => &[PmkForm::Hex],
            PmkForm::Passphrase => &[PmkForm::Passphrase],
        };
        let mut last = Err(FwError::Status(-1));
        for f in forms {
            let Some(mut p) = wsec_pmk(key, &req.ssid, *f) else { continue };
            let r = self.fw.cmd_set(cmd::SET_WSEC_PMK, &p);
            for b in p.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) } }
            let name = match f { PmkForm::Raw => "raw", PmkForm::Hex => "hex", _ => "passphrase" };
            last = self.step(&alloc::format!("WLC_SET_WSEC_PMK pmk={}", name), r);
            if last.is_ok() { break; }
        }
        last.map(|_| false)
    }

    /// Host supplicant: take EAPOL frames off the RX queue, answer them and
    /// install the keys ("wsec_key"). No-op unless a host-handshake join is active.
    fn host_eapol(&mut self) {
        let Some(mut h) = self.host.take() else { return };
        let mut eap: Vec<Vec<u8>> = Vec::new();
        self.rx.rx.retain(|f| {
            if f.len() >= 18 && f[12..14] == ETH_P_PAE { eap.push(f.to_vec()); false } else { true }
        });
        for f in eap {
            self.stats.eapol_rx += 1;
            self.rsn.eapol_rx_join += 1;
            if h.sup.is_none() {
                let mut ap = [0u8; 6];
                ap.copy_from_slice(&f[6..12]);
                if h.ap == [0; 6] { h.ap = ap; }
                let mut snonce = [0u8; 32];
                (self.env.random)(&mut snonce);
                // Message 2 must carry the RSN IE of the association request
                // byte for byte: take the one the firmware actually sent.
                let fw_ie = self.assoc_req_rsn();
                self.rsn.assoc_fw = match &fw_ie { Some(i) => hex(i), None => "unavailable".into() };
                let (ie, src) = match (self.opts.msg2ie, fw_ie) {
                    (Msg2Ie::Ours, _) | (_, None) => (h.rsn_ie.clone(), "ours"),
                    (_, Some(i)) => (i, "fw"),
                };
                if src == "fw" && ie != h.rsn_ie {
                    self.log(&alloc::format!("join: firmware association RSN IE differs from ours (fw {} ours {}); message 2 uses the firmware's",
                                             hex(&ie), hex(&h.rsn_ie)));
                }
                let akm = parse_ap_rsn(&ie).and_then(|r| r.akm.first().copied()).map(|a| (a & 0xff) as u8).unwrap_or(h.akm);
                self.rsn.msg2_ie = alloc::format!("{} akm={} {}", src, akm, hex(&ie));
                self.log(&alloc::format!("host supplicant: message 1 from {}; EAPOL tx via firmware data path (BDC ifidx 0, not the IP stack), own MAC {} (fw cur_etheraddr), wifi0 netctl ifindex {}, data frames rx {} dropped {} (since boot)",
                    fmt_mac6(&h.ap), fmt_mac6(&self.mac), NETCTL_IFINDEX.load(core::sync::atomic::Ordering::Relaxed),
                    self.rx.rx_frames, self.rx.rx_dropped));
                let mut sup = crate::wifi_crypto::Supplicant::new(h.pmk, self.mac, h.ap, ie, snonce);
                sup.set_akm(akm);
                sup.set_group_tkip(h.group_tkip);
                h.sup = Some(sup);
            }
            if let Some(k) = crate::wifi_crypto::parse_eapol_key(&f[14..]) {
                let ki = k.key_info;
                let line = alloc::format!("eapol_ver={} desc={} key_info=0x{:04x} (ver={} pairwise={} install={} ack={} mic={} secure={} encdata={}) key_len={} replay={} key_data_len={}",
                    k.version, k.descriptor_type, ki, ki & 7, (ki >> 3) & 1, (ki >> 6) & 1, (ki >> 7) & 1, (ki >> 8) & 1,
                    (ki >> 9) & 1, (ki >> 12) & 1, k.key_len, u64::from_be_bytes(k.replay_counter), k.key_data.len());
                if ki & 0x0108 == 0x0008 && self.rsn.msg1.is_empty() { self.rsn.msg1 = line.clone(); }
                if ki & 0x0108 == 0x0008 && self.rsn.eapol_sent > 0 { self.rsn.msg1_after_msg2 += 1; }
                self.rsn.last = line;
            }
            let evs = h.sup.as_mut().map(|s| s.on_eapol(&f[14..])).unwrap_or_default();
            for e in evs {
                use crate::wifi_crypto::HsEvent;
                match e {
                    HsEvent::Send(pdu) => {
                        let mut fr = Vec::with_capacity(14 + pdu.len());
                        fr.extend_from_slice(&h.ap);
                        fr.extend_from_slice(&self.mac);
                        fr.extend_from_slice(&ETH_P_PAE);
                        fr.extend_from_slice(&pdu);
                        match self.fw.tx_data(&fr) {
                            Ok(()) => { self.stats.eapol_tx += 1; self.rsn.eapol_sent += 1; }
                            Err(e) => self.log(&alloc::format!("host supplicant: EAPOL tx failed: {:?}", e)),
                        }
                    }
                    HsEvent::InstallPtk(tk) => {
                        // Message 4 must leave unencrypted before the key is in.
                        let _ = self.fw.poll(&mut self.rx, 2_000);
                        (self.env.sleep_ms)(20);
                        let r = self.fw.iovar_set("wsec_key", &wsec_key(0, &tk, Some(h.ap)));
                        let _ = self.step("iovar wsec_key (pairwise)", r);
                    }
                    HsEvent::InstallGtk { key_id, key, .. } => {
                        let r = self.fw.iovar_set("wsec_key", &wsec_key(key_id as u32, &key, None));
                        let _ = self.step(&alloc::format!("iovar wsec_key (group {})", key_id), r);
                    }
                    HsEvent::InstallIgtk { key_id, key, .. } => {
                        // As Linux brcmfmac: BIP key through "wsec_key" at index 4/5.
                        let r = self.fw.iovar_set("wsec_key", &wsec_key(key_id as u32, &key, None));
                        let _ = self.step(&alloc::format!("iovar wsec_key (igtk {})", key_id), r);
                    }
                    HsEvent::Complete => {
                        if !self.rx.sup_done { self.log("host supplicant: 4-way handshake complete"); }
                        self.rx.sup_done = true;
                    }
                    HsEvent::Error(m) => {
                        self.rsn.last = alloc::format!("{} -> {}", self.rsn.last, m);
                        self.log(&alloc::format!("host supplicant: {}", m))
                    }
                }
            }
        }
        self.host = Some(h);
    }

    /// Join `req` (WPA2-PSK through the firmware supplicant, or open) and
    /// wait for the outcome. On success the data path is open.
    /// After a failed keyed join: (message 2 really sent, whether a wrong
    /// PMK is the likely cause). Only the host supplicant sees the EAPOL
    /// exchange; with the firmware one both are unknown (None).
    pub fn handshake_evidence(&self) -> Option<(bool, bool)> {
        if self.sup_used != "host" { return None; }
        Some((self.rsn.eapol_sent > 0, self.rsn.msg1_after_msg2 > 0))
    }

    pub fn join(&mut self, req: JoinReq, obs: &mut dyn JoinObserver, timeout_ms: u64) -> Result<(), JoinFail> {
        self.connected = false;
        self.stats.joins += 1;
        let keyed = req.key != Key::Open;
        self.rx.begin_join(keyed);
        let r = self.join_inner(&req, obs, timeout_ms);
        self.rx.joining = false;
        if r.is_err() { self.host = None; }
        match r {
            Ok(()) => {
                self.connected = true;
                self.last = Some(req);
                Ok(())
            }
            Err(e) => {
                self.stats.join_fails += 1;
                if self.sup_used == "host" {
                    self.log(&alloc::format!("host supplicant: join ended without keys: eapol rx {} tx {} (this join), message 1 after message 2: {}, data frames rx {} dropped {} (since boot)",
                        self.rsn.eapol_rx_join, self.rsn.eapol_sent, self.rsn.msg1_after_msg2, self.rx.rx_frames, self.rx.rx_dropped));
                }
                let _ = self.fw.cmd_set(cmd::DISASSOC, &scb_val(REASON_LEAVING, [0; 6]));
                self.rx.link_up = false;
                self.rx.down = None;
                // Keep the request for a later reconnect unless it was refused for good.
                self.last = Some(req);
                Err(e)
            }
        }
    }

    fn join_inner(&mut self, req: &JoinReq, obs: &mut dyn JoinObserver, timeout_ms: u64) -> Result<(), JoinFail> {
        self.fail_cmd.clear();
        self.host = None;
        self.sup_used = "-";
        let r = self.set_u32(cmd::SET_INFRA, 1);
        self.step("WLC_SET_INFRA 1", r).map_err(JoinFail::Fw)?;
        self.configure_security(req).map_err(JoinFail::Fw)?;
        self.log(&alloc::format!("join: security set ({}; handshake: {})", self.opts.describe(), self.sup_used));
        obs.step(Step::Associating);
        let method = self.opts.method;
        let mut joined = false;
        if method != JoinMethod::SetSsid {
            let r = self.fw.iovar_set("join", &ext_join_params(&req.ssid, req.bssid, req.channel));
            let r = self.step("iovar join (ext join params)", r);
            if r.is_ok() { joined = true; } else if method == JoinMethod::JoinIovar { return r.map_err(JoinFail::Fw); }
        }
        if !joined {
            let r = self.fw.cmd_set(cmd::SET_SSID, &join_params(&req.ssid, req.bssid, req.channel));
            self.step("WLC_SET_SSID (join params)", r).map_err(JoinFail::Fw)?;
            self.fail_cmd.clear();
        }
        let keyed = self.rx.keyed;
        let mut stepped = false;
        let w = self.wait(timeout_ms, obs, |r| r.joined() || r.down.is_some(), |r, o| {
            if keyed && r.link_seen && !stepped { stepped = true; o.step(Step::Handshake); }
        }).map_err(JoinFail::Fw)?;
        if self.rx.joined() && self.rx.down.is_none() { return Ok(()); }
        let msg2 = self.rx.link_seen && keyed;
        match (w, self.rx.down) {
            (_, Some(Down::JoinFailed { status, .. })) if status == status::NO_NETWORKS => Err(JoinFail::NoNetwork),
            (_, Some(Down::JoinFailed { status, reason })) => Err(JoinFail::Assoc { status, reason }),
            (_, Some(Down::Deauth { reason, .. })) | (_, Some(Down::Disassoc { reason, .. })) => {
                if keyed && self.rx.link_seen { Err(JoinFail::Handshake { msg2, reason: Some(reason) }) }
                else { Err(JoinFail::Assoc { status: status::FAIL, reason: reason as u32 }) }
            }
            (_, Some(Down::SupFailed { reason, .. })) => {
                Err(JoinFail::Handshake { msg2: msg2 || self.rx.sup_max >= status::FWSUP_WAIT_M3,
                                          reason: if reason != 0 { Some(reason as u16) } else { None } })
            }
            (_, Some(Down::LinkLost { .. })) => {
                if keyed { Err(JoinFail::Handshake { msg2, reason: None }) } else { Err(JoinFail::Timeout) }
            }
            (Wait::Aborted, None) => Err(JoinFail::Aborted),
            (_, None) => {
                if keyed && self.rx.link_seen { Err(JoinFail::Handshake { msg2, reason: None }) } else { Err(JoinFail::Timeout) }
            }
        }
    }

    /// Rejoin the last network (same SSID, BSSID, key).
    pub fn reconnect(&mut self, obs: &mut dyn JoinObserver, timeout_ms: u64) -> Result<(), JoinFail> {
        match self.last.clone() {
            Some(req) => self.join(req, obs, timeout_ms),
            None => Err(JoinFail::NoNetwork),
        }
    }

    /// Leave the network (WLC_DISASSOC, reason 3). The data path closes at once.
    pub fn disconnect(&mut self) -> Result<(), FwError> {
        let was = self.connected;
        self.connected = false;
        self.host = None;
        self.rx.link_up = false;
        self.rx.down = None;
        if was { self.fw.cmd_set(cmd::DISASSOC, &scb_val(REASON_LEAVING, self.rx.bssid)) } else { Ok(()) }
    }

    /// Associated: service the bus; `Some` when the link went away.
    pub fn poll_link(&mut self, budget_us: u32) -> Result<Option<Down>, FwError> {
        self.fw.poll(&mut self.rx, budget_us)?;
        self.host_eapol();
        if self.connected {
            if let Some(d) = self.rx.down.take() {
                self.connected = false;
                self.rx.link_up = false;
                self.stats.link_downs += 1;
                return Ok(Some(d));
            }
        } else {
            self.rx.down = None;
        }
        Ok(None)
    }

    /// Send one 802.3 frame; refused while not connected.
    pub fn tx(&mut self, frame: &[u8]) -> Result<(), FwError> {
        if !self.connected {
            self.stats.tx_errors += 1;
            return Err(FwError::Status(-1));
        }
        match self.fw.tx_data(frame) {
            Ok(()) => { self.stats.tx_frames += 1; Ok(()) }
            Err(e) => { self.stats.tx_errors += 1; Err(e) }
        }
    }

    /// Next received data frame.
    pub fn take_rx(&mut self) -> Option<Vec<u8>> { self.rx.rx.pop_front() }

    /// RSSI of the associated BSS (dBm).
    pub fn rssi(&mut self) -> Option<i8> {
        if !self.connected { return None; }
        let mut b = [0u8; 10];
        match self.fw.cmd_get(cmd::GET_RSSI, &mut b) {
            Ok(n) if n >= 4 => Some((le32(&b, 0) as i32).clamp(-128, 0) as i8),
            _ => None,
        }
    }

    /// Current TX rate in kbit/s (the firmware reports 500 kbit/s units).
    pub fn rate_kbps(&mut self) -> u32 {
        let mut b = [0u8; 4];
        match self.fw.cmd_get(cmd::GET_RATE, &mut b) { Ok(4) => le32(&b, 0).saturating_mul(500), _ => 0 }
    }

    /// Soft radio switch (WLC_UP / WLC_DOWN).
    pub fn set_radio(&mut self, on: bool) -> Result<(), FwError> {
        if !on && self.connected { let _ = self.disconnect(); }
        self.set_u32(if on { cmd::UP } else { cmd::DOWN }, 1)?;
        self.radio_on = on;
        Ok(())
    }
    pub fn radio_on(&self) -> bool { self.radio_on }
}

/// `brcmf_scb_val_le`: reason + peer.
pub fn scb_val(reason: u32, ea: [u8; 6]) -> [u8; 10] {
    let mut b = [0u8; 10];
    b[0..4].copy_from_slice(&reason.to_le_bytes());
    b[4..10].copy_from_slice(&ea);
    b
}

// ── bring-up and control helpers (shared with brcmf_boot / netdev; host-tested) ──

/// `xx:xx:xx:xx:xx:xx` (or `-` separated) as a usable unicast MAC.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut m = [0u8; 6];
    let mut n = 0;
    for part in s.split(|c| c == ':' || c == '-') {
        if n == 6 || part.len() != 2 { return None; }
        m[n] = u8::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    let usable = n == 6 && m[0] & 1 == 0 && m.iter().any(|&b| b != 0) && m.iter().any(|&b| b != 0xff);
    if usable { Some(m) } else { None }
}

/// The Pi's Wi-Fi MAC from its Ethernet MAC: NIC part + 1 (what the Pi
/// firmware assigns; same rule as `bcm2712_otp::wifi_from_eth`).
pub fn wifi_from_eth(e: &[u8; 6]) -> [u8; 6] {
    let nic = (((e[3] as u32) << 16) | ((e[4] as u32) << 8) | e[5] as u32).wrapping_add(1) & 0xff_ffff;
    [e[0], e[1], e[2], (nic >> 16) as u8, (nic >> 8) as u8, nic as u8]
}

/// Wi-Fi MAC from the kernel command line: `brcmf.mac=` as given, else the
/// Pi firmware's `smsc95xx.macaddr=` (the board Ethernet MAC) + 1. Unlike the
/// DTB's `local-mac-address`, the command line survives a kexec
/// (`--reuse-cmdline`); the kexec'd kernel gets a synthetic DTB without the
/// Wi-Fi node.
pub fn mac_from_cmdline(cmdline: &str) -> Option<([u8; 6], &'static str)> {
    let val = |k: &str| cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix(k).and_then(|r| r.strip_prefix('='))).last();
    if let Some(m) = val("brcmf.mac").and_then(parse_mac) { return Some((m, "cmdline brcmf.mac")); }
    val("smsc95xx.macaddr").and_then(parse_mac).map(|e| (wifi_from_eth(&e), "cmdline smsc95xx.macaddr+1"))
}

/// One `/etc/brcmf.ctl` command (`seq=<n>` tokens from `brcmfctl` ignored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlCmd<'a> {
    Status,
    Scan,
    /// `join [knobs..] [ssid=<rest of line>]`: ssid None = the wifi.conf
    /// network; knobs as in [`JoinOpts::apply`] (they persist).
    Join { ssid: Option<&'a str>, knobs: alloc::vec::Vec<&'a str> },
    Diag(usize),
    /// `retry [knobs]`: the knob words, `seq=` dropped.
    Retry(alloc::vec::Vec<&'a str>),
    /// Anything else (peek/poke, unknown): the command word.
    Other(&'a str),
}

pub fn parse_ctl(text: &str) -> CtlCmd<'_> {
    let mut words = text.split_ascii_whitespace().filter(|w| !w.starts_with("seq="));
    let cmd = words.next().unwrap_or("");
    match cmd {
        "" | "status" => CtlCmd::Status,
        "scan" => CtlCmd::Scan,
        "join" => {
            // Knob words come before `ssid=`, which takes the rest of the line.
            let head = text.find("ssid=").map_or(text, |i| &text[..i]);
            let knobs = head.split_ascii_whitespace().filter(|w| !w.starts_with("seq=")).skip(1).collect();
            CtlCmd::Join { ssid: text.find("ssid=").map(|i| text[i + 5..].trim()).filter(|s| !s.is_empty()), knobs }
        }
        "diag" => CtlCmd::Diag(words.find_map(|w| w.strip_prefix("lines=")).and_then(|v| v.parse().ok()).unwrap_or(32)),
        "retry" => CtlCmd::Retry(words.collect()),
        other => CtlCmd::Other(other),
    }
}

impl<F: Fwil> Brcmf<F> {
    /// One status line of driver counters and the last few firmware events
    /// (`type/status/reason`), for /proc/net/nic and `brcmfctl status`.
    pub fn counters_line(&self) -> alloc::string::String {
        use core::fmt::Write;
        let s = &self.stats;
        let r = &self.rx;
        let mut out = alloc::format!("scans {} joins {} join_fails {} link_downs {} events {} bad_events {} if_events {} rx_frames {} rx_dropped {} tx {} tx_err {}",
            s.scans, s.joins, s.join_fails, s.link_downs, r.events, r.bad_events, r.if_events, r.rx_frames, r.rx_dropped,
            s.tx_frames, s.tx_errors);
        out.push_str(&alloc::format!(" eapol_rx {} eapol_tx {} handshake {}", s.eapol_rx, s.eapol_tx, self.sup_used));
        if !r.recent.is_empty() {
            out.push_str(" recent");
            for (t, st, rs) in r.recent.iter().rev().take(8) { let _ = write!(out, " {}/{}/{}", t, st, rs); }
        }
        out
    }
}

// ── data-path bisection knob (lane piwedge) ────────────────────────────────

/// `dp=on`: normal. `dp=off`: keep the association but never raise wifi0's
/// carrier (received data is drained from the bus and dropped, nothing is
/// sent). `dp=rx`: carrier up and frames delivered, but every transmit is
/// dropped (no DHCP/ARP leaves the box).
pub const DP_ON: u8 = 0;
pub const DP_OFF: u8 = 1;
pub const DP_RX: u8 = 2;

/// Parses `dp <on|off|rx> [yield|noyield]` (ctl) or `dp=<..>` (cmdline form).
/// `yield` makes the manager loop sleep 1 ms on every pass. Returns
/// (mode, yield change), or None if `text` is not a dp command.
pub fn parse_dp(text: &str) -> Option<(Option<u8>, Option<bool>)> {
    // brcmfctl prefixes a `seq=<n>` token; `dp=off,yield` is the cmdline form.
    let mut words = text.split_ascii_whitespace().filter(|w| !w.starts_with("seq="))
        .flat_map(|w| w.split(|c: char| c == ',' || c == '=')).filter(|w| !w.is_empty());
    if words.next() != Some("dp") { return None; }
    let mut mode = None;
    let mut y = None;
    for w in words {
        match w {
            "on" => mode = Some(DP_ON),
            "off" => mode = Some(DP_OFF),
            "rx" => mode = Some(DP_RX),
            "yield" => y = Some(true),
            "noyield" => y = Some(false),
            _ => return None,
        }
    }
    Some((mode, y))
}

/// Bytes of each frame `dump` prints.
pub const DUMP_BYTES: usize = 64;
/// Most frames one `dump` arms.
pub const DUMP_MAX: u32 = 64;

/// Parses `dump [N]` (ctl, after an optional `seq=` token): hex-dump the next
/// N received data frames (default 8, at most [`DUMP_MAX`]) to the kernel log.
pub fn parse_dump(text: &str) -> Option<u32> {
    let mut words = text.split_ascii_whitespace().filter(|w| !w.starts_with("seq="));
    if words.next()? != "dump" { return None; }
    let n = match words.next() { Some(w) => w.parse::<u32>().ok()?, None => 8 };
    Some(n.min(DUMP_MAX))
}

/// One `dump` line: `rxdump <idx> len <L> <hex of the first DUMP_BYTES>`.
/// `parse_dump_line` reads it back (host replay tests).
pub fn fmt_dump_line(idx: u32, frame: &[u8]) -> alloc::string::String {
    let mut s = alloc::format!("rxdump {} len {} ", idx, frame.len());
    for b in frame.iter().take(DUMP_BYTES) {
        let d = b"0123456789abcdef";
        s.push(d[(b >> 4) as usize] as char);
        s.push(d[(b & 15) as usize] as char);
    }
    s
}

/// Inverse of [`fmt_dump_line`] (accepts the `[brcmf] ` log prefix):
/// (full length, captured bytes).
pub fn parse_dump_line(line: &str) -> Option<(usize, Vec<u8>)> {
    let i = line.find("rxdump ")?;
    let mut w = line[i..].split_ascii_whitespace();
    w.next()?; w.next()?;
    if w.next()? != "len" { return None; }
    let len = w.next()?.parse().ok()?;
    let hex = w.next().unwrap_or("");
    if hex.len() % 2 != 0 { return None; }
    let bytes = (0..hex.len() / 2).map(|k| u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).ok()).collect::<Option<Vec<u8>>>()?;
    Some((len, bytes))
}

pub fn dp_mode_name(m: u8) -> &'static str {
    match m { DP_OFF => "off", DP_RX => "rx", _ => "on" }
}

#[cfg(test)]
mod dp_tests {
    use super::*;
    #[test]
    fn dp_knob_parses() {
        assert_eq!(parse_dp("dp off"), Some((Some(DP_OFF), None)));
        assert_eq!(parse_dp("dp=rx"), Some((Some(DP_RX), None)));
        assert_eq!(parse_dp("dp on yield"), Some((Some(DP_ON), Some(true))));
        assert_eq!(parse_dp("dp=off,yield"), Some((Some(DP_OFF), Some(true))));
        assert_eq!(parse_dp("dp noyield"), Some((None, Some(false))));
        assert_eq!(parse_dp("dp"), Some((None, None)));
        assert_eq!(parse_dp("dp bogus"), None);
        assert_eq!(parse_dp("diag"), None);
        assert_eq!(parse_dp("dpx off"), None);
        assert_eq!(parse_dp("seq=3.1234 dp off"), Some((Some(DP_OFF), None)));
        assert_eq!(parse_dp("seq=4.9 dp rx yield"), Some((Some(DP_RX), Some(true))));
        assert_eq!(parse_dp("seq=5.1 diag"), None);
    }
}
