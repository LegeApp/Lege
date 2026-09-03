//! PdfPageArtifact: the page contract submitted by pipeline workers.
//! PdfImageElement, PdfImageResource (Jpeg/Jpx/Jbig2/CcittGroup4/Indexed8),
//! PreparedTextLayer, PreparedGlyphLayer, exact resident_bytes(). PLAN.md §4.2.
//!
//! This is the *entire* public input vocabulary. Every variant maps to one PDF
//! image dictionary shape (see `images.rs`). Adding a variant is a plan change,
//! not a drop-in — invalid combinations are made unrepresentable here.
//!
//! `PreparedGlyphLayer` is the visible-text counterpart of the invisible OCR
//! layer: glyph ids into the document-wide glyph font (`/F2`, see
//! `DocumentWriter::set_glyph_font`), positioned per line in PDF user space.

use std::sync::Arc;

use crate::resources::SharedResourceId;
use crate::types::{Affine, PdfRect, ResourceName};

/// A fully processed page, ready for the writer. Submitted in any completion
/// order; the writer records its object id by `index`.
#[derive(Debug)]
pub struct PdfPageArtifact {
    /// Logical (0-based) page index; establishes order in the final page tree.
    pub index: u32,
    /// Page boundary in points.
    pub media_box: PdfRect,
    /// Draw order = paint order (background element before any stencil mask).
    pub elements: Box<[PdfImageElement]>,
    /// Invisible OCR text layer, if any (emitted from M2 onward).
    pub text_layer: Option<PreparedTextLayer>,
    /// Visible glyph-font text (the page's printed text as text objects drawn
    /// with the document-wide glyph font), if any.
    pub glyph_layer: Option<PreparedGlyphLayer>,
    /// How the reader should turn the page for display (`/Rotate`). The page's
    /// own content stays in the coordinates it was written in.
    pub rotation: PageRotation,
}

/// The `/Rotate` entry: how far clockwise a reader turns the page before
/// displaying it. Scanned pages keep the orientation they were scanned in;
/// this says which way is up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PageRotation {
    /// No `/Rotate` entry is written.
    #[default]
    Upright,
    Clockwise90,
    Half,
    Clockwise270,
}

impl PageRotation {
    /// Quarter turns clockwise, 0-3. Values outside that range wrap.
    pub fn from_quarter_turns(turns: u8) -> Self {
        match turns % 4 {
            1 => Self::Clockwise90,
            2 => Self::Half,
            3 => Self::Clockwise270,
            _ => Self::Upright,
        }
    }

    /// The `/Rotate` value in degrees.
    pub fn degrees(self) -> u16 {
        match self {
            Self::Upright => 0,
            Self::Clockwise90 => 90,
            Self::Half => 180,
            Self::Clockwise270 => 270,
        }
    }
}

impl PdfPageArtifact {
    /// Bytes uniquely owned by this artifact. Shared resources (JBIG2 globals,
    /// fonts) are budgeted once by the registry, so they are excluded here.
    pub fn resident_bytes(&self) -> u64 {
        let mut total = 0u64;
        for el in self.elements.iter() {
            total += el.image.owned_bytes();
        }
        if let Some(tl) = &self.text_layer {
            for run in tl.runs.iter() {
                total += run.text.len() as u64;
            }
        }
        if let Some(gl) = &self.glyph_layer {
            for line in gl.lines.iter() {
                total += (line.items.len() * std::mem::size_of::<GlyphItem>()) as u64;
            }
        }
        total
    }
}

/// One image placed on the page under an affine transform.
#[derive(Debug)]
pub struct PdfImageElement {
    /// The `cm` matrix mapping the unit image square to page space.
    pub transform: Affine,
    pub image: PdfImageResource,
}

/// 8-bit continuous-tone color model for JPEG/JPX payloads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorModel {
    Rgb,
    Gray,
}

/// The closed set of encoded image payloads Lege emits. Bytes are always
/// already-encoded and never re-encoded by the writer.
#[derive(Debug)]
pub enum PdfImageResource {
    /// Baseline JPEG (DCTDecode).
    Jpeg {
        data: Arc<[u8]>,
        width: u32,
        height: u32,
        color: ColorModel,
    },
    /// JPEG 2000 (JPXDecode).
    Jpx {
        data: Arc<[u8]>,
        width: u32,
        height: u32,
        color: ColorModel,
    },
    /// JBIG2 bilevel. `globals` references shared symbol data via the registry;
    /// `image_mask` selects a stencil mask (painted with the current fill).
    Jbig2 {
        data: Arc<[u8]>,
        width: u32,
        height: u32,
        globals: Option<SharedResourceId>,
        image_mask: bool,
        /// For stencil masks, paint decoded sample 1 (`/Decode [1 0]`) rather
        /// than decoded sample 0 (`/Decode [0 1]`). Ignored for opaque images.
        image_mask_paints_one: bool,
    },
    /// CCITT Group 4 (T.6) bilevel.
    CcittGroup4 {
        data: Arc<[u8]>,
        width: u32,
        height: u32,
        /// True when a set bit is black (Lege's encoder emits 1=black).
        black_is_one: bool,
    },
    /// 8-bit indexed color with an RGB palette; palette and indices are already
    /// split (no 768-byte-prefix convention).
    Indexed8 {
        palette: Arc<[u8]>,
        indices: Arc<[u8]>,
        width: u32,
        height: u32,
    },
}

impl PdfImageResource {
    pub fn width(&self) -> u32 {
        match self {
            PdfImageResource::Jpeg { width, .. }
            | PdfImageResource::Jpx { width, .. }
            | PdfImageResource::Jbig2 { width, .. }
            | PdfImageResource::CcittGroup4 { width, .. }
            | PdfImageResource::Indexed8 { width, .. } => *width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            PdfImageResource::Jpeg { height, .. }
            | PdfImageResource::Jpx { height, .. }
            | PdfImageResource::Jbig2 { height, .. }
            | PdfImageResource::CcittGroup4 { height, .. }
            | PdfImageResource::Indexed8 { height, .. } => *height,
        }
    }

    /// True for a stencil mask that must be painted with a fill color rather
    /// than drawn as an opaque image.
    pub fn is_image_mask(&self) -> bool {
        matches!(
            self,
            PdfImageResource::Jbig2 {
                image_mask: true,
                ..
            }
        )
    }

    /// Bytes uniquely owned by this resource (shared globals excluded).
    fn owned_bytes(&self) -> u64 {
        match self {
            PdfImageResource::Jpeg { data, .. }
            | PdfImageResource::Jpx { data, .. }
            | PdfImageResource::Jbig2 { data, .. }
            | PdfImageResource::CcittGroup4 { data, .. } => data.len() as u64,
            PdfImageResource::Indexed8 {
                palette, indices, ..
            } => (palette.len() + indices.len()) as u64,
        }
    }
}

/// A prepared, PDF-space invisible text layer. hOCR parsing, line grouping, and
/// the Y flip all happen in Lege before this point; the writer only emits.
#[derive(Debug)]
pub struct PreparedTextLayer {
    pub runs: Box<[TextRun]>,
    pub font: TextFont,
}

/// One positioned text run (already in PDF user space, bottom-left origin).
#[derive(Debug)]
pub struct TextRun {
    pub text: String,
    pub x: f64,
    pub y: f64,
    /// Font size (text-matrix scale); Lege uses the word height, clamped ≥ 1.0.
    pub size: f64,
}

/// A prepared, PDF-space visible glyph layer. Line grouping, baseline
/// estimation, and the Y flip all happen in Lege; the writer only emits.
#[derive(Debug)]
pub struct PreparedGlyphLayer {
    pub lines: Box<[GlyphLine]>,
    /// Which glyph font the page's ids index (see [`glyph_font_resource`]).
    /// A page draws from one font; a document that outgrows one font's
    /// 16-bit id space carries several, and later pages move on to the next.
    pub font: u16,
}

/// One text line: a text matrix (scale + baseline origin, already in PDF user
/// space) followed by a `TJ` array of glyph ids with positioning adjustments.
#[derive(Debug)]
pub struct GlyphLine {
    /// The `Tm` for this line. The font is selected at size 1, so the matrix
    /// scale is the em size in points.
    pub matrix: Affine,
    pub items: Box<[GlyphItem]>,
}

/// One glyph occurrence inside a line. Units are thousandths of a text-space
/// unit (the `TJ` convention), i.e. glyph-font units at 1000 units per em.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphItem {
    /// Glyph id (CID under Identity-H; `CIDToGIDMap /Identity`).
    pub gid: u16,
    /// `TJ` adjustment applied *before* this glyph. Positive moves the pen
    /// left, negative moves it right (PDF 32000-1 §9.4.3).
    pub adjust: i32,
    /// Text rise (`Ts`) for this glyph, in thousandths of a text-space unit.
    /// Zero for glyphs that sit where the font's own baseline puts them.
    pub rise: i32,
}

/// Which font resource a text layer draws with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextFont {
    /// The embedded glyphless Type0 font (Identity-H, UTF-16BE), resource `/F0`.
    Embedded,
    /// The non-embedded Helvetica fallback (WINDOWS-1252), resource `/F1`.
    HelveticaFallback,
}

impl TextFont {
    pub fn resource_name(&self) -> ResourceName {
        match self {
            TextFont::Embedded => ResourceName::Font(0),
            TextFont::HelveticaFallback => ResourceName::Font(1),
        }
    }
}

/// The page resource name of the document-wide glyph font (`/F2`).
/// The page resource name of glyph font `bank`. One font holds at most
/// 65536 glyph ids, so a document with more shapes than that carries several
/// (`/F2`, `/F3`, …), each page drawing from exactly one.
pub fn glyph_font_resource(bank: u16) -> ResourceName {
    ResourceName::Font(2u16.saturating_add(bank))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_bytes_counts_owned_only() {
        let art = PdfPageArtifact {
            index: 0,
            media_box: PdfRect::from_size(100.0, 100.0),
            elements: Box::new([PdfImageElement {
                transform: Affine::IDENTITY,
                image: PdfImageResource::Jbig2 {
                    data: Arc::from(&[0u8; 500][..]),
                    width: 10,
                    height: 10,
                    globals: Some(SharedResourceId(7)),
                    image_mask: false,
                    image_mask_paints_one: false,
                },
            }]),
            text_layer: None,
            glyph_layer: None,
            rotation: PageRotation::Upright,
        };
        // Globals excluded; only the 500-byte page payload counts.
        assert_eq!(art.resident_bytes(), 500);
    }

    #[test]
    fn indexed8_counts_palette_and_indices() {
        let img = PdfImageResource::Indexed8 {
            palette: Arc::from(&[0u8; 768][..]),
            indices: Arc::from(&[0u8; 100][..]),
            width: 10,
            height: 10,
        };
        assert_eq!(img.owned_bytes(), 868);
    }
}
