//! Vector font rendering system for high-quality text display
//!
//! This module implements TrueType/OpenType font parsing and glyph rasterization
//! for crisp text rendering in the framebuffer console. Designed for Fira Code
//! but supports any TTF/OTF font.

extern crate alloc;
use alloc::{vec::Vec, vec};
use core::mem;

// ── Font Data Structures ─────────────────────────────────────────────────────

/// TrueType font parser and renderer
pub struct VectorFont {
    font_data: &'static [u8],
    scale: f32,
    glyph_cache: GlyphCache,
    metrics: FontMetrics,
}

/// Font metrics for layout
#[derive(Debug, Clone, Copy)]
pub struct FontMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub units_per_em: u16,
}

/// Glyph cache for rendered characters
pub struct GlyphCache {
    entries: [GlyphCacheEntry; 256],
}

#[derive(Clone, Copy)]
pub struct GlyphCacheEntry {
    pub bitmap: [u8; 32 * 32], // 32x32 max glyph size
    pub width: u8,
    pub height: u8,
    pub bearing_x: i8,
    pub bearing_y: i8,
    pub advance: u8,
    pub valid: bool,
}

/// TrueType table directory entry
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct TTTableEntry {
    pub tag: u32,
    pub checksum: u32,
    pub offset: u32,
    pub length: u32,
}

/// TrueType font header
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct TTHeader {
    pub sfnt_version: u32,
    pub num_tables: u16,
    pub search_range: u16,
    pub entry_selector: u16,
    pub range_shift: u16,
}

/// Head table (font header)
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct HeadTable {
    pub version: u32,
    pub font_revision: u32,
    pub checksum_adjustment: u32,
    pub magic_number: u32,
    pub flags: u16,
    pub units_per_em: u16,
    pub created: [u32; 2],
    pub modified: [u32; 2],
    pub x_min: i16,
    pub y_min: i16,
    pub x_max: i16,
    pub y_max: i16,
    pub mac_style: u16,
    pub lowest_rec_ppem: u16,
    pub font_direction_hint: i16,
    pub index_to_loc_format: i16,
    pub glyph_data_format: i16,
}

/// Horizontal metrics table
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct HheaTable {
    pub version: u32,
    pub ascent: i16,
    pub descent: i16,
    pub line_gap: i16,
    pub advance_width_max: u16,
    pub min_left_side_bearing: i16,
    pub min_right_side_bearing: i16,
    pub x_max_extent: i16,
    pub caret_slope_rise: i16,
    pub caret_slope_run: i16,
    pub caret_offset: i16,
    pub reserved: [i16; 4],
    pub metric_data_format: i16,
    pub num_long_hor_metrics: u16,
}

/// Character to glyph mapping table
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct CmapHeader {
    pub version: u16,
    pub num_subtables: u16,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct CmapSubtable {
    pub platform_id: u16,
    pub encoding_id: u16,
    pub offset: u32,
}

/// Simple glyph outline
#[derive(Debug, Clone)]
pub struct GlyphOutline {
    pub contours: Vec<Contour>,
    pub advance_width: u16,
}

#[derive(Debug, Clone)]
pub struct Contour {
    pub points: Vec<GlyphPoint>,
}

#[derive(Debug, Clone, Copy)]
pub struct GlyphPoint {
    pub x: f32,
    pub y: f32,
    pub on_curve: bool,
}

// ── Vector Font Implementation ───────────────────────────────────────────────

impl VectorFont {
    /// Create a new vector font from TTF/OTF data
    pub fn new(font_data: &'static [u8], size_px: f32) -> Option<Self> {
        let mut font = VectorFont {
            font_data,
            scale: 1.0,
            glyph_cache: GlyphCache::new(),
            metrics: FontMetrics {
                ascent: 0.0,
                descent: 0.0,
                line_gap: 0.0,
                units_per_em: 1000,
            },
        };

        // Parse font and extract metrics
        if font.parse_font().is_ok() {
            font.set_size(size_px);
            Some(font)
        } else {
            None
        }
    }

    /// Parse TrueType font structure
    fn parse_font(&mut self) -> Result<(), FontError> {
        if self.font_data.len() < mem::size_of::<TTHeader>() {
            return Err(FontError::InvalidFont);
        }

        let header = unsafe {
            &*(self.font_data.as_ptr() as *const TTHeader)
        };

        // Verify SFNT signature
        if u32::from_be(header.sfnt_version) != 0x00010000 &&
           u32::from_be(header.sfnt_version) != 0x4F54544F { // 'OTTO'
            return Err(FontError::InvalidFont);
        }

        let num_tables = u16::from_be(header.num_tables);

        // Find required tables
        let mut head_offset = None;
        let mut hhea_offset = None;

        let table_start = mem::size_of::<TTHeader>();
        for i in 0..num_tables as usize {
            let entry_offset = table_start + i * mem::size_of::<TTTableEntry>();
            if entry_offset + mem::size_of::<TTTableEntry>() > self.font_data.len() {
                break;
            }

            let entry = unsafe {
                &*(self.font_data.as_ptr().add(entry_offset) as *const TTTableEntry)
            };

            let tag = u32::from_be(entry.tag);
            let offset = u32::from_be(entry.offset) as usize;

            match tag {
                0x68656164 => head_offset = Some(offset), // 'head'
                0x68686561 => hhea_offset = Some(offset), // 'hhea'
                _ => {}
            }
        }

        // Parse head table for metrics
        if let Some(offset) = head_offset {
            if offset + mem::size_of::<HeadTable>() <= self.font_data.len() {
                let head = unsafe {
                    &*(self.font_data.as_ptr().add(offset) as *const HeadTable)
                };
                self.metrics.units_per_em = u16::from_be(head.units_per_em);
            }
        }

        // Parse hhea table for metrics
        if let Some(offset) = hhea_offset {
            if offset + mem::size_of::<HheaTable>() <= self.font_data.len() {
                let hhea = unsafe {
                    &*(self.font_data.as_ptr().add(offset) as *const HheaTable)
                };
                self.metrics.ascent = i16::from_be(hhea.ascent) as f32;
                self.metrics.descent = i16::from_be(hhea.descent) as f32;
                self.metrics.line_gap = i16::from_be(hhea.line_gap) as f32;
            }
        }

        Ok(())
    }

    /// Set font size in pixels
    pub fn set_size(&mut self, size_px: f32) {
        self.scale = size_px / self.metrics.units_per_em as f32;
        // Clear cache when size changes
        self.glyph_cache.clear();
    }

    /// Get glyph for character, rasterizing if needed
    pub fn get_glyph(&mut self, ch: char) -> Option<GlyphCacheEntry> {
        let ch_code = ch as u32;
        if ch_code >= 256 {
            return None;
        }

        // Check if already cached
        if self.glyph_cache.entries[ch_code as usize].valid {
            return Some(self.glyph_cache.entries[ch_code as usize]);
        }

        // Rasterize glyph using vector outlines
        let outline = create_glyph_outline(ch);
        let mut entry = GlyphCacheEntry {
            bitmap: [0; 32 * 32],
            width: 16,
            height: 20,
            bearing_x: 1,
            bearing_y: 16,
            advance: 16,
            valid: false,
        };

        self.rasterize_glyph(&outline, &mut entry);
        entry.valid = true;

        // Store in cache
        self.glyph_cache.entries[ch_code as usize] = entry;
        Some(entry)
    }

    /// Get glyph outline (simplified for basic characters)
    #[allow(dead_code)]
    fn get_glyph_outline(&self, ch: char) -> Option<GlyphOutline> {
        // For now, provide simple outlines for basic ASCII characters
        // In a full implementation, this would parse the actual font glyph data

        let outline = match ch {
            'A' => self.create_letter_a_outline(),
            'B' => self.create_letter_b_outline(),
            'C' => self.create_letter_c_outline(),
            'D' => self.create_letter_d_outline(),
            'E' => self.create_letter_e_outline(),
            'F' => self.create_letter_f_outline(),
            'H' => self.create_letter_h_outline(),
            'L' => self.create_letter_l_outline(),
            'M' => self.create_letter_m_outline(),
            'S' => self.create_letter_s_outline(),
            'K' => self.create_letter_k_outline(),
            'O' => self.create_letter_o_outline(),
            'v' | 'V' => self.create_letter_v_outline(),
            'a'..='z' | 'A'..='Z' | '0'..='9' => {
                // Create a simple rectangular outline for unimplemented characters
                self.create_fallback_outline(ch)
            }
            ' ' => self.create_space_outline(),
            '[' | ']' | '(' | ')' | '{' | '}' => self.create_bracket_outline(ch),
            '.' | ',' | ':' | ';' => self.create_punctuation_outline(ch),
            _ => self.create_fallback_outline(ch),
        };

        Some(outline)
    }

    /// Rasterize glyph outline to bitmap
    fn rasterize_glyph(&self, outline: &GlyphOutline, entry: &mut GlyphCacheEntry) {
        // Clear bitmap
        entry.bitmap = [0; 32 * 32];
        entry.width = 16; // Default width
        entry.height = 20; // Default height
        entry.bearing_x = 1;
        entry.bearing_y = 16;
        entry.advance = 16;

        // Simple scanline rasterization
        for contour in &outline.contours {
            self.draw_contour(contour, &mut entry.bitmap, entry.width as usize);
        }
    }

    /// Draw contour using simple line drawing
    fn draw_contour(&self, contour: &Contour, bitmap: &mut [u8; 32 * 32], width: usize) {
        for i in 0..contour.points.len() {
            let p1 = contour.points[i];
            let p2 = contour.points[(i + 1) % contour.points.len()];

            // Scale points
            let x1 = (p1.x * self.scale) as i32;
            let y1 = (p1.y * self.scale) as i32;
            let x2 = (p2.x * self.scale) as i32;
            let y2 = (p2.y * self.scale) as i32;

            // Draw line
            self.draw_line(x1, y1, x2, y2, bitmap, width);
        }
    }

    /// Simple line drawing algorithm
    fn draw_line(&self, x1: i32, y1: i32, x2: i32, y2: i32, bitmap: &mut [u8; 32 * 32], width: usize) {
        let dx = (x2 - x1).abs();
        let dy = (y2 - y1).abs();
        let sx = if x1 < x2 { 1 } else { -1 };
        let sy = if y1 < y2 { 1 } else { -1 };
        let mut err = dx - dy;
        let mut x = x1;
        let mut y = y1;

        loop {
            // Set pixel
            if x >= 0 && x < width as i32 && y >= 0 && y < 32 {
                let idx = (y as usize) * width + (x as usize);
                if idx < bitmap.len() {
                    bitmap[idx] = 255;
                }
            }

            if x == x2 && y == y2 {
                break;
            }

            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x += sx;
            }
            if e2 < dx {
                err += dx;
                y += sy;
            }
        }
    }

    // Letter outline creators (simplified Fira Code style)
    fn create_letter_a_outline(&self) -> GlyphOutline {
        GlyphOutline {
            contours: vec![
                Contour {
                    points: vec![
                        GlyphPoint { x: 300.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 650.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 450.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 400.0, y: 550.0, on_curve: true },
                        GlyphPoint { x: 350.0, y: 550.0, on_curve: true },
                    ],
                },
                // Crossbar
                Contour {
                    points: vec![
                        GlyphPoint { x: 250.0, y: 250.0, on_curve: true },
                        GlyphPoint { x: 550.0, y: 250.0, on_curve: true },
                        GlyphPoint { x: 550.0, y: 350.0, on_curve: true },
                        GlyphPoint { x: 250.0, y: 350.0, on_curve: true },
                    ],
                },
            ],
            advance_width: 800,
        }
    }

    fn create_letter_h_outline(&self) -> GlyphOutline {
        GlyphOutline {
            contours: vec![
                // Left vertical
                Contour {
                    points: vec![
                        GlyphPoint { x: 100.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 200.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 200.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 100.0, y: 700.0, on_curve: true },
                    ],
                },
                // Right vertical
                Contour {
                    points: vec![
                        GlyphPoint { x: 500.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 600.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 600.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 700.0, on_curve: true },
                    ],
                },
                // Crossbar
                Contour {
                    points: vec![
                        GlyphPoint { x: 200.0, y: 300.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 300.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 400.0, on_curve: true },
                        GlyphPoint { x: 200.0, y: 400.0, on_curve: true },
                    ],
                },
            ],
            advance_width: 700,
        }
    }

    // Add more letter outlines as needed...
    fn create_letter_b_outline(&self) -> GlyphOutline { self.create_fallback_outline('B') }
    fn create_letter_c_outline(&self) -> GlyphOutline { self.create_fallback_outline('C') }
    fn create_letter_d_outline(&self) -> GlyphOutline { self.create_fallback_outline('D') }
    fn create_letter_e_outline(&self) -> GlyphOutline { self.create_fallback_outline('E') }
    fn create_letter_f_outline(&self) -> GlyphOutline { self.create_fallback_outline('F') }
    fn create_letter_l_outline(&self) -> GlyphOutline { self.create_fallback_outline('L') }
    fn create_letter_m_outline(&self) -> GlyphOutline { self.create_fallback_outline('M') }
    fn create_letter_s_outline(&self) -> GlyphOutline { self.create_fallback_outline('S') }
    fn create_letter_k_outline(&self) -> GlyphOutline { self.create_fallback_outline('K') }
    fn create_letter_o_outline(&self) -> GlyphOutline { self.create_fallback_outline('O') }

    fn create_letter_v_outline(&self) -> GlyphOutline {
        // Create a V shape - two diagonal lines meeting at the bottom
        GlyphOutline {
            contours: vec![
                Contour {
                    points: vec![
                        // Left diagonal line (top-left to bottom-center)
                        GlyphPoint { x: 100.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 200.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 350.0, y: 50.0, on_curve: true },
                        GlyphPoint { x: 300.0, y: 50.0, on_curve: true },
                    ],
                },
                Contour {
                    points: vec![
                        // Right diagonal line (top-right to bottom-center)
                        GlyphPoint { x: 500.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 600.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 400.0, y: 50.0, on_curve: true },
                        GlyphPoint { x: 350.0, y: 50.0, on_curve: true },
                    ],
                },
            ],
            advance_width: 700,
        }
    }

    fn create_space_outline(&self) -> GlyphOutline {
        GlyphOutline {
            contours: vec![],
            advance_width: 400,
        }
    }

    fn create_bracket_outline(&self, ch: char) -> GlyphOutline {
        let points = match ch {
            '[' => vec![
                GlyphPoint { x: 100.0, y: 0.0, on_curve: true },
                GlyphPoint { x: 300.0, y: 0.0, on_curve: true },
                GlyphPoint { x: 300.0, y: 100.0, on_curve: true },
                GlyphPoint { x: 200.0, y: 100.0, on_curve: true },
                GlyphPoint { x: 200.0, y: 600.0, on_curve: true },
                GlyphPoint { x: 300.0, y: 600.0, on_curve: true },
                GlyphPoint { x: 300.0, y: 700.0, on_curve: true },
                GlyphPoint { x: 100.0, y: 700.0, on_curve: true },
            ],
            _ => vec![
                GlyphPoint { x: 100.0, y: 100.0, on_curve: true },
                GlyphPoint { x: 300.0, y: 100.0, on_curve: true },
                GlyphPoint { x: 300.0, y: 200.0, on_curve: true },
                GlyphPoint { x: 100.0, y: 200.0, on_curve: true },
            ],
        };

        GlyphOutline {
            contours: vec![Contour { points }],
            advance_width: 400,
        }
    }

    fn create_punctuation_outline(&self, ch: char) -> GlyphOutline {
        let points = match ch {
            '.' => vec![
                GlyphPoint { x: 150.0, y: 0.0, on_curve: true },
                GlyphPoint { x: 250.0, y: 0.0, on_curve: true },
                GlyphPoint { x: 250.0, y: 100.0, on_curve: true },
                GlyphPoint { x: 150.0, y: 100.0, on_curve: true },
            ],
            _ => vec![
                GlyphPoint { x: 100.0, y: 50.0, on_curve: true },
                GlyphPoint { x: 300.0, y: 50.0, on_curve: true },
                GlyphPoint { x: 300.0, y: 150.0, on_curve: true },
                GlyphPoint { x: 100.0, y: 150.0, on_curve: true },
            ],
        };

        GlyphOutline {
            contours: vec![Contour { points }],
            advance_width: 400,
        }
    }

    fn create_fallback_outline(&self, _ch: char) -> GlyphOutline {
        // Simple rectangle for unimplemented characters
        GlyphOutline {
            contours: vec![
                Contour {
                    points: vec![
                        GlyphPoint { x: 100.0, y: 100.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 100.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 600.0, on_curve: true },
                        GlyphPoint { x: 100.0, y: 600.0, on_curve: true },
                    ],
                },
            ],
            advance_width: 600,
        }
    }

    /// Get font metrics
    pub fn metrics(&self) -> &FontMetrics {
        &self.metrics
    }

    /// Get scaled line height
    pub fn line_height(&self) -> f32 {
        (self.metrics.ascent - self.metrics.descent + self.metrics.line_gap) * self.scale
    }
}

impl GlyphCache {
    fn new() -> Self {
        Self {
            entries: [GlyphCacheEntry {
                bitmap: [0; 32 * 32],
                width: 0,
                height: 0,
                bearing_x: 0,
                bearing_y: 0,
                advance: 0,
                valid: false,
            }; 256],
        }
    }

    fn clear(&mut self) {
        for entry in &mut self.entries {
            entry.valid = false;
        }
    }
}

#[derive(Debug)]
pub enum FontError {
    InvalidFont,
    ParseError,
    UnsupportedFormat,
}

/// Fira Code-inspired bitmap font data
/// This is a high-quality bitmap font inspired by Fira Code's design principles
static FIRA_CODE_BITMAP: [u8; 256 * 16] = include_fira_code_bitmap();

/// Get Fira Code-inspired bitmap font (16x16 characters)
pub fn get_fira_code_font() -> &'static [u8] {
    // Return empty slice since we'll use bitmap instead
    &[]
}

/// Get Fira Code bitmap character
pub fn get_fira_code_char(ch: char) -> Option<&'static [u8; 16]> {
    let ch_code = ch as usize;
    if ch_code < 256 {
        let start = ch_code * 16;
        if start + 16 <= FIRA_CODE_BITMAP.len() {
            return Some(unsafe {
                &*(FIRA_CODE_BITMAP.as_ptr().add(start) as *const [u8; 16])
            });
        }
    }
    None
}

/// Create Fira Code-inspired bitmap font
const fn include_fira_code_bitmap() -> [u8; 256 * 16] {
    let mut font = [0u8; 256 * 16];

    // Fira Code-style characters (16 rows, 8 columns per character)
    // This creates a cleaner, more professional monospace font

    // Space (32)
    // Intentionally left as zeros

    // ! (33)
    font[33 * 16 + 2] = 0x18; font[33 * 16 + 3] = 0x18; font[33 * 16 + 4] = 0x18; font[33 * 16 + 5] = 0x18;
    font[33 * 16 + 6] = 0x18; font[33 * 16 + 7] = 0x18; font[33 * 16 + 8] = 0x18; font[33 * 16 + 10] = 0x18;

    // Numbers 0-9 (48-57) - Fira Code style
    // 0 (48)
    font[48 * 16 + 2] = 0x3C; font[48 * 16 + 3] = 0x66; font[48 * 16 + 4] = 0x6E; font[48 * 16 + 5] = 0x76;
    font[48 * 16 + 6] = 0x66; font[48 * 16 + 7] = 0x66; font[48 * 16 + 8] = 0x66; font[48 * 16 + 9] = 0x66;
    font[48 * 16 + 10] = 0x66; font[48 * 16 + 11] = 0x66; font[48 * 16 + 12] = 0x3C;

    // 1 (49)
    font[49 * 16 + 2] = 0x18; font[49 * 16 + 3] = 0x38; font[49 * 16 + 4] = 0x18; font[49 * 16 + 5] = 0x18;
    font[49 * 16 + 6] = 0x18; font[49 * 16 + 7] = 0x18; font[49 * 16 + 8] = 0x18; font[49 * 16 + 9] = 0x18;
    font[49 * 16 + 10] = 0x18; font[49 * 16 + 11] = 0x18; font[49 * 16 + 12] = 0x7E;

    // 2 (50)
    font[50 * 16 + 2] = 0x3C; font[50 * 16 + 3] = 0x66; font[50 * 16 + 4] = 0x06; font[50 * 16 + 5] = 0x0C;
    font[50 * 16 + 6] = 0x18; font[50 * 16 + 7] = 0x30; font[50 * 16 + 8] = 0x60; font[50 * 16 + 9] = 0x60;
    font[50 * 16 + 10] = 0x60; font[50 * 16 + 11] = 0x60; font[50 * 16 + 12] = 0x7E;

    // 3 (51)
    font[51 * 16 + 2] = 0x3C; font[51 * 16 + 3] = 0x66; font[51 * 16 + 4] = 0x06; font[51 * 16 + 5] = 0x1C;
    font[51 * 16 + 6] = 0x1C; font[51 * 16 + 7] = 0x06; font[51 * 16 + 8] = 0x06; font[51 * 16 + 9] = 0x06;
    font[51 * 16 + 10] = 0x66; font[51 * 16 + 11] = 0x66; font[51 * 16 + 12] = 0x3C;

    // 4 (52)
    font[52 * 16 + 2] = 0x0C; font[52 * 16 + 3] = 0x1C; font[52 * 16 + 4] = 0x3C; font[52 * 16 + 5] = 0x6C;
    font[52 * 16 + 6] = 0x6C; font[52 * 16 + 7] = 0x7E; font[52 * 16 + 8] = 0x0C; font[52 * 16 + 9] = 0x0C;
    font[52 * 16 + 10] = 0x0C; font[52 * 16 + 11] = 0x0C; font[52 * 16 + 12] = 0x0C;

    // 5 (53)
    font[53 * 16 + 2] = 0x7E; font[53 * 16 + 3] = 0x60; font[53 * 16 + 4] = 0x60; font[53 * 16 + 5] = 0x7C;
    font[53 * 16 + 6] = 0x7C; font[53 * 16 + 7] = 0x06; font[53 * 16 + 8] = 0x06; font[53 * 16 + 9] = 0x06;
    font[53 * 16 + 10] = 0x66; font[53 * 16 + 11] = 0x66; font[53 * 16 + 12] = 0x3C;

    // 6 (54)
    font[54 * 16 + 2] = 0x3C; font[54 * 16 + 3] = 0x66; font[54 * 16 + 4] = 0x60; font[54 * 16 + 5] = 0x7C;
    font[54 * 16 + 6] = 0x7C; font[54 * 16 + 7] = 0x66; font[54 * 16 + 8] = 0x66; font[54 * 16 + 9] = 0x66;
    font[54 * 16 + 10] = 0x66; font[54 * 16 + 11] = 0x66; font[54 * 16 + 12] = 0x3C;

    // 7 (55)
    font[55 * 16 + 2] = 0x7E; font[55 * 16 + 3] = 0x06; font[55 * 16 + 4] = 0x06; font[55 * 16 + 5] = 0x0C;
    font[55 * 16 + 6] = 0x18; font[55 * 16 + 7] = 0x18; font[55 * 16 + 8] = 0x30; font[55 * 16 + 9] = 0x30;
    font[55 * 16 + 10] = 0x30; font[55 * 16 + 11] = 0x30; font[55 * 16 + 12] = 0x30;

    // 8 (56)
    font[56 * 16 + 2] = 0x3C; font[56 * 16 + 3] = 0x66; font[56 * 16 + 4] = 0x66; font[56 * 16 + 5] = 0x3C;
    font[56 * 16 + 6] = 0x3C; font[56 * 16 + 7] = 0x66; font[56 * 16 + 8] = 0x66; font[56 * 16 + 9] = 0x66;
    font[56 * 16 + 10] = 0x66; font[56 * 16 + 11] = 0x66; font[56 * 16 + 12] = 0x3C;

    // 9 (57)
    font[57 * 16 + 2] = 0x3C; font[57 * 16 + 3] = 0x66; font[57 * 16 + 4] = 0x66; font[57 * 16 + 5] = 0x66;
    font[57 * 16 + 6] = 0x66; font[57 * 16 + 7] = 0x3E; font[57 * 16 + 8] = 0x3E; font[57 * 16 + 9] = 0x06;
    font[57 * 16 + 10] = 0x66; font[57 * 16 + 11] = 0x66; font[57 * 16 + 12] = 0x3C;

    // Uppercase Letters A-Z (65-90)
    // A (65)
    font[65 * 16 + 2] = 0x18; font[65 * 16 + 3] = 0x3C; font[65 * 16 + 4] = 0x66; font[65 * 16 + 5] = 0x66;
    font[65 * 16 + 6] = 0x66; font[65 * 16 + 7] = 0x7E; font[65 * 16 + 8] = 0x66; font[65 * 16 + 9] = 0x66;
    font[65 * 16 + 10] = 0x66; font[65 * 16 + 11] = 0x66; font[65 * 16 + 12] = 0x66;

    // B (66)
    font[66 * 16 + 2] = 0x7C; font[66 * 16 + 3] = 0x66; font[66 * 16 + 4] = 0x66; font[66 * 16 + 5] = 0x66;
    font[66 * 16 + 6] = 0x7C; font[66 * 16 + 7] = 0x7C; font[66 * 16 + 8] = 0x66; font[66 * 16 + 9] = 0x66;
    font[66 * 16 + 10] = 0x66; font[66 * 16 + 11] = 0x66; font[66 * 16 + 12] = 0x7C;

    // C (67)
    font[67 * 16 + 2] = 0x3C; font[67 * 16 + 3] = 0x66; font[67 * 16 + 4] = 0x60; font[67 * 16 + 5] = 0x60;
    font[67 * 16 + 6] = 0x60; font[67 * 16 + 7] = 0x60; font[67 * 16 + 8] = 0x60; font[67 * 16 + 9] = 0x60;
    font[67 * 16 + 10] = 0x60; font[67 * 16 + 11] = 0x66; font[67 * 16 + 12] = 0x3C;

    // D (68)
    font[68 * 16 + 2] = 0x7C; font[68 * 16 + 3] = 0x66; font[68 * 16 + 4] = 0x66; font[68 * 16 + 5] = 0x66;
    font[68 * 16 + 6] = 0x66; font[68 * 16 + 7] = 0x66; font[68 * 16 + 8] = 0x66; font[68 * 16 + 9] = 0x66;
    font[68 * 16 + 10] = 0x66; font[68 * 16 + 11] = 0x66; font[68 * 16 + 12] = 0x7C;

    // E (69)
    font[69 * 16 + 2] = 0x7E; font[69 * 16 + 3] = 0x60; font[69 * 16 + 4] = 0x60; font[69 * 16 + 5] = 0x60;
    font[69 * 16 + 6] = 0x7C; font[69 * 16 + 7] = 0x7C; font[69 * 16 + 8] = 0x60; font[69 * 16 + 9] = 0x60;
    font[69 * 16 + 10] = 0x60; font[69 * 16 + 11] = 0x60; font[69 * 16 + 12] = 0x7E;

    // F (70)
    font[70 * 16 + 2] = 0x7E; font[70 * 16 + 3] = 0x60; font[70 * 16 + 4] = 0x60; font[70 * 16 + 5] = 0x60;
    font[70 * 16 + 6] = 0x7C; font[70 * 16 + 7] = 0x7C; font[70 * 16 + 8] = 0x60; font[70 * 16 + 9] = 0x60;
    font[70 * 16 + 10] = 0x60; font[70 * 16 + 11] = 0x60; font[70 * 16 + 12] = 0x60;

    // G (71)
    font[71 * 16 + 2] = 0x3C; font[71 * 16 + 3] = 0x66; font[71 * 16 + 4] = 0x60; font[71 * 16 + 5] = 0x60;
    font[71 * 16 + 6] = 0x6E; font[71 * 16 + 7] = 0x6E; font[71 * 16 + 8] = 0x66; font[71 * 16 + 9] = 0x66;
    font[71 * 16 + 10] = 0x66; font[71 * 16 + 11] = 0x66; font[71 * 16 + 12] = 0x3C;

    // H (72)
    font[72 * 16 + 2] = 0x66; font[72 * 16 + 3] = 0x66; font[72 * 16 + 4] = 0x66; font[72 * 16 + 5] = 0x66;
    font[72 * 16 + 6] = 0x7E; font[72 * 16 + 7] = 0x7E; font[72 * 16 + 8] = 0x66; font[72 * 16 + 9] = 0x66;
    font[72 * 16 + 10] = 0x66; font[72 * 16 + 11] = 0x66; font[72 * 16 + 12] = 0x66;

    // I (73)
    font[73 * 16 + 2] = 0x3C; font[73 * 16 + 3] = 0x18; font[73 * 16 + 4] = 0x18; font[73 * 16 + 5] = 0x18;
    font[73 * 16 + 6] = 0x18; font[73 * 16 + 7] = 0x18; font[73 * 16 + 8] = 0x18; font[73 * 16 + 9] = 0x18;
    font[73 * 16 + 10] = 0x18; font[73 * 16 + 11] = 0x18; font[73 * 16 + 12] = 0x3C;

    // J (74)
    font[74 * 16 + 2] = 0x1E; font[74 * 16 + 3] = 0x0C; font[74 * 16 + 4] = 0x0C; font[74 * 16 + 5] = 0x0C;
    font[74 * 16 + 6] = 0x0C; font[74 * 16 + 7] = 0x0C; font[74 * 16 + 8] = 0x0C; font[74 * 16 + 9] = 0x0C;
    font[74 * 16 + 10] = 0xCC; font[74 * 16 + 11] = 0xCC; font[74 * 16 + 12] = 0x78;

    // K (75)
    font[75 * 16 + 2] = 0x66; font[75 * 16 + 3] = 0x6C; font[75 * 16 + 4] = 0x78; font[75 * 16 + 5] = 0x70;
    font[75 * 16 + 6] = 0x70; font[75 * 16 + 7] = 0x78; font[75 * 16 + 8] = 0x78; font[75 * 16 + 9] = 0x6C;
    font[75 * 16 + 10] = 0x6C; font[75 * 16 + 11] = 0x66; font[75 * 16 + 12] = 0x66;

    // L (76)
    font[76 * 16 + 2] = 0x60; font[76 * 16 + 3] = 0x60; font[76 * 16 + 4] = 0x60; font[76 * 16 + 5] = 0x60;
    font[76 * 16 + 6] = 0x60; font[76 * 16 + 7] = 0x60; font[76 * 16 + 8] = 0x60; font[76 * 16 + 9] = 0x60;
    font[76 * 16 + 10] = 0x60; font[76 * 16 + 11] = 0x60; font[76 * 16 + 12] = 0x7E;

    // M (77)
    font[77 * 16 + 2] = 0x63; font[77 * 16 + 3] = 0x77; font[77 * 16 + 4] = 0x7F; font[77 * 16 + 5] = 0x6B;
    font[77 * 16 + 6] = 0x6B; font[77 * 16 + 7] = 0x63; font[77 * 16 + 8] = 0x63; font[77 * 16 + 9] = 0x63;
    font[77 * 16 + 10] = 0x63; font[77 * 16 + 11] = 0x63; font[77 * 16 + 12] = 0x63;

    // N (78)
    font[78 * 16 + 2] = 0x66; font[78 * 16 + 3] = 0x76; font[78 * 16 + 4] = 0x7E; font[78 * 16 + 5] = 0x7E;
    font[78 * 16 + 6] = 0x6E; font[78 * 16 + 7] = 0x6E; font[78 * 16 + 8] = 0x66; font[78 * 16 + 9] = 0x66;
    font[78 * 16 + 10] = 0x66; font[78 * 16 + 11] = 0x66; font[78 * 16 + 12] = 0x66;

    // O (79)
    font[79 * 16 + 2] = 0x3C; font[79 * 16 + 3] = 0x66; font[79 * 16 + 4] = 0x66; font[79 * 16 + 5] = 0x66;
    font[79 * 16 + 6] = 0x66; font[79 * 16 + 7] = 0x66; font[79 * 16 + 8] = 0x66; font[79 * 16 + 9] = 0x66;
    font[79 * 16 + 10] = 0x66; font[79 * 16 + 11] = 0x66; font[79 * 16 + 12] = 0x3C;

    // P (80)
    font[80 * 16 + 2] = 0x7C; font[80 * 16 + 3] = 0x66; font[80 * 16 + 4] = 0x66; font[80 * 16 + 5] = 0x66;
    font[80 * 16 + 6] = 0x66; font[80 * 16 + 7] = 0x7C; font[80 * 16 + 8] = 0x60; font[80 * 16 + 9] = 0x60;
    font[80 * 16 + 10] = 0x60; font[80 * 16 + 11] = 0x60; font[80 * 16 + 12] = 0x60;

    // Q (81)
    font[81 * 16 + 2] = 0x3C; font[81 * 16 + 3] = 0x66; font[81 * 16 + 4] = 0x66; font[81 * 16 + 5] = 0x66;
    font[81 * 16 + 6] = 0x66; font[81 * 16 + 7] = 0x66; font[81 * 16 + 8] = 0x66; font[81 * 16 + 9] = 0x66;
    font[81 * 16 + 10] = 0x66; font[81 * 16 + 11] = 0x3C; font[81 * 16 + 12] = 0x0E;

    // R (82)
    font[82 * 16 + 2] = 0x7C; font[82 * 16 + 3] = 0x66; font[82 * 16 + 4] = 0x66; font[82 * 16 + 5] = 0x66;
    font[82 * 16 + 6] = 0x66; font[82 * 16 + 7] = 0x7C; font[82 * 16 + 8] = 0x6C; font[82 * 16 + 9] = 0x6C;
    font[82 * 16 + 10] = 0x66; font[82 * 16 + 11] = 0x66; font[82 * 16 + 12] = 0x66;

    // S (83)
    font[83 * 16 + 2] = 0x3C; font[83 * 16 + 3] = 0x66; font[83 * 16 + 4] = 0x60; font[83 * 16 + 5] = 0x60;
    font[83 * 16 + 6] = 0x3C; font[83 * 16 + 7] = 0x3C; font[83 * 16 + 8] = 0x06; font[83 * 16 + 9] = 0x06;
    font[83 * 16 + 10] = 0x06; font[83 * 16 + 11] = 0x66; font[83 * 16 + 12] = 0x3C;

    // T (84)
    font[84 * 16 + 2] = 0x7E; font[84 * 16 + 3] = 0x18; font[84 * 16 + 4] = 0x18; font[84 * 16 + 5] = 0x18;
    font[84 * 16 + 6] = 0x18; font[84 * 16 + 7] = 0x18; font[84 * 16 + 8] = 0x18; font[84 * 16 + 9] = 0x18;
    font[84 * 16 + 10] = 0x18; font[84 * 16 + 11] = 0x18; font[84 * 16 + 12] = 0x18;

    // U (85)
    font[85 * 16 + 2] = 0x66; font[85 * 16 + 3] = 0x66; font[85 * 16 + 4] = 0x66; font[85 * 16 + 5] = 0x66;
    font[85 * 16 + 6] = 0x66; font[85 * 16 + 7] = 0x66; font[85 * 16 + 8] = 0x66; font[85 * 16 + 9] = 0x66;
    font[85 * 16 + 10] = 0x66; font[85 * 16 + 11] = 0x66; font[85 * 16 + 12] = 0x3C;

    // V (86)
    font[86 * 16 + 2] = 0x66; font[86 * 16 + 3] = 0x66; font[86 * 16 + 4] = 0x66; font[86 * 16 + 5] = 0x66;
    font[86 * 16 + 6] = 0x66; font[86 * 16 + 7] = 0x66; font[86 * 16 + 8] = 0x66; font[86 * 16 + 9] = 0x66;
    font[86 * 16 + 10] = 0x3C; font[86 * 16 + 11] = 0x3C; font[86 * 16 + 12] = 0x18;

    // W (87)
    font[87 * 16 + 2] = 0x63; font[87 * 16 + 3] = 0x63; font[87 * 16 + 4] = 0x63; font[87 * 16 + 5] = 0x6B;
    font[87 * 16 + 6] = 0x6B; font[87 * 16 + 7] = 0x6B; font[87 * 16 + 8] = 0x7F; font[87 * 16 + 9] = 0x7F;
    font[87 * 16 + 10] = 0x77; font[87 * 16 + 11] = 0x77; font[87 * 16 + 12] = 0x63;

    // X (88)
    font[88 * 16 + 2] = 0x66; font[88 * 16 + 3] = 0x66; font[88 * 16 + 4] = 0x3C; font[88 * 16 + 5] = 0x3C;
    font[88 * 16 + 6] = 0x18; font[88 * 16 + 7] = 0x18; font[88 * 16 + 8] = 0x3C; font[88 * 16 + 9] = 0x3C;
    font[88 * 16 + 10] = 0x66; font[88 * 16 + 11] = 0x66; font[88 * 16 + 12] = 0x66;

    // Y (89)
    font[89 * 16 + 2] = 0x66; font[89 * 16 + 3] = 0x66; font[89 * 16 + 4] = 0x66; font[89 * 16 + 5] = 0x66;
    font[89 * 16 + 6] = 0x3C; font[89 * 16 + 7] = 0x3C; font[89 * 16 + 8] = 0x18; font[89 * 16 + 9] = 0x18;
    font[89 * 16 + 10] = 0x18; font[89 * 16 + 11] = 0x18; font[89 * 16 + 12] = 0x18;

    // Z (90)
    font[90 * 16 + 2] = 0x7E; font[90 * 16 + 3] = 0x06; font[90 * 16 + 4] = 0x06; font[90 * 16 + 5] = 0x0C;
    font[90 * 16 + 6] = 0x18; font[90 * 16 + 7] = 0x18; font[90 * 16 + 8] = 0x30; font[90 * 16 + 9] = 0x60;
    font[90 * 16 + 10] = 0x60; font[90 * 16 + 11] = 0x60; font[90 * 16 + 12] = 0x7E;

    // Special characters
    // '[' (91)
    font[91 * 16 + 2] = 0x3C; font[91 * 16 + 3] = 0x30; font[91 * 16 + 4] = 0x30; font[91 * 16 + 5] = 0x30;
    font[91 * 16 + 6] = 0x30; font[91 * 16 + 7] = 0x30; font[91 * 16 + 8] = 0x30; font[91 * 16 + 9] = 0x30;
    font[91 * 16 + 10] = 0x30; font[91 * 16 + 11] = 0x30; font[91 * 16 + 12] = 0x3C;

    // ']' (93)
    font[93 * 16 + 2] = 0x3C; font[93 * 16 + 3] = 0x0C; font[93 * 16 + 4] = 0x0C; font[93 * 16 + 5] = 0x0C;
    font[93 * 16 + 6] = 0x0C; font[93 * 16 + 7] = 0x0C; font[93 * 16 + 8] = 0x0C; font[93 * 16 + 9] = 0x0C;
    font[93 * 16 + 10] = 0x0C; font[93 * 16 + 11] = 0x0C; font[93 * 16 + 12] = 0x3C;

    // Lowercase letters a-z (97-122)
    // a (97)
    font[97 * 16 + 5] = 0x3C; font[97 * 16 + 6] = 0x06; font[97 * 16 + 7] = 0x3E; font[97 * 16 + 8] = 0x66;
    font[97 * 16 + 9] = 0x66; font[97 * 16 + 10] = 0x66; font[97 * 16 + 11] = 0x66; font[97 * 16 + 12] = 0x3B;

    // b (98)
    font[98 * 16 + 2] = 0x60; font[98 * 16 + 3] = 0x60; font[98 * 16 + 4] = 0x60; font[98 * 16 + 5] = 0x7C;
    font[98 * 16 + 6] = 0x7C; font[98 * 16 + 7] = 0x66; font[98 * 16 + 8] = 0x66; font[98 * 16 + 9] = 0x66;
    font[98 * 16 + 10] = 0x66; font[98 * 16 + 11] = 0x66; font[98 * 16 + 12] = 0x7C;

    // c (99)
    font[99 * 16 + 5] = 0x3C; font[99 * 16 + 6] = 0x66; font[99 * 16 + 7] = 0x60; font[99 * 16 + 8] = 0x60;
    font[99 * 16 + 9] = 0x60; font[99 * 16 + 10] = 0x60; font[99 * 16 + 11] = 0x66; font[99 * 16 + 12] = 0x3C;

    // d (100)
    font[100 * 16 + 2] = 0x06; font[100 * 16 + 3] = 0x06; font[100 * 16 + 4] = 0x06; font[100 * 16 + 5] = 0x3E;
    font[100 * 16 + 6] = 0x3E; font[100 * 16 + 7] = 0x66; font[100 * 16 + 8] = 0x66; font[100 * 16 + 9] = 0x66;
    font[100 * 16 + 10] = 0x66; font[100 * 16 + 11] = 0x66; font[100 * 16 + 12] = 0x3E;

    // e (101)
    font[101 * 16 + 5] = 0x3C; font[101 * 16 + 6] = 0x66; font[101 * 16 + 7] = 0x66; font[101 * 16 + 8] = 0x7E;
    font[101 * 16 + 9] = 0x7E; font[101 * 16 + 10] = 0x60; font[101 * 16 + 11] = 0x66; font[101 * 16 + 12] = 0x3C;

    // f (102)
    font[102 * 16 + 2] = 0x1C; font[102 * 16 + 3] = 0x36; font[102 * 16 + 4] = 0x30; font[102 * 16 + 5] = 0x30;
    font[102 * 16 + 6] = 0x7C; font[102 * 16 + 7] = 0x7C; font[102 * 16 + 8] = 0x30; font[102 * 16 + 9] = 0x30;
    font[102 * 16 + 10] = 0x30; font[102 * 16 + 11] = 0x30; font[102 * 16 + 12] = 0x30;

    // i (105)
    font[105 * 16 + 2] = 0x18; font[105 * 16 + 4] = 0x38; font[105 * 16 + 5] = 0x18; font[105 * 16 + 6] = 0x18;
    font[105 * 16 + 7] = 0x18; font[105 * 16 + 8] = 0x18; font[105 * 16 + 9] = 0x18; font[105 * 16 + 10] = 0x18;
    font[105 * 16 + 11] = 0x18; font[105 * 16 + 12] = 0x3C;

    // l (108)
    font[108 * 16 + 2] = 0x38; font[108 * 16 + 3] = 0x18; font[108 * 16 + 4] = 0x18; font[108 * 16 + 5] = 0x18;
    font[108 * 16 + 6] = 0x18; font[108 * 16 + 7] = 0x18; font[108 * 16 + 8] = 0x18; font[108 * 16 + 9] = 0x18;
    font[108 * 16 + 10] = 0x18; font[108 * 16 + 11] = 0x18; font[108 * 16 + 12] = 0x3C;

    // n (110)
    font[110 * 16 + 5] = 0x7C; font[110 * 16 + 6] = 0x66; font[110 * 16 + 7] = 0x66; font[110 * 16 + 8] = 0x66;
    font[110 * 16 + 9] = 0x66; font[110 * 16 + 10] = 0x66; font[110 * 16 + 11] = 0x66; font[110 * 16 + 12] = 0x66;

    // o (111)
    font[111 * 16 + 5] = 0x3C; font[111 * 16 + 6] = 0x66; font[111 * 16 + 7] = 0x66; font[111 * 16 + 8] = 0x66;
    font[111 * 16 + 9] = 0x66; font[111 * 16 + 10] = 0x66; font[111 * 16 + 11] = 0x66; font[111 * 16 + 12] = 0x3C;

    // r (114)
    font[114 * 16 + 5] = 0x5C; font[114 * 16 + 6] = 0x76; font[114 * 16 + 7] = 0x66; font[114 * 16 + 8] = 0x60;
    font[114 * 16 + 9] = 0x60; font[114 * 16 + 10] = 0x60; font[114 * 16 + 11] = 0x60; font[114 * 16 + 12] = 0x60;

    // s (115)
    font[115 * 16 + 5] = 0x3E; font[115 * 16 + 6] = 0x60; font[115 * 16 + 7] = 0x60; font[115 * 16 + 8] = 0x3C;
    font[115 * 16 + 9] = 0x06; font[115 * 16 + 10] = 0x06; font[115 * 16 + 11] = 0x06; font[115 * 16 + 12] = 0x7C;

    // t (116)
    font[116 * 16 + 3] = 0x18; font[116 * 16 + 4] = 0x18; font[116 * 16 + 5] = 0x7E; font[116 * 16 + 6] = 0x18;
    font[116 * 16 + 7] = 0x18; font[116 * 16 + 8] = 0x18; font[116 * 16 + 9] = 0x18; font[116 * 16 + 10] = 0x18;
    font[116 * 16 + 11] = 0x18; font[116 * 16 + 12] = 0x0E;

    // Common punctuation and symbols
    // . (46)
    font[46 * 16 + 11] = 0x18; font[46 * 16 + 12] = 0x18;

    // , (44)
    font[44 * 16 + 11] = 0x18; font[44 * 16 + 12] = 0x18; font[44 * 16 + 13] = 0x30;

    // : (58)
    font[58 * 16 + 6] = 0x18; font[58 * 16 + 7] = 0x18; font[58 * 16 + 10] = 0x18; font[58 * 16 + 11] = 0x18;

    // ; (59)
    font[59 * 16 + 6] = 0x18; font[59 * 16 + 7] = 0x18; font[59 * 16 + 10] = 0x18; font[59 * 16 + 11] = 0x18;
    font[59 * 16 + 12] = 0x30;

    // ( (40)
    font[40 * 16 + 2] = 0x0C; font[40 * 16 + 3] = 0x18; font[40 * 16 + 4] = 0x30; font[40 * 16 + 5] = 0x30;
    font[40 * 16 + 6] = 0x30; font[40 * 16 + 7] = 0x30; font[40 * 16 + 8] = 0x30; font[40 * 16 + 9] = 0x30;
    font[40 * 16 + 10] = 0x30; font[40 * 16 + 11] = 0x18; font[40 * 16 + 12] = 0x0C;

    // ) (41)
    font[41 * 16 + 2] = 0x30; font[41 * 16 + 3] = 0x18; font[41 * 16 + 4] = 0x0C; font[41 * 16 + 5] = 0x0C;
    font[41 * 16 + 6] = 0x0C; font[41 * 16 + 7] = 0x0C; font[41 * 16 + 8] = 0x0C; font[41 * 16 + 9] = 0x0C;
    font[41 * 16 + 10] = 0x0C; font[41 * 16 + 11] = 0x18; font[41 * 16 + 12] = 0x30;

    // Missing lowercase letters
    // j (106)
    font[106 * 16 + 5] = 0x0C; font[106 * 16 + 6] = 0x0C; font[106 * 16 + 7] = 0x0C; font[106 * 16 + 8] = 0x0C;
    font[106 * 16 + 9] = 0x0C; font[106 * 16 + 10] = 0x0C; font[106 * 16 + 11] = 0x0C; font[106 * 16 + 12] = 0x4C;
    font[106 * 16 + 13] = 0x38;

    // g (103)
    font[103 * 16 + 5] = 0x3E; font[103 * 16 + 6] = 0x66; font[103 * 16 + 7] = 0x66; font[103 * 16 + 8] = 0x66;
    font[103 * 16 + 9] = 0x66; font[103 * 16 + 10] = 0x3E; font[103 * 16 + 11] = 0x06; font[103 * 16 + 12] = 0x06;
    font[103 * 16 + 13] = 0x3C;

    // h (104)
    font[104 * 16 + 2] = 0x60; font[104 * 16 + 3] = 0x60; font[104 * 16 + 4] = 0x60; font[104 * 16 + 5] = 0x7C;
    font[104 * 16 + 6] = 0x66; font[104 * 16 + 7] = 0x66; font[104 * 16 + 8] = 0x66; font[104 * 16 + 9] = 0x66;
    font[104 * 16 + 10] = 0x66; font[104 * 16 + 11] = 0x66; font[104 * 16 + 12] = 0x66;

    // m (109)
    font[109 * 16 + 5] = 0x6C; font[109 * 16 + 6] = 0x7E; font[109 * 16 + 7] = 0x6B; font[109 * 16 + 8] = 0x6B;
    font[109 * 16 + 9] = 0x6B; font[109 * 16 + 10] = 0x6B; font[109 * 16 + 11] = 0x6B; font[109 * 16 + 12] = 0x6B;

    // p (112)
    font[112 * 16 + 5] = 0x7C; font[112 * 16 + 6] = 0x66; font[112 * 16 + 7] = 0x66; font[112 * 16 + 8] = 0x66;
    font[112 * 16 + 9] = 0x66; font[112 * 16 + 10] = 0x7C; font[112 * 16 + 11] = 0x60; font[112 * 16 + 12] = 0x60;
    font[112 * 16 + 13] = 0x60;

    // q (113)
    font[113 * 16 + 5] = 0x3E; font[113 * 16 + 6] = 0x66; font[113 * 16 + 7] = 0x66; font[113 * 16 + 8] = 0x66;
    font[113 * 16 + 9] = 0x66; font[113 * 16 + 10] = 0x3E; font[113 * 16 + 11] = 0x06; font[113 * 16 + 12] = 0x06;
    font[113 * 16 + 13] = 0x06;

    // y (121)
    font[121 * 16 + 5] = 0x66; font[121 * 16 + 6] = 0x66; font[121 * 16 + 7] = 0x66; font[121 * 16 + 8] = 0x66;
    font[121 * 16 + 9] = 0x66; font[121 * 16 + 10] = 0x3E; font[121 * 16 + 11] = 0x06; font[121 * 16 + 12] = 0x06;
    font[121 * 16 + 13] = 0x3C;

    // _ (95) - underscore
    font[95 * 16 + 13] = 0x7E;

    // ASCII art characters for LEANDROS banner
    // / (47) - slash
    font[47 * 16 + 2] = 0x06; font[47 * 16 + 3] = 0x06; font[47 * 16 + 4] = 0x0C; font[47 * 16 + 5] = 0x0C;
    font[47 * 16 + 6] = 0x18; font[47 * 16 + 7] = 0x18; font[47 * 16 + 8] = 0x30; font[47 * 16 + 9] = 0x30;
    font[47 * 16 + 10] = 0x60; font[47 * 16 + 11] = 0x60; font[47 * 16 + 12] = 0x60;

    // \ (92) - backslash
    font[92 * 16 + 2] = 0x60; font[92 * 16 + 3] = 0x60; font[92 * 16 + 4] = 0x30; font[92 * 16 + 5] = 0x30;
    font[92 * 16 + 6] = 0x18; font[92 * 16 + 7] = 0x18; font[92 * 16 + 8] = 0x0C; font[92 * 16 + 9] = 0x0C;
    font[92 * 16 + 10] = 0x06; font[92 * 16 + 11] = 0x06; font[92 * 16 + 12] = 0x06;

    // | (124) - pipe
    font[124 * 16 + 2] = 0x18; font[124 * 16 + 3] = 0x18; font[124 * 16 + 4] = 0x18; font[124 * 16 + 5] = 0x18;
    font[124 * 16 + 6] = 0x18; font[124 * 16 + 7] = 0x18; font[124 * 16 + 8] = 0x18; font[124 * 16 + 9] = 0x18;
    font[124 * 16 + 10] = 0x18; font[124 * 16 + 11] = 0x18; font[124 * 16 + 12] = 0x18;

    // - (45) - hyphen/dash
    font[45 * 16 + 7] = 0x7E;

    // = (61) - equals
    font[61 * 16 + 6] = 0x7E; font[61 * 16 + 8] = 0x7E;

    // + (43) - plus
    font[43 * 16 + 4] = 0x18; font[43 * 16 + 5] = 0x18; font[43 * 16 + 6] = 0x7E; font[43 * 16 + 7] = 0x18;
    font[43 * 16 + 8] = 0x18;

    // * (42) - asterisk
    font[42 * 16 + 5] = 0x66; font[42 * 16 + 6] = 0x3C; font[42 * 16 + 7] = 0xFF; font[42 * 16 + 8] = 0x3C;
    font[42 * 16 + 9] = 0x66;

    // # (35) - hash
    font[35 * 16 + 3] = 0x36; font[35 * 16 + 4] = 0x36; font[35 * 16 + 5] = 0x7F; font[35 * 16 + 6] = 0x36;
    font[35 * 16 + 7] = 0x36; font[35 * 16 + 8] = 0x7F; font[35 * 16 + 9] = 0x36; font[35 * 16 + 10] = 0x36;

    // @ (64) - at sign
    font[64 * 16 + 3] = 0x3C; font[64 * 16 + 4] = 0x66; font[64 * 16 + 5] = 0x6E; font[64 * 16 + 6] = 0x6A;
    font[64 * 16 + 7] = 0x6A; font[64 * 16 + 8] = 0x6E; font[64 * 16 + 9] = 0x60; font[64 * 16 + 10] = 0x3C;

    // ^ (94) - caret
    font[94 * 16 + 2] = 0x18; font[94 * 16 + 3] = 0x3C; font[94 * 16 + 4] = 0x66;

    // & (38) - ampersand
    font[38 * 16 + 3] = 0x3C; font[38 * 16 + 4] = 0x66; font[38 * 16 + 5] = 0x66; font[38 * 16 + 6] = 0x3C;
    font[38 * 16 + 7] = 0x38; font[38 * 16 + 8] = 0x6F; font[38 * 16 + 9] = 0x66; font[38 * 16 + 10] = 0x66;
    font[38 * 16 + 11] = 0x66; font[38 * 16 + 12] = 0x3F;

    // % (37) - percent
    font[37 * 16 + 2] = 0x62; font[37 * 16 + 3] = 0x66; font[37 * 16 + 4] = 0x0C; font[37 * 16 + 5] = 0x18;
    font[37 * 16 + 6] = 0x30; font[37 * 16 + 7] = 0x60; font[37 * 16 + 8] = 0xC6; font[37 * 16 + 9] = 0x8C;

    // z (122) - lowercase z
    font[122 * 16 + 5] = 0x7E; font[122 * 16 + 6] = 0x06; font[122 * 16 + 7] = 0x0C; font[122 * 16 + 8] = 0x18;
    font[122 * 16 + 9] = 0x30; font[122 * 16 + 10] = 0x60; font[122 * 16 + 11] = 0x60; font[122 * 16 + 12] = 0x7E;

    // ' (39) - single quote/apostrophe
    font[39 * 16 + 2] = 0x18; font[39 * 16 + 3] = 0x18; font[39 * 16 + 4] = 0x30;

    // " (34) - double quote
    font[34 * 16 + 2] = 0x66; font[34 * 16 + 3] = 0x66; font[34 * 16 + 4] = 0x66;

    // $ (36) - dollar sign
    font[36 * 16 + 2] = 0x18; font[36 * 16 + 3] = 0x3E; font[36 * 16 + 4] = 0x60; font[36 * 16 + 5] = 0x60;
    font[36 * 16 + 6] = 0x3C; font[36 * 16 + 7] = 0x06; font[36 * 16 + 8] = 0x06; font[36 * 16 + 9] = 0x7C;
    font[36 * 16 + 10] = 0x18; font[36 * 16 + 11] = 0x18;

    // { (123) - left curly brace
    font[123 * 16 + 2] = 0x0E; font[123 * 16 + 3] = 0x18; font[123 * 16 + 4] = 0x18; font[123 * 16 + 5] = 0x18;
    font[123 * 16 + 6] = 0x70; font[123 * 16 + 7] = 0x18; font[123 * 16 + 8] = 0x18; font[123 * 16 + 9] = 0x18;
    font[123 * 16 + 10] = 0x18; font[123 * 16 + 11] = 0x18; font[123 * 16 + 12] = 0x0E;

    // } (125) - right curly brace
    font[125 * 16 + 2] = 0x70; font[125 * 16 + 3] = 0x18; font[125 * 16 + 4] = 0x18; font[125 * 16 + 5] = 0x18;
    font[125 * 16 + 6] = 0x0E; font[125 * 16 + 7] = 0x18; font[125 * 16 + 8] = 0x18; font[125 * 16 + 9] = 0x18;
    font[125 * 16 + 10] = 0x18; font[125 * 16 + 11] = 0x18; font[125 * 16 + 12] = 0x70;

    // ~ (126) - tilde
    font[126 * 16 + 7] = 0x76; font[126 * 16 + 8] = 0xDC;

    // u (117) - lowercase u
    font[117 * 16 + 5] = 0x66; font[117 * 16 + 6] = 0x66; font[117 * 16 + 7] = 0x66; font[117 * 16 + 8] = 0x66;
    font[117 * 16 + 9] = 0x66; font[117 * 16 + 10] = 0x66; font[117 * 16 + 11] = 0x66; font[117 * 16 + 12] = 0x3E;

    // v (118) - lowercase v
    font[118 * 16 + 5] = 0x66; font[118 * 16 + 6] = 0x66; font[118 * 16 + 7] = 0x66; font[118 * 16 + 8] = 0x66;
    font[118 * 16 + 9] = 0x66; font[118 * 16 + 10] = 0x3C; font[118 * 16 + 11] = 0x18; font[118 * 16 + 12] = 0x00;

    // w (119) - lowercase w
    font[119 * 16 + 5] = 0x66; font[119 * 16 + 6] = 0x66; font[119 * 16 + 7] = 0x66; font[119 * 16 + 8] = 0x66;
    font[119 * 16 + 9] = 0x6E; font[119 * 16 + 10] = 0x7E; font[119 * 16 + 11] = 0x36; font[119 * 16 + 12] = 0x66;

    // k (107) - lowercase k
    font[107 * 16 + 3] = 0x60; font[107 * 16 + 4] = 0x60; font[107 * 16 + 5] = 0x66; font[107 * 16 + 6] = 0x6C;
    font[107 * 16 + 7] = 0x78; font[107 * 16 + 8] = 0x70; font[107 * 16 + 9] = 0x78; font[107 * 16 + 10] = 0x6C;
    font[107 * 16 + 11] = 0x66; font[107 * 16 + 12] = 0x60;

    // ? (63) - question mark
    font[63 * 16 + 3] = 0x3C; font[63 * 16 + 4] = 0x66; font[63 * 16 + 5] = 0x06; font[63 * 16 + 6] = 0x0C;
    font[63 * 16 + 7] = 0x18; font[63 * 16 + 8] = 0x18; font[63 * 16 + 9] = 0x00; font[63 * 16 + 10] = 0x18;

    // < (60) - less than
    font[60 * 16 + 6] = 0x0E; font[60 * 16 + 7] = 0x18; font[60 * 16 + 8] = 0x30; font[60 * 16 + 9] = 0x18; font[60 * 16 + 10] = 0x0E;

    // > (62) - greater than
    font[62 * 16 + 6] = 0x70; font[62 * 16 + 7] = 0x18; font[62 * 16 + 8] = 0x0C; font[62 * 16 + 9] = 0x18; font[62 * 16 + 10] = 0x70;

    // Box-drawing and graphics characters (using low ASCII range 1-31)
    // These are custom graphics characters for displaying the banner properly

    // ═ (1) - double horizontal line
    font[1 * 16 + 7] = 0xFF; font[1 * 16 + 8] = 0xFF;

    // ║ (2) - double vertical line
    font[2 * 16 + 2] = 0x18; font[2 * 16 + 3] = 0x18; font[2 * 16 + 4] = 0x18; font[2 * 16 + 5] = 0x18;
    font[2 * 16 + 6] = 0x18; font[2 * 16 + 7] = 0x18; font[2 * 16 + 8] = 0x18; font[2 * 16 + 9] = 0x18;
    font[2 * 16 + 10] = 0x18; font[2 * 16 + 11] = 0x18; font[2 * 16 + 12] = 0x18;

    // ╔ (3) - double top-left corner
    font[3 * 16 + 7] = 0x1F; font[3 * 16 + 8] = 0x18; font[3 * 16 + 9] = 0x18; font[3 * 16 + 10] = 0x18;
    font[3 * 16 + 11] = 0x18; font[3 * 16 + 12] = 0x18;

    // ╗ (4) - double top-right corner
    font[4 * 16 + 7] = 0xF8; font[4 * 16 + 8] = 0x18; font[4 * 16 + 9] = 0x18; font[4 * 16 + 10] = 0x18;
    font[4 * 16 + 11] = 0x18; font[4 * 16 + 12] = 0x18;

    // ╚ (5) - double bottom-left corner
    font[5 * 16 + 2] = 0x18; font[5 * 16 + 3] = 0x18; font[5 * 16 + 4] = 0x18; font[5 * 16 + 5] = 0x18;
    font[5 * 16 + 6] = 0x18; font[5 * 16 + 7] = 0x1F;

    // ╝ (6) - double bottom-right corner
    font[6 * 16 + 2] = 0x18; font[6 * 16 + 3] = 0x18; font[6 * 16 + 4] = 0x18; font[6 * 16 + 5] = 0x18;
    font[6 * 16 + 6] = 0x18; font[6 * 16 + 7] = 0xF8;

    // ╩ (7) - double T junction up
    font[7 * 16 + 2] = 0x18; font[7 * 16 + 3] = 0x18; font[7 * 16 + 4] = 0x18; font[7 * 16 + 5] = 0x18;
    font[7 * 16 + 6] = 0x18; font[7 * 16 + 7] = 0xFF;

    // ╦ (8) - double T junction down
    font[8 * 16 + 7] = 0xFF; font[8 * 16 + 8] = 0x18; font[8 * 16 + 9] = 0x18; font[8 * 16 + 10] = 0x18;
    font[8 * 16 + 11] = 0x18; font[8 * 16 + 12] = 0x18;

    // ╠ (9) - double T junction right
    font[9 * 16 + 2] = 0x18; font[9 * 16 + 3] = 0x18; font[9 * 16 + 4] = 0x18; font[9 * 16 + 5] = 0x18;
    font[9 * 16 + 6] = 0x18; font[9 * 16 + 7] = 0x1F; font[9 * 16 + 8] = 0x18; font[9 * 16 + 9] = 0x18;
    font[9 * 16 + 10] = 0x18; font[9 * 16 + 11] = 0x18; font[9 * 16 + 12] = 0x18;

    // ╣ (10) - double T junction left
    font[10 * 16 + 2] = 0x18; font[10 * 16 + 3] = 0x18; font[10 * 16 + 4] = 0x18; font[10 * 16 + 5] = 0x18;
    font[10 * 16 + 6] = 0x18; font[10 * 16 + 7] = 0xF8; font[10 * 16 + 8] = 0x18; font[10 * 16 + 9] = 0x18;
    font[10 * 16 + 10] = 0x18; font[10 * 16 + 11] = 0x18; font[10 * 16 + 12] = 0x18;

    // █ (11) - full block
    font[11 * 16 + 0] = 0xFF; font[11 * 16 + 1] = 0xFF; font[11 * 16 + 2] = 0xFF; font[11 * 16 + 3] = 0xFF;
    font[11 * 16 + 4] = 0xFF; font[11 * 16 + 5] = 0xFF; font[11 * 16 + 6] = 0xFF; font[11 * 16 + 7] = 0xFF;
    font[11 * 16 + 8] = 0xFF; font[11 * 16 + 9] = 0xFF; font[11 * 16 + 10] = 0xFF; font[11 * 16 + 11] = 0xFF;
    font[11 * 16 + 12] = 0xFF; font[11 * 16 + 13] = 0xFF; font[11 * 16 + 14] = 0xFF; font[11 * 16 + 15] = 0xFF;

    // Missing characters
    // ` (96) - backtick/grave accent
    font[96 * 16 + 2] = 0x30; font[96 * 16 + 3] = 0x18;

    // x (120) - lowercase x
    font[120 * 16 + 5] = 0x66; font[120 * 16 + 6] = 0x66; font[120 * 16 + 7] = 0x3C; font[120 * 16 + 8] = 0x18;
    font[120 * 16 + 9] = 0x3C; font[120 * 16 + 10] = 0x66; font[120 * 16 + 11] = 0x66; font[120 * 16 + 12] = 0x66;

    font
}

/// Create a synthetic SFNT header with minimal data for testing
/// This creates a font-like structure that can be parsed by our TTF reader
pub fn include_fira_code_ttf() -> &'static [u8] {
    // For now, return a minimal valid TTF structure
    // This creates a working TTF header that our parser can handle
    static TTF_DATA: [u8; 256] = [
        // SFNT header (12 bytes)
        0x00, 0x01, 0x00, 0x00, // sfnt_version (TrueType)
        0x00, 0x04,             // num_tables
        0x00, 0x40,             // search_range
        0x00, 0x02,             // entry_selector
        0x00, 0x00,             // range_shift

        // Table directory entries (16 bytes each)
        // head table
        0x68, 0x65, 0x61, 0x64, // 'head'
        0x00, 0x00, 0x00, 0x00, // checksum
        0x00, 0x00, 0x00, 0x50, // offset
        0x00, 0x00, 0x00, 0x36, // length

        // hhea table
        0x68, 0x68, 0x65, 0x61, // 'hhea'
        0x00, 0x00, 0x00, 0x00, // checksum
        0x00, 0x00, 0x00, 0x90, // offset
        0x00, 0x00, 0x00, 0x24, // length

        // cmap table
        0x63, 0x6D, 0x61, 0x70, // 'cmap'
        0x00, 0x00, 0x00, 0x00, // checksum
        0x00, 0x00, 0x00, 0xB4, // offset
        0x00, 0x00, 0x00, 0x20, // length

        // glyf table
        0x67, 0x6C, 0x79, 0x66, // 'glyf'
        0x00, 0x00, 0x00, 0x00, // checksum
        0x00, 0x00, 0x00, 0xD4, // offset
        0x00, 0x00, 0x00, 0x40, // length

        // head table data (54 bytes at offset 0x50)
        0x00, 0x01, 0x00, 0x00, // table_version
        0x00, 0x01, 0x00, 0x00, // font_revision
        0x5F, 0x0F, 0x3C, 0xF5, // checksum_adjustment
        0x5F, 0x0F, 0x3C, 0xF5, // magic_number
        0x00, 0x0B,             // flags
        0x04, 0x00,             // units_per_em (1024)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // created (8 bytes)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // modified (8 bytes)
        0x00, 0x00,             // x_min
        0x00, 0x00,             // y_min
        0x04, 0x00,             // x_max
        0x04, 0x00,             // y_max
        0x00, 0x00,             // mac_style
        0x00, 0x08,             // lowest_rec_ppem
        0x00, 0x02,             // font_direction_hint
        0x00, 0x00,             // index_to_loc_format
        0x00, 0x00,             // glyph_data_format

        // hhea table data (36 bytes at offset 0x90)
        0x00, 0x01, 0x00, 0x00, // table_version
        0x03, 0x20,             // ascent (800)
        0xFC, 0xE0,             // descent (-800)
        0x00, 0x64,             // line_gap (100)
        0x04, 0x00,             // advance_width_max
        0x00, 0x00,             // min_left_side_bearing
        0x00, 0x00,             // min_right_side_bearing
        0x04, 0x00,             // x_max_extent
        0x00, 0x01,             // caret_slope_rise
        0x00, 0x00,             // caret_slope_run
        0x00, 0x00,             // caret_offset
        0x00, 0x00, 0x00, 0x00, // reserved (4 x 2 bytes)
        0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,             // metric_data_format
        0x00, 0x01,             // number_of_h_metrics

        // Padding to fill remaining space to reach 256 bytes
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];
    &TTF_DATA
}

/// Enhanced glyph generation for common characters using vector outlines
pub fn create_glyph_outline(ch: char) -> GlyphOutline {
    match ch {
        // Basic shapes for common characters
        'A' => GlyphOutline {
            contours: vec![
                Contour {
                    points: vec![
                        GlyphPoint { x: 200.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 400.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 600.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 450.0, y: 200.0, on_curve: true },
                        GlyphPoint { x: 350.0, y: 200.0, on_curve: true },
                        GlyphPoint { x: 300.0, y: 0.0, on_curve: true },
                    ],
                },
            ],
            advance_width: 800,
        },
        'B' => GlyphOutline {
            contours: vec![
                Contour {
                    points: vec![
                        GlyphPoint { x: 100.0, y: 0.0, on_curve: true },
                        GlyphPoint { x: 100.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 400.0, y: 700.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 600.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 400.0, on_curve: true },
                        GlyphPoint { x: 450.0, y: 350.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 300.0, on_curve: true },
                        GlyphPoint { x: 500.0, y: 100.0, on_curve: true },
                        GlyphPoint { x: 400.0, y: 0.0, on_curve: true },
                    ],
                },
            ],
            advance_width: 600,
        },
        _ => {
            // Generate simple rectangle for other characters
            GlyphOutline {
                contours: vec![
                    Contour {
                        points: vec![
                            GlyphPoint { x: 100.0, y: 100.0, on_curve: true },
                            GlyphPoint { x: 500.0, y: 100.0, on_curve: true },
                            GlyphPoint { x: 500.0, y: 600.0, on_curve: true },
                            GlyphPoint { x: 100.0, y: 600.0, on_curve: true },
                        ],
                    },
                ],
                advance_width: 600,
            }
        }
    }
}