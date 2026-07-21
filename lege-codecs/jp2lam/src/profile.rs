#![allow(dead_code)]
// Content-aware compression scaffolding.
//
// Defines the types for profile-aware and block-class-aware PCRD optimization.
// These types are not yet wired into the encode pipeline.
//
// Insertion points when ready:
//   - `rate::subband_weight_for`: multiply the norm-squared weight by
//     `class_distortion_weight(class, is_ll, resolution)`
//   - `pcrd::estimate_pass_distortion_delta`: pass the adjusted weight
//   - `quality_to_lambda`: accept an optional `ContentProfile` to shift
//     the base lambda for content-specific aggressiveness

/// High-level content profile driving compression policy.
///
/// The profile is orthogonal to quality: quality sets the target size/fidelity
/// tradeoff; the profile chooses how that tradeoff is distributed across
/// different kinds of content in the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ContentProfile {
    /// General photographic or mixed content (default).
    /// Balanced allocation; allow more loss in noisy/textured regions.
    #[default]
    Photo,
    /// Web/UI content: screenshots, flat fills, logos, composited images.
    /// Protect hard edges, flat-color regions, and anti-aliased text.
    /// Avoid ringing or chroma bleed around contrasting areas.
    WebUi,
    /// Scanned documents for e-ink or screen reading.
    /// Readability first: protect text edge sharpness above all else.
    /// Aggressively compress paper texture and flat backgrounds.
    ScanReadable,
    /// Mixed document and photo content.
    /// Use context-sensitive allocation between text and image regions.
    Mixed,
}

/// Block-level content class derived from local image statistics.
///
/// Each class changes specific encoder decisions rather than existing for
/// classification's sake. The current hook point is the distortion weight
/// in `rate::subband_weight_for` via `class_distortion_weight`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BlockClass {
    /// Nearly uniform color or solid fill.
    /// Very low variance, very low gradient energy.
    /// Policy: stop spending bytes once smooth; penalize tiny refinements
    /// that would add visible noise to flat regions.
    Flat,
    /// Sharp edge or text glyph.
    /// High gradient energy, strong directional edge concentration.
    /// Policy: reward early passes that preserve sharp structure.
    EdgeText,
    /// Smooth monotone ramp or soft shadow.
    /// Moderate variance, smooth gradient, low edge density.
    /// Policy: protect low-frequency structure; penalize contouring steps.
    Gradient,
    /// Photo-like texture.
    /// High entropy, high variance, weak directional dominance.
    /// Policy: conventional balanced RD behavior.
    #[default]
    TexturePhoto,
    /// Background noise or paper texture.
    /// Low contrast but noisy residuals.
    /// Policy: aggressively downweight; rarely helps perception.
    BackgroundNoise,
}

/// Cheap per-block statistics for classification.
///
/// Computed from the spatial-domain block before or during transform.
/// Values are normalized to [0.0, 1.0] or described per-field.
///
/// Not yet computed in the encode pipeline. Once integrated, these feed
/// `BlockFeatures::classify` to produce a `BlockClass` per code-block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BlockFeatures {
    /// Normalized pixel variance in [0, 1] (1 = max possible for bit depth).
    pub variance_norm: f32,
    /// Normalized mean absolute gradient energy in [0, 1].
    pub gradient_energy_norm: f32,
    /// Fraction of nearly-equal neighboring pixel pairs.
    pub flatness_ratio: f32,
    /// Distinct quantized color bins divided by block sample count.
    pub color_diversity_norm: f32,
    /// Block identified as edge or text by directional gradient analysis.
    pub likely_edge_text: bool,
}

impl BlockFeatures {
    /// Classify this block given a content profile.
    ///
    /// Rule-based classification using cheap spatial statistics.
    /// Thresholds are tuned for 8-bit photographic and document content.
    /// Profile-specific threshold adjustments will be added once the
    /// classification is wired into the encode pipeline and empirically tuned.
    pub(crate) fn classify(&self, _profile: ContentProfile) -> BlockClass {
        if self.variance_norm < 0.02 && self.gradient_energy_norm < 0.05 {
            return BlockClass::Flat;
        }
        if self.likely_edge_text || self.gradient_energy_norm > 0.40 {
            return BlockClass::EdgeText;
        }
        if self.variance_norm < 0.08 && self.gradient_energy_norm < 0.15 {
            return BlockClass::BackgroundNoise;
        }
        if self.variance_norm < 0.25 && self.gradient_energy_norm < 0.20 {
            return BlockClass::Gradient;
        }
        BlockClass::TexturePhoto
    }
}

/// Classify a code-block by the fraction of its quantized coefficients that
/// are non-zero.  Uses only the coefficient-domain sparsity, which is cheap
/// to compute during Tier-1 analysis.
///
/// Flat regions produce very sparse quantized subbands (≤5 % non-zero).
/// Smooth gradients are moderately sparse (≤30 %).  Photographic texture is
/// dense.  EdgeText and BackgroundNoise require spatial statistics not
/// available here and are therefore not returned by this classifier.
pub(crate) fn classify_from_nonzero_fraction(f: f32) -> BlockClass {
    if f <= 0.05 {
        BlockClass::Flat
    } else if f <= 0.30 {
        BlockClass::Gradient
    } else {
        BlockClass::TexturePhoto
    }
}

/// Class-relative distortion weight multiplier for PCRD.
///
/// Multiply the norm-squared subband weight by this factor before passing
/// it into `estimate_pass_distortion_delta`.  A weight above 1.0 causes the
/// PCRD rate-distortion optimizer to allocate more bytes to these blocks;
/// below 1.0, fewer bytes.
///
/// `band_is_ll` is true for the LL subband only (most perceptually important).
/// `resolution` is the DWT resolution level (0 = coarsest).
pub(crate) fn class_distortion_weight(class: BlockClass, band_is_ll: bool, _resolution: u8) -> f64 {
    let base = match class {
        BlockClass::Flat => 2.5,
        BlockClass::Gradient => 1.5,
        BlockClass::TexturePhoto => 1.0,
        BlockClass::BackgroundNoise => 0.7,
        BlockClass::EdgeText => 1.0,
    };
    if band_is_ll { base * 1.1 } else { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_block_classifies_as_flat() {
        let f = BlockFeatures {
            variance_norm: 0.005,
            gradient_energy_norm: 0.01,
            flatness_ratio: 0.98,
            color_diversity_norm: 0.01,
            likely_edge_text: false,
        };
        assert_eq!(f.classify(ContentProfile::Photo), BlockClass::Flat);
    }

    #[test]
    fn high_gradient_classifies_as_edge_text() {
        let f = BlockFeatures {
            variance_norm: 0.4,
            gradient_energy_norm: 0.6,
            flatness_ratio: 0.2,
            color_diversity_norm: 0.3,
            likely_edge_text: false,
        };
        assert_eq!(f.classify(ContentProfile::Photo), BlockClass::EdgeText);
    }

    #[test]
    fn noisy_low_contrast_classifies_as_background_noise() {
        let f = BlockFeatures {
            variance_norm: 0.05,
            gradient_energy_norm: 0.10,
            flatness_ratio: 0.6,
            color_diversity_norm: 0.2,
            likely_edge_text: false,
        };
        assert_eq!(
            f.classify(ContentProfile::Photo),
            BlockClass::BackgroundNoise
        );
    }

    #[test]
    fn classify_from_nonzero_fraction_boundaries() {
        assert_eq!(classify_from_nonzero_fraction(0.0), BlockClass::Flat);
        assert_eq!(classify_from_nonzero_fraction(0.05), BlockClass::Flat);
        assert_eq!(classify_from_nonzero_fraction(0.06), BlockClass::Gradient);
        assert_eq!(classify_from_nonzero_fraction(0.30), BlockClass::Gradient);
        assert_eq!(
            classify_from_nonzero_fraction(0.31),
            BlockClass::TexturePhoto
        );
        assert_eq!(
            classify_from_nonzero_fraction(1.0),
            BlockClass::TexturePhoto
        );
    }

    #[test]
    fn class_distortion_weight_flat_is_highest() {
        let flat = class_distortion_weight(BlockClass::Flat, false, 0);
        let grad = class_distortion_weight(BlockClass::Gradient, false, 0);
        let photo = class_distortion_weight(BlockClass::TexturePhoto, false, 0);
        let noise = class_distortion_weight(BlockClass::BackgroundNoise, false, 0);
        assert!(flat > grad, "flat should outweigh gradient");
        assert!(grad > photo, "gradient should outweigh texture");
        assert!(photo > noise, "texture should outweigh noise");
    }

    #[test]
    fn class_distortion_weight_ll_band_boost() {
        for class in [
            BlockClass::Flat,
            BlockClass::Gradient,
            BlockClass::TexturePhoto,
            BlockClass::BackgroundNoise,
        ] {
            let ll = class_distortion_weight(class, true, 0);
            let other = class_distortion_weight(class, false, 0);
            assert!(
                ll > other,
                "LL band should have higher weight for {class:?}"
            );
        }
    }

    #[test]
    fn class_distortion_weight_specific_values() {
        assert_eq!(class_distortion_weight(BlockClass::Flat, false, 0), 2.5);
        assert_eq!(
            class_distortion_weight(BlockClass::BackgroundNoise, false, 0),
            0.7
        );
        assert_eq!(
            class_distortion_weight(BlockClass::TexturePhoto, false, 0),
            1.0
        );
        assert!((class_distortion_weight(BlockClass::Flat, true, 0) - 2.75).abs() < 1e-10);
    }
}
