//! Bluetooth: H4 UART transport and Broadcom controller bring-up (CYW43455 on
//! the Raspberry Pi 5) and the USB transport (btusb + Intel btintel set-up,
//! the laptop's 8265). `h4`, `hcd` and `bringup` are pure and host-tested
//! (drivers/bt-tests); `boot` is the rpi5 hardware glue.
pub mod h4;
pub mod hcd;
pub mod bringup;
pub mod hcidev_core;
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod boot;
pub mod hcidev;
pub mod btintel;
pub mod usb_reasm;
pub mod usb;
