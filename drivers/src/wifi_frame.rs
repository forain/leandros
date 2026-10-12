//! Station-side IEEE 802.11 frame handling for the iwlwifi driver.
//!
//! The driver builds and parses these frames itself rather than relying on
//! the firmware's MLME: scan results (beacons and probe responses, 802.11-2016
//! clauses 9.3.3.3 / 9.3.3.11), open-system authentication (9.3.3.12),
//! (re)association (9.3.3.6 - 9.3.3.8), deauthentication and disassociation
//! (9.3.3.13 / 9.3.3.5), and the data path conversion between 802.11 data
//! MPDUs (clause 9.3.2) and Ethernet II frames, including RFC 1042 / 802.1H
//! LLC/SNAP encapsulation and A-MSDU de-aggregation (9.3.2.2).
//!
//! Everything here is pure computation over byte slices: no hardware access,
//! no unsafe code, no floating point. Every parser bounds-checks its input
//! and returns `None` (or an empty result) on malformed or truncated frames
//! instead of panicking. Frames handled are MPDUs without the trailing FCS.

use alloc::string::String;
use alloc::vec::Vec;

/// Frame Control field: type/subtype values and flag bits.
///
/// Follows the Linux convention: the field is the little-endian `u16` of the
/// first two frame bytes, the type mask is `0x000c` and the subtype mask is
/// `0x00f0`. The `STYPE_*` values include the type bits (`fc & 0x00fc`).
pub mod fc {
    pub const TYPE_MASK: u16 = 0x000c;
    pub const STYPE_MASK: u16 = 0x00fc;

    pub const TYPE_MGMT: u16 = 0x0000;
    pub const TYPE_CTL: u16 = 0x0004;
    pub const TYPE_DATA: u16 = 0x0008;

    pub const STYPE_ASSOC_REQ: u16 = 0x0000;
    pub const STYPE_ASSOC_RESP: u16 = 0x0010;
    pub const STYPE_REASSOC_RESP: u16 = 0x0030;
    pub const STYPE_PROBE_REQ: u16 = 0x0040;
    pub const STYPE_PROBE_RESP: u16 = 0x0050;
    pub const STYPE_BEACON: u16 = 0x0080;
    pub const STYPE_DISASSOC: u16 = 0x00a0;
    pub const STYPE_AUTH: u16 = 0x00b0;
    pub const STYPE_DEAUTH: u16 = 0x00c0;
    pub const STYPE_ACTION: u16 = 0x00d0;

    pub const STYPE_DATA: u16 = 0x0008;
    pub const STYPE_NULLFUNC: u16 = 0x0048;
    pub const STYPE_QOS_DATA: u16 = 0x0088;
    pub const STYPE_QOS_NULLFUNC: u16 = 0x00c8;

    pub const TO_DS: u16 = 0x0100;
    pub const FROM_DS: u16 = 0x0200;
    pub const MORE_FRAG: u16 = 0x0400;
    pub const RETRY: u16 = 0x0800;
    pub const PWR_MGT: u16 = 0x1000;
    pub const MORE_DATA: u16 = 0x2000;
    pub const PROTECTED: u16 = 0x4000;
    pub const ORDER: u16 = 0x8000;
}

/// Capability Information bits (9.4.1.4).
pub const CAP_ESS: u16 = 0x0001;
pub const CAP_PRIVACY: u16 = 0x0010;

/// Information element ids used here.
const IE_SSID: u8 = 0;
const IE_RATES: u8 = 1;
const IE_DS_PARAMS: u8 = 3;
const IE_TIM: u8 = 5;
const IE_RSN: u8 = 48;
const IE_HT_CAPS: u8 = 45;
const IE_EXT_RATES: u8 = 50;
const IE_HT_OP: u8 = 61;
const IE_VHT_CAPS: u8 = 191;
const IE_VENDOR: u8 = 221;

/// Cipher / AKM suite selectors as big-endian `u32` of OUI || type.
const SUITE_CCMP: u32 = 0x000f_ac04;
const AKM_8021X: u32 = 0x000f_ac01;
const AKM_PSK: u32 = 0x000f_ac02;
const AKM_PSK_SHA256: u32 = 0x000f_ac06;
const AKM_SAE: u32 = 0x000f_ac08;

const BROADCAST: [u8; 6] = [0xff; 6];
const MGMT_HDR_LEN: usize = 24;
const LLC_SNAP_RFC1042: [u8; 6] = [0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00];
const LLC_SNAP_BRIDGE_TUNNEL: [u8; 6] = [0xaa, 0xaa, 0x03, 0x00, 0x00, 0xf8];

// ---------------------------------------------------------------------------
// Frame control and header accessors
// ---------------------------------------------------------------------------

/// Frame Control of a frame, or `None` if shorter than two bytes.
pub fn frame_control(frame: &[u8]) -> Option<u16> {
    let b = frame.get(0..2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

pub fn is_mgmt(fc: u16) -> bool {
    fc & fc::TYPE_MASK == fc::TYPE_MGMT
}

pub fn is_data(fc: u16) -> bool {
    fc & fc::TYPE_MASK == fc::TYPE_DATA
}

/// Type and subtype bits of a Frame Control value (`fc & 0x00fc`).
pub fn subtype(fc: u16) -> u16 {
    fc & fc::STYPE_MASK
}

/// 802.11 MAC header length implied by `fc`: 24 bytes, 30 with Address 4
/// (ToDS and FromDS both set), +2 for QoS Control, +4 for HT Control when
/// the Order bit is set on a QoS data frame.
pub fn hdr_len(fc: u16) -> usize {
    let mut len = 24;
    if is_data(fc) {
        if fc & fc::TO_DS != 0 && fc & fc::FROM_DS != 0 {
            len += 6;
        }
        if fc & 0x0080 != 0 {
            len += 2;
            if fc & fc::ORDER != 0 {
                len += 4;
            }
        }
    }
    len
}

fn mac_at(frame: &[u8], off: usize) -> Option<[u8; 6]> {
    let b = frame.get(off..off + 6)?;
    let mut m = [0u8; 6];
    m.copy_from_slice(b);
    Some(m)
}

pub fn addr1(frame: &[u8]) -> Option<[u8; 6]> {
    mac_at(frame, 4)
}

pub fn addr2(frame: &[u8]) -> Option<[u8; 6]> {
    mac_at(frame, 10)
}

pub fn addr3(frame: &[u8]) -> Option<[u8; 6]> {
    mac_at(frame, 16)
}

/// Sequence Control field (fragment number in bits 0-3, sequence in 4-15).
pub fn seq_ctrl(frame: &[u8]) -> Option<u16> {
    let b = frame.get(22..24)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn rd16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn rd32be(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

// ---------------------------------------------------------------------------
// Information elements
// ---------------------------------------------------------------------------

/// Iterator over the information elements of a frame body.
pub struct IeIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for IeIter<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.len() < 2 {
            self.rest = &[];
            return None;
        }
        let id = self.rest[0];
        let len = self.rest[1] as usize;
        if self.rest.len() < 2 + len {
            // Truncated element: stop here.
            self.rest = &[];
            return None;
        }
        let data = &self.rest[2..2 + len];
        self.rest = &self.rest[2 + len..];
        Some((id, data))
    }
}

/// Iterate `(id, data)` over the IEs of `body`, stopping at the first
/// truncated element.
pub fn ies(body: &[u8]) -> IeIter<'_> {
    IeIter { rest: body }
}

/// Data of the first IE with the given id.
pub fn find_ie(body: &[u8], id: u8) -> Option<&[u8]> {
    ies(body).find(|(i, _)| *i == id).map(|(_, d)| d)
}

/// Data of the first vendor-specific IE (id 221) whose OUI and OUI type
/// match `oui_type` (e.g. `[0x00, 0x50, 0xf2, 0x01]` for WPA1). The returned
/// slice is the data following the four OUI/type bytes.
pub fn find_vendor_ie<'a>(body: &'a [u8], oui_type: [u8; 4]) -> Option<&'a [u8]> {
    ies(body)
        .find(|(i, d)| *i == IE_VENDOR && d.len() >= 4 && d[0..4] == oui_type)
        .map(|(_, d)| &d[4..])
}

// ---------------------------------------------------------------------------
// RSN
// ---------------------------------------------------------------------------

/// Parsed RSN information element (9.4.2.24).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RsnInfo {
    pub version: u16,
    pub group: u32,
    pub pairwise: Vec<u32>,
    pub akm: Vec<u32>,
    pub caps: u16,
}

fn rd_suites(d: &[u8], off: &mut usize) -> Option<Vec<u32>> {
    let n = rd16(d, *off)? as usize;
    *off += 2;
    let mut v = Vec::new();
    for _ in 0..n {
        v.push(rd32be(d, *off)?);
        *off += 4;
    }
    Some(v)
}

/// Parse the data of an RSN IE (after id/length). Absent optional fields take
/// the spec defaults: group CCMP, pairwise CCMP, AKM 802.1X. A field that is
/// present but truncated makes the whole element invalid.
pub fn parse_rsn(ie_data: &[u8]) -> Option<RsnInfo> {
    let version = rd16(ie_data, 0)?;
    let mut off = 2;
    let mut info = RsnInfo {
        version,
        group: SUITE_CCMP,
        pairwise: alloc::vec![SUITE_CCMP],
        akm: alloc::vec![AKM_8021X],
        caps: 0,
    };
    if off == ie_data.len() {
        return Some(info);
    }
    info.group = rd32be(ie_data, off)?;
    off += 4;
    if off == ie_data.len() {
        return Some(info);
    }
    info.pairwise = rd_suites(ie_data, &mut off)?;
    if off == ie_data.len() {
        return Some(info);
    }
    info.akm = rd_suites(ie_data, &mut off)?;
    if let Some(c) = rd16(ie_data, off) {
        info.caps = c;
    }
    Some(info)
}

// ---------------------------------------------------------------------------
// Beacon / probe response
// ---------------------------------------------------------------------------

/// A basic service set discovered by scanning.
#[derive(Clone, Debug)]
pub struct Bss {
    pub bssid: [u8; 6],
    pub ssid: Vec<u8>,
    pub channel: u8,
    pub beacon_interval: u16,
    pub capability: u16,
    pub tsf: u64,
    /// Supported Rates + Extended Supported Rates, raw (basic bit kept).
    pub rates: Vec<u8>,
    pub rsn: Option<RsnInfo>,
    pub wpa1: bool,
    pub ht: bool,
    pub vht: bool,
    pub dtim_period: u8,
    /// Filled in by the caller; parsing leaves it 0.
    pub rssi: i8,
}

/// Parse a full beacon or probe-response MPDU (header + body, no FCS).
/// Channel comes from the DS Parameter Set IE, else the HT Operation primary
/// channel, else `rx_channel`.
pub fn parse_beacon(frame: &[u8], rx_channel: u8) -> Option<Bss> {
    let fcv = frame_control(frame)?;
    if !is_mgmt(fcv) {
        return None;
    }
    let st = subtype(fcv);
    if st != fc::STYPE_BEACON && st != fc::STYPE_PROBE_RESP {
        return None;
    }
    let bssid = addr3(frame)?;
    let fixed = frame.get(MGMT_HDR_LEN..MGMT_HDR_LEN + 12)?;
    let mut tsf_b = [0u8; 8];
    tsf_b.copy_from_slice(&fixed[0..8]);
    let tsf = u64::from_le_bytes(tsf_b);
    let beacon_interval = u16::from_le_bytes([fixed[8], fixed[9]]);
    let capability = u16::from_le_bytes([fixed[10], fixed[11]]);
    let body = &frame[MGMT_HDR_LEN + 12..];

    let mut bss = Bss {
        bssid,
        ssid: Vec::new(),
        channel: rx_channel,
        beacon_interval,
        capability,
        tsf,
        rates: Vec::new(),
        rsn: None,
        wpa1: false,
        ht: false,
        vht: false,
        dtim_period: 0,
        rssi: 0,
    };
    let mut have_ssid = false;
    let mut ds_chan: Option<u8> = None;
    let mut ht_chan: Option<u8> = None;
    for (id, d) in ies(body) {
        match id {
            IE_SSID => {
                if !have_ssid {
                    bss.ssid = d.to_vec();
                    have_ssid = true;
                }
            }
            IE_RATES | IE_EXT_RATES => bss.rates.extend_from_slice(d),
            IE_DS_PARAMS => {
                if let Some(&c) = d.first() {
                    ds_chan.get_or_insert(c);
                }
            }
            IE_TIM => {
                if d.len() >= 2 {
                    bss.dtim_period = d[1];
                }
            }
            IE_HT_CAPS => bss.ht = true,
            IE_HT_OP => {
                if let Some(&c) = d.first() {
                    ht_chan.get_or_insert(c);
                }
            }
            IE_VHT_CAPS => bss.vht = true,
            IE_RSN => {
                if bss.rsn.is_none() {
                    bss.rsn = parse_rsn(d);
                }
            }
            IE_VENDOR => {
                if d.len() >= 4 && d[0..4] == [0x00, 0x50, 0xf2, 0x01] {
                    bss.wpa1 = true;
                }
            }
            _ => {}
        }
    }
    if let Some(c) = ds_chan.or(ht_chan) {
        bss.channel = c;
    }
    Some(bss)
}

/// Short human-readable security classification of a BSS.
pub fn security_summary(b: &Bss) -> &'static str {
    if let Some(r) = &b.rsn {
        let has = |s: u32| r.akm.contains(&s);
        let psk = has(AKM_PSK) || has(AKM_PSK_SHA256) || has(0x000f_ac04);
        let sae = has(AKM_SAE) || has(0x000f_ac09);
        let eap = has(AKM_8021X) || has(0x000f_ac03) || has(0x000f_ac05);
        return if psk && sae {
            "WPA2/WPA3-PSK"
        } else if sae {
            "WPA3-SAE"
        } else if psk {
            "WPA2-PSK"
        } else if eap {
            "WPA2-EAP"
        } else {
            "WPA2"
        };
    }
    if b.wpa1 {
        "WPA"
    } else if b.capability & CAP_PRIVACY != 0 {
        "WEP"
    } else {
        "open"
    }
}

/// True when the BSS can be joined with WPA2-PSK + CCMP: an RSN IE with a
/// PSK AKM, CCMP among the pairwise ciphers and CCMP as group cipher.
/// Transition-mode networks advertising PSK next to SAE qualify.
pub fn supports_wpa2_psk_ccmp(b: &Bss) -> bool {
    match &b.rsn {
        Some(r) => {
            (r.akm.contains(&AKM_PSK) || r.akm.contains(&AKM_PSK_SHA256))
                && r.pairwise.contains(&SUITE_CCMP)
                && r.group == SUITE_CCMP
        }
        None => false,
    }
}

/// Printable rendering of an SSID: printable ASCII kept, everything else as
/// `\xNN`; an empty SSID is shown as `<hidden>`.
pub fn ssid_display(ssid: &[u8]) -> String {
    if ssid.is_empty() {
        return String::from("<hidden>");
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::new();
    for &c in ssid {
        if (0x20..0x7f).contains(&c) {
            s.push(c as char);
        } else {
            s.push_str("\\x");
            s.push(HEX[(c >> 4) as usize] as char);
            s.push(HEX[(c & 0xf) as usize] as char);
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Management frame builders / parsers
// ---------------------------------------------------------------------------

fn mgmt_header(out: &mut Vec<u8>, fcv: u16, da: &[u8; 6], sa: &[u8; 6], bssid: &[u8; 6]) {
    out.extend_from_slice(&fcv.to_le_bytes());
    out.extend_from_slice(&[0, 0]); // duration
    out.extend_from_slice(da);
    out.extend_from_slice(sa);
    out.extend_from_slice(bssid);
    out.extend_from_slice(&[0, 0]); // sequence control
}

fn push_ie(out: &mut Vec<u8>, id: u8, data: &[u8]) {
    let n = data.len().min(255);
    out.push(id);
    out.push(n as u8);
    out.extend_from_slice(&data[..n]);
}

/// SSID, Supported Rates (first 8) and Extended Supported Rates (rest).
fn push_ssid_and_rates(out: &mut Vec<u8>, ssid: &[u8], rates: &[u8]) {
    push_ie(out, IE_SSID, &ssid[..ssid.len().min(32)]);
    if !rates.is_empty() {
        let split = rates.len().min(8);
        push_ie(out, IE_RATES, &rates[..split]);
        if rates.len() > split {
            push_ie(out, IE_EXT_RATES, &rates[split..]);
        }
    }
}

/// Open-system authentication request: algorithm 0, sequence 1, status 0.
pub fn build_auth_open(bssid: &[u8; 6], own: &[u8; 6]) -> Vec<u8> {
    let mut v = Vec::with_capacity(30);
    mgmt_header(&mut v, fc::STYPE_AUTH, bssid, own, bssid);
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v
}

/// Fixed fields of an Authentication frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Auth {
    pub alg: u16,
    pub seq: u16,
    pub status: u16,
}

pub fn parse_auth(frame: &[u8]) -> Option<Auth> {
    let fcv = frame_control(frame)?;
    if !is_mgmt(fcv) || subtype(fcv) != fc::STYPE_AUTH {
        return None;
    }
    Some(Auth {
        alg: rd16(frame, MGMT_HDR_LEN)?,
        seq: rd16(frame, MGMT_HDR_LEN + 2)?,
        status: rd16(frame, MGMT_HDR_LEN + 4)?,
    })
}

/// Association Request: capability, listen interval, SSID, Supported Rates
/// (first 8), Extended Supported Rates (rest), then the RSN IE and any extra
/// IEs appended verbatim.
#[allow(clippy::too_many_arguments)]
pub fn build_assoc_req(
    bssid: &[u8; 6],
    own: &[u8; 6],
    capability: u16,
    listen_interval: u16,
    ssid: &[u8],
    rates: &[u8],
    rsn_ie: Option<&[u8]>,
    extra_ies: Option<&[u8]>,
) -> Vec<u8> {
    let mut v = Vec::with_capacity(64 + ssid.len() + rates.len());
    mgmt_header(&mut v, fc::STYPE_ASSOC_REQ, bssid, own, bssid);
    v.extend_from_slice(&capability.to_le_bytes());
    v.extend_from_slice(&listen_interval.to_le_bytes());
    push_ssid_and_rates(&mut v, ssid, rates);
    if let Some(r) = rsn_ie {
        v.extend_from_slice(r);
    }
    if let Some(e) = extra_ies {
        v.extend_from_slice(e);
    }
    v
}

/// Parsed (Re)Association Response.
#[derive(Clone, Debug)]
pub struct AssocResp {
    pub capability: u16,
    pub status: u16,
    /// Association ID with the two top bits stripped.
    pub aid: u16,
    pub rates: Vec<u8>,
    pub ht: bool,
}

pub fn parse_assoc_resp(frame: &[u8]) -> Option<AssocResp> {
    let fcv = frame_control(frame)?;
    if !is_mgmt(fcv) {
        return None;
    }
    let st = subtype(fcv);
    if st != fc::STYPE_ASSOC_RESP && st != fc::STYPE_REASSOC_RESP {
        return None;
    }
    let capability = rd16(frame, MGMT_HDR_LEN)?;
    let status = rd16(frame, MGMT_HDR_LEN + 2)?;
    let aid = rd16(frame, MGMT_HDR_LEN + 4)? & 0x3fff;
    let body = &frame[MGMT_HDR_LEN + 6..];
    let mut rates = Vec::new();
    let mut ht = false;
    for (id, d) in ies(body) {
        match id {
            IE_RATES | IE_EXT_RATES => rates.extend_from_slice(d),
            IE_HT_CAPS => ht = true,
            _ => {}
        }
    }
    Some(AssocResp { capability, status, aid, rates, ht })
}

/// Deauthentication frame from the station to the AP.
pub fn build_deauth(bssid: &[u8; 6], own: &[u8; 6], reason: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(26);
    mgmt_header(&mut v, fc::STYPE_DEAUTH, bssid, own, bssid);
    v.extend_from_slice(&reason.to_le_bytes());
    v
}

/// Reason code of a deauthentication or disassociation frame.
pub fn parse_deauth_reason(frame: &[u8]) -> Option<u16> {
    let fcv = frame_control(frame)?;
    if !is_mgmt(fcv) {
        return None;
    }
    let st = subtype(fcv);
    if st != fc::STYPE_DEAUTH && st != fc::STYPE_DISASSOC {
        return None;
    }
    rd16(frame, MGMT_HDR_LEN)
}

/// Probe Request to the broadcast address (empty `ssid` = wildcard).
pub fn build_probe_req(own: &[u8; 6], ssid: &[u8], rates: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(40 + ssid.len() + rates.len());
    mgmt_header(&mut v, fc::STYPE_PROBE_REQ, &BROADCAST, own, &BROADCAST);
    push_ssid_and_rates(&mut v, ssid, rates);
    v
}

// ---------------------------------------------------------------------------
// Data path
// ---------------------------------------------------------------------------

/// Addresses and payload window of a data frame.
struct DataLayout {
    da: [u8; 6],
    sa: [u8; 6],
    start: usize,
    end: usize,
    /// First byte of QoS Control, when the frame is a QoS data frame.
    qos: Option<u8>,
}

/// Locate DA, SA and the payload of a data frame; `None` for non-data,
/// null-function or malformed frames.
fn data_layout(frame: &[u8], crypto_hdr: usize, trailer: usize) -> Option<DataLayout> {
    let fcv = frame_control(frame)?;
    if !is_data(fcv) || fcv & 0x0040 != 0 {
        // Not data, or a null-function variant (no payload).
        return None;
    }
    let hl = hdr_len(fcv);
    let start = hl.checked_add(crypto_hdr)?;
    let end = frame.len().checked_sub(trailer)?;
    if start > end {
        return None;
    }
    let a1 = addr1(frame)?;
    let a2 = addr2(frame)?;
    let a3 = addr3(frame)?;
    let to_ds = fcv & fc::TO_DS != 0;
    let from_ds = fcv & fc::FROM_DS != 0;
    let (da, sa) = match (to_ds, from_ds) {
        (false, false) => (a1, a2),
        (false, true) => (a1, a3),
        (true, false) => (a3, a2),
        (true, true) => (a3, mac_at(frame, 24)?),
    };
    let qos = if fcv & 0x0080 != 0 {
        let off = if to_ds && from_ds { 30 } else { 24 };
        Some(*frame.get(off)?)
    } else {
        None
    };
    Some(DataLayout { da, sa, start, end, qos })
}

/// Convert one MSDU (DA, SA, LLC/SNAP + payload) to an Ethernet II frame.
fn msdu_to_ethernet(da: &[u8; 6], sa: &[u8; 6], llc: &[u8], out: &mut [u8]) -> Option<usize> {
    if llc.len() < 8 {
        return None;
    }
    if llc[0..6] != LLC_SNAP_RFC1042 && llc[0..6] != LLC_SNAP_BRIDGE_TUNNEL {
        return None;
    }
    let payload = &llc[8..];
    let total = 14 + payload.len();
    if out.len() < total {
        return None;
    }
    out[0..6].copy_from_slice(da);
    out[6..12].copy_from_slice(sa);
    out[12] = llc[6];
    out[13] = llc[7];
    out[14..total].copy_from_slice(payload);
    Some(total)
}

/// Convert a received (hardware-decrypted) data frame to an Ethernet II
/// frame written into `out`; returns its length.
///
/// `crypto_hdr` bytes after the 802.11 header and `trailer` bytes at the end
/// (a CCMP header / MIC still present) are skipped. Null-function frames,
/// A-MSDUs (see [`amsdu_to_ethernet`]) and frames without an RFC 1042 or
/// bridge-tunnel LLC/SNAP header yield `None`.
pub fn data_to_ethernet(
    frame: &[u8],
    crypto_hdr: usize,
    trailer: usize,
    out: &mut [u8],
) -> Option<usize> {
    let l = data_layout(frame, crypto_hdr, trailer)?;
    if let Some(q) = l.qos {
        if q & 0x80 != 0 {
            return None;
        }
    }
    msdu_to_ethernet(&l.da, &l.sa, &frame[l.start..l.end], out)
}

/// De-aggregate an A-MSDU data frame, passing each subframe converted to
/// Ethernet II to `f`. Returns the number of subframes delivered; a
/// malformed trailing subframe ends the walk.
pub fn amsdu_to_ethernet(
    frame: &[u8],
    crypto_hdr: usize,
    trailer: usize,
    f: &mut dyn FnMut(&[u8]),
) -> usize {
    let Some(l) = data_layout(frame, crypto_hdr, trailer) else {
        return 0;
    };
    match l.qos {
        Some(q) if q & 0x80 != 0 => {}
        _ => return 0,
    }
    let body = &frame[l.start..l.end];
    let mut off = 0;
    let mut count = 0;
    let mut buf: Vec<u8> = Vec::new();
    while off + 14 <= body.len() {
        let mut da = [0u8; 6];
        let mut sa = [0u8; 6];
        da.copy_from_slice(&body[off..off + 6]);
        sa.copy_from_slice(&body[off + 6..off + 12]);
        let len = u16::from_be_bytes([body[off + 12], body[off + 13]]) as usize;
        let data_off = off + 14;
        let Some(sub) = body.get(data_off..data_off + len) else {
            break;
        };
        buf.clear();
        buf.resize(14 + len, 0);
        if let Some(n) = msdu_to_ethernet(&da, &sa, sub, &mut buf) {
            f(&buf[..n]);
            count += 1;
        }
        // Each subframe (14-byte header + MSDU) is padded to a multiple of
        // 4 bytes, except the last.
        off += (14 + len + 3) & !3;
    }
    count
}

/// Convert an Ethernet II frame to a ToDS 802.11 data frame for the AP.
///
/// Address 1 is the BSSID, address 2 is `own`, address 3 the Ethernet
/// destination. The payload gets an RFC 1042 LLC/SNAP header (bridge-tunnel
/// for ethertypes 0x80f3 and 0x8137, per 802.1H). `qos_tid` of `Some(t)`
/// produces a QoS Data frame with QoS Control = `t`, else a plain Data
/// frame. Returns the MPDU length, or `None` if `out` is too small or `eth`
/// is shorter than an Ethernet header.
pub fn ethernet_to_data(
    eth: &[u8],
    bssid: &[u8; 6],
    own: &[u8; 6],
    qos_tid: Option<u8>,
    out: &mut [u8],
) -> Option<usize> {
    if eth.len() < 14 {
        return None;
    }
    let hl = if qos_tid.is_some() { 26 } else { 24 };
    let total = hl + 8 + (eth.len() - 14);
    if out.len() < total {
        return None;
    }
    let fcv = match qos_tid {
        Some(_) => fc::STYPE_QOS_DATA,
        None => fc::STYPE_DATA,
    } | fc::TO_DS;
    out[0..2].copy_from_slice(&fcv.to_le_bytes());
    out[2..4].copy_from_slice(&[0, 0]);
    out[4..10].copy_from_slice(bssid);
    out[10..16].copy_from_slice(own);
    out[16..22].copy_from_slice(&eth[0..6]);
    out[22..24].copy_from_slice(&[0, 0]);
    if let Some(t) = qos_tid {
        out[24] = t & 0x0f;
        out[25] = 0;
    }
    let ethertype = u16::from_be_bytes([eth[12], eth[13]]);
    let llc = if ethertype == 0x80f3 || ethertype == 0x8137 {
        &LLC_SNAP_BRIDGE_TUNNEL
    } else {
        &LLC_SNAP_RFC1042
    };
    out[hl..hl + 6].copy_from_slice(llc);
    out[hl + 6] = eth[12];
    out[hl + 7] = eth[13];
    out[hl + 8..total].copy_from_slice(&eth[14..]);
    Some(total)
}

/// True if an Ethernet II frame carries EAPOL (ethertype 0x888e).
pub fn is_eapol(eth: &[u8]) -> bool {
    eth.len() >= 14 && eth[12] == 0x88 && eth[13] == 0x8e
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// Center frequency in MHz of a channel (2.4 GHz 1-14, 5 GHz 32-177), or 0
/// for an invalid channel number.
pub fn channel_to_freq(ch: u8) -> u16 {
    match ch {
        1..=13 => 2407 + 5 * ch as u16,
        14 => 2484,
        32..=177 => 5000 + 5 * ch as u16,
        _ => 0,
    }
}

/// Channel number for a center frequency in MHz, or 0 if not a known channel.
pub fn freq_to_channel(freq: u16) -> u8 {
    match freq {
        2412..=2472 if (freq - 2407) % 5 == 0 => ((freq - 2407) / 5) as u8,
        2484 => 14,
        5160..=5885 if freq % 5 == 0 => ((freq - 5000) / 5) as u8,
        _ => 0,
    }
}

/// True for 5 GHz channel numbers (32-177).
pub fn is_5ghz_channel(ch: u8) -> bool {
    (32..=177).contains(&ch)
}
