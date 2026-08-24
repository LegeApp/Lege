//! Coefficient reconstruction, inverse DWT, and color transform for the decoder.

use crate::dwt::norms::band_gain;
use crate::error::{Jp2LamError, Result};
use crate::j2k::decode_markers::{
    CodestreamHeader, QuantizationStep, QuantizationStyle, WaveletTransform,
};
use crate::model::{ColorSpace, Component, Image};
use crate::plan::BandOrientation;
use crate::simd::PRIMITIVES;
use rayon::prelude::*;

use super::stats::StatsSink;
use super::t1::DecodedTileCoefficients;

/// Parallelize fused pack when the plane has at least this many samples.
const FUSED_PACK_PARALLEL_SAMPLES: usize = 256 * 1024;

pub(crate) fn reconstruct_image_profiled(
    header: &CodestreamHeader,
    colorspace: ColorSpace,
    tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Image> {
    match colorspace {
        ColorSpace::Gray => {
            // Gray+alpha carries a second `cdef` opacity plane; the planar
            // output keeps it as a trailing component, exactly like the sRGB
            // RGBA case, and the PDF layer decides whether to apply it.
            if !matches!(tiles.len(), 1 | 2) {
                return Err(invalid(
                    "grayscale JP2 must decode one component (plus an optional alpha)",
                ));
            }
            reconstruct_grayscale_image_with_alpha(header, tiles, stats)
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
    // Y/Cb/Cr planes (no colour transform applied). Each plane is reconstructed
    // at its own tile-component resolution, so a 4:2:0 chroma plane comes back
    // half-width and half-height and must be upsampled to full resolution before
    // the colour transform.
    let full_width = header.siz.width as usize;
    let full_height = header.siz.height as usize;
    // Capture per-component sample dims before the tiles are consumed.
    let component_dims = (0..header.siz.components.len())
        .map(|c| header.tile_component_dims(c))
        .collect::<Result<Vec<_>>>()?;
    let mut planes = reconstruct_color_planes_finalized(header, tiles, stats)?;
    let start = stats.start();
    // Nearest-neighbour (2x2 replication) chroma upsampling, matching OpenJPEG's
    // `sycc420_to_rgb` for images whose origin is (0, 0) (validated). Each chroma
    // sample fills the `dx`x`dy` block of full-resolution pixels anchored at its
    // top-left, exactly the samples OpenJPEG feeds to `sycc_to_rgb` per pixel.
    for c in 1..planes.len() {
        let comp = header.siz.components[c];
        if comp.dx != 1 || comp.dy != 1 {
            let (cw, ch) = component_dims[c];
            planes[c] = upsample_chroma_nearest(
                &planes[c],
                cw,
                ch,
                full_width,
                full_height,
                usize::from(comp.dx.max(1)),
                usize::from(comp.dy.max(1)),
            );
        }
    }
    sycc_to_rgb_in_place(&mut planes, header.siz.components[0].precision);
    record_finalize_time(stats, start);

    let width = header.siz.width;
    let height = header.siz.height;
    stats.update(|stats| {
        stats.output_pixels = stats
            .output_pixels
            .saturating_add(planes.iter().map(|plane| plane.len() as u64).sum::<u64>());
    });
    // After upsampling + colour transform every plane is a full-resolution sRGB
    // channel, so the output components are all 1:1 (like OpenJPEG resetting
    // `comps[1].dx = comps[0].dx` after `sycc420_to_rgb`).
    let precision = u32::from(header.siz.components[0].precision);
    let signed = header.siz.components[0].signed;
    Ok(Image {
        width,
        height,
        colorspace: ColorSpace::Srgb,
        components: planes
            .into_iter()
            .map(|data| Component {
                data,
                width,
                height,
                precision,
                signed,
                dx: 1,
                dy: 1,
            })
            .collect(),
    })
}

/// Upsample a subsampled chroma plane to full resolution by nearest-neighbour
/// (block) replication: the sample at chroma coordinate `(cx, cy)` fills the
/// `dx`×`dy` block of output pixels whose top-left is `(cx*dx, cy*dy)`. For a
/// (0, 0)-origin image this reproduces OpenJPEG's `sycc420_to_rgb` /
/// `sycc422_to_rgb` chroma selection exactly (same replicated Cb/Cr fed to each
/// output pixel), including odd trailing rows/columns, because `cw = ceil(w/dx)`
/// and `ch = ceil(h/dy)` keep every `x/dx < cw` and `y/dy < ch`.
fn upsample_chroma_nearest(
    src: &[i32],
    cw: usize,
    ch: usize,
    full_width: usize,
    full_height: usize,
    dx: usize,
    dy: usize,
) -> Vec<i32> {
    debug_assert_eq!(src.len(), cw * ch);
    let mut out = vec![0i32; full_width * full_height];
    for y in 0..full_height {
        let cy = (y / dy).min(ch.saturating_sub(1));
        let src_row = &src[cy * cw..cy * cw + cw];
        let out_row = &mut out[y * full_width..y * full_width + full_width];
        for (x, dst) in out_row.iter_mut().enumerate() {
            let cx = (x / dx).min(cw.saturating_sub(1));
            *dst = src_row[cx];
        }
    }
    out
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
        // Match OpenJPEG's `sycc_to_rgb` bit-for-bit (src/bin/common/color.c):
        // integer luma with the chroma term cast `(int)` (truncation toward
        // zero) using the exact coefficients 1.402 / 0.344 / 0.714 / 1.772, then
        // clamped to [0, 2^prec - 1]. Round-to-nearest or the more precise
        // 0.344136/0.714136 constants would diverge from the reference by 1-2
        // codes at chroma-heavy pixels.
        let y = y_plane[i];
        let cb = cb_plane[i] - offset;
        let cr = cr_plane[i] - offset;
        let r = y + (1.402 * cr as f32) as i32;
        let g = y - (0.344 * cb as f32 + 0.714 * cr as f32) as i32;
        let b = y + (1.772 * cb as f32) as i32;
        y_plane[i] = r.clamp(0, max_sample);
        cb_plane[i] = g.clamp(0, max_sample);
        cr_plane[i] = b.clamp(0, max_sample);
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
                let (w, h) = (tile.width, tile.height);
                planes.push(reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    component,
                    w,
                    h,
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
                let (w, h) = (tile.width, tile.height);
                planes.push(reconstruct_irreversible_97_centered(
                    header,
                    tile.into_real()?,
                    w,
                    h,
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
    let (w, h) = (tile.width, tile.height);
    let precision = header.siz.components[component].precision;
    Ok(match header.transform_for(component) {
        WaveletTransform::Irreversible97 => {
            let centered =
                reconstruct_irreversible_97_centered(header, tile.into_real()?, w, h, stats)?;
            finalize_f32_samples(centered, precision)
        }
        WaveletTransform::Reversible53 => {
            let centered = reconstruct_reversible_53_centered(
                header,
                tile.into_integer()?,
                component,
                w,
                h,
                stats,
            )?;
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

/// Reconstruct a grayscale image that may carry a trailing `cdef` opacity
/// plane (Gray+alpha, `ihdr` component count 2).
///
/// The single colour plane takes the normal grayscale path; the alpha plane is
/// reconstructed independently under its own transform (no colour transform)
/// and appended, exactly like a CMYK `K` plane or an sRGB alpha plane.
fn reconstruct_grayscale_image_with_alpha(
    header: &CodestreamHeader,
    mut tiles: Vec<DecodedTileCoefficients>,
    stats: &mut StatsSink<'_>,
) -> Result<Image> {
    tiles.sort_by_key(|tile| tile.component);
    for (idx, tile) in tiles.iter().enumerate() {
        if tile.component != idx {
            return Err(invalid("decoded grayscale components are not contiguous"));
        }
    }
    let alpha_tile = if tiles.len() == 2 { tiles.pop() } else { None };
    let gray = tiles
        .pop()
        .ok_or_else(|| invalid("missing decoded grayscale component"))?;
    let mut image = reconstruct_grayscale_image(header, gray, stats)?;
    if let Some(alpha) = alpha_tile {
        let plane = reconstruct_aux_component_finalized(header, alpha, stats)?;
        let component = header
            .siz
            .components
            .get(1)
            .ok_or_else(|| invalid("missing alpha component header"))?;
        image.components.push(Component {
            data: plane,
            width: image.width,
            height: image.height,
            precision: u32::from(component.precision),
            signed: component.signed,
            dx: u32::from(component.dx),
            dy: u32::from(component.dy),
        });
    }
    Ok(image)
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
            let centered = reconstruct_reversible_53_centered(
                header,
                tile.into_integer()?,
                0,
                width,
                height,
                stats,
            )?;
            let start = stats.start();
            let output = finalize_i32_samples(centered, component.precision);
            record_finalize_time(stats, start);
            output
        }
        WaveletTransform::Irreversible97 => {
            let centered = reconstruct_irreversible_97_centered(
                header,
                tile.into_real()?,
                width,
                height,
                stats,
            )?;
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

/// How reconstructed colour (and optional alpha) planes are interleaved into
/// the packed destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackedLayout {
    /// Planes in natural order: Gray, RGB, RGBA, CMYK, GrayA.
    Sequential,
    /// R, G, B, constant 255 (opaque pad; no alpha plane required).
    Rgbx,
    /// B, G, R, A — A is the opacity plane when present, otherwise 255.
    Bgra,
}

impl PackedLayout {
    pub(crate) fn from_format(format: super::DecodeOutputFormat) -> Self {
        match format {
            super::DecodeOutputFormat::Rgbx8 => Self::Rgbx,
            super::DecodeOutputFormat::Bgra8 => Self::Bgra,
            _ => Self::Sequential,
        }
    }
}

/// Destination for packed reconstruction: a rectangle inside a strided buffer.
///
/// Pixel `(source_x + x, source_y + y)` of the reconstructed tile is written
/// at canvas pixel `(origin_x + x, origin_y + y)`. A full-tile write uses zero
/// source offsets; ROI decode uses a clipped source rectangle while retaining
/// the full reconstructed planes required by the current whole-tile IDWT.
#[derive(Debug)]
pub(crate) struct PackedWriteTarget<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
    pub channels: usize,
    pub origin_x: usize,
    pub origin_y: usize,
    pub source_x: usize,
    pub source_y: usize,
    pub width: usize,
    pub height: usize,
    pub layout: PackedLayout,
}

impl PackedWriteTarget<'_> {
    fn validate(&self) -> Result<()> {
        if self.channels == 0 || self.width == 0 || self.height == 0 {
            return Err(invalid("packed write target has zero extent"));
        }
        let origin_bytes = self
            .origin_x
            .checked_mul(self.channels)
            .ok_or_else(|| invalid("packed write origin overflow"))?;
        let row_bytes = self
            .width
            .checked_mul(self.channels)
            .ok_or_else(|| invalid("packed write row overflow"))?;
        let end_bytes = origin_bytes
            .checked_add(row_bytes)
            .ok_or_else(|| invalid("packed write stride overflow"))?;
        if self.stride < end_bytes {
            return Err(invalid(format!(
                "packed write stride {} too small for origin_x={} width={} channels={}",
                self.stride, self.origin_x, self.width, self.channels
            )));
        }
        let last_row = self
            .origin_y
            .checked_add(self.height - 1)
            .ok_or_else(|| invalid("packed write height overflow"))?;
        let need_len = last_row
            .checked_mul(self.stride)
            .and_then(|offset| offset.checked_add(end_bytes))
            .ok_or_else(|| invalid("packed write buffer size overflow"))?;
        if self.data.len() < need_len {
            return Err(invalid(format!(
                "packed write buffer len {} < required {need_len}",
                self.data.len()
            )));
        }
        Ok(())
    }

    #[inline]
    fn pixel_offset(&self, x: usize, y: usize) -> usize {
        (self.origin_y + y) * self.stride + (self.origin_x + x) * self.channels
    }

    #[inline]
    fn source_index(&self, x: usize, y: usize, source_stride: usize) -> usize {
        (self.source_y + y) * source_stride + self.source_x + x
    }
}

/// Reconstruct packed samples into an existing strided buffer rectangle.
pub(crate) fn reconstruct_packed_u8_into(
    header: &CodestreamHeader,
    colorspace: ColorSpace,
    channels: usize,
    colour_channels: usize,
    layout: PackedLayout,
    tiles: Vec<DecodedTileCoefficients>,
    target: PackedWriteTarget<'_>,
    stats: &mut StatsSink<'_>,
) -> Result<()> {
    target.validate()?;
    if target.channels != channels || target.layout != layout {
        return Err(invalid("packed write target channels/layout mismatch"));
    }
    let source_width = header.siz.width as usize;
    let source_height = header.siz.height as usize;
    let source_x1 = target
        .source_x
        .checked_add(target.width)
        .ok_or_else(|| invalid("packed write source width overflow"))?;
    let source_y1 = target
        .source_y
        .checked_add(target.height)
        .ok_or_else(|| invalid("packed write source height overflow"))?;
    if source_x1 > source_width || source_y1 > source_height {
        return Err(invalid("packed write source rectangle exceeds reduced tile SIZ"));
    }
    match colorspace {
        ColorSpace::Gray if channels == 1 => {
            reconstruct_grayscale_u8_into(header, tiles, target, stats)
        }
        ColorSpace::Gray | ColorSpace::Srgb | ColorSpace::Cmyk => reconstruct_interleaved_u8_into(
            header,
            colorspace,
            channels,
            colour_channels,
            layout,
            tiles,
            target,
            stats,
        ),
        ColorSpace::YCbCr => {
            reconstruct_sycc_packed_into(header, channels, layout, tiles, target, stats)
        }
        other => Err(invalid(format!(
            "unsupported packed output colorspace: {other:?}"
        ))),
    }
}

fn reconstruct_grayscale_u8_into(
    header: &CodestreamHeader,
    mut tiles: Vec<DecodedTileCoefficients>,
    mut target: PackedWriteTarget<'_>,
    stats: &mut StatsSink<'_>,
) -> Result<()> {
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
    if target.channels != 1 {
        return Err(invalid("gray packed write target size/channels mismatch"));
    }

    let start = stats.start();
    match header.transform_for(0) {
        WaveletTransform::Reversible53 => {
            let centered = reconstruct_reversible_53_centered(
                header,
                tile.into_integer()?,
                0,
                width,
                height,
                stats,
            )?;
            let shift = 1i32 << (component.precision - 1);
            let max_sample = (1i32 << component.precision) - 1;
            write_gray_plane_i32(&centered, width, shift, max_sample as u32, &mut target);
        }
        WaveletTransform::Irreversible97 => {
            let centered = reconstruct_irreversible_97_centered(
                header,
                tile.into_real()?,
                width,
                height,
                stats,
            )?;
            let shift = (1u32 << (component.precision - 1)) as f32;
            let max_sample = (1u32 << component.precision) - 1;
            write_gray_plane_f32(&centered, width, shift, max_sample, &mut target);
        }
    }
    record_finalize_time(stats, start);
    stats.update(|stats| {
        stats.output_pixels = stats
            .output_pixels
            .saturating_add((target.width * target.height) as u64);
    });
    Ok(())
}

fn write_gray_plane_i32(
    samples: &[i32],
    source_width: usize,
    shift: i32,
    max_sample: u32,
    target: &mut PackedWriteTarget<'_>,
) {
    // target is mut for exclusive write access to data.
    let w = target.width;
    let h = target.height;
    for y in 0..h {
        for x in 0..w {
            let sample = samples[target.source_index(x, y, source_width)];
            let byte = scale_unsigned_to_u8(
                sample.saturating_add(shift).clamp(0, max_sample as i32) as u32,
                max_sample,
            );
            let off = target.pixel_offset(x, y);
            target.data[off] = byte;
        }
    }
}

fn write_gray_plane_f32(
    samples: &[f32],
    source_width: usize,
    shift: f32,
    max_sample: u32,
    target: &mut PackedWriteTarget<'_>,
) {
    let w = target.width;
    let h = target.height;
    for y in 0..h {
        for x in 0..w {
            let sample = samples[target.source_index(x, y, source_width)];
            let value = (sample + shift + 0.5).clamp(0.0, max_sample as f32) as u32;
            let byte = scale_unsigned_to_u8(value, max_sample);
            let off = target.pixel_offset(x, y);
            target.data[off] = byte;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reconstruct_interleaved_u8_into(
    header: &CodestreamHeader,
    colorspace: ColorSpace,
    channels: usize,
    colour_channels: usize,
    layout: PackedLayout,
    mut tiles: Vec<DecodedTileCoefficients>,
    mut target: PackedWriteTarget<'_>,
    stats: &mut StatsSink<'_>,
) -> Result<()> {
    let decoded = header.siz.components.len();
    if !matches!(channels, 2 | 3 | 4) || tiles.len() != decoded || decoded < colour_channels {
        return Err(invalid(format!(
            "{colorspace:?} packed output needs ≥{colour_channels} colour components of {decoded} decoded (channels={channels})"
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
    let keep = match layout {
        PackedLayout::Rgbx => colour_channels,
        PackedLayout::Bgra => decoded.min(colour_channels + 1),
        PackedLayout::Sequential => channels.min(decoded),
    };
    tiles.truncate(keep);
    let source_width = header.siz.width as usize;
    let pixel_count = source_width * header.siz.height as usize;
    let colour_count = colour_channels.min(tiles.len());

    if can_fuse_packed(header, colour_count, tiles.len(), layout, channels) {
        return reconstruct_fused_packed_into(
            header,
            channels,
            colour_count,
            layout,
            tiles,
            pixel_count,
            &mut target,
            stats,
        );
    }

    let mut tiles_iter = tiles.into_iter();
    let colour_tiles: Vec<DecodedTileCoefficients> =
        tiles_iter.by_ref().take(colour_count).collect();
    let mut component_u8 = reconstruct_colour_u8(header, colour_tiles, stats)?;
    for tile in tiles_iter {
        component_u8.push(reconstruct_aux_component_u8(header, tile, stats)?);
    }

    let start = stats.start();
    let w = target.width;
    let h = target.height;
    for y in 0..h {
        for x in 0..w {
            let index = target.source_index(x, y, source_width);
            let off = target.pixel_offset(x, y);
            match layout {
                PackedLayout::Sequential => {
                    for (ci, plane) in component_u8.iter().enumerate() {
                        target.data[off + ci] = plane[index];
                    }
                }
                PackedLayout::Rgbx => {
                    target.data[off] = component_u8[0][index];
                    target.data[off + 1] = component_u8[1][index];
                    target.data[off + 2] = component_u8[2][index];
                    target.data[off + 3] = 255;
                }
                PackedLayout::Bgra => {
                    let a = if component_u8.len() > 3 {
                        component_u8[3][index]
                    } else {
                        255
                    };
                    target.data[off] = component_u8[2][index];
                    target.data[off + 1] = component_u8[1][index];
                    target.data[off + 2] = component_u8[0][index];
                    target.data[off + 3] = a;
                }
            }
        }
    }
    record_finalize_time(stats, start);
    stats.update(|stats| {
        stats.output_pixels = stats
            .output_pixels
            .saturating_add((target.width * target.height) as u64);
    });
    Ok(())
}

/// True when every kept component is 8-bit and the layout is one of the fused
/// RGB-family packers (including optional single auxiliary plane).
fn can_fuse_packed(
    header: &CodestreamHeader,
    colour_count: usize,
    kept: usize,
    layout: PackedLayout,
    channels: usize,
) -> bool {
    if colour_count != 3 || !matches!(channels, 3 | 4) {
        return false;
    }
    if kept > colour_count + 1 {
        return false;
    }
    // Sequential 4-channel with only 3 kept tiles is Rgb8 dropping alpha — fine.
    // Rgbx always keeps 3. Bgra may keep 3 or 4.
    match layout {
        PackedLayout::Sequential if channels == 4 && kept < 4 => {
            // Would need a synthetic alpha for sequential RGBA without a plane —
            // that case is rejected upstream for Rgba8; Cmyk always has 4 tiles.
            return false;
        }
        PackedLayout::Sequential | PackedLayout::Rgbx | PackedLayout::Bgra => {}
    }
    header
        .siz
        .components
        .iter()
        .take(kept)
        .all(|c| c.precision == 8)
}

/// Inverse DWT → fused inverse MCT (if any) + level-shift + pack into the
/// destination layout without allocating full per-component u8 planes.
fn reconstruct_fused_packed_into(
    header: &CodestreamHeader,
    channels: usize,
    colour_count: usize,
    layout: PackedLayout,
    tiles: Vec<DecodedTileCoefficients>,
    pixel_count: usize,
    target: &mut PackedWriteTarget<'_>,
    stats: &mut StatsSink<'_>,
) -> Result<()> {
    debug_assert_eq!(colour_count, 3);
    let apply_mct = header.cod.use_mct && colour_count == 3;
    let mut tiles_iter = tiles.into_iter();
    let colour_tiles: Vec<DecodedTileCoefficients> =
        tiles_iter.by_ref().take(colour_count).collect();
    let aux_tile = tiles_iter.next();

    match header.cod.transform {
        WaveletTransform::Irreversible97 => {
            let mut planes = Vec::with_capacity(3);
            for tile in colour_tiles {
                let (w, h) = (tile.width, tile.height);
                planes.push(reconstruct_irreversible_97_centered(
                    header,
                    tile.into_real()?,
                    w,
                    h,
                    stats,
                )?);
            }
            let aux_u8 = match aux_tile {
                Some(tile) => Some(reconstruct_aux_component_u8(header, tile, stats)?),
                None => None,
            };
            let start = stats.start();
            fuse_pack_f32_into(
                &planes[0],
                &planes[1],
                &planes[2],
                aux_u8.as_deref(),
                apply_mct,
                layout,
                channels,
                pixel_count,
                header.siz.width as usize,
                target,
            )?;
            record_finalize_time(stats, start);
        }
        WaveletTransform::Reversible53 => {
            let mut planes = Vec::with_capacity(3);
            for tile in colour_tiles {
                let component = tile.component;
                let (w, h) = (tile.width, tile.height);
                planes.push(reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    component,
                    w,
                    h,
                    stats,
                )?);
            }
            let aux_u8 = match aux_tile {
                Some(tile) => Some(reconstruct_aux_component_u8(header, tile, stats)?),
                None => None,
            };
            let start = stats.start();
            fuse_pack_i32_into(
                &planes[0],
                &planes[1],
                &planes[2],
                aux_u8.as_deref(),
                apply_mct,
                layout,
                channels,
                pixel_count,
                header.siz.width as usize,
                target,
            )?;
            record_finalize_time(stats, start);
        }
    }

    stats.update(|stats| {
        stats.output_pixels = stats
            .output_pixels
            .saturating_add((target.width * target.height) as u64);
    });
    Ok(())
}

#[inline]
fn finalize_centered_f32_u8(sample: f32) -> u8 {
    // Matches `centered_f32_to_u8` for precision 8: floor(x + 0.5) via +0.5 then trunc.
    (sample + 128.0 + 0.5).clamp(0.0, 255.0) as u32 as u8
}

#[inline]
fn finalize_centered_i32_u8(sample: i32) -> u8 {
    sample.saturating_add(128).clamp(0, 255) as u8
}

#[inline]
fn store_rgb_layout(
    out: &mut [u8],
    layout: PackedLayout,
    channels: usize,
    r: u8,
    g: u8,
    b: u8,
    a: u8,
) {
    match layout {
        PackedLayout::Sequential if channels == 3 => {
            out[0] = r;
            out[1] = g;
            out[2] = b;
        }
        PackedLayout::Sequential if channels == 4 => {
            out[0] = r;
            out[1] = g;
            out[2] = b;
            out[3] = a;
        }
        PackedLayout::Rgbx => {
            out[0] = r;
            out[1] = g;
            out[2] = b;
            out[3] = 255;
        }
        PackedLayout::Bgra => {
            out[0] = b;
            out[1] = g;
            out[2] = r;
            out[3] = a;
        }
        _ => {
            // Defensive: write as many leading RGB bytes as the channel count allows.
            if !out.is_empty() {
                out[0] = r;
            }
            if out.len() > 1 {
                out[1] = g;
            }
            if out.len() > 2 {
                out[2] = b;
            }
            if out.len() > 3 {
                out[3] = a;
            }
        }
    }
}

fn ict_rgb_f32(y: f32, cb: f32, cr: f32) -> (u8, u8, u8) {
    let rf = y + 1.402f32 * cr;
    let gf = y - 0.344_13f32 * cb - 0.714_14f32 * cr;
    let bf = y + 1.772f32 * cb;
    (
        finalize_centered_f32_u8(rf),
        finalize_centered_f32_u8(gf),
        finalize_centered_f32_u8(bf),
    )
}

fn rct_rgb_i32(y: i32, db: i32, dr: i32) -> (u8, u8, u8) {
    let g = y - ((db + dr) >> 2);
    let r = dr + g;
    let b = db + g;
    (
        finalize_centered_i32_u8(r),
        finalize_centered_i32_u8(g),
        finalize_centered_i32_u8(b),
    )
}

fn fuse_pack_f32_into(
    y: &[f32],
    cb: &[f32],
    cr: &[f32],
    aux: Option<&[u8]>,
    apply_ict: bool,
    layout: PackedLayout,
    channels: usize,
    pixel_count: usize,
    plane_width: usize,
    target: &mut PackedWriteTarget<'_>,
) -> Result<()> {
    if y.len() != pixel_count || cb.len() != pixel_count || cr.len() != pixel_count {
        return Err(invalid("fused pack plane length mismatch"));
    }
    if let Some(a) = aux {
        if a.len() != pixel_count {
            return Err(invalid("fused pack alpha length mismatch"));
        }
    }
    let w = target.width;
    let h = target.height;
    let tight = target.source_x == 0
        && target.source_y == 0
        && w * h == pixel_count
        && target.origin_x == 0
        && target.origin_y == 0
        && target.stride == w * channels
        && pixel_count >= FUSED_PACK_PARALLEL_SAMPLES;

    if tight {
        // Contiguous destination: coarse parallel pixel jobs.
        let pixels_per_job = (pixel_count / rayon::current_num_threads().max(1) / 2).max(1024);
        let active = pixel_count * channels;
        target.data[..active]
            .par_chunks_mut(pixels_per_job * channels)
            .enumerate()
            .for_each(|(job, chunk)| {
                let base = job * pixels_per_job;
                let n = chunk.len() / channels;
                for x in 0..n {
                    let i = base + x;
                    let (r, g, b) = if apply_ict {
                        ict_rgb_f32(y[i], cb[i], cr[i])
                    } else {
                        (
                            finalize_centered_f32_u8(y[i]),
                            finalize_centered_f32_u8(cb[i]),
                            finalize_centered_f32_u8(cr[i]),
                        )
                    };
                    let a = aux.map(|p| p[i]).unwrap_or(255);
                    store_rgb_layout(
                        &mut chunk[x * channels..(x + 1) * channels],
                        layout,
                        channels,
                        r,
                        g,
                        b,
                        a,
                    );
                }
            });
    } else {
        for py in 0..h {
            for px in 0..w {
                let i = target.source_index(px, py, plane_width);
                let (r, g, b) = if apply_ict {
                    ict_rgb_f32(y[i], cb[i], cr[i])
                } else {
                    (
                        finalize_centered_f32_u8(y[i]),
                        finalize_centered_f32_u8(cb[i]),
                        finalize_centered_f32_u8(cr[i]),
                    )
                };
                let a = aux.map(|p| p[i]).unwrap_or(255);
                let off = target.pixel_offset(px, py);
                store_rgb_layout(
                    &mut target.data[off..off + channels],
                    layout,
                    channels,
                    r,
                    g,
                    b,
                    a,
                );
            }
        }
    }
    Ok(())
}

fn fuse_pack_i32_into(
    y: &[i32],
    db: &[i32],
    dr: &[i32],
    aux: Option<&[u8]>,
    apply_rct: bool,
    layout: PackedLayout,
    channels: usize,
    pixel_count: usize,
    plane_width: usize,
    target: &mut PackedWriteTarget<'_>,
) -> Result<()> {
    if y.len() != pixel_count || db.len() != pixel_count || dr.len() != pixel_count {
        return Err(invalid("fused pack plane length mismatch"));
    }
    if let Some(a) = aux {
        if a.len() != pixel_count {
            return Err(invalid("fused pack alpha length mismatch"));
        }
    }
    let w = target.width;
    let h = target.height;
    let tight = target.source_x == 0
        && target.source_y == 0
        && w * h == pixel_count
        && target.origin_x == 0
        && target.origin_y == 0
        && target.stride == w * channels
        && pixel_count >= FUSED_PACK_PARALLEL_SAMPLES;

    if tight {
        let pixels_per_job = (pixel_count / rayon::current_num_threads().max(1) / 2).max(1024);
        let active = pixel_count * channels;
        target.data[..active]
            .par_chunks_mut(pixels_per_job * channels)
            .enumerate()
            .for_each(|(job, chunk)| {
                let base = job * pixels_per_job;
                let n = chunk.len() / channels;
                for x in 0..n {
                    let i = base + x;
                    let (r, g, b) = if apply_rct {
                        rct_rgb_i32(y[i], db[i], dr[i])
                    } else {
                        (
                            finalize_centered_i32_u8(y[i]),
                            finalize_centered_i32_u8(db[i]),
                            finalize_centered_i32_u8(dr[i]),
                        )
                    };
                    let a = aux.map(|p| p[i]).unwrap_or(255);
                    store_rgb_layout(
                        &mut chunk[x * channels..(x + 1) * channels],
                        layout,
                        channels,
                        r,
                        g,
                        b,
                        a,
                    );
                }
            });
    } else {
        for py in 0..h {
            for px in 0..w {
                let i = target.source_index(px, py, plane_width);
                let (r, g, b) = if apply_rct {
                    rct_rgb_i32(y[i], db[i], dr[i])
                } else {
                    (
                        finalize_centered_i32_u8(y[i]),
                        finalize_centered_i32_u8(db[i]),
                        finalize_centered_i32_u8(dr[i]),
                    )
                };
                let a = aux.map(|p| p[i]).unwrap_or(255);
                let off = target.pixel_offset(px, py);
                store_rgb_layout(
                    &mut target.data[off..off + channels],
                    layout,
                    channels,
                    r,
                    g,
                    b,
                    a,
                );
            }
        }
    }
    Ok(())
}

/// 4:2:0 sYCC → packed sRGB without expanding chroma to full resolution.
///
/// Planes are finalized (level-shifted) at native component resolution; chroma
/// is nearest-neighbour sampled at `(x/dx, y/dy)` while applying OpenJPEG's
/// `sycc_to_rgb` integer formula.
fn reconstruct_sycc_packed_into(
    header: &CodestreamHeader,
    channels: usize,
    layout: PackedLayout,
    mut tiles: Vec<DecodedTileCoefficients>,
    mut target: PackedWriteTarget<'_>,
    stats: &mut StatsSink<'_>,
) -> Result<()> {
    let _ = channels; // used via target.channels / store_rgb_layout
    if tiles.len() != 3 || header.siz.components.len() != 3 {
        return Err(invalid("sYCC packed output requires three components"));
    }
    if !matches!(channels, 3 | 4) {
        return Err(invalid("sYCC packed output requires RGB-family channels"));
    }
    tiles.sort_by_key(|tile| tile.component);
    for (idx, tile) in tiles.iter().enumerate() {
        if tile.component != idx {
            return Err(invalid("decoded sYCC components are not contiguous"));
        }
    }
    let full_w = header.siz.width as usize;
    let full_h = header.siz.height as usize;
    let precision = header.siz.components[0].precision;
    if precision != 8 || header.siz.components.iter().any(|c| c.precision != 8) {
        return Err(invalid(
            "packed sYCC path currently requires 8-bit components",
        ));
    }
    let dx1 = usize::from(header.siz.components[1].dx.max(1));
    let dy1 = usize::from(header.siz.components[1].dy.max(1));
    let dx2 = usize::from(header.siz.components[2].dx.max(1));
    let dy2 = usize::from(header.siz.components[2].dy.max(1));
    if dx1 != dx2 || dy1 != dy2 {
        return Err(invalid("sYCC chroma sample factors differ"));
    }

    // Finalize each component at native resolution (no full-size chroma expand).
    let mut native: Vec<(Vec<i32>, usize, usize)> = Vec::with_capacity(3);
    for tile in tiles {
        let c = tile.component;
        let (cw, ch) = (tile.width, tile.height);
        let precision = header.siz.components[c].precision;
        let data = match header.transform_for(c) {
            WaveletTransform::Reversible53 => {
                let centered = reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    c,
                    cw,
                    ch,
                    stats,
                )?;
                finalize_i32_samples(centered, precision)
            }
            WaveletTransform::Irreversible97 => {
                let centered =
                    reconstruct_irreversible_97_centered(header, tile.into_real()?, cw, ch, stats)?;
                finalize_f32_samples(centered, precision)
            }
        };
        if data.len() != cw * ch {
            return Err(invalid("sYCC native plane size mismatch"));
        }
        native.push((data, cw, ch));
    }
    let (y_plane, yw, yh) = &native[0];
    let (cb_plane, cbw, cbh) = &native[1];
    let (cr_plane, crw, _crh) = &native[2];
    if *yw != full_w || *yh != full_h {
        return Err(invalid("sYCC luma plane is not full resolution"));
    }

    let start = stats.start();
    let offset = 1i32 << (precision.saturating_sub(1));
    let max_sample = (1i32 << precision) - 1;
    for y in 0..target.height {
        let source_y = target.source_y + y;
        let cy = (source_y / dy1).min(cbh.saturating_sub(1));
        for x in 0..target.width {
            let source_x = target.source_x + x;
            let cx = (source_x / dx1).min(cbw.saturating_sub(1));
            let yy = y_plane[source_y * full_w + source_x];
            let cb = cb_plane[cy * cbw + cx] - offset;
            let cr = cr_plane[cy * crw + cx] - offset;
            // OpenJPEG sycc_to_rgb bit-exact (truncation toward zero).
            let r = (yy + (1.402 * cr as f32) as i32).clamp(0, max_sample) as u8;
            let g =
                (yy - (0.344 * cb as f32 + 0.714 * cr as f32) as i32).clamp(0, max_sample) as u8;
            let b = (yy + (1.772 * cb as f32) as i32).clamp(0, max_sample) as u8;
            let off = target.pixel_offset(x, y);
            store_rgb_layout(
                &mut target.data[off..off + channels],
                layout,
                channels,
                r,
                g,
                b,
                255,
            );
        }
    }
    record_finalize_time(stats, start);
    stats.update(|stats| {
        stats.output_pixels = stats
            .output_pixels
            .saturating_add((target.width * target.height) as u64);
    });
    Ok(())
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
                let (w, h) = (tile.width, tile.height);
                planes.push(reconstruct_reversible_53_centered(
                    header,
                    tile.into_integer()?,
                    component,
                    w,
                    h,
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
                let (w, h) = (tile.width, tile.height);
                planes.push(reconstruct_irreversible_97_centered(
                    header,
                    tile.into_real()?,
                    w,
                    h,
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
    let (w, h) = (tile.width, tile.height);
    let precision = header.siz.components[component].precision;
    Ok(match header.transform_for(component) {
        WaveletTransform::Irreversible97 => {
            let centered =
                reconstruct_irreversible_97_centered(header, tile.into_real()?, w, h, stats)?;
            centered_f32_to_u8(centered, precision)
        }
        WaveletTransform::Reversible53 => {
            let centered = reconstruct_reversible_53_centered(
                header,
                tile.into_integer()?,
                component,
                w,
                h,
                stats,
            )?;
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

/// True when the tile-component origin is aligned to the full decomposition
/// lattice (`2^levels`). Every synthesis step then has even phase, so the
/// origin-zero optimized inverse DWT is bit-identical to the phase-aware path
/// and is preferred for multi-tile streams with regular tile grids.
fn lattice_aligned_origin(x0: usize, y0: usize, levels: u8) -> bool {
    let Some(alignment) = 1usize.checked_shl(u32::from(levels)) else {
        return false;
    };
    x0.is_multiple_of(alignment) && y0.is_multiple_of(alignment)
}

fn use_common_even_dwt(header: &CodestreamHeader) -> bool {
    lattice_aligned_origin(
        header.siz.x_origin as usize,
        header.siz.y_origin as usize,
        header.cod.decomposition_levels,
    )
}

fn reconstruct_reversible_53_centered(
    header: &CodestreamHeader,
    mut coefficients: Vec<i32>,
    component: usize,
    width: usize,
    height: usize,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<i32>> {
    if header.quant_for(component).style != QuantizationStyle::NoQuantization {
        return Err(invalid(
            "reversible 5/3 reconstruction expects no quantization",
        ));
    }
    let dwt_start = stats.start();
    if use_common_even_dwt(header) {
        if stats.is_enabled() {
            let timing = crate::dwt::inverse_53_2d_in_place_profiled(
                &mut coefficients,
                width,
                height,
                header.cod.decomposition_levels,
                PRIMITIVES.backend != "scalar",
            )?;
            record_dwt_breakdown(stats, timing);
        } else {
            (PRIMITIVES.dwt.inverse_53_2d)(
                &mut coefficients,
                width,
                height,
                header.cod.decomposition_levels,
            )?;
        }
    } else {
        crate::dwt::inverse_53_2d_in_place_at(
            &mut coefficients,
            width,
            height,
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
    width: usize,
    height: usize,
    stats: &mut StatsSink<'_>,
) -> Result<Vec<f32>> {
    // Dequantization is fused into Tier-1 code-block output (see
    // `t1::dequantize_block_to_tile`): `data` already holds the dequantized
    // `f32` subband samples, so reconstruction goes straight to the inverse DWT
    // with no separate full-image coefficient plane or dequant sweep.
    let dwt_start = stats.start();
    if use_common_even_dwt(header) {
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
    fn fused_ict_pack_matches_staged_ict_then_finalize() {
        // Byte-identical to inverse_ict + centered_f32_to_u8 + sequential RGB interleave.
        let n = 64usize;
        let y: Vec<f32> = (0..n).map(|i| (i as f32) * 0.5 - 20.0).collect();
        let cb: Vec<f32> = (0..n).map(|i| (i as f32) * 0.25 - 5.0).collect();
        let cr: Vec<f32> = (0..n).map(|i| 10.0 - (i as f32) * 0.3).collect();

        let mut y_s = y.clone();
        let mut cb_s = cb.clone();
        let mut cr_s = cr.clone();
        crate::simd::scalar::inverse_ict(&mut y_s, &mut cb_s, &mut cr_s);
        let mut staged = Vec::with_capacity(n * 3);
        for i in 0..n {
            staged.push(finalize_centered_f32_u8(y_s[i]));
            staged.push(finalize_centered_f32_u8(cb_s[i]));
            staged.push(finalize_centered_f32_u8(cr_s[i]));
        }

        let mut fused = vec![0u8; n * 3];
        let w = n; // 1-row plane for simplicity
        fuse_pack_f32_into(
            &y,
            &cb,
            &cr,
            None,
            true,
            PackedLayout::Sequential,
            3,
            n,
            w,
            &mut PackedWriteTarget {
                data: &mut fused,
                stride: w * 3,
                channels: 3,
                origin_x: 0,
                origin_y: 0,
                source_x: 0,
                source_y: 0,
                width: w,
                height: 1,
                layout: PackedLayout::Sequential,
            },
        )
        .expect("fuse");
        assert_eq!(fused, staged);
    }

    #[test]
    fn fused_rct_pack_matches_staged_rct_then_finalize() {
        let n = 48usize;
        let y: Vec<i32> = (0..n).map(|i| (i as i32) - 24).collect();
        let db: Vec<i32> = (0..n).map(|i| (i as i32) % 17 - 8).collect();
        let dr: Vec<i32> = (0..n).map(|i| 9 - (i as i32) % 19).collect();

        let mut y_s = y.clone();
        let mut db_s = db.clone();
        let mut dr_s = dr.clone();
        crate::simd::scalar::inverse_rct(&mut y_s, &mut db_s, &mut dr_s);
        let mut staged = Vec::with_capacity(n * 3);
        for i in 0..n {
            staged.push(finalize_centered_i32_u8(y_s[i]));
            staged.push(finalize_centered_i32_u8(db_s[i]));
            staged.push(finalize_centered_i32_u8(dr_s[i]));
        }

        let mut fused = vec![0u8; n * 3];
        fuse_pack_i32_into(
            &y,
            &db,
            &dr,
            None,
            true,
            PackedLayout::Sequential,
            3,
            n,
            n,
            &mut PackedWriteTarget {
                data: &mut fused,
                stride: n * 3,
                channels: 3,
                origin_x: 0,
                origin_y: 0,
                source_x: 0,
                source_y: 0,
                width: n,
                height: 1,
                layout: PackedLayout::Sequential,
            },
        )
        .expect("fuse");
        assert_eq!(fused, staged);
    }

    #[test]
    fn sycc420_packed_matches_upsample_then_sycc_oracle() {
        // 4x4 luma, 2x2 chroma; nearest upsample + sycc_to_rgb vs fused kernel.
        let yw = 4usize;
        let yh = 4usize;
        let y: Vec<i32> = (0..16).map(|i| 16 + i * 8).collect();
        let cb: Vec<i32> = vec![100, 120, 140, 160];
        let cr: Vec<i32> = vec![90, 110, 130, 150];
        let mut expanded = vec![
            y.clone(),
            upsample_chroma_nearest(&cb, 2, 2, yw, yh, 2, 2),
            upsample_chroma_nearest(&cr, 2, 2, yw, yh, 2, 2),
        ];
        sycc_to_rgb_in_place(&mut expanded, 8);
        let mut oracle = Vec::with_capacity(yw * yh * 3);
        for i in 0..yw * yh {
            oracle.push(expanded[0][i] as u8);
            oracle.push(expanded[1][i] as u8);
            oracle.push(expanded[2][i] as u8);
        }

        // Replay fused sampling formula on the same finalized planes.
        let mut fused = vec![0u8; yw * yh * 3];
        let offset = 128i32;
        let max_sample = 255i32;
        for yy in 0..yh {
            let cy = (yy / 2).min(1);
            for xx in 0..yw {
                let cx = (xx / 2).min(1);
                let yv = y[yy * yw + xx];
                let cbv = cb[cy * 2 + cx] - offset;
                let crv = cr[cy * 2 + cx] - offset;
                let r = (yv + (1.402 * crv as f32) as i32).clamp(0, max_sample) as u8;
                let g = (yv - (0.344 * cbv as f32 + 0.714 * crv as f32) as i32).clamp(0, max_sample)
                    as u8;
                let b = (yv + (1.772 * cbv as f32) as i32).clamp(0, max_sample) as u8;
                let o = (yy * yw + xx) * 3;
                fused[o] = r;
                fused[o + 1] = g;
                fused[o + 2] = b;
            }
        }
        assert_eq!(fused, oracle);
    }

    #[test]
    fn lattice_aligned_origin_accepts_zero_and_power_of_two_grids() {
        assert!(lattice_aligned_origin(0, 0, 0));
        assert!(lattice_aligned_origin(0, 0, 5));
        assert!(lattice_aligned_origin(32, 64, 5)); // 2^5 = 32
        assert!(lattice_aligned_origin(128, 256, 6));
        assert!(!lattice_aligned_origin(1, 0, 1));
        assert!(!lattice_aligned_origin(0, 16, 5)); // 16 not multiple of 32
        assert!(!lattice_aligned_origin(48, 0, 5));
    }

    #[test]
    fn lattice_aligned_97_inverse_matches_origin_zero_optimized_path() {
        // A nonzero origin that is still lattice-aligned must produce the same
        // reconstruction as the origin-zero optimized backend on the same local
        // coefficient buffer (every synthesis phase is even).
        let width = 17usize;
        let height = 13usize;
        let levels = 3u8;
        let origin = 1usize << levels; // 8
        let mut coeffs: Vec<f32> = (0..width * height)
            .map(|i| (i as f32 * 0.37) - 40.0)
            .collect();
        let mut optimized = coeffs.clone();
        let mut phase_aware = coeffs.clone();
        crate::dwt::inverse_97_2d_in_place(&mut optimized, width, height, levels);
        crate::dwt::inverse_97_2d_in_place_at(
            &mut phase_aware,
            width,
            height,
            levels,
            origin,
            origin,
        )
        .expect("phase-aware inverse");
        for (a, b) in optimized.iter().zip(&phase_aware) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
        // Sanity: the lattice helper routes this origin to the common path.
        assert!(lattice_aligned_origin(origin, origin, levels));
        let _ = &mut coeffs;
    }

    #[test]
    fn sycc_to_rgb_matches_openjpeg_reference() {
        // These are the exact codes OpenJPEG's `sycc_to_rgb`
        // (src/bin/common/color.c) produces: integer luma plus the `(int)`
        // (truncate-toward-zero) chroma term with coefficients
        // 1.402 / 0.344 / 0.714 / 1.772, clamped to [0, 255]. Neutral chroma
        // (Cb=Cr=128) is a grayscale ramp; the BT.601 primaries saturate one
        // channel. Hand-computed against the reference formula, not round-to-
        // nearest, so the decoder stays bit-exact with `opj_decompress`.
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
        assert_eq!(px(3), (254, 1, 0), "red primary");
        assert_eq!(px(4), (0, 255, 2), "green primary");
        assert_eq!(px(5), (0, 1, 254), "blue primary");
    }

    #[test]
    fn upsample_chroma_nearest_replicates_2x2_blocks() {
        // A 2x2 chroma plane upsampled to a full 4:2:0 (dx=dy=2) 4x4 image:
        // every chroma sample fills the 2x2 block anchored at its top-left,
        // matching OpenJPEG's `sycc420_to_rgb` replication for a (0,0)-origin
        // image.
        let src = vec![10, 20, 30, 40];
        let out = upsample_chroma_nearest(&src, 2, 2, 4, 4, 2, 2);
        assert_eq!(
            out,
            vec![
                10, 10, 20, 20, //
                10, 10, 20, 20, //
                30, 30, 40, 40, //
                30, 30, 40, 40, //
            ]
        );
    }

    #[test]
    fn upsample_chroma_nearest_handles_odd_full_dimensions() {
        // Odd full width/height (5x3): the last column/row reuse the final
        // chroma column/row, exactly as OpenJPEG's odd-dimension tail does with
        // `cw = ceil(5/2) = 3`, `ch = ceil(3/2) = 2`.
        let src = vec![
            1, 2, 3, //
            4, 5, 6, //
        ];
        let out = upsample_chroma_nearest(&src, 3, 2, 5, 3, 2, 2);
        assert_eq!(
            out,
            vec![
                1, 1, 2, 2, 3, //
                1, 1, 2, 2, 3, //
                4, 4, 5, 5, 6, //
            ]
        );
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
