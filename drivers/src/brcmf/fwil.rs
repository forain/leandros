//! Firmware interface layer: what the cfg/netdev code needs from the BCDC/SDPCM protocol code.
use super::bus::BusError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwError {
    Bus(BusError),
    Timeout,
    /// Firmware returned a negative BCME status.
    Status(i32),
    TooLong,
}

/// Receives what the firmware sends up.
pub trait RxSink {
    /// A whole Ethernet frame with ethertype 0x886c (bcmeth + brcmf_event_msg), BCDC header stripped.
    fn on_event(&mut self, frame: &[u8]);
    /// A data Ethernet frame (802.3), BCDC header stripped.
    fn on_data(&mut self, frame: &[u8]);
}

pub trait Fwil {
    /// WLC_* ioctl get; returns the number of bytes the firmware wrote back into `buf`.
    fn cmd_get(&mut self, cmd: u32, buf: &mut [u8]) -> Result<usize, FwError>;
    fn cmd_set(&mut self, cmd: u32, buf: &[u8]) -> Result<(), FwError>;
    /// iovar get. `buf` holds parameters on input and the result on output.
    fn iovar_get(&mut self, name: &str, buf: &mut [u8]) -> Result<usize, FwError>;
    fn iovar_set(&mut self, name: &str, buf: &[u8]) -> Result<(), FwError>;
    /// Sends one Ethernet frame.
    fn tx_data(&mut self, frame: &[u8]) -> Result<(), FwError>;
    /// Services the bus once: drains received frames into `sink`. Never blocks longer than `budget_us`.
    fn poll(&mut self, sink: &mut dyn RxSink, budget_us: u32) -> Result<(), FwError>;
}
