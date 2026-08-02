//! JPEG 2000 encoder facade backed by jp2lam.

use crate::encoding::{EncodingError, Result};

#[derive(Debug, Clone, Default)]
pub struct Jp2Settings {
    pub num_resolutions: Option<i32>,
    pub prog_order: Option<i32>,
    pub rate: Option<f32>,
    pub rates: Option<Vec<f32>>,
    pub psnrs: Option<Vec<f32>>,
    pub irreversible: Option<bool>,
    pub tile_size: Option<(i32, i32)>,
    pub codeblock: Option<(i32, i32)>,
}

pub mod jp2_config {
    use super::Jp2Settings;

    const BASE_TARGET_KB: f64 = 120.0;
    const COVER_TARGET_KB: f64 = 220.0;
    const AREA_REF_PX: f64 = 1_000_000.0;
    const IMAGE_MIN_KB: f64 = 40.0;
    const COVER_MIN_KB: f64 = 80.0;
    const IMAGE_BPPX_MIN: f64 = 0.08;
    const IMAGE_BPPX_MAX: f64 = 0.30;
    const COVER_BPPX_MIN: f64 = 0.10;
    const COVER_BPPX_MAX: f64 = 0.40;
    const SCALING_EXPONENT: f64 = 1.0;
    const IMAGE_BIAS_BETA: f64 = 0.30;
    const COVER_BIAS_BETA: f64 = 0.25;

    fn base_target_kb(area: f64, is_cover: bool) -> f64 {
        let base = if is_cover {
            COVER_TARGET_KB
        } else {
            BASE_TARGET_KB
        };
        base * (area / AREA_REF_PX).powf(SCALING_EXPONENT)
    }

    pub fn texture_score(image_data: &[u8], width: u32, height: u32, channels: u8) -> f64 {
        if width < 3 || height < 3 {
            return 0.0;
        }
        let w = width as usize;
        let h = height as usize;
        let ch = channels.max(1) as usize;
        let area = (w * h) as f64;
        let stride = (area / 100_000.0).sqrt().ceil().max(1.0) as usize;
        let xs = (1..w - 1).step_by(stride);
        let ys = (1..h - 1).step_by(stride);

        let mut sum_grad = 0.0_f64;
        let mut count = 0_u64;
        let mut nonwhite = 0_u64;

        let lum = |x: usize, y: usize| -> f64 {
            let idx = (y * w + x) * ch;
            if ch == 1 {
                image_data.get(idx).copied().unwrap_or(255) as f64
            } else {
                let r = *image_data.get(idx).unwrap_or(&255) as f64;
                let g = *image_data.get(idx + 1).unwrap_or(&255) as f64;
                let b = *image_data.get(idx + 2).unwrap_or(&255) as f64;
                0.299 * r + 0.587 * g + 0.114 * b
            }
        };

        for y in ys.clone() {
            for x in xs.clone() {
                let c = lum(x, y);
                sum_grad += (lum(x + 1, y) - c).abs() + (lum(x, y + 1) - c).abs();
                if c < 245.0 {
                    nonwhite += 1;
                }
                count += 1;
            }
        }
        if count == 0 {
            return 0.0;
        }

        let mut histogram = [0u32; 256];
        for y in ys.clone() {
            for x in xs.clone() {
                histogram[lum(x, y).round().clamp(0.0, 255.0) as usize] += 1;
            }
        }

        let total_samples = (ys.len() * xs.len()) as f64;
        let mut entropy = 0.0;
        if total_samples > 0.0 {
            for &count in &histogram {
                if count > 0 {
                    let p = count as f64 / total_samples;
                    entropy -= p * p.log2();
                }
            }
        }

        let edge_density = ((sum_grad / count as f64) / 510.0).clamp(0.0, 1.0);
        let nonwhite_ratio = (nonwhite as f64 / count as f64).clamp(0.0, 1.0);
        let entropy_norm = (entropy / 8.0).clamp(0.0, 1.0);
        (0.4 * edge_density + 0.3 * nonwhite_ratio + 0.3 * entropy_norm).clamp(0.0, 1.0)
    }

    pub fn calculate_jp2_target_bytes(height: u32, width: u32, is_cover: bool) -> usize {
        let area = (height as f64) * (width as f64);
        let min_kb_floor = if is_cover {
            if area < 250_000.0 {
                12.0
            } else if area < 1_000_000.0 {
                24.0
            } else {
                COVER_MIN_KB
            }
        } else if area < 100_000.0 {
            4.0
        } else if area < 1_000_000.0 {
            10.0
        } else {
            IMAGE_MIN_KB
        };
        let floored_kb = base_target_kb(area, is_cover).max(min_kb_floor);
        let (bppx_min, bppx_max) = if is_cover {
            (COVER_BPPX_MIN, COVER_BPPX_MAX)
        } else {
            (IMAGE_BPPX_MIN, IMAGE_BPPX_MAX)
        };
        let clamped_kb = floored_kb.clamp((bppx_min * area) / 1024.0, (bppx_max * area) / 1024.0);
        (clamped_kb * 1024.0) as usize
    }

    pub fn calculate_jp2_target_bytes_content_aware(
        image_data: &[u8],
        width: u32,
        height: u32,
        channels: u8,
        is_cover: bool,
    ) -> usize {
        let area = (height as f64) * (width as f64);
        let beta = if is_cover {
            COVER_BIAS_BETA
        } else {
            IMAGE_BIAS_BETA
        };
        let mut kb = base_target_kb(area, is_cover)
            * (1.0 + beta * texture_score(image_data, width, height, channels));
        let min_kb_floor = if is_cover {
            if area < 250_000.0 {
                12.0
            } else if area < 1_000_000.0 {
                24.0
            } else {
                COVER_MIN_KB
            }
        } else if area < 100_000.0 {
            4.0
        } else if area < 1_000_000.0 {
            10.0
        } else {
            IMAGE_MIN_KB
        };
        kb = kb.max(min_kb_floor);
        let (bppx_min, bppx_max) = if is_cover {
            (COVER_BPPX_MIN, COVER_BPPX_MAX)
        } else {
            (IMAGE_BPPX_MIN, IMAGE_BPPX_MAX)
        };
        (kb.clamp((bppx_min * area) / 1024.0, (bppx_max * area) / 1024.0) * 1024.0) as usize
    }

    pub fn calculate_jp2_rate(original_bytes: usize, target_bytes: usize) -> f32 {
        (original_bytes as f64 / target_bytes as f64).clamp(2.0, 100.0) as f32
    }

    pub fn get_jp2_settings_for_target(
        image_data: &[u8],
        width: u32,
        height: u32,
        channels: u8,
        is_cover: bool,
    ) -> Jp2Settings {
        let target_bytes =
            calculate_jp2_target_bytes_content_aware(image_data, width, height, channels, is_cover);
        let rate = calculate_jp2_rate(image_data.len(), target_bytes);
        let area = (width as u64) * (height as u64);
        Jp2Settings {
            num_resolutions: Some(6),
            prog_order: None,
            rate: Some(rate),
            rates: None,
            psnrs: None,
            irreversible: Some(true),
            tile_size: None,
            codeblock: if area < 250_000 {
                Some((32, 32))
            } else if area > 4_000_000 {
                Some((64, 64))
            } else {
                None
            },
        }
    }

    pub fn get_jp2_settings_with_logging(
        image_data: &[u8],
        width: u32,
        height: u32,
        channels: u8,
        is_cover: bool,
    ) -> Jp2Settings {
        let settings = get_jp2_settings_for_target(image_data, width, height, channels, is_cover);

        #[cfg(feature = "debug-logging")]
        crate::encoding::streamline::log_debug_message(&format!(
            "JP2 settings created: {}x{} -> target: {}KB, rate: {:.1}, num_resolutions: {:?}, irreversible: {:?}, codeblock: {:?}",
            width,
            height,
            calculate_jp2_target_bytes(height, width, is_cover) / 1024,
            settings.rate.unwrap_or(0.0),
            settings.num_resolutions,
            settings.irreversible,
            settings.codeblock
        ));

        settings
    }
}

pub fn encode_rgb(data: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>> {
    let image = jp2lam::Image::from_rgb_bytes(width, height, data)
        .map_err(|e| EncodingError::InvalidInput(e.to_string()))?;
    encode_image(&image, quality)
}

pub fn encode_gray(data: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>> {
    let image = jp2lam::Image::from_gray_bytes(width, height, data)
        .map_err(|e| EncodingError::InvalidInput(e.to_string()))?;
    encode_image(&image, quality)
}

pub fn encode(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    settings: &Jp2Settings,
) -> Result<Vec<u8>> {
    let quality = quality_from_settings(settings);
    encode_with_quality(input, width, height, channels, quality)
}

pub fn encode_with_quality(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    quality: u8,
) -> Result<Vec<u8>> {
    validate_input(input, width, height, channels)?;
    match channels {
        1 => encode_gray(input, width, height, quality),
        3 => encode_rgb(input, width, height, quality),
        _ => Err(EncodingError::InvalidInput(
            "JP2 requires 1 (grayscale) or 3 (RGB) channels".to_string(),
        )),
    }
}

pub fn encode_to_target_size(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    target_bytes: usize,
) -> Result<(f32, Vec<u8>)> {
    validate_input(input, width, height, channels)?;
    if target_bytes == 0 {
        return Err(EncodingError::InvalidInput(
            "JP2 target size must be greater than zero".to_string(),
        ));
    }

    let mut best: Option<(u8, usize, Vec<u8>)> = None;
    // Ladder floors at JP2_MIN_QUALITY: better to miss a byte target than to
    // ship sub-50 jp2lam output.
    for quality in [92, 84, 76, 68, 60, 52, 50] {
        let data = encode_with_quality(input, width, height, channels, quality)?;
        let size = data.len();
        if best
            .as_ref()
            .map(|(_, best_size, _)| size.abs_diff(target_bytes) < best_size.abs_diff(target_bytes))
            .unwrap_or(true)
        {
            best = Some((quality, size, data));
        }
        if size <= target_bytes {
            break;
        }
    }

    let (quality, _, data) = best.ok_or_else(|| {
        EncodingError::EncoderError("JP2 target-size search produced no candidates".to_string())
    })?;
    Ok((quality_to_rate(quality), data))
}

fn encode_image(image: &jp2lam::Image, quality: u8) -> Result<Vec<u8>> {
    encode_image_with_profile(image, quality, jp2lam::ContentProfile::General)
}

/// Quality floor for every jp2lam encode in Lege: output below 50 is visibly
/// artifacted and unacceptable anywhere in the program.
pub const JP2_MIN_QUALITY: u8 = 50;

fn encode_image_with_profile(
    image: &jp2lam::Image,
    quality: u8,
    profile: jp2lam::ContentProfile,
) -> Result<Vec<u8>> {
    let options = jp2lam::EncodeOptions {
        quality: quality.clamp(JP2_MIN_QUALITY, 100),
        format: jp2lam::OutputFormat::Jp2,
        profile,
        ..jp2lam::EncodeOptions::default()
    };
    jp2lam::BatchEncoder::new(options)
        .encode_one(image)
        .map_err(|e| EncodingError::EncoderError(e.to_string()))
}

/// Encode a full cleaned document page (text/line art on a flat background)
/// as grayscale JP2 using jp2lam's document profile: quantization-driven rate
/// control with a light measured-ΔMSE PCRD trim. ~30–35% smaller than
/// [`encode_gray`] at equal quality on cleaned text pages. Do NOT use for
/// continuous-tone (photo/figure) regions — use [`encode_gray`] for those.
pub fn encode_gray_document(data: &[u8], width: u32, height: u32, quality: u8) -> Result<Vec<u8>> {
    let image = jp2lam::Image::from_gray_bytes(width, height, data)
        .map_err(|e| EncodingError::InvalidInput(e.to_string()))?;
    encode_image_with_profile(&image, quality, jp2lam::ContentProfile::Document)
}

fn validate_input(input: &[u8], width: u32, height: u32, channels: u8) -> Result<()> {
    if channels != 1 && channels != 3 {
        return Err(EncodingError::InvalidInput(
            "JP2 requires 1 (grayscale) or 3 (RGB) channels".to_string(),
        ));
    }
    if width == 0 || height == 0 {
        return Err(EncodingError::InvalidDimensions {
            format: "JP2",
            width,
            height,
        });
    }
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(channels as usize))
        .ok_or_else(|| EncodingError::InvalidInput("JP2 dimensions are too large".to_string()))?;
    if input.len() < expected_len {
        return Err(EncodingError::InvalidInput(
            "Input buffer too small for JP2 dimensions".to_string(),
        ));
    }
    Ok(())
}

fn quality_from_settings(settings: &Jp2Settings) -> u8 {
    if let Some(psnrs) = &settings.psnrs {
        return psnrs
            .last()
            .copied()
            .unwrap_or(45.0)
            .round()
            .clamp(1.0, 100.0) as u8;
    }
    if let Some(rate) = settings.rate {
        return rate_to_quality(rate);
    }
    if let Some(rates) = &settings.rates {
        return rates.last().copied().map(rate_to_quality).unwrap_or(80);
    }
    100
}

fn rate_to_quality(rate: f32) -> u8 {
    if rate <= 0.0 {
        100
    } else {
        (100.0 / rate.sqrt()).round().clamp(1.0, 100.0) as u8
    }
}

fn quality_to_rate(quality: u8) -> f32 {
    let q = quality.max(1) as f32;
    (100.0 / q).powi(2)
}

#[derive(Clone, Copy)]
struct ComponentSampleScale {
    levels: u64,
    minimum: i64,
}

impl ComponentSampleScale {
    fn new(component: &jp2lam::Component) -> crate::encoding::Result<Self> {
        let precision = component.precision;
        if !(1..=32).contains(&precision) {
            return Err(crate::encoding::EncodingError::EncoderError(format!(
                "JP2 component precision {precision} is unsupported"
            )));
        }

        Ok(Self {
            levels: (1u64 << precision) - 1,
            minimum: if component.signed {
                -(1i64 << (precision - 1))
            } else {
                0
            },
        })
    }

    #[inline]
    fn to_u8(self, sample: i32) -> u8 {
        let normalized = (i64::from(sample) - self.minimum).clamp(0, self.levels as i64) as u64;
        ((normalized * 255) / self.levels) as u8
    }
}

fn validate_decoded_component(
    component: &jp2lam::Component,
    pixel_count: usize,
    channel_name: &str,
) -> crate::encoding::Result<()> {
    if component.width == 0 || component.height == 0 || component.data.len() < pixel_count {
        return Err(crate::encoding::EncodingError::EncoderError(format!(
            "JP2 {channel_name} component is smaller than the decoded image"
        )));
    }
    Ok(())
}

/// Convert a decoded `jp2lam::Image` to an interleaved RGB byte buffer.
/// Grayscale is expanded to RGB. High-precision samples are scaled to 8-bit.
fn jp2_image_to_rgb(img: jp2lam::Image) -> crate::encoding::Result<(u32, u32, Vec<u8>)> {
    let width = img.width;
    let height = img.height;
    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| {
            crate::encoding::EncodingError::EncoderError(
                "JP2 decoded dimensions are too large".into(),
            )
        })?;
    let rgb_capacity = pixel_count.checked_mul(3).ok_or_else(|| {
        crate::encoding::EncodingError::EncoderError(
            "JP2 decoded RGB buffer would be too large".into(),
        )
    })?;

    match img.colorspace {
        jp2lam::ColorSpace::Gray => {
            let comp = img.components.into_iter().next().ok_or_else(|| {
                crate::encoding::EncodingError::EncoderError(
                    "JP2 gray image has no component".into(),
                )
            })?;
            validate_decoded_component(&comp, pixel_count, "gray")?;
            let scale = ComponentSampleScale::new(&comp)?;
            let mut rgb = Vec::with_capacity(rgb_capacity);
            for sample in comp.data.iter().take(pixel_count) {
                let v = scale.to_u8(*sample);
                rgb.push(v);
                rgb.push(v);
                rgb.push(v);
            }
            Ok((width, height, rgb))
        }
        jp2lam::ColorSpace::Srgb | jp2lam::ColorSpace::Rgb => {
            let mut comps = img.components.into_iter();
            let r_comp = comps.next().ok_or_else(|| {
                crate::encoding::EncodingError::EncoderError(
                    "JP2 RGB image missing R component".into(),
                )
            })?;
            let g_comp = comps.next().ok_or_else(|| {
                crate::encoding::EncodingError::EncoderError(
                    "JP2 RGB image missing G component".into(),
                )
            })?;
            let b_comp = comps.next().ok_or_else(|| {
                crate::encoding::EncodingError::EncoderError(
                    "JP2 RGB image missing B component".into(),
                )
            })?;
            validate_decoded_component(&r_comp, pixel_count, "red")?;
            validate_decoded_component(&g_comp, pixel_count, "green")?;
            validate_decoded_component(&b_comp, pixel_count, "blue")?;
            let r_scale = ComponentSampleScale::new(&r_comp)?;
            let g_scale = ComponentSampleScale::new(&g_comp)?;
            let b_scale = ComponentSampleScale::new(&b_comp)?;
            let mut rgb = Vec::with_capacity(rgb_capacity);
            for i in 0..pixel_count {
                let r = r_scale.to_u8(r_comp.data[i]);
                let g = g_scale.to_u8(g_comp.data[i]);
                let b = b_scale.to_u8(b_comp.data[i]);
                rgb.push(r);
                rgb.push(g);
                rgb.push(b);
            }
            Ok((width, height, rgb))
        }
        other => Err(crate::encoding::EncodingError::EncoderError(format!(
            "JP2 colorspace {other:?} is not supported for decoding"
        ))),
    }
}

/// Decode a JP2 file from in-memory bytes to an interleaved RGB byte buffer.
///
/// Returns `(width, height, rgb_bytes)` where `rgb_bytes` has length `width * height * 3`.
/// Grayscale JP2 images are expanded to RGB by repeating the luma channel.
/// High-precision (>8-bit) samples are scaled down to 8-bit range.
///
/// Uses the jp2lam batch API internally. For processing many images from the same
/// source use [`Jp2BatchDecoder`] to benefit from profile consistency validation.
pub fn decode_jp2_bytes(bytes: &[u8]) -> crate::encoding::Result<(u32, u32, Vec<u8>)> {
    let img = jp2lam::BatchDecoder::new()
        .decode_one(bytes)
        .map_err(|e| crate::encoding::EncodingError::EncoderError(format!("JP2 decode: {e}")))?;
    jp2_image_to_rgb(img)
}

/// Stateful encoder that validates all images share the same profile.
/// Create one instance per processing job and call [`encode_one`](Jp2BatchEncoder::encode_one)
/// for each image from that source. Enforces consistent dimensions and colour space.
pub struct Jp2BatchEncoder {
    inner: jp2lam::BatchEncoder,
}

impl Jp2BatchEncoder {
    pub fn new(quality: u8) -> Self {
        Self {
            inner: jp2lam::BatchEncoder::new(jp2lam::EncodeOptions {
                quality: quality.clamp(JP2_MIN_QUALITY, 100),
                format: jp2lam::OutputFormat::Jp2,
                profile: jp2lam::ContentProfile::General,
                ..jp2lam::EncodeOptions::default()
            }),
        }
    }

    /// Encode one image and validate it matches the profile of previously encoded images.
    pub fn encode_one(
        &mut self,
        input: &[u8],
        width: u32,
        height: u32,
        channels: u8,
    ) -> Result<Vec<u8>> {
        validate_input(input, width, height, channels)?;
        let image = match channels {
            1 => jp2lam::Image::from_gray_bytes(width, height, input)
                .map_err(|e| EncodingError::InvalidInput(e.to_string()))?,
            3 => jp2lam::Image::from_rgb_bytes(width, height, input)
                .map_err(|e| EncodingError::InvalidInput(e.to_string()))?,
            _ => unreachable!(),
        };
        self.inner
            .encode_one(&image)
            .map_err(|e| EncodingError::EncoderError(e.to_string()))
    }
}

/// Stateful decoder that validates all images share the same profile.
/// Create one instance per processing job and call [`decode_one`](Jp2BatchDecoder::decode_one)
/// for each image from that source. Enforces consistent dimensions and colour space.
pub struct Jp2BatchDecoder {
    inner: jp2lam::BatchDecoder,
}

impl Jp2BatchDecoder {
    pub fn new() -> Self {
        Self {
            inner: jp2lam::BatchDecoder::new(),
        }
    }

    /// Decode one JP2 image and validate it matches the profile of previously decoded images.
    /// Returns `(width, height, rgb_bytes)`.
    pub fn decode_one(&mut self, bytes: &[u8]) -> crate::encoding::Result<(u32, u32, Vec<u8>)> {
        let img = self.inner.decode_one(bytes).map_err(|e| {
            crate::encoding::EncodingError::EncoderError(format!("JP2 batch decode: {e}"))
        })?;
        jp2_image_to_rgb(img)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(data: Vec<i32>, precision: u32, signed: bool) -> jp2lam::Component {
        jp2lam::Component {
            data,
            width: 1,
            height: 1,
            precision,
            signed,
            dx: 1,
            dy: 1,
        }
    }

    #[test]
    fn decoded_rgb_scales_each_component_at_its_own_precision() {
        let image = jp2lam::Image {
            width: 1,
            height: 1,
            components: vec![
                component(vec![255], 8, false),
                component(vec![15], 4, false),
                component(vec![1023], 10, false),
            ],
            colorspace: jp2lam::ColorSpace::Rgb,
        };

        let (_, _, rgb) = jp2_image_to_rgb(image).expect("mixed precision RGB");
        assert_eq!(rgb, vec![255, 255, 255]);
    }

    #[test]
    fn decoded_component_length_is_validated_before_indexing() {
        let image = jp2lam::Image {
            width: 2,
            height: 1,
            components: vec![
                jp2lam::Component {
                    width: 2,
                    ..component(vec![255], 8, false)
                },
                jp2lam::Component {
                    width: 2,
                    ..component(vec![255, 255], 8, false)
                },
                jp2lam::Component {
                    width: 2,
                    ..component(vec![255, 255], 8, false)
                },
            ],
            colorspace: jp2lam::ColorSpace::Rgb,
        };

        assert!(jp2_image_to_rgb(image).is_err());
    }

    #[test]
    fn jp2_input_rejects_zero_dimensions() {
        assert!(matches!(
            validate_input(&[], 0, 1, 1),
            Err(EncodingError::InvalidDimensions { .. })
        ));
    }
}
