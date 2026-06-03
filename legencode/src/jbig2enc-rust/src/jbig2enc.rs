//! This module contains the main JBIG2 encoder logic.
use crate::jbig2arith::{IntProc, Jbig2ArithCoder};
use crate::jbig2classify::{
    FamilyBucketKey, SymbolSignature, compute_symbol_signature as compute_symbol_signature_shared,
    family_bucket_key_for_symbol, family_bucket_neighbors, family_match_details,
    family_signatures_are_compatible, refine_compare_score,
};
use crate::jbig2comparator::{Comparator, MAX_DIMENSION_DELTA};
use crate::jbig2context::build_symbol_context_model;
use crate::jbig2cost::{symbol_dictionary_entries_bytes, symbol_dictionary_entry_bytes};
use crate::jbig2unify::{SymbolUnifyInputs, UnifiedClass};
// Symbol extraction using CC analysis
#[cfg(feature = "symboldict")]
use crate::jbig2cc::analyze_page;
use crate::jbig2structs::{
    FileHeader, GenericRegionConfig, GenericRegionParams, Jbig2Config, LossySymbolMode, PageInfo,
    Segment, SegmentType, SymbolDictParams, TextRegionParams,
};

use crate::jbig2sym::{BitImage, Rect};
use anyhow::{Result, anyhow};

// Define debug and trace macros at the crate root
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        #[cfg(feature = "trace_encoder")]
        log::debug!($($arg)*);

        #[cfg(not(feature = "trace_encoder"))]
        let _ = format_args!($($arg)*);
    };
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "trace_encoder")]
        log::trace!($($arg)*);

        #[cfg(not(feature = "trace_encoder"))]
        let _ = format_args!($($arg)*);
    };
}

// Import the macros for use in this module
#[allow(unused_imports)]
use crate::{debug, trace};

use ndarray::Array2;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// A key type for hashing bitmaps efficiently
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashKey(u64);

const RECENT_SYMBOL_CACHE_CAP: usize = 64;
const SYM_UNIFY_EXACT_ANCHOR_BUDGET: usize = 32;
const SYM_UNIFY_NEIGHBOR_ANCHOR_BUDGET: usize = 16;
const SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE: usize = 8;
const SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN: usize = 4;

fn encoder_diagnostics_enabled() -> bool {
    std::env::var("JBIG2_DIAGNOSTICS").is_ok_and(|value| value != "0" && !value.is_empty())
}

#[inline]
fn indexed_symbol_dictionary_bytes(symbols: &[BitImage], indices: &[usize]) -> usize {
    indices
        .iter()
        .copied()
        .map(|index| symbol_dictionary_entry_bytes(&symbols[index]))
        .sum()
}

#[inline]
fn anchor_map_dictionary_bytes(
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
enum SymUnifyAnchorDecision {
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
    fn label(self) -> &'static str {
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

    fn diagnostic_rank(self) -> u8 {
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
fn update_best_reject(best: &mut Option<SymUnifyAnchorDecision>, decision: SymUnifyAnchorDecision) {
    if !matches!(decision, SymUnifyAnchorDecision::Accept { .. })
        && best.is_none_or(|current| decision.diagnostic_rank() > current.diagnostic_rank())
    {
        *best = Some(decision);
    }
}

#[inline]
fn bitmap_proxy_bytes(symbol: &BitImage) -> usize {
    (symbol.width.saturating_mul(symbol.height).saturating_add(7)) / 8
}

#[inline]
fn classify_residual_shape(symbol: &BitImage) -> ResidualShapeKind {
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
fn record_counterfactual_probe(
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
fn record_labeled_counterfactual_probe(
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
fn relaxed_compare_probe_max_err(candidate: &BitImage, proto: &BitImage) -> u32 {
    candidate
        .width
        .max(proto.width)
        .saturating_mul(candidate.height.max(proto.height)) as u32
}

#[inline]
fn record_detailed_compare_probe(
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
    fn reason_code(self) -> ResidualReasonCode {
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
struct SymUnifyAnchorCandidate {
    anchor_index: usize,
    score: u32,
    dx: i32,
    dy: i32,
    rerank_cost: u32,
    rescued_on_score: bool,
}

#[derive(Debug, Clone, Copy)]
struct ResidualSymbolTrace {
    page_num: usize,
    local_use_count: usize,
    had_local_candidates: bool,
    had_global_candidates: bool,
    local_best_reject: Option<SymUnifyAnchorDecision>,
    global_best_reject: Option<SymUnifyAnchorDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ResidualReasonCode {
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
    fn label(self) -> &'static str {
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
enum ResidualShapeKind {
    Tiny,
    PunctuationLike,
    GlyphLike,
}

#[derive(Debug, Clone, Default)]
struct ResidualReasonStats {
    symbol_count: usize,
    instance_count: usize,
    black_pixels: usize,
    bitmap_proxy_bytes: usize,
    pages: FxHashSet<usize>,
    tiny_count: usize,
    punctuation_like_count: usize,
    glyph_like_count: usize,
    samples: Vec<(usize, usize, usize, usize, usize)>,
}

#[derive(Debug, Clone, Default)]
struct CounterfactualProbeStats {
    symbol_count: usize,
    black_pixels: usize,
    bitmap_proxy_bytes: usize,
    pages: FxHashSet<usize>,
    samples: Vec<(usize, usize, usize, usize)>,
}

#[derive(Debug, Clone, Default)]
struct DetailedCompareProbeStats {
    symbol_count: usize,
    bitmap_proxy_bytes: usize,
    pages: FxHashSet<usize>,
    exact_dims_count: usize,
    strong_anchor_count: usize,
    shift_le1_count: usize,
    over_by_le2_count: usize,
    over_by_le4_count: usize,
    over_by_le8_count: usize,
    over_by_gt8_count: usize,
    samples: Vec<(usize, usize, usize, usize, u32, u32, u32, u32, i32, i32)>,
}

#[derive(Debug)]
struct RecentSymbolCache {
    recent: VecDeque<usize>,
    cap: usize,
}

impl RecentSymbolCache {
    fn new(cap: usize) -> Self {
        Self {
            recent: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn clear(&mut self) {
        self.recent.clear();
    }

    fn touch(&mut self, idx: usize) {
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

    fn copy_into(&self, out: &mut [usize]) -> usize {
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

impl Hash for HashKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl std::fmt::Display for HashKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HashKey({:x})", self.0)
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

/// Segment a document image into symbol candidates.
///
/// This function finds connected components in the input image and returns
/// them as symbol candidates. Each candidate has a bitmap and a bounding box.
///
/// # Arguments
/// * `image` - The input binary image to segment
/// * `dpi` - Resolution in dots per inch (typically 300 for scanned documents)
/// * `losslevel` - 0 for lossless, >0 to enable noise removal
pub fn segment_symbols(image: &BitImage, dpi: i32, losslevel: i32) -> Result<Vec<SymbolCandidate>> {
    #[cfg(feature = "symboldict")]
    {
        // Use the new CC analysis pipeline from jbig2cc
        let cc_image = analyze_page(image, dpi, losslevel);
        let shapes = cc_image.extract_shapes();

        let mut candidates = Vec::with_capacity(shapes.len());
        for (bitmap, bbox) in shapes {
            let rect = Rect {
                x: bbox.xmin as u32,
                y: bbox.ymin as u32,
                width: bbox.width() as u32,
                height: bbox.height() as u32,
            };
            candidates.push(SymbolCandidate { bitmap, bbox: rect });
        }
        Ok(candidates)
    }
    #[cfg(not(feature = "symboldict"))]
    {
        Err(anyhow!("Symbol segmentation requires the symboldict feature"))
    }
}

// Jbig2EncConfig has been removed. Use jbig2structs::Jbig2Config directly.

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

#[derive(Clone)]
pub struct PageData {
    pub image: BitImage,
    pub symbol_instances: Vec<SymbolInstance>,
}

#[derive(Debug, Clone, Default)]
pub struct SymbolModeStageMetrics {
    pub cc_extraction: Duration,
    pub matching_dedup: Duration,
    pub clustering: Duration,
    pub planning: Duration,
    pub symbol_dict_encoding: Duration,
    pub text_region_encoding: Duration,
    pub generic_region_encoding: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct SymbolModeStats {
    pub symbols_discovered: usize,
    pub symbols_exported: usize,
    pub avg_symbol_reuse: f64,
    pub global_symbol_count: usize,
    pub local_symbol_count: usize,
    pub comparator_calls: usize,
    pub comparator_hits: usize,
    pub exact_hits: usize,
    pub refined_hits: usize,
    pub signature_rejects: usize,
}

#[derive(Debug, Clone, Default)]
pub struct EncoderMetrics {
    pub symbol_mode: SymbolModeStageMetrics,
    pub symbol_stats: SymbolModeStats,
}

#[derive(Debug, Clone)]
pub struct PdfSplitOutput {
    pub global_segments: Option<Vec<u8>>,
    pub page_streams: Vec<Vec<u8>>,
    pub local_dict_bytes_per_page: Vec<usize>,
    pub text_region_bytes_per_page: Vec<usize>,
    pub generic_region_bytes_per_page: Vec<usize>,
}

#[derive(Debug)]
struct PlannedPage {
    page_number: u32,
    segments: Vec<Segment>,
}

#[derive(Debug)]
struct PlannedDocument {
    file_header: Option<FileHeader>,
    global_segments: Vec<Segment>,
    pages: Vec<PlannedPage>,
    eof_segment: Option<Segment>,
    next_segment_number: u32,
}

#[derive(Debug, Clone)]
struct PlannedPageLayout {
    page_index: usize,
    page_number: u32,
    page_info_segment_number: u32,
    local_dict_segment_numbers: Vec<u32>,
    local_dict_layout: Option<SymbolDictLayout>,
    region_segment_number: u32,
    residual_region_segment_number: Option<u32>,
    end_of_page_segment_number: u32,
    local_symbols: Vec<usize>,
    residual_symbols: Vec<usize>,
    residual_anchor_remaps: FxHashMap<usize, usize>,
    use_generic_region: bool,
}

#[derive(Debug)]
struct BuiltPage {
    page: PlannedPage,
    symbol_dict_time: Duration,
    text_region_time: Duration,
    generic_region_time: Duration,
}

#[derive(Debug, Clone, Default)]
struct SymbolDictLayout {
    export_input_indices: Vec<usize>,
    refinements: Vec<Option<RefinementPlan>>,
    diagnostics: SymbolDictDiagnostics,
}

impl SymbolDictLayout {
    fn segment_count(&self) -> usize {
        if self.export_input_indices.is_empty() {
            0
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SymbolDictDiagnostics {
    family_count: usize,
    singleton_family_count: usize,
    refined_member_count: usize,
    exported_member_count: usize,
    sample_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct RefinementPlan {
    prototype_input_index: usize,
    refinement_dx: i32,
    refinement_dy: i32,
}

#[derive(Debug, Clone, Default)]
struct EncodedSymbolDictionary {
    payload: Vec<u8>,
    input_to_exported_pos: Vec<u32>,
    exported_symbol_count: u32,
}

/// Mutable state for the encoder that can change during encoding.
#[derive(Debug, Default)]
struct EncoderState {
    pdf_mode: bool,
    full_headers_remaining: bool,
    segment: bool,
    use_refinement: bool,
    use_delta_encoding: bool,
    lossy_symbol_mode_applied: bool,
    ingest_debug_lines: Vec<String>,
    decision_debug_lines: Vec<String>,
}

/// Main JBIG2 encoder that handles document encoding
///
/// This struct manages the encoding state and configuration for JBIG2 documents.
/// It supports both symbol-based and generic region encoding strategies.
pub struct Jbig2Encoder<'a> {
    /// Configuration for the encoder
    config: &'a Jbig2Config,

    /// Internal encoder state
    state: EncoderState,

    /// Global symbols (shared across pages)
    global_symbols: Vec<BitImage>,

    /// Usage count for each global symbol
    symbol_usage: Vec<usize>,

    /// Black pixel count cache for each global symbol (for fast pre-filtering)
    symbol_pixel_counts: Vec<usize>,

    /// Cheap structural signatures used to reject bad matches before full comparison
    symbol_signatures: Vec<SymbolSignature>,

    /// Number of distinct pages where each symbol appears
    symbol_page_count: Vec<usize>,

    /// Last page where the symbol was seen, used to deduplicate per-page membership updates
    symbol_last_page_seen: Vec<Option<usize>>,

    /// Hash map for quick symbol lookup
    hash_map: FxHashMap<HashKey, Vec<usize>>,

    /// Page data for each page in the document
    pages: Vec<PageData>,

    /// Per-page unique symbol indices, built incrementally during extraction
    page_symbol_indices: Vec<Vec<usize>>,

    /// Next available segment number
    next_segment_number: u32,

    /// Segment numbers of the global dictionary segments, in text-region symbol-ID order.
    global_dict_segment_numbers: Vec<u32>,

    /// Encoder metrics used by the benchmark harness
    metrics: EncoderMetrics,
}

impl<'a> Jbig2Encoder<'a> {
    /// Creates a new JBIG2 encoder with the specified configuration
    ///
    /// # Arguments
    /// * `config` - Configuration for the encoder
    pub fn new(config: &'a Jbig2Config) -> Self {
        if config.refine && !config.symbol_mode {
            panic!("Refinement requires symbol mode to be enabled.");
        }

        Self {
            config,
            state: EncoderState {
                pdf_mode: false, // start in raw mode
                full_headers_remaining: config.want_full_headers,
                segment: true,                 // Default to using segments
                use_refinement: config.refine, // Enable refinement based on config
                use_delta_encoding: true,      // Default to using delta encoding
                lossy_symbol_mode_applied: false,
                ingest_debug_lines: Vec::new(),
                decision_debug_lines: Vec::new(),
            },
            global_symbols: Vec::new(),
            symbol_usage: Vec::new(),
            symbol_pixel_counts: Vec::new(),
            symbol_signatures: Vec::new(),
            symbol_page_count: Vec::new(),
            symbol_last_page_seen: Vec::new(),
            hash_map: FxHashMap::default(),
            pages: Vec::new(),
            page_symbol_indices: Vec::new(),
            next_segment_number: 1,
            global_dict_segment_numbers: Vec::new(),
            metrics: EncoderMetrics::default(),
        }
    }

    pub fn dict_only(mut self) -> Self {
        self.state.full_headers_remaining = false;
        self.state.pdf_mode = true;
        self
    }

    /// Returns the number of pages currently added to the encoder
    pub fn get_page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn metrics_snapshot(&self) -> EncoderMetrics {
        self.metrics.clone()
    }

    pub fn decision_debug_log(&self) -> String {
        if self.state.ingest_debug_lines.is_empty() {
            return self.state.decision_debug_lines.join("\n");
        }
        if self.state.decision_debug_lines.is_empty() {
            return self.state.ingest_debug_lines.join("\n");
        }

        let mut out = String::new();
        out.push_str(&self.state.ingest_debug_lines.join("\n"));
        out.push('\n');
        out.push_str(&self.state.decision_debug_lines.join("\n"));
        out
    }

    /// Returns debug information about symbol usage
    pub fn get_symbol_stats(&self) -> String {
        let total_symbols = self.global_symbols.len();
        let avg_usage = if total_symbols > 0 {
            self.symbol_usage.iter().sum::<usize>() as f32 / total_symbols as f32
        } else {
            0.0
        };
        let low_usage_count = self.symbol_usage.iter().filter(|&&u| u < 2).count();

        format!(
            "Total symbols: {}, Average usage: {:.1}, Low usage (<2): {}",
            total_symbols, avg_usage, low_usage_count
        )
    }

    fn compute_symbol_signature(img: &BitImage) -> SymbolSignature {
        compute_symbol_signature_shared(img)
    }

    fn signatures_are_compatible(
        &self,
        candidate: SymbolSignature,
        symbol_index: usize,
        refine: bool,
    ) -> bool {
        let stored = self.symbol_signatures[symbol_index];
        let black_tol = if refine { 12 } else { 8 };
        let pos_tol = if refine { 2 } else { 2 };
        let centroid_tol = if refine { 96 } else { 64 };

        candidate.black.abs_diff(stored.black) <= black_tol
            && candidate.left_col.abs_diff(stored.left_col) <= pos_tol
            && candidate.right_col.abs_diff(stored.right_col) <= pos_tol
            && candidate.top_row.abs_diff(stored.top_row) <= pos_tol
            && candidate.bottom_row.abs_diff(stored.bottom_row) <= pos_tol
            && candidate.cx_times_256.abs_diff(stored.cx_times_256) <= centroid_tol
            && candidate.cy_times_256.abs_diff(stored.cy_times_256) <= centroid_tol
    }

    fn should_skip_symbol_candidate(width: usize, height: usize, black_pixels: usize) -> bool {
        if width == 0 || height == 0 || black_pixels <= 1 {
            return true;
        }
        if (width >= 64 && height <= 2) || (height >= 64 && width <= 2) {
            return true;
        }
        if width > 256 || height > 256 {
            return true;
        }

        let area = width.saturating_mul(height).max(1);
        let density = black_pixels as f32 / area as f32;
        let dense_tiny_mark = width <= 6 && height <= 10 && black_pixels <= 24;
        if dense_tiny_mark {
            return density < 0.01;
        }
        !(0.01..=0.90).contains(&density)
    }

    #[inline(always)]
    fn should_accept_match(
        &self,
        err: u32,
        dx: i32,
        dy: i32,
        exact_dims: bool,
        max_err: u32,
    ) -> (bool, bool) {
        if err == 0 && dx == 0 && dy == 0 && exact_dims {
            return (true, false);
        }

        if self.config.text_refine {
            if dx.abs() <= 1 && dy.abs() <= 1 && err <= (max_err / 2).max(2) {
                return (true, true);
            }
            return (false, false);
        }

        // A non-exact match (err > 0 or a sub-pixel shift) must NOT be substituted
        // wholesale — that renders the wrong glyph (e.g. "i" as "'"). Accept it but
        // flag it for per-instance refinement so the decoder reconstructs the
        // original bitmap from the prototype + the coded difference (lossless).
        // In an explicitly lossy mode (sym-unify) substitution is intended, so the
        // old behaviour (accept without refinement) is preserved there.
        if dx.abs() <= 1 && dy == 0 {
            let lossless = self.config.lossy_symbol_mode == LossySymbolMode::Off;
            return (true, lossless);
        }

        (false, false)
    }

    #[inline]
    fn symbol_unify_assignment_score(result: &crate::jbig2comparator::CompareResult) -> u32 {
        result
            .total_err
            .saturating_add(result.black_delta.saturating_mul(2))
            .saturating_add(result.outside_ink_err.saturating_mul(3))
            .saturating_add(((result.dx.abs() + result.dy.abs()) as u32).saturating_mul(3))
            .saturating_add((result.row_profile_err + result.col_profile_err) / 24)
    }

    fn sym_unify_context_rerank_cost(candidate: &BitImage, proto: &BitImage) -> u32 {
        let width = candidate.width.max(proto.width);
        let height = candidate.height.max(proto.height);
        let mut cost = 0u32;

        for y in 0..height {
            for x in 0..width {
                let cand = candidate.get_usize(x, y);
                let proto_bit = proto.get_usize(x, y);
                if !cand && !proto_bit {
                    continue;
                }

                let proto_support = (-1i32..=1)
                    .flat_map(|dy| (-1i32..=1).map(move |dx| (dx, dy)))
                    .filter(|&(dx, dy)| dx != 0 || dy != 0)
                    .filter(|&(dx, dy)| {
                        let nx = x as i32 + dx;
                        let ny = y as i32 + dy;
                        nx >= 0 && ny >= 0 && proto.get_usize(nx as usize, ny as usize)
                    })
                    .count() as u32;
                let causal_support = [(-1i32, 0i32), (-1, -1), (0, -1), (1, -1)]
                    .into_iter()
                    .filter(|&(dx, dy)| {
                        let nx = x as i32 + dx;
                        let ny = y as i32 + dy;
                        nx >= 0 && ny >= 0 && candidate.get_usize(nx as usize, ny as usize)
                    })
                    .count() as u32;

                if cand == proto_bit {
                    cost = cost.saturating_add(1 + u32::from(cand && proto_support == 0));
                } else {
                    cost = cost.saturating_add(4 + proto_support + causal_support);
                }
            }
        }

        cost
    }

    fn sym_unify_anchor_candidate_is_better(
        &self,
        candidate: SymUnifyAnchorCandidate,
        current: SymUnifyAnchorCandidate,
    ) -> bool {
        (
            !candidate.rescued_on_score,
            std::cmp::Reverse(candidate.rerank_cost),
            std::cmp::Reverse(candidate.score),
            self.symbol_page_count[candidate.anchor_index],
            self.symbol_usage[candidate.anchor_index],
            std::cmp::Reverse(candidate.anchor_index),
        ) > (
            !current.rescued_on_score,
            std::cmp::Reverse(current.rerank_cost),
            std::cmp::Reverse(current.score),
            self.symbol_page_count[current.anchor_index],
            self.symbol_usage[current.anchor_index],
            std::cmp::Reverse(current.anchor_index),
        )
    }

    fn maybe_update_best_sym_unify_anchor_candidate(
        &self,
        best: &mut Option<SymUnifyAnchorCandidate>,
        candidate_bitmap: &BitImage,
        anchor_index: usize,
        score: u32,
        dx: i32,
        dy: i32,
        rescued_on_score: bool,
    ) {
        let rerank_cost = Self::sym_unify_context_rerank_cost(
            candidate_bitmap,
            &self.global_symbols[anchor_index],
        );
        let proposal = SymUnifyAnchorCandidate {
            anchor_index,
            score,
            dx,
            dy,
            rerank_cost,
            rescued_on_score,
        };
        if best.is_none_or(|current| self.sym_unify_anchor_candidate_is_better(proposal, current)) {
            *best = Some(proposal);
        }
    }

    #[inline]
    fn sym_unify_anchor_ready(&self, symbol_index: usize, page_num: usize) -> bool {
        if self.symbol_usage[symbol_index] < 2 || self.symbol_pixel_counts[symbol_index] <= 1 {
            return false;
        }

        let usage_ready =
            self.symbol_usage[symbol_index] >= self.config.sym_unify_min_class_usage.max(2);
        let page_span_ready =
            self.symbol_page_count[symbol_index] >= self.config.sym_unify_min_page_span.max(2);
        let recent_ready = self.symbol_last_page_seen[symbol_index]
            .map(|last| page_num.saturating_sub(last) <= 1)
            .unwrap_or(false)
            && self.symbol_usage[symbol_index] >= 3;

        usage_ready || page_span_ready || recent_ready
    }

    fn build_sym_unify_anchor_map(
        &self,
        page_num: usize,
    ) -> FxHashMap<FamilyBucketKey, Vec<usize>> {
        let mut anchors: FxHashMap<FamilyBucketKey, Vec<usize>> = FxHashMap::default();
        for symbol_index in 0..self.global_symbols.len() {
            if !self.sym_unify_anchor_ready(symbol_index, page_num) {
                continue;
            }
            let key = family_bucket_key_for_symbol(
                &self.global_symbols[symbol_index],
                &self.symbol_signatures[symbol_index],
            );
            anchors.entry(key).or_default().push(symbol_index);
        }
        for bucket in anchors.values_mut() {
            bucket.sort_unstable_by(|&lhs, &rhs| {
                self.symbol_page_count[rhs]
                    .cmp(&self.symbol_page_count[lhs])
                    .then_with(|| self.symbol_usage[rhs].cmp(&self.symbol_usage[lhs]))
                    .then_with(|| self.symbol_pixel_counts[rhs].cmp(&self.symbol_pixel_counts[lhs]))
                    .then_with(|| lhs.cmp(&rhs))
            });
        }
        anchors
    }

    fn maybe_add_sym_unify_anchor(
        &self,
        anchors: &mut FxHashMap<FamilyBucketKey, Vec<usize>>,
        symbol_index: usize,
        page_num: usize,
    ) {
        if !self.sym_unify_anchor_ready(symbol_index, page_num) {
            return;
        }
        let key = family_bucket_key_for_symbol(
            &self.global_symbols[symbol_index],
            &self.symbol_signatures[symbol_index],
        );
        let bucket = anchors.entry(key).or_default();
        if !bucket.contains(&symbol_index) {
            bucket.push(symbol_index);
            bucket.sort_unstable_by(|&lhs, &rhs| {
                self.symbol_page_count[rhs]
                    .cmp(&self.symbol_page_count[lhs])
                    .then_with(|| self.symbol_usage[rhs].cmp(&self.symbol_usage[lhs]))
                    .then_with(|| self.symbol_pixel_counts[rhs].cmp(&self.symbol_pixel_counts[lhs]))
                    .then_with(|| lhs.cmp(&rhs))
            });
        }
    }

    fn residual_symbol_matches_anchor(
        &self,
        residual_index: usize,
        anchor_index: usize,
        comparator: &mut Comparator,
    ) -> bool {
        matches!(
            self.residual_symbol_anchor_decision(residual_index, anchor_index, comparator),
            SymUnifyAnchorDecision::Accept { .. }
        )
    }

    fn residual_symbol_anchor_decision(
        &self,
        residual_index: usize,
        anchor_index: usize,
        comparator: &mut Comparator,
    ) -> SymUnifyAnchorDecision {
        let candidate = &self.global_symbols[residual_index];
        let proto = &self.global_symbols[anchor_index];
        if candidate.width.abs_diff(proto.width) > 1 || candidate.height.abs_diff(proto.height) > 1
        {
            return SymUnifyAnchorDecision::RejectDim;
        }

        let strong_anchor = self.symbol_usage[anchor_index] >= SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE
            || self.symbol_page_count[anchor_index] >= SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN;
        let exact_dims = candidate.width == proto.width && candidate.height == proto.height;
        let area = candidate
            .width
            .max(proto.width)
            .saturating_mul(candidate.height.max(proto.height));
        let pixel_delta_limit = (area / 10).clamp(4, 16) + usize::from(strong_anchor);
        let black_delta = self.symbol_pixel_counts[anchor_index]
            .abs_diff(self.symbol_pixel_counts[residual_index]);
        if black_delta > pixel_delta_limit {
            return SymUnifyAnchorDecision::RejectPixelDelta;
        }
        let signature_compatible = family_signatures_are_compatible(
            self.symbol_signatures[residual_index],
            self.symbol_signatures[anchor_index],
            self.symbol_pixel_counts[residual_index],
            self.symbol_pixel_counts[anchor_index],
        );
        if !signature_compatible {
            let soft_signature_black_delta_limit = 4 + usize::from(strong_anchor);
            if !exact_dims || black_delta > soft_signature_black_delta_limit {
                return SymUnifyAnchorDecision::RejectSignature;
            }
        }

        let overlap_limit = self
            .config
            .sym_unify_max_err
            .max(4)
            .saturating_add(2)
            .saturating_add(u32::from(strong_anchor))
            .min(15);
        let Some(overlap) = comparator.compare_overlap_only(candidate, proto, overlap_limit) else {
            return SymUnifyAnchorDecision::RejectOverlap;
        };
        if overlap.dx.abs() > self.config.sym_unify_max_dx.max(0)
            || overlap.dy.abs() > self.config.sym_unify_max_dy.max(0)
            || overlap.overlap_err > overlap_limit
            || overlap.black_delta > pixel_delta_limit as u32
        {
            return SymUnifyAnchorDecision::RejectOverlap;
        }

        let compare_max_err = self
            .config
            .sym_unify_max_err
            .max(4)
            .saturating_add(u32::from(strong_anchor));
        let Some(result) = comparator.compare_for_symbol_unify(
            candidate,
            proto,
            compare_max_err,
            self.config.sym_unify_max_dx.max(0),
            self.config.sym_unify_max_dy.max(0),
        ) else {
            return SymUnifyAnchorDecision::RejectCompare;
        };

        let outside_limit =
            self.config.sym_unify_max_border_outside_ink.min(1) + u32::from(strong_anchor);
        if result.outside_ink_err > outside_limit {
            return SymUnifyAnchorDecision::RejectOutsideInk;
        }

        let score = Self::symbol_unify_assignment_score(&result);
        let score_limit = self.config.sym_unify_class_accept_limit + u32::from(strong_anchor);
        if score > score_limit {
            return SymUnifyAnchorDecision::RejectScore {
                score,
                limit: score_limit,
                dx: result.dx,
                dy: result.dy,
            };
        }

        SymUnifyAnchorDecision::Accept {
            score,
            dx: result.dx,
            dy: result.dy,
        }
    }

    fn residual_symbol_accept_with_dim_limit(
        &self,
        residual_index: usize,
        anchor_index: usize,
        comparator: &mut Comparator,
        dim_limit: usize,
    ) -> bool {
        let candidate = &self.global_symbols[residual_index];
        let proto = &self.global_symbols[anchor_index];
        if candidate.width.abs_diff(proto.width) > dim_limit
            || candidate.height.abs_diff(proto.height) > dim_limit
        {
            return false;
        }

        let strong_anchor = self.symbol_usage[anchor_index] >= SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE
            || self.symbol_page_count[anchor_index] >= SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN;
        let exact_dims = candidate.width == proto.width && candidate.height == proto.height;
        let area = candidate
            .width
            .max(proto.width)
            .saturating_mul(candidate.height.max(proto.height));
        let pixel_delta_limit = (area / 10).clamp(4, 16) + usize::from(strong_anchor);
        let black_delta = self.symbol_pixel_counts[anchor_index]
            .abs_diff(self.symbol_pixel_counts[residual_index]);
        if black_delta > pixel_delta_limit {
            return false;
        }

        let signature_compatible = family_signatures_are_compatible(
            self.symbol_signatures[residual_index],
            self.symbol_signatures[anchor_index],
            self.symbol_pixel_counts[residual_index],
            self.symbol_pixel_counts[anchor_index],
        );
        if !signature_compatible {
            let soft_signature_black_delta_limit = 4 + usize::from(strong_anchor);
            if !exact_dims || black_delta > soft_signature_black_delta_limit {
                return false;
            }
        }

        let overlap_limit = self
            .config
            .sym_unify_max_err
            .max(4)
            .saturating_add(2)
            .saturating_add(u32::from(strong_anchor))
            .min(15);
        let Some(overlap) = comparator.compare_overlap_only(candidate, proto, overlap_limit) else {
            return false;
        };
        if overlap.dx.abs() > self.config.sym_unify_max_dx.max(0)
            || overlap.dy.abs() > self.config.sym_unify_max_dy.max(0)
            || overlap.overlap_err > overlap_limit
            || overlap.black_delta > pixel_delta_limit as u32
        {
            return false;
        }

        let compare_max_err = self
            .config
            .sym_unify_max_err
            .max(4)
            .saturating_add(u32::from(strong_anchor));
        let Some(result) = comparator.compare_for_symbol_unify(
            candidate,
            proto,
            compare_max_err,
            self.config.sym_unify_max_dx.max(0),
            self.config.sym_unify_max_dy.max(0),
        ) else {
            return false;
        };

        let outside_limit =
            self.config.sym_unify_max_border_outside_ink.min(1) + u32::from(strong_anchor);
        if result.outside_ink_err > outside_limit {
            return false;
        }

        let score = Self::symbol_unify_assignment_score(&result);
        let score_limit = self.config.sym_unify_class_accept_limit + u32::from(strong_anchor);
        score <= score_limit
    }

    fn residual_symbol_accept_without_overlap_prescreen(
        &self,
        residual_index: usize,
        anchor_index: usize,
        comparator: &mut Comparator,
    ) -> bool {
        matches!(
            self.residual_symbol_anchor_decision_without_overlap_prescreen(
                residual_index,
                anchor_index,
                comparator,
            ),
            SymUnifyAnchorDecision::Accept { .. }
        )
    }

    fn residual_symbol_anchor_decision_without_overlap_prescreen(
        &self,
        residual_index: usize,
        anchor_index: usize,
        comparator: &mut Comparator,
    ) -> SymUnifyAnchorDecision {
        let candidate = &self.global_symbols[residual_index];
        let proto = &self.global_symbols[anchor_index];
        if candidate.width.abs_diff(proto.width) > 1 || candidate.height.abs_diff(proto.height) > 1
        {
            return SymUnifyAnchorDecision::RejectDim;
        }

        let strong_anchor = self.symbol_usage[anchor_index] >= SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE
            || self.symbol_page_count[anchor_index] >= SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN;
        let exact_dims = candidate.width == proto.width && candidate.height == proto.height;
        let area = candidate
            .width
            .max(proto.width)
            .saturating_mul(candidate.height.max(proto.height));
        let pixel_delta_limit = (area / 10).clamp(4, 16) + usize::from(strong_anchor);
        let black_delta = self.symbol_pixel_counts[anchor_index]
            .abs_diff(self.symbol_pixel_counts[residual_index]);
        if black_delta > pixel_delta_limit {
            return SymUnifyAnchorDecision::RejectPixelDelta;
        }

        let signature_compatible = family_signatures_are_compatible(
            self.symbol_signatures[residual_index],
            self.symbol_signatures[anchor_index],
            self.symbol_pixel_counts[residual_index],
            self.symbol_pixel_counts[anchor_index],
        );
        if !signature_compatible {
            let soft_signature_black_delta_limit = 4 + usize::from(strong_anchor);
            if !exact_dims || black_delta > soft_signature_black_delta_limit {
                return SymUnifyAnchorDecision::RejectSignature;
            }
        }

        let compare_max_err = self
            .config
            .sym_unify_max_err
            .max(4)
            .saturating_add(u32::from(strong_anchor));
        let Some(result) = comparator.compare_for_symbol_unify(
            candidate,
            proto,
            compare_max_err,
            self.config.sym_unify_max_dx.max(0),
            self.config.sym_unify_max_dy.max(0),
        ) else {
            return SymUnifyAnchorDecision::RejectCompare;
        };

        let outside_limit =
            self.config.sym_unify_max_border_outside_ink.min(1) + u32::from(strong_anchor);
        if result.outside_ink_err > outside_limit {
            return SymUnifyAnchorDecision::RejectOutsideInk;
        }

        let score = Self::symbol_unify_assignment_score(&result);
        let score_limit = self.config.sym_unify_class_accept_limit + u32::from(strong_anchor);
        if score > score_limit {
            return SymUnifyAnchorDecision::RejectScore {
                score,
                limit: score_limit,
                dx: result.dx,
                dy: result.dy,
            };
        }

        SymUnifyAnchorDecision::Accept {
            score,
            dx: result.dx,
            dy: result.dy,
        }
    }

    #[inline(always)]
    fn evaluate_symbol_match(
        &mut self,
        candidate: &BitImage,
        candidate_sig: SymbolSignature,
        candidate_pixels: usize,
        symbol_index: usize,
        comparator: &mut Comparator,
        max_err: u32,
    ) -> Option<(u32, i32, i32, bool)> {
        let proto = &self.global_symbols[symbol_index];
        let dim_limit = if self.config.text_refine { 2 } else { 0 };
        if (candidate.width as i32 - proto.width as i32).unsigned_abs() > dim_limit
            || (candidate.height as i32 - proto.height as i32).unsigned_abs() > dim_limit
        {
            return None;
        }
        if self.symbol_pixel_counts[symbol_index].abs_diff(candidate_pixels)
            > max_err as usize + if self.config.text_refine { 8 } else { 6 }
        {
            return None;
        }
        if !self.signatures_are_compatible(candidate_sig, symbol_index, self.config.text_refine) {
            self.metrics.symbol_stats.signature_rejects += 1;
            return None;
        }

        self.metrics.symbol_stats.comparator_calls += 1;
        let (err, dx, dy) = if self.config.text_refine {
            comparator
                .compare_for_refine_family(candidate, proto, max_err, 1, 1)
                .map(|r| (r.total_err, r.dx, r.dy))?
        } else {
            comparator
                .compare_for_refine_family(candidate, proto, max_err, 1, 0)
                .map(|r| (r.total_err, r.dx, r.dy))?
        };
        self.metrics.symbol_stats.comparator_hits += 1;

        let exact_dims = candidate.width == proto.width && candidate.height == proto.height;
        let (accept, needs_refinement) = self.should_accept_match(err, dx, dy, exact_dims, max_err);
        if !accept {
            return None;
        }

        if needs_refinement {
            self.metrics.symbol_stats.refined_hits += 1;
        } else if err == 0 && dx == 0 && dy == 0 && exact_dims {
            self.metrics.symbol_stats.exact_hits += 1;
        }

        Some((err, dx, dy, needs_refinement))
    }

    #[inline(always)]
    fn evaluate_symbol_unify_anchor_match(
        &mut self,
        candidate: &BitImage,
        candidate_sig: SymbolSignature,
        candidate_pixels: usize,
        symbol_index: usize,
        comparator: &mut Comparator,
    ) -> SymUnifyAnchorDecision {
        let proto = &self.global_symbols[symbol_index];
        if candidate.width.abs_diff(proto.width) > 1 || candidate.height.abs_diff(proto.height) > 1
        {
            return SymUnifyAnchorDecision::RejectDim;
        }

        let area = candidate
            .width
            .max(proto.width)
            .saturating_mul(candidate.height.max(proto.height));
        let strong_anchor = self.symbol_usage[symbol_index] >= SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE
            || self.symbol_page_count[symbol_index] >= SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN;
        let pixel_delta_limit = (area / 10).clamp(4, 16) + usize::from(strong_anchor);
        if self.symbol_pixel_counts[symbol_index].abs_diff(candidate_pixels) > pixel_delta_limit {
            return SymUnifyAnchorDecision::RejectPixelDelta;
        }

        if !family_signatures_are_compatible(
            candidate_sig,
            self.symbol_signatures[symbol_index],
            candidate_pixels,
            self.symbol_pixel_counts[symbol_index],
        ) {
            self.metrics.symbol_stats.signature_rejects += 1;
            return SymUnifyAnchorDecision::RejectSignature;
        }

        let overlap_limit = self
            .config
            .sym_unify_max_err
            .max(4)
            .saturating_add(2)
            .saturating_add(u32::from(strong_anchor))
            .min(15);
        let Some(overlap) = comparator.compare_overlap_only(candidate, proto, overlap_limit) else {
            return SymUnifyAnchorDecision::RejectOverlap;
        };
        if overlap.dx.abs() > self.config.sym_unify_max_dx.max(0)
            || overlap.dy.abs() > self.config.sym_unify_max_dy.max(0)
            || overlap.overlap_err > overlap_limit
            || overlap.black_delta > pixel_delta_limit as u32
        {
            return SymUnifyAnchorDecision::RejectOverlap;
        }

        self.metrics.symbol_stats.comparator_calls += 1;
        let compare_max_err = self
            .config
            .sym_unify_max_err
            .max(4)
            .saturating_add(u32::from(strong_anchor));
        let Some(result) = comparator.compare_for_symbol_unify(
            candidate,
            proto,
            compare_max_err,
            self.config.sym_unify_max_dx.max(0),
            self.config.sym_unify_max_dy.max(0),
        ) else {
            return SymUnifyAnchorDecision::RejectCompare;
        };
        self.metrics.symbol_stats.comparator_hits += 1;

        let score = Self::symbol_unify_assignment_score(&result);
        let outside_limit =
            self.config.sym_unify_max_border_outside_ink.min(1) + u32::from(strong_anchor);
        let score_limit = self.config.sym_unify_class_accept_limit + u32::from(strong_anchor);
        if result.outside_ink_err > outside_limit {
            return SymUnifyAnchorDecision::RejectOutsideInk;
        }
        if score > score_limit {
            return SymUnifyAnchorDecision::RejectScore {
                score,
                limit: score_limit,
                dx: result.dx,
                dy: result.dy,
            };
        }

        SymUnifyAnchorDecision::Accept {
            score,
            dx: result.dx,
            dy: result.dy,
        }
    }

    fn estimate_local_symbol_gain(&self, page: &PageData, symbol_index: usize) -> i64 {
        let uses = page
            .symbol_instances
            .iter()
            .filter(|instance| instance.symbol_index == symbol_index)
            .count() as i64;
        let symbol = &self.global_symbols[symbol_index];
        let area = (symbol.width * symbol.height) as i64;
        let dict_cost = 24 + (area / 8);
        let saved_per_use = (area / 10).max(2);
        (uses * saved_per_use) - dict_cost
    }

    fn estimate_global_symbol_gain(&self, symbol_index: usize) -> i64 {
        let uses = self.symbol_usage[symbol_index] as i64;
        let page_span = self.symbol_page_count[symbol_index] as i64;
        let symbol = &self.global_symbols[symbol_index];
        let area = (symbol.width * symbol.height) as i64;
        let dict_cost = 24 + (area / 8);
        let id_savings = ((uses - page_span).max(0)) * 2;
        let reuse_value = (uses * (area / 12).max(2)) + (page_span * 3);
        reuse_value + id_savings - dict_cost
    }

    fn should_keep_text_local_symbol(&self, page: &PageData, symbol_index: usize) -> bool {
        let _ = (page, symbol_index);
        false
    }

    fn choose_cluster_prototype(&self, members: &[usize]) -> usize {
        if members.len() <= 1 || !self.config.text_refine {
            return *members
                .iter()
                .max_by(|&&lhs, &&rhs| {
                    self.symbol_usage[lhs]
                        .cmp(&self.symbol_usage[rhs])
                        .then_with(|| {
                            self.symbol_pixel_counts[lhs].cmp(&self.symbol_pixel_counts[rhs])
                        })
                        .then_with(|| rhs.cmp(&lhs))
                })
                .unwrap();
        }

        let mut comparator = Comparator::default();
        let mut best_idx = members[0];
        let mut best_cost = u64::MAX;

        for &candidate in members {
            let candidate_symbol = &self.global_symbols[candidate];
            let mut total_cost = 0u64;
            for &other in members {
                if candidate == other {
                    continue;
                }
                let other_symbol = &self.global_symbols[other];
                let area = candidate_symbol.width.max(other_symbol.width)
                    * candidate_symbol.height.max(other_symbol.height);
                let max_err = ((self.symbol_pixel_counts[candidate]
                    .max(self.symbol_pixel_counts[other]) as f32
                    * 0.10) as u32)
                    .max((area / self.config.match_tolerance.max(1) as usize) as u32)
                    .clamp(3, 20);

                match comparator.compare_for_refine_family(
                    other_symbol,
                    candidate_symbol,
                    max_err,
                    2,
                    1,
                ) {
                    Some(result) => {
                        let err = result.total_err;
                        let dx = result.dx;
                        let dy = result.dy;
                        let refinement_penalty = err as u64 + ((dx.abs() + dy.abs()) as u64 * 2);
                        total_cost += refinement_penalty * self.symbol_usage[other] as u64;
                    }
                    None => total_cost += 1_000_000,
                }
            }

            if total_cost < best_cost
                || (total_cost == best_cost
                    && (
                        self.symbol_usage[candidate],
                        self.symbol_pixel_counts[candidate],
                    ) > (
                        self.symbol_usage[best_idx],
                        self.symbol_pixel_counts[best_idx],
                    ))
            {
                best_cost = total_cost;
                best_idx = candidate;
            }
        }

        best_idx
    }

    fn note_symbol_page(&mut self, symbol_index: usize, page_num: usize) {
        if self.symbol_last_page_seen[symbol_index] != Some(page_num) {
            self.symbol_last_page_seen[symbol_index] = Some(page_num);
            self.symbol_page_count[symbol_index] += 1;
            self.page_symbol_indices[page_num].push(symbol_index);
        }
    }

    fn push_symbol(&mut self, symbol: BitImage, pixel_count: usize, page_num: usize) -> usize {
        let idx = self.global_symbols.len();
        self.symbol_signatures
            .push(Self::compute_symbol_signature(&symbol));
        self.symbol_pixel_counts.push(pixel_count);
        self.global_symbols.push(symbol);
        self.symbol_usage.push(1);
        self.symbol_page_count.push(0);
        self.symbol_last_page_seen.push(None);
        self.note_symbol_page(idx, page_num);
        idx
    }

    fn rebuild_symbol_metadata(&mut self) {
        self.symbol_usage = vec![0; self.global_symbols.len()];
        self.symbol_page_count = vec![0; self.global_symbols.len()];
        self.symbol_last_page_seen = vec![None; self.global_symbols.len()];
        self.page_symbol_indices = vec![Vec::new(); self.pages.len()];
        self.symbol_pixel_counts = self
            .global_symbols
            .iter()
            .map(BitImage::count_ones)
            .collect();
        self.symbol_signatures = self
            .global_symbols
            .iter()
            .map(Self::compute_symbol_signature)
            .collect();

        for page_num in 0..self.pages.len() {
            let instance_indices: Vec<usize> = self.pages[page_num]
                .symbol_instances
                .iter()
                .map(|inst| inst.symbol_index)
                .collect();
            for symbol_index in instance_indices {
                self.symbol_usage[symbol_index] += 1;
                self.note_symbol_page(symbol_index, page_num);
            }
        }
    }

    fn rebuild_hash_map(&mut self) {
        self.hash_map.clear();
        self.hash_map.reserve(self.global_symbols.len());
        for (idx, symbol) in self.global_symbols.iter().enumerate() {
            let key = hash_key(symbol);
            self.hash_map.entry(key).or_default().push(idx);
        }
    }

    fn build_symbol_unify_classes(&mut self) -> Vec<UnifiedClass> {
        let diagnostics_enabled = encoder_diagnostics_enabled();
        let context_model =
            build_symbol_context_model(&self.pages, &self.global_symbols, &self.symbol_signatures);
        let (classes, diagnostics) =
            crate::jbig2unify::build_symbol_unify_classes(SymbolUnifyInputs {
                config: self.config,
                global_symbols: &self.global_symbols,
                symbol_usage: &self.symbol_usage,
                symbol_page_count: &self.symbol_page_count,
                symbol_signatures: &self.symbol_signatures,
                symbol_pixel_counts: &self.symbol_pixel_counts,
                context_model: Some(&context_model),
                collect_diagnostics: diagnostics_enabled,
            });
        if diagnostics_enabled {
            self.state.decision_debug_lines.extend(diagnostics.lines);
        }
        classes
    }

    fn compact_symbol_table_after_remap(&mut self) {
        let mut used = vec![false; self.global_symbols.len()];
        for page in &self.pages {
            for instance in &page.symbol_instances {
                if instance.symbol_index < used.len() {
                    used[instance.symbol_index] = true;
                }
            }
        }

        let old_symbols = self.global_symbols.clone();
        let mut new_index = vec![usize::MAX; old_symbols.len()];
        let mut new_symbols = Vec::new();

        for (old_index, symbol) in old_symbols.into_iter().enumerate() {
            if used[old_index] {
                new_index[old_index] = new_symbols.len();
                new_symbols.push(symbol);
            }
        }

        for page in &mut self.pages {
            for instance in &mut page.symbol_instances {
                instance.symbol_index = new_index[instance.symbol_index];
            }
        }

        self.global_symbols = new_symbols;
        self.rebuild_symbol_metadata();
        self.rebuild_hash_map();
    }

    fn alias_local_symbols_to_globals(&mut self) -> Result<()> {
        if self.pages.len() <= 1 || self.global_symbols.is_empty() {
            return Ok(());
        }
        let text_refine = self.config.text_refine;
        let global_indices: Vec<usize> = self
            .global_symbols
            .iter()
            .enumerate()
            .filter(|(i, _)| self.symbol_page_count[*i] > 1)
            .map(|(i, _)| i)
            .collect();
        if global_indices.is_empty() {
            return Ok(());
        }

        let mut global_bucket_map: FxHashMap<HashKey, Vec<usize>> =
            FxHashMap::with_capacity_and_hasher(global_indices.len(), Default::default());
        for &symbol_index in &global_indices {
            global_bucket_map
                .entry(hash_key(&self.global_symbols[symbol_index]))
                .or_default()
                .push(symbol_index);
        }

        let mut comparator = Comparator::default();
        let mut changed = false;
        let mut aliased_symbols = 0usize;
        let mut aliased_instances = 0usize;
        let mut alias_samples = Vec::new();
        for page in &mut self.pages {
            let mut page_local_symbols: FxHashSet<usize> =
                FxHashSet::with_capacity_and_hasher(256, Default::default());
            for instance in &page.symbol_instances {
                if self.symbol_page_count[instance.symbol_index] <= 1 {
                    page_local_symbols.insert(instance.symbol_index);
                }
            }

            for local_symbol_index in page_local_symbols {
                let local_symbol = &self.global_symbols[local_symbol_index];
                let local_sig = self.symbol_signatures[local_symbol_index];
                let pixel_count = self.symbol_pixel_counts[local_symbol_index];
                let area = (local_symbol.width * local_symbol.height) as u32;
                let max_err = if self.config.text_refine {
                    (area / self.config.match_tolerance.max(1)).max(3)
                } else {
                    ((area as f32 * 0.05) as u32).max(2)
                };
                let dim_range: u64 = if self.config.text_refine || self.config.refine {
                    2
                } else {
                    0
                };

                let mut best_match: Option<(usize, u32, i32, i32, bool)> = None;
                let h = local_symbol.height as u64;
                let w = local_symbol.width as u64;
                'bucket_search: for dh_off in 0..=(dim_range * 2) {
                    let dh = h.wrapping_add(dh_off).wrapping_sub(dim_range);
                    if dh >= 10_000 {
                        continue;
                    }
                    for dw_off in 0..=(dim_range * 2) {
                        let dw = w.wrapping_add(dw_off).wrapping_sub(dim_range);
                        if dw >= 10_000 {
                            continue;
                        }
                        let bucket_key = HashKey(dh * 10_000 + dw);
                        let Some(bucket) = global_bucket_map.get(&bucket_key) else {
                            continue;
                        };
                        for &global_symbol_index in bucket {
                            if self.symbol_pixel_counts[global_symbol_index].abs_diff(pixel_count)
                                > max_err as usize + if self.config.text_refine { 8 } else { 6 }
                            {
                                continue;
                            }
                            let stored = self.symbol_signatures[global_symbol_index];
                            let black_tol = if text_refine { 12 } else { 8 };
                            let pos_tol = 2;
                            let centroid_tol = if text_refine { 96 } else { 64 };
                            if local_sig.black.abs_diff(stored.black) > black_tol
                                || local_sig.left_col.abs_diff(stored.left_col) > pos_tol
                                || local_sig.right_col.abs_diff(stored.right_col) > pos_tol
                                || local_sig.top_row.abs_diff(stored.top_row) > pos_tol
                                || local_sig.bottom_row.abs_diff(stored.bottom_row) > pos_tol
                                || local_sig.cx_times_256.abs_diff(stored.cx_times_256)
                                    > centroid_tol
                                || local_sig.cy_times_256.abs_diff(stored.cy_times_256)
                                    > centroid_tol
                            {
                                continue;
                            }
                            let max_dx = if text_refine { 1 } else { 1 };
                            let max_dy = if text_refine { 1 } else { 0 };
                            let Some(result) = comparator.compare_for_refine_family(
                                local_symbol,
                                &self.global_symbols[global_symbol_index],
                                max_err,
                                max_dx,
                                max_dy,
                            ) else {
                                continue;
                            };
                            let err = result.total_err;
                            let dx = result.dx;
                            let dy = result.dy;
                            let exact_dims = local_symbol.width
                                == self.global_symbols[global_symbol_index].width
                                && local_symbol.height
                                    == self.global_symbols[global_symbol_index].height;
                            let (accept, needs_refinement) =
                                if err == 0 && dx == 0 && dy == 0 && exact_dims {
                                    (true, false)
                                } else if text_refine {
                                    (
                                        dx.abs() <= 1
                                            && dy.abs() <= 1
                                            && err <= (max_err / 2).max(2),
                                        true,
                                    )
                                } else if dx.abs() <= 1 && dy == 0 {
                                    // Non-exact: alias onto the global prototype but
                                    // refine per-instance so the original bitmap is
                                    // reconstructed losslessly (no substitution). In an
                                    // explicitly lossy mode (sym-unify) substitution is
                                    // intended, so refinement is not forced there.
                                    (
                                        true,
                                        self.config.lossy_symbol_mode == LossySymbolMode::Off,
                                    )
                                } else {
                                    (false, false)
                                };
                            if !accept {
                                continue;
                            }
                            best_match = Some((global_symbol_index, err, dx, dy, needs_refinement));
                            if err == 0 && dx == 0 && dy == 0 {
                                break 'bucket_search;
                            }
                        }
                    }
                }

                let Some((global_symbol_index, _err, dx, dy, needs_refinement)) = best_match else {
                    continue;
                };
                aliased_symbols += 1;
                for instance in &mut page.symbol_instances {
                    if instance.symbol_index == local_symbol_index {
                        instance.symbol_index = global_symbol_index;
                        instance.needs_refinement = needs_refinement;
                        instance.refinement_dx = if needs_refinement { dx } else { 0 };
                        instance.refinement_dy = if needs_refinement { dy } else { 0 };
                        changed = true;
                        aliased_instances += 1;
                    }
                }
                if alias_samples.len() < 64 {
                    alias_samples.push(format!(
                        "alias local->global: local={} global={} dx={} dy={} refine={}",
                        local_symbol_index, global_symbol_index, dx, dy, needs_refinement
                    ));
                }
            }
        }

        if encoder_diagnostics_enabled() {
            if changed {
                self.state.decision_debug_lines.push(format!(
                    "alias pass: {} local symbols / {} instances remapped onto globals",
                    aliased_symbols, aliased_instances
                ));
                self.state.decision_debug_lines.extend(alias_samples);
            } else {
                self.state
                    .decision_debug_lines
                    .push("alias pass: no local symbols remapped onto globals".to_string());
            }
        }
        if changed {
            self.compact_symbol_table_after_remap();
        }

        Ok(())
    }

    fn apply_symbol_unify(&mut self) -> Result<()> {
        if !self.config.uses_symbol_unify() || self.state.lossy_symbol_mode_applied {
            return Ok(());
        }

        let diagnostics_enabled = encoder_diagnostics_enabled();
        let before_exported = self.global_symbols.len();
        let before_estimated_dict_bytes =
            symbol_dictionary_entries_bytes(self.global_symbols.iter());
        let classes = self.build_symbol_unify_classes();
        if classes.is_empty() {
            if diagnostics_enabled {
                self.state
                    .decision_debug_lines
                    .push("sym_unify: no eligible classes".to_string());
            }
            self.state.lossy_symbol_mode_applied = true;
            return Ok(());
        }

        let mut remap: Vec<usize> = (0..self.global_symbols.len()).collect();
        let mut refinement_remap: Vec<Option<RefinementPlan>> =
            vec![None; self.global_symbols.len()];
        let mut unified_members = 0usize;
        let mut border_unified_members = 0usize;
        let mut refined_members = 0usize;
        let mut refinement_subclusters = 0usize;
        let mut retained_border_members = 0usize;
        let mut retained_outlier_members = 0usize;

        if diagnostics_enabled {
            self.state.decision_debug_lines.push(format!(
                "sym_unify: {} classes eligible across {} symbols",
                classes.len(),
                self.global_symbols.len()
            ));

            for class in classes.iter().take(64) {
                self.state.decision_debug_lines.push(format!(
                    "sym_unify class: representative={} class_size={} core_size={} unified={} border_unified={} refined_subclusters={} refined_members={} retained_border={} retained_outliers={} total_usage={} page_span={} representative_score={} estimated_gain={} subclusters={}",
                    class.representative_index,
                    class.class_size,
                    class.dense_core_size,
                    class.core_members.len(),
                    class.border_members.len(),
                    class.refinement_subclusters.len(),
                    class.refinement_subclusters
                        .iter()
                        .map(|subcluster| subcluster.refined_members.len())
                        .sum::<usize>(),
                    class.retained_border_members,
                    class.retained_outlier_members,
                    class.total_usage,
                    class.page_span,
                    class.representative_score,
                    class.estimated_gain,
                    class.candidate_subclusters
                ));
            }
        }

        for class in &classes {
            retained_border_members += class.retained_border_members;
            retained_outlier_members += class.retained_outlier_members;
            for member in &class.core_members {
                remap[member.member_index] = class.representative_index;
                unified_members += 1;
            }
            for member in &class.border_members {
                remap[member.member_index] = class.representative_index;
                border_unified_members += 1;
            }
            refinement_subclusters += class.refinement_subclusters.len();
            for subcluster in &class.refinement_subclusters {
                for member in &subcluster.refined_members {
                    refinement_remap[member.member_index] = Some(RefinementPlan {
                        prototype_input_index: subcluster.prototype_index,
                        refinement_dx: member.dx,
                        refinement_dy: member.dy,
                    });
                    refined_members += 1;
                }
            }
        }

        for page in &mut self.pages {
            for instance in &mut page.symbol_instances {
                let original_index = instance.symbol_index;
                if let Some(refinement) = refinement_remap[original_index] {
                    instance.symbol_index = refinement.prototype_input_index;
                    instance.needs_refinement = true;
                    instance.refinement_dx = refinement.refinement_dx;
                    instance.refinement_dy = refinement.refinement_dy;
                } else {
                    instance.symbol_index = remap[original_index];
                    instance.needs_refinement = false;
                    instance.refinement_dx = 0;
                    instance.refinement_dy = 0;
                }
            }
        }

        self.compact_symbol_table_after_remap();
        if diagnostics_enabled {
            let after_estimated_dict_bytes =
                symbol_dictionary_entries_bytes(self.global_symbols.iter());
            self.state.decision_debug_lines.push(format!(
                "sym_unify export summary: before={} after={} removed={} dict_bytes_before={} dict_bytes_after={} dict_bytes_saved={} unified_members={} border_unified_members={} refined_members={} refinement_subclusters={} retained_border_members={} retained_outlier_members={}",
                before_exported,
                self.global_symbols.len(),
                before_exported.saturating_sub(self.global_symbols.len()),
                before_estimated_dict_bytes,
                after_estimated_dict_bytes,
                before_estimated_dict_bytes.saturating_sub(after_estimated_dict_bytes),
                unified_members,
                border_unified_members,
                refined_members,
                refinement_subclusters,
                retained_border_members,
                retained_outlier_members
            ));
        }
        self.state.lossy_symbol_mode_applied = true;
        Ok(())
    }

    pub fn add_page(&mut self, image: &Array2<u8>) -> Result<()> {
        let bitimage = crate::jbig2sym::array_to_bitimage(image);
        self.add_page_bitimage(bitimage)
    }

    pub fn add_page_bitimage(&mut self, bitimage: BitImage) -> Result<()> {
        let page_num = self.pages.len();
        self.page_symbol_indices.push(Vec::new());
        let mut symbol_instances = Vec::new();
        let mut comparator = Comparator::default();
        let debug_matching =
            page_num == 0 && std::env::var("JBIG2_DEBUG").map_or(false, |v| v == "1");
        let no_reuse = std::env::var("JBIG2_NO_REUSE").map_or(false, |v| v == "1");

        let mut debug_lines: Vec<String> = Vec::new();
        if debug_matching {
            debug_lines.push("=== PAGE 0 MATCHING LOG ===".to_string());
            debug_lines.push(format!("Image: {}x{}", bitimage.width, bitimage.height));
        }
        let mut cc_index = 0usize;
        let mut sym_unify_anchor_map = (self.config.lossy_symbol_mode
            == LossySymbolMode::SymbolUnify
            && !self.global_symbols.is_empty())
        .then(|| self.build_sym_unify_anchor_map(page_num));
        let sym_unify_initial_anchor_count = sym_unify_anchor_map
            .as_ref()
            .map(|anchors| anchors.values().map(Vec::len).sum::<usize>())
            .unwrap_or(0);
        let sym_unify_initial_anchor_bytes = sym_unify_anchor_map
            .as_ref()
            .map(|anchors| anchor_map_dictionary_bytes(&self.global_symbols, anchors))
            .unwrap_or(0);
        let mut sym_unify_recent_hits = 0usize;
        let mut sym_unify_anchor_hits = 0usize;
        let mut sym_unify_bucket_hits = 0usize;
        let mut sym_unify_new_symbols = 0usize;
        let mut sym_unify_anchor_score_rejects = 0usize;
        let mut sym_unify_anchor_outside_rejects = 0usize;
        let mut sym_unify_anchor_compare_rejects = 0usize;
        let mut sym_unify_anchor_overlap_rejects = 0usize;

        // Extract symbols if symbol mode is enabled
        if self.config.symbol_mode && self.state.segment {
            #[cfg(feature = "symboldict")]
            {
                let dpi = 300; // Default DPI
                let losslevel =
                    if self.config.symbol_mode || self.config.uses_lossy_symbol_dictionary() {
                        0
                    } else if self.config.is_lossless {
                        0
                    } else {
                        1
                    };
                let cc_start = Instant::now();
                let cc_image = analyze_page(&bitimage, dpi, losslevel);
                let extracted = cc_image.extract_shape_refs();
                self.metrics.symbol_mode.cc_extraction += cc_start.elapsed();

                // Check if symbol extraction makes sense for this image
                // If we only get one symbol that covers the entire image,
                // it's better to use generic region encoding
                let should_use_symbols = if extracted.len() == 1 {
                    let bbox = extracted[0].bbox;
                    !(bbox.xmin == 0
                        && bbox.ymin == 0
                        && bbox.width() as usize >= bitimage.width.saturating_sub(2)
                        && bbox.height() as usize >= bitimage.height.saturating_sub(2))
                } else {
                    !extracted.is_empty()
                };

                if should_use_symbols {
                    let matching_start = Instant::now();
                    let mut recent_cache = RecentSymbolCache::new(RECENT_SYMBOL_CACHE_CAP);
                    let mut recent_candidates = [0usize; RECENT_SYMBOL_CACHE_CAP];
                    let mut last_y = 0u32;

                    for shape in extracted {
                        if Self::should_skip_symbol_candidate(
                            shape.bbox.width().max(0) as usize,
                            shape.bbox.height().max(0) as usize,
                            shape.black_pixels,
                        ) || shape.run_count == 0
                        {
                            continue;
                        }
                        let Some(symbol) = cc_image.get_bitmap_for_cc(shape.ccid) else {
                            continue;
                        };
                        let (trim_offset, trimmed) = symbol.trim();
                        let pixel_count = trimmed.count_ones();
                        if Self::should_skip_symbol_candidate(
                            trimmed.width,
                            trimmed.height,
                            pixel_count,
                        ) {
                            continue;
                        }

                        // The CC bbox is the bounding box from CC analysis.
                        // trim() may remove whitespace rows/cols from the symbol.
                        // Adjust position by trim offset so the dictionary bitmap
                        // renders at the correct location on the page.
                        let rect = Rect {
                            x: shape.bbox.xmin as u32 + trim_offset.x,
                            y: shape.bbox.ymin as u32 + trim_offset.y,
                            width: trimmed.width as u32,
                            height: trimmed.height as u32,
                        };
                        if rect.y > last_y.saturating_add(24) {
                            recent_cache.clear();
                        }
                        last_y = rect.y;

                        let trimmed_sig = Self::compute_symbol_signature(&trimmed);
                        let mut matched = false;
                        let mut instance_bitmap = Some(symbol);

                        // Error tolerance for matching.
                        let area = (trimmed.width * trimmed.height) as u32;
                        let max_err = if self.config.text_refine {
                            (area / self.config.match_tolerance).max(3)
                        } else {
                            ((area as f32 * 0.05) as u32).max(2)
                        };

                        if !matched && !no_reuse {
                            let recent_len = recent_cache.copy_into(&mut recent_candidates);
                            'recent_search: for &idx in &recent_candidates[..recent_len] {
                                if let Some((err, dx, dy, needs_refinement)) = self
                                    .evaluate_symbol_match(
                                        &trimmed,
                                        trimmed_sig,
                                        pixel_count,
                                        idx,
                                        &mut comparator,
                                        max_err,
                                    )
                                {
                                    if debug_matching {
                                        let mode = if needs_refinement {
                                            "REFINE"
                                        } else if err == 0 && dx == 0 && dy == 0 {
                                            "EXACT "
                                        } else {
                                            "LOSSY "
                                        };
                                        let proto = &self.global_symbols[idx];
                                        debug_lines.push(format!(
                                            "CC#{:04} {} pos=({},{}) {}x{} → proto#{} {}x{} err={} dx={} dy={} [recent]",
                                            cc_index,
                                            mode,
                                            rect.x,
                                            rect.y,
                                            rect.width,
                                            rect.height,
                                            idx,
                                            proto.width,
                                            proto.height,
                                            err,
                                            dx,
                                            dy
                                        ));
                                    }

                                    self.symbol_usage[idx] += 1;
                                    self.note_symbol_page(idx, page_num);
                                    symbol_instances.push(SymbolInstance {
                                        symbol_index: idx,
                                        position: rect,
                                        instance_bitmap: instance_bitmap.take().unwrap(),
                                        needs_refinement,
                                        refinement_dx: if needs_refinement { dx } else { 0 },
                                        refinement_dy: if needs_refinement { dy } else { 0 },
                                    });
                                    recent_cache.touch(idx);
                                    if self.config.lossy_symbol_mode == LossySymbolMode::SymbolUnify
                                    {
                                        sym_unify_recent_hits += 1;
                                    }
                                    matched = true;
                                    break 'recent_search;
                                }
                            }
                        }

                        if !matched
                            && !no_reuse
                            && self.config.lossy_symbol_mode == LossySymbolMode::SymbolUnify
                        {
                            if let Some(anchor_map) = sym_unify_anchor_map.as_mut() {
                                let anchor_key =
                                    family_bucket_key_for_symbol(&trimmed, &trimmed_sig);
                                let mut visited = FxHashSet::default();
                                let mut exact_examined = 0usize;
                                if let Some(bucket) = anchor_map.get(&anchor_key) {
                                    'anchor_search_exact: for &idx in bucket {
                                        if exact_examined >= SYM_UNIFY_EXACT_ANCHOR_BUDGET {
                                            break 'anchor_search_exact;
                                        }
                                        exact_examined += 1;
                                        if !visited.insert(idx) {
                                            continue;
                                        }
                                        let decision = self.evaluate_symbol_unify_anchor_match(
                                            &trimmed,
                                            trimmed_sig,
                                            pixel_count,
                                            idx,
                                            &mut comparator,
                                        );
                                        let (score, dx, dy) = match decision {
                                            SymUnifyAnchorDecision::Accept { score, dx, dy } => {
                                                (score, dx, dy)
                                            }
                                            SymUnifyAnchorDecision::RejectScore { .. } => {
                                                sym_unify_anchor_score_rejects += 1;
                                                continue;
                                            }
                                            SymUnifyAnchorDecision::RejectOutsideInk => {
                                                sym_unify_anchor_outside_rejects += 1;
                                                continue;
                                            }
                                            SymUnifyAnchorDecision::RejectCompare => {
                                                sym_unify_anchor_compare_rejects += 1;
                                                continue;
                                            }
                                            SymUnifyAnchorDecision::RejectOverlap => {
                                                sym_unify_anchor_overlap_rejects += 1;
                                                continue;
                                            }
                                            _ => continue,
                                        };

                                        if debug_matching {
                                            let proto = &self.global_symbols[idx];
                                            debug_lines.push(format!(
                                                "CC#{:04} UNIFY  pos=({},{}) {}x{} → proto#{} {}x{} score={} dx={} dy={} [anchor]",
                                                cc_index,
                                                rect.x,
                                                rect.y,
                                                rect.width,
                                                rect.height,
                                                idx,
                                                proto.width,
                                                proto.height,
                                                score,
                                                dx,
                                                dy
                                            ));
                                        }

                                        self.symbol_usage[idx] += 1;
                                        self.note_symbol_page(idx, page_num);
                                        self.maybe_add_sym_unify_anchor(anchor_map, idx, page_num);
                                        symbol_instances.push(SymbolInstance {
                                            symbol_index: idx,
                                            position: rect,
                                            instance_bitmap: instance_bitmap.take().unwrap(),
                                            needs_refinement: false,
                                            refinement_dx: 0,
                                            refinement_dy: 0,
                                        });
                                        recent_cache.touch(idx);
                                        sym_unify_anchor_hits += 1;
                                        matched = true;
                                        break;
                                    }
                                }

                                if !matched {
                                    let mut neighbor_examined = 0usize;
                                    'anchor_search_neighbors: for neighbor in
                                        family_bucket_neighbors(anchor_key)
                                    {
                                        if neighbor == anchor_key {
                                            continue;
                                        }
                                        let Some(bucket) = anchor_map.get(&neighbor) else {
                                            continue;
                                        };
                                        for &idx in bucket {
                                            if neighbor_examined >= SYM_UNIFY_NEIGHBOR_ANCHOR_BUDGET
                                            {
                                                break 'anchor_search_neighbors;
                                            }
                                            neighbor_examined += 1;
                                            if !visited.insert(idx) {
                                                continue;
                                            }
                                            let decision = self.evaluate_symbol_unify_anchor_match(
                                                &trimmed,
                                                trimmed_sig,
                                                pixel_count,
                                                idx,
                                                &mut comparator,
                                            );
                                            let (score, dx, dy) = match decision {
                                                SymUnifyAnchorDecision::Accept {
                                                    score,
                                                    dx,
                                                    dy,
                                                } => (score, dx, dy),
                                                SymUnifyAnchorDecision::RejectScore { .. } => {
                                                    sym_unify_anchor_score_rejects += 1;
                                                    continue;
                                                }
                                                SymUnifyAnchorDecision::RejectOutsideInk => {
                                                    sym_unify_anchor_outside_rejects += 1;
                                                    continue;
                                                }
                                                SymUnifyAnchorDecision::RejectCompare => {
                                                    sym_unify_anchor_compare_rejects += 1;
                                                    continue;
                                                }
                                                SymUnifyAnchorDecision::RejectOverlap => {
                                                    sym_unify_anchor_overlap_rejects += 1;
                                                    continue;
                                                }
                                                _ => continue,
                                            };

                                            if debug_matching {
                                                let proto = &self.global_symbols[idx];
                                                debug_lines.push(format!(
                                                    "CC#{:04} UNIFY  pos=({},{}) {}x{} → proto#{} {}x{} score={} dx={} dy={} [anchor]",
                                                    cc_index,
                                                    rect.x,
                                                    rect.y,
                                                    rect.width,
                                                    rect.height,
                                                    idx,
                                                    proto.width,
                                                    proto.height,
                                                    score,
                                                    dx,
                                                    dy
                                                ));
                                            }

                                            self.symbol_usage[idx] += 1;
                                            self.note_symbol_page(idx, page_num);
                                            self.maybe_add_sym_unify_anchor(
                                                anchor_map, idx, page_num,
                                            );
                                            symbol_instances.push(SymbolInstance {
                                                symbol_index: idx,
                                                position: rect,
                                                instance_bitmap: instance_bitmap.take().unwrap(),
                                                needs_refinement: false,
                                                refinement_dx: 0,
                                                refinement_dy: 0,
                                            });
                                            recent_cache.touch(idx);
                                            sym_unify_anchor_hits += 1;
                                            matched = true;
                                            break 'anchor_search_neighbors;
                                        }
                                    }
                                }
                            }
                        }

                        if !matched && !no_reuse {
                            let h = trimmed.height as u64;
                            let w = trimmed.width as u64;
                            let dim_range: u64 = if self.config.text_refine { 2 } else { 0 };

                            'bucket_search: for dh_off in 0..=(dim_range * 2) {
                                let dh = h.wrapping_add(dh_off).wrapping_sub(dim_range);
                                if dh >= 10_000 {
                                    continue;
                                }
                                for dw_off in 0..=(dim_range * 2) {
                                    let dw = w.wrapping_add(dw_off).wrapping_sub(dim_range);
                                    if dw >= 10_000 {
                                        continue;
                                    }

                                    let nk = HashKey(dh * 10_000 + dw);
                                    if let Some(bucket) = self.hash_map.get(&nk) {
                                        let bucket_len = bucket.len();
                                        let bucket_ptr = bucket.as_ptr();
                                        for bucket_pos in 0..bucket_len {
                                            let idx = unsafe { *bucket_ptr.add(bucket_pos) };
                                            let Some((err, dx, dy, needs_refinement)) = self
                                                .evaluate_symbol_match(
                                                    &trimmed,
                                                    trimmed_sig,
                                                    pixel_count,
                                                    idx,
                                                    &mut comparator,
                                                    max_err,
                                                )
                                            else {
                                                continue;
                                            };

                                            if debug_matching {
                                                let mode = if needs_refinement {
                                                    "REFINE"
                                                } else if err == 0 && dx == 0 && dy == 0 {
                                                    "EXACT "
                                                } else {
                                                    "LOSSY "
                                                };
                                                let proto = &self.global_symbols[idx];
                                                debug_lines.push(format!(
                                                    "CC#{:04} {} pos=({},{}) {}x{} → proto#{} {}x{} err={} dx={} dy={}",
                                                    cc_index,
                                                    mode,
                                                    rect.x,
                                                    rect.y,
                                                    rect.width,
                                                    rect.height,
                                                    idx,
                                                    proto.width,
                                                    proto.height,
                                                    err,
                                                    dx,
                                                    dy
                                                ));
                                            }

                                            self.symbol_usage[idx] += 1;
                                            self.note_symbol_page(idx, page_num);
                                            if let Some(anchor_map) = sym_unify_anchor_map.as_mut()
                                            {
                                                self.maybe_add_sym_unify_anchor(
                                                    anchor_map, idx, page_num,
                                                );
                                            }
                                            symbol_instances.push(SymbolInstance {
                                                symbol_index: idx,
                                                position: rect,
                                                instance_bitmap: instance_bitmap.take().unwrap(),
                                                needs_refinement,
                                                refinement_dx: if needs_refinement {
                                                    dx
                                                } else {
                                                    0
                                                },
                                                refinement_dy: if needs_refinement {
                                                    dy
                                                } else {
                                                    0
                                                },
                                            });
                                            recent_cache.touch(idx);
                                            if self.config.lossy_symbol_mode
                                                == LossySymbolMode::SymbolUnify
                                            {
                                                sym_unify_bucket_hits += 1;
                                            }
                                            matched = true;
                                            break 'bucket_search;
                                        }
                                    }
                                }
                            }
                        }

                        if !matched {
                            let idx = self.push_symbol(trimmed, pixel_count, page_num);
                            self.metrics.symbol_stats.symbols_discovered += 1;
                            if debug_matching {
                                debug_lines.push(format!(
                                    "CC#{:04} NEW    pos=({},{}) {}x{} trim_off=({},{}) → new proto#{} {}x{}",
                                    cc_index, rect.x, rect.y, rect.width, rect.height,
                                    trim_offset.x, trim_offset.y,
                                    idx, self.global_symbols[idx].width, self.global_symbols[idx].height
                                ));
                            }
                            let key = hash_key(&self.global_symbols[idx]);
                            self.hash_map.entry(key).or_default().push(idx);
                            if let Some(anchor_map) = sym_unify_anchor_map.as_mut() {
                                self.maybe_add_sym_unify_anchor(anchor_map, idx, page_num);
                            }
                            symbol_instances.push(SymbolInstance {
                                symbol_index: idx,
                                position: rect,
                                instance_bitmap: instance_bitmap.take().unwrap(),
                                needs_refinement: false,
                                refinement_dx: 0,
                                refinement_dy: 0,
                            });
                            recent_cache.touch(idx);
                            if self.config.lossy_symbol_mode == LossySymbolMode::SymbolUnify {
                                sym_unify_new_symbols += 1;
                            }
                        }
                        cc_index += 1;
                    }
                    self.metrics.symbol_mode.matching_dedup += matching_start.elapsed();
                }
            }
        }

        // Write page 0 matching debug log
        if debug_matching && !debug_lines.is_empty() {
            debug_lines.push(format!(
                "\nTotal CCs: {}, Instances: {}",
                cc_index,
                symbol_instances.len()
            ));
            let log_path = std::path::Path::new("jbig2_debug_page0.log");
            if let Ok(mut f) = std::fs::File::create(log_path) {
                use std::io::Write;
                for line in &debug_lines {
                    let _ = writeln!(f, "{}", line);
                }
            }
        }

        if self.config.lossy_symbol_mode == LossySymbolMode::SymbolUnify
            && encoder_diagnostics_enabled()
        {
            let final_anchor_count = sym_unify_anchor_map
                .as_ref()
                .map(|anchors| anchors.values().map(Vec::len).sum::<usize>())
                .unwrap_or(0);
            let final_anchor_bytes = sym_unify_anchor_map
                .as_ref()
                .map(|anchors| anchor_map_dictionary_bytes(&self.global_symbols, anchors))
                .unwrap_or(0);
            self.state.ingest_debug_lines.push(format!(
                "sym_unify ingest page={}: cc={} recent_hits={} anchor_hits={} bucket_hits={} new_symbols={} initial_anchors={} final_anchors={} initial_anchor_bytes={} final_anchor_bytes={} anchor_score_rejects={} anchor_outside_rejects={} anchor_compare_rejects={} anchor_overlap_rejects={}",
                page_num + 1,
                cc_index,
                sym_unify_recent_hits,
                sym_unify_anchor_hits,
                sym_unify_bucket_hits,
                sym_unify_new_symbols,
                sym_unify_initial_anchor_count,
                final_anchor_count,
                sym_unify_initial_anchor_bytes,
                final_anchor_bytes,
                sym_unify_anchor_score_rejects,
                sym_unify_anchor_outside_rejects,
                sym_unify_anchor_compare_rejects,
                sym_unify_anchor_overlap_rejects,
            ));
        }

        self.pages.push(PageData {
            image: bitimage,
            symbol_instances,
        });
        Ok(())
    }

    pub fn collect_symbols(&mut self, roi: &Array2<u8>) -> Result<()> {
        let bitimage = crate::jbig2sym::array_to_bitimage(roi);
        let (_, trimmed) = bitimage.trim();
        let key = hash_key(&trimmed);
        let page_num = self.pages.len();
        if self.page_symbol_indices.len() <= page_num {
            self.page_symbol_indices.resize_with(page_num + 1, Vec::new);
        }

        if !self.hash_map.contains_key(&key) {
            let pixel_count = trimmed.count_ones();
            let idx = self.push_symbol(trimmed, pixel_count, page_num);
            self.metrics.symbol_stats.symbols_discovered += 1;
            self.hash_map.insert(key, vec![idx]);
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<Vec<u8>> {
        let include_header = self.state.full_headers_remaining;
        self.state.decision_debug_lines.clear();
        match self.config.lossy_symbol_mode {
            LossySymbolMode::SymbolUnify => self.apply_symbol_unify()?,
            LossySymbolMode::Off => {}
        }
        let plan = self.plan_document(include_header)?;
        self.validate_plan(&plan)?;
        let output = self.serialize_full_document(&plan)?;
        self.state.full_headers_remaining = false;
        self.next_segment_number = plan.next_segment_number;
        Ok(output)
    }

    pub fn flush_pdf_split(&mut self) -> Result<PdfSplitOutput> {
        self.state.pdf_mode = true;
        self.state.decision_debug_lines.clear();
        match self.config.lossy_symbol_mode {
            LossySymbolMode::SymbolUnify => self.apply_symbol_unify()?,
            LossySymbolMode::Off => {}
        }
        let plan = self.plan_document(false)?;
        self.validate_plan(&plan)?;
        let (
            global_segments,
            page_streams,
            local_dict_bytes_per_page,
            text_region_bytes_per_page,
            generic_region_bytes_per_page,
        ) = self.serialize_pdf_split(&plan)?;
        self.next_segment_number = plan.next_segment_number;
        Ok(PdfSplitOutput {
            global_segments,
            page_streams,
            local_dict_bytes_per_page,
            text_region_bytes_per_page,
            generic_region_bytes_per_page,
        })
    }

    fn plan_document(&mut self, include_header: bool) -> Result<PlannedDocument> {
        debug!("Symbol stats before encoding: {}", self.get_symbol_stats());
        let diagnostics_enabled = encoder_diagnostics_enabled();
        let planning_start = Instant::now();

        if self.config.auto_thresh {
            let clustering_start = Instant::now();
            self.cluster_symbols()?;
            self.metrics.symbol_mode.clustering += clustering_start.elapsed();
        }

        self.prune_symbols_if_needed();
        self.alias_local_symbols_to_globals()?;
        self.validate_symbol_instance_indices()?;

        // For PDF split mode, ALL symbols go in the global dictionary
        // because page streams cannot have local dictionaries
        let multi_page_candidates: Vec<usize> = if self.state.pdf_mode {
            // PDF mode: include all symbols in global dictionary
            (0..self.global_symbols.len()).collect()
        } else {
            // Standalone mode: only multi-page symbols in global dict
            self.global_symbols
                .iter()
                .enumerate()
                .filter(|(i, _)| self.symbol_page_count[*i] > 1 || self.pages.len() == 1)
                .map(|(i, _)| i)
                .collect()
        };
        let global_symbol_indices: Vec<usize> = multi_page_candidates.clone();
        let global_set: HashSet<usize> = global_symbol_indices.iter().copied().collect();
        let estimated_global_dict_bytes =
            indexed_symbol_dictionary_bytes(&self.global_symbols, &global_symbol_indices);
        let low_value_global_candidates: Vec<(usize, usize, usize, i64)> = multi_page_candidates
            .iter()
            .copied()
            .filter(|symbol_index| !global_set.contains(symbol_index))
            .map(|symbol_index| {
                (
                    symbol_index,
                    self.symbol_usage[symbol_index],
                    self.symbol_page_count[symbol_index],
                    self.estimate_global_symbol_gain(symbol_index),
                )
            })
            .take(16)
            .collect();
        let multi_page_non_global = multi_page_candidates
            .len()
            .saturating_sub(global_symbol_indices.len());
        if diagnostics_enabled {
            self.state.decision_debug_lines.push(format!(
                "planning globals: selected={} multi_page_non_global={} estimated_dict_bytes={} low_value_candidates={}",
                global_symbol_indices.len(),
                multi_page_non_global,
                estimated_global_dict_bytes,
                low_value_global_candidates.len()
            ));
            for (symbol_index, usage, page_span, gain) in low_value_global_candidates {
                self.state.decision_debug_lines.push(format!(
                    "planning global candidate: symbol={} usage={} page_span={} estimated_global_gain={}",
                    symbol_index,
                    usage,
                    page_span,
                    gain
                ));
            }
        }

        // For PDF split mode, no local symbols - all are in global dictionary
        let mut page_local_symbols: Vec<Vec<usize>> = if self.state.pdf_mode {
            vec![Vec::new(); self.pages.len()]
        } else {
            self.page_symbol_indices
                .iter()
                .map(|symbols| {
                    symbols
                        .iter()
                        .copied()
                        .filter(|i| !global_set.contains(i))
                        .collect()
                })
                .collect()
        };
        let mut page_residual_symbols = vec![Vec::new(); self.pages.len()];
        let mut page_residual_anchor_remaps: Vec<FxHashMap<usize, usize>> = (0..self.pages.len())
            .map(|_| FxHashMap::default())
            .collect();
        let sym_unify_global_anchor_map =
            if self.config.lossy_symbol_mode == LossySymbolMode::SymbolUnify {
                let mut anchors: FxHashMap<FamilyBucketKey, Vec<usize>> = FxHashMap::default();
                for &symbol_index in &global_symbol_indices {
                    if !self.sym_unify_anchor_ready(symbol_index, self.pages.len()) {
                        continue;
                    }
                    let key = family_bucket_key_for_symbol(
                        &self.global_symbols[symbol_index],
                        &self.symbol_signatures[symbol_index],
                    );
                    anchors.entry(key).or_default().push(symbol_index);
                }
                Some(anchors)
            } else {
                None
            };
        let sym_unify_global_anchor_bytes = sym_unify_global_anchor_map
            .as_ref()
            .map(|anchors| anchor_map_dictionary_bytes(&self.global_symbols, anchors))
            .unwrap_or(0);
        let mut planning_anchor_comparator = Comparator::default();
        let mut planning_local_anchor_attach_count = 0usize;
        let mut planning_anchor_attach_count = 0usize;
        let mut planning_local_score_rescue_count = 0usize;
        let mut planning_anchor_score_rescue_count = 0usize;
        let mut planning_local_anchor_attach_sample = Vec::new();
        let mut planning_anchor_attach_sample = Vec::new();
        let mut planning_local_rescue_count = 0usize;
        let mut planning_local_rescue_sample = Vec::new();
        let mut residual_symbol_traces: FxHashMap<usize, ResidualSymbolTrace> =
            FxHashMap::default();
        let mut counterfactual_local_dim_relax2 = CounterfactualProbeStats::default();
        let mut counterfactual_global_overlap_skip = CounterfactualProbeStats::default();
        let mut page_uses_generic_region = vec![false; self.pages.len()];
        for (page_num, page) in self.pages.iter().enumerate() {
            if self.config.uses_lossy_symbol_dictionary()
                || self.config.refine
                || self.config.text_refine
            {
                let mut local_use_counts = HashMap::new();
                for instance in &page.symbol_instances {
                    *local_use_counts
                        .entry(instance.symbol_index)
                        .or_insert(0usize) += 1;
                }
                let local_anchor_candidates: Vec<usize> = page_local_symbols[page_num]
                    .iter()
                    .copied()
                    .filter(|&symbol_index| {
                        local_use_counts.get(&symbol_index).copied().unwrap_or(0) > 1
                            || self.should_keep_text_local_symbol(page, symbol_index)
                    })
                    .collect();
                let mut kept_local_symbols = Vec::with_capacity(page_local_symbols[page_num].len());
                for &symbol_index in &page_local_symbols[page_num] {
                    if local_use_counts.get(&symbol_index).copied().unwrap_or(0) <= 1 {
                        if self.should_keep_text_local_symbol(page, symbol_index) {
                            kept_local_symbols.push(symbol_index);
                            planning_local_rescue_count += 1;
                            if planning_local_rescue_sample.len() < 16 {
                                planning_local_rescue_sample.push((
                                    page_num + 1,
                                    symbol_index,
                                    self.global_symbols[symbol_index].width,
                                    self.global_symbols[symbol_index].height,
                                ));
                            }
                            continue;
                        }
                        let mut local_best_anchor = None;
                        let mut local_best_reject = None;
                        let mut had_local_candidates = false;
                        for &anchor_index in &local_anchor_candidates {
                            if anchor_index == symbol_index {
                                continue;
                            }
                            had_local_candidates = true;
                            match self.residual_symbol_anchor_decision(
                                symbol_index,
                                anchor_index,
                                &mut planning_anchor_comparator,
                            ) {
                                SymUnifyAnchorDecision::Accept { score, dx, dy } => {
                                    self.maybe_update_best_sym_unify_anchor_candidate(
                                        &mut local_best_anchor,
                                        &self.global_symbols[symbol_index],
                                        anchor_index,
                                        score,
                                        dx,
                                        dy,
                                        false,
                                    );
                                }
                                SymUnifyAnchorDecision::RejectScore {
                                    score,
                                    limit,
                                    dx,
                                    dy,
                                } if score
                                    <= limit.saturating_add(
                                        self.config.sym_unify_score_rescue_slack,
                                    ) =>
                                {
                                    self.maybe_update_best_sym_unify_anchor_candidate(
                                        &mut local_best_anchor,
                                        &self.global_symbols[symbol_index],
                                        anchor_index,
                                        score,
                                        dx,
                                        dy,
                                        true,
                                    );
                                }
                                other => update_best_reject(&mut local_best_reject, other),
                            }
                        }
                        if let Some(anchor_choice) = local_best_anchor {
                            page_residual_anchor_remaps[page_num]
                                .insert(symbol_index, anchor_choice.anchor_index);
                            planning_local_anchor_attach_count += 1;
                            if anchor_choice.rescued_on_score {
                                planning_local_score_rescue_count += 1;
                            }
                            if planning_local_anchor_attach_sample.len() < 16 {
                                planning_local_anchor_attach_sample.push((
                                    page_num + 1,
                                    symbol_index,
                                    anchor_choice.anchor_index,
                                ));
                            }
                            continue;
                        }
                        if diagnostics_enabled
                            && matches!(local_best_reject, Some(SymUnifyAnchorDecision::RejectDim))
                            && local_anchor_candidates.iter().copied().any(|anchor_index| {
                                anchor_index != symbol_index
                                    && self.residual_symbol_accept_with_dim_limit(
                                        symbol_index,
                                        anchor_index,
                                        &mut planning_anchor_comparator,
                                        2,
                                    )
                            })
                        {
                            record_counterfactual_probe(
                                &mut counterfactual_local_dim_relax2,
                                page_num,
                                symbol_index,
                                &self.global_symbols[symbol_index],
                                self.symbol_pixel_counts[symbol_index],
                            );
                        }
                        let mut attached_anchor = None;
                        let mut global_best_reject = None;
                        let mut had_global_candidates = false;
                        if let Some(anchor_map) = &sym_unify_global_anchor_map {
                            let bucket = family_bucket_key_for_symbol(
                                &self.global_symbols[symbol_index],
                                &self.symbol_signatures[symbol_index],
                            );
                            let mut visited = FxHashSet::default();
                            let mut best_anchor = None;
                            for neighbor in family_bucket_neighbors(bucket) {
                                let Some(candidates) = anchor_map.get(&neighbor) else {
                                    continue;
                                };
                                for &anchor_index in candidates {
                                    if anchor_index == symbol_index || !visited.insert(anchor_index)
                                    {
                                        continue;
                                    }
                                    had_global_candidates = true;
                                    match self.residual_symbol_anchor_decision(
                                        symbol_index,
                                        anchor_index,
                                        &mut planning_anchor_comparator,
                                    ) {
                                        SymUnifyAnchorDecision::Accept { score, dx, dy } => {
                                            self.maybe_update_best_sym_unify_anchor_candidate(
                                                &mut best_anchor,
                                                &self.global_symbols[symbol_index],
                                                anchor_index,
                                                score,
                                                dx,
                                                dy,
                                                false,
                                            );
                                        }
                                        SymUnifyAnchorDecision::RejectScore {
                                            score,
                                            limit,
                                            dx,
                                            dy,
                                        } if score
                                            <= limit.saturating_add(
                                                self.config.sym_unify_score_rescue_slack,
                                            ) =>
                                        {
                                            self.maybe_update_best_sym_unify_anchor_candidate(
                                                &mut best_anchor,
                                                &self.global_symbols[symbol_index],
                                                anchor_index,
                                                score,
                                                dx,
                                                dy,
                                                true,
                                            );
                                        }
                                        other => update_best_reject(&mut global_best_reject, other),
                                    }
                                }
                            }
                            attached_anchor = best_anchor;
                        }
                        if let Some(anchor_choice) = attached_anchor {
                            page_residual_anchor_remaps[page_num]
                                .insert(symbol_index, anchor_choice.anchor_index);
                            planning_anchor_attach_count += 1;
                            if anchor_choice.rescued_on_score {
                                planning_anchor_score_rescue_count += 1;
                            }
                            if planning_anchor_attach_sample.len() < 16 {
                                planning_anchor_attach_sample.push((
                                    page_num + 1,
                                    symbol_index,
                                    anchor_choice.anchor_index,
                                ));
                            }
                        } else {
                            if diagnostics_enabled
                                && matches!(
                                    global_best_reject,
                                    Some(SymUnifyAnchorDecision::RejectOverlap)
                                )
                            {
                                let bucket = family_bucket_key_for_symbol(
                                    &self.global_symbols[symbol_index],
                                    &self.symbol_signatures[symbol_index],
                                );
                                let mut visited = FxHashSet::default();
                                let recovered_without_overlap_prescreen = sym_unify_global_anchor_map
                                    .as_ref()
                                    .is_some_and(|anchor_map| {
                                        family_bucket_neighbors(bucket).into_iter().any(|neighbor| {
                                            anchor_map.get(&neighbor).is_some_and(|candidates| {
                                                candidates.iter().copied().any(|anchor_index| {
                                                    anchor_index != symbol_index
                                                        && visited.insert(anchor_index)
                                                        && self.residual_symbol_accept_without_overlap_prescreen(
                                                            symbol_index,
                                                            anchor_index,
                                                            &mut planning_anchor_comparator,
                                                        )
                                                })
                                            })
                                        })
                                    });
                                if recovered_without_overlap_prescreen {
                                    record_counterfactual_probe(
                                        &mut counterfactual_global_overlap_skip,
                                        page_num,
                                        symbol_index,
                                        &self.global_symbols[symbol_index],
                                        self.symbol_pixel_counts[symbol_index],
                                    );
                                }
                            }
                            page_residual_symbols[page_num].push(symbol_index);
                            residual_symbol_traces.insert(
                                symbol_index,
                                ResidualSymbolTrace {
                                    page_num,
                                    local_use_count: local_use_counts
                                        .get(&symbol_index)
                                        .copied()
                                        .unwrap_or(0),
                                    had_local_candidates,
                                    had_global_candidates,
                                    local_best_reject,
                                    global_best_reject,
                                },
                            );
                        }
                    } else {
                        kept_local_symbols.push(symbol_index);
                    }
                }
                page_local_symbols[page_num] = kept_local_symbols;
            }

            let local_symbols = &page_local_symbols[page_num];
            let page_local_gain: i64 = local_symbols
                .iter()
                .map(|&symbol_index| self.estimate_local_symbol_gain(page, symbol_index))
                .sum();
            let uses_only_locals = page.symbol_instances.iter().all(|inst| {
                !global_set.contains(&inst.symbol_index)
                    && !page_residual_anchor_remaps[page_num].contains_key(&inst.symbol_index)
                    && !page_residual_symbols[page_num].contains(&inst.symbol_index)
            });
            if uses_only_locals
                && local_symbols.len() <= 2
                && page.symbol_instances.len() <= 2
                && page_local_gain <= 0
            {
                page_local_symbols[page_num].clear();
                page_uses_generic_region[page_num] = true;
            }

            let has_kept_symbol_instances = page.symbol_instances.iter().any(|inst| {
                global_set.contains(&inst.symbol_index)
                    || page_residual_anchor_remaps[page_num].contains_key(&inst.symbol_index)
                    || page_local_symbols[page_num].contains(&inst.symbol_index)
            });
            if !has_kept_symbol_instances {
                page_uses_generic_region[page_num] = true;
            }
        }

        let total_residual_symbols: usize = page_residual_symbols.iter().map(Vec::len).sum();
        let full_generic_pages = page_uses_generic_region.iter().filter(|&&v| v).count();
        if diagnostics_enabled {
            self.state.decision_debug_lines.push(format!(
                "planning residuals: {} page-local one-off symbols moved to generic residuals",
                total_residual_symbols
            ));
            self.state.decision_debug_lines.push(format!(
                "planning page modes: full_generic_pages={} text_pages={}",
                full_generic_pages,
                self.pages.len().saturating_sub(full_generic_pages)
            ));
            if self.config.lossy_symbol_mode == LossySymbolMode::SymbolUnify {
                self.state.decision_debug_lines.push(format!(
                    "sym_unify planning symbol rescues: local_kept={} local_anchor_remaps={} global_anchor_remaps={} local_score_rescues={} global_score_rescues={} anchor_ready_bytes={}",
                    planning_local_rescue_count,
                    planning_local_anchor_attach_count,
                    planning_anchor_attach_count,
                    planning_local_score_rescue_count,
                    planning_anchor_score_rescue_count,
                    sym_unify_global_anchor_bytes,
                ));
                if !planning_local_rescue_sample.is_empty() {
                    self.state.decision_debug_lines.push(format!(
                        "sym_unify planning local rescue sample: {:?}",
                        planning_local_rescue_sample
                    ));
                }
                if !planning_local_anchor_attach_sample.is_empty() {
                    self.state.decision_debug_lines.push(format!(
                        "sym_unify planning local-anchor sample: {:?}",
                        planning_local_anchor_attach_sample
                    ));
                }
                if !planning_anchor_attach_sample.is_empty() {
                    self.state.decision_debug_lines.push(format!(
                        "sym_unify planning anchor sample: {:?}",
                        planning_anchor_attach_sample
                    ));
                }
                self.state.decision_debug_lines.push(format!(
                    "sym_unify residual counterfactuals: local_dim_relax2_symbols={} local_dim_relax2_bitmap_proxy_bytes={} local_dim_relax2_pages={} global_overlap_skip_symbols={} global_overlap_skip_bitmap_proxy_bytes={} global_overlap_skip_pages={}",
                    counterfactual_local_dim_relax2.symbol_count,
                    counterfactual_local_dim_relax2.bitmap_proxy_bytes,
                    counterfactual_local_dim_relax2.pages.len(),
                    counterfactual_global_overlap_skip.symbol_count,
                    counterfactual_global_overlap_skip.bitmap_proxy_bytes,
                    counterfactual_global_overlap_skip.pages.len(),
                ));
                if !counterfactual_local_dim_relax2.samples.is_empty() {
                    self.state.decision_debug_lines.push(format!(
                        "sym_unify counterfactual local_dim_relax2 sample: {:?}",
                        counterfactual_local_dim_relax2.samples
                    ));
                }
                if !counterfactual_global_overlap_skip.samples.is_empty() {
                    self.state.decision_debug_lines.push(format!(
                        "sym_unify counterfactual global_overlap_skip sample: {:?}",
                        counterfactual_global_overlap_skip.samples
                    ));
                }
            }
        }
        if diagnostics_enabled
            && self.config.lossy_symbol_mode == LossySymbolMode::SymbolUnify
            && total_residual_symbols > 0
        {
            let mut comparator = Comparator::default();
            let mut residual_unique = FxHashSet::default();
            for residuals in &page_residual_symbols {
                residual_unique.extend(residuals.iter().copied());
            }
            let anchor_map = self.build_sym_unify_anchor_map(self.pages.len());
            let mut any_global_map: FxHashMap<FamilyBucketKey, Vec<usize>> = FxHashMap::default();
            for &symbol_index in &global_symbol_indices {
                let key = family_bucket_key_for_symbol(
                    &self.global_symbols[symbol_index],
                    &self.symbol_signatures[symbol_index],
                );
                any_global_map.entry(key).or_default().push(symbol_index);
            }
            let mut attachable = 0usize;
            let mut attachable_with_score_rescue = 0usize;
            let mut attachable_to_any_global = 0usize;
            let mut sampled = Vec::new();
            let mut sampled_score_rescue = Vec::new();
            let mut sampled_any_global = Vec::new();
            let mut visited = FxHashSet::default();
            let mut reject_counts: FxHashMap<&'static str, usize> = FxHashMap::default();
            let mut area_buckets = [0usize; 4];
            for residual_index in residual_unique.iter().copied() {
                let symbol = &self.global_symbols[residual_index];
                let area = symbol.width.saturating_mul(symbol.height);
                let bucket_index = if area <= 16 {
                    0
                } else if area <= 32 {
                    1
                } else if area <= 64 {
                    2
                } else {
                    3
                };
                area_buckets[bucket_index] += 1;
                let bucket =
                    family_bucket_key_for_symbol(symbol, &self.symbol_signatures[residual_index]);
                visited.clear();
                let mut matched_anchor = None;
                let mut best_reject = SymUnifyAnchorDecision::RejectDim;
                'anchor_search: for neighbor in family_bucket_neighbors(bucket) {
                    let Some(candidates) = anchor_map.get(&neighbor) else {
                        continue;
                    };
                    for &anchor_index in candidates {
                        if anchor_index == residual_index || !visited.insert(anchor_index) {
                            continue;
                        }
                        let decision = self.residual_symbol_anchor_decision(
                            residual_index,
                            anchor_index,
                            &mut comparator,
                        );
                        match decision {
                            SymUnifyAnchorDecision::Accept { .. } => {
                                matched_anchor = Some(anchor_index);
                                break 'anchor_search;
                            }
                            _ => {
                                if decision.diagnostic_rank() > best_reject.diagnostic_rank() {
                                    best_reject = decision;
                                }
                            }
                        }
                    }
                }
                if let Some(anchor_index) = matched_anchor {
                    attachable += 1;
                    attachable_with_score_rescue += 1;
                    if sampled.len() < 16 {
                        sampled.push((residual_index, anchor_index));
                    }
                } else {
                    *reject_counts.entry(best_reject.label()).or_insert(0) += 1;

                    visited.clear();
                    let mut rescued_anchor = None;
                    'score_rescue_search: for neighbor in family_bucket_neighbors(bucket) {
                        let Some(candidates) = anchor_map.get(&neighbor) else {
                            continue;
                        };
                        for &anchor_index in candidates {
                            if anchor_index == residual_index || !visited.insert(anchor_index) {
                                continue;
                            }
                            match self.residual_symbol_anchor_decision(
                                residual_index,
                                anchor_index,
                                &mut comparator,
                            ) {
                                SymUnifyAnchorDecision::Accept { .. } => {
                                    rescued_anchor = Some(anchor_index);
                                    break 'score_rescue_search;
                                }
                                SymUnifyAnchorDecision::RejectScore { score, limit, .. }
                                    if score
                                        <= limit.saturating_add(
                                            self.config.sym_unify_score_rescue_slack,
                                        ) =>
                                {
                                    rescued_anchor = Some(anchor_index);
                                    break 'score_rescue_search;
                                }
                                _ => {}
                            }
                        }
                    }
                    if let Some(anchor_index) = rescued_anchor {
                        attachable_with_score_rescue += 1;
                        if sampled_score_rescue.len() < 16 {
                            sampled_score_rescue.push((residual_index, anchor_index));
                        }
                    }
                }

                visited.clear();
                'any_global_search: for neighbor in family_bucket_neighbors(bucket) {
                    let Some(candidates) = any_global_map.get(&neighbor) else {
                        continue;
                    };
                    for &anchor_index in candidates {
                        if anchor_index == residual_index || !visited.insert(anchor_index) {
                            continue;
                        }
                        if matches!(
                            self.residual_symbol_anchor_decision(
                                residual_index,
                                anchor_index,
                                &mut comparator,
                            ),
                            SymUnifyAnchorDecision::Accept { .. }
                        ) {
                            attachable_to_any_global += 1;
                            if sampled_any_global.len() < 16 {
                                sampled_any_global.push((residual_index, anchor_index));
                            }
                            break 'any_global_search;
                        }
                    }
                }
            }
            self.state.decision_debug_lines.push(format!(
                "sym_unify residual anchor scan: residual_unique={} attachable_to_current_anchors={} attachable_with_score_rescue={} score_rescue_extra={} unattached={}",
                residual_unique.len(),
                attachable,
                attachable_with_score_rescue,
                attachable_with_score_rescue.saturating_sub(attachable),
                residual_unique.len().saturating_sub(attachable)
            ));
            self.state.decision_debug_lines.push(format!(
                "sym_unify residual reject breakdown: dim={} pixel_delta={} signature={} overlap={} compare={} outside_ink={} score={} area_le16={} area_le32={} area_le64={} area_gt64={}",
                reject_counts.get("dim").copied().unwrap_or(0),
                reject_counts.get("pixel_delta").copied().unwrap_or(0),
                reject_counts.get("signature").copied().unwrap_or(0),
                reject_counts.get("overlap").copied().unwrap_or(0),
                reject_counts.get("compare").copied().unwrap_or(0),
                reject_counts.get("outside_ink").copied().unwrap_or(0),
                reject_counts.get("score").copied().unwrap_or(0),
                area_buckets[0],
                area_buckets[1],
                area_buckets[2],
                area_buckets[3],
            ));
            self.state.decision_debug_lines.push(format!(
                "sym_unify residual any-global scan: residual_unique={} attachable_to_any_global={} extra_beyond_anchor_ready={}",
                residual_unique.len(),
                attachable_to_any_global,
                attachable_to_any_global.saturating_sub(attachable),
            ));
            if !sampled.is_empty() {
                self.state
                    .decision_debug_lines
                    .push(format!("sym_unify residual anchor sample: {:?}", sampled));
            }
            if !sampled_score_rescue.is_empty() {
                self.state.decision_debug_lines.push(format!(
                    "sym_unify residual score-rescue sample: {:?}",
                    sampled_score_rescue
                ));
            }
            if !sampled_any_global.is_empty() {
                self.state.decision_debug_lines.push(format!(
                    "sym_unify residual any-global sample: {:?}",
                    sampled_any_global
                ));
            }

            let mut reason_stats: FxHashMap<ResidualReasonCode, ResidualReasonStats> =
                FxHashMap::default();
            for (&symbol_index, trace) in &residual_symbol_traces {
                let reason = trace.reason_code();
                let stats = reason_stats.entry(reason).or_default();
                let symbol = &self.global_symbols[symbol_index];
                let instance_count = trace.local_use_count.max(1);
                stats.symbol_count += 1;
                stats.instance_count += instance_count;
                stats.black_pixels += self.symbol_pixel_counts[symbol_index] * instance_count;
                stats.bitmap_proxy_bytes += bitmap_proxy_bytes(symbol) * instance_count;
                stats.pages.insert(trace.page_num);
                match classify_residual_shape(symbol) {
                    ResidualShapeKind::Tiny => stats.tiny_count += 1,
                    ResidualShapeKind::PunctuationLike => stats.punctuation_like_count += 1,
                    ResidualShapeKind::GlyphLike => stats.glyph_like_count += 1,
                }
                if stats.samples.len() < 8 {
                    stats.samples.push((
                        trace.page_num + 1,
                        symbol_index,
                        symbol.width,
                        symbol.height,
                        trace.local_use_count,
                    ));
                }
            }

            let mut sorted_reason_stats: Vec<_> = reason_stats.into_iter().collect();
            sorted_reason_stats.sort_by(|lhs, rhs| {
                rhs.1
                    .bitmap_proxy_bytes
                    .cmp(&lhs.1.bitmap_proxy_bytes)
                    .then_with(|| rhs.1.symbol_count.cmp(&lhs.1.symbol_count))
                    .then_with(|| lhs.0.label().cmp(rhs.0.label()))
            });
            let total_reason_proxy_bytes: usize = sorted_reason_stats
                .iter()
                .map(|(_, stats)| stats.bitmap_proxy_bytes)
                .sum();
            self.state.decision_debug_lines.push(format!(
                "sym_unify residual reason summary: reasons={} residual_symbols={} bitmap_proxy_bytes={}",
                sorted_reason_stats.len(),
                residual_symbol_traces.len(),
                total_reason_proxy_bytes,
            ));
            for (reason, stats) in sorted_reason_stats {
                self.state.decision_debug_lines.push(format!(
                    "  residual reason {}: symbols={} instances={} pages={} black_pixels={} bitmap_proxy_bytes={} tiny={} punct_like={} glyph_like={} sample={:?}",
                    reason.label(),
                    stats.symbol_count,
                    stats.instance_count,
                    stats.pages.len(),
                    stats.black_pixels,
                    stats.bitmap_proxy_bytes,
                    stats.tiny_count,
                    stats.punctuation_like_count,
                    stats.glyph_like_count,
                    stats.samples
                ));
            }

            let mut symbol_home_page = vec![usize::MAX; self.global_symbols.len()];
            for (page_idx, symbols) in self.page_symbol_indices.iter().enumerate() {
                for &symbol_index in symbols {
                    if symbol_home_page[symbol_index] == usize::MAX {
                        symbol_home_page[symbol_index] = page_idx;
                    }
                }
            }
            let mut all_symbol_bucket_map: FxHashMap<FamilyBucketKey, Vec<usize>> =
                FxHashMap::default();
            for (symbol_index, symbol) in self.global_symbols.iter().enumerate() {
                let key =
                    family_bucket_key_for_symbol(symbol, &self.symbol_signatures[symbol_index]);
                all_symbol_bucket_map
                    .entry(key)
                    .or_default()
                    .push(symbol_index);
            }

            let mut local_dim_cross_page_current = CounterfactualProbeStats::default();
            let mut local_dim_cross_page_dim2 = CounterfactualProbeStats::default();
            let mut cross_page_comparator = Comparator::default();
            let mut overlap_bypass_outcomes: FxHashMap<&'static str, CounterfactualProbeStats> =
                FxHashMap::default();
            let mut overlap_bypass_comparator = Comparator::default();
            let mut overlap_compare_probe_outcomes: FxHashMap<
                &'static str,
                CounterfactualProbeStats,
            > = FxHashMap::default();
            let mut overlap_compare_probe_comparator = Comparator::default();
            let mut overlap_bypass_compare_total_err_details = DetailedCompareProbeStats::default();
            let mut global_compare_total_err_details = DetailedCompareProbeStats::default();
            let mut compare_slack2_from_global_compare = CounterfactualProbeStats::default();
            let mut compare_slack4_from_global_compare = CounterfactualProbeStats::default();
            let mut compare_slack2_from_overlap_compare = CounterfactualProbeStats::default();
            let mut compare_slack4_from_overlap_compare = CounterfactualProbeStats::default();
            for (&symbol_index, trace) in &residual_symbol_traces {
                if trace.reason_code() != ResidualReasonCode::UseCountOneLocalRejectDim {
                    if trace.reason_code() == ResidualReasonCode::UseCountOneGlobalRejectOverlap {
                        let bucket = family_bucket_key_for_symbol(
                            &self.global_symbols[symbol_index],
                            &self.symbol_signatures[symbol_index],
                        );
                        let mut visited = FxHashSet::default();
                        let mut best_bypass_reject = None;
                        let mut recovered = false;
                        let mut best_compare_total_err: Option<(
                            crate::jbig2comparator::CompareResult,
                            u32,
                            bool,
                            bool,
                        )> = None;
                        if let Some(anchor_map) = &sym_unify_global_anchor_map {
                            'overlap_bypass_search: for neighbor in family_bucket_neighbors(bucket)
                            {
                                let Some(candidates) = anchor_map.get(&neighbor) else {
                                    continue;
                                };
                                for &anchor_index in candidates {
                                    if anchor_index == symbol_index || !visited.insert(anchor_index)
                                    {
                                        continue;
                                    }
                                    let strong_anchor = self.symbol_usage[anchor_index]
                                        >= SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE
                                        || self.symbol_page_count[anchor_index]
                                            >= SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN;
                                    match self
                                        .residual_symbol_anchor_decision_without_overlap_prescreen(
                                            symbol_index,
                                            anchor_index,
                                            &mut overlap_bypass_comparator,
                                        ) {
                                        SymUnifyAnchorDecision::Accept { .. } => {
                                            recovered = true;
                                            break 'overlap_bypass_search;
                                        }
                                        SymUnifyAnchorDecision::RejectCompare => {
                                            update_best_reject(
                                                &mut best_bypass_reject,
                                                SymUnifyAnchorDecision::RejectCompare,
                                            );
                                            let candidate = &self.global_symbols[symbol_index];
                                            let proto = &self.global_symbols[anchor_index];
                                            let compare_max_err = self
                                                .config
                                                .sym_unify_max_err
                                                .max(4)
                                                .saturating_add(u32::from(strong_anchor));
                                            if let Some(result) = overlap_compare_probe_comparator
                                                .compare_for_symbol_unify(
                                                    candidate,
                                                    proto,
                                                    relaxed_compare_probe_max_err(candidate, proto),
                                                    self.config.sym_unify_max_dx.max(0),
                                                    self.config.sym_unify_max_dy.max(0),
                                                )
                                            {
                                                let exact_dims = candidate.width == proto.width
                                                    && candidate.height == proto.height;
                                                if best_compare_total_err.is_none_or(
                                                    |(current, _, _, _)| {
                                                        result.total_err < current.total_err
                                                    },
                                                ) {
                                                    best_compare_total_err = Some((
                                                        result,
                                                        compare_max_err,
                                                        exact_dims,
                                                        strong_anchor,
                                                    ));
                                                }
                                            }
                                        }
                                        other => update_best_reject(&mut best_bypass_reject, other),
                                    }
                                }
                            }
                        }
                        let label = if recovered {
                            "accept"
                        } else {
                            best_bypass_reject
                                .map(SymUnifyAnchorDecision::label)
                                .unwrap_or("no_candidates")
                        };
                        record_labeled_counterfactual_probe(
                            &mut overlap_bypass_outcomes,
                            label,
                            trace.page_num,
                            symbol_index,
                            &self.global_symbols[symbol_index],
                            self.symbol_pixel_counts[symbol_index],
                        );
                        if label == "compare"
                            && let Some((result, compare_max_err, exact_dims, strong_anchor)) =
                                best_compare_total_err
                        {
                            let outside_limit = self
                                .config
                                .sym_unify_max_border_outside_ink
                                .min(1)
                                .saturating_add(u32::from(strong_anchor));
                            let score_limit =
                                self.config.sym_unify_class_accept_limit + u32::from(strong_anchor);
                            let score = Self::symbol_unify_assignment_score(&result);
                            record_detailed_compare_probe(
                                &mut overlap_bypass_compare_total_err_details,
                                trace.page_num,
                                symbol_index,
                                &self.global_symbols[symbol_index],
                                result,
                                compare_max_err,
                                exact_dims,
                                strong_anchor,
                            );
                            if result.total_err <= compare_max_err.saturating_add(2)
                                && result.outside_ink_err <= outside_limit
                                && score <= score_limit
                            {
                                record_counterfactual_probe(
                                    &mut compare_slack2_from_overlap_compare,
                                    trace.page_num,
                                    symbol_index,
                                    &self.global_symbols[symbol_index],
                                    self.symbol_pixel_counts[symbol_index],
                                );
                            }
                            if result.total_err <= compare_max_err.saturating_add(4)
                                && result.outside_ink_err <= outside_limit
                                && score <= score_limit
                            {
                                record_counterfactual_probe(
                                    &mut compare_slack4_from_overlap_compare,
                                    trace.page_num,
                                    symbol_index,
                                    &self.global_symbols[symbol_index],
                                    self.symbol_pixel_counts[symbol_index],
                                );
                            }
                        }
                    }
                    if trace.reason_code() == ResidualReasonCode::UseCountOneGlobalRejectCompare {
                        let bucket = family_bucket_key_for_symbol(
                            &self.global_symbols[symbol_index],
                            &self.symbol_signatures[symbol_index],
                        );
                        let mut visited = FxHashSet::default();
                        let mut best_probe_label = "no_candidates";
                        let mut best_total_err = u32::MAX;
                        let mut best_total_err_detail: Option<(
                            crate::jbig2comparator::CompareResult,
                            u32,
                            bool,
                            bool,
                        )> = None;
                        if let Some(anchor_map) = &sym_unify_global_anchor_map {
                            for neighbor in family_bucket_neighbors(bucket) {
                                let Some(candidates) = anchor_map.get(&neighbor) else {
                                    continue;
                                };
                                for &anchor_index in candidates {
                                    if anchor_index == symbol_index || !visited.insert(anchor_index)
                                    {
                                        continue;
                                    }

                                    let candidate = &self.global_symbols[symbol_index];
                                    let proto = &self.global_symbols[anchor_index];
                                    let strong_anchor = self.symbol_usage[anchor_index]
                                        >= SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE
                                        || self.symbol_page_count[anchor_index]
                                            >= SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN;
                                    let compare_max_err = self
                                        .config
                                        .sym_unify_max_err
                                        .max(4)
                                        .saturating_add(u32::from(strong_anchor));
                                    let outside_limit = self
                                        .config
                                        .sym_unify_max_border_outside_ink
                                        .min(1)
                                        .saturating_add(u32::from(strong_anchor));
                                    let relaxed = overlap_compare_probe_comparator
                                        .compare_for_symbol_unify(
                                            candidate,
                                            proto,
                                            relaxed_compare_probe_max_err(candidate, proto),
                                            self.config.sym_unify_max_dx.max(0),
                                            self.config.sym_unify_max_dy.max(0),
                                        );
                                    let (label, total_err) = if let Some(result) = relaxed {
                                        let score = Self::symbol_unify_assignment_score(&result);
                                        let score_limit = self.config.sym_unify_class_accept_limit
                                            + u32::from(strong_anchor);
                                        let label = if result.total_err <= compare_max_err {
                                            if result.outside_ink_err > outside_limit {
                                                "outside_ink"
                                            } else if score > score_limit {
                                                "score"
                                            } else {
                                                "accept"
                                            }
                                        } else if result.outside_ink_err > outside_limit {
                                            "total_err+outside_ink"
                                        } else {
                                            "total_err"
                                        };
                                        (label, result.total_err)
                                    } else {
                                        ("relaxed_none", u32::MAX)
                                    };

                                    if total_err < best_total_err {
                                        best_total_err = total_err;
                                        best_probe_label = label;
                                        if let Some(result) = relaxed {
                                            let exact_dims = candidate.width == proto.width
                                                && candidate.height == proto.height;
                                            best_total_err_detail = Some((
                                                result,
                                                compare_max_err,
                                                exact_dims,
                                                strong_anchor,
                                            ));
                                        } else {
                                            best_total_err_detail = None;
                                        }
                                    } else if best_total_err == u32::MAX
                                        && best_probe_label == "no_candidates"
                                    {
                                        best_probe_label = label;
                                    }
                                }
                            }
                        }

                        record_labeled_counterfactual_probe(
                            &mut overlap_compare_probe_outcomes,
                            best_probe_label,
                            trace.page_num,
                            symbol_index,
                            &self.global_symbols[symbol_index],
                            self.symbol_pixel_counts[symbol_index],
                        );
                        if best_probe_label == "total_err"
                            && let Some((result, compare_max_err, exact_dims, strong_anchor)) =
                                best_total_err_detail
                        {
                            let outside_limit = self
                                .config
                                .sym_unify_max_border_outside_ink
                                .min(1)
                                .saturating_add(u32::from(strong_anchor));
                            let score_limit =
                                self.config.sym_unify_class_accept_limit + u32::from(strong_anchor);
                            let score = Self::symbol_unify_assignment_score(&result);
                            record_detailed_compare_probe(
                                &mut global_compare_total_err_details,
                                trace.page_num,
                                symbol_index,
                                &self.global_symbols[symbol_index],
                                result,
                                compare_max_err,
                                exact_dims,
                                strong_anchor,
                            );
                            if result.total_err <= compare_max_err.saturating_add(2)
                                && result.outside_ink_err <= outside_limit
                                && score <= score_limit
                            {
                                record_counterfactual_probe(
                                    &mut compare_slack2_from_global_compare,
                                    trace.page_num,
                                    symbol_index,
                                    &self.global_symbols[symbol_index],
                                    self.symbol_pixel_counts[symbol_index],
                                );
                            }
                            if result.total_err <= compare_max_err.saturating_add(4)
                                && result.outside_ink_err <= outside_limit
                                && score <= score_limit
                            {
                                record_counterfactual_probe(
                                    &mut compare_slack4_from_global_compare,
                                    trace.page_num,
                                    symbol_index,
                                    &self.global_symbols[symbol_index],
                                    self.symbol_pixel_counts[symbol_index],
                                );
                            }
                        }
                    }
                    continue;
                }

                let bucket = family_bucket_key_for_symbol(
                    &self.global_symbols[symbol_index],
                    &self.symbol_signatures[symbol_index],
                );
                let mut visited = FxHashSet::default();
                let mut found_current = false;
                let mut found_dim2 = false;
                'cross_page_search: for neighbor in family_bucket_neighbors(bucket) {
                    let Some(candidates) = all_symbol_bucket_map.get(&neighbor) else {
                        continue;
                    };
                    for &candidate_index in candidates {
                        if candidate_index == symbol_index
                            || !visited.insert(candidate_index)
                            || symbol_home_page[candidate_index] == trace.page_num
                        {
                            continue;
                        }
                        if self.residual_symbol_matches_anchor(
                            symbol_index,
                            candidate_index,
                            &mut cross_page_comparator,
                        ) {
                            found_current = true;
                            break 'cross_page_search;
                        }
                        if self.residual_symbol_accept_with_dim_limit(
                            symbol_index,
                            candidate_index,
                            &mut cross_page_comparator,
                            2,
                        ) {
                            found_dim2 = true;
                        }
                    }
                }

                if found_current {
                    record_counterfactual_probe(
                        &mut local_dim_cross_page_current,
                        trace.page_num,
                        symbol_index,
                        &self.global_symbols[symbol_index],
                        self.symbol_pixel_counts[symbol_index],
                    );
                } else if found_dim2 {
                    record_counterfactual_probe(
                        &mut local_dim_cross_page_dim2,
                        trace.page_num,
                        symbol_index,
                        &self.global_symbols[symbol_index],
                        self.symbol_pixel_counts[symbol_index],
                    );
                }
            }
            self.state.decision_debug_lines.push(format!(
                "sym_unify cross-page local-dim probes: current_symbols={} current_bitmap_proxy_bytes={} current_pages={} dim2_only_symbols={} dim2_only_bitmap_proxy_bytes={} dim2_only_pages={}",
                local_dim_cross_page_current.symbol_count,
                local_dim_cross_page_current.bitmap_proxy_bytes,
                local_dim_cross_page_current.pages.len(),
                local_dim_cross_page_dim2.symbol_count,
                local_dim_cross_page_dim2.bitmap_proxy_bytes,
                local_dim_cross_page_dim2.pages.len(),
            ));
            if !local_dim_cross_page_current.samples.is_empty() {
                self.state.decision_debug_lines.push(format!(
                    "sym_unify cross-page local-dim current sample: {:?}",
                    local_dim_cross_page_current.samples
                ));
            }
            if !local_dim_cross_page_dim2.samples.is_empty() {
                self.state.decision_debug_lines.push(format!(
                    "sym_unify cross-page local-dim dim2 sample: {:?}",
                    local_dim_cross_page_dim2.samples
                ));
            }

            let mut sorted_overlap_bypass_outcomes: Vec<_> =
                overlap_bypass_outcomes.into_iter().collect();
            sorted_overlap_bypass_outcomes.sort_by(|lhs, rhs| {
                rhs.1
                    .bitmap_proxy_bytes
                    .cmp(&lhs.1.bitmap_proxy_bytes)
                    .then_with(|| rhs.1.symbol_count.cmp(&lhs.1.symbol_count))
                    .then_with(|| lhs.0.cmp(rhs.0))
            });
            let overlap_bypass_total_symbols: usize = sorted_overlap_bypass_outcomes
                .iter()
                .map(|(_, stats)| stats.symbol_count)
                .sum();
            let overlap_bypass_total_bitmap_proxy_bytes: usize = sorted_overlap_bypass_outcomes
                .iter()
                .map(|(_, stats)| stats.bitmap_proxy_bytes)
                .sum();
            self.state.decision_debug_lines.push(format!(
                "sym_unify overlap-bypass outcomes: outcomes={} symbols={} bitmap_proxy_bytes={}",
                sorted_overlap_bypass_outcomes.len(),
                overlap_bypass_total_symbols,
                overlap_bypass_total_bitmap_proxy_bytes,
            ));
            for (label, stats) in sorted_overlap_bypass_outcomes {
                self.state.decision_debug_lines.push(format!(
                    "  overlap-bypass {}: symbols={} pages={} black_pixels={} bitmap_proxy_bytes={} sample={:?}",
                    label,
                    stats.symbol_count,
                    stats.pages.len(),
                    stats.black_pixels,
                    stats.bitmap_proxy_bytes,
                    stats.samples
                ));
            }

            let mut sorted_overlap_compare_probe_outcomes: Vec<_> =
                overlap_compare_probe_outcomes.into_iter().collect();
            sorted_overlap_compare_probe_outcomes.sort_by(|lhs, rhs| {
                rhs.1
                    .bitmap_proxy_bytes
                    .cmp(&lhs.1.bitmap_proxy_bytes)
                    .then_with(|| rhs.1.symbol_count.cmp(&lhs.1.symbol_count))
                    .then_with(|| lhs.0.cmp(rhs.0))
            });
            let overlap_compare_probe_total_symbols: usize = sorted_overlap_compare_probe_outcomes
                .iter()
                .map(|(_, stats)| stats.symbol_count)
                .sum();
            let overlap_compare_probe_total_bitmap_proxy_bytes: usize =
                sorted_overlap_compare_probe_outcomes
                    .iter()
                    .map(|(_, stats)| stats.bitmap_proxy_bytes)
                    .sum();
            self.state.decision_debug_lines.push(format!(
                "sym_unify global-compare relaxed probe: outcomes={} symbols={} bitmap_proxy_bytes={}",
                sorted_overlap_compare_probe_outcomes.len(),
                overlap_compare_probe_total_symbols,
                overlap_compare_probe_total_bitmap_proxy_bytes,
            ));
            for (label, stats) in sorted_overlap_compare_probe_outcomes {
                self.state.decision_debug_lines.push(format!(
                    "  global-compare relaxed {}: symbols={} pages={} black_pixels={} bitmap_proxy_bytes={} sample={:?}",
                    label,
                    stats.symbol_count,
                    stats.pages.len(),
                    stats.black_pixels,
                    stats.bitmap_proxy_bytes,
                    stats.samples
                ));
            }
            self.state.decision_debug_lines.push(format!(
                "sym_unify overlap-bypass compare total_err detail: symbols={} bitmap_proxy_bytes={} exact_dims={} strong_anchor={} shift_le1={} over_by_le2={} over_by_le4={} over_by_le8={} over_by_gt8={} sample={:?}",
                overlap_bypass_compare_total_err_details.symbol_count,
                overlap_bypass_compare_total_err_details.bitmap_proxy_bytes,
                overlap_bypass_compare_total_err_details.exact_dims_count,
                overlap_bypass_compare_total_err_details.strong_anchor_count,
                overlap_bypass_compare_total_err_details.shift_le1_count,
                overlap_bypass_compare_total_err_details.over_by_le2_count,
                overlap_bypass_compare_total_err_details.over_by_le4_count,
                overlap_bypass_compare_total_err_details.over_by_le8_count,
                overlap_bypass_compare_total_err_details.over_by_gt8_count,
                overlap_bypass_compare_total_err_details.samples
            ));
            self.state.decision_debug_lines.push(format!(
                "sym_unify global-compare total_err detail: symbols={} bitmap_proxy_bytes={} exact_dims={} strong_anchor={} shift_le1={} over_by_le2={} over_by_le4={} over_by_le8={} over_by_gt8={} sample={:?}",
                global_compare_total_err_details.symbol_count,
                global_compare_total_err_details.bitmap_proxy_bytes,
                global_compare_total_err_details.exact_dims_count,
                global_compare_total_err_details.strong_anchor_count,
                global_compare_total_err_details.shift_le1_count,
                global_compare_total_err_details.over_by_le2_count,
                global_compare_total_err_details.over_by_le4_count,
                global_compare_total_err_details.over_by_le8_count,
                global_compare_total_err_details.over_by_gt8_count,
                global_compare_total_err_details.samples
            ));
            self.state.decision_debug_lines.push(format!(
                "sym_unify compare-slack probes: global_total_err_slack2_symbols={} global_total_err_slack2_bitmap_proxy_bytes={} global_total_err_slack4_symbols={} global_total_err_slack4_bitmap_proxy_bytes={} overlap_compare_slack2_symbols={} overlap_compare_slack2_bitmap_proxy_bytes={} overlap_compare_slack4_symbols={} overlap_compare_slack4_bitmap_proxy_bytes={}",
                compare_slack2_from_global_compare.symbol_count,
                compare_slack2_from_global_compare.bitmap_proxy_bytes,
                compare_slack4_from_global_compare.symbol_count,
                compare_slack4_from_global_compare.bitmap_proxy_bytes,
                compare_slack2_from_overlap_compare.symbol_count,
                compare_slack2_from_overlap_compare.bitmap_proxy_bytes,
                compare_slack4_from_overlap_compare.symbol_count,
                compare_slack4_from_overlap_compare.bitmap_proxy_bytes
            ));
        }
        if diagnostics_enabled {
            for (page_num, residuals) in page_residual_symbols.iter().enumerate().take(32) {
                if !residuals.is_empty() {
                    self.state.decision_debug_lines.push(format!(
                        "page {} residual symbols: count={} sample={:?}",
                        page_num + 1,
                        residuals.len(),
                        &residuals[..residuals.len().min(8)]
                    ));
                }
            }
        }

        self.validate_symbol_partition(
            &global_symbol_indices,
            &page_local_symbols,
            &page_residual_symbols,
            &page_residual_anchor_remaps,
            &page_uses_generic_region,
        )?;

        let mut current_segment_number = self.next_segment_number;
        let mut global_segments = Vec::new();

        self.global_dict_segment_numbers.clear();
        let mut encoded_global_dict = EncodedSymbolDictionary::default();
        let mut global_refinement_map = vec![None; self.global_symbols.len()];
        if !global_symbol_indices.is_empty() {
            let refs: Vec<&BitImage> = global_symbol_indices
                .iter()
                .map(|&i| &self.global_symbols[i])
                .collect();
            let dict_usage: Vec<usize> = global_symbol_indices
                .iter()
                .map(|&i| self.symbol_usage[i])
                .collect();
            let dict_layout =
                plan_symbol_dictionary_layout(&refs, &self.config, Some(&dict_usage))?;
            if diagnostics_enabled {
                self.state.decision_debug_lines.push(format!(
                    "global dict layout: families={} singletons={} refined_members={} exported_members={}",
                    dict_layout.diagnostics.family_count,
                    dict_layout.diagnostics.singleton_family_count,
                    dict_layout.diagnostics.refined_member_count,
                    dict_layout.diagnostics.exported_member_count
                ));
                self.state.decision_debug_lines.extend(
                    dict_layout
                        .diagnostics
                        .sample_lines
                        .iter()
                        .take(64)
                        .cloned(),
                );
            }
            let dict_start = Instant::now();
            encoded_global_dict =
                encode_symbol_dictionary_segments(&refs, &self.config, &dict_layout)?;
            self.metrics.symbol_mode.symbol_dict_encoding += dict_start.elapsed();
            for (subset_index, refinement) in dict_layout.refinements.iter().enumerate() {
                if let Some(refinement) = refinement {
                    let gs_idx = global_symbol_indices[subset_index];
                    global_refinement_map[gs_idx] = Some(RefinementPlan {
                        prototype_input_index: global_symbol_indices
                            [refinement.prototype_input_index],
                        refinement_dx: refinement.refinement_dx,
                        refinement_dy: refinement.refinement_dy,
                    });
                }
            }
            let segment_number = current_segment_number;
            current_segment_number += 1;
            self.global_dict_segment_numbers.push(segment_number);
            global_segments.push(Segment {
                number: segment_number,
                seg_type: SegmentType::SymbolDictionary,
                deferred_non_retain: false,
                retain_flags: 0,
                page_association_type: 2,
                referred_to: Vec::new(),
                page: None,
                payload: encoded_global_dict.payload.clone(),
            });
        }

        let mut global_sym_to_dict_pos = vec![u32::MAX; self.global_symbols.len()];
        for (refs_idx, &dict_pos) in encoded_global_dict.input_to_exported_pos.iter().enumerate() {
            if dict_pos != u32::MAX {
                let gs_idx = global_symbol_indices[refs_idx];
                global_sym_to_dict_pos[gs_idx] = dict_pos;
            }
        }
        let num_global_dict_symbols = encoded_global_dict.exported_symbol_count;

        // Debug: check for out-of-range mappings
        if self.state.pdf_mode {
            for (gs_idx, &dict_pos) in global_sym_to_dict_pos.iter().enumerate() {
                if dict_pos != u32::MAX && dict_pos >= num_global_dict_symbols {
                    log::warn!(
                        "BUG: global_sym_to_dict_pos[{}] = {} but num_global_dict_symbols = {}",
                        gs_idx,
                        dict_pos,
                        num_global_dict_symbols
                    );
                }
            }
        }

        let mut planned_local_export_count = 0usize;
        self.metrics.symbol_stats.global_symbol_count = num_global_dict_symbols as usize;

        let page_segment_start = current_segment_number;
        let mut page_layouts = Vec::with_capacity(self.pages.len());
        for (page_num, page) in self.pages.iter().enumerate() {
            let page_number = if self.state.pdf_mode {
                1u32
            } else {
                page_num as u32 + 1
            };
            if self.state.pdf_mode {
                current_segment_number = page_segment_start;
            }
            let page_info_segment_number = current_segment_number;
            current_segment_number += 1;
            let local_dict_layout = if self.config.symbol_mode
                && !page.symbol_instances.is_empty()
                && !page_local_symbols[page_num].is_empty()
            {
                let refs: Vec<&BitImage> = page_local_symbols[page_num]
                    .iter()
                    .map(|&i| &self.global_symbols[i])
                    .collect();
                let mut local_usage = vec![0usize; page_local_symbols[page_num].len()];
                let local_index_by_symbol: HashMap<usize, usize> = page_local_symbols[page_num]
                    .iter()
                    .enumerate()
                    .map(|(idx, &symbol_index)| (symbol_index, idx))
                    .collect();
                for instance in &page.symbol_instances {
                    if let Some(&local_idx) = local_index_by_symbol.get(&instance.symbol_index) {
                        local_usage[local_idx] += 1;
                    }
                }
                Some(plan_symbol_dictionary_layout(
                    &refs,
                    &self.config,
                    Some(&local_usage),
                )?)
            } else {
                None
            };
            let mut local_dict_segment_numbers = Vec::new();
            if let Some(local_dict_layout) = &local_dict_layout {
                if diagnostics_enabled {
                    self.state.decision_debug_lines.push(format!(
                        "page {} local dict layout: families={} singletons={} refined_members={} exported_members={}",
                        page_num + 1,
                        local_dict_layout.diagnostics.family_count,
                        local_dict_layout.diagnostics.singleton_family_count,
                        local_dict_layout.diagnostics.refined_member_count,
                        local_dict_layout.diagnostics.exported_member_count
                    ));
                    self.state.decision_debug_lines.extend(
                        local_dict_layout
                            .diagnostics
                            .sample_lines
                            .iter()
                            .take(16)
                            .cloned(),
                    );
                }
                for _ in 0..local_dict_layout.segment_count() {
                    local_dict_segment_numbers.push(current_segment_number);
                    current_segment_number += 1;
                }
                planned_local_export_count += local_dict_layout.export_input_indices.len();
            }
            let region_segment_number = current_segment_number;
            current_segment_number += 1;
            let has_residual_region = !page_residual_symbols[page_num].is_empty()
                && !page_uses_generic_region[page_num]
                && page.symbol_instances.iter().any(|inst| {
                    global_set.contains(&inst.symbol_index)
                        || page_local_symbols[page_num].contains(&inst.symbol_index)
                });
            let residual_region_segment_number = if has_residual_region {
                let number = current_segment_number;
                current_segment_number += 1;
                Some(number)
            } else {
                None
            };
            let end_of_page_segment_number = current_segment_number;
            current_segment_number += 1;
            let use_generic_region = page_uses_generic_region[page_num];
            if diagnostics_enabled {
                self.state.decision_debug_lines.push(format!(
                    "page {} plan: full_generic={} residual_region={} local_symbols={} residual_symbols={} anchor_remaps={} instances={}",
                    page_num + 1,
                    use_generic_region,
                    has_residual_region,
                    page_local_symbols[page_num].len(),
                    page_residual_symbols[page_num].len(),
                    page_residual_anchor_remaps[page_num].len(),
                    page.symbol_instances.len()
                ));
            }

            page_layouts.push(PlannedPageLayout {
                page_index: page_num,
                page_number,
                page_info_segment_number,
                local_dict_segment_numbers,
                local_dict_layout,
                region_segment_number,
                residual_region_segment_number,
                end_of_page_segment_number,
                local_symbols: page_local_symbols[page_num].clone(),
                residual_symbols: page_residual_symbols[page_num].clone(),
                residual_anchor_remaps: page_residual_anchor_remaps[page_num].clone(),
                use_generic_region,
            });
        }

        self.metrics.symbol_stats.local_symbol_count = planned_local_export_count;
        self.metrics.symbol_stats.symbols_exported =
            self.metrics.symbol_stats.global_symbol_count + planned_local_export_count;
        self.metrics.symbol_stats.avg_symbol_reuse =
            if self.metrics.symbol_stats.symbols_exported > 0 {
                self.symbol_usage.iter().sum::<usize>() as f64
                    / self.metrics.symbol_stats.symbols_exported as f64
            } else {
                0.0
            };

        self.metrics.symbol_mode.planning += planning_start.elapsed();

        #[cfg(feature = "parallel")]
        let built_pages = if self.state.pdf_mode || self.pages.len() > 1 {
            page_layouts
                .par_iter()
                .map(|layout| {
                    self.build_planned_page(
                        layout,
                        &global_sym_to_dict_pos,
                        num_global_dict_symbols,
                        &global_refinement_map,
                    )
                })
                .collect::<Vec<_>>()
        } else {
            page_layouts
                .iter()
                .map(|layout| {
                    self.build_planned_page(
                        layout,
                        &global_sym_to_dict_pos,
                        num_global_dict_symbols,
                        &global_refinement_map,
                    )
                })
                .collect::<Vec<_>>()
        };

        #[cfg(not(feature = "parallel"))]
        let built_pages = page_layouts
            .iter()
            .map(|layout| {
                self.build_planned_page(
                    layout,
                    &global_sym_to_dict_pos,
                    num_global_dict_symbols,
                    &global_refinement_map,
                )
            })
            .collect::<Vec<_>>();

        let mut pages = Vec::with_capacity(built_pages.len());
        for built_page in built_pages {
            let built_page = built_page?;
            self.metrics.symbol_mode.symbol_dict_encoding += built_page.symbol_dict_time;
            self.metrics.symbol_mode.text_region_encoding += built_page.text_region_time;
            self.metrics.symbol_mode.generic_region_encoding += built_page.generic_region_time;
            pages.push(built_page.page);
        }

        let eof_segment = Some(Segment {
            number: current_segment_number,
            seg_type: SegmentType::EndOfFile,
            deferred_non_retain: false,
            retain_flags: 0,
            page_association_type: 2,
            referred_to: vec![],
            page: None,
            payload: vec![],
        });
        current_segment_number += 1;

        Ok(PlannedDocument {
            file_header: if include_header {
                Some(FileHeader {
                    // Sequential organisation matches the on-disk layout this
                    // encoder produces (segment header + payload, repeated).
                    organisation_type: false,
                    unknown_n_pages: false,
                    n_pages: self.pages.len() as u32,
                })
            } else {
                None
            },
            global_segments,
            pages,
            eof_segment,
            next_segment_number: current_segment_number,
        })
    }

    fn build_planned_page(
        &self,
        layout: &PlannedPageLayout,
        global_sym_to_dict_pos: &[u32],
        num_global_dict_symbols: u32,
        global_refinement_map: &[Option<RefinementPlan>],
    ) -> Result<BuiltPage> {
        let page = &self.pages[layout.page_index];
        let mut page_segments = Vec::new();
        let mut symbol_dict_time = Duration::default();
        let mut text_region_time = Duration::default();
        let mut generic_region_time = Duration::default();

        let page_info_payload = PageInfo {
            width: page.image.width as u32,
            height: page.image.height as u32,
            default_pixel: false,
            xres: self.config.generic.dpi,
            yres: self.config.generic.dpi,
            ..Default::default()
        }
        .to_bytes();

        page_segments.push(Segment {
            number: layout.page_info_segment_number,
            seg_type: SegmentType::PageInformation,
            deferred_non_retain: false,
            retain_flags: 0,
            page_association_type: 0,
            referred_to: vec![],
            page: Some(layout.page_number),
            payload: page_info_payload,
        });

        if self.config.symbol_mode
            && !page.symbol_instances.is_empty()
            && !layout.use_generic_region
        {
            // In PDF split mode, the global dictionary is in a separate PDF object.
            // The text region segment must reference it as segment 1 in its referred_to field.
            // The PDF /DecodeParms points to the actual global dict PDF object.
            let mut referred_to_for_text_region = if self.state.pdf_mode {
                vec![1u32] // Reference global dictionary as segment 1
            } else {
                self.global_dict_segment_numbers.clone()
            };
            let residual_set: HashSet<usize> = layout.residual_symbols.iter().copied().collect();
            let residual_anchor_remaps = &layout.residual_anchor_remaps;

            let mut local_sym_to_dict_pos = vec![u32::MAX; self.global_symbols.len()];
            let mut local_refinement_map = vec![None; self.global_symbols.len()];
            let num_local_dict_symbols = if let Some(local_dict_layout) = &layout.local_dict_layout
            {
                let refs: Vec<&BitImage> = layout
                    .local_symbols
                    .iter()
                    .map(|&i| &self.global_symbols[i])
                    .collect();
                let dict_start = Instant::now();
                let encoded_local_dict =
                    encode_symbol_dictionary_segments(&refs, self.config, local_dict_layout)?;
                symbol_dict_time += dict_start.elapsed();

                for (refs_idx, &dict_pos) in
                    encoded_local_dict.input_to_exported_pos.iter().enumerate()
                {
                    if dict_pos != u32::MAX {
                        let gs_idx = layout.local_symbols[refs_idx];
                        local_sym_to_dict_pos[gs_idx] = dict_pos;
                    }
                }
                for (subset_index, refinement) in local_dict_layout.refinements.iter().enumerate() {
                    if let Some(refinement) = refinement {
                        let gs_idx = layout.local_symbols[subset_index];
                        local_refinement_map[gs_idx] = Some(RefinementPlan {
                            prototype_input_index: layout.local_symbols
                                [refinement.prototype_input_index],
                            refinement_dx: refinement.refinement_dx,
                            refinement_dy: refinement.refinement_dy,
                        });
                    }
                }

                // For PDF split mode, skip local dictionary segments
                // All symbols should be in the global dictionary for PDF embedding
                if !self.state.pdf_mode {
                    for segment_number in layout.local_dict_segment_numbers.iter().copied() {
                        page_segments.push(Segment {
                            number: segment_number,
                            seg_type: SegmentType::SymbolDictionary,
                            deferred_non_retain: false,
                            retain_flags: 0,
                            page_association_type: 0,
                            referred_to: Vec::new(),
                            page: Some(layout.page_number),
                            payload: encoded_local_dict.payload.clone(),
                        });
                    }
                    referred_to_for_text_region
                        .extend(layout.local_dict_segment_numbers.iter().copied());
                }
                encoded_local_dict.exported_symbol_count
            } else {
                0
            };

            let mut planned_instances = Vec::with_capacity(page.symbol_instances.len());
            let mut residual_instances = Vec::new();
            for instance in &page.symbol_instances {
                if let Some(&anchor_index) = residual_anchor_remaps.get(&instance.symbol_index) {
                    let mut remapped = instance.clone();
                    remapped.symbol_index = anchor_index;
                    remapped.needs_refinement = false;
                    remapped.refinement_dx = 0;
                    remapped.refinement_dy = 0;
                    planned_instances.push(remapped);
                } else if residual_set.contains(&instance.symbol_index) {
                    residual_instances.push(instance.clone());
                } else {
                    planned_instances.push(instance.clone());
                }
            }
            let mut needs_family_refinement = false;
            for instance in &mut planned_instances {
                if let Some(refinement) = local_refinement_map[instance.symbol_index] {
                    instance.symbol_index = refinement.prototype_input_index;
                    instance.needs_refinement = true;
                    instance.refinement_dx = refinement.refinement_dx;
                    instance.refinement_dy = refinement.refinement_dy;
                    needs_family_refinement = true;
                } else if let Some(refinement) = global_refinement_map[instance.symbol_index] {
                    instance.symbol_index = refinement.prototype_input_index;
                    instance.needs_refinement = true;
                    instance.refinement_dx = refinement.refinement_dx;
                    instance.refinement_dy = refinement.refinement_dy;
                    needs_family_refinement = true;
                }
            }

            // NOTE: PDF-embedded text regions previously stripped all per-instance
            // refinement here ("until global dictionary encoding is fixed"). That
            // turned every lossy symbol match into a hard substitution (e.g. "i"
            // rendered as "'"). Per-instance refinement (SBREFINE=1) round-trips
            // correctly through jbig2dec in embedded/PDF mode, so the refinement is
            // now retained and the lossy match is corrected at decode time.

            if !planned_instances.is_empty() {
                let text_start = Instant::now();
                let has_instance_refinement = planned_instances
                    .iter()
                    .any(|instance| instance.needs_refinement);
                let use_refinement_text_region = has_instance_refinement
                    || (!self.config.uses_lossy_symbol_dictionary()
                        && (self.config.text_refine
                            || self.config.refine
                            || needs_family_refinement));
                let region_payload = if use_refinement_text_region {
                    encode_text_region_with_refinement(
                        &planned_instances,
                        self.config,
                        &self.global_symbols,
                        global_sym_to_dict_pos,
                        num_global_dict_symbols,
                        &local_sym_to_dict_pos,
                        num_local_dict_symbols,
                    )?
                } else {
                    encode_text_region_mapped(
                        &planned_instances,
                        self.config,
                        &self.global_symbols,
                        global_sym_to_dict_pos,
                        num_global_dict_symbols,
                        &local_sym_to_dict_pos,
                        layout.page_index,
                        num_local_dict_symbols,
                    )?
                };
                text_region_time += text_start.elapsed();

                page_segments.push(Segment {
                    number: layout.region_segment_number,
                    seg_type: SegmentType::ImmediateTextRegion,
                    deferred_non_retain: false,
                    retain_flags: 0,
                    page_association_type: 0,
                    referred_to: referred_to_for_text_region,
                    page: Some(layout.page_number),
                    payload: region_payload,
                });
            }

            if let Some((residual_bitmap, residual_x, residual_y)) =
                self.build_instance_residual_bitmap(&residual_instances)?
            {
                let generic_start = Instant::now();
                let residual_payload = self.encode_generic_region_payload_at(
                    &residual_bitmap,
                    residual_x,
                    residual_y,
                )?;
                generic_region_time += generic_start.elapsed();
                page_segments.push(Segment {
                    number: layout
                        .residual_region_segment_number
                        .unwrap_or(layout.region_segment_number),
                    seg_type: SegmentType::ImmediateGenericRegion,
                    deferred_non_retain: false,
                    retain_flags: 0,
                    page_association_type: 0,
                    referred_to: Vec::new(),
                    page: Some(layout.page_number),
                    payload: residual_payload,
                });
            }
        } else {
            let generic_start = Instant::now();
            let generic_region_payload =
                self.encode_generic_region_payload_at(&page.image, 0, 0)?;
            generic_region_time += generic_start.elapsed();

            page_segments.push(Segment {
                number: layout.region_segment_number,
                seg_type: SegmentType::ImmediateGenericRegion,
                deferred_non_retain: false,
                retain_flags: 0,
                page_association_type: 0,
                referred_to: Vec::new(),
                page: Some(layout.page_number),
                payload: generic_region_payload,
            });
        }

        // For PDF split mode, skip EndOfPage segment - not needed for PDF embedding
        if !self.state.pdf_mode {
            page_segments.push(Segment {
                number: layout.end_of_page_segment_number,
                seg_type: SegmentType::EndOfPage,
                deferred_non_retain: false,
                retain_flags: 0,
                page_association_type: 0,
                referred_to: Vec::new(),
                page: Some(layout.page_number),
                payload: Vec::new(),
            });
        }

        Ok(BuiltPage {
            page: PlannedPage {
                page_number: layout.page_number,
                segments: page_segments,
            },
            symbol_dict_time,
            text_region_time,
            generic_region_time,
        })
    }

    fn validate_plan(&self, plan: &PlannedDocument) -> Result<()> {
        let mut global_numbers = HashSet::new();

        for seg in &plan.global_segments {
            if !global_numbers.insert(seg.number) {
                anyhow::bail!("Duplicate segment number in globals: {}", seg.number);
            }
        }

        for (page_idx, page) in plan.pages.iter().enumerate() {
            // In PDF mode, each page is an independent stream, so segment
            // numbers only need to be unique within globals + that page.
            let mut page_numbers = global_numbers.clone();
            for seg in &page.segments {
                if !page_numbers.insert(seg.number) {
                    anyhow::bail!(
                        "Duplicate segment number {} on page {}",
                        seg.number,
                        page_idx
                    );
                }
            }

            for seg in &page.segments {
                for referred in &seg.referred_to {
                    if !page_numbers.contains(referred) {
                        anyhow::bail!(
                            "Page {} segment {} refers to missing segment {}",
                            page.page_number,
                            seg.number,
                            referred
                        );
                    }
                    if global_numbers.contains(referred) && plan.global_segments.is_empty() {
                        anyhow::bail!(
                            "Page {} segment {} refers to global {} but no globals stream exists",
                            page.page_number,
                            seg.number,
                            referred
                        );
                    }
                }
            }
        }

        if let Some(eof) = &plan.eof_segment {
            if global_numbers.contains(&eof.number) {
                anyhow::bail!("EOF segment number {} conflicts with globals", eof.number);
            }
        }

        for seg in &plan.global_segments {
            for referred in &seg.referred_to {
                if !global_numbers.contains(referred) {
                    anyhow::bail!(
                        "Global segment {} refers to missing segment {}",
                        seg.number,
                        referred
                    );
                }
            }
        }

        Ok(())
    }

    fn serialize_full_document(&self, plan: &PlannedDocument) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        if let Some(header) = &plan.file_header {
            output.extend(header.to_bytes());
        }
        for seg in &plan.global_segments {
            seg.write_into(&mut output)?;
        }
        for page in &plan.pages {
            for seg in &page.segments {
                seg.write_into(&mut output)?;
            }
        }
        if let Some(eof) = &plan.eof_segment {
            eof.write_into(&mut output)?;
        }
        Ok(output)
    }

    fn serialize_pdf_split(
        &self,
        plan: &PlannedDocument,
    ) -> Result<(
        Option<Vec<u8>>,
        Vec<Vec<u8>>,
        Vec<usize>,
        Vec<usize>,
        Vec<usize>,
    )> {
        let global_segments = if plan.global_segments.is_empty() {
            None
        } else {
            let mut out = Vec::new();
            for seg in &plan.global_segments {
                seg.write_into(&mut out)?;
            }
            Some(out)
        };

        #[cfg(feature = "parallel")]
        let page_streams = plan
            .pages
            .par_iter()
            .map(|page| {
                let mut page_out = Vec::new();
                let mut local_dict_bytes = 0usize;
                let mut text_region_bytes = 0usize;
                let mut generic_region_bytes = 0usize;
                for seg in &page.segments {
                    let start_len = page_out.len();
                    seg.write_into(&mut page_out)?;
                    let seg_len = page_out.len().saturating_sub(start_len);
                    match seg.seg_type {
                        SegmentType::SymbolDictionary => local_dict_bytes += seg_len,
                        SegmentType::ImmediateTextRegion => text_region_bytes += seg_len,
                        SegmentType::ImmediateGenericRegion => generic_region_bytes += seg_len,
                        _ => {}
                    }
                }
                Ok((
                    page_out,
                    local_dict_bytes,
                    text_region_bytes,
                    generic_region_bytes,
                ))
            })
            .collect::<Vec<Result<(Vec<u8>, usize, usize, usize)>>>()
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        #[cfg(not(feature = "parallel"))]
        let page_streams = {
            let mut page_streams = Vec::with_capacity(plan.pages.len());
            for page in &plan.pages {
                let mut page_out = Vec::new();
                let mut local_dict_bytes = 0usize;
                let mut text_region_bytes = 0usize;
                let mut generic_region_bytes = 0usize;
                for seg in &page.segments {
                    let start_len = page_out.len();
                    seg.write_into(&mut page_out)?;
                    let seg_len = page_out.len().saturating_sub(start_len);
                    match seg.seg_type {
                        SegmentType::SymbolDictionary => local_dict_bytes += seg_len,
                        SegmentType::ImmediateTextRegion => text_region_bytes += seg_len,
                        SegmentType::ImmediateGenericRegion => generic_region_bytes += seg_len,
                        _ => {}
                    }
                }
                page_streams.push((
                    page_out,
                    local_dict_bytes,
                    text_region_bytes,
                    generic_region_bytes,
                ));
            }
            page_streams
        };

        let mut raw_pages = Vec::with_capacity(page_streams.len());
        let mut local_dict_bytes_per_page = Vec::with_capacity(page_streams.len());
        let mut text_region_bytes_per_page = Vec::with_capacity(page_streams.len());
        let mut generic_region_bytes_per_page = Vec::with_capacity(page_streams.len());
        for (page_out, local_dict_bytes, text_region_bytes, generic_region_bytes) in page_streams {
            raw_pages.push(page_out);
            local_dict_bytes_per_page.push(local_dict_bytes);
            text_region_bytes_per_page.push(text_region_bytes);
            generic_region_bytes_per_page.push(generic_region_bytes);
        }

        Ok((
            global_segments,
            raw_pages,
            local_dict_bytes_per_page,
            text_region_bytes_per_page,
            generic_region_bytes_per_page,
        ))
    }

    fn prune_symbols_if_needed(&mut self) {
        // No pruning — JBIG2 supports large dictionaries and pruning drops
        // symbol instances, leaving holes in the rendered output.
    }

    /// Cluster similar symbols into groups and select prototypes.
    ///
    /// This is the key optimization for symbol-mode compression. After all pages
    /// have been extracted, we group symbols that look similar (e.g., different
    /// renderings of the letter "e") into clusters. Only one prototype per cluster
    /// is stored in the dictionary. Instances that don't exactly match their
    /// prototype are marked for refinement coding (SPM).
    ///
    /// This replaces the naive O(n²) auto_threshold with a dimension-bucketed
    /// approach that's much faster for large symbol sets.
    fn cluster_symbols(&mut self) -> Result<()> {
        let n = self.global_symbols.len();
        if n < 2 {
            return Ok(());
        }

        // Union-find with path compression and union by rank
        let mut parent: Vec<usize> = (0..n).collect();
        let mut uf_rank: Vec<u32> = vec![0; n];
        let mut comparator = Comparator::default();

        // Group by exact dimensions and compare only neighboring sizes.
        let mut buckets: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
        for (i, sym) in self.global_symbols.iter().enumerate() {
            buckets.entry((sym.height, sym.width)).or_default().push(i);
        }

        // Compare within each bucket and adjacent buckets
        let mut bucket_keys: Vec<(usize, usize)> = buckets.keys().copied().collect();
        bucket_keys.sort_unstable();

        let mut compare_pair = |a_idx: usize, b_idx: usize| {
            if uf_find(&mut parent, a_idx) == uf_find(&mut parent, b_idx) {
                return;
            }

            let a_sym = &self.global_symbols[a_idx];
            let b_sym = &self.global_symbols[b_idx];
            let dim_limit = if self.config.text_refine { 2 } else { 1 };
            if (a_sym.width as i32 - b_sym.width as i32).abs() > dim_limit
                || (a_sym.height as i32 - b_sym.height as i32).abs() > dim_limit
            {
                return;
            }

            let area = a_sym.width.max(b_sym.width) * a_sym.height.max(b_sym.height);
            let max_err = if self.config.text_refine {
                ((self.symbol_pixel_counts[a_idx].max(self.symbol_pixel_counts[b_idx]) as f32
                    * 0.10) as u32)
                    .max(((area as f32) * 0.05) as u32)
                    .clamp(3, 20)
            } else {
                ((area as f32 * 0.04) as u32).clamp(2, 12)
            };
            if self.symbol_pixel_counts[a_idx].abs_diff(self.symbol_pixel_counts[b_idx])
                > max_err as usize
            {
                return;
            }

            let dy_limit = if self.config.text_refine { 1 } else { 0 };
            if let Some(result) =
                comparator.compare_for_refine_family(a_sym, b_sym, max_err, dim_limit, dy_limit)
            {
                let dx = result.dx;
                let dy = result.dy;
                if dx.abs() <= dim_limit && dy.abs() <= dy_limit {
                    uf_union(&mut parent, &mut uf_rank, a_idx, b_idx);
                }
            }
        };

        for &(bh, bw) in &bucket_keys {
            let current_bucket = &buckets[&(bh, bw)];
            for ci in 0..current_bucket.len() {
                for cj in (ci + 1)..current_bucket.len() {
                    compare_pair(current_bucket[ci], current_bucket[cj]);
                }
            }

            for dh in -1i32..=1 {
                for dw in -1i32..=1 {
                    let nh = bh as i32 + dh;
                    let nw = bw as i32 + dw;
                    if nh < 0 || nw < 0 {
                        continue;
                    }
                    let neighbor_key = (nh as usize, nw as usize);
                    if neighbor_key <= (bh, bw) {
                        continue;
                    }
                    if let Some(neighbor_bucket) = buckets.get(&neighbor_key) {
                        for &a_idx in current_bucket {
                            for &b_idx in neighbor_bucket {
                                compare_pair(a_idx, b_idx);
                            }
                        }
                    }
                }
            }
        }

        // Build cluster groups
        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..n {
            let root = uf_find(&mut parent, i);
            clusters.entry(root).or_default().push(i);
        }

        // Select prototype deterministically by usage, then black pixels, then original index.
        let mut old_to_prototype: Vec<usize> = (0..n).collect();
        for (_, members) in &clusters {
            if members.len() <= 1 {
                continue;
            }
            let prototype = self.choose_cluster_prototype(members);
            for &m in members {
                old_to_prototype[m] = prototype;
            }
        }

        // Build new compact symbol list (prototypes only) and index mapping
        let mut seen_prototypes: HashMap<usize, usize> = HashMap::new();
        let mut new_symbols: Vec<BitImage> = Vec::new();
        let mut old_to_new: Vec<usize> = vec![0; n];

        // Process in order so prototype positions are deterministic
        for i in 0..n {
            let proto = old_to_prototype[i];
            if let Some(&new_idx) = seen_prototypes.get(&proto) {
                old_to_new[i] = new_idx;
            } else {
                let new_idx = new_symbols.len();
                new_symbols.push(self.global_symbols[proto].clone());
                seen_prototypes.insert(proto, new_idx);
                old_to_new[i] = new_idx;
            }
        }

        let old_count = n;
        let new_count = new_symbols.len();

        // Remap all instances and mark which ones need refinement
        for page in &mut self.pages {
            for inst in &mut page.symbol_instances {
                let old_idx = inst.symbol_index;
                let new_idx = old_to_new[old_idx];
                let proto = old_to_prototype[old_idx];

                // If this instance's original symbol was NOT the prototype,
                // it needs refinement encoding to preserve quality
                if old_idx != proto {
                    inst.needs_refinement = true;
                    // Compute alignment offset between instance and prototype.
                    // Use a generous error limit (not u32::MAX which overflows in Comparator).
                    let (_, trimmed_inst) = inst.instance_bitmap.trim();
                    let max_ref_err = (trimmed_inst.width * trimmed_inst.height) as u32;
                    if let Some((_, dx, dy)) =
                        comparator.distance(&trimmed_inst, &new_symbols[new_idx], max_ref_err)
                    {
                        inst.refinement_dx = dx;
                        inst.refinement_dy = dy;
                    }
                }

                inst.symbol_index = new_idx;
            }
        }

        // Replace internal state
        self.global_symbols = new_symbols;
        self.symbol_pixel_counts = self
            .global_symbols
            .iter()
            .map(BitImage::count_ones)
            .collect();
        self.rebuild_symbol_metadata();
        self.rebuild_hash_map();

        debug!(
            "Clustering: {} -> {} prototype symbols ({:.1}% reduction)",
            old_count,
            new_count,
            (1.0 - new_count as f64 / old_count.max(1) as f64) * 100.0
        );

        Ok(())
    }

    fn validate_symbol_instance_indices(&self) -> Result<()> {
        for (page_num, page) in self.pages.iter().enumerate() {
            for instance in &page.symbol_instances {
                if instance.symbol_index >= self.global_symbols.len() {
                    anyhow::bail!(
                        "Page {} has symbol instance {} out of range after pruning (max {})",
                        page_num + 1,
                        instance.symbol_index,
                        self.global_symbols.len().saturating_sub(1)
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_symbol_partition(
        &self,
        global_symbol_indices: &[usize],
        page_local_symbols: &[Vec<usize>],
        page_residual_symbols: &[Vec<usize>],
        page_residual_anchor_remaps: &[FxHashMap<usize, usize>],
        page_uses_generic_region: &[bool],
    ) -> Result<()> {
        let global_set: HashSet<usize> = global_symbol_indices.iter().copied().collect();
        for (page_num, page) in self.pages.iter().enumerate() {
            if page_uses_generic_region[page_num] {
                continue;
            }
            let local_set: HashSet<usize> = page_local_symbols[page_num].iter().copied().collect();
            let residual_set: HashSet<usize> =
                page_residual_symbols[page_num].iter().copied().collect();
            for inst in &page.symbol_instances {
                let idx = inst.symbol_index;
                if !global_set.contains(&idx)
                    && !page_residual_anchor_remaps[page_num].contains_key(&idx)
                    && !local_set.contains(&idx)
                    && !residual_set.contains(&idx)
                {
                    anyhow::bail!(
                        "Page {} symbol {} was not resolved to global, local, or residual output",
                        page_num + 1,
                        idx
                    );
                }
            }
        }
        Ok(())
    }

    fn auto_threshold(&mut self) -> Result<()> {
        let mut i = 0;
        let mut comparator = Comparator::default();
        while i < self.global_symbols.len() {
            let mut j = i + 1;
            while j < self.global_symbols.len() {
                if comparator
                    .distance(&self.global_symbols[i], &self.global_symbols[j], 0)
                    .is_some()
                {
                    self.unite_templates(i, j)?;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
        Ok(())
    }

    fn auto_threshold_using_hash(&mut self) -> Result<()> {
        // Repeatedly scan for exact-match duplicates until no more merges occur.
        // Each call to unite_templates invalidates indices, so we rebuild the
        // hash buckets from scratch after every merge.
        loop {
            let mut hashed_templates: HashMap<u32, Vec<usize>> = HashMap::new();
            for (i, symbol) in self.global_symbols.iter().enumerate() {
                let hash = compute_symbol_hash(symbol);
                hashed_templates.entry(hash).or_default().push(i);
            }

            let mut comparator = Comparator::default();
            let mut merged = false;

            for (_, bucket) in &hashed_templates {
                if bucket.len() < 2 {
                    continue;
                }
                // Find first mergeable pair in this bucket
                'outer: for bi in 0..bucket.len() {
                    for bj in (bi + 1)..bucket.len() {
                        if comparator
                            .distance(
                                &self.global_symbols[bucket[bi]],
                                &self.global_symbols[bucket[bj]],
                                0,
                            )
                            .is_some()
                        {
                            self.unite_templates(bucket[bi], bucket[bj])?;
                            merged = true;
                            break 'outer;
                        }
                    }
                }
                if merged {
                    break; // Indices are stale, restart the scan
                }
            }

            if !merged {
                break;
            }
        }
        Ok(())
    }

    fn unite_templates(&mut self, target_idx: usize, source_idx: usize) -> Result<()> {
        if source_idx >= self.global_symbols.len() {
            anyhow::bail!("Source index out of range");
        }

        for page in &mut self.pages {
            for instance in &mut page.symbol_instances {
                if instance.symbol_index == source_idx {
                    instance.symbol_index = target_idx;
                } else if instance.symbol_index > source_idx {
                    instance.symbol_index -= 1;
                }
            }
        }

        self.global_symbols.remove(source_idx);
        self.symbol_pixel_counts.remove(source_idx);
        self.rebuild_symbol_metadata();
        self.rebuild_hash_map();

        Ok(())
    }

    pub fn next_segment_number(&mut self) -> u32 {
        let num = self.next_segment_number;
        self.next_segment_number += 1;
        num
    }

    pub fn flush_dict(&mut self) -> Result<Vec<u8>> {
        if self.global_symbols.is_empty() {
            return Ok(Vec::new());
        }

        let symbol_refs: Vec<&BitImage> = self.global_symbols.iter().collect();
        let dict_data = encode_symbol_dict(&symbol_refs, &self.config, 0)?;

        let dict_segment = Segment {
            number: self.next_segment_number,
            seg_type: SegmentType::SymbolDictionary,
            deferred_non_retain: false,
            retain_flags: 0,
            page_association_type: if self.state.pdf_mode { 2 } else { 0 },
            referred_to: Vec::new(),
            page: if self.state.pdf_mode { None } else { Some(1) },
            payload: dict_data,
        };
        self.next_segment_number += 1;

        let mut output = Vec::new();
        if self.state.pdf_mode {
            dict_segment.write_into(&mut output)?;
            return Ok(output);
        }

        let header = FileHeader {
            // Sequential organisation — payload follows each segment header.
            organisation_type: false,
            unknown_n_pages: false,
            n_pages: 1,
        };
        output.extend(header.to_bytes());
        dict_segment.write_into(&mut output)?;

        Ok(output)
    }

    fn build_instance_residual_bitmap(
        &self,
        instances: &[SymbolInstance],
    ) -> Result<Option<(BitImage, u32, u32)>> {
        if instances.is_empty() {
            return Ok(None);
        }

        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut has_pixels = false;

        for instance in instances {
            if instance.instance_bitmap.count_ones() == 0 {
                continue;
            }
            has_pixels = true;
            min_x = min_x.min(instance.position.x);
            min_y = min_y.min(instance.position.y);
            max_x = max_x.max(instance.position.x + instance.instance_bitmap.width as u32);
            max_y = max_y.max(instance.position.y + instance.instance_bitmap.height as u32);
        }

        if !has_pixels || max_x <= min_x || max_y <= min_y {
            return Ok(None);
        }

        let width = max_x - min_x;
        let height = max_y - min_y;
        let mut residual = BitImage::new(width, height).map_err(|e| anyhow!(e))?;
        for instance in instances {
            let offset_x = (instance.position.x - min_x) as usize;
            let offset_y = (instance.position.y - min_y) as usize;
            for y in 0..instance.instance_bitmap.height {
                for x in 0..instance.instance_bitmap.width {
                    if instance.instance_bitmap.get_usize(x, y) {
                        residual.set_usize(offset_x + x, offset_y + y, true);
                    }
                }
            }
        }

        if residual.count_ones() == 0 {
            return Ok(None);
        }

        Ok(Some((residual, min_x, min_y)))
    }

    fn encode_generic_region_payload_at(
        &self,
        image: &BitImage,
        x: u32,
        y: u32,
    ) -> Result<Vec<u8>> {
        let mut gr_cfg = GenericRegionConfig::new(
            image.width as u32,
            image.height as u32,
            self.config.generic.dpi,
        );
        gr_cfg.x = x;
        gr_cfg.y = y;
        gr_cfg.comb_operator = self.config.generic.comb_operator;
        gr_cfg.mmr = self.config.generic.mmr;
        gr_cfg.tpgdon = self.config.generic.tpgdon;
        gr_cfg.validate().map_err(|e: &'static str| anyhow!(e))?;

        let coder_data = Jbig2ArithCoder::encode_generic_payload_cfg(image, &gr_cfg)?;
        let params: GenericRegionParams = gr_cfg.clone().into();
        let mut payload = params.to_bytes();
        payload.extend_from_slice(&coder_data);
        Ok(payload)
    }
}

/// Encodes a generic region, optionally wrapping it in a complete JBIG2 file.
/// This function is intended to be the top-level entry point for encoding a single generic region.
pub fn encode_generic_region(img: &BitImage, cfg: &Jbig2Config) -> Result<Vec<u8>> {
    // Build generic region config from high-level parameters
    let mut gr_cfg = GenericRegionParams::new(img.width as u32, img.height as u32, cfg.generic.dpi);
    gr_cfg.comb_operator = cfg.generic.comb_operator;
    gr_cfg.mmr = cfg.generic.mmr;
    gr_cfg.tpgdon = cfg.generic.tpgdon;
    gr_cfg.validate().map_err(|e: &'static str| anyhow!(e))?;

    let coder_data =
        Jbig2ArithCoder::encode_generic_payload(img, gr_cfg.template, &gr_cfg.at_pixels)?;

    let params: GenericRegionParams = gr_cfg.clone();

    let mut generic_region_payload = params.to_bytes();
    generic_region_payload.extend_from_slice(&coder_data);

    // Create the generic region segment (segment number 1)
    let generic_region_segment = Segment {
        number: 1, // Segment number 1
        seg_type: SegmentType::ImmediateGenericRegion,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0, // Explicit page association
        referred_to: Vec::new(),
        page: Some(1),                           // Page 1
        payload: generic_region_payload.clone(), // Clone to avoid move
    };

    // If caller wants only the segment, we're done
    if !cfg.want_full_headers {
        let mut seg_bytes = Vec::new();
        generic_region_segment.write_into(&mut seg_bytes)?;
        return Ok(seg_bytes);
    }

    // Otherwise wrap it in a complete one-page JBIG2 file
    let mut out = Vec::with_capacity(generic_region_payload.len() + 64);

    // File header — sequential organisation (matches segment layout below).
    out.extend_from_slice(
        &FileHeader {
            organisation_type: false,
            unknown_n_pages: false,
            n_pages: 1,
        }
        .to_bytes(),
    );

    // Page Information segment (segment number 0)
    Segment {
        number: 0,
        seg_type: SegmentType::PageInformation,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(1),
        payload: PageInfo {
            width: img.width as u32,
            height: img.height as u32,
            xres: cfg.generic.dpi,
            yres: cfg.generic.dpi,
            is_lossless: cfg.is_lossless,
            default_pixel: cfg.default_pixel,
            default_operator: cfg.generic.comb_operator,
            ..Default::default()
        }
        .to_bytes(),
    }
    .write_into(&mut out)?;

    // Generic region segment (segment number 1)
    generic_region_segment.write_into(&mut out)?;

    // EOF segment (segment number 2)
    Segment {
        number: 2,
        seg_type: SegmentType::EndOfFile,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 2,
        referred_to: vec![],
        page: None,
        payload: vec![],
    }
    .write_into(&mut out)?;

    Ok(out)
}

pub fn encode_symbol_dict(
    symbols: &[&BitImage],
    _config: &Jbig2Config,
    num_imported_symbols: u32,
) -> Result<Vec<u8>> {
    let (payload, _order) = encode_symbol_dict_with_order(symbols, _config, num_imported_symbols)?;
    Ok(payload)
}

/// Computes the canonical encoding order for a list of symbols.
///
/// Returns a `Vec<usize>` where each element is an index into the input `symbols` slice,
/// giving the order symbols will appear in the encoded dictionary (after filtering out
/// zero-size symbols, deduplication, and sorting by height class then width).
///
/// This order must be used when mapping symbol instance IDs in text regions.
pub fn canonicalize_dict_symbols(symbols: &[&BitImage]) -> Vec<usize> {
    // Step 1: Filter zero-size, tracking original indices
    let mut valid: Vec<(usize, &BitImage)> = symbols
        .iter()
        .enumerate()
        .filter(|(_, sym)| sym.width > 0 && sym.height > 0)
        .map(|(i, sym)| (i, *sym))
        .collect();

    // Step 2: Sort by (height ASC, width ASC) — same order as sort_symbols_for_dictionary
    // Use stable sort to preserve input order for identical dimensions.
    // No dedup here: the encoder already deduplicates during extraction + auto_threshold.
    // Removing a symbol would leave text region instances without a valid dictionary mapping.
    valid.sort_by(|a, b| (a.1.height, a.1.width).cmp(&(b.1.height, b.1.width)));

    // Return original indices in canonical order
    valid.into_iter().map(|(orig_idx, _)| orig_idx).collect()
}

fn plan_symbol_dictionary_layout(
    symbols: &[&BitImage],
    config: &Jbig2Config,
    usage_weights: Option<&[usize]>,
) -> Result<SymbolDictLayout> {
    let canonical_order = canonicalize_dict_symbols(symbols);
    if canonical_order.is_empty() {
        return Err(anyhow!(
            "encode_symbol_dict: no valid symbols supplied (all symbols had zero width or height)"
        ));
    }

    let _ = (config, usage_weights);
    Ok(SymbolDictLayout {
        export_input_indices: canonical_order,
        refinements: vec![None; symbols.len()],
        diagnostics: SymbolDictDiagnostics {
            singleton_family_count: symbols.len(),
            exported_member_count: symbols.len(),
            ..Default::default()
        },
    })
}

fn build_refinement_family_layout(
    symbols: &[&BitImage],
    canonical_order: &[usize],
    usage_weights: Option<&[usize]>,
) -> SymbolDictLayout {
    let mut comparator = Comparator::default();
    let signatures: Vec<SymbolSignature> = symbols
        .iter()
        .map(|sym| compute_symbol_signature_shared(sym))
        .collect();
    let black_counts: Vec<usize> = symbols.iter().map(|sym| sym.count_ones()).collect();

    let mut canonical_pos = vec![usize::MAX; symbols.len()];
    for (pos, &input_index) in canonical_order.iter().enumerate() {
        canonical_pos[input_index] = pos;
    }

    let mut bucket_map: HashMap<FamilyBucketKey, Vec<usize>> = HashMap::new();
    for &input_index in canonical_order {
        let key = family_bucket_key_for_symbol(symbols[input_index], &signatures[input_index]);
        bucket_map.entry(key).or_default().push(input_index);
    }

    let mut parent: Vec<usize> = (0..symbols.len()).collect();
    let mut rank = vec![0u32; symbols.len()];

    for &input_index in canonical_order {
        let symbol = symbols[input_index];
        let key = family_bucket_key_for_symbol(symbol, &signatures[input_index]);

        for neighbor in family_bucket_neighbors(key) {
            let Some(bucket) = bucket_map.get(&neighbor) else {
                continue;
            };
            for &other_input_index in bucket {
                if canonical_pos[other_input_index] >= canonical_pos[input_index] {
                    continue;
                }
                if family_match_details(
                    &mut comparator,
                    symbol,
                    input_index,
                    symbols[other_input_index],
                    other_input_index,
                    &signatures,
                    &black_counts,
                )
                .is_some()
                {
                    uf_union(&mut parent, &mut rank, input_index, other_input_index);
                }
            }
        }
    }

    let mut families: HashMap<usize, Vec<usize>> = HashMap::new();
    for &input_index in canonical_order {
        let root = uf_find(&mut parent, input_index);
        families.entry(root).or_default().push(input_index);
    }

    let mut export_input_indices = Vec::new();
    let mut refinements = vec![None; symbols.len()];
    let mut diagnostics = SymbolDictDiagnostics::default();

    let mut family_members: Vec<Vec<usize>> = families.into_values().collect();
    family_members.sort_by_key(|members| canonical_pos[members[0]]);
    diagnostics.family_count = family_members.len();

    for mut members in family_members {
        members.sort_by_key(|&input_index| canonical_pos[input_index]);
        if members.len() == 1 {
            diagnostics.singleton_family_count += 1;
            diagnostics.exported_member_count += 1;
            export_input_indices.push(members[0]);
            continue;
        }

        let prototype_input_index = choose_family_prototype(
            &members,
            symbols,
            usage_weights,
            &canonical_pos,
            &signatures,
            &black_counts,
        );
        if diagnostics.sample_lines.len() < 128 {
            diagnostics.sample_lines.push(format!(
                "refine family: prototype={} members={} prototype_usage={}",
                prototype_input_index,
                members.len(),
                usage_weights
                    .and_then(|weights| weights.get(prototype_input_index).copied())
                    .unwrap_or(1)
            ));
        }
        export_input_indices.push(prototype_input_index);
        diagnostics.exported_member_count += 1;

        for &member_input_index in &members {
            if member_input_index == prototype_input_index {
                continue;
            }

            let maybe_match = family_match_details(
                &mut comparator,
                symbols[member_input_index],
                member_input_index,
                symbols[prototype_input_index],
                prototype_input_index,
                &signatures,
                &black_counts,
            );

            match maybe_match {
                Some((err, dx, dy))
                    if family_should_refine(
                        symbols[member_input_index],
                        symbols[prototype_input_index],
                        err,
                        dx,
                        dy,
                        usage_weights
                            .and_then(|weights| weights.get(member_input_index).copied())
                            .unwrap_or(1),
                    ) =>
                {
                    refinements[member_input_index] = Some(RefinementPlan {
                        prototype_input_index,
                        refinement_dx: dx,
                        refinement_dy: dy,
                    });
                    diagnostics.refined_member_count += 1;
                    if diagnostics.sample_lines.len() < 128 {
                        diagnostics.sample_lines.push(format!(
                            "  refine member={} -> prototype={} dx={} dy={} err={} usage={}",
                            member_input_index,
                            prototype_input_index,
                            dx,
                            dy,
                            err,
                            usage_weights
                                .and_then(|weights| weights.get(member_input_index).copied())
                                .unwrap_or(1)
                        ));
                    }
                }
                _ => {
                    export_input_indices.push(member_input_index);
                    diagnostics.exported_member_count += 1;
                    if diagnostics.sample_lines.len() < 128 {
                        diagnostics.sample_lines.push(format!(
                            "  export member={} as standalone usage={}",
                            member_input_index,
                            usage_weights
                                .and_then(|weights| weights.get(member_input_index).copied())
                                .unwrap_or(1)
                        ));
                    }
                }
            }
        }
    }

    export_input_indices.sort_by_key(|&input_index| canonical_pos[input_index]);

    SymbolDictLayout {
        export_input_indices,
        refinements,
        diagnostics,
    }
}

fn family_refinement_gain(
    target: &BitImage,
    reference: &BitImage,
    err: u32,
    dx: i32,
    dy: i32,
) -> i64 {
    let plain_cost = symbol_dictionary_entry_bytes(target) as i64 + 10;
    let refine_cost = 10
        + err as i64
        + ((dx.abs() + dy.abs()) as i64 * 3)
        + (target.width.abs_diff(reference.width) + target.height.abs_diff(reference.height))
            as i64
            * 2;
    plain_cost - refine_cost
}

fn family_should_refine(
    target: &BitImage,
    reference: &BitImage,
    err: u32,
    dx: i32,
    dy: i32,
    usage_count: usize,
) -> bool {
    if usage_count > 1 {
        return false;
    }
    let export_gain = family_refinement_gain(target, reference, err, dx, dy);
    export_gain > 12
}

fn choose_family_prototype(
    members: &[usize],
    symbols: &[&BitImage],
    usage_weights: Option<&[usize]>,
    canonical_pos: &[usize],
    signatures: &[SymbolSignature],
    black_counts: &[usize],
) -> usize {
    if members.len() == 1 {
        return members[0];
    }

    let mut comparator = Comparator::default();
    let mut best_idx = members[0];
    let mut best_cost = u64::MAX;
    let mut best_support = 0u64;

    for &candidate in members {
        let mut total_cost = 0u64;
        for &other in members {
            if candidate == other {
                continue;
            }
            let weight = usage_weights
                .and_then(|weights| weights.get(other).copied())
                .unwrap_or(1) as u64;
            match family_match_details(
                &mut comparator,
                symbols[other],
                other,
                symbols[candidate],
                candidate,
                signatures,
                black_counts,
            ) {
                Some((err, dx, dy)) => {
                    total_cost += (refine_compare_score(err, dx, dy) as u64 + 4) * weight;
                }
                None => total_cost += 1_000_000 * weight,
            }
        }

        let candidate_support = usage_weights
            .and_then(|weights| weights.get(candidate).copied())
            .unwrap_or(1) as u64;
        let score_close = if best_cost == u64::MAX {
            false
        } else {
            total_cost <= best_cost + best_cost / 50
        };

        if total_cost < best_cost
            || (score_close && candidate_support > best_support)
            || (total_cost == best_cost
                && candidate_support == best_support
                && canonical_pos[candidate] < canonical_pos[best_idx])
        {
            best_cost = total_cost;
            best_idx = candidate;
            best_support = candidate_support;
        }
    }

    best_idx
}

fn encode_symbol_dictionary_segments(
    symbols: &[&BitImage],
    config: &Jbig2Config,
    layout: &SymbolDictLayout,
) -> Result<EncodedSymbolDictionary> {
    let mut encoded = EncodedSymbolDictionary {
        payload: Vec::new(),
        input_to_exported_pos: vec![u32::MAX; symbols.len()],
        exported_symbol_count: 0,
    };

    let (dict_payload, base_order) =
        encode_symbol_dict_subset_with_order(symbols, config, &layout.export_input_indices, 0)?;
    for (dict_pos, &input_index) in base_order.iter().enumerate() {
        encoded.input_to_exported_pos[input_index] = dict_pos as u32;
    }
    encoded.exported_symbol_count = base_order.len() as u32;
    encoded.payload = dict_payload;

    for (input_index, refinement) in layout.refinements.iter().enumerate() {
        if let Some(refinement) = refinement {
            let prototype_pos = encoded.input_to_exported_pos[refinement.prototype_input_index];
            if prototype_pos != u32::MAX {
                encoded.input_to_exported_pos[input_index] = prototype_pos;
            }
        }
    }

    Ok(encoded)
}

fn encode_symbol_dict_subset_with_order(
    symbols: &[&BitImage],
    config: &Jbig2Config,
    subset_indices: &[usize],
    num_imported_symbols: u32,
) -> Result<(Vec<u8>, Vec<usize>)> {
    let subset_symbols: Vec<&BitImage> = subset_indices.iter().map(|&i| symbols[i]).collect();
    let (payload, subset_order) =
        encode_symbol_dict_with_order(&subset_symbols, config, num_imported_symbols)?;
    let input_order = subset_order
        .into_iter()
        .map(|subset_index| subset_indices[subset_index])
        .collect();
    Ok((payload, input_order))
}

/// Encodes a symbol dictionary, returning both the payload and the mapping from
/// encoded dictionary position → input index.
pub fn encode_symbol_dict_with_order(
    symbols: &[&BitImage],
    _config: &Jbig2Config,
    num_imported_symbols: u32,
) -> Result<(Vec<u8>, Vec<usize>)> {
    // Compute canonical order (filter + dedup + sort)
    let canonical_order = canonicalize_dict_symbols(symbols);

    if canonical_order.is_empty() {
        return Err(anyhow!(
            "encode_symbol_dict: no valid symbols supplied (all symbols had zero width or height)"
        ));
    }

    // Build the ordered symbol list
    let ordered_symbols: Vec<&BitImage> = canonical_order.iter().map(|&i| symbols[i]).collect();

    // Verify symbol dimensions are within JBIG2 limits
    for (i, sym) in ordered_symbols.iter().enumerate() {
        if sym.width > (1 << 24) || sym.height > (1 << 24) {
            return Err(anyhow!(
                "Symbol at index {} exceeds maximum dimensions ({}x{})",
                i,
                sym.width,
                sym.height
            ));
        }
    }

    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    let num_new_syms = ordered_symbols.len() as u32;
    // Per T.88 §7.4.2.1.6, SDNUMEXSYMS is the total count of exported symbols which
    // includes any imported symbols carried forward from referenced dictionaries.
    let num_export_syms = num_imported_symbols.saturating_add(num_new_syms);

    // Create symbol dictionary parameters
    let params = SymbolDictParams {
        sd_template: 0, // Use standard template 0
        // Match jbig2enc's template-0 adaptive pixels for symbol dictionaries.
        at: [(3, -1), (-3, -1), (2, -2), (-2, -2)],
        refine_aggregate: false,
        refine_template: 0,
        refine_at: [(0, 0), (0, 0)],
        exsyms: num_export_syms,
        newsyms: num_new_syms,
    };

    if cfg!(debug_assertions) {
        debug!("encode_symbol_dict: Exporting {} symbols", num_export_syms);
        trace!("encode_symbol_dict: SymbolDictParams details: {:?}", params);
    }

    // Write the symbol dictionary parameters
    payload.extend(params.to_bytes());

    // Symbols are already in canonical (height, width) order from canonicalize_dict_symbols.
    // We need to encode them in this exact order, grouped by height class for delta encoding.
    // Build height classes from the already-sorted ordered_symbols to preserve the canonical order.
    let mut height_classes: Vec<Vec<&BitImage>> = Vec::new();
    let mut current_height: Option<usize> = None;
    let mut current_class: Vec<&BitImage> = Vec::new();

    for &sym in &ordered_symbols {
        match current_height {
            None => {
                // First symbol
                current_height = Some(sym.height);
                current_class.push(sym);
            }
            Some(h) if sym.height == h => {
                // Same height class
                current_class.push(sym);
            }
            Some(_) => {
                // New height class - push previous and start new
                height_classes.push(current_class);
                current_height = Some(sym.height);
                current_class = vec![sym];
            }
        }
    }
    if !current_class.is_empty() {
        height_classes.push(current_class);
    }

    // Debug: log the encoding order and first few pixels of each symbol for verification
    #[cfg(debug_assertions)]
    {
        debug!(
            "Symbol dictionary encoding order ({} symbols):",
            ordered_symbols.len()
        );
        let mut dict_pos = 0u32;
        for (hc_idx, symbols_in_class) in height_classes.iter().enumerate() {
            debug!(
                "  Height class {}: {} symbols",
                hc_idx,
                symbols_in_class.len()
            );
            for (sym_idx, sym) in symbols_in_class.iter().enumerate() {
                // Log first pixel position for each symbol
                let first_pixel = first_black_pixel(sym);
                if sym_idx < 5 || sym_idx >= symbols_in_class.len() - 2 {
                    debug!(
                        "    dict_pos={} -> {}x{} first_pixel={:?}",
                        dict_pos, sym.width, sym.height, first_pixel
                    );
                } else if sym_idx == 5 {
                    debug!(
                        "    ... ({} symbols omitted) ...",
                        symbols_in_class.len() - 7
                    );
                }
                dict_pos += 1;
            }
        }
    }

    let mut last_height = 0;

    // 4. Encode the height classes
    for symbols_in_class in &height_classes {
        let h = symbols_in_class[0].height; // All symbols in class have same height
        // A. Encode Delta Height
        let delta_h = h as i32 - last_height as i32;
        let _ = coder.encode_integer(crate::jbig2arith::IntProc::Iadh, delta_h);
        last_height = h;

        let mut last_width = 0;
        #[cfg(debug_assertions)]
        let mut dict_pos = 0u32;

        // Debug: check symbols in this height class (disabled in release)
        #[cfg(debug_assertions)]
        {
            debug!("Height class {} has {} symbols:", h, symbols_in_class.len());
            for (i, symbol) in symbols_in_class.iter().enumerate() {
                debug!("  Symbol {}: {}x{}", i, symbol.width, symbol.height);
            }
        }

        // B. Encode symbols within this height class
        // Symbols within each height class are already sorted by width from canonicalize_dict_symbols.
        for (i, symbol) in symbols_in_class.iter().enumerate() {
            // I. Encode Delta Width
            let delta_w = symbol.width as i32 - last_width;

            // Debug output to help diagnose the issue (disabled in release)
            #[cfg(debug_assertions)]
            debug!(
                "Height class {}, Symbol {}: width={}, last_width={}, delta_w={}",
                h, i, symbol.width, last_width, delta_w
            );

            let _ = coder.encode_integer(crate::jbig2arith::IntProc::Iadw, delta_w);
            last_width = symbol.width as i32; // last_width becomes current width

            // II. Encode Symbol Bitmap using Generic Region Procedure
            let packed = symbol.packed_words();

            // Debug: dump first few symbols' bitmap data for verification
            #[cfg(debug_assertions)]
            {
                debug!(
                    "  dict_pos={} {}x{} first_word={:08x}",
                    dict_pos,
                    symbol.width,
                    symbol.height,
                    packed.get(0).unwrap_or(&0)
                );
            }

            // Verify bit-order correctness: first black pixel should match between symbol and packed data
            if let Some(expected_first_pixel) = first_black_pixel(symbol) {
                let actual_first_pixel = crate::jbig2sym::first_black_pixel_in_packed(
                    packed,
                    symbol.width,
                    symbol.height,
                );
                assert_eq!(
                    actual_first_pixel,
                    Some(expected_first_pixel),
                    "bit-order / row-order mismatch in symbol dict packer! Expected first black pixel at {:?}, got {:?}",
                    expected_first_pixel,
                    actual_first_pixel
                );
            }

            coder.encode_generic_region(
                packed,
                symbol.width,
                symbol.height,
                params.sd_template,
                &[(3, -1), (-3, -1), (2, -2), (-2, -2)],
            )?;

            #[cfg(debug_assertions)]
            {
                dict_pos += 1;
            }
        }

        // OOB marks the end of this height class.
        let _ = coder.encode_oob(IntProc::Iadw);
    }

    // Export flags come after the symbol bitmap data (run-length form).
    // T.88 §7.4.2.2: the run-length must cover SDNUMINSYMS + SDNUMNEWSYMS slots.
    // We export every symbol (both imported and new), so the sequence is a single
    // "all exported" run covering num_export_syms = num_imported + num_new.
    let _ = coder.encode_integer(IntProc::Iaex, 0);
    let _ = coder.encode_integer(IntProc::Iaex, num_export_syms as i32);

    // 5. flush the coder ONCE
    coder.flush(true);

    // 6. Append the single, complete arithmetic payload
    payload.extend(coder.as_bytes());

    Ok((payload, canonical_order))
}

/// Computes the bounding box that contains all symbol instances.
///
/// # Arguments
/// * `instances` - Slice of symbol instances to compute bounds for
/// * `all_known_symbols` - All available symbol bitmaps
///
/// # Returns
/// A tuple of (min_x, min_y, width, height) representing the bounding box
fn compute_region_bounds(
    instances: &[TextRegionSymbolInstance],
    all_known_symbols: &[&BitImage],
) -> (u32, u32, u32, u32) {
    if instances.is_empty() {
        return (0, 0, 0, 0);
    }
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0u32;
    let mut max_y_coord = 0u32;

    for instance in instances {
        let sym_idx = instance.symbol_id as usize;
        if sym_idx >= all_known_symbols.len() {
            continue; // Skip invalid symbol indices
        }

        let pos = Rect {
            x: instance.x as u32, // Convert i32 to u32
            y: instance.y as u32, // Convert i32 to u32
            width: crate::jbig2shared::usize_to_u32(all_known_symbols[sym_idx].width),
            height: crate::jbig2shared::usize_to_u32(all_known_symbols[sym_idx].height),
        };

        min_x = min_x.min(pos.x);
        min_y = min_y.min(pos.y);
        max_x_coord = max_x_coord.max(pos.x + pos.width);
        max_y_coord = max_y_coord.max(pos.y + pos.height);
    }

    // Handle potential underflow if max < min (shouldn't happen with valid coordinates)
    let region_width = if max_x_coord > min_x {
        max_x_coord - min_x
    } else {
        0
    };

    let region_height = if max_y_coord > min_y {
        max_y_coord - min_y
    } else {
        0
    };

    (min_x, min_y, region_width, region_height)
}

// (The previous `encode_refine` helper was removed: it built only 4 refinement
// contexts instead of the 8192-state GRREF template required by T.88 §6.3, so
// any output it produced was not decodable. It had no callers in the crate.
// Use `Jbig2ArithCoder::encode_refinement_region` (called from
// `encode_text_region_with_refinement`) for spec-compliant refinement coding.)

/// Encodes a text region segment using pre-computed dictionary position maps.
///
/// Unlike `encode_text_region` which maps by list position, this function uses
/// explicit global_symbols_index → dictionary_position maps that account for the
/// canonical (filter/dedup/sort) order produced by `encode_symbol_dict_with_order`.
///
/// The decoder concatenates dictionary exports: global_dict[0..N] then local_dict[0..M].
/// Symbol IDs 0..N-1 map to the global dict, N..N+M-1 map to the local dict.
#[inline]
fn symbol_id_from_dense_maps(
    symbol_index: usize,
    global_sym_to_dict_pos: &[u32],
    num_global_dict_symbols: u32,
    local_sym_to_dict_pos: &[u32],
) -> Option<u32> {
    let global = global_sym_to_dict_pos
        .get(symbol_index)
        .copied()
        .unwrap_or(u32::MAX);
    if global != u32::MAX {
        return Some(global);
    }
    let local = local_sym_to_dict_pos
        .get(symbol_index)
        .copied()
        .unwrap_or(u32::MAX);
    if local != u32::MAX {
        Some(num_global_dict_symbols + local)
    } else {
        None
    }
}

pub fn encode_text_region_mapped(
    instances: &[SymbolInstance],
    config: &Jbig2Config,
    all_symbols: &[BitImage],
    global_sym_to_dict_pos: &[u32],
    num_global_dict_symbols: u32,
    local_sym_to_dict_pos: &[u32],
    page_num: usize,
    num_local_dict_symbols: u32,
) -> Result<Vec<u8>> {
    if instances.is_empty() {
        return Err(anyhow!("No symbol instances provided for text region"));
    }

    let debug_encoding = page_num == 0 && std::env::var("JBIG2_DEBUG").map_or(false, |v| v == "1");
    let mut enc_debug_lines: Vec<String> = Vec::new();

    let num_total_dict_symbols = num_global_dict_symbols + num_local_dict_symbols;

    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0u32;
    let mut max_y_coord = 0u32;

    for instance in instances {
        let sym = &all_symbols[instance.symbol_index];
        min_x = min_x.min(instance.position.x);
        min_y = min_y.min(instance.position.y);
        max_x_coord = max_x_coord.max(instance.position.x + sym.width as u32);
        max_y_coord = max_y_coord.max(instance.position.y + sym.height as u32);
    }

    let region_width = max_x_coord.saturating_sub(min_x);
    let region_height = max_y_coord.saturating_sub(min_y);

    let params = TextRegionParams {
        width: region_width,
        height: region_height,
        x: min_x,
        y: min_y,
        ds_offset: config.text_ds_offset,
        refine: config.text_refine,
        log_strips: config.text_log_strips,
        ref_corner: config.text_ref_corner,
        transposed: config.text_transposed,
        comb_op: config.text_comb_op,
        refine_template: config.text_refine_template,
    };

    payload.extend(params.to_bytes());
    payload.extend_from_slice(&(instances.len() as u32).to_be_bytes());

    // T.88 §7.4.3.1.7 / §6.5.8.2.3 (SDHUFF=0): arithmetic text-region IAID uses
    // SBSYMCODELEN = ceil(log2(SBNUMSYMS)) with NO floor of 1. The formulas differ
    // only at SBNUMSYMS == 1, where the decoder reads zero ID bits; emitting a
    // spurious 1-bit IAID there desyncs the arithmetic decoder for every later
    // symbol (S-coordinate corruption). encode_iaid is skipped when this is 0.
    let symbol_id_bits = log2up(num_total_dict_symbols.max(1));

    #[derive(Clone, Copy)]
    struct EncodedInstance {
        strip_base: i32,
        x: i32,
        t_offset: i32,
        symbol_id: u32,
        symbol_width: i32,
    }

    let strip_width = 1i32 << params.log_strips.min(3);
    let mut encoded_instances = Vec::with_capacity(instances.len());

    for instance in instances {
        let gs_idx = instance.symbol_index;
        let sym = &all_symbols[gs_idx];

        // Map to dictionary position using the canonical order maps.
        // First check global dict, then local dict (offset by num_global_dict_symbols).
        let symbol_id = if let Some(symbol_id) = symbol_id_from_dense_maps(
            gs_idx,
            global_sym_to_dict_pos,
            num_global_dict_symbols,
            local_sym_to_dict_pos,
        ) {
            symbol_id
        } else {
            anyhow::bail!(
                "Symbol instance (global_symbols index {}) not found in any dictionary!",
                gs_idx
            );
        };

        let abs = instance.position;
        let rel_x = abs.x as i32 - min_x as i32;
        // REFCORNER=TOPLEFT (value 1): T is the top of the original bounding box.
        let rel_y = abs.y as i32 - min_y as i32;
        let strip_base = (rel_y / strip_width) * strip_width;
        let t_offset = rel_y - strip_base;

        encoded_instances.push(EncodedInstance {
            strip_base,
            x: rel_x,
            t_offset,
            symbol_id,
            symbol_width: sym.width as i32,
        });
    }

    encoded_instances.sort_by_key(|e| (e.strip_base, e.x));

    if debug_encoding {
        enc_debug_lines.push(format!("=== PAGE 0 ENCODING LOG ==="));
        enc_debug_lines.push(format!(
            "Region: {}x{} at ({},{})",
            params.width, params.height, params.x, params.y
        ));
        enc_debug_lines.push(format!(
            "min_x={} min_y={} strip_width={}",
            min_x, min_y, strip_width
        ));
        enc_debug_lines.push(format!(
            "Total instances: {}, dict symbols: {}",
            encoded_instances.len(),
            num_total_dict_symbols
        ));
        enc_debug_lines.push(String::new());

        // Show mapping from symbol_id to dimensions for reference
        enc_debug_lines.push("Symbol ID -> dimensions lookup (first 30):".to_string());
        for (dict_id, sym) in all_symbols.iter().enumerate().take(30) {
            let dict_pos = symbol_id_from_dense_maps(
                dict_id,
                global_sym_to_dict_pos,
                num_global_dict_symbols,
                local_sym_to_dict_pos,
            )
            .unwrap_or(u32::MAX);
            enc_debug_lines.push(format!(
                "  gs_idx={} -> dict_pos={} ({}x{})",
                dict_id, dict_pos, sym.width, sym.height
            ));
        }
        enc_debug_lines.push(String::new());

        enc_debug_lines.push(format!(
            "{:<6} {:<8} {:<8} {:<10} {:<8} {:<10} {:<10} {:<10}",
            "Idx", "SymID", "SymW", "StripBase", "TOffset", "RelX", "DeltaT", "DeltaS"
        ));
    }

    let mut strip_t = 0i32;
    let mut first_s = 0i32;
    let mut idx = 0usize;

    // §6.4.5 step 1: initial STRIPT value (decoder reads one IADT before the loop)
    let _ = coder.encode_integer(IntProc::Iadt, 0);

    let sbdsoffset = params.ds_offset as i32;

    while idx < encoded_instances.len() {
        let current_strip = encoded_instances[idx].strip_base;
        let delta_t = current_strip - strip_t;
        let _ = coder.encode_integer(IntProc::Iadt, delta_t / strip_width);

        if debug_encoding && delta_t != 0 {
            enc_debug_lines.push(format!(
                "--- strip break: IADT delta_t={} (strip_t {} → {})",
                delta_t, strip_t, current_strip
            ));
        }
        strip_t = current_strip;

        let mut first_symbol_in_strip = true;
        let mut current_s = 0i32;
        while idx < encoded_instances.len() && encoded_instances[idx].strip_base == current_strip {
            let item = encoded_instances[idx];
            let delta_s;
            if first_symbol_in_strip {
                delta_s = item.x - first_s;
                let _ = coder.encode_integer(IntProc::Iafs, delta_s);
                first_s += delta_s;
                current_s = first_s;
                first_symbol_in_strip = false;
            } else {
                // T.88 §6.4.5 step 4d: decoder computes CURS = CURS + IADS + SBDSOFFSET.
                // Therefore encode IADS = (item.x - current_s) - sbdsoffset so the
                // decoder lands on item.x. With the default ds_offset = 0 this is a
                // no-op, but it makes the encoder behave correctly when callers set a
                // non-zero text_ds_offset.
                delta_s = item.x - current_s - sbdsoffset;
                let _ = coder.encode_integer(IntProc::Iads, delta_s);
                current_s += delta_s + sbdsoffset;
            }

            if debug_encoding {
                enc_debug_lines.push(format!(
                    "{:<6} {:<8} {:<8} {:<10} {:<8} {:<10} {:<10} {:<10}",
                    idx,
                    item.symbol_id,
                    item.symbol_width,
                    item.strip_base,
                    item.t_offset,
                    item.x,
                    delta_t,
                    delta_s
                ));
            }

            if strip_width > 1 {
                let _ = coder.encode_integer(IntProc::Iait, item.t_offset);
            }
            if symbol_id_bits > 0 {
                let _ = coder.encode_iaid(item.symbol_id, symbol_id_bits as u8);
            }
            current_s += item.symbol_width - 1;
            idx += 1;
        }
        let _ = coder.encode_oob(IntProc::Iads);
    }

    // Decode simulation: replay §6.4.5 from the encoder's perspective
    // to verify positions match what the decoder will compute.
    if debug_encoding {
        enc_debug_lines.push(String::new());
        enc_debug_lines.push(format!("=== DECODE SIMULATION ==="));
        enc_debug_lines.push(format!(
            "{:<6} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<8}",
            "Idx", "ExpX", "ExpY", "DecS", "DecT", "AbsX", "AbsY", "Match?"
        ));

        // Collect the IADT, IAFS, IADS, IAIT values and symbol_ids in encoding order for replay
        let sbstrips = strip_width;
        let sbdsoffset = params.ds_offset as i32;
        let mut dec_stript = 0i32;
        let mut dec_firsts = 0i32;
        let mut sim_idx = 0usize;
        let mut strip_start = 0usize;

        // Group instances by strip_base (they're already sorted)
        while sim_idx < encoded_instances.len() {
            let current_strip = encoded_instances[sim_idx].strip_base;
            // Compute delta_t the same way the encoder did
            let delta_t = if sim_idx == 0 && current_strip == 0 {
                0 // first IADT is the initial STRIPT value
            } else if sim_idx == strip_start {
                // Changed strip: the encoder emits IADT = (current_strip - prev_strip_t) / strip_width
                // But we need to replay the exact values. Let's just recompute.
                current_strip - dec_stript
            } else {
                0 // same strip, no IADT
            };

            // §6.4.5 step 2: STRIPT = STRIPT + IADT × SBSTRIPS
            if sim_idx == strip_start || sim_idx == 0 {
                let iadt_value = (current_strip - dec_stript) / sbstrips;
                dec_stript += iadt_value * sbstrips;
            }

            let mut first_in_strip = true;
            let mut dec_curs = 0i32;
            let strip_base = current_strip;

            while sim_idx < encoded_instances.len()
                && encoded_instances[sim_idx].strip_base == strip_base
            {
                let item = encoded_instances[sim_idx];

                if first_in_strip {
                    // §6.4.5: FIRSTS = FIRSTS + IAFS; CURS = FIRSTS
                    let iafs = item.x - dec_firsts;
                    dec_firsts += iafs;
                    dec_curs = dec_firsts;
                    first_in_strip = false;
                } else {
                    // §6.4.5: encoder emits IADS = item.x - dec_curs - SBDSOFFSET so that
                    // the decoder's CURS = CURS + IADS + SBDSOFFSET lands on item.x.
                    let iads = item.x - dec_curs - sbdsoffset;
                    dec_curs += iads + sbdsoffset;
                }

                // §6.4.5: TI = STRIPT * SBSTRIPS + IAIT (IAIT=0 when SBSTRIPS=1)
                let dec_ti = dec_stript;
                let dec_si = dec_curs;

                // Absolute page coords the decoder would compute
                let abs_x = dec_si + min_x as i32;
                let abs_y = dec_ti + min_y as i32;

                // Expected absolute coords (what we intended)
                let exp_x = item.x + min_x as i32;
                let exp_y = item.strip_base + min_y as i32;

                let ok = abs_x == exp_x && abs_y == exp_y;
                let tag = if ok { "OK" } else { "MISMATCH!" };

                if !ok || sim_idx < 60 {
                    enc_debug_lines.push(format!(
                        "{:<6} {:<10} {:<10} {:<10} {:<10} {:<10} {:<10} {:<8}",
                        sim_idx, exp_x, exp_y, dec_si, dec_ti, abs_x, abs_y, tag
                    ));
                }

                // §6.4.5 step 4g: CURS = CURS + WI - 1
                dec_curs += item.symbol_width - 1;
                sim_idx += 1;
            }
            strip_start = sim_idx;
        }
    }

    // Write encoding debug log for page 0
    if debug_encoding && !enc_debug_lines.is_empty() {
        let log_path = std::path::Path::new("jbig2_debug_page0.log");
        // Append to same file as matching log
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            use std::io::Write;
            let _ = writeln!(f, "");
            for line in &enc_debug_lines {
                let _ = writeln!(f, "{}", line);
            }
        }
    }

    coder.flush(true);
    payload.extend(coder.as_bytes());

    Ok(payload)
}

/// Encodes a text region with Soft Pattern Matching (SPM / refinement coding).
///
/// This is the SBREFINE=1 variant of text region encoding. For each symbol instance:
/// - Encode the symbol ID and position (same as non-refinement)
/// - Encode RI (refinement indicator) via IARI
///   - RI=0: direct substitution of the dictionary symbol (no refinement)
///   - RI=1: encode size deltas (IARDW, IARDH), position offsets (IARDX, IARDY),
///     then a pixel-by-pixel refinement region using the dictionary symbol as reference
///
/// This allows lossy symbol clustering (small dictionary) while preserving
/// per-instance fidelity through the refinement residual.
pub fn encode_text_region_with_refinement(
    instances: &[SymbolInstance],
    config: &Jbig2Config,
    all_symbols: &[BitImage],
    global_sym_to_dict_pos: &[u32],
    num_global_dict_symbols: u32,
    local_sym_to_dict_pos: &[u32],
    num_local_dict_symbols: u32,
) -> Result<Vec<u8>> {
    if instances.is_empty() {
        return Err(anyhow!("No symbol instances provided for text region"));
    }

    let num_total_dict_symbols = num_global_dict_symbols + num_local_dict_symbols;

    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    // Compute region bounds. For refined instances, use the actual instance
    // bitmap size (which may be larger than the prototype) so the region is
    // large enough to hold the refined glyphs.
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0u32;
    let mut max_y_coord = 0u32;

    for instance in instances {
        let (w, h) = if instance.needs_refinement {
            let (_, trimmed) = instance.instance_bitmap.trim();
            (trimmed.width as u32, trimmed.height as u32)
        } else {
            let sym = &all_symbols[instance.symbol_index];
            (sym.width as u32, sym.height as u32)
        };

        min_x = min_x.min(instance.position.x);
        min_y = min_y.min(instance.position.y);
        max_x_coord = max_x_coord.max(instance.position.x + w);
        max_y_coord = max_y_coord.max(instance.position.y + h);
    }

    let region_width = max_x_coord.saturating_sub(min_x);
    let region_height = max_y_coord.saturating_sub(min_y);

    // SBREFINE=1 in the text region params
    let params = TextRegionParams {
        width: region_width,
        height: region_height,
        x: min_x,
        y: min_y,
        ds_offset: config.text_ds_offset,
        refine: true, // SBREFINE = 1
        log_strips: config.text_log_strips,
        ref_corner: config.text_ref_corner,
        transposed: config.text_transposed,
        comb_op: config.text_comb_op,
        refine_template: config.text_refine_template,
    };

    payload.extend(params.to_bytes());
    payload.extend_from_slice(&(instances.len() as u32).to_be_bytes());

    // T.88 §7.4.3.1.7 / §6.5.8.2.3 (SDHUFF=0): arithmetic text-region IAID uses
    // SBSYMCODELEN = ceil(log2(SBNUMSYMS)) with NO floor of 1. The formulas differ
    // only at SBNUMSYMS == 1, where the decoder reads zero ID bits; emitting a
    // spurious 1-bit IAID there desyncs the arithmetic decoder for every later
    // symbol (S-coordinate corruption). encode_iaid is skipped when this is 0.
    let symbol_id_bits = log2up(num_total_dict_symbols.max(1));

    // Prepare instances with dictionary mapping (same structure as non-refinement)
    #[derive(Clone)]
    struct RefinedInstance {
        strip_base: i32,
        x: i32,
        t_offset: i32,
        symbol_id: u32,
        symbol_width: i32,
        // Refinement data
        needs_refinement: bool,
        /// Index into original instances array (for accessing instance_bitmap)
        orig_idx: usize,
    }

    let strip_width = 1i32 << params.log_strips.min(3);
    let mut encoded_instances = Vec::with_capacity(instances.len());

    for (orig_idx, instance) in instances.iter().enumerate() {
        let gs_idx = instance.symbol_index;
        let sym = &all_symbols[gs_idx];

        let symbol_id = if let Some(symbol_id) = symbol_id_from_dense_maps(
            gs_idx,
            global_sym_to_dict_pos,
            num_global_dict_symbols,
            local_sym_to_dict_pos,
        ) {
            symbol_id
        } else {
            anyhow::bail!(
                "Symbol instance (global_symbols index {}) not found in any dictionary!",
                gs_idx
            );
        };

        let abs = instance.position;
        let rel_x = abs.x as i32 - min_x as i32;
        // REFCORNER=TOPLEFT (value 1): T is the top of the original bounding box.
        let rel_y = abs.y as i32 - min_y as i32;
        let strip_base = (rel_y / strip_width) * strip_width;
        let t_offset = rel_y - strip_base;

        encoded_instances.push(RefinedInstance {
            strip_base,
            x: rel_x,
            t_offset,
            symbol_id,
            symbol_width: sym.width as i32,
            needs_refinement: instance.needs_refinement,
            orig_idx,
        });
    }

    encoded_instances.sort_by_key(|e| (e.strip_base, e.x));

    // Encode strip-by-strip, symbol-by-symbol (same loop structure as non-refinement)
    let mut strip_t = 0i32;
    let mut first_s = 0i32;
    let mut idx = 0usize;

    // Default refinement AT pixel: (-1, -1), matching jbig2enc convention
    let grat: [(i8, i8); 1] = [(-1, -1)];

    // §6.4.5 step 1: initial STRIPT value (decoder reads one IADT before the loop)
    let _ = coder.encode_integer(IntProc::Iadt, 0);

    while idx < encoded_instances.len() {
        let current_strip = encoded_instances[idx].strip_base;
        let delta_t = current_strip - strip_t;
        let _ = coder.encode_integer(IntProc::Iadt, delta_t / strip_width);
        strip_t = current_strip;

        let mut first_symbol_in_strip = true;
        let mut current_s = 0i32;

        while idx < encoded_instances.len() && encoded_instances[idx].strip_base == current_strip {
            let item = &encoded_instances[idx];
            if first_symbol_in_strip {
                let delta_fs = item.x - first_s;
                let _ = coder.encode_integer(IntProc::Iafs, delta_fs);
                first_s += delta_fs;
                current_s = first_s;
                first_symbol_in_strip = false;
            } else {
                let delta_s = item.x - current_s;
                let _ = coder.encode_integer(IntProc::Iads, delta_s);
                current_s += delta_s;
            }

            if strip_width > 1 {
                let _ = coder.encode_integer(IntProc::Iait, item.t_offset);
            }

            // Symbol ID
            if symbol_id_bits > 0 {
                let _ = coder.encode_iaid(item.symbol_id, symbol_id_bits as u8);
            }

            // ── SPM: Refinement indicator (RI) ──
            let ri = if item.needs_refinement { 1i32 } else { 0i32 };
            let _ = coder.encode_integer(IntProc::Iari, ri);

            if item.needs_refinement {
                // Get the original instance data and the prototype
                let orig_instance = &instances[item.orig_idx];
                let prototype = &all_symbols[orig_instance.symbol_index];

                // Trim the instance bitmap to get the actual glyph
                let (_, trimmed_instance) = orig_instance.instance_bitmap.trim();

                // Size deltas: how much wider/taller is the instance vs prototype
                let rdwi = trimmed_instance.width as i32 - prototype.width as i32;
                let rdhi = trimmed_instance.height as i32 - prototype.height as i32;

                let _ = coder.encode_integer(IntProc::Iardw, rdwi);
                let _ = coder.encode_integer(IntProc::Iardh, rdhi);

                // Position offsets for aligning the reference within the target.
                // Per §6.4.11.3.2: GRDX = (RDWI/2) + RDXI, GRDY = (RDHI/2) + RDYI
                // Use the pre-computed alignment offsets from clustering
                let rdxi = orig_instance.refinement_dx;
                let rdyi = orig_instance.refinement_dy;

                let _ = coder.encode_integer(IntProc::Iardx, rdxi);
                let _ = coder.encode_integer(IntProc::Iardy, rdyi);

                // Compute GRDX/GRDY for the refinement region.
                // §6.4.11.3.2 / §6.3: GRREFERENCEDX = floor(RDW/2) + RDX. Rust's `/`
                // truncates toward zero, which differs from floor for negative RDW
                // (instance narrower/shorter than the prototype) and would shift the
                // reference by one pixel vs the decoder. Use floor division.
                let grdx = rdwi.div_euclid(2) + rdxi;
                let grdy = rdhi.div_euclid(2) + rdyi;

                // Encode the refinement region: pixel-by-pixel difference
                // between the trimmed instance and the prototype
                coder.encode_refinement_region(
                    &trimmed_instance,
                    prototype,
                    grdx,
                    grdy,
                    config.text_refine_template,
                    &grat,
                )?;

                // Do NOT reset the refinement (GR) contexts between symbol
                // instances. T.88 §6.4.11 codes each instance's refinement with the
                // generic refinement procedure using the text region's shared GR
                // statistics; jbig2dec carries them across instances, so resetting
                // here desynchronises every refinement after the first.
            }

            current_s += item.symbol_width - 1;
            idx += 1;
        }
        let _ = coder.encode_oob(IntProc::Iads);
    }

    coder.flush(true);
    payload.extend(coder.as_bytes());

    Ok(payload)
}

/// Encodes a text region segment to the output.
///
/// This function takes a list of symbols and their instances in the text region,
/// and encodes them according to JBIG2 spec §6.4.10. It supports both absolute coordinates
/// and IADW/IADH delta encoding for more efficient compression.
pub fn encode_text_region(
    instances: &[SymbolInstance],
    config: &Jbig2Config,
    all_known_symbols: &[&BitImage],
    global_dict_indices: &[usize],
    local_dict_indices: &[usize],
) -> Result<Vec<u8>> {
    // Validate instances
    if instances.is_empty() {
        return Err(anyhow!("No symbol instances provided for text region"));
    }

    // Validate global dictionary indices
    if global_dict_indices
        .iter()
        .any(|&idx| idx >= all_known_symbols.len())
    {
        return Err(anyhow!("Invalid global dictionary index in text region"));
    }

    // Validate local dictionary indices if provided
    if !local_dict_indices.is_empty() {
        if local_dict_indices
            .iter()
            .any(|&idx| idx >= all_known_symbols.len())
        {
            return Err(anyhow!("Invalid local dictionary index in text region"));
        }
    }

    // Validate each instance
    for (i, instance) in instances.iter().enumerate() {
        if instance.symbol_index >= all_known_symbols.len() {
            return Err(anyhow!(
                "Symbol instance {} references invalid symbol index {} (max {})",
                i,
                instance.symbol_index,
                all_known_symbols.len() - 1
            ));
        }

        let symbol = &all_known_symbols[instance.symbol_index];
        if instance.position.x as u64 + symbol.width as u64 > u32::MAX as u64
            || instance.position.y as u64 + symbol.height as u64 > u32::MAX as u64
        {
            return Err(anyhow!(
                "Symbol instance {} at position ({}, {}) would overflow 32-bit coordinates",
                i,
                instance.position.x,
                instance.position.y
            ));
        }
    }
    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0;
    let mut max_y_coord = 0;

    if instances.is_empty() {
        min_x = 0;
        min_y = 0;
    } else {
        for instance in instances {
            let pos = instance.position();
            let sym_idx_in_all_known_list = instance.symbol_index();
            let symbol_width = all_known_symbols[sym_idx_in_all_known_list].width as i32;
            let symbol_height = all_known_symbols[sym_idx_in_all_known_list].height as i32;

            min_x = min_x.min(pos.x as u32);
            min_y = min_y.min(pos.y as u32);
            max_x_coord = max_x_coord.max((pos.x as i32 + symbol_width) as u32);
            max_y_coord = max_y_coord.max((pos.y as i32 + symbol_height) as u32);
        }
    }

    let region_width = if max_x_coord > min_x {
        max_x_coord - min_x
    } else {
        0
    };
    let region_height = if max_y_coord > min_y {
        max_y_coord - min_y
    } else {
        0
    };

    let params = TextRegionParams {
        width: region_width,
        height: region_height,
        x: min_x,
        y: min_y,
        ds_offset: config.text_ds_offset,
        refine: config.text_refine,
        log_strips: config.text_log_strips,
        ref_corner: config.text_ref_corner,
        transposed: config.text_transposed,
        comb_op: config.text_comb_op,
        refine_template: config.text_refine_template,
    };
    if cfg!(debug_assertions) {
        trace!("encode_text_region: TextRegionParams details: {:?}", params);
    }
    // Write text-region header and number of instances (SBNUMINSTANCES).
    payload.extend(params.to_bytes());
    payload.extend_from_slice(&(instances.len() as u32).to_be_bytes());

    // Number of bits used by IAID symbol coding.
    let num_total_dict_symbols = (global_dict_indices.len() + local_dict_indices.len()) as u32;
    // T.88 §7.4.3.1.7 / §6.5.8.2.3 (SDHUFF=0): arithmetic text-region IAID uses
    // SBSYMCODELEN = ceil(log2(SBNUMSYMS)) with NO floor of 1. The formulas differ
    // only at SBNUMSYMS == 1, where the decoder reads zero ID bits; emitting a
    // spurious 1-bit IAID there desyncs the arithmetic decoder for every later
    // symbol (S-coordinate corruption). encode_iaid is skipped when this is 0.
    let symbol_id_bits = log2up(num_total_dict_symbols.max(1));

    #[derive(Clone, Copy)]
    struct EncodedInstance {
        strip_base: i32,
        x: i32,
        t_offset: i32,
        symbol_id: u32,
        symbol_width: i32,
    }

    let strip_width = 1i32 << params.log_strips.min(3);
    let mut encoded_instances = Vec::with_capacity(instances.len());

    for instance in instances {
        let sym_idx_in_all_known_list = instance.symbol_index();
        let symbol_props = &all_known_symbols[sym_idx_in_all_known_list];
        let symbol_id_to_encode = if let Some(pos_global) = global_dict_indices
            .iter()
            .position(|&idx| idx == sym_idx_in_all_known_list)
        {
            pos_global as u32
        } else if let Some(pos_local) = local_dict_indices
            .iter()
            .position(|&idx| idx == sym_idx_in_all_known_list)
        {
            (global_dict_indices.len() + pos_local) as u32
        } else {
            anyhow::bail!(
                "Symbol instance (index {}) not found in referred dictionaries!",
                sym_idx_in_all_known_list
            );
        };

        // REFCORNER=TOPLEFT (value 1): T is the top of the original bounding box.
        let abs = instance.position();
        let rel_x = abs.x as i32 - min_x as i32;
        let rel_y = abs.y as i32 - min_y as i32;
        let strip_base = (rel_y / strip_width) * strip_width;
        let t_offset = rel_y - strip_base;

        encoded_instances.push(EncodedInstance {
            strip_base,
            x: rel_x,
            t_offset,
            symbol_id: symbol_id_to_encode,
            symbol_width: symbol_props.width as i32,
        });
    }

    // Sort strip-wise (top to bottom), then left to right inside each strip.
    encoded_instances.sort_by_key(|e| (e.strip_base, e.x));

    let mut strip_t = 0i32;
    let mut first_s = 0i32;
    let mut idx = 0usize;

    // §6.4.5 step 1: initial STRIPT value (decoder reads one IADT before the loop)
    let _ = coder.encode_integer(IntProc::Iadt, 0);

    while idx < encoded_instances.len() {
        let current_strip = encoded_instances[idx].strip_base;
        let delta_t = current_strip - strip_t;
        let _ = coder.encode_integer(IntProc::Iadt, delta_t / strip_width);
        strip_t = current_strip;

        let mut first_symbol_in_strip = true;
        let mut current_s = 0i32;
        while idx < encoded_instances.len() && encoded_instances[idx].strip_base == current_strip {
            let item = encoded_instances[idx];
            if first_symbol_in_strip {
                let delta_fs = item.x - first_s;
                let _ = coder.encode_integer(IntProc::Iafs, delta_fs);
                first_s += delta_fs;
                current_s = first_s;
                first_symbol_in_strip = false;
            } else {
                let delta_s = item.x - current_s;
                let _ = coder.encode_integer(IntProc::Iads, delta_s);
                current_s += delta_s;
            }

            if strip_width > 1 {
                let _ = coder.encode_integer(IntProc::Iait, item.t_offset);
            }
            if symbol_id_bits > 0 {
                let _ = coder.encode_iaid(item.symbol_id, symbol_id_bits as u8);
            }
            current_s += item.symbol_width - 1;
            idx += 1;
        }
        let _ = coder.encode_oob(IntProc::Iads);
    }

    coder.flush(true);
    payload.extend(coder.as_bytes());

    Ok(payload)
}

// ── Union-Find helpers for symbol clustering ──────────────────────────

fn uf_find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]]; // path halving
        i = parent[i];
    }
    i
}

fn uf_union(parent: &mut [usize], rank: &mut [u32], a: usize, b: usize) {
    let ra = uf_find(parent, a);
    let rb = uf_find(parent, b);
    if ra == rb {
        return;
    }
    if rank[ra] < rank[rb] {
        parent[ra] = rb;
    } else if rank[ra] > rank[rb] {
        parent[rb] = ra;
    } else {
        parent[rb] = ra;
        rank[ra] += 1;
    }
}

fn compute_symbol_hash(symbol: &BitImage) -> u32 {
    let w = symbol.width as u32;
    let h = symbol.height as u32;
    (10 * h + 10000 * w) % 10000000
}

fn log2up(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let is_pow_of_2 = (v & (v - 1)) == 0;
    let mut r = 0;
    let mut val = v;
    while val > 1 {
        val >>= 1;
        r += 1;
    }
    r + if is_pow_of_2 { 0 } else { 1 }
}

/// Encodes a sequence of images as a JBIG2 document.
///
/// # Arguments
/// * `images` - A slice of 2D arrays containing the input images
/// * `config` - Configuration for the encoder
///
/// # Returns
/// A `Result` containing the encoded JBIG2 document as a byte vector if successful,
/// or an error if encoding fails.
pub fn encode_document(images: &[Array2<u8>], config: &Jbig2Config) -> Result<Vec<u8>> {
    let mut encoder = Jbig2Encoder::new(config);
    for image in images {
        encoder.add_page(image)?;
    }
    encoder.flush()
}

/// Represents a single symbol instance in a text region, with refinement info.
#[derive(Debug, Clone)]
pub struct TextRegionSymbolInstance {
    /// The ID of the symbol in the dictionary.
    pub symbol_id: u32,
    /// The x-coordinate of the instance's top-left corner.
    pub x: i32,
    /// The y-coordinate of the instance's top-left corner.
    pub y: i32,
    /// The horizontal refinement offset.
    pub dx: i32,
    /// The vertical refinement offset.
    pub dy: i32,
    /// Whether this instance is a refinement of a dictionary symbol.
    pub is_refinement: bool,
}

impl TextRegionSymbolInstance {
    /// Returns the position of this symbol instance as a Rect.
    pub fn position(&self) -> crate::jbig2sym::Rect {
        crate::jbig2sym::Rect {
            x: self.x as u32,
            y: self.y as u32,
            width: 0,  // These will be set by the caller
            height: 0, // These will be set by the caller
        }
    }

    /// Returns the symbol index for this instance.
    pub fn symbol_index(&self) -> usize {
        self.symbol_id as usize
    }

    /// Converts to a SymbolInstance
    pub fn to_symbol_instance(&self, symbol_bitmap: &BitImage) -> SymbolInstance {
        SymbolInstance {
            symbol_index: self.symbol_id as usize,
            position: self.position(),
            instance_bitmap: symbol_bitmap.clone(),
            needs_refinement: self.is_refinement,
            refinement_dx: self.dx,
            refinement_dy: self.dy,
        }
    }
}

pub fn build_dictionary_and_get_instances(
    symbols: &[(Rect, BitImage)],
    comparator: &mut Comparator,
) -> (Vec<BitImage>, Vec<TextRegionSymbolInstance>) {
    let mut dictionary_symbols: Vec<BitImage> = Vec::with_capacity(symbols.len());
    let mut dictionary_black_pixels = Vec::with_capacity(symbols.len());
    let mut instances = Vec::with_capacity(symbols.len());

    for (rect, symbol_image) in symbols.iter() {
        let mut found_match = false;
        // Use a 10% error threshold for matching, as recommended.
        let max_err = ((symbol_image.width * symbol_image.height) / 10).max(3) as u32;
        let symbol_black_pixels = symbol_image.count_ones();

        for (dict_idx, dict_symbol) in dictionary_symbols.iter().enumerate() {
            if symbol_image.width.abs_diff(dict_symbol.width) > MAX_DIMENSION_DELTA
                || symbol_image.height.abs_diff(dict_symbol.height) > MAX_DIMENSION_DELTA
            {
                continue;
            }

            if symbol_black_pixels.abs_diff(dictionary_black_pixels[dict_idx]) > max_err as usize {
                continue;
            }

            // Use a low max_err for finding near-duplicates
            if let Some((err, dx, dy)) = comparator.distance(symbol_image, dict_symbol, max_err) {
                instances.push(TextRegionSymbolInstance {
                    symbol_id: dict_idx as u32,
                    x: rect.x as i32,
                    y: rect.y as i32,
                    dx,
                    dy,
                    is_refinement: err > 0,
                });
                found_match = true;
                break;
            }
        }

        if !found_match {
            let new_idx = dictionary_symbols.len();
            dictionary_symbols.push(symbol_image.clone());
            dictionary_black_pixels.push(symbol_black_pixels);
            instances.push(TextRegionSymbolInstance {
                symbol_id: new_idx as u32,
                x: rect.x as i32,
                y: rect.y as i32,
                dx: 0,
                dy: 0,
                is_refinement: false,
            });
        }
    }

    (dictionary_symbols, instances)
}

/// Encodes a single page image using a symbol dictionary.
/// This is a high-level function that demonstrates the new encoding pipeline.
pub fn encode_page_with_symbol_dictionary(
    image: &BitImage,
    config: &Jbig2Config,
    next_segment_num: u32,
) -> Result<(Vec<u8>, u32)> {
    // 1. Extract symbols from the page image using CC analysis
    #[cfg(feature = "symboldict")]
    let extracted_symbols = {
        let dpi = 300; // Default DPI
        let losslevel = if config.is_lossless { 0 } else { 1 };
        let cc_image = analyze_page(image, dpi, losslevel);
        let shapes = cc_image.extract_shapes();
        // Convert to (Rect, BitImage) format
        shapes
            .into_iter()
            .map(|(bitmap, bbox)| {
                let rect = Rect {
                    x: bbox.xmin as u32,
                    y: bbox.ymin as u32,
                    width: bbox.width() as u32,
                    height: bbox.height() as u32,
                };
                (rect, bitmap)
            })
            .collect::<Vec<_>>()
    };
    #[cfg(not(feature = "symboldict"))]
    let extracted_symbols: Vec<(Rect, BitImage)> = Vec::new();

    if extracted_symbols.is_empty() {
        return Ok((Vec::new(), next_segment_num));
    }

    // 2. Build the symbol dictionary and get symbol instances
    let mut comparator = Comparator::default();
    let (dictionary_symbols, text_region_instances) =
        build_dictionary_and_get_instances(&extracted_symbols, &mut comparator);
    debug!(
        "Built dictionary with {} symbols and {} instances",
        dictionary_symbols.len(),
        text_region_instances.len()
    );

    let mut output = Vec::new();
    let mut current_segment_number = next_segment_num;

    // 3. Encode the symbol dictionary segment, getting the final symbol-ID mapping.
    let dict_refs: Vec<&BitImage> = dictionary_symbols.iter().collect();
    let dict_layout = plan_symbol_dictionary_layout(&dict_refs, config, None)?;
    let encoded_dict = encode_symbol_dictionary_segments(&dict_refs, config, &dict_layout)?;
    let dict_segment_number = current_segment_number;
    current_segment_number += 1;
    Segment {
        number: dict_segment_number,
        seg_type: SegmentType::SymbolDictionary,
        referred_to: Vec::new(),
        page: Some(1),
        payload: encoded_dict.payload.clone(),
        ..Default::default()
    }
    .write_into(&mut output)?;

    // 4. Encode the text region segment using canonical symbol IDs.
    let mut symbol_instances: Vec<SymbolInstance> = text_region_instances
        .iter()
        .map(|instance| {
            let orig_id = instance.symbol_id as usize;
            let symbol_bitmap = if orig_id < dictionary_symbols.len() {
                &dictionary_symbols[orig_id]
            } else {
                &dictionary_symbols[0]
            };
            SymbolInstance {
                symbol_index: orig_id,
                position: instance.position(),
                instance_bitmap: symbol_bitmap.clone(),
                needs_refinement: instance.is_refinement,
                refinement_dx: instance.dx,
                refinement_dy: instance.dy,
            }
        })
        .collect();

    for (orig_idx, refinement) in dict_layout.refinements.iter().enumerate() {
        if let Some(refinement) = refinement {
            for instance in &mut symbol_instances {
                if instance.symbol_index == orig_idx {
                    instance.symbol_index = refinement.prototype_input_index;
                    instance.needs_refinement = true;
                    instance.refinement_dx = refinement.refinement_dx;
                    instance.refinement_dy = refinement.refinement_dy;
                }
            }
        }
    }

    let region_payload = if !config.uses_lossy_symbol_dictionary()
        && (config.refine || symbol_instances.iter().any(|inst| inst.needs_refinement))
    {
        encode_text_region_with_refinement(
            &symbol_instances,
            config,
            &dictionary_symbols,
            &encoded_dict.input_to_exported_pos,
            encoded_dict.exported_symbol_count,
            &[],
            0,
        )?
    } else {
        encode_text_region_mapped(
            &symbol_instances,
            config,
            &dictionary_symbols,
            &encoded_dict.input_to_exported_pos,
            encoded_dict.exported_symbol_count,
            &[],
            0,
            0,
        )?
    };

    let region_segment = Segment {
        number: current_segment_number,
        seg_type: SegmentType::ImmediateTextRegion,
        retain_flags: 0,
        referred_to: vec![dict_segment_number],
        page: Some(1), // Assuming page 1
        payload: region_payload,
        ..Default::default()
    };

    // You might want to log text_region_params here too if they are accessible
    region_segment.write_into(&mut output)?;
    current_segment_number += 1;

    Ok((output, current_segment_number))
}

pub fn get_version() -> &'static str {
    "0.2.0"
}

#[inline]
pub fn hash_key(img: &BitImage) -> HashKey {
    // Dimension-based bucketing: symbols with similar dimensions land in the same
    // bucket, enabling fuzzy matching via the Comparator during extraction.
    // The Comparator handles size differences up to MAX_DIMENSION_DELTA (10px),
    // so we bucket by (height, width) to keep buckets tight.
    let h = img.height as u64;
    let w = img.width as u64;
    HashKey(h * 10_000 + w)
}

/// Helper function to find the first black pixel in the BitImage
/// Returns (x, y) coordinates of the first black pixel, or None if no black pixels
pub fn first_black_pixel(image: &BitImage) -> Option<(usize, usize)> {
    for y in 0..image.height {
        for x in 0..image.width {
            if image.get_usize(x, y) {
                return Some((x, y));
            }
        }
    }
    None
}

#[cfg(all(test, feature = "refine"))]
mod refine_tests {
    use super::*;

    fn symbol_from_rows(rows: &[&str]) -> BitImage {
        let height = rows.len() as u32;
        let width = rows.first().map_or(0, |row| row.len()) as u32;
        let mut image = BitImage::new(width, height).expect("test bitmap");
        for (y, row) in rows.iter().enumerate() {
            for (x, ch) in row.bytes().enumerate() {
                if ch == b'1' {
                    image.set(x as u32, y as u32, true);
                }
            }
        }
        image
    }

    #[test]
    fn refinement_layout_collapses_to_prototypes() {
        let base = symbol_from_rows(&["0110", "1001", "1111", "1001", "1001"]);
        let variant = symbol_from_rows(&["0110", "1001", "1111", "1001", "1001"]);
        let symbols = vec![&base, &variant];

        let mut config = Jbig2Config::text();
        config.refine = true;
        config.text_refine = false;

        let layout = plan_symbol_dictionary_layout(&symbols, &config, None).expect("layout");
        assert_eq!(layout.segment_count(), 1);
        assert_eq!(layout.export_input_indices.len(), 1);
        assert!(layout.refinements[1].is_some());

        let encoded =
            encode_symbol_dictionary_segments(&symbols, &config, &layout).expect("encode");
        assert_eq!(encoded.exported_symbol_count, 1);
        assert!(
            encoded
                .input_to_exported_pos
                .iter()
                .all(|&pos| pos != u32::MAX)
        );
    }
}
