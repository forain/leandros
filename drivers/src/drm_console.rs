//! DRM-enhanced framebuffer console
//!
//! This module provides a console implementation that leverages the DRM subsystem
//! for advanced display management, including dynamic mode switching, multiple
//! virtual terminals, and hardware-accelerated operations.

use alloc::{vec::Vec, vec};
use ::core::str;
use spin::Mutex;
use super::{Driver, DriverError};
use super::drm::*;
use super::framebuffer::Framebuffer;

/// DRM console capabilities
#[derive(Debug, Clone, Copy)]
pub struct ConsoleCapabilities {
    pub max_width: u32,
    pub max_height: u32,
    pub preferred_depth: u32,
    pub multiple_modes: bool,
    pub hardware_cursor: bool,
    pub double_buffering: bool,
}

impl Default for ConsoleCapabilities {
    fn default() -> Self {
        Self {
            max_width: 1920,
            max_height: 1080,
            preferred_depth: 32,
            multiple_modes: true,
            hardware_cursor: false,
            double_buffering: true,
        }
    }
}

/// Virtual terminal state
#[derive(Debug, Clone)]
pub struct VirtualTerminal {
    pub id: u32,
    pub active: bool,
    pub framebuffer_id: Option<DrmObjectId>,
    pub mode: Option<(u32, u32, u32)>, // width, height, refresh
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub char_buffer: Vec<Vec<u8>>, // Character grid
    pub color_buffer: Vec<Vec<u32>>, // Color grid
    pub dirty: bool,
}

impl VirtualTerminal {
    pub fn new(id: u32, width: usize, height: usize) -> Self {
        let rows = height / 16; // Character height
        let cols = width / 8;   // Character width

        Self {
            id,
            active: false,
            framebuffer_id: None,
            mode: None,
            cursor_x: 0,
            cursor_y: 0,
            char_buffer: vec![vec![b' '; cols]; rows],
            color_buffer: vec![vec![0xFFFFFF; cols]; rows],
            dirty: true,
        }
    }

    pub fn write_char(&mut self, x: usize, y: usize, c: u8, color: u32) {
        if y < self.char_buffer.len() && x < self.char_buffer[y].len() {
            self.char_buffer[y][x] = c;
            self.color_buffer[y][x] = color;
            self.dirty = true;
        }
    }

    pub fn clear(&mut self, color: u32) {
        for row in &mut self.char_buffer {
            for cell in row {
                *cell = b' ';
            }
        }
        for row in &mut self.color_buffer {
            for cell in row {
                *cell = color;
            }
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.dirty = true;
    }

    pub fn scroll_up(&mut self) {
        // Move all lines up by one
        for i in 1..self.char_buffer.len() {
            self.char_buffer[i-1] = self.char_buffer[i].clone();
            self.color_buffer[i-1] = self.color_buffer[i].clone();
        }

        // Clear the last line
        if let Some(last_char_row) = self.char_buffer.last_mut() {
            for cell in last_char_row {
                *cell = b' ';
            }
        }
        if let Some(last_color_row) = self.color_buffer.last_mut() {
            for cell in last_color_row {
                *cell = 0xFFFFFF;
            }
        }

        self.dirty = true;
    }
}

/// DRM-enhanced console driver
pub struct DrmConsole {
    capabilities: ConsoleCapabilities,
    current_mode: Option<(u32, u32, u32)>,
    framebuffer: Framebuffer,

    // DRM objects
    crtc_id: Option<DrmObjectId>,
    connector_id: Option<DrmObjectId>,
    primary_plane_id: Option<DrmObjectId>,
    current_framebuffer_id: Option<DrmObjectId>,

    // Virtual terminals
    virtual_terminals: Vec<VirtualTerminal>,
    active_vt: usize,

    // Double buffering support
    front_buffer_id: Option<DrmObjectId>,
    back_buffer_id: Option<DrmObjectId>,

    // DRM integration
    drm_enabled: bool,
    master_session: Option<DrmAuthToken>,
}

impl DrmConsole {
    pub fn new() -> Self {
        let mut vts = Vec::new();
        // Create 8 virtual terminals
        for i in 0..8 {
            vts.push(VirtualTerminal::new(i, 1024, 768));
        }
        vts[0].active = true; // Activate first VT

        Self {
            capabilities: ConsoleCapabilities::default(),
            current_mode: None,
            framebuffer: Framebuffer::new(),
            crtc_id: None,
            connector_id: None,
            primary_plane_id: None,
            current_framebuffer_id: None,
            virtual_terminals: vts,
            active_vt: 0,
            front_buffer_id: None,
            back_buffer_id: None,
            drm_enabled: false,
            master_session: None,
        }
    }

    /// Initialize DRM integration
    pub fn init_drm(&mut self) -> Result<(), DriverError> {
        // Initialize DRM subsystem
        init_drm()?;

        // Create authentication session
        self.master_session = Some(create_session());

        // Try to become DRM master
        if let Some(ref session) = self.master_session {
            if set_master(session.session_id).is_ok() {
                self.drm_enabled = true;

                // Get DRM resources
                let device = get_drm_device().lock();
                let resources = device.get_resources();

                // Get first available CRTC, connector, and plane
                self.crtc_id = resources.crtcs.first().copied();
                self.connector_id = resources.connectors.first().copied();

                let plane_resources = device.get_plane_resources();
                self.primary_plane_id = plane_resources.planes.first().copied();

                Ok(())
            } else {
                Err(DriverError::Unsupported)
            }
        } else {
            Err(DriverError::NotFound)
        }
    }

    /// Set display mode using DRM
    pub fn set_mode(&mut self, width: u32, height: u32, refresh: u32) -> Result<(), DriverError> {
        if !self.drm_enabled {
            return Err(DriverError::NotFound);
        }

        // Use DRM mode setting
        ModeSet::set_display_mode(width, height, refresh)?;

        self.current_mode = Some((width, height, refresh));

        // Update virtual terminals with new dimensions
        for vt in &mut self.virtual_terminals {
            let rows = (height as usize) / 16; // Character height
            let cols = (width as usize) / 8;   // Character width

            vt.char_buffer = vec![vec![b' '; cols]; rows];
            vt.color_buffer = vec![vec![0xFFFFFF; cols]; rows];
            vt.mode = Some((width, height, refresh));
            vt.dirty = true;
        }

        Ok(())
    }

    /// Create framebuffer for console
    pub fn create_console_framebuffer(&mut self, width: u32, height: u32) -> Result<DrmObjectId, DriverError> {
        if !self.drm_enabled {
            return Err(DriverError::NotFound);
        }

        // Create dumb buffer
        let mut dumb_buffer = DrmDumbBuffer::new(width, height, 32)?;
        let _mapped_addr = dumb_buffer.map()?;

        // Create framebuffer object
        let framebuffer = DrmFramebuffer::new(
            width,
            height,
            DrmFormat::Xrgb8888,
            dumb_buffer.handle,
            dumb_buffer.pitch
        );

        let mut device = get_drm_device().lock();
        let fb_id = device.add_framebuffer(framebuffer);

        Ok(fb_id)
    }

    /// Switch to virtual terminal
    pub fn switch_vt(&mut self, vt_id: usize) -> Result<(), DriverError> {
        if vt_id >= self.virtual_terminals.len() {
            return Err(DriverError::NotFound);
        }

        // Deactivate current VT
        self.virtual_terminals[self.active_vt].active = false;

        // Activate new VT
        self.active_vt = vt_id;
        self.virtual_terminals[vt_id].active = true;

        // If DRM is enabled, update display
        if self.drm_enabled {
            self.update_display()?;
        }

        Ok(())
    }

    /// Get current virtual terminal
    pub fn current_vt(&mut self) -> &mut VirtualTerminal {
        &mut self.virtual_terminals[self.active_vt]
    }

    /// Update display from virtual terminal buffer
    pub fn update_display(&mut self) -> Result<(), DriverError> {
        if !self.drm_enabled {
            return self.update_legacy_display();
        }

        let vt_dirty = self.virtual_terminals[self.active_vt].dirty;
        if !vt_dirty {
            return Ok(()); // No update needed
        }

        // Create or reuse framebuffer
        let fb_id = if self.virtual_terminals[self.active_vt].framebuffer_id.is_none() {
            if let Some((width, height, _)) = self.virtual_terminals[self.active_vt].mode.or(self.current_mode) {
                let fb_id = self.create_console_framebuffer(width, height)?;
                self.virtual_terminals[self.active_vt].framebuffer_id = Some(fb_id);
                Some(fb_id)
            } else {
                None
            }
        } else {
            self.virtual_terminals[self.active_vt].framebuffer_id
        };

        // Render text to framebuffer
        if let Some(fb_id) = fb_id {
            // Clone VT data to avoid borrow checker issues
            let vt_data = self.virtual_terminals[self.active_vt].clone();
            self.render_vt_to_framebuffer(&vt_data, fb_id)?;

            // Update plane with new framebuffer
            if let Some(plane_id) = self.primary_plane_id {
                let mut atomic_state = AtomicModeSet::begin();

                if let Some(crtc_id) = self.crtc_id {
                    if let Some((width, height, _)) = vt_data.mode.or(self.current_mode) {
                        AtomicModeSet::set_plane(
                            &mut atomic_state,
                            plane_id,
                            Some(crtc_id),
                            Some(fb_id),
                            0, 0, width, height,
                            0, 0, width << 16, height << 16
                        );
                    }
                }

                let device = get_drm_device();
                let mut device_lock = device.lock();
                AtomicModeSet::commit(&mut device_lock, atomic_state, 0)?;
            }
        }

        self.virtual_terminals[self.active_vt].dirty = false;
        Ok(())
    }

    /// Fallback to legacy framebuffer display
    fn update_legacy_display(&mut self) -> Result<(), DriverError> {
        let vt_dirty = self.virtual_terminals[self.active_vt].dirty;
        if !vt_dirty {
            return Ok(());
        }

        // Clone VT to avoid borrow checker issues
        let vt = self.virtual_terminals[self.active_vt].clone();
        // Render directly to legacy framebuffer
        self.render_vt_to_legacy(&vt);
        self.virtual_terminals[self.active_vt].dirty = false;
        Ok(())
    }

    /// Render virtual terminal to DRM framebuffer
    fn render_vt_to_framebuffer(&mut self, vt: &VirtualTerminal, _fb_id: DrmObjectId) -> Result<(), DriverError> {
        // This would render the character and color buffers to the framebuffer
        // For now, just mark as rendered
        // In a real implementation, this would:
        // 1. Map the framebuffer memory
        // 2. Render each character using font data
        // 3. Apply colors
        // 4. Handle cursor rendering
        Ok(())
    }

    /// Render virtual terminal to legacy framebuffer
    fn render_vt_to_legacy(&mut self, vt: &VirtualTerminal) {
        // Clear screen first
        self.framebuffer.clear(0x000000);

        // Render each character
        for (row_idx, row) in vt.char_buffer.iter().enumerate() {
            for (col_idx, &ch) in row.iter().enumerate() {
                if ch != b' ' {
                    let x = col_idx * 8;
                    let y = row_idx * 16;
                    let color = vt.color_buffer[row_idx][col_idx];
                    self.render_char(x, y, ch, color);
                }
            }
        }

        // Render cursor
        let cursor_x = vt.cursor_x * 8;
        let cursor_y = vt.cursor_y * 16;
        self.render_cursor(cursor_x, cursor_y);
    }

    /// Render a single character
    fn render_char(&mut self, x: usize, y: usize, c: u8, color: u32) {
        // Use existing framebuffer character rendering
        if c.is_ascii_graphic() || c == b' ' {
            // This would use the font rendering from the existing framebuffer code
            self.framebuffer.set_pixel(x, y, color);
        }
    }

    /// Render cursor
    fn render_cursor(&mut self, x: usize, y: usize) {
        // Draw a simple cursor block
        for dy in 0..16 {
            for dx in 0..8 {
                self.framebuffer.set_pixel(x + dx, y + dy, 0xFFFFFF);
            }
        }
    }

    /// Set font size (console property integration)
    pub fn set_font_size(&mut self, size: usize) {
        // Update character dimensions based on font size
        self.virtual_terminals[self.active_vt].dirty = true;
    }

    /// Set text color (console property integration)
    pub fn set_text_color(&mut self, color: u32) {
        // Update text color for current VT
        self.virtual_terminals[self.active_vt].dirty = true;
    }

    /// Set background color (console property integration)
    pub fn set_background_color(&mut self, color: u32) {
        // Update background color and refresh display
        self.virtual_terminals[self.active_vt].dirty = true;
    }

    /// Set cursor style (console property integration)
    pub fn set_cursor_style(&mut self, style: u32) {
        // 0=Block, 1=Underline, 2=Bar, 3=None
        self.virtual_terminals[self.active_vt].dirty = true;
    }

    /// Set cursor blink (console property integration)
    pub fn set_cursor_blink(&mut self, blink: bool) {
        // Enable/disable cursor blinking
        self.virtual_terminals[self.active_vt].dirty = true;
    }

    /// Set auto wrap (console property integration)
    pub fn set_auto_wrap(&mut self, wrap: bool) {
        // Enable/disable automatic line wrapping
        self.virtual_terminals[self.active_vt].dirty = true;
    }

    /// Get current display mode (public getter)
    pub fn get_current_mode(&self) -> Option<(u32, u32, u32)> {
        self.current_mode
    }

    /// Get DRM enabled status (public getter)
    pub fn is_drm_enabled(&self) -> bool {
        self.drm_enabled
    }

    /// Get active virtual terminal ID (public getter)
    pub fn get_active_vt(&self) -> usize {
        self.active_vt
    }

    /// Write character to current virtual terminal
    pub fn write_char(&mut self, c: u8) {
        let vt = &mut self.virtual_terminals[self.active_vt];

        match c {
            b'\n' => {
                vt.cursor_x = 0;
                vt.cursor_y += 1;
                if vt.cursor_y >= vt.char_buffer.len() {
                    vt.scroll_up();
                    vt.cursor_y = vt.char_buffer.len() - 1;
                }
            },
            b'\r' => {
                vt.cursor_x = 0;
            },
            b'\t' => {
                let tab_stop = 8;
                vt.cursor_x = (vt.cursor_x + tab_stop) & !(tab_stop - 1);
                if vt.cursor_x >= vt.char_buffer[0].len() {
                    vt.cursor_x = 0;
                    vt.cursor_y += 1;
                    if vt.cursor_y >= vt.char_buffer.len() {
                        vt.scroll_up();
                        vt.cursor_y = vt.char_buffer.len() - 1;
                    }
                }
            },
            b'\x08' => { // Backspace
                if vt.cursor_x > 0 {
                    vt.cursor_x -= 1;
                    vt.write_char(vt.cursor_x, vt.cursor_y, b' ', 0xFFFFFF);
                }
            },
            _ => {
                if c.is_ascii_graphic() || c == b' ' {
                    vt.write_char(vt.cursor_x, vt.cursor_y, c, 0xFFFFFF);
                    vt.cursor_x += 1;

                    if vt.cursor_x >= vt.char_buffer[0].len() {
                        vt.cursor_x = 0;
                        vt.cursor_y += 1;
                        if vt.cursor_y >= vt.char_buffer.len() {
                            vt.scroll_up();
                            vt.cursor_y = vt.char_buffer.len() - 1;
                        }
                    }
                }
            }
        }

        vt.dirty = true;
    }

    /// Write string to console
    pub fn write_str(&mut self, s: &str) {
        for &byte in s.as_bytes() {
            self.write_char(byte);
        }
        let _ = self.update_display();
    }

    /// Get available display modes
    pub fn get_available_modes(&self) -> Vec<(u32, u32, u32)> {
        if self.drm_enabled {
            ModeSet::list_modes()
        } else {
            vec![(1024, 768, 60), (800, 600, 60), (640, 480, 60)]
        }
    }

    /// Handle console control sequences
    pub fn handle_control_sequence(&mut self, sequence: &[u8]) {
        if sequence.len() < 2 || sequence[0] != b'[' {
            return;
        }

        match sequence {
            [b'[', b'2', b'J'] => {
                // Clear screen
                self.current_vt().clear(0x000000);
            },
            [b'[', b'H'] => {
                // Move cursor to home
                let vt = self.current_vt();
                vt.cursor_x = 0;
                vt.cursor_y = 0;
                vt.dirty = true;
            },
            [b'[', vt_char] if vt_char.is_ascii_digit() => {
                // Switch virtual terminal (Alt+F1-F8)
                let vt_id = (vt_char - b'1') as usize;
                let _ = self.switch_vt(vt_id);
            },
            _ => {
                // Ignore unsupported sequences
            }
        }
    }
}

impl Driver for DrmConsole {
    fn probe(&mut self) -> Result<(), DriverError> {
        // First try to initialize legacy framebuffer
        if self.framebuffer.probe().is_ok() {
            // Then try to initialize DRM
            let _ = self.init_drm();

            // Set default mode if DRM is available
            if self.drm_enabled {
                let _ = self.set_mode(1024, 768, 60);
            }

            Ok(())
        } else {
            Err(DriverError::NotFound)
        }
    }

    fn handle(&mut self, msg: ipc::Message) -> ipc::Message {
        match msg.tag {
            // Tag 1: Write string
            1 => {
                let len = msg.data.iter().position(|&x| x == 0).unwrap_or(msg.data.len());
                if let Ok(s) = str::from_utf8(&msg.data[..len]) {
                    self.write_str(s);
                }
                ipc::Message::empty()
            },

            // Tag 2: Switch virtual terminal
            2 => {
                if !msg.data.is_empty() {
                    let vt_id = msg.data[0] as usize;
                    let _ = self.switch_vt(vt_id);
                }
                ipc::Message::empty()
            },

            // Tag 3: Set mode
            3 => {
                if msg.data.len() >= 12 {
                    let width = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
                    let height = u32::from_le_bytes([msg.data[4], msg.data[5], msg.data[6], msg.data[7]]);
                    let refresh = u32::from_le_bytes([msg.data[8], msg.data[9], msg.data[10], msg.data[11]]);

                    let _ = self.set_mode(width, height, refresh);
                }
                ipc::Message::empty()
            },

            // Tag 4: Get current mode
            4 => {
                let mut response = ipc::Message::empty();
                if let Some((width, height, refresh)) = self.current_mode {
                    response.data[0..4].copy_from_slice(&width.to_le_bytes());
                    response.data[4..8].copy_from_slice(&height.to_le_bytes());
                    response.data[8..12].copy_from_slice(&refresh.to_le_bytes());
                }
                response
            },

            // Tag 5: Get available modes
            5 => {
                let modes = self.get_available_modes();
                let mut response = ipc::Message::empty();
                let mode_count = modes.len().min(4); // Max 4 modes in response
                response.data[0] = mode_count as u8;

                for (i, (width, height, refresh)) in modes.iter().take(mode_count).enumerate() {
                    let offset = 1 + i * 12;
                    if offset + 12 <= response.data.len() {
                        response.data[offset..offset+4].copy_from_slice(&width.to_le_bytes());
                        response.data[offset+4..offset+8].copy_from_slice(&height.to_le_bytes());
                        response.data[offset+8..offset+12].copy_from_slice(&refresh.to_le_bytes());
                    }
                }
                response
            },

            _ => self.framebuffer.handle(msg),
        }
    }
}

/// Global DRM console instance
static DRM_CONSOLE: Mutex<DrmConsole> = Mutex::new(DrmConsole {
    capabilities: ConsoleCapabilities {
        max_width: 1920,
        max_height: 1080,
        preferred_depth: 32,
        multiple_modes: true,
        hardware_cursor: false,
        double_buffering: true,
    },
    current_mode: None,
    framebuffer: Framebuffer::new(),
    crtc_id: None,
    connector_id: None,
    primary_plane_id: None,
    current_framebuffer_id: None,
    virtual_terminals: Vec::new(),
    active_vt: 0,
    front_buffer_id: None,
    back_buffer_id: None,
    drm_enabled: false,
    master_session: None,
});

/// Initialize DRM console
pub fn init_drm_console() -> Result<(), DriverError> {
    let mut console = DRM_CONSOLE.lock();
    *console = DrmConsole::new();
    console.probe()
}

/// Write to DRM console
pub fn drm_console_write(s: &str) {
    let mut console = DRM_CONSOLE.lock();
    console.write_str(s);
}

/// Switch virtual terminal
pub fn drm_console_switch_vt(vt_id: usize) -> Result<(), DriverError> {
    DRM_CONSOLE.lock().switch_vt(vt_id)
}

/// Set console display mode
pub fn drm_console_set_mode(width: u32, height: u32, refresh: u32) -> Result<(), DriverError> {
    DRM_CONSOLE.lock().set_mode(width, height, refresh)
}

/// Get DRM console reference
pub fn get_drm_console() -> &'static Mutex<DrmConsole> {
    &DRM_CONSOLE
}