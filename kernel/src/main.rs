//! Leandros kernel entry point.

#![no_std]
#![no_main]

extern crate alloc;

mod init;
mod syscall;
mod mem;

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(include_str!("entry_aarch64.s"));
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(include_str!("entry_x86_64.s"));

#[repr(C, align(16))]
pub struct Stack<const N: usize>([u8; N]);

#[no_mangle]
pub static mut EARLY_STACK: Stack<0x10000> = Stack([0u8; 0x10000]);

#[global_allocator]
static ALLOCATOR: mm::slab::SlabAllocator = mm::slab::SlabAllocator;

// ── Limine Revision 6 Compliance ─────────────────────────────────────────────

#[used]
#[link_section = ".limine_reqs_start"]
static START_MARKER: limine::RequestsStartMarker = limine::RequestsStartMarker::new();

#[used]
#[link_section = ".limine_reqs"]
static BASE_REVISION: limine::BaseRevision = limine::BaseRevision::new();

#[used]
#[link_section = ".limine_reqs_end"]
static END_MARKER: limine::RequestsEndMarker = limine::RequestsEndMarker::new();

pub static mut BOOT_INFO_PTR: usize = 0;

// ── Serial and Framebuffer Console ──────────────────────────────────────────

static mut FB_CONSOLE: Option<FbConsole> = None;

struct FbConsole {
    base:   *mut u32,
    width:  usize,
    height: usize,
    pitch:  usize,
    cursor_x: usize,
    cursor_y: usize,
}

impl FbConsole {
    const CHAR_WIDTH: usize = 8;
    const CHAR_HEIGHT: usize = 8;
    const FONT: [u8; 128 * 8] = include_font();

    fn new(base: *mut u32, width: usize, height: usize, pitch: usize) -> Self {
        Self { base, width, height, pitch, cursor_x: 0, cursor_y: 0 }
    }

    fn putc(&mut self, c: u8) {
        if c == b'\n' {
            self.cursor_x = 0;
            self.cursor_y += Self::CHAR_HEIGHT;
        } else if c == b'\r' {
            self.cursor_x = 0;
        } else {
            // Skip UTF-8 continuation bytes to keep cursor alignment
            if (c & 0xC0) != 0x80 {
                self.draw_char(self.cursor_x, self.cursor_y, c, 0xFFFFFF);
                self.cursor_x += Self::CHAR_WIDTH;
                if self.cursor_x + Self::CHAR_WIDTH > self.width {
                    self.cursor_x = 0;
                    self.cursor_y += Self::CHAR_HEIGHT;
                }
            }
        }

        if self.cursor_y + Self::CHAR_HEIGHT > self.height {
            self.scroll();
        }
    }

    fn draw_char(&mut self, x: usize, y: usize, c: u8, color: u32) {
        if (c as usize) * 8 + 8 > Self::FONT.len() {
            return;
        }
        let glyph = &Self::FONT[(c as usize) * 8 .. (c as usize) * 8 + 8];
        for (gy, &row) in glyph.iter().enumerate() {
            for gx in 0..8 {
                if (row & (1 << (7 - gx))) != 0 {
                    self.set_pixel(x + gx, y + gy, color);
                }
            }
        }
    }

    fn set_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x < self.width && y < self.height {
            unsafe {
                let offset = y * (self.pitch / 4) + x;
                self.base.add(offset).write_volatile(color);
            }
        }
    }

    fn scroll(&mut self) {
        let rows_to_copy = self.height - Self::CHAR_HEIGHT;
        unsafe {
            core::ptr::copy(
                self.base.add(Self::CHAR_HEIGHT * (self.pitch / 4)),
                self.base,
                rows_to_copy * (self.pitch / 4)
            );
            // Clear bottom line
            let bottom_start = rows_to_copy * (self.pitch / 4);
            core::ptr::write_bytes(self.base.add(bottom_start), 0, Self::CHAR_HEIGHT * self.pitch);
        }
        self.cursor_y -= Self::CHAR_HEIGHT;
    }
}

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

pub fn serial_write_byte(b: u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        arch_x86_64::putc(b);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        // Direct write to UART data register (assuming QEMU virt base)
        core::arch::asm!(
            "str {val:w}, [{base}]",
            val = in(reg) b as u32,
            base = in(reg) 0x09000000usize,
            options(nostack, nomem)
        );
    }

    unsafe {
        if let Some(ref mut console) = FB_CONSOLE {
            console.putc(b);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn arch_serial_putc(c: u8) {
    serial_write_byte(c);
}

#[no_mangle]
pub unsafe extern "C" fn serial_print_bytes(ptr: *const u8, len: usize) {
    let slice = core::slice::from_raw_parts(ptr, len);
    for &b in slice { serial_write_byte(b); }
}

pub fn serial_read_byte() -> Option<u8> {
    #[cfg(target_arch = "x86_64")]
    unsafe { arch_x86_64::serial_read_byte() }
    #[cfg(target_arch = "aarch64")]
    None
}

pub fn serial_has_data() -> bool {
    #[cfg(target_arch = "x86_64")]
    unsafe { arch_x86_64::serial_has_data() }
    #[cfg(target_arch = "aarch64")]
    false
}

pub fn serial_write_raw(msg: &[u8]) {
    for &b in msg { serial_write_byte(b); }
}

pub fn serial_print(msg: &str) {
    serial_write_raw(msg.as_bytes());
}

pub fn print_number(n: u32) {
    if n == 0 { serial_write_byte(b'0'); return; }
    let mut buf = [0u8; 10];
    let mut i = 0;
    let mut num = n;
    while num > 0 { buf[i] = b'0' + (num % 10) as u8; num /= 10; i += 1; }
    for j in (0..i).rev() { serial_write_byte(buf[j]); }
}

pub fn print_hex(n: usize) {
    let digits = b"0123456789ABCDEF";
    serial_print("0x");
    for i in (0..16).rev() { serial_write_byte(digits[(n >> (i * 4)) & 0xF]); }
}

// ── Kernel Entry ─────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn kernel_main(boot_info_addr: usize) -> ! {
    // Dummy references to ensure markers are linked.
    core::hint::black_box(&START_MARKER);
    core::hint::black_box(&BASE_REVISION);
    core::hint::black_box(&END_MARKER);

    serial_write_byte(b'M');
    serial_write_byte(b'1');

    serial_print("\n[LEANDROS] Kernel starting...\n");

    let is_limine = boot::limine::HHDM_REQUEST.response().is_some();

    let boot_info = if is_limine {
        unsafe { boot::limine::parse() }
    } else {
        #[cfg(target_arch = "x86_64")]
        { unsafe { boot::multiboot2::parse(boot_info_addr) } }
        #[cfg(target_arch = "aarch64")]
        { unsafe { boot::device_tree::parse(boot_info_addr) } }
    };

    mm::init_with_map(boot_info.memory_regions(), boot_info.hhdm_offset as usize);
    serial_print("  mm::phys_to_virt(0) = ");
    print_hex(mm::phys_to_virt(0));
    serial_print("\n");

    serial_print("[INIT] Architecture-specific init...\n");
    #[cfg(target_arch = "x86_64")]
    { arch_x86_64::init(&boot_info); }
    #[cfg(target_arch = "aarch64")]
    { arch_aarch64::init(&boot_info); }

    if boot_info.framebuffer_base != 0 {
        unsafe {
            let console = FbConsole::new(
                mm::phys_to_virt(boot_info.framebuffer_base as usize) as *mut u32,
                boot_info.framebuffer_width as usize,
                boot_info.framebuffer_height as usize,
                boot_info.framebuffer_pitch as usize,
            );
            // Clear screen
            for i in 0..(console.height * (console.pitch / 4)) {
                console.base.add(i).write_volatile(0);
            }
            FB_CONSOLE = Some(console);
        }
        serial_print("[INIT] Framebuffer console initialized\n");
    }

    serial_print("[INIT] Scheduler init...\n");
    sched::init();

    unsafe {
        BOOT_INFO_PTR = &boot_info as *const _ as usize;
    }
    init::init_task_main(&boot_info);
}

struct SerialWriter;
impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        serial_print(s);
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial_print("\n[LEANDROS] KERNEL PANIC: ");
    let mut writer = SerialWriter;
    let _ = core::fmt::write(&mut writer, core::format_args!("{}", info));
    loop { core::hint::spin_loop(); }
}
