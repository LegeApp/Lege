#![allow(dead_code)]

use crate::encode::profile_enter;
use crate::error::{Jp2LamError, Result};
use rayon::prelude::*;
#[cfg(feature = "simd")]
use wide::f32x8;

// ISO/IEC 15444-1 Annex F.4.8.2 irreversible 9/7 lifting constants.
const ALPHA: f32 = -1.586_134_342_059_924;
const BETA: f32 = -0.052_980_118_572_961;
const GAMMA: f32 = 0.882_911_075_530_934;
const DELTA: f32 = 0.443_506_852_043_971;
const K: f32 = 1.230_174_104_914_001;
const INV_K: f32 = 1.0 / K;
const PARALLEL_ROW_THRESHOLD: usize = 2 * 1024 * 1024;
// Disabled by default (never reached in practice — no real image resolution
// comes anywhere near `usize::MAX / 2` pixels; not literally `usize::MAX` to
// avoid a clippy::absurd_extreme_comparisons hard error on the `>=` check).
// Two approaches to parallelizing the vertical lift (snapshot-copy, then a
// zero-copy raw-pointer version — see `apply_vertical_lift_parallel`'s doc
// comment) were both measured as consistent net *regressions* on real images
// (`lear.png`, interleaved A/B verified: ~2-12% slower depending on metric)
// versus the sequential `wide`-vectorized loop. After SIMD, a row-pair lift
// is fast enough that rayon's per-task scheduling overhead outweighs the
// parallelism gained. The implementation is kept (and unit-tested directly,
// bypassing this threshold — see `dwt::irrev97::tests`) in case a future
// pass finds a coarser-grained or otherwise lower-overhead approach; until
// then it should not be enabled.
const PARALLEL_COLUMN_THRESHOLD: usize = usize::MAX / 2;

// Column band width for vertical 9/7 synthesis/analysis. Matches the reversible
// 5/3 design: process independent column bands so scratch is
// `VERTICAL_BAND_COLS * active_height` rather than a full active plane, and so
// the deinterleave/interleave steps never allocate a W×H temporary.
const VERTICAL_BAND_COLS: usize = 256;

pub(crate) fn forward_97_2d_in_place(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    forward_97_2d_in_place_impl(data, width, height, levels, false)
}

pub(crate) fn forward_97_2d_in_place_at(
    data: &mut [f32],
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
    crate::encode::counters::record_dwt_scratch(width.max(height) * 2 * std::mem::size_of::<f32>());
    let mut line = vec![0.0f32; width.max(height)];
    let mut scratch = vec![0.0f32; width.max(height)];
    for &(rw, rh, x_even, y_even) in &steps {
        for x in 0..rw {
            for y in 0..rh {
                line[y] = data[y * width + x];
            }
            forward_97_1d_with_scratch_phase(&mut line[..rh], &mut scratch[..rh], y_even, false);
            for y in 0..rh {
                data[y * width + x] = line[y];
            }
        }
        for y in 0..rh {
            let start = y * width;
            forward_97_1d_with_scratch_phase(
                &mut data[start..start + rw],
                &mut scratch[..rw],
                x_even,
                false,
            );
        }
    }
    Ok(())
}

#[cfg(feature = "simd")]
pub(crate) fn forward_97_2d_in_place_wide(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
) -> Result<()> {
    forward_97_2d_in_place_impl(data, width, height, levels, true)
}

fn forward_97_2d_in_place_impl(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
    use_wide: bool,
) -> Result<()> {
    let _p = profile_enter("dwt::forward_97_2d_in_place");
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
    let mut scratch = vec![0f32; width.max(height)];

    for &(rw, rh) in resolutions.iter().skip(1).rev() {
        forward_97_vertical_in_place(data, width, rw, rh, use_wide);

        forward_97_rows_in_place(data, width, rw, rh, &mut scratch, use_wide);
    }

    Ok(())
}

/// Forward 9/7 lifting on one row, even-origin, with whole-sample symmetric
/// extension at boundaries. Output is deinterleaved: samples[..sn] are the low
/// (s) coefficients, samples[sn..] are the high (d) coefficients.
fn forward_97_1d_in_place(samples: &mut [f32]) {
    let mut scratch = vec![0f32; samples.len()];
    forward_97_1d_with_scratch(samples, &mut scratch, false);
}

fn forward_97_1d_with_scratch(samples: &mut [f32], tmp: &mut [f32], use_wide: bool) {
    forward_97_1d_with_scratch_phase(samples, tmp, true, use_wide);
}

fn forward_97_1d_with_scratch_phase(
    samples: &mut [f32],
    tmp: &mut [f32],
    even_origin: bool,
    use_wide: bool,
) {
    let n = samples.len();
    if n < 2 {
        if n == 1 && !even_origin {
            // Annex F's one-element odd-origin signal contains only a
            // high-pass sample, so retain the high-pass normalization. An
            // even-origin one-element signal is an already-complete low-pass
            // signal and the degenerate split is an identity operation.
            samples[0] *= K;
        }
        return;
    }

    let low_parity = usize::from(!even_origin);
    let high_parity = 1 - low_parity;

    apply_lift(samples, high_parity, ALPHA, use_wide);
    apply_lift(samples, low_parity, BETA, use_wide);
    apply_lift(samples, high_parity, GAMMA, use_wide);
    apply_lift(samples, low_parity, DELTA, use_wide);

    for i in (low_parity..n).step_by(2) {
        samples[i] *= INV_K;
    }
    for i in (high_parity..n).step_by(2) {
        samples[i] *= K;
    }

    let sn = (n + usize::from(even_origin)) / 2;
    let dn = n - sn;
    let tmp = &mut tmp[..n];
    for i in 0..sn {
        tmp[i] = samples[2 * i + low_parity];
    }
    for i in 0..dn {
        tmp[sn + i] = samples[2 * i + high_parity];
    }
    samples.copy_from_slice(tmp);
}

fn forward_97_rows_in_place(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    scratch: &mut [f32],
    use_wide: bool,
) {
    if active_width.saturating_mul(active_height) >= PARALLEL_ROW_THRESHOLD {
        // Coarse jobs (not one Rayon task per row): each job owns one line
        // scratch reused across its rows. Rayon `for_each_init` is *per job*,
        // not per worker, so a single allocation per coarse chunk is the
        // reliable bound.
        let workers = rayon::current_num_threads().max(1);
        let rows_per_job = active_height.div_ceil(workers * 2).max(1);
        data[..active_height * stride]
            .par_chunks_mut(rows_per_job * stride)
            .for_each(|rows| {
                let mut local_scratch = vec![0.0f32; active_width];
                for row in rows.chunks_mut(stride) {
                    if row.len() < active_width {
                        break;
                    }
                    forward_97_1d_with_scratch(
                        &mut row[..active_width],
                        &mut local_scratch,
                        use_wide,
                    );
                }
            });
    } else {
        for y in 0..active_height {
            let row_start = y * stride;
            forward_97_1d_with_scratch(
                &mut data[row_start..row_start + active_width],
                &mut scratch[..active_width],
                use_wide,
            );
        }
    }
}

fn apply_lift(samples: &mut [f32], start_parity: usize, coeff: f32, use_wide: bool) {
    #[cfg(feature = "simd")]
    if use_wide {
        apply_lift_wide(samples, start_parity, coeff);
        return;
    }
    let _ = use_wide;

    let n = samples.len();
    let mut j = start_parity;
    while j < n {
        let left = fetch_sym(samples, j as isize - 1);
        let right = fetch_sym(samples, j as isize + 1);
        samples[j] += coeff * (left + right);
        j += 2;
    }
}

/// Vectorized `apply_lift`: batches 8 lifted positions (spanning 16 raw
/// samples) at a time over the region where every lane's `j-1`/`j+1` neighbor
/// is in-bounds, so no symmetric extension is needed. Falls back to the
/// scalar `fetch_sym` path for the boundary and any remainder — bit-exact
/// with the scalar reference because the vector lanes read the same
/// unchecked in-bounds values `fetch_sym`'s fast path would and perform the
/// identical `cur + coeff * (left + right)` sequence.
#[cfg(feature = "simd")]
fn apply_lift_wide(samples: &mut [f32], start_parity: usize, coeff: f32) {
    let n = samples.len();
    let mut j = start_parity;

    // The only position that can need left-side symmetric extension is j==0
    // (only possible when start_parity==0); handle it scalar, then the
    // vector loop below only ever sees in-bounds neighbors.
    if j == 0 && n >= 2 {
        let left = fetch_sym(samples, -1);
        let right = fetch_sym(samples, 1);
        samples[0] += coeff * (left + right);
        j += 2;
    }

    let coeff_vec = f32x8::new([coeff; 8]);
    while j + 15 < n {
        let mut cur = [0f32; 8];
        let mut left = [0f32; 8];
        let mut right = [0f32; 8];
        for (k, ((c, l), r)) in cur
            .iter_mut()
            .zip(left.iter_mut())
            .zip(right.iter_mut())
            .enumerate()
        {
            let jj = j + 2 * k;
            *c = samples[jj];
            *l = samples[jj - 1];
            *r = samples[jj + 1];
        }
        let result = f32x8::new(cur) + coeff_vec * (f32x8::new(left) + f32x8::new(right));
        let arr = result.to_array();
        for (k, &value) in arr.iter().enumerate() {
            samples[j + 2 * k] = value;
        }
        j += 16;
    }

    while j < n {
        let left = fetch_sym(samples, j as isize - 1);
        let right = fetch_sym(samples, j as isize + 1);
        samples[j] += coeff * (left + right);
        j += 2;
    }
}

/// Whole-sample symmetric extension: x[-i] = x[i], x[n-1+i] = x[n-1-i].
#[inline(always)]
fn fetch_sym(samples: &[f32], i: isize) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return samples[0];
    }

    // Fast path: in-bounds access (most common case in DWT lifting)
    if i >= 0 && i < n as isize {
        // SAFETY: bounds checked above
        return unsafe { *samples.get_unchecked(i as usize) };
    }

    // Slow path: symmetric extension for out-of-bounds indices
    let period = 2 * (n as isize - 1);
    let k = i.rem_euclid(period);
    let idx = if k >= n as isize {
        (period - k) as usize
    } else {
        k as usize
    };
    samples[idx]
}

/// Annex F.4.3/F.4.8.2 vertical 9/7 decomposition over the top-left
/// `active_width` x `active_height` rectangle, using contiguous column bands
/// so scratch stays `VERTICAL_BAND_COLS × active_height` rather than a full
/// active plane (and so deinterleave never allocates a W×H temporary).
fn forward_97_vertical_in_place(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    use_wide: bool,
) {
    if active_width == 0 || active_height == 0 {
        return;
    }
    if active_height < 2 {
        // Identity, mirroring the inverse below and OpenJPEG's
        // `opj_dwt_encode_and_deinterleave_v_real`, which returns on
        // `height == 1` without scaling. The old INV_K/K pair round-tripped
        // through our own encoder but did not match streams from conformant
        // encoders, which leave a one-row resolution untouched.
        return;
    }

    let mut band_scratch = vec![0.0f32; VERTICAL_BAND_COLS * active_height];
    for band_x in (0..active_width).step_by(VERTICAL_BAND_COLS) {
        let band_width = VERTICAL_BAND_COLS.min(active_width - band_x);
        let scratch = &mut band_scratch[..band_width * active_height];

        // Gather interleaved columns into a contiguous band (source is still
        // interleaved spatial samples before the forward vertical deinterleave).
        for y in 0..active_height {
            let src = y * stride + band_x;
            let dst = y * band_width;
            scratch[dst..dst + band_width].copy_from_slice(&data[src..src + band_width]);
        }

        apply_vertical_lift(
            scratch,
            band_width,
            band_width,
            active_height,
            1,
            ALPHA,
            use_wide,
        );
        apply_vertical_lift(
            scratch,
            band_width,
            band_width,
            active_height,
            0,
            BETA,
            use_wide,
        );
        apply_vertical_lift(
            scratch,
            band_width,
            band_width,
            active_height,
            1,
            GAMMA,
            use_wide,
        );
        apply_vertical_lift(
            scratch,
            band_width,
            band_width,
            active_height,
            0,
            DELTA,
            use_wide,
        );
        scale_rows_interleaved(
            scratch,
            band_width,
            band_width,
            active_height,
            INV_K,
            K,
            use_wide,
        );
        deinterleave_rows_inplace(scratch, band_width, active_height);

        for y in 0..active_height {
            let src = y * band_width;
            let dst = y * stride + band_x;
            data[dst..dst + band_width].copy_from_slice(&scratch[src..src + band_width]);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_vertical_lift(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    start_parity: usize,
    coeff: f32,
    use_wide: bool,
) {
    if active_width.saturating_mul(active_height) >= PARALLEL_COLUMN_THRESHOLD {
        apply_vertical_lift_parallel(
            data,
            stride,
            active_width,
            active_height,
            start_parity,
            coeff,
            use_wide,
        );
        return;
    }

    #[cfg(feature = "simd")]
    if use_wide {
        apply_vertical_lift_disjoint::<true>(
            data,
            stride,
            active_width,
            active_height,
            start_parity,
            coeff,
        );
        return;
    }
    let _ = use_wide;
    apply_vertical_lift_disjoint::<false>(
        data,
        stride,
        active_width,
        active_height,
        start_parity,
        coeff,
    );
}

fn apply_vertical_lift_disjoint<const WIDE: bool>(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    start_parity: usize,
    coeff: f32,
) {
    let mut y = start_parity;
    while y < active_height {
        let left_y = sym_index(active_height, y as isize - 1);
        let right_y = sym_index(active_height, y as isize + 1);
        let target_range = y * stride..y * stride + active_width;
        let left_range = left_y * stride..left_y * stride + active_width;
        let right_range = right_y * stride..right_y * stride + active_width;
        if left_y == right_y {
            let [target, neighbour] = data
                .get_disjoint_mut([target_range, left_range])
                .expect("target and opposite-parity boundary row are disjoint");
            if WIDE {
                #[cfg(feature = "simd")]
                lift_row_wide(target, neighbour, neighbour, coeff);
                #[cfg(not(feature = "simd"))]
                lift_row(target, neighbour, neighbour, coeff, false);
            } else {
                lift_row(target, neighbour, neighbour, coeff, false);
            }
        } else {
            let [target, left, right] = data
                .get_disjoint_mut([target_range, left_range, right_range])
                .expect("target and opposite-parity neighbour rows are disjoint");
            if WIDE {
                #[cfg(feature = "simd")]
                lift_row_wide(target, left, right, coeff);
                #[cfg(not(feature = "simd"))]
                lift_row(target, left, right, coeff, false);
            } else {
                lift_row(target, left, right, coeff, false);
            }
        }
        y += 2;
    }
}

/// Same lift as the sequential loop in `apply_vertical_lift`, but splits the
/// target-parity rows across rayon tasks. A first attempt at this used an
/// immutable snapshot `Vec` for the reads (still correct, see git history),
/// but measured as a consistent 5-9% *regression* on real images
/// (`lear.png`, verified with interleaved A/B runs) — the snapshot's memcpy
/// plus rayon's per-row task overhead cost more than the already-`wide`-fast
/// lift saves. This version is zero-copy: it reads/writes `data` directly
/// through raw pointers, justified below, instead of copying.
///
/// SAFETY: A lift step only ever reads rows of the *opposite* parity to
/// `start_parity` and only ever writes rows of `start_parity` (see
/// `apply_vertical_lift`'s sequential loop, which this mirrors). Therefore,
/// for any two rows `y1 != y2` in this call:
/// - two target (write) rows never alias, because every task's `y` is a
///   distinct value of `start_parity + chunk_idx * 2`;
/// - a target (write) row never aliases a `left`/`right` (read) row, because
///   the target always has `start_parity` and the reads always have the
///   opposite parity;
/// - the `left`/`right` (read) rows are never concurrently written, because
///   nothing in this call writes to opposite-parity rows.
///
/// All row slices are within bounds because `y`, `left_y`, `right_y` are all
/// `< active_height` (by construction / `sym_index`'s clamping) and
/// `active_height * stride <= data.len()` (the caller's invariant), and
/// `active_width <= stride` (a row's active region never exceeds its pitch).
fn apply_vertical_lift_parallel(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    start_parity: usize,
    coeff: f32,
    use_wide: bool,
) {
    debug_assert!(active_height * stride <= data.len());
    debug_assert!(active_width <= stride);
    let base = data.as_mut_ptr() as usize;
    let row_count = (active_height - start_parity).div_ceil(2);
    (0..row_count).into_par_iter().for_each(|chunk_idx| {
        let y = start_parity + chunk_idx * 2;
        let left_y = sym_index(active_height, y as isize - 1);
        let right_y = sym_index(active_height, y as isize + 1);
        let ptr = base as *mut f32;
        // SAFETY: see the function-level safety argument above.
        let left = unsafe { std::slice::from_raw_parts(ptr.add(left_y * stride), active_width) };
        let right = unsafe { std::slice::from_raw_parts(ptr.add(right_y * stride), active_width) };
        let target = unsafe { std::slice::from_raw_parts_mut(ptr.add(y * stride), active_width) };
        lift_row(target, left, right, coeff, use_wide);
    });
}

#[inline]
fn lift_row(target: &mut [f32], left: &[f32], right: &[f32], coeff: f32, use_wide: bool) {
    #[cfg(feature = "simd")]
    if use_wide {
        lift_row_wide(target, left, right, coeff);
        return;
    }
    let _ = use_wide;
    for x in 0..target.len() {
        target[x] += coeff * (left[x] + right[x]);
    }
}

#[cfg(feature = "simd")]
#[inline]
fn lift_row_wide(target: &mut [f32], left: &[f32], right: &[f32], coeff: f32) {
    let coeff_scalar = coeff;
    let coeff = f32x8::new([coeff_scalar; 8]);
    let mut x = 0usize;
    while x + 8 <= target.len() {
        let target_values = f32x8::new(target[x..x + 8].try_into().expect("8 lanes"));
        let left_values = f32x8::new(left[x..x + 8].try_into().expect("8 lanes"));
        let right_values = f32x8::new(right[x..x + 8].try_into().expect("8 lanes"));
        let lifted = target_values + coeff * (left_values + right_values);
        target[x..x + 8].copy_from_slice(&lifted.to_array());
        x += 8;
    }
    while x < target.len() {
        target[x] += coeff_scalar * (left[x] + right[x]);
        x += 1;
    }
}

#[inline]
fn scale_row_with(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    y: usize,
    scale: f32,
    use_wide: bool,
) {
    scale_slice(row_mut(data, stride, active_width, y), scale, use_wide);
}

/// Scales the interleaved even/odd rows of a vertical pass in one pass:
/// `even_scale` for even `y`, `odd_scale` for odd `y`. Unlike the lift steps,
/// scaling has no cross-row read dependency at all (each row only reads and
/// writes itself), so above the parallel threshold this simply hands every
/// row to rayon as an independent chunk — no snapshot needed.
fn scale_rows_interleaved(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    even_scale: f32,
    odd_scale: f32,
    use_wide: bool,
) {
    if active_width.saturating_mul(active_height) >= PARALLEL_COLUMN_THRESHOLD {
        data[..active_height * stride]
            .par_chunks_mut(stride)
            .enumerate()
            .for_each(|(y, chunk)| {
                let scale = if y % 2 == 0 { even_scale } else { odd_scale };
                scale_slice(&mut chunk[..active_width], scale, use_wide);
            });
        return;
    }
    for y in (0..active_height).step_by(2) {
        scale_row_with(data, stride, active_width, y, even_scale, use_wide);
    }
    for y in (1..active_height).step_by(2) {
        scale_row_with(data, stride, active_width, y, odd_scale, use_wide);
    }
}

#[inline]
fn scale_slice(target: &mut [f32], scale: f32, use_wide: bool) {
    #[cfg(feature = "simd")]
    if use_wide {
        scale_slice_wide(target, scale);
        return;
    }
    let _ = use_wide;
    for value in target {
        *value *= scale;
    }
}

#[cfg(feature = "simd")]
#[inline]
fn scale_slice_wide(target: &mut [f32], scale: f32) {
    let scale_scalar = scale;
    let scale = f32x8::new([scale_scalar; 8]);
    let mut x = 0usize;
    while x + 8 <= target.len() {
        let values = f32x8::new(target[x..x + 8].try_into().expect("8 lanes"));
        target[x..x + 8].copy_from_slice(&(values * scale).to_array());
        x += 8;
    }
    while x < target.len() {
        target[x] *= scale_scalar;
        x += 1;
    }
}

/// Deinterleave even/odd rows into low/high blocks inside a contiguous band
/// buffer where `stride == active_width`. Uses a single line of temporary
/// storage rather than a full-plane temporary.
fn deinterleave_rows_inplace(data: &mut [f32], active_width: usize, active_height: usize) {
    if active_width == 0 || active_height < 2 {
        return;
    }
    let sn = active_height.div_ceil(2);
    let dn = active_height - sn;
    // Pull odds into a side buffer, then pack lows followed by highs.
    let mut odds = vec![0.0f32; dn * active_width];
    for i in 0..dn {
        let src = (2 * i + 1) * active_width;
        odds[i * active_width..(i + 1) * active_width]
            .copy_from_slice(&data[src..src + active_width]);
    }
    // Compact even (low) rows toward the top.
    for i in 0..sn {
        let src = (2 * i) * active_width;
        let dst = i * active_width;
        if src != dst {
            data.copy_within(src..src + active_width, dst);
        }
    }
    data[sn * active_width..(sn + dn) * active_width].copy_from_slice(&odds);
}

#[inline]
fn row_mut(data: &mut [f32], stride: usize, active_width: usize, y: usize) -> &mut [f32] {
    &mut data[y * stride..y * stride + active_width]
}

#[inline]
fn sym_index(n: usize, i: isize) -> usize {
    if n <= 1 {
        return 0;
    }
    if i >= 0 && i < n as isize {
        return i as usize;
    }
    let period = 2 * (n as isize - 1);
    let k = i.rem_euclid(period);
    if k >= n as isize {
        (period - k) as usize
    } else {
        k as usize
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

/// Inverse 9/7 2-D lifting, in-place. Undoes `forward_97_2d_in_place`.
///
/// `data` must be in the deinterleaved JPEG 2000 subband layout produced by the
/// forward transform. After this call, `data` holds the reconstructed signal.
pub(crate) fn inverse_97_2d_in_place(data: &mut [f32], width: usize, height: usize, levels: u8) {
    inverse_97_2d_in_place_impl(data, width, height, levels, false, None);
}

pub(crate) fn inverse_97_2d_in_place_at(
    data: &mut [f32],
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
    crate::encode::counters::record_dwt_scratch(width.max(height) * 2 * std::mem::size_of::<f32>());
    let mut line = vec![0.0f32; width.max(height)];
    let mut scratch = vec![0.0f32; width.max(height)];
    for &(rw, rh, x_even, y_even) in steps.iter().rev() {
        for y in 0..rh {
            let start = y * width;
            inverse_97_1d_with_scratch_phase(
                &mut data[start..start + rw],
                &mut scratch[..rw],
                x_even,
                false,
            );
        }
        for x in 0..rw {
            for y in 0..rh {
                line[y] = data[y * width + x];
            }
            inverse_97_1d_with_scratch_phase(&mut line[..rh], &mut scratch[..rh], y_even, false);
            for y in 0..rh {
                data[y * width + x] = line[y];
            }
        }
    }
    Ok(())
}

#[cfg(feature = "simd")]
pub(crate) fn inverse_97_2d_in_place_wide(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
) {
    inverse_97_2d_in_place_impl(data, width, height, levels, true, None);
}

pub(crate) fn inverse_97_2d_in_place_profiled(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
    use_wide: bool,
) -> crate::dwt::InverseDwtTiming {
    let mut timing = crate::dwt::InverseDwtTiming::default();
    inverse_97_2d_in_place_impl(data, width, height, levels, use_wide, Some(&mut timing));
    timing
}

fn inverse_97_2d_in_place_impl(
    data: &mut [f32],
    width: usize,
    height: usize,
    levels: u8,
    use_wide: bool,
    mut timing: Option<&mut crate::dwt::InverseDwtTiming>,
) {
    if width == 0 || height == 0 || levels == 0 {
        return;
    }
    let resolutions = encode_resolutions(width, height, levels);
    let mut scratch = vec![0f32; width.max(height)];

    for &(rw, rh) in resolutions.iter().skip(1) {
        let horizontal_start = timing.as_ref().map(|_| std::time::Instant::now());
        inverse_97_rows_in_place(data, width, rw, rh, &mut scratch, use_wide);
        let horizontal = horizontal_start.map(|start| start.elapsed());
        let vertical_start = timing.as_ref().map(|_| std::time::Instant::now());
        inverse_97_vertical_in_place(data, width, rw, rh, use_wide);
        if let (Some(timing), Some(horizontal), Some(vertical_start)) =
            (timing.as_deref_mut(), horizontal, vertical_start)
        {
            timing.record_level(horizontal, vertical_start.elapsed());
        }
    }
}

fn inverse_97_1d_in_place(samples: &mut [f32]) {
    let mut scratch = vec![0f32; samples.len()];
    inverse_97_1d_with_scratch(samples, &mut scratch, false);
}

fn inverse_97_1d_with_scratch(samples: &mut [f32], inter: &mut [f32], use_wide: bool) {
    inverse_97_1d_with_scratch_phase(samples, inter, true, use_wide);
}

fn inverse_97_1d_with_scratch_phase(
    samples: &mut [f32],
    inter: &mut [f32],
    even_origin: bool,
    use_wide: bool,
) {
    let n = samples.len();
    if n < 2 {
        if n == 1 && !even_origin {
            // Inverse of the odd-origin high-pass-only normalization above.
            // The even-origin low-pass-only case is deliberately a no-op:
            // applying K here makes every later vertical synthesis row drift.
            samples[0] *= INV_K;
        }
        return;
    }

    let low_parity = usize::from(!even_origin);
    let high_parity = 1 - low_parity;
    let sn = (n + usize::from(even_origin)) / 2;
    let dn = n - sn;
    let inter = &mut inter[..n];
    for i in 0..sn {
        inter[2 * i + low_parity] = samples[i];
    }
    for i in 0..dn {
        inter[2 * i + high_parity] = samples[sn + i];
    }

    for i in (low_parity..n).step_by(2) {
        inter[i] *= K;
    }
    for i in (high_parity..n).step_by(2) {
        inter[i] *= INV_K;
    }

    apply_lift(inter, low_parity, -DELTA, use_wide);
    apply_lift(inter, high_parity, -GAMMA, use_wide);
    apply_lift(inter, low_parity, -BETA, use_wide);
    apply_lift(inter, high_parity, -ALPHA, use_wide);

    samples.copy_from_slice(inter);
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

fn inverse_97_rows_in_place(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    scratch: &mut [f32],
    use_wide: bool,
) {
    if active_width.saturating_mul(active_height) >= PARALLEL_ROW_THRESHOLD {
        // Coarse jobs reuse one line scratch per job; allocation count is
        // O(jobs × levels), not O(rows × levels).
        let workers = rayon::current_num_threads().max(1);
        let rows_per_job = active_height.div_ceil(workers * 2).max(1);
        data[..active_height * stride]
            .par_chunks_mut(rows_per_job * stride)
            .for_each(|rows| {
                let mut local_scratch = vec![0.0f32; active_width];
                for row in rows.chunks_mut(stride) {
                    if row.len() < active_width {
                        break;
                    }
                    inverse_97_1d_with_scratch(
                        &mut row[..active_width],
                        &mut local_scratch,
                        use_wide,
                    );
                }
            });
    } else {
        for y in 0..active_height {
            let row_start = y * stride;
            inverse_97_1d_with_scratch(
                &mut data[row_start..row_start + active_width],
                &mut scratch[..active_width],
                use_wide,
            );
        }
    }
}

fn inverse_97_vertical_in_place(
    data: &mut [f32],
    stride: usize,
    active_width: usize,
    active_height: usize,
    use_wide: bool,
) {
    if active_width == 0 || active_height == 0 {
        return;
    }
    if active_height < 2 {
        // A one-row resolution is an even-origin low-pass-only interval
        // (sn == 1, dn == 0): Annex F makes the synthesis an identity, and
        // OpenJPEG's `opj_v8dwt_decode` returns before any scaling for exactly
        // this case. Applying K here would compound once per degenerate level —
        // a stream that signals more decomposition levels than its dimensions
        // need (17 resolutions on 884x1344) passes through five 1x1 levels and
        // came out with its DC multiplied by K^5 = 2.8158, a washed-out page
        // with pixel-perfect AC. The horizontal 1-D path already treats this
        // case as an identity; the two must agree.
        return;
    }

    let sn = active_height.div_ceil(2);
    let dn = active_height - sn;
    let mut band_scratch = vec![0.0f32; VERTICAL_BAND_COLS * active_height];
    for band_x in (0..active_width).step_by(VERTICAL_BAND_COLS) {
        let band_width = VERTICAL_BAND_COLS.min(active_width - band_x);
        let scratch = &mut band_scratch[..band_width * active_height];

        // Gather low/high deinterleaved columns into interleaved band layout.
        for i in 0..sn {
            let src = i * stride + band_x;
            let dst = (2 * i) * band_width;
            scratch[dst..dst + band_width].copy_from_slice(&data[src..src + band_width]);
        }
        for i in 0..dn {
            let src = (sn + i) * stride + band_x;
            let dst = (2 * i + 1) * band_width;
            scratch[dst..dst + band_width].copy_from_slice(&data[src..src + band_width]);
        }

        scale_rows_interleaved(
            scratch,
            band_width,
            band_width,
            active_height,
            K,
            INV_K,
            use_wide,
        );
        apply_vertical_lift(
            scratch,
            band_width,
            band_width,
            active_height,
            0,
            -DELTA,
            use_wide,
        );
        apply_vertical_lift(
            scratch,
            band_width,
            band_width,
            active_height,
            1,
            -GAMMA,
            use_wide,
        );
        apply_vertical_lift(
            scratch,
            band_width,
            band_width,
            active_height,
            0,
            -BETA,
            use_wide,
        );
        apply_vertical_lift(
            scratch,
            band_width,
            band_width,
            active_height,
            1,
            -ALPHA,
            use_wide,
        );

        for y in 0..active_height {
            let src = y * band_width;
            let dst = y * stride + band_x;
            data[dst..dst + band_width].copy_from_slice(&scratch[src..src + band_width]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        forward_97_1d_in_place, forward_97_1d_with_scratch_phase, forward_97_2d_in_place,
        forward_97_2d_in_place_at, forward_97_vertical_in_place, inverse_97_1d_in_place,
        inverse_97_1d_with_scratch_phase, inverse_97_2d_in_place, inverse_97_2d_in_place_at,
        inverse_97_vertical_in_place,
    };

    const ROUNDTRIP_TOL: f32 = 1e-3;

    /// Direct correctness check for `apply_vertical_lift_parallel` against
    /// the sequential loop in `apply_vertical_lift`, bypassing
    /// `PARALLEL_COLUMN_THRESHOLD` entirely (it's disabled by default — see
    /// the constant's doc comment — so nothing else exercises this path).
    fn check_vertical_lift_parallel_matches_sequential(use_wide: bool) {
        let width = 64usize;
        let height = 41usize; // odd, to exercise the last-chunk-of-1-row case too.
        let stride = width;
        for &(start_parity, coeff) in &[(1usize, super::ALPHA), (0usize, super::BETA)] {
            let original = (0..width * height)
                .map(|i| ((i as f32 * 0.037).sin() * 255.0) + (i % 23) as f32)
                .collect::<Vec<_>>();
            let mut seq = original.clone();
            let mut par = original.clone();
            super::apply_vertical_lift(
                &mut seq,
                stride,
                width,
                height,
                start_parity,
                coeff,
                use_wide,
            );
            super::apply_vertical_lift_parallel(
                &mut par,
                stride,
                width,
                height,
                start_parity,
                coeff,
                use_wide,
            );
            assert_eq!(
                seq, par,
                "use_wide={use_wide} start_parity={start_parity} coeff={coeff}"
            );
        }
    }

    #[test]
    fn apply_vertical_lift_parallel_matches_sequential_scalar() {
        check_vertical_lift_parallel_matches_sequential(false);
    }

    #[test]
    #[cfg(feature = "simd")]
    fn apply_vertical_lift_parallel_matches_sequential_wide() {
        check_vertical_lift_parallel_matches_sequential(true);
    }

    #[test]
    fn forward_then_inverse_97_roundtrips_within_tolerance_for_small_images() {
        for height in 1..=8 {
            for width in 1..=8 {
                let levels = max_decompositions(width, height).min(3) as u8;
                for (name, original) in tiny_patterns(width, height) {
                    let mut data = original.iter().map(|&v| v as f32).collect::<Vec<_>>();
                    forward_97_2d_in_place(&mut data, width, height, levels).expect("forward dwt");
                    inverse_97_2d_in_place(&mut data, width, height, levels);
                    for (idx, (&out, &orig)) in data.iter().zip(original.iter()).enumerate() {
                        let diff = (out - orig as f32).abs();
                        assert!(
                            diff < ROUNDTRIP_TOL,
                            "{name} {width}x{height} levels={levels} idx={idx} diff={diff} out={out} orig={orig}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn annex_f_origin_phases_roundtrip_within_tolerance() {
        for y0 in 0..8usize {
            for x0 in 0..8usize {
                for height in 1..=9usize {
                    for width in 1..=9usize {
                        let levels = max_decompositions(width, height).min(3) as u8;
                        let original = (0..height)
                            .flat_map(|y| {
                                (0..width).map(move |x| {
                                    let gx = (x0 + x) as f32;
                                    let gy = (y0 + y) as f32;
                                    gx * 1.75 - gy * 2.25 + (gx * gy * 0.03125)
                                })
                            })
                            .collect::<Vec<_>>();
                        let mut data = original.clone();
                        forward_97_2d_in_place_at(&mut data, width, height, levels, x0, y0)
                            .expect("phase-aware forward 9/7");
                        inverse_97_2d_in_place_at(&mut data, width, height, levels, x0, y0)
                            .expect("phase-aware inverse 9/7");
                        for (actual, expected) in data.iter().zip(&original) {
                            assert!(
                                (actual - expected).abs() < 2e-3,
                                "origin=({x0},{y0}) size={width}x{height}: {actual} != {expected}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn constant_signal_concentrates_into_single_ll_coefficient() {
        // Per Annex F, the low-pass filter of the 9/7 is normalized so that a
        // DC input survives only in LL. After N levels, only position (0,0)
        // should be non-zero; all detail bands should be near zero.
        let (w, h) = (8usize, 8usize);
        let mut data = vec![128.0f32; w * h];
        forward_97_2d_in_place(&mut data, w, h, 3).expect("forward dwt");
        for y in 0..h {
            for x in 0..w {
                if x == 0 && y == 0 {
                    continue;
                }
                let v = data[y * w + x].abs();
                assert!(v < 1e-2, "detail coefficient at ({x},{y}) = {v}");
            }
        }
        assert!(data[0].abs() > 1.0, "LL coefficient should be non-trivial");
    }

    #[test]
    fn annex_f_even_origin_length_one_signal_is_identity() {
        let mut row = [100.0f32];
        forward_97_1d_in_place(&mut row);
        assert_eq!(row, [100.0]);
        inverse_97_1d_in_place(&mut row);
        assert_eq!(row, [100.0]);
    }

    #[test]
    fn annex_f_odd_origin_length_one_signal_keeps_highpass_normalization() {
        let mut row = [100.0f32];
        let mut scratch = [0.0f32];
        forward_97_1d_with_scratch_phase(&mut row, &mut scratch, false, false);
        assert_ne!(row, [100.0]);
        inverse_97_1d_with_scratch_phase(&mut row, &mut scratch, false, false);
        assert!((row[0] - 100.0).abs() < ROUNDTRIP_TOL);
    }

    #[test]
    fn one_row_resolution_is_an_identity_in_both_directions() {
        // Annex F: an even-origin one-sample interval is already the low-pass
        // signal, so the split and its synthesis are identities. OpenJPEG's
        // `opj_dwt_encode_and_deinterleave_v_real` / `opj_v8dwt_decode` both
        // return on height 1 without scaling. Scaling by K/INV_K here still
        // round-trips through our own encoder but compounds once per degenerate
        // level against a conformant stream: a codestream signalling more
        // resolutions than its dimensions need passes through several 1x1
        // levels and its DC comes out multiplied by K^n.
        let original = vec![7.5f32, -3.25, 11.0, 0.0];
        let mut data = original.clone();
        forward_97_vertical_in_place(&mut data, 4, 4, 1, false);
        assert_eq!(data, original, "forward one-row pass must not scale");
        inverse_97_vertical_in_place(&mut data, 4, 4, 1, false);
        assert_eq!(data, original, "inverse one-row pass must not scale");
    }

    #[test]
    fn banded_vertical_roundtrips_widths_beyond_one_band() {
        // Force at least two vertical bands (VERTICAL_BAND_COLS = 256) so the
        // band gather/scatter and the trailing partial band are both exercised.
        let width = super::VERTICAL_BAND_COLS + 37;
        let height = 41usize;
        let levels = 3u8;
        let original: Vec<f32> = (0..width * height)
            .map(|i| {
                let x = (i % width) as f32;
                let y = (i / width) as f32;
                x * 0.17 - y * 0.31 + (x * y * 0.001)
            })
            .collect();
        let mut data = original.clone();
        forward_97_2d_in_place(&mut data, width, height, levels).expect("forward");
        inverse_97_2d_in_place(&mut data, width, height, levels);
        for (actual, expected) in data.iter().zip(&original) {
            assert!(
                (actual - expected).abs() < 2e-3,
                "{actual} vs {expected} at multi-band {width}x{height}"
            );
        }
    }

    #[test]
    fn annex_f_degenerate_axis_roundtrips_at_five_levels() {
        // The fifth vertical step is 1 -> 1 for this 176x16 signal while the
        // horizontal low-pass interval continues shrinking. This is the shape
        // used by real Kakadu JPX streams that previously drifted after IDWT.
        let (width, height, levels) = (176usize, 16usize, 5u8);
        let original = (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    ((x as f32 * 0.17).sin() * 31.0)
                        + ((y as f32 * 0.41).cos() * 47.0)
                        + (x ^ y) as f32 * 0.03125
                })
            })
            .collect::<Vec<_>>();
        let mut transformed = original.clone();
        forward_97_2d_in_place(&mut transformed, width, height, levels)
            .expect("degenerate-axis forward DWT");
        inverse_97_2d_in_place(&mut transformed, width, height, levels);
        for (index, (actual, expected)) in transformed.iter().zip(&original).enumerate() {
            assert!(
                (actual - expected).abs() < 2e-3,
                "sample {index}: {actual} != {expected}"
            );
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
