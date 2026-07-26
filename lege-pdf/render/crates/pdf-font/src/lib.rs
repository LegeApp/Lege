//! Font semantics: encodings, CMaps, and the font-program abstraction.
//!
//! The concurrency plan's three-way split is structural here from day one:
//!
//! - [`FontProgram`]: immutable bytes + parsed tables — shareable.
//! - `FontInstance` (worker-owned, defined with the raster work): size,
//!   transform, hinting — NEVER shared across workers.
//! - Glyph caches: worker-local or explicitly synchronized — later phases.
//!
//! Never one mutex around a font engine; that just relocates PDFium's
//! bottleneck.

use std::sync::Arc;

use pdf_object::ObjectId;

mod agl_table;
pub mod cff;
pub mod cid;
pub mod cmap;
mod cmap_tables;
pub mod encoding;
pub mod engine;
pub mod metrics;
pub mod standard;
pub mod system;
pub mod type1;
pub mod unicode;
pub mod widths;

pub use cff::{cid_to_gid_from_cff, is_bare_cff, wrap_bare_cff};
pub use cid::{CidToGid, CidVerticalMetrics, CidWidths, GlyphMap};
pub use cmap::{CMap, parse_embedded_cmap, predefined_cmap};
pub use cmap_tables::cid_to_unicode;
pub use encoding::{BaseEncoding, builtin_glyph_name, glyph_name_to_char};
pub use engine::{AUTO_HINT_MAX_PPEM, FontProgram, HintingPolicy, Outline, OutlineVerb};
pub use metrics::{DecodedCode, FontMetrics, VerticalPlacement};
pub use standard::{
    Family, StandardFont, SubstitutionRequest, Synthesis, standard_by_name, strip_subset_tag,
    substitute, substitute_with_style, synthesis,
};
pub use system::{
    Charset, FolderFontProvider, SystemFont, SystemFontProvider, SystemFontRequest,
    default_font_paths,
};
pub use unicode::{UnicodeMap, UnicodeMapping, UnicodeSource, parse_to_unicode};
pub use widths::SimpleWidths;

/// Glyph index within a font program (not a character code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphId(pub u32);

/// A character code as it appears in a content stream string (1–4 bytes
/// depending on the encoding/CMap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CharCode(pub u32);

/// The PDF font subtypes (ISO 32000-1 §9.5–9.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontKind {
    Type1,
    TrueType,
    Type0,
    Type3,
    Mm1, // MMType1
    CidFontType0,
    CidFontType2,
}

/// Semantic description of a font as referenced by a page: enough for the
/// content interpreter to map codes → glyphs + widths without any raster
/// state. The parsed outline program lives in [`engine::FontProgram`].
#[derive(Debug)]
pub struct FontDesc {
    pub kind: FontKind,
    pub object: ObjectId,
    pub program: Option<Arc<engine::FontProgram>>,
    // Later: encoding, ToUnicode, CIDToGIDMap, descendant info.
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum FontError {
    #[error("unsupported font kind {0:?}")]
    Unsupported(FontKind),
    #[error("malformed font dictionary: {0}")]
    Malformed(&'static str),
    #[error("font program parse failure: {0}")]
    BadProgram(&'static str),
    #[error("no glyph for code {0:?}")]
    NoGlyph(CharCode),
}
