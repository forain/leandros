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
            crate::pci::rdebug("[KMS] VirtIO GPU available\n");

            if gpu.scanout_configured() {
                // The early boot console (direct-boot path) already brought up
                // the VirtIO GPU and bound a RAM-backed surface to resource 1 as
                // scanout 0.  BOOT_FB and the kernel console already target that
                // surface, so re-creating resource 1 here is both unnecessary and
                // harmful: the host rejects the duplicate resource and the device
                // reset wedges the control queue.  Reuse the existing scanout and
                // only report its mode.
                if let Some((_phys, width, height, _pitch)) = get_hardware_fb_info() {
                    mode.width = width;
                    mode.height = height;
                }
                crate::pci::rdebug("[KMS] Reusing existing VirtIO GPU console scanout\n");
            } else if let Some((limine_phys, width, height, _pitch)) = get_hardware_fb_info() {
                crate::pci::rdebug("[KMS] Limine FB at phys=");
                crate::pci::rdebug_hex(limine_phys as u32);
                crate::pci::rdebug(" ");
                crate::pci::rdebug_hex(width);
                crate::pci::rdebug("x");
                crate::pci::rdebug_hex(height);
                crate::pci::rdebug("\n");

                let fb_bytes = (width as usize) * (height as usize) * 4;

                // VirtIO GPU RESOURCE_ATTACH_BACKING requires guest physical pages in
                // regular RAM — it cannot DMA from device MMIO/VRAM.  With virtio-vga,
                // OVMF's GOP framebuffer often lives in the VirtIO VGA VRAM BAR, so
                // directly backing resource 1 with that address causes TRANSFER_TO_HOST_2D
                // to read zeroes and the scanout goes black the instant SET_SCANOUT fires.
                //
                // Allocate a fresh RAM buffer instead.  Copy existing Limine FB content
                // so the boot console history is preserved across the DRM handoff.
                let pages  = (fb_bytes + 4095) >> 12;
                let order  = (usize::BITS - pages.leading_zeros()) as usize; // ceil_log2
                let order  = order.min(mm::buddy::MAX_ORDER - 1);

                if let Some(ram_phys) = mm::buddy::alloc(order) {
                    let ram_virt = mm::phys_to_virt(ram_phys) as *mut u8;

                    // Copy Limine FB → new RAM buffer (preserves pre-DRM console text)
                    unsafe {
                        let src = mm::phys_to_virt(limine_phys as usize) as *const u8;
                        core::ptr::copy_nonoverlapping(src, ram_virt, fb_bytes);
                    }

                    crate::pci::rdebug("[KMS] RAM-backed FB at phys=");
                    crate::pci::rdebug_hex(ram_phys as u32);
                    crate::pci::rdebug("\n");

                    // Back resource 1 with the new RAM buffer and set it as the scanout
                    gpu.create_resource_2d(1, width, height);
                    gpu.attach_backing(1, ram_phys as u64, fb_bytes as u32);
                    gpu.set_scanout(1, width, height);
                    gpu.flush(1, 0, 0, width, height);

                    // Redirect BOOT_FB and the kernel console to the RAM buffer so that
                    // perform_software_scaling() and fb_flush() both target the same surface
                    // that VirtIO GPU is reading.
                    crate::framebuffer::set_boot_framebuffer(
                        ram_phys as u64, width, height, width * 4);
                    unsafe {
                        crate::framebuffer::init_kernel_fb(
                            ram_virt as *mut u32,
                            width as usize, height as usize, (width * 4) as usize);
                    }

                    mode.width  = width;
                    mode.height = height;
                    crate::pci::rdebug("[KMS] Resource 1 configured with RAM backing\n");
                } else {
                    // Out of contiguous RAM — fall back to Limine FB address.
                    // May produce a black scanout on virtio-vga but won't crash.
                    crate::pci::rdebug("[KMS] RAM alloc failed, falling back to Limine FB\n");
                    gpu.create_resource_2d(1, width, height);
                    gpu.attach_backing(1, limine_phys, fb_bytes as u32);
                    gpu.set_scanout(1, width, height);
                    gpu.flush(1, 0, 0, width, height);
                    mode.width  = width;
                    mode.height = height;
                }
            } else {
                crate::pci::rdebug("[KMS] No Limine FB — running without console surface\n");
            }
        }

        self.current_mode = Some(mode);
        crate::pci::rdebug("[KMS] detect_and_configure done\n");
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
    crate::pci::rdebug("[KMS] init_kms starting, locking KMS_DRIVER\n");
    let mut kms = KMS_DRIVER.lock();
    crate::pci::rdebug("[KMS] KMS_DRIVER locked, calling detect_and_configure\n");
    kms.detect_and_configure()
}

pub fn get_kms_mode() -> Option<DisplayMode> {
    KMS_DRIVER.lock().get_current_mode()
}
