use crate::error::Result;

use super::{AnalyzePrimitives, ColorPrimitives, DwtPrimitives, Primitives, QuantPrimitives};

pub(crate) fn primitives() -> Primitives {
    Primitives {
        dwt: DwtPrimitives {
            forward_97_2d,
            inverse_97_2d,
            forward_53_2d,
            inverse_53_2d,
        },
        analyze: AnalyzePrimitives {
            i32_max_magnitude_and_nnz,
        },
        quant: QuantPrimitives {
            quantize_f32_rect,
            dequantize_i32_rect,
        },
        color: ColorPrimitives {
            level_shift_f32,
            level_shift_i32,
            forward_ict_component,
            forward_rct_component,
            inverse_ict,
            inverse_rct,
            finalize_i32,
            finalize_f32,
        },
        backend: "scalar",
    }
}

pub(crate) fn forward_97_2d(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    crate::dwt::forward_97_2d_in_place(data, width, height, levels)
}

pub(crate) fn inverse_97_2d(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    crate::dwt::inverse_97_2d_in_place(data, width, height, levels);
    Ok(())
}

pub(crate) fn forward_53_2d(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    crate::dwt::forward_53_2d_in_place(data, width, height, levels)
}

pub(crate) fn inverse_53_2d(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    crate::dwt::inverse_53_2d_in_place(data, width, height, levels)
}

pub(crate) fn i32_max_magnitude_and_nnz(coefficients: &[i32]) -> (u32, usize) {
    coefficients
        .iter()
        .fold((0u32, 0usize), |(max_mag, nonzero), &value| {
            let mag = value.unsigned_abs();
            let new_max = max_mag.max(mag);
            let new_nonzero = if value != 0 { nonzero + 1 } else { nonzero };
            (new_max, new_nonzero)
        })
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
    for y in y0..y1 {
        let row = y * stride;
        for x in x0..x1 {
            out[row + x] = quantize_f32_to_i32(data[row + x], step);
        }
    }
}

pub(crate) fn quantize_f32_to_i32(v: f32, step: f32) -> i32 {
    if step <= 0.0 || !v.is_finite() {
        return 0;
    }
    let q = (v / step).trunc();
    q.clamp(i32::MIN as f32, i32::MAX as f32) as i32
}

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
    for y in y0..y1 {
        let row = y * stride;
        for x in x0..x1 {
            output[row + x] = dequantize_i32_to_f32(input[row + x], step);
        }
    }
}

pub(crate) fn dequantize_i32_to_f32(q: i32, step: f32) -> f32 {
    if q == 0 {
        0.0
    } else {
        let sign = if q >= 0 { 1.0 } else { -1.0 };
        sign * (q.unsigned_abs() as f32 + 0.5) * step
    }
}

pub(crate) fn level_shift_f32(input: &[i32], shift: i32, out: &mut [f32]) {
    debug_assert_eq!(input.len(), out.len());
    for (&sample, dst) in input.iter().zip(out.iter_mut()) {
        *dst = (sample - shift) as f32;
    }
}

pub(crate) fn level_shift_i32(input: &[i32], shift: i32, out: &mut [i32]) {
    debug_assert_eq!(input.len(), out.len());
    for (&sample, dst) in input.iter().zip(out.iter_mut()) {
        *dst = sample - shift;
    }
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
    for i in 0..r.len() {
        let rf = (r[i] - shift) as f32;
        let gf = (g[i] - shift) as f32;
        let bf = (b[i] - shift) as f32;
        out[i] = match component_index {
            0 => 0.299f32 * rf + 0.587f32 * gf + 0.114f32 * bf,
            1 => -0.168_75f32 * rf + -0.331_26f32 * gf + 0.5f32 * bf,
            2 => 0.5f32 * rf - 0.418_69f32 * gf - 0.081_31f32 * bf,
            _ => unreachable!("ICT only has components 0..2"),
        };
    }
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
    for i in 0..r.len() {
        let rv = r[i] - shift;
        let gv = g[i] - shift;
        let bv = b[i] - shift;
        out[i] = match component_index {
            0 => (rv + 2 * gv + bv) >> 2,
            1 => bv - gv,
            2 => rv - gv,
            _ => unreachable!("RCT only has components 0..2"),
        };
    }
}

pub(crate) fn inverse_ict(y: &mut [f32], cb: &mut [f32], cr: &mut [f32]) {
    debug_assert_eq!(y.len(), cb.len());
    debug_assert_eq!(y.len(), cr.len());
    for i in 0..y.len() {
        let yy = y[i];
        let cbb = cb[i];
        let crr = cr[i];
        y[i] = yy + 1.402f32 * crr;
        cb[i] = yy - 0.344_13f32 * cbb - 0.714_14f32 * crr;
        cr[i] = yy + 1.772f32 * cbb;
    }
}

pub(crate) fn inverse_rct(y: &mut [i32], db: &mut [i32], dr: &mut [i32]) {
    debug_assert_eq!(y.len(), db.len());
    debug_assert_eq!(y.len(), dr.len());
    for i in 0..y.len() {
        let yy = y[i];
        let dbv = db[i];
        let drv = dr[i];
        let g = yy - ((dbv + drv) >> 2);
        y[i] = drv + g;
        db[i] = g;
        dr[i] = dbv + g;
    }
}

pub(crate) fn finalize_i32(input: &[i32], out: &mut [i32]) {
    debug_assert_eq!(input.len(), out.len());
    for (&sample, dst) in input.iter().zip(out.iter_mut()) {
        *dst = (sample + 128).clamp(0, 255);
    }
}

pub(crate) fn finalize_f32(input: &[f32], out: &mut [i32]) {
    debug_assert_eq!(input.len(), out.len());
    for (&sample, dst) in input.iter().zip(out.iter_mut()) {
        *dst = (sample + 128.0).round().clamp(0.0, 255.0) as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_analysis_handles_signed_extremes() {
        let input = [0, 1, -2, i32::MAX, i32::MIN + 1];
        assert_eq!(
            i32_max_magnitude_and_nnz(&input),
            ((i32::MIN + 1).unsigned_abs(), 4)
        );
    }

    #[test]
    fn scalar_quantization_truncates_toward_zero() {
        let input = [-3.9, -1.0, -0.9, 0.0, 0.9, 1.0, 3.9];
        let mut out = [0; 7];
        quantize_f32_rect(&input, &mut out, 7, 0, 0, 7, 1, 1.0);
        assert_eq!(out, [-3, -1, 0, 0, 0, 1, 3]);
    }

    #[test]
    fn scalar_dequantization_uses_midpoint_reconstruction() {
        let input = [-2, -1, 0, 1, 2];
        let mut out = [0.0; 5];
        dequantize_i32_rect(&input, &mut out, 5, 0, 0, 5, 1, 0.5);
        assert_eq!(out, [-1.25, -0.75, 0.0, 0.75, 1.25]);
    }
}
