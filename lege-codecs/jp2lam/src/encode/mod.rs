pub(crate) mod backend;
pub(crate) mod block_store;
pub(crate) mod context;

#[cfg(feature = "counters")]
pub mod counters;

use crate::error::{Jp2LamError, Result};
use crate::j2k::CodestreamParts;
use crate::jp2;
use crate::model::{EncodeOptions, Image, ImageView, OutputFormat};
use backend::{CodestreamBackend, NativeBackend};
use context::EncodeContext;
use std::io::Write;
#[cfg(feature = "profile")]
use std::time::Instant;

#[cfg(feature = "profile")]
static TIMING_DATA: std::sync::Mutex<Vec<(String, std::time::Duration)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(feature = "profile")]
pub fn profile_enter(name: &'static str) -> ProfileScope {
    ProfileScope(name, Instant::now())
}

#[cfg(not(feature = "profile"))]
pub fn profile_enter(_name: &'static str) -> ProfileScope {
    ProfileScope
}

#[cfg(feature = "profile")]
pub fn print_timing_data() {
    if let Ok(times) = TIMING_DATA.lock() {
        if times.is_empty() {
            return;
        }
        let mut sorted: Vec<_> = times.clone();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        let total: std::time::Duration = sorted.iter().map(|t| t.1).sum();
        println!("\n=== Profile Timing ({} entries) ===", sorted.len());
        println!("  primitives: {}", crate::simd::active_backend());
        for (name, dur) in sorted.iter().take(20) {
            let pct = 100.0 * dur.as_secs_f64() / total.as_secs_f64();
            println!(
                "  {:>6.2}% {:12.3}ms  {}",
                pct,
                dur.as_secs_f64() * 1000.0,
                name
            );
        }
        println!("  Total: {:.3}ms", total.as_secs_f64() * 1000.0);
    }
}

#[cfg(not(feature = "profile"))]
pub fn print_timing_data() {}

#[cfg(feature = "profile")]
pub fn clear_timing_data() {
    if let Ok(mut times) = TIMING_DATA.lock() {
        times.clear();
    }
}

#[cfg(not(feature = "profile"))]
pub fn clear_timing_data() {}

#[cfg(not(feature = "counters"))]
pub mod counters {
    pub(crate) fn record_tile_samples(_: usize) {}
    pub(crate) fn record_dwt_coefficients(_: usize) {}
    pub(crate) fn record_dwt_scratch(_: usize) {}
    pub(crate) fn record_codeblock_worker(_: usize) {}
    pub(crate) fn record_encoded_store(_: usize, _: u64) {}
    pub(crate) fn record_rd_metadata(_: usize) {}
    pub(crate) fn record_packet_header(_: usize) {}
    pub(crate) fn record_output_buffer(_: usize) {}
}

#[cfg(feature = "profile")]
pub struct ProfileScope(&'static str, Instant);

#[cfg(not(feature = "profile"))]
pub struct ProfileScope;

#[cfg(feature = "profile")]
impl Drop for ProfileScope {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            let elapsed = self.1.elapsed();
            if let Ok(mut times) = TIMING_DATA.lock() {
                times.push((self.0.to_string(), elapsed));
            }
        }
    }
}

pub fn encode(image: &Image, options: &EncodeOptions) -> Result<Vec<u8>> {
    encode_view(image.as_view()?, options)
}

pub fn encode_view(image: ImageView<'_>, options: &EncodeOptions) -> Result<Vec<u8>> {
    let _p = profile_enter("encode::total");
    let context = EncodeContext::new_view(image, options)?;
    let native = NativeBackend;
    if !native.supports(&context) {
        return Err(Jp2LamError::EncodeFailed(
            "native backend does not support this lane".to_string(),
        ));
    }
    let backend_codestream = native.encode_codestream(&context)?;
    CodestreamParts::parse_single_tile(&backend_codestream)?;
    let codestream = backend_codestream;

    let output = match context.plan.output_format {
        OutputFormat::J2k => Ok(codestream),
        OutputFormat::Jp2 => {
            jp2::wrap_codestream_view(&context.image, &context.plan.color_encoding, &codestream)
        }
    }?;
    counters::record_output_buffer(output.capacity());
    Ok(output)
}

pub fn encode_to_writer<W: Write>(
    image: &Image,
    options: &EncodeOptions,
    writer: &mut W,
) -> Result<()> {
    encode_view_to_writer(image.as_view()?, options, writer)
}

pub fn encode_view_to_writer<W: Write>(
    image: ImageView<'_>,
    options: &EncodeOptions,
    writer: &mut W,
) -> Result<()> {
    let _p = profile_enter("encode_to_writer::total");
    let context = EncodeContext::new_view(image, options)?;
    let native = NativeBackend;
    if !native.supports(&context) {
        return Err(Jp2LamError::EncodeFailed(
            "native backend does not support this lane".to_string(),
        ));
    }
    let parts = native.prepare_codestream_parts(&context)?;
    let emit_plan = native.emit_plan(&context.plan);
    match context.plan.output_format {
        OutputFormat::J2k => parts.write_to(&emit_plan, writer),
        OutputFormat::Jp2 => {
            let codestream_len = parts.byte_len(&emit_plan)?;
            jp2::write_jp2_header_for_view(
                &context.image,
                &context.plan.color_encoding,
                codestream_len,
                writer,
            )?;
            parts.write_to(&emit_plan, writer)
        }
    }
}

/// Image quality metrics from an encode cycle.
#[derive(Debug, Clone, Copy)]
pub struct EncodeMetrics {
    /// PSNR in dB. `f64::INFINITY` for lossless encodes.
    pub psnr_db: f64,
    /// Mean SSIM over 8×8 luma blocks, in [0, 1]. Higher is better.
    /// 1.0 for lossless encodes.
    pub ssim: f64,
}

/// Encode and compute internal quality metrics (PSNR + SSIM) in one call.
///
/// Decodes the returned bytes with the crate's internal decoder, so the
/// metrics include the actual tile layout and rate selection.
/// For lossless encodes (quality == 100), returns `psnr_db = f64::INFINITY`
/// and `ssim = 1.0`.
pub fn encode_with_psnr(
    image: &Image,
    options: &EncodeOptions,
) -> Result<(Vec<u8>, EncodeMetrics)> {
    let bytes = encode(image, options)?;
    let context = EncodeContext::new(image, options)?;
    let native = NativeBackend;
    let metrics = native.compute_quality_metrics(&context, &bytes)?;
    Ok((bytes, metrics))
}

#[cfg(test)]
mod tests {
    use super::{encode, encode_to_writer, encode_view, encode_view_to_writer, encode_with_psnr};
    use crate::model::{
        EncodeOptions, Image, ImageView, OutputFormat, RateControl, ResourceLimits, TilePolicy,
    };

    #[test]
    fn encode_to_writer_matches_buffered_j2k() {
        let image = Image::from_gray_bytes(17, 13, &gray_samples(17, 13)).expect("image");
        let options = EncodeOptions {
            quality: 100,
            format: OutputFormat::J2k,
            profile: Default::default(),
            ..Default::default()
        };

        let buffered = encode(&image, &options).expect("buffered encode");
        let mut streamed = Vec::new();
        encode_to_writer(&image, &options, &mut streamed).expect("writer encode");

        assert_eq!(streamed, buffered);
    }

    #[test]
    fn encode_to_writer_matches_buffered_jp2() {
        let image = Image::from_rgb_bytes(11, 9, &rgb_samples(11, 9)).expect("image");
        let options = EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        };

        let buffered = encode(&image, &options).expect("buffered encode");
        let mut streamed = Vec::new();
        encode_to_writer(&image, &options, &mut streamed).expect("writer encode");

        assert_eq!(streamed, buffered);
    }

    #[test]
    fn encode_to_writer_matches_buffered_spilled_multi_tile_j2k() {
        let image = Image::from_gray_bytes(17, 13, &gray_samples(17, 13)).expect("image");
        let options = EncodeOptions {
            quality: 100,
            format: OutputFormat::J2k,
            tile_policy: TilePolicy::Fixed {
                width: 8,
                height: 8,
            },
            resource_limits: ResourceLimits {
                encoded_store_memory_limit: Some(1),
                ..Default::default()
            },
            ..Default::default()
        };

        let buffered = encode(&image, &options).expect("buffered encode");
        let mut streamed = Vec::new();
        encode_to_writer(&image, &options, &mut streamed).expect("writer encode");

        assert_eq!(streamed, buffered);
    }

    #[test]
    fn encode_view_to_writer_matches_buffered_view_jp2() {
        let samples = rgb_samples(7, 5);
        let options = EncodeOptions {
            quality: 100,
            format: OutputFormat::Jp2,
            profile: Default::default(),
            ..Default::default()
        };

        let buffered = encode_view(
            ImageView::from_rgb8_interleaved(7, 5, &samples).expect("view"),
            &options,
        )
        .expect("buffered view encode");
        let mut streamed = Vec::new();
        encode_view_to_writer(
            ImageView::from_rgb8_interleaved(7, 5, &samples).expect("view"),
            &options,
            &mut streamed,
        )
        .expect("writer view encode");

        assert_eq!(streamed, buffered);
    }

    #[test]
    fn annex_a_4_2_fixed_tiles_emit_raster_order_tile_parts() {
        let image = Image::from_gray_bytes(17, 13, &gray_samples(17, 13)).expect("image");
        let options = EncodeOptions {
            quality: 100,
            format: OutputFormat::J2k,
            tile_policy: TilePolicy::Fixed {
                width: 8,
                height: 8,
            },
            ..Default::default()
        };

        let bytes = encode(&image, &options).expect("multi-tile encode");
        let parts = crate::j2k::CodestreamParts::parse_single_tile(&bytes).expect("parse");

        assert_eq!(parts.tile_parts.len(), 6);
        for (expected_index, tile_part) in parts.tile_parts.iter().enumerate() {
            assert_eq!(usize::from(tile_part.header.tile_index), expected_index);
            assert_eq!(tile_part.header.part_index, 0);
            assert_eq!(tile_part.header.total_parts, 1);
            assert!(tile_part.payload.byte_len() > 0);
        }
    }

    #[test]
    fn lossy_multi_tile_uses_global_pcrd_and_decodes() {
        let image = Image::from_gray_bytes(17, 13, &gray_samples(17, 13)).expect("image");
        let mut sizes = Vec::new();
        for quality in [30, 80] {
            let options = EncodeOptions {
                quality,
                format: OutputFormat::Jp2,
                tile_policy: TilePolicy::Fixed {
                    width: 7,
                    height: 5,
                },
                resource_limits: ResourceLimits {
                    encoded_store_memory_limit: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            };

            let encoded = encode(&image, &options).expect("lossy multi-tile encode");
            let decoded = crate::decode::decode_jp2(&encoded).expect("decode lossy multi-tile JP2");
            assert_eq!((decoded.width, decoded.height), (image.width, image.height));
            assert_eq!(decoded.components.len(), 1);
            sizes.push(encoded.len());
        }
        assert!(
            sizes[0] < sizes[1],
            "higher-quality global selection must retain more bytes: {sizes:?}"
        );
    }

    #[test]
    fn tiled_quality_metrics_match_the_returned_codestream_decode() {
        let width = 16;
        let height = 16;
        let source = gray_samples(width, height);
        let image = Image::from_gray_bytes(width, height, &source).expect("image");
        for format in [OutputFormat::J2k, OutputFormat::Jp2] {
            let options = EncodeOptions {
                quality: 55,
                format,
                tile_policy: TilePolicy::Fixed {
                    width: 7,
                    height: 5,
                },
                resource_limits: ResourceLimits {
                    encoded_store_memory_limit: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            };

            let (encoded, metrics) =
                encode_with_psnr(&image, &options).expect("tiled quality metrics");
            let decoded = crate::decode::decode_jp2(&encoded).expect("decode returned bytes");
            let reconstructed = &decoded.components[0].data;
            let expected_psnr = psnr_from_gray(&source, reconstructed);
            let expected_ssim =
                ssim_from_gray(&source, reconstructed, width as usize, height as usize);

            assert!(
                (metrics.psnr_db - expected_psnr).abs() < 1e-10,
                "{format:?}"
            );
            assert!((metrics.ssim - expected_ssim).abs() < 1e-12, "{format:?}");
        }
    }

    #[test]
    fn exact_rate_modes_target_complete_jp2_output() {
        let width = 128;
        let height = 96;
        let image =
            Image::from_gray_bytes(width, height, &gray_samples(width, height)).expect("image");
        let target_bytes = 1_536u64;
        let modes = [
            RateControl::TargetBytes(target_bytes),
            RateControl::TargetBitsPerPixel(1.0),
            RateControl::CompressionRatio(8.0),
        ];
        let mut encoded_sizes = Vec::new();

        for rate_control in modes {
            let options = EncodeOptions {
                rate_control: Some(rate_control),
                format: OutputFormat::Jp2,
                ..Default::default()
            };
            let encoded = encode(&image, &options).expect("exact-rate encode");
            crate::decode::decode_jp2(&encoded).expect("decode exact-rate output");
            assert!(encoded.len() as u64 <= target_bytes);
            assert!(
                encoded.len() as u64 >= target_bytes * 97 / 100,
                "target {target_bytes}, actual {} for {rate_control:?}",
                encoded.len()
            );
            encoded_sizes.push(encoded.len());
        }

        assert!(encoded_sizes.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn exact_rate_ladders_are_monotone_for_j2k_and_jp2() {
        let width = 128;
        let height = 96;
        let image =
            Image::from_gray_bytes(width, height, &gray_samples(width, height)).expect("image");

        for format in [OutputFormat::J2k, OutputFormat::Jp2] {
            let mut previous = 0usize;
            for target in [900u64, 1_200, 1_500] {
                let encoded = encode(
                    &image,
                    &EncodeOptions {
                        rate_control: Some(RateControl::TargetBytes(target)),
                        format,
                        ..Default::default()
                    },
                )
                .expect("exact-rate ladder encode");
                assert!(encoded.len() >= previous, "non-monotone {format:?} ladder");
                assert!(encoded.len() as u64 <= target);
                assert!(
                    encoded.len() as u64 >= target * 97 / 100,
                    "target {target}, actual {} for {format:?}",
                    encoded.len()
                );
                previous = encoded.len();
            }
        }
    }

    fn gray_samples(width: u32, height: u32) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| ((x * 7 + y * 11 + (x ^ y)) & 0xff) as u8))
            .collect()
    }

    fn rgb_samples(width: u32, height: u32) -> Vec<u8> {
        let mut samples = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            for x in 0..width {
                samples.push(((x * 17 + y * 3) & 0xff) as u8);
                samples.push(((x * 5 + y * 19 + 33) & 0xff) as u8);
                samples.push(((x * y + x * 11 + y * 7) & 0xff) as u8);
            }
        }
        samples
    }

    fn psnr_from_gray(source: &[u8], reconstructed: &[i32]) -> f64 {
        assert_eq!(source.len(), reconstructed.len());
        let sse = source
            .iter()
            .zip(reconstructed)
            .map(|(&expected, &actual)| {
                let difference = f64::from(actual) - f64::from(expected);
                difference * difference
            })
            .sum::<f64>();
        let mse = sse / source.len() as f64;
        if mse < 1e-10 {
            100.0
        } else {
            20.0 * (255.0 / mse.sqrt()).log10()
        }
    }

    fn ssim_from_gray(source: &[u8], reconstructed: &[i32], width: usize, height: usize) -> f64 {
        const BLOCK: usize = 8;
        const C1: f64 = (0.01 * 255.0) * (0.01 * 255.0);
        const C2: f64 = (0.03 * 255.0) * (0.03 * 255.0);
        let mut total = 0.0;
        let mut blocks = 0usize;
        for block_y in (0..height / BLOCK).map(|index| index * BLOCK) {
            for block_x in (0..width / BLOCK).map(|index| index * BLOCK) {
                let mut sum_source = 0.0;
                let mut sum_reconstructed = 0.0;
                let mut sum_source_squared = 0.0;
                let mut sum_reconstructed_squared = 0.0;
                let mut sum_product = 0.0;
                for y in block_y..block_y + BLOCK {
                    for x in block_x..block_x + BLOCK {
                        let index = y * width + x;
                        let expected = f64::from(source[index]);
                        let actual = f64::from(reconstructed[index]);
                        sum_source += expected;
                        sum_reconstructed += actual;
                        sum_source_squared += expected * expected;
                        sum_reconstructed_squared += actual * actual;
                        sum_product += expected * actual;
                    }
                }
                let n = (BLOCK * BLOCK) as f64;
                let mean_source = sum_source / n;
                let mean_reconstructed = sum_reconstructed / n;
                let variance_source = (sum_source_squared / n - mean_source * mean_source).max(0.0);
                let variance_reconstructed = (sum_reconstructed_squared / n
                    - mean_reconstructed * mean_reconstructed)
                    .max(0.0);
                let covariance = sum_product / n - mean_source * mean_reconstructed;
                total += ((2.0 * mean_source * mean_reconstructed + C1) * (2.0 * covariance + C2))
                    / ((mean_source * mean_source + mean_reconstructed * mean_reconstructed + C1)
                        * (variance_source + variance_reconstructed + C2));
                blocks += 1;
            }
        }
        total / blocks as f64
    }
}
