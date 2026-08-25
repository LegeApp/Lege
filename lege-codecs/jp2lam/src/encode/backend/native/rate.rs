//! PCRD adapters that turn native Tier-1 output into rate-distortion curves.
//!
//! This layer keeps the PCRD module itself (`crate::dwt::pcrd`) independent of
//! any Tier-1 data shape. It is the narrow glue point between
//! `NativeEncodedTier1Layout` and `CodeBlockPcrdCurve`.

use crate::dwt::norms::{band_gain, get_norm_97};
use crate::dwt::pcrd::{
    BandKind, CodeBlockPcrdCurve, PcrdError, RawPassRecord, apply_contrast_masking_to_delta,
    band_distortion_bias, build_hull_curve,
};
use crate::encode::block_store::StoredTier1Layout;
use crate::plan::{BandOrientation, SubbandQuant};
use crate::profile::{BlockClass, class_distortion_weight};

#[cfg(test)]
use super::t1::{NativeEncodedTier1CodeBlock, NativeEncodedTier1Layout};
use super::t1::NativeEncodedTier1Pass;

/// Estimated packet-header signaling cost per included code-block (bytes).
///
/// A single-layer, single-termination encode pays roughly 2–4 bytes of overhead
/// per block for inclusion tag-tree bits, zero-bitplane tag-tree bits, pass-count
/// comma-code, and segment-length field. Adding this to cumulative_length means
/// the slope from "omit block" to "include first pass" reflects the real cost,
/// without affecting the marginal slope between consecutive included passes.
const HEADER_OVERHEAD_BYTES: u32 = 2;

/// Conservative activation window for the first perceptual allocator.
///
/// The broad q50..84 shoulder can have too few interchangeable truncation
/// points at low rates, and the q96..99 tail already retains nearly every
/// useful pass. Gate the shoulder by its actual byte operating point and keep
/// the tail on measured MSE until a finer-grained allocator is available.
pub(crate) fn perceptual_weighting_candidate(quality: u8) -> bool {
    matches!(quality, 1..=95)
}

pub(crate) fn perceptual_weighting_enabled(
    quality: u8,
    target_bytes: u32,
    pixel_count: u64,
) -> bool {
    if !perceptual_weighting_candidate(quality) {
        return false;
    }
    if pixel_count == 0 {
        return false;
    }
    let target_bpp = f64::from(target_bytes) * 8.0 / pixel_count as f64;
    match quality {
        50..=60 => target_bpp >= 0.10,
        61..=84 => target_bpp >= 0.80,
        _ => true,
    }
}

/// Build pruned PCRD hull curves for every code-block in a Tier-1 layout.
///
/// `num_resolutions` is the number of DWT resolutions in the component.
/// `subband_quants` and `precision` supply the quantization step Δ for each
/// subband, making distortion estimates dimensionally consistent with the actual
/// coefficient-domain magnitudes.
/// Distortion starts with the measured Annex-J ΔMSE recorded during Tier-1,
/// scaled to image-domain MSE via the quantization step and squared synthesis
/// norm. When `perceptual` is true, a conservative block/band visibility
/// multiplier changes only the ordering of code-block truncation points. The
/// caller size-matches that ordering against the unweighted curves, so enabling
/// the model does not move the public quality setting's byte operating point.
/// Superseded by [`curves_from_stored_layout`] (used by the live tile-rect
/// path). Note that the live path *also* leaves Taubman masking neutral —
/// `curves_from_stored_layout` passes a hardcoded `1.0` weight into
/// `raw_records_for_passes` rather than a real per-block weight from
/// `TaubmanMaskMap` (see that type's doc comment in
/// `perceptual::taubman_masking`) — this function's now-dead
/// `taubman_weights: Option<&[f64]>` parameter was the other half of that
/// same never-activated feature. Kept under `#[cfg(test)]`; see
/// `native::layout`'s module doc comment for the broader legacy-pipeline
/// context.
#[cfg(test)]
pub(crate) fn curves_from_tier1_layout(
    layout: &NativeEncodedTier1Layout,
    num_resolutions: u8,
    subband_quants: &[SubbandQuant],
    precision: u32,
    quality: u8,
    component_weight: f64,
    contrast_mask: Option<&crate::perceptual::ContrastMaskMap>,
    taubman_weights: Option<&[f64]>,
    perceptual: bool,
) -> Result<Vec<CodeBlockPcrdCurve>, PcrdError> {
    let mut curves = Vec::new();
    let mut next_id = 0usize;
    let mut next_taubman_weight = 0usize;

    for band in &layout.bands {
        let weight = subband_weight_for(num_resolutions, band.resolution, band.band);
        let band_x0 = band.blocks.iter().map(|block| block.x0).min().unwrap_or(0);
        let band_y0 = band.blocks.iter().map(|block| block.y0).min().unwrap_or(0);
        let quant_step = subband_quants
            .iter()
            .find(|sq| sq.resolution == band.resolution && sq.band == band.band)
            .map(|sq| quant_step_from_subband(*sq, precision))
            .unwrap_or(1.0);
        for block in &band.blocks {
            let contrast_weight = compute_block_contrast_weight(
                contrast_mask,
                block.x0,
                block.y0,
                block.x1,
                block.y1,
                band_x0,
                band_y0,
                0,
                0,
                band.resolution,
                band.band,
                num_resolutions,
            );
            let taubman_weight = taubman_weights
                .and_then(|weights| weights.get(next_taubman_weight))
                .copied()
                .unwrap_or(1.0);
            next_taubman_weight += 1;
            let raws = raw_records_for_block(
                block,
                weight,
                quant_step,
                component_weight,
                band.resolution,
                band.band,
                quality,
                contrast_weight,
                taubman_weight,
                perceptual,
            );
            curves.push(build_hull_curve(next_id, &raws)?);
            next_id += 1;
        }
    }

    Ok(curves)
}

/// Append PCRD curves for one stored tile-component layout.
///
/// `first_block_id` lets the caller allocate one dense identifier space across
/// every tile and component. The returned identifiers therefore map directly
/// back to a global flat selection table without making payload ownership part
/// of the PCRD module.
pub(crate) fn curves_from_stored_layout(
    layout: &StoredTier1Layout,
    first_block_id: usize,
    num_resolutions: u8,
    subband_quants: &[SubbandQuant],
    precision: u32,
    component_weight: f64,
    quality: u8,
    contrast_mask: Option<&crate::perceptual::ContrastMaskMap>,
    tile_x0: usize,
    tile_y0: usize,
    perceptual: bool,
) -> Result<Vec<CodeBlockPcrdCurve>, PcrdError> {
    let mut curves = Vec::new();

    for band in &layout.bands {
        let weight = subband_weight_for(num_resolutions, band.resolution, band.band);
        let band_x0 = band.blocks.iter().map(|block| block.x0).min().unwrap_or(0);
        let band_y0 = band.blocks.iter().map(|block| block.y0).min().unwrap_or(0);
        let quant_step = subband_quants
            .iter()
            .find(|sq| sq.resolution == band.resolution && sq.band == band.band)
            .map(|sq| quant_step_from_subband(*sq, precision))
            .unwrap_or(1.0);
        for block in &band.blocks {
            let contrast_weight = compute_block_contrast_weight(
                contrast_mask,
                block.x0,
                block.y0,
                block.x1,
                block.y1,
                band_x0,
                band_y0,
                tile_x0,
                tile_y0,
                band.resolution,
                band.band,
                num_resolutions,
            );
            let raws = raw_records_for_passes(
                &block.passes,
                weight,
                quant_step,
                component_weight,
                band.resolution,
                band.band,
                quality,
                block.block_class,
                contrast_weight,
                1.0,
                perceptual,
            );
            curves.push(build_hull_curve(first_block_id + curves.len(), &raws)?);
        }
    }

    Ok(curves)
}

#[cfg(test)]
fn raw_records_for_block(
    block: &NativeEncodedTier1CodeBlock,
    subband_weight: f64,
    quant_step: f64,
    component_weight: f64,
    resolution: u8,
    band: BandOrientation,
    quality: u8,
    contrast_visibility_weight: f64,
    taubman_masking_weight: f64,
    perceptual: bool,
) -> Vec<RawPassRecord> {
    raw_records_for_passes(
        &block.passes,
        subband_weight,
        quant_step,
        component_weight,
        resolution,
        band,
        quality,
        block.block_class,
        contrast_visibility_weight,
        taubman_masking_weight,
        perceptual,
    )
}

#[allow(clippy::too_many_arguments)]
fn raw_records_for_passes(
    passes: &[NativeEncodedTier1Pass],
    subband_weight: f64,
    quant_step: f64,
    component_weight: f64,
    resolution: u8,
    band: BandOrientation,
    quality: u8,
    block_class: BlockClass,
    contrast_visibility_weight: f64,
    taubman_masking_weight: f64,
    perceptual: bool,
) -> Vec<RawPassRecord> {
    let mut prev_cumulative = 0u32;
    let mut records = Vec::with_capacity(passes.len());

    for pass in passes {
        // Annex-J measured ΔMSE: Tier-1 numerator is in quantized-coefficient
        // units; Δ² maps to DWT-coefficient units and the squared synthesis
        // norm maps to image-domain squared error. Midpoint reconstruction
        // can make an individual pass's aggregate slightly negative (some
        // samples worsen); clamp — the hull prune drops zero-slope points.
        let measured_delta =
            (pass.mse_numerator * quant_step * quant_step * subband_weight * component_weight)
                .max(0.0);
        let distortion_delta = if perceptual {
            perceptual_distortion_delta(
                measured_delta,
                resolution,
                band,
                block_class,
                quality,
                contrast_visibility_weight,
                taubman_masking_weight,
            )
        } else {
            measured_delta
        };

        let cumulative = pass.cumulative_length as u32 + HEADER_OVERHEAD_BYTES;
        let incremental = cumulative - prev_cumulative;
        prev_cumulative = cumulative;
        records.push(RawPassRecord::new(
            pass.pass_index,
            incremental,
            cumulative,
            distortion_delta,
        ));
    }

    records
}

/// Compute contrast visibility weight for a code-block.
///
/// Maps the code-block's wavelet-domain position to the original image space
/// and averages the contrast mask over that region.
#[allow(clippy::too_many_arguments)]
fn compute_block_contrast_weight(
    contrast_mask: Option<&crate::perceptual::ContrastMaskMap>,
    block_x0: usize,
    block_y0: usize,
    block_x1: usize,
    block_y1: usize,
    band_x0: usize,
    band_y0: usize,
    tile_x0: usize,
    tile_y0: usize,
    resolution: u8,
    band: BandOrientation,
    num_resolutions: u8,
) -> f64 {
    use crate::perceptual::{SourceRect, average_mask_for_source_rect};
    use crate::plan::BandOrientation;

    let Some(mask) = contrast_mask else {
        return 1.0; // No masking if mask not provided
    };

    // Compute decomposition level: how many times this subband was downsampled
    let decomposition_level = if matches!(band, BandOrientation::Ll) {
        // LL band is at the coarsest level
        num_resolutions.saturating_sub(1)
    } else {
        // High-pass bands: level = (num_resolutions - 1) - resolution
        num_resolutions.saturating_sub(1).saturating_sub(resolution)
    };

    // Source scale: each wavelet level doubles the spatial extent
    let source_scale = 1 << decomposition_level;

    // Map code-block coordinates to source image space
    let local_x0 = block_x0.saturating_sub(band_x0);
    let local_y0 = block_y0.saturating_sub(band_y0);
    let local_x1 = block_x1.saturating_sub(band_x0);
    let local_y1 = block_y1.saturating_sub(band_y0);
    let x0 = tile_x0.saturating_add(local_x0.saturating_mul(source_scale));
    let y0 = tile_y0.saturating_add(local_y0.saturating_mul(source_scale));
    let x1 = tile_x0.saturating_add(local_x1.saturating_mul(source_scale));
    let y1 = tile_y0.saturating_add(local_y1.saturating_mul(source_scale));

    let rect = SourceRect { x0, y0, x1, y1 };

    average_mask_for_source_rect(mask, rect)
}

#[cfg(test)]
pub(crate) fn contrast_weight_for_stored_block(
    layout: &StoredTier1Layout,
    contrast_mask: Option<&crate::perceptual::ContrastMaskMap>,
    tile_x0: usize,
    tile_y0: usize,
    num_resolutions: u8,
    block_id: usize,
) -> Option<f64> {
    let mut next_block = 0usize;
    for band in &layout.bands {
        let band_x0 = band.blocks.iter().map(|block| block.x0).min().unwrap_or(0);
        let band_y0 = band.blocks.iter().map(|block| block.y0).min().unwrap_or(0);
        for block in &band.blocks {
            if next_block == block_id {
                return Some(compute_block_contrast_weight(
                    contrast_mask,
                    block.x0,
                    block.y0,
                    block.x1,
                    block.y1,
                    band_x0,
                    band_y0,
                    tile_x0,
                    tile_y0,
                    band.resolution,
                    band.band,
                    num_resolutions,
                ));
            }
            next_block += 1;
        }
    }
    None
}

fn perceptual_distortion_delta(
    measured_delta: f64,
    resolution: u8,
    band: BandOrientation,
    block_class: BlockClass,
    quality: u8,
    contrast_visibility_weight: f64,
    taubman_masking_weight: f64,
) -> f64 {
    if measured_delta <= 0.0 {
        return 0.0;
    }
    let band_kind = match band {
        BandOrientation::Ll => BandKind::Ll,
        BandOrientation::Hl => BandKind::Hl,
        BandOrientation::Lh => BandKind::Lh,
        BandOrientation::Hh => BandKind::Hh,
    };
    let band_weight = band_distortion_bias(band_kind, quality).clamp(0.82, 1.20);
    let class_weight =
        class_distortion_weight(block_class, matches!(band, BandOrientation::Ll), resolution)
            .clamp(0.85, 1.35);
    let taubman_weight = taubman_masking_weight.clamp(0.20, 1.0);
    let weighted = apply_contrast_masking_to_delta(
        measured_delta * band_weight * class_weight * taubman_weight,
        contrast_visibility_weight,
        block_class,
        quality,
    );
    let strength = perceptual_weighting_strength(quality);
    (measured_delta + strength * (weighted - measured_delta)).max(0.0)
}

/// Empirically calibrated blend from measured MSE to perceptual distortion.
///
/// Smooth triangular shoulders around the benchmarked q50 and q75 operating
/// points avoid making adjacent public quality settings behave as unrelated
/// modes; the high-quality range uses a deliberately weak blend. The
/// bpp gate above separately prevents the stronger shoulders from reshaping
/// sparse allocations with too few viable passes.
fn perceptual_weighting_strength(quality: u8) -> f64 {
    let triangle = |center: i16, half_width: f64| {
        let distance = f64::from((i16::from(quality) - center).unsigned_abs());
        (1.0 - distance / half_width).clamp(0.0, 1.0)
    };

    match quality {
        0..=60 => {
            let base = 0.08 + 0.32 * f64::from(quality) / 60.0;
            base + (1.0 - base) * triangle(50, 5.0)
        }
        61..=64 => 0.08 + 0.32 * f64::from(65 - quality) / 5.0,
        65..=84 => 0.08 + 0.42 * triangle(75, 5.0),
        85..=95 => 0.08 + 0.02 * triangle(90, 5.0),
        _ => 0.0,
    }
}

fn subband_weight_for(num_resolutions: u8, resolution: u8, band: BandOrientation) -> f64 {
    // 9/7 synthesis norm squared: contribution of a unit coefficient in this
    // subband to the reconstructed image's squared-error budget.
    let level = match band {
        BandOrientation::Ll => num_resolutions.saturating_sub(1),
        _ => num_resolutions.saturating_sub(1).saturating_sub(resolution),
    };
    let norm = get_norm_97(u32::from(level), band);
    norm * norm
}

/// Decode the quantization step Δ from a packed `SubbandQuant`.
///
/// JPEG 2000 scalar-expounded step: Δ = (1 + μ/2048) · 2^(numbps − ε)
/// where numbps = precision + band_gain, ε = exponent, μ = mantissa.
/// For reversible 5/3 encoding, mantissa = 0 and exponent = numbps, giving Δ = 1.
fn quant_step_from_subband(sq: SubbandQuant, precision: u32) -> f64 {
    let numbps = precision + u32::from(band_gain(sq.band));
    let mantissa_frac = f64::from(sq.mantissa) / 2048.0;
    (1.0 + mantissa_frac) * (2.0f64).powi(numbps as i32 - i32::from(sq.exponent))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::encode::backend::native::NativeBackend;
    use crate::encode::context::EncodeContext;
    use crate::model::{ColorSpace, Component, EncodeOptions, Image, OutputFormat, Preset};

    fn tiny_gray_ctx() -> (Image, EncodeOptions) {
        let image = Image {
            width: 4,
            height: 4,
            components: vec![Component {
                data: vec![
                    0, 16, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240,
                ],
                width: 4,
                height: 4,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }],
            colorspace: ColorSpace::Gray,
        };
        let options = EncodeOptions {
            quality: Preset::DocumentHigh.quality(),
            format: OutputFormat::J2k,
            profile: Default::default(),
            ..Default::default()
        };
        (image, options)
    }

    #[test]
    fn curves_from_tier1_layout_produce_hulls_with_monotone_slopes() {
        let (image, options) = tiny_gray_ctx();
        let context = EncodeContext::new(&image, &options).expect("build context");
        let encoded = NativeBackend
            .prepare_tier1_encoded_layout(&context)
            .expect("tier1 encoded layout");
        let num_resolutions = context.plan.num_resolutions;

        let precision = context
            .plan
            .components
            .first()
            .map(|c| c.precision)
            .unwrap_or(8);
        let curves = curves_from_tier1_layout(
            &encoded,
            num_resolutions,
            &context.plan.subband_quants,
            precision,
            context.plan.quality,
            1.0,
            None, // No contrast masking in test
            None, // No taubman masking in test
            false,
        )
        .expect("curves");
        assert!(!curves.is_empty(), "expected at least one block");

        for curve in &curves {
            // Origin point first.
            assert_eq!(curve.points[0].passes, 0);
            assert_eq!(curve.points[0].bytes, 0);
            // Strictly decreasing slopes after origin (monotone convex hull).
            for pair in curve.points.windows(2).skip(1) {
                assert!(
                    pair[1].slope < pair[0].slope,
                    "non-monotone slope in block {}: {} -> {}",
                    curve.block_id,
                    pair[0].slope,
                    pair[1].slope
                );
            }
        }
    }

    #[test]
    fn perceptual_activation_avoids_unstable_curve_shoulders() {
        for quality in [1, 25, 50, 60, 75, 85, 90, 95] {
            assert!(perceptual_weighting_candidate(quality), "q{quality}");
        }
        for quality in [0, 96, 98, 99, 100] {
            assert!(!perceptual_weighting_candidate(quality), "q{quality}");
        }
        assert!(perceptual_weighting_enabled(50, 12_500, 1_000_000));
        assert!(!perceptual_weighting_enabled(50, 12_499, 1_000_000));
        assert!(perceptual_weighting_enabled(75, 100_000, 1_000_000));
        assert!(!perceptual_weighting_enabled(75, 99_999, 1_000_000));
        assert!(perceptual_weighting_enabled(25, 1, 1_000_000));
        assert!(perceptual_weighting_enabled(90, 1, 1_000_000));
        assert!(!perceptual_weighting_enabled(98, u32::MAX, 1));
    }

    #[test]
    fn perceptual_strength_is_smooth_around_calibrated_points() {
        assert_eq!(super::perceptual_weighting_strength(50), 1.0);
        assert_eq!(super::perceptual_weighting_strength(75), 0.5);
        assert_eq!(super::perceptual_weighting_strength(90), 0.1);
        for center in [50, 75, 90] {
            let left = super::perceptual_weighting_strength(center - 1);
            let peak = super::perceptual_weighting_strength(center);
            let right = super::perceptual_weighting_strength(center + 1);
            assert!(
                left < peak && right < peak,
                "q{center}: {left}, {peak}, {right}"
            );
        }
        assert_eq!(super::perceptual_weighting_strength(96), 0.0);
    }
}
