//! Isochronous endpoint descriptors scanned out of a configuration descriptor.
//!
//! Dependency-free (alloc only): shared with the uvc module build by `#[path]`.

use alloc::vec::Vec;

/// One isochronous endpoint of one interface alternate setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IsochEpDesc {
    pub iface: u8,
    pub alt: u8,
    pub class: u8,
    pub subclass: u8,
    pub ep_addr: u8,
    /// bmAttributes (bits 3:2 sync type, 5:4 usage).
    pub attrs: u8,
    /// Raw wMaxPacketSize (HS: bits 12:11 are additional transactions).
    pub mps: u16,
    pub interval: u8,
    /// SuperSpeed companion: bMaxBurst, Mult, wBytesPerInterval.
    pub ss_burst: u8,
    pub ss_mult: u8,
    pub ss_bytes: u16,
}

impl IsochEpDesc {
    pub fn is_in(&self) -> bool { self.ep_addr & 0x80 != 0 }
    /// A feedback endpoint (usage type 01), not a data stream.
    pub fn is_feedback(&self) -> bool { (self.attrs >> 4) & 3 == 1 }
}

/// Every isochronous endpoint in a configuration descriptor, in order.
pub fn scan_isoch(cfg: &[u8]) -> Vec<IsochEpDesc> {
    let mut out: Vec<IsochEpDesc> = Vec::new();
    let (mut iface, mut alt, mut class, mut sub) = (0u8, 0u8, 0u8, 0u8);
    let mut last_isoch = false;
    let mut i = 0usize;
    while i + 2 <= cfg.len() {
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() { break; }
        let d = &cfg[i..i + len];
        match d[1] {
            4 if len >= 9 => {
                iface = d[2]; alt = d[3]; class = d[5]; sub = d[6];
                last_isoch = false;
            }
            5 if len >= 7 => {
                last_isoch = d[3] & 3 == 1;
                if last_isoch {
                    out.push(IsochEpDesc {
                        iface, alt, class, subclass: sub, ep_addr: d[2], attrs: d[3],
                        mps: u16::from_le_bytes([d[4], d[5]]), interval: d[6],
                        ..Default::default()
                    });
                }
            }
            0x30 if len >= 6 && last_isoch => {
                if let Some(e) = out.last_mut() {
                    e.ss_burst = d[2];
                    e.ss_mult = d[3] & 3;
                    e.ss_bytes = u16::from_le_bytes([d[4], d[5]]);
                }
            }
            _ => {}
        }
        i += len;
    }
    out
}
