//! Narrow JPEG 2000 decoder entry points.
//!
//! This module starts with the Annex I JP2 container and Annex A codestream
//! header slice of the decoder plan. Later Tier-2 and Tier-1 stages should
//! consume these typed headers rather than reparsing marker bytes.

mod codestream;
mod jp2_parse;
mod reconstruct;
mod stats;
pub(crate) mod t1;
pub(crate) mod t2;

use crate::error::Result;
use crate::model::{ColorEncoding, ColorSpace, Image};
use std::io::Read;

pub use stats::Jp2DecodeStats;
use stats::StatsSink;

pub use crate::j2k::decode_markers::{
    CodSegment, CodeBlockStyle, CodestreamHeader, ComponentSiz, PrecinctSize, ProgressionOrder,
    QcdSegment, QuantizationStep, QuantizationStyle, SizSegment, WaveletTransform,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeMetadata {
    pub width: u32,
    pub height: u32,
    pub colorspace: ColorSpace,
    pub color_encoding: ColorEncoding,
    pub has_ipr_metadata: bool,
    pub codestream: CodestreamHeader,
    pub tile_part_count: usize,
    pub first_tile_payload_len: usize,
    /// An in-codestream opacity channel declared by a `cdef` box, when present.
    /// The consumer surfaces it as a soft mask (PDF `/SMaskInData`); request
    /// [`DecodeOutputFormat::Rgba8`] to receive the interleaved alpha plane.
    pub in_data_alpha: Option<InDataAlpha>,
}

/// A JPEG 2000 in-codestream opacity channel (from a `cdef` box).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InDataAlpha {
    /// The codestream component carrying opacity.
    pub component: u16,
    /// Whether the colour channels are premultiplied by this opacity.
    pub premultiplied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DecodeResolution {
    /// Reconstruct every wavelet resolution.
    Full,
    /// Omit this many highest wavelet resolutions.
    ReduceLevels(u8),
    /// Select the lowest-cost resolution that remains at least this large
    /// after applying the quality margin.
    AtLeast {
        width: u32,
        height: u32,
        quality_margin: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeOutputFormat {
    /// Preserve the compatibility decoder's planar integer [`Image`] output.
    NativePlanarI32,
    /// Write one packed 8-bit grayscale sample per pixel.
    Gray8,
    /// Write packed 8-bit red, green, and blue samples.
    Rgb8,
    /// Write packed 8-bit red, green, blue, and opacity samples (the codestream
    /// must carry a `cdef` opacity channel).
    Rgba8,
    /// Write packed 8-bit cyan, magenta, yellow, and black samples.
    Cmyk8,
}

impl DecodeOutputFormat {
    /// Packed samples per pixel for this format (native planar output is not
    /// interleaved and reports one).
    fn component_count(self) -> usize {
        match self {
            DecodeOutputFormat::NativePlanarI32 | DecodeOutputFormat::Gray8 => 1,
            DecodeOutputFormat::Rgb8 => 3,
            DecodeOutputFormat::Rgba8 | DecodeOutputFormat::Cmyk8 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeConcurrency {
    Serial,
    Budgeted(usize),
}

/// A rectangular region of interest to decode.
///
/// Coordinates and extent are always expressed in the **full-resolution image
/// grid**, regardless of any [`DecodeResolution`] reduction requested alongside
/// it. When combined with `ReduceLevels`/`AtLeast`, the region is projected to
/// the selected reduced resolution and the returned raster is exactly the
/// crop of the reduced image that the region covers — byte-for-byte identical
/// to cropping a full (region-less) decode at the same resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodeRequest {
    pub resolution: DecodeResolution,
    pub output: DecodeOutputFormat,
    pub region: Option<DecodeRegion>,
    pub concurrency: DecodeConcurrency,
}

impl Default for DecodeRequest {
    fn default() -> Self {
        Self {
            resolution: DecodeResolution::Full,
            output: DecodeOutputFormat::NativePlanarI32,
            region: None,
            concurrency: DecodeConcurrency::Serial,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRaster {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: DecodeOutputFormat,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum DecodeResult {
    Native(Image),
    Raster(DecodedRaster),
}

struct DecodeScratch {
    tier1: t1::Tier1Scratch,
}

impl DecodeScratch {
    fn new() -> Self {
        Self {
            tier1: t1::Tier1Scratch::new(),
        }
    }
}

pub struct Jp2Decoder {
    scratch: DecodeScratch,
    pool: Option<(usize, rayon::ThreadPool)>,
}

impl std::fmt::Debug for Jp2Decoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Jp2Decoder").finish_non_exhaustive()
    }
}

impl Default for Jp2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Jp2Decoder {
    /// Construct a decoder session with reusable, lazily grown working memory.
    pub fn new() -> Self {
        Self {
            scratch: DecodeScratch::new(),
            pool: None,
        }
    }

    /// Decode one image while retaining bounded Tier-1 scratch for later calls.
    pub fn decode(&mut self, bytes: &[u8], request: &DecodeRequest) -> Result<DecodeResult> {
        validate_decode_request(request)?;
        let thread_count = decode_thread_count(request.concurrency)?;
        if self
            .pool
            .as_ref()
            .is_none_or(|(cached_count, _)| *cached_count != thread_count)
        {
            self.pool = Some((thread_count, build_decode_pool(thread_count)?));
        }
        let Self { scratch, pool } = self;
        let pool = &pool.as_ref().expect("decode pool was initialized").1;
        pool.install(|| decode_jp2_request_with_scratch(bytes, request, scratch))
    }
}

/// Parse the JP2 wrapper and JPEG 2000 Part 1 main header.
///
/// This is the first tactical decoder slice: it validates the container,
/// extracts the first `jp2c` codestream box, and decodes the SIZ/COD/QCD marker
/// segments needed by packet and Tier-1 decoding.
pub fn inspect_jp2(bytes: &[u8]) -> Result<DecodeMetadata> {
    let mut stats = StatsSink::disabled();
    let core = parse_jp2_core(bytes, &mut stats)?;
    let first_payload = core.parts.tile_parts[0].payload;
    Ok(DecodeMetadata {
        width: core.header.width,
        height: core.header.height,
        colorspace: core.header.colorspace,
        color_encoding: core.header.color_encoding.clone(),
        has_ipr_metadata: core.header.has_ipr_metadata,
        first_tile_payload_len: first_payload.len(),
        tile_part_count: core.parts.tile_parts.len(),
        in_data_alpha: core.header.alpha.map(|alpha| InDataAlpha {
            component: alpha.component as u16,
            premultiplied: alpha.premultiplied,
        }),
        codestream: core.codestream,
    })
}

/// Decode a Part 1 JP2 image or bare J2K codestream into the crate's native
/// [`Image`] model.
///
/// This currently targets unsigned 8–16-bit grayscale, sRGB, CMYK, and
/// palette-mapped images. All five Part 1 progression orders, explicit
/// precinct partitions, SOP/EPH packet delimiters, and packet-boundary
/// multi-tile-parts are supported. MQ context reset, pass termination,
/// vertical-causal contexts, predictable termination, and segmentation symbols
/// are supported; selective arithmetic bypass remains outside this slice.
pub fn decode_jp2(bytes: &[u8]) -> Result<Image> {
    let mut stats = StatsSink::disabled();
    let mut scratch = DecodeScratch::new();
    decode_jp2_impl(bytes, DecodeResolution::Full, None, &mut stats, &mut scratch)
}

/// Decode a JP2/J2K image and return decoder-internal stage attribution.
///
/// The compatibility [`decode_jp2`] path does not start timers or update
/// counters; callers opt into the small profiling cost through this function.
pub fn decode_jp2_with_stats(bytes: &[u8]) -> Result<(Image, Jp2DecodeStats)> {
    let mut stats = Jp2DecodeStats::default();
    let mut scratch = DecodeScratch::new();
    let total_start = std::time::Instant::now();
    let image = {
        let mut sink = StatsSink::enabled(&mut stats);
        decode_jp2_impl(bytes, DecodeResolution::Full, None, &mut sink, &mut scratch)?
    };
    stats.total_ns = u64::try_from(total_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    Ok((image, stats))
}

pub fn decode_jp2_request(bytes: &[u8], request: &DecodeRequest) -> Result<DecodeResult> {
    validate_decode_request(request)?;
    let thread_count = decode_thread_count(request.concurrency)?;
    let pool = build_decode_pool(thread_count)?;
    let mut scratch = DecodeScratch::new();
    pool.install(|| decode_jp2_request_with_scratch(bytes, request, &mut scratch))
}

fn validate_decode_request(request: &DecodeRequest) -> Result<()> {
    if let Some(region) = &request.region {
        if region.width == 0 || region.height == 0 {
            return Err(crate::Jp2LamError::InvalidInput(
                "decode region must have non-zero width and height".into(),
            ));
        }
        region
            .x
            .checked_add(region.width)
            .and_then(|_| region.y.checked_add(region.height))
            .ok_or_else(|| {
                crate::Jp2LamError::InvalidInput("decode region extent overflows".into())
            })?;
    }
    decode_thread_count(request.concurrency)?;
    Ok(())
}

/// A region window as an absolute coefficient rectangle in the reduced
/// tile-component reference grid (resolution `L'`, the reconstructed sample
/// grid). Used to restrict Tier-1 to the code blocks whose subband footprints
/// influence the region's output pixels.
#[derive(Debug, Clone, Copy)]
struct RegionSpatial {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

/// Retain a code block iff its packed rectangle intersects the region window
/// computed for its subband. A block with no matching subband window lies
/// wholly outside the region's inverse-DWT support and is dropped.
fn code_block_intersects_region(
    windows: &[t2::RegionBandWindow],
    block: &t2::DecodedCodeBlock<'_>,
) -> bool {
    windows.iter().any(|w| {
        w.resolution == block.resolution
            && w.band == block.band
            && block.x0 < w.x1
            && w.x0 < block.x1
            && block.y0 < w.y1
            && w.y0 < block.y1
    })
}

/// Conservative inverse-lifting support margin (in coefficient samples) added
/// per synthesis level when projecting a region window to coarser resolutions.
fn region_margin(transform: WaveletTransform) -> u32 {
    match transform {
        WaveletTransform::Reversible53 => 2,
        WaveletTransform::Irreversible97 => 5,
    }
}

fn decode_thread_count(concurrency: DecodeConcurrency) -> Result<usize> {
    match concurrency {
        DecodeConcurrency::Serial => Ok(1),
        DecodeConcurrency::Budgeted(0) => Err(crate::Jp2LamError::InvalidInput(
            "decode concurrency budget must be at least one".into(),
        )),
        DecodeConcurrency::Budgeted(threads) => Ok(threads),
    }
}

fn build_decode_pool(thread_count: usize) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .thread_name(|index| format!("jp2lam-decode-{index}"))
        .build()
        .map_err(|error| {
            crate::Jp2LamError::DecodeFailed(format!(
                "could not create {thread_count}-thread decode pool: {error}"
            ))
        })
}

fn decode_jp2_request_with_scratch(
    bytes: &[u8],
    request: &DecodeRequest,
    scratch: &mut DecodeScratch,
) -> Result<DecodeResult> {
    if request.region.is_some() {
        return decode_region_request(bytes, request, scratch);
    }
    if request.output != DecodeOutputFormat::NativePlanarI32 {
        if let Some(raster) =
            decode_packed_direct(bytes, request.resolution, request.output, None, scratch)?
        {
            return Ok(DecodeResult::Raster(raster));
        }
    }
    let mut stats = StatsSink::disabled();
    let image = decode_jp2_impl(bytes, request.resolution, None, &mut stats, scratch)?;
    match request.output {
        DecodeOutputFormat::NativePlanarI32 => Ok(DecodeResult::Native(image)),
        format => Ok(DecodeResult::Raster(pack_image_8bit(&image, format)?)),
    }
}

fn decode_packed_direct(
    bytes: &[u8],
    resolution: DecodeResolution,
    format: DecodeOutputFormat,
    region: Option<RegionSpatial>,
    scratch: &mut DecodeScratch,
) -> Result<Option<DecodedRaster>> {
    let mut stats = StatsSink::disabled();
    let core = parse_jp2_core(bytes, &mut stats)?;
    let expected_space = match format {
        DecodeOutputFormat::Gray8 => ColorSpace::Gray,
        DecodeOutputFormat::Rgb8 | DecodeOutputFormat::Rgba8 => ColorSpace::Srgb,
        DecodeOutputFormat::Cmyk8 => ColorSpace::Cmyk,
        DecodeOutputFormat::NativePlanarI32 => return Ok(None),
    };
    if core.header.palette.is_some() || core.header.colorspace != expected_space {
        return Ok(None);
    }
    // Rgba8 interleaves a 4th (opacity) plane; without an in-data alpha channel
    // there is nothing to fill it, so fall back to the planar path.
    if matches!(format, DecodeOutputFormat::Rgba8) && core.header.alpha.is_none() {
        return Ok(None);
    }
    let reduce_levels = select_reduce_levels(&core.codestream, resolution)?;
    let channels = format.component_count();

    // Single tile: reconstruct straight into the packed raster (no intermediate
    // planar image), the phase-1/2 fast path.
    if core.tile_parts_by_tile.len() == 1 {
        let (header, components) = decode_tile_components(
            &core,
            0,
            &core.tile_parts_by_tile[0],
            reduce_levels,
            region,
            &mut stats,
            scratch,
        )?;
        let data = reconstruct::reconstruct_packed_u8_profiled(
            &header,
            expected_space,
            channels,
            components,
            &mut stats,
        )?;
        let stride = (header.siz.width as usize).checked_mul(channels).ok_or_else(|| {
            crate::Jp2LamError::DecodeFailed("packed output stride overflow".into())
        })?;
        return Ok(Some(DecodedRaster {
            width: header.siz.width,
            height: header.siz.height,
            stride,
            format,
            data,
        }));
    }

    // Multi-tile: reconstruct each tile's finished samples (per-tile inverse
    // RCT/ICT, clamp, scale, interleave) and stitch them straight into the
    // packed output rectangle. No intermediate full-image planar buffer.
    let siz = &core.codestream.siz;
    let (canvas_x0, canvas_x1) =
        reduced_axis_bounds(siz.x_origin, siz.x_origin + siz.width, reduce_levels);
    let (canvas_y0, canvas_y1) =
        reduced_axis_bounds(siz.y_origin, siz.y_origin + siz.height, reduce_levels);
    let out_width = usize::try_from(canvas_x1 - canvas_x0)
        .map_err(|_| crate::Jp2LamError::DecodeFailed("packed canvas width overflow".into()))?;
    let out_height = usize::try_from(canvas_y1 - canvas_y0)
        .map_err(|_| crate::Jp2LamError::DecodeFailed("packed canvas height overflow".into()))?;
    let stride = out_width
        .checked_mul(channels)
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("packed output stride overflow".into()))?;
    let total = stride
        .checked_mul(out_height)
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("packed output size overflow".into()))?;
    let mut data = vec![0u8; total];

    for (tile_index, part_indices) in core.tile_parts_by_tile.iter().enumerate() {
        let tile_index = u16::try_from(tile_index).map_err(|_| {
            crate::Jp2LamError::DecodeFailed("tile index exceeds Isot range".into())
        })?;
        let (x0, y0, width, height) = tile_rect(&core.codestream, tile_index)?;
        let (reduced_x0, reduced_x1) = reduced_axis_bounds(x0, x0 + width, reduce_levels);
        let (reduced_y0, reduced_y1) = reduced_axis_bounds(y0, y0 + height, reduce_levels);
        if let Some(region) = region {
            if reduced_x0 >= region.x1
                || region.x0 >= reduced_x1
                || reduced_y0 >= region.y1
                || region.y0 >= reduced_y1
            {
                continue;
            }
        }
        let (header, components) = decode_tile_components(
            &core,
            tile_index,
            part_indices,
            reduce_levels,
            region,
            &mut stats,
            scratch,
        )?;
        let tile_data = reconstruct::reconstruct_packed_u8_profiled(
            &header,
            expected_space,
            channels,
            components,
            &mut stats,
        )?;
        let tile_width = header.siz.width as usize;
        let tile_height = header.siz.height as usize;
        let tile_stride = tile_width * channels;
        let dst_x = (reduced_x0 - canvas_x0) as usize * channels;
        let dst_y = (reduced_y0 - canvas_y0) as usize;
        let stitch_start = stats.start();
        for row in 0..tile_height {
            let src = row * tile_stride;
            let dst = (dst_y + row) * stride + dst_x;
            data[dst..dst + tile_stride].copy_from_slice(&tile_data[src..src + tile_stride]);
        }
        stats.finish(stitch_start, |stats, elapsed| {
            stats.tile_stitch_ns = stats.tile_stitch_ns.saturating_add(elapsed);
        });
    }

    Ok(Some(DecodedRaster {
        width: out_width as u32,
        height: out_height as u32,
        stride,
        format,
        data,
    }))
}

/// A crop rectangle in reduced-image pixel indices (0-based within the
/// reconstructed reduced image).
#[derive(Debug, Clone, Copy)]
struct CropRect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

fn ceil_shift(value: u32, shift: u8) -> u32 {
    if shift == 0 {
        return value;
    }
    let add = (1u32 << shift) - 1;
    value.saturating_add(add) >> shift
}

/// Region-of-interest decode. `region` coordinates are in the FULL-resolution
/// image grid; they are projected to the selected reduced resolution, the
/// decoder restricts Tier-1 (single tile) or tile decoding (multi-tile) to the
/// covering coefficients, reconstructs the reduced image, and crops out exactly
/// the region's reduced raster. The result is byte-for-byte identical to the
/// same crop of a full decode.
/// Project a full-resolution-grid region to the reduced tile-component grid,
/// returning both the absolute coefficient window (`RegionSpatial`, used to
/// restrict decoding) and the reduced-pixel crop rectangle (`CropRect`, used to
/// slice the reconstructed image). Both derive from the same floor-start /
/// ceil-end projection so the region decode and a full-decode crop agree.
fn project_region(
    siz: &SizSegment,
    region: DecodeRegion,
    reduce_levels: u8,
) -> Result<(RegionSpatial, CropRect)> {
    let region_x1 = region.x.checked_add(region.width);
    let region_y1 = region.y.checked_add(region.height);
    let (region_x1, region_y1) = match (region_x1, region_y1) {
        (Some(x1), Some(y1)) if x1 <= siz.width && y1 <= siz.height => (x1, y1),
        _ => {
            return Err(crate::Jp2LamError::InvalidInput(format!(
                "decode region {}x{}+{}+{} lies outside the {}x{} image",
                region.width, region.height, region.x, region.y, siz.width, siz.height
            )));
        }
    };

    // Absolute reference-grid coordinates of the region and of the full reduced
    // image, then project the region to reduced pixel indices (floor start,
    // ceil end) so the crop covers every reduced pixel the region touches.
    let abs_x0 = siz.x_origin + region.x;
    let abs_y0 = siz.y_origin + region.y;
    let abs_x1 = siz.x_origin + region_x1;
    let abs_y1 = siz.y_origin + region_y1;
    let (img_rx0, img_rx1) =
        reduced_axis_bounds(siz.x_origin, siz.x_origin + siz.width, reduce_levels);
    let (img_ry0, img_ry1) =
        reduced_axis_bounds(siz.y_origin, siz.y_origin + siz.height, reduce_levels);
    let ax0 = (abs_x0 >> reduce_levels).clamp(img_rx0, img_rx1);
    let ay0 = (abs_y0 >> reduce_levels).clamp(img_ry0, img_ry1);
    let ax1 = ceil_shift(abs_x1, reduce_levels).clamp(ax0.max(img_rx0), img_rx1);
    let ay1 = ceil_shift(abs_y1, reduce_levels).clamp(ay0.max(img_ry0), img_ry1);
    // A non-empty full-resolution region always projects to at least one reduced
    // pixel; guard against a degenerate (fully clamped) window regardless.
    let ax1 = ax1.max(ax0 + 1).min(img_rx1);
    let ay1 = ay1.max(ay0 + 1).min(img_ry1);

    Ok((
        RegionSpatial {
            x0: ax0,
            y0: ay0,
            x1: ax1,
            y1: ay1,
        },
        CropRect {
            x0: ax0 - img_rx0,
            y0: ay0 - img_ry0,
            x1: ax1 - img_rx0,
            y1: ay1 - img_ry0,
        },
    ))
}

fn decode_region_request(
    bytes: &[u8],
    request: &DecodeRequest,
    scratch: &mut DecodeScratch,
) -> Result<DecodeResult> {
    let region = request.region.expect("region decode requires a region");
    let mut stats = StatsSink::disabled();
    let core = parse_jp2_core(bytes, &mut stats)?;
    let reduce_levels = select_reduce_levels(&core.codestream, request.resolution)?;
    let (region_spatial, crop) = project_region(&core.codestream.siz, region, reduce_levels)?;
    drop(core);

    // Packed direct output stitches the reduced raster; crop it in place.
    if request.output != DecodeOutputFormat::NativePlanarI32 {
        if let Some(raster) = decode_packed_direct(
            bytes,
            request.resolution,
            request.output,
            Some(region_spatial),
            scratch,
        )? {
            return Ok(DecodeResult::Raster(crop_raster(&raster, crop)?));
        }
    }

    // Native (or packed-ineligible) path: reduced image, cropped, then packed.
    let image = decode_jp2_impl(
        bytes,
        request.resolution,
        Some(region_spatial),
        &mut stats,
        scratch,
    )?;
    let cropped = crop_image(&image, crop)?;
    match request.output {
        DecodeOutputFormat::NativePlanarI32 => Ok(DecodeResult::Native(cropped)),
        format => Ok(DecodeResult::Raster(pack_image_8bit(&cropped, format)?)),
    }
}

/// Crop a reconstructed planar image to a reduced-pixel rectangle.
fn crop_image(image: &Image, crop: CropRect) -> Result<Image> {
    let width = image.width as usize;
    let height = image.height as usize;
    let cx0 = crop.x0 as usize;
    let cy0 = crop.y0 as usize;
    let cx1 = crop.x1 as usize;
    let cy1 = crop.y1 as usize;
    if cx1 > width || cy1 > height || cx0 >= cx1 || cy0 >= cy1 {
        return Err(crate::Jp2LamError::DecodeFailed(
            "region crop rectangle is outside the reconstructed image".into(),
        ));
    }
    let crop_width = cx1 - cx0;
    let crop_height = cy1 - cy0;
    let components = image
        .components
        .iter()
        .map(|component| {
            let mut data = Vec::with_capacity(crop_width * crop_height);
            for row in cy0..cy1 {
                let start = row * width + cx0;
                data.extend_from_slice(&component.data[start..start + crop_width]);
            }
            crate::model::Component {
                data,
                width: crop_width as u32,
                height: crop_height as u32,
                precision: component.precision,
                signed: component.signed,
                dx: component.dx,
                dy: component.dy,
            }
        })
        .collect();
    Ok(Image {
        width: crop_width as u32,
        height: crop_height as u32,
        colorspace: image.colorspace,
        components,
    })
}

/// Crop a packed 8-bit raster to a reduced-pixel rectangle, re-striding it.
fn crop_raster(raster: &DecodedRaster, crop: CropRect) -> Result<DecodedRaster> {
    let width = raster.width as usize;
    let height = raster.height as usize;
    let channels = raster.format.component_count();
    let cx0 = crop.x0 as usize;
    let cy0 = crop.y0 as usize;
    let cx1 = crop.x1 as usize;
    let cy1 = crop.y1 as usize;
    if cx1 > width || cy1 > height || cx0 >= cx1 || cy0 >= cy1 {
        return Err(crate::Jp2LamError::DecodeFailed(
            "region crop rectangle is outside the reconstructed raster".into(),
        ));
    }
    let crop_width = cx1 - cx0;
    let crop_height = cy1 - cy0;
    let out_stride = crop_width * channels;
    let mut data = Vec::with_capacity(out_stride * crop_height);
    for row in cy0..cy1 {
        let start = row * raster.stride + cx0 * channels;
        data.extend_from_slice(&raster.data[start..start + out_stride]);
    }
    Ok(DecodedRaster {
        width: crop_width as u32,
        height: crop_height as u32,
        stride: out_stride,
        format: raster.format,
        data,
    })
}

fn decode_jp2_impl(
    bytes: &[u8],
    resolution: DecodeResolution,
    region: Option<RegionSpatial>,
    stats: &mut StatsSink<'_>,
    scratch: &mut DecodeScratch,
) -> Result<Image> {
    let core = parse_jp2_core(bytes, stats)?;
    let reduce_levels = select_reduce_levels(&core.codestream, resolution)?;
    // A palettized image decodes as its single index component (grayscale-like)
    // and is expanded to the container's channels afterwards; the codestream
    // itself carries one component, not the container's channel count.
    let reconstruct_space = if core.header.palette.is_some() {
        ColorSpace::Gray
    } else {
        core.header.colorspace
    };
    let tile_parts = &core.tile_parts_by_tile;

    // The overwhelmingly common JP2/PDF case is one tile covering the image.
    // Return that reconstructed tile directly instead of allocating a second
    // full-image set of planes and copying every sample into it.
    if tile_parts.len() == 1 {
        let mut image = decode_tile(
            &core,
            reconstruct_space,
            0,
            &tile_parts[0],
            reduce_levels,
            region,
            stats,
            scratch,
        )?;
        if let Some(palette) = &core.header.palette {
            image = expand_palette(&image, palette, core.header.colorspace)?;
        }
        return Ok(image);
    }

    // Multi-tile: build the (possibly reduced) full-image canvas, then place
    // each tile's reduced reconstruction at its phase-aware reduced origin.
    // Adjacent tiles share a full-resolution boundary `b`; both tiles derive
    // their reduced edge from `ceil(b / 2^reduce)` (see `reduced_axis_bounds`),
    // so the reduced tiles still partition the reduced canvas with no gap or
    // overlap.
    let siz = &core.codestream.siz;
    let (canvas_x0, _) =
        reduced_axis_bounds(siz.x_origin, siz.x_origin + siz.width, reduce_levels);
    let (canvas_y0, _) =
        reduced_axis_bounds(siz.y_origin, siz.y_origin + siz.height, reduce_levels);
    let mut image = empty_decoded_image_reduced(&core.codestream, reconstruct_space, reduce_levels)?;
    for (tile_index, part_indices) in tile_parts.iter().enumerate() {
        let tile_index = u16::try_from(tile_index).map_err(|_| {
            crate::Jp2LamError::DecodeFailed("tile index exceeds Isot range".into())
        })?;
        let (x0, y0, width, height) = tile_rect(&core.codestream, tile_index)?;
        let (reduced_x0, reduced_x1) = reduced_axis_bounds(x0, x0 + width, reduce_levels);
        let (reduced_y0, reduced_y1) = reduced_axis_bounds(y0, y0 + height, reduce_levels);
        // Region decode across tiles: decode only the tiles whose reduced rect
        // intersects the region window, leaving the rest zero in the canvas
        // (the final crop only reads region pixels, which those tiles cover).
        if let Some(region) = region {
            if reduced_x0 >= region.x1
                || region.x0 >= reduced_x1
                || reduced_y0 >= region.y1
                || region.y0 >= reduced_y1
            {
                continue;
            }
        }
        let tile_image = decode_tile(
            &core,
            reconstruct_space,
            tile_index,
            part_indices,
            reduce_levels,
            None,
            stats,
            scratch,
        )?;
        let stitch_start = stats.start();
        stitch_tile(
            &mut image,
            &tile_image,
            reduced_x0 - canvas_x0,
            reduced_y0 - canvas_y0,
        )?;
        stats.finish(stitch_start, |stats, elapsed| {
            stats.tile_stitch_ns = stats.tile_stitch_ns.saturating_add(elapsed);
        });
    }

    if let Some(palette) = &core.header.palette {
        image = expand_palette(&image, palette, core.header.colorspace)?;
    }

    Ok(image)
}

fn decode_tile(
    core: &ParsedJp2Core<'_>,
    colorspace: ColorSpace,
    tile_index: u16,
    part_indices: &[usize],
    reduce_levels: u8,
    region: Option<RegionSpatial>,
    stats: &mut StatsSink<'_>,
    scratch: &mut DecodeScratch,
) -> Result<Image> {
    let (reconstruction_header, components) = decode_tile_components(
        core,
        tile_index,
        part_indices,
        reduce_levels,
        region,
        stats,
        scratch,
    )?;
    reconstruct::reconstruct_image_profiled(&reconstruction_header, colorspace, components, stats)
}

fn decode_tile_components(
    core: &ParsedJp2Core<'_>,
    tile_index: u16,
    part_indices: &[usize],
    reduce_levels: u8,
    region: Option<RegionSpatial>,
    stats: &mut StatsSink<'_>,
    scratch: &mut DecodeScratch,
) -> Result<(CodestreamHeader, Vec<t1::DecodedTileCoefficients>)> {
    let (x0, y0, width, height) = tile_rect(&core.codestream, tile_index)?;
    // Fold this tile's tile-part header overrides (QCD/QCC/COC, or a redundant
    // COD restatement) onto the main-header defaults before deriving the
    // tile-local geometry. Gathering across all of the tile's parts is safe:
    // the markers only appear in the TPsot==0 part (Annex A.4.2).
    let overridden = core.codestream.with_tile_overrides(
        part_indices
            .iter()
            .flat_map(|&i| core.parts.tile_parts[i].header_segments.iter().copied()),
    )?;
    let full_tile_header = tile_local_header(&overridden, x0, y0, width, height);
    let highest_resolution = full_tile_header
        .cod
        .decomposition_levels
        .checked_sub(reduce_levels)
        .ok_or_else(|| {
            crate::Jp2LamError::InvalidInput(format!(
                "requested {reduce_levels} reduction levels, but COD defines only {}",
                full_tile_header.cod.decomposition_levels
            ))
        })?;
    let setup_start = stats.start();
    let mut packet_decoder = t2::TilePacketDecoder::new_with_options(
        &full_tile_header,
        stats.is_enabled(),
        highest_resolution,
    )?;
    stats.finish(setup_start, |stats, elapsed| {
        stats.tier2_setup_ns = stats.tier2_setup_ns.saturating_add(elapsed);
    });
    for &part_index in part_indices {
        let part = &core.parts.tile_parts[part_index];
        let packet_start = stats.start();
        let merge_before = packet_decoder.merge_ns();
        packet_decoder.push_tile_part(part.payload).map_err(|err| {
            crate::Jp2LamError::DecodeFailed(format!(
                "tile {tile_index} tile-part {}: {}",
                part.header.part_index,
                err.message()
            ))
        })?;
        let merge_elapsed = packet_decoder.merge_ns().saturating_sub(merge_before);
        stats.finish(packet_start, |stats, elapsed| {
            stats.tier2_packet_headers_ns = stats
                .tier2_packet_headers_ns
                .saturating_add(elapsed.saturating_sub(merge_elapsed));
            stats.tier2_merge_ns = stats.tier2_merge_ns.saturating_add(merge_elapsed);
        });
    }
    let concat_start = stats.start();
    let mut packets = packet_decoder.finish()?;
    stats.finish(concat_start, |stats, elapsed| {
        stats.tier2_concat_ns = stats.tier2_concat_ns.saturating_add(elapsed);
    });
    let reconstruction_header = reduced_tile_header(&full_tile_header, reduce_levels);
    // Region decode: retain only the code blocks whose subband footprints
    // influence the requested region's output pixels (the inverse-DWT support).
    // The dropped blocks stay zero in the coefficient plane; because the inverse
    // DWT is local, region pixels depend only on retained coefficients and match
    // a full decode exactly.
    if let Some(region) = region {
        let windows = t2::region_band_windows(
            &reconstruction_header,
            region.x0,
            region.y0,
            region.x1,
            region.y1,
            region_margin(reconstruction_header.cod.transform),
        )?;
        packets
            .codeblocks
            .retain(|block| code_block_intersects_region(&windows, block));
    }
    stats.update(|stats| {
        stats.packets = stats
            .packets
            .saturating_add(u64::try_from(packets.packets.len()).unwrap_or(u64::MAX));
        stats.packet_header_bytes = stats.packet_header_bytes.saturating_add(
            packets
                .packets
                .iter()
                .map(|packet| packet.header_len as u64)
                .sum(),
        );
        stats.codeword_bytes = stats.codeword_bytes.saturating_add(
            packets
                .packets
                .iter()
                .map(|packet| packet.body_len as u64)
                .sum(),
        );
        stats.codeblocks = stats
            .codeblocks
            .saturating_add(u64::try_from(packets.codeblocks.len()).unwrap_or(u64::MAX));
        for block in &packets.codeblocks {
            for pass in 0..block.passes {
                match pass {
                    0 => stats.cleanup_passes = stats.cleanup_passes.saturating_add(1),
                    _ if (pass - 1) % 3 == 0 => {
                        stats.significance_passes = stats.significance_passes.saturating_add(1)
                    }
                    _ if (pass - 1) % 3 == 1 => {
                        stats.refinement_passes = stats.refinement_passes.saturating_add(1)
                    }
                    _ => stats.cleanup_passes = stats.cleanup_passes.saturating_add(1),
                }
            }
            let pixels = u64::from(block.x1 - block.x0) * u64::from(block.y1 - block.y0);
            stats.coefficient_pixels = stats.coefficient_pixels.saturating_add(pixels);
            stats.peak_scratch_bytes = stats.peak_scratch_bytes.max(pixels.saturating_mul(6));
        }
    });
    let tier1_start = stats.start();
    let components = t1::decode_tile_components_with_scratch(
        &reconstruction_header,
        &packets,
        stats,
        &mut scratch.tier1,
    )?;
    stats.finish(tier1_start, |stats, elapsed| {
        stats.tier1_total_ns = stats.tier1_total_ns.saturating_add(elapsed);
    });
    Ok((reconstruction_header, components))
}

struct ParsedJp2Core<'a> {
    header: jp2_parse::Jp2Header,
    codestream: CodestreamHeader,
    parts: codestream::CodestreamView<'a>,
    tile_parts_by_tile: Vec<Vec<usize>>,
}

/// The SOC (start of codestream) marker. A `/JPXDecode` stream may be a bare
/// J2K codestream with no JP2 container (ISO 32000 permits it); such data
/// begins with SOC directly instead of the JP2 signature box.
const MARKER_SOC: [u8; 2] = [0xFF, 0x4F];

fn parse_jp2_core<'a>(bytes: &'a [u8], stats: &mut StatsSink<'_>) -> Result<ParsedJp2Core<'a>> {
    // Raw J2K codestream (no JP2 boxes): decode the codestream directly and
    // synthesize the container-level header from SIZ.
    let (header, codestream_bytes) = if bytes.starts_with(&MARKER_SOC) {
        (None, bytes)
    } else {
        let start = stats.start();
        let parsed = jp2_parse::parse_jp2(bytes)?;
        stats.finish(start, |stats, elapsed| {
            stats.container_parse_ns = stats.container_parse_ns.saturating_add(elapsed);
        });
        (Some(parsed.header), parsed.codestream)
    };

    let codestream_start = stats.start();
    let parts = codestream::parse_codestream_view(codestream_bytes)?;
    let first_tile = parts
        .tile_parts
        .first()
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("codestream has no tile-part".into()))?;
    let tile_count = parts.tile_parts.len();
    // The base header carries only main-header defaults; each tile applies its
    // own COD/COC/QCD/QCC overrides during decode (`with_tile_overrides`), so a
    // first tile that legitimately carries overrides no longer fails here.
    let mut codestream = CodestreamHeader::from_marker_segments(
        parts.main_header_segments.iter().copied(),
        first_tile.header,
        tile_count,
    )?;
    // Fold the first tile-part's COM markers back into the count so
    // DecodeMetadata still reports every comment in the codestream.
    codestream.comment_count += first_tile
        .header_segments
        .iter()
        .filter(|seg| {
            seg.get(0..2)
                .is_some_and(|m| u16::from_be_bytes([m[0], m[1]]) == crate::j2k::MARKER_COM)
        })
        .count();

    // For a bare codestream there is no JP2 header to cross-check; synthesize
    // one from SIZ (color space inferred from the component count, as OpenJPEG
    // does for `.j2k` input).
    let header = match header {
        Some(header) => header,
        None => synthesize_header_from_codestream(&codestream)?,
    };
    validate_jp2_decode_scope(&header, &codestream)?;
    stats.finish(codestream_start, |stats, elapsed| {
        stats.codestream_parse_ns = stats.codestream_parse_ns.saturating_add(elapsed);
    });
    let tile_plan_start = stats.start();
    let tile_parts_by_tile = tile_part_indices_by_tile(&parts, &codestream)?;
    stats.finish(tile_plan_start, |stats, elapsed| {
        stats.tile_plan_ns = stats.tile_plan_ns.saturating_add(elapsed);
    });
    Ok(ParsedJp2Core {
        header,
        codestream,
        parts,
        tile_parts_by_tile,
    })
}

/// Build a JP2 container header from a bare codestream's SIZ: dimensions and
/// component count come straight from SIZ, and the color space is inferred by
/// component count (1 → grayscale, 3 → sRGB, 4 → CMYK).
fn synthesize_header_from_codestream(
    codestream: &CodestreamHeader,
) -> Result<jp2_parse::Jp2Header> {
    let siz = &codestream.siz;
    let ncomp = siz.components.len();
    let (colorspace, color_encoding) = match ncomp {
        1 => (ColorSpace::Gray, ColorEncoding::Gray),
        3 => (ColorSpace::Srgb, ColorEncoding::Srgb),
        4 => (ColorSpace::Cmyk, ColorEncoding::Cmyk),
        n => {
            return Err(crate::Jp2LamError::UnsupportedFeature(format!(
                "raw codestream with {n} components has no default color space"
            )));
        }
    };
    let bits_per_component = siz
        .components
        .first()
        .map(|c| c.precision)
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("SIZ has no components".into()))?;
    Ok(jp2_parse::Jp2Header {
        width: siz.width,
        height: siz.height,
        component_count: ncomp as u16,
        bits_per_component,
        colorspace,
        color_encoding,
        has_ipr_metadata: false,
        palette: None,
        alpha: None,
    })
}

/// Validate Annex A.4.2 / Annex B.11 tile-part sequencing and return the
/// codestream-order indices belonging to each raster-order tile.
///
/// `TPsot` must advance from zero independently for every tile. `TNsot` may
/// either declare the exact per-tile part count or be zero (unspecified), and
/// tile-parts from other tiles may appear between two parts of the same tile.
fn tile_part_indices_by_tile(
    parts: &codestream::CodestreamView<'_>,
    header: &CodestreamHeader,
) -> Result<Vec<Vec<usize>>> {
    let tiles_x = header.siz.width.div_ceil(header.siz.tile_width);
    let tiles_y = header.siz.height.div_ceil(header.siz.tile_height);
    let tile_count = u64::from(tiles_x)
        .checked_mul(u64::from(tiles_y))
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("SIZ tile count overflow".into()))?;
    if tile_count > usize::from(u16::MAX) {
        return Err(crate::Jp2LamError::UnsupportedFeature(
            "tile grid exceeds the Part 1 Isot index range".to_string(),
        ));
    }

    let mut tile_parts = vec![Vec::new(); tile_count];
    let mut next_part_index = vec![0u16; tile_count];

    for (stream_index, tile_part) in parts.tile_parts.iter().enumerate() {
        let tile_index = usize::from(tile_part.header.tile_index);
        if tile_index >= tile_count {
            return Err(crate::Jp2LamError::DecodeFailed(format!(
                "tile-part {stream_index} refers to tile {tile_index}, but SIZ defines {tile_count} tiles"
            )));
        }
        // TPsot ordering IS load-bearing (parts are concatenated in this order),
        // so a tile's parts must still arrive as 0, 1, 2, …. TNsot, however, is
        // only advisory: real Kakadu-encoded files routinely emit one more
        // tile-part than TNsot declares (e.g. 6 parts with TNsot=5), and both
        // OpenJPEG and PDFium decode them. We therefore accept however many
        // ordered parts arrive and do not treat TNsot as a hard bound.
        if u16::from(tile_part.header.part_index) != next_part_index[tile_index] {
            return Err(crate::Jp2LamError::DecodeFailed(format!(
                "tile {tile_index} tile-part order is invalid: expected TPsot {}, found {}",
                next_part_index[tile_index], tile_part.header.part_index
            )));
        }
        // Quantization/coding overrides (COD/COC/QCD/QCC) are honored per tile
        // during decode by `CodestreamHeader::with_tile_overrides` (the
        // authoritative semantic gate). Permit them here and reject up front
        // only markers that would reshape Tier-2 packet iteration per tile
        // (RGN/POC/PPT/PPM). PLT/COM are inert hints.
        for segment in &tile_part.header_segments {
            let marker = segment
                .get(0..2)
                .map(|m| u16::from_be_bytes([m[0], m[1]]))
                .unwrap_or(0);
            if !matches!(
                marker,
                crate::j2k::MARKER_PLT
                    | crate::j2k::MARKER_COM
                    | crate::j2k::MARKER_COD
                    | crate::j2k::MARKER_COC
                    | crate::j2k::MARKER_QCD
                    | crate::j2k::MARKER_QCC
            ) {
                return Err(crate::Jp2LamError::UnsupportedFeature(format!(
                    "unsupported per-tile marker 0x{marker:04x} in tile-part header"
                )));
            }
        }
        next_part_index[tile_index] += 1;
        tile_parts[tile_index].push(stream_index);
    }

    for (tile_index, parts_for_tile) in tile_parts.iter().enumerate() {
        if parts_for_tile.is_empty() {
            return Err(crate::Jp2LamError::DecodeFailed(format!(
                "SIZ tile {tile_index} has no tile-part"
            )));
        }
    }

    Ok(tile_parts)
}

fn tile_rect(header: &CodestreamHeader, tile_index: u16) -> Result<(u32, u32, u32, u32)> {
    let siz = &header.siz;
    let tiles_x = siz.width.div_ceil(siz.tile_width);
    let index = u32::from(tile_index);
    let tile_x = index % tiles_x;
    let tile_y = index / tiles_x;
    let x0 = tile_x
        .checked_mul(siz.tile_width)
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("tile x origin overflow".into()))?;
    let y0 = tile_y
        .checked_mul(siz.tile_height)
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("tile y origin overflow".into()))?;
    let x1 = x0.saturating_add(siz.tile_width).min(siz.width);
    let y1 = y0.saturating_add(siz.tile_height).min(siz.height);
    Ok((x0, y0, x1 - x0, y1 - y0))
}

fn tile_local_header(
    header: &CodestreamHeader,
    x0: u32,
    y0: u32,
    width: u32,
    height: u32,
) -> CodestreamHeader {
    let mut local = header.clone();
    // Preserve the tile-component reference-grid phase for Annex B/F geometry.
    // Width and height remain tile-local while these origin fields carry the
    // interval start consumed by Tier-2 and inverse DWT reconstruction.
    local.siz.x_origin = x0;
    local.siz.y_origin = y0;
    local.siz.width = width;
    local.siz.height = height;
    local.siz.tile_width = width;
    local.siz.tile_height = height;
    local
}

fn select_reduce_levels(header: &CodestreamHeader, resolution: DecodeResolution) -> Result<u8> {
    let levels = header.cod.decomposition_levels;
    match resolution {
        DecodeResolution::Full => Ok(0),
        DecodeResolution::ReduceLevels(reduce) if reduce <= levels => Ok(reduce),
        DecodeResolution::ReduceLevels(reduce) => Err(crate::Jp2LamError::InvalidInput(format!(
            "requested {reduce} reduction levels, but COD defines only {levels}"
        ))),
        DecodeResolution::AtLeast {
            width,
            height,
            quality_margin,
        } => {
            if width == 0 || height == 0 || !quality_margin.is_finite() || quality_margin < 1.0 {
                return Err(crate::Jp2LamError::InvalidInput(
                    "AtLeast resolution needs non-zero dimensions and a finite quality margin >= 1"
                        .into(),
                ));
            }
            let required_width = (width as f64 * f64::from(quality_margin)).ceil() as u64;
            let required_height = (height as f64 * f64::from(quality_margin)).ceil() as u64;
            let full_x0 = header.siz.x_origin;
            let full_y0 = header.siz.y_origin;
            let full_x1 = full_x0.saturating_add(header.siz.width);
            let full_y1 = full_y0.saturating_add(header.siz.height);
            let mut selected = 0;
            for reduce in 1..=levels {
                let (x0, x1) = reduced_axis_bounds(full_x0, full_x1, reduce);
                let (y0, y1) = reduced_axis_bounds(full_y0, full_y1, reduce);
                if u64::from(x1 - x0) >= required_width && u64::from(y1 - y0) >= required_height {
                    selected = reduce;
                } else {
                    break;
                }
            }
            Ok(selected)
        }
    }
}

fn reduced_tile_header(header: &CodestreamHeader, reduce_levels: u8) -> CodestreamHeader {
    if reduce_levels == 0 {
        return header.clone();
    }
    let mut reduced = header.clone();
    let full_x1 = header.siz.x_origin + header.siz.width;
    let full_y1 = header.siz.y_origin + header.siz.height;
    let (x0, x1) = reduced_axis_bounds(header.siz.x_origin, full_x1, reduce_levels);
    let (y0, y1) = reduced_axis_bounds(header.siz.y_origin, full_y1, reduce_levels);
    reduced.siz.x_origin = x0;
    reduced.siz.y_origin = y0;
    reduced.siz.width = x1 - x0;
    reduced.siz.height = y1 - y0;
    reduced.siz.tile_width = reduced.siz.width;
    reduced.siz.tile_height = reduced.siz.height;
    reduced.cod.decomposition_levels -= reduce_levels;
    reduced
}

fn reduced_axis_bounds(mut start: u32, mut end: u32, reduce_levels: u8) -> (u32, u32) {
    for _ in 0..reduce_levels {
        start = start.div_ceil(2);
        end = end.div_ceil(2);
    }
    (start, end)
}

fn pack_image_8bit(image: &Image, format: DecodeOutputFormat) -> Result<DecodedRaster> {
    let expected_components = match format {
        DecodeOutputFormat::Gray8 => 1,
        DecodeOutputFormat::Rgb8 => 3,
        DecodeOutputFormat::Rgba8 | DecodeOutputFormat::Cmyk8 => 4,
        DecodeOutputFormat::NativePlanarI32 => {
            return Err(crate::Jp2LamError::InvalidInput(
                "native planar output is not a packed raster format".into(),
            ));
        }
    };
    if image.components.len() != expected_components {
        return Err(crate::Jp2LamError::InvalidInput(format!(
            "{format:?} output requires {expected_components} components, decoded image has {}",
            image.components.len()
        )));
    }
    let width = image.width as usize;
    let height = image.height as usize;
    let stride = width
        .checked_mul(expected_components)
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("packed output stride overflow".into()))?;
    let len = stride
        .checked_mul(height)
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("packed output size overflow".into()))?;
    let mut data = Vec::with_capacity(len);
    for index in 0..width * height {
        for component in &image.components {
            let sample = component.data[index].max(0) as u64;
            let max_sample = (1u64 << component.precision.min(31)) - 1;
            let scaled = if max_sample == 255 {
                sample.min(255)
            } else {
                sample.saturating_mul(255).saturating_add(max_sample / 2) / max_sample.max(1)
            };
            data.push(scaled.min(255) as u8);
        }
    }
    Ok(DecodedRaster {
        width: image.width,
        height: image.height,
        stride,
        format,
        data,
    })
}

/// Allocate the zero-filled destination canvas for a multi-tile decode at the
/// selected reduced resolution. Dimensions come from the phase-aware reduced
/// bounds of the full reference grid so multi-tile stitching lands each tile at
/// the right reduced offset.
fn empty_decoded_image_reduced(
    header: &CodestreamHeader,
    colorspace: ColorSpace,
    reduce_levels: u8,
) -> Result<Image> {
    let siz = &header.siz;
    let (x0, x1) = reduced_axis_bounds(siz.x_origin, siz.x_origin + siz.width, reduce_levels);
    let (y0, y1) = reduced_axis_bounds(siz.y_origin, siz.y_origin + siz.height, reduce_levels);
    let width = x1 - x0;
    let height = y1 - y0;
    let sample_count = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("decoded image size overflow".into()))?;
    Ok(Image {
        width,
        height,
        colorspace,
        components: header
            .siz
            .components
            .iter()
            .map(|component| crate::model::Component {
                data: vec![0; sample_count],
                width,
                height,
                precision: u32::from(component.precision),
                signed: component.signed,
                dx: u32::from(component.dx),
                dy: u32::from(component.dy),
            })
            .collect(),
    })
}

/// Expand a decoded single-component index image into the container's output
/// channels via a resolved `pclr`+`cmap` palette (ISO/IEC 15444-1 §I.5.3.4).
fn expand_palette(
    index_image: &Image,
    palette: &jp2_parse::Palette,
    colorspace: ColorSpace,
) -> Result<Image> {
    let index = index_image
        .components
        .first()
        .ok_or_else(|| crate::Jp2LamError::DecodeFailed("palette input has no component".into()))?;
    let (width, height) = (index_image.width, index_image.height);
    let components = palette
        .output_columns
        .iter()
        .map(|column| {
            let data = index
                .data
                .iter()
                .map(|&sample| {
                    let idx = sample.clamp(0, column.len().saturating_sub(1) as i32) as usize;
                    column.get(idx).copied().unwrap_or(0) as i32
                })
                .collect();
            crate::model::Component {
                data,
                width,
                height,
                precision: 8,
                signed: false,
                dx: 1,
                dy: 1,
            }
        })
        .collect();
    Ok(Image {
        width,
        height,
        colorspace,
        components,
    })
}

fn stitch_tile(image: &mut Image, tile: &Image, x0: u32, y0: u32) -> Result<()> {
    if image.components.len() != tile.components.len() {
        return Err(crate::Jp2LamError::DecodeFailed(
            "decoded tile component count mismatch".into(),
        ));
    }
    let image_width = image.width as usize;
    let tile_width = tile.width as usize;
    for (destination, source) in image.components.iter_mut().zip(&tile.components) {
        for row in 0..tile.height as usize {
            let src_start = row * tile_width;
            let dst_start = (y0 as usize + row) * image_width + x0 as usize;
            destination.data[dst_start..dst_start + tile_width]
                .copy_from_slice(&source.data[src_start..src_start + tile_width]);
        }
    }
    Ok(())
}

fn validate_jp2_decode_scope(
    header: &jp2_parse::Jp2Header,
    codestream: &CodestreamHeader,
) -> Result<()> {
    if header.width != codestream.siz.width || header.height != codestream.siz.height {
        return Err(crate::Jp2LamError::DecodeFailed(format!(
            "JP2 ihdr dimensions {}x{} do not match SIZ dimensions {}x{}",
            header.width, header.height, codestream.siz.width, codestream.siz.height
        )));
    }
    if !(8..=16).contains(&header.bits_per_component) {
        return Err(crate::Jp2LamError::UnsupportedFeature(format!(
            "unsupported JP2 bit depth: decoder supports 8..=16-bit components, found {} bits",
            header.bits_per_component
        )));
    }
    if header.component_count != codestream.siz.components.len() as u16 {
        return Err(crate::Jp2LamError::DecodeFailed(format!(
            "JP2 ihdr component count {} does not match SIZ component count {}",
            header.component_count,
            codestream.siz.components.len()
        )));
    }
    // A palettized image carries exactly one index component in the codestream;
    // the palette expands it to the container colorspace's channel count. The
    // remaining checks apply to that post-expansion channel count.
    if let Some(palette) = &header.palette {
        if codestream.siz.components.len() != 1 {
            return Err(crate::Jp2LamError::DecodeFailed(format!(
                "palettized JP2 must have one index component, found {}",
                codestream.siz.components.len()
            )));
        }
        let expected = match header.colorspace {
            ColorSpace::Gray => 1,
            ColorSpace::Srgb => 3,
            ColorSpace::Cmyk => 4,
            other => {
                return Err(crate::Jp2LamError::UnsupportedFeature(format!(
                    "unsupported palettized JP2 colorspace {other:?}"
                )));
            }
        };
        if palette.channel_count() != expected {
            return Err(crate::Jp2LamError::DecodeFailed(format!(
                "palette produces {} channels but {:?} expects {expected}",
                palette.channel_count(),
                header.colorspace
            )));
        }
        return Ok(());
    }
    if codestream
        .siz
        .components
        .iter()
        .any(|component| component.precision != header.bits_per_component)
    {
        return Err(crate::Jp2LamError::DecodeFailed(
            "JP2 ihdr bit depth does not match SIZ component precision".into(),
        ));
    }
    if header.colorspace == ColorSpace::Gray && header.component_count != 1 {
        return Err(crate::Jp2LamError::UnsupportedFeature(format!(
            "unsupported JP2 component count: decoder currently supports one grayscale component, found {} components",
            header.component_count
        )));
    }
    if header.colorspace == ColorSpace::Srgb && header.component_count != 3 {
        // sRGB + a single `cdef` opacity channel on component 3 is an RGBA image
        // whose alpha the PDF layer applies as an in-data soft mask; accept it.
        // Any other sRGB + N is still unsupported.
        let is_rgba = header.component_count == 4
            && header.alpha.is_some_and(|alpha| alpha.component == 3);
        if !is_rgba {
            return Err(crate::Jp2LamError::UnsupportedFeature(format!(
                "unsupported JP2 component count: decoder currently supports three sRGB components, found {} components",
                header.component_count
            )));
        }
    }
    if header.colorspace == ColorSpace::Cmyk && header.component_count != 4 {
        return Err(crate::Jp2LamError::UnsupportedFeature(format!(
            "unsupported JP2 component count: CMYK requires four components, found {} components",
            header.component_count
        )));
    }
    if header.colorspace == ColorSpace::YCbCr {
        // sYCC (EnumCS 18) carries three Y/Cb/Cr planes with no MCT; the decoder
        // applies the inverse sYCC→sRGB matrix. An MCT-decorrelated stream can't
        // simultaneously be sYCC, so a signalled MCT is contradictory.
        if header.component_count != 3 {
            return Err(crate::Jp2LamError::UnsupportedFeature(format!(
                "unsupported JP2 component count: sYCC requires three components, found {} components",
                header.component_count
            )));
        }
        if codestream.cod.use_mct {
            return Err(crate::Jp2LamError::UnsupportedFeature(
                "unsupported JP2 feature: sYCC combined with the multiple-component transform".into(),
            ));
        }
    }
    if header.colorspace != ColorSpace::Gray
        && header.colorspace != ColorSpace::Srgb
        && header.colorspace != ColorSpace::Cmyk
        && header.colorspace != ColorSpace::YCbCr
    {
        return Err(crate::Jp2LamError::UnsupportedFeature(format!(
            "unsupported JP2 colorspace: decoder currently supports EnumCS=17 grayscale, EnumCS=16 sRGB, EnumCS=18 sYCC, and EnumCS=12 CMYK, found {:?}",
            header.colorspace
        )));
    }
    Ok(())
}

/// Read a complete JP2 stream from memory-backed or file-backed input and
/// decode it into an [`Image`].
///
/// Prefer [`decode_jp2`] when the bytes are already available; it borrows the
/// input buffer through JP2 parsing, codestream framing, and Tier-2 packet
/// parsing instead of copying tile payload bytes.
pub fn decode_from_reader<R: Read>(reader: &mut R) -> Result<Image> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|err| crate::Jp2LamError::Io(format!("failed to read JP2 input: {err}")))?;
    decode_jp2(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_org_sample_header_matches_decoder_scope() {
        let Some(bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let metadata = inspect_jp2(&bytes).expect("inspect sample jp2");

        assert_eq!(metadata.width, 3494);
        assert_eq!(metadata.height, 4967);
        assert_eq!(metadata.colorspace, ColorSpace::Gray);
        assert!(!metadata.has_ipr_metadata);
        assert_eq!(metadata.tile_part_count, 1);
        assert!(metadata.first_tile_payload_len > 1_000_000);

        let siz = &metadata.codestream.siz;
        assert_eq!(siz.width, 3494);
        assert_eq!(siz.height, 4967);
        assert_eq!(siz.tile_width, 3494);
        assert_eq!(siz.tile_height, 4967);
        assert_eq!(siz.components.len(), 1);
        assert_eq!(siz.components[0].precision, 8);
        assert!(!siz.components[0].signed);
        assert_eq!(siz.components[0].dx, 1);
        assert_eq!(siz.components[0].dy, 1);

        let cod = &metadata.codestream.cod;
        assert_eq!(cod.progression_order, ProgressionOrder::Lrcp);
        assert_eq!(cod.layers, 1);
        assert_eq!(cod.decomposition_levels, 5);
        assert_eq!(cod.code_block_width, 64);
        assert_eq!(cod.code_block_height, 64);
        assert_eq!(cod.code_block_style, CodeBlockStyle::default());
        assert_eq!(cod.transform, WaveletTransform::Irreversible97);
        assert!(!cod.uses_precincts);
        assert!(!cod.sop_markers);
        assert!(!cod.eph_markers);

        let qcd = &metadata.codestream.qcd;
        assert_eq!(qcd.style, QuantizationStyle::ScalarExpounded);
        assert_eq!(qcd.guard_bits, 1);
        assert_eq!(qcd.steps.len(), 16);
        assert_eq!(metadata.codestream.comment_count, 2);
    }

    #[test]
    fn archive_org_rgb_sample_header_matches_decoder_scope() {
        let Some(bytes) = maybe_read_archive_org_rgb_sample() else {
            return;
        };
        let metadata = inspect_jp2(&bytes).expect("inspect rgb sample jp2");

        assert_eq!(metadata.width, 6000);
        assert_eq!(metadata.height, 4000);
        assert_eq!(metadata.colorspace, ColorSpace::Srgb);
        assert_eq!(metadata.tile_part_count, 1);

        let siz = &metadata.codestream.siz;
        assert_eq!(siz.components.len(), 3);
        for component in &siz.components {
            assert_eq!(component.precision, 8);
            assert!(!component.signed);
            assert_eq!(component.dx, 1);
            assert_eq!(component.dy, 1);
        }

        let cod = &metadata.codestream.cod;
        assert_eq!(cod.progression_order, ProgressionOrder::Lrcp);
        assert_eq!(cod.layers, 1);
        assert!(cod.use_mct);
        assert_eq!(cod.decomposition_levels, 5);
        assert_eq!(cod.code_block_width, 64);
        assert_eq!(cod.code_block_height, 64);
        assert_eq!(cod.code_block_style, CodeBlockStyle::default());
        assert_eq!(cod.transform, WaveletTransform::Irreversible97);
        assert!(!cod.uses_precincts);

        let qcd = &metadata.codestream.qcd;
        assert_eq!(qcd.style, QuantizationStyle::ScalarExpounded);
        assert_eq!(qcd.guard_bits, 1);
        assert_eq!(qcd.steps.len(), 16);
    }

    #[test]
    fn inspect_jp2_rejects_jp2_codestream_component_mismatch() {
        let Some(mut bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let ihdr = find_box_payload(&bytes, b"ihdr").expect("find ihdr");
        bytes[ihdr + 8..ihdr + 10].copy_from_slice(&2u16.to_be_bytes());

        let err = inspect_jp2(&bytes)
            .expect_err("JP2/SIZ component mismatch should fail during inspection")
            .to_string();

        assert!(err.contains("JP2 ihdr component count"), "{err}");
    }

    #[test]
    fn inspect_jp2_rejects_jp2_codestream_dimension_mismatch() {
        let Some(mut bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let ihdr = find_box_payload(&bytes, b"ihdr").expect("find ihdr");
        bytes[ihdr + 4..ihdr + 8].copy_from_slice(&1234u32.to_be_bytes());

        let err = inspect_jp2(&bytes)
            .expect_err("JP2/SIZ dimension mismatch should fail during inspection")
            .to_string();

        assert!(err.contains("JP2 ihdr dimensions"), "{err}");
    }

    #[test]
    fn decode_jp2_rejects_truncated_tile_payload_before_image_output() {
        let Some(bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let truncated = bytes[..bytes.len() - 4096].to_vec();

        let err =
            decode_jp2(&truncated).expect_err("truncated JP2 should fail before reconstruction");

        assert!(
            matches!(err, crate::Jp2LamError::DecodeFailed(_)),
            "{err:?}"
        );
        let err = err.to_string();
        assert!(
            err.contains("packet body extends past tile payload")
                || err.contains("tile payload has")
                || err.contains("tile-part length exceeded codestream size")
                || err.contains("EOC"),
            "{err}"
        );
    }

    #[test]
    fn decode_errors_expose_matchable_unsupported_feature_variant() {
        let Some(mut bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let ihdr = find_box_payload(&bytes, b"ihdr").expect("find ihdr");
        bytes[ihdr + 10] = 15;

        let err = inspect_jp2(&bytes).expect_err("unsupported JP2 bit depth should be matchable");

        assert!(err.is_decode_failure());
        assert!(err.is_unsupported_feature());
        assert!(
            matches!(err, crate::Jp2LamError::UnsupportedFeature(_)),
            "{err:?}"
        );
        assert!(err.message().contains("bit depth"));
    }

    #[test]
    #[ignore = "scans the full provided Archive.org RGB JP2 directory"]
    fn archive_org_rgb_directory_headers_match_decoder_scope() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/moreboysgirlsofh0000rhod_e0h1_orig_jp2"
        );
        let mut count = 0usize;
        for entry in std::fs::read_dir(dir).expect("read rgb jp2 directory") {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jp2") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read rgb jp2 page");
            let metadata = inspect_jp2(&bytes)
                .unwrap_or_else(|err| panic!("inspect {}: {err}", path.display()));
            assert_eq!(metadata.colorspace, ColorSpace::Srgb, "{}", path.display());
            assert_eq!(
                metadata.codestream.siz.components.len(),
                3,
                "{}",
                path.display()
            );
            assert_eq!(
                metadata.codestream.cod.progression_order,
                ProgressionOrder::Lrcp,
                "{}",
                path.display()
            );
            assert_eq!(metadata.codestream.cod.layers, 1, "{}", path.display());
            assert!(metadata.codestream.cod.use_mct, "{}", path.display());
            assert_eq!(
                metadata.codestream.cod.transform,
                WaveletTransform::Irreversible97,
                "{}",
                path.display()
            );
            assert_eq!(
                metadata.codestream.qcd.style,
                QuantizationStyle::ScalarExpounded,
                "{}",
                path.display()
            );
            count += 1;
        }
        assert!(count > 0, "expected at least one RGB JP2 page");
    }

    #[test]
    #[ignore = "decodes representative full-size Archive.org RGB JP2 pages"]
    fn archive_org_rgb_representative_pages_decode() {
        for name in [
            "moreboysgirlsofh0000rhod_e0h1_orig_0000.jp2",
            "moreboysgirlsofh0000rhod_e0h1_orig_0138.jp2",
            "moreboysgirlsofh0000rhod_e0h1_orig_0291.jp2",
        ] {
            let path = archive_org_rgb_path(name);
            let bytes = std::fs::read(&path).expect("read rgb jp2 page");
            let image =
                decode_jp2(&bytes).unwrap_or_else(|err| panic!("decode {}: {err}", path.display()));

            assert_eq!(image.width, 6000, "{}", path.display());
            assert_eq!(image.height, 4000, "{}", path.display());
            assert_eq!(image.colorspace, ColorSpace::Srgb, "{}", path.display());
            assert_eq!(image.components.len(), 3, "{}", path.display());
            assert_decoded_components_are_8bit_full_size(&image);
            assert!(image.components.iter().all(|component| {
                component.data.iter().any(|&sample| sample != 0)
                    && component.data.iter().any(|&sample| sample != 255)
            }));
        }
    }

    #[test]
    #[ignore = "decodes every provided Archive.org RGB JP2 page; expensive in debug builds"]
    fn archive_org_rgb_directory_decodes_all_pages() {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/moreboysgirlsofh0000rhod_e0h1_orig_jp2"
        );
        let mut paths = std::fs::read_dir(dir)
            .expect("read rgb jp2 directory")
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jp2"))
            .collect::<Vec<_>>();
        paths.sort();

        let mut count = 0usize;
        for path in paths {
            let bytes = std::fs::read(&path).expect("read rgb jp2 page");
            let image =
                decode_jp2(&bytes).unwrap_or_else(|err| panic!("decode {}: {err}", path.display()));
            assert_eq!(image.colorspace, ColorSpace::Srgb, "{}", path.display());
            assert_decoded_components_are_8bit_full_size(&image);
            count += 1;
        }
        assert!(count > 0, "expected at least one RGB JP2 page");
    }

    #[test]
    #[ignore = "requires ImageMagick; compares decoder output against an independent JP2 decoder"]
    fn archive_org_gray_crop_matches_imagemagick() {
        if !imagemagick_available() {
            eprintln!("skipping ImageMagick comparison because `magick` is not available");
            return;
        }

        let bytes = read_archive_org_sample();
        let image = decode_jp2(&bytes).expect("decode grayscale sample");
        let reference = imagemagick_raw_crop(
            &archive_org_gray_path(),
            RawCrop {
                x: 256,
                y: 256,
                width: 96,
                height: 96,
                channels: 1,
            },
        );

        assert_crop_close_to_reference(&image, 256, 256, 96, 96, &reference, 1, 1, 0.05);
    }

    #[test]
    #[ignore = "requires ImageMagick; compares decoder output against an independent JP2 decoder"]
    fn archive_org_rgb_crop_matches_imagemagick() {
        if !imagemagick_available() {
            eprintln!("skipping ImageMagick comparison because `magick` is not available");
            return;
        }

        let path = archive_org_rgb_path("moreboysgirlsofh0000rhod_e0h1_orig_0000.jp2");
        let bytes = std::fs::read(&path).expect("read rgb jp2 page");
        let image = decode_jp2(&bytes).expect("decode rgb sample");
        let reference = imagemagick_raw_crop(
            &path,
            RawCrop {
                x: 128,
                y: 128,
                width: 64,
                height: 64,
                channels: 3,
            },
        );

        assert_crop_close_to_reference(&image, 128, 128, 64, 64, &reference, 3, 3, 0.25);
    }

    #[test]
    fn archive_org_sample_packet_headers_split_tile_payload() {
        let Some(bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let parsed = jp2_parse::parse_jp2(&bytes).expect("parse jp2");
        let parts = codestream::parse_codestream_view(parsed.codestream).expect("parse j2k");
        let codestream = CodestreamHeader::from_marker_segments_with_tile_headers(
            parts.main_header_segments.iter().copied(),
            parts.tile_parts[0].header_segments.iter().copied(),
            parts.tile_parts[0].header,
            parts.tile_parts.len(),
        )
        .expect("decode headers");
        let payload = parts.tile_parts[0].payload;
        let packets =
            t2::parse_tile_part_payload(&codestream, payload).expect("parse packet headers");

        assert_eq!(
            packets.packets.len(),
            codestream.cod.layers as usize * (usize::from(codestream.cod.decomposition_levels) + 1)
        );
        assert_eq!(
            packets
                .packets
                .iter()
                .map(|packet| packet.header_len + packet.body_len)
                .sum::<usize>(),
            payload.len()
        );
        assert!(!packets.codeblocks.is_empty());
        assert!(packets.codeblocks.iter().any(|block| block.passes > 0));
    }

    #[test]
    fn archive_org_sample_tier1_decodes_quantized_coefficients() {
        let Some(bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let parsed = jp2_parse::parse_jp2(&bytes).expect("parse jp2");
        let parts = codestream::parse_codestream_view(parsed.codestream).expect("parse j2k");
        let codestream = CodestreamHeader::from_marker_segments_with_tile_headers(
            parts.main_header_segments.iter().copied(),
            parts.tile_parts[0].header_segments.iter().copied(),
            parts.tile_parts[0].header,
            parts.tile_parts.len(),
        )
        .expect("decode headers");
        let payload = parts.tile_parts[0].payload;
        let packets =
            t2::parse_tile_part_payload(&codestream, payload).expect("parse packet headers");
        let tile =
            t1::decode_tile_coefficients(&codestream, &packets).expect("decode tier1 blocks");

        assert_eq!(tile.width, 3494);
        assert_eq!(tile.height, 4967);
        // The Archive.org grayscale sample is irreversible 9/7, so Tier-1 now
        // emits the fused, already-dequantized `f32` subband plane.
        let plane = tile.into_real().expect("irreversible coefficient plane");
        assert_eq!(plane.len(), 3494 * 4967);
        assert!(plane.iter().any(|&coefficient| coefficient != 0.0));
    }

    #[test]
    fn archive_org_sample_reconstructs_grayscale_image() {
        let Some(bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let image = decode_jp2(&bytes).expect("decode sample jp2");

        assert_eq!(image.width, 3494);
        assert_eq!(image.height, 4967);
        assert_eq!(image.colorspace, ColorSpace::Gray);
        assert_eq!(image.components.len(), 1);
        assert_eq!(image.components[0].data.len(), 3494 * 4967);
        assert!(
            image.components[0]
                .data
                .iter()
                .all(|&sample| (0..=255).contains(&sample))
        );
        assert!(image.components[0].data.iter().any(|&sample| sample != 0));
        assert!(image.components[0].data.iter().any(|&sample| sample != 255));
    }

    #[test]
    fn decode_jp2_roundtrips_native_gray_lossless() {
        let width = 32;
        let height = 32;
        let samples = (0..height)
            .flat_map(|y| (0..width).map(move |x| ((x * 7 + y * 11 + (x ^ y)) & 0xff) as u8))
            .collect::<Vec<_>>();
        let image = Image::from_gray_bytes(width, height, &samples).expect("source image");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 100,
                format: crate::OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("encode jp2");

        let decoded = decode_jp2(&encoded).expect("decode jp2");
        assert_eq!(decoded.width, width);
        assert_eq!(decoded.height, height);
        assert_eq!(decoded.colorspace, ColorSpace::Gray);
        assert_eq!(decoded.components[0].data, image.components[0].data);
    }

    #[test]
    fn reduced_gray_decode_omits_high_resolution_codeblocks() {
        let width = 257;
        let height = 129;
        let image =
            Image::from_gray_bytes(width, height, &gray_fixture(width, height)).expect("source");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 75,
                format: crate::OutputFormat::Jp2,
                tile_policy: crate::TilePolicy::Single,
                ..Default::default()
            },
        )
        .expect("encode lossy jp2");

        let result = decode_jp2_request(
            &encoded,
            &DecodeRequest {
                resolution: DecodeResolution::ReduceLevels(1),
                output: DecodeOutputFormat::Gray8,
                ..Default::default()
            },
        )
        .expect("reduced decode");
        let DecodeResult::Raster(raster) = result else {
            panic!("expected packed raster");
        };
        assert_eq!((raster.width, raster.height), (129, 65));
        assert_eq!(raster.stride, 129);
        assert_eq!(raster.data.len(), 129 * 65);
        assert!(raster.data.iter().any(|&sample| sample != 0));

        let at_least = decode_jp2_request(
            &encoded,
            &DecodeRequest {
                resolution: DecodeResolution::AtLeast {
                    width: 100,
                    height: 50,
                    quality_margin: 1.25,
                },
                output: DecodeOutputFormat::Gray8,
                ..Default::default()
            },
        )
        .expect("AtLeast decode");
        let DecodeResult::Raster(at_least) = at_least else {
            panic!("expected packed AtLeast raster");
        };
        assert_eq!((at_least.width, at_least.height), (129, 65));
    }

    #[test]
    fn reduced_rgb_decode_interleaves_directly() {
        let width = 129;
        let height = 65;
        let samples = (0..height)
            .flat_map(|y| {
                (0..width).flat_map(move |x| {
                    [
                        ((x * 5 + y * 3) & 0xff) as u8,
                        ((x * 2 + y * 11) & 0xff) as u8,
                        ((x * 13 + y * 7) & 0xff) as u8,
                    ]
                })
            })
            .collect::<Vec<_>>();
        let image = Image::from_rgb_bytes(width, height, &samples).expect("source");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 75,
                format: crate::OutputFormat::Jp2,
                tile_policy: crate::TilePolicy::Single,
                ..Default::default()
            },
        )
        .expect("encode lossy RGB jp2");

        let result = decode_jp2_request(
            &encoded,
            &DecodeRequest {
                resolution: DecodeResolution::ReduceLevels(1),
                output: DecodeOutputFormat::Rgb8,
                ..Default::default()
            },
        )
        .expect("reduced RGB decode");
        let DecodeResult::Raster(raster) = result else {
            panic!("expected packed RGB raster");
        };
        assert_eq!((raster.width, raster.height), (65, 33));
        assert_eq!(raster.stride, 65 * 3);
        assert_eq!(raster.data.len(), 65 * 33 * 3);
        assert!(raster.data.iter().any(|&sample| sample != 0));
    }

    #[test]
    fn decoder_session_reuses_scratch_without_changing_output() {
        let width = 127;
        let height = 67;
        let image =
            Image::from_gray_bytes(width, height, &gray_fixture(width, height)).expect("source");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 75,
                format: crate::OutputFormat::Jp2,
                tile_policy: crate::TilePolicy::Single,
                ..Default::default()
            },
        )
        .expect("encode lossy jp2");
        let request = DecodeRequest {
            resolution: DecodeResolution::ReduceLevels(1),
            output: DecodeOutputFormat::Gray8,
            ..Default::default()
        };
        let mut decoder = Jp2Decoder::new();

        let first = decoder.decode(&encoded, &request).expect("first decode");
        let mut parallel_request = request.clone();
        parallel_request.concurrency = DecodeConcurrency::Budgeted(2);
        let second = decoder
            .decode(&encoded, &parallel_request)
            .expect("second decode");
        let (DecodeResult::Raster(first), DecodeResult::Raster(second)) = (first, second) else {
            panic!("expected packed rasters");
        };
        assert_eq!(first, second);
    }

    #[test]
    fn serial_and_budgeted_tier1_are_byte_identical() {
        // Large enough that every high-frequency subband holds several 64x64
        // code blocks, so the parallel Tier-1 loop fans real work across
        // workers and its disjoint plane writes must reproduce the serial path.
        let width = 300;
        let height = 220;
        for quality in [75, 100] {
            let image = Image::from_gray_bytes(width, height, &gray_fixture(width, height))
                .expect("source image");
            let encoded = crate::encode(
                &image,
                &crate::EncodeOptions {
                    quality,
                    format: crate::OutputFormat::Jp2,
                    tile_policy: crate::TilePolicy::Single,
                    ..Default::default()
                },
            )
            .expect("encode jp2");

            let serial = decode_jp2_request(
                &encoded,
                &DecodeRequest {
                    concurrency: DecodeConcurrency::Serial,
                    ..Default::default()
                },
            )
            .expect("serial decode");
            let budgeted = decode_jp2_request(
                &encoded,
                &DecodeRequest {
                    concurrency: DecodeConcurrency::Budgeted(4),
                    ..Default::default()
                },
            )
            .expect("budgeted decode");
            let (DecodeResult::Native(serial), DecodeResult::Native(budgeted)) = (serial, budgeted)
            else {
                panic!("expected native images");
            };
            assert_eq!(
                serial.components[0].data, budgeted.components[0].data,
                "quality {quality}: serial and Budgeted(4) Tier-1 output diverged"
            );
            // The compatibility path decodes on the global pool; confirm it too
            // matches the strictly serial output bit-for-bit.
            let compat = decode_jp2(&encoded).expect("compat decode");
            assert_eq!(
                serial.components[0].data, compat.components[0].data,
                "quality {quality}: decode_jp2 diverged from serial Tier-1"
            );
        }
    }

    #[test]
    fn multi_tile_reduced_decode_stitches_reduced_tiles() {
        let width = 300u32;
        let height = 220u32;
        // A smooth gradient so a reduced (LL-band) reconstruction tracks a 2:1
        // subsample of the source; a mis-placed tile would shift whole regions.
        let samples: Vec<u8> = (0..height)
            .flat_map(|y| {
                (0..width)
                    .map(move |x| ((x * 200 / width + y * 200 / height).min(255)) as u8)
            })
            .collect();
        let image = Image::from_gray_bytes(width, height, &samples).expect("source image");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 100,
                format: crate::OutputFormat::Jp2,
                tile_policy: crate::TilePolicy::Fixed {
                    width: 128,
                    height: 128,
                },
                ..Default::default()
            },
        )
        .expect("encode multi-tile jp2");

        let metadata = inspect_jp2(&encoded).expect("inspect multi-tile jp2");
        assert!(
            metadata.tile_part_count >= 4,
            "expected several tiles, found {}",
            metadata.tile_part_count
        );

        // Full-resolution multi-tile decode round-trips losslessly.
        let full = decode_jp2(&encoded).expect("full multi-tile decode");
        assert_eq!((full.width, full.height), (width, height));
        assert_eq!(
            full.components[0].data,
            samples.iter().map(|&s| i32::from(s)).collect::<Vec<_>>()
        );

        // Reduced-by-one decode across every tile (previously rejected).
        let reduced = decode_jp2_request(
            &encoded,
            &DecodeRequest {
                resolution: DecodeResolution::ReduceLevels(1),
                output: DecodeOutputFormat::NativePlanarI32,
                ..Default::default()
            },
        )
        .expect("reduced multi-tile decode");
        let DecodeResult::Native(reduced) = reduced else {
            panic!("expected native image");
        };
        assert_eq!((reduced.width, reduced.height), (150, 110));
        assert_eq!(
            reduced.components[0].data.len(),
            150 * 110,
            "reduced canvas sample count"
        );

        let rw = reduced.width as usize;
        let (mut total, mut count) = (0i64, 0i64);
        for ry in 0..reduced.height as usize {
            for rx in 0..rw {
                let got = reduced.components[0].data[ry * rw + rx];
                let sx = (rx * 2).min(width as usize - 1);
                let sy = (ry * 2).min(height as usize - 1);
                let want = i32::from(samples[sy * width as usize + sx]);
                total += i64::from((got - want).abs());
                count += 1;
            }
        }
        let mad = total as f64 / count as f64;
        assert!(
            mad < 12.0,
            "reduced multi-tile decode diverged from a 2:1 source subsample: MAD={mad}"
        );
    }

    #[test]
    fn b11_multi_tile_parts_reassemble_one_tile_packet_sequence() {
        let width = 64;
        let height = 64;
        let samples = (0..height)
            .flat_map(|y| (0..width).map(move |x| ((x * 13 + y * 29 + (x ^ y)) & 0xff) as u8))
            .collect::<Vec<_>>();
        let image = Image::from_gray_bytes(width, height, &samples).expect("source image");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 100,
                format: crate::OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("encode jp2");

        // ISO/IEC 15444-1 B.11 permits both the exact part count and zero
        // (unspecified) for TNsot. Both streams contain the same two ordered
        // packet-boundary tile-parts.
        for declared_part_count in [2, 0] {
            let multi_part =
                split_native_jp2_tile_at_packet_boundary(&encoded, declared_part_count);
            let metadata = inspect_jp2(&multi_part).expect("inspect multi-part jp2");
            assert_eq!(metadata.tile_part_count, 2);

            let decoded = decode_jp2(&multi_part).expect("decode multi-part jp2");
            assert_eq!(decoded.components[0].data, image.components[0].data);
        }
    }

    #[test]
    fn a42_rejects_out_of_order_tile_part_indices() {
        let image = Image::from_gray_bytes(64, 64, &gray_fixture(64, 64)).expect("source image");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 100,
                format: crate::OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("encode jp2");
        let mut multi_part = split_native_jp2_tile_at_packet_boundary(&encoded, 2);
        let (codestream_start, codestream_end) = top_level_box_range(&multi_part, b"jp2c");
        let second_sot = multi_part[codestream_start..codestream_end]
            .windows(2)
            .enumerate()
            .filter_map(|(offset, marker)| (marker == [0xff, 0x90]).then_some(offset))
            .nth(1)
            .expect("second SOT marker");
        // TPsot is byte 10 of the fixed 12-byte SOT segment. Repeating zero
        // violates A.4.2's required per-tile ordering.
        multi_part[codestream_start + second_sot + 10] = 0;

        let err = decode_jp2(&multi_part)
            .expect_err("out-of-order tile-parts must be rejected")
            .to_string();
        assert!(err.contains("tile-part order is invalid"), "{err}");
    }

    #[test]
    fn b11_groups_interleaved_tile_parts_by_their_tile() {
        let image = Image::from_gray_bytes(64, 64, &gray_fixture(64, 64)).expect("source image");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 100,
                format: crate::OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("encode jp2");
        let mut header = inspect_jp2(&encoded).expect("inspect jp2").codestream;
        header.siz.width = 128;
        header.siz.tile_width = 64;

        let parts = codestream::CodestreamView {
            main_header_segments: Vec::new(),
            tile_parts: vec![
                tile_part_view(0, 0, 2),
                tile_part_view(1, 0, 2),
                tile_part_view(0, 1, 2),
                tile_part_view(1, 1, 2),
            ],
        };

        assert_eq!(
            tile_part_indices_by_tile(&parts, &header).expect("valid B.11 order"),
            vec![vec![0, 2], vec![1, 3]]
        );
    }

    #[test]
    fn tnsot_is_advisory_extra_tile_part_is_accepted() {
        // Real Kakadu-encoded archive.org scans emit one more tile-part than
        // TNsot declares (here TPsot 0..=5 with TNsot=5). OpenJPEG and PDFium
        // decode these; we must too, treating TNsot as advisory while still
        // requiring TPsot to arrive in order. See `tile_part_indices_by_tile`.
        let image = Image::from_gray_bytes(64, 64, &gray_fixture(64, 64)).expect("source image");
        let encoded = crate::encode(
            &image,
            &crate::EncodeOptions {
                quality: 100,
                format: crate::OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            },
        )
        .expect("encode jp2");
        let header = inspect_jp2(&encoded).expect("inspect jp2").codestream;

        let parts = codestream::CodestreamView {
            main_header_segments: Vec::new(),
            tile_parts: vec![
                tile_part_view(0, 0, 5),
                tile_part_view(0, 1, 5),
                tile_part_view(0, 2, 5),
                tile_part_view(0, 3, 5),
                tile_part_view(0, 4, 5),
                tile_part_view(0, 5, 5), // one past the declared TNsot=5
            ],
        };

        assert_eq!(
            tile_part_indices_by_tile(&parts, &header).expect("TNsot must be advisory"),
            vec![vec![0, 1, 2, 3, 4, 5]]
        );
    }

    #[test]
    fn decode_from_reader_matches_slice_decode() {
        let Some(bytes) = maybe_read_archive_org_sample() else {
            return;
        };
        let from_slice = decode_jp2(&bytes).expect("decode slice");
        let mut cursor = std::io::Cursor::new(&bytes);
        let from_reader = decode_from_reader(&mut cursor).expect("decode reader");

        assert_eq!(from_reader.width, from_slice.width);
        assert_eq!(from_reader.height, from_slice.height);
        assert_eq!(from_reader.colorspace, from_slice.colorspace);
        assert_eq!(
            from_reader.components[0].data,
            from_slice.components[0].data
        );
    }

    fn maybe_read_archive_org_sample() -> Option<Vec<u8>> {
        read_optional_fixture(&archive_org_gray_path(), "Archive.org grayscale JP2")
    }

    fn read_archive_org_sample() -> Vec<u8> {
        maybe_read_archive_org_sample().expect("read sample jp2")
    }

    fn maybe_read_archive_org_rgb_sample() -> Option<Vec<u8>> {
        read_optional_fixture(
            &archive_org_rgb_path("moreboysgirlsofh0000rhod_e0h1_orig_0000.jp2"),
            "Archive.org RGB JP2",
        )
    }

    fn read_archive_org_rgb_sample() -> Vec<u8> {
        maybe_read_archive_org_rgb_sample().expect("read rgb sample jp2")
    }

    fn maybe_read_rgba_alpha_sample() -> Option<Vec<u8>> {
        read_optional_fixture(
            &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("rgba_gradient_alpha_sample.jp2"),
            "sRGB+alpha JP2",
        )
    }

    /// An sRGB image with an in-data (`cdef`) opacity channel decodes to packed
    /// RGBA8: the three colour planes take the inverse ICT, the 4th (alpha) plane
    /// is reconstructed straight and interleaved. The fixture's alpha is a
    /// horizontal 0..255 gradient (losslessly coded), so it reconstructs exactly.
    #[test]
    fn srgb_alpha_decodes_to_rgba_with_gradient_mask() {
        let Some(bytes) = maybe_read_rgba_alpha_sample() else {
            return;
        };
        let meta = inspect_jp2(&bytes).expect("inspect rgba");
        assert_eq!(meta.colorspace, ColorSpace::Srgb);
        let alpha = meta.in_data_alpha.expect("in-data alpha channel");
        assert_eq!(alpha.component, 3);
        assert!(!alpha.premultiplied);

        let mut decoder = Jp2Decoder::new();
        let rgba = match decoder
            .decode(
                &bytes,
                &DecodeRequest {
                    resolution: DecodeResolution::Full,
                    output: DecodeOutputFormat::Rgba8,
                    region: None,
                    concurrency: DecodeConcurrency::Serial,
                },
            )
            .expect("rgba decode")
        {
            DecodeResult::Raster(raster) => raster,
            other => panic!("expected packed raster, got {other:?}"),
        };
        let w = rgba.width as usize;
        let h = rgba.height as usize;
        assert_eq!(rgba.stride, w * 4);
        for y in 0..h {
            let mut prev = 0u8;
            for x in 0..w {
                let a = rgba.data[(y * w + x) * 4 + 3];
                assert!(a >= prev, "alpha must be non-decreasing across x at row {y}");
                prev = a;
            }
            assert_eq!(rgba.data[y * w * 4 + 3], 0, "left column transparent");
            assert_eq!(rgba.data[(y * w + w - 1) * 4 + 3], 255, "right column opaque");
        }

        // Rgb8 on the same stream drops the alpha (three interleaved planes).
        let rgb = match decoder
            .decode(
                &bytes,
                &DecodeRequest {
                    resolution: DecodeResolution::Full,
                    output: DecodeOutputFormat::Rgb8,
                    region: None,
                    concurrency: DecodeConcurrency::Serial,
                },
            )
            .expect("rgb decode")
        {
            DecodeResult::Raster(raster) => raster,
            other => panic!("expected packed raster, got {other:?}"),
        };
        assert_eq!(rgb.stride, w * 3);
    }

    fn read_optional_fixture(path: &std::path::Path, label: &str) -> Option<Vec<u8>> {
        match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "skipping {label} fixture test because {} is not present",
                    path.display()
                );
                None
            }
            Err(err) => panic!("read {}: {err}", path.display()),
        }
    }

    fn find_box_payload(bytes: &[u8], box_type: &[u8; 4]) -> Option<usize> {
        fn walk_boxes(bytes: &[u8], box_type: &[u8; 4]) -> Option<usize> {
            let mut pos = 0usize;
            while pos + 8 <= bytes.len() {
                let start = pos;
                let lbox = u32::from_be_bytes([
                    bytes[pos],
                    bytes[pos + 1],
                    bytes[pos + 2],
                    bytes[pos + 3],
                ]) as usize;
                let current_type = &bytes[pos + 4..pos + 8];
                pos += 8;
                let (payload_start, end) = if lbox == 1 {
                    if pos + 8 > bytes.len() {
                        return None;
                    }
                    let xlbox = u64::from_be_bytes([
                        bytes[pos],
                        bytes[pos + 1],
                        bytes[pos + 2],
                        bytes[pos + 3],
                        bytes[pos + 4],
                        bytes[pos + 5],
                        bytes[pos + 6],
                        bytes[pos + 7],
                    ]) as usize;
                    pos += 8;
                    (pos, start.checked_add(xlbox)?)
                } else if lbox == 0 {
                    (pos, bytes.len())
                } else {
                    (pos, start.checked_add(lbox)?)
                };
                if end > bytes.len() || end < payload_start {
                    return None;
                }
                if current_type == box_type {
                    return Some(payload_start);
                }
                if current_type == b"jp2h" {
                    if let Some(found) = walk_boxes(&bytes[payload_start..end], box_type) {
                        return Some(payload_start + found);
                    }
                }
                pos = end;
            }
            None
        }

        walk_boxes(bytes, box_type)
    }

    fn split_native_jp2_tile_at_packet_boundary(bytes: &[u8], total_parts: u8) -> Vec<u8> {
        let (codestream_start, codestream_end) = top_level_box_range(bytes, b"jp2c");
        let codestream = &bytes[codestream_start..codestream_end];
        let parts = codestream::parse_codestream_view(codestream).expect("parse native codestream");
        assert_eq!(
            parts.tile_parts.len(),
            1,
            "native fixture has one tile-part"
        );
        assert!(parts.tile_parts[0].header_segments.is_empty());
        let header = CodestreamHeader::from_marker_segments(
            parts.main_header_segments.iter().copied(),
            parts.tile_parts[0].header,
            1,
        )
        .expect("parse native headers");
        let packets = t2::parse_tile_part_payload(&header, parts.tile_parts[0].payload)
            .expect("parse native packet boundaries");
        assert!(packets.packets.len() >= 2, "fixture needs multiple packets");
        let split_at = packets.packets[0].header_len + packets.packets[0].body_len;
        let payload = parts.tile_parts[0].payload;
        assert!(split_at > 0 && split_at < payload.len());

        let sot_start = codestream
            .windows(2)
            .position(|marker| marker == [0xff, 0x90])
            .expect("native SOT marker");
        let payload_start = sot_start + 14;
        assert_eq!(&codestream[sot_start + 12..payload_start], &[0xff, 0x93]);
        assert_eq!(
            &codestream[payload_start..codestream.len() - 2],
            payload,
            "native fixture has no tile header markers"
        );

        let mut split_codestream = Vec::with_capacity(codestream.len() + 14);
        split_codestream.extend_from_slice(&codestream[..sot_start]);
        append_tile_part(
            &mut split_codestream,
            0,
            0,
            total_parts,
            &payload[..split_at],
        );
        append_tile_part(
            &mut split_codestream,
            0,
            1,
            total_parts,
            &payload[split_at..],
        );
        split_codestream.extend_from_slice(&[0xff, 0xd9]);

        let mut output = Vec::with_capacity(bytes.len() + 14);
        output.extend_from_slice(&bytes[..codestream_start - 8]);
        let box_len = u32::try_from(split_codestream.len() + 8).expect("jp2c box length");
        output.extend_from_slice(&box_len.to_be_bytes());
        output.extend_from_slice(b"jp2c");
        output.extend_from_slice(&split_codestream);
        output.extend_from_slice(&bytes[codestream_end..]);
        output
    }

    fn append_tile_part(
        output: &mut Vec<u8>,
        tile_index: u16,
        part_index: u8,
        total_parts: u8,
        payload: &[u8],
    ) {
        let psot = u32::try_from(14usize + payload.len()).expect("tile-part length");
        output.extend_from_slice(&[0xff, 0x90]);
        output.extend_from_slice(&10u16.to_be_bytes());
        output.extend_from_slice(&tile_index.to_be_bytes());
        output.extend_from_slice(&psot.to_be_bytes());
        output.push(part_index);
        output.push(total_parts);
        output.extend_from_slice(&[0xff, 0x93]);
        output.extend_from_slice(payload);
    }

    /// Like [`append_tile_part`] but injects `header_segments` (e.g. a QCD/QCC
    /// override) between the SOT and SOD markers, recomputing Psot.
    fn append_tile_part_with_header(
        output: &mut Vec<u8>,
        tile_index: u16,
        part_index: u8,
        total_parts: u8,
        header_segments: &[Vec<u8>],
        payload: &[u8],
    ) {
        let header_len: usize = header_segments.iter().map(Vec::len).sum();
        let psot = u32::try_from(14usize + header_len + payload.len()).expect("tile-part length");
        output.extend_from_slice(&[0xff, 0x90]);
        output.extend_from_slice(&10u16.to_be_bytes());
        output.extend_from_slice(&tile_index.to_be_bytes());
        output.extend_from_slice(&psot.to_be_bytes());
        output.push(part_index);
        output.push(total_parts);
        for segment in header_segments {
            output.extend_from_slice(segment);
        }
        output.extend_from_slice(&[0xff, 0x93]);
        output.extend_from_slice(payload);
    }

    /// Re-emit a single-tile native JP2 with `header_segments` placed in the
    /// tile-part header, preserving the main header, payload, and container.
    fn rebuild_with_tile_header_segments(bytes: &[u8], header_segments: &[Vec<u8>]) -> Vec<u8> {
        let (codestream_start, codestream_end) = top_level_box_range(bytes, b"jp2c");
        let codestream = &bytes[codestream_start..codestream_end];
        let parts = codestream::parse_codestream_view(codestream).expect("parse native codestream");
        assert_eq!(parts.tile_parts.len(), 1, "native fixture has one tile-part");
        let payload = parts.tile_parts[0].payload;

        let sot_start = codestream
            .windows(2)
            .position(|marker| marker == [0xff, 0x90])
            .expect("native SOT marker");

        let mut split_codestream = Vec::with_capacity(codestream.len() + 64);
        split_codestream.extend_from_slice(&codestream[..sot_start]);
        append_tile_part_with_header(&mut split_codestream, 0, 0, 1, header_segments, payload);
        split_codestream.extend_from_slice(&[0xff, 0xd9]);

        let mut output = Vec::with_capacity(bytes.len() + 64);
        output.extend_from_slice(&bytes[..codestream_start - 8]);
        let box_len = u32::try_from(split_codestream.len() + 8).expect("jp2c box length");
        output.extend_from_slice(&box_len.to_be_bytes());
        output.extend_from_slice(b"jp2c");
        output.extend_from_slice(&split_codestream);
        output.extend_from_slice(&bytes[codestream_end..]);
        output
    }

    /// A scalar-expounded QCD marker (0xff5c) with `step_count` steps all at
    /// `exponent` (mantissa 0), for injecting a tile-part quantization override.
    fn qcd_override_segment(step_count: usize, exponent: u16) -> Vec<u8> {
        let packed = exponent << 11;
        let mut body = vec![0x22u8]; // 1 guard bit, scalar-expounded
        for _ in 0..step_count {
            body.extend_from_slice(&packed.to_be_bytes());
        }
        let mut segment = vec![0xff, 0x5c];
        let len = u16::try_from(body.len() + 2).expect("qcd length");
        segment.extend_from_slice(&len.to_be_bytes());
        segment.extend_from_slice(&body);
        segment
    }

    #[test]
    fn tile_part_qcd_override_is_applied_end_to_end() {
        // Encode a single-tile 9/7 gray image, then re-emit the codestream with a
        // QCD override in the tile-part header. It must decode (this marker was
        // previously a hard reject) and produce different samples than the same
        // stream without the override — proving the tile QCD is parsed and
        // applied through the whole dequant pipeline, not silently ignored.
        let width = 64u32;
        let height = 64u32;
        let image = Image::from_gray_bytes(width, height, &gray_fixture(width, height))
            .expect("source image");
        let encoded = crate::encode(&image, &region_opts(75)).expect("encode 9/7 jp2");

        let meta = inspect_jp2(&encoded).expect("inspect encoded");
        assert_eq!(meta.codestream.cod.transform, WaveletTransform::Irreversible97);
        let step_count = 1 + usize::from(meta.codestream.cod.decomposition_levels) * 3;
        let base_exp = meta.codestream.qcd.steps[0].exponent;
        let new_exp = u16::from(if base_exp >= 3 { base_exp - 2 } else { base_exp + 2 });

        let base = rebuild_with_tile_header_segments(&encoded, &[]);
        let overridden =
            rebuild_with_tile_header_segments(&encoded, &[qcd_override_segment(step_count, new_exp)]);

        let base_px = decode_jp2(&base).expect("decode without override").components[0]
            .data
            .clone();
        let over_px = decode_jp2(&overridden).expect("decode with tile QCD override").components[0]
            .data
            .clone();

        assert_eq!(base_px.len(), over_px.len());
        assert_ne!(
            base_px, over_px,
            "tile-part QCD override must change the decoded samples"
        );
    }

    fn top_level_box_range(bytes: &[u8], box_type: &[u8; 4]) -> (usize, usize) {
        let mut start = 0usize;
        while start + 8 <= bytes.len() {
            let length = u32::from_be_bytes(bytes[start..start + 4].try_into().expect("LBox"));
            assert_ne!(length, 0, "test fixture uses bounded JP2 boxes");
            assert_ne!(length, 1, "test fixture uses 32-bit JP2 box lengths");
            let end = start
                .checked_add(length as usize)
                .expect("JP2 box length overflow");
            assert!(end <= bytes.len(), "JP2 box exceeds fixture length");
            if &bytes[start + 4..start + 8] == box_type {
                return (start + 8, end);
            }
            start = end;
        }
        panic!("missing top-level JP2 box {:?}", box_type);
    }

    fn gray_fixture(width: u32, height: u32) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| ((x * 7 + y * 11 + (x ^ y)) & 0xff) as u8))
            .collect()
    }

    fn rgb_fixture(width: u32, height: u32) -> Vec<u8> {
        (0..height)
            .flat_map(|y| {
                (0..width).flat_map(move |x| {
                    [
                        ((x * 5 + y * 3 + (x ^ y)) & 0xff) as u8,
                        ((x * 2 + y * 11) & 0xff) as u8,
                        ((x * 13 + y * 7 + y) & 0xff) as u8,
                    ]
                })
            })
            .collect()
    }

    fn region_opts(quality: u8) -> crate::EncodeOptions {
        crate::EncodeOptions {
            quality,
            format: crate::OutputFormat::Jp2,
            tile_policy: crate::TilePolicy::Single,
            ..Default::default()
        }
    }

    /// Decode `region` and assert it is byte-for-byte the crop of a full decode
    /// at the same resolution/format, and that Serial and Budgeted region
    /// decodes agree.
    fn assert_region_matches_full_crop(
        encoded: &[u8],
        resolution: DecodeResolution,
        output: DecodeOutputFormat,
        region: DecodeRegion,
    ) {
        let base = DecodeRequest {
            resolution,
            output,
            region: None,
            concurrency: DecodeConcurrency::Serial,
        };
        let full = decode_jp2_request(encoded, &base).expect("full decode");
        let roi = decode_jp2_request(
            encoded,
            &DecodeRequest {
                region: Some(region),
                ..base.clone()
            },
        )
        .expect("region decode");
        let roi_budgeted = decode_jp2_request(
            encoded,
            &DecodeRequest {
                region: Some(region),
                concurrency: DecodeConcurrency::Budgeted(4),
                ..base.clone()
            },
        )
        .expect("budgeted region decode");

        let meta = inspect_jp2(encoded).expect("inspect");
        let reduce = select_reduce_levels(&meta.codestream, resolution).expect("reduce");
        let (_, crop) = project_region(&meta.codestream.siz, region, reduce).expect("project");
        let label = format!("{output:?} {resolution:?} region {region:?}");

        match (full, roi, roi_budgeted) {
            (DecodeResult::Native(full), DecodeResult::Native(roi), DecodeResult::Native(bud)) => {
                let cropped = crop_image(&full, crop).expect("crop full");
                assert_eq!((cropped.width, cropped.height), (roi.width, roi.height), "{label}");
                for (c, r) in cropped.components.iter().zip(&roi.components) {
                    assert_eq!(c.data, r.data, "region native mismatch: {label}");
                }
                for (r, b) in roi.components.iter().zip(&bud.components) {
                    assert_eq!(r.data, b.data, "serial vs budgeted region: {label}");
                }
            }
            (DecodeResult::Raster(full), DecodeResult::Raster(roi), DecodeResult::Raster(bud)) => {
                let cropped = crop_raster(&full, crop).expect("crop full");
                assert_eq!(cropped, roi, "region packed mismatch: {label}");
                assert_eq!(roi, bud, "serial vs budgeted region: {label}");
            }
            _ => panic!("mismatched result kinds: {label}"),
        }
    }

    #[test]
    fn region_decode_matches_full_crop_gray_and_rgb() {
        let width = 200u32;
        let height = 168u32;
        // Corner, interior odd-offset, right edge, odd extent, and a small far
        // corner — the offsets/parities where inverse-DWT region math misaligns.
        let regions = [
            DecodeRegion { x: 0, y: 0, width: 40, height: 40 },
            DecodeRegion { x: 71, y: 53, width: 49, height: 33 },
            DecodeRegion { x: 138, y: 0, width: 62, height: 90 },
            DecodeRegion { x: 15, y: 137, width: 33, height: 31 },
            DecodeRegion { x: 191, y: 159, width: 9, height: 9 },
        ];
        // quality 100 -> reversible 5/3, quality 75 -> irreversible 9/7.
        for quality in [100u8, 75u8] {
            let gray = Image::from_gray_bytes(width, height, &gray_fixture(width, height))
                .expect("gray source");
            let gray_enc = crate::encode(&gray, &region_opts(quality)).expect("encode gray");
            let rgb =
                Image::from_rgb_bytes(width, height, &rgb_fixture(width, height)).expect("rgb");
            let rgb_enc = crate::encode(&rgb, &region_opts(quality)).expect("encode rgb");

            for resolution in [DecodeResolution::Full, DecodeResolution::ReduceLevels(1)] {
                for &region in &regions {
                    assert_region_matches_full_crop(
                        &gray_enc,
                        resolution,
                        DecodeOutputFormat::NativePlanarI32,
                        region,
                    );
                    assert_region_matches_full_crop(
                        &gray_enc,
                        resolution,
                        DecodeOutputFormat::Gray8,
                        region,
                    );
                    assert_region_matches_full_crop(
                        &rgb_enc,
                        resolution,
                        DecodeOutputFormat::NativePlanarI32,
                        region,
                    );
                    assert_region_matches_full_crop(
                        &rgb_enc,
                        resolution,
                        DecodeOutputFormat::Rgb8,
                        region,
                    );
                }
            }
        }
    }

    #[test]
    fn region_decode_matches_full_crop_multi_tile() {
        let width = 300u32;
        let height = 220u32;
        let gray = Image::from_gray_bytes(width, height, &gray_fixture(width, height))
            .expect("gray source");
        for quality in [100u8, 75u8] {
            let encoded = crate::encode(
                &gray,
                &crate::EncodeOptions {
                    quality,
                    format: crate::OutputFormat::Jp2,
                    tile_policy: crate::TilePolicy::Fixed {
                        width: 128,
                        height: 128,
                    },
                    ..Default::default()
                },
            )
            .expect("encode multi-tile");
            let regions = [
                DecodeRegion { x: 10, y: 12, width: 50, height: 44 },
                DecodeRegion { x: 150, y: 100, width: 90, height: 70 },
                DecodeRegion { x: 260, y: 190, width: 40, height: 30 },
            ];
            for resolution in [DecodeResolution::Full, DecodeResolution::ReduceLevels(1)] {
                for &region in &regions {
                    assert_region_matches_full_crop(
                        &encoded,
                        resolution,
                        DecodeOutputFormat::NativePlanarI32,
                        region,
                    );
                    assert_region_matches_full_crop(
                        &encoded,
                        resolution,
                        DecodeOutputFormat::Gray8,
                        region,
                    );
                }
            }
        }
    }

    #[test]
    fn multi_tile_packed_direct_matches_native_pack() {
        // Gray8 and Rgb8 exercise the multi-tile packed-direct stitch (1- and
        // 3-channel interleave, inverse RCT/ICT for RGB). Cmyk8 shares the same
        // channel-generic stitch but the encoder is CMYK-decode-only, so it
        // cannot be round-tripped here.
        let width = 300u32;
        let height = 220u32;
        let tile_policy = crate::TilePolicy::Fixed {
            width: 128,
            height: 128,
        };
        let gray = Image::from_gray_bytes(width, height, &gray_fixture(width, height)).unwrap();
        let rgb = Image::from_rgb_bytes(width, height, &rgb_fixture(width, height)).unwrap();

        let cases: [(&Image, DecodeOutputFormat); 2] = [
            (&gray, DecodeOutputFormat::Gray8),
            (&rgb, DecodeOutputFormat::Rgb8),
        ];
        for (image, format) in cases {
            let encoded = crate::encode(
                image,
                &crate::EncodeOptions {
                    quality: 75,
                    format: crate::OutputFormat::Jp2,
                    tile_policy,
                    ..Default::default()
                },
            )
            .expect("encode multi-tile");
            // Confirm the fixture is genuinely multi-tile.
            assert!(
                inspect_jp2(&encoded).unwrap().tile_part_count >= 4,
                "expected multi-tile fixture"
            );
            for resolution in [DecodeResolution::Full, DecodeResolution::ReduceLevels(1)] {
                let packed = decode_jp2_request(
                    &encoded,
                    &DecodeRequest {
                        resolution,
                        output: format,
                        region: None,
                        concurrency: DecodeConcurrency::Serial,
                    },
                )
                .expect("packed multi-tile decode");
                let native = decode_jp2_request(
                    &encoded,
                    &DecodeRequest {
                        resolution,
                        output: DecodeOutputFormat::NativePlanarI32,
                        region: None,
                        concurrency: DecodeConcurrency::Serial,
                    },
                )
                .expect("native multi-tile decode");
                let (DecodeResult::Raster(packed), DecodeResult::Native(native)) = (packed, native)
                else {
                    panic!("unexpected result kinds");
                };
                let expected = pack_image_8bit(&native, format).expect("pack native");
                assert_eq!(
                    packed, expected,
                    "{format:?} {resolution:?}: multi-tile packed-direct diverged from native+pack"
                );
            }
        }
    }

    #[test]
    fn region_decode_rejects_out_of_bounds() {
        let width = 96u32;
        let height = 64u32;
        let gray = Image::from_gray_bytes(width, height, &gray_fixture(width, height))
            .expect("gray source");
        let encoded = crate::encode(&gray, &region_opts(75)).expect("encode");
        let err = decode_jp2_request(
            &encoded,
            &DecodeRequest {
                region: Some(DecodeRegion {
                    x: 80,
                    y: 0,
                    width: 32,
                    height: 16,
                }),
                ..Default::default()
            },
        )
        .expect_err("region past right edge must fail");
        assert!(err.to_string().contains("outside"), "{err}");
    }

    fn tile_part_view(
        tile_index: u16,
        part_index: u8,
        total_parts: u8,
    ) -> codestream::TilePartView<'static> {
        codestream::TilePartView {
            header: crate::j2k::TilePartHeader {
                tile_index,
                part_index,
                total_parts,
            },
            header_segments: Vec::new(),
            payload: &[],
        }
    }

    fn archive_org_gray_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("2015.207614.Finnegans-Wake_0012.jp2")
    }

    fn archive_org_rgb_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("moreboysgirlsofh0000rhod_e0h1_orig_jp2")
            .join(name)
    }

    fn assert_decoded_components_are_8bit_full_size(image: &Image) {
        let pixel_count = image.width as usize * image.height as usize;
        for component in &image.components {
            assert_eq!(component.width, image.width);
            assert_eq!(component.height, image.height);
            assert_eq!(component.precision, 8);
            assert!(!component.signed);
            assert_eq!(component.dx, 1);
            assert_eq!(component.dy, 1);
            assert_eq!(component.data.len(), pixel_count);
            assert!(
                component
                    .data
                    .iter()
                    .all(|&sample| (0..=255).contains(&sample))
            );
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct RawCrop {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        channels: usize,
    }

    fn imagemagick_available() -> bool {
        std::process::Command::new("magick")
            .arg("-version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn imagemagick_raw_crop(path: &std::path::Path, crop: RawCrop) -> Vec<u8> {
        let raw_path = std::env::temp_dir().join(format!(
            "jp2lam_ref_{}_{}_{}_{}.raw",
            std::process::id(),
            crop.x,
            crop.y,
            crop.channels
        ));
        let format = if crop.channels == 1 { "gray" } else { "rgb" };
        let output_arg = format!("{}:{}", format, raw_path.display());
        let status = std::process::Command::new("magick")
            .arg(path)
            .arg("-crop")
            .arg(format!(
                "{}x{}+{}+{}",
                crop.width, crop.height, crop.x, crop.y
            ))
            .arg("+repage")
            .arg("-depth")
            .arg("8")
            .arg(output_arg)
            .status()
            .expect("run ImageMagick");
        assert!(
            status.success(),
            "ImageMagick failed for {}",
            path.display()
        );
        let bytes = std::fs::read(&raw_path).expect("read ImageMagick raw crop");
        let _ = std::fs::remove_file(raw_path);
        assert_eq!(
            bytes.len(),
            crop.width as usize * crop.height as usize * crop.channels
        );
        bytes
    }

    fn assert_crop_close_to_reference(
        image: &Image,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        reference: &[u8],
        channels: usize,
        max_abs_allowed: i32,
        mean_abs_allowed: f64,
    ) {
        assert_eq!(image.components.len(), channels);
        let mut max_abs = 0i32;
        let mut total_abs = 0u64;
        let mut count = 0u64;
        for yy in 0..height {
            for xx in 0..width {
                let src_idx = ((y + yy) * image.width + (x + xx)) as usize;
                let ref_idx = ((yy * width + xx) as usize) * channels;
                for channel in 0..channels {
                    let actual = image.components[channel].data[src_idx];
                    let expected = i32::from(reference[ref_idx + channel]);
                    let delta = (actual - expected).abs();
                    max_abs = max_abs.max(delta);
                    total_abs += delta as u64;
                    count += 1;
                }
            }
        }
        let mean_abs = total_abs as f64 / count as f64;
        assert!(
            max_abs <= max_abs_allowed,
            "max abs diff {max_abs} exceeded {max_abs_allowed}"
        );
        assert!(
            mean_abs <= mean_abs_allowed,
            "mean abs diff {mean_abs:.4} exceeded {mean_abs_allowed:.4}"
        );
    }
}
