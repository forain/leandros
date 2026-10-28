//! Host tests for the Bluetooth H4 transport, `.hcd` parser and bring-up (lane pibt-uart).
//! The kernel modules use `super::h4` etc., which resolves to these crate-root modules.
extern crate alloc;

#[allow(dead_code)]
#[path = "../../src/bt/h4.rs"]
pub mod h4;
#[allow(dead_code)]
#[path = "../../src/bt/hcd.rs"]
pub mod hcd;
#[allow(dead_code)]
#[path = "../../src/bt/bringup.rs"]
pub mod bringup;
#[allow(dead_code)]
#[path = "../../src/bt/hcidev_core.rs"]
pub mod hcidev_core;
#[allow(dead_code)]
#[path = "../../src/bt/btintel.rs"]
pub mod btintel;
#[allow(dead_code)]
#[path = "../../src/bt/usb_reasm.rs"]
pub mod usb_reasm;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod usb_tests;
