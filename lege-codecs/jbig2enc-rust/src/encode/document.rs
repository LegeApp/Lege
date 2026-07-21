//! This module contains the main JBIG2 encoder logic.
//!
//! (Was `encode/jbig2enc/mod.rs`; renamed to `encode/document.rs` in the
//! Phase 0 restructure. Its `symbol` subdirectory stays a child module at
//! `encode/document/symbol/` — see Gap A.)
mod symbol;

#[allow(unused_imports)]
use symbol::dictionary::{encode_symbol_dictionary_segments, plan_symbol_dictionary_layout};
#[allow(unused_imports)]
use symbol::text_region::{
    compute_symbol_hash, log2up, symbol_id_from_dense_maps, uf_find, uf_union,
};
#[allow(unused_imports)]
use symbol::types::*;

// Re-exports preserving the original `crate::jbig2enc::*` public paths after
// the symbol-dictionary/text-region machinery moved into `symbol::*` submodules.
pub use symbol::dictionary::{
    TextRegionSymbolInstance, build_dictionary_and_get_instances, canonicalize_dict_symbols,
    encode_symbol_dict, encode_symbol_dict_with_order,
};
pub use symbol::extraction::segment_symbols;
pub use symbol::text_region::{
    encode_page_with_symbol_dictionary, encode_text_region, encode_text_region_mapped,
};
pub use symbol::text_region_refine::encode_text_region_with_refinement;
pub use symbol::types::SymbolInstance;

use crate::jbig2arith::Jbig2ArithCoder;
use crate::jbig2classify::{
    SymbolSignature, family_bucket_key_for_symbol, family_bucket_neighbors,
};
use crate::jbig2comparator::Comparator;
// Symbol extraction using CC analysis
#[cfg(feature = "symboldict")]
use crate::jbig2cc::analyze_page;
use crate::jbig2structs::{
    FileHeader, GenericRegionParams, Jbig2Config, LossySymbolMode, PageInfo, Segment, SegmentType,
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
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// A key type for hashing bitmaps efficiently
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashKey(u64);

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

const RECENT_SYMBOL_CACHE_CAP: usize = 64;
const SYM_UNIFY_EXACT_ANCHOR_BUDGET: usize = 32;
const SYM_UNIFY_NEIGHBOR_ANCHOR_BUDGET: usize = 16;
const SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE: usize = 8;
const SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN: usize = 4;

// Jbig2EncConfig has been removed. Use jbig2structs::Jbig2Config directly.

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

    pub fn add_page(&mut self, image: &Array2<u8>) -> Result<()> {
        let bitimage = crate::jbig2sym::array_to_bitimage(image);
        self.add_page_bitimage(bitimage)
    }

    // `add_page_bitimage` (~546 lines) is the per-page symbol-extraction and
    // matching loop. Its body threads a dozen mutable locals — `comparator`,
    // `debug_lines`, `cc_index`, `symbol_instances`, `instance_bitmap`,
    // `recent_cache`, `sym_unify_anchor_map`, and the `sym_unify_*` counters —
    // through three nested match searches (recent-cache, anchor, hash-bucket)
    // that share early-exit control flow (`matched`, labelled `break`s).
    // Splitting it into helper methods would mean passing all of that state
    // across function boundaries via a dozen `&mut` parameters or a new
    // carrier struct, for a bit-exact encoder where a subtle slip would only
    // surface as silently different output bytes. That risk is not worth the
    // line-count savings here — this is the second documented exception to
    // the ~1000-line target called out in REFACTOR_JBIG2ENC.md (alongside
    // `plan_document` in `symbol/planning.rs`). It must also stay in `mod.rs`
    // as a `pub fn`: external integration test crates call it directly on
    // `Jbig2Encoder`, and `mod symbol` is a private module, so relocating it
    // there would make it externally unreachable.
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
