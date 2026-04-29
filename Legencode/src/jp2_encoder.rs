//! JPEG 2000 encoder facade backed by jp2lam.

use crate::{EncodingError, Result};

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
        (original_bytes as f64 / target_bytes as f64)
            .clamp(2.0, 100.0) as f32
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
        crate::streamline::log_debug_message(&format!(
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
    for quality in [92, 84, 76, 68, 60, 52, 44, 36, 28, 20, 12, 6] {
        let data = encode_with_quality(input, width, height, channels, quality)?;
        let size = data.len();
        if best
            .as_ref()
            .map(|(_, best_size, _)| {
                size.abs_diff(target_bytes) < best_size.abs_diff(target_bytes)
            })
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
    let options = jp2lam::EncodeOptions {
        quality: quality.min(100),
        format: jp2lam::OutputFormat::Jp2,
    };
    jp2lam::encode(image, &options).map_err(|e| EncodingError::EncoderError(e.to_string()))
}

fn validate_input(input: &[u8], width: u32, height: u32, channels: u8) -> Result<()> {
    if channels != 1 && channels != 3 {
        return Err(EncodingError::InvalidInput(
            "JP2 requires 1 (grayscale) or 3 (RGB) channels".to_string(),
        ));
    }
    let expected_len = width as usize * height as usize * channels as usize;
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
        return rates
            .last()
            .copied()
            .map(rate_to_quality)
            .unwrap_or(80);
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
