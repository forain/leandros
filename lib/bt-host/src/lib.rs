//! Bluetooth host core for LeandrOS (lane btcore).
//!
//! * [`hci`]  — H4 packet types, opcodes, event codes, command builders.
//! * [`host`] — the sans-IO host state machine: controller init, inquiry and
//!   LE scan, ACL links, L2CAP signalling + basic-mode channels, SSP / legacy
//!   PIN pairing, LE ATT/SMP stubs. Feed it H4 packets and requests, drain
//!   [`host::Host::take_out`] (packets for `/dev/hci0`) and the client events.
//! * [`api`]  — the `/run/bluetooth/btd.sock` frame format (docs/servers/bluetooth.md).
//! * [`keys`] — the `/var/lib/bluetooth/linkkeys` text format.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod api;
pub mod hci;
pub mod host;
pub mod keys;

#[cfg(test)]
mod tests;

use core::fmt;

/// A BD_ADDR in HCI byte order (least significant byte first).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Addr(pub [u8; 6]);

impl Addr {
    pub fn from_slice(b: &[u8]) -> Addr {
        let mut a = [0u8; 6];
        a.copy_from_slice(&b[..6]);
        Addr(a)
    }
    /// Parses the printed form `AA:BB:CC:DD:EE:FF` (most significant first).
    pub fn parse(s: &str) -> Option<Addr> {
        let mut a = [0u8; 6];
        let mut n = 0;
        for part in s.split(':') {
            if n == 6 || part.len() != 2 { return None; }
            a[5 - n] = u8::from_str_radix(part, 16).ok()?;
            n += 1;
        }
        if n == 6 { Some(Addr(a)) } else { None }
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let a = &self.0;
        write!(f, "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", a[5], a[4], a[3], a[2], a[1], a[0])
    }
}

pub(crate) fn le16(b: &[u8], i: usize) -> u16 { u16::from_le_bytes([b[i], b[i + 1]]) }
