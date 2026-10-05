//! USB Mass Storage Class: Bulk-Only Transport (BOT) plus a SCSI
//! transparent-command-set block device layer.
//!
//! This module is hardware independent. It talks to the device only through
//! the [`transport::BulkTransport`] trait, never allocates, and uses nothing
//! outside `core` and its own submodules so that it can also be compiled by a
//! host-side test crate (`drivers/usb/msc-tests`) via `#[path]`.
//!
//!   transport — `BulkTransport` trait and `UsbErr`
//!   bot       — CBW/CSW framing, error recovery (USB MSC BOT 1.0)
//!   scsi      — `ScsiDisk`: INQUIRY, TUR, READ CAPACITY, READ/WRITE, flush

pub mod bot;
pub mod scsi;
pub mod transport;

pub use scsi::{MscErr, ScsiDisk};
pub use transport::{BulkTransport, UsbErr};
