//! Grading one page against PDFium.
//!
//! The two engines will never be pixel-identical: anti-aliasing, hinting and
//! glyph rasterization all differ legitimately. So the metrics deliberately
//! separate *noise* from *signal*:
//!
//! - `mean_abs`   — overall drift. Small values are AA differences.
//! - `gross_frac` — pixels differing by more than [`GROSS`]. Edge pixels of
//!   glyphs move a little; a wrong colour, a missing glyph or a stray box
//!   moves a lot.
//! - `ink_delta`  — difference in how much of the page is marked at all.
//!   This is the one that screams when a page renders blank (Separation),
//!   fills with placement boxes (bare CFF), or loses its images.
//! - `continuous_ink_delta` — difference in total darkness, independent of a
//!   threshold. This distinguishes missing ink from equally-dark ink spread
//!   across more mid-gray pixels by a renderer's smoothing policy.
//!
//! Ranking by `ink_delta` then `gross_frac` puts real defects first and
//! leaves rasterization differences at the bottom, which is what makes the
//! report readable across a whole corpus.

/// A per-channel difference above this is not explainable by anti-aliasing.
pub const GROSS: u8 = 48;

/// A pixel darker than this counts as "marked" for the ink ratio.
const INK: u8 = 200;

#[derive(Debug, Clone, Copy, Default)]
pub struct Diff {
    pub mean_abs: f64,
    pub gross_frac: f64,
    pub ours_ink: f64,
    pub ref_ink: f64,
    pub ours_continuous_ink: f64,
    pub ref_continuous_ink: f64,
}

impl Diff {
    /// How much this page's ink coverage disagrees — the blank-page /
    /// box-page detector.
    pub fn ink_delta(&self) -> f64 {
        (self.ours_ink - self.ref_ink).abs()
    }

    /// Difference in total page darkness. Unlike [`Self::ink_delta`], this is
    /// insensitive to ink moving across the hard [`INK`] threshold.
    pub fn continuous_ink_delta(&self) -> f64 {
        (self.ours_continuous_ink - self.ref_continuous_ink).abs()
    }

    /// A single severity for ranking. Ink disagreement dominates because it
    /// catches the failures that make a page unreadable; gross pixels break
    /// ties.
    pub fn severity(&self) -> f64 {
        self.ink_delta() * 10.0 + self.gross_frac
    }

    /// Whether this page looks broken rather than merely differently
    /// rasterized.
    pub fn is_suspect(&self) -> bool {
        (self.ink_delta() > 0.01 && self.continuous_ink_delta() > 0.003) || self.gross_frac > 0.05
    }
}

/// Compare our RGBA (top-down) against PDFium's BGRA (top-down).
pub fn compare(ours_rgba: &[u8], theirs_bgra: &[u8], w: u32, h: u32) -> Diff {
    let n = (w as usize) * (h as usize);
    if n == 0 || ours_rgba.len() < n * 4 || theirs_bgra.len() < n * 4 {
        return Diff::default();
    }
    let mut sum = 0u64;
    let mut gross = 0usize;
    let mut ours_ink = 0usize;
    let mut ref_ink = 0usize;
    let mut ours_darkness = 0u64;
    let mut ref_darkness = 0u64;
    for i in 0..n {
        let o = &ours_rgba[i * 4..i * 4 + 3]; // R,G,B
        let t = &theirs_bgra[i * 4..i * 4 + 3]; // B,G,R
        let d = [
            o[0].abs_diff(t[2]) as u32,
            o[1].abs_diff(t[1]) as u32,
            o[2].abs_diff(t[0]) as u32,
        ];
        let worst = d[0].max(d[1]).max(d[2]);
        sum += (d[0] + d[1] + d[2]) as u64;
        if worst as u8 > GROSS {
            gross += 1;
        }
        // Luma-ish: cheap and enough to spot "is anything painted here".
        let ol = (o[0] as u32 + o[1] as u32 + o[2] as u32) / 3;
        let tl = (t[0] as u32 + t[1] as u32 + t[2] as u32) / 3;
        if ol < INK as u32 {
            ours_ink += 1;
        }
        if tl < INK as u32 {
            ref_ink += 1;
        }
        ours_darkness += u64::from(255 - ol);
        ref_darkness += u64::from(255 - tl);
    }
    Diff {
        mean_abs: sum as f64 / (n * 3) as f64,
        gross_frac: gross as f64 / n as f64,
        ours_ink: ours_ink as f64 / n as f64,
        ref_ink: ref_ink as f64 / n as f64,
        ours_continuous_ink: ours_darkness as f64 / (n * 255) as f64,
        ref_continuous_ink: ref_darkness as f64 / (n * 255) as f64,
    }
}

/// Write a side-by-side + difference PPM for eyeballing a suspect page.
pub fn write_triptych(
    path: &std::path::Path,
    ours_rgba: &[u8],
    theirs_bgra: &[u8],
    w: u32,
    h: u32,
) -> std::io::Result<()> {
    let (wu, hu) = (w as usize, h as usize);
    let out_w = wu * 3;
    let mut rgb = vec![255u8; out_w * hu * 3];
    for y in 0..hu {
        for x in 0..wu {
            let i = y * wu + x;
            let o = &ours_rgba[i * 4..i * 4 + 3];
            let t = &theirs_bgra[i * 4..i * 4 + 3];
            let put = |rgb: &mut [u8], col: usize, v: [u8; 3]| {
                let p = (y * out_w + col) * 3;
                rgb[p..p + 3].copy_from_slice(&v);
            };
            put(&mut rgb, x, [o[0], o[1], o[2]]);
            put(&mut rgb, wu + x, [t[2], t[1], t[0]]);
            // Difference, inverted so identical = white.
            let d = o[0]
                .abs_diff(t[2])
                .max(o[1].abs_diff(t[1]))
                .max(o[2].abs_diff(t[0]));
            put(&mut rgb, wu * 2 + x, [255 - d, 255 - d, 255 - d]);
        }
    }
    let mut buf = format!("P6\n{out_w} {hu}\n255\n").into_bytes();
    buf.extend_from_slice(&rgb);
    std::fs::write(path, buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_gray(values: &[u8]) -> Vec<u8> {
        values.iter().flat_map(|&v| [v, v, v, 255]).collect()
    }

    fn bgra_gray(values: &[u8]) -> Vec<u8> {
        rgba_gray(values)
    }

    #[test]
    fn equal_energy_smoothing_is_not_a_suspect() {
        let mut crisp = vec![255; 1_000];
        crisp[..12].fill(0);
        let mut soft = vec![255; 1_000];
        // Twenty-four half-dark pixels carry almost exactly the same darkness
        // as twelve black pixels, but twice as many cross the legacy threshold.
        soft[..24].fill(128);

        let diff = compare(&rgba_gray(&crisp), &bgra_gray(&soft), 1_000, 1);
        assert!(diff.ink_delta() > 0.01);
        assert!(diff.continuous_ink_delta() < 0.003);
        assert!(diff.gross_frac < 0.05);
        assert!(!diff.is_suspect());
    }

    #[test]
    fn genuinely_missing_ink_remains_a_suspect() {
        let mut marked = vec![255; 1_000];
        marked[..12].fill(0);
        let blank = vec![255; 1_000];

        let diff = compare(&rgba_gray(&marked), &bgra_gray(&blank), 1_000, 1);
        assert!(diff.ink_delta() > 0.01);
        assert!(diff.continuous_ink_delta() > 0.003);
        assert!(diff.is_suspect());
    }
}
