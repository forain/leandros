//! Driver framework — drivers are user-space servers communicating via IPC.
//!
//! This crate provides the skeleton each driver server implements.
//! Mirrors Linux's driver model (bus/device/driver) but enforced by the
//! microkernel: a crashing driver doesn't take down the kernel.

#![no_std]

extern crate alloc;

pub mod serial;
pub mod framebuffer;
pub mod kms;
pub mod vector_font;
pub mod drm;
pub mod drm_driver;
pub mod drm_console;
pub mod console_commands;
pub mod console_properties;
pub mod drm_device_interface;

/// Trait every driver server must implement.
pub trait Driver {
    /// One-time hardware initialisation.
    fn probe(&mut self) -> Result<(), DriverError>;
    /// Called when the driver's IPC port receives a message.
    fn handle(&mut self, msg: ipc::Message) -> ipc::Message;
}

#[derive(Debug)]
pub enum DriverError {
    NotFound,
    Io,
    Unsupported,
    InvalidParameter,
}
