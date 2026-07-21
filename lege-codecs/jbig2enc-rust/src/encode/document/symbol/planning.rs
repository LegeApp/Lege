// `plan_document` (~1565 lines), `build_planned_page` (~260 lines), and
// `validate_plan` decide every segment, dictionary, and text-region layout
// choice for the document. They are mutually recursive and share a large
// amount of context, so splitting their bodies across files would be far
// riskier than the line-count savings are worth — this file is the
// documented exception to the ~1000-line target called out in
// REFACTOR_JBIG2ENC.md.
use super::super::{
    Jbig2Encoder, SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN, SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE,
};
use super::dictionary::{encode_symbol_dictionary_segments, plan_symbol_dictionary_layout};
use super::text_region::encode_text_region_mapped;
use super::text_region_refine::encode_text_region_with_refinement;
use super::types::{
    BuiltPage, CounterfactualProbeStats, DetailedCompareProbeStats, EncodedSymbolDictionary,
    PlannedDocument, PlannedPage, PlannedPageLayout, RefinementPlan, ResidualReasonCode,
    ResidualReasonStats, ResidualShapeKind, ResidualSymbolTrace, SymUnifyAnchorDecision,
    anchor_map_dictionary_bytes, bitmap_proxy_bytes, classify_residual_shape,
    encoder_diagnostics_enabled, indexed_symbol_dictionary_bytes, record_counterfactual_probe,
    record_detailed_compare_probe, record_labeled_counterfactual_probe,
    relaxed_compare_probe_max_err, update_best_reject,
};
use crate::debug;
use crate::jbig2classify::{
    FamilyBucketKey, family_bucket_key_for_symbol, family_bucket_neighbors,
};
use crate::jbig2comparator::Comparator;
use crate::jbig2structs::{FileHeader, LossySymbolMode, PageInfo, Segment, SegmentType};
use crate::jbig2sym::BitImage;
use anyhow::Result;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

impl<'a> Jbig2Encoder<'a> {
    pub(crate) fn plan_document(&mut self, include_header: bool) -> Result<PlannedDocument> {
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

    pub(crate) fn build_planned_page(
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

    pub(crate) fn validate_plan(&self, plan: &PlannedDocument) -> Result<()> {
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
}
