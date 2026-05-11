//! Direct Rendering Manager (DRM) subsystem for LeandrOS
//!
//! This module implements a DRM subsystem compatible with the existing
//! framebuffer and KMS drivers. It provides:
//!
//! - Device management and enumeration
//! - Mode setting and display configuration
//! - Framebuffer object management
//! - Property system for dynamic configuration
//! - Authentication and security model
//!
//! Architecture:
//! - DRM Master: Controls mode setting and configuration
//! - DRM Auth: Authentication tokens for secure access
//! - DRM Device: Hardware abstraction layer
//! - DRM Objects: CRTC, Encoder, Connector, Plane abstractions

pub mod core;
pub mod device;
pub mod framebuffer;
pub mod modes;
pub mod properties;
pub mod auth;

pub use core::*;
pub use device::*;
pub use framebuffer::*;
pub use modes::*;
pub use properties::*;
pub use auth::*;