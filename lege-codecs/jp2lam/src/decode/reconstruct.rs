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
        ColorSpace::YCbCr => reconstruct_ycbcr_image(header, tiles, stats),
        ColorSpace::Cmyk => reconstruct_cmyk_image(header, tiles, stats),
        _ => Err(invalid(format!(
            "unsupported JP2 colorspace for reconstruction: {colorspace:?}"
        ))),
    }
}

/// Reconstruct an sYCC image (EnumCS 18): three Y/Cb/Cr planes coded without
/// MCT, converted to sRGB by the inverse sYCC matrix (ITU-R BT.601 full range,
/// the transform OpenJPEG's `sycc_to_rgb` applies). The result is a DeviceRGB
/// image.
fn reconstruct_ycbcr_image(
    header: &CodestreamHeader,
    mut tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Image> {
    if tiles.len() != 3 || header.siz.components.len() != 3 {
        return Err(invalid("sYCC JP2 must decode exactly three components"));
    }
    tiles.sort_by_key(|tile| tile.component);
    for (idx, tile) in tiles.iter().enumerate() {
        if tile.component != idx {
            return Err(invalid("decoded sYCC components are not contiguous"));
        }
    }
    // `use_mct` is validated false for sYCC, so these are the raw, level-shifted
    // Y/Cb/Cr planes (no colour transform applied).
    let mut planes = reconstruct_color_planes_finalized(header, tiles, stats)?;
    let start = stats.start();
    sycc_to_rgb_in_place(&mut planes, header.siz.components[0].precision);
    record_finalize_time(stats, start);

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

/// In-place inverse sYCC→sRGB on three level-shifted planes (Y, Cb, Cr →
/// R, G, B), ITU-R BT.601 full range with the chroma centred at `2^(prec-1)`.
fn sycc_to_rgb_in_place(planes: &mut [Vec<i32>], precision: u8) {
    let offset = 1i32 << (precision.saturating_sub(1));
    let max_sample = (1i32 << precision) - 1;
    let (y_plane, rest) = planes.split_first_mut().expect("three sYCC planes");
    let (cb_plane, cr_plane) = rest.split_first_mut().expect("three sYCC planes");
    let cr_plane = &mut cr_plane[0];
    for i in 0..y_plane.len() {
        let y = y_plane[i] as f32;
        let cb = (cb_plane[i] - offset) as f32;
        let cr = (cr_plane[i] - offset) as f32;
        let r = y + 1.402 * cr;
        let g = y - 0.344_136 * cb - 0.714_136 * cr;
        let b = y + 1.772 * cb;
        y_plane[i] = (r + 0.5).clamp(0.0, max_sample as f32) as i32;
        cb_plane[i] = (g + 0.5).clamp(0.0, max_sample as f32) as i32;
        cr_plane[i] = (b + 0.5).clamp(0.0, max_sample as f32) as i32;
    }
}

/// Reconstruct a 4-component CMYK image. Each plane is reconstructed like a
/// grayscale component; if the stream signals MCT, the inverse color transform
/// is applied to the first three components (K passes through), matching
/// OpenJPEG. The consumer interleaves the four planes as C/M/Y/K.
/// Reconstruct the MCT colour components (the leading `tiles`, contiguous from
/// component 0) to finalized, precision-clamped `i32` output planes: inverse DWT
/// under the shared COD transform, then the inverse colour transform (RCT/ICT)
/// when signalled. A COC transform override on a colour component is rejected at
/// parse time, so these components always share a transform and a single
/// dispatch is correct.
fn reconstruct_color_planes_finalized(
    header: &CodestreamHeader,
    tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<Vec<i32>>> {
    let apply_mct = header.cod.use_mct && tiles.len() == 3;
    Ok(match header.cod.transform {
        WaveletTransform::Reversible53 => {
            let mut planes = Vec::with_capacity(tiles.len());
            for tile in tiles {
                let component = tile.component;
                planes.push(reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    component,
                    stats,
                )?);
            }
            if apply_mct {
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
            let mut planes = Vec::with_capacity(tiles.len());
            for tile in tiles {
                planes.push(reconstruct_irreversible_97_centered(
                    header,
                    tile.into_real()?,
                    stats,
                )?);
            }
            if apply_mct {
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
    })
}

/// Reconstruct one auxiliary (non-MCT) component to a finalized `i32` output
/// plane under its own, possibly COC-overridden, transform — e.g. a CMYK K
/// channel that a COC codes with 5/3 while the colour components stay 9/7.
fn reconstruct_aux_component_finalized(
    header: &CodestreamHeader,
    tile: DecodedTileCoefficients,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<i32>> {
    let component = tile.component;
    let precision = header.siz.components[component].precision;
    Ok(match header.transform_for(component) {
        WaveletTransform::Irreversible97 => {
            let centered = reconstruct_irreversible_97_centered(header, tile.into_real()?, stats)?;
            finalize_f32_samples(centered, precision)
        }
        WaveletTransform::Reversible53 => {
            let centered =
                reconstruct_reversible_53_centered(header, tile.into_integer()?, component, stats)?;
            finalize_i32_samples(centered, precision)
        }
    })
}

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
    // Components 0-2 are the MCT colour planes (uniform transform); component 3
    // (K) is auxiliary and may carry a COC transform override, so reconstruct it
    // independently. `tiles` is sorted ascending, so the last is component 3.
    let k_tile = tiles
        .pop()
        .ok_or_else(|| invalid("CMYK JP2 must decode exactly four components"))?;
    let mut planes = reconstruct_color_planes_finalized(header, tiles, stats)?;
    planes.push(reconstruct_aux_component_finalized(header, k_tile, stats)?);

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

    let samples = match header.transform_for(0) {
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

/// `channels` is the number of interleaved output planes, taken from the output
/// format rather than `colorspace.component_count()` — an sRGB image with an
/// in-data alpha channel is `Srgb` but produces 4 interleaved planes (RGBA).
pub(crate) fn reconstruct_packed_u8_profiled(
    header: &CodestreamHeader,
    colorspace: ColorSpace,
    channels: usize,
    tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<u8>> {
    match colorspace {
        ColorSpace::Gray => reconstruct_grayscale_u8_profiled(header, tiles, stats),
        ColorSpace::Srgb | ColorSpace::Cmyk => {
            reconstruct_interleaved_u8_profiled(header, colorspace, channels, tiles, stats)
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

    let output = match header.transform_for(0) {
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
    channels: usize,
    mut tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<u8>> {
    let decoded = header.siz.components.len();
    if !matches!(channels, 3 | 4) || tiles.len() != decoded || decoded < channels {
        return Err(invalid(format!(
            "{colorspace:?} packed output requires {channels} of {decoded} decoded components"
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
    // Keep exactly the requested output planes: an sRGB image with an in-data
    // alpha decodes 4 components but yields 3 (Rgb8, alpha dropped) or 4 (Rgba8).
    tiles.truncate(channels);
    let pixel_count = header.siz.width as usize * header.siz.height as usize;

    // Components 0..min(3,channels) are the MCT colour set (uniform transform);
    // any further component (a CMYK K plane, or an sRGB alpha plane) is
    // auxiliary and may carry a COC transform override, so scale it from its own
    // transform. Per-component u8 planes are then interleaved. For a uniform
    // stream this is byte-identical to a single-transform pass.
    let colour_count = channels.min(3);
    let mut tiles_iter = tiles.into_iter();
    let colour_tiles: Vec<DecodedTileCoefficients> = tiles_iter.by_ref().take(colour_count).collect();
    let mut component_u8 = reconstruct_colour_u8(header, colour_tiles, stats)?;
    for tile in tiles_iter {
        component_u8.push(reconstruct_aux_component_u8(header, tile, stats)?);
    }

    let start = stats.start();
    let mut output = Vec::with_capacity(pixel_count * channels);
    for index in 0..pixel_count {
        for plane in &component_u8 {
            output.push(plane[index]);
        }
    }
    record_finalize_time(stats, start);
    stats.update(|stats| {
        stats.output_pixels = stats.output_pixels.saturating_add(pixel_count as u64);
    });
    Ok(output)
}

/// Scale one centered `i32` (reversible 5/3) component to unsigned u8 samples.
fn centered_i32_to_u8(centered: Vec<i32>, precision: u8) -> Vec<u8> {
    let shift = 1i32 << (precision - 1);
    let max_sample = (1i32 << precision) - 1;
    centered
        .into_iter()
        .map(|sample| {
            scale_unsigned_to_u8(
                sample.saturating_add(shift).clamp(0, max_sample) as u32,
                max_sample as u32,
            )
        })
        .collect()
}

/// Scale one centered `f32` (irreversible 9/7) component to unsigned u8 samples.
fn centered_f32_to_u8(centered: Vec<f32>, precision: u8) -> Vec<u8> {
    let shift = (1u32 << (precision - 1)) as f32;
    let max_sample = (1u32 << precision) - 1;
    centered
        .into_iter()
        .map(|sample| {
            let value = (sample + shift + 0.5).clamp(0.0, max_sample as f32) as u32;
            scale_unsigned_to_u8(value, max_sample)
        })
        .collect()
}

/// Reconstruct the MCT colour components to per-component u8 planes: inverse DWT
/// under the shared COD transform, inverse colour transform when signalled (only
/// for a full three-component set), then per-component scaling.
fn reconstruct_colour_u8(
    header: &CodestreamHeader,
    tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<Vec<u8>>> {
    let apply_mct = header.cod.use_mct && tiles.len() == 3;
    Ok(match header.cod.transform {
        WaveletTransform::Reversible53 => {
            let mut planes = Vec::with_capacity(tiles.len());
            for tile in tiles {
                let component = tile.component;
                planes.push(reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    component,
                    stats,
                )?);
            }
            if apply_mct {
                let start = stats.start();
                inverse_rct_centered(&mut planes)?;
                record_mct_time(stats, start);
            }
            planes
                .into_iter()
                .enumerate()
                .map(|(index, plane)| {
                    centered_i32_to_u8(plane, header.siz.components[index].precision)
                })
                .collect()
        }
        WaveletTransform::Irreversible97 => {
            let mut planes = Vec::with_capacity(tiles.len());
            for tile in tiles {
                planes.push(reconstruct_irreversible_97_centered(
                    header,
                    tile.into_real()?,
                    stats,
                )?);
            }
            if apply_mct {
                let start = stats.start();
                inverse_ict_centered(&mut planes)?;
                record_mct_time(stats, start);
            }
            planes
                .into_iter()
                .enumerate()
                .map(|(index, plane)| {
                    centered_f32_to_u8(plane, header.siz.components[index].precision)
                })
                .collect()
        }
    })
}

/// Reconstruct one auxiliary (non-MCT) component to u8 under its own transform.
fn reconstruct_aux_component_u8(
    header: &CodestreamHeader,
    tile: DecodedTileCoefficients,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<u8>> {
    let component = tile.component;
    let precision = header.siz.components[component].precision;
    Ok(match header.transform_for(component) {
        WaveletTransform::Irreversible97 => {
            let centered = reconstruct_irreversible_97_centered(header, tile.into_real()?, stats)?;
            centered_f32_to_u8(centered, precision)
        }
        WaveletTransform::Reversible53 => {
            let centered =
                reconstruct_reversible_53_centered(header, tile.into_integer()?, component, stats)?;
            centered_i32_to_u8(centered, precision)
        }
    })
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
    // An sRGB stream may carry a 4th `cdef` opacity plane (an RGBA image): the
    // three colour planes take the inverse ICT/RCT, and the alpha plane is
    // reconstructed independently (no colour transform) and appended, exactly
    // like a CMYK K plane.
    let ncomp = header.siz.components.len();
    if !matches!(ncomp, 3 | 4) || tiles.len() != ncomp {
        return Err(invalid(
            "sRGB JP2 must decode three colour components (plus an optional alpha)",
        ));
    }
    tiles.sort_by_key(|tile| tile.component);
    for (idx, tile) in tiles.iter().enumerate() {
        if tile.component != idx {
            return Err(invalid("decoded sRGB components are not contiguous"));
        }
    }

    let alpha_tile = if ncomp == 4 { tiles.pop() } else { None };
    let mut planes = reconstruct_color_planes_finalized(header, tiles, stats)?;
    if let Some(alpha) = alpha_tile {
        planes.push(reconstruct_aux_component_finalized(header, alpha, stats)?);
    }

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
    fn sycc_to_rgb_matches_bt601_full_range() {
        // Neutral chroma (Cb=Cr=128) is a pure grayscale ramp: R=G=B=Y exactly
        // (the transform OpenJPEG's sycc_to_rgb applies). The BT.601 primaries
        // round-trip to a dominant channel with the others near zero.
        let mut planes = vec![
            vec![0, 128, 255, 76, 150, 29],    // Y
            vec![128, 128, 128, 85, 44, 255],  // Cb
            vec![128, 128, 128, 255, 21, 107], // Cr
        ];
        sycc_to_rgb_in_place(&mut planes, 8);
        let px = |i: usize| (planes[0][i], planes[1][i], planes[2][i]);
        assert_eq!(px(0), (0, 0, 0));
        assert_eq!(px(1), (128, 128, 128));
        assert_eq!(px(2), (255, 255, 255));
        // Red / green / blue primaries: the named channel saturates, the others
        // stay ≤ 1 (sub-LSB from the forward Y quantization).
        let (r, g, b) = px(3);
        assert!(r >= 254 && g <= 1 && b <= 1, "red primary: {:?}", px(3));
        let (r, g, b) = px(4);
        assert!(g >= 254 && r <= 1 && b <= 1, "green primary: {:?}", px(4));
        let (r, g, b) = px(5);
        assert!(b >= 254 && r <= 1 && g <= 1, "blue primary: {:?}", px(5));
    }

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
