//! Minimal pipe abstraction the MSC layer needs from a host controller.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbErr {
    Stall,
    Timeout,
    Babble,
    Disconnected,
    Other(u8),
}

pub trait BulkTransport {
    fn bulk_out(&mut self, data: &[u8], timeout_ms: u32) -> Result<usize, UsbErr>;
    fn bulk_in(&mut self, buf: &mut [u8], timeout_ms: u32) -> Result<usize, UsbErr>;
    /// bmRequestType 0x21 (class, interface, host to device).
    fn class_control_out(&mut self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), UsbErr>;
    /// bmRequestType 0xA1 (class, interface, device to host).
    fn class_control_in(&mut self, request: u8, value: u16, index: u16, buf: &mut [u8]) -> Result<usize, UsbErr>;
    fn clear_halt_in(&mut self) -> Result<(), UsbErr>;
    fn clear_halt_out(&mut self) -> Result<(), UsbErr>;
    fn interface_number(&self) -> u8;
}
