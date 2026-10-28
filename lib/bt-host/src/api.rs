//! The btd client wire format (`/run/bluetooth/btd.sock`, docs/servers/bluetooth.md):
//! frames `len:u16 LE | op:u8 | payload`, `len` counting op + payload.
use crate::{le16, Addr};
use alloc::string::String;
use alloc::vec::Vec;

pub mod status {
    pub const OK: u8 = 0;
    pub const NOT_READY: u8 = 1;
    pub const PAGE_TIMEOUT: u8 = 2;
    pub const AUTH_FAILED: u8 = 3;
    pub const REFUSED: u8 = 4;
    pub const SECURITY_BLOCK: u8 = 5;
    pub const NO_RESOURCES: u8 = 6;
    pub const BUSY: u8 = 7;
    pub const BAD_REQUEST: u8 = 8;
    pub const TIMEOUT: u8 = 9;
    pub const LINK_LOST: u8 = 10;
    pub const LOCAL_CLOSE: u8 = 11;

    pub fn name(s: u8) -> &'static str {
        match s {
            0 => "ok", 1 => "no controller", 2 => "page timeout", 3 => "authentication failed",
            4 => "refused", 5 => "security block", 6 => "no resources", 7 => "busy",
            8 => "bad request", 9 => "timeout", 10 => "link lost", 11 => "closed", _ => "error",
        }
    }
}

pub mod op {
    pub const CONNECT: u8 = 0x01;
    pub const LISTEN: u8 = 0x02;
    pub const SEND: u8 = 0x03;
    pub const CLOSE: u8 = 0x04;
    pub const SCAN: u8 = 0x10;
    pub const DEVICES: u8 = 0x11;
    pub const PAIR: u8 = 0x12;
    pub const REMOVE: u8 = 0x13;
    pub const INFO: u8 = 0x14;
    pub const DISCONNECT: u8 = 0x15;
    pub const CONNECT_RESULT: u8 = 0x81;
    pub const RECV: u8 = 0x83;
    pub const CLOSED: u8 = 0x84;
    pub const INCOMING: u8 = 0x85;
    pub const LINK: u8 = 0x86;
    pub const DEVICE: u8 = 0x90;
    pub const RESULT: u8 = 0x91;
    pub const INFO_RSP: u8 = 0x92;
}

pub mod dev_flags {
    pub const PAIRED: u8 = 1;
    pub const CONNECTED: u8 = 2;
    pub const LE: u8 = 4;
    pub const LE_RANDOM: u8 = 8;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub addr: Addr,
    pub flags: u8,
    pub rssi: i8,
    pub class: u32,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    // client -> btd
    Connect { req: u16, addr: Addr, psm: u16 },
    Listen { psm: u16 },
    Send { cid: u16, data: Vec<u8> },
    Close { cid: u16 },
    Scan { secs: u8 },
    Devices,
    Pair { addr: Addr },
    Remove { addr: Addr },
    Info,
    Disconnect { addr: Addr },
    // btd -> client
    ConnectResult { req: u16, status: u8, cid: u16, peer_mtu: u16 },
    Recv { cid: u16, data: Vec<u8> },
    Closed { cid: u16, reason: u8 },
    Incoming { cid: u16, addr: Addr, psm: u16, peer_mtu: u16 },
    /// A BR/EDR ACL came up or went down; `incoming` = the peer made it.
    Link { addr: Addr, up: bool, incoming: bool },
    Device(DeviceInfo),
    Result { op: u8, status: u8 },
    InfoRsp { addr: Addr, hci_ver: u8, lmp_subver: u16, manufacturer: u16, state: u8, name: String },
}

impl Msg {
    /// Appends this message as one frame to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0, 0]);
        let w16 = |o: &mut Vec<u8>, v: u16| o.extend_from_slice(&v.to_le_bytes());
        match self {
            Msg::Connect { req, addr, psm } => { out.push(op::CONNECT); w16(out, *req); out.extend_from_slice(&addr.0); w16(out, *psm); }
            Msg::Listen { psm } => { out.push(op::LISTEN); w16(out, *psm); }
            Msg::Send { cid, data } => { out.push(op::SEND); w16(out, *cid); out.extend_from_slice(data); }
            Msg::Close { cid } => { out.push(op::CLOSE); w16(out, *cid); }
            Msg::Scan { secs } => { out.push(op::SCAN); out.push(*secs); }
            Msg::Devices => out.push(op::DEVICES),
            Msg::Pair { addr } => { out.push(op::PAIR); out.extend_from_slice(&addr.0); }
            Msg::Remove { addr } => { out.push(op::REMOVE); out.extend_from_slice(&addr.0); }
            Msg::Info => out.push(op::INFO),
            Msg::Disconnect { addr } => { out.push(op::DISCONNECT); out.extend_from_slice(&addr.0); }
            Msg::ConnectResult { req, status, cid, peer_mtu } => {
                out.push(op::CONNECT_RESULT); w16(out, *req); out.push(*status); w16(out, *cid); w16(out, *peer_mtu);
            }
            Msg::Recv { cid, data } => { out.push(op::RECV); w16(out, *cid); out.extend_from_slice(data); }
            Msg::Closed { cid, reason } => { out.push(op::CLOSED); w16(out, *cid); out.push(*reason); }
            Msg::Incoming { cid, addr, psm, peer_mtu } => {
                out.push(op::INCOMING); w16(out, *cid); out.extend_from_slice(&addr.0); w16(out, *psm); w16(out, *peer_mtu);
            }
            Msg::Link { addr, up, incoming } => {
                out.push(op::LINK); out.extend_from_slice(&addr.0); out.push(*up as u8); out.push(*incoming as u8);
            }
            Msg::Device(d) => {
                out.push(op::DEVICE); out.extend_from_slice(&d.addr.0); out.push(d.flags); out.push(d.rssi as u8);
                out.extend_from_slice(&d.class.to_le_bytes()[..3]); out.extend_from_slice(d.name.as_bytes());
            }
            Msg::Result { op: o, status } => { out.push(op::RESULT); out.push(*o); out.push(*status); }
            Msg::InfoRsp { addr, hci_ver, lmp_subver, manufacturer, state, name } => {
                out.push(op::INFO_RSP); out.extend_from_slice(&addr.0); out.push(*hci_ver); w16(out, *lmp_subver);
                w16(out, *manufacturer); out.push(*state); out.extend_from_slice(name.as_bytes());
            }
        }
        let len = (out.len() - start - 2) as u16;
        out[start..start + 2].copy_from_slice(&len.to_le_bytes());
    }

    /// Decodes one frame body (`op | payload`, without the length prefix).
    pub fn decode(b: &[u8]) -> Option<Msg> {
        let (&o, p) = b.split_first()?;
        let need = |n: usize| if p.len() >= n { Some(()) } else { None };
        let addr = |i: usize| Addr::from_slice(&p[i..i + 6]);
        let text = |i: usize| String::from_utf8_lossy(&p[i..]).into_owned();
        Some(match o {
            op::CONNECT => { need(10)?; Msg::Connect { req: le16(p, 0), addr: addr(2), psm: le16(p, 8) } }
            op::LISTEN => { need(2)?; Msg::Listen { psm: le16(p, 0) } }
            op::SEND => { need(2)?; Msg::Send { cid: le16(p, 0), data: p[2..].to_vec() } }
            op::CLOSE => { need(2)?; Msg::Close { cid: le16(p, 0) } }
            op::SCAN => { need(1)?; Msg::Scan { secs: p[0] } }
            op::DEVICES => Msg::Devices,
            op::PAIR => { need(6)?; Msg::Pair { addr: addr(0) } }
            op::REMOVE => { need(6)?; Msg::Remove { addr: addr(0) } }
            op::INFO => Msg::Info,
            op::DISCONNECT => { need(6)?; Msg::Disconnect { addr: addr(0) } }
            op::CONNECT_RESULT => { need(7)?; Msg::ConnectResult { req: le16(p, 0), status: p[2], cid: le16(p, 3), peer_mtu: le16(p, 5) } }
            op::RECV => { need(2)?; Msg::Recv { cid: le16(p, 0), data: p[2..].to_vec() } }
            op::CLOSED => { need(3)?; Msg::Closed { cid: le16(p, 0), reason: p[2] } }
            op::INCOMING => { need(12)?; Msg::Incoming { cid: le16(p, 0), addr: addr(2), psm: le16(p, 8), peer_mtu: le16(p, 10) } }
            op::LINK => { need(8)?; Msg::Link { addr: addr(0), up: p[6] != 0, incoming: p[7] != 0 } }
            op::DEVICE => {
                need(11)?;
                Msg::Device(DeviceInfo { addr: addr(0), flags: p[6], rssi: p[7] as i8,
                    class: p[8] as u32 | (p[9] as u32) << 8 | (p[10] as u32) << 16, name: text(11) })
            }
            op::RESULT => { need(2)?; Msg::Result { op: p[0], status: p[1] } }
            op::INFO_RSP => {
                need(12)?;
                Msg::InfoRsp { addr: addr(0), hci_ver: p[6], lmp_subver: le16(p, 7), manufacturer: le16(p, 9), state: p[11], name: text(12) }
            }
            _ => return None,
        })
    }
}

/// Accumulates stream bytes and yields whole frames.
#[derive(Default)]
pub struct FrameBuf {
    buf: Vec<u8>,
}

impl FrameBuf {
    pub fn push(&mut self, b: &[u8]) { self.buf.extend_from_slice(b); }
    /// Next decoded frame: `Some(Some(msg))`, `Some(None)` for a malformed/unknown frame
    /// (consumed), `None` when more bytes are needed.
    pub fn next(&mut self) -> Option<Option<Msg>> {
        if self.buf.len() < 2 { return None; }
        let len = le16(&self.buf, 0) as usize;
        if self.buf.len() < 2 + len { return None; }
        let m = Msg::decode(&self.buf[2..2 + len]);
        self.buf.drain(..2 + len);
        Some(m)
    }
}
