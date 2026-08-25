//! Contrast masking based on Ponomarenko et al. "On between-coefficient contrast masking"
//!
//! This module implements perceptual masking to identify regions where compression
//! artifacts are hidden by texture. The core idea:
//! - Textured areas can tolerate more distortion (artifacts blend with texture)
//! - Smooth areas and edges need higher quality (artifacts are visible)
//!
//! The implementation uses DCT-based analysis with edge correction to avoid
//! misclassifying clean edges as texture.

use std::cmp::Ordering;

/// Parameters controlling contrast masking strength and behavior
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContrastMaskParams {
    /// Paper uses 16.0. Higher = weaker masking.
    pub masking_divisor: f64,
    /// Global strength of masking in PCRD.
    /// 0.0 disables, 1.0 applies normally.
    pub strength: f64,
    /// Prevents texture blocks from being completely ignored.
    pub min_visibility_weight: f64,
    /// Prevents masking from increasing preservation weight.
    pub max_visibility_weight: f64,
    /// Numerical safety for zero-variance blocks.
    pub variance_epsilon: f64,
    /// Minimum visibility weight applied to near-white blocks (mean luma > 210).
    /// Much lower than `min_visibility_weight` so PCRD truncates white regions
    /// toward zero passes, letting them decode as pure white without artifacts.
    pub white_min_visibility: f64,
}

impl Default for ContrastMaskParams {
    fn default() -> Self {
        Self {
            masking_divisor: 16.0,
            strength: 0.75,
            min_visibility_weight: 0.25,
            max_visibility_weight: 1.0,
            variance_epsilon: 1e-9,
            white_min_visibility: 0.05,
        }
    }
}

/// Ponomarenko et al. CSF-derived DCT coefficient weights.
/// C[0][0] is 0 because DC does not contribute to contrast masking.
pub const PSNR_HVS_M_CSF_WEIGHTS: [[f64; 8]; 8] = [
    [
        0.0000, 0.8264, 1.0000, 0.3906, 0.1736, 0.0625, 0.0384, 0.0269,
    ],
    [
        0.6944, 0.6944, 0.5102, 0.2770, 0.1479, 0.0297, 0.0278, 0.0331,
    ],
    [
        0.5102, 0.5917, 0.3906, 0.1736, 0.0625, 0.0308, 0.0210, 0.0319,
    ],
    [
        0.5102, 0.3460, 0.2066, 0.1189, 0.0384, 0.0132, 0.0156, 0.0260,
    ],
    [
        0.3086, 0.2066, 0.0730, 0.0319, 0.0216, 0.0084, 0.0094, 0.0169,
    ],
    [
        0.1736, 0.0816, 0.0331, 0.0244, 0.0152, 0.0092, 0.0078, 0.0118,
    ],
    [
        0.0416, 0.0244, 0.0164, 0.0132, 0.0094, 0.0068, 0.0069, 0.0098,
    ],
    [
        0.0193, 0.0118, 0.0111, 0.0104, 0.0080, 0.0100, 0.0094, 0.0102,
    ],
];

/// Result of contrast masking analysis for one 8×8 block
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContrastMask {
    /// Weighted DCT energy Ew.
    pub weighted_energy: f64,
    /// Edge correction δ. Near 1.0 = texture-like. Near 0.0 = edge-like.
    pub edge_delta: f64,
    /// Masking energy Em = Ew * δ / divisor.
    pub masking_energy: f64,
    /// 0..1-ish normalized masking strength, useful for debugging/maps.
    pub normalized_masking: f64,
    /// PCRD multiplier. Lower means "less visible distortion here."
    pub visibility_weight: f64,
}

/// Compute 8×8 DCT for luminance block
///
/// This is a straightforward implementation, not optimized.
/// Can be replaced with a faster separable DCT later.
pub fn dct8x8_luma(block: &[f64; 64]) -> [[f64; 8]; 8] {
    let mut out = [[0.0f64; 8]; 8];

    for u in 0..8 {
        for v in 0..8 {
            let au = if u == 0 { 1.0 / 2.0f64.sqrt() } else { 1.0 };
            let av = if v == 0 { 1.0 / 2.0f64.sqrt() } else { 1.0 };

            let mut sum = 0.0;
            for y in 0..8 {
                for x in 0..8 {
                    let sx = block[y * 8 + x];
                    let cx = (((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI) / 16.0).cos();
                    let cy = (((2 * y + 1) as f64 * v as f64 * std::f64::consts::PI) / 16.0).cos();
                    sum += sx * cx * cy;
                }
            }

            out[u][v] = 0.25 * au * av * sum;
        }
    }

    out
}

/// Compute weighted DCT energy using CSF weights
///
/// This measures perceptually-weighted high-frequency content.
/// Higher values indicate more texture/detail that can mask artifacts.
pub fn weighted_dct_energy(coeffs: &[[f64; 8]; 8]) -> f64 {
    let mut ew = 0.0;

    for i in 0..8 {
        for j in 0..8 {
            let c = PSNR_HVS_M_CSF_WEIGHTS[i][j];
            ew += coeffs[i][j] * coeffs[i][j] * c;
        }
    }

    ew
}

/// Compute variance of a sequence of values
pub fn variance(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    let n = values.len() as f64;
    let mean = values.iter().copied().sum::<f64>() / n;

    values
        .iter()
        .map(|v| {
            let d = *v - mean;
            d * d
        })
        .sum::<f64>()
        / n
}

/// Compute edge correction factor δ using quadrant variance
///
/// This prevents misclassifying sharp edges as texture:
/// - Texture: each quadrant is also textured → δ ≈ 1.0
/// - Edge: whole block has high variance, but quadrants are flat → δ < 1.0
/// - Flat: whole variance near zero → δ = 0.0
pub fn quadrant_variance_delta(block: &[f64; 64], eps: f64) -> f64 {
    let whole_var = variance(block);
    if whole_var <= eps {
        return 0.0;
    }

    let mut q = [[0.0f64; 16]; 4];

    for y in 0..8 {
        for x in 0..8 {
            let qi = match (x < 4, y < 4) {
                (true, true) => 0,
                (false, true) => 1,
                (true, false) => 2,
                (false, false) => 3,
            };
            let lx = x & 3;
            let ly = y & 3;
            q[qi][ly * 4 + lx] = block[y * 8 + x];
        }
    }

    let avg_quadrant_var =
        (variance(&q[0]) + variance(&q[1]) + variance(&q[2]) + variance(&q[3])) / 4.0;

    (avg_quadrant_var / whole_var).clamp(0.0, 1.0)
}

/// Compute contrast mask for a single 8×8 luma block, using a fixed masking
/// divisor rather than the image-relative normalization
/// [`build_contrast_mask_map_from_luma_u8`] uses. Only exercised by tests,
/// as a context-free reference implementation the whole-image adaptive path
/// is checked against.
#[cfg(test)]
pub fn contrast_mask_for_luma_block8x8(
    block: &[f64; 64],
    params: ContrastMaskParams,
) -> ContrastMask {
    let coeffs = dct8x8_luma(block);
    let ew = weighted_dct_energy(&coeffs);
    let delta = quadrant_variance_delta(block, params.variance_epsilon);

    let masking_energy = ew * delta / params.masking_divisor;

    // Normalize with a soft saturating curve.
    // This keeps debug values usable without requiring a hard global scale.
    let normalized_masking = masking_energy / (masking_energy + 1024.0);

    let raw_visibility = 1.0 / (1.0 + params.strength * normalized_masking * 4.0);

    let visibility_weight =
        raw_visibility.clamp(params.min_visibility_weight, params.max_visibility_weight);

    ContrastMask {
        weighted_energy: ew,
        edge_delta: delta,
        masking_energy,
        normalized_masking,
        visibility_weight,
    }
}

/// Full-image contrast mask map
#[derive(Debug, Clone)]
pub struct ContrastMaskMap {
    pub blocks_x: usize,
    pub blocks_y: usize,
    pub masks: Vec<ContrastMask>,
}

impl ContrastMaskMap {
    pub fn get(&self, bx: usize, by: usize) -> Option<ContrastMask> {
        if bx >= self.blocks_x || by >= self.blocks_y {
            return None;
        }
        Some(self.masks[by * self.blocks_x + bx])
    }
}

/// Build contrast mask map from luma channel
///
/// This uses image-relative normalization: the normalizer is computed
/// from the 75th percentile of masking energies across all blocks.
/// This makes masking strength adapt to the image content.
pub fn build_contrast_mask_map_from_luma_u8(
    luma: &[u8],
    width: usize,
    height: usize,
    params: ContrastMaskParams,
) -> ContrastMaskMap {
    let blocks_x = width.div_ceil(8);
    let blocks_y = height.div_ceil(8);

    let mut raw_blocks = Vec::with_capacity(blocks_x * blocks_y);
    let mut energies = Vec::with_capacity(blocks_x * blocks_y);

    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let block = gather_luma_block8x8_edge_replicate(luma, width, height, bx * 8, by * 8);
            let mean_luma = block.iter().sum::<f64>() / 64.0;
            let coeffs = dct8x8_luma(&block);
            let ew = weighted_dct_energy(&coeffs);
            let delta = quadrant_variance_delta(&block, params.variance_epsilon);
            let masking_energy = ew * delta / params.masking_divisor;

            raw_blocks.push((ew, delta, masking_energy, mean_luma));
            energies.push(masking_energy);
        }
    }

    let normalizer = percentile_f64(&mut energies, 0.75).max(1.0);

    let masks = raw_blocks
        .into_iter()
        .map(|(ew, delta, masking_energy, mean_luma)| {
            let normalized = masking_energy / (masking_energy + normalizer);
            let raw_visibility = 1.0 / (1.0 + params.strength * normalized * 4.0);
            let mut visibility_weight =
                raw_visibility.clamp(params.min_visibility_weight, params.max_visibility_weight);

            // Near-white override: PCRD should truncate white-region blocks to zero
            // passes so they decode as pure white rather than retaining wavelet ringing
            // from adjacent content edges. Luma 210→250 fades the ceiling down to
            // white_min_visibility.
            const WHITE_START: f64 = 210.0;
            const WHITE_END: f64 = 250.0;
            if mean_luma > WHITE_START {
                let fade = ((mean_luma - WHITE_START) / (WHITE_END - WHITE_START)).clamp(0.0, 1.0);
                let white_ceil = params.min_visibility_weight * (1.0 - fade)
                    + params.white_min_visibility * fade;
                visibility_weight = visibility_weight.min(white_ceil);
            }

            ContrastMask {
                weighted_energy: ew,
                edge_delta: delta,
                masking_energy,
                normalized_masking: normalized,
                visibility_weight,
            }
        })
        .collect();

    ContrastMaskMap {
        blocks_x,
        blocks_y,
        masks,
    }
}

/// Gather an 8×8 luma block from the image, replicating edges as needed
fn gather_luma_block8x8_edge_replicate(
    luma: &[u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
) -> [f64; 64] {
    let mut block = [0.0f64; 64];

    for y in 0..8 {
        let sy = (y0 + y).min(height.saturating_sub(1));
        for x in 0..8 {
            let sx = (x0 + x).min(width.saturating_sub(1));
            block[y * 8 + x] = luma[sy * width + sx] as f64;
        }
    }

    block
}

/// Compute percentile of a mutable slice
fn percentile_f64(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 1.0;
    }

    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let idx = ((values.len() - 1) as f64 * p.clamp(0.0, 1.0)).round() as usize;
    values[idx]
}

/// Source rectangle for mapping code-blocks to image space
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

/// Average mask visibility weight over a source rectangle
///
/// For JPEG 2000 code-blocks, the source rectangle is computed as:
/// ```text
/// source_scale ≈ 2^(decomposition_level)
/// source_rectangle ≈ codeblock_rectangle * source_scale
/// ```
pub fn average_mask_for_source_rect(map: &ContrastMaskMap, rect: SourceRect) -> f64 {
    let bx0 = rect.x0 / 8;
    let by0 = rect.y0 / 8;
    let bx1 = rect.x1.saturating_add(7) / 8;
    let by1 = rect.y1.saturating_add(7) / 8;

    let mut sum = 0.0;
    let mut count = 0usize;

    for by in by0..by1.min(map.blocks_y) {
        for bx in bx0..bx1.min(map.blocks_x) {
            if let Some(mask) = map.get(bx, by) {
                sum += mask.visibility_weight;
                count += 1;
            }
        }
    }

    if count == 0 { 1.0 } else { sum / count as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_block_has_low_masking() {
        let block = [128.0f64; 64];
        let mask = contrast_mask_for_luma_block8x8(&block, ContrastMaskParams::default());

        assert!(mask.weighted_energy.abs() < 1e-6);
        assert!(mask.masking_energy.abs() < 1e-6);
        assert!(mask.visibility_weight > 0.95);
    }

    #[test]
    fn checker_texture_has_more_masking_than_flat() {
        let flat = [128.0f64; 64];

        let mut checker = [0.0f64; 64];
        for y in 0..8 {
            for x in 0..8 {
                checker[y * 8 + x] = if ((x + y) & 1) == 0 { 64.0 } else { 192.0 };
            }
        }

        let params = ContrastMaskParams::default();
        let m_flat = contrast_mask_for_luma_block8x8(&flat, params);
        let m_checker = contrast_mask_for_luma_block8x8(&checker, params);

        assert!(m_checker.masking_energy > m_flat.masking_energy);
        assert!(m_checker.visibility_weight < m_flat.visibility_weight);
    }

    #[test]
    fn clean_edge_gets_edge_delta_reduction() {
        let mut edge = [0.0f64; 64];
        for y in 0..8 {
            for x in 0..8 {
                edge[y * 8 + x] = if x < 4 { 32.0 } else { 224.0 };
            }
        }

        let delta = quadrant_variance_delta(&edge, 1e-9);

        // A perfect vertical split has high whole-block variance but low
        // within-quadrant variance, so delta should be low.
        assert!(delta < 0.2);
    }
}
