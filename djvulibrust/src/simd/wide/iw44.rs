//! IW44 wavelet transform SIMD kernels.
//!
//! `filter_fv` (vertical) is the first target: for `scale == 1`, each row is
//! contiguous in memory (no interleaving), so vectorizing across columns is
//! straightforward once the fused predict/update loop is split into two
//! passes. `filter_fh` (horizontal) is harder — at `scale == 1`, low/high
//! samples are interleaved in the same row (`L H L H ...`), so a SIMD
//! kernel needs either a deinterleave/scratch step or shuffle instructions;
//! it stays scalar for now (see llm-docs/SIMD_AND_PARALLELISM_PLAN.md
//! Phase 2, "horizontal transform is harder").
//!
//! Measured result (`examples/benchmark.rs`, 1600x1200 synthetic image,
//! release build, 5 wavelet levels): `filter_fv_scale1_wide` cuts the whole
//! `Encode::forward` transform stage (both filters, all 5 levels — only
//! level 0's `filter_fv` is actually vectorized here) from ~10.7ms to
//! ~7.4ms per call, a consistent ~31% wall-time reduction across repeated
//! runs. End-to-end page encode improves ~8% (626ms -> 577ms/iter). This is
//! a real win, unlike the color kernel in `wide::color` — the difference is
//! `load8`/`store8` here use genuine bulk vector load/widen and
//! narrow/store instructions (`i16x8::from`/`from_i32x8_truncate`), not a
//! per-lane scalar gather. An earlier version of this kernel *did* use a
//! per-lane scalar gather (same mistake as the color kernel) and measured
//! ~0% improvement — fixed before this became the default. Installed under
//! both `setup_all` and `setup_auto`.
//!
//! `filter_fh_split_scalar` (test-only, see its doc comment) was built to
//! prove the horizontal split is *correct* before attempting SIMD on it
//! (Phase 2C in llm-docs/SIMD_AND_PARALLELISM_PLAN.md), and measured ~85%
//! *slower* than the original streaming scalar (2.79ms -> 5.16ms/iter,
//! `filter_fh_split_scalar_benchmark`) — dominated by a `Vec<Vec<i32>>`
//! scratch allocation the split requires (see below). It is **not** wired
//! into any dispatch table. Horizontal SIMD (Phase 2D) stays deferred: it
//! needs both a deinterleave/scratch design for the `L H L H` interleaving
//! *and* now a proven way to carry full i32 predict precision across the
//! predict/update boundary without per-row heap allocation — a real
//! finding, not just the "harder to vectorize" issue the plan anticipated.

use super::super::Primitives;
use wide::{i16x8, i32x8};

pub(super) fn setup_all(primitives: &mut Primitives) {
    primitives.iw44.filter_fv = filter_fv_dispatch;
}

pub(super) fn setup_auto(primitives: &mut Primitives) {
    // filter_fv_scale1_wide measured faster than scalar for scale==1 rows
    // above the size threshold below — see examples/benchmark.rs and
    // llm-docs/SIMD_AND_PARALLELISM_PLAN.md. filter_fh stays scalar (no
    // wide kernel implemented yet, see module doc above).
    primitives.iw44.filter_fv = filter_fv_dispatch;
}

const LANES: usize = 8;
/// Below this row width, per-chunk SIMD setup/store overhead isn't worth it
/// — matches jp2lam's own row-parallel threshold philosophy (measure, don't
/// assume small inputs benefit). Chosen conservatively; revisit with data.
const MIN_WIDTH_FOR_WIDE: usize = 32;

/// Bulk contiguous load + widen (single vector load + sign-extend on
/// avx2/sse2, not a per-lane scalar gather — see `i16x8::from` / `wide`'s
/// `from_i16x8`, which picks a real SIMD widen instruction per target).
fn load8(buf: &[i16], offset: usize) -> i32x8 {
    let arr: [i16; LANES] = buf[offset..offset + LANES].try_into().unwrap();
    i32x8::from_i16x8(i16x8::new(arr))
}

/// Bulk contiguous store + narrow. Uses `from_i32x8_truncate`, the
/// non-saturating (wrapping) narrow — matching Rust's `as i16` semantics
/// exactly, unlike a saturating pack (e.g. AVX2 `packssdw`), which would
/// silently change output whenever a lane overflows i16 range. See the
/// advice in llm-docs/SIMD_AND_PARALLELISM_PLAN.md on this exact trap.
fn store8(buf: &mut [i16], offset: usize, v: i32x8) {
    let narrowed = i16x8::from_i32x8_truncate(v);
    buf[offset..offset + LANES].copy_from_slice(narrowed.as_array_ref());
}

/// Vectorized generic-row predict: `buf[q..q+w] -= (9*(row-1 + row+1) - (row-3 + row+3) + 8) >> 4`.
/// Caller guarantees `q >= s3` (see safety argument in the doc comment on
/// `filter_fv_scale1_wide` below), so no bounds guards are needed here —
/// same simplification the scalar generic branch already relies on.
fn predict_row_generic_wide(buf: &mut [i16], q: usize, w: usize, s: usize, s3: usize) {
    let c8 = i32x8::new([8; LANES]);
    let mut x = 0;
    while x + LANES <= w {
        let off = q + x;
        let a = load8(buf, off - s) + load8(buf, off + s);
        let b = load8(buf, off - s3) + load8(buf, off + s3);
        let cur = load8(buf, off);
        let delta = ((a << 3i32) + a - b + c8) >> 4i32;
        store8(buf, off, cur - delta);
        x += LANES;
    }
    while x < w {
        let off = q + x;
        let a = buf[off - s] as i32 + buf[off + s] as i32;
        let b = buf[off - s3] as i32 + buf[off + s3] as i32;
        let delta = ((a << 3) + a - b + 8) >> 4;
        buf[off] = (buf[off] as i32 - delta) as i16;
        x += 1;
    }
}

/// Vectorized generic-row update: `buf[q..q+w] += (9*(row-1 + row+1) - (row-3 + row+3) + 16) >> 5`.
/// Same `q >= s3` guarantee as `predict_row_generic_wide`.
fn update_row_generic_wide(buf: &mut [i16], q: usize, w: usize, s: usize, s3: usize) {
    let c16 = i32x8::new([16; LANES]);
    let mut x = 0;
    while x + LANES <= w {
        let off = q + x;
        let a = load8(buf, off - s) + load8(buf, off + s);
        let b = load8(buf, off - s3) + load8(buf, off + s3);
        let cur = load8(buf, off);
        let delta = ((a << 3i32) + a - b + c16) >> 5i32;
        store8(buf, off, cur + delta);
        x += LANES;
    }
    while x < w {
        let off = q + x;
        let a = buf[off - s] as i32 + buf[off + s] as i32;
        let b = buf[off - s3] as i32 + buf[off + s3] as i32;
        let delta = ((a << 3) + a - b + 16) >> 5;
        buf[off] = (buf[off] as i32 + delta) as i16;
        x += 1;
    }
}

/// `filter_fv` for `scale == 1` only: the generic-row branches (the vast
/// majority of rows in a typical image, see llm-docs/SIMD_AND_PARALLELISM_PLAN.md
/// "Approximate work distribution by level") are vectorized 8 columns at a
/// time; every boundary/special-case branch is a literal copy of
/// `filter_fv_split_scalar`'s corresponding branch with `scale` hardcoded
/// to `1` (i.e. `q += scale` becomes `q += 1`).
///
/// Safety argument for dropping the `if q >= s`/`if q >= s3` guards in the
/// generic branches: within the generic predict branch, `y >= 3` and
/// `q == p + column_offset` where `p == s*y` and `column_offset >= 0`, so
/// `q >= 3*s == s3 >= s` always. Symmetrically for the generic update
/// branch, `y >= 6` gives `q_i == s*(y-3) >= 3*s == s3`, so `q >= s3 >= s`
/// for every column in that row. Both are proven, not assumed — see the
/// `filter_fv_split_scalar` equality tests, which exercise scale==1 across
/// the same matrix and would catch a violation.
fn filter_fv_scale1_wide(buf: &mut [i16], w: usize, h: usize, rowsize: usize) {
    let s = rowsize;
    let s3 = s + s + s;
    let hlimit = h;

    // Pass 1: predict all high rows.
    {
        let mut y = 1usize;
        let mut p = s;
        while (y as isize) - 3 < hlimit as isize {
            let q = p;
            if y >= 3 && y + 3 < hlimit {
                predict_row_generic_wide(buf, q, w, s, s3);
            } else if y < hlimit {
                let e = q + w;
                let mut qq = q;
                let mut q1 = if y + 1 < hlimit { q + s } else { q - s };
                while qq < e {
                    let a = buf[qq - s] as i32 + buf[q1] as i32;
                    buf[qq] = (buf[qq] as i32 - ((a + 1) >> 1)) as i16;
                    qq += 1;
                    q1 += 1;
                }
            }
            y += 2;
            p += s + s;
        }
    }

    // Pass 2: update all low rows, using pass-1-finalized high rows.
    {
        let mut y = 1usize;
        let mut p = s;
        while (y as isize) - 3 < hlimit as isize {
            let q_i = p as isize - s3 as isize;
            if q_i >= 0 {
                let q = q_i as usize;
                if y >= 6 && y < hlimit {
                    update_row_generic_wide(buf, q, w, s, s3);
                } else if y >= 3 {
                    let e = q + w;
                    let mut qq = q;
                    let mut q1 = if y >= 2 && y - 2 < hlimit {
                        Some(q + s)
                    } else {
                        None
                    };
                    let mut q3 = if y < hlimit { Some(q + s3) } else { None };

                    if y >= 6 {
                        while qq < e {
                            let a = if qq >= s { buf[qq - s] as i32 } else { 0 }
                                + q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = if qq >= s3 { buf[qq - s3] as i32 } else { 0 }
                                + q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[qq] = (buf[qq] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            qq += 1;
                            if let Some(ref mut idx) = q1 {
                                *idx += 1;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += 1;
                            }
                        }
                    } else if y >= 4 {
                        while qq < e {
                            let a = if qq >= s { buf[qq - s] as i32 } else { 0 }
                                + q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[qq] = (buf[qq] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            qq += 1;
                            if let Some(ref mut idx) = q1 {
                                *idx += 1;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += 1;
                            }
                        }
                    } else {
                        while qq < e {
                            let a = q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[qq] = (buf[qq] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            qq += 1;
                            if let Some(ref mut idx) = q1 {
                                *idx += 1;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += 1;
                            }
                        }
                    }
                }
            }
            y += 2;
            p += s + s;
        }
    }
}

/// Dispatched from `crate::simd::PRIMITIVES.iw44.filter_fv`: routes to the
/// scale==1 SIMD kernel when it applies and the row is wide enough to
/// amortize SIMD setup, otherwise falls back to the original scalar
/// `filter_fv` (proven equal to `filter_fv_split_scalar`, so this fallback
/// is exactly as correct as scalar always was).
fn filter_fv_dispatch(buf: &mut [i16], w: usize, h: usize, rowsize: usize, scale: usize) {
    if scale == 1 && w >= MIN_WIDTH_FOR_WIDE {
        filter_fv_scale1_wide(buf, w, h, rowsize);
    } else {
        crate::encode::iw44::transform::filter_fv(buf, w, h, rowsize, scale);
    }
}

/// Mechanical split of `filter_fv`'s fused streaming predict+update loop
/// into two full passes over the buffer:
///
///   Pass 1 (predict): writes only "high" rows, reading only original
///   ("low") rows that pass 1 never touches.
///   Pass 2 (update): writes only "low" rows, reading only "high" rows —
///   all of which pass 1 has already finalized by the time pass 2 runs.
///
/// This split is safe because the original fused loop's update step at
/// iteration `y` only ever reads high rows at or before the current `y`
/// (never a not-yet-predicted future row), so running all of pass 1 before
/// any of pass 2 can only give those reads *more* finished data than the
/// fused order did, never less. Every branch condition and formula below is
/// copied verbatim from `transform::filter_fv` — this function changes
/// execution order, not arithmetic — and is checked bit-for-bit against it
/// in the tests below across a wide matrix of sizes/scales/data.
///
/// Exists to (a) prove the split is safe before any SIMD touches it, and
/// (b) serve as the literal scalar template the `scale1_wide` kernel below
/// vectorizes the generic-row inner loop of.
#[cfg(test)]
pub(super) fn filter_fv_split_scalar(
    buf: &mut [i16],
    w: usize,
    h: usize,
    rowsize: usize,
    scale: usize,
) {
    let s = scale * rowsize;
    let s3 = s + s + s;
    let h_adj = if h > 0 { ((h - 1) / scale) + 1 } else { 0 };
    let hlimit = h_adj;

    // Pass 1: predict all high rows.
    {
        let mut y = 1usize;
        let mut p = s;
        while y as isize - 3 < hlimit as isize {
            let mut q = p;
            let e = q + w;
            if y >= 3 && y + 3 < hlimit {
                while q < e {
                    let a = if q >= s { buf[q - s] as i32 } else { 0 } + buf[q + s] as i32;
                    let b = if q >= s3 { buf[q - s3] as i32 } else { 0 } + buf[q + s3] as i32;
                    buf[q] = (buf[q] as i32 - (((a << 3) + a - b + 8) >> 4)) as i16;
                    q += scale;
                }
            } else if y < hlimit {
                let mut q1 = if y + 1 < hlimit { q + s } else { q - s };
                while q < e {
                    let val_qs = buf[q - s] as i32;
                    let val_q1 = buf[q1] as i32;
                    let a = val_qs + val_q1;
                    buf[q] = (buf[q] as i32 - ((a + 1) >> 1)) as i16;
                    q += scale;
                    q1 += scale;
                }
            }
            y += 2;
            p += s + s;
        }
    }

    // Pass 2: update all low rows, using pass-1-finalized high rows.
    {
        let mut y = 1usize;
        let mut p = s;
        while y as isize - 3 < hlimit as isize {
            let q_i = p as isize - s3 as isize;
            if q_i >= 0 {
                let mut q = q_i as usize;
                let e = q + w;
                if y >= 6 && y < hlimit {
                    while q < e {
                        let a = if q >= s { buf[q - s] as i32 } else { 0 } + buf[q + s] as i32;
                        let b = if q >= s3 { buf[q - s3] as i32 } else { 0 } + buf[q + s3] as i32;
                        buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                        q += scale;
                    }
                } else if y >= 3 {
                    let mut q1 = if y >= 2 && y - 2 < hlimit {
                        Some(q + s)
                    } else {
                        None
                    };
                    let mut q3 = if y < hlimit { Some(q + s3) } else { None };

                    if y >= 6 {
                        while q < e {
                            let a = if q >= s { buf[q - s] as i32 } else { 0 }
                                + q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = if q >= s3 { buf[q - s3] as i32 } else { 0 }
                                + q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 {
                                *idx += scale;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += scale;
                            }
                        }
                    } else if y >= 4 {
                        while q < e {
                            let a = if q >= s { buf[q - s] as i32 } else { 0 }
                                + q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 {
                                *idx += scale;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += scale;
                            }
                        }
                    } else {
                        while q < e {
                            let a = q1.map(|idx| buf[idx] as i32).unwrap_or(0);
                            let b = q3.map(|idx| buf[idx] as i32).unwrap_or(0);
                            buf[q] = (buf[q] as i32 + (((a << 3) + a - b + 16) >> 5)) as i16;
                            q += scale;
                            if let Some(ref mut idx) = q1 {
                                *idx += scale;
                            }
                            if let Some(ref mut idx) = q3 {
                                *idx += scale;
                            }
                        }
                    }
                }
            }
            y += 2;
            p += s + s;
        }
    }
}

/// Mechanical split of `filter_fh`'s fused streaming predict+update loop,
/// same technique as `filter_fv_split_scalar` above but at the per-sample
/// level (horizontal low/high samples are interleaved `L H L H ...` within
/// one row, rather than low/high living in separate contiguous rows like
/// vertical does — see the module doc's note on why horizontal is harder).
///
/// This went through two failed attempts before the version below, both
/// caught by the exhaustive equality test rather than reasoning alone —
/// worth keeping as documented traps for whoever touches this next:
///
/// 1. **Nesting bug.** The original's three trailing `while` loops are
///    *siblings* of the `if q < e` prologue, not nested inside it — they
///    must still run when the prologue's condition is false (e.g.
///    `w < scale`, so `q` starts already `>= e`). An early version nested
///    them inside the `if`, silently dropping valid update work for narrow
///    rows.
/// 2. **Precision-loss bug.** The original's `a`/`b` registers are `i32`
///    and are carried across loop iterations *by register*, never by
///    re-reading the output buffer — only the final `buf[q] = b3 as i16`
///    write is truncated. A split that has pass 2 recover `b3` by reading
///    `buf[q]` back is reading the *lossy, truncated* value, which differs
///    from the original whenever a predict value overflows i16 range (the
///    `extremes` test pattern below hits this). Fixed by having pass 1
///    additionally record the full-precision `i32` predict values into a
///    scratch array (`highs_per_row`), and pass 2 read from that instead of
///    the buffer. Pass 1 still writes the truncated i16 to `buf[q]` too —
///    that's the real output later levels/callers need.
#[cfg(test)]
pub(super) fn filter_fh_split_scalar(
    buf: &mut [i16],
    w: usize,
    h: usize,
    mut rowsize: usize,
    scale: usize,
) {
    let s = scale;
    let s3 = s + s + s;
    rowsize *= scale;

    // Pass 1: predict all high positions, recording full i32 precision.
    let mut highs_per_row: Vec<Vec<i32>> = Vec::new();
    {
        let mut y = 0usize;
        let mut p = 0usize;
        while y < h {
            let mut q = p + s;
            let e = p + w;
            let mut row_highs = Vec::new();

            let mut a1 = 0i32;
            let mut a2 = 0i32;
            let mut a3 = 0i32;

            if q < e {
                a1 = buf[q - s] as i32;
                a2 = a1;
                a3 = a1;
                if q + s < e {
                    a2 = buf[q + s] as i32;
                }
                if q + s3 < e {
                    a3 = buf[q + s3] as i32;
                }
                let b3 = (buf[q] as i32) - ((a1 + a2 + 1) >> 1);
                buf[q] = b3 as i16;
                row_highs.push(b3);
                q += s + s;
            }

            while q + s3 < e {
                let a0 = a1;
                a1 = a2;
                a2 = a3;
                a3 = buf[q + s3] as i32;
                let b3 = (buf[q] as i32) - ((((a1 + a2) << 3) + (a1 + a2) - a0 - a3 + 8) >> 4);
                buf[q] = b3 as i16;
                row_highs.push(b3);
                q += s + s;
            }

            while q < e {
                a1 = a2;
                a2 = a3;
                let b3 = (buf[q] as i32) - ((a1 + a2 + 1) >> 1);
                buf[q] = b3 as i16;
                row_highs.push(b3);
                q += s + s;
            }
            // The trailing "while (q-s3) < e" loop in the original does
            // only update work (b3 forced to 0, no predict) — nothing for
            // pass 1 to record there.

            highs_per_row.push(row_highs);
            y += scale;
            p += rowsize;
        }
    }

    // Pass 2: update all low positions, using pass-1's full-precision highs
    // (not a lossy re-read of the truncated i16 buffer — see doc comment).
    {
        let mut y = 0usize;
        let mut p = 0usize;
        let mut row_idx = 0usize;
        while y < h {
            let mut q = p + s;
            let e = p + w;
            let row_highs = &highs_per_row[row_idx];
            let mut hi = 0usize;

            let mut b1 = 0i32;
            let mut b2 = 0i32;
            let mut b3 = 0i32;

            if q < e {
                b3 = row_highs[hi];
                hi += 1;
                q += s + s;
            }

            while q + s3 < e {
                let b0 = b1;
                b1 = b2;
                b2 = b3;
                b3 = row_highs[hi];
                hi += 1;
                let idx_i = q as isize - s3 as isize;
                if idx_i >= 0 {
                    let idx = idx_i as usize;
                    let updated =
                        (buf[idx] as i32) + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
                    buf[idx] = updated as i16;
                }
                q += s + s;
            }

            while q < e {
                let b0 = b1;
                b1 = b2;
                b2 = b3;
                b3 = row_highs[hi];
                hi += 1;
                let idx_i = q as isize - s3 as isize;
                if idx_i >= p as isize {
                    let idx = idx_i as usize;
                    let updated =
                        (buf[idx] as i32) + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
                    buf[idx] = updated as i16;
                }
                q += s + s;
            }

            while (q as isize) - (s3 as isize) < e as isize {
                let b0 = b1;
                b1 = b2;
                b2 = b3;
                b3 = 0;
                let idx_i = q as isize - s3 as isize;
                if idx_i >= p as isize {
                    let idx = idx_i as usize;
                    let updated =
                        (buf[idx] as i32) + ((((b1 + b2) << 3) + (b1 + b2) - b0 - b3 + 16) >> 5);
                    buf[idx] = updated as i16;
                }
                q += s + s;
            }

            row_idx += 1;
            y += scale;
            p += rowsize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::iw44::transform::filter_fh as filter_fh_original;
    use crate::encode::iw44::transform::filter_fv as filter_fv_original;

    fn xorshift(state: &mut u32) -> u32 {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        *state
    }

    fn make_buf(rowsize: usize, h: usize, pattern: &str, seed: u32) -> Vec<i16> {
        let n = rowsize * h;
        match pattern {
            "zero" => vec![0i16; n],
            "one" => vec![1i16; n],
            "neg_one" => vec![-1i16; n],
            "ramp_x" => (0..n)
                .map(|i| ((i % rowsize) as i32 - (rowsize as i32 / 2)) as i16)
                .collect(),
            "ramp_y" => (0..n)
                .map(|i| ((i / rowsize) as i32 - (h as i32 / 2)) as i16)
                .collect(),
            "checkerboard" => (0..n)
                .map(|i| {
                    let x = i % rowsize;
                    let y = i / rowsize;
                    if (x + y) % 2 == 0 { 1000i16 } else { -1000i16 }
                })
                .collect(),
            "random_small" => {
                let mut state = seed | 1;
                (0..n)
                    .map(|_| ((xorshift(&mut state) % 200) as i32 - 100) as i16)
                    .collect()
            }
            "random_full" => {
                let mut state = seed | 1;
                (0..n).map(|_| xorshift(&mut state) as i16).collect()
            }
            "extremes" => {
                let mut state = seed | 1;
                (0..n)
                    .map(|_| {
                        if xorshift(&mut state) % 2 == 0 {
                            i16::MIN + (xorshift(&mut state) % 4) as i16
                        } else {
                            i16::MAX - (xorshift(&mut state) % 4) as i16
                        }
                    })
                    .collect()
            }
            _ => unreachable!(),
        }
    }

    const SIZES: &[usize] = &[0, 1, 2, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65];
    const PATTERNS: &[&str] = &[
        "zero",
        "one",
        "neg_one",
        "ramp_x",
        "ramp_y",
        "checkerboard",
        "random_small",
        "random_full",
        "extremes",
    ];

    #[test]
    fn split_scalar_matches_original_exhaustive() {
        let mut cases = 0usize;
        for &w in SIZES {
            for &h in &[0usize, 1, 2, 3, 4, 5, 7, 8, 16, 17, 31, 32, 65] {
                if w == 0 || h == 0 {
                    continue;
                }
                for &rowsize_extra in &[0usize, 1, 7] {
                    let rowsize = w + rowsize_extra;
                    for &scale in &[1usize, 2, 4] {
                        // Original filter_fv's `y` loop only exercises rows
                        // when h/scale allows at least the smallest window;
                        // trivial combinations are still valid inputs, so
                        // just run them all rather than filtering.
                        for &pattern in PATTERNS {
                            let seed =
                                (w * 7919 + h * 104729 + rowsize * 13 + scale * 5 + pattern.len())
                                    as u32;
                            let mut buf_a = make_buf(rowsize, h, pattern, seed);
                            let mut buf_b = buf_a.clone();

                            filter_fv_original(&mut buf_a, w, h, rowsize, scale);
                            filter_fv_split_scalar(&mut buf_b, w, h, rowsize, scale);

                            assert_eq!(
                                buf_a, buf_b,
                                "mismatch: w={w} h={h} rowsize={rowsize} scale={scale} pattern={pattern}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert!(
            cases > 100,
            "expected a large exhaustive matrix, got {cases}"
        );
    }

    #[test]
    fn wide_scale1_matches_original_exhaustive() {
        // Widths straddling both the 8-lane boundary and the
        // MIN_WIDTH_FOR_WIDE(32) dispatch threshold.
        const WIDE_SIZES: &[usize] = &[
            1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 39, 40, 63, 64, 65, 100, 127, 128, 129,
        ];
        let mut cases = 0usize;
        for &w in WIDE_SIZES {
            for &h in &[1usize, 2, 3, 4, 5, 7, 8, 16, 17, 31, 32, 33, 65] {
                for &rowsize_extra in &[0usize, 1, 7] {
                    let rowsize = w + rowsize_extra;
                    for &pattern in PATTERNS {
                        let seed = (w * 7919 + h * 104729 + rowsize * 13 + pattern.len()) as u32;
                        let mut buf_a = make_buf(rowsize, h, pattern, seed);
                        let mut buf_b = buf_a.clone();

                        filter_fv_original(&mut buf_a, w, h, rowsize, 1);
                        filter_fv_scale1_wide(&mut buf_b, w, h, rowsize);

                        assert_eq!(
                            buf_a, buf_b,
                            "mismatch: w={w} h={h} rowsize={rowsize} pattern={pattern}"
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert!(
            cases > 100,
            "expected a large exhaustive matrix, got {cases}"
        );
    }

    #[test]
    fn dispatch_matches_original_including_fallback_paths() {
        // Exercise filter_fv_dispatch end-to-end: scale==1 large (SIMD
        // path), scale==1 small (below MIN_WIDTH_FOR_WIDE, scalar
        // fallback), and scale>1 (always scalar fallback).
        for &(w, h, scale) in &[
            (100usize, 40usize, 1usize),
            (16, 20, 1),  // below MIN_WIDTH_FOR_WIDE
            (100, 40, 2), // scale != 1
            (100, 40, 4),
        ] {
            let rowsize = w + 3;
            for &pattern in PATTERNS {
                let seed = (w * 7919 + h * 104729 + scale * 13 + pattern.len()) as u32;
                let mut buf_a = make_buf(rowsize, h, pattern, seed);
                let mut buf_b = buf_a.clone();

                filter_fv_original(&mut buf_a, w, h, rowsize, scale);
                filter_fv_dispatch(&mut buf_b, w, h, rowsize, scale);

                assert_eq!(
                    buf_a, buf_b,
                    "dispatch mismatch: w={w} h={h} scale={scale} pattern={pattern}"
                );
            }
        }
    }

    #[test]
    fn filter_fh_split_scalar_matches_original_exhaustive() {
        // Deliberately includes w < scale (e.g. w=1,2,3 with scale=4) —
        // the case that would have caught the prologue-nesting bug found
        // and fixed while writing filter_fh_split_scalar (see its doc
        // comment): with w < scale, `q = p + s >= e`, so the prologue's
        // `if q < e` is false and the sibling while-loops must still run
        // unnested for the row to be processed at all.
        let mut cases = 0usize;
        for &w in SIZES {
            for &h in &[0usize, 1, 2, 3, 4, 5, 7, 8, 16, 17, 31, 32, 65] {
                if w == 0 || h == 0 {
                    continue;
                }
                for &rowsize_extra in &[0usize, 1, 7] {
                    let rowsize = w + rowsize_extra;
                    for &scale in &[1usize, 2, 4, 8] {
                        for &pattern in PATTERNS {
                            let seed =
                                (w * 7919 + h * 104729 + rowsize * 13 + scale * 5 + pattern.len())
                                    as u32;
                            let mut buf_a = make_buf(rowsize, h, pattern, seed);
                            let mut buf_b = buf_a.clone();

                            filter_fh_original(&mut buf_a, w, h, rowsize, scale);
                            filter_fh_split_scalar(&mut buf_b, w, h, rowsize, scale);

                            assert_eq!(
                                buf_a, buf_b,
                                "mismatch: w={w} h={h} rowsize={rowsize} scale={scale} pattern={pattern}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert!(
            cases > 100,
            "expected a large exhaustive matrix, got {cases}"
        );
    }

    #[test]
    fn filter_fh_split_scalar_benchmark() {
        // Not a correctness test — a quick in-process timing comparison to
        // decide whether the horizontal split-scalar refactor is worth
        // wiring in as the new scalar default even before any SIMD touches
        // it (Phase 2C in llm-docs/SIMD_AND_PARALLELISM_PLAN.md: "measure
        // it, don't assume"). Run with `--nocapture` to see the numbers;
        // this asserts nothing about which is faster, just that both
        // produce identical output before timing (so a regression in one
        // can't silently invalidate the comparison).
        let w = 1600usize;
        let h = 1200usize;
        let rowsize = (w + 31) & !31;
        let padded_h = (h + 31) & !31;
        let pristine: Vec<i16> = (0..rowsize * padded_h)
            .map(|i| (((i as u32).wrapping_mul(2654435761) >> 16) as i16).wrapping_sub(16384))
            .collect();

        let iters = 30;
        let mut buf = pristine.clone();

        let start = std::time::Instant::now();
        for _ in 0..iters {
            buf.copy_from_slice(&pristine);
            filter_fh_original(&mut buf, w, h, rowsize, 1);
        }
        let original_elapsed = start.elapsed();
        let original_result = buf.clone();

        let mut buf2 = pristine.clone();
        let start = std::time::Instant::now();
        for _ in 0..iters {
            buf2.copy_from_slice(&pristine);
            filter_fh_split_scalar(&mut buf2, w, h, rowsize, 1);
        }
        let split_elapsed = start.elapsed();

        assert_eq!(
            original_result, buf2,
            "split and original must agree before comparing timing"
        );

        eprintln!(
            "filter_fh scale=1 {w}x{h}: original={:.3}ms/iter split_scalar={:.3}ms/iter",
            original_elapsed.as_secs_f64() * 1000.0 / iters as f64,
            split_elapsed.as_secs_f64() * 1000.0 / iters as f64,
        );
    }
}
