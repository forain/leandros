//! DRM device interface for userspace applications
//!
//! This module provides the kernel-side interface that userspace applications
//! like DOOM can use to communicate with the DRM subsystem.

use alloc::vec::Vec;
use ::core::slice;
use ::core::ptr;
use super::drm::*;
use super::drm_driver::*;
use super::{Driver, DriverError};

/// DRM device interface for userspace communication
pub struct DrmDeviceInterface {
    driver: DrmDriver,
    device_path: &'static str,
}

impl DrmDeviceInterface {
    /// Create new DRM device interface
    pub fn new() -> Self {
        Self {
            driver: DrmDriver::new(),
            device_path: "/dev/drm0",
        }
    }

    /// Handle incoming IPC messages
    pub fn handle_ioctl(&mut self, cmd: u32, arg: usize) -> Result<usize, DriverError> {
        let device = get_drm_device();
        let mut device_lock = device.lock();

        match cmd {
            // Mode setting ioctls
            0x1001 => self.handle_set_mode(&mut device_lock, arg),
            0x1003 => self.handle_get_mode(&mut device_lock, arg),

            // Framebuffer ioctls
            0x1002 => self.handle_create_framebuffer(&mut device_lock, arg),
            0x1004 => self.handle_flip_page(&mut device_lock, arg),
            0x1005 => self.handle_set_plane(&mut device_lock, arg),

            // Capability ioctls
            0x1006 => self.handle_get_capabilities(arg),

            // Mmap ioctl - returns physical address for device mapping
            0x1007 => self.handle_ioctl_mmap(arg),

            _ => Err(DriverError::Unsupported),
        }
    }

    /// Handle DRM_IOCTL_SET_MODE
    fn handle_set_mode(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg points to [width, height, refresh] array
        let mode_data = unsafe {
            slice::from_raw_parts(arg as *const u32, 3)
        };

        let width = mode_data[0];
        let height = mode_data[1];
        let refresh = mode_data[2];

        // Drop the lock here because ModeSet::set_display_mode will acquire it
        // This is safe because we are at the top level handler
        drop(_device);

        // Set display mode using our DRM subsystem
        match ModeSet::set_display_mode(width, height, refresh) {
            Ok(()) => Ok(0),
            Err(_) => Err(DriverError::Io),
        }
    }

    /// Handle DRM_IOCTL_GET_MODE
    fn handle_get_mode(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg points to [width, height, refresh] array to fill
        let mode_data = unsafe {
            slice::from_raw_parts_mut(arg as *mut u32, 3)
        };

        // Try to get actual display mode first
        if let Some(crtc) = device.crtcs.first() {
            if let Some(mode) = &crtc.mode {
                mode_data[0] = mode.hdisplay as u32;
                mode_data[1] = mode.vdisplay as u32;
                mode_data[2] = mode.vrefresh;
                return Ok(0);
            }
        }

        // Get mode from existing KMS framebuffer console
        // Use the bootloader framebuffer dimensions stored in VFS
        extern "C" {
            fn vfs_get_framebuffer_info() -> (u32, u32, u32);
        }

        let (width, height, _pitch) = unsafe { vfs_get_framebuffer_info() };

        if width > 0 && height > 0 {
            mode_data[0] = width;   // Bootloader framebuffer width
            mode_data[1] = height;  // Bootloader framebuffer height
            mode_data[2] = 60;      // Default refresh rate
            Ok(0)
        } else {
            // Final fallback - should always work
            mode_data[0] = 1280;
            mode_data[1] = 800;
            mode_data[2] = 60;
            Ok(0)
        }
    }

    /// Handle DRM_IOCTL_CREATE_FB
    fn handle_create_framebuffer(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg points to [width, height, format, fb_id_out, buffer_ptr_out, mmap_offset_out]
        let fb_data = unsafe {
            slice::from_raw_parts_mut(arg as *mut u32, 6)
        };

        let width = fb_data[0];
        let height = fb_data[1];
        let _format = fb_data[2];

        // Allocate dumb buffer
        let buffer = DrmDumbBuffer::create(width, height, 32)?;
        let mmap_offset = buffer.mmap_offset;

        // Create framebuffer object
        let mut fb = DrmFramebuffer::new(
            width,
            height,
            DrmFormat::Xrgb8888,
            buffer.handle,
            width * 4 // pitch
        );
        fb.physical_addresses[0] = mmap_offset as u64;

        let fb_id = fb.id().0;
        device.framebuffers.insert(fb.id(), fb);

        // Return results to userspace
        fb_data[3] = fb_id; // fb_id
        fb_data[4] = 0;     // No direct memory address - use mmap()
        fb_data[5] = mmap_offset as u32; // Pass physical address as mmap offset

        Ok(0)
    }

    /// Handle DRM_IOCTL_FLIP_PAGE with hardware scaling
    fn handle_flip_page(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg points to [fb_id, flags, src_width, src_height] for scaling support
        let flip_data = unsafe {
            slice::from_raw_parts(arg as *const u32, 4)
        };

        let fb_id = DrmObjectId(flip_data[0]);
        let _flags = flip_data[1];
        let src_width = if flip_data[2] != 0 { flip_data[2] } else { 320 };
        let src_height = if flip_data[3] != 0 { flip_data[3] } else { 200 };

        // Get first CRTC for page flip
        if let Some(crtc) = device.crtcs.first() {
            let crtc_id = crtc.id();

            // Get display dimensions
            let (display_width, display_height) = if let Some(mode) = &crtc.mode {
                (mode.hdisplay as u32, mode.vdisplay as u32)
            } else {
                // Fallback to VFS info if mode not initialized
                extern "C" {
                    fn vfs_get_framebuffer_info() -> (u32, u32, u32);
                }
                let (w, h, _) = unsafe { vfs_get_framebuffer_info() };
                (w, h)
            };

            if display_width == 0 || display_height == 0 {
                return Err(DriverError::NotFound);
            }

            // Set the new framebuffer on the primary plane with hardware scaling
            if let Some(plane) = device.planes.first() {
                let plane_id = plane.id();

                // Create atomic state for hardware-scaled page flip
                let mut atomic_state = AtomicModeSet::begin();

                // Use hardware scaling from source framebuffer to full display
                AtomicModeSet::set_plane_scaling(
                    &mut atomic_state,
                    plane_id,
                    crtc_id,
                    fb_id,
                    src_width,     // Source framebuffer dimensions (e.g., 320x200)
                    src_height,
                    0, 0,          // Destination position (full screen)
                    display_width, // Destination dimensions (e.g., 1280x800)
                    display_height
                )?;

                // Commit the atomic state with hardware scaling
                // Pass device directly to avoid deadlock
                AtomicModeSet::commit(device, atomic_state, 0)?;
                Ok(0)
            } else {
                Err(DriverError::NotFound)
            }
        } else {
            Err(DriverError::NotFound)
        }
    }

    /// Handle DRM_IOCTL_SET_PLANE
    fn handle_set_plane(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        // arg points to plane configuration data
        let plane_data = unsafe {
            slice::from_raw_parts(arg as *const u32, 12)
        };

        let plane_id = DrmObjectId(plane_data[0]);
        let crtc_id = if plane_data[1] != 0 { Some(DrmObjectId(plane_data[1])) } else { None };
        let fb_id = if plane_data[2] != 0 { Some(DrmObjectId(plane_data[2])) } else { None };
        let crtc_x = plane_data[3] as i32;
        let crtc_y = plane_data[4] as i32;
        let crtc_w = plane_data[5];
        let crtc_h = plane_data[6];
        let src_x = plane_data[7];
        let src_y = plane_data[8];
        let src_w = plane_data[9];
        let src_h = plane_data[10];

        let mut atomic_state = AtomicModeSet::begin();

        AtomicModeSet::set_plane(
            &mut atomic_state,
            plane_id,
            crtc_id,
            fb_id,
            crtc_x,
            crtc_y,
            crtc_w,
            crtc_h,
            src_x,
            src_y,
            src_w,
            src_h,
        );

        AtomicModeSet::commit(device, atomic_state, 0)?;
        Ok(0)
    }

    /// Handle DRM_IOCTL_GET_CAPS
    fn handle_get_capabilities(&mut self, arg: usize) -> Result<usize, DriverError> {
        // arg points to [capability, value_out]
        let caps_data = unsafe {
            slice::from_raw_parts_mut(arg as *mut u32, 2)
        };

        let capability = caps_data[0];

        let value = match capability {
            0x1 => 1, // DRM_CAP_DUMB_BUFFER - supported
            0x2 => 1, // DRM_CAP_VBLANK - supported
            0x3 => 0, // DRM_CAP_PRIME - not supported
            0x7 => 1, // DRM_CAP_ASYNC_PAGE_FLIP - supported
            0x8 => 64, // DRM_CAP_CURSOR_WIDTH
            0x9 => 64, // DRM_CAP_CURSOR_HEIGHT
            _ => 0,
        };

        caps_data[1] = value;
        Ok(0)
    }

    /// Handle DRM_IOCTL_MMAP - returns physical address of framebuffer
    fn handle_ioctl_mmap(&mut self, arg: usize) -> Result<usize, DriverError> {
        // arg points to a u64 which contains the requested physical address/offset
        if arg == 0 {
            return Err(DriverError::InvalidParameter);
        }

        let phys_addr_ptr = arg as *mut u64;
        let requested_phys = unsafe { *phys_addr_ptr };

        if requested_phys == 0 {
            // Default: return the hardware framebuffer
            extern "C" {
                fn vfs_get_framebuffer_base() -> u64;
            }

            let fb_base = unsafe { vfs_get_framebuffer_base() };
            if fb_base == 0 {
                return Err(DriverError::NotFound);
            }
            unsafe {
                *phys_addr_ptr = fb_base;
            }
        } else {
            // The physical address was passed as the offset to mmap()
            // We just return it back to the kernel to confirm we support mapping it.
            unsafe {
                *phys_addr_ptr = requested_phys;
            }
        }

        Ok(0)
    }

    /// Handle read operations (for events)
    pub fn handle_read(&mut self, _buffer: &mut [u8]) -> Result<usize, DriverError> {
        // For now, return no events
        // In a full implementation, this would return DRM events like vsync
        Ok(0)
    }

    /// Handle write operations (for framebuffer data)
    pub fn handle_write(&mut self, buffer: &[u8]) -> Result<usize, DriverError> {
        // Find the primary framebuffer and its buffer
        let device = get_drm_device();
        let mut device_lock = device.lock();
        
        // Use the first CRTC's current framebuffer
        if let Some(_crtc) = device_lock.crtcs.first() {
            if let Some(fb_id) = device_lock.planes.first().and_then(|p| p.fb_id) {
                if let Some(fb) = device_lock.get_framebuffer(fb_id) {
                    let src_phys = fb.physical_addresses[0];
                    if src_phys != 0 {
                        let src_virt = mm::phys_to_virt(src_phys as usize) as *mut u8;
                        let count = buffer.len().min(fb.size() as usize);
                        
                        // Copy data to the private DRM buffer
                        unsafe {
                            ptr::copy_nonoverlapping(buffer.as_ptr(), src_virt, count);
                        }
                        
                        // Now trigger the flip logic to perform the scaling copy to screen
                        // Create a dummy arg for flip_page
                        let flip_data = [fb_id.raw(), 0, fb.width, fb.height];
                        
                        // Pass device_lock directly
                        self.handle_flip_page(&mut device_lock, &flip_data as *const _ as usize)?;
                        
                        return Ok(count);
                    }
                }
            }
        }
        
        Err(DriverError::Unsupported)
    }

    /// Handle mmap operations for framebuffer access
    pub fn handle_mmap(&mut self, _offset: usize, size: usize) -> Result<*mut u8, DriverError> {
        // Get the real framebuffer base address from VFS
        extern "C" {
            fn vfs_get_framebuffer_base() -> u64;
        }

        let fb_base = unsafe { vfs_get_framebuffer_base() };
        if fb_base == 0 {
            return Err(DriverError::NotFound);
        }

        // VFS returns virtual address ready for userspace access
        let buffer_ptr = fb_base as *mut u8;

        // Validate the requested mapping size
        if size > 0x1000000 { // Limit to 16MB max for safety
            return Err(DriverError::Unsupported);
        }

        Ok(buffer_ptr)
    }
}

impl Driver for DrmDeviceInterface {
    fn probe(&mut self) -> Result<(), DriverError> {
        self.driver.probe()
    }

    fn handle(&mut self, msg: ipc::Message) -> ipc::Message {
        // Parse DRM ioctl from message
        let cmd = msg.tag as u32;
        let arg = if msg.data.len() >= 8 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&msg.data[0..8]);
            usize::from_le_bytes(bytes)
        } else {
            0
        };

        match self.handle_ioctl(cmd, arg) {
            Ok(result) => {
                let mut response = ipc::Message::empty();
                response.tag = 0; // Success
                let result_bytes = result.to_le_bytes();
                response.data[0..8].copy_from_slice(&result_bytes);
                response
            },
            Err(_) => {
                let mut response = ipc::Message::empty();
                response.tag = 1; // Error
                response
            },
        }
    }
}

/// DRM-specific dumb buffer structure
#[derive(Debug, Clone)]
pub struct DrmDumbBuffer {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub pitch: u32,
    pub size: u32,
    pub handle: u32,
    pub mmap_offset: usize,
}

impl DrmDumbBuffer {
    /// Create a dumb buffer for simple framebuffer access
    pub fn create(width: u32, height: u32, bpp: u32) -> Result<Self, DriverError> {
        let pitch = width * ((bpp + 7) / 8);
        let size = pitch * height;

        // Calculate pages and buddy order
        let pages = (size as usize + 4095) / 4096;
        let order = pages.next_power_of_two().trailing_zeros() as usize;

        // Allocate physical memory for the framebuffer
        // We use buddy_alloc to get contiguous physical memory
        let phys_addr = mm::buddy::alloc(order).ok_or(DriverError::Io)? as u64;

        // Zero the newly allocated buffer
        let virt_addr = mm::phys_to_virt(phys_addr as usize) as *mut u8;
        unsafe {
            ptr::write_bytes(virt_addr, 0, size as usize);
        }

        let handle = Self::next_handle();
        
        // mmap_offset for userspace will be the physical address
        // The syscall handler will use this to map the device memory
        let mmap_offset = phys_addr as usize;

        Ok(DrmDumbBuffer {
            width,
            height,
            bpp,
            pitch,
            size,
            handle,
            mmap_offset,
        })
    }

    /// Get next available handle
    fn next_handle() -> u32 {
        static mut NEXT_HANDLE: u32 = 1;
        unsafe {
            let handle = NEXT_HANDLE;
            NEXT_HANDLE += 1;
            handle
        }
    }
}

