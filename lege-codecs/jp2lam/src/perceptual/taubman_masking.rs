/// Taubman 2000 §VI subband-domain visual masking.
///
/// Reference: Taubman, D. (2000). "High Performance Scalable Image Compression
/// with EBCOT." IEEE TIP 9(7), eqs. (4)–(5), p. 1166–1167.
///
/// The masker operates on wavelet subband coefficients, not image-domain pixels.
/// For each 8×8 cell of coefficients in a subband:
///
///   ν(cell) = (1/N) · Σ_{(u,v)∈cell} (‖w_b‖ · |v_{u,v}|)   [eq. (4), α=0.5]
///
/// The perceptual distortion weight for any coefficient in that cell is:
///
///   w_vis = ‖w_b‖² / (ν² + f²)   where f² = 1/512
///
/// High-activity cells (large ν) get lower weight → more distortion allowed.
/// Flat cells (ν ≈ 0) get weight ≈ ‖w_b‖² · 512 → maximum preservation.
///
/// For PCRD integration, each code block receives a single scalar multiplier:
/// the mean of w_vis over all coefficients in the block, divided by ‖w_b‖²
/// (since subband_weight already accounts for that factor). The result is
/// a dimensionless weight in (0, 512], with 1.0 meaning "unmasked".

const F_SQUARED: f64 = 1.0 / 512.0;
const CELL_SIZE: usize = 8;
const CELL_AREA: f64 = (CELL_SIZE * CELL_SIZE) as f64;

/// Per-block Taubman masking weight, suitable for scaling distortion estimates.
///
/// Values < 1.0 indicate the block is in a textured region (masking hides errors).
/// Values > 1.0 indicate a flat region (no masking — every error is visible).
/// The value 1.0 represents the flat-cell floor when ν = 1/√512.
#[derive(Debug, Clone)]
pub struct TaubmanMaskMap {
    /// Width of the subband in cells (ceil(subband_width / 8)).
    pub cell_cols: usize,
    /// Height of the subband in cells (ceil(subband_height / 8)).
    pub cell_rows: usize,
    /// Per-cell ν value = mean(‖w_b‖·|v|) over the 8×8 cell.
    pub cell_nu: Vec<f64>,
}

impl TaubmanMaskMap {
    /// Compute a Taubman masking map from wavelet subband coefficients.
    ///
    /// `coeffs` is row-major with stride `width`.
    /// `synthesis_norm` is ‖w_b‖ (not squared) for the subband.
    pub fn from_subband(coeffs: &[f64], width: usize, height: usize, synthesis_norm: f64) -> Self {
        let cell_cols = width.div_ceil(CELL_SIZE);
        let cell_rows = height.div_ceil(CELL_SIZE);
        let mut cell_nu = vec![0.0f64; cell_cols * cell_rows];

        for cell_row in 0..cell_rows {
            for cell_col in 0..cell_cols {
                let row_start = cell_row * CELL_SIZE;
                let col_start = cell_col * CELL_SIZE;
                let row_end = (row_start + CELL_SIZE).min(height);
                let col_end = (col_start + CELL_SIZE).min(width);

                let mut sum = 0.0f64;
                let mut count = 0usize;
                for row in row_start..row_end {
                    for col in col_start..col_end {
                        let v = coeffs[row * width + col];
                        sum += synthesis_norm * v.abs();
                        count += 1;
                    }
                }
                let nu = if count > 0 { sum / count as f64 } else { 0.0 };
                cell_nu[cell_row * cell_cols + cell_col] = nu;
            }
        }

        Self {
            cell_cols,
            cell_rows,
            cell_nu,
        }
    }

    /// Per-cell visibility divisor: ν² + f².
    #[inline]
    pub fn visibility_divisor(&self, cell_row: usize, cell_col: usize) -> f64 {
        let nu = self.cell_nu[cell_row * self.cell_cols + cell_col];
        nu * nu + F_SQUARED
    }

    /// Compute the mean perceptual distortion multiplier for a code block.
    ///
    /// The multiplier = mean(1 / (ν² + f²)) over all 8×8 cells overlapping
    /// the block. It is normalized by dividing by (1/f²) so that 1.0
    /// corresponds to the completely flat (ν=0) case.
    ///
    /// Returns a value in (0.0, 1.0] where:
    /// - 1.0 = perfectly flat, no masking (highest perceptual cost per bit)
    /// - ~0.0 = very textured, strong masking (lowest perceptual cost per bit)
    ///
    /// `block_{col,row}` are in coefficient coordinates, `block_{w,h}` in samples.
    pub fn block_masking_multiplier(
        &self,
        block_col: usize,
        block_row: usize,
        block_w: usize,
        block_h: usize,
    ) -> f64 {
        let cell_col_start = block_col / CELL_SIZE;
        let cell_col_end = (block_col + block_w)
            .div_ceil(CELL_SIZE)
            .min(self.cell_cols);
        let cell_row_start = block_row / CELL_SIZE;
        let cell_row_end = (block_row + block_h)
            .div_ceil(CELL_SIZE)
            .min(self.cell_rows);

        let mut sum_inv = 0.0f64;
        let mut cell_count = 0usize;
        for cr in cell_row_start..cell_row_end {
            for cc in cell_col_start..cell_col_end {
                sum_inv += 1.0 / self.visibility_divisor(cr, cc);
                cell_count += 1;
            }
        }

        if cell_count == 0 {
            return 1.0;
        }

        let mean_inv = sum_inv / cell_count as f64;
        // Normalize so that the flat-cell case (ν=0 → divisor=f²) maps to 1.0.
        let flat_inv = 1.0 / F_SQUARED;
        (mean_inv / flat_inv).clamp(0.0, 1.0)
    }
}

/// Convenience: compute the average cell ν across an entire subband.
///
/// Useful for diagnostics and per-subband activity metrics.
pub fn mean_subband_nu(mask: &TaubmanMaskMap) -> f64 {
    if mask.cell_nu.is_empty() {
        return 0.0;
    }
    mask.cell_nu.iter().copied().sum::<f64>() / mask.cell_nu.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_subband_gives_multiplier_one() {
        let width = 16;
        let height = 16;
        let coeffs = vec![0.0f64; width * height];
        let mask = TaubmanMaskMap::from_subband(&coeffs, width, height, 2.0);
        let m = mask.block_masking_multiplier(0, 0, 16, 16);
        assert!(
            (m - 1.0).abs() < 1e-10,
            "flat subband must give 1.0, got {m}"
        );
    }

    #[test]
    fn textured_subband_gives_lower_multiplier() {
        let width = 16;
        let height = 16;
        // High-amplitude coefficients → large ν → small multiplier
        let coeffs = vec![100.0f64; width * height];
        let mask = TaubmanMaskMap::from_subband(&coeffs, width, height, 2.0);
        let m = mask.block_masking_multiplier(0, 0, 16, 16);
        assert!(
            m < 0.1,
            "textured subband should give low multiplier, got {m}"
        );
    }

    #[test]
    fn multiplier_is_in_range() {
        let width = 32;
        let height = 32;
        // Mix of flat and textured
        let mut coeffs = vec![0.0f64; width * height];
        for i in (0..width * height).step_by(3) {
            coeffs[i] = 50.0;
        }
        let mask = TaubmanMaskMap::from_subband(&coeffs, width, height, 1.965);
        let m = mask.block_masking_multiplier(0, 0, 32, 32);
        assert!(m > 0.0 && m <= 1.0, "multiplier out of range: {m}");
    }

    #[test]
    fn cell_count_matches_subband_dims() {
        let mask = TaubmanMaskMap::from_subband(&vec![1.0; 9 * 9], 9, 9, 1.0);
        assert_eq!(mask.cell_cols, 2); // ceil(9/8)
        assert_eq!(mask.cell_rows, 2);
        assert_eq!(mask.cell_nu.len(), 4);
    }

    #[test]
    fn partial_cell_uses_available_samples_only() {
        // 9×9 subband: corner cell is 1×1, should still give finite ν
        let coeffs = vec![10.0f64; 9 * 9];
        let mask = TaubmanMaskMap::from_subband(&coeffs, 9, 9, 1.0);
        // All cells should have the same ν because the amplitude is uniform
        for nu in &mask.cell_nu {
            assert!(nu.is_finite() && *nu > 0.0, "bad ν: {nu}");
        }
    }

    #[test]
    fn f_squared_floor_prevents_division_by_zero() {
        let coeffs = vec![0.0f64; 64];
        let mask = TaubmanMaskMap::from_subband(&coeffs, 8, 8, 0.0);
        let denom = mask.visibility_divisor(0, 0);
        assert_eq!(denom, F_SQUARED);
    }
}
