//! Bluetooth: H4 UART transport and Broadcom controller bring-up (CYW43455 on
//! the Raspberry Pi 5). `h4`, `hcd` and `bringup` are pure and host-tested
//! (drivers/bt-tests); `boot` is the rpi5 hardware glue.
pub mod h4;
pub mod hcd;
pub mod bringup;
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod boot;
