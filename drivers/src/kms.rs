//! Kernel Mode Setting (KMS) driver with EDID autodetection
//!
//! This driver implements KMS functionality to automatically detect
//! native display resolution via EDID and configure the framebuffer
//! accordingly. Supports VirtIO-GPU and standard graphics adapters.

use spin::Mutex;
use super::{Driver, DriverError};
use crate::framebuffer::{Framebuffer, get_hardware_fb_info};

// ── EDID Data Structures ─────────────────────────────────────────────────────

/// EDID (Extended Display Identification Data) structure
/// Standard 128-byte EDID 1.3/1.4 block
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct EdidBlock {
    header: [u8; 8],
    manufacturer_id: [u8; 2],
    product_code: [u8; 2],
    serial_number: [u8; 4],
    week_of_manufacture: u8,
    year_of_manufacture: u8,
    edid_version: u8,
    edid_revision: u8,
    video_input: u8,
    horizontal_screen_size: u8,
    vertical_screen_size: u8,
    gamma: u8,
    features: u8,
    color_characteristics: [u8; 10],
    established_timings: [u8; 3],
    standard_timings: [u8; 16],
    detailed_timings: [u8; 72],
    extension_blocks: u8,
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

impl EdidBlock {
    pub fn is_valid(&self) -> bool {
        const EXPECTED_HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
        if self.header != EXPECTED_HEADER { return false; }
        let bytes = unsafe { core::slice::from_raw_parts(self as *const _ as *const u8, 128) };
        let sum: u32 = bytes.iter().map(|&b| b as u32).sum();
        (sum & 0xFF) == 0
    }

    pub fn get_preferred_mode(&self) -> Option<DisplayMode> {
        let detailed = &self.detailed_timings[0..18];
        if detailed[0] == 0 && detailed[1] == 0 { return None; }
        let pixel_clock = u16::from_le_bytes([detailed[0], detailed[1]]) as u32 * 10;
        let h_active = (detailed[2] as u32) | (((detailed[4] & 0xF0) as u32) << 4);
        let v_active = (detailed[5] as u32) | (((detailed[7] & 0xF0) as u32) << 4);
        if h_active == 0 || v_active == 0 { return None; }
        let h_total = h_active + (detailed[3] as u32) + (((detailed[4] & 0x0F) as u32) << 8);
        let v_total = v_active + (detailed[6] as u32) + (((detailed[7] & 0x0F) as u32) << 8);
        let refresh_rate = if h_total > 0 && v_total > 0 { (pixel_clock * 1000) / (h_total * v_total) } else { 60 };
        Some(DisplayMode { width: h_active, height: v_active, refresh_rate, pixel_clock })
    }
}

// ── KMS Driver ───────────────────────────────────────────────────────────────

pub struct KmsDriver {
    current_mode: Option<DisplayMode>,
    framebuffer: Framebuffer,
}

static KMS_DRIVER: Mutex<KmsDriver> = Mutex::new(KmsDriver {
    current_mode: None,
    framebuffer: Framebuffer::new(),
});

impl KmsDriver {
    pub fn new() -> Self {
        Self {
            current_mode: None,
            framebuffer: Framebuffer::new(),
        }
    }

    pub fn detect_and_configure(&mut self) -> Result<DisplayMode, DriverError> {
        // 1. Initialize global VirtIO-GPU
        crate::virtio_gpu::init();

        let mut mode = DisplayMode {
            width: 1024,
            height: 768,
            refresh_rate: 60,
            pixel_clock: 65000,
        };

        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            crate::pci::serial_debug("[KMS] VirtIO GPU available, configuring boot resource\n");
            
            // Register bootloader FB as resource 1 (for kernel console)
            crate::pci::serial_debug("[KMS] Checking boot FB info...\n");
            if let Some((base, width, height, _pitch)) = get_hardware_fb_info() {
                crate::pci::serial_debug("[KMS] Boot FB found: ");
                crate::pci::serial_debug_hex(base as u32);
                crate::pci::serial_debug(" (");
                crate::pci::serial_debug_hex(width);
                crate::pci::serial_debug("x");
                crate::pci::serial_debug_hex(height);
                crate::pci::serial_debug(")\n");
                
                crate::pci::serial_debug("[KMS] Creating resource 1\n");
                gpu.create_resource_2d(1, width, height);
                crate::pci::serial_debug("[KMS] Attaching backing to resource 1\n");
                gpu.attach_backing(1, base, width * height * 4);
                crate::pci::serial_debug("[KMS] Setting scanout for resource 1\n");
                gpu.set_scanout(1, width, height);
                crate::pci::serial_debug("[KMS] Flushing resource 1\n");
                gpu.flush(1, 0, 0, width, height);
                crate::pci::serial_debug("[KMS] Resource 1 configured successfully\n");
                
                mode.width = width;
                mode.height = height;
            } else {
                crate::pci::serial_debug("[KMS] No boot FB info available\n");
            }
        }

        self.current_mode = Some(mode);
        crate::pci::serial_debug("[KMS] detect_and_configure returning OK\n");
        Ok(mode)
    }

    pub fn get_current_mode(&self) -> Option<DisplayMode> {
        self.current_mode
    }
}

impl Driver for KmsDriver {
    fn probe(&mut self) -> Result<(), DriverError> {
        if self.framebuffer.probe().is_ok() {
            self.detect_and_configure().map(|_| ())?;
            Ok(())
        } else {
            self.detect_and_configure().map(|_| ())
        }
    }

    fn handle(&mut self, msg: ipc::Message) -> ipc::Message {
        match msg.tag {
            1 => self.framebuffer.handle(msg),
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
            _ => self.framebuffer.handle(msg),
        }
    }
}

pub fn init_kms() -> Result<DisplayMode, DriverError> {
    crate::pci::serial_debug("[KMS] init_kms starting, locking KMS_DRIVER\n");
    let mut kms = KMS_DRIVER.lock();
    crate::pci::serial_debug("[KMS] KMS_DRIVER locked, calling detect_and_configure\n");
    kms.detect_and_configure()
}

pub fn get_kms_mode() -> Option<DisplayMode> {
    KMS_DRIVER.lock().get_current_mode()
}
