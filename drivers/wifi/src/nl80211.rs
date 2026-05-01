//! nl80211 IPC interface — userspace control plane for WiFi.
//!
//! In Linux, nl80211 is a Netlink family.  Here we use the Leandros IPC port system
//! to achieve the same: a userspace WiFi manager sends typed messages to the
//! nl80211 port, which dispatches them to cfg80211 / mac80211.

use ipc::Message;
use ipc::message::MESSAGE_INLINE_BYTES;
use crate::ieee80211::{MacAddr, ETH_ALEN};

// ── nl80211 command codes — mirrors enum nl80211_commands ────────────────────

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nl80211Cmd {
    GetWiphy          = 1,
    SetWiphy          = 2,
    NewWiphy          = 3,
    DelWiphy          = 4,
    GetInterface      = 5,
    SetInterface      = 6,
    NewInterface      = 7,
    DelInterface      = 8,
    GetKey            = 9,
    SetKey            = 10,
    NewKey            = 11,
    DelKey            = 12,
    GetBeacon         = 13,
    SetBeacon         = 14,
    StartAp           = 15,
    StopAp            = 16,
    GetStation        = 17,
    SetStation        = 18,
    NewStation        = 19,
    DelStation        = 20,
    GetMpath          = 21,
    SetMpath          = 22,
    NewMpath          = 23,
    DelMpath          = 24,
    SetBss            = 25,
    SetReg            = 26,
    ReqSetReg         = 27,
    GetMeshConfig     = 28,
    SetMeshConfig     = 29,
    TriggerScan       = 33,
    NewScanResults    = 34,
    ScanAborted       = 35,
    RegChange         = 36,
    Authenticate      = 37,
    Associate         = 38,
    Deauthenticate    = 39,
    Disassociate      = 40,
    MicFailure        = 41,
    RegBeaconHint     = 42,
    JoinIbss          = 43,
    LeaveIbss         = 44,
    Testmode          = 45,
    Connect           = 46,
    Roam              = 47,
    Disconnect        = 48,
    SetWiphyNetns     = 49,
    GetSurvey         = 50,
    NewSurveyResults  = 51,
    SetPmksa          = 52,
    DelPmksa          = 53,
    FlushPmksa        = 54,
    RemainOnChannel   = 55,
    CancelRemainOnChannel = 56,
    SetTxBitrateMask  = 57,
    RegisterAction    = 58,
    Frame             = 59,
    FrameTxStatus     = 60,
    SetPowerSave      = 61,
    GetPowerSave      = 62,
    SetCqm           = 63,
    NotifyCqm        = 64,
    SetChannel        = 65,
    SetWdsPeer        = 66,
    FrameWaitCancel   = 67,
    JoinMesh          = 68,
    LeaveMesh         = 69,
    GetReg            = 70,
    Unknown           = 0xFFFF,
}

impl From<u32> for Nl80211Cmd {
    fn from(v: u32) -> Self {
        // Simple linear scan — replace with a lookup table if performance matters.
        match v {
            1  => Self::GetWiphy,       2  => Self::SetWiphy,
            3  => Self::NewWiphy,       4  => Self::DelWiphy,
            5  => Self::GetInterface,   6  => Self::SetInterface,
            33 => Self::TriggerScan,    34 => Self::NewScanResults,
            37 => Self::Authenticate,   38 => Self::Associate,
            46 => Self::Connect,        48 => Self::Disconnect,
            65 => Self::SetChannel,
            _  => Self::Unknown,
        }
    }
}

// ── Wire message format ───────────────────────────────────────────────────────

/// Message layout in `ipc::Message.data`:
///
/// ```text
/// bytes  0..3   : Nl80211Cmd (LE u32)
/// bytes  4..7   : payload length (LE u32)
/// bytes  8..55  : payload (up to 48 bytes inline; large payloads via shared mem)
/// ```
pub const NL80211_CMD_OFFSET:     usize = 0;
pub const NL80211_LEN_OFFSET:     usize = 4;
pub const NL80211_PAYLOAD_OFFSET: usize = 8;

/// Build an IPC Message encoding a nl80211 command with optional payload.
pub fn encode(cmd: Nl80211Cmd, payload: &[u8]) -> Message {
    let mut msg = Message::empty();
    msg.tag = cmd as u64;
    let plen = payload.len().min(MESSAGE_INLINE_BYTES - NL80211_PAYLOAD_OFFSET);
    msg.data[NL80211_CMD_OFFSET..NL80211_CMD_OFFSET + 4]
        .copy_from_slice(&(cmd as u32).to_le_bytes());
    msg.data[NL80211_LEN_OFFSET..NL80211_LEN_OFFSET + 4]
        .copy_from_slice(&(plen as u32).to_le_bytes());
    msg.data[NL80211_PAYLOAD_OFFSET..NL80211_PAYLOAD_OFFSET + plen]
        .copy_from_slice(&payload[..plen]);
    msg
}

/// Decode a nl80211 command from a received IPC Message.
pub fn decode_cmd(msg: &Message) -> (Nl80211Cmd, &[u8]) {
    let cmd_val = u32::from_le_bytes(
        msg.data[NL80211_CMD_OFFSET..NL80211_CMD_OFFSET + 4]
            .try_into().unwrap_or([0; 4]));
    let plen = u32::from_le_bytes(
        msg.data[NL80211_LEN_OFFSET..NL80211_LEN_OFFSET + 4]
            .try_into().unwrap_or([0; 4])) as usize;
    let plen = plen.min(MESSAGE_INLINE_BYTES - NL80211_PAYLOAD_OFFSET);
    let payload = &msg.data[NL80211_PAYLOAD_OFFSET..NL80211_PAYLOAD_OFFSET + plen];
    (Nl80211Cmd::from(cmd_val), payload)
}

// ── Attribute helpers for common nl80211 payloads ────────────────────────────

/// Encode a TRIGGER_SCAN request: SSID list (one or zero SSIDs).
pub fn encode_scan_req(ssid: Option<&[u8]>) -> [u8; 34] {
    let mut buf = [0u8; 34];
    buf[0] = Nl80211Cmd::TriggerScan as u8;
    if let Some(s) = ssid {
        let len = s.len().min(32);
        buf[2] = len as u8;
        buf[3..3 + len].copy_from_slice(&s[..len]);
    }
    buf
}

/// Encode a CONNECT request: SSID (max 32 bytes) + BSSID (6 bytes, optional).
pub fn encode_connect(ssid: &[u8], bssid: Option<&MacAddr>) -> [u8; 40] {
    let mut buf = [0u8; 40];
    let slen = ssid.len().min(32);
    buf[0] = slen as u8;
    buf[1..1 + slen].copy_from_slice(&ssid[..slen]);
    if let Some(b) = bssid {
        buf[33] = 1; // has_bssid
        buf[34..34 + ETH_ALEN].copy_from_slice(b);
    }
    buf
}

/// Encode a DISCONNECT request with a reason code.
pub fn encode_disconnect(reason: u16) -> [u8; 2] {
    reason.to_le_bytes()
}
