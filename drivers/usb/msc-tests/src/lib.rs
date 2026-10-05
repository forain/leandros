//! Host-side harness for the USB mass-storage module. Run with
//! `cargo test --manifest-path drivers/usb/msc-tests/Cargo.toml`.

#[path = "../../src/msc/mod.rs"]
pub mod msc;

#[cfg(test)]
mod tests;
