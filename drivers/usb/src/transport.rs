//! The bulk-only-transport seam between a host controller driver and the
//! mass-storage class driver (`msc`).
//!
//! This is the only definition: `msc::transport` re-exports it, and the
//! host-side msc test crate includes this file by `#[path]`, so keep it free
//! of dependencies beyond `core`. The xHCI implementation is
//! `crate::xhci::XhciBulk`; obtain instances with
//! `crate::mass_storage_devices`.

/// Why a USB transfer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbErr {
    /// The endpoint returned STALL (the endpoint is now halted; recover with
    /// `clear_halt_in`/`clear_halt_out`). A STALL on the control endpoint is
    /// recovered internally before this is returned.
    Stall,
    /// No completion within the timeout. The transfer has been cancelled
    /// and the endpoint is ready for the next one.
    Timeout,
    /// The device sent more data than requested.
    Babble,
    /// The device is gone (port no longer connected or slot disabled).
    Disconnected,
    /// Any other failure; the payload is the xHCI completion code (or 0xFF
    /// for a driver-side failure such as an out-of-memory bounce buffer or a
    /// dead controller).
    Other(u8),
}

pub trait BulkTransport {
    /// Send `data` on the bulk OUT endpoint. Returns bytes sent.
    fn bulk_out(&mut self, data: &[u8], timeout_ms: u32) -> Result<usize, UsbErr>;
    /// Receive up to `buf.len()` bytes on the bulk IN endpoint. Returns the
    /// byte count actually received (a short packet ends the transfer).
    fn bulk_in(&mut self, buf: &mut [u8], timeout_ms: u32) -> Result<usize, UsbErr>;
    /// Class request to the interface, host-to-device (bmRequestType 0x21).
    fn class_control_out(&mut self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), UsbErr>;
    /// Class request to the interface, device-to-host (bmRequestType 0xA1).
    fn class_control_in(&mut self, request: u8, value: u16, index: u16, buf: &mut [u8]) -> Result<usize, UsbErr>;
    /// Reset Endpoint + Set TR Dequeue Pointer on the host side, then
    /// CLEAR_FEATURE(ENDPOINT_HALT) to the device, for the bulk IN endpoint.
    fn clear_halt_in(&mut self) -> Result<(), UsbErr>;
    /// Same for the bulk OUT endpoint.
    fn clear_halt_out(&mut self) -> Result<(), UsbErr>;
    /// bInterfaceNumber of the bulk-only interface.
    fn interface_number(&self) -> u8;
}
