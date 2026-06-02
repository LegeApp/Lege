//! Utility functions for the JBIG2 encoder

use crate::jbig2sym::BitImage;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub struct ComponentBox {
    pub min_x: usize,
    pub min_y: usize,
    pub max_x: usize,
    pub max_y: usize,
}

impl ComponentBox {
    #[inline]
    pub fn width(self) -> usize {
        self.max_x - self.min_x + 1
    }

    #[inline]
    pub fn height(self) -> usize {
        self.max_y - self.min_y + 1
    }

    #[inline]
    pub fn area(self) -> usize {
        self.width() * self.height()
    }
}

#[derive(Debug, Clone)]
pub struct TinyComponent {
    pub pixels: Vec<(usize, usize)>,
    pub bbox: ComponentBox,
    pub pixel_count: usize,
}

// ==============================================
// Type conversion utilities
// ==============================================

/// Safely convert from u32 to usize with a panic if the value is too large.
#[inline]
pub fn u32_to_usize(x: u32) -> usize {
    x as usize
}

/// Safely convert from usize to u32 with a panic if the value is too large.
#[inline]
pub fn usize_to_u32(x: usize) -> u32 {
    u32::try_from(x).expect("value exceeds u32 range")
}

// ==============================================
// Debug utilities
// (Moved from jbig2sym.rs)
// ==============================================

/// Save a BitImage to a PBM file in the debug directory
/// Only saves files in debug builds (when not built with --release)
pub fn save_debug_pbm(image: &BitImage, filename: &str) -> std::io::Result<()> {
    if cfg!(debug_assertions) {
        let debug_dir = Path::new("debug-output");
        if !debug_dir.exists() {
            fs::create_dir_all(debug_dir)?;
        }

        let path = debug_dir.join(filename);
        let mut file = File::create(&path)?;

        // Write PBM header
        writeln!(&mut file, "P4\n{} {}\n", image.width, image.height)?;

        // Write image data
        file.write_all(&image.to_jbig2_format())?;
    }

    Ok(())
}

pub fn extract_symbol_components(symbol: &BitImage) -> Vec<TinyComponent> {
    let w = symbol.width;
    let h = symbol.height;
    if w == 0 || h == 0 {
        return Vec::new();
    }

    let mut visited = vec![false; w * h];
    let mut out = Vec::new();
    let idx = |x: usize, y: usize| y * w + x;

    for y in 0..h {
        for x in 0..w {
            if visited[idx(x, y)] || !symbol.get_usize(x, y) {
                continue;
            }

            let mut stack = vec![(x, y)];
            visited[idx(x, y)] = true;
            let mut pixels = Vec::new();
            let mut min_x = x;
            let mut max_x = x;
            let mut min_y = y;
            let mut max_y = y;

            while let Some((cx, cy)) = stack.pop() {
                pixels.push((cx, cy));
                min_x = min_x.min(cx);
                max_x = max_x.max(cx);
                min_y = min_y.min(cy);
                max_y = max_y.max(cy);

                let x0 = cx.saturating_sub(1);
                let y0 = cy.saturating_sub(1);
                let x1 = (cx + 1).min(w - 1);
                let y1 = (cy + 1).min(h - 1);

                for ny in y0..=y1 {
                    for nx in x0..=x1 {
                        let i = idx(nx, ny);
                        if visited[i] || !symbol.get_usize(nx, ny) {
                            continue;
                        }
                        visited[i] = true;
                        stack.push((nx, ny));
                    }
                }
            }

            out.push(TinyComponent {
                pixel_count: pixels.len(),
                pixels,
                bbox: ComponentBox {
                    min_x,
                    min_y,
                    max_x,
                    max_y,
                },
            });
        }
    }

    out
}

#[inline]
pub fn symbol_likely_punctuation_or_mark(symbol: &BitImage) -> bool {
    let w = symbol.width;
    let h = symbol.height;
    let black = symbol.count_ones();

    (w <= 4 && h <= 4 && black >= 1)
        || (w <= 3 && h <= 6 && black <= 10)
        || (w <= 4 && h <= 10 && black <= 20)
        || (w <= 8 && h <= 6 && black <= 24)
}

#[inline]
fn looks_like_tiny_terminal_punctuation(symbol: &BitImage, comp: &TinyComponent) -> bool {
    let bw = comp.bbox.width();
    let bh = comp.bbox.height();

    if bw <= 3 && bh <= 3 && comp.pixel_count >= 1 {
        return true;
    }

    if bw <= 2 && bh <= 5 && comp.pixel_count >= 2 {
        return true;
    }

    if comp.bbox.max_y + 1 >= symbol.height.saturating_sub(1)
        && bw <= 4
        && bh <= 4
        && comp.pixel_count <= 8
    {
        return true;
    }

    false
}

#[inline]
fn looks_like_dot_above_stem(
    symbol: &BitImage,
    comp_index: usize,
    components: &[TinyComponent],
) -> bool {
    let comp = &components[comp_index];
    let bw = comp.bbox.width();
    let bh = comp.bbox.height();

    if bw > 4 || bh > 4 || comp.pixel_count > 10 {
        return false;
    }
    if comp.bbox.max_y > symbol.height / 2 {
        return false;
    }

    let cx = (comp.bbox.min_x + comp.bbox.max_x) / 2;
    for (other_index, other) in components.iter().enumerate() {
        if other_index == comp_index {
            continue;
        }
        let ow = other.bbox.width();
        let oh = other.bbox.height();
        if other.bbox.min_y <= comp.bbox.max_y {
            continue;
        }
        if oh < 4 || ow > 6 {
            continue;
        }
        let ox0 = other.bbox.min_x.saturating_sub(2);
        let ox1 = other.bbox.max_x + 2;
        if cx >= ox0 && cx <= ox1 {
            return true;
        }
    }

    false
}

#[inline]
fn looks_like_upper_diacritic_component(
    symbol: &BitImage,
    comp_index: usize,
    components: &[TinyComponent],
) -> bool {
    let comp = &components[comp_index];
    let bw = comp.bbox.width();
    let bh = comp.bbox.height();

    if bw > 8 || bh > 5 || comp.pixel_count > 24 {
        return false;
    }
    if comp.bbox.max_y > (symbol.height * 2) / 5 {
        return false;
    }

    let center_x = (comp.bbox.min_x + comp.bbox.max_x) / 2;
    for (other_index, other) in components.iter().enumerate() {
        if other_index == comp_index {
            continue;
        }
        let ow = other.bbox.width();
        let oh = other.bbox.height();
        if other.bbox.min_y <= comp.bbox.max_y {
            continue;
        }
        if oh < 4 || ow < 2 {
            continue;
        }
        let ox0 = other.bbox.min_x.saturating_sub(3);
        let ox1 = other.bbox.max_x + 3;
        if center_x >= ox0 && center_x <= ox1 {
            return true;
        }
    }

    false
}

#[inline]
fn looks_like_lower_diacritic_component(
    symbol: &BitImage,
    comp_index: usize,
    components: &[TinyComponent],
) -> bool {
    let comp = &components[comp_index];
    let bw = comp.bbox.width();
    let bh = comp.bbox.height();

    if bw > 8 || bh > 6 || comp.pixel_count > 24 {
        return false;
    }
    if comp.bbox.min_y < (symbol.height * 3) / 5 {
        return false;
    }

    let center_x = (comp.bbox.min_x + comp.bbox.max_x) / 2;
    for (other_index, other) in components.iter().enumerate() {
        if other_index == comp_index {
            continue;
        }
        let ow = other.bbox.width();
        let oh = other.bbox.height();
        if other.bbox.max_y >= comp.bbox.min_y {
            continue;
        }
        if oh < 4 || ow < 2 {
            continue;
        }
        let ox0 = other.bbox.min_x.saturating_sub(3);
        let ox1 = other.bbox.max_x + 3;
        if center_x >= ox0 && center_x <= ox1 {
            return true;
        }
    }

    false
}

pub fn denoise_symbol_preserving_marks(symbol: &BitImage, preserve_exact: bool) -> BitImage {
    if preserve_exact || symbol.width == 0 || symbol.height == 0 {
        return symbol.clone();
    }

    let components = extract_symbol_components(symbol);
    if components.is_empty() {
        return symbol.clone();
    }

    let mut cleaned = symbol.clone();
    for (i, comp) in components.iter().enumerate() {
        let keep = if components.len() == 1 {
            true
        } else if looks_like_tiny_terminal_punctuation(symbol, comp) {
            true
        } else if looks_like_dot_above_stem(symbol, i, &components) {
            true
        } else if looks_like_upper_diacritic_component(symbol, i, &components) {
            true
        } else if looks_like_lower_diacritic_component(symbol, i, &components) {
            true
        } else {
            let bw = comp.bbox.width();
            let bh = comp.bbox.height();
            let near_edge = comp.bbox.min_x == 0
                || comp.bbox.min_y == 0
                || comp.bbox.max_x + 1 >= symbol.width
                || comp.bbox.max_y + 1 >= symbol.height;
            !((comp.pixel_count <= 2 && bw <= 2 && bh <= 2)
                || (near_edge && comp.pixel_count <= 3 && bw <= 2 && bh <= 2))
        };

        if !keep {
            for &(x, y) in &comp.pixels {
                cleaned.set_usize(x, y, false);
            }
        }
    }

    let (_, trimmed) = cleaned.trim();
    trimmed
}

// (PDF helpers removed - this crate no longer creates PDFs directly.)

pub mod jbig2wrapper {

    pub fn push_file_header(out: &mut Vec<u8>) {
        out.extend_from_slice(&[0x97, 0x4A, 0x42, 0x32, 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    pub fn push_page_info(out: &mut Vec<u8>, width: u32, height: u32) {
        // Segment header for Page Information Segment (Section 7.4.1)
        // Segment number (arbitrary, but 1 for first page info)
        out.extend_from_slice(&0u32.to_be_bytes());
        // Page Information Segment type (0x00)
        out.push(0x00);
        // Page Information Segment flags (Section 7.4.1.1)
        // Bit 7: Default Pixel Value (0 = black, 1 = white) - set to 1 for white
        // Bit 6: Page Striping (0 = no striping, 1 = striping) - set to 0
        // Bits 5-0: Page X-Resolution and Y-Resolution (0 = no resolution specified)
        out.push(0b10000000); // Flags1: DP=1, PS=0, R=0
        out.push(0x00); // Flags2: Reserved, set to 0

        // Page width and height
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());

        // X and Y resolution (0 = no resolution specified)
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());

        // Page segments (number of segments associated with this page)
        // For a single page with one generic region and EOF, this is 2
        out.extend_from_slice(&2u32.to_be_bytes());
    }

    pub fn push_eof(out: &mut Vec<u8>, segment_number: u32) {
        // Segment header for End of File Segment (Section 7.4.2)
        out.extend_from_slice(&segment_number.to_be_bytes());
        out.push(0x02); // End of File Segment type (0x02)
        out.extend_from_slice(&0u16.to_be_bytes()); // Flags: Reserved, set to 0
        out.extend_from_slice(&0u32.to_be_bytes()); // Segment page association: 0 for global
        out.extend_from_slice(&0u32.to_be_bytes()); // Segment data length: 0 for EOF
    }
}
