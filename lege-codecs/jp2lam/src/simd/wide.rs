//! Portable SIMD kernels built on the `wide` crate.

use super::Primitives;
use wide::{CmpEq, CmpLt, f32x8, i32x8, u32x8};

pub(crate) fn setup(primitives: &mut Primitives) {
    primitives.dwt.forward_97_2d = forward_97_2d;
    primitives.dwt.inverse_97_2d = inverse_97_2d;
    primitives.analyze.i32_max_magnitude_and_nnz = i32_max_magnitude_and_nnz;
    primitives.quant.quantize_f32_rect = quantize_f32_rect;
    primitives.quant.dequantize_i32_rect = dequantize_i32_rect;
    primitives.color.level_shift_f32 = level_shift_f32;
    primitives.color.level_shift_i32 = level_shift_i32;
    primitives.color.forward_ict_component = forward_ict_component;
    primitives.color.forward_rct_component = forward_rct_component;
    primitives.color.inverse_ict = inverse_ict;
    primitives.color.inverse_rct = inverse_rct;
    primitives.color.finalize_i32 = finalize_i32;
    // finalize_f32 stays scalar: matching `f32::round()` (ties away from zero)
    // exactly with `wide`'s round-to-nearest-even intrinsic risks a rare
    // off-by-one at .5 boundaries, which would make reconstructed pixels
    // depend on the active SIMD backend. Not worth it without a measured win.
    primitives.dwt.forward_53_2d = forward_53_2d;
    primitives.dwt.inverse_53_2d = inverse_53_2d;
    primitives.backend = "wide";
}

pub(crate) fn forward_97_2d(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
) -> crate::error::Result<()> {
    crate::dwt::forward_97_2d_in_place_wide(data, width, height, levels)
}

pub(crate) fn inverse_97_2d(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
) -> crate::error::Result<()> {
    crate::dwt::inverse_97_2d_in_place_wide(data, width, height, levels);
    Ok(())
}

pub(crate) fn forward_53_2d(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
) -> crate::error::Result<()> {
    crate::dwt::forward_53_2d_in_place_wide(data, width, height, levels)
}

pub(crate) fn inverse_53_2d(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
) -> crate::error::Result<()> {
    crate::dwt::inverse_53_2d_in_place_wide(data, width, height, levels)
}

pub(crate) fn i32_max_magnitude_and_nnz(coefficients: &[i32]) -> (u32, usize) {
    let mut max_vec = u32x8::new([0; 8]);
    let mut nonzero_count = 0usize;
    let mut i = 0usize;

    while i + 8 <= coefficients.len() {
        let values = i32x8::new(coefficients[i..i + 8].try_into().expect("8 lanes"));
        max_vec = max_vec.max(values.unsigned_abs());
        nonzero_count += values
            .to_array()
            .into_iter()
            .filter(|&value| value != 0)
            .count();
        i += 8;
    }

    let mut max_magnitude = max_vec.to_array().into_iter().max().unwrap_or(0);
    while i < coefficients.len() {
        let value = coefficients[i];
        max_magnitude = max_magnitude.max(value.unsigned_abs());
        nonzero_count += usize::from(value != 0);
        i += 1;
    }

    (max_magnitude, nonzero_count)
}

pub(crate) fn quantize_f32_rect(
    data: &[f32],
    out: &mut [i32],
    stride: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    step: f32,
) {
    if step <= 0.0 {
        super::scalar::quantize_f32_rect(data, out, stride, x0, y0, x1, y1, step);
        return;
    }
    let step_vec = f32x8::new([step; 8]);
    for y in y0..y1 {
        let row = y * stride;
        let mut x = x0;
        while x + 8 <= x1 {
            let values = f32x8::new(data[row + x..row + x + 8].try_into().expect("8 lanes"));
            let quantized = (values / step_vec).trunc_int().to_array();
            out[row + x..row + x + 8].copy_from_slice(&quantized);
            x += 8;
        }
        while x < x1 {
            out[row + x] = super::scalar::quantize_f32_to_i32(data[row + x], step);
            x += 1;
        }
    }
}

/// Mirrors `scalar::dequantize_i32_to_f32`: `sign * (|q| as f32 + 0.5) * step`,
/// with an explicit zero override. Uses `q.abs()` (signed) rather than
/// `q.unsigned_abs()` for the magnitude conversion; this only differs from the
/// scalar reference at `q == i32::MIN`, a magnitude far outside any quantized
/// JPEG 2000 coefficient produced by this codec.
pub(crate) fn dequantize_i32_rect(
    input: &[i32],
    output: &mut [f32],
    stride: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    step: f32,
) {
    let step_vec = f32x8::new([step; 8]);
    let half_vec = f32x8::new([0.5; 8]);
    let zero_vec = f32x8::new([0.0; 8]);
    let pos_one = f32x8::new([1.0; 8]);
    let neg_one = f32x8::new([-1.0; 8]);
    for y in y0..y1 {
        let row = y * stride;
        let mut x = x0;
        while x + 8 <= x1 {
            let q = i32x8::new(input[row + x..row + x + 8].try_into().expect("8 lanes"));
            let qf = q.round_float();
            let magnitude = q.abs().round_float();
            let is_zero = qf.cmp_eq(zero_vec);
            let is_negative = qf.cmp_lt(zero_vec);
            let sign = is_negative.blend(neg_one, pos_one);
            let dequantized = (sign * (magnitude + half_vec)) * step_vec;
            let result = is_zero.blend(zero_vec, dequantized);
            output[row + x..row + x + 8].copy_from_slice(&result.to_array());
            x += 8;
        }
        while x < x1 {
            output[row + x] = super::scalar::dequantize_i32_to_f32(input[row + x], step);
            x += 1;
        }
    }
}

pub(crate) fn finalize_i32(input: &[i32], out: &mut [i32]) {
    debug_assert_eq!(input.len(), out.len());
    let shift = i32x8::new([128; 8]);
    let lo = i32x8::new([0; 8]);
    let hi = i32x8::new([255; 8]);
    let mut i = 0usize;
    while i + 8 <= input.len() {
        let values = (i32x8::new(input[i..i + 8].try_into().expect("8 lanes")) + shift)
            .max(lo)
            .min(hi);
        out[i..i + 8].copy_from_slice(&values.to_array());
        i += 8;
    }
    super::scalar::finalize_i32(&input[i..], &mut out[i..]);
}

pub(crate) fn level_shift_f32(input: &[i32], shift: i32, out: &mut [f32]) {
    debug_assert_eq!(input.len(), out.len());
    let shift_vec = i32x8::new([shift; 8]);
    let mut i = 0usize;
    while i + 8 <= input.len() {
        let values = i32x8::new(input[i..i + 8].try_into().expect("8 lanes")) - shift_vec;
        out[i..i + 8].copy_from_slice(&values.round_float().to_array());
        i += 8;
    }
    super::scalar::level_shift_f32(&input[i..], shift, &mut out[i..]);
}

pub(crate) fn level_shift_i32(input: &[i32], shift: i32, out: &mut [i32]) {
    debug_assert_eq!(input.len(), out.len());
    let shift_vec = i32x8::new([shift; 8]);
    let mut i = 0usize;
    while i + 8 <= input.len() {
        let values = i32x8::new(input[i..i + 8].try_into().expect("8 lanes")) - shift_vec;
        out[i..i + 8].copy_from_slice(&values.to_array());
        i += 8;
    }
    super::scalar::level_shift_i32(&input[i..], shift, &mut out[i..]);
}

pub(crate) fn forward_ict_component(
    r: &[i32],
    g: &[i32],
    b: &[i32],
    component_index: usize,
    shift: i32,
    out: &mut [f32],
) {
    debug_assert_eq!(r.len(), g.len());
    debug_assert_eq!(r.len(), b.len());
    debug_assert_eq!(r.len(), out.len());

    let shift_vec = i32x8::new([shift; 8]);
    let mut i = 0usize;
    while i + 8 <= r.len() {
        let rv = (i32x8::new(r[i..i + 8].try_into().expect("8 lanes")) - shift_vec).round_float();
        let gv = (i32x8::new(g[i..i + 8].try_into().expect("8 lanes")) - shift_vec).round_float();
        let bv = (i32x8::new(b[i..i + 8].try_into().expect("8 lanes")) - shift_vec).round_float();
        let transformed = match component_index {
            0 => {
                (rv * f32x8::new([0.299; 8]) + gv * f32x8::new([0.587; 8]))
                    + bv * f32x8::new([0.114; 8])
            }
            1 => {
                (rv * f32x8::new([-0.168_75; 8]) + gv * f32x8::new([-0.331_26; 8]))
                    + bv * f32x8::new([0.5; 8])
            }
            2 => {
                (rv * f32x8::new([0.5; 8]) - gv * f32x8::new([0.418_69; 8]))
                    - bv * f32x8::new([0.081_31; 8])
            }
            _ => unreachable!("ICT only has components 0..2"),
        };
        out[i..i + 8].copy_from_slice(&transformed.to_array());
        i += 8;
    }
    super::scalar::forward_ict_component(
        &r[i..],
        &g[i..],
        &b[i..],
        component_index,
        shift,
        &mut out[i..],
    );
}

pub(crate) fn forward_rct_component(
    r: &[i32],
    g: &[i32],
    b: &[i32],
    component_index: usize,
    shift: i32,
    out: &mut [i32],
) {
    debug_assert_eq!(r.len(), g.len());
    debug_assert_eq!(r.len(), b.len());
    debug_assert_eq!(r.len(), out.len());

    let shift_vec = i32x8::new([shift; 8]);
    let mut i = 0usize;
    while i + 8 <= r.len() {
        let rv = i32x8::new(r[i..i + 8].try_into().expect("8 lanes")) - shift_vec;
        let gv = i32x8::new(g[i..i + 8].try_into().expect("8 lanes")) - shift_vec;
        let bv = i32x8::new(b[i..i + 8].try_into().expect("8 lanes")) - shift_vec;
        let transformed = match component_index {
            0 => (rv + gv + gv + bv) >> 2,
            1 => bv - gv,
            2 => rv - gv,
            _ => unreachable!("RCT only has components 0..2"),
        };
        out[i..i + 8].copy_from_slice(&transformed.to_array());
        i += 8;
    }
    super::scalar::forward_rct_component(
        &r[i..],
        &g[i..],
        &b[i..],
        component_index,
        shift,
        &mut out[i..],
    );
}

pub(crate) fn inverse_ict(y: &mut [f32], cb: &mut [f32], cr: &mut [f32]) {
    debug_assert_eq!(y.len(), cb.len());
    debug_assert_eq!(y.len(), cr.len());

    let mut i = 0usize;
    while i + 8 <= y.len() {
        let yy = f32x8::new(y[i..i + 8].try_into().expect("8 lanes"));
        let cbb = f32x8::new(cb[i..i + 8].try_into().expect("8 lanes"));
        let crr = f32x8::new(cr[i..i + 8].try_into().expect("8 lanes"));

        let r = yy + f32x8::new([1.402; 8]) * crr;
        let g = (yy - f32x8::new([0.344_13; 8]) * cbb) - f32x8::new([0.714_14; 8]) * crr;
        let b = yy + f32x8::new([1.772; 8]) * cbb;

        y[i..i + 8].copy_from_slice(&r.to_array());
        cb[i..i + 8].copy_from_slice(&g.to_array());
        cr[i..i + 8].copy_from_slice(&b.to_array());
        i += 8;
    }

    super::scalar::inverse_ict(&mut y[i..], &mut cb[i..], &mut cr[i..]);
}

pub(crate) fn inverse_rct(y: &mut [i32], db: &mut [i32], dr: &mut [i32]) {
    debug_assert_eq!(y.len(), db.len());
    debug_assert_eq!(y.len(), dr.len());

    let mut i = 0usize;
    while i + 8 <= y.len() {
        let yy = i32x8::new(y[i..i + 8].try_into().expect("8 lanes"));
        let dbv = i32x8::new(db[i..i + 8].try_into().expect("8 lanes"));
        let drv = i32x8::new(dr[i..i + 8].try_into().expect("8 lanes"));
        let g: i32x8 = yy - ((dbv + drv) >> 2);
        let r: i32x8 = drv + g;
        let b: i32x8 = dbv + g;

        y[i..i + 8].copy_from_slice(&r.to_array());
        db[i..i + 8].copy_from_slice(&g.to_array());
        dr[i..i + 8].copy_from_slice(&b.to_array());
        i += 8;
    }

    super::scalar::inverse_rct(&mut y[i..], &mut db[i..], &mut dr[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simd::scalar;

    fn test_i32_data(len: usize) -> Vec<i32> {
        (0..len)
            .map(|i| {
                let x = i as i32;
                ((x * 1_103 + 97) % 8_191) - 4_096
            })
            .collect()
    }

    #[test]
    fn analysis_matches_scalar() {
        for len in 0..129 {
            let data = test_i32_data(len);
            assert_eq!(
                i32_max_magnitude_and_nnz(&data),
                scalar::i32_max_magnitude_and_nnz(&data)
            );
        }
    }

    #[test]
    fn quantize_rect_matches_scalar() {
        let width = 23usize;
        let height = 11usize;
        let data = (0..width * height)
            .map(|i| ((i as f32 * 0.731).sin() * 1024.0) + (i as f32 % 17.0) - 8.0)
            .collect::<Vec<_>>();
        let mut scalar_out = vec![0; data.len()];
        let mut wide_out = vec![0; data.len()];
        scalar::quantize_f32_rect(&data, &mut scalar_out, width, 3, 2, 22, 10, 3.25);
        quantize_f32_rect(&data, &mut wide_out, width, 3, 2, 22, 10, 3.25);
        assert_eq!(wide_out, scalar_out);
    }

    #[test]
    fn dwt_97_matches_scalar() {
        // Widths chosen to exercise the horizontal `apply_lift_wide` batching
        // boundary (16 raw samples per vector batch) at several offsets: below
        // threshold, exactly at it, one past it, and a multi-batch width.
        for &(width, height, levels) in &[
            (8usize, 8usize, 3u8),
            (16, 6, 2),
            (17, 6, 2),
            (18, 6, 2),
            (33, 6, 2),
            (17, 11, 3),
            (64, 48, 4),
            (127, 65, 3),
        ] {
            let original = (0..width * height)
                .map(|i| ((i as f32 * 0.231).sin() * 255.0) + (i % 19) as f32)
                .collect::<Vec<_>>();

            let mut scalar_coeffs = original.clone();
            let mut wide_coeffs = original.clone();
            scalar::forward_97_2d(&mut scalar_coeffs, width, height, levels).expect("scalar dwt");
            forward_97_2d(&mut wide_coeffs, width, height, levels).expect("wide dwt");
            assert_eq!(wide_coeffs, scalar_coeffs);

            scalar::inverse_97_2d(&mut scalar_coeffs, width, height, levels)
                .expect("scalar inverse dwt");
            inverse_97_2d(&mut wide_coeffs, width, height, levels).expect("wide inverse dwt");
            assert_eq!(wide_coeffs, scalar_coeffs);
        }
    }

    #[test]
    fn dwt_97_matches_scalar_above_parallel_column_threshold() {
        // 1536x1400 exceeds DWT's PARALLEL_COLUMN_THRESHOLD (2*1024*1024) at
        // the finest resolution, exercising `apply_vertical_lift_parallel`.
        // `scalar::forward_97_2d` also takes this path (the row/column rayon
        // gate is independent of the SIMD backend), so this checks the
        // parallel snapshot-based lift is bit-exact against the sequential
        // reference under both scalar and `wide` arithmetic.
        let (width, height, levels) = (1536usize, 1400usize, 1u8);
        let original = (0..width * height)
            .map(|i| ((i as f32 * 0.037).sin() * 255.0) + (i % 23) as f32)
            .collect::<Vec<_>>();

        let mut scalar_coeffs = original.clone();
        let mut wide_coeffs = original.clone();
        scalar::forward_97_2d(&mut scalar_coeffs, width, height, levels).expect("scalar dwt");
        forward_97_2d(&mut wide_coeffs, width, height, levels).expect("wide dwt");
        assert_eq!(wide_coeffs, scalar_coeffs);

        scalar::inverse_97_2d(&mut scalar_coeffs, width, height, levels)
            .expect("scalar inverse dwt");
        inverse_97_2d(&mut wide_coeffs, width, height, levels).expect("wide inverse dwt");
        assert_eq!(wide_coeffs, scalar_coeffs);

        // 9/7 is irreversible (floating-point lifting), so forward+inverse is
        // never bit-exact to the original — only wide-vs-scalar equality is.
        const ROUNDTRIP_TOL: f32 = 1e-3;
        for (i, (&out, &orig)) in wide_coeffs.iter().zip(original.iter()).enumerate() {
            let diff = (out - orig).abs();
            assert!(
                diff < ROUNDTRIP_TOL,
                "idx={i} diff={diff} out={out} orig={orig}"
            );
        }
    }

    #[test]
    fn color_kernels_match_scalar() {
        let r = test_i32_data(137)
            .into_iter()
            .map(|v| v + 4096)
            .collect::<Vec<_>>();
        let g = test_i32_data(137)
            .into_iter()
            .map(|v| 255 - (v & 255))
            .collect::<Vec<_>>();
        let b = test_i32_data(137)
            .into_iter()
            .map(|v| (v * 3) & 255)
            .collect::<Vec<_>>();

        for component in 0..3 {
            let mut scalar_f32 = vec![0.0; r.len()];
            let mut wide_f32 = vec![0.0; r.len()];
            scalar::forward_ict_component(&r, &g, &b, component, 128, &mut scalar_f32);
            forward_ict_component(&r, &g, &b, component, 128, &mut wide_f32);
            assert_eq!(wide_f32, scalar_f32);

            let mut scalar_i32 = vec![0; r.len()];
            let mut wide_i32 = vec![0; r.len()];
            scalar::forward_rct_component(&r, &g, &b, component, 128, &mut scalar_i32);
            forward_rct_component(&r, &g, &b, component, 128, &mut wide_i32);
            assert_eq!(wide_i32, scalar_i32);
        }
    }

    #[test]
    fn inverse_color_kernels_match_scalar() {
        let mut y_scalar = test_i32_data(137)
            .into_iter()
            .map(|v| v as f32 * 0.25)
            .collect::<Vec<_>>();
        let mut cb_scalar = test_i32_data(137)
            .into_iter()
            .map(|v| v as f32 * -0.125)
            .collect::<Vec<_>>();
        let mut cr_scalar = test_i32_data(137)
            .into_iter()
            .map(|v| v as f32 * 0.5)
            .collect::<Vec<_>>();
        let (mut y_wide, mut cb_wide, mut cr_wide) =
            (y_scalar.clone(), cb_scalar.clone(), cr_scalar.clone());
        scalar::inverse_ict(&mut y_scalar, &mut cb_scalar, &mut cr_scalar);
        inverse_ict(&mut y_wide, &mut cb_wide, &mut cr_wide);
        assert_eq!(y_wide, y_scalar);
        assert_eq!(cb_wide, cb_scalar);
        assert_eq!(cr_wide, cr_scalar);

        let mut y_scalar = test_i32_data(137);
        let mut db_scalar = test_i32_data(137)
            .into_iter()
            .map(|v| v / 2)
            .collect::<Vec<_>>();
        let mut dr_scalar = test_i32_data(137)
            .into_iter()
            .map(|v| -v / 3)
            .collect::<Vec<_>>();
        let (mut y_wide, mut db_wide, mut dr_wide) =
            (y_scalar.clone(), db_scalar.clone(), dr_scalar.clone());
        scalar::inverse_rct(&mut y_scalar, &mut db_scalar, &mut dr_scalar);
        inverse_rct(&mut y_wide, &mut db_wide, &mut dr_wide);
        assert_eq!(y_wide, y_scalar);
        assert_eq!(db_wide, db_scalar);
        assert_eq!(dr_wide, dr_scalar);
    }

    #[test]
    fn dequantize_rect_matches_scalar() {
        let width = 23usize;
        let height = 11usize;
        // Includes a run of exact zeros to exercise the zero special-case lane.
        let data = (0..width * height)
            .map(|i| {
                if i % 7 == 0 {
                    0
                } else {
                    ((i as i32 * 31) % 4001) - 2000
                }
            })
            .collect::<Vec<_>>();
        let mut scalar_out = vec![0.0; data.len()];
        let mut wide_out = vec![0.0; data.len()];
        scalar::dequantize_i32_rect(&data, &mut scalar_out, width, 3, 2, 22, 10, 0.125);
        dequantize_i32_rect(&data, &mut wide_out, width, 3, 2, 22, 10, 0.125);
        assert_eq!(wide_out, scalar_out);
    }

    #[test]
    fn finalize_i32_matches_scalar() {
        let mut data = test_i32_data(133);
        data.extend([-4096, 4095, -128, 127, 0]);
        let mut scalar_out = vec![0; data.len()];
        let mut wide_out = vec![0; data.len()];
        scalar::finalize_i32(&data, &mut scalar_out);
        finalize_i32(&data, &mut wide_out);
        assert_eq!(wide_out, scalar_out);
    }

    #[test]
    fn dwt_53_matches_scalar() {
        // Widths chosen to exercise the horizontal predict/update `wide`
        // batching boundaries at several offsets (below, at, and past the
        // 8-lane threshold, plus a multi-batch width), for both even and odd
        // widths since `sn`/`dn` differ by one there.
        for &(width, height, levels) in &[
            (8usize, 8usize, 3u8),
            (9, 6, 2),
            (16, 6, 2),
            (17, 6, 2),
            (18, 6, 2),
            (33, 6, 2),
            (17, 11, 3),
            (64, 48, 4),
            (127, 65, 3),
        ] {
            let original = (0..width * height)
                .map(|i| ((i as i32 * 37) % 511) - 255)
                .collect::<Vec<_>>();

            let mut scalar_coeffs = original.clone();
            let mut wide_coeffs = original.clone();
            scalar::forward_53_2d(&mut scalar_coeffs, width, height, levels).expect("scalar dwt");
            forward_53_2d(&mut wide_coeffs, width, height, levels).expect("wide dwt");
            assert_eq!(wide_coeffs, scalar_coeffs);

            scalar::inverse_53_2d(&mut scalar_coeffs, width, height, levels)
                .expect("scalar inverse dwt");
            inverse_53_2d(&mut wide_coeffs, width, height, levels).expect("wide inverse dwt");
            assert_eq!(wide_coeffs, scalar_coeffs);
            assert_eq!(wide_coeffs, original);
        }
    }

    #[test]
    fn dwt_53_matches_scalar_above_parallel_column_threshold() {
        // 1536x1400 (even height) and 1536x1401 (odd height, sn = dn+1) both
        // exceed rev53's PARALLEL_COLUMN_THRESHOLD (2*1024*1024) at the
        // finest resolution, exercising the split_at_mut-based forward
        // predict/update parallelism and the snapshot-based inverse
        // predict/update parallelism on both the even and odd chunk-count
        // boundary cases. 5/3 is reversible, so exact roundtrip-to-original
        // is a valid assertion here (unlike 9/7's float lifting).
        for &(width, height, levels) in &[(1536usize, 1400usize, 1u8), (1536, 1401, 1)] {
            let original = (0..width * height)
                .map(|i| ((i as i32 * 37) % 511) - 255)
                .collect::<Vec<_>>();

            let mut scalar_coeffs = original.clone();
            let mut wide_coeffs = original.clone();
            scalar::forward_53_2d(&mut scalar_coeffs, width, height, levels).expect("scalar dwt");
            forward_53_2d(&mut wide_coeffs, width, height, levels).expect("wide dwt");
            assert_eq!(wide_coeffs, scalar_coeffs);

            scalar::inverse_53_2d(&mut scalar_coeffs, width, height, levels)
                .expect("scalar inverse dwt");
            inverse_53_2d(&mut wide_coeffs, width, height, levels).expect("wide inverse dwt");
            assert_eq!(wide_coeffs, scalar_coeffs);
            assert_eq!(wide_coeffs, original);
        }
    }
}
