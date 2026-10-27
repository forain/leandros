//! Broadcom `.hcd` patch files (btbcm.c `btbcm_patchram`).
//!
//! The file is a flat list of HCI command records: opcode (LE16), parameter
//! length (u8), parameters. Each record is sent verbatim as an HCI command
//! after Download Minidriver (0xfc2e); in practice they are Write RAM (0xfc4c)
//! records ending with one Launch RAM (0xfc4e).
use super::h4::{OP_BCM_LAUNCH_RAM, OP_BCM_WRITE_RAM};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HcdError {
    /// Fewer than three header bytes, or fewer parameter bytes than announced, at this offset.
    Truncated(usize),
    /// A record whose opcode is not a vendor (OGF 0x3f) command, at this offset.
    BadOpcode(usize, u16),
    Empty,
    /// No Launch RAM record, or records after it.
    NoLaunch,
}

/// Iterates the records of an `.hcd` image: `(offset, opcode, params)`.
pub struct Records<'a> {
    data: &'a [u8],
    off: usize,
    failed: bool,
}

pub fn records(data: &[u8]) -> Records<'_> { Records { data, off: 0, failed: false } }

impl<'a> Iterator for Records<'a> {
    type Item = Result<(usize, u16, &'a [u8]), HcdError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.off >= self.data.len() { return None; }
        let o = self.off;
        let d = self.data;
        if d.len() - o < 3 { self.failed = true; return Some(Err(HcdError::Truncated(o))); }
        let op = u16::from_le_bytes([d[o], d[o + 1]]);
        let n = d[o + 2] as usize;
        if d.len() - o - 3 < n { self.failed = true; return Some(Err(HcdError::Truncated(o))); }
        self.off = o + 3 + n;
        Some(Ok((o, op, &d[o + 3..o + 3 + n])))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub records: usize,
    pub write_ram: usize,
    pub payload_bytes: usize,
}

/// Checks a whole image before anything is sent to the controller: every
/// record well-formed and a vendor command, exactly one Launch RAM, last.
pub fn validate(data: &[u8]) -> Result<Summary, HcdError> {
    let mut s = Summary::default();
    let mut launched = false;
    for r in records(data) {
        let (o, op, p) = r?;
        if launched { return Err(HcdError::NoLaunch); }
        if op >> 10 != 0x3f { return Err(HcdError::BadOpcode(o, op)); }
        s.records += 1;
        s.payload_bytes += p.len();
        if op == OP_BCM_WRITE_RAM { s.write_ram += 1; }
        if op == OP_BCM_LAUNCH_RAM { launched = true; }
    }
    if s.records == 0 { return Err(HcdError::Empty); }
    if !launched { return Err(HcdError::NoLaunch); }
    Ok(s)
}
