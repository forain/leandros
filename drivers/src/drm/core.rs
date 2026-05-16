//! Core DRM structures and definitions
//!
//! Defines the fundamental DRM objects and their relationships.

use alloc::vec::Vec;
use spin::Mutex;
use super::super::DriverError;

/// DRM object types
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrmObjectType {
    Crtc = 0xcccccccc,
    Connector = 0xc0c0c0c0,
    Encoder = 0xe0e0e0e0,
    Mode = 0xdededede,
    Property = 0xb0b0b0b0,
    Plane = 0xeeeeeeee,
    Blob = 0xbbbbbbbb,
}

/// DRM object ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DrmObjectId(pub u32);

impl DrmObjectId {
    pub fn new() -> Self {
        static NEXT_ID: Mutex<u32> = Mutex::new(1);
        let mut id = NEXT_ID.lock();
        let current = *id;
        *id += 1;
        Self(current)
    }

    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Base DRM object
pub trait DrmObject {
    fn id(&self) -> DrmObjectId;
    fn object_type(&self) -> DrmObjectType;
}

/// DRM mode information
#[derive(Debug, Clone)]
pub struct DrmModeInfo {
    pub clock: u32,        // Pixel clock in kHz
    pub hdisplay: u16,     // Horizontal display size
    pub hsync_start: u16,  // Horizontal sync start
    pub hsync_end: u16,    // Horizontal sync end
    pub htotal: u16,       // Horizontal total size
    pub hskew: u16,        // Horizontal skew
    pub vdisplay: u16,     // Vertical display size
    pub vsync_start: u16,  // Vertical sync start
    pub vsync_end: u16,    // Vertical sync end
    pub vtotal: u16,       // Vertical total size
    pub vscan: u16,        // Vertical scan
    pub vrefresh: u32,     // Refresh rate in Hz
    pub flags: u32,        // Mode flags
    pub type_: u32,        // Mode type
    pub name: [u8; 32],    // Mode name
}

impl DrmModeInfo {
    pub fn new(width: u16, height: u16, refresh: u32) -> Self {
        // Calculate timing parameters (simplified)
        let htotal = width + (width / 8);  // Add 12.5% horizontal blank
        let vtotal = height + (height / 20); // Add 5% vertical blank
        let clock = ((htotal as u32 * vtotal as u32 * refresh) + 500) / 1000;

        // Create name string manually since format! isn't available
        let mut name = [0u8; 32];
        let width_str = match width {
            640 => "640",
            800 => "800",
            1024 => "1024",
            1280 => "1280",
            1366 => "1366",
            1920 => "1920",
            3840 => "3840",
            _ => "unknown",
        };
        let height_str = match height {
            480 => "480",
            600 => "600",
            768 => "768",
            1024 => "1024",
            1080 => "1080",
            1200 => "1200",
            2160 => "2160",
            _ => "unknown",
        };

        let mut pos = 0;
        for &b in width_str.as_bytes() {
            if pos < 31 { name[pos] = b; pos += 1; }
        }
        if pos < 31 { name[pos] = b'x'; pos += 1; }
        for &b in height_str.as_bytes() {
            if pos < 31 { name[pos] = b; pos += 1; }
        }

        Self {
            clock,
            hdisplay: width,
            hsync_start: width + 1,
            hsync_end: width + (width / 32),
            htotal,
            hskew: 0,
            vdisplay: height,
            vsync_start: height + 1,
            vsync_end: height + 3,
            vtotal,
            vscan: 0,
            vrefresh: refresh,
            flags: 0,
            type_: 0x40, // DRM_MODE_TYPE_DRIVER
            name,
        }
    }
}

/// CRTC (Cathode Ray Tube Controller) - controls display timing
pub struct DrmCrtc {
    id: DrmObjectId,
    pub x: i32,
    pub y: i32,
    pub gamma_size: u32,
    pub possible_crtcs: u32,
    pub mode: Option<DrmModeInfo>,
    pub enabled: bool,
}

impl DrmCrtc {
    pub fn new() -> Self {
        Self {
            id: DrmObjectId::new(),
            x: 0,
            y: 0,
            gamma_size: 256,
            possible_crtcs: 1,
            mode: None,
            enabled: false,
        }
    }

    pub fn set_mode(&mut self, mode: DrmModeInfo) -> Result<(), DriverError> {
        self.mode = Some(mode);
        self.enabled = true;
        Ok(())
    }

    pub fn disable(&mut self) {
        self.enabled = false;
        self.mode = None;
    }
}

impl DrmObject for DrmCrtc {
    fn id(&self) -> DrmObjectId { self.id }
    fn object_type(&self) -> DrmObjectType { DrmObjectType::Crtc }
}

/// Connector types
#[repr(u32)]
#[derive(Debug, Clone, Copy)]
pub enum DrmConnectorType {
    Unknown = 0,
    Vga = 1,
    DviI = 2,
    DviD = 3,
    DviA = 4,
    Composite = 5,
    Svideo = 6,
    Lvds = 7,
    Component = 8,
    Din9 = 9,
    DisplayPort = 10,
    Hdmi = 11,
    HdmiB = 12,
    Tv = 13,
    Edp = 14,
    Virtual = 15,
    Dsi = 16,
}

/// Connection states
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrmConnectorStatus {
    Connected = 1,
    Disconnected = 2,
    Unknown = 3,
}

/// DRM Connector - represents physical outputs
pub struct DrmConnector {
    id: DrmObjectId,
    pub connector_type: DrmConnectorType,
    pub connector_type_id: u32,
    pub status: DrmConnectorStatus,
    pub modes: Vec<DrmModeInfo>,
    pub possible_crtcs: u32,
    pub dpms: u32,
}

impl DrmConnector {
    pub fn new(connector_type: DrmConnectorType) -> Self {
        Self {
            id: DrmObjectId::new(),
            connector_type,
            connector_type_id: 1,
            status: DrmConnectorStatus::Unknown,
            modes: Vec::new(),
            possible_crtcs: 1,
            dpms: 0, // DRM_MODE_DPMS_ON
        }
    }

    pub fn add_mode(&mut self, mode: DrmModeInfo) {
        self.modes.push(mode);
    }

    pub fn set_status(&mut self, status: DrmConnectorStatus) {
        self.status = status;
    }

    pub fn detect(&mut self) -> DrmConnectorStatus {
        // Simplified detection - always return connected for virtual displays
        self.status = DrmConnectorStatus::Connected;
        self.status
    }
}

impl DrmObject for DrmConnector {
    fn id(&self) -> DrmObjectId { self.id }
    fn object_type(&self) -> DrmObjectType { DrmObjectType::Connector }
}

/// DRM Encoder - converts CRTC output to connector format
pub struct DrmEncoder {
    id: DrmObjectId,
    pub encoder_type: u32,
    pub crtc_id: Option<DrmObjectId>,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

impl DrmEncoder {
    pub fn new(encoder_type: u32) -> Self {
        Self {
            id: DrmObjectId::new(),
            encoder_type,
            crtc_id: None,
            possible_crtcs: 1,
            possible_clones: 0,
        }
    }

    pub fn set_crtc(&mut self, crtc_id: DrmObjectId) {
        self.crtc_id = Some(crtc_id);
    }
}

impl DrmObject for DrmEncoder {
    fn id(&self) -> DrmObjectId { self.id }
    fn object_type(&self) -> DrmObjectType { DrmObjectType::Encoder }
}

/// DRM Plane - hardware overlay support with scaling capabilities
pub struct DrmPlane {
    id: DrmObjectId,
    pub plane_type: u32,
    pub possible_crtcs: u32,
    pub formats: Vec<u32>,
    pub crtc_id: Option<DrmObjectId>,
    pub fb_id: Option<DrmObjectId>,

    // Destination (CRTC) coordinates and size
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,

    // Source framebuffer coordinates and size (fixed-point 16.16)
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,

    // Hardware scaling capabilities
    pub scaling_supported: bool,
    pub max_upscale: u32,    // Maximum upscaling factor (e.g., 8 = 8x upscale)
    pub max_downscale: u32,  // Maximum downscaling factor (e.g., 4 = 1/4 downscale)
}

impl DrmPlane {
    pub fn new(plane_type: u32) -> Self {
        let mut formats = Vec::new();
        formats.push(0x34325258); // DRM_FORMAT_XRGB8888
        formats.push(0x34324752); // DRM_FORMAT_ARGB8888

        Self {
            id: DrmObjectId::new(),
            plane_type,
            possible_crtcs: 1,
            formats,
            crtc_id: None,
            fb_id: None,
            crtc_x: 0,
            crtc_y: 0,
            crtc_w: 0,
            crtc_h: 0,
            src_x: 0,
            src_y: 0,
            src_w: 0,
            src_h: 0,
            scaling_supported: true,
            max_upscale: 8,    // Support up to 8x upscaling
            max_downscale: 4,  // Support down to 1/4 downscaling
        }
    }

    /// Set plane scaling from source to destination
    pub fn set_scaling(&mut self,
                      src_x: u32, src_y: u32, src_w: u32, src_h: u32,
                      crtc_x: i32, crtc_y: i32, crtc_w: u32, crtc_h: u32) -> Result<(), DriverError> {

        if !self.scaling_supported {
            return Err(DriverError::Unsupported);
        }

        // Validate scaling factors
        let upscale_x = if src_w > 0 { (crtc_w + (src_w >> 16) - 1) / (src_w >> 16) } else { 1 };
        let upscale_y = if src_h > 0 { (crtc_h + (src_h >> 16) - 1) / (src_h >> 16) } else { 1 };
        let downscale_x = if crtc_w > 0 { ((src_w >> 16) + crtc_w - 1) / crtc_w } else { 1 };
        let downscale_y = if crtc_h > 0 { ((src_h >> 16) + crtc_h - 1) / crtc_h } else { 1 };

        if upscale_x > self.max_upscale || upscale_y > self.max_upscale {
            return Err(DriverError::InvalidParameter);
        }
        if downscale_x > self.max_downscale || downscale_y > self.max_downscale {
            return Err(DriverError::InvalidParameter);
        }

        // Set the scaling parameters
        self.src_x = src_x;
        self.src_y = src_y;
        self.src_w = src_w;
        self.src_h = src_h;
        self.crtc_x = crtc_x;
        self.crtc_y = crtc_y;
        self.crtc_w = crtc_w;
        self.crtc_h = crtc_h;

        Ok(())
    }

    /// Calculate scaling factors for debugging/info
    pub fn get_scale_factors(&self) -> (f32, f32) {
        let scale_x = if self.src_w > 0 {
            (self.crtc_w as f32) / ((self.src_w >> 16) as f32)
        } else { 1.0 };
        let scale_y = if self.src_h > 0 {
            (self.crtc_h as f32) / ((self.src_h >> 16) as f32)
        } else { 1.0 };
        (scale_x, scale_y)
    }
}

impl DrmObject for DrmPlane {
    fn id(&self) -> DrmObjectId { self.id }
    fn object_type(&self) -> DrmObjectType { DrmObjectType::Plane }
}

/// DRM atomic state for mode setting operations
pub struct DrmAtomicState {
    pub crtc_states: Vec<(DrmObjectId, DrmModeInfo, bool)>, // (crtc_id, mode, enabled)
    pub connector_states: Vec<(DrmObjectId, Option<DrmObjectId>)>, // (connector_id, crtc_id)
    pub plane_states: Vec<DrmPlaneState>,
}

pub struct DrmPlaneState {
    pub plane_id: DrmObjectId,
    pub crtc_id: Option<DrmObjectId>,
    pub fb_id: Option<DrmObjectId>,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
}

impl DrmAtomicState {
    pub fn new() -> Self {
        Self {
            crtc_states: Vec::new(),
            connector_states: Vec::new(),
            plane_states: Vec::new(),
        }
    }

    pub fn add_crtc(&mut self, crtc_id: DrmObjectId, mode: DrmModeInfo, enabled: bool) {
        self.crtc_states.push((crtc_id, mode, enabled));
    }

    pub fn add_connector(&mut self, connector_id: DrmObjectId, crtc_id: Option<DrmObjectId>) {
        self.connector_states.push((connector_id, crtc_id));
    }

    pub fn add_plane(&mut self, state: DrmPlaneState) {
        self.plane_states.push(state);
    }
}