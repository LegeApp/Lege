//! The decoder half of the crate.
//!
//! Implemented in phases per `jbig2decplan.md` §23. Decoder code operates on
//! untrusted PDF input: no input-reachable `unwrap`/`expect`/`assert!`, all
//! allocations checked against `DecodeLimits`.
//!
//! Phase 1 (this milestone) decodes every generic-region stream the encoder
//! emits in its default and lossless configurations — standalone sequential
//! files and bare embedded/PDF segment sequences — exactly. Symbol, halftone,
//! refinement, MMR, and Product-B compatibility features surface as typed
//! `Unsupported` errors.
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod arith;
pub mod context;
pub mod error;
pub mod file;
pub mod generic;
pub mod globals;
pub mod halftone_region;
pub mod huffman;
pub mod iaid;
pub mod integer;
pub mod mmr;
pub mod page;
pub mod pattern_dictionary;
pub mod refinement;
pub mod segment;
pub mod store;
pub mod symbol_dictionary;
pub mod text_region;

pub use context::{DecodeOptions, DecodeStrictness, DecoderContext, RecoveryEvent};
pub use error::{DecodeError, LimitError, ParseError, UnsupportedFeature};
pub use file::{FileOrganization, ParsedDocument, ParsedSegment};
pub use globals::{DecodedGlobals, decode_globals};
pub use page::{DecodedPage, PageInformation};
pub use store::{DecodedSegment, SegmentStore};
pub use symbol_dictionary::SymbolDictionary;

pub use crate::shared::limits::DecodeLimits;

use crate::shared::bitmap::MonoBitmap;

/// A fully decoded document: one bitmap per page, in stream order.
#[derive(Clone, Debug)]
pub struct DecodedDocument {
    pub organization: FileOrganization,
    pub pages: Vec<DecodedPage>,
}

impl DecodedDocument {
    /// The first page's bitmap, if any.
    pub fn first_page(&self) -> Option<&MonoBitmap> {
        self.pages.first().map(|p| &p.bitmap)
    }
}

/// Decode a standalone JBIG2 file (or a bare embedded stream — the organisation
/// is auto-detected from the file magic).
pub fn decode_file(data: &[u8], options: &DecodeOptions) -> Result<DecodedDocument, DecodeError> {
    let mut ctx = DecoderContext::new();
    decode_file_with_context(data, options, &mut ctx)
}

/// Decode a standalone/auto-detected document reusing a worker-local context.
pub fn decode_file_with_context(
    data: &[u8],
    options: &DecodeOptions,
    ctx: &mut DecoderContext,
) -> Result<DecodedDocument, DecodeError> {
    let doc = file::parse_auto_with(data, &options.limits, options.strictness)?;
    let organization = doc.organization;
    let pages = page::process_document(&doc, &options.limits, ctx)?;
    Ok(DecodedDocument {
        organization,
        pages,
    })
}

/// Decode a single PDF-embedded page stream, returning its page bitmap
/// (jbig2decplan.md §5).
///
/// `globals`, when present, is a PDF `JBIG2Globals` byte stream carrying shared
/// symbol dictionaries; it is decoded (once) and its symbols made available to
/// this page's text regions.
pub fn decode_embedded(
    globals: Option<&[u8]>,
    page_data: &[u8],
    options: &DecodeOptions,
) -> Result<MonoBitmap, DecodeError> {
    let mut ctx = DecoderContext::new();
    decode_embedded_with_context(globals, page_data, options, &mut ctx)
}

/// Decode a single embedded page stream reusing a worker-local context. Any
/// `globals` bytes are decoded on every call; prefer
/// [`decode_globals`] + [`decode_embedded_with_globals`] to decode shared
/// globals once and reuse them across pages (jbig2decplan.md §6).
pub fn decode_embedded_with_context(
    globals: Option<&[u8]>,
    page_data: &[u8],
    options: &DecodeOptions,
    ctx: &mut DecoderContext,
) -> Result<MonoBitmap, DecodeError> {
    let decoded_globals = match globals {
        Some(g) if !g.is_empty() => Some(decode_globals(g, options)?),
        _ => None,
    };
    decode_embedded_with_globals(decoded_globals.as_ref(), page_data, options, ctx)
}

/// Decode a single embedded page stream against already-decoded shared globals
/// (jbig2decplan.md §5, §6). The immutable `&DecodedGlobals` may be shared by
/// many page workers concurrently; `ctx` is this worker's mutable scratch.
pub fn decode_embedded_with_globals(
    globals: Option<&DecodedGlobals>,
    page_data: &[u8],
    options: &DecodeOptions,
    ctx: &mut DecoderContext,
) -> Result<MonoBitmap, DecodeError> {
    let doc = file::parse_auto_with(page_data, &options.limits, options.strictness)?;
    let mut pages = page::process_document_with_globals(&doc, globals, &options.limits, ctx)?;
    if pages.is_empty() {
        return Err(DecodeError::InvalidFileHeader);
    }
    Ok(pages.remove(0).bitmap)
}

/// Decode a single embedded page stream **into** a caller-provided bitmap,
/// reusing its backing allocation when the page fits (jbig2decplan.md §5, the
/// zero-alloc renderer API). The `target` is overwritten with the first page;
/// pair with a reused [`DecoderContext`] for a steady state that does not grow
/// its allocations across same-size pages.
pub fn decode_embedded_into(
    target: &mut MonoBitmap,
    globals: Option<&[u8]>,
    page_data: &[u8],
    options: &DecodeOptions,
    ctx: &mut DecoderContext,
) -> Result<(), DecodeError> {
    let decoded_globals = match globals {
        Some(g) if !g.is_empty() => Some(decode_globals(g, options)?),
        _ => None,
    };
    let doc = file::parse_auto_with(page_data, &options.limits, options.strictness)?;
    let produced =
        page::process_document_into(&doc, decoded_globals.as_ref(), &options.limits, ctx, target)?;
    if !produced {
        return Err(DecodeError::InvalidFileHeader);
    }
    Ok(())
}
