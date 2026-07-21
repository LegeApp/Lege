//! Decoder options, strictness, and the reusable [`DecoderContext`]
//! (jbig2decplan.md §13, §20).
//!
//! Phase 1 only needs generic-region contexts and a little scratch; later
//! phases grow this struct (integer/IAID contexts, symbol scratch, …). The
//! context belongs to one worker and is reused across pages so allocations are
//! not repeated per page.

use std::sync::Arc;

use crate::decode::iaid::IaidContexts;
use crate::decode::integer::IntegerContexts;
use crate::decode::refinement::REFINEMENT_CONTEXT_COUNT;
use crate::shared::bitmap::MonoBitmap;
use crate::shared::limits::DecodeLimits;
use crate::shared::mq_table::MqContext;

/// Decoder strictness (jbig2decplan.md §20).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DecodeStrictness {
    /// Enforce reserved bits, reject inconsistent counts and unsupported flag
    /// combinations, and treat any malformation as a hard error.
    #[default]
    Strict,
    /// Permit documented recovery for malformed streams; each recovery records
    /// a [`RecoveryEvent`] in the [`DecoderContext`].
    Compatible,
}

/// A malformation tolerated in [`DecodeStrictness::Compatible`] mode
/// (jbig2decplan.md §20). Recorded in [`DecoderContext::recovery_events`] so the
/// caller can log or reject the stream after the fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryEvent {
    /// Trailing bytes after the last well-formed segment did not parse as a
    /// segment header; they were ignored. `offset` is where they began.
    TrailingGarbage { offset: usize, bytes: usize },
}

/// Options controlling a decode (jbig2decplan.md §5, §20).
#[derive(Clone, Debug)]
pub struct DecodeOptions {
    /// Resource limits applied to every allocation.
    pub limits: DecodeLimits,
    /// Strictness mode.
    pub strictness: DecodeStrictness,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            limits: DecodeLimits::default(),
            strictness: DecodeStrictness::Strict,
        }
    }
}

impl DecodeOptions {
    /// Options with the given limits and strict mode.
    pub fn with_limits(limits: DecodeLimits) -> Self {
        Self {
            limits,
            strictness: DecodeStrictness::Strict,
        }
    }
}

/// The number of arithmetic contexts a template-0 generic region needs
/// (16-bit context => 65536 states). Later templates use fewer, but sizing to
/// the maximum lets the same buffer serve every template.
pub const GENERIC_CONTEXT_COUNT: usize = 1 << 16;

/// Reusable, worker-local decoder scratch (jbig2decplan.md §13).
///
/// Not behind a mutex: the PDF renderer allocates one context per worker.
/// Buffers are reused across pages and grown as needed.
#[derive(Default)]
pub struct DecoderContext {
    /// Generic-region arithmetic contexts (grown lazily to
    /// [`GENERIC_CONTEXT_COUNT`]).
    pub generic_contexts: Vec<MqContext>,
    /// Arithmetic integer procedure contexts (§11), reused across pages.
    pub integer_contexts: IntegerContexts,
    /// IAID symbol-id contexts (§11), grown as symbol-id width increases.
    pub iaid_contexts: IaidContexts,
    /// Refinement-region contexts (§17), reused across text regions; grown to
    /// [`REFINEMENT_CONTEXT_COUNT`] lazily.
    pub refinement_contexts: Vec<MqContext>,
    /// Scratch for the combined referred-symbol list, reused per text region.
    pub symbol_scratch: Vec<Arc<MonoBitmap>>,
    /// Scratch for a temporary region bitmap when a region cannot decode
    /// straight into the page.
    pub temporary_bitmap: MonoBitmap,
    /// Reusable row buffers for the generic decoder, pooled across every region
    /// and dictionary symbol so the hot path allocates them once, not per call.
    pub generic_scratch: crate::decode::generic::GenericScratch,
    /// Per-document segment store, pooled across pages so a reused worker does
    /// not reallocate its backing maps. `process_document` swaps this out with
    /// [`std::mem::take`], clears it, and swaps it back after decoding.
    pub segment_store: crate::decode::store::SegmentStore,
    /// Per-document "unknown height" flags (one per page), pooled likewise.
    pub unknown_height_scratch: Vec<bool>,
    /// Pooled `DecodedPage` list for the zero-copy `decode_embedded_into` path,
    /// swapped out and drained back each call so the `Vec` allocation is reused.
    pub pages_scratch: Vec<crate::decode::page::DecodedPage>,
    /// Pool of recyclable page/region bitmaps for the zero-copy
    /// `decode_embedded_into` path. Page and region buffers are drawn from here
    /// and returned after each decode, so the steady state does not reallocate.
    pub bitmap_pool: Vec<MonoBitmap>,
    /// Malformations tolerated during the last decode in
    /// [`DecodeStrictness::Compatible`] mode (jbig2decplan.md §20). Cleared at
    /// the start of each decode.
    pub recovery_events: Vec<RecoveryEvent>,
}

impl DecoderContext {
    /// Create an empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a zeroed generic-context slice of [`GENERIC_CONTEXT_COUNT`]
    /// entries, reusing the existing allocation.
    pub fn generic_contexts(&mut self) -> &mut [MqContext] {
        self.ensure_generic();
        for c in &mut self.generic_contexts[..GENERIC_CONTEXT_COUNT] {
            *c = MqContext::default();
        }
        &mut self.generic_contexts[..GENERIC_CONTEXT_COUNT]
    }

    /// A zeroed generic-context slice together with the reusable generic row
    /// scratch (disjoint fields, borrowed at once). Used by the region decoders,
    /// which need both.
    pub fn generic_and_scratch(
        &mut self,
    ) -> (&mut [MqContext], &mut crate::decode::generic::GenericScratch) {
        self.ensure_generic();
        for c in &mut self.generic_contexts[..GENERIC_CONTEXT_COUNT] {
            *c = MqContext::default();
        }
        (
            &mut self.generic_contexts[..GENERIC_CONTEXT_COUNT],
            &mut self.generic_scratch,
        )
    }

    /// Ensure the generic-context buffer holds at least [`GENERIC_CONTEXT_COUNT`]
    /// entries, *without* zeroing (the symbol-dictionary decoder zeroes once and
    /// carries state across symbols).
    pub fn ensure_generic(&mut self) {
        if self.generic_contexts.len() < GENERIC_CONTEXT_COUNT {
            self.generic_contexts
                .resize(GENERIC_CONTEXT_COUNT, MqContext::default());
        }
    }

    /// Return a zeroed refinement-context slice of [`REFINEMENT_CONTEXT_COUNT`]
    /// entries, reusing the existing allocation. Reset once per text-region
    /// segment; refined instances within a region share the state.
    pub fn refinement_contexts(&mut self) -> &mut [MqContext] {
        if self.refinement_contexts.len() < REFINEMENT_CONTEXT_COUNT {
            self.refinement_contexts
                .resize(REFINEMENT_CONTEXT_COUNT, MqContext::default());
        }
        for c in &mut self.refinement_contexts[..REFINEMENT_CONTEXT_COUNT] {
            *c = MqContext::default();
        }
        &mut self.refinement_contexts[..REFINEMENT_CONTEXT_COUNT]
    }

    /// Trim buffers that exceed `retained_words` after a pathological page so
    /// one malicious page does not permanently inflate a worker
    /// (jbig2decplan.md §13).
    pub fn trim_to(&mut self, retained_words: usize) {
        if self.generic_contexts.capacity() > GENERIC_CONTEXT_COUNT.saturating_mul(2) {
            self.generic_contexts = Vec::new();
        }
        // A pathological page can leave many recyclable bitmaps pooled; cap the
        // pool so it does not permanently inflate a worker.
        const MAX_POOLED_BITMAPS: usize = 8;
        if self.bitmap_pool.len() > MAX_POOLED_BITMAPS {
            self.bitmap_pool.truncate(MAX_POOLED_BITMAPS);
            self.bitmap_pool.shrink_to_fit();
        }
        let _ = retained_words;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn generic_contexts_are_zeroed_and_sized() {
        let mut ctx = DecoderContext::new();
        {
            let cs = ctx.generic_contexts();
            assert_eq!(cs.len(), GENERIC_CONTEXT_COUNT);
            cs[5] = MqContext(30);
        }
        // Re-borrow must return a freshly zeroed buffer (reused allocation).
        let cs = ctx.generic_contexts();
        assert_eq!(cs[5], MqContext::default());
    }

    #[test]
    fn default_options_are_strict() {
        assert_eq!(DecodeOptions::default().strictness, DecodeStrictness::Strict);
    }
}
