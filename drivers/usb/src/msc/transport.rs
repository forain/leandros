//! The pipe abstraction the MSC layer needs from a host controller.
//!
//! The single definition lives in `crate::transport` (shared with the xHCI
//! driver); this module only re-exports it so `msc` code can keep using
//! `super::transport::...`. The host test crate provides `crate::transport`
//! the same way, by including `src/transport.rs` with `#[path]`.

pub use crate::transport::{BulkTransport, UsbErr};
