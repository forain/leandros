//! Linear framebuffer driver (UEFI GOP / VESA / multiboot2).
//!
//! Boot-time flow:
//!   1. The boot parser (multiboot2 / DTB) calls `set_boot_framebuffer()` with
//!      the parameters it found in the boot information structure.
//!   2. The driver server calls `probe()`.  If boot info was recorded it
//!      initialises `self` from that info; otherwise it returns `NotFound`.

use spin::Mutex;
use super::{Driver, DriverError};
use crate::vector_font::{VectorFont, get_fira_code_char};

// ── Boot-time registration ────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct FramebufferInfo {
    base:   u64,
    width:  u32,
    height: u32,
    pitch:  u32,
}

pub static BOOT_FB: Mutex<Option<FramebufferInfo>> = Mutex::new(None);

/// Record framebuffer parameters discovered from boot information.
///
/// Must be called before the driver server runs `probe()`.  Safe to call
/// multiple times; only the last call takes effect.
pub fn set_boot_framebuffer(base: u64, width: u32, height: u32, pitch: u32) {
    *BOOT_FB.lock() = Some(FramebufferInfo { base, width, height, pitch });
}

/// Get hardware framebuffer information for DRM integration.
pub fn get_hardware_fb_info() -> Option<(u64, u32, u32, u32)> {
    crate::pci::rdebug("[FB] Locking BOOT_FB...\n");
    let lock = BOOT_FB.lock();
    crate::pci::rdebug("[FB] BOOT_FB locked\n");
    lock.as_ref().map(|fb| (fb.base, fb.width, fb.height, fb.pitch))
}

/// Resolve an xterm-256 colour index: 0-15 palette, 16-231 6×6×6 cube,
/// 232-255 greyscale ramp.
fn xterm256(n: usize, base: &[u32; 8], bright: &[u32; 8]) -> u32 {
    match n {
        0..=7   => base[n],
        8..=15  => bright[n - 8],
        16..=231 => {
            const LEVELS: [u32; 6] = [0, 95, 135, 175, 215, 255];
            let i = n - 16;
            (LEVELS[i / 36] << 16) | (LEVELS[(i / 6) % 6] << 8) | LEVELS[i % 6]
        }
        232..=255 => {
            let v = 8 + (n as u32 - 232) * 10;
            (v << 16) | (v << 8) | v
        }
        _ => 0xFFFFFF,
    }
}

// ── Driver struct ─────────────────────────────────────────────────────────────

/// Where the escape-sequence parser is in a multi-byte sequence.
///
/// The console is the *only* VT emulator on the framebuffer path (serial hands
/// raw bytes to the host terminal, which does this itself), so anything a line
/// editor emits has to be understood here or it is lost.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EscState {
    /// Not in a sequence — bytes are printable/control.
    Ground,
    /// Saw ESC; the next byte selects the sequence type.
    Esc,
    /// Saw `ESC [` — collecting parameter/intermediate bytes (0x20..=0x3F)
    /// until a final byte (0x40..=0x7E).
    Csi,
    /// Saw `ESC ]` — swallow the string until BEL or ST (`ESC \`).
    Osc,
    /// Saw ESC inside an OSC string — `\` ends it, anything else resumes.
    OscEsc,
    /// Saw a two-byte escape like `ESC ( B` — swallow exactly one more byte.
    Discard1,
}

pub struct Framebuffer {
    base:   *mut u32,
    /// Write-back copy of the surface that all drawing goes to, blitted to
    /// `base` a dirty rectangle at a time by [`fb_flush`]. Null before
    /// [`Framebuffer::alloc_shadow`] succeeds (or if it cannot), in which case
    /// drawing falls back to `base` directly.
    ///
    /// The framebuffer is uncached — necessarily, since the scanout reads it
    /// without participating in cache coherency. Uncached *stores* can at least
    /// be made to burst (that is what the Write Combining / Normal-NC mapping in
    /// each `arch::init` buys). Uncached *loads* cannot: no line fill, no
    /// prefetch, one transaction per access. And the console's dominant
    /// operation reads the surface — `scroll_px` copies it onto itself.
    ///
    /// Measured on x86_64/KVM at 1920x1080x4 (8.29 MB): `ls -l /bin` took ~40 s
    /// with the copy running on the framebuffer, and scaling `SCROLL_ROWS` from
    /// 8 to 32 cut it to ~6.9 s — i.e. cost tracked scroll count almost exactly,
    /// so the scroll was essentially the whole bill. Making the mapping WC moved
    /// it only 40 s → 38.7 s, because that only accelerated the copy's write
    /// half.
    ///
    /// With the shadow, that copy is an ordinary cached memmove and the surface
    /// is never read at all — only written, sequentially, in the mode the
    /// mapping is now good at.
    shadow: *mut u32,
    /// Buddy allocation backing `shadow`, kept for the free that never happens
    /// (the console lives for the life of the kernel) but which a future
    /// mode-set path will need.
    shadow_phys: usize,
    shadow_order: usize,
    width:  usize,
    height: usize,
    pitch:  usize, // bytes per row
    cursor_x: usize,
    cursor_y: usize,
    vector_font: Option<VectorFont>,
    char_width: usize,
    char_height: usize,
    /// Escape-sequence parser state.
    esc: EscState,
    /// Raw parameter/intermediate bytes of the CSI sequence being collected.
    params: [u8; 32],
    params_len: usize,
    /// Current SGR foreground colour; reset to white by `SGR 0`.
    fg: u32,
    /// Cursor saved by `ESC 7` / `CSI s`, restored by `ESC 8` / `CSI u`.
    saved_cursor: (usize, usize),
    /// Bounding box of pixels modified since the last flush, as
    /// `(min_x, min_y, max_x, max_y)` with the maxima exclusive.  Lets
    /// `fb_flush` transfer only the changed region to the GPU instead of the
    /// whole screen — cheap enough to flush on every character, which keeps the
    /// shell prompt and typed input visible on the VirtIO-GPU path.
    dirty: Option<(usize, usize, usize, usize)>,
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
            shadow: core::ptr::null_mut(),
            shadow_phys: 0,
            shadow_order: 0,
            width:  0,
            height: 0,
            pitch:  0,
            cursor_x: 0,
            cursor_y: 0,
            vector_font: None,
            char_width: 12,  // Vector font character width
            char_height: 20, // Vector font character height
            esc: EscState::Ground,
            params: [0; 32],
            params_len: 0,
            fg: 0xFFFFFF,
            saved_cursor: (0, 0),
            dirty: None,
        }
    }

    /// Where drawing goes: the write-back shadow when there is one, the raw
    /// framebuffer otherwise. Every pixel write in this file must go through
    /// this — writing `base` directly bypasses the shadow and the next blit
    /// overwrites it.
    #[inline]
    fn surface(&self) -> *mut u32 {
        if self.shadow.is_null() { self.base } else { self.shadow }
    }

    /// Back this console with a write-back shadow sized to the current mode.
    ///
    /// Best-effort: if the buddy allocator cannot satisfy it (or has not been
    /// brought up yet), the console keeps drawing straight to the framebuffer,
    /// which is exactly the behaviour that shipped before the shadow existed.
    /// Never allocates twice for the same geometry.
    fn alloc_shadow(&mut self) {
        let bytes = self.pitch * self.height;
        if bytes == 0 || self.base.is_null() { return; }
        if !self.shadow.is_null() {
            if (1usize << self.shadow_order) * 4096 >= bytes { return; }
            mm::buddy::free(self.shadow_phys, self.shadow_order);
            self.shadow = core::ptr::null_mut();
        }
        let pages = (bytes + 4095) / 4096;
        let mut order = 0;
        while (1usize << order) < pages { order += 1; }
        if let Some(phys) = mm::buddy::alloc(order) {
            self.shadow_phys = phys;
            self.shadow_order = order;
            self.shadow = mm::phys_to_virt(phys) as *mut u32;
            // Start from the surface's current contents rather than black, so
            // enabling the shadow mid-session does not blank what boot already
            // painted. This is the one read of the framebuffer that remains, and
            // it happens once.
            unsafe { core::ptr::copy_nonoverlapping(self.base, self.shadow, bytes / 4); }
        }
    }

    /// Copy one dirty rectangle from the shadow into the real framebuffer.
    ///
    /// No-op when there is no shadow — drawing already went straight to `base`.
    /// Row by row rather than one call, because the rectangle is a sub-span of
    /// each row and the rows are `pitch` apart.
    fn blit_to_framebuffer(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.shadow.is_null() || self.base.is_null() { return; }
        let stride = self.pitch / 4;
        for row in y..(y + h).min(self.height) {
            let off = row * stride + x;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.shadow.add(off),
                    self.base.add(off),
                    w.min(self.width.saturating_sub(x)),
                );
            }
        }
    }

    /// Expand the dirty bounding box to include the `w`×`h` region at `(x, y)`.
    fn mark_dirty(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let (x1, y1) = (x + w, y + h);
        self.dirty = Some(match self.dirty {
            Some((dx0, dy0, dx1, dy1)) => (dx0.min(x), dy0.min(y), dx1.max(x1), dy1.max(y1)),
            None => (x, y, x1, y1),
        });
    }

    /// Take the accumulated dirty region (clamped to the screen) and clear it.
    /// Returns `(x, y, width, height)`, or `None` if nothing changed.
    pub fn take_dirty(&mut self) -> Option<(usize, usize, usize, usize)> {
        let (x0, y0, x1, y1) = self.dirty.take()?;
        let x1 = x1.min(self.width);
        let y1 = y1.min(self.height);
        if x1 <= x0 || y1 <= y0 { return None; }
        Some((x0, y0, x1 - x0, y1 - y0))
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
            let dst = self.surface();
            unsafe {
                let offset = y * (self.pitch / 4) + x;
                dst.add(offset).write_volatile(color);
            }
            self.mark_dirty(x, y, 1, 1);
        }
    }

    pub fn clear(&mut self, color: u32) {
        if self.base.is_null() { return; }
        let total_words = self.height * (self.pitch / 4);
        let dst = self.surface();
        if color == 0 {
            unsafe {
                core::ptr::write_bytes(dst, 0, total_words);
            }
        } else {
            unsafe {
                for i in 0..total_words {
                    dst.add(i).write_volatile(color);
                }
            }
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        let (w, h) = (self.width, self.height);
        self.mark_dirty(0, 0, w, h);
    }

    pub fn putc(&mut self, c: u8) {
        if self.base.is_null() { return; }

        static mut UTF8_STATE: (usize, u32) = (0, 0);

        // ESC restarts a sequence from any state (except inside an OSC string,
        // where it may be the first half of the ST terminator).
        if c == 0x1b {
            self.esc = if self.esc == EscState::Osc { EscState::OscEsc } else { EscState::Esc };
            self.params_len = 0;
            return;
        }

        match self.esc {
            EscState::Ground => {}
            EscState::Esc => {
                self.esc = match c {
                    b'[' => { self.params_len = 0; EscState::Csi }
                    b']' => EscState::Osc,
                    // Charset / DEC selectors take one more byte: ESC ( B, ESC # 8, …
                    b'(' | b')' | b'*' | b'+' | b'#' | b'%' => EscState::Discard1,
                    b'7' => { self.saved_cursor = (self.cursor_x, self.cursor_y); EscState::Ground }
                    b'8' => { let (x, y) = self.saved_cursor;
                              self.cursor_x = x; self.cursor_y = y; EscState::Ground }
                    b'M' => { self.reverse_index(); EscState::Ground }
                    // ESC =, ESC >, ESC c, … — nothing we need to render.
                    _ => EscState::Ground,
                };
                return;
            }
            EscState::Csi => {
                if (0x20..=0x3f).contains(&c) {
                    // Parameter or intermediate byte — accumulate. Overlong
                    // sequences keep parsing; only the excess bytes are lost.
                    if self.params_len < self.params.len() {
                        self.params[self.params_len] = c;
                        self.params_len += 1;
                    }
                } else if (0x40..=0x7e).contains(&c) {
                    // Final byte — `params` is Copy, so take it by value to
                    // release the borrow before the &mut self dispatch.
                    let (params, len) = (self.params, self.params_len);
                    self.handle_csi(&params[..len], c);
                    self.esc = EscState::Ground;
                } else {
                    // A C0 control aborted the sequence; drop it and resync.
                    self.esc = EscState::Ground;
                }
                return;
            }
            EscState::Osc => {
                // OSC strings (e.g. `ESC ]0;title BEL`) carry arbitrary text.
                // The old parser stopped at the first letter and spilled the
                // rest of the title onto the screen; swallow to BEL instead.
                if c == 0x07 { self.esc = EscState::Ground; }
                return;
            }
            EscState::OscEsc => {
                self.esc = if c == b'\\' { EscState::Ground } else { EscState::Osc };
                return;
            }
            EscState::Discard1 => {
                self.esc = EscState::Ground;
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
                    self.draw_char_vector(self.cursor_x, self.cursor_y, c as char, self.fg);
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
                        self.draw_char_vector(self.cursor_x, self.cursor_y, display_char, self.fg);
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

    // ── Cell-grid helpers ─────────────────────────────────────────────────
    //
    // `cursor_x`/`cursor_y` are pixel coordinates, but CSI positioning is in
    // character cells, so convert at the boundary.

    fn cols(&self) -> usize { self.width / self.char_width }
    fn rows(&self) -> usize { self.height / self.char_height }
    fn col(&self)  -> usize { self.cursor_x / self.char_width }
    fn row(&self)  -> usize { self.cursor_y / self.char_height }

    fn set_cell(&mut self, col: usize, row: usize) {
        self.cursor_x = col * self.char_width;
        self.cursor_y = row * self.char_height;
    }

    /// Fill a pixel rectangle, marking the whole region dirty once instead of
    /// per pixel (erases cover thousands of pixels on every repaint).
    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        if self.base.is_null() { return; }
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        if x >= x1 || y >= y1 { return; }
        let stride = self.pitch / 4;
        let dst = self.surface();
        unsafe {
            for yy in y..y1 {
                let row = dst.add(yy * stride);
                for xx in x..x1 { row.add(xx).write_volatile(color); }
            }
        }
        self.mark_dirty(x, y, x1 - x, y1 - y);
    }

    /// `ESC M` — move up one line, scrolling down if already at the top.
    fn reverse_index(&mut self) {
        if self.cursor_y >= self.char_height {
            self.cursor_y -= self.char_height;
        }
    }

    /// Dispatch a CSI sequence given its raw parameter bytes and final byte.
    ///
    /// This replaces an exact-match on three literal sequences.  Parameterised
    /// forms are what line editors actually emit — reedline repaints with
    /// `CSI <row>;1 H` followed by `CSI J`, neither of which the literal match
    /// recognised, so the cursor never returned to the prompt and every
    /// keystroke appended a fresh prompt line.
    fn handle_csi(&mut self, params: &[u8], final_byte: u8) {
        if self.cols() == 0 || self.rows() == 0 { return; }

        // Parse `1;2;3` into numbers, tracking which were actually present so
        // omitted parameters can take their (per-sequence) default rather than 0.
        let mut nums = [0usize; 8];
        let mut present = [false; 8];
        let mut count = 0usize;
        let mut cur: Option<usize> = None;
        // A leading `?` marks a DEC private sequence (cursor visibility,
        // bracketed paste, …) — parsed, then ignored below.
        let private = params.first() == Some(&b'?');
        for &b in params {
            match b {
                b'0'..=b'9' => cur = Some(cur.unwrap_or(0) * 10 + (b - b'0') as usize),
                b';' => {
                    if count < nums.len() {
                        nums[count] = cur.unwrap_or(0);
                        present[count] = cur.is_some();
                        count += 1;
                    }
                    cur = None;
                }
                _ => {}
            }
        }
        if count < nums.len() {
            nums[count] = cur.unwrap_or(0);
            present[count] = cur.is_some();
            count += 1;
        }
        let p = |i: usize, default: usize| {
            if i < count && present[i] { nums[i] } else { default }
        };

        let (cols, rows) = (self.cols(), self.rows());
        let (mut col, mut row) = (self.col(), self.row());

        match final_byte {
            _ if private => {}                                   // DEC private modes
            b'A' => row = row.saturating_sub(p(0, 1)),            // CUU
            b'B' => row = (row + p(0, 1)).min(rows - 1),          // CUD
            b'C' => col = (col + p(0, 1)).min(cols - 1),          // CUF
            b'D' => col = col.saturating_sub(p(0, 1)),            // CUB
            b'E' => { row = (row + p(0, 1)).min(rows - 1); col = 0; }        // CNL
            b'F' => { row = row.saturating_sub(p(0, 1)); col = 0; }          // CPL
            b'G' | b'`' => col = p(0, 1).saturating_sub(1).min(cols - 1),    // CHA
            b'd' => row = p(0, 1).saturating_sub(1).min(rows - 1),           // VPA
            b'H' | b'f' => {                                                 // CUP
                row = p(0, 1).saturating_sub(1).min(rows - 1);
                col = p(1, 1).saturating_sub(1).min(cols - 1);
            }
            b'J' => self.erase_in_display(p(0, 0)),              // ED
            b'K' => self.erase_in_line(p(0, 0)),                 // EL
            b'm' => self.apply_sgr(&nums[..count], &present[..count]),       // SGR
            b's' => self.saved_cursor = (self.cursor_x, self.cursor_y),
            b'u' => { let (x, y) = self.saved_cursor; self.cursor_x = x; self.cursor_y = y; }
            _ => {}                                              // unimplemented — drop
        }

        self.set_cell(col, row);
    }

    /// `CSI <n> J` — 0: cursor to end of screen, 1: start of screen to cursor,
    /// 2/3: the whole screen.  The cursor does not move (both in-tree callers
    /// pair `CSI 2J` with an explicit `CSI H`).
    fn erase_in_display(&mut self, mode: usize) {
        let (w, h, ch) = (self.width, self.height, self.char_height);
        let (x, y) = (self.cursor_x, self.cursor_y);
        match mode {
            0 => {
                self.fill_rect(x, y, w - x.min(w), ch, 0x000000);
                self.fill_rect(0, y + ch, w, h.saturating_sub(y + ch), 0x000000);
            }
            1 => {
                self.fill_rect(0, 0, w, y, 0x000000);
                self.fill_rect(0, y, x, ch, 0x000000);
            }
            _ => self.fill_rect(0, 0, w, h, 0x000000),
        }
    }

    /// `CSI <n> K` — 0: cursor to end of line, 1: start of line to cursor,
    /// 2: the whole line.
    fn erase_in_line(&mut self, mode: usize) {
        let (w, ch) = (self.width, self.char_height);
        let (x, y) = (self.cursor_x, self.cursor_y);
        match mode {
            0 => self.fill_rect(x, y, w - x.min(w), ch, 0x000000),
            1 => self.fill_rect(0, y, x, ch, 0x000000),
            _ => self.fill_rect(0, y, w, ch, 0x000000),
        }
    }

    /// `CSI ... m` — foreground colour only; the background stays black.
    /// Previously all text rendered hardcoded white, so a highlighting prompt
    /// lost its colours entirely.
    fn apply_sgr(&mut self, nums: &[usize], present: &[bool]) {
        const BASE: [u32; 8] = [
            0x000000, 0xCD0000, 0x00CD00, 0xCDCD00,
            0x0000EE, 0xCD00CD, 0x00CDCD, 0xE5E5E5,
        ];
        const BRIGHT: [u32; 8] = [
            0x7F7F7F, 0xFF0000, 0x00FF00, 0xFFFF00,
            0x5C5CFF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
        ];

        // A bare `CSI m` means `CSI 0 m`.
        if nums.is_empty() || !present.first().copied().unwrap_or(false) {
            self.fg = 0xFFFFFF;
            if nums.len() <= 1 { return; }
        }

        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 => self.fg = 0xFFFFFF,
                30..=37 => self.fg = BASE[nums[i] - 30],
                90..=97 => self.fg = BRIGHT[nums[i] - 90],
                39 => self.fg = 0xFFFFFF,
                38 => {
                    // 38;5;<n> (256-colour) or 38;2;<r>;<g>;<b> (truecolour)
                    match nums.get(i + 1) {
                        Some(5) => {
                            if let Some(&n) = nums.get(i + 2) {
                                self.fg = xterm256(n, &BASE, &BRIGHT);
                            }
                            i += 2;
                        }
                        Some(2) => {
                            let r = nums.get(i + 2).copied().unwrap_or(0) as u32;
                            let g = nums.get(i + 3).copied().unwrap_or(0) as u32;
                            let b = nums.get(i + 4).copied().unwrap_or(0) as u32;
                            self.fg = (r << 16) | (g << 8) | b;
                            i += 4;
                        }
                        _ => {}
                    }
                }
                _ => {} // bold/underline/background — not rendered
            }
            i += 1;
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
        // Clear the cell first: glyph rendering only lights set bits, so
        // overwriting one character with another would leave the previous
        // glyph's pixels behind.  Append-only output never hit this, but a
        // line editor repaints the same cells on every keystroke.
        let (cw, ch) = (self.char_width, self.char_height);
        self.fill_rect(x, y, cw, ch, 0x000000);

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

    #[allow(dead_code)]
    fn scroll(&mut self) {
        let shift = self.scroll_shift_px(8); // fallback char height
        self.scroll_px(shift);
    }

    /// Pixel rows one scroll advances by, for a console whose text rows are
    /// `char_h` pixels tall: [`SCROLL_ROWS`] of them, clamped so at least one
    /// text row of surface is left above the cursor.
    fn scroll_shift_px(&self, char_h: usize) -> usize {
        if char_h == 0 || self.height <= char_h { return 0; }
        let usable_rows = (self.height / char_h).saturating_sub(1).max(1);
        char_h * SCROLL_ROWS.min(usable_rows)
    }

    /// Shift the surface up by `shift_px` pixel rows and blank what that
    /// exposes at the bottom.
    ///
    /// This used to be the console's whole cost, because the copy both read and
    /// wrote the surface through the uncached framebuffer mapping. It now runs
    /// against the write-back shadow (see [`Framebuffer::shadow`]), so it is an
    /// ordinary cached memmove; what reaches the framebuffer is the blit in
    /// [`fb_flush`], which only ever writes.
    ///
    /// It still dirties the whole screen — every pixel really did move — so a
    /// scroll still costs one full-surface blit and one full-surface transfer.
    /// That is why [`SCROLL_ROWS`] still advances several rows at a time.
    fn scroll_px(&mut self, shift_px: usize) {
        if self.base.is_null() || shift_px == 0 || shift_px >= self.height { return; }
        let words_per_row = self.pitch / 4;
        let rows_to_copy = self.height - shift_px;
        let dst = self.surface();
        unsafe {
            core::ptr::copy(
                dst.add(shift_px * words_per_row),
                dst,
                rows_to_copy * words_per_row,
            );
            core::ptr::write_bytes(
                dst.add(rows_to_copy * words_per_row),
                0,
                shift_px * words_per_row,
            );
        }
        self.cursor_y = self.cursor_y.saturating_sub(shift_px);
        // The scroll rewrote the whole surface via raw memory ops (bypassing
        // set_pixel), so the entire screen must be re-transmitted.
        let (w, h) = (self.width, self.height);
        self.mark_dirty(0, 0, w, h);
    }

    /// Scroll screen for vector font
    fn scroll_vector(&mut self) {
        let shift = self.scroll_shift_px(self.char_height);
        self.scroll_px(shift);
    }
}

impl Driver for Framebuffer {
    /// Initialise from boot-provided parameters.
    ///
    /// Returns `Err(DriverError::NotFound)` if the bootloader did not supply a
    /// linear framebuffer (e.g. text-mode boot, or the DTB has no /framebuffer
    /// node).
    fn probe(&mut self) -> Result<(), DriverError> {
        let info = (*BOOT_FB.lock()).ok_or(DriverError::NotFound)?;

        if info.base == 0 || info.width == 0 || info.height == 0 || info.pitch == 0 {
            return Err(DriverError::NotFound);
        }

        self.base   = mm::phys_to_virt(info.base as usize) as *mut u32;
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

// ── Scanout ownership ─────────────────────────────────────────────────────────
//
// The framebuffer console and the DRM scanout are the SAME surface. `kms.rs`
// points BOOT_FB and KERNEL_FB at the RAM buffer that backs virtio-gpu resource
// 1, and every DRM present composites into whatever `get_hardware_fb_info()`
// reports — see `perform_software_scaling` and `present_damaged` in
// `drivers/src/drm/device.rs`, which resolve their destination from exactly
// that call.
//
// So a console write during a live session is not an overlay that a repaint
// will cover: a single '\n' past the last text row runs `scroll_vector`, which
// memmoves the WHOLE surface up one row and blacks the bottom one. A
// compositor repaints only what it damaged, so everything static is scrolled
// off and never redrawn. Measured on a COSMIC session with a client logging one
// line per frame: 334503 distinct colours collapsed to 177, 79% of the screen
// went pure black, and the client's four previous frames were left smeared into
// bands an exact multiple of the text row apart. Running the same client quiet
// restored the wallpaper.
//
// The predicate that gates the console is therefore not "did a particular ioctl
// arrive". That is what this replaces: a hardcoded list of SETCRTC / PAGE_FLIP
// / two custom numbers, which missed DRM_IOCTL_MODE_ATOMIC — the path
// cosmic-comp actually drives — so an atomic compositor never turned the
// console off at all. The predicate is "has a DRM client composited into this
// surface and not yet given it back", claimed from the present path itself
// (`handle_ioctl` in drivers/src/drm_device_interface.rs), so a present path
// added later is covered without being listed anywhere.
//
// Ownership is per card0 *open*, not a global flag, because the other half of
// the same defect was that ANY card0 close handed the console back — and
// reclaim CLEARS the screen, so a short-lived second open of the node wiped a
// live compositor's display outright.

/// Nobody holds the scanout; the console owns it and paints normally.
const SCANOUT_UNOWNED: u32 = 0;
/// Claimed by a caller with no open identity (the legacy `Driver::handle`
/// path, which has no per-open cookie to key on). Any card0 close releases it,
/// which is what that path effectively did before ownership existed.
const SCANOUT_ANON: u32 = u32::MAX;

static SCANOUT_OWNER: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(SCANOUT_UNOWNED);

/// The card0 open currently scanning out, or [`SCANOUT_UNOWNED`].
pub fn scanout_owner() -> u32 {
    SCANOUT_OWNER.load(core::sync::atomic::Ordering::SeqCst)
}

/// A DRM present just composited into the shared surface on behalf of
/// `open_id`. Silences the console until that open gives the scanout back.
///
/// Called on every present, so it must stay cheap: one RMW and, only on the
/// first claim, the flag flip that stops `serial_write_byte` reaching
/// `fb_putc`.
pub fn drm_scanout_claim(open_id: u32) {
    let owner = if open_id == 0 { SCANOUT_ANON } else { open_id };
    if SCANOUT_OWNER.swap(owner, core::sync::atomic::Ordering::SeqCst) == SCANOUT_UNOWNED {
        set_console_disabled(true);
    }
}

/// A card0 open closed. Give the console back iff that open was the one
/// scanning out — closing some other open of the node must not resurrect the
/// console under a live master, because reclaim clears the screen.
///
/// `open_id` 0 means "no open identity" and releases unconditionally: it is
/// what the VFS_CLOSE_ALL path passes, and an anonymous claim has nothing
/// better to match against.
pub fn drm_scanout_release(open_id: u32) {
    let owner = SCANOUT_OWNER.load(core::sync::atomic::Ordering::SeqCst);
    if owner == SCANOUT_UNOWNED { return; }
    if open_id != 0 && owner != SCANOUT_ANON && owner != open_id { return; }
    if SCANOUT_OWNER.swap(SCANOUT_UNOWNED, core::sync::atomic::Ordering::SeqCst)
        != SCANOUT_UNOWNED
    {
        set_console_disabled(false);
    }
}

/// Take the scanout back for a kernel panic, unconditionally.
///
/// A panic during a live session would otherwise be invisible on screen — the
/// console has yielded, and nothing is ever going to give it back. Deliberately
/// two atomic stores and no locks and no repaint: the panicking thread may
/// already hold `KERNEL_FB`, and a panic handler that blocks on it prints
/// nothing at all. The panic text simply overwrites the compositor's last
/// frame, which is the right outcome for a panic.
pub fn console_force_reclaim() {
    SCANOUT_OWNER.store(SCANOUT_UNOWNED, core::sync::atomic::Ordering::SeqCst);
    extern "C" { fn kernel_set_console_enabled(enabled: bool); }
    unsafe { kernel_set_console_enabled(true); }
}

/// Disable or enable kernel console output to prevent blinking during DRM operations.
pub fn set_console_disabled(disabled: bool) {
    extern "C" { fn kernel_set_console_enabled(enabled: bool); }
    unsafe { kernel_set_console_enabled(!disabled); }
    
    // If we are re-enabling the console, trigger a redraw
    if !disabled {
        let mut fb = KERNEL_FB.lock();
        fb.clear(0x000000);
        fb.cursor_x = 0;
        fb.cursor_y = 0;
        
        // Print a message to show the console is back
        let msg = b"\n[Console Resumed]\n> \0";
        for &b in msg {
            if b == 0 { break; }
            fb.putc(b);
        }
    }
}

/// Output a character to the global kernel framebuffer.
pub fn fb_putc(c: u8) {
    KERNEL_FB.lock().putc(c);
}

/// Console cursor position in 1-based `(row, col)` character cells.
///
/// This is the authoritative answer to a `CSI 6 n` cursor-position report: the
/// framebuffer is the primary console, so its cursor — not whatever terminal
/// happens to be listening on the serial line — is what a line editor must be
/// told about.  Returns `(1, 1)` before the framebuffer is initialised.
pub fn fb_cursor_cell() -> (usize, usize) {
    let fb = KERNEL_FB.lock();
    if fb.char_width == 0 || fb.char_height == 0 { return (1, 1); }
    (fb.row() + 1, fb.col() + 1)
}

/// Console size in character cells as `(cols, rows)`, or `None` if the
/// framebuffer has not been initialised yet.
pub fn fb_console_size() -> Option<(usize, usize)> {
    let fb = KERNEL_FB.lock();
    if fb.width == 0 || fb.height == 0 || fb.char_width == 0 || fb.char_height == 0 {
        return None;
    }
    Some((fb.cols(), fb.rows()))
}

/// Nesting depth of [`FlushBatch`]. Non-zero means "a caller is mid-burst;
/// accumulate into the dirty box and transfer once at the end".
static FLUSH_DEFER: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Suppresses [`fb_flush`] for the lifetime of the guard, then flushes once.
///
/// Each flush is two *synchronous* virtqueue round-trips — TRANSFER_TO_HOST_2D
/// then RESOURCE_FLUSH, each allocating and freeing buddy pages, ringing the
/// notify register (a VM exit) and spinning until the host posts a used-ring
/// entry. `serial_write_byte` pays that per byte, so an 80-column line of `ls`
/// output cost 160 round-trips to deliver 80 characters.
///
/// The dirty region is a bounding box that already accumulates across
/// characters, so deferring changes nothing about what gets transferred for a
/// left-to-right run of text — the box after 80 characters is the same box, one
/// text row tall. It only stops re-transferring the growing prefix 80 times.
///
/// Hold this around a whole `write()` worth of output, not longer: the point at
/// which it drops is the point at which the user sees the text.
pub struct FlushBatch(());

impl FlushBatch {
    /// Begin deferring. Nests, so an inner batch does not flush early.
    pub fn new() -> Self {
        FLUSH_DEFER.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        FlushBatch(())
    }
}

impl Drop for FlushBatch {
    fn drop(&mut self) {
        // Drop-based so an early return or a panic mid-burst cannot strand the
        // counter above zero and silence the console permanently.
        if FLUSH_DEFER.fetch_sub(1, core::sync::atomic::Ordering::SeqCst) == 1 {
            fb_flush();
        }
    }
}

/// Flush the kernel framebuffer to the GPU if present.
///
/// Drops the KERNEL_FB lock before acquiring VIRTIO_GPU to avoid lock-order
/// inversions.  Skips the call if dimensions are still zero (framebuffer not
/// yet initialised) so that set_scanout(1, 0, 0) is never sent to the host.
///
/// A no-op while a [`FlushBatch`] is live; that guard flushes on drop.
#[no_mangle]
pub fn fb_flush() {
    if FLUSH_DEFER.load(core::sync::atomic::Ordering::SeqCst) != 0 { return; }
    // Pull (and clear) the changed region under the KERNEL_FB lock, then release
    // it before taking the VIRTIO_GPU lock to preserve lock ordering.
    let rect = {
        let mut fb = KERNEL_FB.lock();
        if fb.width == 0 || fb.height == 0 { return; }
        let rect = fb.take_dirty();
        // Push the changed rows out of the shadow while still holding the lock,
        // so a concurrent writer cannot alter them between the blit and the
        // transfer below. Pure sequential stores into the framebuffer — the one
        // access pattern an uncached-but-write-combining mapping is good at, and
        // the reason `scroll_px` no longer touches it at all.
        if let Some((x, y, w, h)) = rect {
            fb.blit_to_framebuffer(x, y, w, h);
        }
        rect
    };
    if let Some((x, y, w, h)) = rect {
        publish_pixels();
        if let Some(gpu) = &mut *crate::virtio_gpu::VIRTIO_GPU.lock() {
            gpu.flush(1, x as u32, y as u32, w as u32, h as u32);
        }
    }
}

/// Order every pixel store issued so far ahead of the doorbell that tells the
/// device to go read them.
///
/// The framebuffer is mapped Write Combining on x86_64 and Normal
/// Inner/Outer Non-cacheable on AArch64 (see each `arch::init`). Both are
/// uncached — no cache maintenance is needed and none is done here — but unlike
/// the strongly-ordered Device/UC- mappings they replaced, both are *weakly
/// ordered*: stores may sit in a write-combining buffer and may be observed out
/// of order. Without this fence the `TRANSFER_TO_HOST_2D` below can race the
/// pixels it is meant to transfer, and the host reads a partially written
/// surface — which shows up as torn or stale rectangles, not as a hang.
#[inline]
fn publish_pixels() {
    #[cfg(target_arch = "x86_64")]
    unsafe { core::arch::asm!("sfence", options(nostack, nomem, preserves_flags)); }
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("dsb sy", options(nostack, nomem, preserves_flags)); }
}

/// Initialize the kernel-space framebuffer console.
pub unsafe fn init_kernel_fb(base: *mut u32, width: usize, height: usize, pitch: usize) {
    let mut fb = KERNEL_FB.lock();
    fb.base = base;
    fb.width = width;
    fb.height = height;
    fb.pitch = pitch;
    fb.init_vector_font(); // Initialize vector font
    fb.alloc_shadow();
    fb.clear(0);
    // `clear` only painted the shadow; put those pixels on screen now rather
    // than leaving whatever the bootloader left there until the first write.
    let (w, h) = (fb.width, fb.height);
    fb.blit_to_framebuffer(0, 0, w, h);
}

/// Initialize the kernel-space framebuffer console without clearing screen.
pub unsafe fn update_kernel_fb(base: *mut u32, width: usize, height: usize, pitch: usize) {
    let mut fb = KERNEL_FB.lock();
    fb.base = base;
    fb.width = width;
    fb.height = height;
    fb.pitch = pitch;
    fb.init_vector_font(); // Initialize vector font
    // Don't clear - preserve existing content. `alloc_shadow` seeds itself from
    // the current surface, so the existing content survives into the shadow too.
    fb.alloc_shadow();
}

/// Text rows the console advances per scroll, rather than one.
///
/// A scroll copies the WHOLE surface and then marks the whole surface dirty, so
/// it costs the same whether it advances one row or many — and because a full
/// screen scrolls on every newline, that cost was the console's throughput
/// ceiling. Measured on x86_64/KVM at 1920x1080x4 (8.29 MB, mapped uncached —
/// PAT UC-, see arch/x86_64/src/lib.rs): 100 lines of 41 characters took
/// 157.9 s and 220 lines of 1 character took 400.6 s. That is 1.58 s and 1.82 s
/// PER LINE for a 24x difference in bytes, which is what identifies the scroll
/// rather than the UART as the ceiling: the console was not dropping output, it
/// was delivering it at ~0.6 lines/s. Long test output therefore looked
/// truncated, and a harness reading it looked like it had lost bytes.
///
/// Advancing N rows per scroll divides that rate by N. The price is that the
/// console jumps N rows and leaves N blank rows under the cursor — the trade
/// fbcon makes when it has no panning. Removing the cost rather than dividing
/// it means never reading the surface back (a write-back shadow) AND giving it
/// a write-combining mapping; arch/x86_64/src/paging.rs is explicit that a
/// cached alias of host-visible memory is a bug, so that belongs to a lane that
/// can verify the memory type end to end.
const SCROLL_ROWS: usize = 8;

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
