#![allow(dead_code)]

//! PCRD adapters that turn native Tier-1 output into rate-distortion curves.
//!
//! This layer keeps the PCRD module itself (`crate::dwt::pcrd`) independent of
//! any Tier-1 data shape. It is the narrow glue point between
//! `NativeEncodedTier1Layout` and `CodeBlockPcrdCurve`.

use crate::dwt::norms::{band_gain, get_norm_97};
use crate::dwt::pcrd::{CodeBlockPcrdCurve, PcrdError, RawPassRecord, build_hull_curve};
use crate::encode::block_store::StoredTier1Layout;
use crate::perceptual::taubman_masking::TaubmanMaskMap;
use crate::plan::{BandOrientation, SubbandQuant};

// P1.3 (real Annex J ΔMSE) is rolled back pending lambda recalibration.
// Keep these imports parked for when we revisit:
// use crate::dwt::pcrd::{apply_contrast_masking_to_delta, band_distortion_bias};
// use crate::profile::class_distortion_weight;

use super::t1::{
    NativeEncodedTier1CodeBlock, NativeEncodedTier1Layout, NativeEncodedTier1Pass,
};

/// Estimated packet-header signaling cost per included code-block (bytes).
///
/// A single-layer, single-termination encode pays roughly 2–4 bytes of overhead
/// per block for inclusion tag-tree bits, zero-bitplane tag-tree bits, pass-count
/// comma-code, and segment-length field. Adding this to cumulative_length means
/// the slope from "omit block" to "include first pass" reflects the real cost,
/// without affecting the marginal slope between consecutive included passes.
const HEADER_OVERHEAD_BYTES: u32 = 2;

/// Build pruned PCRD hull curves for every code-block in a Tier-1 layout.
///
/// `num_resolutions` is the number of DWT resolutions in the component.
/// `subband_quants` and `precision` supply the quantization step Δ for each
/// subband, making distortion estimates dimensionally consistent with the actual
/// coefficient-domain magnitudes.
/// `quality` is carried for diagnostics; distortion is the measured Annex-J
/// ΔMSE recorded during Tier-1, scaled to image-domain MSE via the
/// quantization step and squared synthesis norm.
/// `contrast_mask` / `taubman_weights` are reserved for perceptual weighting
/// (currently unused).
pub(crate) fn curves_from_tier1_layout(
    layout: &NativeEncodedTier1Layout,
    num_resolutions: u8,
    subband_quants: &[SubbandQuant],
    precision: u32,
    quality: u8,
    component_weight: f64,
    contrast_mask: Option<&crate::perceptual::ContrastMaskMap>,
    taubman_weights: Option<&[f64]>,
) -> Result<Vec<CodeBlockPcrdCurve>, PcrdError> {
    let mut curves = Vec::new();
    let mut next_id = 0usize;
    let _ = (quality, contrast_mask, taubman_weights);

    for band in &layout.bands {
        let weight = subband_weight_for(num_resolutions, band.resolution, band.band);
        let quant_step = subband_quants
            .iter()
            .find(|sq| sq.resolution == band.resolution && sq.band == band.band)
            .map(|sq| quant_step_from_subband(*sq, precision))
            .unwrap_or(1.0);
        for block in &band.blocks {
            let raws = raw_records_for_block(block, weight, quant_step, component_weight);
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
) -> Result<Vec<CodeBlockPcrdCurve>, PcrdError> {
    let mut curves = Vec::new();

    for band in &layout.bands {
        let weight = subband_weight_for(num_resolutions, band.resolution, band.band);
        let quant_step = subband_quants
            .iter()
            .find(|sq| sq.resolution == band.resolution && sq.band == band.band)
            .map(|sq| quant_step_from_subband(*sq, precision))
            .unwrap_or(1.0);
        for block in &band.blocks {
            let raws = raw_records_for_passes(
                &block.passes,
                weight,
                quant_step,
                component_weight,
            );
            curves.push(build_hull_curve(first_block_id + curves.len(), &raws)?);
        }
    }

    Ok(curves)
}

/// Compute per-block Taubman §VI masking weights for all blocks in a layout.
///
/// Takes the pre-quantization 9/7 DWT coefficients (f32, row-major in the full
/// image array at `dwt_width` stride) and computes the masking weight for each
/// code block in band-major, block-major order — the same traversal used by
/// `curves_from_tier1_layout`.
///
/// Each weight is the Taubman §VI block_masking_multiplier ∈ (0, 1]:
/// 1.0 = flat (maximum perceptual cost), ~0 = highly textured (masking hides errors).
pub(crate) fn build_taubman_weights_for_layout(
    layout: &NativeEncodedTier1Layout,
    dwt_f32: &[f32],
    dwt_width: usize,
    num_resolutions: u8,
) -> Vec<f64> {
    let mut weights = Vec::new();
    for band in &layout.bands {
        let level = match band.band {
            BandOrientation::Ll => num_resolutions.saturating_sub(1),
            _ => num_resolutions
                .saturating_sub(1)
                .saturating_sub(band.resolution),
        };
        let synthesis_norm = get_norm_97(u32::from(level), band.band);

        for block in &band.blocks {
            let bw = block.x1 - block.x0;
            let bh = block.y1 - block.y0;
            // Extract block coefficients from the full interleaved DWT array
            let mut coeffs = Vec::with_capacity(bw * bh);
            for row in block.y0..block.y1 {
                let row_start = row * dwt_width + block.x0;
                for &v in &dwt_f32[row_start..row_start + bw] {
                    coeffs.push(v as f64);
                }
            }
            let mask = TaubmanMaskMap::from_subband(&coeffs, bw, bh, synthesis_norm);
            weights.push(mask.block_masking_multiplier(0, 0, bw, bh));
        }
    }
    weights
}

fn raw_records_for_block(
    block: &NativeEncodedTier1CodeBlock,
    subband_weight: f64,
    quant_step: f64,
    component_weight: f64,
) -> Vec<RawPassRecord> {
    raw_records_for_passes(
        &block.passes,
        subband_weight,
        quant_step,
        component_weight,
    )
}

fn raw_records_for_passes(
    passes: &[NativeEncodedTier1Pass],
    subband_weight: f64,
    quant_step: f64,
    component_weight: f64,
) -> Vec<RawPassRecord> {
    let mut prev_cumulative = 0u32;
    let mut records = Vec::with_capacity(passes.len());

    for pass in passes {
        // Annex-J measured ΔMSE: Tier-1 numerator is in quantized-coefficient
        // units; Δ² maps to DWT-coefficient units and the squared synthesis
        // norm maps to image-domain squared error. Midpoint reconstruction
        // can make an individual pass's aggregate slightly negative (some
        // samples worsen); clamp — the hull prune drops zero-slope points.
        let distortion_delta =
            (pass.mse_numerator * quant_step * quant_step * subband_weight * component_weight)
                .max(0.0);

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
fn compute_block_contrast_weight(
    contrast_mask: Option<&crate::perceptual::ContrastMaskMap>,
    block: &NativeEncodedTier1CodeBlock,
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
    let x0 = block.x0 as usize * source_scale;
    let y0 = block.y0 as usize * source_scale;
    let x1 = block.x1 as usize * source_scale;
    let y1 = block.y1 as usize * source_scale;

    let rect = SourceRect { x0, y0, x1, y1 };

    average_mask_for_source_rect(mask, rect)
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
}
