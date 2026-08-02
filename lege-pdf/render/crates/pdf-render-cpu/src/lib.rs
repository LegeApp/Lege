//! The CPU raster backend — the engine's **normative implementation** of
//! rendering semantics (roadmap §2.3) and permanent fallback/oracle for the
//! GPU backend.
//!
//! Architecture (Phase 4): a tiled rasterizer; `CompiledPage` lowers to
//! per-tile operation lists preserving painter order; every worker owns a
//! [`CpuWorkerContext`] with all scratch buffers — no shared mutable state,
//! no global allocator pressure.
//!
//! This is the **production scalar raster engine**, not a throwaway reference:
//! it is designed for speed from the first commit, and its scalar kernels are
//! the reference for later SIMD kernels. The pipeline (per the performance
//! architecture) is:
//!
//! ```text
//! CompiledPage + RenderRequest
//!   → CpuPreparedPage      (transform, cull, classify, flatten — once/request)
//!   → executor             (compact commands; no Arc/lookup in the hot loop)
//!   → RasterKernel         (analytic scanline coverage → u8 rows)
//!   → KernelSet spans      (opaque-fill / const-blend / mask-blend, chosen once/span)
//!   → Surface / HostPage
//! ```
//!
//! Current state (Phase 4a): **solid fills with constant alpha**, with an
//! opaque-integer-rect fast path and per-render instrumentation
//! ([`RenderStats`]). Clipping, strokes, images, text, blend modes, and
//! transparency arrive in later 4x checkpoints (advice §16 order); preflight
//! (`supports()`) advertises exactly what is implemented so unsupported pages
//! route elsewhere. Adaptive band/tile execution is deferred until direct-page
//! rendering is competitive (advice §2).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pdf_page_ir::{CompiledPage, DeviceSize, ImageMask, Matrix, PageFeatures};
use pdf_render_api::{
    BackendCapabilities, BackendId, HostPage, OutputFormat, PostprocessCapabilities, RenderBackend,
    RenderError, RenderLimits, RenderRequest, RenderTicket, RenderedPage, SubmitError,
    SupportLevel, UnsupportedFeature,
};

pub mod attribution;
mod exec;
mod image;
mod kernels;
mod mask;
mod prepared;
mod raster;
mod stats;
mod stroke;
mod surface;

pub use exec::CpuWorkerContext;
pub use kernels::KernelSet;
#[cfg(feature = "profiling")]
pub use prepared::DecodedImageCache;
pub use prepared::{CpuPreparedPage, DrawClass};
pub use stats::RenderStats;

use surface::Surface;
use {
    mask::{ClipGeom, build_clip_mask_cancellable},
    raster::RasterKernel,
};

fn image_decode_limits(limits: &RenderLimits) -> pdf_image::DecodeLimits {
    let defaults = pdf_image::DecodeLimits::default();
    pdf_image::DecodeLimits {
        // A request may tighten the codec defaults, but must not silently raise
        // their defensive encoded-input or scratch-memory ceilings.
        max_input_bytes: defaults.max_input_bytes.min(limits.max_page_bytes),
        max_pixels: defaults.max_pixels,
        max_output_bytes: defaults.max_output_bytes.min(limits.max_page_bytes),
        max_working_bytes: defaults.max_working_bytes.min(limits.max_page_bytes),
        should_cancel: limits.cancellation.clone().map(|token| {
            std::sync::Arc::new(move || token.is_cancelled())
                as std::sync::Arc<dyn Fn() -> bool + Send + Sync>
        }),
    }
}

/// One image draw lowered to RGB8 plus optional independent opacity for a
/// non-CPU raster backend.
///
/// This deliberately exposes only the small, semantics-neutral subset used
/// by the experimental GPU image renderer. PDF image decoding, `/Decode`,
/// color-space conversion, clipping, and request transforms remain owned by
/// this crate.
#[derive(Debug, Clone)]
pub struct PreparedRgbImage {
    pub bounds: pdf_page_ir::DeviceRect,
    /// Device coordinates to the image's normalized `[0, 1]²` square.
    pub device_to_image: Matrix,
    pub width: u32,
    pub height: u32,
    pub samples: Arc<[u8]>,
    pub interpolation: pdf_page_ir::InterpolationMode,
    /// Source texels covered by one destination pixel on each image axis.
    pub footprint: [f64; 2],
    /// Independently sampled opacity for an image `/SMask`, explicit `/Mask`,
    /// or solid-colour `/ImageMask` stencil. The samples are tight alpha8 in
    /// top-down order.
    pub opacity: Option<PreparedImageOpacity>,
    /// Device-space analytic clip coverage. Unlike image opacity this plane is
    /// already rasterized at destination resolution and is addressed by
    /// absolute device coordinates.
    pub clip: Option<PreparedImageClip>,
    /// Active page-level `/SMask`, derived by the normative CPU group
    /// executor and sampled in absolute device coordinates. This is separate
    /// from `opacity` (an image-resource mask) and `clip` because all three
    /// coverages multiply independently.
    pub soft_mask: Option<PreparedImageSoftMask>,
    /// Constant graphics-state opacity for this image draw.
    pub alpha: u8,
    /// PDF compositing mode for this image draw.
    pub blend: pdf_page_ir::BlendMode,
    /// Solid brush for `/ImageMask`; `None` means sample `samples` as RGB8.
    pub stencil_rgb: Option<[u8; 3]>,
}

/// One solid path draw in the ordered experimental GPU command stream.
#[derive(Debug, Clone)]
pub struct PreparedGpuPath {
    pub bounds: pdf_page_ir::DeviceRect,
    /// Packed GPU raster data: four-word header, directed f32 edges, 16-row
    /// band offsets, then edge indices. Values are u32 so the same storage
    /// binding can carry float bits and integer lookup tables.
    pub raster_data: Arc<[u32]>,
    pub edge_count: u32,
    pub band_edge_references: u32,
    pub even_odd: bool,
    pub rgb: [u8; 3],
    pub alpha: u8,
    pub blend: pdf_page_ir::BlendMode,
    pub clip: Option<PreparedImageClip>,
    pub soft_mask: Option<PreparedImageSoftMask>,
}

/// One active 8×8 destination tile and its painter-ordered path list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedGpuPathTile {
    pub x: u32,
    pub y: u32,
    pub path_offset: u32,
    pub path_count: u32,
}

/// A maximal painter-ordered path run prepared for one GPU dispatch.
#[derive(Debug, Clone)]
pub struct PreparedGpuPathBatch {
    pub paths: Arc<[PreparedGpuPath]>,
    pub tiles: Arc<[PreparedGpuPathTile]>,
    pub tile_path_indices: Arc<[u32]>,
    pub geometry_bytes: usize,
    pub mask_bytes: usize,
    pub max_tile_depth: u32,
}

/// Painter-ordered vocabulary accepted by the experimental GPU renderer.
#[derive(Debug, Clone)]
pub enum PreparedGpuCommand {
    Image(u32),
    /// Retained as an internal diagnostic/reference command. Normal
    /// preparation coalesces every such path into `PathBatch`.
    Path(PreparedGpuPath),
    PathBatch(Arc<PreparedGpuPathBatch>),
}

/// One normalized alpha8 plane sampled in the image's unit-square geometry.
#[derive(Debug, Clone)]
pub struct PreparedImageOpacity {
    pub width: u32,
    pub height: u32,
    pub samples: Arc<[u8]>,
    /// Mask texels covered by one destination pixel on each mask axis.
    pub footprint: [f64; 2],
    /// Soft masks and image stencils represent coverage and may be averaged
    /// during minification. Explicit hard `/Mask` values remain binary and
    /// are sampled at the destination pixel's source point.
    pub box_filter: bool,
}

/// One exact anti-aliased path-clip mask over a bounded device rectangle.
#[derive(Debug, Clone)]
pub struct PreparedImageClip {
    pub bounds: pdf_page_ir::DeviceRect,
    pub samples: Arc<[u8]>,
}

/// One derived page-level soft-mask coverage plane over a bounded device
/// rectangle. Pixels outside `bounds` use `outside`; this is normally zero,
/// but a luminosity mask with `/BC` (or `/TR`) can make it nonzero.
#[derive(Debug, Clone)]
pub struct PreparedImageSoftMask {
    pub bounds: pdf_page_ir::DeviceRect,
    pub samples: Arc<[u8]>,
    pub outside: u8,
}

/// A patterned `/ImageMask` brush rasterized by the normative CPU pattern
/// executor into a bounded straight-RGB plane plus independent alpha.
///
/// Pattern cells may contain paths, text, images, groups, and nested patterns;
/// keeping that vocabulary on the CPU makes this a narrow image-compositor
/// bridge rather than an accidental mixed-content GPU renderer.
#[derive(Debug, Clone)]
pub(crate) struct PreparedPatternedStencil {
    pub bounds: pdf_page_ir::DeviceRect,
    pub samples: Arc<[u8]>,
    pub opacity: Arc<[u8]>,
}

/// An image-only page prepared for an offscreen raster backend.
#[derive(Debug, Clone)]
pub struct PreparedRgbImagePage {
    pub size: DeviceSize,
    pub images: Vec<PreparedRgbImage>,
    /// Exact painter order. Image commands index `images`; path commands own
    /// their immutable flattened edge geometry.
    pub commands: Vec<PreparedGpuCommand>,
}

/// Maximum full-source RGB8 allocation made solely to prepare a non-RGB8
/// image for GPU upload. Larger converted sources stay on the CPU's
/// destination-driven sampler instead of multiplying a compact bilevel or
/// grayscale source into a very large transient/cache allocation.
pub const MAX_PREPARED_RGB_CONVERSION_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PREPARED_OPACITY_CONVERSION_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PREPARED_PATTERNED_STENCIL_BYTES: usize = 64 * 1024 * 1024;

/// Source-texel footprint above which automatic routing keeps packed
/// bilevel images on the CPU. The CPU has a destination-driven popcount/box
/// filter for this minifying case, while preparing RGB8 would expand the
/// source and add upload work. Forced GPU routing deliberately ignores this
/// policy boundary.
pub const AUTO_CPU_BILEVEL_MINIFICATION_FOOTPRINT: f64 = 1.0;
const PREPARED_SOFT_MASK_CACHE_BYTES: usize = 64 * 1024 * 1024;
const PREPARED_PATTERNED_STENCIL_CACHE_BYTES: usize = 64 * 1024 * 1024;
const PREPARED_GPU_PAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const GPU_PATH_BAND_HEIGHT: f32 = 16.0;
const GPU_PATH_SIMPLIFY_TOLERANCE: f32 = 0.1;
pub const GPU_PATH_BATCH_TILE_EDGE: u32 = 8;
const GPU_PATH_BATCH_MAX_TILE_DEPTH: usize = 64;
const GPU_PATH_BATCH_MAX_PATHS: usize = 4096;
const GPU_PATH_BATCH_MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;

/// Default logical tile size (roadmap §7 Phase 4); revisit after benchmarks.
pub const DEFAULT_TILE_SIZE: u32 = 512;

fn point_segment_distance_squared(point: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let ap = [point[0] - a[0], point[1] - a[1]];
    let denominator = ab[0] * ab[0] + ab[1] * ab[1];
    if denominator <= f32::EPSILON {
        return ap[0] * ap[0] + ap[1] * ap[1];
    }
    let t = ((ap[0] * ab[0] + ap[1] * ab[1]) / denominator).clamp(0.0, 1.0);
    let dx = point[0] - (a[0] + t * ab[0]);
    let dy = point[1] - (a[1] + t * ab[1]);
    dx * dx + dy * dy
}

fn mark_rdp_points(
    points: &[[f32; 2]],
    first: usize,
    last: usize,
    tolerance_squared: f32,
    keep: &mut [bool],
) {
    if last <= first + 1 {
        return;
    }
    let mut farthest = first;
    let mut farthest_distance = 0.0f32;
    for index in first + 1..last {
        let distance = point_segment_distance_squared(points[index], points[first], points[last]);
        if distance > farthest_distance {
            farthest = index;
            farthest_distance = distance;
        }
    }
    if farthest_distance <= tolerance_squared {
        return;
    }
    keep[farthest] = true;
    mark_rdp_points(points, first, farthest, tolerance_squared, keep);
    mark_rdp_points(points, farthest, last, tolerance_squared, keep);
}

fn simplify_open_gpu_polyline(points: &[[f32; 2]], tolerance: f32) -> Vec<[f32; 2]> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    mark_rdp_points(
        points,
        0,
        points.len() - 1,
        tolerance * tolerance,
        &mut keep,
    );
    points
        .iter()
        .zip(keep)
        .filter_map(|(point, keep)| keep.then_some(*point))
        .collect()
}

/// Reduce the CPU flattener's deliberately conservative 16-segment curves to
/// device-relevant geometry before GPU upload. Splitting a closed contour at
/// two distant vertices avoids the degenerate identical-endpoint case while
/// retaining winding direction and holes.
fn simplify_closed_gpu_contour(points: &[[f32; 2]], tolerance: f32) -> Vec<[f32; 2]> {
    let mut contour = Vec::with_capacity(points.len());
    for &point in points {
        if contour.last().copied() != Some(point) {
            contour.push(point);
        }
    }
    if contour.len() > 1 && contour.first() == contour.last() {
        contour.pop();
    }
    if contour.len() <= 3 {
        return contour;
    }

    let first = contour[0];
    let mut split = 1usize;
    let mut farthest = 0.0f32;
    for (index, &point) in contour.iter().enumerate().skip(1) {
        let dx = point[0] - first[0];
        let dy = point[1] - first[1];
        let distance = dx * dx + dy * dy;
        if distance > farthest {
            split = index;
            farthest = distance;
        }
    }
    if split == 0 || split >= contour.len() {
        return contour;
    }

    let first_arc = simplify_open_gpu_polyline(&contour[..=split], tolerance);
    let mut second_source = Vec::with_capacity(contour.len() - split + 1);
    second_source.extend_from_slice(&contour[split..]);
    second_source.push(contour[0]);
    let second_arc = simplify_open_gpu_polyline(&second_source, tolerance);

    let mut simplified = first_arc;
    let second_interior = second_arc.len().saturating_sub(2);
    simplified.extend(second_arc.into_iter().skip(1).take(second_interior));
    if simplified.len() < 3 {
        contour
    } else {
        simplified
    }
}

fn pack_gpu_path_raster(
    bounds: pdf_page_ir::DeviceRect,
    edges: &[[f32; 4]],
) -> Option<(Arc<[u32]>, u32)> {
    let edge_count = u32::try_from(edges.len()).ok()?;
    let band_count = bounds.height.div_ceil(GPU_PATH_BAND_HEIGHT as u32).max(1);
    let mut bands = vec![Vec::<u32>::new(); band_count as usize];
    let origin_y = bounds.y as f32;
    for (edge_index, edge) in edges.iter().enumerate() {
        let min_y = edge[1].min(edge[3]);
        let max_y = edge[1].max(edge[3]);
        if !min_y.is_finite() || !max_y.is_finite() || max_y <= min_y {
            continue;
        }
        let first = (((min_y - origin_y) / GPU_PATH_BAND_HEIGHT).floor() as i32)
            .clamp(0, band_count as i32 - 1);
        let last = ((((max_y - origin_y - 1.0e-4) / GPU_PATH_BAND_HEIGHT).floor()) as i32)
            .clamp(0, band_count as i32 - 1);
        for band in first..=last {
            bands[band as usize].push(edge_index as u32);
        }
    }

    let offsets_base = 4usize.checked_add(edges.len().checked_mul(4)?)?;
    let indices_base = offsets_base.checked_add(bands.len().checked_add(1)?)?;
    let reference_count: usize = bands.iter().map(Vec::len).sum();
    let total = indices_base.checked_add(reference_count)?;
    let mut data = Vec::with_capacity(total);
    data.extend_from_slice(&[
        edge_count,
        band_count,
        u32::try_from(offsets_base).ok()?,
        u32::try_from(indices_base).ok()?,
    ]);
    for edge in edges {
        data.extend(edge.iter().map(|value| value.to_bits()));
    }
    let mut offset = 0u32;
    data.push(offset);
    for band in &bands {
        offset = offset.checked_add(u32::try_from(band.len()).ok()?)?;
        data.push(offset);
    }
    for band in bands {
        data.extend(band);
    }
    Some((data.into(), u32::try_from(reference_count).ok()?))
}

fn gpu_path_tiles(size: DeviceSize, bounds: pdf_page_ir::DeviceRect) -> Option<Vec<(u32, u32)>> {
    let page_width = i64::from(size.width);
    let page_height = i64::from(size.height);
    let x0 = i64::from(bounds.x).clamp(0, page_width);
    let y0 = i64::from(bounds.y).clamp(0, page_height);
    let x1 = (i64::from(bounds.x) + i64::from(bounds.width)).clamp(0, page_width);
    let y1 = (i64::from(bounds.y) + i64::from(bounds.height)).clamp(0, page_height);
    if x0 >= x1 || y0 >= y1 {
        return Some(Vec::new());
    }
    let first_x = u32::try_from(x0).ok()? / GPU_PATH_BATCH_TILE_EDGE;
    let first_y = u32::try_from(y0).ok()? / GPU_PATH_BATCH_TILE_EDGE;
    let last_x = u32::try_from(x1).ok()?.div_ceil(GPU_PATH_BATCH_TILE_EDGE);
    let last_y = u32::try_from(y1).ok()?.div_ceil(GPU_PATH_BATCH_TILE_EDGE);
    let count = usize::try_from(last_x.checked_sub(first_x)?)
        .ok()?
        .checked_mul(usize::try_from(last_y.checked_sub(first_y)?).ok()?)?;
    let mut tiles = Vec::with_capacity(count);
    for tile_y in first_y..last_y {
        for tile_x in first_x..last_x {
            tiles.push((
                tile_x * GPU_PATH_BATCH_TILE_EDGE,
                tile_y * GPU_PATH_BATCH_TILE_EDGE,
            ));
        }
    }
    Some(tiles)
}

type GpuPathMaskKey = (usize, usize);

fn gpu_path_new_masks(
    path: &PreparedGpuPath,
    existing: &HashSet<GpuPathMaskKey>,
) -> Option<(usize, Vec<GpuPathMaskKey>)> {
    let mut new_keys = Vec::with_capacity(2);
    let mut bytes = 0usize;
    for samples in [
        path.clip.as_ref().map(|clip| &clip.samples),
        path.soft_mask.as_ref().map(|mask| &mask.samples),
    ]
    .into_iter()
    .flatten()
    {
        let key = (samples.as_ptr() as usize, samples.len());
        if !existing.contains(&key) && !new_keys.contains(&key) {
            bytes = bytes.checked_add(samples.len())?;
            new_keys.push(key);
        }
    }
    Some((bytes, new_keys))
}

fn flush_gpu_path_batch(
    commands: &mut Vec<PreparedGpuCommand>,
    paths: &mut Vec<PreparedGpuPath>,
    tile_paths: &mut BTreeMap<(u32, u32), Vec<u32>>,
    mask_planes: &mut HashSet<GpuPathMaskKey>,
    geometry_bytes: &mut usize,
    mask_bytes: &mut usize,
) -> Option<()> {
    if paths.is_empty() {
        return Some(());
    }
    let mut tiles = Vec::with_capacity(tile_paths.len());
    let reference_count = tile_paths.values().map(Vec::len).sum();
    let mut indices = Vec::with_capacity(reference_count);
    let mut max_tile_depth = 0u32;
    for (&(x, y), path_indices) in tile_paths.iter() {
        let path_offset = u32::try_from(indices.len()).ok()?;
        let path_count = u32::try_from(path_indices.len()).ok()?;
        max_tile_depth = max_tile_depth.max(path_count);
        indices.extend_from_slice(path_indices);
        tiles.push(PreparedGpuPathTile {
            x,
            y,
            path_offset,
            path_count,
        });
    }
    commands.push(PreparedGpuCommand::PathBatch(Arc::new(
        PreparedGpuPathBatch {
            paths: std::mem::take(paths).into(),
            tiles: tiles.into(),
            tile_path_indices: indices.into(),
            geometry_bytes: *geometry_bytes,
            mask_bytes: *mask_bytes,
            max_tile_depth,
        },
    )));
    tile_paths.clear();
    mask_planes.clear();
    *geometry_bytes = 0;
    *mask_bytes = 0;
    Some(())
}

fn batch_gpu_path_commands(
    size: DeviceSize,
    source: Vec<PreparedGpuCommand>,
    cancellation: Option<&pdf_render_api::CancellationToken>,
) -> Option<Vec<PreparedGpuCommand>> {
    let mut commands = Vec::with_capacity(source.len());
    let mut paths = Vec::new();
    let mut tile_paths = BTreeMap::<(u32, u32), Vec<u32>>::new();
    let mut mask_planes = HashSet::<GpuPathMaskKey>::new();
    let mut geometry_bytes = 0usize;
    let mut mask_bytes = 0usize;

    for command in source {
        if cancellation.is_some_and(pdf_render_api::CancellationToken::is_cancelled) {
            return None;
        }
        let PreparedGpuCommand::Path(path) = command else {
            flush_gpu_path_batch(
                &mut commands,
                &mut paths,
                &mut tile_paths,
                &mut mask_planes,
                &mut geometry_bytes,
                &mut mask_bytes,
            )?;
            commands.push(command);
            continue;
        };

        let path_tiles = gpu_path_tiles(size, path.bounds)?;
        if path_tiles.is_empty() {
            continue;
        }
        let path_geometry_bytes = path
            .raster_data
            .len()
            .checked_mul(std::mem::size_of::<u32>())?;
        let (mut path_mask_bytes, mut path_mask_keys) = gpu_path_new_masks(&path, &mask_planes)?;
        let existing_refs: usize = tile_paths.values().map(Vec::len).sum();
        let new_tile_count = path_tiles
            .iter()
            .filter(|tile| !tile_paths.contains_key(tile))
            .count();
        let exceeds = paths.len() >= GPU_PATH_BATCH_MAX_PATHS
            || geometry_bytes
                .checked_add(path_geometry_bytes)
                .is_none_or(|bytes| bytes > GPU_PATH_BATCH_MAX_COMPONENT_BYTES)
            || mask_bytes
                .checked_add(path_mask_bytes)
                .is_none_or(|bytes| bytes > GPU_PATH_BATCH_MAX_COMPONENT_BYTES)
            || existing_refs
                .checked_add(path_tiles.len())
                .and_then(|refs| refs.checked_mul(std::mem::size_of::<u32>()))
                .is_none_or(|bytes| bytes > GPU_PATH_BATCH_MAX_COMPONENT_BYTES)
            || tile_paths
                .len()
                .checked_add(new_tile_count)
                .and_then(|tiles| tiles.checked_mul(std::mem::size_of::<PreparedGpuPathTile>()))
                .is_none_or(|bytes| bytes > GPU_PATH_BATCH_MAX_COMPONENT_BYTES)
            || path_tiles.iter().any(|tile| {
                tile_paths
                    .get(tile)
                    .is_some_and(|indices| indices.len() >= GPU_PATH_BATCH_MAX_TILE_DEPTH)
            });
        if exceeds && !paths.is_empty() {
            flush_gpu_path_batch(
                &mut commands,
                &mut paths,
                &mut tile_paths,
                &mut mask_planes,
                &mut geometry_bytes,
                &mut mask_bytes,
            )?;
            (path_mask_bytes, path_mask_keys) = gpu_path_new_masks(&path, &mask_planes)?;
        }

        if path_geometry_bytes > GPU_PATH_BATCH_MAX_COMPONENT_BYTES
            || path_mask_bytes > GPU_PATH_BATCH_MAX_COMPONENT_BYTES
            || path_tiles
                .len()
                .checked_mul(std::mem::size_of::<u32>())
                .is_none_or(|bytes| bytes > GPU_PATH_BATCH_MAX_COMPONENT_BYTES)
        {
            return None;
        }
        let path_index = u32::try_from(paths.len()).ok()?;
        for tile in path_tiles {
            tile_paths.entry(tile).or_default().push(path_index);
        }
        geometry_bytes = geometry_bytes.checked_add(path_geometry_bytes)?;
        mask_bytes = mask_bytes.checked_add(path_mask_bytes)?;
        mask_planes.extend(path_mask_keys);
        paths.push(path);
    }
    flush_gpu_path_batch(
        &mut commands,
        &mut paths,
        &mut tile_paths,
        &mut mask_planes,
        &mut geometry_bytes,
        &mut mask_bytes,
    )?;
    Some(commands)
}

fn prepare_external_clip(
    page: &CpuPreparedPage,
    clip_masks: &mut [Option<Option<PreparedImageClip>>],
    raster: &mut RasterKernel,
    clip: Option<u32>,
    has_mask: bool,
) -> Result<Option<PreparedImageClip>, RenderError> {
    if !has_mask {
        return Ok(None);
    }
    let Some(active_clip) = clip else {
        return Err(RenderError::Unsupported(PageFeatures::CLIPPING));
    };
    let source = page.clips[active_clip as usize]
        .mask_source
        .unwrap_or(active_clip);
    if clip_masks[source as usize].is_none() {
        let Some(mask) = build_clip_mask_cancellable(
            raster,
            ClipGeom::of(page),
            source,
            page.decode_limits.should_cancel.as_deref(),
        ) else {
            return Err(RenderError::Cancelled);
        };
        clip_masks[source as usize] = Some(if mask.all_opaque {
            None
        } else {
            Some(PreparedImageClip {
                bounds: mask.bounds,
                samples: Arc::from(mask.data),
            })
        });
    }
    Ok(clip_masks[source as usize].clone().flatten())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PreparedSoftMaskCacheKey {
    page_ptr: usize,
    output_width: u32,
    output_height: u32,
    max_page_bytes: u64,
    color_policy: pdf_render_api::RenderColorPolicy,
    matrix: [u64; 6],
}

impl PreparedSoftMaskCacheKey {
    fn new(request: &RenderRequest) -> Self {
        let matrix = request.transform.matrix;
        Self {
            page_ptr: Arc::as_ptr(&request.page) as usize,
            output_width: request.output_size.width,
            output_height: request.output_size.height,
            max_page_bytes: request.limits.max_page_bytes,
            color_policy: request.color_policy,
            matrix: [
                matrix.a.to_bits(),
                matrix.b.to_bits(),
                matrix.c.to_bits(),
                matrix.d.to_bits(),
                matrix.e.to_bits(),
                matrix.f.to_bits(),
            ],
        }
    }
}

#[derive(Debug)]
struct PreparedSoftMaskCacheEntry {
    // Retaining the page makes `page_ptr` collision-free for this entry's
    // lifetime without hashing a complete compiled page.
    page: Arc<CompiledPage>,
    masks: Vec<Option<PreparedImageSoftMask>>,
    charge: usize,
    last_used: u64,
}

#[derive(Debug, Default)]
struct PreparedSoftMaskCacheState {
    entries: HashMap<PreparedSoftMaskCacheKey, PreparedSoftMaskCacheEntry>,
    resident_bytes: usize,
    clock: u64,
}

#[derive(Debug, Default)]
struct SharedPreparedSoftMaskCache {
    state: Mutex<PreparedSoftMaskCacheState>,
}

impl SharedPreparedSoftMaskCache {
    fn get(&self, request: &RenderRequest) -> Option<Vec<Option<PreparedImageSoftMask>>> {
        let key = PreparedSoftMaskCacheKey::new(request);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        let entry = state.entries.get_mut(&key)?;
        if !Arc::ptr_eq(&entry.page, &request.page) {
            return None;
        }
        entry.last_used = clock;
        Some(entry.masks.clone())
    }

    fn insert(
        &self,
        request: &RenderRequest,
        masks: Vec<Option<PreparedImageSoftMask>>,
    ) -> Vec<Option<PreparedImageSoftMask>> {
        let key = PreparedSoftMaskCacheKey::new(request);
        let charge = masks
            .iter()
            .filter_map(Option::as_ref)
            .map(|mask| mask.samples.len())
            .sum();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        if let Some(previous) = state.entries.insert(
            key,
            PreparedSoftMaskCacheEntry {
                page: Arc::clone(&request.page),
                masks: masks.clone(),
                charge,
                last_used: clock,
            },
        ) {
            state.resident_bytes = state.resident_bytes.saturating_sub(previous.charge);
        }
        state.resident_bytes = state.resident_bytes.saturating_add(charge);
        // Keep one oversized page for an immediate revisit, matching the GPU
        // upload cache policy, but otherwise stay within the session budget.
        while state.resident_bytes > PREPARED_SOFT_MASK_CACHE_BYTES && state.entries.len() > 1 {
            let Some(victim) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(removed) = state.entries.remove(&victim) {
                state.resident_bytes = state.resident_bytes.saturating_sub(removed.charge);
            }
        }
        masks
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PreparedPatternedStencilCacheKey {
    request: PreparedSoftMaskCacheKey,
    op_index: usize,
}

#[derive(Debug)]
struct PreparedPatternedStencilCacheEntry {
    page: Arc<CompiledPage>,
    prepared: PreparedPatternedStencil,
    charge: usize,
    last_used: u64,
}

#[derive(Debug, Default)]
struct PreparedPatternedStencilCacheState {
    entries: HashMap<PreparedPatternedStencilCacheKey, PreparedPatternedStencilCacheEntry>,
    resident_bytes: usize,
    clock: u64,
}

#[derive(Debug, Default)]
struct SharedPreparedPatternedStencilCache {
    state: Mutex<PreparedPatternedStencilCacheState>,
}

impl SharedPreparedPatternedStencilCache {
    fn get(&self, request: &RenderRequest, op_index: usize) -> Option<PreparedPatternedStencil> {
        let key = PreparedPatternedStencilCacheKey {
            request: PreparedSoftMaskCacheKey::new(request),
            op_index,
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        let entry = state.entries.get_mut(&key)?;
        if !Arc::ptr_eq(&entry.page, &request.page) {
            return None;
        }
        entry.last_used = clock;
        Some(entry.prepared.clone())
    }

    fn insert(
        &self,
        request: &RenderRequest,
        op_index: usize,
        prepared: PreparedPatternedStencil,
    ) -> PreparedPatternedStencil {
        let key = PreparedPatternedStencilCacheKey {
            request: PreparedSoftMaskCacheKey::new(request),
            op_index,
        };
        let charge = prepared
            .samples
            .len()
            .saturating_add(prepared.opacity.len());
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        if let Some(previous) = state.entries.insert(
            key,
            PreparedPatternedStencilCacheEntry {
                page: Arc::clone(&request.page),
                prepared: prepared.clone(),
                charge,
                last_used: clock,
            },
        ) {
            state.resident_bytes = state.resident_bytes.saturating_sub(previous.charge);
        }
        state.resident_bytes = state.resident_bytes.saturating_add(charge);
        while state.resident_bytes > PREPARED_PATTERNED_STENCIL_CACHE_BYTES
            && state.entries.len() > 1
        {
            let Some(victim) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(removed) = state.entries.remove(&victim) {
                state.resident_bytes = state.resident_bytes.saturating_sub(removed.charge);
            }
        }
        prepared
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PreparedGpuPageCacheKey {
    request: PreparedSoftMaskCacheKey,
    automatic_routing: bool,
}

#[derive(Debug)]
struct PreparedGpuPageCacheEntry {
    page: Arc<CompiledPage>,
    prepared: PreparedRgbImagePage,
    charge: usize,
    last_used: u64,
}

#[derive(Debug, Default)]
struct PreparedGpuPageCacheState {
    entries: HashMap<PreparedGpuPageCacheKey, PreparedGpuPageCacheEntry>,
    resident_bytes: usize,
    clock: u64,
}

#[derive(Debug, Default)]
struct SharedPreparedGpuPageCache {
    state: Mutex<PreparedGpuPageCacheState>,
}

impl SharedPreparedGpuPageCache {
    fn get(
        &self,
        request: &RenderRequest,
        automatic_routing: bool,
    ) -> Option<PreparedRgbImagePage> {
        let key = PreparedGpuPageCacheKey {
            request: PreparedSoftMaskCacheKey::new(request),
            automatic_routing,
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        let entry = state.entries.get_mut(&key)?;
        if !Arc::ptr_eq(&entry.page, &request.page) {
            return None;
        }
        entry.last_used = clock;
        Some(entry.prepared.clone())
    }

    fn insert(
        &self,
        request: &RenderRequest,
        automatic_routing: bool,
        prepared: PreparedRgbImagePage,
    ) -> PreparedRgbImagePage {
        let key = PreparedGpuPageCacheKey {
            request: PreparedSoftMaskCacheKey::new(request),
            automatic_routing,
        };
        let image_bytes = prepared.images.iter().fold(0usize, |total, image| {
            total
                .saturating_add(image.samples.len())
                .saturating_add(
                    image
                        .opacity
                        .as_ref()
                        .map_or(0, |opacity| opacity.samples.len()),
                )
                .saturating_add(image.clip.as_ref().map_or(0, |clip| clip.samples.len()))
                .saturating_add(
                    image
                        .soft_mask
                        .as_ref()
                        .map_or(0, |mask| mask.samples.len()),
                )
        });
        let path_bytes = prepared
            .commands
            .iter()
            .fold(0usize, |total, command| match command {
                PreparedGpuCommand::Path(path) => total
                    .saturating_add(
                        path.raster_data
                            .len()
                            .saturating_mul(std::mem::size_of::<u32>()),
                    )
                    .saturating_add(path.clip.as_ref().map_or(0, |clip| clip.samples.len()))
                    .saturating_add(path.soft_mask.as_ref().map_or(0, |mask| mask.samples.len())),
                PreparedGpuCommand::PathBatch(batch) => total
                    .saturating_add(batch.geometry_bytes)
                    .saturating_add(batch.mask_bytes)
                    .saturating_add(
                        batch
                            .tiles
                            .len()
                            .saturating_mul(std::mem::size_of::<PreparedGpuPathTile>()),
                    )
                    .saturating_add(
                        batch
                            .tile_path_indices
                            .len()
                            .saturating_mul(std::mem::size_of::<u32>()),
                    ),
                PreparedGpuCommand::Image(_) => total,
            });
        let charge = image_bytes.saturating_add(path_bytes);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        if let Some(previous) = state.entries.insert(
            key,
            PreparedGpuPageCacheEntry {
                page: Arc::clone(&request.page),
                prepared: prepared.clone(),
                charge,
                last_used: clock,
            },
        ) {
            state.resident_bytes = state.resident_bytes.saturating_sub(previous.charge);
        }
        state.resident_bytes = state.resident_bytes.saturating_add(charge);
        while state.resident_bytes > PREPARED_GPU_PAGE_CACHE_BYTES && state.entries.len() > 1 {
            let Some(victim) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(removed) = state.entries.remove(&victim) {
                state.resident_bytes = state.resident_bytes.saturating_sub(removed.charge);
            }
        }
        prepared
    }
}

/// Scale an oversized render down uniformly so its RGBA8 surface fits
/// `max_bytes`, adjusting the page transform to match so content still fills
/// the (now smaller) device box.
///
/// A poster-sized MediaBox — e.g. a 9600×14400 pt foldout rendered at screen
/// DPI — would otherwise demand a multi-GB surface and fail the page outright.
/// Real viewers cap effective DPI for such pages; clamping does the same,
/// preserving the whole page at lower resolution instead of dropping it. The
/// downscale is uniform (aspect preserved); the exact per-axis ratios are
/// folded into the transform so the mapping stays consistent after flooring.
///
/// Returns the (possibly reduced) size, the adjusted matrix, and whether a
/// clamp occurred. A `max_bytes` of 0 disables clamping (no budget).
fn clamp_output_to_budget(
    size: DeviceSize,
    matrix: Matrix,
    max_bytes: u64,
) -> (DeviceSize, Matrix, bool) {
    let surface_bytes = (size.width as u64)
        .saturating_mul(size.height as u64)
        .saturating_mul(4);
    if max_bytes == 0 || surface_bytes <= max_bytes {
        return (size, matrix, false);
    }
    // Uniform factor k with (w·k)·(h·k)·4 == max_bytes. Flooring only ever
    // shrinks the result, so the clamped surface is guaranteed ≤ max_bytes.
    let k = ((max_bytes as f64) / (surface_bytes as f64)).sqrt();
    let new_w = ((size.width as f64 * k).floor() as u32).max(1);
    let new_h = ((size.height as f64 * k).floor() as u32).max(1);
    let kx = new_w as f64 / size.width as f64;
    let ky = new_h as f64 / size.height as f64;
    let adjusted = matrix.then(Matrix::scale(kx, ky));
    (
        DeviceSize {
            width: new_w,
            height: new_h,
        },
        adjusted,
        true,
    )
}

/// CPU backend configuration.
#[derive(Debug, Clone)]
pub struct CpuBackendOptions {
    /// Rayon threads for intra-backend parallelism; `None` = rayon default.
    /// Page-level parallelism is the scheduler's job, so this mostly
    /// matters for tile parallelism inside very large single pages.
    pub threads: Option<usize>,
    pub tile_size: u32,
    /// The injected image-codec registry (codec.rs: never global). The
    /// default deployment bundles JPEG (in-house), JPEG 2000 (jp2lam), and
    /// JBIG2 (temporary, see `pdf_image::jbig2`); pass
    /// [`pdf_image::CodecRegistry::empty()`] for a codec-free build.
    pub codecs: pdf_image::CodecRegistry,
    /// Glyph grid-fitting (fonts.md Font Phase 4).
    ///
    /// Defaults to [`HintingPolicy::None`]: this backend is the *normative*
    /// implementation of the frozen surface contract, and hinting moves glyph
    /// geometry, so it is opt-in rather than a silent default. A screen-facing
    /// caller wanting crisp small text sets [`HintingPolicy::Auto`], which
    /// hints only where it pays (axis-aligned, at or below
    /// `pdf_font::AUTO_HINT_MAX_PPEM`).
    pub hinting: pdf_font::HintingPolicy,
}

impl Default for CpuBackendOptions {
    fn default() -> Self {
        let codecs = pdf_image::CodecRegistry::new([
            std::sync::Arc::new(pdf_image::JpegCodec) as std::sync::Arc<dyn pdf_image::ImageCodec>,
            std::sync::Arc::new(pdf_image::JpxCodec),
            std::sync::Arc::new(pdf_image::Jbig2Codec::default()),
            std::sync::Arc::new(pdf_image::CcittCodec),
        ]);
        Self {
            threads: None,
            tile_size: DEFAULT_TILE_SIZE,
            codecs,
            hinting: pdf_font::HintingPolicy::None,
        }
    }
}

/// The CPU render backend.
#[derive(Debug)]
pub struct CpuBackend {
    options: CpuBackendOptions,
    job_counter: AtomicU64,
    /// Document/render-session-scoped parsed-font cache, shared by every worker
    /// that renders through this backend. One backend is created per document
    /// render (the scheduler owns it for the page range), so its lifetime scopes
    /// the cache to that document. See [`prepared::SharedFontProgramCache`].
    shared_fonts: Arc<prepared::SharedFontProgramCache>,
    /// Document/render-session-scoped rendered-glyph coverage cache (PDFium
    /// `CFX_GlyphCache` analog), shared by every worker. Scoped like
    /// `shared_fonts`. See [`prepared::SharedGlyphCache`].
    shared_glyphs: Arc<prepared::SharedGlyphCache>,
    /// Document/render-session-scoped decoded-image cache, shared by every
    /// worker. Scoped like `shared_fonts`. See [`prepared::SharedImageCache`].
    shared_images: Arc<prepared::SharedImageCache>,
    /// Document/render-session-scoped RGB8 conversion cache for the
    /// experimental GPU preparation seam. Stable Arc identity lets the GPU
    /// upload cache serve subsequent tiles without converting or uploading
    /// the same source again.
    shared_rgb_images: Arc<image::SharedRgbImageCache>,
    /// Normalized image `/SMask` and stencil coverage planes. Kept separate
    /// from RGB conversions so packed 1-bit masks remain compact until a GPU
    /// request actually needs an upload-ready alpha plane.
    shared_opacity_images: Arc<image::SharedOpacityImageCache>,
    /// Device-space page `/SMask` planes derived through the CPU executor for
    /// the GPU image seam. Stable sample Arcs preserve warm GPU uploads.
    shared_prepared_soft_masks: Arc<SharedPreparedSoftMaskCache>,
    /// Bounded device-space pattern brushes for patterned `/ImageMask` draws.
    /// Stable RGB/alpha Arcs feed the existing GPU upload cache on revisits.
    shared_prepared_patterned_stencils: Arc<SharedPreparedPatternedStencilCache>,
    /// Complete external-raster command pages. Besides removing repeated
    /// outline lowering, stable path/clip Arcs let the WGPU upload caches reuse
    /// immutable device buffers on page revisits.
    shared_prepared_gpu_pages: Arc<SharedPreparedGpuPageCache>,
}

impl CpuBackend {
    pub fn new(options: CpuBackendOptions) -> Self {
        Self {
            options,
            job_counter: AtomicU64::new(0),
            shared_fonts: Arc::new(prepared::SharedFontProgramCache::default()),
            shared_glyphs: Arc::new(prepared::SharedGlyphCache::default()),
            shared_images: Arc::new(prepared::SharedImageCache::default()),
            shared_rgb_images: Arc::new(image::SharedRgbImageCache::default()),
            shared_opacity_images: Arc::new(image::SharedOpacityImageCache::default()),
            shared_prepared_soft_masks: Arc::new(SharedPreparedSoftMaskCache::default()),
            shared_prepared_patterned_stencils: Arc::new(
                SharedPreparedPatternedStencilCache::default(),
            ),
            shared_prepared_gpu_pages: Arc::new(SharedPreparedGpuPageCache::default()),
        }
    }

    /// Features the rasterizer implements today. Grows phase by phase; the
    /// scheduler routes pages by comparing this to `page.features`.
    ///
    /// Solid paths + constant alpha + clipping (rect and path). Alpha needs no
    /// feature flag; `TRANSPARENCY` denotes non-Normal blend / groups, not yet
    /// implemented.
    fn implemented_features() -> PageFeatures {
        PageFeatures::BASIC_PATHS
            | PageFeatures::CLIPPING
            | PageFeatures::DASHED_STROKES
            | PageFeatures::NONSEPARABLE_BLENDS
            | PageFeatures::TRANSPARENCY
            // Text renders as synthetic placement boxes (fonts.md Font Phase 1);
            // real glyph outlines are a later font phase.
            | PageFeatures::TEXT
            // Axial + radial shadings (`sh` and shading patterns).
            | PageFeatures::SHADINGS
            // Tiling patterns (PatternType 1) + shading patterns.
            | PageFeatures::PATTERNS
            // Images with Flate/RunLength/raw data (codec images route away
            // via the NEEDS_* flags, which stay outside this set).
            | PageFeatures::IMAGES
            | PageFeatures::STENCIL_MASKS
    }

    /// The full advertised feature set: the rasterizer's static
    /// capabilities plus `NEEDS_*` codec coverage from the injected
    /// registry (an empty registry advertises no codecs and codec pages
    /// keep routing away, exactly as before).
    pub fn features(&self) -> PageFeatures {
        Self::implemented_features() | pdf_image::registry_features(&self.options.codecs)
    }

    /// Render into host memory, returning instrumentation. This is the direct
    /// entry point benchmarks and tests use; `submit` wraps it.
    pub fn render_to_host(
        &self,
        request: &RenderRequest,
    ) -> Result<(HostPage, RenderStats), RenderError> {
        let mut ctx = CpuWorkerContext::new();
        self.render_with(request, &mut ctx)
    }

    /// Lower a request into the exact prepared representation consumed by the
    /// diagnostic attribution pass. Production painting should continue to
    /// use [`Self::render_to_host`]; this intentionally exposes preparation
    /// only for non-normative renderer diagnostics.
    pub fn prepare_attribution(
        &self,
        request: &RenderRequest,
    ) -> Result<CpuPreparedPage, RenderError> {
        self.prepare_page_for_external_raster(request, true)
    }

    /// Lower for a non-CPU raster backend. Disabling the rendered-glyph cache
    /// deliberately keeps glyphs as flattened outline paths, allowing the GPU
    /// to generate their coverage instead of uploading CPU-rasterized bitmaps.
    fn prepare_page_for_external_raster(
        &self,
        request: &RenderRequest,
        use_rendered_glyph_cache: bool,
    ) -> Result<CpuPreparedPage, RenderError> {
        if let Some(token) = &request.limits.cancellation
            && token.is_cancelled()
        {
            return Err(RenderError::Cancelled);
        }
        let DeviceSize { width, height } = request.output_size;
        if width == 0 || height == 0 {
            return Err(RenderError::LimitExceeded("zero output dimension"));
        }
        let surface_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(RenderError::LimitExceeded("output size overflow"))?;
        if surface_bytes as u64 > request.limits.max_page_bytes {
            return Err(RenderError::LimitExceeded("max_page_bytes"));
        }
        let decode_limits = image_decode_limits(&request.limits);
        let mut worker = CpuWorkerContext::new();
        let shared_fonts = shared_font_cache_enabled().then_some(self.shared_fonts.as_ref());
        let shared_images = image_cache_enabled().then_some(&self.shared_images);
        let prepared = if use_rendered_glyph_cache {
            prepared::lower_with_font_cache(
                &request.page,
                request.transform.matrix,
                request.output_size,
                &self.options.codecs,
                &decode_limits,
                self.options.hinting,
                request.color_policy,
                &mut worker.fonts,
                shared_fonts,
                glyph_cache_enabled().then_some(self.shared_glyphs.as_ref()),
                shared_images,
            )
        } else {
            prepared::lower_for_external_raster(
                &request.page,
                request.transform.matrix,
                request.output_size,
                &self.options.codecs,
                &decode_limits,
                self.options.hinting,
                request.color_policy,
                &mut worker.fonts,
                shared_fonts,
                shared_images,
            )
        };
        if request
            .limits
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(RenderError::Cancelled);
        }
        Ok(prepared)
    }

    /// Decode and lower an image-only request into the narrow RGB8 + optional
    /// alpha8 vocabulary consumed by the experimental GPU renderer.
    /// Color-space, `/Decode`, image `/SMask`, explicit hard `/Mask`, and solid
    /// stencil semantics are resolved here through the CPU renderer's
    /// normative preparation path.
    ///
    /// `Ok(None)` is a normal capability decline: the page contains another
    /// paint operation or an image requiring text clipping. The caller should
    /// route that request to the regular CPU renderer.
    pub fn prepare_rgb_image_page(
        &self,
        request: &RenderRequest,
    ) -> Result<Option<PreparedRgbImagePage>, RenderError> {
        self.prepare_rgb_image_page_with_policy(request, false)
    }

    /// Prepare an image-only request using the measured automatic routing
    /// policy. Minified 1-bit images decline before RGB expansion so the
    /// caller can use the CPU's packed-bilevel destination sampler.
    pub fn prepare_rgb_image_page_for_auto(
        &self,
        request: &RenderRequest,
    ) -> Result<Option<PreparedRgbImagePage>, RenderError> {
        self.prepare_rgb_image_page_with_policy(request, true)
    }

    fn prepare_rgb_image_page_with_policy(
        &self,
        request: &RenderRequest,
        automatic_routing: bool,
    ) -> Result<Option<PreparedRgbImagePage>, RenderError> {
        if request
            .limits
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(RenderError::Cancelled);
        }
        if let Some(prepared) = self
            .shared_prepared_gpu_pages
            .get(request, automatic_routing)
        {
            return Ok(Some(prepared));
        }
        let prepared = self.prepare_page_for_external_raster(request, false)?;
        let mut images = Vec::with_capacity(prepared.ops.len());
        let mut commands = Vec::with_capacity(prepared.ops.len());
        let mut clip_masks: Vec<Option<Option<PreparedImageClip>>> =
            vec![None; prepared.clips.len()];
        let mut clip_raster = RasterKernel::default();
        let has_soft_masks = prepared
            .ops
            .iter()
            .any(|op| matches!(op, prepared::PreparedOp::PushSoftMask { .. }));
        let mut derived_soft_masks = if has_soft_masks {
            if let Some(masks) = self.shared_prepared_soft_masks.get(request) {
                masks
            } else {
                let Some(masks) = exec::prepare_soft_masks(&prepared) else {
                    return Err(RenderError::Cancelled);
                };
                let masks = masks
                    .into_iter()
                    .map(|mask| {
                        mask.map(|mask| PreparedImageSoftMask {
                            bounds: mask.bounds,
                            samples: Arc::from(mask.data),
                            outside: mask.outside,
                        })
                    })
                    .collect();
                self.shared_prepared_soft_masks.insert(request, masks)
            }
        } else {
            std::iter::repeat_with(|| None)
                .take(prepared.ops.len())
                .collect()
        };
        let mut active_soft_masks: Vec<Option<PreparedImageSoftMask>> = Vec::new();
        let has_native_image_draw = {
            let mut scan = 0;
            let mut found = false;
            while scan < prepared.ops.len() {
                match &prepared.ops[scan] {
                    prepared::PreparedOp::Image(_) => {
                        found = true;
                        break;
                    }
                    prepared::PreparedOp::PushSoftMask { content_end, .. } => {
                        scan = *content_end as usize;
                    }
                    _ => scan += 1,
                }
            }
            found
        };

        let mut op_index = 0;
        while op_index < prepared.ops.len() {
            let op = &prepared.ops[op_index];
            let image = match op {
                prepared::PreparedOp::Image(image) => image,
                prepared::PreparedOp::Draw(command) => {
                    if command.alpha == 0 {
                        op_index += 1;
                        continue;
                    }
                    if command.shading.is_some() {
                        return Ok(None);
                    }
                    let mut edges = Vec::new();
                    for &(start, end) in &prepared.subpaths
                        [command.subpath_range.0 as usize..command.subpath_range.1 as usize]
                    {
                        let points = &prepared.points[start..end];
                        if points.len() < 2 {
                            continue;
                        }
                        let simplified =
                            simplify_closed_gpu_contour(points, GPU_PATH_SIMPLIFY_TOLERANCE);
                        for pair in simplified.windows(2) {
                            edges.push([pair[0][0], pair[0][1], pair[1][0], pair[1][1]]);
                        }
                        let first = simplified[0];
                        let last = simplified[simplified.len() - 1];
                        if first != last {
                            edges.push([last[0], last[1], first[0], first[1]]);
                        }
                    }
                    if edges.is_empty() {
                        op_index += 1;
                        continue;
                    }
                    let Some((raster_data, band_edge_references)) =
                        pack_gpu_path_raster(command.bounds, &edges)
                    else {
                        return Ok(None);
                    };
                    let clip = prepare_external_clip(
                        &prepared,
                        &mut clip_masks,
                        &mut clip_raster,
                        command.clip,
                        command.clip_has_mask,
                    )?;
                    commands.push(PreparedGpuCommand::Path(PreparedGpuPath {
                        bounds: command.bounds,
                        raster_data,
                        edge_count: edges.len() as u32,
                        band_edge_references,
                        even_odd: matches!(command.rule, raster::FillRule::EvenOdd),
                        rgb: command.rgb,
                        alpha: command.alpha,
                        blend: command.blend,
                        clip,
                        soft_mask: active_soft_masks.last().cloned().flatten(),
                    }));
                    op_index += 1;
                    continue;
                }
                prepared::PreparedOp::TiledFill(tiling) if tiling.stencil.is_some() => {
                    // Preparing a pattern brush already paints its complete
                    // bounded source on CPU. Auto uses this bridge only when
                    // the page also has a native image draw whose GPU gain can
                    // amortize that work; forced GPU remains available for
                    // semantic validation and experimentation.
                    if automatic_routing && !has_native_image_draw {
                        return Ok(None);
                    }
                    let patterned = if let Some(cached) = self
                        .shared_prepared_patterned_stencils
                        .get(request, op_index)
                    {
                        cached
                    } else {
                        let pattern_bytes = (tiling.bounds.width as usize)
                            .checked_mul(tiling.bounds.height as usize)
                            .and_then(|pixels| pixels.checked_mul(4));
                        if pattern_bytes
                            .is_none_or(|bytes| bytes > MAX_PREPARED_PATTERNED_STENCIL_BYTES)
                        {
                            return Ok(None);
                        }
                        let Some(prepared) = exec::prepare_patterned_stencil(&prepared, tiling)
                        else {
                            if request
                                .limits
                                .cancellation
                                .as_ref()
                                .is_some_and(|token| token.is_cancelled())
                            {
                                return Err(RenderError::Cancelled);
                            }
                            return Ok(None);
                        };
                        self.shared_prepared_patterned_stencils
                            .insert(request, op_index, prepared)
                    };
                    let bounds = patterned.bounds;
                    let width = f64::from(bounds.width);
                    let height = f64::from(bounds.height);
                    if width == 0.0 || height == 0.0 {
                        return Ok(None);
                    }
                    let image_index = images.len() as u32;
                    images.push(PreparedRgbImage {
                        bounds,
                        // The prepared brush is a top-down, one-texel-per-
                        // device-pixel plane. WGPU's image convention has v=1
                        // at the top, hence the negative d term.
                        device_to_image: Matrix {
                            a: 1.0 / width,
                            b: 0.0,
                            c: 0.0,
                            d: -1.0 / height,
                            e: -f64::from(bounds.x) / width,
                            f: 1.0 + f64::from(bounds.y) / height,
                        },
                        width: bounds.width,
                        height: bounds.height,
                        samples: patterned.samples,
                        interpolation: pdf_page_ir::InterpolationMode::Nearest,
                        footprint: [1.0, 1.0],
                        opacity: Some(PreparedImageOpacity {
                            width: bounds.width,
                            height: bounds.height,
                            samples: patterned.opacity,
                            footprint: [1.0, 1.0],
                            box_filter: false,
                        }),
                        clip: None,
                        soft_mask: active_soft_masks.last().cloned().flatten(),
                        alpha: tiling.alpha,
                        blend: tiling.blend,
                        stencil_rgb: None,
                    });
                    commands.push(PreparedGpuCommand::Image(image_index));
                    op_index += 1;
                    continue;
                }
                prepared::PreparedOp::PushSoftMask { content_end, .. } => {
                    let Some(mask) = derived_soft_masks[op_index].take() else {
                        return Ok(None);
                    };
                    active_soft_masks.push(Some(mask));
                    op_index = *content_end as usize;
                    continue;
                }
                prepared::PreparedOp::PushSoftMaskNone => {
                    active_soft_masks.push(None);
                    op_index += 1;
                    continue;
                }
                prepared::PreparedOp::PopSoftMask => {
                    active_soft_masks.pop();
                    op_index += 1;
                    continue;
                }
                // Searchable scan PDFs often overlay zero-alpha OCR text.
                // It is semantically non-painting and may be discarded at
                // this narrow image-renderer seam just as the CPU executor's
                // alpha blend would discard it.
                prepared::PreparedOp::GlyphRun(run) if run.alpha == 0 => {
                    op_index += 1;
                    continue;
                }
                _ => return Ok(None),
            };
            if automatic_routing
                && !image.is_stencil
                && image.bpc == 1
                && image
                    .footprint
                    .iter()
                    .any(|axis| *axis > AUTO_CPU_BILEVEL_MINIFICATION_FOOTPRINT)
            {
                return Ok(None);
            }
            let samples = if image.is_stencil {
                Arc::from([])
            } else {
                let Some(samples) = self
                    .shared_rgb_images
                    .get_or_convert(image, request.limits.cancellation.as_ref())?
                else {
                    return Ok(None);
                };
                samples
            };
            let opacity = if image.is_stencil {
                let Some(samples) = self.shared_opacity_images.get_or_expand(
                    &image.samples,
                    image.width,
                    image.height,
                    image.bpc,
                    image.decode.as_ref(),
                    true,
                    request.limits.cancellation.as_ref(),
                )?
                else {
                    return Ok(None);
                };
                Some(PreparedImageOpacity {
                    width: image.width,
                    height: image.height,
                    samples,
                    footprint: image.footprint,
                    box_filter: true,
                })
            } else if let Some(mask) = image.smask.as_ref() {
                let Some(samples) = self.shared_opacity_images.get_or_expand(
                    &mask.samples,
                    mask.width,
                    mask.height,
                    mask.bits_per_component,
                    mask.decode.as_ref(),
                    false,
                    request.limits.cancellation.as_ref(),
                )?
                else {
                    return Ok(None);
                };
                Some(PreparedImageOpacity {
                    width: mask.width,
                    height: mask.height,
                    samples,
                    footprint: [
                        (image.inv.a.abs() + image.inv.c.abs()) * mask.width as f64,
                        (image.inv.b.abs() + image.inv.d.abs()) * mask.height as f64,
                    ],
                    box_filter: true,
                })
            } else if let Some(mask) = image.mask.as_ref() {
                match mask {
                    ImageMask::ColorKey(ranges) => {
                        let Some(samples) = self.shared_opacity_images.get_or_expand_color_key(
                            &image.samples,
                            image.width,
                            image.height,
                            image.bpc,
                            image.color_space.components(),
                            ranges,
                            request.limits.cancellation.as_ref(),
                        )?
                        else {
                            return Ok(None);
                        };
                        Some(PreparedImageOpacity {
                            width: image.width,
                            height: image.height,
                            samples,
                            footprint: image.footprint,
                            box_filter: false,
                        })
                    }
                    ImageMask::Stencil(mask) => {
                        let Some(samples) = self.shared_opacity_images.get_or_expand(
                            &mask.samples,
                            mask.width,
                            mask.height,
                            mask.bits_per_component,
                            mask.decode.as_ref(),
                            true,
                            request.limits.cancellation.as_ref(),
                        )?
                        else {
                            return Ok(None);
                        };
                        Some(PreparedImageOpacity {
                            width: mask.width,
                            height: mask.height,
                            samples,
                            footprint: [
                                (image.inv.a.abs() + image.inv.c.abs()) * mask.width as f64,
                                (image.inv.b.abs() + image.inv.d.abs()) * mask.height as f64,
                            ],
                            box_filter: false,
                        })
                    }
                }
            } else {
                None
            };
            let clip = prepare_external_clip(
                &prepared,
                &mut clip_masks,
                &mut clip_raster,
                image.clip,
                image.clip_has_mask,
            )?;

            let image_index = images.len() as u32;
            images.push(PreparedRgbImage {
                bounds: image.bounds,
                device_to_image: image.inv,
                width: image.width,
                height: image.height,
                samples,
                interpolation: image.interpolation,
                footprint: image.footprint,
                opacity,
                clip,
                soft_mask: active_soft_masks.last().cloned().flatten(),
                alpha: image.alpha,
                blend: image.blend,
                stencil_rgb: image.is_stencil.then_some(image.stencil_rgb),
            });
            commands.push(PreparedGpuCommand::Image(image_index));
            op_index += 1;
        }

        if commands.is_empty() || (automatic_routing && images.is_empty()) {
            return Ok(None);
        }
        let Some(commands) = batch_gpu_path_commands(
            prepared.size,
            commands,
            request.limits.cancellation.as_ref(),
        ) else {
            if request
                .limits
                .cancellation
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
            {
                return Err(RenderError::Cancelled);
            }
            return Ok(None);
        };
        // Native vector coverage remains forced-GPU-only until the measured
        // cost gate has a corpus-backed crossover. Auto must never turn a
        // newly supported semantic shape into an unmeasured performance loss.
        if automatic_routing
            && commands.iter().any(|command| {
                matches!(
                    command,
                    PreparedGpuCommand::Path(_) | PreparedGpuCommand::PathBatch(_)
                )
            })
        {
            return Ok(None);
        }
        let prepared = PreparedRgbImagePage {
            size: prepared.size,
            images,
            commands,
        };
        Ok(Some(self.shared_prepared_gpu_pages.insert(
            request,
            automatic_routing,
            prepared,
        )))
    }

    /// Render reusing a caller-owned worker context (avoids reallocating the
    /// coverage/flatten scratch across pages).
    pub fn render_with(
        &self,
        request: &RenderRequest,
        ctx: &mut CpuWorkerContext,
    ) -> Result<(HostPage, RenderStats), RenderError> {
        if let Some(token) = &request.limits.cancellation
            && token.is_cancelled()
        {
            return Err(RenderError::Cancelled);
        }
        if request.output_size.width == 0 || request.output_size.height == 0 {
            return Err(RenderError::LimitExceeded("zero output dimension"));
        }
        // Clamp an oversized page down to the byte budget rather than failing
        // it (a poster-sized MediaBox at screen DPI). Output dimensions and the
        // transform shrink together; the page renders whole, at lower res.
        let (output_size, transform_matrix, was_clamped) = clamp_output_to_budget(
            request.output_size,
            request.transform.matrix,
            request.limits.max_page_bytes,
        );
        let DeviceSize { width, height } = output_size;
        let bpp = request.output_format.bytes_per_pixel();
        let out_stride = width as usize * bpp;
        let out_bytes = out_stride
            .checked_mul(height as usize)
            .ok_or(RenderError::LimitExceeded("output size overflow"))?;
        // The internal surface is always RGBA8; bound against that. After the
        // clamp the RGBA8 surface fits by construction — this stays a hard
        // backstop for an output format wider than 4 bytes/pixel.
        let surface_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|p| p.checked_mul(4))
            .ok_or(RenderError::LimitExceeded("output size overflow"))?;
        if surface_bytes.max(out_bytes) as u64 > request.limits.max_page_bytes {
            return Err(RenderError::LimitExceeded("max_page_bytes"));
        }

        let mut stats = RenderStats::default();
        if was_clamped {
            stats.recovery_notes.push(format!(
                "oversized page clamped to {width}x{height} to fit max_page_bytes"
            ));
        }

        // Request-specific lowering: transform, flatten, cull, classify, once.
        #[cfg(feature = "profiling")]
        let lower_start = std::time::Instant::now();
        let decode_limits = image_decode_limits(&request.limits);
        let prepared = prepared::lower_with_font_cache(
            &request.page,
            transform_matrix,
            output_size,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
            request.color_policy,
            &mut ctx.fonts,
            shared_font_cache_enabled().then_some(self.shared_fonts.as_ref()),
            glyph_cache_enabled().then_some(self.shared_glyphs.as_ref()),
            image_cache_enabled().then_some(&self.shared_images),
        );
        #[cfg(feature = "profiling")]
        {
            stats.lower = lower_start.elapsed();
        }
        stats.ops_total = request.page.operations.len() as u32;

        #[cfg(feature = "profiling")]
        let prep_start = std::time::Instant::now();
        let mut surface = Surface::new(width as usize, height as usize, request.background);
        stats.surface_bytes = surface.bytes();
        #[cfg(feature = "profiling")]
        {
            stats.prep = prep_start.elapsed();
        }

        exec::execute(&prepared, &mut surface, ctx, &mut stats);
        if stats.cancelled {
            return Err(RenderError::Cancelled);
        }
        // Fold in codec draws dropped during top-level lowering (tiling-cell
        // drops are absorbed inside `execute`). A page left blank by an
        // undecodable image is now observable, never a silent clean pass.
        stats.absorb_diagnostics(&prepared.diagnostics);
        #[cfg(feature = "profiling")]
        stats.profile.merge(&prepared.profile_report());
        let _ = &self.options;

        #[cfg(feature = "profiling")]
        let output_start = std::time::Instant::now();
        let (stride, pixels) = surface.into_output(request.output_format);
        let pixels: Arc<[u8]> = Arc::from(pixels);
        stats.output_bytes = pixels.len() as u64;
        #[cfg(feature = "profiling")]
        {
            stats.output = output_start.elapsed();
        }
        Ok((
            HostPage {
                width,
                height,
                stride,
                format: request.output_format,
                pixels,
            },
            stats,
        ))
    }

    /// Render with the same semantics as [`Self::render_with`] while returning
    /// the feature-gated performance report consumed by benchmark tooling.
    #[cfg(feature = "profiling")]
    pub fn render_profiled_with(
        &self,
        request: &RenderRequest,
        ctx: &mut CpuWorkerContext,
    ) -> Result<(HostPage, RenderStats, pdf_profiling::ProfileReport), RenderError> {
        let total_start = std::time::Instant::now();
        let (host, stats) = self.render_with(request, ctx)?;
        let mut profile = pdf_profiling::ProfileReport::new();
        profile.add_duration("render.lower", stats.lower);
        profile.add_duration("render.surface", stats.prep);
        profile.add_duration("render.execute", stats.raster);
        profile.add_duration("render.output", stats.output);
        profile.add_duration("render.total", total_start.elapsed());
        profile.increment("render.operations", stats.ops_total as u64);
        profile.increment("render.commands", stats.commands as u64);
        profile.increment("render.painted_ops", stats.ops_painted as u64);
        profile.increment("render.edges", stats.edges);
        profile.increment("render.covered_pixels", stats.covered_pixels);
        profile.increment(
            "render.transparency_groups",
            stats.transparency_groups as u64,
        );
        profile.increment("render.soft_masks", stats.soft_masks as u64);
        profile.increment("render.output_bytes", stats.output_bytes);
        profile.merge(&stats.profile);
        let transient_render_bytes = stats.surface_bytes.saturating_add(stats.output_bytes);
        profile.allocate_bytes(transient_render_bytes);
        profile.release_bytes(transient_render_bytes);
        Ok((host, stats, profile))
    }

    /// Lower a compiled page once for repeated prepared-page execution
    /// experiments. The opaque prepared page is intentionally feature-gated:
    /// production callers should keep using `render_with`.
    #[cfg(feature = "profiling")]
    pub fn prepare_profiled(
        &self,
        request: &RenderRequest,
    ) -> Result<(CpuPreparedPage, pdf_profiling::ProfileReport), RenderError> {
        let DeviceSize { width, height } = request.output_size;
        if width == 0 || height == 0 {
            return Err(RenderError::LimitExceeded("zero output dimension"));
        }
        let surface_bytes = width as u64 * height as u64 * 4;
        if surface_bytes > request.limits.max_page_bytes {
            return Err(RenderError::LimitExceeded("max_page_bytes"));
        }
        let start = std::time::Instant::now();
        let decode_limits = image_decode_limits(&request.limits);
        let prepared = prepared::lower(
            &request.page,
            request.transform.matrix,
            request.output_size,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
        );
        if decode_limits.is_cancelled() {
            return Err(RenderError::Cancelled);
        }
        let mut profile = prepared.profile_report();
        profile.add_duration("render.prepare", start.elapsed());
        Ok((prepared, profile))
    }

    /// Prepare using profiling-only decoded-image residency shared by the
    /// caller across repeated preparations.
    #[cfg(feature = "profiling")]
    pub fn prepare_with_decode_cache_profiled(
        &self,
        request: &RenderRequest,
        cache: DecodedImageCache,
    ) -> Result<(CpuPreparedPage, pdf_profiling::ProfileReport), RenderError> {
        let DeviceSize { width, height } = request.output_size;
        if width == 0 || height == 0 {
            return Err(RenderError::LimitExceeded("zero output dimension"));
        }
        let surface_bytes = width as u64 * height as u64 * 4;
        if surface_bytes > request.limits.max_page_bytes {
            return Err(RenderError::LimitExceeded("max_page_bytes"));
        }
        let start = std::time::Instant::now();
        let decode_limits = image_decode_limits(&request.limits);
        let prepared = prepared::lower_with_decode_cache(
            &request.page,
            request.transform.matrix,
            request.output_size,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
            cache,
        );
        if decode_limits.is_cancelled() {
            return Err(RenderError::Cancelled);
        }
        let mut profile = prepared.profile_report();
        profile.add_duration("render.prepare", start.elapsed());
        Ok((prepared, profile))
    }

    /// Decode the compiled page's unique embedded codec payloads without
    /// lowering geometry or rasterizing.
    #[cfg(feature = "profiling")]
    pub fn decode_page_profiled(
        &self,
        request: &RenderRequest,
    ) -> Result<pdf_profiling::ProfileReport, RenderError> {
        let decode_limits = image_decode_limits(&request.limits);
        let profile = prepared::decode_page(
            &request.page,
            &self.options.codecs,
            &decode_limits,
            self.options.hinting,
        );
        if decode_limits.is_cancelled() {
            return Err(RenderError::Cancelled);
        }
        Ok(profile)
    }

    /// Execute a page previously returned by [`Self::prepare_profiled`].
    #[cfg(feature = "profiling")]
    pub fn execute_prepared_profiled(
        &self,
        request: &RenderRequest,
        prepared: &CpuPreparedPage,
        ctx: &mut CpuWorkerContext,
    ) -> Result<(HostPage, RenderStats, pdf_profiling::ProfileReport), RenderError> {
        let DeviceSize { width, height } = request.output_size;
        if prepared.size != request.output_size || width == 0 || height == 0 {
            return Err(RenderError::Backend(
                "prepared page does not match request".into(),
            ));
        }
        let total_start = std::time::Instant::now();
        let surface_start = std::time::Instant::now();
        let mut surface = Surface::new(width as usize, height as usize, request.background);
        let mut stats = RenderStats {
            surface_bytes: surface.bytes(),
            ..RenderStats::default()
        };
        stats.prep = surface_start.elapsed();
        exec::execute(prepared, &mut surface, ctx, &mut stats);
        if stats.cancelled {
            return Err(RenderError::Cancelled);
        }
        stats.absorb_diagnostics(&prepared.diagnostics);
        let output_start = std::time::Instant::now();
        let (stride, pixels) = surface.into_output(request.output_format);
        let pixels: Arc<[u8]> = Arc::from(pixels);
        stats.output_bytes = pixels.len() as u64;
        stats.output = output_start.elapsed();
        let mut profile = stats.profile.clone();
        profile.add_duration("render.surface", stats.prep);
        profile.add_duration("render.execute", stats.raster);
        profile.add_duration("render.output", stats.output);
        profile.add_duration("render.prepared_total", total_start.elapsed());
        profile.increment("render.output_bytes", stats.output_bytes);
        let transient_render_bytes = stats.surface_bytes.saturating_add(stats.output_bytes);
        profile.allocate_bytes(transient_render_bytes);
        profile.release_bytes(transient_render_bytes);
        Ok((
            HostPage {
                width,
                height,
                stride,
                format: request.output_format,
                pixels,
            },
            stats,
            profile,
        ))
    }
}

impl Default for CpuBackend {
    fn default() -> Self {
        Self::new(CpuBackendOptions::default())
    }
}

/// Whether the document-scoped shared font parse cache is active. On by default;
/// `PDF_RENDERER_FONTCACHE=off` (or `0`) disables it, for paired A/B isolation
/// of the cache on one binary. Read once.
fn shared_font_cache_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("PDF_RENDERER_FONTCACHE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "off" && v != "0" && v != "false"
        }
        Err(_) => true,
    })
}

/// Whether the document-scoped rendered-glyph coverage cache is active. On by
/// default; `PDF_RENDERER_GLYPHCACHE=off` (or `0`) disables it, for paired A/B
/// isolation of the cache on one binary. Read once.
fn glyph_cache_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("PDF_RENDERER_GLYPHCACHE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "off" && v != "0" && v != "false"
        }
        Err(_) => true,
    })
}

/// Whether the document-scoped decoded-image cache is active. On by default;
/// `PDF_RENDERER_IMAGECACHE=off` (or `0`) disables it, for paired A/B
/// isolation of the cache on one binary. Read once.
fn image_cache_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("PDF_RENDERER_IMAGECACHE") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            v != "off" && v != "0" && v != "false"
        }
        Err(_) => true,
    })
}

impl Drop for CpuBackend {
    fn drop(&mut self) {
        // Optional observability for the parse-cache measurement: on backend
        // teardown (one document render session), report how often the shared
        // Type 1 / bare-CFF parse cache served a program without reparsing.
        if std::env::var_os("PDF_RENDERER_FONTCACHE_STATS").is_some() {
            let (hits, inserts) = self.shared_fonts.stats();
            eprintln!("shared-font-cache: hits={hits} inserts={inserts}");
        }
        if std::env::var_os("PDF_RENDERER_GLYPHCACHE_STATS").is_some() {
            let (hits, misses, inserts) = self.shared_glyphs.stats();
            let total = hits + misses;
            let rate = if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            };
            eprintln!(
                "shared-glyph-cache: hits={hits} misses={misses} inserts={inserts} hit_rate={rate:.4}"
            );
        }
        if std::env::var_os("PDF_RENDERER_IMAGECACHE_STATS").is_some() {
            let (hits, misses, inserts) = self.shared_images.stats();
            let total = hits + misses;
            let rate = if total > 0 {
                hits as f64 / total as f64
            } else {
                0.0
            };
            eprintln!(
                "shared-image-cache: hits={hits} misses={misses} inserts={inserts} hit_rate={rate:.4}"
            );
        }
    }
}

impl RenderBackend for CpuBackend {
    fn id(&self) -> BackendId {
        BackendId::Cpu
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            formats: vec![OutputFormat::Rgba8PremultipliedSrgb, OutputFormat::Gray8],
            max_surface: DeviceSize {
                width: 1 << 16,
                height: 1 << 16,
            },
            features: self.features(),
            resident_surfaces: false,
            postprocess: PostprocessCapabilities::HOST_ALL,
        }
    }

    fn supports(&self, page: &CompiledPage, _request: &RenderRequest) -> SupportLevel {
        let missing = page.features.difference(self.features());
        if missing.is_empty() {
            SupportLevel::Native
        } else {
            SupportLevel::Unsupported(UnsupportedFeature {
                missing,
                detail: "feature not yet implemented in CPU rasterizer",
            })
        }
    }

    fn submit(&self, request: RenderRequest) -> Result<RenderTicket, SubmitError> {
        let job_id = self.job_counter.fetch_add(1, Ordering::Relaxed);
        let (ticket, tx) = RenderTicket::new(job_id);
        // Synchronous fulfillment for now; the scheduler provides page-level
        // parallelism by calling submit from its own worker pool. Phase 4
        // may move fulfillment onto an internal rayon pool for tile
        // parallelism inside huge pages.
        let result = self
            .render_to_host(&request)
            .map(|(host, _stats)| RenderedPage::Host(host));
        let _ = tx.send(result);
        Ok(ticket)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn image_limits_tighten_defaults_and_share_cancellation() {
        let token = pdf_render_api::CancellationToken::new();
        let limits = RenderLimits {
            max_page_bytes: 4096,
            cancellation: Some(token.clone()),
            ..RenderLimits::default()
        };
        let codec = image_decode_limits(&limits);
        assert_eq!(codec.max_input_bytes, 4096);
        assert_eq!(codec.max_output_bytes, 4096);
        assert_eq!(codec.max_working_bytes, 4096);
        assert!(!codec.is_cancelled());
        token.cancel();
        assert!(codec.is_cancelled());

        let loose = image_decode_limits(&RenderLimits::default());
        let defaults = pdf_image::DecodeLimits::default();
        assert_eq!(loose.max_input_bytes, defaults.max_input_bytes);
        assert_eq!(loose.max_working_bytes, defaults.max_working_bytes);
    }

    #[test]
    fn clamp_leaves_within_budget_pages_untouched() {
        let size = DeviceSize {
            width: 1000,
            height: 800,
        };
        let (out, m, clamped) = clamp_output_to_budget(size, Matrix::IDENTITY, 2 << 30);
        assert!(!clamped);
        assert_eq!(out, size);
        assert_eq!(m, Matrix::IDENTITY);
    }

    #[test]
    fn clamp_zero_budget_disables() {
        let size = DeviceSize {
            width: 100_000,
            height: 100_000,
        };
        let (out, _, clamped) = clamp_output_to_budget(size, Matrix::IDENTITY, 0);
        assert!(!clamped);
        assert_eq!(out, size);
    }

    #[test]
    fn capabilities_advertise_complete_host_postprocessing() {
        let postprocess = CpuBackend::default().capabilities().postprocess;
        assert_eq!(
            postprocess.operations,
            pdf_render_api::PostprocessOperations::all()
        );
        assert!(!postprocess.resident_execution);
    }

    #[test]
    fn clamp_oversized_fits_budget_and_folds_scale_into_transform() {
        // A poster page whose RGBA8 surface far exceeds the budget.
        let size = DeviceSize {
            width: 20_000,
            height: 30_000,
        };
        let budget = 2u64 << 30; // 2 GiB
        let (out, m, clamped) = clamp_output_to_budget(size, Matrix::scale(2.0, 2.0), budget);
        assert!(clamped);
        // Clamped surface fits the budget by construction.
        let surface = out.width as u64 * out.height as u64 * 4;
        assert!(surface <= budget, "surface {surface} > budget {budget}");
        // Aspect ratio is preserved within a pixel of rounding.
        let want = size.width as f64 / size.height as f64;
        let got = out.width as f64 / out.height as f64;
        assert!((want - got).abs() < 1e-3);
        // The original scale-2 matrix mapped page (10000,15000) onto the old
        // device far corner (20000,30000); after the clamp the same page point
        // must land on the new box's far corner.
        let corner = m.apply(pdf_page_ir::Point {
            x: 10_000.0,
            y: 15_000.0,
        });
        assert!((corner.x - out.width as f64).abs() < 2.0);
        assert!((corner.y - out.height as f64).abs() < 2.0);
    }

    use pdf_page_ir::{PageBounds, Rect};
    use pdf_render_api::{
        AnnotationMode, Background, OutputResidency, PageTransform, RenderLimits, RenderQuality,
        render_blocking,
    };

    fn basic_request(format: OutputFormat, background: Background) -> RenderRequest {
        let page = CompiledPage::empty(PageBounds {
            crop: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 612.0,
                y1: 792.0,
            },
            rotate: 0,
        });
        RenderRequest {
            page: Arc::new(page),
            transform: PageTransform {
                matrix: pdf_page_ir::Matrix::IDENTITY,
            },
            crop: None,
            output_size: DeviceSize {
                width: 8,
                height: 4,
            },
            output_format: format,
            background,
            annotations: AnnotationMode::None,
            color_policy: pdf_render_api::RenderColorPolicy::Original,
            quality: RenderQuality::Normal,
            limits: RenderLimits::default(),
            residency: OutputResidency::HostRequired,
        }
    }

    #[test]
    fn renders_white_background_end_to_end() {
        let backend = CpuBackend::default();
        let request = basic_request(OutputFormat::Rgba8PremultipliedSrgb, Background::White);
        assert!(matches!(
            backend.supports(&request.page, &request),
            SupportLevel::Native
        ));
        let page = render_blocking(&backend, request).unwrap();
        let host = page.as_host().unwrap();
        assert_eq!((host.width, host.height, host.stride), (8, 4, 32));
        assert!(host.pixels.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn respects_memory_limit() {
        // A budget too small for the full surface clamps the page down to fit
        // rather than rejecting it: the render succeeds and the RGBA8 surface
        // stays within budget (the OOM guard is preserved, just non-fatally).
        let backend = CpuBackend::default();
        let mut request = basic_request(OutputFormat::Gray8, Background::White);
        request.limits.max_page_bytes = 8;
        let page = render_blocking(&backend, request).unwrap();
        let host = page.as_host().unwrap();
        assert!(host.width >= 1 && host.height >= 1);
        assert!(
            host.width as u64 * host.height as u64 * 4 <= 8,
            "clamped surface {}x{} exceeds the 8-byte budget",
            host.width,
            host.height
        );
    }

    #[test]
    fn declines_pages_with_unimplemented_features() {
        let backend = CpuBackend::default();
        let request = basic_request(OutputFormat::Gray8, Background::White);
        let mut page = (*request.page).clone();
        // Soft masks are not implemented yet, so such a page is declined.
        page.features = PageFeatures::SOFT_MASKS;
        assert!(matches!(
            backend.supports(&page, &request),
            SupportLevel::Unsupported(_)
        ));
    }
}
