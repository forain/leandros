//! USB subsystem.
//!
//! Architecture:
//!   platform   — clock / yield / log hooks installed by the kernel
//!   dma        — `DmaBuf`, the only memory ever handed to a controller
//!   transport  — `BulkTransport` + `UsbErr`, the seam to the class driver
//!   xhci       — xHCI host controller driver (polled, root ports only)
//!   selftest   — raw BOT self-test over any `BulkTransport`
//!   descriptor, endpoint, device, transfer, hub, hcd — protocol types and
//!                the older (not yet wired) hub/HCD model
//!
//! Entry points for the kernel: [`probe_xhci`] once per controller, then
//! [`mass_storage_devices`] to take the bulk-only transports and
//! [`devices`] for `lsusb`.

#![no_std]

extern crate alloc;

pub mod platform;
pub mod dma;
pub mod transport;
pub mod selftest;
pub mod descriptor;
pub mod device;
pub mod endpoint;
pub mod hcd;
pub mod hub;
pub mod transfer;
pub mod xhci;

pub use descriptor::*;
pub use device::UsbDevice;
pub use endpoint::{Endpoint, TransferType, Direction};
pub use hcd::HostControllerDriver;
pub use transfer::{Urb, UrbStatus, TransferFlags};
pub use transport::{BulkTransport, UsbErr};
pub use xhci::XhciBulk;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

/// One enumerated device (for `lsusb` and logs).
#[derive(Clone, Debug)]
pub struct UsbDeviceInfo {
    /// Controller number, 1-based.
    pub bus: u8,
    /// Root-hub port, 1-based.
    pub port: u8,
    pub slot: u8,
    /// USB device address assigned by the controller.
    pub address: u8,
    pub vendor: u16,
    pub product: u16,
    /// Port speed ID (xhci::context::SPEED_*).
    pub speed: u8,
    /// bDeviceClass, or the first interface's class when that is 0.
    pub class: u8,
    pub manufacturer: String,
    pub product_name: String,
    /// A bulk-only mass-storage interface was configured.
    pub mass_storage: bool,
    pub interface: u8,
}

type Controller = Arc<Mutex<xhci::Xhci>>;

static CONTROLLERS: Mutex<Vec<Controller>> = Mutex::new(Vec::new());
static DEVICES: Mutex<Vec<UsbDeviceInfo>> = Mutex::new(Vec::new());
static MASS_STORAGE: Mutex<Vec<XhciBulk>> = Mutex::new(Vec::new());

/// Bring up the xHCI controller whose BAR is mapped at `base` (`len`
/// bytes), enumerate its root ports, print one lsusb-style line per device,
/// run the BOT self-test if enabled, and register the results. Returns the
/// number of devices found.
///
/// # Safety
/// `base..base+len` must be the controller's BAR mapped as device memory,
/// with PCI memory decoding and bus mastering enabled.
pub unsafe fn probe_xhci(base: usize, len: usize) -> Result<usize, &'static str> {
    let bus = (CONTROLLERS.lock().len() + 1) as u8;
    let (hc, found) = xhci::probe(base, len, bus)?;
    let n = found.len();
    for e in found {
        let i = &e.info;
        usb_log!("[USB] Bus {:03} Device {:03}: ID {:04x}:{:04x} {} {} [port {}, {}, class {:02x}{}]\n",
                 i.bus, i.address, i.vendor, i.product,
                 if i.manufacturer.is_empty() { "?" } else { &i.manufacturer },
                 if i.product_name.is_empty() { "?" } else { &i.product_name },
                 i.port, xhci::context::speed_name(i.speed), i.class,
                 if i.mass_storage { ", bulk-only mass storage" } else { "" });
        DEVICES.lock().push(e.info.clone());
        if let Some(mut b) = e.bulk {
            if platform::selftest() {
                let mut label = String::new();
                let _ = core::fmt::Write::write_fmt(&mut label,
                    format_args!("bus {} dev {} if {}", i.bus, i.address, i.interface));
                selftest::run(&mut b, &label);
            }
            MASS_STORAGE.lock().push(b);
        }
    }
    CONTROLLERS.lock().push(hc);
    Ok(n)
}

/// Every enumerated device.
pub fn devices() -> Vec<UsbDeviceInfo> {
    DEVICES.lock().clone()
}

/// Number of bulk-only mass-storage transports not yet taken.
pub fn mass_storage_count() -> usize {
    MASS_STORAGE.lock().len()
}

/// Take the transports of every enumerated bulk-only mass-storage
/// interface. Each transport owns its endpoints, so they are handed out
/// once: a second call returns only interfaces enumerated since.
pub fn mass_storage_devices() -> Vec<XhciBulk> {
    core::mem::take(&mut *MASS_STORAGE.lock())
}
