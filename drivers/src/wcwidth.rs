//! Terminal cell width of a code point, wcwidth(3)-compatible, plus the tab
//! stop bitmap both console emulators share.
//!
//! This file is compiled twice: as `drivers::wcwidth` (the framebuffer
//! console) and, via `#[path]`, into `tty-server` (the per-VT text mirror).
//! The two emulators must agree on widths to the cell or a VT switch repaints
//! the screen differently from how it was first drawn. Core only.

/// Inclusive ranges that occupy two cells (East Asian Wide / Fullwidth and the
/// emoji presentation blocks). Sorted, non-overlapping.
const WIDE: &[(u32, u32)] = &[
    (0x1100, 0x115F), (0x231A, 0x231B), (0x2329, 0x232A), (0x23E9, 0x23EC),
    (0x23F0, 0x23F0), (0x23F3, 0x23F3), (0x25FD, 0x25FE), (0x2614, 0x2615),
    (0x2648, 0x2653), (0x267F, 0x267F), (0x2693, 0x2693), (0x26A1, 0x26A1),
    (0x26AA, 0x26AB), (0x26BD, 0x26BE), (0x26C4, 0x26C5), (0x26CE, 0x26CE),
    (0x26D4, 0x26D4), (0x26EA, 0x26EA), (0x26F2, 0x26F3), (0x26F5, 0x26F5),
    (0x26FA, 0x26FA), (0x26FD, 0x26FD), (0x2705, 0x2705), (0x270A, 0x270B),
    (0x2728, 0x2728), (0x274C, 0x274C), (0x274E, 0x274E), (0x2753, 0x2755),
    (0x2757, 0x2757), (0x2795, 0x2797), (0x27B0, 0x27B0), (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C), (0x2B50, 0x2B50), (0x2B55, 0x2B55), (0x2E80, 0x303E),
    (0x3041, 0x33FF), (0x3400, 0x4DBF), (0x4E00, 0xA4CF), (0xA960, 0xA97F),
    (0xAC00, 0xD7A3), (0xF900, 0xFAFF), (0xFE10, 0xFE19), (0xFE30, 0xFE6F),
    (0xFF00, 0xFF60), (0xFFE0, 0xFFE6), (0x1F300, 0x1F64F), (0x1F680, 0x1F6FF),
    (0x1F900, 0x1F9FF), (0x1FA70, 0x1FAFF), (0x20000, 0x2FFFD), (0x30000, 0x3FFFD),
];

/// Inclusive ranges that occupy no cell: format characters, variation
/// selectors and combining marks (there is no way to compose them here, so
/// they are dropped rather than given a cell of their own).
const ZERO: &[(u32, u32)] = &[
    (0x0300, 0x036F), (0x1AB0, 0x1AFF), (0x1DC0, 0x1DFF), (0x200B, 0x200F),
    (0x202A, 0x202E), (0x2060, 0x2064), (0x20D0, 0x20FF), (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F), (0xFEFF, 0xFEFF), (0xE0100, 0xE01EF),
];

fn in_table(t: &[(u32, u32)], u: u32) -> bool {
    let (mut lo, mut hi) = (0usize, t.len());
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (a, b) = t[mid];
        if u < a { hi = mid } else if u > b { lo = mid + 1 } else { return true }
    }
    false
}

/// Cells `u` occupies: 0, 1 or 2.
pub fn width(u: u32) -> usize {
    if u < 0x300 { return 1; }
    if in_table(ZERO, u) { 0 } else if in_table(WIDE, u) { 2 } else { 1 }
}

/// Tab stops for up to 256 columns, one bit per column.
#[derive(Clone, Copy)]
pub struct TabStops(pub [u64; 4]);

impl TabStops {
    /// A stop every 8 columns (columns 8, 16, ...; column 0 is never a stop).
    pub const fn default_stops() -> Self {
        let mut w = [0u64; 4];
        let mut c = 8;
        while c < 256 { w[c / 64] |= 1 << (c % 64); c += 8; }
        TabStops(w)
    }
    pub fn set(&mut self, col: usize) { if col < 256 { self.0[col / 64] |= 1 << (col % 64); } }
    pub fn clear(&mut self, col: usize) { if col < 256 { self.0[col / 64] &= !(1 << (col % 64)); } }
    pub fn clear_all(&mut self) { self.0 = [0; 4]; }
    fn is_set(&self, col: usize) -> bool { col < 256 && self.0[col / 64] >> (col % 64) & 1 != 0 }
    /// The column `n` stops to the right of `col`, clamped to `cols - 1`.
    pub fn next(&self, col: usize, n: usize, cols: usize) -> usize {
        let mut c = col;
        for _ in 0..n.max(1) {
            c += 1;
            while c < cols - 1 && !self.is_set(c) { c += 1; }
            if c >= cols - 1 { return cols - 1; }
        }
        c
    }
    /// The column `n` stops to the left of `col`, clamped to 0.
    pub fn prev(&self, col: usize, n: usize) -> usize {
        let mut c = col;
        for _ in 0..n.max(1) {
            if c == 0 { return 0; }
            c -= 1;
            while c > 0 && !self.is_set(c) { c -= 1; }
        }
        c
    }
}
