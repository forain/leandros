//! xHCI device / input contexts (xHCI 1.2 §6.2), CSZ-aware.
//!
//! Context entries are 32 bytes on CSZ=0 controllers and 64 bytes on CSZ=1
//! controllers (many Intel parts). Rather than fixed `repr(C)` structs —
//! which silently assume 32 — every access goes through an entry index and
//! the controller's context size, into a one-page `DmaBuf`.
//!
//! Output (device) context: entry 0 = slot, entry DCI = endpoint DCI.
//! Input context: entry 0 = input control, entry 1 = slot, entry DCI+1 =
//! endpoint DCI.

use crate::dma::DmaBuf;

// Endpoint types (EP context DW1 [5:3]).
pub const EP_TYPE_BULK_OUT: u32 = 2;
pub const EP_TYPE_INTR_OUT: u32 = 3;
pub const EP_TYPE_CTRL: u32 = 4;
pub const EP_TYPE_BULK_IN: u32 = 6;
pub const EP_TYPE_INTR_IN: u32 = 7;

// Endpoint states (EP context DW0 [2:0]).
pub const EP_STATE_DISABLED: u32 = 0;
pub const EP_STATE_RUNNING: u32 = 1;
pub const EP_STATE_HALTED: u32 = 2;
pub const EP_STATE_STOPPED: u32 = 3;
pub const EP_STATE_ERROR: u32 = 4;

// Port / slot speed IDs (default PSI mapping, Table 7-13).
pub const SPEED_FS: u8 = 1;
pub const SPEED_LS: u8 = 2;
pub const SPEED_HS: u8 = 3;
pub const SPEED_SS: u8 = 4;
pub const SPEED_SSP: u8 = 5;

pub fn speed_name(s: u8) -> &'static str {
    match s {
        SPEED_FS => "full-speed (12M)",
        SPEED_LS => "low-speed (1.5M)",
        SPEED_HS => "high-speed (480M)",
        SPEED_SS => "super-speed (5G)",
        SPEED_SSP => "super-speed+ (10G)",
        _ => "unknown-speed",
    }
}

/// Default control-endpoint max packet size before the device descriptor
/// has been read (USB 2.0 §5.5.3, USB 3.x §9.6.6).
pub fn default_mps0(speed: u8) -> u16 {
    match speed {
        SPEED_LS | SPEED_FS => 8,
        SPEED_HS => 64,
        _ => 512,
    }
}

/// Endpoint context Interval field (xHCI §6.2.3.6) from bInterval, for
/// periodic endpoints. Bulk/control endpoints use 0.
pub fn interval_field(speed: u8, periodic_iso: bool, b_interval: u8) -> u32 {
    match speed {
        SPEED_HS | SPEED_SS | SPEED_SSP => (b_interval.clamp(1, 16) - 1) as u32,
        _ if periodic_iso => (b_interval.clamp(1, 16) as u32 - 1) + 3,
        _ => {
            // FS/LS interrupt: bInterval is in 1 ms frames; the field is
            // log2 of 125 µs units, rounded down, range 3..=10.
            let frames = (b_interval.max(1) as u32) * 8;
            (31 - frames.leading_zeros()).clamp(3, 10)
        }
    }
}

/// View of one context page with a given entry size.
pub struct Ctx<'a> {
    pub buf: &'a DmaBuf,
    pub csz: usize,
    /// 1 for an input context (entry 0 is the input control context), 0 for
    /// an output context.
    pub shift: usize,
}

impl<'a> Ctx<'a> {
    pub fn input(buf: &'a DmaBuf, csz: usize) -> Self { Ctx { buf, csz, shift: 1 } }
    pub fn output(buf: &'a DmaBuf, csz: usize) -> Self { Ctx { buf, csz, shift: 0 } }

    #[inline]
    fn off(&self, entry: usize, dw: usize) -> usize { (entry + self.shift) * self.csz + dw * 4 }

    // Input control context (input contexts only).
    pub fn set_drop(&self, flags: u32) { self.buf.write32(0, flags); }
    pub fn set_add(&self, flags: u32) { self.buf.write32(4, flags); }

    /// Slot context dword.
    pub fn slot(&self, dw: usize) -> u32 { self.buf.read32(self.off(0, dw)) }
    pub fn set_slot(&self, dw: usize, v: u32) { self.buf.write32(self.off(0, dw), v) }
    /// Endpoint context dword, by DCI (1..=31).
    pub fn ep(&self, dci: usize, dw: usize) -> u32 { self.buf.read32(self.off(dci, dw)) }
    pub fn set_ep(&self, dci: usize, dw: usize, v: u32) { self.buf.write32(self.off(dci, dw), v) }

    /// Clear the whole input context (control + slot + 31 endpoints).
    pub fn clear(&self) { self.buf.zero(0, 33 * self.csz); }

    pub fn ep_state(&self, dci: usize) -> u32 { self.ep(dci, 0) & 0x7 }
    pub fn slot_state(&self) -> u32 { self.slot(3) >> 27 }
    pub fn usb_address(&self) -> u8 { self.slot(3) as u8 }

    /// Fill an endpoint context.
    #[allow(clippy::too_many_arguments)]
    pub fn write_ep(&self, dci: usize, ep_type: u32, mps: u16, max_burst: u8,
                    interval: u32, avg_trb_len: u16, max_esit: u32, deq: u64) {
        let cerr = 3u32;
        self.set_ep(dci, 0, (interval & 0xFF) << 16 | ((max_esit >> 16) & 0xFF) << 24);
        self.set_ep(dci, 1, cerr << 1 | ep_type << 3 | (max_burst as u32) << 8 | (mps as u32) << 16);
        self.set_ep(dci, 2, deq as u32);
        self.set_ep(dci, 3, (deq >> 32) as u32);
        self.set_ep(dci, 4, avg_trb_len as u32 | (max_esit & 0xFFFF) << 16);
    }
}

/// Slot context DW0.
pub fn slot_dw0(route: u32, speed: u8, context_entries: u32) -> u32 {
    (route & 0xF_FFFF) | ((speed as u32 & 0xF) << 20) | ((context_entries & 0x1F) << 27)
}

/// Slot context DW1: root hub port number.
pub fn slot_dw1(root_port: u8) -> u32 { (root_port as u32) << 16 }

/// Device Context Index of an endpoint address (bit 7 = IN).
pub fn dci_of(ep_addr: u8) -> usize {
    let n = (ep_addr & 0x0F) as usize;
    if n == 0 { 1 } else { n * 2 + (ep_addr >> 7) as usize }
}
