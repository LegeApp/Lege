// "The comparator's performance is critical; rewriting it to pixel-by-pixel is unacceptable.
// Fix bugs without changing the high-level algorithm."

//! jbig2comparator.rs
//! ==========================================================
//! Stage 1 keeps the existing word-wise shifted XOR/popcount
//! search to find the best alignment quickly.
//!
//! Stage 2 runs only once for that best shift and extracts
//! richer glyph metrics so family matching can distinguish
//! structural mismatch from harmless edge noise.
//! ==========================================================

use crate::jbig2simd::{shift_xor_popcnt_u32_rows, xor_popcnt_u32_rows};
use crate::jbig2sym::BitImage;

/// Maximum absolute shift (in pixels) that we search in x/y.
const SEARCH_RADIUS: i32 = 5;
const SEARCH_OFFSETS: [i32; (SEARCH_RADIUS as usize) * 2 + 1] =
    [0, -1, 1, -2, 2, -3, 3, -4, 4, -5, 5];
/// Maximum width/height delta that can still produce a match.
pub const MAX_DIMENSION_DELTA: usize = (SEARCH_RADIUS as usize) * 2;

#[derive(Debug, Clone, Copy, Default)]
pub struct CompareResult {
    pub total_err: u32,
    pub overlap_err: u32,
    pub outside_ink_err: u32,
    pub row_profile_err: u32,
    pub col_profile_err: u32,
    pub black_delta: u32,
    pub dx: i32,
    pub dy: i32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CollapseCompareLimits {
    pub outside_limit: u32,
    pub row_limit: u32,
    pub col_limit: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CompareWeights {
    pub overlap_err: u32,
    pub outside_ink_err: u32,
    pub row_profile_err: u32,
    pub col_profile_err: u32,
    pub black_delta: u32,
}

impl CompareWeights {
    pub const LIVE_MATCH: Self = Self {
        overlap_err: 8,
        outside_ink_err: 5,
        row_profile_err: 2,
        col_profile_err: 2,
        black_delta: 1,
    };

    pub const COLLAPSE: Self = Self {
        overlap_err: 6,
        outside_ink_err: 3,
        row_profile_err: 2,
        col_profile_err: 2,
        black_delta: 1,
    };

    pub const REFINE: Self = Self {
        overlap_err: 6,
        outside_ink_err: 4,
        row_profile_err: 2,
        col_profile_err: 2,
        black_delta: 1,
    };

    #[inline]
    pub fn score(&self, r: &CompareResult) -> u32 {
        self.overlap_err
            .saturating_mul(r.overlap_err)
            .saturating_add(self.outside_ink_err.saturating_mul(r.outside_ink_err))
            .saturating_add(self.row_profile_err.saturating_mul(r.row_profile_err))
            .saturating_add(self.col_profile_err.saturating_mul(r.col_profile_err))
            .saturating_add(self.black_delta.saturating_mul(r.black_delta))
    }
}

#[derive(Default)]
pub struct Comparator {
    col_a: Vec<u32>,
    col_b: Vec<u32>,
}

impl Comparator {
    #[inline]
    fn exact_match(a: &BitImage, b: &BitImage) -> bool {
        a.width == b.width && a.height == b.height && a.packed_words() == b.packed_words()
    }

    pub fn distance(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
    ) -> Option<(u32, i32, i32)> {
        self.best_alignment_result(a, b, max_err)
    }

    pub fn compare_overlap_only(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
    ) -> Option<CompareResult> {
        if Self::exact_match(a, b) {
            return Some(CompareResult {
                dx: 0,
                dy: 0,
                black_delta: 0,
                ..Default::default()
            });
        }

        if a.width.abs_diff(b.width) > MAX_DIMENSION_DELTA
            || a.height.abs_diff(b.height) > MAX_DIMENSION_DELTA
        {
            return None;
        }

        let (overlap_err, dx, dy) = self.best_alignment_by_xor(a, b, max_err)?;
        Some(CompareResult {
            total_err: overlap_err,
            overlap_err,
            black_delta: a.count_ones().abs_diff(b.count_ones()) as u32,
            dx,
            dy,
            ..Default::default()
        })
    }

    pub fn compare_detailed(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
    ) -> Option<CompareResult> {
        self.compare_detailed_with_limits(a, b, max_err, SEARCH_RADIUS, SEARCH_RADIUS)
    }

    pub fn compare_detailed_with_limits(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
        max_dx: i32,
        max_dy: i32,
    ) -> Option<CompareResult> {
        if Self::exact_match(a, b) {
            return Some(CompareResult {
                dx: 0,
                dy: 0,
                ..Default::default()
            });
        }

        if a.width.abs_diff(b.width) > MAX_DIMENSION_DELTA
            || a.height.abs_diff(b.height) > MAX_DIMENSION_DELTA
        {
            return None;
        }

        let (overlap_err, dx, dy) =
            self.best_alignment_by_xor_limited(a, b, max_err, max_dx, max_dy)?;
        let mut result = self.metrics_for_alignment(a, b, dx, dy);
        result.overlap_err = overlap_err.max(result.overlap_err);
        result.total_err = result.overlap_err.saturating_add(result.outside_ink_err);

        if result.total_err <= max_err {
            Some(result)
        } else {
            None
        }
    }

    pub fn compare_for_refine_family(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
        max_dx: i32,
        max_dy: i32,
    ) -> Option<CompareResult> {
        let (overlap_err, dx, dy) =
            self.best_alignment_result_with_limits(a, b, max_err, max_dx, max_dy)?;
        let result = CompareResult {
            total_err: overlap_err,
            overlap_err,
            dx,
            dy,
            ..Default::default()
        };
        if result.dx.abs() > max_dx || result.dy.abs() > max_dy {
            return None;
        }
        Some(result)
    }

    #[inline]
    fn best_alignment_result(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
    ) -> Option<(u32, i32, i32)> {
        self.best_alignment_result_with_limits(a, b, max_err, SEARCH_RADIUS, SEARCH_RADIUS)
    }

    #[inline]
    fn best_alignment_result_with_limits(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
        max_dx: i32,
        max_dy: i32,
    ) -> Option<(u32, i32, i32)> {
        if Self::exact_match(a, b) {
            return Some((0, 0, 0));
        }

        if a.width.abs_diff(b.width) > MAX_DIMENSION_DELTA
            || a.height.abs_diff(b.height) > MAX_DIMENSION_DELTA
        {
            return None;
        }

        self.best_alignment_by_xor_limited(a, b, max_err, max_dx, max_dy)
    }

    pub fn compare_for_collapse_family(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
        max_dx: i32,
        max_dy: i32,
    ) -> Option<CompareResult> {
        let result = self.compare_detailed_with_limits(a, b, max_err, max_dx, max_dy)?;
        if result.dx.abs() > max_dx || result.dy.abs() > max_dy {
            return None;
        }

        let limits = Self::collapse_compare_limits(&result);

        if result.outside_ink_err > limits.outside_limit
            || result.row_profile_err > limits.row_limit
            || result.col_profile_err > limits.col_limit
        {
            return None;
        }

        Some(result)
    }

    pub fn compare_for_symbol_unify(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
        max_dx: i32,
        max_dy: i32,
    ) -> Option<CompareResult> {
        let result = self.compare_detailed_with_limits(a, b, max_err, max_dx, max_dy)?;
        if result.dx.abs() > max_dx || result.dy.abs() > max_dy {
            return None;
        }
        Some(result)
    }

    pub fn collapse_compare_limits(result: &CompareResult) -> CollapseCompareLimits {
        let shift_slack = (result.dx.abs() + result.dy.abs()) as u32;
        CollapseCompareLimits {
            outside_limit: (result.overlap_err / 2).max(2)
                + result.black_delta.min(4)
                + shift_slack,
            row_limit: result.overlap_err.saturating_mul(6).saturating_add(24),
            col_limit: result.overlap_err.saturating_mul(6).saturating_add(24),
        }
    }

    fn best_alignment_by_xor(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
    ) -> Option<(u32, i32, i32)> {
        self.best_alignment_by_xor_limited(a, b, max_err, SEARCH_RADIUS, SEARCH_RADIUS)
    }

    fn best_alignment_by_xor_limited(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        max_err: u32,
        max_dx: i32,
        max_dy: i32,
    ) -> Option<(u32, i32, i32)> {
        let awpr = (a.width + 31) >> 5;
        let bwpr = (b.width + 31) >> 5;
        let a_words = a.packed_words();
        let b_words = b.packed_words();

        let mut best_err = max_err + 1;
        let mut best_dx = 0;
        let mut best_dy = 0;

        for &dy in &SEARCH_OFFSETS {
            if dy.abs() > max_dy {
                continue;
            }
            let top_i = dy.max(0);
            let bottom_i = (dy + b.height as i32).min(a.height as i32);
            if top_i >= bottom_i {
                continue;
            }
            let top = top_i as usize;
            let bottom = bottom_i as usize;
            let overlap_height = bottom - top;
            let b_row_start = (top as i32 - dy) as usize;

            for &dx in &SEARCH_OFFSETS {
                if dx.abs() > max_dx {
                    continue;
                }
                let left_i = dx.max(0);
                let right_i = (dx + b.width as i32).min(a.width as i32);
                if left_i >= right_i {
                    continue;
                }

                let overlap_width = (right_i - left_i) as usize;
                let cols_words = (overlap_width + 31) >> 5;
                let bit_shift = (dx & 31) as u32;
                let word_shift = (dx >> 5) as isize;
                let mut err = 0u32;
                let mut early_break = false;

                for row in 0..overlap_height {
                    let a_start = (top + row) * awpr;
                    let b_start = (b_row_start + row) * bwpr;
                    let a_row = &a_words[a_start..a_start + awpr];
                    let b_row = &b_words[b_start..b_start + bwpr];

                    let result = if bit_shift == 0 {
                        row_kernel_noshift(
                            a_row, b_row, cols_words, word_shift, err, best_err, max_err,
                        )
                    } else {
                        row_kernel_shift(
                            a_row, b_row, cols_words, word_shift, bit_shift, err, best_err, max_err,
                        )
                    };
                    err = result.0;
                    if result.1 {
                        early_break = true;
                        break;
                    }
                }

                if err < best_err {
                    best_err = err;
                    best_dx = dx;
                    best_dy = dy;
                } else if err == best_err && !early_break {
                    let curr_dist = dx.abs() + dy.abs();
                    let best_dist = best_dx.abs() + best_dy.abs();
                    if curr_dist < best_dist {
                        best_dx = dx;
                        best_dy = dy;
                    }
                }
            }
        }

        if best_err <= max_err {
            Some((best_err, best_dx, best_dy))
        } else {
            None
        }
    }

    fn ensure_profile_buffers(&mut self, cols: usize) {
        if self.col_a.len() < cols {
            self.col_a.resize(cols, 0);
            self.col_b.resize(cols, 0);
        }
    }

    fn metrics_for_alignment(
        &mut self,
        a: &BitImage,
        b: &BitImage,
        dx: i32,
        dy: i32,
    ) -> CompareResult {
        let min_x = 0.min(dx);
        let min_y = 0.min(dy);
        let max_x = (a.width as i32).max(dx + b.width as i32);
        let max_y = (a.height as i32).max(dy + b.height as i32);
        let used_rows = (max_y - min_y).max(0) as usize;
        let used_cols = (max_x - min_x).max(0) as usize;

        self.ensure_profile_buffers(used_cols);
        self.col_a[..used_cols].fill(0);
        self.col_b[..used_cols].fill(0);

        let mut overlap_err = 0u32;
        let mut outside_ink_err = 0u32;
        let mut row_profile_err = 0u32;
        for gy in min_y..max_y {
            let mut a_row_count = 0u32;
            let mut b_row_count = 0u32;

            for gx in min_x..max_x {
                let ax = gx;
                let ay = gy;
                let bx = gx - dx;
                let by = gy - dy;

                let in_a =
                    ax >= 0 && ay >= 0 && (ax as usize) < a.width && (ay as usize) < a.height;
                let in_b =
                    bx >= 0 && by >= 0 && (bx as usize) < b.width && (by as usize) < b.height;
                let a_on = in_a && a.get_usize(ax as usize, ay as usize);
                let b_on = in_b && b.get_usize(bx as usize, by as usize);

                if !(a_on || b_on) {
                    continue;
                }

                let rx = (gx - min_x) as usize;
                if a_on {
                    a_row_count += 1;
                    self.col_a[rx] += 1;
                }
                if b_on {
                    b_row_count += 1;
                    self.col_b[rx] += 1;
                }

                if a_on != b_on {
                    if in_a && in_b {
                        overlap_err += 1;
                    } else {
                        outside_ink_err += 1;
                    }
                }
            }

            row_profile_err += a_row_count.abs_diff(b_row_count);
        }

        let mut col_profile_err = 0u32;
        for i in 0..used_cols {
            col_profile_err += self.col_a[i].abs_diff(self.col_b[i]);
        }

        CompareResult {
            total_err: overlap_err.saturating_add(outside_ink_err),
            overlap_err,
            outside_ink_err,
            row_profile_err,
            col_profile_err,
            black_delta: a.count_ones().abs_diff(b.count_ones()) as u32,
            dx,
            dy,
        }
    }
}

#[inline(always)]
unsafe fn row_valid_word(row: *const u32, word_index: usize, width: usize) -> u32 {
    let mut word = unsafe { *row.add(word_index) };
    let remainder = width & 31;
    if remainder != 0 && word_index == (width >> 5) {
        word &= u32::MAX << (32 - remainder);
    }
    word
}

#[inline(always)]
fn row_range_mask(start_bit: usize, end_bit: usize) -> u32 {
    if start_bit >= end_bit {
        return 0;
    }
    let start_mask = u32::MAX >> start_bit;
    let end_mask = if end_bit == 32 {
        u32::MAX
    } else {
        u32::MAX << (32 - end_bit)
    };
    start_mask & end_mask
}

#[inline(always)]
unsafe fn count_row_range_ones(
    row: *const u32,
    words_per_row: usize,
    width: usize,
    start: usize,
    end: usize,
) -> u32 {
    let start = start.min(width);
    let end = end.min(width);
    if start >= end {
        return 0;
    }

    let start_word = start >> 5;
    let end_word = (end - 1) >> 5;
    let start_bit = start & 31;
    let end_bit = ((end - 1) & 31) + 1;

    let mut total = 0u32;
    for word_index in start_word..=end_word {
        let mut word = if word_index < words_per_row {
            unsafe { row_valid_word(row, word_index, width) }
        } else {
            0
        };
        let range_start = if word_index == start_word {
            start_bit
        } else {
            0
        };
        let range_end = if word_index == end_word { end_bit } else { 32 };
        word &= row_range_mask(range_start, range_end);
        total += word.count_ones();
    }
    total
}

#[inline(always)]
unsafe fn count_row_overlap_xor(
    a_row: *const u32,
    awpr: usize,
    b_row: *const u32,
    bwpr: usize,
    overlap_width: usize,
    dx: i32,
) -> u32 {
    let cols_words = (overlap_width + 31) >> 5;
    let bit_shift = (dx & 31) as u32;
    let word_shift = (dx >> 5) as isize;
    let last_mask = if overlap_width & 31 == 0 {
        u32::MAX
    } else {
        u32::MAX << (32 - (overlap_width & 31))
    };

    let mut total = 0u32;
    for w in 0..cols_words {
        let a_idx = w as isize + word_shift;
        let mut aw = if a_idx < 0 || (a_idx as usize) >= awpr {
            0
        } else {
            unsafe { *a_row.add(a_idx as usize) }
        };
        let mut bw = if w >= bwpr {
            0
        } else {
            unsafe { *b_row.add(w) }
        };
        let aligned_a = if bit_shift == 0 {
            aw
        } else {
            let aw_next = if a_idx + 1 < 0 || (a_idx as usize + 1) >= awpr {
                0
            } else {
                unsafe { *a_row.add(a_idx as usize + 1) }
            };
            (aw << bit_shift) | (aw_next >> (32 - bit_shift))
        };

        if w + 1 == cols_words {
            bw &= last_mask;
            total += ((aligned_a & last_mask) ^ bw).count_ones();
        } else {
            total += (aligned_a ^ bw).count_ones();
        }
    }
    total
}

#[inline(always)]
unsafe fn accumulate_row_columns(
    row: *const u32,
    words_per_row: usize,
    width: usize,
    columns: &mut [u32],
    x_offset: usize,
) -> u32 {
    let mut total = 0u32;
    for word_index in 0..words_per_row {
        let mut word = unsafe { row_valid_word(row, word_index, width) };
        total += word.count_ones();
        let base_x = x_offset + word_index * 32;
        while word != 0 {
            let bit_index = word.leading_zeros() as usize;
            columns[base_x + bit_index] += 1;
            word &= !(1u32 << (31 - bit_index));
        }
    }
    total
}

#[inline(always)]
fn row_kernel_noshift(
    a_row: &[u32],
    b_row: &[u32],
    cols_words: usize,
    word_shift: isize,
    mut err: u32,
    best_err: u32,
    max_err: u32,
) -> (u32, bool) {
    if cols_words >= 8 && word_shift >= 0 {
        let result =
            xor_popcnt_u32_rows(a_row, b_row, word_shift, cols_words, err, best_err, max_err);
        return (result.err, result.broke);
    }

    for w in 0..cols_words {
        let a_idx = w as isize + word_shift;
        let aw = load_word_or_zero(a_row, a_idx);
        let bw = b_row[w];
        err = err.saturating_add((aw ^ bw).count_ones());
        if err >= best_err || err > max_err {
            return (err, true);
        }
    }
    (err, false)
}

#[inline(always)]
fn row_kernel_shift(
    a_row: &[u32],
    b_row: &[u32],
    cols_words: usize,
    word_shift: isize,
    bit_shift: u32,
    mut err: u32,
    best_err: u32,
    max_err: u32,
) -> (u32, bool) {
    if cols_words >= 8 && word_shift >= 0 {
        let result = shift_xor_popcnt_u32_rows(
            a_row, b_row, word_shift, bit_shift, cols_words, err, best_err, max_err,
        );
        return (result.err, result.broke);
    }

    let rshift = 32 - bit_shift;
    for w in 0..cols_words {
        let a_idx = w as isize + word_shift;
        let aw = load_word_or_zero(a_row, a_idx);
        let aw_next = load_word_or_zero(a_row, a_idx + 1);
        let aligned_a = (aw << bit_shift) | (aw_next >> rshift);
        let bw = b_row[w];
        err = err.saturating_add((aligned_a ^ bw).count_ones());
        if err >= best_err || err > max_err {
            return (err, true);
        }
    }
    (err, false)
}

fn load_word_or_zero(row: &[u32], idx: isize) -> u32 {
    if idx < 0 {
        0
    } else {
        row.get(idx as usize).copied().unwrap_or(0)
    }
}
