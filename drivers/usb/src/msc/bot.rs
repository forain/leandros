//! Bulk-Only Transport framing and recovery (USB Mass Storage BOT 1.0).

use super::transport::{BulkTransport, UsbErr};

pub const CBW_SIG: u32 = 0x4342_5355; // "USBC"
pub const CSW_SIG: u32 = 0x5342_5355; // "USBS"
pub const CBW_LEN: usize = 31;
pub const CSW_LEN: usize = 13;

pub const CSW_OK: u8 = 0;
pub const CSW_FAILED: u8 = 1;
pub const CSW_PHASE: u8 = 2;

const REQ_RESET: u8 = 0xFF;
const REQ_GET_MAX_LUN: u8 = 0xFE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotErr {
    /// Underlying pipe error (reset recovery already performed if required).
    Transport(UsbErr),
    /// CSW was malformed or its tag did not match; reset recovery performed.
    BadCsw,
    /// Device reported a phase error; reset recovery performed.
    Phase,
}

/// Direction and buffer of the data stage.
pub enum Data<'a> {
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

impl<'a> Data<'a> {
    pub fn reborrow(&mut self) -> Data<'_> {
        match self {
            Data::None => Data::None,
            Data::In(b) => Data::In(&mut **b),
            Data::Out(b) => Data::Out(&**b),
        }
    }
    fn len(&self) -> usize {
        match self {
            Data::None => 0,
            Data::In(b) => b.len(),
            Data::Out(b) => b.len(),
        }
    }
}

/// Result of a command whose CSW was valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    /// bCSWStatus: `CSW_OK` or `CSW_FAILED` (phase error is `BotErr::Phase`).
    pub status: u8,
    /// dCSWDataResidue as reported by the device.
    pub residue: u32,
    /// Bytes actually moved during the data stage.
    pub transferred: usize,
}

pub struct Bot<T: BulkTransport> {
    pub transport: T,
    tag: u32,
    pub timeout_ms: u32,
}

impl<T: BulkTransport> Bot<T> {
    pub fn new(transport: T) -> Self {
        Bot { transport, tag: 0, timeout_ms: 5000 }
    }

    /// Get Max LUN; a stall (or any failure) means a single LUN.
    pub fn max_lun(&mut self) -> u8 {
        let mut b = [0u8; 1];
        let idx = self.transport.interface_number() as u16;
        match self.transport.class_control_in(REQ_GET_MAX_LUN, 0, idx, &mut b) {
            Ok(1..) => b[0] & 0x0F,
            _ => 0,
        }
    }

    /// Bulk-Only Mass Storage Reset followed by clear-halt on both pipes.
    pub fn reset_recovery(&mut self) -> Result<(), UsbErr> {
        let idx = self.transport.interface_number() as u16;
        self.transport.class_control_out(REQ_RESET, 0, idx, &[])?;
        self.transport.clear_halt_in()?;
        self.transport.clear_halt_out()?;
        Ok(())
    }

    fn fail_with_reset(&mut self, e: UsbErr) -> BotErr {
        if e != UsbErr::Disconnected {
            let _ = self.reset_recovery();
        }
        BotErr::Transport(e)
    }

    fn read_csw(&mut self) -> Result<[u8; CSW_LEN], UsbErr> {
        let mut raw = [0u8; CSW_LEN];
        let n = self.transport.bulk_in(&mut raw, self.timeout_ms)?;
        if n != CSW_LEN {
            return Err(UsbErr::Other(0xFE));
        }
        Ok(raw)
    }

    pub fn execute(&mut self, lun: u8, cdb: &[u8], mut data: Data<'_>) -> Result<Completion, BotErr> {
        if cdb.is_empty() || cdb.len() > 16 {
            return Err(BotErr::Transport(UsbErr::Other(0xFD)));
        }
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;
        let dlen = data.len() as u32;

        let mut cbw = [0u8; CBW_LEN];
        cbw[0..4].copy_from_slice(&CBW_SIG.to_le_bytes());
        cbw[4..8].copy_from_slice(&tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&dlen.to_le_bytes());
        cbw[12] = if matches!(data, Data::In(_)) { 0x80 } else { 0 };
        cbw[13] = lun & 0x0F;
        cbw[14] = cdb.len() as u8;
        cbw[15..15 + cdb.len()].copy_from_slice(cdb);

        match self.transport.bulk_out(&cbw, self.timeout_ms) {
            Ok(CBW_LEN) => {}
            Ok(_) => return Err(self.fail_with_reset(UsbErr::Other(0xFE))),
            Err(e) => return Err(self.fail_with_reset(e)),
        }

        let mut transferred = 0usize;
        match data.reborrow() {
            Data::None => {}
            Data::In(buf) => match self.transport.bulk_in(buf, self.timeout_ms) {
                Ok(n) => transferred = n,
                Err(UsbErr::Stall) => {
                    self.transport.clear_halt_in().map_err(BotErr::Transport)?;
                }
                Err(e) => return Err(self.fail_with_reset(e)),
            },
            Data::Out(buf) => match self.transport.bulk_out(buf, self.timeout_ms) {
                Ok(n) => transferred = n,
                Err(UsbErr::Stall) => {
                    self.transport.clear_halt_out().map_err(BotErr::Transport)?;
                }
                Err(e) => return Err(self.fail_with_reset(e)),
            },
        }

        let raw = match self.read_csw() {
            Ok(r) => r,
            Err(UsbErr::Stall) => {
                self.transport.clear_halt_in().map_err(BotErr::Transport)?;
                match self.read_csw() {
                    Ok(r) => r,
                    Err(e) => return Err(self.fail_with_reset(e)),
                }
            }
            Err(e) => return Err(self.fail_with_reset(e)),
        };

        let sig = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let rtag = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let residue = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
        let status = raw[12];
        if sig != CSW_SIG || rtag != tag || status > CSW_PHASE {
            let _ = self.reset_recovery();
            return Err(BotErr::BadCsw);
        }
        if status == CSW_PHASE {
            let _ = self.reset_recovery();
            return Err(BotErr::Phase);
        }
        Ok(Completion { status, residue, transferred })
    }
}
