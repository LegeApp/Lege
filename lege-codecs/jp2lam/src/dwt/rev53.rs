use crate::encode::profile_enter;
use crate::error::{Jp2LamError, Result};
use rayon::prelude::*;
#[cfg(feature = "simd")]
use wide::i32x8;

// Disabled by default — see the matching constant in `dwt::irrev97` for why:
// both a snapshot-copy and a zero-copy version of this parallelism measured
// as net regressions on real images. Kept and directly unit-tested (see
// `dwt::rev53::tests`) for a future lower-overhead attempt.
const PARALLEL_COLUMN_THRESHOLD: usize = usize::MAX / 2;

// Number of columns processed per vertical-lift band. The vertical 5/3 lift is
// independent per column, so processing columns in bounded bands keeps the
// working set to `VERTICAL_BAND_COLS * height` instead of a full `width *
// height` scratch plane, while still deinterleaving into contiguous, SIMD- and
// cache-friendly band rows. See `forward_53_vertical_even_in_place`.
const VERTICAL_BAND_COLS: usize = 256;

#[cfg(test)]
pub(crate) fn forward_53_2d_in_place(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    forward_53_2d_in_place_impl(data, width, height, levels, false)
}

/// Annex F reversible 5/3 transform for a tile-component whose sample bounds
/// begin at `(x0, y0)` on the component reference grid. At each decomposition
/// level, low-pass samples are the even reference-grid coordinates, so the
/// lifting phase and low/high lengths follow the recursively ceil-divided
/// bounds rather than the local buffer index alone.
pub(crate) fn forward_53_2d_in_place_at(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
    x0: usize,
    y0: usize,
) -> Result<()> {
    validate_area(data.len(), width, height)?;
    if width == 0 || height == 0 || levels == 0 {
        return Ok(());
    }
    let steps = phase_steps(x0, y0, width, height, levels);
    crate::encode::counters::record_dwt_scratch(width.max(height) * 2 * std::mem::size_of::<i32>());
    let mut line = vec![0i32; width.max(height)];
    let mut scratch = vec![0i32; width.max(height)];
    for &(rw, rh, x_even, y_even) in &steps {
        for x in 0..rw {
            gather_column(data, width, rh, x, &mut line);
            forward_53_1d_with_scratch(&mut line[..rh], y_even, &mut scratch[..rh], false);
            scatter_column(data, width, rh, x, &line);
        }
        for y in 0..rh {
            let start = y * width;
            forward_53_1d_with_scratch(
                &mut data[start..start + rw],
                x_even,
                &mut scratch[..rw],
                false,
            );
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "simd"))]
pub(crate) fn forward_53_2d_in_place_wide(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    forward_53_2d_in_place_impl(data, width, height, levels, true)
}

#[cfg(test)]
fn forward_53_2d_in_place_impl(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
    use_wide: bool,
) -> Result<()> {
    let _p = profile_enter("dwt::forward_53_2d_in_place");
    let expected_len = width
        .checked_mul(height)
        .ok_or_else(|| Jp2LamError::EncodeFailed("DWT image dimensions overflow".to_string()))?;
    if data.len() != expected_len {
        return Err(Jp2LamError::EncodeFailed(format!(
            "DWT input length {} did not match image area {expected_len}",
            data.len()
        )));
    }
    if width == 0 || height == 0 || levels == 0 {
        return Ok(());
    }

    let resolutions = encode_resolutions(width, height, levels);
    let max_span = width.max(height);
    crate::encode::counters::record_dwt_scratch(max_span * 4 * std::mem::size_of::<i32>());
    let mut work = vec![0i32; max_span * 3];
    let mut vertical = vec![0i32; vertical_scratch_len(width, height)];

    for &(rw, rh) in resolutions.iter().skip(1).rev() {
        forward_53_vertical_even_in_place(data, width, rw, rh, &mut vertical, use_wide);

        for y in 0..rh {
            let row_start = y * width;
            forward_53_1d_with_scratch(
                &mut data[row_start..row_start + rw],
                true,
                &mut work[..rw],
                use_wide,
            );
        }
    }

    Ok(())
}

pub(crate) fn inverse_53_2d_in_place(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    inverse_53_2d_in_place_impl(data, width, height, levels, false, None)
}

pub(crate) fn inverse_53_2d_in_place_at(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
    x0: usize,
    y0: usize,
) -> Result<()> {
    validate_area(data.len(), width, height)?;
    if width == 0 || height == 0 || levels == 0 {
        return Ok(());
    }
    let steps = phase_steps(x0, y0, width, height, levels);
    if !inverse_fast_path_is_safe(data, levels) {
        return inverse_53_checked_fallback(data, width, &steps);
    }
    let max_span = width.max(height);
    let mut line = vec![0i32; max_span];
    let mut scratch = vec![0i32; max_span * 3];
    for &(rw, rh, x_even, y_even) in steps.iter().rev() {
        for y in 0..rh {
            let start = y * width;
            inverse_53_1d_with_scratch(
                &mut data[start..start + rw],
                x_even,
                &mut scratch[..rw * 3],
                false,
            );
        }
        for x in 0..rw {
            gather_column(data, width, rh, x, &mut line);
            inverse_53_1d_with_scratch(&mut line[..rh], y_even, &mut scratch[..rh * 3], false);
            scatter_column(data, width, rh, x, &line);
        }
    }
    Ok(())
}

fn validate_area(actual: usize, width: usize, height: usize) -> Result<()> {
    let expected = width
        .checked_mul(height)
        .ok_or_else(|| Jp2LamError::EncodeFailed("DWT image dimensions overflow".into()))?;
    if actual != expected {
        return Err(Jp2LamError::EncodeFailed(format!(
            "DWT input length {actual} did not match image area {expected}"
        )));
    }
    Ok(())
}

fn phase_steps(
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
    levels: u8,
) -> Vec<(usize, usize, bool, bool)> {
    let (mut ax, mut ay) = (x0, y0);
    let (mut bx, mut by) = (x0 + width, y0 + height);
    let mut steps = Vec::with_capacity(usize::from(levels));
    for _ in 0..levels {
        steps.push((bx - ax, by - ay, ax.is_multiple_of(2), ay.is_multiple_of(2)));
        ax = ax.div_ceil(2);
        ay = ay.div_ceil(2);
        bx = bx.div_ceil(2);
        by = by.div_ceil(2);
    }
    steps
}

#[cfg(feature = "simd")]
pub(crate) fn inverse_53_2d_in_place_wide(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    inverse_53_2d_in_place_impl(data, width, height, levels, true, None)
}

pub(crate) fn inverse_53_2d_in_place_profiled(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
    use_wide: bool,
) -> Result<crate::dwt::InverseDwtTiming> {
    let mut timing = crate::dwt::InverseDwtTiming::default();
    inverse_53_2d_in_place_impl(data, width, height, levels, use_wide, Some(&mut timing))?;
    Ok(timing)
}

fn inverse_53_2d_in_place_impl(
    data: &mut [i32],
    width: usize,
    height: usize,
    levels: u8,
    use_wide: bool,
    mut timing: Option<&mut crate::dwt::InverseDwtTiming>,
) -> Result<()> {
    let _p = profile_enter("dwt::inverse_53_2d_in_place");
    let expected_len = width
        .checked_mul(height)
        .ok_or_else(|| Jp2LamError::EncodeFailed("DWT image dimensions overflow".to_string()))?;
    if data.len() != expected_len {
        return Err(Jp2LamError::EncodeFailed(format!(
            "DWT input length {} did not match image area {expected_len}",
            data.len()
        )));
    }
    if width == 0 || height == 0 || levels == 0 {
        return Ok(());
    }
    if !inverse_fast_path_is_safe(data, levels) {
        let steps = phase_steps(0, 0, width, height, levels);
        return inverse_53_checked_fallback(data, width, &steps);
    }

    let resolutions = encode_resolutions(width, height, levels);
    let max_span = width.max(height);
    let mut work = vec![0i32; max_span * 3];
    let mut vertical = vec![0i32; vertical_scratch_len(width, height)];

    for &(rw, rh) in resolutions.iter().skip(1) {
        let horizontal_start = timing.as_ref().map(|_| std::time::Instant::now());
        for y in 0..rh {
            let row_start = y * width;
            inverse_53_1d_even_with_scratch(
                &mut data[row_start..row_start + rw],
                &mut work[..rw * 3],
                use_wide,
            );
        }
        let horizontal = horizontal_start.map(|start| start.elapsed());

        let vertical_start = timing.as_ref().map(|_| std::time::Instant::now());
        inverse_53_vertical_even_in_place(data, width, rw, rh, &mut vertical, use_wide);
        if let (Some(timing), Some(horizontal), Some(vertical_start)) =
            (timing.as_deref_mut(), horizontal, vertical_start)
        {
            timing.record_level(horizontal, vertical_start.elapsed());
        }
    }

    Ok(())
}

/// The optimized inverse path stores every lifting intermediate in `i32`.
/// Bound two one-dimensional lifting passes per level before entering SIMD or
/// parallel arithmetic. Suspicious coefficient sets use the exact checked
/// `i128` fallback below, so malformed streams cannot panic or wrap.
fn inverse_fast_path_is_safe(data: &[i32], levels: u8) -> bool {
    let mut bound = data
        .iter()
        .map(|value| i128::from(value.unsigned_abs()))
        .max()
        .unwrap_or(0);
    for _ in 0..usize::from(levels).saturating_mul(2) {
        let updated = bound + ((bound * 2 + 2) >> 2);
        let predicted = bound + updated;
        bound = updated.max(predicted);
        if bound > i128::from(i32::MAX) {
            return false;
        }
    }
    true
}

fn inverse_53_checked_fallback(
    data: &mut [i32],
    stride: usize,
    steps: &[(usize, usize, bool, bool)],
) -> Result<()> {
    let mut plane = Vec::new();
    plane
        .try_reserve_exact(data.len())
        .map_err(|_| Jp2LamError::DecodeFailed("checked DWT plane allocation failed".into()))?;
    plane.extend(data.iter().copied().map(i128::from));
    let max_span = steps
        .iter()
        .map(|&(width, height, _, _)| width.max(height))
        .max()
        .unwrap_or(0);
    let mut line = vec![0i128; max_span];
    let mut out = vec![0i128; max_span];

    for &(width, height, x_even, y_even) in steps.iter().rev() {
        for y in 0..height {
            let start = y
                .checked_mul(stride)
                .ok_or_else(|| Jp2LamError::DecodeFailed("checked DWT row overflow".into()))?;
            inverse_53_1d_i128(&mut plane[start..start + width], x_even, &mut out[..width]);
        }
        for x in 0..width {
            for y in 0..height {
                line[y] = plane[y * stride + x];
            }
            inverse_53_1d_i128(&mut line[..height], y_even, &mut out[..height]);
            for y in 0..height {
                plane[y * stride + x] = line[y];
            }
        }
    }

    for (destination, value) in data.iter_mut().zip(plane) {
        *destination = i32::try_from(value).map_err(|_| {
            Jp2LamError::DecodeFailed(
                "reversible 5/3 reconstruction exceeds signed 32-bit storage".into(),
            )
        })?;
    }
    Ok(())
}

fn inverse_53_1d_i128(coefficients: &mut [i128], even: bool, out: &mut [i128]) {
    let width = coefficients.len();
    if width == 0 {
        return;
    }
    if width == 1 {
        if !even {
            coefficients[0] /= 2;
        }
        return;
    }

    if even {
        let low_count = width.div_ceil(2);
        let high_count = width - low_count;
        let (low, high) = coefficients.split_at(low_count);
        for index in 0..low_count {
            let left = high[index.saturating_sub(1).min(high_count - 1)];
            let right = high[index.min(high_count - 1)];
            out[2 * index] = low[index] - ((left + right + 2) >> 2);
        }
        for index in 0..high_count {
            let left = out[2 * index];
            let right = out[(2 * index + 2).min(width - 1)];
            out[2 * index + 1] = high[index] + ((left + right) >> 1);
        }
    } else {
        let low_count = width / 2;
        let high_count = width - low_count;
        let (low, high) = coefficients.split_at(low_count);
        for index in 0..low_count {
            let right = high[(index + 1).min(high_count - 1)];
            out[2 * index + 1] = low[index] - ((high[index] + right + 2) >> 2);
        }
        out[0] = high[0] + out[1];
        for index in 1..low_count {
            out[2 * index] = high[index] + ((out[2 * index - 1] + out[2 * index + 1]) >> 1);
        }
        if !width.is_multiple_of(2) {
            out[width - 1] = high[high_count - 1] + out[width - 2];
        }
    }
    coefficients.copy_from_slice(&out[..width]);
}

fn forward_53_1d_with_scratch(
    samples: &mut [i32],
    even: bool,
    scratch: &mut [i32],
    use_wide: bool,
) {
    let width = samples.len();
    if width <= 1 {
        if !even && width == 1 {
            samples[0] *= 2;
        }
        return;
    }

    let sn = (width + if even { 1 } else { 0 }) >> 1;
    let dn = width - sn;
    let scratch = &mut scratch[..width];
    if even {
        for value in scratch.iter_mut() {
            *value = 0;
        }
        for (index, &sample) in samples.iter().enumerate() {
            if index.is_multiple_of(2) {
                scratch[index / 2] = sample;
            } else {
                scratch[sn + index / 2] = sample;
            }
        }

        // Reversible 5/3 predict step on odd samples:
        // scratch[sn+i] -= (scratch[i] + scratch[(i+1).min(sn-1)]) >> 1, for i in 0..dn.
        forward_predict_horizontal(scratch, sn, dn, use_wide);

        // Reversible 5/3 update step on even samples:
        // scratch[i] += (scratch[sn+i.saturating_sub(1).min(dn-1)] + scratch[sn+i.min(dn-1)] + 2) >> 2, for i in 0..sn.
        forward_update_horizontal(scratch, sn, dn, use_wide);

        samples.copy_from_slice(scratch);
    } else {
        scratch[sn] = samples[0] - samples[1];
        for i in 1..sn {
            scratch[sn + i] =
                samples[2 * i] - ((samples[2 * i + 1] + samples[2 * (i - 1) + 1]) >> 1);
        }
        if !width.is_multiple_of(2) {
            let i = sn;
            scratch[sn + i] = samples[2 * i] - samples[2 * (i - 1) + 1];
        }

        for i in 0..dn.saturating_sub(1) {
            samples[i] = samples[2 * i + 1] + ((scratch[sn + i] + scratch[sn + i + 1] + 2) >> 2);
        }
        if width.is_multiple_of(2) {
            let i = dn - 1;
            samples[i] = samples[2 * i + 1] + ((scratch[sn + i] + scratch[sn + i] + 2) >> 2);
        }
        samples[sn..sn + dn].copy_from_slice(&scratch[sn..sn + dn]);
    }
}

/// `scratch[sn+i] -= (scratch[i] + scratch[(i+1).min(sn-1)]) >> 1`, for `i in 0..dn`.
/// The low (`0..sn`) and high (`sn..sn+dn`) regions of `scratch` never overlap,
/// so the vector loop below is a plain read-then-write with no aliasing.
#[inline]
fn forward_predict_horizontal(scratch: &mut [i32], sn: usize, dn: usize, use_wide: bool) {
    let mut i = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        // Interior where `i+1 < sn`, i.e. `right` reads unclamped.
        let safe_len = dn.min(sn.saturating_sub(1));
        while i + 8 <= safe_len {
            let left = i32x8::new(scratch[i..i + 8].try_into().expect("8 lanes"));
            let right = i32x8::new(scratch[i + 1..i + 9].try_into().expect("8 lanes"));
            let target = i32x8::new(scratch[sn + i..sn + i + 8].try_into().expect("8 lanes"));
            let lifted = target - ((left + right) >> 1i32);
            scratch[sn + i..sn + i + 8].copy_from_slice(&lifted.to_array());
            i += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while i < dn {
        let left = scratch[i];
        let right = scratch[(i + 1).min(sn - 1)];
        scratch[sn + i] -= (left + right) >> 1;
        i += 1;
    }
}

/// `scratch[i] += (scratch[sn+i.saturating_sub(1).min(dn-1)] + scratch[sn+i.min(dn-1)] + 2) >> 2`,
/// for `i in 0..sn`. `i == 0` is a genuine formula special case (both neighbor
/// indices clamp to `sn+0`), handled scalar; the vector loop only covers `i in
/// 1..dn` where both neighbor reads are unclamped and contiguous.
#[inline]
fn forward_update_horizontal(scratch: &mut [i32], sn: usize, dn: usize, use_wide: bool) {
    if sn == 0 {
        return;
    }
    let both = scratch[sn];
    scratch[0] += (both + both + 2) >> 2;

    let mut i = 1usize;
    #[cfg(feature = "simd")]
    if use_wide {
        let two = i32x8::new([2; 8]);
        while i + 8 <= dn {
            let left = i32x8::new(scratch[sn + i - 1..sn + i + 7].try_into().expect("8 lanes"));
            let right = i32x8::new(scratch[sn + i..sn + i + 8].try_into().expect("8 lanes"));
            let target = i32x8::new(scratch[i..i + 8].try_into().expect("8 lanes"));
            let lifted = target + ((left + right + two) >> 2i32);
            scratch[i..i + 8].copy_from_slice(&lifted.to_array());
            i += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while i < sn {
        let left = scratch[sn + i.saturating_sub(1).min(dn - 1)];
        let right = scratch[sn + i.min(dn - 1)];
        scratch[i] += (left + right + 2) >> 2;
        i += 1;
    }
}

fn inverse_53_1d_with_scratch(
    coefficients: &mut [i32],
    even: bool,
    scratch: &mut [i32],
    use_wide: bool,
) {
    if even {
        inverse_53_1d_even_with_scratch(coefficients, scratch, use_wide);
    } else {
        inverse_53_1d_odd_with_scratch(coefficients, scratch);
    }
}

fn inverse_53_1d_odd_with_scratch(coefficients: &mut [i32], scratch: &mut [i32]) {
    let width = coefficients.len();
    if width == 0 {
        return;
    }
    if width == 1 {
        coefficients[0] /= 2;
        return;
    }

    let low_count = width / 2;
    let high_count = width - low_count;
    let (low, high_and_out) = scratch[..low_count + high_count + width].split_at_mut(low_count);
    low.copy_from_slice(&coefficients[..low_count]);
    let (high, out) = high_and_out.split_at_mut(high_count);
    high.copy_from_slice(&coefficients[low_count..]);

    // Undo the update on global-even low-pass positions, which are local odd
    // samples for an odd tile-component origin.
    for i in 0..low_count {
        let right = if i + 1 < high_count {
            high[i + 1]
        } else {
            high[i]
        };
        out[2 * i + 1] = low[i] - ((high[i] + right + 2) >> 2);
    }

    // Undo prediction on global-odd high-pass positions (local evens).
    out[0] = high[0] + out[1];
    for i in 1..low_count {
        out[2 * i] = high[i] + ((out[2 * i - 1] + out[2 * i + 1]) >> 1);
    }
    if !width.is_multiple_of(2) {
        out[width - 1] = high[high_count - 1] + out[width - 2];
    }
    coefficients.copy_from_slice(out);
}

fn inverse_53_1d_even_with_scratch(coefficients: &mut [i32], scratch: &mut [i32], use_wide: bool) {
    let width = coefficients.len();
    if width <= 1 {
        return;
    }

    let sn = width.div_ceil(2);
    let dn = width - sn;
    let low_start = 0;
    let high_start = sn;
    let even_start = width;
    let out_start = width + sn;
    let scratch = &mut scratch[..out_start + width];
    scratch[low_start..low_start + sn].copy_from_slice(&coefficients[..sn]);
    scratch[high_start..high_start + dn].copy_from_slice(&coefficients[sn..]);

    if dn == 0 {
        return;
    }

    // Undo the reversible 5/3 update step on even samples.
    scratch[even_start] =
        scratch[low_start] - ((scratch[high_start] + scratch[high_start] + 2) >> 2);
    inverse_undo_update_horizontal(scratch, low_start, high_start, even_start, sn, dn, use_wide);

    // Undo the reversible 5/3 predict step on odd samples, then interleave.
    for i in 0..sn {
        scratch[out_start + 2 * i] = scratch[even_start + i];
    }
    inverse_undo_predict_horizontal(scratch, high_start, even_start, out_start, sn, dn, use_wide);

    coefficients.copy_from_slice(&scratch[out_start..out_start + width]);
}

/// `scratch[even_start+i] = scratch[low_start+i] - ((scratch[high_start+i-1] + right + 2) >> 2)`,
/// for `i in 1..sn`, where `right = scratch[high_start+i]` if `i < dn` else
/// `scratch[high_start+i-1]`. The vector loop covers `i in 1..dn` where `right`
/// is unclamped; `low_start`/`high_start`/`even_start` are disjoint regions
/// (`0..sn`, `sn..width`, `width..width+sn`), so there is no aliasing.
#[inline]
#[allow(clippy::too_many_arguments)]
fn inverse_undo_update_horizontal(
    scratch: &mut [i32],
    low_start: usize,
    high_start: usize,
    even_start: usize,
    sn: usize,
    dn: usize,
    use_wide: bool,
) {
    let mut i = 1usize;
    #[cfg(feature = "simd")]
    if use_wide {
        let two = i32x8::new([2; 8]);
        while i + 8 <= dn {
            let low = i32x8::new(
                scratch[low_start + i..low_start + i + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let high_left = i32x8::new(
                scratch[high_start + i - 1..high_start + i + 7]
                    .try_into()
                    .expect("8 lanes"),
            );
            let right = i32x8::new(
                scratch[high_start + i..high_start + i + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let result = low - ((high_left + right + two) >> 2i32);
            scratch[even_start + i..even_start + i + 8].copy_from_slice(&result.to_array());
            i += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while i < sn {
        let right = if i < dn {
            scratch[high_start + i]
        } else {
            scratch[high_start + i - 1]
        };
        scratch[even_start + i] =
            scratch[low_start + i] - ((scratch[high_start + i - 1] + right + 2) >> 2);
        i += 1;
    }
}

/// `scratch[out_start+2*i+1] = scratch[high_start+i] + ((scratch[even_start+i] +
/// scratch[even_start+i+1]) >> 1)`, for `i in 0..dn`, with the last element
/// (when `i+1 >= sn`) reusing `scratch[even_start+i]` for both operands. Reads
/// are contiguous; writes are strided (every other position), so the vector
/// batch computes 8 lanes then scatter-stores them individually, mirroring
/// `apply_lift_wide` in `irrev97.rs`.
#[inline]
#[allow(clippy::too_many_arguments)]
fn inverse_undo_predict_horizontal(
    scratch: &mut [i32],
    high_start: usize,
    even_start: usize,
    out_start: usize,
    sn: usize,
    dn: usize,
    use_wide: bool,
) {
    let mut i = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        // Interior where `i+1 < sn`, i.e. the `even_start+i+1` read is unclamped.
        let safe_len = dn.min(sn.saturating_sub(1));
        while i + 8 <= safe_len {
            let high = i32x8::new(
                scratch[high_start + i..high_start + i + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let even = i32x8::new(
                scratch[even_start + i..even_start + i + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let even_next = i32x8::new(
                scratch[even_start + i + 1..even_start + i + 9]
                    .try_into()
                    .expect("8 lanes"),
            );
            let result = high + ((even + even_next) >> 1i32);
            let arr = result.to_array();
            for (k, &value) in arr.iter().enumerate() {
                scratch[out_start + 2 * (i + k) + 1] = value;
            }
            i += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while i < dn {
        scratch[out_start + 2 * i + 1] = if i + 1 < sn {
            scratch[high_start + i] + ((scratch[even_start + i] + scratch[even_start + i + 1]) >> 1)
        } else {
            scratch[high_start + i] + scratch[even_start + i]
        };
        i += 1;
    }
}

#[cfg(test)]
fn forward_53_vertical_even_in_place(
    data: &mut [i32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    scratch: &mut [i32],
    use_wide: bool,
) {
    if active_width == 0 || active_height <= 1 {
        return;
    }

    let sn = active_height.div_ceil(2);
    let dn = active_height - sn;

    for band_x in (0..active_width).step_by(VERTICAL_BAND_COLS) {
        let band_width = VERTICAL_BAND_COLS.min(active_width - band_x);
        let scratch = &mut scratch[..band_width * active_height];

        for i in 0..sn {
            let src = (2 * i) * stride + band_x;
            let dst = i * band_width;
            scratch[dst..dst + band_width].copy_from_slice(&data[src..src + band_width]);
        }
        for i in 0..dn {
            let src = (2 * i + 1) * stride + band_x;
            let dst = (sn + i) * band_width;
            scratch[dst..dst + band_width].copy_from_slice(&data[src..src + band_width]);
        }

        if band_width.saturating_mul(active_height) >= PARALLEL_COLUMN_THRESHOLD {
            // The predict step only ever reads the low region (`0..sn*band_width`)
            // and only ever writes the high region (`sn*band_width..`); those
            // regions are disjoint by construction (the deinterleave above put
            // "even" source rows in `0..sn` and "odd" source rows in `sn..`), so
            // `split_at_mut` gives a real compiler-checked proof of no aliasing —
            // no snapshot copy needed, unlike the interleaved-buffer cases.
            let (low, high) = scratch.split_at_mut(sn * band_width);
            high[..dn * band_width]
                .par_chunks_mut(band_width)
                .enumerate()
                .for_each(|(i, high_row)| {
                    let left = i * band_width;
                    let right = (i + 1).min(sn - 1) * band_width;
                    predict_row_split(low, left, right, high_row, use_wide);
                });
        } else {
            for i in 0..dn {
                let left = i * band_width;
                let right = (i + 1).min(sn - 1) * band_width;
                let high = (sn + i) * band_width;
                predict_row(scratch, left, right, high, band_width, use_wide);
            }
        }

        if band_width.saturating_mul(active_height) >= PARALLEL_COLUMN_THRESHOLD {
            // Same disjointness argument as the predict step, mirrored: update
            // only reads the high region and only writes the low region.
            let (low, high) = scratch.split_at_mut(sn * band_width);
            low[..sn * band_width]
                .par_chunks_mut(band_width)
                .enumerate()
                .for_each(|(i, low_row)| {
                    let left = i.saturating_sub(1).min(dn - 1) * band_width;
                    let right = i.min(dn - 1) * band_width;
                    update_row_split(high, left, right, low_row, use_wide);
                });
        } else {
            for i in 0..sn {
                let left = (sn + i.saturating_sub(1).min(dn - 1)) * band_width;
                let right = (sn + i.min(dn - 1)) * band_width;
                let low = i * band_width;
                update_row(scratch, left, right, low, band_width, use_wide);
            }
        }

        for y in 0..active_height {
            let src = y * band_width;
            let dst = y * stride + band_x;
            data[dst..dst + band_width].copy_from_slice(&scratch[src..src + band_width]);
        }
    }
}

/// `scratch[high+x] -= (scratch[left+x] + scratch[right+x]) >> 1` over `0..width`.
#[inline]
#[cfg(test)]
fn predict_row(
    scratch: &mut [i32],
    left: usize,
    right: usize,
    high: usize,
    width: usize,
    use_wide: bool,
) {
    let mut x = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        while x + 8 <= width {
            let l = i32x8::new(scratch[left + x..left + x + 8].try_into().expect("8 lanes"));
            let r = i32x8::new(
                scratch[right + x..right + x + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let h = i32x8::new(scratch[high + x..high + x + 8].try_into().expect("8 lanes"));
            let lifted = h - ((l + r) >> 1i32);
            scratch[high + x..high + x + 8].copy_from_slice(&lifted.to_array());
            x += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while x < width {
        scratch[high + x] -= (scratch[left + x] + scratch[right + x]) >> 1;
        x += 1;
    }
}

/// `scratch[low+x] += (scratch[left+x] + scratch[right+x] + 2) >> 2` over `0..width`.
#[inline]
#[cfg(test)]
fn update_row(
    scratch: &mut [i32],
    left: usize,
    right: usize,
    low: usize,
    width: usize,
    use_wide: bool,
) {
    let mut x = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        let two = i32x8::new([2; 8]);
        while x + 8 <= width {
            let l = i32x8::new(scratch[left + x..left + x + 8].try_into().expect("8 lanes"));
            let r = i32x8::new(
                scratch[right + x..right + x + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let lo = i32x8::new(scratch[low + x..low + x + 8].try_into().expect("8 lanes"));
            let lifted = lo + ((l + r + two) >> 2i32);
            scratch[low + x..low + x + 8].copy_from_slice(&lifted.to_array());
            x += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while x < width {
        scratch[low + x] += (scratch[left + x] + scratch[right + x] + 2) >> 2;
        x += 1;
    }
}

/// `high_row[x] -= (low[left+x] + low[right+x]) >> 1` over `0..high_row.len()`.
/// Same arithmetic as `predict_row`, but reading `low` and writing `high_row`
/// as separate slices (the halves of a `split_at_mut`) instead of one shared
/// `scratch` buffer at absolute offsets.
#[inline]
#[cfg(test)]
fn predict_row_split(low: &[i32], left: usize, right: usize, high_row: &mut [i32], use_wide: bool) {
    let width = high_row.len();
    let mut x = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        while x + 8 <= width {
            let l = i32x8::new(low[left + x..left + x + 8].try_into().expect("8 lanes"));
            let r = i32x8::new(low[right + x..right + x + 8].try_into().expect("8 lanes"));
            let h = i32x8::new(high_row[x..x + 8].try_into().expect("8 lanes"));
            let lifted = h - ((l + r) >> 1i32);
            high_row[x..x + 8].copy_from_slice(&lifted.to_array());
            x += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while x < width {
        high_row[x] -= (low[left + x] + low[right + x]) >> 1;
        x += 1;
    }
}

/// `low_row[x] += (high[left+x] + high[right+x] + 2) >> 2` over `0..low_row.len()`.
/// Same arithmetic as `update_row`, with `high`/`low_row` as separate slices.
#[inline]
#[cfg(test)]
fn update_row_split(high: &[i32], left: usize, right: usize, low_row: &mut [i32], use_wide: bool) {
    let width = low_row.len();
    let mut x = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        let two = i32x8::new([2; 8]);
        while x + 8 <= width {
            let l = i32x8::new(high[left + x..left + x + 8].try_into().expect("8 lanes"));
            let r = i32x8::new(high[right + x..right + x + 8].try_into().expect("8 lanes"));
            let lo = i32x8::new(low_row[x..x + 8].try_into().expect("8 lanes"));
            let lifted = lo + ((l + r + two) >> 2i32);
            low_row[x..x + 8].copy_from_slice(&lifted.to_array());
            x += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while x < width {
        low_row[x] += (high[left + x] + high[right + x] + 2) >> 2;
        x += 1;
    }
}

fn inverse_53_vertical_even_in_place(
    data: &mut [i32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    scratch: &mut [i32],
    use_wide: bool,
) {
    if active_width == 0 || active_height <= 1 {
        return;
    }

    let sn = active_height.div_ceil(2);
    let dn = active_height - sn;

    for band_x in (0..active_width).step_by(VERTICAL_BAND_COLS) {
        let band_width = VERTICAL_BAND_COLS.min(active_width - band_x);
        let scratch = &mut scratch[..band_width * active_height];

        if band_width.saturating_mul(active_height) >= PARALLEL_COLUMN_THRESHOLD {
            // Reads only come from `data` (a separate, read-only buffer here),
            // and each chunk's first row is exactly the "even" row this loop
            // writes for a given `i` — no cross-chunk reads, so plain
            // `par_chunks_mut` needs no snapshot.
            scratch
                .par_chunks_mut(2 * band_width)
                .enumerate()
                .for_each(|(i, chunk)| {
                    let low = i * stride + band_x;
                    let high_left = (sn + i.saturating_sub(1).min(dn - 1)) * stride + band_x;
                    let high_right = (sn + i.min(dn - 1)) * stride + band_x;
                    inverse_predict_row_into(
                        data,
                        low,
                        high_left,
                        high_right,
                        &mut chunk[..band_width],
                        use_wide,
                    );
                });
        } else {
            for i in 0..sn {
                let low = i * stride + band_x;
                let high_left = (sn + i.saturating_sub(1).min(dn - 1)) * stride + band_x;
                let high_right = (sn + i.min(dn - 1)) * stride + band_x;
                let even = (2 * i) * band_width;
                inverse_predict_row(
                    data, scratch, low, high_left, high_right, even, band_width, use_wide,
                );
            }
        }

        if band_width.saturating_mul(active_height) >= PARALLEL_COLUMN_THRESHOLD {
            inverse_update_rows_parallel(
                data, stride, band_x, scratch, sn, dn, band_width, use_wide,
            );
        } else {
            for i in 0..dn {
                let high = (sn + i) * stride + band_x;
                let even = (2 * i) * band_width;
                let odd = (2 * i + 1) * band_width;
                let right_even = if i + 1 < sn {
                    (2 * (i + 1)) * band_width
                } else {
                    even
                };
                inverse_update_row(
                    data, scratch, high, even, right_even, odd, band_width, use_wide,
                );
            }
        }

        for y in 0..active_height {
            let src = y * band_width;
            let dst = y * stride + band_x;
            data[dst..dst + band_width].copy_from_slice(&scratch[src..src + band_width]);
        }
    }
}

/// `right_even` can land in the *next* row-pair (`i+1 < sn`), which a
/// `par_chunks_mut`-based split can't read across without a copy. An earlier
/// version copied `scratch` into a snapshot `Vec` for the reads; that was
/// correct but measured as a net regression on real images (`lear.png`,
/// interleaved A/B verified) — the extra memcpy plus rayon's per-row
/// overhead cost more than the parallel lift saved. This is zero-copy
/// instead: even rows (`2*i`) are only ever read here (the previous loop,
/// `inverse_predict_row`/`_into`, finished writing them before this runs),
/// and odd rows (`2*i+1`) are only ever written here, each by exactly one
/// `i` — so no two rows accessed in this loop ever alias. Each raw-pointer
/// conversion is deliberately restricted to one row; creating an immutable
/// slice over the whole scratch plane would itself alias the mutable odd-row
/// slices, even if callers only indexed its even rows.
#[allow(clippy::too_many_arguments)]
fn inverse_update_rows_parallel(
    data: &[i32],
    stride: usize,
    x_offset: usize,
    scratch: &mut [i32],
    sn: usize,
    dn: usize,
    active_width: usize,
    use_wide: bool,
) {
    debug_assert!(active_width * (sn + dn) <= scratch.len());
    let base = scratch.as_mut_ptr().expose_provenance();
    (0..dn).into_par_iter().for_each(|i| {
        let ptr = std::ptr::with_exposed_provenance_mut(base);
        // SAFETY: see the function-level doc comment above. The address was
        // exposed from `scratch` immediately before Rayon was entered, and
        // `scratch` remains live and exclusively borrowed until all jobs join.
        unsafe {
            inverse_update_row_from_scratch_ptr(
                data,
                stride,
                x_offset,
                ptr,
                sn,
                i,
                active_width,
                use_wide,
            );
        }
    });
}

/// Applies one inverse-update row using the disjoint even/odd row layout in
/// `scratch`.
///
/// # Safety
///
/// `scratch` must point to at least `active_width * (sn + dn)` initialized
/// `i32` values for the caller's transform, where `i < dn`. The even row for
/// `i`, its clamped right neighbor, and the odd row for `i` must all be in
/// bounds. During this call, no other access may write either even row and no
/// other access may read or write the odd row.
#[allow(clippy::too_many_arguments)]
unsafe fn inverse_update_row_from_scratch_ptr(
    data: &[i32],
    stride: usize,
    x_offset: usize,
    scratch: *mut i32,
    sn: usize,
    i: usize,
    active_width: usize,
    use_wide: bool,
) {
    let high = (sn + i) * stride + x_offset;
    let even = (2 * i) * active_width;
    let right_even = if i + 1 < sn {
        (2 * (i + 1)) * active_width
    } else {
        even
    };
    let odd = (2 * i + 1) * active_width;
    // SAFETY: guaranteed by the caller. Each slice is restricted to its
    // individual row so the two immutable even rows cannot overlap the
    // mutable odd row.
    let even_row = unsafe { std::slice::from_raw_parts(scratch.add(even), active_width) };
    let right_even_row =
        unsafe { std::slice::from_raw_parts(scratch.add(right_even), active_width) };
    let odd_row = unsafe { std::slice::from_raw_parts_mut(scratch.add(odd), active_width) };
    inverse_update_row_into(data, even_row, right_even_row, high, odd_row, use_wide);
}

/// `scratch[even+x] = data[low+x] - ((data[high_left+x] + data[high_right+x] + 2) >> 2)`.
#[inline]
#[allow(clippy::too_many_arguments)]
fn inverse_predict_row(
    data: &[i32],
    scratch: &mut [i32],
    low: usize,
    high_left: usize,
    high_right: usize,
    even: usize,
    width: usize,
    use_wide: bool,
) {
    let mut x = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        let two = i32x8::new([2; 8]);
        while x + 8 <= width {
            let lo = i32x8::new(data[low + x..low + x + 8].try_into().expect("8 lanes"));
            let hl = i32x8::new(
                data[high_left + x..high_left + x + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let hr = i32x8::new(
                data[high_right + x..high_right + x + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let result = lo - ((hl + hr + two) >> 2i32);
            scratch[even + x..even + x + 8].copy_from_slice(&result.to_array());
            x += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while x < width {
        scratch[even + x] = data[low + x] - ((data[high_left + x] + data[high_right + x] + 2) >> 2);
        x += 1;
    }
}

/// `scratch[odd+x] = data[high+x] + ((scratch[even+x] + scratch[right_even+x]) >> 1)`.
#[inline]
#[allow(clippy::too_many_arguments)]
fn inverse_update_row(
    data: &[i32],
    scratch: &mut [i32],
    high: usize,
    even: usize,
    right_even: usize,
    odd: usize,
    width: usize,
    use_wide: bool,
) {
    let mut x = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        while x + 8 <= width {
            let h = i32x8::new(data[high + x..high + x + 8].try_into().expect("8 lanes"));
            let e = i32x8::new(scratch[even + x..even + x + 8].try_into().expect("8 lanes"));
            let re = i32x8::new(
                scratch[right_even + x..right_even + x + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let result = h + ((e + re) >> 1i32);
            scratch[odd + x..odd + x + 8].copy_from_slice(&result.to_array());
            x += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while x < width {
        scratch[odd + x] = data[high + x] + ((scratch[even + x] + scratch[right_even + x]) >> 1);
        x += 1;
    }
}

/// `even_row[x] = data[low+x] - ((data[high_left+x] + data[high_right+x] + 2) >> 2)`.
/// Same arithmetic as `inverse_predict_row`, writing to `even_row` (relative
/// indices) instead of `scratch[even+x]` (absolute).
#[inline]
fn inverse_predict_row_into(
    data: &[i32],
    low: usize,
    high_left: usize,
    high_right: usize,
    even_row: &mut [i32],
    use_wide: bool,
) {
    let width = even_row.len();
    let mut x = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        let two = i32x8::new([2; 8]);
        while x + 8 <= width {
            let lo = i32x8::new(data[low + x..low + x + 8].try_into().expect("8 lanes"));
            let hl = i32x8::new(
                data[high_left + x..high_left + x + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let hr = i32x8::new(
                data[high_right + x..high_right + x + 8]
                    .try_into()
                    .expect("8 lanes"),
            );
            let result = lo - ((hl + hr + two) >> 2i32);
            even_row[x..x + 8].copy_from_slice(&result.to_array());
            x += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while x < width {
        even_row[x] = data[low + x] - ((data[high_left + x] + data[high_right + x] + 2) >> 2);
        x += 1;
    }
}

/// `odd_row[x] = data[high+x] + ((even_row[x] + right_even_row[x]) >> 1)`.
/// Same arithmetic as `inverse_update_row`, reading the (already-finalized)
/// even rows through disjoint row slices and writing to `odd_row`.
#[inline]
#[allow(clippy::too_many_arguments)]
fn inverse_update_row_into(
    data: &[i32],
    even_row: &[i32],
    right_even_row: &[i32],
    high: usize,
    odd_row: &mut [i32],
    use_wide: bool,
) {
    let width = odd_row.len();
    let mut x = 0usize;
    #[cfg(feature = "simd")]
    if use_wide {
        while x + 8 <= width {
            let h = i32x8::new(data[high + x..high + x + 8].try_into().expect("8 lanes"));
            let e = i32x8::new(even_row[x..x + 8].try_into().expect("8 lanes"));
            let re = i32x8::new(right_even_row[x..x + 8].try_into().expect("8 lanes"));
            let result = h + ((e + re) >> 1i32);
            odd_row[x..x + 8].copy_from_slice(&result.to_array());
            x += 8;
        }
    }
    #[cfg(not(feature = "simd"))]
    let _ = use_wide;
    while x < width {
        odd_row[x] = data[high + x] + ((even_row[x] + right_even_row[x]) >> 1);
        x += 1;
    }
}

fn encode_resolutions(width: usize, height: usize, levels: u8) -> Vec<(usize, usize)> {
    let mut resolutions = Vec::with_capacity(levels as usize + 1);
    let mut w = width;
    let mut h = height;
    resolutions.push((w, h));
    for _ in 0..levels {
        w = w.div_ceil(2);
        h = h.div_ceil(2);
        resolutions.push((w, h));
    }
    resolutions.reverse();
    resolutions
}

fn vertical_scratch_len(width: usize, height: usize) -> usize {
    width.min(VERTICAL_BAND_COLS).saturating_mul(height)
}

fn gather_column(data: &[i32], stride: usize, height: usize, x: usize, out: &mut [i32]) {
    for y in 0..height {
        out[y] = data[y * stride + x];
    }
}

fn scatter_column(data: &mut [i32], stride: usize, height: usize, x: usize, values: &[i32]) {
    for y in 0..height {
        data[y * stride + x] = values[y];
    }
}

#[cfg(test)]
mod tests {
    use super::{
        forward_53_2d_in_place, forward_53_2d_in_place_at, inverse_53_2d_in_place,
        inverse_53_2d_in_place_at,
    };

    #[test]
    fn one_level_transform_matches_known_2x2_case() {
        let mut data = vec![1, 2, 3, 4];
        forward_53_2d_in_place(&mut data, 2, 2, 1).expect("forward dwt");
        assert_eq!(data, vec![3, 1, 2, 0]);
    }

    #[test]
    fn one_level_transform_matches_known_1d_row_case() {
        let mut data = vec![10, 20, 30, 40];
        forward_53_2d_in_place(&mut data, 4, 1, 1).expect("forward row dwt");
        assert_eq!(data, vec![10, 33, 0, 10]);
    }

    /// Direct correctness checks for the parallel-path helpers, bypassing
    /// `PARALLEL_COLUMN_THRESHOLD` entirely (it's disabled by default — see
    /// the constant's doc comment — so nothing else exercises these paths).
    fn check_forward_split_matches_sequential(use_wide: bool) {
        let width = 40usize;
        let sn = 5usize;
        let dn = 5usize;
        let total = (sn + dn) * width;
        let original: Vec<i32> = (0..total).map(|i| ((i as i32 * 37) % 511) - 255).collect();

        let mut seq = original.clone();
        for i in 0..dn {
            let left = i * width;
            let right = (i + 1).min(sn - 1) * width;
            let high = (sn + i) * width;
            super::predict_row(&mut seq, left, right, high, width, use_wide);
        }
        for i in 0..sn {
            let left = (sn + i.saturating_sub(1).min(dn - 1)) * width;
            let right = (sn + i.min(dn - 1)) * width;
            let low = i * width;
            super::update_row(&mut seq, left, right, low, width, use_wide);
        }

        let mut par = original.clone();
        {
            let (low, high) = par.split_at_mut(sn * width);
            for i in 0..dn {
                let left = i * width;
                let right = (i + 1).min(sn - 1) * width;
                super::predict_row_split(
                    low,
                    left,
                    right,
                    &mut high[i * width..(i + 1) * width],
                    use_wide,
                );
            }
        }
        {
            let (low, high) = par.split_at_mut(sn * width);
            for i in 0..sn {
                let left = i.saturating_sub(1).min(dn - 1) * width;
                let right = i.min(dn - 1) * width;
                super::update_row_split(
                    high,
                    left,
                    right,
                    &mut low[i * width..(i + 1) * width],
                    use_wide,
                );
            }
        }

        assert_eq!(seq, par, "use_wide={use_wide}");
    }

    #[test]
    fn forward_split_matches_sequential_scalar() {
        check_forward_split_matches_sequential(false);
    }

    #[test]
    #[cfg(feature = "simd")]
    fn forward_split_matches_sequential_wide() {
        check_forward_split_matches_sequential(true);
    }

    fn check_inverse_update_rows_parallel_matches_sequential(use_wide: bool) {
        let width = 40usize;
        // sn = dn + 1, to exercise the `right_even` clamp-to-`even` boundary case.
        let sn = 6usize;
        let dn = 5usize;
        let stride = width;
        let scratch_len = width * (sn + dn);
        let data: Vec<i32> = (0..(sn + dn) * stride)
            .map(|i| ((i as i32 * 53) % 401) - 200)
            .collect();
        let original_scratch: Vec<i32> = (0..scratch_len)
            .map(|i| ((i as i32 * 37) % 511) - 255)
            .collect();

        let mut seq = original_scratch.clone();
        for i in 0..dn {
            let high = (sn + i) * stride;
            let even = (2 * i) * width;
            let odd = (2 * i + 1) * width;
            let right_even = if i + 1 < sn {
                (2 * (i + 1)) * width
            } else {
                even
            };
            super::inverse_update_row(
                &data, &mut seq, high, even, right_even, odd, width, use_wide,
            );
        }

        let mut par = original_scratch.clone();
        super::inverse_update_rows_parallel(&data, stride, 0, &mut par, sn, dn, width, use_wide);

        assert_eq!(seq, par, "use_wide={use_wide}");
    }

    #[test]
    fn inverse_update_rows_parallel_matches_sequential_scalar() {
        check_inverse_update_rows_parallel_matches_sequential(false);
    }

    #[test]
    fn inverse_update_row_raw_helper_matches_safe_sequential_scalar() {
        let width = 40usize;
        let sn = 6usize;
        let dn = 5usize;
        let stride = width;
        let scratch_len = width * (sn + dn);
        let data: Vec<i32> = (0..(sn + dn) * stride)
            .map(|i| ((i as i32 * 53) % 401) - 200)
            .collect();
        let original_scratch: Vec<i32> = (0..scratch_len)
            .map(|i| ((i as i32 * 37) % 511) - 255)
            .collect();

        let mut expected = original_scratch.clone();
        for i in 0..dn {
            let high = (sn + i) * stride;
            let even = (2 * i) * width;
            let odd = (2 * i + 1) * width;
            let right_even = if i + 1 < sn {
                (2 * (i + 1)) * width
            } else {
                even
            };
            super::inverse_update_row(
                &data,
                &mut expected,
                high,
                even,
                right_even,
                odd,
                width,
                false,
            );
        }

        let mut actual = original_scratch;
        let scratch = actual.as_mut_ptr();
        for i in 0..dn {
            // SAFETY: `actual` has `width * (sn + dn)` initialized elements.
            // Calls are sequential; the even rows are read-only here and each
            // iteration writes a distinct odd row.
            unsafe {
                super::inverse_update_row_from_scratch_ptr(
                    &data, stride, 0, scratch, sn, i, width, false,
                );
            }
        }

        assert_eq!(expected, actual);
    }

    #[test]
    #[cfg(feature = "simd")]
    fn inverse_update_rows_parallel_matches_sequential_wide() {
        check_inverse_update_rows_parallel_matches_sequential(true);
    }

    #[test]
    fn inverse_reversible_overflow_is_reported_without_panicking() {
        let mut coefficients = [i32::MAX, i32::MAX];
        let error = inverse_53_2d_in_place(&mut coefficients, 2, 1, 1)
            .expect_err("malformed extreme coefficients must exceed i32 output")
            .to_string();
        assert!(error.contains("exceeds signed 32-bit"), "{error}");
    }

    #[test]
    fn multi_level_transform_preserves_length_and_runs_on_odd_sizes() {
        let mut data = (0..35).collect::<Vec<_>>();
        forward_53_2d_in_place(&mut data, 5, 7, 2).expect("forward dwt");
        assert_eq!(data.len(), 35);
    }

    #[test]
    fn forward_then_inverse_53_roundtrips_exactly_for_small_images() {
        for height in 1..=8 {
            for width in 1..=8 {
                let levels = max_decompositions(width, height).min(3) as u8;
                for (name, original) in tiny_patterns(width, height) {
                    let mut data = original.clone();
                    forward_53_2d_in_place(&mut data, width, height, levels).expect("forward dwt");
                    inverse_53_2d_in_place(&mut data, width, height, levels).expect("inverse dwt");
                    assert_eq!(
                        data, original,
                        "{name} failed for {width}x{height} with {levels} levels"
                    );
                }
            }
        }
    }

    #[test]
    fn forward_then_inverse_53_roundtrips_exactly_at_5_levels_non_pow2() {
        // These are the actual sizes used by the encoder for RGB lossless test images.
        let cases: &[(usize, usize, u8)] = &[(48, 40, 5), (64, 48, 5), (32, 32, 5)];
        for &(width, height, levels) in cases {
            for (name, original) in tiny_patterns(width, height) {
                let mut data = original.clone();
                forward_53_2d_in_place(&mut data, width, height, levels).expect("forward dwt");
                inverse_53_2d_in_place(&mut data, width, height, levels).expect("inverse dwt");
                assert_eq!(
                    data, original,
                    "{name} failed for {width}x{height} with {levels} levels"
                );
            }
        }
    }

    #[test]
    fn vertical_scratch_is_bounded_by_column_band() {
        let width = super::VERTICAL_BAND_COLS * 3 + 17;
        let height = 41usize;

        assert_eq!(
            super::vertical_scratch_len(width, height),
            super::VERTICAL_BAND_COLS * height
        );
        assert!(super::vertical_scratch_len(width, height) < width * height);
    }

    #[test]
    fn forward_then_inverse_53_roundtrips_exactly_across_vertical_bands() {
        let width = super::VERTICAL_BAND_COLS + 77;
        let height = 29usize;
        let levels = 4u8;
        let original: Vec<i32> = (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    let x = x as i32;
                    let y = y as i32;
                    ((x * 17 + y * 31 + (x ^ y) * 7) % 511) - 255
                })
            })
            .collect();

        let mut data = original.clone();
        forward_53_2d_in_place(&mut data, width, height, levels).expect("forward dwt");
        inverse_53_2d_in_place(&mut data, width, height, levels).expect("inverse dwt");

        assert_eq!(data, original);
    }

    #[test]
    fn annex_f_origin_phases_roundtrip_exactly() {
        for y0 in 0..8usize {
            for x0 in 0..8usize {
                for height in 1..=9usize {
                    for width in 1..=9usize {
                        let levels = max_decompositions(width, height).min(3) as u8;
                        let original = (0..height)
                            .flat_map(|y| {
                                (0..width).map(move |x| {
                                    let gx = (x0 + x) as i32;
                                    let gy = (y0 + y) as i32;
                                    ((gx * 37 + gy * 61 + (gx ^ gy) * 11) & 0x3ff) - 512
                                })
                            })
                            .collect::<Vec<_>>();
                        let mut data = original.clone();
                        forward_53_2d_in_place_at(&mut data, width, height, levels, x0, y0)
                            .expect("phase-aware forward 5/3");
                        inverse_53_2d_in_place_at(&mut data, width, height, levels, x0, y0)
                            .expect("phase-aware inverse 5/3");
                        assert_eq!(data, original, "origin=({x0},{y0}) size={width}x{height}");
                    }
                }
            }
        }
    }

    fn max_decompositions(width: usize, height: usize) -> usize {
        let min_dim = width.min(height);
        if min_dim <= 1 {
            return 0;
        }
        usize::BITS as usize - 1 - min_dim.leading_zeros() as usize
    }

    fn tiny_patterns(width: usize, height: usize) -> Vec<(&'static str, Vec<i32>)> {
        let len = width * height;
        let mut patterns = vec![
            ("zeros", vec![0; len]),
            ("ones", vec![1; len]),
            (
                "horizontal_ramp",
                (0..height)
                    .flat_map(|_| (0..width).map(|x| x as i32))
                    .collect(),
            ),
            (
                "vertical_ramp",
                (0..height)
                    .flat_map(|y| (0..width).map(move |_| y as i32))
                    .collect(),
            ),
            (
                "checkerboard",
                (0..height)
                    .flat_map(|y| (0..width).map(move |x| ((x + y) & 1) as i32))
                    .collect(),
            ),
        ];

        for y in 0..height {
            for x in 0..width {
                let mut data = vec![0; len];
                data[y * width + x] = 255;
                patterns.push(("impulse", data));
            }
        }

        patterns
    }
}
