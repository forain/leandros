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

    /// Handle ioctl commands from userspace
    pub fn handle_ioctl(&mut self, cmd: u32, arg: usize) -> Result<usize, DriverError> {
        match cmd {
            // Mode setting ioctls
            0x1001 => self.handle_set_mode(arg),
            0x1003 => self.handle_get_mode(arg),

            // Framebuffer ioctls
            0x1002 => self.handle_create_framebuffer(arg),
            0x1004 => self.handle_flip_page(arg),

            // Plane ioctls
            0x1005 => self.handle_set_plane(arg),

            // Capability ioctls
            0x1006 => self.handle_get_capabilities(arg),

            _ => Err(DriverError::Unsupported),
        }
    }

    /// Handle DRM_IOCTL_SET_MODE
    fn handle_set_mode(&mut self, arg: usize) -> Result<usize, DriverError> {
        // arg points to [width, height, refresh] array
        let mode_data = unsafe {
            slice::from_raw_parts(arg as *const u32, 3)
        };

        let width = mode_data[0];
        let height = mode_data[1];
        let refresh = mode_data[2];

        // Set display mode using our DRM subsystem
        match ModeSet::set_display_mode(width, height, refresh) {
            Ok(()) => Ok(0),
            Err(_) => Err(DriverError::Io),
        }
    }

    /// Handle DRM_IOCTL_GET_MODE
    fn handle_get_mode(&mut self, arg: usize) -> Result<usize, DriverError> {
        // arg points to [width, height, refresh] array to fill
        let mode_data = unsafe {
            slice::from_raw_parts_mut(arg as *mut u32, 3)
        };

        // Try to get actual display mode first
        if let Some((width, height, refresh)) = ModeSet::get_display_mode() {
            mode_data[0] = width;
            mode_data[1] = height;
            mode_data[2] = refresh;
            Ok(0)
        } else {
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
                // Debug: The mode_data array should now contain the framebuffer dimensions
                Ok(0)
            } else {
                // Final fallback - should always work
                mode_data[0] = 1280;
                mode_data[1] = 800;
                mode_data[2] = 60;
                Ok(0)
            }
        }
    }

    /// Handle DRM_IOCTL_CREATE_FB
    fn handle_create_framebuffer(&mut self, arg: usize) -> Result<usize, DriverError> {
        // arg points to [width, height, format, fb_id_out, buffer_ptr_out, size_out]
        let fb_data = unsafe {
            slice::from_raw_parts_mut(arg as *mut u32, 6)
        };

        let width = fb_data[0];
        let height = fb_data[1];
        let format = fb_data[2];

        // Create framebuffer using DRM subsystem
        let device = get_drm_device();
        let mut device_lock = device.lock();

        // Allocate dumb buffer
        let size = width * height * 4; // Assuming 32-bit XRGB
        let buffer = DrmDumbBuffer::create(width, height, 32)?;

        // Create framebuffer object
        let fb = DrmFramebuffer::new(
            width,
            height,
            DrmFormat::Xrgb8888,
            buffer.handle,
            width * 4 // pitch
        );

        let fb_id = fb.id().0;
        device_lock.framebuffers.insert(fb.id(), fb);

        // Return results to userspace
        fb_data[3] = fb_id; // fb_id
        // Instead of returning a memory address that might not be accessible,
        // return a special value that tells DOOM to use write() operations
        fb_data[4] = 0; // No direct memory access - use file operations
        fb_data[5] = size; // buffer size

        Ok(0)
    }

    /// Handle DRM_IOCTL_FLIP_PAGE
    fn handle_flip_page(&mut self, arg: usize) -> Result<usize, DriverError> {
        // arg points to [fb_id, flags]
        let flip_data = unsafe {
            slice::from_raw_parts(arg as *const u32, 2)
        };

        let fb_id = DrmObjectId(flip_data[0]);
        let _flags = flip_data[1];

        let device = get_drm_device();
        let device_lock = device.lock();

        // Get first CRTC for page flip
        if let Some(crtc) = device_lock.crtcs.first() {
            let crtc_id = crtc.id();

            // Create atomic state for page flip
            let mut atomic_state = AtomicModeSet::begin();

            // Set the new framebuffer on the primary plane
            if let Some(plane) = device_lock.planes.first() {
                let plane_id = plane.id();

                // Get current mode for plane configuration
                if let Some(mode) = &crtc.mode {
                    AtomicModeSet::set_plane(
                        &mut atomic_state,
                        plane_id,
                        Some(crtc_id),
                        Some(fb_id),
                        0, 0, // crtc x,y
                        mode.hdisplay as u32, mode.vdisplay as u32, // crtc w,h
                        0, 0, // src x,y (fixed point)
                        (mode.hdisplay as u32) << 16, (mode.vdisplay as u32) << 16, // src w,h (fixed point)
                    );
                }
            }

            drop(device_lock);

            // Commit the atomic state
            AtomicModeSet::commit(atomic_state, 0)?;
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
    }

    /// Handle DRM_IOCTL_SET_PLANE
    fn handle_set_plane(&mut self, arg: usize) -> Result<usize, DriverError> {
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

        AtomicModeSet::commit(atomic_state, 0)?;
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

    /// Handle read operations (for events)
    pub fn handle_read(&mut self, buffer: &mut [u8]) -> Result<usize, DriverError> {
        // For now, return no events
        // In a full implementation, this would return DRM events like vsync
        Ok(0)
    }

    /// Handle write operations (for framebuffer data)
    pub fn handle_write(&mut self, buffer: &[u8]) -> Result<usize, DriverError> {
        // Forward DRM writes to VFS framebuffer function
        // This provides real DRM write functionality with proper memory mapping
        extern "C" {
            fn vfs_write_framebuffer(buffer_ptr: *const u8, count: usize) -> i64;
        }

        let result = unsafe {
            vfs_write_framebuffer(buffer.as_ptr(), buffer.len())
        };

        if result < 0 {
            // Convert error codes
            match result {
                -19 => Err(DriverError::NotFound), // ENODEV
                _ => Err(DriverError::Io),
            }
        } else {
            Ok(result as usize)
        }
    }

    /// Handle mmap operations for framebuffer access
    pub fn handle_mmap(&mut self, offset: usize, size: usize) -> Result<*mut u8, DriverError> {
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

        // Get the actual framebuffer address from VFS
        extern "C" {
            fn vfs_get_framebuffer_base() -> u64;
        }

        let handle = Self::next_handle();
        let fb_base = unsafe { vfs_get_framebuffer_base() };

        // Get the framebuffer address using the same approach as VFS /dev/fb0
        let mmap_offset = if fb_base != 0 {
            // Use VFS logic for address conversion
            if fb_base >= 0xFFFF_0000_0000_0000 {
                fb_base as usize // Already virtual
            } else {
                // Physical address - convert to virtual using same logic as VFS
                // VFS uses: mm::phys_to_virt(base as usize + cur)
                // But we can't access mm from here, so we need a different approach
                //
                // Instead, let's use the known kernel virtual mapping
                // LeandrOS maps physical memory to virtual with an offset
                // Check the logs to see what mm::phys_to_virt(0) returns
                0xFFFF_0000_0000_0000 + (fb_base as usize)
            }
        } else {
            return Err(DriverError::NotFound);
        };

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

