//! Coefficient reconstruction, inverse DWT, and color transform for the decoder.

use crate::dwt::norms::band_gain;
use crate::error::{Jp2LamError, Result};
use crate::j2k::decode_markers::{
    CodestreamHeader, QuantizationStep, QuantizationStyle, WaveletTransform,
};
use crate::model::{ColorSpace, Component, Image};
use crate::plan::BandOrientation;
use crate::simd::PRIMITIVES;

use super::stats::StatsSink;
use super::t1::DecodedTileCoefficients;

pub(crate) fn reconstruct_image_profiled(
    header: &CodestreamHeader,
    colorspace: ColorSpace,
    tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Image> {
    match colorspace {
        ColorSpace::Gray => {
            if tiles.len() != 1 {
                return Err(invalid("grayscale JP2 must decode exactly one component"));
            }
            reconstruct_grayscale_image(header, tiles.into_iter().next().unwrap(), stats)
        }
        ColorSpace::Srgb => reconstruct_srgb_image(header, tiles, stats),
        ColorSpace::Cmyk => reconstruct_cmyk_image(header, tiles, stats),
        _ => Err(invalid(format!(
            "unsupported JP2 colorspace for reconstruction: {colorspace:?}"
        ))),
    }
}

/// Reconstruct a 4-component CMYK image. Each plane is reconstructed like a
/// grayscale component; if the stream signals MCT, the inverse color transform
/// is applied to the first three components (K passes through), matching
/// OpenJPEG. The consumer interleaves the four planes as C/M/Y/K.
fn reconstruct_cmyk_image(
    header: &CodestreamHeader,
    mut tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Image> {
    if tiles.len() != 4 || header.siz.components.len() != 4 {
        return Err(invalid("CMYK JP2 must decode exactly four components"));
    }
    tiles.sort_by_key(|tile| tile.component);
    for (idx, tile) in tiles.iter().enumerate() {
        if tile.component != idx {
            return Err(invalid("decoded CMYK components are not contiguous"));
        }
    }

    // Reconstruct all four planes in the centered domain first. If the stream
    // signals MCT, the color transform applies to the first three components
    // (K passes through), exactly as for a 3-component image — so it must run
    // before finalize, like `reconstruct_srgb_image`.
    let planes: Vec<Vec<i32>> = match header.cod.transform {
        WaveletTransform::Reversible53 => {
            let mut centered: Vec<Vec<i32>> = Vec::with_capacity(4);
            for tile in tiles {
                let component = tile.component;
                centered.push(reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    component,
                    stats,
                )?);
            }
            if header.cod.use_mct {
                let start = stats.start();
                inverse_rct_centered(&mut centered[0..3])?;
                record_mct_time(stats, start);
            }
            let start = stats.start();
            let output: Vec<Vec<i32>> = centered
                .into_iter()
                .enumerate()
                .map(|(idx, plane)| {
                    finalize_i32_samples(plane, header.siz.components[idx].precision)
                })
                .collect();
            record_finalize_time(stats, start);
            output
        }
        WaveletTransform::Irreversible97 => {
            let mut centered: Vec<Vec<f32>> = Vec::with_capacity(4);
            for tile in tiles {
                centered.push(reconstruct_irreversible_97_centered(
                    header,
                    tile.into_real()?,
                    stats,
                )?);
            }
            if header.cod.use_mct {
                let start = stats.start();
                inverse_ict_centered(&mut centered[0..3])?;
                record_mct_time(stats, start);
            }
            let start = stats.start();
            let output: Vec<Vec<i32>> = centered
                .into_iter()
                .enumerate()
                .map(|(idx, plane)| {
                    finalize_f32_samples(plane, header.siz.components[idx].precision)
                })
                .collect();
            record_finalize_time(stats, start);
            output
        }
    };

    let width = header.siz.width;
    let height = header.siz.height;
    stats.update(|stats| {
        stats.output_pixels = stats
            .output_pixels
            .saturating_add(planes.iter().map(|plane| plane.len() as u64).sum::<u64>());
    });
    Ok(Image {
        width,
        height,
        colorspace: ColorSpace::Cmyk,
        components: planes
            .into_iter()
            .enumerate()
            .map(|(idx, data)| {
                let component = header.siz.components[idx];
                Component {
                    data,
                    width,
                    height,
                    precision: u32::from(component.precision),
                    signed: component.signed,
                    dx: u32::from(component.dx),
                    dy: u32::from(component.dy),
                }
            })
            .collect(),
    })
}

pub(crate) fn reconstruct_grayscale_image(
    header: &CodestreamHeader,
    tile: DecodedTileCoefficients,
    stats: &mut StatsSink<'_>,
) -> Result<Image> {
    let component = header
        .siz
        .components
        .first()
        .ok_or_else(|| invalid("missing decoded component header"))?;
    let width =
        usize::try_from(header.siz.width).map_err(|_| invalid("decoded width exceeds usize"))?;
    let height =
        usize::try_from(header.siz.height).map_err(|_| invalid("decoded height exceeds usize"))?;
    if tile.component != 0 || tile.width != width || tile.height != height {
        return Err(invalid(
            "decoded coefficient tile dimensions do not match SIZ",
        ));
    }

    let samples = match header.cod.transform {
        WaveletTransform::Reversible53 => {
            let centered =
                reconstruct_reversible_53_centered(header, tile.into_integer()?, 0, stats)?;
            let start = stats.start();
            let output = finalize_i32_samples(centered, component.precision);
            record_finalize_time(stats, start);
            output
        }
        WaveletTransform::Irreversible97 => {
            let centered = reconstruct_irreversible_97_centered(header, tile.into_real()?, stats)?;
            let start = stats.start();
            let output = finalize_f32_samples(centered, component.precision);
            record_finalize_time(stats, start);
            output
        }
    };
    stats.update(|stats| {
        stats.output_pixels = stats.output_pixels.saturating_add(samples.len() as u64);
    });

    Ok(Image {
        width: header.siz.width,
        height: header.siz.height,
        colorspace: ColorSpace::Gray,
        components: vec![Component {
            data: samples,
            width: header.siz.width,
            height: header.siz.height,
            precision: u32::from(component.precision),
            signed: component.signed,
            dx: u32::from(component.dx),
            dy: u32::from(component.dy),
        }],
    })
}

pub(crate) fn reconstruct_packed_u8_profiled(
    header: &CodestreamHeader,
    colorspace: ColorSpace,
    tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<u8>> {
    match colorspace {
        ColorSpace::Gray => reconstruct_grayscale_u8_profiled(header, tiles, stats),
        ColorSpace::Srgb | ColorSpace::Cmyk => {
            reconstruct_interleaved_u8_profiled(header, colorspace, tiles, stats)
        }
        other => Err(invalid(format!(
            "unsupported packed output colorspace: {other:?}"
        ))),
    }
}

fn reconstruct_grayscale_u8_profiled(
    header: &CodestreamHeader,
    mut tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<u8>> {
    if tiles.len() != 1 {
        return Err(invalid("Gray8 output requires exactly one component"));
    }
    let tile = tiles.remove(0);
    let component = header
        .siz
        .components
        .first()
        .ok_or_else(|| invalid("missing decoded component header"))?;
    let width = header.siz.width as usize;
    let height = header.siz.height as usize;
    if tile.component != 0 || tile.width != width || tile.height != height {
        return Err(invalid(
            "decoded coefficient tile dimensions do not match reduced SIZ",
        ));
    }

    let output = match header.cod.transform {
        WaveletTransform::Reversible53 => {
            let centered =
                reconstruct_reversible_53_centered(header, tile.into_integer()?, 0, stats)?;
            let start = stats.start();
            let shift = 1i32 << (component.precision - 1);
            let max_sample = (1i32 << component.precision) - 1;
            let output: Vec<u8> = centered
                .into_iter()
                .map(|sample| {
                    scale_unsigned_to_u8(
                        sample.saturating_add(shift).clamp(0, max_sample) as u32,
                        max_sample as u32,
                    )
                })
                .collect();
            record_finalize_time(stats, start);
            output
        }
        WaveletTransform::Irreversible97 => {
            let centered = reconstruct_irreversible_97_centered(header, tile.into_real()?, stats)?;
            let start = stats.start();
            let shift = (1u32 << (component.precision - 1)) as f32;
            let max_sample = (1u32 << component.precision) - 1;
            let output: Vec<u8> = centered
                .into_iter()
                .map(|sample| {
                    let value = (sample + shift + 0.5).clamp(0.0, max_sample as f32) as u32;
                    scale_unsigned_to_u8(value, max_sample)
                })
                .collect();
            record_finalize_time(stats, start);
            output
        }
    };
    stats.update(|stats| {
        stats.output_pixels = stats.output_pixels.saturating_add(output.len() as u64);
    });
    Ok(output)
}

fn reconstruct_interleaved_u8_profiled(
    header: &CodestreamHeader,
    colorspace: ColorSpace,
    mut tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<u8>> {
    let channels = colorspace.component_count();
    if !matches!(channels, 3 | 4)
        || tiles.len() != channels
        || header.siz.components.len() != channels
    {
        return Err(invalid(format!(
            "{colorspace:?} packed output requires {channels} decoded components"
        )));
    }
    tiles.sort_by_key(|tile| tile.component);
    if tiles
        .iter()
        .enumerate()
        .any(|(index, tile)| tile.component != index)
    {
        return Err(invalid(
            "decoded packed-output components are not contiguous",
        ));
    }
    let pixel_count = header.siz.width as usize * header.siz.height as usize;

    let output = match header.cod.transform {
        WaveletTransform::Reversible53 => {
            let mut planes = Vec::with_capacity(channels);
            for tile in tiles {
                let component = tile.component;
                planes.push(reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    component,
                    stats,
                )?);
            }
            if header.cod.use_mct {
                let start = stats.start();
                inverse_rct_centered(&mut planes[..3])?;
                record_mct_time(stats, start);
            }
            let start = stats.start();
            let mut output = Vec::with_capacity(pixel_count * channels);
            for index in 0..pixel_count {
                for (component, plane) in header.siz.components.iter().zip(&planes) {
                    let shift = 1i32 << (component.precision - 1);
                    let max_sample = (1i32 << component.precision) - 1;
                    let sample = plane[index].saturating_add(shift).clamp(0, max_sample);
                    output.push(scale_unsigned_to_u8(sample as u32, max_sample as u32));
                }
            }
            record_finalize_time(stats, start);
            output
        }
        WaveletTransform::Irreversible97 => {
            let mut planes = Vec::with_capacity(channels);
            for tile in tiles {
                planes.push(reconstruct_irreversible_97_centered(
                    header,
                    tile.into_real()?,
                    stats,
                )?);
            }
            if header.cod.use_mct {
                let start = stats.start();
                inverse_ict_centered(&mut planes[..3])?;
                record_mct_time(stats, start);
            }
            let start = stats.start();
            let mut output = Vec::with_capacity(pixel_count * channels);
            for index in 0..pixel_count {
                for (component, plane) in header.siz.components.iter().zip(&planes) {
                    let shift = (1u32 << (component.precision - 1)) as f32;
                    let max_sample = (1u32 << component.precision) - 1;
                    let sample = (plane[index] + shift + 0.5).clamp(0.0, max_sample as f32) as u32;
                    output.push(scale_unsigned_to_u8(sample, max_sample));
                }
            }
            record_finalize_time(stats, start);
            output
        }
    };
    stats.update(|stats| {
        stats.output_pixels = stats.output_pixels.saturating_add(pixel_count as u64);
    });
    Ok(output)
}

fn scale_unsigned_to_u8(sample: u32, max_sample: u32) -> u8 {
    if max_sample == 255 {
        sample.min(255) as u8
    } else {
        ((u64::from(sample) * 255 + u64::from(max_sample) / 2) / u64::from(max_sample.max(1)))
            .min(255) as u8
    }
}

fn reconstruct_srgb_image(
    header: &CodestreamHeader,
    mut tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Image> {
    if tiles.len() != 3 || header.siz.components.len() != 3 {
        return Err(invalid("sRGB JP2 must decode exactly three components"));
    }
    tiles.sort_by_key(|tile| tile.component);
    for (idx, tile) in tiles.iter().enumerate() {
        if tile.component != idx {
            return Err(invalid("decoded sRGB components are not contiguous"));
        }
    }

    let planes: Vec<Vec<i32>> = match header.cod.transform {
        WaveletTransform::Reversible53 => {
            let mut planes = Vec::with_capacity(3);
            for tile in tiles {
                let component = tile.component;
                planes.push(reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    component,
                    stats,
                )?);
            }
            if header.cod.use_mct {
                let start = stats.start();
                inverse_rct_centered(&mut planes)?;
                record_mct_time(stats, start);
            }
            let start = stats.start();
            let output = planes
                .into_iter()
                .enumerate()
                .map(|(index, plane)| {
                    finalize_i32_samples(plane, header.siz.components[index].precision)
                })
                .collect();
            record_finalize_time(stats, start);
            output
        }
        WaveletTransform::Irreversible97 => {
            let mut planes = Vec::with_capacity(3);
            for tile in tiles {
                planes.push(reconstruct_irreversible_97_centered(
                    header,
                    tile.into_real()?,
                    stats,
                )?);
            }
            if header.cod.use_mct {
                let start = stats.start();
                inverse_ict_centered(&mut planes)?;
                record_mct_time(stats, start);
            }
            let start = stats.start();
            let output = planes
                .into_iter()
                .enumerate()
                .map(|(index, plane)| {
                    finalize_f32_samples(plane, header.siz.components[index].precision)
                })
                .collect();
            record_finalize_time(stats, start);
            output
        }
    };

    let width = header.siz.width;
    let height = header.siz.height;
    stats.update(|stats| {
        stats.output_pixels = stats
            .output_pixels
            .saturating_add(planes.iter().map(|plane| plane.len() as u64).sum::<u64>());
    });
    Ok(Image {
        width,
        height,
        colorspace: ColorSpace::Srgb,
        components: planes
            .into_iter()
            .enumerate()
            .map(|(idx, data)| {
                let component = header.siz.components[idx];
                Component {
                    data,
                    width,
                    height,
                    precision: u32::from(component.precision),
                    signed: component.signed,
                    dx: u32::from(component.dx),
                    dy: u32::from(component.dy),
                }
            })
            .collect(),
    })
}

fn reconstruct_reversible_53_centered(
    header: &CodestreamHeader,
    mut coefficients: Vec<i32>,
    component: usize,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<i32>> {
    if header.quant_for(component).style != QuantizationStyle::NoQuantization {
        return Err(invalid(
            "reversible 5/3 reconstruction expects no quantization",
        ));
    }
    let dwt_start = stats.start();
    if header.siz.x_origin == 0 && header.siz.y_origin == 0 {
        if stats.is_enabled() {
            let timing = crate::dwt::inverse_53_2d_in_place_profiled(
                &mut coefficients,
                header.siz.width as usize,
                header.siz.height as usize,
                header.cod.decomposition_levels,
                PRIMITIVES.backend != "scalar",
            )?;
            record_dwt_breakdown(stats, timing);
        } else {
            (PRIMITIVES.dwt.inverse_53_2d)(
                &mut coefficients,
                header.siz.width as usize,
                header.siz.height as usize,
                header.cod.decomposition_levels,
            )?;
        }
    } else {
        crate::dwt::inverse_53_2d_in_place_at(
            &mut coefficients,
            header.siz.width as usize,
            header.siz.height as usize,
            header.cod.decomposition_levels,
            header.siz.x_origin as usize,
            header.siz.y_origin as usize,
        )?;
    }
    record_dwt_time(stats, dwt_start);
    stats.update(|stats| {
        stats.reconstructed_pixels = stats
            .reconstructed_pixels
            .saturating_add(coefficients.len() as u64);
    });
    Ok(coefficients)
}

fn inverse_ict_centered(planes: &mut [Vec<f32>]) -> Result<()> {
    let [y, cb, cr] = planes else {
        return Err(invalid("ICT requires exactly three components"));
    };
    if y.len() != cb.len() || y.len() != cr.len() {
        return Err(invalid("ICT component lengths differ"));
    }
    (PRIMITIVES.color.inverse_ict)(y, cb, cr);
    Ok(())
}

fn inverse_rct_centered(planes: &mut [Vec<i32>]) -> Result<()> {
    let [y, db, dr] = planes else {
        return Err(invalid("RCT requires exactly three components"));
    };
    if y.len() != db.len() || y.len() != dr.len() {
        return Err(invalid("RCT component lengths differ"));
    }
    (PRIMITIVES.color.inverse_rct)(y, db, dr);
    Ok(())
}

fn reconstruct_irreversible_97_centered(
    header: &CodestreamHeader,
    mut data: Vec<f32>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<f32>> {
    // Dequantization is fused into Tier-1 code-block output (see
    // `t1::dequantize_block_to_tile`): `data` already holds the dequantized
    // `f32` subband samples, so reconstruction goes straight to the inverse DWT
    // with no separate full-image coefficient plane or dequant sweep.
    let width = header.siz.width as usize;
    let height = header.siz.height as usize;

    let dwt_start = stats.start();
    if header.siz.x_origin == 0 && header.siz.y_origin == 0 {
        if stats.is_enabled() {
            let timing = crate::dwt::inverse_97_2d_in_place_profiled(
                &mut data,
                width,
                height,
                header.cod.decomposition_levels,
                PRIMITIVES.backend != "scalar",
            );
            record_dwt_breakdown(stats, timing);
        } else {
            (PRIMITIVES.dwt.inverse_97_2d)(
                &mut data,
                width,
                height,
                header.cod.decomposition_levels,
            )?;
        }
    } else {
        crate::dwt::inverse_97_2d_in_place_at(
            &mut data,
            width,
            height,
            header.cod.decomposition_levels,
            header.siz.x_origin as usize,
            header.siz.y_origin as usize,
        )?;
    }
    record_dwt_time(stats, dwt_start);
    stats.update(|stats| {
        stats.reconstructed_pixels = stats.reconstructed_pixels.saturating_add(data.len() as u64);
    });

    Ok(data)
}

fn finalize_i32_samples(samples: Vec<i32>, precision: u8) -> Vec<i32> {
    let shift = 1i32 << (precision - 1);
    let max_sample = (1i32 << precision) - 1;
    samples
        .into_iter()
        .map(|sample| sample.saturating_add(shift).clamp(0, max_sample))
        .collect()
}

fn finalize_f32_samples(samples: Vec<f32>, precision: u8) -> Vec<i32> {
    let shift = (1u32 << (precision - 1)) as f32;
    let max_sample = ((1u32 << precision) - 1) as f32;
    samples
        .into_iter()
        // The level shift and clamp make the converted domain non-negative,
        // so round-to-nearest is exactly truncation after adding 0.5. This
        // avoids a scalar libm `roundf` call for every reconstructed sample.
        .map(|sample| (sample + shift + 0.5).clamp(0.0, max_sample) as i32)
        .collect()
}

pub(crate) fn quant_step(precision: u32, band: BandOrientation, quant: QuantizationStep) -> f32 {
    let numbps = (precision + u32::from(band_gain(band))) as i32;
    let exponent = i32::from(quant.exponent);
    let base = 1.0 + f32::from(quant.mantissa) / 2048.0;
    (base * 2f32.powi(numbps - exponent)).max(1e-6)
}

fn record_dwt_time(stats: &mut StatsSink<'_>, start: Option<std::time::Instant>) {
    stats.finish(start, |stats, elapsed| {
        stats.dwt_total_ns = stats.dwt_total_ns.saturating_add(elapsed);
    });
}

fn record_dwt_breakdown(stats: &mut StatsSink<'_>, timing: crate::dwt::InverseDwtTiming) {
    stats.update(|stats| {
        stats.dwt_horizontal_ns = stats.dwt_horizontal_ns.saturating_add(timing.horizontal_ns);
        stats.dwt_vertical_ns = stats.dwt_vertical_ns.saturating_add(timing.vertical_ns);
        if stats.dwt_level_ns.len() < timing.level_ns.len() {
            stats.dwt_level_ns.resize(timing.level_ns.len(), 0);
        }
        for (total, elapsed) in stats.dwt_level_ns.iter_mut().zip(timing.level_ns) {
            *total = total.saturating_add(elapsed);
        }
    });
}

fn record_mct_time(stats: &mut StatsSink<'_>, start: Option<std::time::Instant>) {
    stats.finish(start, |stats, elapsed| {
        stats.inverse_mct_ns = stats.inverse_mct_ns.saturating_add(elapsed);
    });
}

fn record_finalize_time(stats: &mut StatsSink<'_>, start: Option<std::time::Instant>) {
    stats.finish(start, |stats, elapsed| {
        stats.finalize_ns = stats.finalize_ns.saturating_add(elapsed);
    });
}

fn invalid(message: impl Into<String>) -> Jp2LamError {
    Jp2LamError::DecodeFailed(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_ict_uses_centered_unclipped_chroma() {
        let mut planes = vec![vec![0.0], vec![80.25], vec![-30.5]];

        inverse_ict_centered(&mut planes).expect("inverse ict");

        assert_eq!(finalize_f32_samples(planes.remove(0), 8), vec![85]);
        assert_eq!(finalize_f32_samples(planes.remove(0), 8), vec![122]);
        assert_eq!(
            finalize_f32_samples(planes.remove(0), 8),
            vec![270_i32.clamp(0, 255)]
        );
    }

    #[test]
    fn inverse_rct_uses_centered_unclipped_differences() {
        let mut planes = vec![vec![10], vec![80], vec![-30]];

        inverse_rct_centered(&mut planes).expect("inverse rct");

        assert_eq!(finalize_i32_samples(planes.remove(0), 8), vec![96]);
        assert_eq!(finalize_i32_samples(planes.remove(0), 8), vec![126]);
        assert_eq!(finalize_i32_samples(planes.remove(0), 8), vec![206]);
    }

    #[test]
    fn irreversible_finalization_rounds_and_clamps_without_libm() {
        assert_eq!(
            finalize_f32_samples(vec![-129.0, -128.5, -127.5, 0.49, 0.5, 127.6, 200.0], 8),
            vec![0, 0, 1, 128, 129, 255, 255]
        );
    }
}
