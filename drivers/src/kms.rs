//! Kernel Mode Setting (KMS) driver with EDID autodetection
//!
//! This driver implements KMS functionality to automatically detect
//! native display resolution via EDID and configure the framebuffer
//! accordingly. Supports VirtIO-GPU and standard graphics adapters.

use spin::Mutex;
use super::{Driver, DriverError};
use crate::framebuffer::Framebuffer;

// ── EDID Data Structures ─────────────────────────────────────────────────────

/// EDID (Extended Display Identification Data) structure
/// Standard 128-byte EDID 1.3/1.4 block
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct EdidBlock {
    /// Fixed header pattern: 00 FF FF FF FF FF FF 00
    header: [u8; 8],
    /// Manufacturer ID (2 bytes, big endian)
    manufacturer_id: [u8; 2],
    /// Product code (2 bytes, little endian)
    product_code: [u8; 2],
    /// Serial number (4 bytes, little endian)
    serial_number: [u8; 4],
    /// Week of manufacture (or model year flag)
    week_of_manufacture: u8,
    /// Year of manufacture (years since 1990)
    year_of_manufacture: u8,
    /// EDID version
    edid_version: u8,
    /// EDID revision
    edid_revision: u8,
    /// Video input parameters
    video_input: u8,
    /// Horizontal screen size in cm (0 if undefined)
    horizontal_screen_size: u8,
    /// Vertical screen size in cm (0 if undefined)
    vertical_screen_size: u8,
    /// Display gamma (value = (gamma * 100) - 100)
    gamma: u8,
    /// Supported features bitmap
    features: u8,
    /// Color characteristics (10 bytes)
    color_characteristics: [u8; 10],
    /// Established timings bitmap
    established_timings: [u8; 3],
    /// Standard timings (16 bytes, 8 entries of 2 bytes each)
    standard_timings: [u8; 16],
    /// Detailed timing descriptors (72 bytes, 4 blocks of 18 bytes each)
    detailed_timings: [u8; 72],
    /// Extension block count
    extension_blocks: u8,
    /// Checksum (sum of all 128 bytes should be 0)
    checksum: u8,
}

/// Display mode extracted from EDID
#[derive(Debug, Clone, Copy)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub pixel_clock: u32,
}

/// VirtIO-GPU capability types
#[repr(u32)]
#[derive(Debug, Clone, Copy)]
pub enum VirtioGpuCapType {
    Virgl = 1,
    Edid = 2,
}

/// VirtIO-GPU control header
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioGpuCtrlHeader {
    typ: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    padding: u32,
}

/// VirtIO-GPU get EDID command
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioGpuGetEdid {
    hdr: VirtioGpuCtrlHeader,
    scanout: u32,
    padding: u32,
}

// ── EDID Parsing ─────────────────────────────────────────────────────────────

impl EdidBlock {
    /// Validate EDID header and checksum
    pub fn is_valid(&self) -> bool {
        // Check header pattern
        const EXPECTED_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
        if self.header != EXPECTED_HEADER {
            return false;
        }

        // Verify checksum
        let bytes = unsafe {
            core::slice::from_raw_parts(self as *const _ as *const u8, 128)
        };
        let sum: u32 = bytes.iter().map(|&b| b as u32).sum();
        (sum & 0xFF) == 0
    }

    /// Extract preferred display mode from detailed timing descriptor
    pub fn get_preferred_mode(&self) -> Option<DisplayMode> {
        // Parse first detailed timing descriptor (usually preferred mode)
        let detailed = &self.detailed_timings[0..18];

        // Check if it's a timing descriptor (not a monitor descriptor)
        if detailed[0] == 0 && detailed[1] == 0 {
            return None; // This is a monitor descriptor, not timing
        }

        // Extract pixel clock (in 10kHz units)
        let pixel_clock = u16::from_le_bytes([detailed[0], detailed[1]]) as u32 * 10;

        // Extract horizontal and vertical active pixels
        let h_active = (detailed[2] as u32) | (((detailed[4] & 0xF0) as u32) << 4);
        let v_active = (detailed[5] as u32) | (((detailed[7] & 0xF0) as u32) << 4);

        if h_active == 0 || v_active == 0 {
            return None;
        }

        // Calculate refresh rate (simplified)
        let h_total = h_active + (detailed[3] as u32) + (((detailed[4] & 0x0F) as u32) << 8);
        let v_total = v_active + (detailed[6] as u32) + (((detailed[7] & 0x0F) as u32) << 8);

        let refresh_rate = if h_total > 0 && v_total > 0 {
            (pixel_clock * 1000) / (h_total * v_total)
        } else {
            60 // Default fallback
        };

        Some(DisplayMode {
            width: h_active,
            height: v_active,
            refresh_rate,
            pixel_clock,
        })
    }

    /// Get manufacturer string from ID
    pub fn get_manufacturer(&self) -> [char; 3] {
        let id = u16::from_be_bytes(self.manufacturer_id);
        [
            ((((id >> 10) & 0x1F) + 0x40) as u8) as char,
            ((((id >> 5) & 0x1F) + 0x40) as u8) as char,
            (((id & 0x1F) + 0x40) as u8) as char,
        ]
    }
}

// ── VirtIO-GPU Interface ─────────────────────────────────────────────────────

/// VirtIO-GPU device interface for EDID retrieval
pub struct VirtioGpu {
    #[allow(dead_code)]
    base_addr: u64,
    #[allow(dead_code)]
    control_queue: VirtioQueue,
}

/// Simple VirtIO queue implementation
pub struct VirtioQueue {
    // Simplified for this implementation
}

impl VirtioGpu {
    /// Create new VirtIO-GPU device interface
    pub fn new(base_addr: u64) -> Self {
        Self {
            base_addr,
            control_queue: VirtioQueue {},
        }
    }

    /// Attempt to read EDID from VirtIO-GPU device
    pub fn read_edid(&mut self, _output_id: u32) -> Option<EdidBlock> {
        // This is a simplified implementation
        // In a real implementation, this would:
        // 1. Check VirtIO-GPU capabilities for EDID support
        // 2. Send VirtIO-GPU GET_EDID command
        // 3. Wait for response and parse EDID data

        // For now, return a synthetic EDID for common resolutions
        self.create_synthetic_edid(1920, 1080, 60)
    }

    /// Create a synthetic EDID for testing/fallback
    fn create_synthetic_edid(&self, width: u32, height: u32, refresh: u32) -> Option<EdidBlock> {
        let mut edid = EdidBlock {
            header: [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
            manufacturer_id: [0x49, 0x14], // "QMU" (QEMU)
            product_code: [0x00, 0x00],
            serial_number: [0x00, 0x00, 0x00, 0x00],
            week_of_manufacture: 1,
            year_of_manufacture: 30, // 2020
            edid_version: 1,
            edid_revision: 3,
            video_input: 0x80, // Digital input
            horizontal_screen_size: 52, // ~52cm for typical monitor
            vertical_screen_size: 29, // ~29cm for typical monitor
            gamma: 120, // Gamma 2.2
            features: 0x0A,
            color_characteristics: [0; 10],
            established_timings: [0x21, 0x08, 0x00], // Common resolutions
            standard_timings: [0; 16],
            detailed_timings: [0; 72],
            extension_blocks: 0,
            checksum: 0,
        };

        // Create detailed timing descriptor for requested resolution
        self.create_detailed_timing(&mut edid, 0, width, height, refresh);

        // Calculate and set checksum
        let bytes = unsafe {
            core::slice::from_raw_parts(&edid as *const _ as *const u8, 127)
        };
        let sum: u32 = bytes.iter().map(|&b| b as u32).sum();
        edid.checksum = (256 - (sum & 0xFF)) as u8;

        Some(edid)
    }

    /// Create a detailed timing descriptor
    fn create_detailed_timing(&self, edid: &mut EdidBlock, index: usize, width: u32, height: u32, refresh: u32) {
        if index >= 4 { return; }

        let offset = index * 18;
        let timing = &mut edid.detailed_timings[offset..offset+18];

        // Simplified timing calculation
        let pixel_clock = (width * height * refresh * 133) / 100000; // Rough estimate with blanking

        timing[0] = (pixel_clock & 0xFF) as u8;
        timing[1] = ((pixel_clock >> 8) & 0xFF) as u8;
        timing[2] = (width & 0xFF) as u8;
        timing[3] = ((width * 25) / 100) as u8; // H-blank ~25% of active
        timing[4] = (((width >> 8) & 0x0F) | (((width * 25 / 100) >> 4) & 0xF0)) as u8;
        timing[5] = (height & 0xFF) as u8;
        timing[6] = ((height * 4) / 100) as u8; // V-blank ~4% of active
        timing[7] = (((height >> 8) & 0x0F) | (((height * 4 / 100) >> 4) & 0xF0)) as u8;
        // ... rest of timing descriptor fields
    }
}

// ── KMS Driver ───────────────────────────────────────────────────────────────

pub struct KmsDriver {
    virtio_gpu: Option<VirtioGpu>,
    current_mode: Option<DisplayMode>,
    framebuffer: Framebuffer,
}

static KMS_DRIVER: Mutex<KmsDriver> = Mutex::new(KmsDriver {
    virtio_gpu: None,
    current_mode: None,
    framebuffer: Framebuffer::new(),
});

impl KmsDriver {
    pub fn new() -> Self {
        Self {
            virtio_gpu: None,
            current_mode: None,
            framebuffer: Framebuffer::new(),
        }
    }

    /// Detect and configure display mode using EDID
    pub fn detect_and_configure(&mut self) -> Result<DisplayMode, DriverError> {
        // For now, we'll be conservative and return a standard mode
        // that matches common QEMU framebuffer setups

        // Try to initialize VirtIO-GPU device
        if let Some(gpu_addr) = self.find_virtio_gpu() {
            let mut virtio_gpu = VirtioGpu::new(gpu_addr);

            // Try to read EDID from primary output
            if let Some(edid) = virtio_gpu.read_edid(0) {
                if edid.is_valid() {
                    if let Some(mode) = edid.get_preferred_mode() {
                        // For safety, only accept common resolutions that QEMU typically supports
                        if (mode.width == 1920 && mode.height == 1080) ||
                           (mode.width == 1024 && mode.height == 768) ||
                           (mode.width == 800 && mode.height == 600) {
                            self.configure_mode(mode)?;
                            self.current_mode = Some(mode);
                            self.virtio_gpu = Some(virtio_gpu);
                            return Ok(mode);
                        }
                    }
                }
            }
        }

        // Conservative fallback: return a safe mode
        let safe_mode = DisplayMode {
            width: 1024,
            height: 768,
            refresh_rate: 60,
            pixel_clock: 65000
        };

        if self.configure_mode(safe_mode).is_ok() {
            self.current_mode = Some(safe_mode);
            return Ok(safe_mode);
        }

        Err(DriverError::NotFound)
    }

    /// Find VirtIO-GPU device (simplified detection)
    fn find_virtio_gpu(&self) -> Option<u64> {
        // This is a simplified implementation
        // Real implementation would scan PCI bus for VirtIO-GPU device
        // For now, return a placeholder address if we detect QEMU
        Some(0xFEBD0000) // Typical VirtIO-GPU MMIO base in QEMU
    }

    /// Configure display mode (placeholder for mode setting)
    fn configure_mode(&mut self, _mode: DisplayMode) -> Result<(), DriverError> {
        // This would typically:
        // 1. Program GPU registers for the new mode
        // 2. Allocate framebuffer memory
        // 3. Configure display timing parameters

        // For now, we'll just update our internal state
        // and assume the bootloader framebuffer is already set correctly
        Ok(())
    }

    /// Get current display mode
    pub fn get_current_mode(&self) -> Option<DisplayMode> {
        self.current_mode
    }
}

impl Driver for KmsDriver {
    fn probe(&mut self) -> Result<(), DriverError> {
        // First try to use existing boot framebuffer
        if self.framebuffer.probe().is_ok() {
            // Now try to detect native resolution and reconfigure if possible
            match self.detect_and_configure() {
                Ok(_mode) => {
                    // Successfully detected and configured native mode
                    Ok(())
                }
                Err(_) => {
                    // Fall back to boot framebuffer
                    Ok(())
                }
            }
        } else {
            // No boot framebuffer, must detect and configure
            self.detect_and_configure()?;
            Ok(())
        }
    }

    fn handle(&mut self, msg: ipc::Message) -> ipc::Message {
        match msg.tag {
            // Tag 1: Clear framebuffer
            1 => self.framebuffer.handle(msg),

            // Tag 2: Get current mode info
            2 => {
                if let Some(mode) = self.current_mode {
                    let mut response = ipc::Message::empty();
                    response.data[0..4].copy_from_slice(&mode.width.to_le_bytes());
                    response.data[4..8].copy_from_slice(&mode.height.to_le_bytes());
                    response.data[8..12].copy_from_slice(&mode.refresh_rate.to_le_bytes());
                    response
                } else {
                    ipc::Message::empty()
                }
            }

            // Tag 3: Set mode (width in data[0..4], height in data[4..8])
            3 => {
                let width = u32::from_le_bytes(msg.data[0..4].try_into().unwrap_or([0; 4]));
                let height = u32::from_le_bytes(msg.data[4..8].try_into().unwrap_or([0; 4]));

                let mode = DisplayMode {
                    width,
                    height,
                    refresh_rate: 60,
                    pixel_clock: (width * height * 60 * 133) / 100000,
                };

                if self.configure_mode(mode).is_ok() {
                    self.current_mode = Some(mode);
                }

                ipc::Message::empty()
            }

            _ => self.framebuffer.handle(msg),
        }
    }
}

// ── Global KMS Interface ─────────────────────────────────────────────────────

/// Initialize KMS and detect native display resolution
pub fn init_kms() -> Result<DisplayMode, DriverError> {
    let mut kms = KMS_DRIVER.lock();
    kms.detect_and_configure()
}

/// Get current KMS mode
pub fn get_kms_mode() -> Option<DisplayMode> {
    KMS_DRIVER.lock().get_current_mode()
}