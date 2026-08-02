// src/pcrd.rs
#![allow(dead_code)]

use core::cmp::Ordering;

/// Alpha PCRD (post-compression rate-distortion) support for a JPEG 2000 encoder.
///
/// This module assumes Tier-1 has already produced an embedded stream for each
/// code-block, with candidate truncation points at coding-pass boundaries.
/// Each pass contributes:
///   - incremental bytes
///   - cumulative bytes
///   - an estimated incremental distortion reduction
///
/// The module then:
///   1. builds cumulative R-D curves per code-block
///   2. prunes each curve to a monotone decreasing-slope hull
///   3. selects one truncation point per block for a target byte budget
///      using a global slope threshold (lambda)
///
/// This is intentionally an alpha implementation:
///   - no packet-header feedback yet
///   - no exact Annex-J distortion accounting yet
///   - no Tier-2 integration yet
///
/// But it is enough to:
///   - enforce monotone budget behavior
///   - select pass counts per code-block
///   - drive multi-layer cumulative truncation planning

/// Incremental pass record, typically adapted from Tier-1 output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawPassRecord {
    /// Coding-pass index within the code-block.
    pub pass_index: u16,
    /// Incremental bytes contributed by this pass alone.
    pub bytes: u32,
    /// Cumulative bytes through and including this pass.
    pub cumulative_bytes: u32,
    /// Estimated incremental distortion reduction from including this pass.
    pub distortion_delta: f64,
}

impl RawPassRecord {
    pub fn new(pass_index: u16, bytes: u32, cumulative_bytes: u32, distortion_delta: f64) -> Self {
        Self {
            pass_index,
            bytes,
            cumulative_bytes,
            distortion_delta,
        }
    }
}

/// One cumulative truncation point on a code-block R-D curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PcrdPoint {
    /// Number of passes retained from the front of the embedded stream.
    pub passes: u16,
    /// Cumulative bytes for those passes.
    pub bytes: u32,
    /// Cumulative distortion reduction achieved by those passes.
    pub distortion_reduction: f64,
    /// Incremental slope from previous retained point to this one.
    ///
    /// For point 0 (omit block), this is `INFINITY`.
    pub slope: f64,
}

impl PcrdPoint {
    pub fn omitted() -> Self {
        Self {
            passes: 0,
            bytes: 0,
            distortion_reduction: 0.0,
            slope: f64::INFINITY,
        }
    }
}

/// Cumulative R-D curve for one code-block.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeBlockPcrdCurve {
    pub block_id: usize,
    /// Cumulative truncation points.
    ///
    /// `points[0]` should always be the omitted-block point.
    pub points: Vec<PcrdPoint>,
}

impl CodeBlockPcrdCurve {
    pub fn is_empty(&self) -> bool {
        self.points.len() <= 1
    }

    pub fn max_bytes(&self) -> u32 {
        self.points.last().map(|p| p.bytes).unwrap_or(0)
    }
}

/// Chosen truncation point for one code-block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSelection {
    pub block_id: usize,
    /// Number of retained coding passes from the front of the stream.
    pub passes: u16,
}

impl BlockSelection {
    pub fn omitted(block_id: usize) -> Self {
        Self {
            block_id,
            passes: 0,
        }
    }
}

/// Cumulative selection across all code-blocks for one target budget.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerSelection {
    pub target_bytes: u32,
    pub actual_bytes: u32,
    pub lambda: f64,
    pub selections: Vec<BlockSelection>,
}

/// Cumulative target for one quality layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerBudget {
    pub layer_index: usize,
    /// Cumulative byte target up to and including this layer.
    pub target_bytes_cumulative: u32,
}

/// Optional richer statistics for debugging / tests.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionStats {
    pub total_bytes: u32,
    pub total_distortion_reduction: f64,
    pub chosen_points: Vec<PcrdPoint>,
}

/// Build the raw cumulative curve from per-pass data.
///
/// This performs basic validation:
/// - cumulative bytes must be nondecreasing
/// - per-pass bytes must match cumulative deltas
/// - distortion deltas must be nonnegative
pub fn build_raw_curve(
    block_id: usize,
    passes: &[RawPassRecord],
) -> Result<CodeBlockPcrdCurve, PcrdError> {
    let mut points = Vec::with_capacity(passes.len() + 1);
    points.push(PcrdPoint::omitted());

    let mut prev_cum_bytes = 0u32;
    let mut cum_distortion = 0.0f64;

    for pass in passes {
        if pass.cumulative_bytes < prev_cum_bytes {
            return Err(PcrdError::NonMonotoneCumulativeBytes {
                block_id,
                pass_index: pass.pass_index,
                previous: prev_cum_bytes,
                current: pass.cumulative_bytes,
            });
        }

        let expected_increment = pass.cumulative_bytes - prev_cum_bytes;
        if pass.bytes != expected_increment {
            return Err(PcrdError::InconsistentIncrementalBytes {
                block_id,
                pass_index: pass.pass_index,
                bytes: pass.bytes,
                expected: expected_increment,
            });
        }

        if !(pass.distortion_delta.is_finite()) || pass.distortion_delta < 0.0 {
            return Err(PcrdError::InvalidDistortionDelta {
                block_id,
                pass_index: pass.pass_index,
                distortion_delta: pass.distortion_delta,
            });
        }

        cum_distortion += pass.distortion_delta;
        points.push(PcrdPoint {
            passes: pass.pass_index + 1,
            bytes: pass.cumulative_bytes,
            distortion_reduction: cum_distortion,
            slope: 0.0,
        });

        prev_cum_bytes = pass.cumulative_bytes;
    }

    fill_slopes(&mut points)?;
    Ok(CodeBlockPcrdCurve { block_id, points })
}

/// Prune a raw curve to its monotone decreasing-slope hull.
///
/// Intuition:
/// - if a middle point produces a nondecreasing slope sequence, it is dominated
///   and should never be selected by global PCRD optimization
pub fn prune_to_convex_hull(curve: &CodeBlockPcrdCurve) -> Result<CodeBlockPcrdCurve, PcrdError> {
    if curve.points.is_empty() {
        return Err(PcrdError::EmptyCurve {
            block_id: curve.block_id,
        });
    }

    let mut hull: Vec<PcrdPoint> = Vec::with_capacity(curve.points.len());

    for point in &curve.points {
        hull.push(*point);

        while hull.len() >= 3 {
            let c = hull[hull.len() - 1];
            let b = hull[hull.len() - 2];
            let a = hull[hull.len() - 3];

            let slope_ab = rd_slope(a, b)?;
            let slope_bc = rd_slope(b, c)?;

            // Keep strictly decreasing slopes.
            if slope_bc >= slope_ab {
                hull.remove(hull.len() - 2);
            } else {
                break;
            }
        }
    }

    fill_slopes(&mut hull)?;
    Ok(CodeBlockPcrdCurve {
        block_id: curve.block_id,
        points: hull,
    })
}

/// Build then prune a curve in one call.
pub fn build_hull_curve(
    block_id: usize,
    passes: &[RawPassRecord],
) -> Result<CodeBlockPcrdCurve, PcrdError> {
    let raw = build_raw_curve(block_id, passes)?;
    prune_to_convex_hull(&raw)
}

/// Build hull curves for many blocks at once.
pub fn build_hull_curves<I>(blocks: I) -> Result<Vec<CodeBlockPcrdCurve>, PcrdError>
where
    I: IntoIterator<Item = (usize, Vec<RawPassRecord>)>,
{
    let mut out = Vec::new();
    for (block_id, passes) in blocks {
        out.push(build_hull_curve(block_id, &passes)?);
    }
    Ok(out)
}

/// Select truncation points for a target total byte budget.
///
/// This uses a global lambda search over the pruned hull curves.
/// The result is cumulative: one chosen point per block.
pub fn select_for_target_bytes(
    curves: &[CodeBlockPcrdCurve],
    target_bytes: u32,
) -> Result<LayerSelection, PcrdError> {
    if curves.is_empty() {
        return Ok(LayerSelection {
            target_bytes,
            actual_bytes: 0,
            lambda: 0.0,
            selections: Vec::new(),
        });
    }

    validate_curves(curves)?;

    let max_lambda = max_slope(curves)?;
    let mut lo = 0.0f64;
    let mut hi = if max_lambda.is_finite() && max_lambda > 0.0 {
        max_lambda
    } else {
        1.0
    };

    // Best known selection that does not exceed target.
    let mut best = evaluate_lambda(curves, hi)?;
    if best.actual_bytes > target_bytes {
        // If even very high lambda still overshoots, force all omitted.
        best = evaluate_lambda(curves, f64::INFINITY)?;
    }

    // Search for a maximal quality solution within budget.
    for _ in 0..48 {
        let mid = 0.5 * (lo + hi);
        let trial = evaluate_lambda(curves, mid)?;

        match trial.actual_bytes.cmp(&target_bytes) {
            Ordering::Greater => {
                // Too many bytes => require steeper slopes.
                lo = mid;
            }
            Ordering::Less | Ordering::Equal => {
                best = trial;
                hi = mid;
            }
        }
    }

    let mut selection = LayerSelection {
        target_bytes,
        actual_bytes: best.actual_bytes,
        lambda: best.lambda,
        selections: best.selections,
    };
    fill_remaining_budget(curves, &mut selection)?;
    Ok(selection)
}

/// Greedily add the best next valid hull point that fits the residual budget.
/// Lambda search identifies the global operating point; this discrete polish
/// avoids systematic undershoot when code-block pass boundaries are coarse.
fn fill_remaining_budget(
    curves: &[CodeBlockPcrdCurve],
    selection: &mut LayerSelection,
) -> Result<(), PcrdError> {
    loop {
        let remaining = selection
            .target_bytes
            .saturating_sub(selection.actual_bytes);
        let mut best: Option<(usize, PcrdPoint, f64)> = None;

        for (selection_index, (curve, chosen)) in
            curves.iter().zip(selection.selections.iter()).enumerate()
        {
            let point_index = curve
                .points
                .iter()
                .position(|point| point.passes == chosen.passes)
                .ok_or(PcrdError::InvalidSelectedPassCount {
                    block_id: curve.block_id,
                    passes: chosen.passes,
                })?;
            let Some(next) = curve.points.get(point_index + 1).copied() else {
                continue;
            };
            let current = curve.points[point_index];
            let extra_bytes = next.bytes - current.bytes;
            if extra_bytes > remaining {
                continue;
            }
            let benefit = next.distortion_reduction - current.distortion_reduction;
            let slope = if extra_bytes == 0 {
                f64::INFINITY
            } else {
                benefit / f64::from(extra_bytes)
            };
            if best.as_ref().is_none_or(|(best_index, _, best_slope)| {
                slope > *best_slope
                    || (slope == *best_slope && curve.block_id < curves[*best_index].block_id)
            }) {
                best = Some((selection_index, next, slope));
            }
        }

        let Some((selection_index, next, _)) = best else {
            break;
        };
        let curve = &curves[selection_index];
        let chosen = &mut selection.selections[selection_index];
        let previous = curve
            .points
            .iter()
            .find(|point| point.passes == chosen.passes)
            .ok_or(PcrdError::InvalidSelectedPassCount {
                block_id: curve.block_id,
                passes: chosen.passes,
            })?;
        selection.actual_bytes = selection
            .actual_bytes
            .checked_add(next.bytes - previous.bytes)
            .ok_or(PcrdError::TotalBytesOverflow)?;
        chosen.passes = next.passes;
    }
    Ok(())
}

/// Build cumulative selections for multiple layers.
///
/// Each budget is interpreted as cumulative bytes up to that layer.
pub fn build_layer_selections(
    curves: &[CodeBlockPcrdCurve],
    budgets: &[LayerBudget],
) -> Result<Vec<LayerSelection>, PcrdError> {
    let mut out = Vec::with_capacity(budgets.len());
    for budget in budgets {
        let mut sel = select_for_target_bytes(curves, budget.target_bytes_cumulative)?;
        sel.target_bytes = budget.target_bytes_cumulative;
        out.push(sel);
    }
    Ok(out)
}

/// Evaluate a fixed lambda and return both selected points and summary stats.
///
/// Selection rule:
/// - walk forward on each hull while point.slope >= lambda
/// - choose the deepest such point
pub fn evaluate_lambda(
    curves: &[CodeBlockPcrdCurve],
    lambda: f64,
) -> Result<LayerSelection, PcrdError> {
    validate_curves(curves)?;

    let mut actual_bytes = 0u32;
    let mut selections = Vec::with_capacity(curves.len());

    for curve in curves {
        let chosen = choose_block_point(curve, lambda)?;
        actual_bytes = actual_bytes
            .checked_add(chosen.bytes)
            .ok_or(PcrdError::TotalBytesOverflow)?;

        selections.push(BlockSelection {
            block_id: curve.block_id,
            passes: chosen.passes,
        });
    }

    Ok(LayerSelection {
        target_bytes: 0,
        actual_bytes,
        lambda,
        selections,
    })
}

/// Same as `evaluate_lambda`, but also returns chosen points and cumulative
/// distortion reduction for testing and diagnostics.
pub fn evaluate_lambda_with_stats(
    curves: &[CodeBlockPcrdCurve],
    lambda: f64,
) -> Result<SelectionStats, PcrdError> {
    validate_curves(curves)?;

    let mut total_bytes = 0u32;
    let mut total_distortion_reduction = 0.0f64;
    let mut chosen_points = Vec::with_capacity(curves.len());

    for curve in curves {
        let chosen = choose_block_point(curve, lambda)?;
        total_bytes = total_bytes
            .checked_add(chosen.bytes)
            .ok_or(PcrdError::TotalBytesOverflow)?;
        total_distortion_reduction += chosen.distortion_reduction;
        chosen_points.push(chosen);
    }

    Ok(SelectionStats {
        total_bytes,
        total_distortion_reduction,
        chosen_points,
    })
}

/// Choose the retained point for one block at a fixed lambda.
pub fn choose_block_point(curve: &CodeBlockPcrdCurve, lambda: f64) -> Result<PcrdPoint, PcrdError> {
    validate_curve(curve)?;

    // Higher lambda = stricter threshold = keep passes with slope >= lambda = fewer passes = smaller file
    let mut best = curve.points[0];
    for point in curve.points.iter().skip(1) {
        // Keep this pass if its slope is >= threshold
        if point.slope >= lambda {
            best = *point;
        } else {
            // Once we drop below threshold, stop
            break;
        }
    }
    Ok(best)
}

/// Turn cumulative block selections into per-layer incremental pass counts.
///
/// Example:
/// - layer 0 chooses 3 passes for block A
/// - layer 1 chooses 5 passes for block A
/// Then incremental contribution of layer 1 is 2 passes.
///
/// Output shape:
///   [layer][block] -> incremental passes contributed in that layer
pub fn cumulative_to_incremental_passes(
    cumulative: &[LayerSelection],
) -> Result<Vec<Vec<(usize, u16)>>, PcrdError> {
    if cumulative.is_empty() {
        return Ok(Vec::new());
    }

    let block_count = cumulative[0].selections.len();
    for (layer_idx, layer) in cumulative.iter().enumerate() {
        if layer.selections.len() != block_count {
            return Err(PcrdError::InconsistentSelectionBlockCounts {
                layer_index: layer_idx,
                expected: block_count,
                actual: layer.selections.len(),
            });
        }
    }

    let mut out = Vec::with_capacity(cumulative.len());
    let mut previous_passes = vec![0u16; block_count];

    for (layer_idx, layer) in cumulative.iter().enumerate() {
        let mut this_layer = Vec::with_capacity(block_count);

        for (i, selection) in layer.selections.iter().enumerate() {
            let prev = previous_passes[i];
            if selection.passes < prev {
                return Err(PcrdError::NonMonotoneLayerSelection {
                    layer_index: layer_idx,
                    block_id: selection.block_id,
                    previous_passes: prev,
                    current_passes: selection.passes,
                });
            }

            let delta = selection.passes - prev;
            previous_passes[i] = selection.passes;
            this_layer.push((selection.block_id, delta));
        }

        out.push(this_layer);
    }

    Ok(out)
}

fn validate_curves(curves: &[CodeBlockPcrdCurve]) -> Result<(), PcrdError> {
    for curve in curves {
        validate_curve(curve)?;
    }
    Ok(())
}

fn validate_curve(curve: &CodeBlockPcrdCurve) -> Result<(), PcrdError> {
    if curve.points.is_empty() {
        return Err(PcrdError::EmptyCurve {
            block_id: curve.block_id,
        });
    }

    let first = curve.points[0];
    if first.passes != 0 || first.bytes != 0 || first.distortion_reduction != 0.0 {
        return Err(PcrdError::InvalidOriginPoint {
            block_id: curve.block_id,
        });
    }

    let mut prev = first;
    for point in curve.points.iter().skip(1) {
        if point.passes <= prev.passes {
            return Err(PcrdError::NonMonotonePasses {
                block_id: curve.block_id,
                previous: prev.passes,
                current: point.passes,
            });
        }
        if point.bytes < prev.bytes {
            return Err(PcrdError::NonMonotonePointBytes {
                block_id: curve.block_id,
                previous: prev.bytes,
                current: point.bytes,
            });
        }
        if point.distortion_reduction < prev.distortion_reduction {
            return Err(PcrdError::NonMonotoneDistortion {
                block_id: curve.block_id,
                previous: prev.distortion_reduction,
                current: point.distortion_reduction,
            });
        }
        prev = *point;
    }

    Ok(())
}

fn fill_slopes(points: &mut [PcrdPoint]) -> Result<(), PcrdError> {
    if points.is_empty() {
        return Ok(());
    }

    points[0].slope = f64::INFINITY;
    for i in 1..points.len() {
        points[i].slope = rd_slope(points[i - 1], points[i])?;
    }
    Ok(())
}

fn rd_slope(a: PcrdPoint, b: PcrdPoint) -> Result<f64, PcrdError> {
    if b.bytes < a.bytes {
        return Err(PcrdError::NegativeByteDelta);
    }

    let db = (b.bytes - a.bytes) as f64;
    let dd = b.distortion_reduction - a.distortion_reduction;

    if db == 0.0 {
        // Equal-byte points are legal to detect but not useful for optimization.
        // Treat pure distortion improvement at zero byte cost as infinite slope.
        return Ok(if dd > 0.0 { f64::INFINITY } else { 0.0 });
    }

    Ok(dd / db)
}

fn max_slope(curves: &[CodeBlockPcrdCurve]) -> Result<f64, PcrdError> {
    let mut max_value = 0.0f64;
    for curve in curves {
        validate_curve(curve)?;
        for point in curve.points.iter().skip(1) {
            if point.slope.is_finite() {
                max_value = max_value.max(point.slope);
            }
        }
    }
    Ok(max_value)
}

// ---------------------------------------------------------------------------
// Distortion estimation — family of models
// ---------------------------------------------------------------------------

/// Coding pass type, used by pass-kind-aware distortion models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassKind {
    SignificancePropagation,
    MagnitudeRefinement,
    Cleanup,
}

/// Subband orientation, used by band-bias distortion models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandKind {
    Ll,
    Hl,
    Lh,
    Hh,
}

/// All context needed to estimate distortion for one coding pass.
///
/// Constructed in `rate.rs` from Tier-1 pass metadata and subband parameters.
#[derive(Debug, Clone, Copy)]
pub struct PassDistortionContext {
    pub pass_kind: PassKind,
    pub bitplane: u8,
    /// Newly-significant samples in this pass.
    pub newly_significant: usize,
    /// Already-significant samples (MR refinement count).
    pub refinement_samples: usize,
    /// Synthesis-norm² for the subband.
    pub subband_weight: f64,
    /// Scalar-expounded quantization step Δ. 1.0 for reversible 5/3.
    pub quant_step: f64,
    pub band_kind: BandKind,
    /// Quality setting (0-100) for quality-dependent weighting.
    pub quality: u8,
    /// Block classification for spatial-domain weighting.
    pub block_class: crate::profile::BlockClass,
    /// Contrast masking visibility weight (0.25-1.0).
    /// Lower = texture can hide errors = spend fewer bits.
    /// Higher = smooth/edges need quality = spend more bits.
    pub contrast_visibility_weight: f64,
    /// Taubman §VI subband masking weight (0.0–1.0).
    /// 1.0 = flat region (maximum perceptual cost per bit).
    /// ~0.0 = highly textured (masking hides errors, fewer bits needed).
    /// Only used when DistortionModel::Taubman2000 is active; defaults to 1.0.
    pub taubman_masking_weight: f64,
}

/// Distortion model selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DistortionModel {
    /// Quant-aware energy, 0.25 MR factor, flat band bias.
    BaselineAlpha,
    /// Separate SP/MR/CL weights + subband orientation bias.
    #[default]
    PassKindAware,
    /// Taubman 2000 §VI subband-domain visual masking.
    ///
    /// Requires `PassDistortionContext::taubman_masking_weight` to be populated
    /// from `TaubmanMaskMap::block_masking_multiplier`. Falls back to
    /// `PassKindAware` behavior when the weight is 1.0 (flat/unmasked).
    Taubman2000,
}

/// Dispatch to the selected distortion model.
pub fn estimate_pass_distortion_delta_with_model(
    ctx: &PassDistortionContext,
    model: DistortionModel,
) -> f64 {
    match model {
        DistortionModel::BaselineAlpha => estimate_pass_distortion_delta_baseline(ctx),
        DistortionModel::PassKindAware => estimate_pass_distortion_delta_pass_kind_aware(ctx),
        DistortionModel::Taubman2000 => estimate_pass_distortion_delta_taubman2000(ctx),
    }
}

/// Baseline model — identical to the original `estimate_pass_distortion_delta`.
///
/// Energy = (Δ·2^b)². MR factor = 0.25 from E[(±Δ_b/2)²] = energy/4 per unknown bit.
pub fn estimate_pass_distortion_delta_baseline(ctx: &PassDistortionContext) -> f64 {
    let plane_weight = (1u64 << ctx.bitplane.min(30)) as f64 * ctx.quant_step;
    let energy = plane_weight * plane_weight;
    let sig_term = ctx.newly_significant as f64 * energy;
    let ref_term = ctx.refinement_samples as f64 * energy * 0.25;
    ctx.subband_weight * (sig_term + ref_term)
}

/// Pass-kind-aware model with quality-dependent and spatial weighting.
///
/// Applies separate weights per pass kind and combines:
/// - **Band bias** (quality-dependent): Preserves LL at low quality, balanced at high quality
/// - **Block classification**: Preserves flat areas, allows compression in textured areas
///
/// SP gets the full energy allocation; MR uses 0.25 (= energy/4, the expected
/// squared error from one unknown refinement bit ±Δ_b/2); CL uses 0.85 because
/// it encodes residual context bits after the significance pass.
pub fn estimate_pass_distortion_delta_pass_kind_aware(ctx: &PassDistortionContext) -> f64 {
    let plane_weight = (1u64 << ctx.bitplane.min(30)) as f64 * ctx.quant_step;
    let energy = plane_weight * plane_weight;

    let pass_contribution = match ctx.pass_kind {
        PassKind::SignificancePropagation => ctx.newly_significant as f64 * energy,
        PassKind::MagnitudeRefinement => ctx.refinement_samples as f64 * energy * 0.25,
        PassKind::Cleanup => ctx.newly_significant as f64 * energy * 0.85,
    };

    // Combined weighting strategy:
    // - subband_weight: Synthesis norm (frequency-domain energy)
    // - band_bias: Quality-dependent LL/HH balancing
    // - block_class_weight: Spatial-domain content awareness
    // - contrast_masking: Perceptual masking based on local texture
    let band_bias = band_distortion_bias(ctx.band_kind, ctx.quality);
    let is_ll = matches!(ctx.band_kind, BandKind::Ll);
    let block_weight = crate::profile::class_distortion_weight(ctx.block_class, is_ll, 0);

    let raw_delta = ctx.subband_weight * band_bias * block_weight * pass_contribution;

    apply_contrast_masking_to_delta(
        raw_delta,
        ctx.contrast_visibility_weight,
        ctx.block_class,
        ctx.quality,
    )
}

/// Taubman 2000 §VI perceptual masking model.
///
/// Replaces the image-domain contrast masker with a subband-domain masker.
/// The distortion weight per block is scaled by `taubman_masking_weight`,
/// which must be pre-computed from `TaubmanMaskMap::block_masking_multiplier`.
///
/// Formula: ΔD_vis = subband_weight · taubman_masking_weight · pass_contribution
/// where `taubman_masking_weight` ∈ (0,1] encodes mean(1/(ν²+f²)) normalized
/// to 1.0 at the flat-cell floor ([T2000] §VI eqs. 4–5).
pub fn estimate_pass_distortion_delta_taubman2000(ctx: &PassDistortionContext) -> f64 {
    let plane_weight = (1u64 << ctx.bitplane.min(30)) as f64 * ctx.quant_step;
    let energy = plane_weight * plane_weight;

    let pass_contribution = match ctx.pass_kind {
        PassKind::SignificancePropagation => ctx.newly_significant as f64 * energy,
        PassKind::MagnitudeRefinement => ctx.refinement_samples as f64 * energy * 0.25,
        PassKind::Cleanup => ctx.newly_significant as f64 * energy * 0.85,
    };

    ctx.subband_weight * ctx.taubman_masking_weight * pass_contribution
}

/// Subband orientation bias for the pass-kind-aware model.
///
/// LL carries low-frequency energy and is perceptually most important.
/// HH diagonals are least salient.
///
/// **Quality-dependent strategy:**
/// - At low quality (q<50): Aggressively preserve LL structure, discard HH detail
/// - At high quality (q≥50): Mild bias to maintain balanced detail
pub fn band_distortion_bias(band: BandKind, quality: u8) -> f64 {
    if quality < 50 {
        // Low quality: smoothly interpolate between aggressive structure-only at
        // q=0 and the q=50 boundary (which matches the q≥50 branch below).
        // q=0:  LL=3.0, HL/LH=1.0, HH=0.20  (structure-only, drop diagonals)
        // q=49: LL≈1.22, HL/LH=1.0, HH≈0.81 (continuous with q≥50 branch)
        let t = quality as f64 / 50.0;
        match band {
            BandKind::Ll => 3.0 - 1.8 * t,
            BandKind::Hl | BandKind::Lh => 1.0,
            BandKind::Hh => 0.20 + 0.62 * t,
        }
    } else {
        // High quality: mild bias (original behavior, unchanged)
        match band {
            BandKind::Ll => 1.20,
            BandKind::Hl | BandKind::Lh => 1.00,
            BandKind::Hh => 0.82,
        }
    }
}

/// Apply contrast masking to distortion delta based on block class and quality.
///
/// This implements the visibility weight multiplier from the Ponomarenko et al.
/// contrast masking model. Textured areas can hide compression artifacts, so we
/// reduce their distortion value (spend fewer bits). Smooth areas and edges need
/// quality, so we preserve their distortion value.
///
/// Class-specific strength prevents damaging text/line art:
/// - EdgeText: minimal masking (0.15)
/// - Flat/Gradient: minimal masking (0.10-0.20)
/// - TexturePhoto: strong masking (1.00)
/// - BackgroundNoise: strongest masking (1.20)
///
/// Masking is stronger at low quality and weaker at high quality.
pub fn apply_contrast_masking_to_delta(
    raw_delta: f64,
    contrast_visibility_weight: f64,
    block_class: crate::profile::BlockClass,
    quality: u8,
) -> f64 {
    use crate::profile::BlockClass;

    if raw_delta <= 0.0 {
        return 0.0;
    }

    // Stronger at low quality, weaker at high quality.
    let q = quality as f64 / 100.0;
    let low_rate_strength = (1.0 - q).clamp(0.0, 1.0);

    let class_strength = match block_class {
        // Do not let masking erase text/line art.
        BlockClass::EdgeText => 0.15,

        // Flat/gradient errors are visible as blotches/banding.
        BlockClass::Flat => 0.10,
        BlockClass::Gradient => 0.20,

        // True texture/photo can hide more.
        BlockClass::TexturePhoto => 1.00,

        // Paper/noise can be sacrificed aggressively.
        BlockClass::BackgroundNoise => 1.20,
    };

    let effective_strength = low_rate_strength * class_strength;

    // Blend between no masking and full masking.
    let blended_weight = 1.0 + effective_strength * (contrast_visibility_weight - 1.0);

    raw_delta * blended_weight.clamp(0.20, 1.0)
}

/// Debug breakdown of one distortion estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct PassDistortionExplanation {
    pub model: DistortionModel,
    pub energy_per_sample: f64,
    pub effective_samples: f64,
    pub band_bias: f64,
    pub subband_weight: f64,
    pub total: f64,
}

/// Return a full breakdown of the distortion estimate for one pass.
///
/// Useful for unit tests and offline calibration — not called in the hot path.
pub fn explain_pass_distortion(
    ctx: &PassDistortionContext,
    model: DistortionModel,
) -> PassDistortionExplanation {
    let plane_weight = (1u64 << ctx.bitplane.min(30)) as f64 * ctx.quant_step;
    let energy_per_sample = plane_weight * plane_weight;
    let band_bias = match model {
        DistortionModel::PassKindAware => band_distortion_bias(ctx.band_kind, ctx.quality),
        DistortionModel::BaselineAlpha | DistortionModel::Taubman2000 => 1.0,
    };
    let effective_samples = match (model, ctx.pass_kind) {
        (_, PassKind::MagnitudeRefinement) => ctx.refinement_samples as f64 * 0.25,
        (DistortionModel::PassKindAware | DistortionModel::Taubman2000, PassKind::Cleanup) => {
            ctx.newly_significant as f64 * 0.85
        }
        _ => ctx.newly_significant as f64,
    };
    let masking = match model {
        DistortionModel::Taubman2000 => ctx.taubman_masking_weight,
        _ => 1.0,
    };
    let total = ctx.subband_weight * band_bias * masking * effective_samples * energy_per_sample;
    PassDistortionExplanation {
        model,
        energy_per_sample,
        effective_samples,
        band_bias,
        subband_weight: ctx.subband_weight,
        total,
    }
}

/// Suggested weight hook for subbands in an alpha encoder.
///
/// For now this is intentionally conservative.
/// You can later replace it with synthesis-norm or perceptual weighting.
pub fn default_subband_weight(resolution: u8, is_ll: bool, is_hh: bool) -> f64 {
    let mut w = 1.0;

    if is_ll {
        w *= 1.15;
    }
    if is_hh {
        w *= 0.90;
    }
    if resolution == 0 {
        w *= 1.10;
    }

    w
}

#[derive(Debug, Clone, PartialEq)]
pub enum PcrdError {
    EmptyCurve {
        block_id: usize,
    },
    InvalidOriginPoint {
        block_id: usize,
    },
    NonMonotoneCumulativeBytes {
        block_id: usize,
        pass_index: u16,
        previous: u32,
        current: u32,
    },
    InconsistentIncrementalBytes {
        block_id: usize,
        pass_index: u16,
        bytes: u32,
        expected: u32,
    },
    InvalidDistortionDelta {
        block_id: usize,
        pass_index: u16,
        distortion_delta: f64,
    },
    NonMonotonePasses {
        block_id: usize,
        previous: u16,
        current: u16,
    },
    NonMonotonePointBytes {
        block_id: usize,
        previous: u32,
        current: u32,
    },
    NonMonotoneDistortion {
        block_id: usize,
        previous: f64,
        current: f64,
    },
    NegativeByteDelta,
    TotalBytesOverflow,
    InvalidSelectedPassCount {
        block_id: usize,
        passes: u16,
    },
    InconsistentSelectionBlockCounts {
        layer_index: usize,
        expected: usize,
        actual: usize,
    },
    NonMonotoneLayerSelection {
        layer_index: usize,
        block_id: usize,
        previous_passes: u16,
        current_passes: u16,
    },
}

impl core::fmt::Display for PcrdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyCurve { block_id } => {
                write!(f, "PCRD curve for block {} is empty", block_id)
            }
            Self::InvalidOriginPoint { block_id } => {
                write!(
                    f,
                    "PCRD curve for block {} has invalid origin point",
                    block_id
                )
            }
            Self::NonMonotoneCumulativeBytes {
                block_id,
                pass_index,
                previous,
                current,
            } => write!(
                f,
                "block {} pass {} has non-monotone cumulative bytes: {} -> {}",
                block_id, pass_index, previous, current
            ),
            Self::InconsistentIncrementalBytes {
                block_id,
                pass_index,
                bytes,
                expected,
            } => write!(
                f,
                "block {} pass {} has inconsistent incremental bytes: got {}, expected {}",
                block_id, pass_index, bytes, expected
            ),
            Self::InvalidDistortionDelta {
                block_id,
                pass_index,
                distortion_delta,
            } => write!(
                f,
                "block {} pass {} has invalid distortion delta {}",
                block_id, pass_index, distortion_delta
            ),
            Self::NonMonotonePasses {
                block_id,
                previous,
                current,
            } => write!(
                f,
                "block {} has non-monotone pass counts: {} -> {}",
                block_id, previous, current
            ),
            Self::NonMonotonePointBytes {
                block_id,
                previous,
                current,
            } => write!(
                f,
                "block {} has non-monotone point bytes: {} -> {}",
                block_id, previous, current
            ),
            Self::NonMonotoneDistortion {
                block_id,
                previous,
                current,
            } => write!(
                f,
                "block {} has non-monotone distortion reduction: {} -> {}",
                block_id, previous, current
            ),
            Self::NegativeByteDelta => write!(f, "PCRD point had negative byte delta"),
            Self::TotalBytesOverflow => write!(f, "PCRD total byte count overflowed u32"),
            Self::InvalidSelectedPassCount { block_id, passes } => write!(
                f,
                "PCRD selection for block {block_id} references absent pass count {passes}"
            ),
            Self::InconsistentSelectionBlockCounts {
                layer_index,
                expected,
                actual,
            } => write!(
                f,
                "layer {} has inconsistent block count: expected {}, got {}",
                layer_index, expected, actual
            ),
            Self::NonMonotoneLayerSelection {
                layer_index,
                block_id,
                previous_passes,
                current_passes,
            } => write!(
                f,
                "layer {} block {} regressed in cumulative passes: {} -> {}",
                layer_index, block_id, previous_passes, current_passes
            ),
        }
    }
}

impl std::error::Error for PcrdError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_curve(block_id: usize) -> CodeBlockPcrdCurve {
        // Incremental bytes: 10, 10, 10, 10
        // Incremental distortion: 100, 50, 20, 5
        // Slopes: 10, 5, 2, 0.5
        build_hull_curve(
            block_id,
            &[
                RawPassRecord::new(0, 10, 10, 100.0),
                RawPassRecord::new(1, 10, 20, 50.0),
                RawPassRecord::new(2, 10, 30, 20.0),
                RawPassRecord::new(3, 10, 40, 5.0),
            ],
        )
        .expect("build sample curve")
    }

    #[test]
    fn build_raw_curve_includes_omitted_origin() {
        let curve = build_raw_curve(
            7,
            &[
                RawPassRecord::new(0, 4, 4, 9.0),
                RawPassRecord::new(1, 3, 7, 4.0),
            ],
        )
        .expect("build curve");

        assert_eq!(curve.points.len(), 3);
        assert_eq!(curve.points[0].passes, 0);
        assert_eq!(curve.points[0].bytes, 0);
        assert_eq!(curve.points[1].passes, 1);
        assert_eq!(curve.points[1].bytes, 4);
        assert_eq!(curve.points[2].passes, 2);
        assert_eq!(curve.points[2].bytes, 7);
        assert!((curve.points[2].distortion_reduction - 13.0).abs() < 1e-9);
    }

    #[test]
    fn hull_prunes_non_convex_middle_points() {
        // Slopes: 10, then 12, then 1 => middle should be pruned.
        let raw = build_raw_curve(
            1,
            &[
                RawPassRecord::new(0, 10, 10, 100.0),
                RawPassRecord::new(1, 10, 20, 120.0),
                RawPassRecord::new(2, 10, 30, 10.0),
            ],
        )
        .expect("build raw");

        let hull = prune_to_convex_hull(&raw).expect("prune hull");
        assert!(hull.points.len() < raw.points.len());

        for i in 2..hull.points.len() {
            assert!(hull.points[i].slope < hull.points[i - 1].slope);
        }
    }

    #[test]
    fn choose_block_point_obeys_lambda_threshold() {
        let curve = sample_curve(0);

        let p0 = choose_block_point(&curve, 20.0).expect("lambda 20");
        let p1 = choose_block_point(&curve, 6.0).expect("lambda 6");
        let p2 = choose_block_point(&curve, 3.0).expect("lambda 3");
        let p3 = choose_block_point(&curve, 0.75).expect("lambda 0.75");
        let p4 = choose_block_point(&curve, 0.1).expect("lambda 0.1");

        assert_eq!(p0.passes, 0);
        assert_eq!(p1.passes, 1);
        assert_eq!(p2.passes, 2);
        assert_eq!(p3.passes, 3);
        assert_eq!(p4.passes, 4);
    }

    #[test]
    fn evaluate_lambda_is_monotone_in_total_bytes() {
        let curves = vec![
            sample_curve(0),
            sample_curve(1),
            build_hull_curve(
                2,
                &[
                    RawPassRecord::new(0, 8, 8, 64.0),
                    RawPassRecord::new(1, 8, 16, 16.0),
                    RawPassRecord::new(2, 8, 24, 4.0),
                ],
            )
            .expect("curve 2"),
        ];

        let hi = evaluate_lambda(&curves, 100.0).expect("high lambda");
        let mid = evaluate_lambda(&curves, 4.0).expect("mid lambda");
        let lo = evaluate_lambda(&curves, 0.1).expect("low lambda");

        assert!(hi.actual_bytes <= mid.actual_bytes);
        assert!(mid.actual_bytes <= lo.actual_bytes);
    }

    #[test]
    fn target_byte_selection_is_monotone() {
        let curves = vec![sample_curve(0), sample_curve(1)];

        let a = select_for_target_bytes(&curves, 10).expect("target 10");
        let b = select_for_target_bytes(&curves, 30).expect("target 30");
        let c = select_for_target_bytes(&curves, 80).expect("target 80");

        assert!(a.actual_bytes <= b.actual_bytes);
        assert!(b.actual_bytes <= c.actual_bytes);
        assert_eq!(
            b.actual_bytes, 30,
            "discrete fill should use the residual budget"
        );

        for i in 0..a.selections.len() {
            assert!(a.selections[i].passes <= b.selections[i].passes);
            assert!(b.selections[i].passes <= c.selections[i].passes);
        }
    }

    #[test]
    fn cumulative_to_incremental_passes_works() {
        let layers = vec![
            LayerSelection {
                target_bytes: 10,
                actual_bytes: 10,
                lambda: 1.0,
                selections: vec![
                    BlockSelection {
                        block_id: 0,
                        passes: 1,
                    },
                    BlockSelection {
                        block_id: 1,
                        passes: 0,
                    },
                ],
            },
            LayerSelection {
                target_bytes: 20,
                actual_bytes: 20,
                lambda: 0.8,
                selections: vec![
                    BlockSelection {
                        block_id: 0,
                        passes: 3,
                    },
                    BlockSelection {
                        block_id: 1,
                        passes: 1,
                    },
                ],
            },
            LayerSelection {
                target_bytes: 30,
                actual_bytes: 30,
                lambda: 0.4,
                selections: vec![
                    BlockSelection {
                        block_id: 0,
                        passes: 3,
                    },
                    BlockSelection {
                        block_id: 1,
                        passes: 2,
                    },
                ],
            },
        ];

        let inc = cumulative_to_incremental_passes(&layers).expect("incremental");
        assert_eq!(inc.len(), 3);
        assert_eq!(inc[0], vec![(0, 1), (1, 0)]);
        assert_eq!(inc[1], vec![(0, 2), (1, 1)]);
        assert_eq!(inc[2], vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn distortion_prefers_higher_bitplanes() {
        let ctx_low = PassDistortionContext {
            pass_kind: PassKind::SignificancePropagation,
            bitplane: 2,
            newly_significant: 3,
            refinement_samples: 0,
            subband_weight: 1.0,
            quant_step: 1.0,
            band_kind: BandKind::Ll,
            quality: 75,
            block_class: crate::profile::BlockClass::TexturePhoto,
            contrast_visibility_weight: 1.0,
            taubman_masking_weight: 1.0,
        };
        let ctx_high = PassDistortionContext {
            bitplane: 5,
            ..ctx_low
        };
        let low =
            estimate_pass_distortion_delta_with_model(&ctx_low, DistortionModel::PassKindAware);
        let high =
            estimate_pass_distortion_delta_with_model(&ctx_high, DistortionModel::PassKindAware);
        assert!(high > low);
    }

    // ----- Step 7: failure-oriented tests -----

    #[test]
    fn build_raw_curve_rejects_nonmonotone_cumulative_bytes() {
        let err = build_raw_curve(
            0,
            &[
                RawPassRecord::new(0, 10, 10, 5.0),
                RawPassRecord::new(1, 5, 8, 3.0), // cumulative drops: 10 → 8
            ],
        );
        assert!(
            matches!(err, Err(PcrdError::NonMonotoneCumulativeBytes { .. })),
            "expected NonMonotoneCumulativeBytes, got {:?}",
            err
        );
    }

    #[test]
    fn build_raw_curve_rejects_inconsistent_incremental_bytes() {
        let err = build_raw_curve(
            0,
            &[
                RawPassRecord::new(0, 10, 10, 5.0),
                // incremental says 3 but cumulative went from 10 to 20 (+10)
                RawPassRecord::new(1, 3, 20, 3.0),
            ],
        );
        assert!(
            matches!(err, Err(PcrdError::InconsistentIncrementalBytes { .. })),
            "expected InconsistentIncrementalBytes, got {:?}",
            err
        );
    }

    #[test]
    fn build_raw_curve_rejects_negative_distortion_delta() {
        let err = build_raw_curve(0, &[RawPassRecord::new(0, 10, 10, -1.0)]);
        assert!(
            matches!(err, Err(PcrdError::InvalidDistortionDelta { .. })),
            "expected InvalidDistortionDelta, got {:?}",
            err
        );
    }

    #[test]
    fn validate_curve_rejects_bad_origin() {
        let curve = CodeBlockPcrdCurve {
            block_id: 0,
            points: vec![
                PcrdPoint {
                    passes: 1,
                    bytes: 0,
                    distortion_reduction: 0.0,
                    slope: f64::INFINITY,
                },
                PcrdPoint {
                    passes: 2,
                    bytes: 10,
                    distortion_reduction: 5.0,
                    slope: 0.5,
                },
            ],
        };
        let err = choose_block_point(&curve, 0.1);
        assert!(
            matches!(err, Err(PcrdError::InvalidOriginPoint { .. })),
            "expected InvalidOriginPoint, got {:?}",
            err
        );
    }

    #[test]
    fn hull_dominates_equal_slope_middle_points() {
        // Passes 0→1 and 1→2 have the same slope (100/10 = 10.0 each).
        // The middle individual point should be pruned, leaving fewer hull points
        // than the raw curve, all with strictly decreasing slopes.
        let raw = build_raw_curve(
            0,
            &[
                RawPassRecord::new(0, 10, 10, 100.0),
                RawPassRecord::new(1, 10, 20, 100.0),
                RawPassRecord::new(2, 10, 30, 20.0),
            ],
        )
        .expect("build raw");
        let hull = prune_to_convex_hull(&raw).expect("prune hull");
        assert!(
            hull.points.len() < raw.points.len(),
            "hull should be shorter than raw: hull={}, raw={}",
            hull.points.len(),
            raw.points.len()
        );
        // Slopes after origin must be strictly decreasing on the hull.
        for pair in hull.points.windows(2).skip(1) {
            assert!(
                pair[1].slope < pair[0].slope,
                "non-strictly-decreasing slopes: {} >= {}",
                pair[1].slope,
                pair[0].slope
            );
        }
    }

    #[test]
    fn evaluate_lambda_monotone_in_quality() {
        let curves = vec![sample_curve(0), sample_curve(1)];
        let hi_lambda = evaluate_lambda(&curves, 50.0).expect("high lambda");
        let lo_lambda = evaluate_lambda(&curves, 1.0).expect("low lambda");
        // Higher lambda → fewer passes → fewer bytes.
        assert!(
            hi_lambda.actual_bytes <= lo_lambda.actual_bytes,
            "monotonicity broken: {} > {}",
            hi_lambda.actual_bytes,
            lo_lambda.actual_bytes
        );
    }

    #[test]
    fn cumulative_to_incremental_rejects_pass_regression() {
        let layers = vec![
            LayerSelection {
                target_bytes: 10,
                actual_bytes: 10,
                lambda: 1.0,
                selections: vec![BlockSelection {
                    block_id: 0,
                    passes: 3,
                }],
            },
            LayerSelection {
                target_bytes: 20,
                actual_bytes: 20,
                lambda: 0.5,
                // Regresses from 3 → 1, which is illegal.
                selections: vec![BlockSelection {
                    block_id: 0,
                    passes: 1,
                }],
            },
        ];
        let err = cumulative_to_incremental_passes(&layers);
        assert!(
            matches!(err, Err(PcrdError::NonMonotoneLayerSelection { .. })),
            "expected NonMonotoneLayerSelection, got {:?}",
            err
        );
    }

    #[test]
    fn distortion_models_agree_on_sp_passes() {
        let ctx = PassDistortionContext {
            pass_kind: PassKind::SignificancePropagation,
            bitplane: 4,
            newly_significant: 8,
            refinement_samples: 0,
            subband_weight: 1.5,
            quant_step: 1.0,
            band_kind: BandKind::Ll,
            quality: 75,
            block_class: crate::profile::BlockClass::TexturePhoto,
            contrast_visibility_weight: 1.0,
            taubman_masking_weight: 1.0,
        };
        let baseline = estimate_pass_distortion_delta_baseline(&ctx);
        let pka = estimate_pass_distortion_delta_pass_kind_aware(&ctx);
        // PassKindAware applies band_bias + block_class weighting, so pka > baseline.
        assert!(
            pka > baseline,
            "PassKindAware should apply LL bias: {} vs {}",
            pka,
            baseline
        );
    }

    #[test]
    fn explain_pass_distortion_matches_with_model() {
        let ctx = PassDistortionContext {
            pass_kind: PassKind::Cleanup,
            bitplane: 3,
            newly_significant: 5,
            refinement_samples: 0,
            subband_weight: 1.0,
            quant_step: 2.0,
            band_kind: BandKind::Hh,
            quality: 75,
            block_class: crate::profile::BlockClass::TexturePhoto,
            contrast_visibility_weight: 1.0,
            taubman_masking_weight: 1.0,
        };
        for model in [
            DistortionModel::BaselineAlpha,
            DistortionModel::PassKindAware,
        ] {
            let direct = estimate_pass_distortion_delta_with_model(&ctx, model);
            let explained = explain_pass_distortion(&ctx, model);
            assert!(
                (explained.total - direct).abs() < 1e-10,
                "explain total mismatch for {:?}: {} vs {}",
                model,
                explained.total,
                direct
            );
        }
    }
}

/// Map quality setting (0-100) to lambda threshold for PCRD pass selection.
///
/// Uses a log-scale curve spanning ~3 orders of magnitude. q=99 is calibrated
/// to the perceptually lossless point (no visible artifacts on photographic
/// content); q=100 is reserved for mathematical losslessness (5/3 + RCT).
///
/// Calibration reference for 8-bit photographic content (4–5 DWT levels):
///   q=1  -> λ≈3.2e6 (absolute floor: heavy blocking, barely recognizable)
///   q=10 -> λ≈1.3e6 (extreme compression, heavy artifacts)
///   q=25 -> λ≈3.8e5 (heavy compression, clear artifacts)
///   q=50 -> λ≈1.5e5 (moderate compression, visible artifacts)
///   q=75 -> λ≈2.5e4 (light compression, minor artifacts)
///   q=90 -> λ≈7.5e3 (very light compression)
///   q=99 -> λ≈3e3   (perceptually lossless — no visible artifacts)
///   q=100 -> λ=0    (mathematically lossless, handled separately)
///
/// Ceiling anchored at 6.5 (λ≈3.2e6), the empirically measured onset of the
/// first non-trivial passes in a 8-bit photo at 4–5 DWT levels. Below this
/// threshold essentially no passes survive, yielding only bare headers.
/// q=99 anchored at log10(3000)=3.476 → span = 6.5 - 3.476 = 3.024.
/// Map quality setting (0-100) to lambda threshold, scaled by image resolution.
///
/// **Resolution-aware scaling:** Lambda is adjusted based on pixel count to ensure
/// consistent quality behavior across different image sizes. A reference resolution
/// of 12M pixels (4000×3000) serves as the baseline for the curve calibration.
///
/// Smaller images get proportionally lower lambda values (less aggressive truncation)
/// to compensate for JPEG 2000's fixed overhead and simpler wavelet structures.
///
/// Calibration reference for 8-bit photographic content at 12M pixels:
///   q=1  -> λ≈3.2e6 (absolute floor: heavy blocking, barely recognizable)
///   q=25 -> λ≈8.5e4 (heavy compression, clear artifacts)
///   q=50 -> λ≈1.7e4 (moderate compression, visible artifacts)
///   q=75 -> λ≈4.4e3 (light compression, minor artifacts)
///   q=99 -> λ≈1.2e3 (perceptually lossless)
///   q=100 -> λ=0    (mathematically lossless)
pub fn quality_to_lambda(quality: u8, pixel_count: u64) -> f64 {
    if quality >= 100 {
        return 0.0;
    }
    if quality == 0 {
        return f64::MAX;
    }

    // Base curve calibrated for a 12M pixel reference image against the
    // MEASURED Annex-J ΔMSE distortion scale (2026-07 recalibration): slopes
    // are image-domain squared-error per byte. Anchors were bisected on
    // lear.png so each quality reproduces the file size of the previous
    // heuristic-model curve within ~1%: q10→4.8e5, q25→1.9e5, q40→6.5e4,
    // q55→2.7e4, q70→1.1e4, q85→4.1e3.
    let t = (quality as f64 - 1.0) / 98.0; // 0.0 at q=1, 1.0 at q=99
    let log_lambda = 5.9609 - 2.7138 * t.powf(0.9544);
    let base_lambda = 10f64.powf(log_lambda).max(1e-3);

    // q90..q99 needs a steeper tail than the mid-range curve. At that end the
    // quantizer is already using finer step sizes, and keeping lambda too high
    // can paradoxically retain fewer passes than a lower-quality encode. Fade
    // lambda toward zero so q99 means "all available lossy 9/7 passes"; q100 is
    // still handled separately as reversible lossless.
    let tail_multiplier = if quality >= 90 {
        // Exponent 1.66 re-anchors q95 to its bisected lambda under the
        // measured-ΔMSE scale (continuous with the main curve at q90).
        let tail = (99.0 - quality.min(99) as f64) / 9.0;
        tail.powf(1.66)
    } else {
        1.0
    };

    // Resolution scaling: smaller images need lower lambda (larger files per pixel)
    // to maintain consistent visual quality despite fixed overhead
    const REFERENCE_PIXELS: f64 = 12_000_000.0; // 4000×3000
    let resolution_factor = (pixel_count as f64 / REFERENCE_PIXELS).powf(0.35);

    base_lambda * resolution_factor * tail_multiplier
}

/// Slope threshold for `RateMode::DocumentTrim` pass trimming.
///
/// Slopes under this mode are measured image-domain ΔMSE per byte, so the
/// threshold has physical meaning: drop passes where one byte buys less than
/// this much squared-error reduction. Calibrated on cleaned 300-dpi text
/// pages: trimming at 1e3 shaves a few percent of bytes with no measurable
/// PSNR loss; the quantizer (see `apply_document_step_scaling`) is the actual
/// rate control.
pub const DOCUMENT_TRIM_LAMBDA: f64 = 1.0e3;

/// Select passes for `RateMode::DocumentTrim`: a fixed measured-slope cutoff.
pub fn select_for_document_trim(
    curves: &[CodeBlockPcrdCurve],
) -> Result<LayerSelection, PcrdError> {
    if curves.is_empty() {
        return Ok(LayerSelection {
            target_bytes: 0,
            actual_bytes: 0,
            lambda: DOCUMENT_TRIM_LAMBDA,
            selections: Vec::new(),
        });
    }
    evaluate_lambda(curves, DOCUMENT_TRIM_LAMBDA)
}

/// Select passes based on quality (via lambda), not byte budget.
///
/// Applies per-image lambda calibration: if `quality_to_lambda(1)` exceeds all
/// slopes in `curves` (meaning q=1 would produce bare-header output), the entire
/// lambda curve is scaled down proportionally so that q=1 lands just above the
/// image's maximum slope (by a 1.2× margin). This makes quality 1..99 stable
/// and meaningful regardless of image size or complexity.
pub fn select_for_quality(
    curves: &[CodeBlockPcrdCurve],
    quality: u8,
    pixel_count: u64,
) -> Result<LayerSelection, PcrdError> {
    if curves.is_empty() {
        return Ok(LayerSelection {
            target_bytes: 0,
            actual_bytes: 0,
            lambda: 0.0,
            selections: Vec::new(),
        });
    }

    let raw_lambda = quality_to_lambda(quality, pixel_count);
    let lambda = if quality >= 100 {
        raw_lambda
    } else {
        calibrate_lambda(curves, raw_lambda, pixel_count)
    };
    evaluate_lambda(curves, lambda)
}

/// Scale `raw_lambda` down if the image's max slope is below `quality_to_lambda(1)`.
///
/// The scale factor is `min(1.0, (max_slope * 1.2) / quality_to_lambda(1))`,
/// applied to all quality levels uniformly. This keeps the quality curve shape
/// intact while ensuring q=1 produces at least a minimal non-trivial output.
fn calibrate_lambda(curves: &[CodeBlockPcrdCurve], raw_lambda: f64, pixel_count: u64) -> f64 {
    let max_slope = curves
        .iter()
        .flat_map(|c| c.points.iter().skip(1)) // skip origin (slope = ∞)
        .filter(|p| p.slope.is_finite() && p.slope > 0.0)
        .map(|p| p.slope)
        .fold(0.0f64, f64::max);

    if max_slope <= 0.0 {
        return raw_lambda;
    }

    let floor_lambda = quality_to_lambda(1, pixel_count);
    let target_floor = max_slope * 1.2;

    if target_floor >= floor_lambda {
        // Current calibration already puts q=1 below max_slope — no adjustment needed.
        return raw_lambda;
    }

    raw_lambda * (target_floor / floor_lambda)
}

#[cfg(test)]
mod quality_count_tests {
    use super::quality_to_lambda;

    #[test]
    fn quality_calibration_accepts_pixel_counts_above_u32() {
        let large = quality_to_lambda(75, u64::from(u32::MAX) + 1);
        let wrapped = quality_to_lambda(75, 0);
        assert!(large.is_finite());
        assert_ne!(large, wrapped);
    }
}
