//! Virtual terminals — `tty1`..`tty6` sharing one framebuffer console.
//!
//! # What a VT is here
//!
//! The framebuffer console and the DRM scanout are the same surface (see the
//! "Scanout ownership" block in `drivers/src/framebuffer.rs`), and that file
//! already answers "who owns the display" for N=1 by claiming it from the
//! *present itself* rather than from an ioctl allow-list. A VT is that same
//! question for N=6: which console owns the surface right now.
//!
//! What N>1 adds is **memory**. With one console the surface is the state — a
//! yielded console has nothing to come back to, which is why reclaiming it used
//! to clear the screen and print a banner. With six, each console needs its own
//! text so a switch can put it back, so this module keeps a character-cell
//! mirror of every VT and repaints the target on switch. The framebuffer keeps
//! doing all the drawing; this module only decides *what* is on screen and
//! remembers what was.
//!
//! # Why the mirror is a mirror and not the renderer
//!
//! `Framebuffer::putc` is the tree's only VT emulator and it draws straight to
//! pixels — it has no cell memory to copy. Rather than move that parser here
//! (a rewrite of the one thing on the console path that is known to work), this
//! module runs a *second* pass of the same rules over the same byte stream and
//! stores the result. [`Screen::putc`] is therefore a deliberate transcription
//! of `Framebuffer::putc` + `handle_csi`; the two must move together, and the
//! scroll geometry is read back from the framebuffer at runtime
//! (`fb_vt_scroll_rows`) rather than duplicated as a constant, because that one
//! is a silent-divergence hazard: a mismatched scroll step desyncs the cursor
//! and every later repaint lands text on the wrong row.
//!
//! # Locks and contexts
//!
//! [`console_out`] runs wherever the kernel prints, which includes IRQ context
//! (the framebuffer console is driven from the timer IRQ drain). It therefore
//! never blocks and never calls into the `drivers` crate: it `try_lock`s
//! [`SCREENS`] and counts a drop if it loses, and it reads the console geometry
//! from a cache that only task context refreshes. [`switch_request`] is the
//! same story one step further — it is called from the input drain, so it is a
//! single atomic store and the real work happens later in [`poll_deferred`].
//!
//! # Storage
//!
//! Every static here is a zero image so the ~865 KB of cell buffers land in
//! `.bss`. A single non-zero byte anywhere in the aggregate makes LLVM emit the
//! whole thing as an explicit initialiser in `.data`, which is exactly the cost
//! TODO.md item 15 recorded. Two fields would otherwise have non-zero defaults
//! and are encoded around it: foreground colour is stored XOR-inverted so the
//! default white is zero (see [`FG_XOR`]), and the keyboard mode is stored
//! biased by one so zero means "never set — `K_XLATE`".

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

// ── Geometry ──────────────────────────────────────────────────────────────────

/// Number of virtual consoles: `/dev/tty1` .. `/dev/tty6`.
pub const VT_COUNT: usize = 6;

/// Cell-grid bound. The framebuffer console is 8x16 cells, so 1920x1080 is
/// 240x67 and 1280x800 is 160x50 (`fb_console_size`); this leaves headroom for
/// a wider mode without letting the table grow without bound. Text outside the
/// bound is dropped by the mirror, not by the console — a repaint of an
/// over-wide console loses the right-hand columns rather than corrupting.
pub const VT_COLS: usize = 256;
/// Cell-grid bound, rows. See [`VT_COLS`].
pub const VT_ROWS: usize = 72;

/// Geometry assumed until task context reads the real console size. Matches the
/// `kernel_console_winsize` fallback, so a mirror built before the framebuffer
/// exists agrees with what programs were told.
const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;

/// Foreground colours are stored XOR'd with this so the console default
/// (0xFFFFFF, white) encodes as zero and the cell table is a zero image.
/// A plain "0 means default" sentinel would have made `SGR 30` (black) and
/// "unset" the same value; XOR is a bijection, so every colour round-trips.
const FG_XOR: u32 = 0x00FF_FFFF;

/// Blank cell: no glyph, default colour. Zero, which is the whole point.
const BLANK: u64 = 0;

#[inline]
fn cell(ch: u32, fg_enc: u32) -> u64 { ((fg_enc as u64) << 32) | ch as u64 }

// ── Framebuffer bridge ────────────────────────────────────────────────────────
//
// The tty crate depends on `ipc`, `sched` and `spin` only — deliberately, and
// adding `drivers` would invert the dependency (the framebuffer console is a
// driver, the VT layer is policy above it). The two halves talk through
// `#[no_mangle]` symbols, which is the same seam `kernel_set_console_enabled`
// and `kernel_console_winsize` already use for exactly this reason.

extern "C" {
    /// Clear the surface and reset the console cursor. Starts a repaint; does
    /// not flush.
    fn fb_vt_repaint_begin();
    /// Draw one row of `len` packed cells (`(fg_enc << 32) | ch`) at text row
    /// `row`. Blank cells are skipped — `fb_vt_repaint_begin` already blanked.
    fn fb_vt_paint_row(row: u32, cells: *const u64, len: u32);
    /// Park the console cursor at `(col, row)`, restore the SGR foreground and
    /// reset the escape parser, then flush the whole repaint in one transfer.
    fn fb_vt_repaint_end(col: u32, row: u32, fg: u32);
    /// Enable or disable framebuffer console output. No clear, no banner.
    fn fb_vt_console_gate(enabled: bool);
    /// True while a DRM open holds the scanout.
    fn fb_vt_scanout_owned() -> bool;
    /// Drop any DRM scanout claim, unconditionally.
    fn fb_vt_scanout_revoke();
    /// Text rows the console advances per scroll (`SCROLL_ROWS`).
    fn fb_vt_scroll_rows() -> u32;
    /// Console size in character cells, or the 80x24 fallback.
    fn kernel_console_winsize(rows: *mut u16, cols: *mut u16);
    /// VT `n` (1-based) is now on screen: force `SYN_DROPPED` onto every evdev
    /// queue pinned to it, so a client that was gated off resynchronises
    /// instead of replaying state from before the switch.
    ///
    /// The seam runs this way round because `evdev-server` already depends on
    /// `tty-server` for [`chord_key`]; naming it back directly would be a
    /// cycle.
    fn evdev_vt_activated(n: u32);
}

// ── Per-VT text mirror ────────────────────────────────────────────────────────

/// Escape-parser states, mirroring `EscState` in `drivers/src/framebuffer.rs`.
/// Numeric rather than an enum so the zero value (Ground) is the zero image.
const ESC_GROUND: u8 = 0;
const ESC_ESC: u8 = 1;
const ESC_CSI: u8 = 2;
const ESC_OSC: u8 = 3;
const ESC_OSC_ESC: u8 = 4;
const ESC_DISCARD1: u8 = 5;

/// One VT's text plane: what a repaint puts back on screen.
struct Screen {
    /// Row-major, stride [`VT_COLS`] regardless of the live console width, so
    /// a geometry change only invalidates content, never indexing.
    cells: [u64; VT_COLS * VT_ROWS],
    cur_col: u32,
    cur_row: u32,
    /// Current SGR foreground, XOR-encoded (see [`FG_XOR`]).
    fg_enc: u32,
    /// `ESC 7` / `CSI s` save slot.
    saved_col: u32,
    saved_row: u32,
    esc: u8,
    params_len: u8,
    params: [u8; 32],
    /// UTF-8 continuation bytes still expected, and the accumulator.
    utf8_left: u8,
    utf8_acc: u32,
}

impl Screen {
    const fn new() -> Self {
        Self {
            cells: [BLANK; VT_COLS * VT_ROWS],
            cur_col: 0,
            cur_row: 0,
            fg_enc: 0,
            saved_col: 0,
            saved_row: 0,
            esc: ESC_GROUND,
            params_len: 0,
            params: [0; 32],
            utf8_left: 0,
            utf8_acc: 0,
        }
    }

    fn blank(&mut self) {
        self.cells = [BLANK; VT_COLS * VT_ROWS];
        self.cur_col = 0;
        self.cur_row = 0;
        self.fg_enc = 0;
        self.esc = ESC_GROUND;
        self.params_len = 0;
        self.utf8_left = 0;
    }

    #[inline]
    fn put_cell(&mut self, col: usize, row: usize, ch: u32) {
        if col >= VT_COLS || row >= VT_ROWS { return; }
        let fg = self.fg_enc;
        self.cells[row * VT_COLS + col] = cell(ch, fg);
    }

    /// Blank `[c0, c1)` of `row`.
    fn blank_span(&mut self, row: usize, c0: usize, c1: usize) {
        if row >= VT_ROWS { return; }
        let c1 = c1.min(VT_COLS);
        let base = row * VT_COLS;
        for c in c0.min(c1)..c1 { self.cells[base + c] = BLANK; }
    }

    /// Mirror of `Framebuffer::scroll_px` in text terms: advance by
    /// `scroll_rows` rows at once, blanking what that exposes.
    ///
    /// The multi-row step is not an optimisation here — it is fidelity. The
    /// console really does jump `SCROLL_ROWS` rows per scroll (a full-surface
    /// copy costs the same whether it moves one row or eight, and that copy was
    /// the console's throughput ceiling), so a mirror that scrolled one row at a
    /// time would put the cursor eight rows off after the first overflow and
    /// every repaint afterwards would be wrong.
    fn scroll(&mut self, rows: usize, scroll_rows: usize) {
        let shift = scroll_rows.min(rows.saturating_sub(1)).max(1);
        if shift >= rows {
            self.blank();
            return;
        }
        let keep = rows - shift;
        self.cells.copy_within(shift * VT_COLS..(shift + keep) * VT_COLS, 0);
        for r in keep..rows { self.blank_span(r, 0, VT_COLS); }
        self.cur_row = self.cur_row.saturating_sub(shift as u32);
    }

    /// Feed one byte, mirroring `Framebuffer::putc`.
    fn putc(&mut self, c: u8, cols: usize, rows: usize, scroll_rows: usize) {
        if cols == 0 || rows == 0 { return; }

        if c == 0x1b {
            self.esc = if self.esc == ESC_OSC { ESC_OSC_ESC } else { ESC_ESC };
            self.params_len = 0;
            return;
        }

        match self.esc {
            ESC_GROUND => {}
            ESC_ESC => {
                self.esc = match c {
                    b'[' => { self.params_len = 0; ESC_CSI }
                    b']' => ESC_OSC,
                    b'(' | b')' | b'*' | b'+' | b'#' | b'%' => ESC_DISCARD1,
                    b'7' => { self.saved_col = self.cur_col; self.saved_row = self.cur_row; ESC_GROUND }
                    b'8' => { self.cur_col = self.saved_col; self.cur_row = self.saved_row; ESC_GROUND }
                    // ESC M — reverse index.
                    b'M' => { self.cur_row = self.cur_row.saturating_sub(1); ESC_GROUND }
                    _ => ESC_GROUND,
                };
                return;
            }
            ESC_CSI => {
                if (0x20..=0x3f).contains(&c) {
                    let n = self.params_len as usize;
                    if n < self.params.len() {
                        self.params[n] = c;
                        self.params_len += 1;
                    }
                } else if (0x40..=0x7e).contains(&c) {
                    let (params, len) = (self.params, self.params_len as usize);
                    self.handle_csi(&params[..len], c, cols, rows);
                    self.esc = ESC_GROUND;
                } else {
                    self.esc = ESC_GROUND;
                }
                return;
            }
            ESC_OSC => {
                if c == 0x07 { self.esc = ESC_GROUND; }
                return;
            }
            ESC_OSC_ESC => {
                self.esc = if c == b'\\' { ESC_GROUND } else { ESC_OSC };
                return;
            }
            _ => { self.esc = ESC_GROUND; return; }
        }

        if c == b'\n' {
            self.cur_col = 0;
            self.cur_row += 1;
        } else if c == b'\r' {
            self.cur_col = 0;
        } else if c == 0x08 {
            if self.cur_col > 0 {
                self.cur_col -= 1;
            } else if self.cur_row > 0 {
                self.cur_row -= 1;
                self.cur_col = (cols - 1) as u32;
            }
            let (col, row) = (self.cur_col as usize, self.cur_row as usize);
            self.put_cell(col, row, 0);
        } else {
            let ch = if c < 0x80 {
                self.utf8_left = 0;
                Some(c as u32)
            } else if c & 0xE0 == 0xC0 {
                self.utf8_left = 1; self.utf8_acc = (c & 0x1F) as u32; None
            } else if c & 0xF0 == 0xE0 {
                self.utf8_left = 2; self.utf8_acc = (c & 0x0F) as u32; None
            } else if c & 0xF8 == 0xF0 {
                self.utf8_left = 3; self.utf8_acc = (c & 0x07) as u32; None
            } else if c & 0xC0 == 0x80 && self.utf8_left > 0 {
                self.utf8_acc = (self.utf8_acc << 6) | (c & 0x3F) as u32;
                self.utf8_left -= 1;
                if self.utf8_left == 0 { Some(self.utf8_acc) } else { None }
            } else {
                self.utf8_left = 0;
                None
            };

            if let Some(ch) = ch {
                let (col, row) = (self.cur_col as usize, self.cur_row as usize);
                self.put_cell(col, row, ch);
                self.cur_col += 1;
                if self.cur_col as usize >= cols {
                    self.cur_col = 0;
                    self.cur_row += 1;
                }
            }
        }

        if self.cur_row as usize >= rows {
            self.scroll(rows, scroll_rows);
        }
    }

    /// Mirror of `Framebuffer::handle_csi`.
    fn handle_csi(&mut self, params: &[u8], final_byte: u8, cols: usize, rows: usize) {
        let mut nums = [0usize; 8];
        let mut present = [false; 8];
        let mut count = 0usize;
        let mut cur: Option<usize> = None;
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

        let (mut col, mut row) = (self.cur_col as usize, self.cur_row as usize);

        match final_byte {
            _ if private => {}
            b'A' => row = row.saturating_sub(p(0, 1)),
            b'B' => row = (row + p(0, 1)).min(rows - 1),
            b'C' => col = (col + p(0, 1)).min(cols - 1),
            b'D' => col = col.saturating_sub(p(0, 1)),
            b'E' => { row = (row + p(0, 1)).min(rows - 1); col = 0; }
            b'F' => { row = row.saturating_sub(p(0, 1)); col = 0; }
            b'G' | b'`' => col = p(0, 1).saturating_sub(1).min(cols - 1),
            b'd' => row = p(0, 1).saturating_sub(1).min(rows - 1),
            b'H' | b'f' => {
                row = p(0, 1).saturating_sub(1).min(rows - 1);
                col = p(1, 1).saturating_sub(1).min(cols - 1);
            }
            b'J' => self.erase_in_display(p(0, 0), cols, rows),
            b'K' => self.erase_in_line(p(0, 0), cols),
            b'm' => self.apply_sgr(&nums[..count], &present[..count]),
            b's' => { self.saved_col = self.cur_col; self.saved_row = self.cur_row; }
            b'u' => { self.cur_col = self.saved_col; self.cur_row = self.saved_row; }
            _ => {}
        }

        self.cur_col = col as u32;
        self.cur_row = row as u32;
    }

    fn erase_in_display(&mut self, mode: usize, cols: usize, rows: usize) {
        let (col, row) = (self.cur_col as usize, self.cur_row as usize);
        match mode {
            0 => {
                self.blank_span(row, col, cols);
                for r in row + 1..rows { self.blank_span(r, 0, cols); }
            }
            1 => {
                for r in 0..row.min(rows) { self.blank_span(r, 0, cols); }
                self.blank_span(row, 0, col);
            }
            _ => for r in 0..rows { self.blank_span(r, 0, cols); },
        }
    }

    fn erase_in_line(&mut self, mode: usize, cols: usize) {
        let (col, row) = (self.cur_col as usize, self.cur_row as usize);
        match mode {
            0 => self.blank_span(row, col, cols),
            1 => self.blank_span(row, 0, col),
            _ => self.blank_span(row, 0, cols),
        }
    }

    fn apply_sgr(&mut self, nums: &[usize], present: &[bool]) {
        const BASE: [u32; 8] = [
            0x000000, 0xCD0000, 0x00CD00, 0xCDCD00,
            0x0000EE, 0xCD00CD, 0x00CDCD, 0xE5E5E5,
        ];
        const BRIGHT: [u32; 8] = [
            0x7F7F7F, 0xFF0000, 0x00FF00, 0xFFFF00,
            0x5C5CFF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
        ];

        if nums.is_empty() || !present.first().copied().unwrap_or(false) {
            self.fg_enc = 0xFFFFFF ^ FG_XOR;
            if nums.len() <= 1 { return; }
        }

        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                0 | 39 => self.fg_enc = 0xFFFFFF ^ FG_XOR,
                30..=37 => self.fg_enc = BASE[nums[i] - 30] ^ FG_XOR,
                90..=97 => self.fg_enc = BRIGHT[nums[i] - 90] ^ FG_XOR,
                38 => {
                    match nums.get(i + 1) {
                        Some(5) => {
                            if let Some(&n) = nums.get(i + 2) {
                                self.fg_enc = xterm256(n, &BASE, &BRIGHT) ^ FG_XOR;
                            }
                            i += 2;
                        }
                        Some(2) => {
                            let r = nums.get(i + 2).copied().unwrap_or(0) as u32;
                            let g = nums.get(i + 3).copied().unwrap_or(0) as u32;
                            let b = nums.get(i + 4).copied().unwrap_or(0) as u32;
                            self.fg_enc = ((r << 16) | (g << 8) | b) ^ FG_XOR;
                            i += 4;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Transcribed from `xterm256` in `drivers/src/framebuffer.rs`; the mirror has
/// to resolve `SGR 38;5;n` to the same pixel value the console drew.
fn xterm256(n: usize, base: &[u32; 8], bright: &[u32; 8]) -> u32 {
    match n {
        0..=7 => base[n],
        8..=15 => bright[n - 8],
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

// ── Per-VT mode state ─────────────────────────────────────────────────────────

/// `KDSETMODE` values.
pub const KD_TEXT: u32 = 0x00;
pub const KD_GRAPHICS: u32 = 0x01;

/// `KDSKBMODE` values.
pub const K_RAW: u32 = 0x00;
pub const K_XLATE: u32 = 0x01;
pub const K_MEDIUMRAW: u32 = 0x02;
pub const K_UNICODE: u32 = 0x03;
pub const K_OFF: u32 = 0x04;

/// `struct vt_mode.mode` values.
const VT_AUTO: u8 = 0x00;
const VT_PROCESS: u8 = 0x01;
/// `VT_RELDISP` argument acknowledging an acquire.
const VT_ACKACQ: usize = 0x02;

/// Everything about a VT except its text.
struct VtState {
    /// Whether this console has ever been claimed. `VT_OPENQRY` hands out the
    /// first one that has not.
    allocated: bool,
    /// `KD_GRAPHICS` when a client has taken this VT for a DRM session. Stored
    /// as the non-default so the zero image means `KD_TEXT`.
    graphics: bool,
    /// Keyboard mode biased by one: 0 means "never set", which is `K_XLATE`.
    kb_mode_p1: u8,
    /// `KDSETLED` bitmask.
    leds: u8,
    /// `struct vt_mode.mode` — [`VT_AUTO`] or [`VT_PROCESS`].
    mode: u8,
    waitv: u8,
    relsig: u16,
    acqsig: u16,
    frsig: u16,
    /// Thread group that issued `VT_SETMODE(VT_PROCESS)`; 0 when unowned. The
    /// handshake signals go here, and a dead owner reverts the VT to
    /// [`VT_AUTO`] rather than wedging every future switch.
    owner: u32,
}

impl VtState {
    const fn new() -> Self {
        Self {
            allocated: false,
            graphics: false,
            kb_mode_p1: 0,
            leds: 0,
            mode: VT_AUTO,
            waitv: 0,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
            owner: 0,
        }
    }

    fn kb_mode(&self) -> u32 {
        if self.kb_mode_p1 == 0 { K_XLATE } else { (self.kb_mode_p1 - 1) as u32 }
    }
}

// ── Statics ───────────────────────────────────────────────────────────────────

static SCREENS: Mutex<[Screen; VT_COUNT]> =
    Mutex::new([const { Screen::new() }; VT_COUNT]);

static MODES: Mutex<[VtState; VT_COUNT]> =
    Mutex::new([const { VtState::new() }; VT_COUNT]);

/// Serialises whole switch transactions (request → signal → RELDISP →
/// complete). Task context only; nothing on the IRQ path takes it.
static SWITCH_LOCK: Mutex<()> = Mutex::new(());

/// Active console, **zero-based**, so the zero image is `tty1`. The public
/// [`active`] returns the 1-based number userspace uses.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Mirror of `MODES[active].graphics`, kept as an atomic so the console gate
/// and [`console_out`] never take a lock to consult it.
static ACTIVE_GRAPHICS: AtomicBool = AtomicBool::new(false);

/// Mirror of `MODES[active].kb_mode_p1`, for the same reason and with the same
/// bias-by-one encoding: the input drain runs in IRQ context and must read the
/// active VT's keyboard mode without touching [`MODES`], which task context
/// holds. Zero means "never set" — `K_XLATE` — which keeps this static, like
/// every other one here, part of the zero image.
static ACTIVE_KB_P1: AtomicU32 = AtomicU32::new(0);

/// Set once task context has read the real console geometry. Until then the
/// mirror runs at the 80x24 fallback and [`vt_console_reclaim`] declines, which
/// leaves the pre-VT reclaim behaviour in place during early boot.
static READY: AtomicBool = AtomicBool::new(false);

/// Console geometry cache: `(cols << 40) | (rows << 16) | scroll_rows`.
/// Zero means "not read yet". Refreshed only from task context — the hot path
/// must not call into the framebuffer, which would take `KERNEL_FB` from IRQ
/// context while a repaint on another CPU holds it.
static GRID: AtomicU64 = AtomicU64::new(0);

/// Deferred switch target (1-based), 0 = none. Written by [`switch_request`]
/// from the input drain, consumed by [`poll_deferred`].
static PENDING: AtomicUsize = AtomicUsize::new(0);

const PHASE_IDLE: u32 = 0;
const PHASE_WAIT_REL: u32 = 1;
const PHASE_WAIT_ACQ: u32 = 2;

static PHASE: AtomicU32 = AtomicU32::new(PHASE_IDLE);
/// Zero-based endpoints of the switch in flight.
static PHASE_FROM: AtomicUsize = AtomicUsize::new(0);
static PHASE_TO: AtomicUsize = AtomicUsize::new(0);
/// Tick the current phase was entered at, for the watchdog.
static PHASE_SINCE: AtomicU64 = AtomicU64::new(0);

/// Bytes [`console_out`] could not mirror because [`SCREENS`] was held.
/// The only holder is a switch repaint, so this should stay at or near zero;
/// a growing count means the repaint is holding the lock far longer than the
/// per-row snapshot it is written as.
static MIRROR_DROPS: AtomicU64 = AtomicU64::new(0);

/// Modifier keys currently held, as a bitmask — see [`chord_key`].
static CHORD_MODS: AtomicU32 = AtomicU32::new(0);

/// Ctrl+Alt+Esc was pressed and [`rescue`] is owed. Set from the input IRQ by
/// [`escape_key`], consumed by [`poll_deferred`] in task context, for exactly
/// the reason [`PENDING`] is.
static RESCUE: AtomicBool = AtomicBool::new(false);

/// A `VT_PROCESS` client that never answers must not own the display forever.
/// 5 s at the 100 Hz scheduler tick; long enough that no honest handshake trips
/// it, short enough that a wedged compositor is recoverable by hand — which is
/// the entire reason TODO.md item 14 was reopened.
const HANDSHAKE_TIMEOUT_TICKS: u64 = 500;

// ── Errors ────────────────────────────────────────────────────────────────────

const EPERM: isize = -1;
const EINTR: isize = -4;
const EFAULT: isize = -14;
const EBUSY: isize = -16;
const EINVAL: isize = -22;
const ENOTTY: isize = -25;

// ── Geometry cache ────────────────────────────────────────────────────────────

#[inline]
fn grid() -> (usize, usize, usize) {
    let g = GRID.load(Ordering::Relaxed);
    if g == 0 { return (DEFAULT_COLS, DEFAULT_ROWS, 1); }
    (
        ((g >> 40) & 0xFFFFFF) as usize,
        ((g >> 16) & 0xFFFFFF) as usize,
        (g & 0xFFFF) as usize,
    )
}

/// Read the live console geometry. Task context only.
///
/// A geometry change cannot be reflowed — the mirror stores cells, not the
/// byte stream that produced them — so every screen is blanked when it moves.
/// In practice this happens exactly once, when the framebuffer console replaces
/// the 80x24 fallback during boot.
fn refresh_grid() {
    let (mut r, mut c) = (0u16, 0u16);
    unsafe { kernel_console_winsize(&mut r as *mut u16, &mut c as *mut u16) };
    let cols = (c as usize).min(VT_COLS).max(1);
    let rows = (r as usize).min(VT_ROWS).max(1);
    let scroll = (unsafe { fb_vt_scroll_rows() } as usize).max(1);
    let packed = ((cols as u64) << 40) | ((rows as u64) << 16) | scroll as u64;
    let prev = GRID.swap(packed, Ordering::Relaxed);
    if prev != 0 && (prev >> 16) != (packed >> 16) {
        let mut s = SCREENS.lock();
        for scr in s.iter_mut() { scr.blank(); }
    }
}

/// Bring the VT layer up: read the console geometry and claim `tty1`.
///
/// Idempotent. Must run in task context after the framebuffer console is
/// initialised; before it, [`console_out`] mirrors at 80x24 and the reclaim
/// path falls back to the pre-VT behaviour.
pub fn init() {
    refresh_grid();
    MODES.lock()[0].allocated = true;
    READY.store(true, Ordering::Release);
}

// ── Queries ───────────────────────────────────────────────────────────────────

/// The console currently on screen, 1-based (`tty1`..`tty6`).
pub fn active() -> usize { ACTIVE.load(Ordering::Relaxed) + 1 }

/// True when the active VT is in `KD_TEXT`, i.e. the framebuffer console is
/// allowed to paint. Lock-free: the console gate consults this from contexts
/// that must not block.
pub fn is_text_console() -> bool { !ACTIVE_GRAPHICS.load(Ordering::Relaxed) }

/// Keyboard mode of the active VT (`K_XLATE` by default), readable from IRQ
/// context.
///
/// `_relaxed` is a contract, not decoration: this is the ONLY keyboard-mode
/// accessor, deliberately, because the input drain that needs it runs in IRQ
/// context and a `MODES`-taking twin sitting next to it under an inviting name
/// is a deadlock waiting for the next reader — task context holds that lock
/// across whole switch transactions. The value is mirrored into
/// [`ACTIVE_KB_P1`] wherever it can change, which is `KDSKBMODE` on the active
/// VT and every completed switch.
pub fn kb_mode_active_relaxed() -> u32 {
    let p1 = ACTIVE_KB_P1.load(Ordering::Relaxed);
    if p1 == 0 { K_XLATE } else { p1 - 1 }
}

/// True when console keystrokes belong to the kernel's line discipline.
///
/// Two relaxed atomic loads, so the input drain can gate on it. The answer is
/// no when a compositor has taken the active VT with `KD_GRAPHICS`, and no when
/// the VT's owner reads scancodes itself (`K_RAW`/`K_MEDIUMRAW`) or has asked
/// for nothing at all (`K_OFF`) — in every one of those cases a keystroke
/// belongs to that owner, and queueing a copy for the console is what makes a
/// shell replay everything typed into a full-screen client after it exits.
pub fn console_keyboard_active() -> bool {
    is_text_console() && kb_mode_active_relaxed() == K_XLATE
}

/// Console bytes lost from the mirror to lock contention. Diagnostic.
pub fn mirror_drops() -> u64 { MIRROR_DROPS.load(Ordering::Relaxed) }

// ── Console mirror ────────────────────────────────────────────────────────────

/// Record one console byte against the **active** VT.
///
/// Called from the kernel's byte-at-a-time console writer, which runs in IRQ
/// context as well as task context, so this must never block and never call
/// into `drivers`. It is deliberately placed *before* the console-enabled gate
/// at its call site: a VT that is not on screen still accumulates its text, and
/// that accumulation is the entire thing a switch back has to repaint.
///
/// On lock contention the byte is dropped from the mirror rather than waited
/// for; the only holder is a repaint, which is itself about to overwrite the
/// screen. [`mirror_drops`] counts these so "the console came back wrong" has a
/// number attached instead of a theory.
pub fn console_out(b: u8) {
    let (cols, rows, scroll) = grid();
    let idx = ACTIVE.load(Ordering::Relaxed);
    match SCREENS.try_lock() {
        Some(mut s) => s[idx].putc(b, cols, rows, scroll),
        None => { MIRROR_DROPS.fetch_add(1, Ordering::Relaxed); }
    }
}

// ── Repaint ───────────────────────────────────────────────────────────────────

/// Put VT `idx` (zero-based) back on screen.
///
/// [`SCREENS`] is taken and released once per text row rather than held across
/// the whole paint: a full 240x67 repaint is thousands of glyph draws, and
/// holding the lock for all of them would make every concurrent console byte a
/// [`mirror_drops`] entry. The snapshot buffer is one row — 2 KB — which keeps
/// the frame far below the 48 KiB the build enforces.
fn repaint(idx: usize) {
    let (cols, rows, _) = grid();
    unsafe { fb_vt_repaint_begin() };
    let mut row_buf = [0u64; VT_COLS];
    for r in 0..rows {
        {
            let s = SCREENS.lock();
            let base = r * VT_COLS;
            row_buf[..cols].copy_from_slice(&s[idx].cells[base..base + cols]);
        }
        unsafe { fb_vt_paint_row(r as u32, row_buf.as_ptr(), cols as u32) };
    }
    let (col, row, fg) = {
        let s = SCREENS.lock();
        (s[idx].cur_col, s[idx].cur_row, s[idx].fg_enc ^ FG_XOR)
    };
    unsafe { fb_vt_repaint_end(col, row, fg) };
}

/// Apply the console gate: the framebuffer console paints iff the active VT is
/// in `KD_TEXT` and no DRM open holds the scanout.
fn apply_gate() {
    let paint = is_text_console() && !unsafe { fb_vt_scanout_owned() };
    unsafe { fb_vt_console_gate(paint) };
}

/// The DRM scanout was released — hand the display back to the active VT.
///
/// Returns false when the VT layer is not up yet, which tells the framebuffer
/// to fall back to its pre-VT reclaim. Replaces that reclaim's clear-and-banner
/// once it is: a reclaim must show whatever text the console it is returning to
/// already had, and "[Console Resumed]" was the single-console assumption
/// written down — with one screen there was nothing to come back to, so
/// inventing a line was as good as anything.
#[no_mangle]
pub extern "C" fn vt_console_reclaim() -> bool {
    if !READY.load(Ordering::Acquire) { return false; }
    apply_gate();
    if is_text_console() { repaint(ACTIVE.load(Ordering::Relaxed)); }
    true
}

// ── Switching ─────────────────────────────────────────────────────────────────

/// Ask for a switch to VT `n` (1-based) from a context that cannot block.
///
/// One atomic store and nothing else. The input drain runs in IRQ context and
/// the framebuffer console runs from the timer IRQ on the same CPU, so a
/// switch that took `SCREENS` or `KERNEL_FB` here would deadlock against a
/// console write it interrupted — the same shape as the `arch::putc` wedge.
/// [`poll_deferred`] performs the switch from task context.
pub fn switch_request(n: usize) {
    if n >= 1 && n <= VT_COUNT { PENDING.store(n, Ordering::Relaxed); }
}

/// Service a deferred switch and the handshake watchdog. Task context.
///
/// Cheap enough for the syscall-return path: two relaxed atomic loads when
/// there is nothing to do.
pub fn poll_deferred() {
    if RESCUE.swap(false, Ordering::AcqRel) { rescue(); }
    if PHASE.load(Ordering::Relaxed) != PHASE_IDLE { watchdog(); }
    let n = PENDING.load(Ordering::Relaxed);
    if n == 0 { return; }
    if PENDING.compare_exchange(n, 0, Ordering::AcqRel, Ordering::Relaxed).is_err() { return; }
    if switch_to(n) == EBUSY {
        // A handshake was still outstanding. Re-arm rather than drop it: the
        // request came from a keypress, and silently losing the one chord a
        // user pressed to escape a wedged session is the failure this whole
        // item exists to prevent. The watchdog bounds how long it can bounce.
        let _ = PENDING.compare_exchange(0, n, Ordering::AcqRel, Ordering::Relaxed);
    }
}

/// Ctrl+Alt+Esc's deferred half: put **VT 1** back into a state a human can use
/// and move the display to it, asking nobody.
///
/// Every other path back from a graphical session is cooperative somewhere —
/// the release handshake asks the outgoing owner, [`chord_key`] asks the
/// keyboard mode, `KDSETMODE(KD_TEXT)` asks the client to issue it. This one is
/// the last resort, so it asks nothing:
///
/// * VT 1 is forced to `KD_TEXT` and `K_XLATE`, because releasing the keyboard
///   to a console that is still gated off behind someone else's `KD_GRAPHICS`
///   is a rescue nobody can see. This is the half [`crate::vt::escape_key`]'s
///   companion `release_all_grabs` cannot do.
/// * VT 1 is dropped to `VT_AUTO`, so a dead or wedged `VT_PROCESS` owner
///   cannot refuse the display.
/// * Any handshake in flight is abandoned rather than waited out — the whole
///   point is not to wait 5 s on the watchdog for a client that has already
///   proven it will not answer.
///
/// Only VT 1 is touched. A rescue that reset whichever VT happened to be on
/// screen would drop a *healthy* compositor out of graphics mode on its way
/// past, which is a rescue with collateral damage.
fn rescue() {
    let _guard = SWITCH_LOCK.lock();
    {
        let mut m = MODES.lock();
        let v = &mut m[0];
        v.allocated = true;
        v.graphics = false;
        v.kb_mode_p1 = (K_XLATE + 1) as u8;
        v.mode = VT_AUTO;
        v.owner = 0;
    }
    PHASE.store(PHASE_IDLE, Ordering::Relaxed);
    PHASE_FROM.store(ACTIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    PHASE_TO.store(0, Ordering::Relaxed);
    // Straight to `complete_switch`, not `switch_to`: the release handshake is
    // the thing being bypassed. It repaints VT 1, revokes the scanout claim and
    // — through `evdev_vt_activated` — releases every input grab a second time.
    complete_switch(0);
}

/// Force a stalled `VT_PROCESS` handshake through.
///
/// Linux has no timeout here; it drops a VT to `VT_AUTO` only when the signal
/// cannot be delivered at all. That is not enough for us, because the reason
/// this item exists is "switch away from a wedged compositor" — a compositor
/// that is alive enough to hold its signal handler off is exactly the case a
/// delivery check passes and a human still cannot get their screen back.
fn watchdog() {
    let since = PHASE_SINCE.load(Ordering::Relaxed);
    if sched::ticks().saturating_sub(since) < HANDSHAKE_TIMEOUT_TICKS { return; }
    let _guard = SWITCH_LOCK.lock();
    let phase = PHASE.load(Ordering::Relaxed);
    if phase == PHASE_IDLE { return; }
    let from = PHASE_FROM.load(Ordering::Relaxed);
    let to = PHASE_TO.load(Ordering::Relaxed);
    // A VT whose owner would not answer loses VT_PROCESS: leaving it set would
    // stall the next switch the same way, forever.
    let stuck = if phase == PHASE_WAIT_REL { from } else { to };
    {
        let mut m = MODES.lock();
        m[stuck].mode = VT_AUTO;
        m[stuck].owner = 0;
    }
    if phase == PHASE_WAIT_REL {
        complete_switch(to);
    } else {
        PHASE.store(PHASE_IDLE, Ordering::Relaxed);
    }
}

/// Switch to VT `n` (1-based), running the full handshake.
///
/// Returns 0 when the switch has happened or has been started; a `VT_PROCESS`
/// console makes it asynchronous, exactly as `VT_ACTIVATE` is on Linux, and
/// `VT_WAITACTIVE` is how a caller finds out it finished.
pub fn switch_to(n: usize) -> isize {
    if n < 1 || n > VT_COUNT { return EINVAL; }
    let to = n - 1;
    let _guard = SWITCH_LOCK.lock();

    let from = ACTIVE.load(Ordering::Relaxed);
    if to == from && PHASE.load(Ordering::Relaxed) == PHASE_IDLE { return 0; }

    // One switch at a time. A second request while a handshake is outstanding
    // would leave two owners believing they had been asked to release.
    if PHASE.load(Ordering::Relaxed) != PHASE_IDLE {
        return if PHASE_TO.load(Ordering::Relaxed) == to { 0 } else { EBUSY };
    }

    // Does the outgoing console want to be asked first?
    let (mode, relsig, owner) = {
        let m = MODES.lock();
        (m[from].mode, m[from].relsig, m[from].owner)
    };
    if mode == VT_PROCESS && owner != 0 && sched::exists_probe(owner) >= 0 && relsig != 0 {
        PHASE_FROM.store(from, Ordering::Relaxed);
        PHASE_TO.store(to, Ordering::Relaxed);
        PHASE_SINCE.store(sched::ticks(), Ordering::Relaxed);
        PHASE.store(PHASE_WAIT_REL, Ordering::Relaxed);
        // `MODES` was read in the scoped block above and is already released:
        // `deliver_signal_process` takes RUN_QUEUE, and no server lock may be
        // held across that.
        // SI_KERNEL: a VT switch has no originating process to name.
        sched::deliver_signal_process(owner, relsig as u32, sched::SigInfo::KERNEL);
        return 0;
    }
    if mode == VT_PROCESS {
        // The owner is gone (or never gave a release signal). Linux reverts to
        // VT_AUTO in exactly this case rather than blocking on a corpse.
        let mut m = MODES.lock();
        m[from].mode = VT_AUTO;
        m[from].owner = 0;
    }

    PHASE_FROM.store(from, Ordering::Relaxed);
    PHASE_TO.store(to, Ordering::Relaxed);
    complete_switch(to);
    0
}

/// Perform the display handoff to VT `to` (zero-based) and, if that console
/// runs `VT_PROCESS`, ask it to acknowledge.
///
/// Called with [`SWITCH_LOCK`] held.
fn complete_switch(to: usize) {
    let (graphics, kb_p1, mode, acqsig, owner) = {
        let mut m = MODES.lock();
        m[to].allocated = true;
        (m[to].graphics, m[to].kb_mode_p1, m[to].mode, m[to].acqsig, m[to].owner)
    };

    ACTIVE.store(to, Ordering::Relaxed);
    ACTIVE_GRAPHICS.store(graphics, Ordering::Relaxed);
    ACTIVE_KB_P1.store(kb_p1 as u32, Ordering::Relaxed);

    // Input follows the display. AFTER the store, never before: the evdev gate
    // reads `active()` on the push path, so a resync issued while the old VT
    // was still published would be immediately followed by events the gate
    // still routed to the outgoing console.
    unsafe { evdev_vt_activated((to + 1) as u32) };

    if graphics {
        // Silence the console without touching a pixel. The outgoing VT's text
        // is in its mirror, and whatever the incoming session last drew is
        // still on the surface — clearing here would blank a live compositor
        // that is about to be handed the display back.
        unsafe { fb_vt_console_gate(false) };
    } else {
        // Taking the display back from a graphical session is the whole point
        // of the item: revoke the scanout claim through the same ownership
        // word the present path sets, so the console is not merely drawing
        // over a client that still believes it owns the surface. The client's
        // next present re-claims it — which is correct, because by then the
        // user has switched back to it.
        unsafe { fb_vt_scanout_revoke() };
        apply_gate();
        repaint(to);
    }

    if mode == VT_PROCESS && owner != 0 && sched::exists_probe(owner) >= 0 && acqsig != 0 {
        PHASE_SINCE.store(sched::ticks(), Ordering::Relaxed);
        PHASE.store(PHASE_WAIT_ACQ, Ordering::Relaxed);
        // SI_KERNEL, as for the release signal above.
        sched::deliver_signal_process(owner, acqsig as u32, sched::SigInfo::KERNEL);
    } else {
        PHASE.store(PHASE_IDLE, Ordering::Relaxed);
    }

    // Wake VT_WAITACTIVE sleepers; they park on the poll wait-channel rather
    // than yield-spinning, so nothing else brings them back promptly.
    sched::wake_poll();
}

/// `VT_RELDISP` — the client's half of the handshake.
///
/// `arg` 0 refuses a release, 1 grants it, [`VT_ACKACQ`] acknowledges an
/// acquire.
fn reldisp(idx: usize, arg: usize) -> isize {
    let _guard = SWITCH_LOCK.lock();
    let phase = PHASE.load(Ordering::Relaxed);
    if phase == PHASE_IDLE { return EINVAL; }

    if phase == PHASE_WAIT_REL {
        if idx != PHASE_FROM.load(Ordering::Relaxed) { return EINVAL; }
        if arg == 0 {
            // The owner refused. Abandon the switch; the display does not move.
            PHASE.store(PHASE_IDLE, Ordering::Relaxed);
            sched::wake_poll();
            return 0;
        }
        complete_switch(PHASE_TO.load(Ordering::Relaxed));
        return 0;
    }

    // PHASE_WAIT_ACQ — only VT_ACKACQ closes it.
    if idx != PHASE_TO.load(Ordering::Relaxed) { return EINVAL; }
    if arg != VT_ACKACQ { return EINVAL; }
    PHASE.store(PHASE_IDLE, Ordering::Relaxed);
    sched::wake_poll();
    0
}

/// `VT_WAITACTIVE` — block until VT `n` (1-based) is the one on screen.
///
/// Waits on the console being active, not on the acquire acknowledgement: a
/// `VT_PROCESS` client that never sends `VT_RELDISP(VT_ACKACQ)` would otherwise
/// hold every waiter on the system, and the display has genuinely moved by then.
/// The outstanding ack blocks the *next* switch (and the watchdog clears it),
/// which is the part that actually needs the client's consent.
fn wait_active(n: usize) -> isize {
    if n < 1 || n > VT_COUNT { return EINVAL; }
    let want = n - 1;
    loop {
        poll_deferred();
        if ACTIVE.load(Ordering::Relaxed) == want { return 0; }
        if sched::has_deliverable_signal() { return EINTR; }
        // Park rather than yield-spin: a VT_WAITACTIVE'd session manager can
        // sit here for the whole life of another session, and a yield loop
        // there pins a CPU at 100% (the same defect sys_wait4 was fixed for).
        // The 20 ms deadline bounds any missed wake edge.
        sched::block_on_poll_prepare_until(sched::ticks() + 2);
        if ACTIVE.load(Ordering::Relaxed) == want {
            sched::block_on_poll_cancel();
            return 0;
        }
        sched::block_on_poll_commit();
    }
}

// ── Ctrl+Alt+Fn ───────────────────────────────────────────────────────────────

const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_F1: u16 = 59;
const KEY_ESC: u16 = 1;

const MOD_CTRL: u32 = 1 << 0;
const MOD_ALT: u32 = 1 << 1;

/// Feed one evdev `EV_KEY` event to the chord recogniser.
///
/// Returns true when the event was consumed as a VT switch, so the caller can
/// keep it out of every client's input ring — otherwise the foreground
/// application also sees the F-key that moved the display out from under it.
///
/// Lock-free by construction: this runs on the input IRQ drain path. All it
/// does on a hit is [`switch_request`], which is one more atomic store, and the
/// keyboard-mode gate reads [`ACTIVE_KB_P1`] rather than `MODES` for the same
/// reason — see [`kb_mode_active_relaxed`].
///
/// # Three deliberate divergences from Linux
///
/// **1. Linux does not hide the chord from evdev clients; we do.** Its VT
/// switch runs in a separate input handler that sits alongside the evdev one
/// rather than in front of it, so an X server or libinput client sees the F-key
/// as well. Swallowing it is stricter, and defensible — the application that
/// just lost the display has no use for the keystroke that took it — but it is
/// a divergence, not a reimplementation, and anything that compares our event
/// counts against a Linux box will see it.
///
/// **2. Only the key-DOWN is swallowed, so clients see an unpaired release.**
/// That is a no-op for xkb, which tracks key state and ignores a release for a
/// key it never saw pressed. It is deliberate that the modifiers are NOT
/// swallowed: this returns false for Ctrl and Alt themselves, so both edges of
/// both keys reach every client and no client can be left holding a modifier
/// down. "Fixing" the unpaired release by also swallowing the modifiers would
/// manufacture exactly the stuck-modifier bug it looks like it prevents.
///
/// **3. `K_RAW`/`K_MEDIUMRAW` disable the chord entirely, as on Linux**, where
/// a raw-mode VT gets scancodes with no keysym translation and therefore no
/// switch. This costs us the "escape a wedged compositor" route for a client
/// that took the keyboard raw; `VT_ACTIVATE` from another process and the
/// release-handshake watchdog remain, and matching Linux is worth more than a
/// second escape hatch that no userspace would expect to exist.
pub fn chord_key(code: u16, value: i32) -> bool {
    let down = value != 0;
    let bit = match code {
        KEY_LEFTCTRL | KEY_RIGHTCTRL => MOD_CTRL,
        KEY_LEFTALT | KEY_RIGHTALT => MOD_ALT,
        _ => 0,
    };
    if bit != 0 {
        if down {
            CHORD_MODS.fetch_or(bit, Ordering::Relaxed);
        } else {
            CHORD_MODS.fetch_and(!bit, Ordering::Relaxed);
        }
        return false;
    }
    // Key-down only (value 1); autorepeat (2) would queue a switch per repeat.
    if value != 1 { return false; }
    if code < KEY_F1 || code >= KEY_F1 + VT_COUNT as u16 { return false; }
    if CHORD_MODS.load(Ordering::Relaxed) & (MOD_CTRL | MOD_ALT) != (MOD_CTRL | MOD_ALT) {
        return false;
    }
    // The active VT's owner does its own switching when it reads the keyboard
    // itself. Divergence 3 above.
    if kb_mode_active_relaxed() != K_XLATE { return false; }
    switch_request((code - KEY_F1) as usize + 1);
    true
}

/// **Ctrl+Alt+Esc — the escape hatch.** Returns true when the event is that
/// chord's key-down, which the caller must consume.
///
/// WHY THIS EXISTS AND WHY IT IS NOT [`chord_key`]. `EVIOCGRAB` is enforced now
/// (TODO.md item 20), and enforcing it is the one change in this tree whose
/// failure mode is a machine nobody can ask anything. The structural argument
/// that Ctrl+Alt+Fn is safe — it runs ahead of all per-client routing, so no
/// grab can swallow it — is an argument about the code as it stands today, and
/// the thing being defended against is a change nobody has made yet.
///
/// Ctrl+Alt+Fn is also not sufficient on its own, and the gap is exact rather
/// than hypothetical: [`chord_key`] deliberately disables itself outside
/// `K_XLATE`, matching Linux, so a client that sets `K_RAW` **and** grabs the
/// keyboard has closed the only keyboard route back. That client is trivial to
/// write by accident. This one carries **no keyboard-mode gate at all** — it is
/// the last resort, and a last resort with a precondition is not one.
///
/// It does two things, and needs both: release every evdev grab (the caller's
/// half, in `evdev-server`, immediately and on this IRQ) and put VT 1 back into
/// a usable state and move the display to it ([`rescue`], deferred to task
/// context because it repaints). Ungrabbing alone hands the keyboard back to a
/// console that is still gated off behind the wedged client's `KD_GRAPHICS`,
/// which is a rescue you cannot see.
///
/// Lock-free, IRQ-safe, and it reads [`CHORD_MODS`] rather than tracking its
/// own modifiers — [`chord_key`] runs first on every key and maintains that
/// state, so the two recognisers cannot disagree about whether Ctrl is down.
/// Only the key-down is consumed, for the reason divergence 2 gives on
/// [`chord_key`].
pub fn escape_key(code: u16, value: i32) -> bool {
    if value != 1 || code != KEY_ESC { return false; }
    if CHORD_MODS.load(Ordering::Relaxed) & (MOD_CTRL | MOD_ALT) != (MOD_CTRL | MOD_ALT) {
        return false;
    }
    RESCUE.store(true, Ordering::Relaxed);
    true
}

// ── Process teardown ──────────────────────────────────────────────────────────

/// A process exited — release any VT it held in `VT_PROCESS`.
///
/// Without this, a compositor that crashes mid-session leaves its VT waiting
/// for a release signal nobody will ever answer, and the watchdog is the only
/// way back. The watchdog is the backstop for a *live* process that will not
/// answer; a dead one should cost nothing.
pub fn cleanup_pid(pid: u32) {
    let mut touched = false;
    {
        let mut m = MODES.lock();
        for v in m.iter_mut() {
            if v.owner == pid {
                v.owner = 0;
                v.mode = VT_AUTO;
                touched = true;
            }
        }
    }
    if !touched { return; }
    let _guard = SWITCH_LOCK.lock();
    match PHASE.load(Ordering::Relaxed) {
        PHASE_WAIT_REL => complete_switch(PHASE_TO.load(Ordering::Relaxed)),
        PHASE_WAIT_ACQ => {
            PHASE.store(PHASE_IDLE, Ordering::Relaxed);
            sched::wake_poll();
        }
        _ => {}
    }
}

/// Give up VT `n` (1-based). `n == 0` frees every VT that is not on screen.
fn disallocate(n: usize) -> isize {
    let active = ACTIVE.load(Ordering::Relaxed);
    if n == 0 {
        let mut m = MODES.lock();
        let mut s = SCREENS.lock();
        for i in 0..VT_COUNT {
            if i == active || m[i].owner != 0 { continue; }
            m[i] = VtState::new();
            s[i].blank();
        }
        return 0;
    }
    if n > VT_COUNT { return EINVAL; }
    let idx = n - 1;
    if idx == active { return EBUSY; }
    MODES.lock()[idx] = VtState::new();
    SCREENS.lock()[idx].blank();
    0
}

// ── ioctl ─────────────────────────────────────────────────────────────────────

// VT_* — <linux/vt.h>
const VT_OPENQRY: usize = 0x5600;
const VT_GETMODE: usize = 0x5601;
const VT_SETMODE: usize = 0x5602;
const VT_GETSTATE: usize = 0x5603;
const VT_RELDISP: usize = 0x5605;
const VT_ACTIVATE: usize = 0x5606;
const VT_WAITACTIVE: usize = 0x5607;
const VT_DISALLOCATE: usize = 0x5608;

// KD* — <linux/kd.h>
const KDGETLED: usize = 0x4B31;
const KDSETLED: usize = 0x4B32;
const KDGKBTYPE: usize = 0x4B33;
const KDSETMODE: usize = 0x4B3A;
const KDGETMODE: usize = 0x4B3B;
const KDGKBMODE: usize = 0x4B44;
const KDSKBMODE: usize = 0x4B45;

/// `KDGKBTYPE` — the only value Linux has reported for decades.
const KB_101: u8 = 0x02;

/// `struct vt_mode` is 8 bytes, not 6.
///
/// ```c
/// struct vt_mode { char mode; char waitv; short relsig, acqsig, frsig; };
/// ```
///
/// Two chars then three shorts, with the shorts 2-byte aligned: 1 + 1 + 2 + 2 +
/// 2 = 8. Counting the chars and shorts and stopping at 6 is the easy mistake,
/// and it is not a harmless one — a 6-byte copy drops `frsig` and, worse, a
/// 6-byte `VT_GETMODE` leaves two bytes of the caller's struct unwritten, so a
/// client that round-trips GETMODE→SETMODE feeds back stack garbage as its
/// acquire signal.
const VT_MODE_SIZE: usize = 8;
/// `struct vt_stat { unsigned short v_active, v_signal, v_state; }`.
const VT_STAT_SIZE: usize = 6;

/// Handle a VT/KD ioctl against VT `vt` (1-based; 0 means "the active one",
/// which is what `/dev/tty0` names).
///
/// # Safety
///
/// `arg` is a user pointer for the commands that take one. The caller must
/// have validated it against the calling address space; this mirrors the rest
/// of the tty server's ioctl surface, which reads and writes user structs
/// directly. Every such access here happens with no lock held, per the tree's
/// standing rule that user memory is never touched under a spinlock — a
/// demand-paging fault under one re-enters the scheduler and freezes every CPU.
pub unsafe fn ioctl(vt: usize, cmd: usize, arg: usize) -> isize {
    if !READY.load(Ordering::Acquire) { init(); }

    let idx = if vt == 0 {
        ACTIVE.load(Ordering::Relaxed)
    } else if vt <= VT_COUNT {
        vt - 1
    } else {
        return EINVAL;
    };

    match cmd {
        VT_OPENQRY => {
            if arg == 0 { return EFAULT; }
            let free = {
                let m = MODES.lock();
                (0..VT_COUNT).find(|&i| !m[i].allocated).map(|i| i as i32 + 1)
            };
            // Linux reports "none free" in the out-parameter as -1 and still
            // succeeds; callers test the value, not the return.
            core::ptr::write(arg as *mut i32, free.unwrap_or(-1));
            0
        }

        VT_GETMODE => {
            if arg == 0 { return EFAULT; }
            let mut buf = [0u8; VT_MODE_SIZE];
            {
                let m = MODES.lock();
                buf[0] = m[idx].mode;
                buf[1] = m[idx].waitv;
                buf[2..4].copy_from_slice(&m[idx].relsig.to_ne_bytes());
                buf[4..6].copy_from_slice(&m[idx].acqsig.to_ne_bytes());
                buf[6..8].copy_from_slice(&m[idx].frsig.to_ne_bytes());
            }
            core::ptr::copy_nonoverlapping(buf.as_ptr(), arg as *mut u8, VT_MODE_SIZE);
            0
        }

        VT_SETMODE => {
            if arg == 0 { return EFAULT; }
            let mut buf = [0u8; VT_MODE_SIZE];
            core::ptr::copy_nonoverlapping(arg as *const u8, buf.as_mut_ptr(), VT_MODE_SIZE);
            let mode = buf[0];
            if mode != VT_AUTO && mode != VT_PROCESS { return EINVAL; }
            let owner = sched::current_tgid();
            let mut m = MODES.lock();
            m[idx].mode = mode;
            m[idx].waitv = buf[1];
            m[idx].relsig = u16::from_ne_bytes([buf[2], buf[3]]);
            m[idx].acqsig = u16::from_ne_bytes([buf[4], buf[5]]);
            m[idx].frsig = u16::from_ne_bytes([buf[6], buf[7]]);
            m[idx].owner = if mode == VT_PROCESS { owner } else { 0 };
            m[idx].allocated = true;
            0
        }

        VT_GETSTATE => {
            if arg == 0 { return EFAULT; }
            let v_active = (ACTIVE.load(Ordering::Relaxed) + 1) as u16;
            // v_state is a bitmask of "in use" consoles. Linux always sets bit
            // 0 (there is no VT 0 to be in use, and clients test bits 1..N).
            let mut v_state: u16 = 1;
            {
                let m = MODES.lock();
                for i in 0..VT_COUNT {
                    if m[i].allocated { v_state |= 1 << (i + 1); }
                }
            }
            let mut buf = [0u8; VT_STAT_SIZE];
            buf[0..2].copy_from_slice(&v_active.to_ne_bytes());
            buf[2..4].copy_from_slice(&0u16.to_ne_bytes()); // v_signal — unused
            buf[4..6].copy_from_slice(&v_state.to_ne_bytes());
            core::ptr::copy_nonoverlapping(buf.as_ptr(), arg as *mut u8, VT_STAT_SIZE);
            0
        }

        VT_RELDISP => {
            let owner = { MODES.lock()[idx].owner };
            // Only the console's own VT_PROCESS owner may answer for it;
            // otherwise any process could complete somebody else's handshake.
            if owner != 0 && owner != sched::current_tgid() { return EPERM; }
            reldisp(idx, arg)
        }

        VT_ACTIVATE => switch_to(arg),
        VT_WAITACTIVE => wait_active(arg),
        VT_DISALLOCATE => disallocate(arg),

        KDGETMODE => {
            if arg == 0 { return EFAULT; }
            let g = { MODES.lock()[idx].graphics };
            core::ptr::write(arg as *mut i32, if g { KD_GRAPHICS as i32 } else { KD_TEXT as i32 });
            0
        }

        KDSETMODE => {
            let graphics = match arg as u32 {
                KD_TEXT => false,
                KD_GRAPHICS => true,
                _ => return EINVAL,
            };
            let changed = {
                let mut m = MODES.lock();
                let prev = m[idx].graphics;
                m[idx].graphics = graphics;
                m[idx].allocated = true;
                prev != graphics
            };
            // KD_GRAPHICS on the console you are looking at is how a compositor
            // says "stop drawing on me"; KD_TEXT is how it hands the screen
            // back on the way out. Both must take effect now, not at the next
            // switch — cosmic-comp sets KD_GRAPHICS before its first present,
            // so waiting would leave the console painting over its opening
            // frames, which is the exact failure the scanout ownership block
            // documents.
            if changed && idx == ACTIVE.load(Ordering::Relaxed) {
                ACTIVE_GRAPHICS.store(graphics, Ordering::Relaxed);
                if graphics {
                    fb_vt_console_gate(false);
                } else {
                    fb_vt_scanout_revoke();
                    apply_gate();
                    repaint(idx);
                }
            }
            0
        }

        KDGKBMODE => {
            if arg == 0 { return EFAULT; }
            let mode = { MODES.lock()[idx].kb_mode() };
            core::ptr::write(arg as *mut i32, mode as i32);
            0
        }

        KDSKBMODE => {
            let mode = arg as u32;
            if mode > K_OFF { return EINVAL; }
            MODES.lock()[idx].kb_mode_p1 = (mode + 1) as u8;
            // Publish to the IRQ-readable mirror on the same path that changes
            // it, not on the next switch: a client sets K_RAW immediately
            // before it starts reading, and a mode that lands one switch late
            // is a window in which its scancodes go to the line discipline.
            if idx == ACTIVE.load(Ordering::Relaxed) {
                ACTIVE_KB_P1.store(mode + 1, Ordering::Relaxed);
            }
            0
        }

        KDGKBTYPE => {
            if arg == 0 { return EFAULT; }
            core::ptr::write(arg as *mut u8, KB_101);
            0
        }

        KDGETLED => {
            if arg == 0 { return EFAULT; }
            let leds = { MODES.lock()[idx].leds };
            core::ptr::write(arg as *mut u8, leds);
            0
        }

        KDSETLED => {
            // Bit 7 is Linux's "go back to following the keyboard flags"
            // escape; we have no LED hardware either way, so the value is
            // simply remembered for KDGETLED.
            MODES.lock()[idx].leds = arg as u8;
            0
        }

        _ => ENOTTY,
    }
}

/// True for the commands [`ioctl`] answers, so the caller can route without
/// duplicating the table. Keeps the dispatch in syscall.rs to one predicate.
pub fn owns_ioctl(cmd: usize) -> bool {
    matches!(
        cmd,
        VT_OPENQRY | VT_GETMODE | VT_SETMODE | VT_GETSTATE | VT_RELDISP
            | VT_ACTIVATE | VT_WAITACTIVE | VT_DISALLOCATE
            | KDGETLED | KDSETLED | KDGKBTYPE | KDSETMODE | KDGETMODE
            | KDGKBMODE | KDSKBMODE
    )
}
