//! Boot-progress squares painted straight into the bootloader's framebuffer,
//! for the stretch of boot before the framebuffer console exists (lane
//! rebootq, 2026-10-16: a real laptop showing only a black screen gives no
//! other clue where it stopped; the kernel log reaches the USB stick only
//! once USB storage is up, far later).
//!
//! Stage `n` (1-based) paints a 20x20 square at x = 8 + 28*(n-1), y = 8, in a
//! colour that cycles through white, red, green, blue, yellow, cyan,
//! magenta, orange — so a photo of the screen tells how many stages
//! completed (count the squares; the colour of the last one double-checks
//! the count). The console clears the screen when it starts, so a normal
//! boot shows them only for an instant.
//!
//! The address is the Limine framebuffer response's own pointer (mapped by
//! Limine in the page tables the kernel keeps running on), 32-bpp only.
//! `earlyfb=0` on the command line turns it off. The stage list is in
//! `kernel/src/main.rs` (`early_mark`).

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

static ADDR: AtomicUsize = AtomicUsize::new(0);
static PITCH: AtomicUsize = AtomicUsize::new(0);
static WIDTH: AtomicU32 = AtomicU32::new(0);
static HEIGHT: AtomicU32 = AtomicU32::new(0);

const COLORS: [u32; 8] = [0x00FF_FFFF, 0x00FF_0000, 0x0000_FF00, 0x0000_00FF,
                          0x00FF_FF00, 0x0000_FFFF, 0x00FF_00FF, 0x00FF_8000];

/// Arm the markers. `addr` is a mapped virtual address of a 32-bpp surface.
pub fn init(addr: usize, width: u32, height: u32, pitch_bytes: usize) {
    if addr == 0 || width < 64 || height < 64 || pitch_bytes < width as usize * 4 { return; }
    PITCH.store(pitch_bytes, Ordering::Relaxed);
    WIDTH.store(width, Ordering::Relaxed);
    HEIGHT.store(height, Ordering::Relaxed);
    ADDR.store(addr, Ordering::Release);
}

/// Stop painting (the console owns the surface from now on).
pub fn disable() { ADDR.store(0, Ordering::Release); }

/// Paint stage `n`'s square (1-based).
pub fn mark(n: u32) {
    let base = ADDR.load(Ordering::Acquire);
    if base == 0 || n == 0 { return; }
    let pitch = PITCH.load(Ordering::Relaxed);
    let w = WIDTH.load(Ordering::Relaxed) as usize;
    let h = HEIGHT.load(Ordering::Relaxed) as usize;
    // Wrap to a second row after the first one fills.
    let per_row = ((w - 8) / 28).max(1);
    let idx = (n - 1) as usize;
    let x0 = 8 + 28 * (idx % per_row);
    let y0 = 8 + 28 * (idx / per_row);
    if y0 + 20 > h { return; }
    let c = COLORS[idx % COLORS.len()];
    for y in y0..y0 + 20 {
        let row = base + y * pitch;
        for x in x0..(x0 + 20).min(w) {
            unsafe { core::ptr::write_volatile((row + x * 4) as *mut u32, c); }
        }
    }
}
