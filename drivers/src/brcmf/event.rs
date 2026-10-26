//! Firmware events: the bcmeth header and `brcmf_event_msg` that follow an
//! Ethernet header with ethertype 0x886c (all event-header fields are
//! big-endian), and the `event_msgs` mask the host enables them with.
//!
//! Layout of one event frame as `RxSink::on_event` gets it (BCDC stripped):
//!
//! ```text
//!  0  ether_header   dst[6] src[6] type=0x886c (BE)
//! 14  bcmeth_hdr     subtype=0x8001 (BE) length (BE) version=0 oui=00:10:18
//!                    usr_subtype=1 (BE, BCMILCP_BCM_SUBTYPE_EVENT)
//! 24  brcmf_event_msg_be
//!                    version u16, flags u16, event_type u32, status u32,
//!                    reason u32, auth_type u32, datalen u32, addr[6],
//!                    ifname[16], ifidx u8, bsscfgidx u8        (48 bytes)
//! 72  event data     `datalen` bytes; its own fields are little-endian
//! ```

use alloc::vec::Vec;

pub const ETH_P_LINK_CTL: u16 = 0x886c;
pub const BCMILCP_SUBTYPE_VENDOR_LONG: u16 = 0x8001;
pub const BRCM_OUI: [u8; 3] = [0x00, 0x10, 0x18];
pub const BCMILCP_BCM_SUBTYPE_EVENT: u16 = 1;
pub const ETH_HDR_LEN: usize = 14;
pub const BCMETH_HDR_LEN: usize = 10;
pub const EVENT_MSG_LEN: usize = 48;
/// Offset of the event data from the start of the frame.
pub const EVENT_HDR_LEN: usize = ETH_HDR_LEN + BCMETH_HDR_LEN + EVENT_MSG_LEN;

/// Event types (`BRCMF_E_*`).
pub mod ev {
    pub const SET_SSID: u32 = 0;
    pub const JOIN: u32 = 1;
    pub const AUTH: u32 = 3;
    pub const DEAUTH: u32 = 5;
    pub const DEAUTH_IND: u32 = 6;
    pub const ASSOC: u32 = 7;
    pub const REASSOC: u32 = 9;
    pub const DISASSOC: u32 = 11;
    pub const DISASSOC_IND: u32 = 12;
    pub const LINK: u32 = 16;
    pub const ROAM: u32 = 19;
    pub const PSK_SUP: u32 = 46;
    pub const IF: u32 = 54;
    pub const ESCAN_RESULT: u32 = 69;
}

/// Event status values (`BRCMF_E_STATUS_*`).
pub mod status {
    pub const SUCCESS: u32 = 0;
    pub const FAIL: u32 = 1;
    pub const TIMEOUT: u32 = 2;
    pub const NO_NETWORKS: u32 = 3;
    pub const ABORT: u32 = 4;
    pub const NO_ACK: u32 = 5;
    pub const UNSOLICITED: u32 = 6;
    pub const ATTEMPT: u32 = 7;
    pub const PARTIAL: u32 = 8;
    pub const NEWSCAN: u32 = 9;
    pub const NEWASSOC: u32 = 10;
    // PSK_SUP (firmware supplicant) states.
    pub const FWSUP_WAIT_M1: u32 = 4;
    pub const FWSUP_PREP_M2: u32 = 5;
    pub const FWSUP_COMPLETED: u32 = 6;
    pub const FWSUP_TIMEOUT: u32 = 7;
    pub const FWSUP_WAIT_M3: u32 = 8;
    pub const FWSUP_PREP_M4: u32 = 9;
    pub const FWSUP_WAIT_G1: u32 = 10;
    pub const FWSUP_PREP_G2: u32 = 11;
}

/// `flags` bit: the link is up (LINK event).
pub const FLAG_LINK: u16 = 0x01;

/// `brcmf_if_event.action`.
pub mod if_action {
    pub const ADD: u8 = 1;
    pub const DEL: u8 = 2;
    pub const CHANGE: u8 = 3;
}

pub fn name(t: u32) -> &'static str {
    match t {
        ev::SET_SSID => "SET_SSID",
        ev::JOIN => "JOIN",
        ev::AUTH => "AUTH",
        ev::DEAUTH => "DEAUTH",
        ev::DEAUTH_IND => "DEAUTH_IND",
        ev::ASSOC => "ASSOC",
        ev::REASSOC => "REASSOC",
        ev::DISASSOC => "DISASSOC",
        ev::DISASSOC_IND => "DISASSOC_IND",
        ev::LINK => "LINK",
        ev::ROAM => "ROAM",
        ev::PSK_SUP => "PSK_SUP",
        ev::IF => "IF",
        ev::ESCAN_RESULT => "ESCAN_RESULT",
        _ => "?",
    }
}

/// One parsed firmware event; `data` borrows the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event<'a> {
    pub version: u16,
    pub flags: u16,
    pub event_type: u32,
    pub status: u32,
    pub reason: u32,
    pub auth_type: u32,
    pub addr: [u8; 6],
    pub ifname: [u8; 16],
    pub ifidx: u8,
    pub bsscfgidx: u8,
    pub data: &'a [u8],
}

fn be16(b: &[u8], o: usize) -> u16 { u16::from_be_bytes([b[o], b[o + 1]]) }
fn be32(b: &[u8], o: usize) -> u32 { u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }

/// Parse an event frame. `None` when it is not a Broadcom event (wrong
/// ethertype, OUI or subtype) or is truncated. A `datalen` past the end of
/// the frame is clamped to what is there.
pub fn parse(frame: &[u8]) -> Option<Event<'_>> {
    if frame.len() < EVENT_HDR_LEN { return None; }
    if be16(frame, 12) != ETH_P_LINK_CTL { return None; }
    let h = &frame[ETH_HDR_LEN..];
    if be16(h, 0) != BCMILCP_SUBTYPE_VENDOR_LONG { return None; }
    if h[5..8] != BRCM_OUI { return None; }
    if be16(h, 8) != BCMILCP_BCM_SUBTYPE_EVENT { return None; }
    let m = &frame[ETH_HDR_LEN + BCMETH_HDR_LEN..];
    let datalen = be32(m, 20) as usize;
    let mut addr = [0u8; 6];
    addr.copy_from_slice(&m[24..30]);
    let mut ifname = [0u8; 16];
    ifname.copy_from_slice(&m[30..46]);
    let rest = &frame[EVENT_HDR_LEN..];
    Some(Event {
        version: be16(m, 0),
        flags: be16(m, 2),
        event_type: be32(m, 4),
        status: be32(m, 8),
        reason: be32(m, 12),
        auth_type: be32(m, 16),
        addr,
        ifname,
        ifidx: m[46],
        bsscfgidx: m[47],
        data: &rest[..datalen.min(rest.len())],
    })
}

/// Encode an event frame (the inverse of [`parse`]); used by tests and mocks.
pub fn build(event_type: u32, status: u32, reason: u32, flags: u16, addr: [u8; 6], data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(EVENT_HDR_LEN + data.len());
    f.extend_from_slice(&[0xff; 6]);
    f.extend_from_slice(&addr);
    f.extend_from_slice(&ETH_P_LINK_CTL.to_be_bytes());
    f.extend_from_slice(&BCMILCP_SUBTYPE_VENDOR_LONG.to_be_bytes());
    f.extend_from_slice(&((EVENT_MSG_LEN + data.len() + 4) as u16).to_be_bytes());
    f.push(0);
    f.extend_from_slice(&BRCM_OUI);
    f.extend_from_slice(&BCMILCP_BCM_SUBTYPE_EVENT.to_be_bytes());
    f.extend_from_slice(&2u16.to_be_bytes());
    f.extend_from_slice(&flags.to_be_bytes());
    for v in [event_type, status, reason, 0, data.len() as u32] { f.extend_from_slice(&v.to_be_bytes()); }
    f.extend_from_slice(&addr);
    let mut ifname = [0u8; 16];
    ifname[..5].copy_from_slice(b"wlan0");
    f.extend_from_slice(&ifname);
    f.push(0);
    f.push(0);
    f.extend_from_slice(data);
    f
}

/// `brcmf_if_event`: the firmware added, removed or changed an interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IfEvent {
    pub ifidx: u8,
    pub action: u8,
    pub flags: u8,
    pub bsscfgidx: u8,
    pub role: u8,
}

pub fn parse_if(data: &[u8]) -> Option<IfEvent> {
    if data.len() < 5 { return None; }
    Some(IfEvent { ifidx: data[0], action: data[1], flags: data[2], bsscfgidx: data[3], role: data[4] })
}

/// Bytes in the `event_msgs` mask (`WL_EVENTING_MASK_LEN` of the 7.45
/// firmware family the CYW43455 runs; events 0..127).
pub const MASK_LEN: usize = 16;

/// Builder for the `event_msgs` iovar: bit `n` enables event type `n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventMask(pub [u8; MASK_LEN]);

impl EventMask {
    pub const fn none() -> Self { EventMask([0; MASK_LEN]) }
    pub fn set(mut self, t: u32) -> Self {
        if (t as usize) < MASK_LEN * 8 { self.0[t as usize / 8] |= 1 << (t % 8); }
        self
    }
    pub fn clear(mut self, t: u32) -> Self {
        if (t as usize) < MASK_LEN * 8 { self.0[t as usize / 8] &= !(1 << (t % 8)); }
        self
    }
    pub fn has(&self, t: u32) -> bool { (t as usize) < MASK_LEN * 8 && self.0[t as usize / 8] & (1 << (t % 8)) != 0 }
    pub fn bytes(&self) -> &[u8] { &self.0 }
    /// What a station needs: join progress, link, the supplicant, scans,
    /// interfaces, and every way the link can go away.
    pub fn station() -> Self {
        let mut m = Self::none();
        for t in [ev::SET_SSID, ev::JOIN, ev::AUTH, ev::DEAUTH, ev::DEAUTH_IND, ev::ASSOC, ev::REASSOC, ev::DISASSOC,
                  ev::DISASSOC_IND, ev::LINK, ev::ROAM, ev::PSK_SUP, ev::IF, ev::ESCAN_RESULT] {
            m = m.set(t);
        }
        m
    }
}
