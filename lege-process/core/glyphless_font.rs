//! Minimal glyphless TrueType font for the invisible OCR/searchable text layer.
//!
//! The text layer is drawn with text-render-mode 3 (never rasterized) and its
//! Unicode is carried by a ToUnicode CMap, so the embedded font program needs
//! no real glyph outlines — it exists only to satisfy PDF's requirement that a
//! CIDFontType2 reference a font. This generates a ~1 KB font with two empty
//! glyphs (`.notdef` + one blank) instead of embedding a ~1 MB system font.
//!
//! This is the same approach OCRmyPDF / Tesseract use for their text layer.
//! `units_per_em` is 1000 so the descendant font's `DW 1000` matches. The
//! table assembly itself lives in `truetype_writer`, shared with the visible
//! glyph font.

use crate::truetype_writer::{GlyphOutline, TrueTypeSpec, build_truetype};

/// Fixed metrics for the glyphless font (units_per_em = 1000).
pub const UNITS_PER_EM: u16 = 1000;
pub const ASCENT: i16 = 800;
pub const DESCENT: i16 = -200;
pub const CAP_HEIGHT: i16 = 700;

/// Build a complete, self-contained glyphless TrueType font.
pub fn build_glyphless_ttf() -> Vec<u8> {
    let glyphs = [GlyphOutline::empty(1000), GlyphOutline::empty(1000)];
    build_truetype(&TrueTypeSpec {
        name: "Glyphless",
        units_per_em: UNITS_PER_EM,
        ascent: ASCENT,
        descent: DESCENT,
        cap_height: CAP_HEIGHT,
        glyphs: &glyphs,
    })
    .expect("two empty glyphs always assemble")
    .data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphless_font_is_small_and_parseable() {
        let font = build_glyphless_ttf();
        assert!(
            font.len() < 4096,
            "glyphless font unexpectedly large: {}",
            font.len()
        );
        // The renderer's font parser must accept it -- the same path that reads
        // any other face, and the one a PDF consumer will use on this output.
        let face = lege_pdf_read::read_face_metrics(&font, 0)
            .expect("glyphless font should parse as a real face");
        assert_eq!(face.num_glyphs, 2);
        assert_eq!(face.units_per_em, UNITS_PER_EM);
    }
}
