//! Portable SIMD kernels using the `wide` crate. Every kernel here must be
//! bit-exact with `scalar` — see the tests below.
//!
//! `rgb_to_ycbcr` cannot vectorize the three 256-entry table lookups
//! themselves (`wide` has no gather instruction), so those stay scalar per
//! pixel; what's batched across 8 lanes is the add/shift/clamp/cast combine
//! step that follows.
//!
//! Measured result (`examples/benchmark.rs`, 1600x1200 synthetic image,
//! release build): this kernel is **~2x slower** than `scalar`, not faster
//! — the per-pixel scalar gather into 9 staging arrays costs more than the
//! batched combine saves, and the scalar version was already cheap per
//! pixel. `select_primitives()` in `mod.rs` does not enable this under
//! `auto` because of that measurement; it's reachable only via explicit
//! `DJVU_PRIMITIVES=wide`. Kept rather than deleted because it's
//! correctness-tested and may become a real win if a future revision
//! removes the per-pixel gather.
//!
//! The obvious "remove the gather" fix — replace the per-k LUT with a
//! single fixed-point coefficient (`k * round(65536*coef)`) — was checked
//! and rejected: it disagrees with the existing per-k-rounded LUT on ~250
//! of 256 input values for every coefficient (float-rounds-then-truncates
//! per k vs. integer-multiplies-a-pre-rounded-constant are different
//! roundings). Adopting it would mean changing the scalar reference's
//! output too, which needs validation against djvulibre's actual decoder
//! output, not just internal scalar/SIMD self-consistency — out of scope
//! for a drive-by SIMD change. Left as a real, scoped follow-up.

use super::super::scalar::ycc_tables;
use super::super::Primitives;
use wide::i32x8;

/// Installs this kernel unconditionally. Only called from `setup_all`
/// (explicit `DJVU_PRIMITIVES=wide`) — never from `setup_auto`, since this
/// kernel measures slower than scalar. See module doc above.
pub(super) fn setup(primitives: &mut Primitives) {
    primitives.color.rgb_to_ycbcr = rgb_to_ycbcr;
}

const LANES: usize = 8;

fn rgb_to_ycbcr(img_raw: &[u8], out_y: &mut [i8], out_cb: &mut [i8], out_cr: &mut [i8]) {
    let (y_tbl, cb_tbl, cr_tbl) = ycc_tables();
    let npix = img_raw.len() / 3;
    let full_chunks = npix / LANES;

    let bias = i32x8::new([32768; LANES]);
    let y_offset = i32x8::new([128; LANES]);
    let clamp_lo = i32x8::new([-128; LANES]);
    let clamp_hi = i32x8::new([127; LANES]);

    for c in 0..full_chunks {
        let base = c * LANES;

        let mut y0 = [0i32; LANES];
        let mut y1 = [0i32; LANES];
        let mut y2 = [0i32; LANES];
        let mut cb0 = [0i32; LANES];
        let mut cb1 = [0i32; LANES];
        let mut cb2 = [0i32; LANES];
        let mut cr0 = [0i32; LANES];
        let mut cr1 = [0i32; LANES];
        let mut cr2 = [0i32; LANES];

        for lane in 0..LANES {
            let px = base + lane;
            let r = img_raw[px * 3] as usize;
            let g = img_raw[px * 3 + 1] as usize;
            let b = img_raw[px * 3 + 2] as usize;

            y0[lane] = y_tbl[0][r];
            y1[lane] = y_tbl[1][g];
            y2[lane] = y_tbl[2][b];
            cb0[lane] = cb_tbl[0][r];
            cb1[lane] = cb_tbl[1][g];
            cb2[lane] = cb_tbl[2][b];
            cr0[lane] = cr_tbl[0][r];
            cr1[lane] = cr_tbl[1][g];
            cr2[lane] = cr_tbl[2][b];
        }

        // Y is intentionally NOT clamped — matches the scalar reference,
        // which truncates straight to i8 with no clamp on this channel.
        let y_val =
            ((i32x8::new(y0) + i32x8::new(y1) + i32x8::new(y2) + bias) >> 16i32) - y_offset;
        let cb_val = ((i32x8::new(cb0) + i32x8::new(cb1) + i32x8::new(cb2) + bias) >> 16i32)
            .max(clamp_lo)
            .min(clamp_hi);
        let cr_val = ((i32x8::new(cr0) + i32x8::new(cr1) + i32x8::new(cr2) + bias) >> 16i32)
            .max(clamp_lo)
            .min(clamp_hi);

        let y_arr = y_val.as_array_ref();
        let cb_arr = cb_val.as_array_ref();
        let cr_arr = cr_val.as_array_ref();

        for lane in 0..LANES {
            out_y[base + lane] = y_arr[lane] as i8;
            out_cb[base + lane] = cb_arr[lane] as i8;
            out_cr[base + lane] = cr_arr[lane] as i8;
        }
    }

    // Scalar tail for the remainder (npix % LANES pixels).
    for i in (full_chunks * LANES)..npix {
        let r = img_raw[i * 3] as usize;
        let g = img_raw[i * 3 + 1] as usize;
        let b = img_raw[i * 3 + 2] as usize;

        let y = y_tbl[0][r] + y_tbl[1][g] + y_tbl[2][b] + 32768;
        out_y[i] = ((y >> 16i32) - 128) as i8;

        let cb = cb_tbl[0][r] + cb_tbl[1][g] + cb_tbl[2][b] + 32768;
        out_cb[i] = (cb >> 16i32).clamp(-128, 127) as i8;

        let cr = cr_tbl[0][r] + cr_tbl[1][g] + cr_tbl[2][b] + 32768;
        out_cr[i] = (cr >> 16i32).clamp(-128, 127) as i8;
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::scalar;
    use super::*;

    // Deterministic xorshift so the test has no extra dev-dependency.
    fn xorshift(state: &mut u32) -> u32 {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        *state
    }

    fn random_rgb(npix: usize, seed: u32) -> Vec<u8> {
        let mut state = seed | 1;
        (0..npix * 3)
            .map(|_| (xorshift(&mut state) & 0xFF) as u8)
            .collect()
    }

    fn check_matches_scalar(npix: usize, seed: u32) {
        let img = random_rgb(npix, seed);

        let mut scalar_y = vec![0i8; npix];
        let mut scalar_cb = vec![0i8; npix];
        let mut scalar_cr = vec![0i8; npix];
        scalar::rgb_to_ycbcr(&img, &mut scalar_y, &mut scalar_cb, &mut scalar_cr);

        let mut wide_y = vec![0i8; npix];
        let mut wide_cb = vec![0i8; npix];
        let mut wide_cr = vec![0i8; npix];
        rgb_to_ycbcr(&img, &mut wide_y, &mut wide_cb, &mut wide_cr);

        assert_eq!(wide_y, scalar_y, "Y mismatch at npix={npix} seed={seed}");
        assert_eq!(wide_cb, scalar_cb, "Cb mismatch at npix={npix} seed={seed}");
        assert_eq!(wide_cr, scalar_cr, "Cr mismatch at npix={npix} seed={seed}");
    }

    #[test]
    fn rgb_to_ycbcr_matches_scalar_across_sizes() {
        // Straddle the 8-lane boundary: below, at, just above, and well above.
        for &npix in &[0, 1, 3, 7, 8, 9, 15, 16, 17, 100, 1024, 4097] {
            check_matches_scalar(npix, 0x1234_5678 ^ npix as u32);
        }
    }

    #[test]
    fn rgb_to_ycbcr_matches_scalar_extremes() {
        // All-black, all-white, and all-saturated-primary images exercise
        // the clamp boundaries directly.
        for &(r, g, b) in &[
            (0u8, 0u8, 0u8),
            (255, 255, 255),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
        ] {
            let npix = 64;
            let img: Vec<u8> = (0..npix).flat_map(|_| [r, g, b]).collect();

            let mut scalar_y = vec![0i8; npix];
            let mut scalar_cb = vec![0i8; npix];
            let mut scalar_cr = vec![0i8; npix];
            scalar::rgb_to_ycbcr(&img, &mut scalar_y, &mut scalar_cb, &mut scalar_cr);

            let mut wide_y = vec![0i8; npix];
            let mut wide_cb = vec![0i8; npix];
            let mut wide_cr = vec![0i8; npix];
            rgb_to_ycbcr(&img, &mut wide_y, &mut wide_cb, &mut wide_cr);

            assert_eq!(wide_y, scalar_y);
            assert_eq!(wide_cb, scalar_cb);
            assert_eq!(wide_cr, scalar_cr);
        }
    }
}
