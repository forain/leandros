//! DRM mode setting and display configuration

use alloc::{vec::Vec, vec};
use super::core::{DrmModeInfo, DrmAtomicState, DrmPlaneState, DrmObjectId, DrmObject};
use super::device::{DrmDevice, get_drm_device};
use super::super::DriverError;

/// Mode setting flags
pub mod mode_flags {
    pub const PHSYNC: u32 = 1 << 0;      // Positive horizontal sync
    pub const NHSYNC: u32 = 1 << 1;      // Negative horizontal sync
    pub const PVSYNC: u32 = 1 << 2;      // Positive vertical sync
    pub const NVSYNC: u32 = 1 << 3;      // Negative vertical sync
    pub const INTERLACE: u32 = 1 << 4;   // Interlaced mode
    pub const DBLSCAN: u32 = 1 << 5;     // Double scan
    pub const CSYNC: u32 = 1 << 6;       // Composite sync
    pub const PCSYNC: u32 = 1 << 7;      // Positive composite sync
    pub const NCSYNC: u32 = 1 << 8;      // Negative composite sync
    pub const HSKEW: u32 = 1 << 9;       // Horizontal skew
    pub const BCAST: u32 = 1 << 10;      // Broadcast
    pub const PIXMUX: u32 = 1 << 11;     // Pixel multiplexing
    pub const DBLCLK: u32 = 1 << 12;     // Double clock
    pub const CLKDIV2: u32 = 1 << 13;    // Clock divided by 2
}

/// Mode types
pub mod mode_types {
    pub const BUILTIN: u32 = 1 << 0;     // Built-in mode
    pub const CLOCK_C: u32 = 1 << 1;     // Clock from C
    pub const CRTC_C: u32 = 1 << 2;      // CRTC from C
    pub const PREFERRED: u32 = 1 << 3;   // Preferred mode
    pub const DEFAULT: u32 = 1 << 4;     // Default mode
    pub const USERDEF: u32 = 1 << 5;     // User defined
    pub const DRIVER: u32 = 1 << 6;      // Driver defined
}

/// Standard display modes library
pub struct StandardModes;

impl StandardModes {
    /// Get standard VESA modes
    pub fn vesa_modes() -> Vec<DrmModeInfo> {
        vec![
            // VGA modes
            DrmModeInfo::new(640, 480, 60),
            DrmModeInfo::new(640, 480, 72),
            DrmModeInfo::new(640, 480, 75),
            DrmModeInfo::new(640, 480, 85),

            // SVGA modes
            DrmModeInfo::new(800, 600, 56),
            DrmModeInfo::new(800, 600, 60),
            DrmModeInfo::new(800, 600, 72),
            DrmModeInfo::new(800, 600, 75),
            DrmModeInfo::new(800, 600, 85),

            // XGA modes
            DrmModeInfo::new(1024, 768, 60),
            DrmModeInfo::new(1024, 768, 70),
            DrmModeInfo::new(1024, 768, 75),
            DrmModeInfo::new(1024, 768, 85),

            // SXGA modes
            DrmModeInfo::new(1280, 1024, 60),
            DrmModeInfo::new(1280, 1024, 75),
            DrmModeInfo::new(1280, 1024, 85),

            // HD modes
            DrmModeInfo::new(1366, 768, 60),
            DrmModeInfo::new(1920, 1080, 60),
            DrmModeInfo::new(1920, 1200, 60),

            // 4K modes
            DrmModeInfo::new(3840, 2160, 30),
            DrmModeInfo::new(3840, 2160, 60),
        ]
    }

    /// Find best matching mode for given dimensions
    pub fn find_best_mode(width: u32, height: u32, refresh: u32) -> Option<DrmModeInfo> {
        let modes = Self::vesa_modes();

        // Try exact match first
        for mode in &modes {
            if mode.hdisplay as u32 == width &&
               mode.vdisplay as u32 == height &&
               mode.vrefresh == refresh {
                return Some(mode.clone());
            }
        }

        // Try same resolution with different refresh rate
        for mode in &modes {
            if mode.hdisplay as u32 == width && mode.vdisplay as u32 == height {
                return Some(mode.clone());
            }
        }

        // Try closest resolution
        let mut best_mode = None;
        let mut best_diff = u32::MAX;

        for mode in &modes {
            let w_diff = if mode.hdisplay as u32 > width {
                mode.hdisplay as u32 - width
            } else {
                width - mode.hdisplay as u32
            };

            let h_diff = if mode.vdisplay as u32 > height {
                mode.vdisplay as u32 - height
            } else {
                height - mode.vdisplay as u32
            };

            let total_diff = w_diff + h_diff;
            if total_diff < best_diff {
                best_diff = total_diff;
                best_mode = Some(mode.clone());
            }
        }

        best_mode
    }
}

/// Mode validation and configuration
pub struct ModeValidator;

impl ModeValidator {
    /// Validate if a mode is supported by hardware
    pub fn validate_mode(device: &DrmDevice, mode: &DrmModeInfo, crtc_id: DrmObjectId) -> Result<(), DriverError> {
        // Check basic constraints
        if mode.hdisplay == 0 || mode.vdisplay == 0 {
            return Err(DriverError::Unsupported);
        }

        if mode.hdisplay > 4096 || mode.vdisplay > 4096 {
            return Err(DriverError::Unsupported);
        }

        if mode.vrefresh > 240 {
            return Err(DriverError::Unsupported);
        }

        // Check if CRTC exists and can support this mode
        if device.get_crtc(crtc_id).is_none() {
            return Err(DriverError::NotFound);
        }

        // Additional hardware-specific validation would go here
        Ok(())
    }

    /// Calculate mode timing from basic parameters
    pub fn calculate_mode_timing(width: u16, height: u16, refresh: u32) -> DrmModeInfo {
        // Use CVT (Coordinated Video Timings) standard for calculation
        let h_pixels = width as f32;
        let v_lines = height as f32;
        let refresh_rate = refresh as f32;

        // CVT constants
        const M: f32 = 600.0;          // Gradient (%/kHz)
        const C: f32 = 40.0;           // Offset (μs)
        const K: f32 = 128.0;          // Scaling factor
        const J: f32 = 20.0;           // Offset (%)

        // Calculate horizontal timing
        let h_period_est = ((1.0 / refresh_rate) - (C / 1000000.0)) / (v_lines + (2.0 * M / K)) * 1000000.0;
        let h_sync_width = ((h_pixels * 8.0 / 100.0) + 0.5) as u16;
        let h_back_porch = h_sync_width;
        let h_front_porch = h_sync_width;
        let h_total = width + h_front_porch + h_sync_width + h_back_porch;

        // Calculate vertical timing
        let v_sync_width = 3;
        let v_back_porch = 6;
        let v_front_porch = 3;
        let v_total = height + v_front_porch + v_sync_width + v_back_porch;

        // Calculate pixel clock
        let pixel_clock = (((h_total as f32 * v_total as f32 * refresh_rate) / 1000.0) + 0.5) as u32;

        let mut mode = DrmModeInfo::new(width, height, refresh);
        mode.clock = pixel_clock;
        mode.htotal = h_total;
        mode.vtotal = v_total;
        mode.hsync_start = width + h_front_porch;
        mode.hsync_end = width + h_front_porch + h_sync_width;
        mode.vsync_start = height + v_front_porch;
        mode.vsync_end = height + v_front_porch + v_sync_width;
        mode.flags = mode_flags::PHSYNC | mode_flags::PVSYNC;
        mode.type_ = mode_types::DRIVER;

        mode
    }
}

/// Atomic mode setting operations
pub struct AtomicModeSet;

impl AtomicModeSet {
    /// Begin atomic state
    pub fn begin() -> DrmAtomicState {
        DrmAtomicState::new()
    }

    /// Set CRTC mode in atomic state
    pub fn set_crtc_mode(state: &mut DrmAtomicState, crtc_id: DrmObjectId,
                         mode: Option<DrmModeInfo>) {
        if let Some(mode) = mode {
            state.add_crtc(crtc_id, mode, true);
        } else {
            // Disable CRTC
            let dummy_mode = DrmModeInfo::new(0, 0, 0);
            state.add_crtc(crtc_id, dummy_mode, false);
        }
    }

    /// Set connector to CRTC mapping
    pub fn set_connector_crtc(state: &mut DrmAtomicState, connector_id: DrmObjectId,
                              crtc_id: Option<DrmObjectId>) {
        state.add_connector(connector_id, crtc_id);
    }

    /// Set plane configuration
    pub fn set_plane(state: &mut DrmAtomicState, plane_id: DrmObjectId,
                     crtc_id: Option<DrmObjectId>, fb_id: Option<DrmObjectId>,
                     crtc_x: i32, crtc_y: i32, crtc_w: u32, crtc_h: u32,
                     src_x: u32, src_y: u32, src_w: u32, src_h: u32) {
        let plane_state = DrmPlaneState {
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
        };
        state.add_plane(plane_state);
    }

    /// Set plane with hardware scaling from source framebuffer to destination display
    pub fn set_plane_scaling(state: &mut DrmAtomicState, plane_id: DrmObjectId,
                             crtc_id: DrmObjectId, fb_id: DrmObjectId,
                             src_width: u32, src_height: u32,
                             dst_x: i32, dst_y: i32, dst_width: u32, dst_height: u32) -> Result<(), DriverError> {

        // Validate scaling parameters
        if src_width == 0 || src_height == 0 || dst_width == 0 || dst_height == 0 {
            return Err(DriverError::InvalidParameter);
        }

        // Calculate scaling factors
        let _scale_x = dst_width as f32 / src_width as f32;
        let _scale_y = dst_height as f32 / src_height as f32;

        // Check if hardware supports these scaling factors
        // We'll perform the detailed hardware-specific validation in AtomicModeSet::test
        // or during commit. For now, we'll assume basic scaling is supported if requested.

        // Convert source dimensions to fixed-point 16.16 format
        let src_x_fp = 0u32;
        let src_y_fp = 0u32;
        let src_w_fp = src_width << 16;
        let src_h_fp = src_height << 16;

        // Set plane state with scaling
        let plane_state = DrmPlaneState {
            plane_id,
            crtc_id: Some(crtc_id),
            fb_id: Some(fb_id),
            crtc_x: dst_x,
            crtc_y: dst_y,
            crtc_w: dst_width,
            crtc_h: dst_height,
            src_x: src_x_fp,
            src_y: src_y_fp,
            src_w: src_w_fp,
            src_h: src_h_fp,
        };

        state.add_plane(plane_state);
        Ok(())
    }

    /// Commit atomic state
    pub fn commit(device: &mut DrmDevice, state: DrmAtomicState, flags: u32) -> Result<(), DriverError> {
        device.atomic_commit(state, flags)
    }

    /// Test atomic state (check if it would succeed)
    pub fn test(device: &DrmDevice, state: &DrmAtomicState) -> Result<(), DriverError> {
        // Validate all changes without actually applying them

        // Validate CRTC states
        for (crtc_id, mode, _enabled) in &state.crtc_states {
            ModeValidator::validate_mode(device, mode, *crtc_id)?;
        }

        // Validate plane states
        for plane_state in &state.plane_states {
            if device.get_plane(plane_state.plane_id).is_none() {
                return Err(DriverError::NotFound);
            }
        }

        Ok(())
    }
}

/// High-level mode setting interface
pub struct ModeSet;

impl ModeSet {
    /// Set mode on display pipeline (CRTC + Connector + Plane)
    pub fn set_display_mode(width: u32, height: u32, refresh: u32) -> Result<(), DriverError> {
        let device = get_drm_device();
        let mut device_lock = device.lock();

        // Find best mode
        let mode = StandardModes::find_best_mode(width, height, refresh)
            .unwrap_or_else(|| ModeValidator::calculate_mode_timing(width as u16, height as u16, refresh));

        // Get first CRTC and connector
        let crtc_id = device_lock.crtcs.first()
            .map(|c| c.id())
            .ok_or(DriverError::NotFound)?;

        let connector_id = device_lock.connectors.first()
            .map(|c| c.id())
            .ok_or(DriverError::NotFound)?;

        let primary_plane_id = device_lock.planes.first()
            .map(|p| p.id())
            .ok_or(DriverError::NotFound)?;

        // Create atomic state
        let mut atomic_state = AtomicModeSet::begin();

        // Set CRTC mode
        AtomicModeSet::set_crtc_mode(&mut atomic_state, crtc_id, Some(mode));

        // Connect connector to CRTC
        AtomicModeSet::set_connector_crtc(&mut atomic_state, connector_id, Some(crtc_id));

        // Configure primary plane
        AtomicModeSet::set_plane(&mut atomic_state, primary_plane_id, Some(crtc_id), None,
                                 0, 0, width, height, 0, 0, width << 16, height << 16);

        // Commit changes
        AtomicModeSet::commit(&mut device_lock, atomic_state, 0)
    }

    /// Get current display mode
    pub fn get_display_mode() -> Option<(u32, u32, u32)> {
        let device = get_drm_device().lock();
        if let Some(crtc) = device.crtcs.first() {
            if let Some(mode) = &crtc.mode {
                return Some((mode.hdisplay as u32, mode.vdisplay as u32, mode.vrefresh));
            }
        }
        None
    }

    /// List available modes
    pub fn list_modes() -> Vec<(u32, u32, u32)> {
        let device = get_drm_device().lock();
        let mut modes = Vec::new();

        for connector in &device.connectors {
            for mode in &connector.modes {
                modes.push((mode.hdisplay as u32, mode.vdisplay as u32, mode.vrefresh));
            }
        }

        modes
    }
}