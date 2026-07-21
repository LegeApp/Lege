use super::super::{
    Jbig2Encoder, SYM_UNIFY_STRONG_ANCHOR_MIN_PAGE_SPAN, SYM_UNIFY_STRONG_ANCHOR_MIN_USAGE,
};
use super::types::{SymUnifyAnchorCandidate, SymUnifyAnchorDecision};
use crate::jbig2classify::{
    FamilyBucketKey, SymbolSignature, compute_symbol_signature as compute_symbol_signature_shared,
    family_bucket_key_for_symbol, family_signatures_are_compatible,
};
use crate::jbig2comparator::Comparator;
use crate::jbig2structs::LossySymbolMode;
use crate::jbig2sym::BitImage;
use rustc_hash::FxHashMap;

impl<'a> Jbig2Encoder<'a> {
    pub(crate) fn compute_symbol_signature(img: &BitImage) -> SymbolSignature {
        compute_symbol_signature_shared(img)
    }

    pub(crate) fn signatures_are_compatible(
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

    pub(crate) fn should_skip_symbol_candidate(
        width: usize,
        height: usize,
        black_pixels: usize,
    ) -> bool {
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
    pub(crate) fn should_accept_match(
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
    pub(crate) fn symbol_unify_assignment_score(
        result: &crate::jbig2comparator::CompareResult,
    ) -> u32 {
        result
            .total_err
            .saturating_add(result.black_delta.saturating_mul(2))
            .saturating_add(result.outside_ink_err.saturating_mul(3))
            .saturating_add(((result.dx.abs() + result.dy.abs()) as u32).saturating_mul(3))
            .saturating_add((result.row_profile_err + result.col_profile_err) / 24)
    }

    pub(crate) fn sym_unify_context_rerank_cost(candidate: &BitImage, proto: &BitImage) -> u32 {
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

    pub(crate) fn sym_unify_anchor_candidate_is_better(
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

    pub(crate) fn maybe_update_best_sym_unify_anchor_candidate(
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
    pub(crate) fn sym_unify_anchor_ready(&self, symbol_index: usize, page_num: usize) -> bool {
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

    pub(crate) fn build_sym_unify_anchor_map(
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

    pub(crate) fn maybe_add_sym_unify_anchor(
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

    pub(crate) fn residual_symbol_matches_anchor(
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

    pub(crate) fn residual_symbol_anchor_decision(
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

    pub(crate) fn residual_symbol_accept_with_dim_limit(
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

    pub(crate) fn residual_symbol_accept_without_overlap_prescreen(
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

    pub(crate) fn residual_symbol_anchor_decision_without_overlap_prescreen(
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
    pub(crate) fn evaluate_symbol_match(
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
    pub(crate) fn evaluate_symbol_unify_anchor_match(
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
}
