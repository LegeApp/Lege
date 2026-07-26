//! PDF glyph-advance metrics (fonts.md §3 "PDF metrics").
//!
//! The PDF's own width tables — not the font engine's advances — control text
//! placement, so a substitute font never changes line lengths. Widths are in
//! glyph space (1000 units per em); callers scale by `font_size / 1000`.
//!
//! This module covers **simple fonts** (`/FirstChar`, `/LastChar`, `/Widths`,
//! `/MissingWidth`). CID composite-font widths (`/DW`, `/W`) arrive with the
//! Type 0 work.

/// Advance widths for a simple (single-byte) font, indexed by character code.
#[derive(Debug, Clone, PartialEq)]
pub struct SimpleWidths {
    /// Code of `widths[0]`.
    first_char: u32,
    /// Per-code advance in glyph-space units (1000/em); empty when the font
    /// declares no `/Widths` (e.g. an un-substituted base-14 font).
    widths: Vec<f32>,
    /// Advance for codes outside `[first_char, first_char+widths.len())`.
    missing_width: f32,
    /// Advance used when there is no width table at all — a rough default so
    /// base-14 fonts without `/Widths` still advance rather than overprint.
    /// (Replaced by real AFM/font metrics in the substitution phase.)
    default_width: f32,
}

impl SimpleWidths {
    /// Build from the resolved PDF pieces. `widths` is the `/Widths` array in
    /// glyph-space units; empty if absent.
    pub fn new(first_char: u32, widths: Vec<f32>, missing_width: f32) -> Self {
        Self {
            first_char,
            widths,
            missing_width,
            default_width: 500.0,
        }
    }

    /// An empty table (no `/Widths`): every code advances by the default.
    pub fn empty() -> Self {
        Self {
            first_char: 0,
            widths: Vec::new(),
            missing_width: 0.0,
            default_width: 500.0,
        }
    }

    /// The advance for `code`, in glyph-space units (1000/em).
    pub fn advance(&self, code: u32) -> f32 {
        if self.widths.is_empty() {
            return self.default_width;
        }
        if code >= self.first_char {
            let idx = (code - self.first_char) as usize;
            if idx < self.widths.len() {
                return self.widths[idx];
            }
        }
        self.missing_width
    }

    pub fn has_widths(&self) -> bool {
        !self.widths.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn in_range_and_out_of_range() {
        // Codes 65..=67 have widths 600/700/800; missing = 250.
        let w = SimpleWidths::new(65, vec![600.0, 700.0, 800.0], 250.0);
        assert_eq!(w.advance(65), 600.0);
        assert_eq!(w.advance(67), 800.0);
        assert_eq!(w.advance(64), 250.0, "below range → missing");
        assert_eq!(w.advance(200), 250.0, "above range → missing");
    }

    #[test]
    fn zero_width_is_honored() {
        let w = SimpleWidths::new(0, vec![0.0, 500.0], 100.0);
        assert_eq!(w.advance(0), 0.0, "explicit 0 width is not 'missing'");
    }

    #[test]
    fn empty_uses_default() {
        let w = SimpleWidths::empty();
        assert_eq!(w.advance(42), 500.0);
        assert!(!w.has_widths());
    }
}
