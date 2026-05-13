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

// ── Standard Linux DRM IOCTL Constants ───────────────────────────────────────

const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xC04064A0;
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = 0xC05064A7;
const DRM_IOCTL_MODE_GETENCODER: u32 = 0xC01464A6;
const DRM_IOCTL_MODE_GETCRTC: u32 = 0xC06864A1;
const DRM_IOCTL_MODE_CREATE_DUMB: u32 = 0xC02064B2;
const DRM_IOCTL_MODE_MAP_DUMB: u32 = 0xC01064B3;
const DRM_IOCTL_MODE_ADDFB: u32 = 0xC01C64AE;
const DRM_IOCTL_MODE_SETCRTC: u32 = 0xC06864A2;
const DRM_IOCTL_MODE_PAGE_FLIP: u32 = 0xC01864B0;
const DRM_IOCTL_VERSION: u32 = 0xC0406400;

// ── Standard Linux DRM Structs ───────────────────────────────────────────────

#[repr(C)]
#[derive(Default)]
struct drm_mode_card_res {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_modeinfo {
    clock: u32,
    hdisplay: u16, hsync_start: u16, hsync_end: u16, htotal: u16, hskew: u16,
    vdisplay: u16, vsync_start: u16, vsync_end: u16, vtotal: u16, vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_connector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_encoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_crtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: drm_mode_modeinfo,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_create_dumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_map_dumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_fb_cmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_crtc_page_flip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

#[repr(C)]
#[derive(Default)]
struct drm_version {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: u64,
    date_len: usize,
    date: u64,
    desc_len: usize,
    desc: u64,
}

use alloc::collections::BTreeMap;
use spin::Mutex;

static DUMB_BUFFERS: Mutex<BTreeMap<u32, usize>> = Mutex::new(BTreeMap::new());

/// DRM device interface for userspace communication
pub struct DrmDeviceInterface {
    driver: DrmDriver,
    device_path: &'static str,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

impl DrmDeviceInterface {
    /// Create new DRM device interface
    pub fn new() -> Self {
        Self {
            driver: DrmDriver::new(),
            device_path: "/dev/dri/card0",
        }
    }

    /// Handle incoming IPC messages
    pub fn handle_ioctl(&mut self, cmd: u32, arg: usize) -> Result<usize, DriverError> {
        let device = get_drm_device();
        let mut device_lock = device.lock();

        // If this is a mode-setting or flip call, disable the kernel console
        if cmd == 0x1001 || cmd == 0x1004 || cmd == 0xC06864A2 || cmd == 0xC01864B0 {
            crate::framebuffer::set_console_disabled(true);
        }

        match cmd {
            // Mode setting ioctls (Custom LeandrOS)
            0x1001 => self.handle_set_mode(&mut device_lock, arg),
            0x1003 => self.handle_get_mode(&mut device_lock, arg),

            // Framebuffer ioctls (Custom LeandrOS)
            0x1002 => self.handle_create_framebuffer(&mut device_lock, arg),
            0x1004 => self.handle_flip_page(&mut device_lock, arg),
            0x1005 => self.handle_set_plane(&mut device_lock, arg),

            // Capability ioctls (Custom LeandrOS)
            0x1006 => self.handle_get_capabilities(arg),

            // Mmap ioctl - returns physical address for device mapping
            0x1007 => self.handle_ioctl_mmap(arg),

            // Standard Linux DRM IOCTLs
            DRM_IOCTL_VERSION => self.std_handle_version(arg),
            DRM_IOCTL_MODE_GETRESOURCES => self.std_handle_get_resources(&mut device_lock, arg),
            DRM_IOCTL_MODE_GETCONNECTOR => self.std_handle_get_connector(&mut device_lock, arg),
            DRM_IOCTL_MODE_GETENCODER => self.std_handle_get_encoder(&mut device_lock, arg),
            DRM_IOCTL_MODE_GETCRTC => self.std_handle_get_crtc(&mut device_lock, arg),
            DRM_IOCTL_MODE_CREATE_DUMB => self.std_handle_create_dumb(&mut device_lock, arg),
            DRM_IOCTL_MODE_MAP_DUMB => self.std_handle_map_dumb(&mut device_lock, arg),
            DRM_IOCTL_MODE_ADDFB => self.std_handle_addfb(&mut device_lock, arg),
            DRM_IOCTL_MODE_SETCRTC => self.std_handle_set_crtc(&mut device_lock, arg),
            DRM_IOCTL_MODE_PAGE_FLIP => self.std_handle_page_flip(&mut device_lock, arg),

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
            fn vfs_get_framebuffer_info(info: &mut FramebufferInfo);
        }

        let mut fb_info = FramebufferInfo { width: 0, height: 0, pitch: 0 };
        unsafe { vfs_get_framebuffer_info(&mut fb_info); }

        if fb_info.width > 0 && fb_info.height > 0 {
            mode_data[0] = fb_info.width;   // Bootloader framebuffer width
            mode_data[1] = fb_info.height;  // Bootloader framebuffer height
            mode_data[2] = 60;      // Default refresh rate
            Ok(0)
        } else {
            // Final fallback
            mode_data[0] = 640;
            mode_data[1] = 480;
            mode_data[2] = 60;
            Ok(0)
        }
    }

    /// Release DRM resources and re-enable kernel console
    pub fn release(&mut self) {
        crate::framebuffer::set_console_disabled(false);
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
                    fn vfs_get_framebuffer_info(info: &mut FramebufferInfo);
                }
                let mut info = FramebufferInfo { width: 0, height: 0, pitch: 0 };
                unsafe { vfs_get_framebuffer_info(&mut info); }
                (info.width, info.height)
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
                AtomicModeSet::set_plane(
                    &mut atomic_state,
                    plane_id,
                    Some(crtc_id),
                    Some(fb_id),
                    0, 0, display_width, display_height, // Dst
                    0, 0, src_width << 16, src_height << 16, // Src
                );

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

    // ── Standard Linux DRM IOCTL Handlers ─────────────────────────────────────

    fn std_handle_version(&mut self, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let v = unsafe { &mut *(arg as *mut drm_version) };
        v.version_major = 1;
        v.version_minor = 6;
        v.version_patchlevel = 0;

        let name = "leandros-drm\0";
        let date = "20261201\0";
        let desc = "LeandrOS DRM driver\0";

        if v.name != 0 && v.name_len >= name.len() {
            unsafe { ptr::copy_nonoverlapping(name.as_ptr(), v.name as *mut u8, name.len()); }
        }
        v.name_len = name.len();

        if v.date != 0 && v.date_len >= date.len() {
            unsafe { ptr::copy_nonoverlapping(date.as_ptr(), v.date as *mut u8, date.len()); }
        }
        v.date_len = date.len();

        if v.desc != 0 && v.desc_len >= desc.len() {
            unsafe { ptr::copy_nonoverlapping(desc.as_ptr(), v.desc as *mut u8, desc.len()); }
        }
        v.desc_len = desc.len();

        Ok(0)
    }

    fn std_handle_get_resources(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let res = unsafe { &mut *(arg as *mut drm_mode_card_res) };
        
        // We report 1 of each for a simple virtual device
        let crtc_ids = [1u32];
        let connector_ids = [1u32];
        let encoder_ids = [1u32];

        if res.crtc_id_ptr != 0 && res.count_crtcs >= 1 {
            unsafe { ptr::copy_nonoverlapping(crtc_ids.as_ptr(), res.crtc_id_ptr as *mut u32, 1); }
        }
        res.count_crtcs = 1;

        if res.connector_id_ptr != 0 && res.count_connectors >= 1 {
            unsafe { ptr::copy_nonoverlapping(connector_ids.as_ptr(), res.connector_id_ptr as *mut u32, 1); }
        }
        res.count_connectors = 1;

        if res.encoder_id_ptr != 0 && res.count_encoders >= 1 {
            unsafe { ptr::copy_nonoverlapping(encoder_ids.as_ptr(), res.encoder_id_ptr as *mut u32, 1); }
        }
        res.count_encoders = 1;

        res.min_width = 320;
        res.max_width = 4096;
        res.min_height = 200;
        res.max_height = 4096;

        Ok(0)
    }

    fn std_handle_get_connector(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let conn = unsafe { &mut *(arg as *mut drm_mode_get_connector) };
        
        conn.connector_id = 1;
        conn.connector_type = 11; // DRM_MODE_CONNECTOR_VIRTUAL
        conn.connector_type_id = 1;
        conn.connection = 1; // Connected
        conn.mm_width = 320;
        conn.mm_height = 200;

        if conn.encoders_ptr != 0 && conn.count_encoders >= 1 {
            let encoders = [1u32];
            unsafe { ptr::copy_nonoverlapping(encoders.as_ptr(), conn.encoders_ptr as *mut u32, 1); }
        }
        conn.count_encoders = 1;

        // Provide at least one mode
        if conn.modes_ptr != 0 && conn.count_modes >= 1 {
            extern "C" { fn vfs_get_framebuffer_info(info: &mut FramebufferInfo); }
            let mut info = FramebufferInfo { width: 0, height: 0, pitch: 0 };
            unsafe { vfs_get_framebuffer_info(&mut info); }
            let mut mode = drm_mode_modeinfo::default();
            mode.hdisplay = info.width as u16;
            mode.vdisplay = info.height as u16;
            mode.vrefresh = 60;
            mode.clock = (info.width * info.height * 60) / 1000;
            let name = b"Native\0";
            mode.name[..name.len()].copy_from_slice(name);
            
            unsafe { ptr::copy_nonoverlapping(&mode, conn.modes_ptr as *mut drm_mode_modeinfo, 1); }
        }
        conn.count_modes = 1;
        conn.encoder_id = 1;

        Ok(0)
    }

    fn std_handle_get_encoder(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let enc = unsafe { &mut *(arg as *mut drm_mode_get_encoder) };
        enc.encoder_id = 1;
        enc.encoder_type = 3; // DRM_MODE_ENCODER_VIRTUAL
        enc.crtc_id = 1;
        enc.possible_crtcs = 1;
        Ok(0)
    }

    fn std_handle_get_crtc(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let crtc_out = unsafe { &mut *(arg as *mut drm_mode_crtc) };
        let crtc_id = DrmObjectId(crtc_out.crtc_id);
        if let Some(crtc) = device.get_crtc(crtc_id) {
            // Find FB ID from planes associated with this CRTC
            crtc_out.fb_id = device.planes.iter()
                .find(|p| p.crtc_id == Some(crtc_id))
                .and_then(|p| p.fb_id)
                .map(|id| id.0)
                .unwrap_or(0);
            crtc_out.x = crtc.x as u32;
            crtc_out.y = crtc.y as u32;
            if let Some(mode) = &crtc.mode {
                crtc_out.mode.hdisplay = mode.hdisplay as u16;
                crtc_out.mode.vdisplay = mode.vdisplay as u16;
                crtc_out.mode.vrefresh = mode.vrefresh;
            }
            crtc_out.mode_valid = if crtc.mode.is_some() { 1 } else { 0 };
            Ok(0)
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn std_handle_create_dumb(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let create = unsafe { &mut *(arg as *mut drm_mode_create_dumb) };
        let buffer = DrmDumbBuffer::create(create.width, create.height, create.bpp)?;
        
        create.handle = buffer.handle;
        create.pitch = buffer.pitch;
        create.size = buffer.size as u64;

        Ok(0)
    }

    fn std_handle_map_dumb(&mut self, _device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let map = unsafe { &mut *(arg as *mut drm_mode_map_dumb) };

        // Return the hardware framebuffer base address to enable direct-to-screen rendering.
        // This is safe because we've disabled the competing kernel-side scaling copy.
        extern "C" { fn vfs_get_framebuffer_base() -> u64; }
        map.offset = unsafe { vfs_get_framebuffer_base() };

        Ok(0)
    }
    fn std_handle_addfb(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let add = unsafe { &mut *(arg as *mut drm_mode_fb_cmd) };
        
        let mut fb = DrmFramebuffer::new(
            add.width,
            add.height,
            DrmFormat::Xrgb8888,
            add.handle,
            add.pitch
        );
        
        // Use the physical address associated with the dumb buffer handle
        let phys_addr = DUMB_BUFFERS.lock().get(&add.handle).copied().unwrap_or(0);
        fb.physical_addresses[0] = phys_addr as u64;
        
        let fb_id = fb.id().0;
        device.framebuffers.insert(fb.id(), fb);
        add.fb_id = fb_id;
        
        Ok(0)
    }

    fn std_handle_set_crtc(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let set = unsafe { &mut *(arg as *mut drm_mode_crtc) };
        let crtc_id = DrmObjectId(set.crtc_id);
        let fb_id = Some(DrmObjectId(set.fb_id));
        let mode = Some(DrmModeInfo::new(set.mode.hdisplay, set.mode.vdisplay, set.mode.vrefresh));
        
        device.set_crtc(crtc_id, mode, set.x, set.y, &[], fb_id)?;
        Ok(0)
    }

    fn std_handle_page_flip(&mut self, device: &mut DrmDevice, arg: usize) -> Result<usize, DriverError> {
        if arg == 0 { return Err(DriverError::InvalidParameter); }
        let flip = unsafe { &mut *(arg as *mut drm_mode_crtc_page_flip) };

        let mut src_w = 320;
        let mut src_h = 200;
        if let Some(fb) = device.get_framebuffer(DrmObjectId(flip.fb_id)) {
            src_w = fb.width;
            src_h = fb.height;
        }

        let flip_args = [flip.fb_id, flip.flags, src_w, src_h];
        self.handle_flip_page(device, flip_args.as_ptr() as usize)
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
        DUMB_BUFFERS.lock().insert(handle, phys_addr as usize);
        
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

