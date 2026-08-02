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
use crate::shared::{error::LimitError, limits::DecodeLimits};

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
    depths: FxHashMap<u32, usize>,
    retained_bytes: usize,
    external_retained_bytes: usize,
}

impl SegmentStore {
    /// Bytes retained by this store for later segment references.
    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

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
        self.depths.clear();
        self.retained_bytes = 0;
        self.external_retained_bytes = 0;
    }

    /// Insert a decoded segment, rejecting a duplicate segment number.
    pub fn insert(&mut self, number: u32, seg: DecodedSegment) -> Result<(), DecodeError> {
        if self.values.contains_key(&number) {
            return Err(DecodeError::DuplicateSegment { number });
        }
        self.values.insert(number, seg);
        Ok(())
    }

    pub(crate) fn record_dependency_depth(
        &mut self,
        number: u32,
        referred: &[u32],
        globals: Option<&SegmentStore>,
        limits: &DecodeLimits,
    ) -> Result<(), DecodeError> {
        self.external_retained_bytes = self
            .external_retained_bytes
            .max(globals.map_or(0, |g| g.retained_bytes));
        let parent = referred
            .iter()
            .filter_map(|n| {
                self.depths
                    .get(n)
                    .or_else(|| globals.and_then(|g| g.depths.get(n)))
            })
            .copied()
            .max()
            .unwrap_or(0);
        let depth = parent.checked_add(1).ok_or(DecodeError::Overflow {
            operation: "segment dependency depth",
        })?;
        if depth > limits.max_dependency_depth {
            return Err(DecodeError::limit(LimitError::Count {
                what: "segment dependency depth",
                value: depth as u64,
                limit: limits.max_dependency_depth as u64,
            }));
        }
        self.depths.insert(number, depth);
        Ok(())
    }

    pub(crate) fn insert_limited(
        &mut self,
        number: u32,
        seg: DecodedSegment,
        globals: Option<&SegmentStore>,
        limits: &DecodeLimits,
    ) -> Result<(), DecodeError> {
        let bytes = decoded_segment_bytes(&seg);
        self.reserve_retained(bytes, globals, limits)?;
        if let Err(err) = self.insert(number, seg) {
            self.retained_bytes -= bytes;
            return Err(err);
        }
        Ok(())
    }

    /// Look up a decoded segment by number.
    #[inline]
    pub fn get(&self, number: u32) -> Option<&DecodedSegment> {
        self.values.get(&number)
    }

    /// Save the retained arithmetic contexts of a symbol dictionary segment.
    pub(crate) fn insert_retained_limited(
        &mut self,
        number: u32,
        ctx: RetainedContexts,
        globals: Option<&SegmentStore>,
        limits: &DecodeLimits,
    ) -> Result<(), DecodeError> {
        let bytes = ctx
            .generic
            .len()
            .saturating_add(ctx.refine.len())
            .saturating_mul(core::mem::size_of::<MqContext>());
        self.reserve_retained(bytes, globals, limits)?;
        self.retained.insert(number, Arc::new(ctx));
        Ok(())
    }

    fn reserve_retained(
        &mut self,
        bytes: usize,
        globals: Option<&SegmentStore>,
        limits: &DecodeLimits,
    ) -> Result<(), DecodeError> {
        let total = self
            .retained_bytes
            .checked_add(
                self.external_retained_bytes
                    .max(globals.map_or(0, |g| g.retained_bytes)),
            )
            .and_then(|v| v.checked_add(bytes))
            .ok_or(DecodeError::Overflow {
                operation: "retained decoded bytes",
            })?;
        if total > limits.max_retained_bytes {
            return Err(DecodeError::limit(LimitError::Count {
                what: "retained decoded bytes",
                value: total as u64,
                limit: limits.max_retained_bytes as u64,
            }));
        }
        self.retained_bytes =
            self.retained_bytes
                .checked_add(bytes)
                .ok_or(DecodeError::Overflow {
                    operation: "retained decoded bytes",
                })?;
        Ok(())
    }

    /// Resolve an intermediate region's retained bitmap (auxiliary buffer,
    /// T.88 §7.4.7.4), the first referred region found in `self` then `globals`.
    pub fn referred_region<'a>(
        &'a self,
        referred: &[u32],
        globals: Option<&'a SegmentStore>,
    ) -> Option<&'a Arc<MonoBitmap>> {
        for &rn in referred {
            match self
                .values
                .get(&rn)
                .or_else(|| globals.and_then(|g| g.get(rn)))
            {
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
            match self
                .values
                .get(&rn)
                .or_else(|| globals.and_then(|g| g.get(rn)))
            {
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
        self.gather_symbols_into(segment, referred, globals, &mut out)?;
        Ok(out)
    }

    /// Append referred exported symbols into a caller-owned scratch vector.
    /// Keeping this separate from [`Self::gather_symbols`] lets the decoder's
    /// hot path reuse one allocation across every text region.
    pub fn gather_symbols_into(
        &self,
        segment: u32,
        referred: &[u32],
        globals: Option<&SegmentStore>,
        out: &mut Vec<Arc<MonoBitmap>>,
    ) -> Result<(), DecodeError> {
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
        Ok(())
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

fn decoded_segment_bytes(seg: &DecodedSegment) -> usize {
    match seg {
        DecodedSegment::SymbolDictionary(d) => {
            d.exported_symbols.iter().map(|b| b.storage_bytes()).sum()
        }
        DecodedSegment::PatternDictionary(d) => d.patterns.iter().map(|b| b.storage_bytes()).sum(),
        DecodedSegment::Region(b) => b.storage_bytes(),
        DecodedSegment::HuffmanTable(_) => core::mem::size_of::<HuffmanTable>(),
        DecodedSegment::Metadata => 0,
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    #[test]
    fn aggregate_retained_bytes_and_dependency_depth_are_enforced() {
        let mut limits = DecodeLimits::default();
        limits.max_retained_bytes = 4;
        limits.max_dependency_depth = 2;
        let mut store = SegmentStore::new();
        store
            .record_dependency_depth(1, &[], None, &limits)
            .unwrap();
        store
            .record_dependency_depth(2, &[1], None, &limits)
            .unwrap();
        assert!(
            store
                .record_dependency_depth(3, &[2], None, &limits)
                .is_err()
        );

        let bm = MonoBitmap::new(32, 1, false, &limits).unwrap();
        store
            .insert_limited(1, DecodedSegment::Region(Arc::new(bm)), None, &limits)
            .unwrap();
        let bm = MonoBitmap::new(32, 1, false, &limits).unwrap();
        assert!(
            store
                .insert_limited(2, DecodedSegment::Region(Arc::new(bm)), None, &limits)
                .is_err()
        );
    }
}
