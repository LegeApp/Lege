//! Decoded PDF `JBIG2Globals` (jbig2decplan.md §6).
//!
//! PDF globals commonly carry the symbol dictionaries shared by every page
//! stream. They are parsed and decoded *once* into an immutable
//! [`DecodedGlobals`] that is `Send + Sync` with no interior mutability and no
//! worker scratch, so many page workers can share `&DecodedGlobals` while each
//! keeps its own mutable [`DecoderContext`].

use std::sync::Arc;

use crate::decode::DecodeOptions;
use crate::decode::context::DecoderContext;
use crate::decode::error::{DecodeError, UnsupportedFeature};
use crate::decode::file;
use crate::decode::page::{decode_pattern_dict_into, decode_symbol_dict_into, decode_tables_into};
use crate::decode::pattern_dictionary::PatternDictionary;
use crate::decode::store::SegmentStore;
use crate::decode::symbol_dictionary::SymbolDictionary;
use crate::shared::segment::SegmentType;

/// Decoded, immutable, shareable global resources (jbig2decplan.md §6).
pub struct DecodedGlobals {
    store: SegmentStore,
    symbol_dictionaries: Vec<Arc<SymbolDictionary>>,
    pattern_dictionaries: Vec<Arc<PatternDictionary>>,
}

impl DecodedGlobals {
    /// Approximate decoded bytes retained by the globals segment store.
    ///
    /// Renderer caches use this to keep shared dictionaries only when their
    /// decoded footprint fits the cache's explicit budget.
    #[inline]
    pub fn retained_bytes(&self) -> usize {
        self.store.retained_bytes()
    }

    /// The decoded segment store (for referred-segment resolution).
    #[inline]
    pub fn store(&self) -> &SegmentStore {
        &self.store
    }

    /// Every symbol dictionary decoded from the globals, in stream order.
    #[inline]
    pub fn symbol_dictionaries(&self) -> &[Arc<SymbolDictionary>] {
        &self.symbol_dictionaries
    }

    /// Every pattern dictionary decoded from the globals, in stream order.
    #[inline]
    pub fn pattern_dictionaries(&self) -> &[Arc<PatternDictionary>] {
        &self.pattern_dictionaries
    }

    /// Whether the globals carried no decodable resource.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.symbol_dictionaries.is_empty() && self.pattern_dictionaries.is_empty()
    }
}

/// Parse and decode a PDF `JBIG2Globals` byte stream once (jbig2decplan.md §6).
///
/// Only symbol-dictionary and metadata segments are expected in globals; a
/// region or page-information segment there is a typed `Unsupported` error.
pub fn decode_globals(data: &[u8], options: &DecodeOptions) -> Result<DecodedGlobals, DecodeError> {
    let doc = file::parse_auto_with(data, &options.limits, options.strictness)?;
    let mut ctx = DecoderContext::new();
    let mut store = SegmentStore::new();
    let mut symbol_dictionaries = Vec::new();
    let mut pattern_dictionaries = Vec::new();

    for seg in &doc.segments {
        match seg.header.segment_type() {
            Some(SegmentType::SymbolDictionary) => {
                let dict =
                    decode_symbol_dict_into(seg, &mut store, None, &options.limits, &mut ctx)?;
                symbol_dictionaries.push(dict);
            }
            Some(SegmentType::PatternDictionary) => {
                let dict = decode_pattern_dict_into(seg, &mut store, &options.limits, &mut ctx)?;
                pattern_dictionaries.push(dict);
            }
            Some(SegmentType::Tables) => {
                decode_tables_into(seg, &mut store, &options.limits)?;
            }
            // Structural / metadata segments carry no resource.
            Some(
                SegmentType::EndOfFile
                | SegmentType::EndOfPage
                | SegmentType::EndOfStripe
                | SegmentType::Profiles
                | SegmentType::ColorPalette
                | SegmentType::FileHeader
                | SegmentType::Extension,
            ) => {}
            // Anything else (regions, page information, pattern dictionaries)
            // does not belong in a globals stream for the self-decoder.
            _ => {
                return Err(DecodeError::Unsupported(UnsupportedFeature::SymbolCoding));
            }
        }
    }

    Ok(DecodedGlobals {
        store,
        symbol_dictionaries,
        pattern_dictionaries,
    })
}

/// Compile-time proof that `DecodedGlobals` is `Send + Sync` (jbig2decplan.md
/// §6, Gap E: the Phase 2 gate).
#[allow(dead_code)]
fn _assert_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<DecodedGlobals>();
}
