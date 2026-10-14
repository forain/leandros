//! 8x16 glyphs for the non-ASCII characters terminal UIs draw with.
//!
//! The console font covers ASCII only, so every other code point used to be
//! drawn as `?`. Full-screen terminal programs (the Claude Code CLI, htop,
//! tmux, dialog) build their frames out of box-drawing lines, block elements,
//! shades and a handful of symbols (selection chevrons, check marks, bullets,
//! arrows); a console that turns all of them into question marks renders
//! the layout but not the picture.
//!
//! Box drawing (U+2500..U+257F) and block elements (U+2580..U+259F) are
//! generated from their structure, so neighbouring cells join up: a line
//! runs through the centre of the cell to its edge, light one pixel wide,
//! heavy two, double as two parallel lines; a block fills its fraction of
//! the cell. The remaining symbols are hand-drawn bitmaps. [`fallback_ascii`]
//! gives an ASCII stand-in (`'` for a curly quote, `-` for a dash) to
//! characters with no glyph here.

/// Rows of an 8x16 glyph, bit 7 = leftmost pixel.
pub type Glyph = [u8; 16];

const W: usize = 8;
const H: usize = 16;
/// Centre column and row lines run through (light lines are one pixel wide).
const CX: usize = 3;
const CY: usize = 7;

/// Line weight of one arm: none, light, heavy, double.
#[derive(Clone, Copy, PartialEq)]
enum Wt { N, L, H, D }

fn set(g: &mut Glyph, x: usize, y: usize) {
    if x < W && y < H { g[y] |= 0x80 >> x; }
}

fn hline(g: &mut Glyph, y: usize, x0: usize, x1: usize) {
    for x in x0..=x1 { set(g, x, y); }
}

fn vline(g: &mut Glyph, x: usize, y0: usize, y1: usize) {
    for y in y0..=y1 { set(g, x, y); }
}

/// Draw the arms (up, right, down, left) of a box-drawing character.
fn arms(up: Wt, right: Wt, down: Wt, left: Wt) -> Glyph {
    let mut g = [0u8; 16];
    // Vertical arms: x positions by weight.
    let vx = |w: Wt| -> &'static [usize] {
        match w { Wt::N => &[], Wt::L => &[CX], Wt::H => &[CX, CX + 1], Wt::D => &[CX - 1, CX + 2] }
    };
    let hy = |w: Wt| -> &'static [usize] {
        match w { Wt::N => &[], Wt::L => &[CY], Wt::H => &[CY, CY + 1], Wt::D => &[CY - 1, CY + 2] }
    };
    // Each arm runs from the cell edge to the far side of the centre
    // band, so perpendicular arms of any weight meet without a gap.
    for &x in vx(up) { vline(&mut g, x, 0, CY + 2); }
    for &x in vx(down) { vline(&mut g, x, CY - 1, H - 1); }
    for &y in hy(left) { hline(&mut g, y, 0, CX + 2); }
    for &y in hy(right) { hline(&mut g, y, CX - 1, W - 1); }
    // A single straight line must not grow stubs from the centre band.
    if up != Wt::N && down != Wt::N && left == Wt::N && right == Wt::N {
        g = [0u8; 16];
        for &x in vx(up) { vline(&mut g, x, 0, H - 1); }
    }
    if left != Wt::N && right != Wt::N && up == Wt::N && down == Wt::N {
        g = [0u8; 16];
        for &y in hy(left) { hline(&mut g, y, 0, W - 1); }
    }
    g
}

/// A dashed straight line: `n` dashes across the cell.
fn dashed(horizontal: bool, heavy: bool, n: usize) -> Glyph {
    let mut g = [0u8; 16];
    let len = if horizontal { W } else { H };
    let seg = len / n;
    for i in 0..n {
        let a = i * seg;
        let b = a + seg - 1 - (seg / 3).max(1).min(seg - 1); // leave a gap
        for t in a..=b {
            if horizontal {
                set(&mut g, t, CY);
                if heavy { set(&mut g, t, CY + 1); }
            } else {
                set(&mut g, CX, t);
                if heavy { set(&mut g, CX + 1, t); }
            }
        }
    }
    g
}

fn box_drawing(u: u32) -> Option<Glyph> {
    use Wt::*;
    let (a, b, c, d) = match u {
        0x2500 => (N, L, N, L), 0x2501 => (N, H, N, H),
        0x2502 => (L, N, L, N), 0x2503 => (H, N, H, N),
        0x2504 => return Some(dashed(true, false, 3)), 0x2505 => return Some(dashed(true, true, 3)),
        0x2506 => return Some(dashed(false, false, 3)), 0x2507 => return Some(dashed(false, true, 3)),
        0x2508 => return Some(dashed(true, false, 4)), 0x2509 => return Some(dashed(true, true, 4)),
        0x250A => return Some(dashed(false, false, 4)), 0x250B => return Some(dashed(false, true, 4)),
        0x250C => (N, L, L, N), 0x250D => (N, H, L, N), 0x250E => (N, L, H, N), 0x250F => (N, H, H, N),
        0x2510 => (N, N, L, L), 0x2511 => (N, N, L, H), 0x2512 => (N, N, H, L), 0x2513 => (N, N, H, H),
        0x2514 => (L, L, N, N), 0x2515 => (L, H, N, N), 0x2516 => (H, L, N, N), 0x2517 => (H, H, N, N),
        0x2518 => (L, N, N, L), 0x2519 => (L, N, N, H), 0x251A => (H, N, N, L), 0x251B => (H, N, N, H),
        0x251C => (L, L, L, N), 0x251D => (L, H, L, N), 0x2520 => (H, L, H, N), 0x2523 => (H, H, H, N),
        0x2524 => (L, N, L, L), 0x2525 => (L, N, L, H), 0x2528 => (H, N, H, L), 0x252B => (H, N, H, H),
        0x252C => (N, L, L, L), 0x252F => (N, H, L, H), 0x2530 => (N, L, H, L), 0x2533 => (N, H, H, H),
        0x2534 => (L, L, N, L), 0x2537 => (L, H, N, H), 0x2538 => (H, L, N, L), 0x253B => (H, H, N, H),
        0x253C => (L, L, L, L), 0x253F => (L, H, L, H), 0x2542 => (H, L, H, L), 0x254B => (H, H, H, H),
        0x254C => return Some(dashed(true, false, 2)), 0x254D => return Some(dashed(true, true, 2)),
        0x254E => return Some(dashed(false, false, 2)), 0x254F => return Some(dashed(false, true, 2)),
        0x2550 => (N, D, N, D), 0x2551 => (D, N, D, N),
        0x2552 => (N, D, L, N), 0x2553 => (N, L, D, N), 0x2554 => (N, D, D, N),
        0x2555 => (N, N, L, D), 0x2556 => (N, N, D, L), 0x2557 => (N, N, D, D),
        0x2558 => (L, D, N, N), 0x2559 => (D, L, N, N), 0x255A => (D, D, N, N),
        0x255B => (L, N, N, D), 0x255C => (D, N, N, L), 0x255D => (D, N, N, D),
        0x255E => (L, D, L, N), 0x255F => (D, L, D, N), 0x2560 => (D, D, D, N),
        0x2561 => (L, N, L, D), 0x2562 => (D, N, D, L), 0x2563 => (D, N, D, D),
        0x2564 => (N, D, L, D), 0x2565 => (N, L, D, L), 0x2566 => (N, D, D, D),
        0x2567 => (L, D, N, D), 0x2568 => (D, L, N, L), 0x2569 => (D, D, N, D),
        0x256A => (L, D, L, D), 0x256B => (D, L, D, L), 0x256C => (D, D, D, D),
        // Rounded corners: drawn square at this size.
        0x256D => (N, L, L, N), 0x256E => (N, N, L, L), 0x256F => (L, N, N, L), 0x2570 => (L, L, N, N),
        0x2574 => (N, N, N, L), 0x2575 => (L, N, N, N), 0x2576 => (N, L, N, N), 0x2577 => (N, N, L, N),
        0x2578 => (N, N, N, H), 0x2579 => (H, N, N, N), 0x257A => (N, H, N, N), 0x257B => (N, N, H, N),
        0x257C => (N, H, N, L), 0x257D => (L, N, H, N), 0x257E => (N, L, N, H), 0x257F => (H, N, L, N),
        _ => return None,
    };
    Some(arms(a, b, c, d))
}

fn fill(x0: usize, y0: usize, x1: usize, y1: usize) -> Glyph {
    let mut g = [0u8; 16];
    for y in y0..y1 { for x in x0..x1 { set(&mut g, x, y); } }
    g
}

fn shade(on: impl Fn(usize, usize) -> bool) -> Glyph {
    let mut g = [0u8; 16];
    for y in 0..H { for x in 0..W { if on(x, y) { set(&mut g, x, y); } } }
    g
}

fn block_element(u: u32) -> Option<Glyph> {
    let (hw, hh) = (W / 2, H / 2);
    let quad = |ul: bool, ur: bool, ll: bool, lr: bool| {
        let mut g = [0u8; 16];
        for (on, x0, y0) in [(ul, 0, 0), (ur, hw, 0), (ll, 0, hh), (lr, hw, hh)] {
            if on { for y in y0..y0 + hh { for x in x0..x0 + hw { set(&mut g, x, y); } } }
        }
        g
    };
    Some(match u {
        0x2580 => fill(0, 0, W, hh),
        0x2581..=0x2587 => { let k = (u - 0x2580) as usize; fill(0, H - k * 2, W, H) }
        0x2588 => fill(0, 0, W, H),
        0x2589..=0x258F => { let k = (0x2590 - u) as usize; fill(0, 0, k, H) }
        0x2590 => fill(hw, 0, W, H),
        0x2591 => shade(|x, y| (x + 2 * (y % 2)) % 4 == 0),
        0x2592 => shade(|x, y| (x + y) % 2 == 0),
        0x2593 => shade(|x, y| (x + 2 * (y % 2)) % 4 != 0),
        0x2594 => fill(0, 0, W, 2),
        0x2595 => fill(W - 1, 0, W, H),
        0x2596 => quad(false, false, true, false),
        0x2597 => quad(false, false, false, true),
        0x2598 => quad(true, false, false, false),
        0x2599 => quad(true, false, true, true),
        0x259A => quad(true, false, false, true),
        0x259B => quad(true, true, true, false),
        0x259C => quad(true, true, false, true),
        0x259D => quad(false, true, false, false),
        0x259E => quad(false, true, true, false),
        0x259F => quad(false, true, true, true),
        _ => return None,
    })
}

/// A bitmap drawn rows `top..` of the cell.
const fn at(top: usize, rows: &[u8]) -> Glyph {
    let mut g = [0u8; 16];
    let mut i = 0;
    while i < rows.len() && top + i < 16 { g[top + i] = rows[i]; i += 1; }
    g
}

fn symbol(u: u32) -> Option<Glyph> {
    Some(match u {
        // ❯ heavy right angle (selection pointer), › single angle
        0x276F => at(3, &[0x60, 0x30, 0x18, 0x0C, 0x06, 0x0C, 0x18, 0x30, 0x60]),
        0x203A => at(5, &[0x20, 0x10, 0x08, 0x10, 0x20]),
        0x276E => at(3, &[0x06, 0x0C, 0x18, 0x30, 0x60, 0x30, 0x18, 0x0C, 0x06]),
        0x2039 => at(5, &[0x08, 0x10, 0x20, 0x10, 0x08]),
        // ✓ ✔ check marks
        0x2713 => at(4, &[0x01, 0x02, 0x02, 0x84, 0x44, 0x28, 0x10, 0x10]),
        0x2714 => at(4, &[0x03, 0x03, 0x06, 0xC6, 0x6C, 0x3C, 0x18, 0x18]),
        // ✗ ✘ × ballot / multiplication x
        0x2717 | 0x00D7 => at(5, &[0x42, 0x24, 0x18, 0x18, 0x24, 0x42]),
        0x2718 => at(4, &[0xC3, 0x66, 0x3C, 0x18, 0x3C, 0x66, 0xC3]),
        // · middle dot, • bullet, ∙ bullet operator
        0x00B7 | 0x2219 => at(7, &[0x18, 0x18]),
        0x2022 => at(6, &[0x3C, 0x7E, 0x7E, 0x3C]),
        // ● ⏺ black circle, ○ white circle, ◉ fisheye
        0x25CF | 0x23FA | 0x2B24 => at(4, &[0x3C, 0x7E, 0xFF, 0xFF, 0xFF, 0xFF, 0x7E, 0x3C]),
        0x25CB | 0x25EF => at(4, &[0x3C, 0x42, 0x81, 0x81, 0x81, 0x81, 0x42, 0x3C]),
        0x25C9 | 0x25CE => at(4, &[0x3C, 0x42, 0x99, 0xBD, 0xBD, 0x99, 0x42, 0x3C]),
        // ■ □ ▪ ▫ squares
        0x25A0 => at(4, &[0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE]),
        0x25A1 => at(4, &[0xFE, 0x82, 0x82, 0x82, 0x82, 0x82, 0xFE]),
        0x25AA => at(6, &[0x3C, 0x3C, 0x3C, 0x3C]),
        0x25AB => at(6, &[0x3C, 0x24, 0x24, 0x3C]),
        // ▲ ▶ ▼ ◀ and the small ▸ ▾ ◂ ▴
        0x25B2 => at(5, &[0x10, 0x38, 0x38, 0x7C, 0x7C, 0xFE]),
        0x25BC => at(5, &[0xFE, 0x7C, 0x7C, 0x38, 0x38, 0x10]),
        0x25B6 => at(3, &[0x80, 0xC0, 0xF0, 0xF8, 0xFE, 0xF8, 0xF0, 0xC0, 0x80]),
        0x25C0 => at(3, &[0x02, 0x06, 0x1E, 0x3E, 0xFE, 0x3E, 0x1E, 0x06, 0x02]),
        0x25B8 => at(5, &[0x40, 0x60, 0x70, 0x60, 0x40]),
        0x25C2 => at(5, &[0x08, 0x18, 0x38, 0x18, 0x08]),
        0x25B4 => at(6, &[0x10, 0x38, 0x7C]),
        0x25BE => at(6, &[0x7C, 0x38, 0x10]),
        // ← ↑ → ↓ arrows, ↵ ⏎ return
        0x2190 => at(5, &[0x10, 0x30, 0x7F, 0x30, 0x10]),
        0x2192 => at(5, &[0x08, 0x0C, 0xFE, 0x0C, 0x08]),
        0x2191 => at(3, &[0x10, 0x38, 0x7C, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10]),
        0x2193 => at(3, &[0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x7C, 0x38, 0x10]),
        0x21B5 | 0x23CE => at(4, &[0x02, 0x02, 0x02, 0x22, 0x62, 0xFE, 0x60, 0x20]),
        // … ellipsis
        0x2026 => at(12, &[0x49]),
        // ⎿ ⎾ ⏵ brackets/pieces Claude Code and git-style UIs use
        0x23BF => arms(Wt::L, Wt::L, Wt::N, Wt::N),
        0x23BE => arms(Wt::N, Wt::L, Wt::L, Wt::N),
        0x23F5 => at(4, &[0x40, 0x60, 0x70, 0x78, 0x70, 0x60, 0x40]),
        // ✻ ✳ ✶ ✢ ✽ ✺ ❋ ✱ spinner stars: one star
        0x2722 | 0x2731 | 0x2733 | 0x2736 | 0x273A | 0x273B | 0x273D | 0x274B | 0x2217 =>
            at(4, &[0x10, 0x92, 0x54, 0x38, 0x54, 0x92, 0x10]),
        // ⚠ warning sign
        0x26A0 => at(2, &[0x10, 0x10, 0x28, 0x28, 0x54, 0x54, 0x92, 0x92, 0x82, 0x92, 0xFE]),
        // ° degree, ± plus-minus
        0x00B0 => at(2, &[0x38, 0x44, 0x44, 0x38]),
        0x00B1 => at(4, &[0x10, 0x10, 0x7C, 0x10, 0x10, 0x00, 0x7C]),
        // ≥ ≤ ≠
        0x2265 => at(4, &[0x40, 0x20, 0x10, 0x08, 0x10, 0x20, 0x40, 0x00, 0x7C]),
        0x2264 => at(4, &[0x04, 0x08, 0x10, 0x20, 0x10, 0x08, 0x04, 0x00, 0x7C]),
        0x2260 => at(5, &[0x04, 0x7C, 0x08, 0x10, 0x7C, 0x40]),
        _ => return None,
    })
}

/// The glyph for code point `u`, if this module draws one.
pub fn glyph(u: u32) -> Option<Glyph> {
    match u {
        0x2500..=0x257F => box_drawing(u),
        0x2580..=0x259F => block_element(u),
        _ => symbol(u),
    }
}

/// An ASCII stand-in for a character with no glyph, or `None` (draw `?`).
pub fn fallback_ascii(u: u32) -> Option<char> {
    Some(match u {
        0x00A0 | 0x2000..=0x200A | 0x202F | 0x3000 => ' ',
        0x2010..=0x2015 | 0x2212 => '-',
        0x2018 | 0x2019 | 0x201A | 0x2032 => '\'',
        0x201C | 0x201D | 0x201E | 0x2033 => '"',
        0x2044 | 0x2215 => '/',
        0x200B..=0x200D | 0xFE0F => return None,
        _ => return None,
    })
}

/// Code points that occupy no cell (zero-width spaces and joiners, the
/// emoji variation selector).
pub fn zero_width(u: u32) -> bool {
    matches!(u, 0x200B..=0x200D | 0x2060 | 0xFE00..=0xFE0F)
}
