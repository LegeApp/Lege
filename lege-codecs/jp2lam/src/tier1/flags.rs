// FlagGrid redesign with incremental neighbor bit maintenance
// This version stores all flag state in packed u16 words and updates
// neighbor significance bits incrementally when samples become significant.

use super::consts::{
    T1_SIGMA_E, T1_SIGMA_N, T1_SIGMA_NE, T1_SIGMA_NW, T1_SIGMA_S, T1_SIGMA_SE, T1_SIGMA_SW,
    T1_SIGMA_W,
};

// Compact flag word type - one per coefficient
type T1Flag = u16;

// Per-sample state bits
const F_SIG: T1Flag = 1 << 0; // Sample is significant
const F_VISITED: T1Flag = 1 << 1; // Visited in current bitplane SP pass
const F_REFINED: T1Flag = 1 << 2; // Has refinement history
const F_SIGN: T1Flag = 1 << 3; // Sign bit (0=positive, 1=negative)

// Neighbor significance bits (maintained incrementally)
const N_W: T1Flag = 1 << 4; // West neighbor is significant
const N_E: T1Flag = 1 << 5; // East neighbor is significant
const N_N: T1Flag = 1 << 6; // North neighbor is significant
const N_S: T1Flag = 1 << 7; // South neighbor is significant
const N_NW: T1Flag = 1 << 8; // Northwest neighbor is significant
const N_NE: T1Flag = 1 << 9; // Northeast neighbor is significant
const N_SW: T1Flag = 1 << 10; // Southwest neighbor is significant
const N_SE: T1Flag = 1 << 11; // Southeast neighbor is significant

// Composite masks
const N_ANY: T1Flag = N_W | N_E | N_N | N_S | N_NW | N_NE | N_SW | N_SE;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlagGrid {
    width: usize,
    height: usize,
    /// Packed flag words: one u16 per coefficient
    flags: Vec<T1Flag>,
}

impl FlagGrid {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        let len = width.saturating_mul(height);
        Self {
            width,
            height,
            flags: vec![0; len],
        }
    }

    /// Reuse the backing allocation for another code-block.
    pub(crate) fn reset(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.flags.resize(width.saturating_mul(height), 0);
        self.flags.fill(0);
    }

    #[inline(always)]
    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    #[inline(always)]
    pub(crate) fn is_significant(&self, x: usize, y: usize) -> bool {
        (self.flags[self.idx(x, y)] & F_SIG) != 0
    }

    #[inline(always)]
    pub(crate) fn is_negative_idx(&self, idx: usize) -> bool {
        (self.flags[idx] & F_SIGN) != 0
    }

    #[inline(always)]
    pub(crate) fn has_refinement_history(&self, x: usize, y: usize) -> bool {
        let idx = self.idx(x, y);
        (self.flags[idx] & F_REFINED) != 0
    }

    #[inline(always)]
    pub(crate) fn is_visited(&self, x: usize, y: usize) -> bool {
        let idx = self.idx(x, y);
        (self.flags[idx] & F_VISITED) != 0
    }

    #[inline(always)]
    pub(crate) fn mark_refined(&mut self, x: usize, y: usize) {
        let idx = self.idx(x, y);
        self.flags[idx] |= F_REFINED;
    }

    #[inline(always)]
    pub(crate) fn mark_visited(&mut self, x: usize, y: usize) {
        let idx = self.idx(x, y);
        self.flags[idx] |= F_VISITED;
    }

    #[inline(always)]
    pub(crate) fn clear_visited(&mut self, x: usize, y: usize) {
        let idx = self.idx(x, y);
        self.flags[idx] &= !F_VISITED;
    }

    pub(crate) fn clear_visited_all(&mut self) {
        for flag in &mut self.flags {
            *flag &= !F_VISITED;
        }
    }

    pub(crate) fn has_visited(&self) -> bool {
        self.flags.iter().any(|flag| (flag & F_VISITED) != 0)
    }

    /// Mark a sample as significant and update neighbor bits incrementally
    pub(crate) fn mark_significant(&mut self, x: usize, y: usize, sign_bit: u8) {
        let idx = self.idx(x, y);

        // Set significant flag and sign
        self.flags[idx] |= F_SIG;
        if sign_bit != 0 {
            self.flags[idx] |= F_SIGN;
        } else {
            self.flags[idx] &= !F_SIGN;
        }

        // Update the 8 neighbor flags incrementally
        let w = self.width;
        let h = self.height;

        // West neighbor (x-1, y) sees us on its East
        if x > 0 {
            self.flags[idx - 1] |= N_E;
        }
        // East neighbor (x+1, y) sees us on its West
        if x + 1 < w {
            self.flags[idx + 1] |= N_W;
        }
        // North neighbor (x, y-1) sees us on its South
        if y > 0 {
            self.flags[idx - w] |= N_S;
        }
        // South neighbor (x, y+1) sees us on its North
        if y + 1 < h {
            self.flags[idx + w] |= N_N;
        }
        // Northwest neighbor (x-1, y-1) sees us on its Southeast
        if x > 0 && y > 0 {
            self.flags[idx - w - 1] |= N_SE;
        }
        // Northeast neighbor (x+1, y-1) sees us on its Southwest
        if x + 1 < w && y > 0 {
            self.flags[idx - w + 1] |= N_SW;
        }
        // Southwest neighbor (x-1, y+1) sees us on its Northeast
        if x > 0 && y + 1 < h {
            self.flags[idx + w - 1] |= N_NE;
        }
        // Southeast neighbor (x+1, y+1) sees us on its Northwest
        if x + 1 < w && y + 1 < h {
            self.flags[idx + w + 1] |= N_NW;
        }
    }

    /// Get neighbor mask - now just reads pre-computed bits!
    #[inline(always)]
    pub(crate) fn neighbour_mask(&self, x: usize, y: usize) -> u32 {
        self.neighbour_mask_idx(self.idx(x, y))
    }

    /// Get neighbor mask from index - ultra-fast path
    #[inline(always)]
    pub(crate) fn neighbour_mask_idx(&self, idx: usize) -> u32 {
        let f = self.flags[idx];
        self.neighbour_mask_from_word(f)
    }

    /// Convert internal neighbor bits to T1_SIGMA_* constants
    #[inline(always)]
    fn neighbour_mask_from_word(&self, f: T1Flag) -> u32 {
        let mut mask = 0u32;
        if (f & N_W) != 0 {
            mask |= T1_SIGMA_W;
        }
        if (f & N_E) != 0 {
            mask |= T1_SIGMA_E;
        }
        if (f & N_N) != 0 {
            mask |= T1_SIGMA_N;
        }
        if (f & N_S) != 0 {
            mask |= T1_SIGMA_S;
        }
        if (f & N_NW) != 0 {
            mask |= T1_SIGMA_NW;
        }
        if (f & N_NE) != 0 {
            mask |= T1_SIGMA_NE;
        }
        if (f & N_SW) != 0 {
            mask |= T1_SIGMA_SW;
        }
        if (f & N_SE) != 0 {
            mask |= T1_SIGMA_SE;
        }
        mask
    }

    /// Check if sample has any significant neighbor (fast check). Only
    /// exercised by tests exhaustively checking all 8 neighbor directions;
    /// production code queries neighbor significance via
    /// [`Self::neighbour_mask_from_word`] as part of context derivation.
    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn has_sig_neighbor_idx(&self, idx: usize) -> bool {
        (self.flags[idx] & N_ANY) != 0
    }

    /// Stripe is clean if no samples are significant, visited, or have significant neighbors
    pub(crate) fn stripe_is_clean(&self, x: usize, k: usize, lim: usize) -> bool {
        self.stripe_is_clean_with_causal(x, k, lim, false)
    }

    /// Check cleanup run-mode eligibility, suppressing neighbours in the next
    /// four-row stripe at a vertically causal boundary (Annex D.7).
    pub(crate) fn stripe_is_clean_with_causal(
        &self,
        x: usize,
        k: usize,
        lim: usize,
        vertical_causal: bool,
    ) -> bool {
        for ci in 0..lim {
            let y = k + ci;
            if y >= self.height {
                break;
            }
            let f = self.flags[self.idx(x, y)];
            let neighbours = if vertical_causal && ci + 1 == lim {
                N_ANY & !(N_S | N_SW | N_SE)
            } else {
                N_ANY
            };
            // Clean means: not significant, not visited, no significant neighbors
            if (f & (F_SIG | F_VISITED | neighbours)) != 0 {
                return false;
            }
        }
        true
    }

    pub(crate) fn cardinal_sign_context(&self, x: usize, y: usize) -> (u32, u8) {
        // Fast path: interior points
        if x > 0 && y > 0 && x < self.width - 1 && y < self.height - 1 {
            unsafe { self.cardinal_sign_context_unchecked(x, y) }
        } else {
            self.cardinal_sign_context_safe(x, y)
        }
    }

    /// Sign context with the south contribution forced insignificant at the
    /// bottom row of a vertically causal four-row stripe.
    pub(crate) fn cardinal_sign_context_without_south(&self, x: usize, y: usize) -> (u32, u8) {
        let w = self.width;
        let center_sign = (self.flags[self.idx(x, y)] & F_SIGN) as u8;
        let get_neighbor = |nx: isize, ny: isize| {
            if nx < 0 || ny < 0 || nx >= w as isize || ny >= self.height as isize {
                (false, 0u8)
            } else {
                let flag = self.flags[ny as usize * w + nx as usize];
                ((flag & F_SIG) != 0, (flag & F_SIGN) as u8)
            }
        };
        let (west_sig, west_sign) = get_neighbor(x as isize - 1, y as isize);
        let (north_sig, north_sign) = get_neighbor(x as isize, y as isize - 1);
        let (east_sig, east_sign) = get_neighbor(x as isize + 1, y as isize);
        (
            super::helpers::sign_lut_index(
                west_sig, west_sign, north_sig, north_sign, east_sig, east_sign, false, 0,
            ),
            center_sign,
        )
    }

    /// Fast unchecked version for interior points.
    /// SAFETY: Caller must ensure 0 < x < width-1 and 0 < y < height-1
    #[inline(always)]
    unsafe fn cardinal_sign_context_unchecked(&self, x: usize, y: usize) -> (u32, u8) {
        let w = self.width;
        let base = y * w + x;
        let flags_ptr = self.flags.as_ptr();

        unsafe {
            let west_flag = *flags_ptr.add(base - 1);
            let north_flag = *flags_ptr.add(base - w);
            let east_flag = *flags_ptr.add(base + 1);
            let south_flag = *flags_ptr.add(base + w);
            let center_flag = *flags_ptr.add(base);

            let west_sig = (west_flag & F_SIG) != 0;
            let west_sign = (west_flag & F_SIGN) as u8;
            let north_sig = (north_flag & F_SIG) != 0;
            let north_sign = (north_flag & F_SIGN) as u8;
            let east_sig = (east_flag & F_SIG) != 0;
            let east_sign = (east_flag & F_SIGN) as u8;
            let south_sig = (south_flag & F_SIG) != 0;
            let south_sign = (south_flag & F_SIGN) as u8;

            (
                super::helpers::sign_lut_index(
                    west_sig, west_sign, north_sig, north_sign, east_sig, east_sign, south_sig,
                    south_sign,
                ),
                (center_flag & F_SIGN) as u8,
            )
        }
    }

    #[inline]
    fn cardinal_sign_context_safe(&self, x: usize, y: usize) -> (u32, u8) {
        let w = self.width;
        let h = self.height;

        let center_idx = self.idx(x, y);
        let center_sign = (self.flags[center_idx] & F_SIGN) as u8;

        // Helper to get sig+sign for neighbor, or (false, 0) if out of bounds
        let get_neighbor = |nx: isize, ny: isize| {
            if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                (false, 0u8)
            } else {
                let idx = (ny as usize) * w + (nx as usize);
                let flag = self.flags[idx];
                ((flag & F_SIG) != 0, (flag & F_SIGN) as u8)
            }
        };

        let (west_sig, west_sign) = get_neighbor(x as isize - 1, y as isize);
        let (north_sig, north_sign) = get_neighbor(x as isize, y as isize - 1);
        let (east_sig, east_sign) = get_neighbor(x as isize + 1, y as isize);
        let (south_sig, south_sign) = get_neighbor(x as isize, y as isize + 1);

        (
            super::helpers::sign_lut_index(
                west_sig, west_sign, north_sig, north_sign, east_sig, east_sign, south_sig,
                south_sign,
            ),
            center_sign,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neighbour_mask_tracks_significant_neighbours() {
        let mut grid = FlagGrid::new(3, 3);
        grid.mark_significant(1, 0, 0);
        grid.mark_significant(0, 1, 1);
        grid.mark_significant(2, 1, 0);
        grid.mark_significant(1, 2, 1);

        let mask = grid.neighbour_mask(1, 1);
        assert_eq!(mask & T1_SIGMA_N, T1_SIGMA_N, "North neighbor");
        assert_eq!(mask & T1_SIGMA_W, T1_SIGMA_W, "West neighbor");
        assert_eq!(mask & T1_SIGMA_E, T1_SIGMA_E, "East neighbor");
        assert_eq!(mask & T1_SIGMA_S, T1_SIGMA_S, "South neighbor");
    }

    #[test]
    fn stripe_is_clean_requires_no_context() {
        let mut grid = FlagGrid::new(3, 8);
        // Before any marking: all stripes should be clean
        assert!(grid.stripe_is_clean(1, 0, 4));
        assert!(grid.stripe_is_clean(1, 4, 4));

        // Marking a neighbor in an adjacent column should dirty the stripe
        grid.mark_significant(0, 2, 0);
        assert!(
            !grid.stripe_is_clean(1, 0, 4),
            "neighbour makes stripe dirty"
        );

        // Visited flag also makes stripe not clean
        let mut grid2 = FlagGrid::new(3, 8);
        grid2.mark_visited(1, 1);
        assert!(!grid2.stripe_is_clean(1, 0, 4));
    }

    #[test]
    fn d7_vertical_causal_ignores_the_next_stripe() {
        let mut grid = FlagGrid::new(1, 8);
        grid.mark_significant(0, 4, 1);

        assert!(!grid.stripe_is_clean(0, 0, 4));
        assert!(grid.stripe_is_clean_with_causal(0, 0, 4, true));
        assert_ne!(
            grid.cardinal_sign_context(0, 3).0,
            grid.cardinal_sign_context_without_south(0, 3).0
        );
    }

    #[test]
    fn incremental_neighbor_bits_work() {
        let mut grid = FlagGrid::new(5, 5);

        // Mark center significant
        grid.mark_significant(2, 2, 0);

        // All 8 neighbors should now have neighbor bits set
        assert!(grid.has_sig_neighbor_idx(grid.idx(1, 1)), "NW neighbor");
        assert!(grid.has_sig_neighbor_idx(grid.idx(2, 1)), "N neighbor");
        assert!(grid.has_sig_neighbor_idx(grid.idx(3, 1)), "NE neighbor");
        assert!(grid.has_sig_neighbor_idx(grid.idx(1, 2)), "W neighbor");
        assert!(grid.has_sig_neighbor_idx(grid.idx(3, 2)), "E neighbor");
        assert!(grid.has_sig_neighbor_idx(grid.idx(1, 3)), "SW neighbor");
        assert!(grid.has_sig_neighbor_idx(grid.idx(2, 3)), "S neighbor");
        assert!(grid.has_sig_neighbor_idx(grid.idx(3, 3)), "SE neighbor");

        // Non-neighbors should not have bits set
        assert!(!grid.has_sig_neighbor_idx(grid.idx(0, 0)), "Far corner");
        assert!(!grid.has_sig_neighbor_idx(grid.idx(4, 4)), "Far corner");
    }
}
