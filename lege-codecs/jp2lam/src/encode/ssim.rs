//! Slow exact perceptual evaluator.
//!
//! A candidate is serialized (or already a JP2/J2K buffer), decoded with
//! jp2lam's own decoder, converted to viewer-visible linear RGB, and scored
//! with the pinned SSIMULACRA2 implementation. Source-side metric work is
//! precomputed once.

use std::time::Instant;

use jpxl_perceptual::{
    LinearRgbView, METRIC_VERSION, PrecomputedReference, ReferenceRetention, SerialExecutor,
    Ssimulacra2,
};

use crate::decode::{DecodeLimits, DecodeRequest, DecodeResult, Jp2Decoder};
use crate::error::{Jp2LamError, Result};
use crate::model::{ColorSpace, Image, ImageView, PerceptualEffort, PerceptualObservation};

pub(crate) const LOSS_EPSILON: f64 = 1e-3;
pub(crate) const BRACKET_RATIO: f64 = 1.8;
pub(crate) const PRIOR_LOSS_EXPONENT: f64 = 0.9;
pub(crate) const EXPANSION_MARGIN: f64 = 1.25;
pub(crate) const MAX_EXPANSION_JUMP: f64 = 16.0;

pub(crate) struct QualityBudget {
    pub pixel_probes: u32,
}

impl QualityBudget {
    pub(crate) fn for_effort(effort: PerceptualEffort) -> Self {
        match effort {
            PerceptualEffort::Fast => Self { pixel_probes: 3 },
            PerceptualEffort::Balanced => Self { pixel_probes: 5 },
            PerceptualEffort::Quality => Self { pixel_probes: 10 },
        }
    }

    /// Navigation cap plus one documented rescue probe.
    pub(crate) fn max_evaluations(self) -> u32 {
        self.pixel_probes.saturating_add(1)
    }
}

pub(crate) fn loss(score: f64) -> f64 {
    (100.0 - score).max(LOSS_EPSILON)
}

pub(crate) fn aim_body_bytes(body: u32, score: f64, target: f64) -> u32 {
    let ratio = (loss(score) / loss(target)).powf(1.0 / PRIOR_LOSS_EXPONENT);
    let aimed = f64::from(body) * ratio * EXPANSION_MARGIN;
    let capped = aimed.min(f64::from(body) * MAX_EXPANSION_JUMP);
    capped.round().clamp(1.0, f64::from(u32::MAX)) as u32
}

pub(crate) fn interpolate_body_bytes(lo: (u32, f64), hi: (u32, f64), target: f64) -> Option<u32> {
    let (lo_body, lo_score) = lo;
    let (hi_body, hi_score) = hi;
    if lo_body == hi_body {
        return None;
    }
    let x0 = f64::from(lo_body).ln();
    let x1 = f64::from(hi_body).ln();
    let y0 = -loss(lo_score).ln();
    let y1 = -loss(hi_score).ln();
    let yt = -loss(target).ln();
    let span = y1 - y0;
    if !span.is_finite() || span.abs() < 1e-12 {
        return None;
    }
    let fraction = ((yt - y0) / span).clamp(0.0, 1.0);
    let x = x0 + fraction * (x1 - x0);
    Some(x.exp().round().clamp(1.0, f64::from(u32::MAX)) as u32)
}

/// Scores reconstructed streams against one precomputed source.
pub struct StreamEvaluator {
    decoder: Jp2Decoder,
    reference: PrecomputedReference,
    metric: Ssimulacra2,
    linear: [Vec<f32>; 3],
    width: u32,
    height: u32,
    max_pixels: u64,
    evaluations: u32,
}

impl StreamEvaluator {
    /// Precomputes source-side SSIMULACRA2 data. Gray is replicated into RGB.
    pub fn from_view(image: ImageView<'_>) -> Result<Self> {
        Self::from_view_ref(&image)
    }

    pub fn from_view_ref(image: &ImageView<'_>) -> Result<Self> {
        let width = image.width;
        let height = image.height;
        let pixels = usize::try_from(u64::from(width) * u64::from(height))
            .map_err(|_| Jp2LamError::InvalidInput("image area exceeds usize".into()))?;
        let mut linear = [vec![0.0; pixels], vec![0.0; pixels], vec![0.0; pixels]];
        source_to_linear_ref(image, &mut linear)?;
        let view = LinearRgbView::new(width, height, &linear[0], &linear[1], &linear[2])
            .map_err(|err| Jp2LamError::InvalidInput(err.to_string()))?;
        let reference =
            PrecomputedReference::new(view, ReferenceRetention::Moments, &SerialExecutor)
                .map_err(|err| Jp2LamError::InvalidInput(err.to_string()))?;
        Ok(Self {
            decoder: Jp2Decoder::new(),
            reference,
            metric: Ssimulacra2::new(),
            linear,
            width,
            height,
            max_pixels: u64::from(width) * u64::from(height),
            evaluations: 0,
        })
    }

    /// Number of stream scores performed (pixel probes).
    #[must_use]
    pub fn evaluations(&self) -> u32 {
        self.evaluations
    }

    /// Pinned metric identity the scores come from.
    #[must_use]
    pub fn metric_version(&self) -> &'static str {
        METRIC_VERSION
    }

    /// Decode `bytes` with jp2lam and score against the precomputed source.
    pub fn score_stream(&mut self, bytes: &[u8]) -> Result<PerceptualObservation> {
        self.evaluations = self.evaluations.saturating_add(1);
        let reconstruct_started = Instant::now();
        let decoded = self.decode_native(bytes)?;
        let reconstruct_millis = millis_since(reconstruct_started);
        self.score_decoded(&decoded, reconstruct_millis)
    }

    /// Score an already reconstructed raster (internal candidate path).
    pub(crate) fn score_image(
        &mut self,
        image: &Image,
        reconstruct_millis: Option<u64>,
    ) -> Result<PerceptualObservation> {
        self.evaluations = self.evaluations.saturating_add(1);
        if image.width != self.width || image.height != self.height {
            return Err(Jp2LamError::EncodeFailed(format!(
                "reconstructed {}x{} does not match source {}x{}",
                image.width, image.height, self.width, self.height
            )));
        }
        self.score_decoded(image, reconstruct_millis.unwrap_or(0))
    }

    fn score_decoded(
        &mut self,
        decoded: &Image,
        reconstruct_millis: u64,
    ) -> Result<PerceptualObservation> {
        let metric_started = Instant::now();
        decoded_to_linear(decoded, &mut self.linear)?;
        let view = LinearRgbView::new(
            self.width,
            self.height,
            &self.linear[0],
            &self.linear[1],
            &self.linear[2],
        )
        .map_err(|err| Jp2LamError::DecodeFailed(err.to_string()))?;
        let result = self
            .metric
            .score(&self.reference, view, &SerialExecutor)
            .map_err(|err| Jp2LamError::EncodeFailed(err.to_string()))?;
        Ok(PerceptualObservation {
            score: result.score,
            reconstruct_millis: Some(reconstruct_millis),
            metric_millis: Some(millis_since(metric_started)),
        })
    }

    fn decode_native(&mut self, bytes: &[u8]) -> Result<Image> {
        let decoded = self.decoder.decode(
            bytes,
            &DecodeRequest {
                limits: DecodeLimits {
                    max_input_bytes: bytes.len().saturating_add(1024 * 1024),
                    max_pixels: self.max_pixels.saturating_add(1),
                    max_working_bytes: usize::MAX,
                    ..DecodeLimits::default()
                },
                ..DecodeRequest::default()
            },
        )?;
        let DecodeResult::Native(image) = decoded else {
            return Err(Jp2LamError::DecodeFailed(
                "perceptual evaluator expected native planar decode".into(),
            ));
        };
        if image.width != self.width || image.height != self.height {
            return Err(Jp2LamError::DecodeFailed(format!(
                "decoded {}x{} does not match source {}x{}",
                image.width, image.height, self.width, self.height
            )));
        }
        Ok(image)
    }
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn srgb8_to_linear(sample: u8) -> f32 {
    let v = f32::from(sample) / 255.0;
    if v <= 12.92 * 0.003_130_8 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn source_to_linear_ref(image: &ImageView<'_>, out: &mut [Vec<f32>; 3]) -> Result<()> {
    fill_linear_from_view(image, out)
}

fn decoded_to_linear(image: &Image, out: &mut [Vec<f32>; 3]) -> Result<()> {
    fill_linear_from_view(&image.as_view()?, out)
}

fn fill_linear_from_view(image: &ImageView<'_>, out: &mut [Vec<f32>; 3]) -> Result<()> {
    let pixels = out[0].len();
    match image.colorspace.encoding_domain() {
        ColorSpace::Gray => {
            let component = image.components.first().ok_or_else(|| {
                Jp2LamError::InvalidInput("grayscale image has no component".into())
            })?;
            if component.precision != 8 || component.signed {
                return Err(Jp2LamError::InvalidInput(
                    "perceptual scoring supports 8-bit unsigned gray".into(),
                ));
            }
            for i in 0..pixels {
                let x = (i as u32) % image.width;
                let y = (i as u32) / image.width;
                let sample = component
                    .sample_at(x, y)
                    .ok_or_else(|| Jp2LamError::InvalidInput("gray sample out of range".into()))?;
                let linear = srgb8_to_linear(sample.clamp(0, 255) as u8);
                out[0][i] = linear;
                out[1][i] = linear;
                out[2][i] = linear;
            }
        }
        ColorSpace::Srgb => {
            if image.components.len() < 3 {
                return Err(Jp2LamError::InvalidInput(
                    "sRGB image has fewer than three components".into(),
                ));
            }
            for plane in 0..3 {
                let component = &image.components[plane];
                if component.precision != 8 || component.signed {
                    return Err(Jp2LamError::InvalidInput(
                        "perceptual scoring supports 8-bit unsigned sRGB".into(),
                    ));
                }
                for i in 0..pixels {
                    let x = (i as u32) % image.width;
                    let y = (i as u32) / image.width;
                    let sample = component.sample_at(x, y).ok_or_else(|| {
                        Jp2LamError::InvalidInput("sRGB sample out of range".into())
                    })?;
                    out[plane][i] = srgb8_to_linear(sample.clamp(0, 255) as u8);
                }
            }
        }
        other => {
            return Err(Jp2LamError::InvalidInput(format!(
                "perceptual scoring supports gray and sRGB, got {other:?}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::StreamEvaluator;
    use crate::model::{EncodeOptions, Image, OutputFormat, RateControl};
    use crate::{decode_jp2, encode};
    use jpxl_perceptual::{LinearRgbView, score_pair};

    fn gray_ramp(width: u32, height: u32) -> Image {
        let n = (width * height) as usize;
        let mut data = Vec::with_capacity(n);
        for y in 0..height {
            for x in 0..width {
                data.push(((x * 13 + y * 7) % 256) as u8);
            }
        }
        Image::from_gray_bytes(width, height, &data).expect("gray")
    }

    fn rgb_ramp(width: u32, height: u32) -> Image {
        let n = (width * height * 3) as usize;
        let mut data = Vec::with_capacity(n);
        for y in 0..height {
            for x in 0..width {
                data.push((x % 256) as u8);
                data.push((y % 256) as u8);
                data.push(((x + y) % 256) as u8);
            }
        }
        Image::from_rgb_bytes(width, height, &data).expect("rgb")
    }

    fn score_decoded_independently(source: &Image, decoded: &Image) -> f64 {
        let mut src_planes = [
            vec![0.0; (source.width * source.height) as usize],
            vec![0.0; (source.width * source.height) as usize],
            vec![0.0; (source.width * source.height) as usize],
        ];
        let mut cand_planes = src_planes.clone();
        super::source_to_linear_ref(&source.as_view().expect("src view"), &mut src_planes)
            .expect("src linear");
        super::decoded_to_linear(decoded, &mut cand_planes).expect("cand linear");
        let src = LinearRgbView::new(
            source.width,
            source.height,
            &src_planes[0],
            &src_planes[1],
            &src_planes[2],
        )
        .expect("src view");
        let cand = LinearRgbView::new(
            decoded.width,
            decoded.height,
            &cand_planes[0],
            &cand_planes[1],
            &cand_planes[2],
        )
        .expect("cand view");
        score_pair(src, cand).expect("score").score
    }

    fn assert_evaluator_matches_independent_decode(source: &Image) {
        let bytes = encode(
            source,
            &EncodeOptions {
                rate_control: Some(RateControl::Quality(50)),
                format: OutputFormat::Jp2,
                ..Default::default()
            },
        )
        .expect("encode");
        let mut evaluator =
            StreamEvaluator::from_view(source.as_view().expect("view")).expect("eval");
        assert_eq!(evaluator.metric_version(), jpxl_perceptual::METRIC_VERSION);
        let in_loop = evaluator.score_stream(&bytes).expect("in-loop");
        let decoded = decode_jp2(&bytes).expect("independent decode");
        let independent = score_decoded_independently(source, &decoded);
        assert_eq!(
            in_loop.score.to_bits(),
            independent.to_bits(),
            "in-loop {} vs independent {}",
            in_loop.score,
            independent
        );
    }

    #[test]
    fn gray_stream_score_matches_independent_decode() {
        assert_evaluator_matches_independent_decode(&gray_ramp(32, 24));
    }

    #[test]
    fn rgb_stream_score_matches_independent_decode() {
        assert_evaluator_matches_independent_decode(&rgb_ramp(24, 20));
    }
}
