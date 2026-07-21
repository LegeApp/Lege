//! Decoded-segment storage and dependency resolution (jbig2decplan.md §19).
//!
//! A typed [`DecodedSegment`] enum (never `Box<dyn Any>`) keyed by segment
//! number in an [`FxHashMap`]. Text regions resolve their referred symbol
//! dictionaries by number; a missing referred segment or a duplicate segment
//! number is a typed error.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::decode::error::DecodeError;
use crate::decode::huffman::HuffmanTable;
use crate::decode::pattern_dictionary::PatternDictionary;
use crate::decode::symbol_dictionary::SymbolDictionary;
use crate::shared::bitmap::MonoBitmap;
use crate::shared::mq_table::MqContext;

/// Generic + generic-refinement arithmetic statistics retained by a symbol
/// dictionary segment (T.88 §6.5.5 steps 3/7, the "bitmap coding context
/// retained" flag) for a later dictionary with "context used" set to import.
#[derive(Clone)]
pub struct RetainedContexts {
    pub generic: Vec<MqContext>,
    pub refine: Vec<MqContext>,
}

/// A decoded segment resource, resolvable by later segments.
#[derive(Clone)]
pub enum DecodedSegment {
    /// A symbol dictionary's exported symbols.
    SymbolDictionary(Arc<SymbolDictionary>),
    /// A pattern dictionary's patterns (referred by halftone regions).
    PatternDictionary(Arc<PatternDictionary>),
    /// A custom Huffman table (segment type 53), referred by symbol
    /// dictionaries and text regions using user-supplied tables (T.88 §7.4.13).
    HuffmanTable(Arc<HuffmanTable>),
    /// A retained region bitmap (intermediate regions; not produced by the
    /// current encoder but modelled for completeness).
    Region(Arc<MonoBitmap>),
    /// A structural/metadata segment with no decoded resource.
    Metadata,
}

/// Segment-number keyed store of decoded resources (jbig2decplan.md §19).
#[derive(Default, Clone)]
pub struct SegmentStore {
    values: FxHashMap<u32, DecodedSegment>,
    retained: FxHashMap<u32, Arc<RetainedContexts>>,
}

impl SegmentStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop all decoded segments and retained contexts, keeping the backing
    /// map capacity so a reused store (pooled in the [`DecoderContext`]) does
    /// not reallocate on the next document.
    pub fn clear(&mut self) {
        self.values.clear();
        self.retained.clear();
    }

    /// Insert a decoded segment, rejecting a duplicate segment number.
    pub fn insert(&mut self, number: u32, seg: DecodedSegment) -> Result<(), DecodeError> {
        if self.values.contains_key(&number) {
            return Err(DecodeError::DuplicateSegment { number });
        }
        self.values.insert(number, seg);
        Ok(())
    }

    /// Look up a decoded segment by number.
    #[inline]
    pub fn get(&self, number: u32) -> Option<&DecodedSegment> {
        self.values.get(&number)
    }

    /// Save the retained arithmetic contexts of a symbol dictionary segment.
    pub fn insert_retained(&mut self, number: u32, ctx: RetainedContexts) {
        self.retained.insert(number, Arc::new(ctx));
    }

    /// Resolve an intermediate region's retained bitmap (auxiliary buffer,
    /// T.88 §7.4.7.4), the first referred region found in `self` then `globals`.
    pub fn referred_region<'a>(
        &'a self,
        referred: &[u32],
        globals: Option<&'a SegmentStore>,
    ) -> Option<&'a Arc<MonoBitmap>> {
        for &rn in referred {
            match self.values.get(&rn).or_else(|| globals.and_then(|g| g.get(rn))) {
                Some(DecodedSegment::Region(bm)) => return Some(bm),
                _ => continue,
            }
        }
        None
    }

    /// The retained contexts of the *last* referred symbol-dictionary segment
    /// that retained them (T.88 §6.5.5 step 3), checking `self` then `globals`.
    pub fn last_retained(
        &self,
        referred: &[u32],
        globals: Option<&SegmentStore>,
    ) -> Option<Arc<RetainedContexts>> {
        for &rn in referred.iter().rev() {
            if let Some(c) = self.retained.get(&rn) {
                return Some(c.clone());
            }
            if let Some(c) = globals.and_then(|g| g.retained.get(&rn)) {
                return Some(c.clone());
            }
        }
        None
    }

    /// Resolve a symbol dictionary by number, erroring if it is missing or of
    /// the wrong type. `segment` is the referring segment (for the error).
    pub fn symbol_dictionary(
        &self,
        segment: u32,
        referred: u32,
    ) -> Result<&Arc<SymbolDictionary>, DecodeError> {
        match self.values.get(&referred) {
            Some(DecodedSegment::SymbolDictionary(d)) => Ok(d),
            Some(_) => Err(DecodeError::WrongReferredSegmentType { segment, referred }),
            None => Err(DecodeError::MissingReferredSegment { segment, referred }),
        }
    }

    /// Resolve a pattern dictionary by number, checking `self` then `globals`,
    /// erroring if it is missing or of the wrong type. `segment` is the
    /// referring (halftone) segment (for the error).
    pub fn pattern_dictionary<'a>(
        &'a self,
        segment: u32,
        referred: &[u32],
        globals: Option<&'a SegmentStore>,
    ) -> Result<&'a Arc<PatternDictionary>, DecodeError> {
        for &rn in referred {
            match self.values.get(&rn).or_else(|| globals.and_then(|g| g.get(rn))) {
                Some(DecodedSegment::PatternDictionary(p)) => return Ok(p),
                _ => continue,
            }
        }
        // No referred pattern dictionary found among the resolvable segments.
        let referred_num = referred.first().copied().unwrap_or(0);
        Err(DecodeError::MissingReferredSegment {
            segment,
            referred: referred_num,
        })
    }

    /// Gather the exported symbols of every referred *symbol dictionary*, in
    /// reference order, checking `self` first then `globals`
    /// (jbig2decplan.md §16, §17). A referred number present in neither store is
    /// a missing-referred-segment error; a referred non-dictionary (e.g. a
    /// Huffman table) contributes no symbols.
    pub fn gather_symbols(
        &self,
        segment: u32,
        referred: &[u32],
        globals: Option<&SegmentStore>,
    ) -> Result<Vec<Arc<MonoBitmap>>, DecodeError> {
        let mut out: Vec<Arc<MonoBitmap>> = Vec::new();
        for &rn in referred {
            let dict = match self.values.get(&rn) {
                Some(DecodedSegment::SymbolDictionary(d)) => Some(d),
                Some(_) => None,
                None => match globals.and_then(|g| g.get(rn)) {
                    Some(DecodedSegment::SymbolDictionary(d)) => Some(d),
                    Some(_) => None,
                    None => {
                        return Err(DecodeError::MissingReferredSegment {
                            segment,
                            referred: rn,
                        });
                    }
                },
            };
            if let Some(d) = dict {
                out.extend(d.exported_symbols.iter().cloned());
            }
        }
        Ok(out)
    }

    /// Gather the referred custom Huffman tables (segment type 53), in reference
    /// order, checking `self` then `globals`. Referred non-table segments are
    /// skipped (a symbol dictionary refers to both its input dictionaries and
    /// its custom tables); a referred number in neither store is ignored here
    /// (symbol/pattern resolution reports genuine missing references).
    pub fn gather_huffman_tables(
        &self,
        referred: &[u32],
        globals: Option<&SegmentStore>,
    ) -> Vec<Arc<HuffmanTable>> {
        let mut out: Vec<Arc<HuffmanTable>> = Vec::new();
        for &rn in referred {
            let found = match self.values.get(&rn) {
                Some(DecodedSegment::HuffmanTable(t)) => Some(t),
                _ => match globals.and_then(|g| g.get(rn)) {
                    Some(DecodedSegment::HuffmanTable(t)) => Some(t),
                    _ => None,
                },
            };
            if let Some(t) = found {
                out.push(t.clone());
            }
        }
        out
    }
}
