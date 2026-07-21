use wide::u32x8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowPopcntResult {
    pub err: u32,
    pub broke: bool,
}

#[inline(always)]
fn load_word_or_zero(row: &[u32], idx: isize) -> u32 {
    if idx < 0 {
        0
    } else {
        row.get(idx as usize).copied().unwrap_or(0)
    }
}

#[inline(always)]
fn count_lanes_ones(v: u32x8) -> u32 {
    v.to_array().into_iter().map(u32::count_ones).sum::<u32>()
}

#[inline(always)]
fn should_break(err: u32, best_err: u32, max_err: u32) -> bool {
    err >= best_err || err > max_err
}

#[inline(always)]
fn load_u32x8(row: &[u32], start: usize) -> u32x8 {
    u32x8::from([
        row[start],
        row[start + 1],
        row[start + 2],
        row[start + 3],
        row[start + 4],
        row[start + 5],
        row[start + 6],
        row[start + 7],
    ])
}

pub(crate) fn xor_popcnt_u32_rows(
    a_row: &[u32],
    b_row: &[u32],
    word_shift: isize,
    cols_words: usize,
    mut err: u32,
    best_err: u32,
    max_err: u32,
) -> RowPopcntResult {
    let mut w = 0usize;

    if word_shift >= 0 {
        let shift = word_shift as usize;
        while w + 8 <= cols_words && shift + w + 8 <= a_row.len() && w + 8 <= b_row.len() {
            let av = load_u32x8(a_row, shift + w);
            let bv = load_u32x8(b_row, w);
            let chunk_err = count_lanes_ones(av ^ bv);
            if should_break(err.saturating_add(chunk_err), best_err, max_err) {
                for lane in 0..8 {
                    let aw = a_row[shift + w + lane];
                    let bw = b_row[w + lane];
                    err = err.saturating_add((aw ^ bw).count_ones());
                    if should_break(err, best_err, max_err) {
                        return RowPopcntResult { err, broke: true };
                    }
                }
            } else {
                err += chunk_err;
            }
            w += 8;
        }
    }

    while w < cols_words {
        let aw = load_word_or_zero(a_row, w as isize + word_shift);
        let bw = b_row[w];
        err = err.saturating_add((aw ^ bw).count_ones());
        if should_break(err, best_err, max_err) {
            return RowPopcntResult { err, broke: true };
        }
        w += 1;
    }

    RowPopcntResult { err, broke: false }
}

pub(crate) fn shift_xor_popcnt_u32_rows(
    a_row: &[u32],
    b_row: &[u32],
    word_shift: isize,
    bit_shift: u32,
    cols_words: usize,
    mut err: u32,
    best_err: u32,
    max_err: u32,
) -> RowPopcntResult {
    debug_assert!((1..32).contains(&bit_shift));
    let rshift = 32 - bit_shift;
    let mut w = 0usize;

    if word_shift >= 0 {
        let shift = word_shift as usize;
        while w + 8 <= cols_words && shift + w + 9 <= a_row.len() && w + 8 <= b_row.len() {
            let av = load_u32x8(a_row, shift + w);
            let next = load_u32x8(a_row, shift + w + 1);
            let bv = load_u32x8(b_row, w);
            let aligned = (av << bit_shift) | (next >> rshift);
            let chunk_err = count_lanes_ones(aligned ^ bv);
            if should_break(err.saturating_add(chunk_err), best_err, max_err) {
                for lane in 0..8 {
                    let a_idx = (w + lane) as isize + word_shift;
                    let aw = load_word_or_zero(a_row, a_idx);
                    let aw_next = load_word_or_zero(a_row, a_idx + 1);
                    let aligned = (aw << bit_shift) | (aw_next >> rshift);
                    let bw = b_row[w + lane];
                    err = err.saturating_add((aligned ^ bw).count_ones());
                    if should_break(err, best_err, max_err) {
                        return RowPopcntResult { err, broke: true };
                    }
                }
            } else {
                err += chunk_err;
            }
            w += 8;
        }
    }

    while w < cols_words {
        let a_idx = w as isize + word_shift;
        let aw = load_word_or_zero(a_row, a_idx);
        let aw_next = load_word_or_zero(a_row, a_idx + 1);
        let aligned = (aw << bit_shift) | (aw_next >> rshift);
        let bw = b_row[w];
        err = err.saturating_add((aligned ^ bw).count_ones());
        if should_break(err, best_err, max_err) {
            return RowPopcntResult { err, broke: true };
        }
        w += 1;
    }

    RowPopcntResult { err, broke: false }
}

pub(crate) fn count_packed_words_ones(words: &[u32], width: usize, height: usize) -> usize {
    let words_per_row = width.div_ceil(32);
    if words_per_row == 0 || height == 0 {
        return 0;
    }

    let tail_bits = width & 31;
    let tail_mask = if tail_bits == 0 {
        u32::MAX
    } else {
        u32::MAX << (32 - tail_bits)
    };

    let mut total = 0usize;
    for y in 0..height {
        let row_start = y * words_per_row;
        let row = &words[row_start..row_start + words_per_row];
        for (idx, &word) in row.iter().enumerate() {
            let masked = if idx + 1 == words_per_row {
                word & tail_mask
            } else {
                word
            };
            total += masked.count_ones() as usize;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar_noshift(
        a_row: &[u32],
        b_row: &[u32],
        word_shift: isize,
        cols_words: usize,
        mut err: u32,
        best_err: u32,
        max_err: u32,
    ) -> RowPopcntResult {
        for w in 0..cols_words {
            let aw = load_word_or_zero(a_row, w as isize + word_shift);
            err += (aw ^ b_row[w]).count_ones();
            if should_break(err, best_err, max_err) {
                return RowPopcntResult { err, broke: true };
            }
        }
        RowPopcntResult { err, broke: false }
    }

    fn scalar_shift(
        a_row: &[u32],
        b_row: &[u32],
        word_shift: isize,
        bit_shift: u32,
        cols_words: usize,
        mut err: u32,
        best_err: u32,
        max_err: u32,
    ) -> RowPopcntResult {
        let rshift = 32 - bit_shift;
        for w in 0..cols_words {
            let a_idx = w as isize + word_shift;
            let aw = load_word_or_zero(a_row, a_idx);
            let aw_next = load_word_or_zero(a_row, a_idx + 1);
            let aligned = (aw << bit_shift) | (aw_next >> rshift);
            err += (aligned ^ b_row[w]).count_ones();
            if should_break(err, best_err, max_err) {
                return RowPopcntResult { err, broke: true };
            }
        }
        RowPopcntResult { err, broke: false }
    }

    #[test]
    fn xor_popcnt_matches_scalar_noshift() {
        let a: Vec<u32> = (0..24)
            .map(|i| 0x1357_9bdfu32.rotate_left(i as u32))
            .collect();
        let b: Vec<u32> = (0..24)
            .map(|i| 0xfedc_ba98u32.rotate_right(i as u32))
            .collect();

        let simd = xor_popcnt_u32_rows(&a, &b, 0, 24, 0, u32::MAX, u32::MAX);
        let scalar = scalar_noshift(&a, &b, 0, 24, 0, u32::MAX, u32::MAX);
        assert_eq!(simd, scalar);
    }

    #[test]
    fn xor_popcnt_matches_scalar_with_negative_offset() {
        let a = [0xffff_0000, 0x0123_4567, 0x89ab_cdef];
        let b = [0xffff_ffff, 0, 0x0f0f_0f0f, 0xf0f0_f0f0];

        let simd = xor_popcnt_u32_rows(&a, &b, -1, 4, 3, u32::MAX, u32::MAX);
        let scalar = scalar_noshift(&a, &b, -1, 4, 3, u32::MAX, u32::MAX);
        assert_eq!(simd, scalar);
    }

    #[test]
    fn shift_xor_popcnt_matches_scalar() {
        let a: Vec<u32> = (0..25)
            .map(|i| 0x8000_0001u32.rotate_left((i * 3) as u32))
            .collect();
        let b: Vec<u32> = (0..24)
            .map(|i| 0x00ff_00ffu32.rotate_right((i * 5) as u32))
            .collect();

        let simd = shift_xor_popcnt_u32_rows(&a, &b, 0, 7, 24, 0, u32::MAX, u32::MAX);
        let scalar = scalar_shift(&a, &b, 0, 7, 24, 0, u32::MAX, u32::MAX);
        assert_eq!(simd, scalar);
    }

    #[test]
    fn shift_xor_popcnt_matches_scalar_tail_width() {
        let a = [0xaaaa_aaaa, 0x5555_5555, 0xffff_0000, 0x0000_ffff];
        let b = [0x1111_1111, 0x2222_2222, 0x3333_3333];

        let simd = shift_xor_popcnt_u32_rows(&a, &b, 0, 13, 3, 0, u32::MAX, u32::MAX);
        let scalar = scalar_shift(&a, &b, 0, 13, 3, 0, u32::MAX, u32::MAX);
        assert_eq!(simd, scalar);
    }

    #[test]
    fn xor_popcnt_preserves_early_exit() {
        let a = [u32::MAX; 16];
        let b = [0; 16];

        let simd = xor_popcnt_u32_rows(&a, &b, 0, 16, 0, 64, 63);
        let scalar = scalar_noshift(&a, &b, 0, 16, 0, 64, 63);
        assert!(simd.broke);
        assert_eq!(simd, scalar);
    }
}
