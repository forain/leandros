//! Linear framebuffer driver (UEFI GOP / VESA / multiboot2).
//!
//! Boot-time flow:
//!   1. The boot parser (multiboot2 / DTB) calls `set_boot_framebuffer()` with
//!      the parameters it found in the boot information structure.
//!   2. The driver server calls `probe()`.  If boot info was recorded it
//!      initialises `self` from that info; otherwise it returns `NotFound`.

use spin::Mutex;
use super::{Driver, DriverError};
use crate::vector_font::{VectorFont, get_fira_code_char, include_fira_code_ttf};

// ── Boot-time registration ────────────────────────────────────────────────────

struct FramebufferInfo {
    base:   u64,
    width:  u32,
    height: u32,
    pitch:  u32,
}

static BOOT_FB: Mutex<Option<FramebufferInfo>> = Mutex::new(None);

/// Record framebuffer parameters discovered from boot information.
///
/// Must be called before the driver server runs `probe()`.  Safe to call
/// multiple times; only the last call takes effect.
pub fn set_boot_framebuffer(base: u64, width: u32, height: u32, pitch: u32) {
    *BOOT_FB.lock() = Some(FramebufferInfo { base, width, height, pitch });
}

// ── Driver struct ─────────────────────────────────────────────────────────────

pub struct Framebuffer {
    base:   *mut u32,
    width:  usize,
    height: usize,
    pitch:  usize, // bytes per row
    cursor_x: usize,
    cursor_y: usize,
    vector_font: Option<VectorFont>,
    char_width: usize,
    char_height: usize,
}

// Safety: kernel owns the framebuffer exclusively.
unsafe impl Send for Framebuffer {}
unsafe impl Sync for Framebuffer {}

impl Framebuffer {
    const FALLBACK_FONT: [u8; 128 * 8] = include_font();

    /// Construct an uninitialised framebuffer driver.
    ///
    /// `probe()` must be called (and succeed) before any drawing methods.
    pub const fn new() -> Self {
        Self {
            base:   core::ptr::null_mut(),
            width:  0,
            height: 0,
            pitch:  0,
            cursor_x: 0,
            cursor_y: 0,
            vector_font: None,
            char_width: 12,  // Vector font character width
            char_height: 20, // Vector font character height
        }
    }

    /// Initialize vector font
    pub fn init_vector_font(&mut self) {
        // Temporarily disable vector font to debug boot crash
        // Use bitmap font only for now
        self.vector_font = None;
        self.char_width = 8;
        self.char_height = 16;
    }

    pub fn set_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x < self.width && y < self.height {
            unsafe {
                let offset = y * (self.pitch / 4) + x;
                self.base.add(offset).write_volatile(color);
            }
        }
    }

    pub fn clear(&mut self, color: u32) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.set_pixel(x, y, color);
            }
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    pub fn putc(&mut self, c: u8) {
        if self.base.is_null() { return; }

        static mut UTF8_STATE: (usize, u32) = (0, 0);
        static mut ANSI_STATE: (bool, [u8; 16], usize) = (false, [0; 16], 0);

        unsafe {
            // Handle ANSI escape sequences
            if c == 0x1b {  // ESC character starts escape sequence
                ANSI_STATE.0 = true;
                ANSI_STATE.2 = 0;
                return;
            } else if ANSI_STATE.0 {
                // We're in an escape sequence
                if ANSI_STATE.2 < ANSI_STATE.1.len() {
                    ANSI_STATE.1[ANSI_STATE.2] = c;
                    ANSI_STATE.2 += 1;
                }

                // Check for complete escape sequences
                if c.is_ascii_alphabetic() {
                    // End of escape sequence
                    self.handle_ansi_sequence(&ANSI_STATE.1[..ANSI_STATE.2]);
                    ANSI_STATE.0 = false;
                    ANSI_STATE.2 = 0;
                }
                return;
            }
        }

        if c == b'\n' {
            self.cursor_x = 0;
            self.cursor_y += self.char_height;
        } else if c == b'\r' {
            self.cursor_x = 0;
        } else if c == b'\x08' {  // Backspace (ASCII 8)
            self.handle_backspace();
        } else {
            unsafe {
                // Handle UTF-8 decoding for Unicode box-drawing characters
                if c < 0x80 {
                    // ASCII character
                    UTF8_STATE = (0, 0);
                    self.draw_char_vector(self.cursor_x, self.cursor_y, c as char, 0xFFFFFF);
                    self.cursor_x += self.char_width;
                } else if c & 0xE0 == 0xC0 {
                    // Start of 2-byte UTF-8
                    UTF8_STATE = (1, (c & 0x1F) as u32);
                } else if c & 0xF0 == 0xE0 {
                    // Start of 3-byte UTF-8
                    UTF8_STATE = (2, (c & 0x0F) as u32);
                } else if c & 0xF8 == 0xF0 {
                    // Start of 4-byte UTF-8
                    UTF8_STATE = (3, (c & 0x07) as u32);
                } else if c & 0xC0 == 0x80 && UTF8_STATE.0 > 0 {
                    // UTF-8 continuation byte
                    UTF8_STATE.1 = (UTF8_STATE.1 << 6) | (c & 0x3F) as u32;
                    UTF8_STATE.0 -= 1;

                    if UTF8_STATE.0 == 0 {
                        // Complete UTF-8 character
                        let unicode_char = UTF8_STATE.1;
                        let display_char = self.map_unicode_to_ascii(unicode_char);
                        self.draw_char_vector(self.cursor_x, self.cursor_y, display_char, 0xFFFFFF);
                        self.cursor_x += self.char_width;
                        UTF8_STATE = (0, 0);
                    }
                } else {
                    // Invalid UTF-8, reset state
                    UTF8_STATE = (0, 0);
                }

                if self.cursor_x + self.char_width > self.width {
                    self.cursor_x = 0;
                    self.cursor_y += self.char_height;
                }
            }
        }

        if self.cursor_y + self.char_height > self.height {
            self.scroll_vector();
        }
    }

    /// Handle backspace character - move cursor back and clear the character
    fn handle_backspace(&mut self) {
        if self.cursor_x >= self.char_width {
            // Move cursor back one character
            self.cursor_x -= self.char_width;

            // Clear the character at the cursor position by drawing a space (background color)
            for y in 0..self.char_height {
                for x in 0..self.char_width {
                    self.set_pixel(self.cursor_x + x, self.cursor_y + y, 0x000000);
                }
            }
        } else if self.cursor_y >= self.char_height {
            // At beginning of line, move to end of previous line
            self.cursor_y -= self.char_height;
            // Find the rightmost position on the previous line by scanning backwards
            // For simplicity, just move to the end of the line
            self.cursor_x = (self.width / self.char_width - 1) * self.char_width;

            // Clear the character at the cursor position
            for y in 0..self.char_height {
                for x in 0..self.char_width {
                    self.set_pixel(self.cursor_x + x, self.cursor_y + y, 0x000000);
                }
            }
        }
        // If we're at position (0,0), do nothing
    }

    /// Handle ANSI escape sequences for terminal control
    fn handle_ansi_sequence(&mut self, sequence: &[u8]) {
        if sequence.len() >= 2 && sequence[0] == b'[' {
            match sequence {
                [b'[', b'2', b'J'] => {
                    // Clear entire screen
                    self.clear_screen();
                }
                [b'[', b'H'] => {
                    // Move cursor to home position (0,0)
                    self.cursor_x = 0;
                    self.cursor_y = 0;
                }
                [b'[', b'K'] => {
                    // Clear from cursor to end of line
                    self.clear_line_from_cursor();
                }
                _ => {
                    // Ignore unsupported escape sequences
                }
            }
        }
    }

    /// Clear the entire screen with background color
    fn clear_screen(&mut self) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.set_pixel(x, y, 0x000000);
            }
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    /// Clear from cursor position to end of current line
    fn clear_line_from_cursor(&mut self) {
        for x in self.cursor_x..self.width {
            for y in self.cursor_y..(self.cursor_y + self.char_height).min(self.height) {
                self.set_pixel(x, y, 0x000000);
            }
        }
    }

    /// Map Unicode box-drawing characters to ASCII equivalents
    fn map_unicode_to_ascii(&self, unicode: u32) -> char {
        match unicode {
            // Map to graphics characters using low ASCII range (1-31)
            0x2550 => 1 as char,  // ═ -> ASCII 1 (custom horizontal line)
            0x2551 => 2 as char,  // ║ -> ASCII 2 (custom vertical line)
            0x2554 => 3 as char,  // ╔ -> ASCII 3 (custom top-left corner)
            0x2557 => 4 as char,  // ╗ -> ASCII 4 (custom top-right corner)
            0x255A => 5 as char,  // ╚ -> ASCII 5 (custom bottom-left corner)
            0x255D => 6 as char,  // ╝ -> ASCII 6 (custom bottom-right corner)
            0x2569 => 7 as char,  // ╩ -> ASCII 7 (custom T junction up)
            0x2566 => 8 as char,  // ╦ -> ASCII 8 (custom T junction down)
            0x2560 => 9 as char,  // ╠ -> ASCII 9 (custom T junction right)
            0x2563 => 10 as char, // ╣ -> ASCII 10 (custom T junction left)
            0x2588 => 11 as char, // █ -> ASCII 11 (custom full block)
            _ => '?',              // Unknown Unicode -> question mark
        }
    }

    fn draw_char(&mut self, x: usize, y: usize, c: u8, color: u32) {
        if (c as usize) * 8 + 8 > Self::FALLBACK_FONT.len() {
            return;
        }
        let glyph = &Self::FALLBACK_FONT[(c as usize) * 8 .. (c as usize) * 8 + 8];
        for (gy, &row) in glyph.iter().enumerate() {
            for gx in 0..8 {
                if (row & (1 << (7 - gx))) != 0 {
                    self.set_pixel(x + gx, y + gy, color);
                } else {
                    self.set_pixel(x + gx, y + gy, 0x000000);
                }
            }
        }
    }

    /// Draw character using Fira Code bitmap font
    fn draw_char_vector(&mut self, x: usize, y: usize, c: char, color: u32) {
        // Simplified to use bitmap font only during debugging
        if let Some(bitmap) = get_fira_code_char(c) {
            // Render Fira Code bitmap (16 rows)
            for (gy, &row) in bitmap.iter().enumerate() {
                for gx in 0..8 {
                    if (row & (1 << (7 - gx))) != 0 {
                        self.set_pixel(x + gx, y + gy, color);
                    }
                }
            }
        } else {
            // Fallback to original bitmap font
            self.draw_char(x, y, c as u8, color);
        }
    }

    fn scroll(&mut self) {
        let rows_to_copy = self.height - 8; // fallback char height
        unsafe {
            core::ptr::copy(
                self.base.add(8 * (self.pitch / 4)),
                self.base,
                rows_to_copy * (self.pitch / 4)
            );
            // Clear bottom line
            let bottom_start = rows_to_copy * (self.pitch / 4);
            core::ptr::write_bytes(self.base.add(bottom_start), 0, 8 * (self.pitch / 4));
        }
        self.cursor_y -= 8;
    }

    /// Scroll screen for vector font
    fn scroll_vector(&mut self) {
        let rows_to_copy = self.height - self.char_height;
        unsafe {
            core::ptr::copy(
                self.base.add(self.char_height * (self.pitch / 4)),
                self.base,
                rows_to_copy * (self.pitch / 4)
            );
            // Clear bottom lines
            let bottom_start = rows_to_copy * (self.pitch / 4);
            core::ptr::write_bytes(self.base.add(bottom_start), 0, self.char_height * (self.pitch / 4));
        }
        self.cursor_y -= self.char_height;
    }
}

impl Driver for Framebuffer {
    /// Initialise from boot-provided parameters.
    ///
    /// Returns `Err(DriverError::NotFound)` if the bootloader did not supply a
    /// linear framebuffer (e.g. text-mode boot, or the DTB has no /framebuffer
    /// node).
    fn probe(&mut self) -> Result<(), DriverError> {
        let info = BOOT_FB.lock().take().ok_or(DriverError::NotFound)?;

        if info.base == 0 || info.width == 0 || info.height == 0 || info.pitch == 0 {
            return Err(DriverError::NotFound);
        }

        self.base   = info.base as *mut u32;
        self.width  = info.width  as usize;
        self.height = info.height as usize;
        self.pitch  = info.pitch  as usize;
        Ok(())
    }

    fn handle(&mut self, msg: ipc::Message) -> ipc::Message {
        // Tag 1 = clear with colour in data[0..4].
        if msg.tag == 1 {
            let color = u32::from_le_bytes(msg.data[0..4].try_into().unwrap_or([0; 4]));
            self.clear(color);
        }
        ipc::Message::empty()
    }
}

// ── Kernel Integration ────────────────────────────────────────────────────────

static KERNEL_FB: Mutex<Framebuffer> = Mutex::new(Framebuffer::new());

/// Output a character to the global kernel framebuffer.
pub fn fb_putc(c: u8) {
    KERNEL_FB.lock().putc(c);
}

/// Initialize the kernel-space framebuffer console.
pub unsafe fn init_kernel_fb(base: *mut u32, width: usize, height: usize, pitch: usize) {
    let mut fb = KERNEL_FB.lock();
    fb.base = base;
    fb.width = width;
    fb.height = height;
    fb.pitch = pitch;
    fb.init_vector_font(); // Initialize vector font
    fb.clear(0);
}

/// Initialize the kernel-space framebuffer console without clearing screen.
pub unsafe fn update_kernel_fb(base: *mut u32, width: usize, height: usize, pitch: usize) {
    let mut fb = KERNEL_FB.lock();
    fb.base = base;
    fb.width = width;
    fb.height = height;
    fb.pitch = pitch;
    fb.init_vector_font(); // Initialize vector font
    // Don't clear - preserve existing content
}

// ── Bitmap Font ───────────────────────────────────────────────────────────────

const fn include_font() -> [u8; 128 * 8] {
    let mut font = [0u8; 128 * 8];
    
    // Numbers 0-9
    font[b'0' as usize * 8 + 1] = 0x3c; font[b'0' as usize * 8 + 2] = 0x66; font[b'0' as usize * 8 + 3] = 0x6e; font[b'0' as usize * 8 + 4] = 0x76; font[b'0' as usize * 8 + 5] = 0x66; font[b'0' as usize * 8 + 6] = 0x3c;
    font[b'1' as usize * 8 + 1] = 0x18; font[b'1' as usize * 8 + 2] = 0x38; font[b'1' as usize * 8 + 3] = 0x18; font[b'1' as usize * 8 + 4] = 0x18; font[b'1' as usize * 8 + 5] = 0x18; font[b'1' as usize * 8 + 6] = 0x3c;
    font[b'2' as usize * 8 + 1] = 0x3c; font[b'2' as usize * 8 + 2] = 0x66; font[b'2' as usize * 8 + 3] = 0x06; font[b'2' as usize * 8 + 4] = 0x0c; font[b'2' as usize * 8 + 5] = 0x30; font[b'2' as usize * 8 + 6] = 0x7e;
    font[b'3' as usize * 8 + 1] = 0x3c; font[b'3' as usize * 8 + 2] = 0x66; font[b'3' as usize * 8 + 3] = 0x1c; font[b'3' as usize * 8 + 4] = 0x06; font[b'3' as usize * 8 + 5] = 0x66; font[b'3' as usize * 8 + 6] = 0x3c;
    font[b'4' as usize * 8 + 1] = 0x0c; font[b'4' as usize * 8 + 2] = 0x1c; font[b'4' as usize * 8 + 3] = 0x3c; font[b'4' as usize * 8 + 4] = 0x6c; font[b'4' as usize * 8 + 5] = 0x7e; font[b'4' as usize * 8 + 6] = 0x0c;
    font[b'5' as usize * 8 + 1] = 0x7e; font[b'5' as usize * 8 + 2] = 0x60; font[b'5' as usize * 8 + 3] = 0x7c; font[b'5' as usize * 8 + 4] = 0x06; font[b'5' as usize * 8 + 5] = 0x66; font[b'5' as usize * 8 + 6] = 0x3c;
    font[b'6' as usize * 8 + 1] = 0x3c; font[b'6' as usize * 8 + 2] = 0x60; font[b'6' as usize * 8 + 3] = 0x7c; font[b'6' as usize * 8 + 4] = 0x66; font[b'6' as usize * 8 + 5] = 0x66; font[b'6' as usize * 8 + 6] = 0x3c;
    font[b'7' as usize * 8 + 1] = 0x7e; font[b'7' as usize * 8 + 2] = 0x06; font[b'7' as usize * 8 + 3] = 0x0c; font[b'7' as usize * 8 + 4] = 0x18; font[b'7' as usize * 8 + 5] = 0x30; font[b'7' as usize * 8 + 6] = 0x30;
    font[b'8' as usize * 8 + 1] = 0x3c; font[b'8' as usize * 8 + 2] = 0x66; font[b'8' as usize * 8 + 3] = 0x3c; font[b'8' as usize * 8 + 4] = 0x66; font[b'8' as usize * 8 + 5] = 0x66; font[b'8' as usize * 8 + 6] = 0x3c;
    font[b'9' as usize * 8 + 1] = 0x3c; font[b'9' as usize * 8 + 2] = 0x66; font[b'9' as usize * 8 + 3] = 0x3e; font[b'9' as usize * 8 + 4] = 0x06; font[b'9' as usize * 8 + 5] = 0x0c; font[b'9' as usize * 8 + 6] = 0x38;

    // Letters (Uppercase A-Z)
    font[b'A' as usize * 8 + 1] = 0x18; font[b'A' as usize * 8 + 2] = 0x3c; font[b'A' as usize * 8 + 3] = 0x66; font[b'A' as usize * 8 + 4] = 0x7e; font[b'A' as usize * 8 + 5] = 0x66; font[b'A' as usize * 8 + 6] = 0x66;
    font[b'B' as usize * 8 + 1] = 0x7c; font[b'B' as usize * 8 + 2] = 0x66; font[b'B' as usize * 8 + 3] = 0x7c; font[b'B' as usize * 8 + 4] = 0x66; font[b'B' as usize * 8 + 5] = 0x66; font[b'B' as usize * 8 + 6] = 0x7c;
    font[b'C' as usize * 8 + 1] = 0x3c; font[b'C' as usize * 8 + 2] = 0x66; font[b'C' as usize * 8 + 3] = 0x60; font[b'C' as usize * 8 + 4] = 0x60; font[b'C' as usize * 8 + 5] = 0x66; font[b'C' as usize * 8 + 6] = 0x3c;
    font[b'D' as usize * 8 + 1] = 0x78; font[b'D' as usize * 8 + 2] = 0x6c; font[b'D' as usize * 8 + 3] = 0x66; font[b'D' as usize * 8 + 4] = 0x66; font[b'D' as usize * 8 + 5] = 0x6c; font[b'D' as usize * 8 + 6] = 0x78;
    font[b'E' as usize * 8 + 1] = 0x7e; font[b'E' as usize * 8 + 2] = 0x60; font[b'E' as usize * 8 + 3] = 0x7c; font[b'E' as usize * 8 + 4] = 0x60; font[b'E' as usize * 8 + 5] = 0x60; font[b'E' as usize * 8 + 6] = 0x7e;
    font[b'F' as usize * 8 + 1] = 0x7e; font[b'F' as usize * 8 + 2] = 0x60; font[b'F' as usize * 8 + 3] = 0x7c; font[b'F' as usize * 8 + 4] = 0x60; font[b'F' as usize * 8 + 5] = 0x60; font[b'F' as usize * 8 + 6] = 0x60;
    font[b'G' as usize * 8 + 1] = 0x3c; font[b'G' as usize * 8 + 2] = 0x66; font[b'G' as usize * 8 + 3] = 0x60; font[b'G' as usize * 8 + 4] = 0x6e; font[b'G' as usize * 8 + 5] = 0x66; font[b'G' as usize * 8 + 6] = 0x3c;
    font[b'H' as usize * 8 + 1] = 0x66; font[b'H' as usize * 8 + 2] = 0x66; font[b'H' as usize * 8 + 3] = 0x7e; font[b'H' as usize * 8 + 4] = 0x66; font[b'H' as usize * 8 + 5] = 0x66; font[b'H' as usize * 8 + 6] = 0x66;
    font[b'I' as usize * 8 + 1] = 0x3c; font[b'I' as usize * 8 + 2] = 0x18; font[b'I' as usize * 8 + 3] = 0x18; font[b'I' as usize * 8 + 4] = 0x18; font[b'I' as usize * 8 + 5] = 0x18; font[b'I' as usize * 8 + 6] = 0x3c;
    font[b'J' as usize * 8 + 1] = 0x1e; font[b'J' as usize * 8 + 2] = 0x0c; font[b'J' as usize * 8 + 3] = 0x0c; font[b'J' as usize * 8 + 4] = 0x0c; font[b'J' as usize * 8 + 5] = 0xcc; font[b'J' as usize * 8 + 6] = 0x78;
    font[b'K' as usize * 8 + 1] = 0x66; font[b'K' as usize * 8 + 2] = 0x6c; font[b'K' as usize * 8 + 3] = 0x78; font[b'K' as usize * 8 + 4] = 0x7c; font[b'K' as usize * 8 + 5] = 0x6e; font[b'K' as usize * 8 + 6] = 0x67;
    font[b'L' as usize * 8 + 1] = 0x60; font[b'L' as usize * 8 + 2] = 0x60; font[b'L' as usize * 8 + 3] = 0x60; font[b'L' as usize * 8 + 4] = 0x60; font[b'L' as usize * 8 + 5] = 0x60; font[b'L' as usize * 8 + 6] = 0x7e;
    font[b'M' as usize * 8 + 1] = 0x63; font[b'M' as usize * 8 + 2] = 0x77; font[b'M' as usize * 8 + 3] = 0x7f; font[b'M' as usize * 8 + 4] = 0x6b; font[b'M' as usize * 8 + 5] = 0x63; font[b'M' as usize * 8 + 6] = 0x63;
    font[b'N' as usize * 8 + 1] = 0x66; font[b'N' as usize * 8 + 2] = 0x76; font[b'N' as usize * 8 + 3] = 0x7e; font[b'N' as usize * 8 + 4] = 0x7e; font[b'N' as usize * 8 + 5] = 0x6e; font[b'N' as usize * 8 + 6] = 0x66;
    font[b'O' as usize * 8 + 1] = 0x3c; font[b'O' as usize * 8 + 2] = 0x66; font[b'O' as usize * 8 + 3] = 0x66; font[b'O' as usize * 8 + 4] = 0x66; font[b'O' as usize * 8 + 5] = 0x66; font[b'O' as usize * 8 + 6] = 0x3c;
    font[b'P' as usize * 8 + 1] = 0x7c; font[b'P' as usize * 8 + 2] = 0x66; font[b'P' as usize * 8 + 3] = 0x7c; font[b'P' as usize * 8 + 4] = 0x60; font[b'P' as usize * 8 + 5] = 0x60; font[b'P' as usize * 8 + 6] = 0x60;
    font[b'Q' as usize * 8 + 1] = 0x3c; font[b'Q' as usize * 8 + 2] = 0x66; font[b'Q' as usize * 8 + 3] = 0x66; font[b'Q' as usize * 8 + 4] = 0x66; font[b'Q' as usize * 8 + 5] = 0x3c; font[b'Q' as usize * 8 + 6] = 0x0e;
    font[b'R' as usize * 8 + 1] = 0x7c; font[b'R' as usize * 8 + 2] = 0x66; font[b'R' as usize * 8 + 3] = 0x7c; font[b'R' as usize * 8 + 4] = 0x6c; font[b'R' as usize * 8 + 5] = 0x66; font[b'R' as usize * 8 + 6] = 0x66;
    font[b'S' as usize * 8 + 1] = 0x3c; font[b'S' as usize * 8 + 2] = 0x60; font[b'S' as usize * 8 + 3] = 0x3c; font[b'S' as usize * 8 + 4] = 0x06; font[b'S' as usize * 8 + 5] = 0x66; font[b'S' as usize * 8 + 6] = 0x3c;
    font[b'T' as usize * 8 + 1] = 0x7e; font[b'T' as usize * 8 + 2] = 0x18; font[b'T' as usize * 8 + 3] = 0x18; font[b'T' as usize * 8 + 4] = 0x18; font[b'T' as usize * 8 + 5] = 0x18; font[b'T' as usize * 8 + 6] = 0x18;
    font[b'U' as usize * 8 + 1] = 0x66; font[b'U' as usize * 8 + 2] = 0x66; font[b'U' as usize * 8 + 3] = 0x66; font[b'U' as usize * 8 + 4] = 0x66; font[b'U' as usize * 8 + 5] = 0x66; font[b'U' as usize * 8 + 6] = 0x3c;
    font[b'V' as usize * 8 + 1] = 0x66; font[b'V' as usize * 8 + 2] = 0x66; font[b'V' as usize * 8 + 3] = 0x66; font[b'V' as usize * 8 + 4] = 0x66; font[b'V' as usize * 8 + 5] = 0x3c; font[b'V' as usize * 8 + 6] = 0x18;
    font[b'W' as usize * 8 + 1] = 0x63; font[b'W' as usize * 8 + 2] = 0x63; font[b'W' as usize * 8 + 3] = 0x6b; font[b'W' as usize * 8 + 4] = 0x7f; font[b'W' as usize * 8 + 5] = 0x77; font[b'W' as usize * 8 + 6] = 0x63;
    font[b'X' as usize * 8 + 1] = 0x66; font[b'X' as usize * 8 + 2] = 0x66; font[b'X' as usize * 8 + 3] = 0x3c; font[b'X' as usize * 8 + 4] = 0x3c; font[b'X' as usize * 8 + 5] = 0x66; font[b'X' as usize * 8 + 6] = 0x66;
    font[b'Y' as usize * 8 + 1] = 0x66; font[b'Y' as usize * 8 + 2] = 0x66; font[b'Y' as usize * 8 + 3] = 0x3c; font[b'Y' as usize * 8 + 4] = 0x18; font[b'Y' as usize * 8 + 5] = 0x18; font[b'Y' as usize * 8 + 6] = 0x18;
    font[b'Z' as usize * 8 + 1] = 0x7e; font[b'Z' as usize * 8 + 2] = 0x06; font[b'Z' as usize * 8 + 3] = 0x0c; font[b'Z' as usize * 8 + 4] = 0x18; font[b'Z' as usize * 8 + 5] = 0x30; font[b'Z' as usize * 8 + 6] = 0x7e;

    // Lowercase letters (a-z)
    font[b'a' as usize * 8 + 3] = 0x3c; font[b'a' as usize * 8 + 4] = 0x06; font[b'a' as usize * 8 + 5] = 0x3e; font[b'a' as usize * 8 + 6] = 0x66; font[b'a' as usize * 8 + 7] = 0x3b;
    font[b'b' as usize * 8 + 1] = 0x60; font[b'b' as usize * 8 + 2] = 0x60; font[b'b' as usize * 8 + 3] = 0x7c; font[b'b' as usize * 8 + 4] = 0x66; font[b'b' as usize * 8 + 5] = 0x66; font[b'b' as usize * 8 + 6] = 0x7c;
    font[b'c' as usize * 8 + 3] = 0x3c; font[b'c' as usize * 8 + 4] = 0x66; font[b'c' as usize * 8 + 5] = 0x60; font[b'c' as usize * 8 + 6] = 0x66; font[b'c' as usize * 8 + 7] = 0x3c;
    font[b'd' as usize * 8 + 1] = 0x06; font[b'd' as usize * 8 + 2] = 0x06; font[b'd' as usize * 8 + 3] = 0x3e; font[b'd' as usize * 8 + 4] = 0x66; font[b'd' as usize * 8 + 5] = 0x66; font[b'd' as usize * 8 + 6] = 0x3e;
    font[b'e' as usize * 8 + 3] = 0x3c; font[b'e' as usize * 8 + 4] = 0x66; font[b'e' as usize * 8 + 5] = 0x7e; font[b'e' as usize * 8 + 6] = 0x60; font[b'e' as usize * 8 + 7] = 0x3c;
    font[b'f' as usize * 8 + 1] = 0x1c; font[b'f' as usize * 8 + 2] = 0x30; font[b'f' as usize * 8 + 3] = 0x7c; font[b'f' as usize * 8 + 4] = 0x30; font[b'f' as usize * 8 + 5] = 0x30; font[b'f' as usize * 8 + 6] = 0x30;
    font[b'g' as usize * 8 + 3] = 0x3e; font[b'g' as usize * 8 + 4] = 0x66; font[b'g' as usize * 8 + 5] = 0x66; font[b'g' as usize * 8 + 6] = 0x3e; font[b'g' as usize * 8 + 7] = 0x06; font[b'g' as usize * 8 + 8] = 0x3c;
    font[b'h' as usize * 8 + 1] = 0x60; font[b'h' as usize * 8 + 2] = 0x60; font[b'h' as usize * 8 + 3] = 0x7c; font[b'h' as usize * 8 + 4] = 0x66; font[b'h' as usize * 8 + 5] = 0x66; font[b'h' as usize * 8 + 6] = 0x66;
    font[b'i' as usize * 8 + 1] = 0x18; font[b'i' as usize * 8 + 3] = 0x38; font[b'i' as usize * 8 + 4] = 0x18; font[b'i' as usize * 8 + 5] = 0x18; font[b'i' as usize * 8 + 6] = 0x3c;
    font[b'j' as usize * 8 + 1] = 0x0c; font[b'j' as usize * 8 + 3] = 0x1c; font[b'j' as usize * 8 + 4] = 0x0c; font[b'j' as usize * 8 + 5] = 0x0c; font[b'j' as usize * 8 + 6] = 0x0c; font[b'j' as usize * 8 + 7] = 0x4c; font[b'j' as usize * 8 + 8] = 0x38;
    font[b'k' as usize * 8 + 1] = 0x60; font[b'k' as usize * 8 + 2] = 0x60; font[b'k' as usize * 8 + 3] = 0x66; font[b'k' as usize * 8 + 4] = 0x6c; font[b'k' as usize * 8 + 5] = 0x78; font[b'k' as usize * 8 + 6] = 0x66;
    font[b'l' as usize * 8 + 1] = 0x30; font[b'l' as usize * 8 + 2] = 0x30; font[b'l' as usize * 8 + 3] = 0x30; font[b'l' as usize * 8 + 4] = 0x30; font[b'l' as usize * 8 + 5] = 0x30; font[b'l' as usize * 8 + 6] = 0x1c;
    font[b'm' as usize * 8 + 3] = 0x6c; font[b'm' as usize * 8 + 4] = 0xfe; font[b'm' as usize * 8 + 5] = 0xfe; font[b'm' as usize * 8 + 6] = 0xd6; font[b'm' as usize * 8 + 7] = 0xc6;
    font[b'n' as usize * 8 + 3] = 0x7c; font[b'n' as usize * 8 + 4] = 0x66; font[b'n' as usize * 8 + 5] = 0x66; font[b'n' as usize * 8 + 6] = 0x66; font[b'n' as usize * 8 + 7] = 0x66;
    font[b'o' as usize * 8 + 3] = 0x3c; font[b'o' as usize * 8 + 4] = 0x66; font[b'o' as usize * 8 + 5] = 0x66; font[b'o' as usize * 8 + 6] = 0x66; font[b'o' as usize * 8 + 7] = 0x3c;
    font[b'p' as usize * 8 + 3] = 0x7c; font[b'p' as usize * 8 + 4] = 0x66; font[b'p' as usize * 8 + 5] = 0x7c; font[b'p' as usize * 8 + 6] = 0x60; font[b'p' as usize * 8 + 7] = 0x60;
    font[b'q' as usize * 8 + 3] = 0x3e; font[b'q' as usize * 8 + 4] = 0x66; font[b'q' as usize * 8 + 5] = 0x3e; font[b'q' as usize * 8 + 6] = 0x06; font[b'q' as usize * 8 + 7] = 0x06;
    font[b'r' as usize * 8 + 3] = 0x7c; font[b'r' as usize * 8 + 4] = 0x66; font[b'r' as usize * 8 + 5] = 0x60; font[b'r' as usize * 8 + 6] = 0x60; font[b'r' as usize * 8 + 7] = 0x60;
    font[b's' as usize * 8 + 3] = 0x3e; font[b's' as usize * 8 + 4] = 0x60; font[b's' as usize * 8 + 5] = 0x3c; font[b's' as usize * 8 + 6] = 0x06; font[b's' as usize * 8 + 7] = 0x7c;
    font[b't' as usize * 8 + 1] = 0x18; font[b't' as usize * 8 + 2] = 0x18; font[b't' as usize * 8 + 3] = 0x7e; font[b't' as usize * 8 + 4] = 0x18; font[b't' as usize * 8 + 5] = 0x18; font[b't' as usize * 8 + 6] = 0x1c;
    font[b'u' as usize * 8 + 3] = 0x66; font[b'u' as usize * 8 + 4] = 0x66; font[b'u' as usize * 8 + 5] = 0x66; font[b'u' as usize * 8 + 6] = 0x66; font[b'u' as usize * 8 + 7] = 0x3e;
    font[b'v' as usize * 8 + 3] = 0x66; font[b'v' as usize * 8 + 4] = 0x66; font[b'v' as usize * 8 + 5] = 0x66; font[b'v' as usize * 8 + 6] = 0x3c; font[b'v' as usize * 8 + 7] = 0x18;
    font[b'w' as usize * 8 + 3] = 0x63; font[b'w' as usize * 8 + 4] = 0x6b; font[b'w' as usize * 8 + 5] = 0x7f; font[b'w' as usize * 8 + 6] = 0x3e; font[b'w' as usize * 8 + 7] = 0x36;
    font[b'x' as usize * 8 + 3] = 0x66; font[b'x' as usize * 8 + 4] = 0x3c; font[b'x' as usize * 8 + 5] = 0x18; font[b'x' as usize * 8 + 6] = 0x3c; font[b'x' as usize * 8 + 7] = 0x66;
    font[b'y' as usize * 8 + 3] = 0x66; font[b'y' as usize * 8 + 4] = 0x66; font[b'y' as usize * 8 + 5] = 0x3e; font[b'y' as usize * 8 + 6] = 0x06; font[b'y' as usize * 8 + 7] = 0x3c;
    font[b'z' as usize * 8 + 3] = 0x7e; font[b'z' as usize * 8 + 4] = 0x0c; font[b'z' as usize * 8 + 5] = 0x18; font[b'z' as usize * 8 + 6] = 0x30; font[b'z' as usize * 8 + 7] = 0x7e;

    // Symbols
    font[b'[' as usize * 8 + 1] = 0x3c; font[b'[' as usize * 8 + 2] = 0x30; font[b'[' as usize * 8 + 3] = 0x30; font[b'[' as usize * 8 + 4] = 0x30; font[b'[' as usize * 8 + 5] = 0x30; font[b'[' as usize * 8 + 6] = 0x3c;
    font[b']' as usize * 8 + 1] = 0x3c; font[b']' as usize * 8 + 2] = 0x0c; font[b']' as usize * 8 + 3] = 0x0c; font[b']' as usize * 8 + 4] = 0x0c; font[b']' as usize * 8 + 5] = 0x0c; font[b']' as usize * 8 + 6] = 0x3c;
    font[b'(' as usize * 8 + 1] = 0x0c; font[b'(' as usize * 8 + 2] = 0x18; font[b'(' as usize * 8 + 3] = 0x18; font[b'(' as usize * 8 + 4] = 0x18; font[b'(' as usize * 8 + 5] = 0x18; font[b'(' as usize * 8 + 6] = 0x0c;
    font[b')' as usize * 8 + 1] = 0x30; font[b')' as usize * 8 + 2] = 0x18; font[b')' as usize * 8 + 3] = 0x18; font[b')' as usize * 8 + 4] = 0x18; font[b')' as usize * 8 + 5] = 0x18; font[b')' as usize * 8 + 6] = 0x30;
    font[b'>' as usize * 8 + 2] = 0x60; font[b'>' as usize * 8 + 3] = 0x30; font[b'>' as usize * 8 + 4] = 0x18; font[b'>' as usize * 8 + 5] = 0x30; font[b'>' as usize * 8 + 6] = 0x60;
    font[b'-' as usize * 8 + 4] = 0x7e;
    font[b'/' as usize * 8 + 1] = 0x06; font[b'/' as usize * 8 + 2] = 0x0c; font[b'/' as usize * 8 + 3] = 0x18; font[b'/' as usize * 8 + 4] = 0x30; font[b'/' as usize * 8 + 5] = 0x60; font[b'/' as usize * 8 + 6] = 0xc0;
    font[b'_' as usize * 8 + 7] = 0xff;
    font[b'*' as usize * 8 + 2] = 0x66; font[b'*' as usize * 8 + 3] = 0x3c; font[b'*' as usize * 8 + 4] = 0xff; font[b'*' as usize * 8 + 5] = 0x3c; font[b'*' as usize * 8 + 6] = 0x66;
    font[b'+' as usize * 8 + 2] = 0x18; font[b'+' as usize * 8 + 3] = 0x18; font[b'+' as usize * 8 + 4] = 0x7e; font[b'+' as usize * 8 + 5] = 0x18; font[b'+' as usize * 8 + 6] = 0x18;
    font[b'=' as usize * 8 + 3] = 0x7e; font[b'=' as usize * 8 + 5] = 0x7e;
    font[b'?' as usize * 8 + 1] = 0x3c; font[b'?' as usize * 8 + 2] = 0x66; font[b'?' as usize * 8 + 3] = 0x06; font[b'?' as usize * 8 + 4] = 0x0c; font[b'?' as usize * 8 + 6] = 0x18;
    font[b'#' as usize * 8 + 2] = 0x66; font[b'#' as usize * 8 + 3] = 0x7e; font[b'#' as usize * 8 + 4] = 0x66; font[b'#' as usize * 8 + 5] = 0x7e; font[b'#' as usize * 8 + 6] = 0x66;
    font[b':' as usize * 8 + 2] = 0x18; font[b':' as usize * 8 + 5] = 0x18;
    font[b'.' as usize * 8 + 6] = 0x18;
    font[b',' as usize * 8 + 6] = 0x18; font[b',' as usize * 8 + 7] = 0x10;
    font[b'!' as usize * 8 + 1] = 0x18; font[b'!' as usize * 8 + 2] = 0x18; font[b'!' as usize * 8 + 3] = 0x18; font[b'!' as usize * 8 + 4] = 0x18; font[b'!' as usize * 8 + 6] = 0x18;
    font[b' ' as usize * 8 + 0] = 0x00;

    font
}
