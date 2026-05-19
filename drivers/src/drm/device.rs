//! DRM device management and resource tracking

use ::core::ptr;
use alloc::{vec::Vec, collections::BTreeMap};
use spin::Mutex;
use super::core::*;
use super::auth::*;
use super::framebuffer::*;
use super::super::DriverError;

/// DRM device capabilities
pub struct DrmDeviceCaps {
    pub dumb_buffer: bool,
    pub vblank_high_crtc: bool,
    pub dumb_preferred_depth: u32,
    pub dumb_prefer_shadow: bool,
    pub prime: bool,
    pub monotonic_timestamp: bool,
    pub atomic: bool,
}

impl Default for DrmDeviceCaps {
    fn default() -> Self {
        Self {
            dumb_buffer: true,
            vblank_high_crtc: true,
            dumb_preferred_depth: 32,
            dumb_prefer_shadow: false,
            prime: false,
            monotonic_timestamp: true,
            atomic: true,
        }
    }
}

/// DRM device state and resource management
pub struct DrmDevice {
    /// Device capabilities
    pub caps: DrmDeviceCaps,

    /// Master authentication
    pub master: Option<DrmMaster>,

    /// Authenticated clients
    pub auth_clients: Vec<DrmAuthToken>,

    /// Display resources
    pub crtcs: Vec<DrmCrtc>,
    pub connectors: Vec<DrmConnector>,
    pub encoders: Vec<DrmEncoder>,
    pub planes: Vec<DrmPlane>,

    /// Framebuffer objects
    pub framebuffers: BTreeMap<DrmObjectId, DrmFramebuffer>,

    /// Mode objects
    pub modes: Vec<DrmModeInfo>,

    /// Integration with existing drivers
    pub kms_integration: bool,
    pub fb_integration: bool,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

impl DrmDevice {
    pub fn new() -> Self {
        let mut device = Self {
            caps: DrmDeviceCaps::default(),
            master: None,
            auth_clients: Vec::new(),
            crtcs: Vec::new(),
            connectors: Vec::new(),
            encoders: Vec::new(),
            planes: Vec::new(),
            framebuffers: BTreeMap::new(),
            modes: Vec::new(),
            kms_integration: false,
            fb_integration: false,
        };

        // Initialize basic display pipeline
        device.init_display_pipeline();

        device
    }

    /// Initialize basic display pipeline (1 CRTC, 1 connector, 1 encoder)
    fn init_display_pipeline(&mut self) {
        // Create primary CRTC
        let crtc = DrmCrtc::new();
        let crtc_id = crtc.id();
        self.crtcs.push(crtc);

        // Create virtual connector
        let mut connector = DrmConnector::new(DrmConnectorType::Virtual);
        connector.detect();

        // Add standard modes
        connector.add_mode(DrmModeInfo::new(1920, 1080, 60));
        connector.add_mode(DrmModeInfo::new(1024, 768, 60));
        connector.add_mode(DrmModeInfo::new(800, 600, 60));

        self.connectors.push(connector);

        // Create encoder
        let mut encoder = DrmEncoder::new(1); // DRM_MODE_ENCODER_VIRTUAL
        encoder.set_crtc(crtc_id);
        self.encoders.push(encoder);

        // Create primary plane
        let primary_plane = DrmPlane::new(1); // DRM_PLANE_TYPE_PRIMARY
        self.planes.push(primary_plane);

        // Create cursor plane
        let cursor_plane = DrmPlane::new(2); // DRM_PLANE_TYPE_CURSOR
        self.planes.push(cursor_plane);
    }

    /// Acquire DRM master privilege
    pub fn set_master(&mut self, token: DrmAuthToken) -> Result<DrmMaster, DriverError> {
        if self.master.is_some() {
            return Err(DriverError::Unsupported); // Already has master
        }

        let master = DrmMaster::new(token);
        self.master = Some(master.clone());
        Ok(master)
    }

    /// Release DRM master privilege
    pub fn drop_master(&mut self, master: &DrmMaster) -> Result<(), DriverError> {
        if let Some(ref current_master) = self.master {
            if current_master.token.session_id == master.token.session_id {
                self.master = None;
                Ok(())
            } else {
                Err(DriverError::Unsupported)
            }
        } else {
            Err(DriverError::NotFound)
        }
    }

    /// Check if caller has master privileges
    pub fn is_master(&self, token: &DrmAuthToken) -> bool {
        if let Some(ref master) = self.master {
            master.token.session_id == token.session_id
        } else {
            false
        }
    }

    /// Authenticate a client
    pub fn authenticate(&mut self, magic: u32) -> Result<DrmAuthToken, DriverError> {
        // Simple authentication - in real implementation this would be more secure
        let token = DrmAuthToken::new_authenticated(magic);
        self.auth_clients.push(token.clone());
        Ok(token)
    }

    /// Get resources (CRTCs, connectors, encoders)
    pub fn get_resources(&self) -> DrmResources {
        DrmResources {
            crtcs: self.crtcs.iter().map(|c| c.id()).collect(),
            connectors: self.connectors.iter().map(|c| c.id()).collect(),
            encoders: self.encoders.iter().map(|e| e.id()).collect(),
            min_width: 320,
            max_width: 4096,
            min_height: 200,
            max_height: 4096,
        }
    }

    /// Get plane resources
    pub fn get_plane_resources(&self) -> DrmPlaneResources {
        DrmPlaneResources {
            planes: self.planes.iter().map(|p| p.id()).collect(),
        }
    }

    /// Get CRTC by ID
    pub fn get_crtc(&self, id: DrmObjectId) -> Option<&DrmCrtc> {
        self.crtcs.iter().find(|c| c.id() == id)
    }

    /// Get mutable CRTC by ID
    pub fn get_crtc_mut(&mut self, id: DrmObjectId) -> Option<&mut DrmCrtc> {
        self.crtcs.iter_mut().find(|c| c.id() == id)
    }

    /// Get connector by ID
    pub fn get_connector(&self, id: DrmObjectId) -> Option<&DrmConnector> {
        self.connectors.iter().find(|c| c.id() == id)
    }

    /// Get mutable connector by ID
    pub fn get_connector_mut(&mut self, id: DrmObjectId) -> Option<&mut DrmConnector> {
        self.connectors.iter_mut().find(|c| c.id() == id)
    }

    /// Get encoder by ID
    pub fn get_encoder(&self, id: DrmObjectId) -> Option<&DrmEncoder> {
        self.encoders.iter().find(|e| e.id() == id)
    }

    /// Get plane by ID
    pub fn get_plane(&self, id: DrmObjectId) -> Option<&DrmPlane> {
        self.planes.iter().find(|p| p.id() == id)
    }

    /// Get mutable plane by ID
    pub fn get_plane_mut(&mut self, id: DrmObjectId) -> Option<&mut DrmPlane> {
        self.planes.iter_mut().find(|p| p.id() == id)
    }

    /// Create a framebuffer object
    pub fn add_framebuffer(&mut self, fb: DrmFramebuffer) -> DrmObjectId {
        let id = fb.id();
        self.framebuffers.insert(id, fb);
        id
    }

    /// Remove a framebuffer object
    pub fn remove_framebuffer(&mut self, id: DrmObjectId) -> Result<(), DriverError> {
        self.framebuffers.remove(&id)
            .map(|_| ())
            .ok_or(DriverError::NotFound)
    }

    /// Get framebuffer by ID
    pub fn get_framebuffer(&self, id: DrmObjectId) -> Option<&DrmFramebuffer> {
        self.framebuffers.get(&id)
    }

    /// Set mode on a CRTC (legacy interface)
    pub fn set_crtc(&mut self, crtc_id: DrmObjectId, mode: Option<DrmModeInfo>,
                     x: u32, y: u32, connectors: &[DrmObjectId],
                     fb_id: Option<DrmObjectId>) -> Result<(), DriverError> {

        let crtc = self.get_crtc_mut(crtc_id)
            .ok_or(DriverError::NotFound)?;

        if let Some(mode) = mode {
            crtc.set_mode(mode)?;
            crtc.x = x as i32;
            crtc.y = y as i32;
        } else {
            crtc.disable();
        }

        // Update connectors (simplified - would need proper validation)
        for &connector_id in connectors {
            if let Some(_connector) = self.get_connector_mut(connector_id) {
                // Associate connector with CRTC
            }
        }

        // Set framebuffer on primary plane if provided
        if let Some(fb_id) = fb_id {
            if let Some(primary_plane) = self.planes.first_mut() {
                primary_plane.fb_id = Some(fb_id);
                primary_plane.crtc_id = Some(crtc_id);
            }
        }

        Ok(())
    }

    /// Atomic mode setting commit
    pub fn atomic_commit(&mut self, state: DrmAtomicState, _flags: u32) -> Result<(), DriverError> {
        // Apply CRTC states
        for (crtc_id, mode, enabled) in state.crtc_states {
            if let Some(crtc) = self.get_crtc_mut(crtc_id) {
                if enabled {
                    crtc.set_mode(mode)?;
                } else {
                    crtc.disable();
                }
            }
        }

        // Apply connector states
        for (_connector_id, _crtc_id) in state.connector_states {
            // Update connector to CRTC associations
        }

        // Apply plane states and perform scaling if needed
        for plane_state in state.plane_states {
            // Perform software scaling copy if integrated with framebuffer
            if self.fb_integration {
                self.perform_software_scaling(&plane_state)?;
            }

            if let Some(plane) = self.get_plane_mut(plane_state.plane_id) {
                plane.crtc_id = plane_state.crtc_id;
                plane.fb_id = plane_state.fb_id;
                plane.crtc_x = plane_state.crtc_x;
                plane.crtc_y = plane_state.crtc_y;
                plane.crtc_w = plane_state.crtc_w;
                plane.crtc_h = plane_state.crtc_h;
                plane.src_x = plane_state.src_x;
                plane.src_y = plane_state.src_y;
                plane.src_w = plane_state.src_w;
                plane.src_h = plane_state.src_h;
            }
        }

        Ok(())
    }

    /// Perform software scaling copy from a DRM framebuffer to the hardware framebuffer
    fn perform_software_scaling(&self, state: &DrmPlaneState) -> Result<(), DriverError> {
        let fb_id = state.fb_id.ok_or(DriverError::InvalidParameter)?;
        let drm_fb = self.framebuffers.get(&fb_id).ok_or(DriverError::NotFound)?;
        
        let (hw_base, hw_width, hw_height, hw_pitch) = crate::framebuffer::get_hardware_fb_info()
            .ok_or(DriverError::NotFound)?;

        let src_phys = drm_fb.physical_addresses[0];
        if src_phys == 0 { return Ok(()); } // No buffer bound

        // Map physical addresses to kernel virtual space
        let src_ptr = mm::phys_to_virt(src_phys as usize) as *const u32;
        let dst_ptr = mm::phys_to_virt(hw_base as usize) as *mut u32;

        let src_w = drm_fb.width as usize;
        let src_h = drm_fb.height as usize;
        let dst_w = hw_width as usize;
        let dst_h = hw_height as usize;
        let dst_pitch = hw_pitch as usize;

        // Perform fast copy if resolutions match
        if src_w == dst_w && src_h == dst_h {
            unsafe {
                ptr::copy_nonoverlapping(src_ptr, dst_ptr, src_w * src_h);
            }
            return Ok(());
        }

        // Nearest-neighbor scaling
        for dy in 0..dst_h {
            let sy = (dy * src_h) / dst_h;
            unsafe {
                let src_row = src_ptr.add(sy * src_w);
                let dst_row = dst_ptr.add(dy * (dst_pitch / 4));
                for dx in 0..dst_w {
                    let sx = (dx * src_w) / dst_w;
                    *dst_row.add(dx) = *src_row.add(sx);
                }
            }
        }

        Ok(())
    }

    /// Enable integration with existing KMS driver
    pub fn enable_kms_integration(&mut self) -> Result<(), DriverError> {
        // Try to detect and configure through existing KMS
        if let Ok(_mode) = crate::kms::init_kms() {
            self.kms_integration = true;
            Ok(())
        } else {
            Err(DriverError::NotFound)
        }
    }

    /// Enable integration with existing framebuffer driver
    pub fn enable_fb_integration(&mut self) -> Result<(), DriverError> {
        // Integration happens automatically through shared framebuffer
        self.fb_integration = true;
        Ok(())
    }
}

/// DRM resources structure returned to clients
pub struct DrmResources {
    pub crtcs: Vec<DrmObjectId>,
    pub connectors: Vec<DrmObjectId>,
    pub encoders: Vec<DrmObjectId>,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

/// DRM plane resources
pub struct DrmPlaneResources {
    pub planes: Vec<DrmObjectId>,
}

/// Global DRM device instance
static DRM_DEVICE: Mutex<DrmDevice> = Mutex::new(DrmDevice {
    caps: DrmDeviceCaps {
        dumb_buffer: true,
        vblank_high_crtc: true,
        dumb_preferred_depth: 32,
        dumb_prefer_shadow: false,
        prime: false,
        monotonic_timestamp: true,
        atomic: true,
    },
    master: None,
    auth_clients: Vec::new(),
    crtcs: Vec::new(),
    connectors: Vec::new(),
    encoders: Vec::new(),
    planes: Vec::new(),
    framebuffers: BTreeMap::new(),
    modes: Vec::new(),
    kms_integration: false,
    fb_integration: false,
});

/// Initialize DRM device
pub fn init_drm() -> Result<(), DriverError> {
    {
        let mut device = DRM_DEVICE.lock();
        *device = DrmDevice::new();
    }

    // Try to integrate with existing drivers (WITHOUT holding the lock)
    // to avoid deadlocks with ModeSet::set_display_mode
    if crate::kms::init_kms().is_ok() {
        DRM_DEVICE.lock().kms_integration = true;
    }
    
    DRM_DEVICE.lock().fb_integration = true;

    Ok(())
}

/// Get reference to global DRM device
pub fn get_drm_device() -> &'static Mutex<DrmDevice> {
    &DRM_DEVICE
}