//! Host-side harness for the USB mass-storage module. Run with
//! `cargo test --manifest-path drivers/usb/msc-tests/Cargo.toml`.

// The one `UsbErr`/`BulkTransport` definition, shared with the xHCI driver.
#[path = "../../src/transport.rs"]
pub mod transport;

// Clock and log hooks (`usb_log!`, `now_ns`), used by the SCSI error ladder.
#[path = "../../src/platform.rs"]
#[macro_use]
pub mod platform;

#[path = "../../src/msc/mod.rs"]
pub mod msc;

#[cfg(test)]
mod tests;
