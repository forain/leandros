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
//!
//! ## Early text (lane fbmm, 2026-10-16)
//!
//! A tiny built-in 8x8 bitmap font drawn at 1x-4x scale into the same
//! surface, with no allocation and no lock a panicking CPU could hold
//! forever, below the squares row. Used for: the Limine memory-map dump
//! (`earlyfb.memmap=1`), a tee of every early serial byte (`earlyfb.log=1`,
//! implied by `earlyfb.memmap=1`), and -- always, knob or not -- the message
//! of a panic or CPU exception that happens before the console exists, which
//! used to end in a silent `loop {}`. At 2x a cell is 16x16 px: 120x65 cells
//! at 1920x1080, 64x45 at 1024x768.
//!
//! The text area wraps (no scrolling: reading back a write-combined
//! framebuffer is slow): at the bottom it continues at the first log row,
//! clearing each line as it is started plus the one after it, so the newest
//! line is always the one just above the blank gap.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

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


/// Paint stage `n`'s square (1-based).
pub fn mark(n: u32) {
    let base = ADDR.load(Ordering::Acquire);
    if n != 0 { LAST_STAGE.store(n, Ordering::Relaxed); }
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

/// Stop painting (the console owns the surface from now on).
pub fn disable() {
    TEXT.store(false, Ordering::Release);
    TEE.store(false, Ordering::Release);
    ADDR.store(0, Ordering::Release);
}

/// True while the markers (and so the early text) can still paint.
pub fn armed() -> bool { ADDR.load(Ordering::Acquire) != 0 }

/// The last stage `mark` was called with (0 = none yet).
pub fn last_stage() -> u32 { LAST_STAGE.load(Ordering::Relaxed) }

static LAST_STAGE: AtomicU32 = AtomicU32::new(0);

// ── Early text ───────────────────────────────────────────────────────────────

/// First pixel row of the text area (below the 20 px squares at y = 8).
const TOP_PX: usize = 36;

static TEXT: AtomicBool = AtomicBool::new(false);
static TEE: AtomicBool = AtomicBool::new(false);
static SCALE: AtomicUsize = AtomicUsize::new(2);
static ROW: AtomicUsize = AtomicUsize::new(0);
static COL: AtomicUsize = AtomicUsize::new(0);
/// First row the wrapping log returns to (rows above it hold the map table).
static LOG_TOP: AtomicUsize = AtomicUsize::new(0);
static FG: AtomicU32 = AtomicU32::new(GREY);
/// Table mode: characters are clipped to columns below CLIP_HI and newlines
/// are ignored.
static CLIP: AtomicBool = AtomicBool::new(false);
static CLIP_HI: AtomicUsize = AtomicUsize::new(0);
static LOCK: AtomicBool = AtomicBool::new(false);

pub const GREY: u32 = 0x00C0_C0C0;
pub const WHITE: u32 = 0x00FF_FFFF;
pub const RED: u32 = 0x00FF_4040;
pub const YELLOW: u32 = 0x00FF_FF40;
pub const GREEN: u32 = 0x0040_FF40;
pub const CYAN: u32 = 0x0040_FFFF;

/// Text columns and rows at `scale`.
pub fn grid(scale: usize) -> (usize, usize) {
    let w = WIDTH.load(Ordering::Relaxed) as usize;
    let h = HEIGHT.load(Ordering::Relaxed) as usize;
    let cell = 8 * scale.max(1);
    (w / cell, h.saturating_sub(TOP_PX) / cell)
}

/// The largest scale (<= `max`) at which `items` table cells `item_w` wide
/// plus `extra_rows` other rows fit the screen; 1 if none does.
pub fn choose_scale(max: usize, items: usize, item_w: usize, extra_rows: usize) -> usize {
    let mut s = max.clamp(1, 4);
    while s > 1 {
        let (c, r) = grid(s);
        let per = (c / item_w.max(1)).max(1);
        if items.div_ceil(per) + extra_rows <= r { return s; }
        s -= 1;
    }
    1
}

/// Start the text area at `scale` (1..=4). `tee` = also mirror early serial
/// output (see `tee_on`). No-op if the markers are not armed.
pub fn text_enable(scale: usize, tee: bool) {
    if !armed() { return; }
    if !TEXT.load(Ordering::Acquire) {
        SCALE.store(scale.clamp(1, 4), Ordering::Relaxed);
        ROW.store(0, Ordering::Relaxed);
        COL.store(0, Ordering::Relaxed);
        LOG_TOP.store(0, Ordering::Relaxed);
        clear_row(0);
        TEXT.store(true, Ordering::Release);
    }
    if tee { TEE.store(true, Ordering::Release); }
}

pub fn text_on() -> bool { TEXT.load(Ordering::Acquire) && armed() }
/// True when early serial output should also be drawn here.
pub fn tee_on() -> bool { TEE.load(Ordering::Acquire) && text_on() }

pub fn set_color(c: u32) { FG.store(c, Ordering::Relaxed); }

fn lock() -> bool {
    // Bounded: a panic on the CPU that holds it must still get its text out
    // (a garbled line beats a hang).
    for _ in 0..1_000_000 {
        if LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() { return true; }
        core::hint::spin_loop();
    }
    false
}
fn unlock(held: bool) { if held { LOCK.store(false, Ordering::Release); } }

fn fill_px(x0: usize, y0: usize, w: usize, h: usize, c: u32) {
    let base = ADDR.load(Ordering::Acquire);
    if base == 0 { return; }
    let pitch = PITCH.load(Ordering::Relaxed);
    let sw = WIDTH.load(Ordering::Relaxed) as usize;
    let sh = HEIGHT.load(Ordering::Relaxed) as usize;
    for y in y0..(y0 + h).min(sh) {
        let row = base + y * pitch;
        for x in x0..(x0 + w).min(sw) {
            unsafe { core::ptr::write_volatile((row + x * 4) as *mut u32, c); }
        }
    }
}

fn clear_row(r: usize) {
    let s = SCALE.load(Ordering::Relaxed);
    let (_, rows) = grid(s);
    if r >= rows { return; }
    fill_px(0, TOP_PX + r * 8 * s, WIDTH.load(Ordering::Relaxed) as usize, 8 * s, 0);
}

fn draw(r: usize, c: usize, ch: u8, fg: u32) {
    let base = ADDR.load(Ordering::Acquire);
    if base == 0 { return; }
    let s = SCALE.load(Ordering::Relaxed);
    let (cols, rows) = grid(s);
    if r >= rows || c >= cols { return; }
    let pitch = PITCH.load(Ordering::Relaxed);
    let g = if (ch as usize) < 128 { ch as usize } else { b'?' as usize };
    let x0 = c * 8 * s;
    let y0 = TOP_PX + r * 8 * s;
    for gy in 0..8 {
        let bits = FONT[g * 8 + gy];
        for sy in 0..s {
            let row = base + (y0 + gy * s + sy) * pitch;
            for gx in 0..8 {
                let px = if bits & (0x80 >> gx) != 0 { fg } else { 0 };
                for sx in 0..s {
                    unsafe { core::ptr::write_volatile((row + (x0 + gx * s + sx) * 4) as *mut u32, px); }
                }
            }
        }
    }
}

fn newline_locked() {
    let s = SCALE.load(Ordering::Relaxed);
    let (_, rows) = grid(s);
    COL.store(0, Ordering::Relaxed);
    let mut r = ROW.load(Ordering::Relaxed) + 1;
    if r >= rows { r = LOG_TOP.load(Ordering::Relaxed).min(rows.saturating_sub(1)); }
    ROW.store(r, Ordering::Relaxed);
    clear_row(r);
    // The blank line after the newest one marks where a wrapped log ends.
    if r + 1 < rows { clear_row(r + 1); }
}

fn putc_locked(b: u8) {
    if CLIP.load(Ordering::Relaxed) {
        if b == b'\n' || b == b'\r' { return; }
        let c = COL.load(Ordering::Relaxed);
        if c < CLIP_HI.load(Ordering::Relaxed) {
            draw(ROW.load(Ordering::Relaxed), c, b, FG.load(Ordering::Relaxed));
            COL.store(c + 1, Ordering::Relaxed);
        }
        return;
    }
    match b {
        b'\n' => newline_locked(),
        b'\r' => COL.store(0, Ordering::Relaxed),
        _ => {
            let (cols, _) = grid(SCALE.load(Ordering::Relaxed));
            if COL.load(Ordering::Relaxed) >= cols { newline_locked(); }
            let c = COL.load(Ordering::Relaxed);
            draw(ROW.load(Ordering::Relaxed), c, if b < 0x20 { b'.' } else { b }, FG.load(Ordering::Relaxed));
            COL.store(c + 1, Ordering::Relaxed);
        }
    }
}

/// Draw one byte of text (no-op unless `text_on`).
pub fn putc(b: u8) {
    if !text_on() { return; }
    let held = lock();
    putc_locked(b);
    unlock(held);
}

pub fn puts(s: &str) {
    if !text_on() { return; }
    let held = lock();
    for &b in s.as_bytes() { putc_locked(b); }
    unlock(held);
}

/// Hex with at least `digits` digits (no prefix).
pub fn put_hex(v: u64, digits: usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 16];
    let mut n = 0;
    let mut x = v;
    loop {
        buf[n] = HEX[(x & 0xF) as usize];
        n += 1;
        x >>= 4;
        if (x == 0 && n >= digits.min(16)) || n == 16 { break; }
    }
    for i in (0..n).rev() { putc(buf[i]); }
}

pub fn put_dec(v: u64) {
    let mut buf = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    loop { buf[n] = b'0' + (x % 10) as u8; n += 1; x /= 10; if x == 0 { break; } }
    for i in (0..n).rev() { putc(buf[i]); }
}

/// Finish the current line if it has text on it.
pub fn line_break() {
    if !text_on() { return; }
    let held = lock();
    if COL.load(Ordering::Relaxed) != 0 { newline_locked(); }
    unlock(held);
}

/// Lay `n` items out as a column-major table of cells `item_w` columns wide
/// starting at the current line, calling `f(i)` with the cursor at cell `i`
/// (text it draws is clipped to the cell). Rows down to the table's end
/// become fixed: the wrapping log continues below it. Items that do not fit
/// (leaving `keep_rows` rows for the log) are left out; returns how many were
/// drawn.
pub fn table(n: usize, item_w: usize, keep_rows: usize, mut f: impl FnMut(usize)) -> usize {
    if !text_on() || n == 0 { return 0; }
    line_break();
    let (cols, rows) = grid(SCALE.load(Ordering::Relaxed));
    let start = ROW.load(Ordering::Relaxed);
    let per = (cols / item_w.max(1)).max(1);
    let avail = rows.saturating_sub(start + keep_rows).max(1);
    let nrows = n.div_ceil(per).min(avail);
    let shown = n.min(nrows * per);
    for r in start..start + nrows { clear_row(r); }
    for i in 0..shown {
        let (r, c) = (start + i % nrows, (i / nrows) * item_w);
        ROW.store(r, Ordering::Relaxed);
        COL.store(c, Ordering::Relaxed);
        CLIP_HI.store((c + item_w - 1).min(cols), Ordering::Relaxed);
        CLIP.store(true, Ordering::Relaxed);
        f(i);
        CLIP.store(false, Ordering::Relaxed);
    }
    let end = (start + nrows).min(rows.saturating_sub(1));
    ROW.store(end, Ordering::Relaxed);
    COL.store(0, Ordering::Relaxed);
    LOG_TOP.store(end, Ordering::Relaxed);
    clear_row(end);
    shown
}

/// A bar in the top-right corner, unmistakable on a photo: the early
/// fatal-error marker (red = panic, magenta = CPU exception).
pub fn fatal_bar(c: u32) {
    let w = WIDTH.load(Ordering::Relaxed) as usize;
    fill_px(w.saturating_sub(208), 8, 200, 20, c);
}

/// Start a fatal report: text on (it may not have been), the corner bar in
/// `bar`, red text. Returns false if nothing can be drawn.
pub fn fatal_begin(bar: u32) -> bool {
    if !armed() { return false; }
    if !TEXT.load(Ordering::Acquire) {
        let (c, _) = grid(2);
        text_enable(if c >= 80 { 2 } else { 1 }, false);
    }
    // The fatal text goes to the screen directly; the serial copy of the
    // same message must not be drawn a second time.
    TEE.store(false, Ordering::Release);
    fatal_bar(bar);
    line_break();
    set_color(RED);
    true
}

/// `core::fmt::Write` onto the early text area (for panic messages).
pub struct Writer;
impl core::fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result { puts(s); Ok(()) }
}

/// Stop this CPU for good (interrupts off). For `earlyfb.stop=` and fatal
/// early errors.
pub fn halt_forever() -> ! {
    loop {
        #[cfg(target_arch = "x86_64")]
        unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)); }
        #[cfg(target_arch = "aarch64")]
        unsafe { core::arch::asm!("msr daifset, #0xf; wfi", options(nomem, nostack)); }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        core::hint::spin_loop();
    }
}

static FONT: [u8; 128 * 8] = build_font();

/// 8x8 glyphs, bit 7 = leftmost pixel (the console's fallback font from
/// drivers/src/framebuffer.rs plus the punctuation a panic message needs).
const fn build_font() -> [u8; 128 * 8] {
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

    // Additions for early-boot text (panic messages, cmdline knobs).
    font[b'<' as usize * 8 + 2] = 0x06; font[b'<' as usize * 8 + 3] = 0x0c; font[b'<' as usize * 8 + 4] = 0x18; font[b'<' as usize * 8 + 5] = 0x0c; font[b'<' as usize * 8 + 6] = 0x06;
    font[b'|' as usize * 8 + 1] = 0x18; font[b'|' as usize * 8 + 2] = 0x18; font[b'|' as usize * 8 + 3] = 0x18; font[b'|' as usize * 8 + 4] = 0x18; font[b'|' as usize * 8 + 5] = 0x18; font[b'|' as usize * 8 + 6] = 0x18;
    font[b'\'' as usize * 8 + 1] = 0x18; font[b'\'' as usize * 8 + 2] = 0x18; font[b'\'' as usize * 8 + 3] = 0x30;
    font[b'"' as usize * 8 + 1] = 0x66; font[b'"' as usize * 8 + 2] = 0x66; font[b'"' as usize * 8 + 3] = 0x44;
    font[b';' as usize * 8 + 2] = 0x18; font[b';' as usize * 8 + 5] = 0x18; font[b';' as usize * 8 + 6] = 0x10;
    font[b'%' as usize * 8 + 1] = 0x62; font[b'%' as usize * 8 + 2] = 0x66; font[b'%' as usize * 8 + 3] = 0x0c; font[b'%' as usize * 8 + 4] = 0x18; font[b'%' as usize * 8 + 5] = 0x66; font[b'%' as usize * 8 + 6] = 0x46;
    font[b'{' as usize * 8 + 1] = 0x0e; font[b'{' as usize * 8 + 2] = 0x18; font[b'{' as usize * 8 + 3] = 0x70; font[b'{' as usize * 8 + 4] = 0x18; font[b'{' as usize * 8 + 5] = 0x18; font[b'{' as usize * 8 + 6] = 0x0e;
    font[b'}' as usize * 8 + 1] = 0x70; font[b'}' as usize * 8 + 2] = 0x18; font[b'}' as usize * 8 + 3] = 0x0e; font[b'}' as usize * 8 + 4] = 0x18; font[b'}' as usize * 8 + 5] = 0x18; font[b'}' as usize * 8 + 6] = 0x70;
    font[b'@' as usize * 8 + 1] = 0x3c; font[b'@' as usize * 8 + 2] = 0x66; font[b'@' as usize * 8 + 3] = 0x6e; font[b'@' as usize * 8 + 4] = 0x6e; font[b'@' as usize * 8 + 5] = 0x60; font[b'@' as usize * 8 + 6] = 0x3e;
    font[b'&' as usize * 8 + 1] = 0x38; font[b'&' as usize * 8 + 2] = 0x6c; font[b'&' as usize * 8 + 3] = 0x38; font[b'&' as usize * 8 + 4] = 0x6e; font[b'&' as usize * 8 + 5] = 0x6c; font[b'&' as usize * 8 + 6] = 0x3e;
    font[b'\\' as usize * 8 + 1] = 0xc0; font[b'\\' as usize * 8 + 2] = 0x60; font[b'\\' as usize * 8 + 3] = 0x30; font[b'\\' as usize * 8 + 4] = 0x18; font[b'\\' as usize * 8 + 5] = 0x0c; font[b'\\' as usize * 8 + 6] = 0x06;
    font[b'$' as usize * 8 + 1] = 0x18; font[b'$' as usize * 8 + 2] = 0x3e; font[b'$' as usize * 8 + 3] = 0x60; font[b'$' as usize * 8 + 4] = 0x3c; font[b'$' as usize * 8 + 5] = 0x06; font[b'$' as usize * 8 + 6] = 0x7c;
    font[b'~' as usize * 8 + 3] = 0x32; font[b'~' as usize * 8 + 4] = 0x4c;
    font[b'^' as usize * 8 + 1] = 0x18; font[b'^' as usize * 8 + 2] = 0x3c; font[b'^' as usize * 8 + 3] = 0x66;
    font[b'`' as usize * 8 + 1] = 0x30; font[b'`' as usize * 8 + 2] = 0x18;

    font
}
