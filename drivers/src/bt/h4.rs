//! HCI UART transport (H4): one packet-type byte in front of each HCI packet.
//!
//! Pure code (no hardware): the command encoder, a streaming byte-at-a-time
//! packet reassembler that resynchronises on garbage, and parsers for the
//! events and return parameters the boot bring-up needs.
use alloc::vec::Vec;

pub const H4_CMD: u8 = 0x01;
pub const H4_ACL: u8 = 0x02;
pub const H4_SCO: u8 = 0x03;
pub const H4_EVT: u8 = 0x04;
pub const H4_ISO: u8 = 0x05;

pub const EVT_COMMAND_COMPLETE: u8 = 0x0e;
pub const EVT_COMMAND_STATUS: u8 = 0x0f;
pub const EVT_VENDOR: u8 = 0xff;

pub const OP_RESET: u16 = 0x0c03;
pub const OP_READ_LOCAL_VERSION: u16 = 0x1001;
pub const OP_READ_BD_ADDR: u16 = 0x1009;
/// Broadcom vendor commands (btbcm.c / hci_bcm.c).
pub const OP_BCM_WRITE_BD_ADDR: u16 = 0xfc01;
pub const OP_BCM_UPDATE_BAUD: u16 = 0xfc18;
pub const OP_BCM_DOWNLOAD_MINIDRV: u16 = 0xfc2e;
pub const OP_BCM_WRITE_RAM: u16 = 0xfc4c;
pub const OP_BCM_LAUNCH_RAM: u16 = 0xfc4e;

/// Appends one H4 command packet: 0x01, opcode (LE), parameter length, parameters.
/// Parameters longer than 255 bytes are truncated by the caller's contract (HCI limit).
pub fn encode_cmd(opcode: u16, params: &[u8], out: &mut Vec<u8>) {
    let n = params.len().min(255);
    out.push(H4_CMD);
    out.extend_from_slice(&opcode.to_le_bytes());
    out.push(n as u8);
    out.extend_from_slice(&params[..n]);
}

/// One reassembled packet: H4 type and the HCI packet (header included, type byte excluded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub kind: u8,
    pub data: Vec<u8>,
}

/// Header length and payload-length decoder per packet type.
fn header_len(kind: u8) -> Option<usize> {
    match kind {
        H4_EVT => Some(2),
        H4_ACL => Some(4),
        H4_SCO => Some(3),
        H4_ISO => Some(4),
        H4_CMD => Some(3),
        _ => None,
    }
}

fn payload_len(kind: u8, h: &[u8]) -> usize {
    match kind {
        H4_EVT => h[1] as usize,
        H4_ACL => u16::from_le_bytes([h[2], h[3]]) as usize,
        H4_SCO => h[2] as usize,
        H4_ISO => (u16::from_le_bytes([h[2], h[3]]) & 0x3fff) as usize,
        H4_CMD => h[2] as usize,
        _ => 0,
    }
}

/// Streaming H4 reassembler. Feed received bytes one at a time; a complete
/// packet comes back from [`H4Parser::feed`]. A byte that is not a known packet
/// type while idle is dropped and counted (`garbage`), which resynchronises the
/// stream after line noise or a baud mismatch.
#[derive(Debug, Default)]
pub struct H4Parser {
    kind: u8,
    buf: Vec<u8>,
    need: usize,
    in_header: bool,
    pub garbage: u32,
}

impl H4Parser {
    pub fn new() -> Self { Self::default() }

    /// Drops any partial packet.
    pub fn reset(&mut self) { self.kind = 0; self.buf.clear(); self.need = 0; self.in_header = false; }

    pub fn feed(&mut self, b: u8) -> Option<Packet> {
        if self.kind == 0 {
            match header_len(b) {
                Some(h) => { self.kind = b; self.buf.clear(); self.need = h; self.in_header = true; }
                None => { self.garbage = self.garbage.wrapping_add(1); }
            }
            return None;
        }
        self.buf.push(b);
        if self.buf.len() < self.need { return None; }
        if self.in_header {
            self.in_header = false;
            let p = payload_len(self.kind, &self.buf);
            self.need += p;
            if p > 0 { return None; }
        }
        let kind = self.kind;
        self.kind = 0;
        Some(Packet { kind, data: core::mem::take(&mut self.buf) })
    }
}

/// An HCI event: code and parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// Command Complete: Num_HCI_Command_Packets, opcode, return parameters
    /// (the first return byte is the status for nearly every command).
    CommandComplete { ncmd: u8, opcode: u16, ret: &'a [u8] },
    /// Command Status: status, Num_HCI_Command_Packets, opcode.
    CommandStatus { status: u8, ncmd: u8, opcode: u16 },
    Other { code: u8, params: &'a [u8] },
}

/// Parses an event packet's data (code, length, parameters). `None` if malformed.
pub fn parse_event(data: &[u8]) -> Option<Event<'_>> {
    if data.len() < 2 || data.len() != 2 + data[1] as usize { return None; }
    let p = &data[2..];
    match data[0] {
        EVT_COMMAND_COMPLETE if p.len() >= 3 => Some(Event::CommandComplete {
            ncmd: p[0], opcode: u16::from_le_bytes([p[1], p[2]]), ret: &p[3..],
        }),
        EVT_COMMAND_STATUS if p.len() >= 4 => Some(Event::CommandStatus {
            status: p[0], ncmd: p[1], opcode: u16::from_le_bytes([p[2], p[3]]),
        }),
        EVT_COMMAND_COMPLETE | EVT_COMMAND_STATUS => None,
        code => Some(Event::Other { code, params: p }),
    }
}

/// Read Local Version Information return parameters (status stripped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalVersion {
    pub hci_ver: u8,
    pub hci_rev: u16,
    pub lmp_ver: u8,
    pub manufacturer: u16,
    pub lmp_subver: u16,
}

/// Parses Read Local Version's return parameters (status byte first).
pub fn parse_local_version(ret: &[u8]) -> Option<LocalVersion> {
    if ret.len() < 9 || ret[0] != 0 { return None; }
    Some(LocalVersion {
        hci_ver: ret[1],
        hci_rev: u16::from_le_bytes([ret[2], ret[3]]),
        lmp_ver: ret[4],
        manufacturer: u16::from_le_bytes([ret[5], ret[6]]),
        lmp_subver: u16::from_le_bytes([ret[7], ret[8]]),
    })
}

/// A Bluetooth device address in HCI (little-endian) byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BdAddr(pub [u8; 6]);

impl BdAddr {
    /// The address BCM4345C0 firmware reports before one is set (43:45:c0:00:1f:ac).
    pub const BCM4345C0_PLACEHOLDER: BdAddr = BdAddr([0xac, 0x1f, 0x00, 0xc0, 0x45, 0x43]);

    pub fn is_zero(&self) -> bool { self.0 == [0; 6] }
    /// Broadcom default addresses btbcm.c treats as "not configured".
    pub fn is_placeholder(&self) -> bool {
        // 43:45:c0:00:1f:ac (BCM4345C0), 00:20:70:02:a0:00 (BCM20702A0), 43:30:b1:00:00:00 (BCM4330B1)
        const OTHERS: [[u8; 6]; 2] = [[0x00, 0xa0, 0x02, 0x70, 0x20, 0x00], [0x00, 0x00, 0x00, 0xb1, 0x30, 0x43]];
        self.is_zero() || *self == Self::BCM4345C0_PLACEHOLDER || OTHERS.contains(&self.0)
    }
}

impl core::fmt::Display for BdAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let a = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", a[5], a[4], a[3], a[2], a[1], a[0])
    }
}

/// Parses Read BD_ADDR's return parameters (status byte first).
pub fn parse_bdaddr(ret: &[u8]) -> Option<BdAddr> {
    if ret.len() < 7 || ret[0] != 0 { return None; }
    let mut a = [0u8; 6];
    a.copy_from_slice(&ret[1..7]);
    Some(BdAddr(a))
}

/// Parameters of Broadcom Update UART Baud Rate (0xfc18): two zero bytes, then the rate (LE).
pub fn bcm_baud_params(baud: u32) -> [u8; 6] {
    let b = baud.to_le_bytes();
    [0, 0, b[0], b[1], b[2], b[3]]
}
