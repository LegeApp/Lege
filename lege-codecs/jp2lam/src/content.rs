//! Content-adaptive quality selection for PDF-derived images.
//!
//! When no explicit quality is given, jp2lam classifies the image into one of two
//! content classes and picks a lossy quality tuned for that class. The classifier is
//! deliberately bare-bones: it reads a single luma histogram (one pass over the
//! pixels — no DWT, edge detection, or connected-component analysis) and decides from
//! two scalars. This keeps detection negligible next to the encode itself.
//!
//! ## Provenance
//! The two classes and their quality targets come from a study of the `pdf-derived`
//! corpus (11,469 page images from ~10,600 PDFs). Images cluster into four document
//! types — continuous-tone photos, line art / bilevel (text, diagrams, music, plans),
//! color maps, and detailed engravings — which collapse, for tuning, into:
//!
//! * **Photo** (continuous tone: photographs, plates, detailed engravings)
//! * **Graphics** (sparse marks on white: line art, text, diagrams, maps)
//!
//! A jp2lam lossy q-sweep measured an edge-aware SSIM (windowed SSIM over text/line
//! regions) vs the PNG source per class. To hold edge-SSIM ≥ 0.95, Photo needs
//! ~q78 while Graphics holds at q55 — so a type-aware quality is ~20% smaller than a
//! single flat quality at equal text fidelity, the win coming almost entirely from
//! line art tolerating aggressive lossy.
//!
//! The two classes are recognizable out-of-sample (grouped by source PDF so no page
//! leaks between train/test) from just `bright_share` and `entropy`: this hardcoded
//! rule scores macro-F1 ≈ 0.88 on held-out images, matching a 92-feature model on the
//! same 2-way task. Maps are folded into Graphics (they are edge/label-heavy like line
//! art); see [`GRAPHICS_QUALITY`] for the fidelity trade-off note.

use crate::model::{ColorSpace, Image};

/// Lossy quality for continuous-tone images (photographs, plates, engravings).
/// Targets edge-aware SSIM ≥ 0.95 on the pdf-derived photo class.
pub const PHOTO_QUALITY: u8 = 78;

/// Lossy quality for sparse-on-white graphics (line art, text, diagrams, maps).
/// Line art holds edge-aware SSIM ≥ 0.98 at this quality. Color maps — folded into
/// this class — sit nearer ~0.88 here; if map-label fidelity matters more than the
/// size win, route colorful graphics higher (see [`ContentStats::colorfulness`]).
pub const GRAPHICS_QUALITY: u8 = 55;

/// Fraction-of-near-white-pixels threshold separating graphics from photos.
const BRIGHT_SHARE_GATE: f32 = 0.45;
/// Luma value (0–255) above which a pixel counts toward `bright_share` (`luma > 224`).
const BRIGHT_LUMA_MIN: u32 = 224;
/// Entropy below which low-tone-variety images are graphics regardless of brightness.
const ENTROPY_LOW: f32 = 3.0;
/// Entropy ceiling for the bright-background branch; above this a bright image is a
/// high-key photo (snow/sky), not a document page.
const ENTROPY_BRIGHT_MAX: f32 = 5.0;

/// Coarse content class used to pick a lossy quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentClass {
    /// Continuous-tone imagery — photographs, plates, detailed engravings.
    Photo,
    /// Sparse marks on a light ground — line art, text, diagrams, maps.
    Graphics,
}

impl ContentClass {
    /// Tuned lossy quality for this class.
    pub fn quality(self) -> u8 {
        match self {
            Self::Photo => PHOTO_QUALITY,
            Self::Graphics => GRAPHICS_QUALITY,
        }
    }

    /// Short label for logging (`"photo"` / `"graphics"`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Graphics => "graphics",
        }
    }
}

/// Cheap global statistics read from a single luma-histogram pass.
#[derive(Debug, Clone, Copy)]
pub struct ContentStats {
    /// Fraction of pixels brighter than [`BRIGHT_LUMA_MIN`] — the near-white page ground.
    pub bright_share: f32,
    /// Shannon entropy (bits) of the 256-bin luma histogram; low for flat document pages.
    pub entropy: f32,
    /// Mean per-pixel chroma spread `max(r,g,b) - min(r,g,b)` in 0–255; ~0 for grayscale,
    /// high for color maps. Not used by [`classify`](ContentStats::classify) but exposed
    /// for callers that want to special-case colorful graphics.
    pub colorfulness: f32,
}

impl ContentStats {
    /// Classify from the two gating statistics.
    ///
    /// Graphics when tonal variety is very low, or when a bright page ground pairs with
    /// modest entropy; otherwise Photo. Matches the rule validated at macro-F1 ≈ 0.88
    /// out-of-sample on the pdf-derived corpus.
    pub fn classify(&self) -> ContentClass {
        let graphics = self.entropy < ENTROPY_LOW
            || (self.bright_share > BRIGHT_SHARE_GATE && self.entropy < ENTROPY_BRIGHT_MAX);
        if graphics {
            ContentClass::Graphics
        } else {
            ContentClass::Photo
        }
    }
}

/// Integer BT.601 luma from 8-bit R/G/B (matches the study's `cv2.COLOR_BGR2GRAY`).
#[inline]
fn luma601(r: i32, g: i32, b: i32) -> usize {
    let r = r.clamp(0, 255);
    let g = g.clamp(0, 255);
    let b = b.clamp(0, 255);
    (((77 * r + 150 * g + 29 * b + 128) >> 8).clamp(0, 255)) as usize
}

#[inline]
fn normalize_to_u8(sample: i32, precision: u32) -> i32 {
    let max = ((1u32 << precision.min(16)) - 1) as f64;
    (f64::from(sample).clamp(0.0, max) * 255.0 / max).round() as i32
}

/// Compute [`ContentStats`] in a single pass over the image's pixels.
pub fn analyze(image: &Image) -> ContentStats {
    let mut hist = [0u64; 256];
    let mut colorful_sum: u64 = 0;
    let mut count: u64 = 0;

    match image.colorspace {
        ColorSpace::Gray => {
            if let Some(c) = image.components.first() {
                for &v in &c.data {
                    hist[normalize_to_u8(v, c.precision) as usize] += 1;
                    count += 1;
                }
            }
        }
        // RGB-family: components are R, G, B planes (see CLI `to_jp2lam_image`).
        _ => {
            if image.components.len() >= 3 {
                let r = &image.components[0].data;
                let g = &image.components[1].data;
                let b = &image.components[2].data;
                let n = r.len().min(g.len()).min(b.len());
                for i in 0..n {
                    let (rv, gv, bv) = (
                        normalize_to_u8(r[i], image.components[0].precision),
                        normalize_to_u8(g[i], image.components[1].precision),
                        normalize_to_u8(b[i], image.components[2].precision),
                    );
                    hist[luma601(rv, gv, bv)] += 1;
                    let mx = rv.max(gv).max(bv).clamp(0, 255);
                    let mn = rv.min(gv).min(bv).clamp(0, 255);
                    colorful_sum += (mx - mn) as u64;
                    count += 1;
                }
            }
        }
    }

    if count == 0 {
        return ContentStats {
            bright_share: 0.0,
            entropy: 0.0,
            colorfulness: 0.0,
        };
    }
    let total = count as f32;

    let bright: u64 = hist[(BRIGHT_LUMA_MIN as usize + 1)..].iter().sum();
    let bright_share = bright as f32 / total;

    let mut entropy = 0.0f32;
    for &c in hist.iter() {
        if c > 0 {
            let p = c as f32 / total;
            entropy -= p * p.log2();
        }
    }

    ContentStats {
        bright_share,
        entropy,
        colorfulness: colorful_sum as f32 / total,
    }
}

/// Classify `image` and return the tuned lossy quality for its content class.
pub fn auto_quality(image: &Image) -> u8 {
    analyze(image).classify().quality()
}

/// Classify `image`, returning both the class (for logging) and its tuned quality.
pub fn auto_class_and_quality(image: &Image) -> (ContentClass, u8) {
    let class = analyze(image).classify();
    (class, class.quality())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Image;

    #[test]
    fn flat_white_page_is_graphics() {
        // Mostly white with a little black "ink": high bright_share, low entropy.
        let (w, h) = (40u32, 40u32);
        let mut px = vec![255u8; (w * h) as usize];
        for p in px.iter_mut().take(200) {
            *p = 0;
        }
        let img = Image::from_gray_bytes(w, h, &px).unwrap();
        let stats = analyze(&img);
        assert!(
            stats.bright_share > 0.45,
            "bright_share={}",
            stats.bright_share
        );
        assert_eq!(stats.classify(), ContentClass::Graphics);
        assert_eq!(auto_quality(&img), GRAPHICS_QUALITY);
    }

    #[test]
    fn high_entropy_gradient_is_photo() {
        // Full-range tonal ramp: high entropy, low bright_share -> photo.
        let (w, h) = (256u32, 64u32);
        let mut px = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                px[(y * w + x) as usize] = x as u8;
            }
        }
        let img = Image::from_gray_bytes(w, h, &px).unwrap();
        let stats = analyze(&img);
        assert!(stats.entropy > 5.0, "entropy={}", stats.entropy);
        assert_eq!(stats.classify(), ContentClass::Photo);
        assert_eq!(auto_quality(&img), PHOTO_QUALITY);
    }

    #[test]
    fn colorfulness_zero_for_gray() {
        let img = Image::from_gray_bytes(8, 8, &[128; 64]).unwrap();
        assert_eq!(analyze(&img).colorfulness, 0.0);
    }
}
