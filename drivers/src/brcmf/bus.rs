//! Seam between the brcmf chip/protocol code and the SDIO host controller.

pub const FUNC_BUS: u8 = 0;
pub const FUNC_BACKPLANE: u8 = 1;
pub const FUNC_WLAN: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusError {
    Timeout,
    Crc,
    NoCard,
    Io,
}

pub trait SdioBus {
    /// CMD52 single-byte read.
    fn read8(&mut self, func: u8, addr: u32) -> Result<u8, BusError>;
    /// CMD52 single-byte write.
    fn write8(&mut self, func: u8, addr: u32, val: u8) -> Result<(), BusError>;
    /// CMD53 read of any length; the host splits it into block and byte transfers.
    /// `incr` selects incrementing (true) or fixed (false) addressing.
    fn read_bytes(&mut self, func: u8, addr: u32, incr: bool, buf: &mut [u8]) -> Result<(), BusError>;
    /// CMD53 write of any length.
    fn write_bytes(&mut self, func: u8, addr: u32, incr: bool, buf: &[u8]) -> Result<(), BusError>;
    /// Sets the function's block size: FBR write plus the host's own setting.
    fn set_block_size(&mut self, func: u8, size: u16) -> Result<(), BusError>;
    /// Waits for the SDIO card interrupt (DAT1). Returns true if it was asserted before the timeout.
    fn wait_card_irq(&mut self, timeout_us: u32) -> bool;
    fn delay_us(&mut self, us: u32);
}
