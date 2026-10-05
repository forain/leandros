//! USB subsystem — ported from Linux drivers/usb/ and include/uapi/linux/usb/
//!
//! Architecture:
//!   descriptor  — USB protocol types (ch9.h)
//!   endpoint    — endpoint model
//!   device      — USB device tree
//!   transfer    — URB-equivalent async transfer
//!   hub         — hub driver (ch11.h)
//!   hcd         — Host Controller Driver trait
//!   xhci        — xHCI (USB 3.x) host controller driver

#![no_std]

pub mod descriptor;
pub mod device;
pub mod endpoint;
pub mod hcd;
pub mod hub;
pub mod msc;
pub mod transfer;
pub mod xhci;

pub use descriptor::*;
pub use device::UsbDevice;
pub use endpoint::{Endpoint, TransferType, Direction};
pub use hcd::HostControllerDriver;
pub use transfer::{Urb, UrbStatus, TransferFlags};
