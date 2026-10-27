//! HCI constants and command builders (Core spec v5.x, vol 4 part E).
use alloc::vec::Vec;

pub const H4_CMD: u8 = 0x01;
pub const H4_ACL: u8 = 0x02;
pub const H4_SCO: u8 = 0x03;
pub const H4_EVT: u8 = 0x04;
pub const H4_ISO: u8 = 0x05;

pub mod op {
    pub const INQUIRY: u16 = 0x0401;
    pub const INQUIRY_CANCEL: u16 = 0x0402;
    pub const CREATE_CONN: u16 = 0x0405;
    pub const DISCONNECT: u16 = 0x0406;
    pub const ACCEPT_CONN: u16 = 0x0409;
    pub const REJECT_CONN: u16 = 0x040a;
    pub const LINK_KEY_REPLY: u16 = 0x040b;
    pub const LINK_KEY_NEG_REPLY: u16 = 0x040c;
    pub const PIN_CODE_REPLY: u16 = 0x040d;
    pub const AUTH_REQUESTED: u16 = 0x0411;
    pub const SET_CONN_ENCRYPT: u16 = 0x0413;
    pub const REMOTE_NAME_REQ: u16 = 0x0419;
    pub const IO_CAP_REPLY: u16 = 0x042b;
    pub const USER_CONFIRM_REPLY: u16 = 0x042c;
    pub const USER_PASSKEY_NEG_REPLY: u16 = 0x042f;
    pub const SET_EVENT_MASK: u16 = 0x0c01;
    pub const RESET: u16 = 0x0c03;
    pub const WRITE_LOCAL_NAME: u16 = 0x0c13;
    pub const WRITE_PAGE_TIMEOUT: u16 = 0x0c18;
    pub const WRITE_SCAN_ENABLE: u16 = 0x0c1a;
    pub const WRITE_CLASS_OF_DEVICE: u16 = 0x0c24;
    pub const WRITE_INQUIRY_MODE: u16 = 0x0c45;
    pub const WRITE_SSP_MODE: u16 = 0x0c56;
    pub const WRITE_LE_HOST_SUPPORTED: u16 = 0x0c6d;
    pub const READ_LOCAL_VERSION: u16 = 0x1001;
    pub const READ_LOCAL_FEATURES: u16 = 0x1003;
    pub const READ_BUFFER_SIZE: u16 = 0x1005;
    pub const READ_BD_ADDR: u16 = 0x1009;
    pub const LE_SET_EVENT_MASK: u16 = 0x2001;
    pub const LE_READ_BUFFER_SIZE: u16 = 0x2002;
    pub const LE_SET_SCAN_PARAMS: u16 = 0x200b;
    pub const LE_SET_SCAN_ENABLE: u16 = 0x200c;
}

pub mod ev {
    pub const INQUIRY_COMPLETE: u8 = 0x01;
    pub const INQUIRY_RESULT: u8 = 0x02;
    pub const CONN_COMPLETE: u8 = 0x03;
    pub const CONN_REQUEST: u8 = 0x04;
    pub const DISCONN_COMPLETE: u8 = 0x05;
    pub const AUTH_COMPLETE: u8 = 0x06;
    pub const REMOTE_NAME: u8 = 0x07;
    pub const ENCRYPT_CHANGE: u8 = 0x08;
    pub const CMD_COMPLETE: u8 = 0x0e;
    pub const CMD_STATUS: u8 = 0x0f;
    pub const HW_ERROR: u8 = 0x10;
    pub const NUM_COMPLETED: u8 = 0x13;
    pub const PIN_REQUEST: u8 = 0x16;
    pub const LINK_KEY_REQUEST: u8 = 0x17;
    pub const LINK_KEY_NOTIFY: u8 = 0x18;
    pub const INQUIRY_RESULT_RSSI: u8 = 0x22;
    pub const EXT_INQUIRY_RESULT: u8 = 0x2f;
    pub const IO_CAP_REQUEST: u8 = 0x31;
    pub const IO_CAP_RESPONSE: u8 = 0x32;
    pub const USER_CONFIRM_REQUEST: u8 = 0x33;
    pub const USER_PASSKEY_REQUEST: u8 = 0x34;
    pub const SSP_COMPLETE: u8 = 0x36;
    pub const LE_META: u8 = 0x3e;
    pub const LE_CONN_COMPLETE: u8 = 0x01;
    pub const LE_ADV_REPORT: u8 = 0x02;
}

/// Linux's default page-1 event mask for 2.1+ controllers.
pub const EVENT_MASK: u64 = 0x3dbf_f807_fffb_ffff;
/// GIAC LAP 0x9E8B33.
pub const GIAC: [u8; 3] = [0x33, 0x8b, 0x9e];
/// Major class Computer, minor Laptop.
pub const CLASS_OF_DEVICE: [u8; 3] = [0x0c, 0x01, 0x00];

/// One H4 command packet.
pub fn cmd(opcode: u16, params: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + params.len());
    v.push(H4_CMD);
    v.extend_from_slice(&opcode.to_le_bytes());
    v.push(params.len() as u8);
    v.extend_from_slice(params);
    v
}

/// One H4 ACL packet. `pb`: 0b10 start (BR/EDR), 0b00 start (LE), 0b01 continuation.
pub fn acl(handle: u16, pb: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + data.len());
    v.push(H4_ACL);
    v.extend_from_slice(&((handle & 0x0fff) | ((pb as u16) << 12)).to_le_bytes());
    v.extend_from_slice(&(data.len() as u16).to_le_bytes());
    v.extend_from_slice(data);
    v
}

/// One H4 event packet (used by tests and the virtual controller).
pub fn event(code: u8, params: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(3 + params.len());
    v.push(H4_EVT);
    v.push(code);
    v.push(params.len() as u8);
    v.extend_from_slice(params);
    v
}

/// Total length of the H4 packet at the front of `b`, if its header is complete.
/// Used to re-frame a byte stream (pty/socketpair stand-ins for /dev/hci0).
pub fn h4_len(b: &[u8]) -> Option<Result<usize, ()>> {
    let t = *b.first()?;
    let (hdr, len) = match t {
        H4_CMD => (4, *b.get(3)? as usize),
        H4_ACL => (5, crate::le16(b.get(..5)?, 3) as usize),
        H4_SCO => (4, *b.get(3)? as usize),
        H4_EVT => (3, *b.get(2)? as usize),
        H4_ISO => (5, (crate::le16(b.get(..5)?, 3) & 0x3fff) as usize),
        _ => return Some(Err(())),
    };
    Some(Ok(hdr + len))
}

/// Walks EIR / AD structures and returns the (complete or shortened) local name.
pub fn eir_name(mut d: &[u8]) -> Option<&[u8]> {
    let mut short = None;
    while d.len() >= 2 {
        let l = d[0] as usize;
        if l == 0 || l + 1 > d.len() { break; }
        let (t, v) = (d[1], &d[2..l + 1]);
        if t == 0x09 { return Some(v); }
        if t == 0x08 { short = Some(v); }
        d = &d[l + 1..];
    }
    short
}

/// Maps an HCI error code to a btd status code (docs/servers/bluetooth.md).
pub fn status_of(hci_err: u8) -> u8 {
    match hci_err {
        0x00 => 0,
        0x04 => 2,
        0x05 | 0x06 | 0x18 | 0x29 => 3,
        0x08 | 0x13 | 0x14 | 0x15 | 0x16 | 0x22 => 10,
        0x0b | 0x0c => 7,
        0x07 | 0x0d => 6,
        0x0e | 0x0f | 0x1a => 4,
        _ => 10,
    }
}
