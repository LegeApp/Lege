use crate::jbig2classify::FamilyBucketKey;
use crate::jbig2cost::symbol_dictionary_entry_bytes;
use crate::jbig2structs::{FileHeader, Segment};
use crate::jbig2sym::{BitImage, Rect};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::hash::Hash;
use std::time::Duration;

pub(crate) fn encoder_diagnostics_enabled() -> bool {
    std::env::var("JBIG2_DIAGNOSTICS").is_ok_and(|value| value != "0" && !value.is_empty())
}

#[inline]
pub(crate) fn indexed_symbol_dictionary_bytes(symbols: &[BitImage], indices: &[usize]) -> usize {
    indices
        .iter()
        .copied()
        .map(|index| symbol_dictionary_entry_bytes(&symbols[index]))
        .sum()
}

#[inline]
pub(crate) fn anchor_map_dictionary_bytes(
    symbols: &[BitImage],
    anchor_map: &FxHashMap<FamilyBucketKey, Vec<usize>>,
) -> usize {
    anchor_map
        .values()
        .flat_map(|bucket| bucket.iter().copied())
        .map(|index| symbol_dictionary_entry_bytes(&symbols[index]))
        .sum()
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SymUnifyAnchorDecision {
    Accept {
        score: u32,
        dx: i32,
        dy: i32,
    },
    RejectDim,
    RejectPixelDelta,
    RejectSignature,
    RejectOverlap,
    RejectCompare,
    RejectScore {
        score: u32,
        limit: u32,
        dx: i32,
        dy: i32,
    },
    RejectOutsideInk,
}

impl SymUnifyAnchorDecision {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SymUnifyAnchorDecision::Accept { .. } => "accept",
            SymUnifyAnchorDecision::RejectDim => "dim",
            SymUnifyAnchorDecision::RejectPixelDelta => "pixel_delta",
            SymUnifyAnchorDecision::RejectSignature => "signature",
            SymUnifyAnchorDecision::RejectOverlap => "overlap",
            SymUnifyAnchorDecision::RejectCompare => "compare",
            SymUnifyAnchorDecision::RejectScore { .. } => "score",
            SymUnifyAnchorDecision::RejectOutsideInk => "outside_ink",
        }
    }

    pub(crate) fn diagnostic_rank(self) -> u8 {
        match self {
            SymUnifyAnchorDecision::Accept { .. } => 255,
            SymUnifyAnchorDecision::RejectScore { .. } => 7,
            SymUnifyAnchorDecision::RejectOutsideInk => 6,
            SymUnifyAnchorDecision::RejectCompare => 5,
            SymUnifyAnchorDecision::RejectOverlap => 4,
            SymUnifyAnchorDecision::RejectSignature => 3,
            SymUnifyAnchorDecision::RejectPixelDelta => 2,
            SymUnifyAnchorDecision::RejectDim => 1,
        }
    }
}

#[inline]
pub(crate) fn update_best_reject(
    best: &mut Option<SymUnifyAnchorDecision>,
    decision: SymUnifyAnchorDecision,
) {
    if !matches!(decision, SymUnifyAnchorDecision::Accept { .. })
        && best.is_none_or(|current| decision.diagnostic_rank() > current.diagnostic_rank())
    {
        *best = Some(decision);
    }
}

#[inline]
pub(crate) fn bitmap_proxy_bytes(symbol: &BitImage) -> usize {
    (symbol.width.saturating_mul(symbol.height).saturating_add(7)) / 8
}

#[inline]
pub(crate) fn classify_residual_shape(symbol: &BitImage) -> ResidualShapeKind {
    let area = symbol.width.saturating_mul(symbol.height);
    let black = symbol.count_ones();
    if area <= 16 || black <= 2 {
        ResidualShapeKind::Tiny
    } else if crate::jbig2shared::symbol_likely_punctuation_or_mark(symbol) {
        ResidualShapeKind::PunctuationLike
    } else {
        ResidualShapeKind::GlyphLike
    }
}

#[inline]
pub(crate) fn record_counterfactual_probe(
    stats: &mut CounterfactualProbeStats,
    page_num: usize,
    symbol_index: usize,
    symbol: &BitImage,
    black_pixels: usize,
) {
    stats.symbol_count += 1;
    stats.black_pixels += black_pixels;
    stats.bitmap_proxy_bytes += bitmap_proxy_bytes(symbol);
    stats.pages.insert(page_num);
    if stats.samples.len() < 8 {
        stats
            .samples
            .push((page_num + 1, symbol_index, symbol.width, symbol.height));
    }
}

#[inline]
pub(crate) fn record_labeled_counterfactual_probe(
    stats_map: &mut FxHashMap<&'static str, CounterfactualProbeStats>,
    label: &'static str,
    page_num: usize,
    symbol_index: usize,
    symbol: &BitImage,
    black_pixels: usize,
) {
    let stats = stats_map.entry(label).or_default();
    record_counterfactual_probe(stats, page_num, symbol_index, symbol, black_pixels);
}

#[inline]
pub(crate) fn relaxed_compare_probe_max_err(candidate: &BitImage, proto: &BitImage) -> u32 {
    candidate
        .width
        .max(proto.width)
        .saturating_mul(candidate.height.max(proto.height)) as u32
}

#[inline]
pub(crate) fn record_detailed_compare_probe(
    stats: &mut DetailedCompareProbeStats,
    page_num: usize,
    symbol_index: usize,
    symbol: &BitImage,
    result: crate::jbig2comparator::CompareResult,
    compare_max_err: u32,
    exact_dims: bool,
    strong_anchor: bool,
) {
    stats.symbol_count += 1;
    stats.bitmap_proxy_bytes += bitmap_proxy_bytes(symbol);
    stats.pages.insert(page_num);
    stats.exact_dims_count += usize::from(exact_dims);
    stats.strong_anchor_count += usize::from(strong_anchor);
    stats.shift_le1_count += usize::from(result.dx.abs() <= 1 && result.dy.abs() <= 1);

    let over_by = result.total_err.saturating_sub(compare_max_err);
    if over_by <= 2 {
        stats.over_by_le2_count += 1;
    } else if over_by <= 4 {
        stats.over_by_le4_count += 1;
    } else if over_by <= 8 {
        stats.over_by_le8_count += 1;
    } else {
        stats.over_by_gt8_count += 1;
    }

    if stats.samples.len() < 8 {
        stats.samples.push((
            page_num + 1,
            symbol_index,
            symbol.width,
            symbol.height,
            result.total_err,
            compare_max_err,
            result.overlap_err,
            result.outside_ink_err,
            result.dx,
            result.dy,
        ));
    }
}

impl ResidualSymbolTrace {
    pub(crate) fn reason_code(self) -> ResidualReasonCode {
        if self.local_use_count != 1 {
            return ResidualReasonCode::NonSingletonResidual;
        }

        if self.had_global_candidates {
            return match self
                .global_best_reject
                .unwrap_or(SymUnifyAnchorDecision::RejectDim)
            {
                SymUnifyAnchorDecision::RejectDim => ResidualReasonCode::UseCountOneGlobalRejectDim,
                SymUnifyAnchorDecision::RejectPixelDelta => {
                    ResidualReasonCode::UseCountOneGlobalRejectPixelDelta
                }
                SymUnifyAnchorDecision::RejectSignature => {
                    ResidualReasonCode::UseCountOneGlobalRejectSignature
                }
                SymUnifyAnchorDecision::RejectOverlap => {
                    ResidualReasonCode::UseCountOneGlobalRejectOverlap
                }
                SymUnifyAnchorDecision::RejectCompare => {
                    ResidualReasonCode::UseCountOneGlobalRejectCompare
                }
                SymUnifyAnchorDecision::RejectOutsideInk => {
                    ResidualReasonCode::UseCountOneGlobalRejectOutsideInk
                }
                SymUnifyAnchorDecision::RejectScore { .. } => {
                    ResidualReasonCode::UseCountOneGlobalRejectScore
                }
                SymUnifyAnchorDecision::Accept { .. } => {
                    ResidualReasonCode::UseCountOneNoCandidates
                }
            };
        }

        if self.had_local_candidates {
            return match self
                .local_best_reject
                .unwrap_or(SymUnifyAnchorDecision::RejectDim)
            {
                SymUnifyAnchorDecision::RejectDim => ResidualReasonCode::UseCountOneLocalRejectDim,
                SymUnifyAnchorDecision::RejectPixelDelta => {
                    ResidualReasonCode::UseCountOneLocalRejectPixelDelta
                }
                SymUnifyAnchorDecision::RejectSignature => {
                    ResidualReasonCode::UseCountOneLocalRejectSignature
                }
                SymUnifyAnchorDecision::RejectOverlap => {
                    ResidualReasonCode::UseCountOneLocalRejectOverlap
                }
                SymUnifyAnchorDecision::RejectCompare => {
                    ResidualReasonCode::UseCountOneLocalRejectCompare
                }
                SymUnifyAnchorDecision::RejectOutsideInk => {
                    ResidualReasonCode::UseCountOneLocalRejectOutsideInk
                }
                SymUnifyAnchorDecision::RejectScore { .. } => {
                    ResidualReasonCode::UseCountOneLocalRejectScore
                }
                SymUnifyAnchorDecision::Accept { .. } => {
                    ResidualReasonCode::UseCountOneNoCandidates
                }
            };
        }

        ResidualReasonCode::UseCountOneNoCandidates
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SymUnifyAnchorCandidate {
    pub(crate) anchor_index: usize,
    pub(crate) score: u32,
    pub(crate) dx: i32,
    pub(crate) dy: i32,
    pub(crate) rerank_cost: u32,
    pub(crate) rescued_on_score: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResidualSymbolTrace {
    pub(crate) page_num: usize,
    pub(crate) local_use_count: usize,
    pub(crate) had_local_candidates: bool,
    pub(crate) had_global_candidates: bool,
    pub(crate) local_best_reject: Option<SymUnifyAnchorDecision>,
    pub(crate) global_best_reject: Option<SymUnifyAnchorDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ResidualReasonCode {
    UseCountOneNoCandidates,
    UseCountOneLocalRejectDim,
    UseCountOneLocalRejectPixelDelta,
    UseCountOneLocalRejectSignature,
    UseCountOneLocalRejectOverlap,
    UseCountOneLocalRejectCompare,
    UseCountOneLocalRejectOutsideInk,
    UseCountOneLocalRejectScore,
    UseCountOneGlobalRejectDim,
    UseCountOneGlobalRejectPixelDelta,
    UseCountOneGlobalRejectSignature,
    UseCountOneGlobalRejectOverlap,
    UseCountOneGlobalRejectCompare,
    UseCountOneGlobalRejectOutsideInk,
    UseCountOneGlobalRejectScore,
    NonSingletonResidual,
}

impl ResidualReasonCode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ResidualReasonCode::UseCountOneNoCandidates => "UseCountOneNoCandidates",
            ResidualReasonCode::UseCountOneLocalRejectDim => "UseCountOneLocalRejectDim",
            ResidualReasonCode::UseCountOneLocalRejectPixelDelta => {
                "UseCountOneLocalRejectPixelDelta"
            }
            ResidualReasonCode::UseCountOneLocalRejectSignature => {
                "UseCountOneLocalRejectSignature"
            }
            ResidualReasonCode::UseCountOneLocalRejectOverlap => "UseCountOneLocalRejectOverlap",
            ResidualReasonCode::UseCountOneLocalRejectCompare => "UseCountOneLocalRejectCompare",
            ResidualReasonCode::UseCountOneLocalRejectOutsideInk => {
                "UseCountOneLocalRejectOutsideInk"
            }
            ResidualReasonCode::UseCountOneLocalRejectScore => "UseCountOneLocalRejectScore",
            ResidualReasonCode::UseCountOneGlobalRejectDim => "UseCountOneGlobalRejectDim",
            ResidualReasonCode::UseCountOneGlobalRejectPixelDelta => {
                "UseCountOneGlobalRejectPixelDelta"
            }
            ResidualReasonCode::UseCountOneGlobalRejectSignature => {
                "UseCountOneGlobalRejectSignature"
            }
            ResidualReasonCode::UseCountOneGlobalRejectOverlap => "UseCountOneGlobalRejectOverlap",
            ResidualReasonCode::UseCountOneGlobalRejectCompare => "UseCountOneGlobalRejectCompare",
            ResidualReasonCode::UseCountOneGlobalRejectOutsideInk => {
                "UseCountOneGlobalRejectOutsideInk"
            }
            ResidualReasonCode::UseCountOneGlobalRejectScore => "UseCountOneGlobalRejectScore",
            ResidualReasonCode::NonSingletonResidual => "NonSingletonResidual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResidualShapeKind {
    Tiny,
    PunctuationLike,
    GlyphLike,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ResidualReasonStats {
    pub(crate) symbol_count: usize,
    pub(crate) instance_count: usize,
    pub(crate) black_pixels: usize,
    pub(crate) bitmap_proxy_bytes: usize,
    pub(crate) pages: FxHashSet<usize>,
    pub(crate) tiny_count: usize,
    pub(crate) punctuation_like_count: usize,
    pub(crate) glyph_like_count: usize,
    pub(crate) samples: Vec<(usize, usize, usize, usize, usize)>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CounterfactualProbeStats {
    pub(crate) symbol_count: usize,
    pub(crate) black_pixels: usize,
    pub(crate) bitmap_proxy_bytes: usize,
    pub(crate) pages: FxHashSet<usize>,
    pub(crate) samples: Vec<(usize, usize, usize, usize)>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DetailedCompareProbeStats {
    pub(crate) symbol_count: usize,
    pub(crate) bitmap_proxy_bytes: usize,
    pub(crate) pages: FxHashSet<usize>,
    pub(crate) exact_dims_count: usize,
    pub(crate) strong_anchor_count: usize,
    pub(crate) shift_le1_count: usize,
    pub(crate) over_by_le2_count: usize,
    pub(crate) over_by_le4_count: usize,
    pub(crate) over_by_le8_count: usize,
    pub(crate) over_by_gt8_count: usize,
    pub(crate) samples: Vec<(usize, usize, usize, usize, u32, u32, u32, u32, i32, i32)>,
}

#[derive(Debug)]
pub(crate) struct RecentSymbolCache {
    pub(crate) recent: VecDeque<usize>,
    pub(crate) cap: usize,
}

impl RecentSymbolCache {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            recent: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.recent.clear();
    }

    pub(crate) fn touch(&mut self, idx: usize) {
        if let Some(pos) = self.recent.iter().position(|&entry| entry == idx) {
            self.recent.remove(pos);
        }
        self.recent.push_front(idx);
        while self.recent.len() > self.cap {
            self.recent.pop_back();
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.recent.iter().copied()
    }

    pub(crate) fn copy_into(&self, out: &mut [usize]) -> usize {
        let mut len = 0usize;
        for idx in self.recent.iter().copied() {
            if len >= out.len() {
                break;
            }
            out[len] = idx;
            len += 1;
        }
        len
    }
}

/// A candidate symbol extracted from a document image.
#[derive(Debug, Clone)]
pub struct SymbolCandidate {
    /// The bitmap image of the symbol.
    pub bitmap: BitImage,
    /// The bounding box of the symbol in the original image.
    pub bbox: Rect,
}

#[derive(Clone)]
pub struct SymbolInstance {
    pub symbol_index: usize,
    pub position: Rect,
    pub instance_bitmap: BitImage,
    /// Whether this instance needs refinement coding (bitmap differs from prototype)
    pub needs_refinement: bool,
    /// Horizontal alignment offset for refinement (from Comparator)
    pub refinement_dx: i32,
    /// Vertical alignment offset for refinement (from Comparator)
    pub refinement_dy: i32,
}

impl SymbolInstance {
    pub fn symbol_index(&self) -> usize {
        self.symbol_index
    }

    pub fn position(&self) -> Rect {
        self.position
    }

    pub fn instance_bitmap(&self) -> &BitImage {
        &self.instance_bitmap
    }
}
#[derive(Debug)]
pub(crate) struct PlannedPage {
    pub(crate) page_number: u32,
    pub(crate) segments: Vec<Segment>,
}

#[derive(Debug)]
pub(crate) struct PlannedDocument {
    pub(crate) file_header: Option<FileHeader>,
    pub(crate) global_segments: Vec<Segment>,
    pub(crate) pages: Vec<PlannedPage>,
    pub(crate) eof_segment: Option<Segment>,
    pub(crate) next_segment_number: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedPageLayout {
    pub(crate) page_index: usize,
    pub(crate) page_number: u32,
    pub(crate) page_info_segment_number: u32,
    pub(crate) local_dict_segment_numbers: Vec<u32>,
    pub(crate) local_dict_layout: Option<SymbolDictLayout>,
    pub(crate) region_segment_number: u32,
    pub(crate) residual_region_segment_number: Option<u32>,
    pub(crate) end_of_page_segment_number: u32,
    pub(crate) local_symbols: Vec<usize>,
    pub(crate) residual_symbols: Vec<usize>,
    pub(crate) residual_anchor_remaps: FxHashMap<usize, usize>,
    pub(crate) use_generic_region: bool,
}

#[derive(Debug)]
pub(crate) struct BuiltPage {
    pub(crate) page: PlannedPage,
    pub(crate) symbol_dict_time: Duration,
    pub(crate) text_region_time: Duration,
    pub(crate) generic_region_time: Duration,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolDictLayout {
    pub(crate) export_input_indices: Vec<usize>,
    pub(crate) refinements: Vec<Option<RefinementPlan>>,
    pub(crate) diagnostics: SymbolDictDiagnostics,
}

impl SymbolDictLayout {
    pub(crate) fn segment_count(&self) -> usize {
        if self.export_input_indices.is_empty() {
            0
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SymbolDictDiagnostics {
    pub(crate) family_count: usize,
    pub(crate) singleton_family_count: usize,
    pub(crate) refined_member_count: usize,
    pub(crate) exported_member_count: usize,
    pub(crate) sample_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RefinementPlan {
    pub(crate) prototype_input_index: usize,
    pub(crate) refinement_dx: i32,
    pub(crate) refinement_dy: i32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EncodedSymbolDictionary {
    pub(crate) payload: Vec<u8>,
    pub(crate) input_to_exported_pos: Vec<u32>,
    pub(crate) exported_symbol_count: u32,
}
