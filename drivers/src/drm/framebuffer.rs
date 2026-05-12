//! DRM framebuffer object management

use alloc::{vec::Vec, vec};
use super::core::{DrmObject, DrmObjectId, DrmObjectType};
use super::super::DriverError;

/// DRM framebuffer pixel formats
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrmFormat {
    /// 32-bit XRGB8888 format (0xRRGGBB)
    Xrgb8888 = 0x34325258,
    /// 32-bit BGRX8888 format (0xBBGGRR)
    Bgrx8888 = 0x34325242,
    /// 32-bit ARGB8888 format with alpha
    Argb8888 = 0x34324752,
    /// 24-bit RGB888 format
    Rgb888 = 0x34324247,
    /// 16-bit RGB565 format
    Rgb565 = 0x36314752,
}

impl DrmFormat {
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            DrmFormat::Xrgb8888 | DrmFormat::Bgrx8888 | DrmFormat::Argb8888 => 4,
            DrmFormat::Rgb888 => 3,
            DrmFormat::Rgb565 => 2,
        }
    }

    pub fn has_alpha(self) -> bool {
        match self {
            DrmFormat::Argb8888 => true,
            _ => false,
        }
    }
}

/// DRM framebuffer object
pub struct DrmFramebuffer {
    id: DrmObjectId,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,        // Bytes per row
    pub format: DrmFormat,
    pub modifier: u64,     // Format modifier (for tiling, compression etc)
    pub handles: [u32; 4], // Buffer object handles for each plane
    pub offsets: [u32; 4], // Byte offset of each plane
    pub pitches: [u32; 4], // Pitch of each plane
    pub physical_addresses: [u64; 4], // Physical address of each plane
}

impl DrmFramebuffer {
    /// Create a new framebuffer object
    pub fn new(width: u32, height: u32, format: DrmFormat, handle: u32, pitch: u32) -> Self {
        let mut handles = [0u32; 4];
        let mut pitches = [0u32; 4];
        let mut offsets = [0u32; 4];
        let physical_addresses = [0u64; 4];

        handles[0] = handle;
        pitches[0] = pitch;
        offsets[0] = 0;
        
        // In our simplified DRM, we store the physical address in the handle
        // or look it up. For now, we'll initialize it to 0 and set it later
        // or pass it in.

        Self {
            id: DrmObjectId::new(),
            width,
            height,
            pitch,
            format,
            modifier: 0, // DRM_FORMAT_MOD_LINEAR
            handles,
            offsets,
            pitches,
            physical_addresses,
        }
    }

    /// Create framebuffer with multiple planes
    pub fn new_planar(width: u32, height: u32, format: DrmFormat,
                      handles: [u32; 4], pitches: [u32; 4], offsets: [u32; 4]) -> Self {
        Self {
            id: DrmObjectId::new(),
            width,
            height,
            pitch: pitches[0],
            format,
            modifier: 0,
            handles,
            offsets,
            pitches,
            physical_addresses: [0u64; 4],
        }
    }

    /// Get buffer size in bytes
    pub fn size(&self) -> u32 {
        self.height * self.pitch
    }

    /// Check if framebuffer is valid
    pub fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0 && self.pitch > 0 && self.handles[0] != 0
    }

    /// Get number of planes for this format
    pub fn num_planes(&self) -> u32 {
        // Most formats use single plane
        match self.format {
            DrmFormat::Xrgb8888 | DrmFormat::Bgrx8888 | DrmFormat::Argb8888 |
            DrmFormat::Rgb888 | DrmFormat::Rgb565 => 1,
        }
    }
}

impl DrmObject for DrmFramebuffer {
    fn id(&self) -> DrmObjectId { self.id }
    fn object_type(&self) -> DrmObjectType { DrmObjectType::Mode }
}

/// Dumb buffer for software rendering
pub struct DrmDumbBuffer {
    pub handle: u32,
    pub size: u32,
    pub pitch: u32,
    pub address: u64,     // Virtual address when mapped
    pub width: u32,
    pub height: u32,
    pub bpp: u32,         // Bits per pixel
}

impl DrmDumbBuffer {
    /// Create a new dumb buffer
    pub fn new(width: u32, height: u32, bpp: u32) -> Result<Self, DriverError> {
        static NEXT_HANDLE: core::sync::atomic::AtomicU32 =
            core::sync::atomic::AtomicU32::new(1);

        let pitch = (width * bpp + 7) / 8; // Round up to byte boundary
        let size = height * pitch;
        let handle = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::SeqCst);

        Ok(Self {
            handle,
            size,
            pitch,
            address: 0, // Will be set when mapped
            width,
            height,
            bpp,
        })
    }

    /// Map buffer into virtual memory
    pub fn map(&mut self) -> Result<*mut u8, DriverError> {
        // In real implementation, this would:
        // 1. Allocate physical pages
        // 2. Map into virtual address space
        // 3. Return virtual address

        // For now, return a placeholder
        self.address = 0xDEADBEEF;
        Ok(self.address as *mut u8)
    }

    /// Unmap buffer from virtual memory
    pub fn unmap(&mut self) -> Result<(), DriverError> {
        self.address = 0;
        Ok(())
    }
}

/// Framebuffer creation request
pub struct DrmFramebufferRequest {
    pub width: u32,
    pub height: u32,
    pub format: DrmFormat,
    pub flags: u32,
}

impl DrmFramebufferRequest {
    pub fn new(width: u32, height: u32, format: DrmFormat) -> Self {
        Self {
            width,
            height,
            format,
            flags: 0,
        }
    }

    /// Validate the framebuffer request
    pub fn validate(&self) -> Result<(), DriverError> {
        if self.width == 0 || self.height == 0 {
            return Err(DriverError::Unsupported);
        }

        if self.width > 4096 || self.height > 4096 {
            return Err(DriverError::Unsupported);
        }

        Ok(())
    }
}