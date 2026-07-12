//! Driver framework — drivers are user-space servers communicating via IPC.
//!
//! This crate provides the skeleton each driver server implements.
//! Mirrors Linux's driver model (bus/device/driver) but enforced by the
//! microkernel: a crashing driver doesn't take down the kernel.

#![no_std]

extern crate alloc;

pub mod serial;
pub mod pci;
pub mod snd;
pub mod framebuffer;
pub mod kms;
pub mod vector_font;
pub mod drm;
pub mod drm_driver;
pub mod drm_console;
pub mod console_commands;
pub mod console_properties;
pub mod drm_device_interface;
pub mod virtio;
pub mod virtio_gpu;
pub mod virtio_blk;
pub mod virtio_keyboard;
pub mod virtio_net;
#[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
pub mod sdhci;

/// The active block-storage backend, selected at compile time. `virtio_blk`
/// on QEMU `virt`/x86_64 (PCI transport); `sdhci` on rpi5/raspi4b builds
/// (no PCI bus exists on either target — see drivers/src/sdhci.rs). Callers
/// (kernel/src/init.rs, kernel/src/syscall.rs, servers/f2fs) use this alias
/// exclusively so the backend swap is a one-line change here, not scattered
/// per-call-site cfg.
#[cfg(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b")))]
pub use sdhci as blkdev;
#[cfg(not(all(target_arch = "aarch64", any(feature = "rpi5", feature = "raspi4b"))))]
pub use virtio_blk as blkdev;

/// Trait every driver server must implement.
pub trait Driver {
    /// One-time hardware initialisation.
    fn probe(&mut self) -> Result<(), DriverError>;
    /// Called when the driver's IPC port receives a message.
    fn handle(&mut self, msg: ipc::Message) -> ipc::Message;
}

#[derive(Debug, Clone, Copy)]
pub enum DriverError {
    NotFound,
    Io,
    Unsupported,
    InvalidParameter,
}
