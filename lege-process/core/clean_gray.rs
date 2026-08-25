//! Grayscale scanned-page cleanup transform + MRC split helpers.
//!
//! Ported from the `graytest` tester (grayscale-tester-area) after rounds 1-7
//! of evaluation. Pipeline:
//!   gray → background estimate (downscale → local max → blur → upscale) →
//!   divide by background → level remap (auto black point) → gamma →
//!   paper-white floor → optional halo clean.
//!
//! The transform is paper-color independent (the divide-by-background step
//! normalizes any paper tone to white). Auto strength derives the black point
//! per page so faint and dark ink are handled uniformly.
//!
//! For MRC/layered output, [`ink_mask`] splits the cleaned page into a bilevel
//! ink-core mask (for JBIG2/JB2) and [`mrc_background`] produces the
//! white-filled residual (for JP2/IW44). See `grayscale-tester-area/FINDINGS.md`.

use anyhow::{Result, anyhow};
use image::{GrayImage, imageops};
use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BgEstimator {
    /// Downscale → gaussian blur → upscale.
    Blur,
    /// Downscale → local max (dilation) → blur → upscale. Less likely to
    /// absorb dense ink into the paper model; the production default.
    MaxBlur,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strength {
    Gentle,
    Standard,
    Strong,
    /// Black point derived per page from the normalized ink percentile; white
    /// point fixed at 0.93. Handles faint and dark ink uniformly. Production
    /// default (see FINDINGS round 2).
    Auto,
}

impl Strength {
    /// (white point, black point) as fractions of full brightness after
    /// normalization. `Auto` returns the gentle fallback; the real value comes
    /// from [`auto_points`] once the normalized image exists.
    pub fn points(self) -> (f64, f64) {
        match self {
            Strength::Gentle | Strength::Auto => (0.93, 0.45),
            Strength::Standard => (0.88, 0.55),
            Strength::Strong => (0.82, 0.62),
        }
    }
}

/// Derive remap points from the normalized image histogram.
///
/// Ink level = 25th percentile of pixels darker than 0.7·255 in the normalized
/// image. The black point sits just below it so ink cores land near 0 whether
/// the source ink is faint or dense. Pages with almost no ink (< 0.3%) fall
/// back to the gentle points.
fn auto_points(norm_hist: &[u64; 256], total: u64) -> (f64, f64) {
    const DARK_CUTOFF: usize = 178; // 0.7 * 255
    let n_dark: u64 = norm_hist[..DARK_CUTOFF].iter().sum();
    if total == 0 || (n_dark as f64) < total as f64 * 0.003 {
        return Strength::Gentle.points();
    }
    let target = n_dark / 4;
    let mut acc = 0u64;
    let mut v_ink = DARK_CUTOFF - 1;
    for (v, &c) in norm_hist[..DARK_CUTOFF].iter().enumerate() {
        acc += c;
        if acc >= target {
            v_ink = v;
            break;
        }
    }
    let black_point = (v_ink as f64 / 255.0 - 0.08).clamp(0.05, 0.60);
    (0.93, black_point)
}

#[derive(Clone, Copy, Debug)]
pub struct CleanOptions {
    pub dpi: u32,
    pub strength: Strength,
    pub bg_estimator: BgEstimator,
    /// Downscale divisor for the background-estimation image.
    pub bg_downscale: u32,
    /// Local-max radius on the downscaled image (MaxBlur only).
    pub bg_max_radius: usize,
    pub gamma: f64,
    /// Values >= this are forced to 255 after remap. 0 disables.
    pub paper_white_floor: u8,
    /// Quantize to this many gray levels after remap. 0 disables (JP2 does not
    /// benefit from pre-quantization — kept for completeness).
    pub quantize_levels: u8,
    /// Halo clean: flatten everything not near real ink to pure white.
    pub halo: Option<HaloOptions>,
    /// Invert the source luma before cleaning (for light-on-dark scans).
    pub invert_input: bool,
}

/// Post-step that guarantees a flat white background: pixels darker than
/// `ink_threshold` form the ink mask; connected components smaller than
/// `min_ink_area` are dropped (despeckle); the surviving mask is dilated by
/// `radius` so antialiasing halos survive; everything outside goes to 255.
#[derive(Clone, Copy, Debug)]
pub struct HaloOptions {
    pub ink_threshold: u8,
    pub radius: usize,
    pub min_ink_area: usize,
}

impl Default for HaloOptions {
    fn default() -> Self {
        Self {
            ink_threshold: 190,
            radius: 3,
            min_ink_area: 9,
        }
    }
}

impl Default for CleanOptions {
    fn default() -> Self {
        Self {
            dpi: 300,
            strength: Strength::Standard,
            bg_estimator: BgEstimator::MaxBlur,
            bg_downscale: 8,
            bg_max_radius: 3,
            gamma: 1.1,
            paper_white_floor: 245,
            quantize_levels: 0,
            halo: None,
            invert_input: false,
        }
    }
}

impl CleanOptions {
    /// Production preset: auto black point + halo clean at the given render DPI.
    /// This is the winning configuration from the tester evaluation.
    pub fn production(dpi: u32, invert_input: bool) -> Self {
        Self {
            dpi,
            strength: Strength::Auto,
            halo: Some(HaloOptions::default()),
            invert_input,
            ..Self::default()
        }
    }

    /// Production preset scaled to the page's render height. The halo
    /// geometry (despeckle area, dilation radius) was tuned on ~2600px pages;
    /// applied unscaled at 1200px it eats fragments of thin light strokes —
    /// glyph pieces vanish from the mask AND the background. Scale it down
    /// with the render size so despeckle only removes true specks.
    pub fn production_for_height(page_height: usize, invert_input: bool) -> Self {
        let scale = (page_height as f64 / 2600.0).clamp(0.3, 2.0);
        let radius = ((3.0 * scale).round() as usize).max(2);
        let min_ink_area = ((9.0 * scale * scale).round() as usize).max(2);
        let dpi = ((300.0 * scale) as u32).clamp(72, 600);
        Self {
            dpi,
            strength: Strength::Auto,
            halo: Some(HaloOptions {
                ink_threshold: 190,
                radius,
                min_ink_area,
            }),
            invert_input,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct CleanStats {
    pub white_pct: f64,
    pub near_white_pct: f64,
    pub black_pct: f64,
    pub mean: f64,
}

pub struct CleanedPage {
    /// 8-bit grayscale, row-major, length `width * height`.
    pub pixels: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub stats: CleanStats,
}

/// BT.601 luma (matches the tester's PIL-compatible conversion).
pub fn rgb_to_luma(rgb: &[u8], gray: &mut [u8]) {
    debug_assert_eq!(rgb.len(), gray.len() * 3);
    gray.par_iter_mut()
        .zip(rgb.par_chunks_exact(3))
        .for_each(|(out, px)| {
            let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
            *out = ((299 * r + 587 * g + 114 * b + 500) / 1000) as u8;
        });
}

/// Clean an already-grayscale page. `gray` is single-channel, row-major.
pub fn clean_gray_page(
    gray: &[u8],
    width: usize,
    height: usize,
    opts: &CleanOptions,
) -> Result<CleanedPage> {
    let n = width
        .checked_mul(height)
        .filter(|_| width > 0 && height > 0)
        .ok_or_else(|| anyhow!("bad page dimensions {}x{}", width, height))?;
    if gray.len() != n {
        return Err(anyhow!(
            "grayscale buffer length {} != {}x{}",
            gray.len(),
            width,
            height
        ));
    }

    // Optional inversion for light-on-dark scans, applied before cleaning so
    // the background estimator sees paper as the bright field.
    let owned;
    let gray: &[u8] = if opts.invert_input {
        owned = gray.iter().map(|&v| 255 - v).collect::<Vec<u8>>();
        &owned
    } else {
        gray
    };

    let bg = estimate_background(gray, width, height, opts)?;

    let mut norm = vec![0u8; n];
    norm.par_iter_mut().enumerate().for_each(|(i, dst)| {
        let g = gray[i] as f64;
        let b = (bg[i] as f64).max(1.0);
        *dst = (g / b * 255.0).clamp(0.0, 255.0) as u8;
    });

    let (white_point, black_point) = if opts.strength == Strength::Auto {
        let mut hist = [0u64; 256];
        for &v in &norm {
            hist[v as usize] += 1;
        }
        auto_points(&hist, n as u64)
    } else {
        opts.strength.points()
    };
    let lo = black_point * 255.0;
    let hi = white_point * 255.0;
    let inv_range = 1.0 / (hi - lo);
    let gamma = if opts.gamma > 0.0 { opts.gamma } else { 1.0 };
    let floor = opts.paper_white_floor;
    let levels = opts.quantize_levels;
    let step = if levels >= 2 {
        255.0 / (levels as f64 - 1.0)
    } else {
        0.0
    };

    let mut out = vec![0u8; n];
    out.par_iter_mut().enumerate().for_each(|(i, dst)| {
        let mapped = ((norm[i] as f64 - lo) * inv_range).clamp(0.0, 1.0);
        let mut v = (mapped.powf(gamma) * 255.0).clamp(0.0, 255.0) as u8;
        if floor > 0 && v >= floor {
            v = 255;
        }
        if levels >= 2 && v != 255 {
            v = ((v as f64 / step).round() * step).clamp(0.0, 255.0) as u8;
        }
        *dst = v;
    });

    if let Some(halo) = opts.halo {
        halo_clean(&mut out, width, height, halo);
    }

    // Collect the reporting statistics in one memory pass; pages are large
    // enough that four separate parallel scans were measurable overhead.
    let (white, near_white, black, sum) = out
        .par_iter()
        .fold(
            || (0usize, 0usize, 0usize, 0u64),
            |(white, near_white, black, sum), &value| {
                (
                    white + usize::from(value == 255),
                    near_white + usize::from(value >= 245),
                    black + usize::from(value <= 10),
                    sum + value as u64,
                )
            },
        )
        .reduce(
            || (0, 0, 0, 0),
            |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2, a.3 + b.3),
        );
    let pct = |c: usize| 100.0 * c as f64 / n as f64;

    Ok(CleanedPage {
        pixels: out,
        width,
        height,
        stats: CleanStats {
            white_pct: pct(white),
            near_white_pct: pct(near_white),
            black_pct: pct(black),
            mean: sum as f64 / n as f64,
        },
    })
}

/// Adaptive mask threshold: Otsu over the cleaned page's non-white pixels,
/// clamped to [100, 200]. Books with dark heavy type land near the bottom of
/// the range (same behavior as the old fixed 100); books whose strokes stay
/// light after cleaning (thin serif faces at low render heights) get a higher
/// cut so the glyph BODIES go into the crisp full-res mask instead of
/// surviving only as gray ghosts in the subsampled background.
pub fn adaptive_ink_threshold(cleaned: &[u8]) -> u8 {
    let mut hist = [0u64; 256];
    for &v in cleaned {
        if v < 245 {
            hist[v as usize] += 1;
        }
    }
    let total: u64 = hist.iter().sum();
    if total < 64 {
        return 100;
    }

    // Otsu between-class variance maximization over the non-white histogram.
    let sum_all: u64 = hist.iter().enumerate().map(|(v, &c)| v as u64 * c).sum();
    let (mut w0, mut sum0) = (0u64, 0u64);
    let (mut best_t, mut best_var) = (100usize, 0.0f64);
    for t in 0..255 {
        w0 += hist[t];
        if w0 == 0 {
            continue;
        }
        let w1 = total - w0;
        if w1 == 0 {
            break;
        }
        sum0 += t as u64 * hist[t];
        let m0 = sum0 as f64 / w0 as f64;
        let m1 = (sum_all - sum0) as f64 / w1 as f64;
        let var = w0 as f64 * w1 as f64 * (m0 - m1) * (m0 - m1);
        if var > best_var {
            best_var = var;
            best_t = t;
        }
    }
    (best_t as u8).clamp(100, 200)
}

/// Produce the MRC split for one rendered page — shared by the PDF and DjVu
/// pipelines so both derive the grayscale background and the ink mask
/// identically. Returns `(cleaned_gray, binarized)` where `binarized` is
/// 0 = ink, 255 = paper (the pipelines' bilevel convention), suitable for the
/// JBIG2/JB2 mask, region masking, and fast OCR.
///
/// `ink_threshold` 0 selects the per-page [`adaptive_ink_threshold`].
pub fn clean_page_for_mrc(
    rgb: &[u8],
    width: usize,
    height: usize,
    opts: &CleanOptions,
    ink_threshold: u8,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let luma = checked_rgb_to_luma(rgb, width, height)?;
    // The halo cleaner must never erase pixels the mask threshold would keep:
    // halo_clean runs first and flattens everything not near sub-`halo.ink_threshold`
    // ink to pure white, so a mask threshold above it (e.g. --mrc-ink-threshold 205
    // for faint scans) would otherwise be silently neutered — faint strokes vanish
    // from BOTH layers and thin staff lines break. Raise the halo cut to match.
    let mut opts = *opts;
    if ink_threshold > 0
        && let Some(halo) = opts.halo.as_mut()
    {
        halo.ink_threshold = halo.ink_threshold.max(ink_threshold);
    }
    let opts = &opts;
    let cleaned = clean_gray_page(&luma, width, height, opts)?;
    let thr = if ink_threshold == 0 {
        adaptive_ink_threshold(&cleaned.pixels)
    } else {
        ink_threshold
    };
    let binarized: Vec<u8> = cleaned
        .pixels
        .par_iter()
        .map(|&v| if v < thr { 0 } else { 255 })
        .collect();
    Ok((cleaned.pixels, binarized))
}

/// Like [`clean_page_for_mrc`], but the ink mask is supplied by the caller
/// (e.g. the Sauvola binarizer in `mrc_adaptive_mask` mode) and only the
/// cleaned background is produced.
///
/// The threshold-keyed halo clean is replaced by a **mask-keyed collar**:
/// gray survives only within `halo.radius` of actual mask ink, everything
/// else goes to paper white. This matters because an adaptive mask is
/// thicker than the cleaned-image ink cores — with the ordinary halo the
/// mask swallows the entire antialiasing skirt and `mrc_background`'s
/// white-fill then leaves the JP2/IW44 layer empty, so the composite looks
/// binarized (blocky edges, no halo). Keying the collar to the mask keeps
/// the real rendered antialiasing in a ring around every masked stroke.
///
/// `ink_mask` uses the pipeline convention: 0 = ink, 255 = paper.
pub fn clean_page_for_mrc_with_mask(
    rgb: &[u8],
    width: usize,
    height: usize,
    opts: &CleanOptions,
    ink_mask: &[u8],
) -> Result<Vec<u8>> {
    let pixel_count = checked_pixel_count(width, height)?;
    if ink_mask.len() != pixel_count {
        return Err(anyhow!(
            "ink mask length {} != {}x{}",
            ink_mask.len(),
            width,
            height
        ));
    }
    let luma = checked_rgb_to_luma(rgb, width, height)?;
    // Run the cleaning WITHOUT the threshold-keyed halo; the collar below
    // replaces it.
    let mut opts_no_halo = *opts;
    let halo = opts_no_halo.halo.take();
    let cleaned = clean_gray_page(&luma, width, height, &opts_no_halo)?;
    let mut pixels = cleaned.pixels;
    if let Some(halo) = halo {
        let seed: Vec<u8> = ink_mask
            .iter()
            .map(|&v| if v == 0 { 255 } else { 0 })
            .collect();
        // Synthetic antialiasing rings. The real skirt cannot be relied on:
        // on faint scans the auto contrast stretch maps it to near-paper
        // (>240), so keeping "real" gray yields a visually empty background
        // and a blocky bilevel composite. Instead, synthesize a soft edge as
        // a function of distance from mask ink — ring 1 mid-gray, ring 2
        // light — and let the REAL cleaned value win wherever it is darker
        // (dark books keep their genuine halo).
        const RING1: u8 = 168;
        const RING2: u8 = 216;
        let ring1 = window_max(&seed, width, height, 1);
        let ring2 = window_max(&seed, width, height, 2);
        let keep = window_max(&seed, width, height, halo.radius.max(2));
        pixels.par_iter_mut().enumerate().for_each(|(i, px)| {
            if seed[i] != 0 {
                // Under mask ink: value is irrelevant (mrc_background
                // white-fills beneath the stencil); leave as-is.
                return;
            }
            let synth = if ring1[i] != 0 {
                RING1
            } else if ring2[i] != 0 {
                RING2
            } else {
                255
            };
            let real = if keep[i] != 0 { *px } else { 255 };
            *px = real.min(synth);
        });
    }
    Ok(pixels)
}

/// Decide whether a detected image region is actually line art (music systems,
/// diagrams, ruled tables) rather than continuous-tone content. Line-art
/// regions should NOT be overlaid as original raster crops — bilevel
/// binarization or the MRC JBIG2 mask + cleaned background renders them
/// far better — while true photos, maps, and engravings keep their overlay.
///
/// Metric: per-32×32-cell variance / Sobel / orientation features ported
/// from the bpg-rs preanalysis classifier. A luma-histogram bimodality
/// test cannot tell maps and engravings from line art.
pub fn region_is_line_art(rgb: &[u8], width: usize, height: usize, bbox: [f32; 4]) -> bool {
    crate::content_class::region_is_line_art(rgb, width, height, bbox)
}

/// Detect thin horizontal line structure that only the grayscale background
/// carries (staff lines, rules): long horizontal runs of visibly-gray pixels
/// that the ink mask did NOT capture. Text pages have only short antialiasing
/// halos around glyphs and score ~zero; sheet music scores hundreds of runs.
/// Such pages need a full-resolution background or the lines smear/break.
pub fn bg_has_thin_line_structure(
    cleaned: &[u8],
    binarized: &[u8],
    width: usize,
    height: usize,
) -> bool {
    const MIN_RUN: usize = 32;
    let long_runs: usize = cleaned
        .par_chunks(width)
        .zip(binarized.par_chunks(width))
        .map(|(crow, brow)| {
            let mut runs = 0usize;
            let mut run = 0usize;
            for (&c, &b) in crow.iter().zip(brow) {
                // Visibly gray in the background layer: not white, not masked ink.
                if c < 210 && b != 0 {
                    run += 1;
                } else {
                    if run >= MIN_RUN {
                        runs += 1;
                    }
                    run = 0;
                }
            }
            if run >= MIN_RUN {
                runs += 1;
            }
            runs
        })
        .sum();
    long_runs >= height / 20
}

/// Clean an RGB page (interleaved, 3 bytes/px).
pub fn clean_rgb_page(
    rgb: &[u8],
    width: usize,
    height: usize,
    opts: &CleanOptions,
) -> Result<CleanedPage> {
    let gray = checked_rgb_to_luma(rgb, width, height)?;
    clean_gray_page(&gray, width, height, opts)
}

fn checked_pixel_count(width: usize, height: usize) -> Result<usize> {
    width
        .checked_mul(height)
        .filter(|_| width > 0 && height > 0)
        .ok_or_else(|| anyhow!("bad page dimensions {}x{}", width, height))
}

fn checked_rgb_to_luma(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    let pixel_count = checked_pixel_count(width, height)?;
    let expected = pixel_count
        .checked_mul(3)
        .ok_or_else(|| anyhow!("RGB buffer size overflow for {}x{}", width, height))?;
    if rgb.len() != expected {
        return Err(anyhow!(
            "rgb buffer {} != {}x{}x3",
            rgb.len(),
            width,
            height
        ));
    }
    let mut gray = vec![0u8; pixel_count];
    rgb_to_luma(rgb, &mut gray);
    Ok(gray)
}

/// Split a cleaned page into a bilevel ink-core mask for MRC/layered output.
///
/// Returns one byte per pixel, `1` = ink (paint through the mask), `0` = paper.
/// Uses a lower threshold than the halo clean so only the dark stroke cores
/// become the mask — the antialiasing ramp stays in the grayscale background.
/// Default `threshold` ≈ 100 at 8-bit (see FINDINGS round 6/7).
pub fn ink_mask(cleaned: &[u8], threshold: u8) -> Vec<u8> {
    cleaned
        .par_iter()
        .map(|&v| u8::from(v < threshold))
        .collect()
}

/// Produce the MRC background: the cleaned page with ink pixels filled white,
/// box-downsampled by `subsample`. The white fill under the mask keeps the
/// composite CRISP — the mask alone renders the glyph bodies — and saves
/// bits. This is safe only because the mask is now reliable: the adaptive
/// ink threshold plus resolution-scaled despeckle mean glyph bodies land in
/// the mask instead of being eaten (the historical blotch/erosion artifacts
/// came from an unreliable mask, not from the white fill itself).
///
/// `binarized` uses the pipeline convention: 0 = ink, 255 = paper.
/// Returns `(pixels, out_width, out_height)`.
pub fn mrc_background(
    cleaned: &[u8],
    binarized: &[u8],
    width: usize,
    height: usize,
    subsample: usize,
) -> (Vec<u8>, usize, usize) {
    let sub = subsample.max(1);
    let ow = width.div_ceil(sub);
    let oh = height.div_ceil(sub);
    let mut out = vec![0u8; ow * oh];
    out.par_chunks_mut(ow).enumerate().for_each(|(by, row)| {
        for (bx, dst) in row.iter_mut().enumerate() {
            let (mut sum, mut n) = (0u32, 0u32);
            for dy in 0..sub {
                let y = by * sub + dy;
                if y >= height {
                    break;
                }
                for dx in 0..sub {
                    let x = bx * sub + dx;
                    if x >= width {
                        break;
                    }
                    let idx = y * width + x;
                    let v = if binarized[idx] == 0 {
                        255
                    } else {
                        cleaned[idx]
                    };
                    sum += v as u32;
                    n += 1;
                }
            }
            *dst = (sum / n.max(1)) as u8;
        }
    });
    (out, ow, oh)
}

/// Produce an MRC background using a continuous source-mask opacity plane.
/// Unlike [`mrc_background`], this never derives or consumes a thresholded
/// scan mask: each cleaned pixel is blended toward paper white by its
/// Lanczos-resampled source `/SMask` coverage before box downsampling.
pub fn mrc_background_with_coverage(
    cleaned: &[u8],
    coverage: &[u8],
    width: usize,
    height: usize,
    subsample: usize,
) -> (Vec<u8>, usize, usize) {
    debug_assert_eq!(cleaned.len(), width.saturating_mul(height));
    debug_assert_eq!(coverage.len(), cleaned.len());
    let sub = subsample.max(1);
    let ow = width.div_ceil(sub);
    let oh = height.div_ceil(sub);
    let mut out = vec![0u8; ow * oh];
    out.par_chunks_mut(ow).enumerate().for_each(|(by, row)| {
        for (bx, dst) in row.iter_mut().enumerate() {
            let (mut sum, mut n) = (0u32, 0u32);
            for dy in 0..sub {
                let y = by * sub + dy;
                if y >= height {
                    break;
                }
                for dx in 0..sub {
                    let x = bx * sub + dx;
                    if x >= width {
                        break;
                    }
                    let idx = y * width + x;
                    let alpha = u32::from(coverage[idx]);
                    let value = (u32::from(cleaned[idx]) * (255 - alpha) + 255 * alpha + 127) / 255;
                    sum += value;
                    n += 1;
                }
            }
            *dst = (sum / n.max(1)) as u8;
        }
    });
    (out, ow, oh)
}

fn estimate_background(
    gray: &[u8],
    width: usize,
    height: usize,
    opts: &CleanOptions,
) -> Result<Vec<u8>> {
    let downscale = opts.bg_downscale.max(1);
    let small_w = ((width as u32) / downscale).max(1);
    let small_h = ((height as u32) / downscale).max(1);
    let blur_sigma = (opts.dpi.clamp(72, 600) / 60).max(3) as f32;

    let src = GrayImage::from_raw(width as u32, height as u32, gray.to_vec())
        .ok_or_else(|| anyhow!("failed to build GrayImage"))?;
    let small = crate::resize::resize_gray_cpu(
        &src,
        small_w,
        small_h,
        crate::resize::ResizeMethod::Bilinear,
    );

    let dilated = match opts.bg_estimator {
        BgEstimator::Blur => small,
        BgEstimator::MaxBlur => {
            let maxed = window_max(
                small.as_raw(),
                small_w as usize,
                small_h as usize,
                opts.bg_max_radius,
            );
            GrayImage::from_raw(small_w, small_h, maxed)
                .ok_or_else(|| anyhow!("failed to build dilated image"))?
        }
    };

    let bg_small = imageops::blur(&dilated, blur_sigma);
    let bg_full = crate::resize::resize_gray_cpu(
        &bg_small,
        width as u32,
        height as u32,
        crate::resize::ResizeMethod::Bilinear,
    );
    Ok(bg_full.into_raw())
}

/// Flatten everything not near surviving ink to pure white. See [`HaloOptions`].
fn halo_clean(pixels: &mut [u8], width: usize, height: usize, opts: HaloOptions) {
    let n = width * height;
    let mut mask: Vec<u8> = pixels
        .iter()
        .map(|&v| if v < opts.ink_threshold { 255 } else { 0 })
        .collect();

    if opts.min_ink_area > 1 {
        remove_small_components(&mut mask, width, height, opts.min_ink_area);
    }

    let dilated = window_max(&mask, width, height, opts.radius);
    for i in 0..n {
        if dilated[i] == 0 {
            pixels[i] = 255;
        }
    }
}

/// Zero out 4-connected mask components smaller than `min_area` pixels.
fn remove_small_components(mask: &mut [u8], width: usize, height: usize, min_area: usize) {
    let n = width * height;
    let mut visited = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut component: Vec<usize> = Vec::new();

    for start in 0..n {
        if mask[start] == 0 || visited[start] {
            continue;
        }
        component.clear();
        stack.push(start);
        visited[start] = true;
        while let Some(i) = stack.pop() {
            component.push(i);
            let (x, y) = (i % width, i / width);
            let mut push = |j: usize| {
                if mask[j] != 0 && !visited[j] {
                    visited[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 {
                push(i - 1);
            }
            if x + 1 < width {
                push(i + 1);
            }
            if y > 0 {
                push(i - width);
            }
            if y + 1 < height {
                push(i + width);
            }
        }
        if component.len() < min_area {
            for &i in &component {
                mask[i] = 0;
            }
        }
    }
}

/// Separable sliding-window max (grayscale dilation with a square kernel).
fn window_max(src: &[u8], w: usize, h: usize, radius: usize) -> Vec<u8> {
    if radius == 0 {
        return src.to_vec();
    }
    let mut tmp = vec![0u8; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, dst)| {
        let row = &src[y * w..(y + 1) * w];
        for x in 0..w {
            let lo = x.saturating_sub(radius);
            let hi = (x + radius).min(w - 1);
            dst[x] = row[lo..=hi].iter().copied().max().unwrap();
        }
    });
    let mut out = vec![0u8; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, dst)| {
        let lo = y.saturating_sub(radius);
        let hi = (y + radius).min(h - 1);
        for x in 0..w {
            let mut m = 0u8;
            for yy in lo..=hi {
                m = m.max(tmp[yy * w + x]);
            }
            dst[x] = m;
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_coverage_whitens_without_thresholding() {
        let cleaned = [10, 20, 30, 40];
        let coverage = [0, 85, 170, 255];
        let (out, width, height) = mrc_background_with_coverage(&cleaned, &coverage, 4, 1, 1);
        assert_eq!((width, height), (4, 1));
        assert_eq!(out, [10, 98, 180, 255]);
    }

    #[test]
    fn flat_white_stays_white() {
        let gray = vec![250u8; 64 * 64];
        let out = clean_gray_page(&gray, 64, 64, &CleanOptions::production(300, false)).unwrap();
        assert!(out.stats.white_pct > 99.0);
    }

    #[test]
    fn ink_mask_marks_dark_cores() {
        let mut gray = vec![255u8; 32 * 32];
        for y in 10..20 {
            for x in 10..20 {
                gray[y * 32 + x] = 0;
            }
        }
        let out = clean_gray_page(&gray, 32, 32, &CleanOptions::production(300, false)).unwrap();
        let mask = ink_mask(&out.pixels, 100);
        assert!(mask.iter().any(|&m| m == 1), "expected some ink");
        let binarized: Vec<u8> = out
            .pixels
            .iter()
            .map(|&v| if v < 100 { 0 } else { 255 })
            .collect();
        let (bg, bw, bh) = mrc_background(&out.pixels, &binarized, 32, 32, 3);
        assert_eq!((bw, bh), (11, 11));
        assert_eq!(bg.len(), 11 * 11);
    }

    #[test]
    fn rgb_cleaners_reject_invalid_buffers_and_dimensions() {
        let opts = CleanOptions::production(300, false);
        assert!(clean_rgb_page(&[255; 11], 2, 2, &opts).is_err());
        assert!(clean_page_for_mrc(&[], 0, 2, &opts, 0).is_err());
        assert!(clean_page_for_mrc_with_mask(&[255; 12], 2, 2, &opts, &[255; 3]).is_err());
    }
}
