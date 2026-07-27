//! Experimental WGPU PDF render backend.
//!
//! The first production-shaped slice paints decoded images prepared as RGB8
//! plus optional alpha8 into one GPU-resident page surface and reads that
//! surface back once.
//! All other PDF paint remains on the CPU backend until its semantics are
//! implemented here. Image decoding, color conversion, and request lowering
//! are shared with the normative CPU renderer through its narrow
//! [`pdf_render_cpu::PreparedRgbImagePage`] seam.

use std::collections::{BTreeSet, HashMap};
#[cfg(test)]
use std::sync::atomic::AtomicU8;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use lege_gpu::compute::{AdapterInfo, SharedGpuContext, wgpu};
use pdf_page_ir::{
    BlendMode, CompiledPage, DeviceSize, DisplayOp, ImageColorSpace, ImageMask, PageFeatures, Paint,
};
#[cfg(test)]
use pdf_page_ir::{PathData, PathVerb};
use pdf_render_api::{
    BackendCapabilities, BackendId, Background, CancellationToken, HostPage, OutputFormat,
    PostprocessCapabilities, RenderBackend, RenderColorPolicy, RenderError, RenderQuality,
    RenderRequest, RenderTicket, RenderedPage, SubmitError, SupportLevel, UnsupportedFeature,
};
use pdf_render_cpu::{
    CpuBackend, CpuBackendOptions, CpuWorkerContext, MAX_PREPARED_OPACITY_CONVERSION_BYTES,
    MAX_PREPARED_RGB_CONVERSION_BYTES, PreparedGpuCommand, PreparedGpuPath, PreparedGpuPathBatch,
    PreparedRgbImage, PreparedRgbImagePage, RenderStats,
};
use wgpu::util::DeviceExt;

const WORKGROUP_EDGE: u32 = 16;
const PATH_WORKGROUP_EDGE: u32 = 8;
const MAX_BATCH_DISPATCH_EDGE: u32 = 65_535;
const MAX_BOX_FOOTPRINT: f64 = 64.0;
const DEFAULT_UPLOAD_CACHE_BYTES: usize = 128 * 1024 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ShaderParams {
    page_width: u32,
    page_height: u32,
    image_width: u32,
    image_height: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
    footprint_x: f32,
    footprint_y: f32,
    interpolation: u32,
    background: u32,
    mask_width: u32,
    mask_height: u32,
    mask_footprint_x: f32,
    mask_footprint_y: f32,
    has_opacity: u32,
    is_stencil: u32,
    stencil_rgb: u32,
    opacity_box_filter: u32,
    clip_x: i32,
    clip_y: i32,
    clip_width: u32,
    clip_height: u32,
    has_clip: u32,
    image_alpha: u32,
    blend_mode: u32,
    _pad2: u32,
    soft_x: i32,
    soft_y: i32,
    soft_width: u32,
    soft_height: u32,
    has_soft_mask: u32,
    soft_outside: u32,
    _pad3: u32,
    _pad4: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PathBatchParams {
    page_width: u32,
    page_height: u32,
    tile_count: u32,
    samples: u32,
    dispatch_width: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuPathDescriptor {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    geometry_base: u32,
    rgb: u32,
    alpha: u32,
    blend_mode: u32,
    even_odd: u32,
    clip_x: i32,
    clip_y: i32,
    clip_width: u32,
    clip_height: u32,
    clip_offset: u32,
    has_clip: u32,
    soft_x: i32,
    soft_y: i32,
    soft_width: u32,
    soft_height: u32,
    soft_offset: u32,
    soft_outside: u32,
    has_soft_mask: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuPathTile {
    x: u32,
    y: u32,
    path_offset: u32,
    path_count: u32,
}

impl ShaderParams {
    fn clear(size: DeviceSize, background: u32) -> Self {
        Self {
            page_width: size.width,
            page_height: size.height,
            image_width: 1,
            image_height: 1,
            x0: 0,
            y0: 0,
            x1: size.width as i32,
            y1: size.height as i32,
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
            footprint_x: 1.0,
            footprint_y: 1.0,
            interpolation: 0,
            background,
            mask_width: 1,
            mask_height: 1,
            mask_footprint_x: 1.0,
            mask_footprint_y: 1.0,
            has_opacity: 0,
            is_stencil: 0,
            stencil_rgb: 0,
            opacity_box_filter: 0,
            clip_x: 0,
            clip_y: 0,
            clip_width: 1,
            clip_height: 1,
            has_clip: 0,
            image_alpha: 255,
            blend_mode: 0,
            _pad2: 0,
            soft_x: 0,
            soft_y: 0,
            soft_width: 0,
            soft_height: 0,
            has_soft_mask: 0,
            soft_outside: 255,
            _pad3: 0,
            _pad4: 0,
        }
    }

    fn image(size: DeviceSize, image: &PreparedRgbImage) -> Self {
        let bounds = image.bounds;
        let matrix = image.device_to_image;
        Self {
            page_width: size.width,
            page_height: size.height,
            image_width: image.width,
            image_height: image.height,
            x0: bounds.x,
            y0: bounds.y,
            x1: bounds.x.saturating_add(bounds.width as i32),
            y1: bounds.y.saturating_add(bounds.height as i32),
            a: matrix.a as f32,
            b: matrix.b as f32,
            c: matrix.c as f32,
            d: matrix.d as f32,
            e: matrix.e as f32,
            f: matrix.f as f32,
            footprint_x: image.footprint[0] as f32,
            footprint_y: image.footprint[1] as f32,
            interpolation: u32::from(matches!(
                image.interpolation,
                pdf_page_ir::InterpolationMode::Bilinear
            )),
            background: 0,
            mask_width: image.opacity.as_ref().map_or(1, |mask| mask.width),
            mask_height: image.opacity.as_ref().map_or(1, |mask| mask.height),
            mask_footprint_x: image
                .opacity
                .as_ref()
                .map_or(1.0, |mask| mask.footprint[0] as f32),
            mask_footprint_y: image
                .opacity
                .as_ref()
                .map_or(1.0, |mask| mask.footprint[1] as f32),
            has_opacity: u32::from(image.opacity.is_some()),
            is_stencil: u32::from(image.stencil_rgb.is_some()),
            stencil_rgb: image.stencil_rgb.map_or(0, |rgb| {
                u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16)
            }),
            opacity_box_filter: image
                .opacity
                .as_ref()
                .map_or(0, |mask| u32::from(mask.box_filter)),
            clip_x: image.clip.as_ref().map_or(0, |clip| clip.bounds.x),
            clip_y: image.clip.as_ref().map_or(0, |clip| clip.bounds.y),
            clip_width: image.clip.as_ref().map_or(1, |clip| clip.bounds.width),
            clip_height: image.clip.as_ref().map_or(1, |clip| clip.bounds.height),
            has_clip: u32::from(image.clip.is_some()),
            image_alpha: u32::from(image.alpha),
            blend_mode: blend_mode_code(image.blend),
            _pad2: 0,
            soft_x: image.soft_mask.as_ref().map_or(0, |mask| mask.bounds.x),
            soft_y: image.soft_mask.as_ref().map_or(0, |mask| mask.bounds.y),
            soft_width: image.soft_mask.as_ref().map_or(0, |mask| mask.bounds.width),
            soft_height: image
                .soft_mask
                .as_ref()
                .map_or(0, |mask| mask.bounds.height),
            has_soft_mask: u32::from(image.soft_mask.is_some()),
            soft_outside: image
                .soft_mask
                .as_ref()
                .map_or(255, |mask| u32::from(mask.outside)),
            _pad3: 0,
            _pad4: 0,
        }
    }

    fn path(size: DeviceSize, path: &PreparedGpuPath, quality: RenderQuality) -> Self {
        let bounds = path.bounds;
        Self {
            page_width: size.width,
            page_height: size.height,
            image_width: path.edge_count,
            image_height: 1,
            x0: bounds.x,
            y0: bounds.y,
            x1: bounds.x.saturating_add(bounds.width as i32),
            y1: bounds.y.saturating_add(bounds.height as i32),
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
            footprint_x: 1.0,
            footprint_y: 1.0,
            interpolation: match quality {
                RenderQuality::Draft => 4,
                RenderQuality::Normal => 8,
            },
            background: 0,
            mask_width: 1,
            mask_height: 1,
            mask_footprint_x: 1.0,
            mask_footprint_y: 1.0,
            has_opacity: 0,
            is_stencil: 0,
            stencil_rgb: u32::from(path.rgb[0])
                | (u32::from(path.rgb[1]) << 8)
                | (u32::from(path.rgb[2]) << 16),
            opacity_box_filter: u32::from(path.even_odd),
            clip_x: path.clip.as_ref().map_or(0, |clip| clip.bounds.x),
            clip_y: path.clip.as_ref().map_or(0, |clip| clip.bounds.y),
            clip_width: path.clip.as_ref().map_or(1, |clip| clip.bounds.width),
            clip_height: path.clip.as_ref().map_or(1, |clip| clip.bounds.height),
            has_clip: u32::from(path.clip.is_some()),
            image_alpha: u32::from(path.alpha),
            blend_mode: blend_mode_code(path.blend),
            _pad2: 0,
            soft_x: path.soft_mask.as_ref().map_or(0, |mask| mask.bounds.x),
            soft_y: path.soft_mask.as_ref().map_or(0, |mask| mask.bounds.y),
            soft_width: path.soft_mask.as_ref().map_or(0, |mask| mask.bounds.width),
            soft_height: path.soft_mask.as_ref().map_or(0, |mask| mask.bounds.height),
            has_soft_mask: u32::from(path.soft_mask.is_some()),
            soft_outside: path
                .soft_mask
                .as_ref()
                .map_or(255, |mask| u32::from(mask.outside)),
            _pad3: 0,
            _pad4: 0,
        }
    }
}

const fn blend_mode_code(mode: BlendMode) -> u32 {
    match mode {
        BlendMode::Normal => 0,
        BlendMode::Multiply => 1,
        BlendMode::Screen => 2,
        BlendMode::Overlay => 3,
        BlendMode::Darken => 4,
        BlendMode::Lighten => 5,
        BlendMode::ColorDodge => 6,
        BlendMode::ColorBurn => 7,
        BlendMode::HardLight => 8,
        BlendMode::SoftLight => 9,
        BlendMode::Difference => 10,
        BlendMode::Exclusion => 11,
        BlendMode::Hue => 12,
        BlendMode::Saturation => 13,
        BlendMode::Color => 14,
        BlendMode::Luminosity => 15,
    }
}

struct PipelineSet {
    layout: wgpu::BindGroupLayout,
    path_batch_layout: wgpu::BindGroupLayout,
    clear: wgpu::ComputePipeline,
    paint: wgpu::ComputePipeline,
    paint_path: wgpu::ComputePipeline,
    paint_path_batch: wgpu::ComputePipeline,
    dummy_source: wgpu::Buffer,
}

impl PipelineSet {
    fn new(context: &SharedGpuContext) -> Result<Self, RenderError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Self::build(context.device())
        }))
        .map_err(|payload| {
            RenderError::GpuUnavailable(format!(
                "WGPU image pipeline initialization panicked: {}",
                pdf_render_api::panic_message(payload)
            ))
        })
    }

    fn build(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pdf-render-wgpu-image-layout"),
            entries: &[
                buffer_entry(0, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(2, wgpu::BufferBindingType::Storage { read_only: false }),
                buffer_entry(3, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(4, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(5, wgpu::BufferBindingType::Storage { read_only: true }),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pdf-render-wgpu-image-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pdf-render-wgpu-image-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });
        let make_pipeline = |label: &'static str, entry: &'static str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let path_batch_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pdf-render-wgpu-path-batch-layout"),
            entries: &[
                buffer_entry(0, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(2, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(3, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(4, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(5, wgpu::BufferBindingType::Storage { read_only: true }),
                buffer_entry(6, wgpu::BufferBindingType::Storage { read_only: false }),
            ],
        });
        let path_batch_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("pdf-render-wgpu-path-batch-pipeline-layout"),
                bind_group_layouts: &[Some(&path_batch_layout)],
                immediate_size: 0,
            });
        let path_batch_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pdf-render-wgpu-path-batch-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("path_batch.wgsl").into()),
        });
        let paint_path_batch = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pdf-render-wgpu-paint-path-batch"),
            layout: Some(&path_batch_pipeline_layout),
            module: &path_batch_shader,
            entry_point: Some("paint_path_batch"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let dummy_source = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pdf-render-wgpu-dummy-source"),
            contents: &[0, 0, 0, 0],
            usage: wgpu::BufferUsages::STORAGE,
        });
        Self {
            layout,
            path_batch_layout,
            clear: make_pipeline("pdf-render-wgpu-clear", "clear_page"),
            paint: make_pipeline("pdf-render-wgpu-paint-image", "paint_image"),
            paint_path: make_pipeline("pdf-render-wgpu-paint-path", "paint_path"),
            paint_path_batch,
            dummy_source,
        }
    }
}

fn buffer_entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// Timings and transfer counts for focused image-render benchmarks.
#[derive(Debug, Clone, Default)]
pub struct GpuRenderStats {
    pub prepare: Duration,
    pub gpu_and_readback: Duration,
    pub total: Duration,
    pub image_draws: u32,
    pub path_draws: u32,
    pub path_batches: u32,
    pub path_dispatches: u32,
    pub path_edges: u64,
    pub band_edge_references: u64,
    pub active_path_tiles: u64,
    pub tile_path_references: u64,
    pub max_tile_depth: u32,
    pub packed_path_bytes: u64,
    pub packed_mask_bytes: u64,
    pub cache_hits: u32,
    pub cache_misses: u32,
    pub uploaded_bytes: u64,
    pub reused_bytes: u64,
    pub readback_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ExecuteTransferStats {
    cache_hits: u32,
    cache_misses: u32,
    uploaded_bytes: u64,
    reused_bytes: u64,
    path_dispatches: u32,
}

/// Lifetime state of the decoded-image device-buffer cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuUploadCacheTelemetry {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub evictions: u64,
    pub entries: usize,
    pub resident_bytes: usize,
    pub budget_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UploadCacheKey {
    data_ptr: usize,
    data_len: usize,
    width: u32,
    height: u32,
}

impl UploadCacheKey {
    fn new(samples: &Arc<[u8]>, width: u32, height: u32) -> Self {
        Self {
            data_ptr: samples.as_ptr() as usize,
            data_len: samples.len(),
            width,
            height,
        }
    }
}

struct UploadCacheEntry {
    buffer: Arc<wgpu::Buffer>,
    // Keeping the immutable allocation alive makes the pointer identity in
    // UploadCacheKey collision-free for the lifetime of this entry.
    samples: Arc<[u8]>,
    charge: usize,
    last_used: u64,
}

#[derive(Default)]
struct UploadCacheState {
    entries: HashMap<UploadCacheKey, UploadCacheEntry>,
    resident_bytes: usize,
    clock: u64,
}

struct GpuUploadCache {
    state: Mutex<UploadCacheState>,
    budget_bytes: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
    evictions: AtomicU64,
}

impl GpuUploadCache {
    fn new(budget_bytes: usize) -> Self {
        Self {
            state: Mutex::new(UploadCacheState::default()),
            budget_bytes: budget_bytes.max(4),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    fn get(&self, samples: &Arc<[u8]>, width: u32, height: u32) -> Option<Arc<wgpu::Buffer>> {
        let key = UploadCacheKey::new(samples, width, height);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        let Some(entry) = state.entries.get_mut(&key) else {
            drop(state);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        // This is stronger than pointer equality alone and documents the
        // allocation-identity contract used instead of hashing image bytes.
        if !Arc::ptr_eq(&entry.samples, samples) {
            drop(state);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        entry.last_used = clock;
        let buffer = Arc::clone(&entry.buffer);
        drop(state);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(buffer)
    }

    fn insert(
        &self,
        samples: &Arc<[u8]>,
        width: u32,
        height: u32,
        buffer: Arc<wgpu::Buffer>,
        source_len: usize,
    ) {
        let key = UploadCacheKey::new(samples, width, height);
        let charge = source_len.next_multiple_of(4);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        if let Some(previous) = state.entries.insert(
            key,
            UploadCacheEntry {
                buffer,
                samples: Arc::clone(samples),
                charge,
                last_used: clock,
            },
        ) {
            state.resident_bytes = state.resident_bytes.saturating_sub(previous.charge);
        }
        state.resident_bytes = state.resident_bytes.saturating_add(charge);
        self.inserts.fetch_add(1, Ordering::Relaxed);

        // Retain one oversized image so a large scan can still be reused on an
        // immediate page revisit; it is displaced by the next distinct image.
        while state.resident_bytes > self.budget_bytes && state.entries.len() > 1 {
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
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.entries.clear();
        state.resident_bytes = 0;
    }

    fn telemetry(&self) -> GpuUploadCacheTelemetry {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        GpuUploadCacheTelemetry {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            inserts: self.inserts.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            entries: state.entries.len(),
            resident_bytes: state.resident_bytes,
            budget_bytes: self.budget_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PathUploadCacheKey {
    batch_ptr: usize,
}

struct UploadedPathBatch {
    descriptors: wgpu::Buffer,
    geometry: wgpu::Buffer,
    tiles: wgpu::Buffer,
    tile_path_indices: wgpu::Buffer,
    masks: wgpu::Buffer,
    tile_count: u32,
    charge: usize,
}

struct PathUploadCacheEntry {
    upload: Arc<UploadedPathBatch>,
    batch: Arc<PreparedGpuPathBatch>,
    charge: usize,
    last_used: u64,
}

#[derive(Default)]
struct PathUploadCacheState {
    entries: HashMap<PathUploadCacheKey, PathUploadCacheEntry>,
    resident_bytes: usize,
    clock: u64,
}

struct PathUploadCache {
    state: Mutex<PathUploadCacheState>,
    budget_bytes: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
    evictions: AtomicU64,
}

impl PathUploadCache {
    fn new(budget_bytes: usize) -> Self {
        Self {
            state: Mutex::new(PathUploadCacheState::default()),
            budget_bytes: budget_bytes.max(4),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inserts: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    fn key(batch: &Arc<PreparedGpuPathBatch>) -> PathUploadCacheKey {
        PathUploadCacheKey {
            batch_ptr: Arc::as_ptr(batch) as usize,
        }
    }

    fn get(&self, batch: &Arc<PreparedGpuPathBatch>) -> Option<Arc<UploadedPathBatch>> {
        let key = Self::key(batch);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        let Some(entry) = state.entries.get_mut(&key) else {
            drop(state);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        if !Arc::ptr_eq(&entry.batch, batch) {
            drop(state);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        entry.last_used = clock;
        let upload = Arc::clone(&entry.upload);
        drop(state);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(upload)
    }

    fn insert(&self, batch: &Arc<PreparedGpuPathBatch>, upload: Arc<UploadedPathBatch>) {
        let key = Self::key(batch);
        let charge = upload.charge;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        if let Some(previous) = state.entries.insert(
            key,
            PathUploadCacheEntry {
                upload,
                batch: Arc::clone(batch),
                charge,
                last_used: clock,
            },
        ) {
            state.resident_bytes = state.resident_bytes.saturating_sub(previous.charge);
        }
        state.resident_bytes = state.resident_bytes.saturating_add(charge);
        self.inserts.fetch_add(1, Ordering::Relaxed);
        while state.resident_bytes > self.budget_bytes && state.entries.len() > 1 {
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
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.entries.clear();
        state.resident_bytes = 0;
    }

    fn telemetry(&self) -> GpuUploadCacheTelemetry {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        GpuUploadCacheTelemetry {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            inserts: self.inserts.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            entries: state.entries.len(),
            resident_bytes: state.resident_bytes,
            budget_bytes: self.budget_bytes,
        }
    }
}

fn append_mask_plane(
    atlas: &mut Vec<u8>,
    offsets: &mut HashMap<(usize, usize), u32>,
    samples: &Arc<[u8]>,
) -> Result<u32, RenderError> {
    let key = (samples.as_ptr() as usize, samples.len());
    if let Some(&offset) = offsets.get(&key) {
        return Ok(offset);
    }
    let offset = u32::try_from(atlas.len())
        .map_err(|_| RenderError::LimitExceeded("GPU path mask atlas offset overflow"))?;
    atlas.extend_from_slice(samples);
    offsets.insert(key, offset);
    Ok(offset)
}

fn create_path_batch_upload(
    device: &wgpu::Device,
    batch: &PreparedGpuPathBatch,
    max_binding: usize,
) -> Result<UploadedPathBatch, RenderError> {
    let mut geometry = Vec::<u32>::with_capacity(batch.geometry_bytes / 4);
    let mut descriptors = Vec::<GpuPathDescriptor>::with_capacity(batch.paths.len());
    let mut masks = Vec::<u8>::with_capacity(batch.mask_bytes);
    let mut mask_offsets = HashMap::<(usize, usize), u32>::new();
    for path in batch.paths.iter() {
        let geometry_base = u32::try_from(geometry.len())
            .map_err(|_| RenderError::LimitExceeded("GPU path geometry offset overflow"))?;
        geometry.extend_from_slice(&path.raster_data);
        let clip_offset = if let Some(clip) = path.clip.as_ref() {
            append_mask_plane(&mut masks, &mut mask_offsets, &clip.samples)?
        } else {
            0
        };
        let soft_offset = if let Some(mask) = path.soft_mask.as_ref() {
            append_mask_plane(&mut masks, &mut mask_offsets, &mask.samples)?
        } else {
            0
        };
        descriptors.push(GpuPathDescriptor {
            x0: path.bounds.x,
            y0: path.bounds.y,
            x1: path.bounds.x.saturating_add(path.bounds.width as i32),
            y1: path.bounds.y.saturating_add(path.bounds.height as i32),
            geometry_base,
            rgb: u32::from(path.rgb[0])
                | (u32::from(path.rgb[1]) << 8)
                | (u32::from(path.rgb[2]) << 16),
            alpha: u32::from(path.alpha),
            blend_mode: blend_mode_code(path.blend),
            even_odd: u32::from(path.even_odd),
            clip_x: path.clip.as_ref().map_or(0, |clip| clip.bounds.x),
            clip_y: path.clip.as_ref().map_or(0, |clip| clip.bounds.y),
            clip_width: path.clip.as_ref().map_or(0, |clip| clip.bounds.width),
            clip_height: path.clip.as_ref().map_or(0, |clip| clip.bounds.height),
            clip_offset,
            has_clip: u32::from(path.clip.is_some()),
            soft_x: path.soft_mask.as_ref().map_or(0, |mask| mask.bounds.x),
            soft_y: path.soft_mask.as_ref().map_or(0, |mask| mask.bounds.y),
            soft_width: path.soft_mask.as_ref().map_or(0, |mask| mask.bounds.width),
            soft_height: path.soft_mask.as_ref().map_or(0, |mask| mask.bounds.height),
            soft_offset,
            soft_outside: path
                .soft_mask
                .as_ref()
                .map_or(255, |mask| u32::from(mask.outside)),
            has_soft_mask: u32::from(path.soft_mask.is_some()),
        });
    }
    let tiles: Vec<GpuPathTile> = batch
        .tiles
        .iter()
        .map(|tile| GpuPathTile {
            x: tile.x,
            y: tile.y,
            path_offset: tile.path_offset,
            path_count: tile.path_count,
        })
        .collect();
    let descriptor_bytes = bytemuck::cast_slice(descriptors.as_slice());
    let geometry_bytes = bytemuck::cast_slice(geometry.as_slice());
    let tile_bytes = bytemuck::cast_slice(tiles.as_slice());
    let reference_bytes = bytemuck::cast_slice(batch.tile_path_indices.as_ref());
    let padded_masks = padded_source(&masks);
    for (bytes, message) in [
        (
            descriptor_bytes.len(),
            "GPU path descriptor buffer exceeds device limit",
        ),
        (
            geometry_bytes.len(),
            "GPU path geometry buffer exceeds device limit",
        ),
        (
            tile_bytes.len(),
            "GPU path tile buffer exceeds device limit",
        ),
        (
            reference_bytes.len(),
            "GPU path tile-reference buffer exceeds device limit",
        ),
        (
            padded_masks.len(),
            "GPU path mask atlas exceeds device limit",
        ),
    ] {
        if bytes > max_binding {
            return Err(RenderError::LimitExceeded(message));
        }
    }
    let make_storage = |label, contents: &[u8]| {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: if contents.is_empty() {
                &[0, 0, 0, 0]
            } else {
                contents
            },
            usage: wgpu::BufferUsages::STORAGE,
        })
    };
    let charge = descriptor_bytes
        .len()
        .saturating_add(geometry_bytes.len())
        .saturating_add(tile_bytes.len())
        .saturating_add(reference_bytes.len())
        .saturating_add(padded_masks.len());
    Ok(UploadedPathBatch {
        descriptors: make_storage("pdf-render-wgpu-path-descriptors", descriptor_bytes),
        geometry: make_storage("pdf-render-wgpu-path-geometry", geometry_bytes),
        tiles: make_storage("pdf-render-wgpu-path-tiles", tile_bytes),
        tile_path_indices: make_storage("pdf-render-wgpu-path-tile-indices", reference_bytes),
        masks: make_storage("pdf-render-wgpu-path-mask-atlas", padded_masks.as_ref()),
        tile_count: u32::try_from(tiles.len())
            .map_err(|_| RenderError::LimitExceeded("GPU active path tile count overflow"))?,
        charge,
    })
}

/// Experimental decoded-image GPU backend.
pub struct WgpuBackend {
    context: SharedGpuContext,
    adapter: AdapterInfo,
    pipelines: Arc<PipelineSet>,
    preparer: Arc<CpuBackend>,
    upload_cache: Arc<GpuUploadCache>,
    path_upload_cache: Arc<PathUploadCache>,
    job_counter: AtomicU64,
    #[cfg(test)]
    test_fault: AtomicU8,
    #[cfg(test)]
    test_after_submit: Mutex<Option<Arc<TestAfterSubmitHook>>>,
}

#[cfg(test)]
struct TestAfterSubmitHook {
    submitted: std::sync::mpsc::SyncSender<()>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl std::fmt::Debug for WgpuBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgpuBackend")
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

impl WgpuBackend {
    /// Initialize on the same process-wide adapter/device used by `lege-gpu`.
    pub fn new() -> Result<Self, RenderError> {
        Self::with_cpu_options(CpuBackendOptions::default())
    }

    /// Initialize with the codec registry and decode policy used during shared
    /// image preparation.
    pub fn with_cpu_options(options: CpuBackendOptions) -> Result<Self, RenderError> {
        Self::with_preparer(Arc::new(CpuBackend::new(options)))
    }

    /// Initialize with an explicit device-buffer cache budget.
    ///
    /// The default is 128 MiB. Keeping this constructor public lets focused
    /// tests and memory-constrained embeddings exercise deterministic eviction.
    pub fn with_cpu_options_and_cache_budget_bytes(
        options: CpuBackendOptions,
        cache_budget_bytes: usize,
    ) -> Result<Self, RenderError> {
        Self::with_preparer_and_cache_budget(Arc::new(CpuBackend::new(options)), cache_budget_bytes)
    }

    fn with_preparer(preparer: Arc<CpuBackend>) -> Result<Self, RenderError> {
        Self::with_preparer_and_cache_budget(preparer, DEFAULT_UPLOAD_CACHE_BYTES)
    }

    fn with_preparer_and_cache_budget(
        preparer: Arc<CpuBackend>,
        cache_budget_bytes: usize,
    ) -> Result<Self, RenderError> {
        let context = SharedGpuContext::get()
            .map_err(|error| RenderError::GpuUnavailable(error.to_string()))?;
        let adapter = context.adapter_info();
        let pipelines = Arc::new(PipelineSet::new(&context)?);
        Ok(Self {
            context,
            adapter,
            pipelines,
            preparer,
            upload_cache: Arc::new(GpuUploadCache::new(cache_budget_bytes)),
            path_upload_cache: Arc::new(PathUploadCache::new(cache_budget_bytes / 2)),
            job_counter: AtomicU64::new(0),
            #[cfg(test)]
            test_fault: AtomicU8::new(0),
            #[cfg(test)]
            test_after_submit: Mutex::new(None),
        })
    }

    pub fn adapter_info(&self) -> &AdapterInfo {
        &self.adapter
    }

    pub fn is_hardware_gpu(&self) -> bool {
        self.adapter.is_hardware_gpu()
    }

    fn device_is_lost(&self) -> bool {
        self.context.is_lost()
    }

    fn device_loss_reason(&self) -> Option<Arc<str>> {
        self.context.device_loss_reason()
    }

    pub fn upload_cache_telemetry(&self) -> GpuUploadCacheTelemetry {
        self.upload_cache.telemetry()
    }

    pub fn path_upload_cache_telemetry(&self) -> GpuUploadCacheTelemetry {
        self.path_upload_cache.telemetry()
    }

    /// Drop all cached decoded-image device buffers.
    pub fn clear_upload_cache(&self) {
        self.upload_cache.clear();
        self.path_upload_cache.clear();
    }

    #[cfg(test)]
    fn inject_device_loss_once(&self) {
        self.test_fault.store(1, Ordering::Release);
    }

    #[cfg(test)]
    fn inject_prepared_render_panic_once(&self) {
        self.test_fault.store(2, Ordering::Release);
    }

    #[cfg(test)]
    fn install_after_submit_hook(
        &self,
        submitted: std::sync::mpsc::SyncSender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        *self
            .test_after_submit
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(TestAfterSubmitHook {
            submitted,
            resume: Mutex::new(resume),
        }));
    }

    /// Render while retaining stage measurements for the focused performance
    /// harness. Production callers normally use the [`RenderBackend`] trait.
    pub fn render_to_host_measured(
        &self,
        request: &RenderRequest,
    ) -> Result<(HostPage, GpuRenderStats), RenderError> {
        self.render_to_host_measured_inner(request)?
            .ok_or(RenderError::Unsupported(request.page.features))
    }

    fn render_to_host_measured_inner(
        &self,
        request: &RenderRequest,
    ) -> Result<Option<(HostPage, GpuRenderStats)>, RenderError> {
        let total_start = Instant::now();
        if !request_shape_supported_forced(&request.page, request) {
            return Err(RenderError::Unsupported(request.page.features));
        }
        if request
            .limits
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err(RenderError::Cancelled);
        }

        let prepare_start = Instant::now();
        let prepared = self.preparer.prepare_rgb_image_page(request)?;
        let Some(prepared) = prepared else {
            return Ok(None);
        };
        let prepare = prepare_start.elapsed();
        self.render_prepared_to_host_measured(request, prepared, prepare, total_start)
            .map(Some)
    }

    fn render_prepared_to_host_measured(
        &self,
        request: &RenderRequest,
        prepared: PreparedRgbImagePage,
        prepare: Duration,
        total_start: Instant,
    ) -> Result<(HostPage, GpuRenderStats), RenderError> {
        #[cfg(test)]
        if self
            .test_fault
            .compare_exchange(2, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            panic!("injected WGPU prepared-render panic");
        }
        validate_prepared(&prepared)?;

        let gpu_start = Instant::now();
        let (host, transfer) = self.execute(
            &prepared,
            request.background,
            request.quality,
            request.limits.cancellation.as_ref(),
        )?;
        let gpu_and_readback = gpu_start.elapsed();
        let readback_bytes = prepared.size.width as u64 * prepared.size.height as u64 * 4;
        Ok((
            host,
            GpuRenderStats {
                prepare,
                gpu_and_readback,
                total: total_start.elapsed(),
                image_draws: prepared.images.len() as u32,
                path_draws: prepared
                    .commands
                    .iter()
                    .map(|command| match command {
                        PreparedGpuCommand::Path(_) => 1,
                        PreparedGpuCommand::PathBatch(batch) => batch.paths.len() as u32,
                        PreparedGpuCommand::Image(_) => 0,
                    })
                    .sum(),
                path_batches: prepared
                    .commands
                    .iter()
                    .filter(|command| matches!(command, PreparedGpuCommand::PathBatch(_)))
                    .count() as u32,
                path_dispatches: transfer.path_dispatches,
                path_edges: prepared
                    .commands
                    .iter()
                    .map(|command| match command {
                        PreparedGpuCommand::Path(path) => u64::from(path.edge_count),
                        PreparedGpuCommand::PathBatch(batch) => batch
                            .paths
                            .iter()
                            .map(|path| u64::from(path.edge_count))
                            .sum(),
                        PreparedGpuCommand::Image(_) => 0,
                    })
                    .sum(),
                band_edge_references: prepared
                    .commands
                    .iter()
                    .map(|command| match command {
                        PreparedGpuCommand::Path(path) => u64::from(path.band_edge_references),
                        PreparedGpuCommand::PathBatch(batch) => batch
                            .paths
                            .iter()
                            .map(|path| u64::from(path.band_edge_references))
                            .sum(),
                        PreparedGpuCommand::Image(_) => 0,
                    })
                    .sum(),
                active_path_tiles: prepared
                    .commands
                    .iter()
                    .map(|command| match command {
                        PreparedGpuCommand::PathBatch(batch) => batch.tiles.len() as u64,
                        _ => 0,
                    })
                    .sum(),
                tile_path_references: prepared
                    .commands
                    .iter()
                    .map(|command| match command {
                        PreparedGpuCommand::PathBatch(batch) => {
                            batch.tile_path_indices.len() as u64
                        }
                        _ => 0,
                    })
                    .sum(),
                max_tile_depth: prepared
                    .commands
                    .iter()
                    .filter_map(|command| match command {
                        PreparedGpuCommand::PathBatch(batch) => Some(batch.max_tile_depth),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0),
                packed_path_bytes: prepared
                    .commands
                    .iter()
                    .map(|command| match command {
                        PreparedGpuCommand::PathBatch(batch) => batch.geometry_bytes as u64,
                        PreparedGpuCommand::Path(path) => {
                            (path.raster_data.len() * std::mem::size_of::<u32>()) as u64
                        }
                        _ => 0,
                    })
                    .sum(),
                packed_mask_bytes: prepared
                    .commands
                    .iter()
                    .map(|command| match command {
                        PreparedGpuCommand::PathBatch(batch) => batch.mask_bytes as u64,
                        _ => 0,
                    })
                    .sum(),
                cache_hits: transfer.cache_hits,
                cache_misses: transfer.cache_misses,
                uploaded_bytes: transfer.uploaded_bytes,
                reused_bytes: transfer.reused_bytes,
                readback_bytes,
            },
        ))
    }

    fn execute(
        &self,
        prepared: &PreparedRgbImagePage,
        background: Background,
        quality: RenderQuality,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(HostPage, ExecuteTransferStats), RenderError> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.execute_inner(prepared, background, quality, cancellation)
        }));
        match result {
            Ok(result) => result,
            Err(payload) => Err(RenderError::Backend(format!(
                "WGPU image execution panicked: {}",
                pdf_render_api::panic_message(payload)
            ))),
        }
    }

    fn execute_inner(
        &self,
        prepared: &PreparedRgbImagePage,
        background: Background,
        quality: RenderQuality,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(HostPage, ExecuteTransferStats), RenderError> {
        #[cfg(test)]
        if self.test_fault.swap(0, Ordering::AcqRel) == 1 {
            return Err(RenderError::GpuUnavailable(
                "injected WGPU device loss".to_owned(),
            ));
        }
        check_cancelled(cancellation)?;
        let device = self.context.device();
        let queue = self.context.queue();
        let pixel_count = checked_pixels(prepared.size)?;
        let page_bytes = pixel_count
            .checked_mul(4)
            .ok_or(RenderError::LimitExceeded("GPU page byte size overflow"))?;
        let max_binding = device.limits().max_storage_buffer_binding_size as usize;
        if device.limits().max_storage_buffers_per_shader_stage < 6 {
            return Err(RenderError::GpuUnavailable(
                "GPU exposes fewer than six storage buffers per shader stage".to_owned(),
            ));
        }
        if page_bytes > max_binding {
            return Err(RenderError::LimitExceeded(
                "GPU page exceeds max storage buffer binding size",
            ));
        }

        let page_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pdf-render-wgpu-page"),
            size: page_bytes.max(4) as u64,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pdf-render-wgpu-readback"),
            size: page_bytes.max(4) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pdf-render-wgpu-image-encoder"),
        });

        let clear_params = ShaderParams::clear(prepared.size, background_rgba(background));
        encode_dispatch(
            device,
            &mut encoder,
            &self.pipelines,
            &self.pipelines.clear,
            &clear_params,
            &self.pipelines.dummy_source,
            &page_buffer,
            &self.pipelines.dummy_source,
            &self.pipelines.dummy_source,
            &self.pipelines.dummy_source,
            prepared.size.width.div_ceil(WORKGROUP_EDGE),
            prepared.size.height.div_ceil(WORKGROUP_EDGE),
        );

        let mut transfer = ExecuteTransferStats::default();
        for command in &prepared.commands {
            check_cancelled(cancellation)?;
            let image = match command {
                PreparedGpuCommand::Image(index) => prepared
                    .images
                    .get(*index as usize)
                    .ok_or(RenderError::Unsupported(PageFeatures::IMAGES))?,
                PreparedGpuCommand::PathBatch(batch) => {
                    if batch.paths.is_empty() || batch.tiles.is_empty() {
                        continue;
                    }
                    let upload = if let Some(upload) = self.path_upload_cache.get(batch) {
                        transfer.cache_hits += 1;
                        transfer.reused_bytes += upload.charge as u64;
                        upload
                    } else {
                        let upload = Arc::new(create_path_batch_upload(
                            device,
                            batch.as_ref(),
                            max_binding,
                        )?);
                        transfer.cache_misses += 1;
                        transfer.uploaded_bytes += upload.charge as u64;
                        self.path_upload_cache.insert(batch, Arc::clone(&upload));
                        upload
                    };
                    encode_path_batch_dispatch(
                        device,
                        &mut encoder,
                        &self.pipelines,
                        prepared.size,
                        quality,
                        upload.as_ref(),
                        &page_buffer,
                    );
                    transfer.path_dispatches += 1;
                    continue;
                }
                PreparedGpuCommand::Path(path) => {
                    if path.bounds.width == 0
                        || path.bounds.height == 0
                        || path.raster_data.is_empty()
                    {
                        continue;
                    }
                    let edge_bytes = bytemuck::cast_slice(path.raster_data.as_ref());
                    if edge_bytes.len() > max_binding {
                        return Err(RenderError::LimitExceeded(
                            "GPU path edge buffer exceeds max storage binding size",
                        ));
                    }
                    let edge_buffer =
                        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: Some("pdf-render-wgpu-path-edges"),
                            contents: edge_bytes,
                            usage: wgpu::BufferUsages::STORAGE,
                        });
                    transfer.cache_misses += 1;
                    transfer.uploaded_bytes += edge_bytes.len() as u64;

                    let clip_buffer = path.clip.as_ref().map(|clip| {
                        let clip_len = clip.samples.len();
                        if let Some(buffer) = self.upload_cache.get(
                            &clip.samples,
                            clip.bounds.width,
                            clip.bounds.height,
                        ) {
                            transfer.cache_hits += 1;
                            transfer.reused_bytes += clip_len as u64;
                            buffer
                        } else {
                            let source = padded_source(&clip.samples);
                            let buffer = Arc::new(device.create_buffer_init(
                                &wgpu::util::BufferInitDescriptor {
                                    label: Some("pdf-render-wgpu-path-clip"),
                                    contents: source.as_ref(),
                                    usage: wgpu::BufferUsages::STORAGE,
                                },
                            ));
                            self.upload_cache.insert(
                                &clip.samples,
                                clip.bounds.width,
                                clip.bounds.height,
                                Arc::clone(&buffer),
                                clip_len,
                            );
                            transfer.cache_misses += 1;
                            transfer.uploaded_bytes += clip_len as u64;
                            buffer
                        }
                    });
                    let soft_mask_buffer = path.soft_mask.as_ref().and_then(|mask| {
                        if mask.samples.is_empty() {
                            return None;
                        }
                        let mask_len = mask.samples.len();
                        Some(
                            if let Some(buffer) = self.upload_cache.get(
                                &mask.samples,
                                mask.bounds.width,
                                mask.bounds.height,
                            ) {
                                transfer.cache_hits += 1;
                                transfer.reused_bytes += mask_len as u64;
                                buffer
                            } else {
                                let source = padded_source(&mask.samples);
                                let buffer = Arc::new(device.create_buffer_init(
                                    &wgpu::util::BufferInitDescriptor {
                                        label: Some("pdf-render-wgpu-path-soft-mask"),
                                        contents: source.as_ref(),
                                        usage: wgpu::BufferUsages::STORAGE,
                                    },
                                ));
                                self.upload_cache.insert(
                                    &mask.samples,
                                    mask.bounds.width,
                                    mask.bounds.height,
                                    Arc::clone(&buffer),
                                    mask_len,
                                );
                                transfer.cache_misses += 1;
                                transfer.uploaded_bytes += mask_len as u64;
                                buffer
                            },
                        )
                    });
                    let params = ShaderParams::path(prepared.size, path, quality);
                    encode_dispatch(
                        device,
                        &mut encoder,
                        &self.pipelines,
                        &self.pipelines.paint_path,
                        &params,
                        &edge_buffer,
                        &page_buffer,
                        &self.pipelines.dummy_source,
                        clip_buffer
                            .as_deref()
                            .unwrap_or(&self.pipelines.dummy_source),
                        soft_mask_buffer
                            .as_deref()
                            .unwrap_or(&self.pipelines.dummy_source),
                        path.bounds.width.div_ceil(PATH_WORKGROUP_EDGE),
                        path.bounds.height.div_ceil(PATH_WORKGROUP_EDGE),
                    );
                    transfer.path_dispatches += 1;
                    continue;
                }
            };
            if image.bounds.width == 0 || image.bounds.height == 0 {
                continue;
            }
            let source_len = if image.stencil_rgb.is_some() {
                0
            } else {
                image.width as usize * image.height as usize * 3
            };
            if source_len > max_binding {
                return Err(RenderError::LimitExceeded(
                    "GPU image exceeds max storage buffer binding size",
                ));
            }
            let source_buffer = if source_len == 0 {
                None
            } else {
                Some(
                    if let Some(buffer) =
                        self.upload_cache
                            .get(&image.samples, image.width, image.height)
                    {
                        transfer.cache_hits += 1;
                        transfer.reused_bytes += source_len as u64;
                        buffer
                    } else {
                        let source = padded_source(&image.samples[..source_len]);
                        let buffer = Arc::new(device.create_buffer_init(
                            &wgpu::util::BufferInitDescriptor {
                                label: Some("pdf-render-wgpu-image-source"),
                                contents: source.as_ref(),
                                usage: wgpu::BufferUsages::STORAGE,
                            },
                        ));
                        self.upload_cache.insert(
                            &image.samples,
                            image.width,
                            image.height,
                            Arc::clone(&buffer),
                            source_len,
                        );
                        transfer.cache_misses += 1;
                        transfer.uploaded_bytes += source_len as u64;
                        buffer
                    },
                )
            };
            let opacity_buffer = if let Some(opacity) = image.opacity.as_ref() {
                let opacity_len = opacity.width as usize * opacity.height as usize;
                if opacity_len > max_binding {
                    return Err(RenderError::LimitExceeded(
                        "GPU image opacity exceeds max storage buffer binding size",
                    ));
                }
                Some(
                    if let Some(buffer) =
                        self.upload_cache
                            .get(&opacity.samples, opacity.width, opacity.height)
                    {
                        transfer.cache_hits += 1;
                        transfer.reused_bytes += opacity_len as u64;
                        buffer
                    } else {
                        let source = padded_source(&opacity.samples[..opacity_len]);
                        let buffer = Arc::new(device.create_buffer_init(
                            &wgpu::util::BufferInitDescriptor {
                                label: Some("pdf-render-wgpu-image-opacity"),
                                contents: source.as_ref(),
                                usage: wgpu::BufferUsages::STORAGE,
                            },
                        ));
                        self.upload_cache.insert(
                            &opacity.samples,
                            opacity.width,
                            opacity.height,
                            Arc::clone(&buffer),
                            opacity_len,
                        );
                        transfer.cache_misses += 1;
                        transfer.uploaded_bytes += opacity_len as u64;
                        buffer
                    },
                )
            } else {
                None
            };
            let clip_buffer = if let Some(clip) = image.clip.as_ref() {
                let clip_len = clip.bounds.width as usize * clip.bounds.height as usize;
                if clip_len > max_binding {
                    return Err(RenderError::LimitExceeded(
                        "GPU image clip exceeds max storage buffer binding size",
                    ));
                }
                Some(
                    if let Some(buffer) =
                        self.upload_cache
                            .get(&clip.samples, clip.bounds.width, clip.bounds.height)
                    {
                        transfer.cache_hits += 1;
                        transfer.reused_bytes += clip_len as u64;
                        buffer
                    } else {
                        let source = padded_source(&clip.samples[..clip_len]);
                        let buffer = Arc::new(device.create_buffer_init(
                            &wgpu::util::BufferInitDescriptor {
                                label: Some("pdf-render-wgpu-image-clip"),
                                contents: source.as_ref(),
                                usage: wgpu::BufferUsages::STORAGE,
                            },
                        ));
                        self.upload_cache.insert(
                            &clip.samples,
                            clip.bounds.width,
                            clip.bounds.height,
                            Arc::clone(&buffer),
                            clip_len,
                        );
                        transfer.cache_misses += 1;
                        transfer.uploaded_bytes += clip_len as u64;
                        buffer
                    },
                )
            } else {
                None
            };
            let soft_mask_buffer = if let Some(mask) = image.soft_mask.as_ref() {
                let mask_len = mask.bounds.width as usize * mask.bounds.height as usize;
                if mask_len > max_binding {
                    return Err(RenderError::LimitExceeded(
                        "GPU page soft mask exceeds max storage buffer binding size",
                    ));
                }
                if mask_len == 0 {
                    None
                } else {
                    Some(
                        if let Some(buffer) = self.upload_cache.get(
                            &mask.samples,
                            mask.bounds.width,
                            mask.bounds.height,
                        ) {
                            transfer.cache_hits += 1;
                            transfer.reused_bytes += mask_len as u64;
                            buffer
                        } else {
                            let source = padded_source(&mask.samples[..mask_len]);
                            let buffer = Arc::new(device.create_buffer_init(
                                &wgpu::util::BufferInitDescriptor {
                                    label: Some("pdf-render-wgpu-page-soft-mask"),
                                    contents: source.as_ref(),
                                    usage: wgpu::BufferUsages::STORAGE,
                                },
                            ));
                            self.upload_cache.insert(
                                &mask.samples,
                                mask.bounds.width,
                                mask.bounds.height,
                                Arc::clone(&buffer),
                                mask_len,
                            );
                            transfer.cache_misses += 1;
                            transfer.uploaded_bytes += mask_len as u64;
                            buffer
                        },
                    )
                }
            } else {
                None
            };
            let params = ShaderParams::image(prepared.size, image);
            encode_dispatch(
                device,
                &mut encoder,
                &self.pipelines,
                &self.pipelines.paint,
                &params,
                source_buffer
                    .as_deref()
                    .unwrap_or(&self.pipelines.dummy_source),
                &page_buffer,
                opacity_buffer
                    .as_deref()
                    .unwrap_or(&self.pipelines.dummy_source),
                clip_buffer
                    .as_deref()
                    .unwrap_or(&self.pipelines.dummy_source),
                soft_mask_buffer
                    .as_deref()
                    .unwrap_or(&self.pipelines.dummy_source),
                image.bounds.width.div_ceil(WORKGROUP_EDGE),
                image.bounds.height.div_ceil(WORKGROUP_EDGE),
            );
        }

        check_cancelled(cancellation)?;
        encoder.copy_buffer_to_buffer(&page_buffer, 0, &readback, 0, page_bytes as u64);
        let submission = queue.submit(std::iter::once(encoder.finish()));
        let slice = readback.slice(..page_bytes as u64);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        #[cfg(test)]
        if let Some(hook) = self
            .test_after_submit
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = hook.submitted.send(());
            let _ = hook
                .resume
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv();
        }
        if cancellation.is_some() {
            wait_for_mapping(device, &receiver, cancellation)?;
        } else {
            self.context
                .wait_for_submission(submission)
                .map_err(|error| RenderError::Readback(format!("GPU poll failed: {error}")))?;
            receiver
                .recv()
                .map_err(|_| RenderError::Readback("GPU map callback disconnected".to_owned()))?
                .map_err(|error| {
                    RenderError::Readback(format!("GPU readback mapping failed: {error}"))
                })?;
        }
        let mapped = slice.get_mapped_range().map_err(|error| {
            RenderError::Readback(format!("GPU readback range failed: {error}"))
        })?;
        let pixels: Arc<[u8]> = Arc::from(&mapped[..page_bytes]);
        drop(mapped);
        readback.unmap();
        Ok((
            HostPage {
                width: prepared.size.width,
                height: prepared.size.height,
                stride: prepared.size.width as usize * 4,
                format: OutputFormat::Rgba8PremultipliedSrgb,
                pixels,
            },
            transfer,
        ))
    }
}

fn check_cancelled(cancellation: Option<&CancellationToken>) -> Result<(), RenderError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        Err(RenderError::Cancelled)
    } else {
        Ok(())
    }
}

fn wait_for_mapping(
    device: &wgpu::Device,
    receiver: &std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    cancellation: Option<&CancellationToken>,
) -> Result<(), RenderError> {
    loop {
        check_cancelled(cancellation)?;
        device
            .poll(wgpu::PollType::Poll)
            .map_err(|error| RenderError::Readback(format!("GPU poll failed: {error:?}")))?;
        match receiver.recv_timeout(Duration::from_millis(1)) {
            Ok(result) => {
                return result.map_err(|error| {
                    RenderError::Readback(format!("GPU readback mapping failed: {error}"))
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RenderError::Readback(
                    "GPU map callback disconnected".to_owned(),
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn checked_pixels(size: DeviceSize) -> Result<usize, RenderError> {
    (size.width as usize)
        .checked_mul(size.height as usize)
        .filter(|pixels| *pixels > 0)
        .ok_or(RenderError::LimitExceeded(
            "zero or overflowing GPU output dimensions",
        ))
}

fn padded_source(samples: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if samples.len().is_multiple_of(4) {
        std::borrow::Cow::Borrowed(samples)
    } else {
        let mut padded = Vec::with_capacity(samples.len().next_multiple_of(4));
        padded.extend_from_slice(samples);
        padded.resize(samples.len().next_multiple_of(4), 0);
        std::borrow::Cow::Owned(padded)
    }
}

fn validate_prepared(page: &PreparedRgbImagePage) -> Result<(), RenderError> {
    checked_pixels(page.size)?;
    for image in &page.images {
        if !image.footprint[0].is_finite()
            || !image.footprint[1].is_finite()
            || image.footprint[0] > MAX_BOX_FOOTPRINT
            || image.footprint[1] > MAX_BOX_FOOTPRINT
        {
            return Err(RenderError::Unsupported(PageFeatures::IMAGES));
        }
        if image.stencil_rgb.is_none() {
            let rgb_len = (image.width as usize)
                .checked_mul(image.height as usize)
                .and_then(|pixels| pixels.checked_mul(3))
                .ok_or(RenderError::LimitExceeded("GPU image byte size overflow"))?;
            if image.samples.len() < rgb_len {
                return Err(RenderError::Unsupported(PageFeatures::IMAGES));
            }
        }
        if image.stencil_rgb.is_some() && image.opacity.is_none() {
            return Err(RenderError::Unsupported(PageFeatures::STENCIL_MASKS));
        }
        if let Some(opacity) = image.opacity.as_ref() {
            if !opacity.footprint[0].is_finite()
                || !opacity.footprint[1].is_finite()
                || opacity.footprint[0] > MAX_BOX_FOOTPRINT
                || opacity.footprint[1] > MAX_BOX_FOOTPRINT
            {
                return Err(RenderError::Unsupported(PageFeatures::IMAGES));
            }
            let opacity_len = (opacity.width as usize)
                .checked_mul(opacity.height as usize)
                .ok_or(RenderError::LimitExceeded(
                    "GPU image opacity byte size overflow",
                ))?;
            if opacity.samples.len() < opacity_len {
                return Err(RenderError::Unsupported(PageFeatures::IMAGES));
            }
        }
        if let Some(clip) = image.clip.as_ref() {
            let clip_len = (clip.bounds.width as usize)
                .checked_mul(clip.bounds.height as usize)
                .ok_or(RenderError::LimitExceeded(
                    "GPU image clip byte size overflow",
                ))?;
            if clip_len == 0 || clip.samples.len() < clip_len {
                return Err(RenderError::Unsupported(PageFeatures::CLIPPING));
            }
        }
    }
    for command in &page.commands {
        match command {
            PreparedGpuCommand::Image(index) => {
                if page.images.get(*index as usize).is_none() {
                    return Err(RenderError::Unsupported(PageFeatures::IMAGES));
                }
            }
            PreparedGpuCommand::Path(path) => {
                if path.edge_count == 0 || path.raster_data.len() < 4 {
                    return Err(RenderError::Unsupported(PageFeatures::BASIC_PATHS));
                }
                let edge_bytes = path
                    .raster_data
                    .len()
                    .checked_mul(std::mem::size_of::<u32>())
                    .ok_or(RenderError::LimitExceeded("GPU path edge byte overflow"))?;
                if edge_bytes == 0 {
                    return Err(RenderError::Unsupported(PageFeatures::BASIC_PATHS));
                }
            }
            PreparedGpuCommand::PathBatch(batch) => {
                if batch.paths.is_empty()
                    || batch.tiles.is_empty()
                    || batch.tile_path_indices.is_empty()
                    || batch.max_tile_depth == 0
                    || batch.max_tile_depth > 64
                {
                    return Err(RenderError::Unsupported(PageFeatures::BASIC_PATHS));
                }
                for tile in batch.tiles.iter() {
                    let end = usize::try_from(tile.path_offset)
                        .ok()
                        .and_then(|offset| {
                            offset.checked_add(usize::try_from(tile.path_count).ok()?)
                        })
                        .ok_or(RenderError::LimitExceeded(
                            "GPU path tile reference overflow",
                        ))?;
                    if end > batch.tile_path_indices.len()
                        || batch.tile_path_indices
                            [usize::try_from(tile.path_offset).unwrap_or(usize::MAX)..end]
                            .iter()
                            .any(|&index| index as usize >= batch.paths.len())
                    {
                        return Err(RenderError::Unsupported(PageFeatures::BASIC_PATHS));
                    }
                }
                for path in batch.paths.iter() {
                    if path.edge_count == 0 || path.raster_data.len() < 4 {
                        return Err(RenderError::Unsupported(PageFeatures::BASIC_PATHS));
                    }
                }
            }
        }
    }
    Ok(())
}

/// A stable reason why a request cannot use the experimental image renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GpuIneligibility {
    OutputFormat,
    Crop,
    ColorPolicy,
    NoImageDraw,
    MissingImageResource,
    ImageBlend,
    ImageAlpha,
    ImageStencil,
    ImageBitsPerComponent,
    ImageColorSpace,
    ImageDecodeArray,
    ImageRgbConversionBudget,
    ImageOpacityConversionBudget,
    ImageSoftMask,
    ImageHardMask,
    ImageInDataAlpha,
    VisibleText,
    Clip,
    Path,
    TransparencyGroup,
    SoftMaskState,
    Shading,
}

impl GpuIneligibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OutputFormat => "output-format",
            Self::Crop => "crop",
            Self::ColorPolicy => "color-policy",
            Self::NoImageDraw => "no-image-draw",
            Self::MissingImageResource => "missing-image-resource",
            Self::ImageBlend => "image-blend",
            Self::ImageAlpha => "image-alpha",
            Self::ImageStencil => "image-stencil",
            Self::ImageBitsPerComponent => "image-bpc",
            Self::ImageColorSpace => "image-color-space",
            Self::ImageDecodeArray => "image-decode-array",
            Self::ImageRgbConversionBudget => "image-rgb-conversion-over-64m",
            Self::ImageOpacityConversionBudget => "image-opacity-conversion-over-64m",
            Self::ImageSoftMask => "image-soft-mask",
            Self::ImageHardMask => "image-hard-mask",
            Self::ImageInDataAlpha => "image-in-data-alpha",
            Self::VisibleText => "visible-text",
            Self::Clip => "clip",
            Self::Path => "path",
            Self::TransparencyGroup => "transparency-group",
            Self::SoftMaskState => "soft-mask-state",
            Self::Shading => "shading",
        }
    }
}

/// Static eligibility report used by routing and corpus coverage tooling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuEligibilityReport {
    pub image_draws: u32,
    pub ignored_text_draws: u32,
    pub reasons: Vec<GpuIneligibility>,
}

impl GpuEligibilityReport {
    pub fn is_eligible(&self) -> bool {
        self.reasons.is_empty()
    }
}

/// Classify a compiled request without decoding images or initializing WGPU.
///
/// The production preflight delegates to this function, so corpus census
/// results cannot silently diverge from actual routing policy.
pub fn classify_gpu_eligibility(
    page: &CompiledPage,
    request: &RenderRequest,
) -> GpuEligibilityReport {
    let mut reasons = BTreeSet::new();
    let mut image_draws = 0u32;
    let mut ignored_text_draws = 0u32;
    let mut soft_mask_content_depth = 0u32;
    if request.output_format != OutputFormat::Rgba8PremultipliedSrgb {
        reasons.insert(GpuIneligibility::OutputFormat);
    }
    if request.crop.is_some() {
        reasons.insert(GpuIneligibility::Crop);
    }
    if request.color_policy != RenderColorPolicy::Original {
        reasons.insert(GpuIneligibility::ColorPolicy);
    }

    for op in page.operations.iter() {
        // Mask-group content is rendered by the normative CPU executor during
        // preparation, then handed to WGPU as one device-space alpha plane.
        // Its internal paths, text, groups, and images therefore do not count
        // as page-paint eligibility blockers or page image draws.
        match op {
            DisplayOp::BeginSoftMask { .. } => {
                soft_mask_content_depth += 1;
                continue;
            }
            DisplayOp::EndSoftMask if soft_mask_content_depth > 0 => {
                soft_mask_content_depth -= 1;
                continue;
            }
            _ if soft_mask_content_depth > 0 => continue,
            _ => {}
        }
        match op {
            DisplayOp::Save
            | DisplayOp::Restore
            | DisplayOp::ConcatTransform(_)
            | DisplayOp::BeginPaintOrigin(_)
            | DisplayOp::EndPaintOrigin => {}
            DisplayOp::DrawImage {
                image,
                paint,
                alpha: _,
                blend: _,
                ..
            } => {
                image_draws += 1;
                let Some(resource) = page.images.get(image.index()) else {
                    reasons.insert(GpuIneligibility::MissingImageResource);
                    continue;
                };
                if resource.is_stencil {
                    let supported_brush =
                        page.paints
                            .get(paint.index())
                            .is_some_and(|paint| match paint {
                                Paint::Solid(_) => true,
                                Paint::Pattern { tiling, .. } => {
                                    page.tilings.get(tiling.index()).is_some()
                                }
                                Paint::Shading { .. } => false,
                            });
                    if !supported_brush {
                        reasons.insert(GpuIneligibility::ImageStencil);
                    }
                }
                if !(1..=16).contains(&resource.bits_per_component) {
                    reasons.insert(GpuIneligibility::ImageBitsPerComponent);
                }
                let direct_rgb8 = resource.bits_per_component == 8
                    && resource.decode.is_none()
                    && matches!(resource.color_space, ImageColorSpace::Rgb);
                let converted_rgb_bytes = (resource.width as usize)
                    .checked_mul(resource.height as usize)
                    .and_then(|pixels| pixels.checked_mul(3));
                if !resource.is_stencil
                    && !direct_rgb8
                    && converted_rgb_bytes
                        .is_none_or(|bytes| bytes > MAX_PREPARED_RGB_CONVERSION_BYTES)
                {
                    reasons.insert(GpuIneligibility::ImageRgbConversionBudget);
                }
                if resource.soft_mask.is_some() {
                    reasons.insert(GpuIneligibility::ImageSoftMask);
                }
                let opacity_pixels = if resource.is_stencil || resource.smask_in_data != 0 {
                    (resource.width as usize).checked_mul(resource.height as usize)
                } else if let Some(mask) = resource.smask.as_ref() {
                    (mask.width as usize).checked_mul(mask.height as usize)
                } else if let Some(mask) = resource.mask.as_ref() {
                    match mask {
                        ImageMask::ColorKey(_) => {
                            (resource.width as usize).checked_mul(resource.height as usize)
                        }
                        ImageMask::Stencil(mask) => {
                            (mask.width as usize).checked_mul(mask.height as usize)
                        }
                    }
                } else {
                    None
                };
                if opacity_pixels.is_some_and(|bytes| bytes > MAX_PREPARED_OPACITY_CONVERSION_BYTES)
                {
                    reasons.insert(GpuIneligibility::ImageOpacityConversionBudget);
                }
            }
            DisplayOp::DrawGlyphRun {
                run,
                paint,
                alpha,
                stroke,
                ..
            } => {
                let Some(glyph_run) = page.glyph_runs.get(run.index()) else {
                    reasons.insert(GpuIneligibility::VisibleText);
                    continue;
                };
                if page.fonts.get(glyph_run.font.index()).is_none() {
                    reasons.insert(GpuIneligibility::VisibleText);
                    continue;
                }
                let mode = glyph_run.render_mode;
                let paints_fill = matches!(mode, 0 | 2 | 4 | 6) && *alpha > 0.0;
                let paints_stroke = matches!(mode, 1 | 2 | 5 | 6)
                    && stroke.as_ref().is_some_and(|value| value.alpha > 0.0);
                let non_painting = !paints_fill && !paints_stroke;
                if non_painting {
                    ignored_text_draws += 1;
                } else {
                    if paints_fill
                        && !page
                            .paints
                            .get(paint.index())
                            .is_some_and(|paint| matches!(paint, Paint::Solid(_)))
                    {
                        reasons.insert(GpuIneligibility::VisibleText);
                    }
                    if paints_stroke
                        && !stroke.as_ref().is_some_and(|stroke| {
                            page.paints
                                .get(stroke.paint.index())
                                .is_some_and(|paint| matches!(paint, Paint::Solid(_)))
                                && page.stroke_styles.get(stroke.style.index()).is_some()
                        })
                    {
                        reasons.insert(GpuIneligibility::VisibleText);
                    }
                }
            }
            DisplayOp::PushClip { path, .. } => {
                if page.paths.get(path.index()).is_none() {
                    reasons.insert(GpuIneligibility::Clip);
                }
            }
            DisplayOp::PushClipText { runs } => {
                if runs
                    .iter()
                    .any(|run| page.glyph_runs.get(run.index()).is_none())
                {
                    reasons.insert(GpuIneligibility::Clip);
                }
            }
            DisplayOp::PopClip => {}
            DisplayOp::FillPath { path, paint, .. } => {
                if page.paths.get(path.index()).is_none() {
                    reasons.insert(GpuIneligibility::Path);
                }
                match page.paints.get(paint.index()) {
                    Some(Paint::Solid(_)) => {}
                    Some(Paint::Shading { .. }) => {
                        reasons.insert(GpuIneligibility::Shading);
                    }
                    Some(Paint::Pattern { .. }) | None => {
                        reasons.insert(GpuIneligibility::Path);
                    }
                }
            }
            DisplayOp::StrokePath {
                path, paint, style, ..
            } => {
                if page.paths.get(path.index()).is_none()
                    || page.stroke_styles.get(style.index()).is_none()
                {
                    reasons.insert(GpuIneligibility::Path);
                }
                match page.paints.get(paint.index()) {
                    Some(Paint::Solid(_)) => {}
                    Some(Paint::Shading { .. }) => {
                        reasons.insert(GpuIneligibility::Shading);
                    }
                    Some(Paint::Pattern { .. }) | None => {
                        reasons.insert(GpuIneligibility::Path);
                    }
                }
            }
            DisplayOp::BeginTransparencyGroup { .. } | DisplayOp::EndTransparencyGroup => {
                reasons.insert(GpuIneligibility::TransparencyGroup);
            }
            // `ApplySoftMask` is the deprecated no-op retained by the IR, and
            // `/SMask /None` plus the balanced state pops are represented by
            // prepared stack commands understood by the image-page seam.
            DisplayOp::ApplySoftMask { .. }
            | DisplayOp::BeginSoftMask { .. }
            | DisplayOp::EndSoftMask
            | DisplayOp::ClearSoftMask => {}
            DisplayOp::DrawShading { .. } => {
                reasons.insert(GpuIneligibility::Shading);
            }
        }
    }
    if image_draws == 0 {
        reasons.insert(GpuIneligibility::NoImageDraw);
    }
    GpuEligibilityReport {
        image_draws,
        ignored_text_draws,
        reasons: reasons.into_iter().collect(),
    }
}

fn request_shape_supported(page: &CompiledPage, request: &RenderRequest) -> bool {
    classify_gpu_eligibility(page, request).is_eligible()
}

/// Forced-GPU mode may exercise the native vector vocabulary without an image
/// draw. Automatic routing retains the image requirement until mixed-content
/// performance crosses its measured gate.
fn request_shape_supported_forced(page: &CompiledPage, request: &RenderRequest) -> bool {
    classify_gpu_eligibility(page, request)
        .reasons
        .iter()
        .all(|reason| *reason == GpuIneligibility::NoImageDraw)
}

fn background_rgba(background: Background) -> u32 {
    let [r, g, b, a] = match background {
        Background::White => [255, 255, 255, 255],
        Background::Transparent => [0, 0, 0, 0],
        Background::Solid(color) => {
            let alpha = to_u8(color.a);
            [
                premultiply(to_u8(color.r), alpha),
                premultiply(to_u8(color.g), alpha),
                premultiply(to_u8(color.b), alpha),
                alpha,
            ]
        }
    };
    u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16) | (u32::from(a) << 24)
}

fn to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn premultiply(value: u8, alpha: u8) -> u8 {
    ((u16::from(value) * u16::from(alpha) + 127) / 255) as u8
}

#[allow(clippy::too_many_arguments)]
fn encode_dispatch(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &PipelineSet,
    pipeline: &wgpu::ComputePipeline,
    params: &ShaderParams,
    source: &wgpu::Buffer,
    destination: &wgpu::Buffer,
    opacity: &wgpu::Buffer,
    clip: &wgpu::Buffer,
    soft_mask: &wgpu::Buffer,
    groups_x: u32,
    groups_y: u32,
) {
    let params_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("pdf-render-wgpu-image-params"),
        contents: bytemuck::bytes_of(params),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("pdf-render-wgpu-image-bind-group"),
        layout: &pipelines.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: source.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: destination.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: opacity.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: clip.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: soft_mask.as_entire_binding(),
            },
        ],
    });
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some("pdf-render-wgpu-image-pass"),
        timestamp_writes: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, &bind_group, &[]);
    pass.dispatch_workgroups(groups_x, groups_y, 1);
}

fn encode_path_batch_dispatch(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &PipelineSet,
    size: DeviceSize,
    quality: RenderQuality,
    upload: &UploadedPathBatch,
    page: &wgpu::Buffer,
) {
    let dispatch_width = upload.tile_count.min(MAX_BATCH_DISPATCH_EDGE).max(1);
    let params = PathBatchParams {
        page_width: size.width,
        page_height: size.height,
        tile_count: upload.tile_count,
        samples: match quality {
            RenderQuality::Draft => 4,
            RenderQuality::Normal => 8,
        },
        dispatch_width,
        _pad0: 0,
        _pad1: 0,
        _pad2: 0,
    };
    let params_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("pdf-render-wgpu-path-batch-params"),
        contents: bytemuck::bytes_of(&params),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("pdf-render-wgpu-path-batch-bind-group"),
        layout: &pipelines.path_batch_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: upload.descriptors.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: upload.geometry.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: upload.tiles.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: upload.tile_path_indices.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: upload.masks.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 6,
                resource: page.as_entire_binding(),
            },
        ],
    });
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some("pdf-render-wgpu-path-batch-pass"),
        timestamp_writes: None,
    });
    pass.set_pipeline(&pipelines.paint_path_batch);
    pass.set_bind_group(0, &bind_group, &[]);
    pass.dispatch_workgroups(
        dispatch_width,
        upload.tile_count.div_ceil(dispatch_width),
        1,
    );
}

impl RenderBackend for WgpuBackend {
    fn id(&self) -> BackendId {
        BackendId::Wgpu
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            formats: vec![OutputFormat::Rgba8PremultipliedSrgb],
            max_surface: DeviceSize {
                width: 1 << 14,
                height: 1 << 14,
            },
            features: PageFeatures::BASIC_PATHS
                | PageFeatures::TEXT
                | PageFeatures::CLIPPING
                | PageFeatures::IMAGES
                | PageFeatures::STENCIL_MASKS
                | PageFeatures::SOFT_MASKS
                | PageFeatures::TRANSPARENCY
                | PageFeatures::NONSEPARABLE_BLENDS
                | PageFeatures::CODEC_MASK,
            resident_surfaces: false,
            postprocess: PostprocessCapabilities::NONE,
        }
    }

    fn supports(&self, page: &CompiledPage, request: &RenderRequest) -> SupportLevel {
        if request_shape_supported_forced(page, request) {
            SupportLevel::Native
        } else {
            SupportLevel::Unsupported(UnsupportedFeature {
                missing: page.features,
                detail: "experimental WGPU backend supports RGB images, solid paths/text, text/path clips, alpha/blend, and CPU-derived soft-mask planes",
            })
        }
    }

    fn submit(&self, request: RenderRequest) -> Result<RenderTicket, SubmitError> {
        let job_id = self.job_counter.fetch_add(1, Ordering::Relaxed);
        let (ticket, tx) = RenderTicket::new(job_id);
        let result = self
            .render_to_host_measured(&request)
            .map(|(host, _stats)| RenderedPage::Host(host));
        let _ = tx.send(result);
        Ok(ticket)
    }
}

/// Selection policy for the decoded-image GPU experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRendererPreference {
    /// Always use the normative CPU renderer.
    Cpu,
    /// Require WGPU initialization, while retaining per-page CPU fallback.
    Gpu,
    /// Use WGPU only when Lege detects a hardware GPU; otherwise use CPU.
    Auto,
}

impl ImageRendererPreference {
    /// Read `LEGE_PDF_IMAGE_RENDERER=cpu|gpu|auto`. The experiment remains
    /// CPU-default until its real-document coverage and stability gates pass.
    pub fn from_env() -> Result<Self, RenderError> {
        match std::env::var("LEGE_PDF_IMAGE_RENDERER") {
            Ok(value) if value.eq_ignore_ascii_case("cpu") => Ok(Self::Cpu),
            Ok(value) if value.eq_ignore_ascii_case("gpu") => Ok(Self::Gpu),
            Ok(value) if value.eq_ignore_ascii_case("auto") => Ok(Self::Auto),
            Ok(value) => Err(RenderError::Backend(format!(
                "invalid LEGE_PDF_IMAGE_RENDERER={value:?}; expected cpu, gpu, or auto"
            ))),
            Err(std::env::VarError::NotPresent) => Ok(Self::Cpu),
            Err(error) => Err(RenderError::Backend(format!(
                "could not read LEGE_PDF_IMAGE_RENDERER: {error}"
            ))),
        }
    }
}

/// Snapshot of experimental routing telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageRendererTelemetry {
    pub gpu_pages: u64,
    pub cpu_pages: u64,
    pub gpu_fallbacks: u64,
    pub gpu_panics: u64,
    pub cpu_failures: u64,
    pub gpu_initializations: u64,
    pub gpu_recoveries: u64,
}

/// Per-page execution measurements from the production-facing renderer seam.
#[derive(Debug, Clone)]
pub enum ImageRenderExecution {
    Cpu(RenderStats),
    Gpu(GpuRenderStats),
}

/// A host page plus the backend-specific measurements that produced it.
#[derive(Debug, Clone)]
pub struct ImageRenderResult {
    pub host: HostPage,
    pub execution: ImageRenderExecution,
}

/// Seamless experimental executor: eligible image-only pages use WGPU and
/// every other page (or any recoverable GPU failure) uses the normative CPU
/// backend. This is the integration surface whose default can later move from
/// [`ImageRendererPreference::Cpu`] to `Auto` without changing callers.
pub struct ExperimentalImageRenderer {
    cpu: Arc<CpuBackend>,
    gpu: Mutex<Option<Arc<WgpuBackend>>>,
    preference: ImageRendererPreference,
    gpu_unavailable_reason: OnceLock<Arc<str>>,
    gpu_recovery_pending: AtomicBool,
    job_counter: AtomicU64,
    gpu_pages: AtomicU64,
    cpu_pages: AtomicU64,
    gpu_fallbacks: AtomicU64,
    gpu_panics: AtomicU64,
    cpu_failures: AtomicU64,
    gpu_initializations: AtomicU64,
    gpu_recoveries: AtomicU64,
}

impl std::fmt::Debug for ExperimentalImageRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gpu_adapter = self.current_gpu().map(|gpu| gpu.adapter_info().clone());
        f.debug_struct("ExperimentalImageRenderer")
            .field("gpu_adapter", &gpu_adapter)
            .field("preference", &self.preference)
            .field("gpu_unavailable_reason", &self.gpu_unavailable_reason.get())
            .finish_non_exhaustive()
    }
}

impl ExperimentalImageRenderer {
    pub fn from_env(options: CpuBackendOptions) -> Result<Self, RenderError> {
        Self::new(ImageRendererPreference::from_env()?, options)
    }

    pub fn new(
        preference: ImageRendererPreference,
        options: CpuBackendOptions,
    ) -> Result<Self, RenderError> {
        let cpu = Arc::new(CpuBackend::new(options));
        let gpu = match preference {
            ImageRendererPreference::Cpu | ImageRendererPreference::Auto => None,
            ImageRendererPreference::Gpu => {
                Some(Arc::new(WgpuBackend::with_preparer(Arc::clone(&cpu))?))
            }
        };
        let gpu_initializations = u64::from(gpu.is_some());
        Ok(Self {
            cpu,
            gpu: Mutex::new(gpu),
            preference,
            gpu_unavailable_reason: OnceLock::new(),
            gpu_recovery_pending: AtomicBool::new(false),
            job_counter: AtomicU64::new(0),
            gpu_pages: AtomicU64::new(0),
            cpu_pages: AtomicU64::new(0),
            gpu_fallbacks: AtomicU64::new(0),
            gpu_panics: AtomicU64::new(0),
            cpu_failures: AtomicU64::new(0),
            gpu_initializations: AtomicU64::new(gpu_initializations),
            gpu_recoveries: AtomicU64::new(0),
        })
    }

    pub fn gpu_adapter_info(&self) -> Option<AdapterInfo> {
        self.current_gpu().map(|gpu| gpu.adapter_info().clone())
    }

    pub fn gpu_unavailable_reason(&self) -> Option<&str> {
        self.gpu_unavailable_reason.get().map(AsRef::as_ref)
    }

    /// Whether this policy executor currently owns an initialized GPU backend.
    /// `Auto` remains false until an eligible request survives CPU preparation.
    pub fn gpu_initialized(&self) -> bool {
        self.current_gpu().is_some()
    }

    pub fn telemetry(&self) -> ImageRendererTelemetry {
        ImageRendererTelemetry {
            gpu_pages: self.gpu_pages.load(Ordering::Relaxed),
            cpu_pages: self.cpu_pages.load(Ordering::Relaxed),
            gpu_fallbacks: self.gpu_fallbacks.load(Ordering::Relaxed),
            gpu_panics: self.gpu_panics.load(Ordering::Relaxed),
            cpu_failures: self.cpu_failures.load(Ordering::Relaxed),
            gpu_initializations: self.gpu_initializations.load(Ordering::Relaxed),
            gpu_recoveries: self.gpu_recoveries.load(Ordering::Relaxed),
        }
    }

    fn current_gpu(&self) -> Option<Arc<WgpuBackend>> {
        self.gpu
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn ensure_gpu(&self) -> Result<Option<Arc<WgpuBackend>>, RenderError> {
        if self.preference == ImageRendererPreference::Cpu {
            return Ok(None);
        }

        let mut slot = self.gpu.lock().unwrap_or_else(|error| error.into_inner());
        if slot.as_ref().is_some_and(|gpu| gpu.device_is_lost()) {
            *slot = None;
            self.gpu_recovery_pending.store(true, Ordering::Release);
        }
        if let Some(gpu) = slot.as_ref() {
            return Ok(Some(Arc::clone(gpu)));
        }
        if self.gpu_unavailable_reason.get().is_some() {
            return Ok(None);
        }

        let initialized = WgpuBackend::with_preparer(Arc::clone(&self.cpu));
        let gpu = match initialized {
            Ok(gpu)
                if self.preference == ImageRendererPreference::Auto && !gpu.is_hardware_gpu() =>
            {
                let reason: Arc<str> = Arc::from(format!(
                    "software adapter {:?} is not eligible for automatic rendering",
                    gpu.adapter_info()
                ));
                if self.gpu_recovery_pending.load(Ordering::Acquire) {
                    return Err(RenderError::GpuUnavailable(reason.to_string()));
                }
                let _ = self.gpu_unavailable_reason.set(reason);
                return Ok(None);
            }
            Ok(gpu) => Arc::new(gpu),
            Err(error) if self.preference == ImageRendererPreference::Auto => {
                if self.gpu_recovery_pending.load(Ordering::Acquire) {
                    return Err(error);
                }
                let _ = self
                    .gpu_unavailable_reason
                    .set(Arc::from(error.to_string()));
                return Ok(None);
            }
            Err(error) => return Err(error),
        };

        self.gpu_initializations.fetch_add(1, Ordering::Relaxed);
        if self.gpu_recovery_pending.swap(false, Ordering::AcqRel) {
            self.gpu_recoveries.fetch_add(1, Ordering::Relaxed);
        }
        *slot = Some(Arc::clone(&gpu));
        Ok(Some(gpu))
    }

    fn invalidate_gpu_after_loss(&self, gpu: &Arc<WgpuBackend>) {
        let mut slot = self.gpu.lock().unwrap_or_else(|error| error.into_inner());
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, gpu))
        {
            *slot = None;
            self.gpu_recovery_pending.store(true, Ordering::Release);
        }
        if let Some(reason) = gpu.device_loss_reason() {
            eprintln!("pdf-render-wgpu: invalidated lost device: {reason}");
        }
    }

    fn quarantine_gpu_after_panic(&self) {
        let mut slot = self.gpu.lock().unwrap_or_else(|error| error.into_inner());
        if self.preference == ImageRendererPreference::Auto {
            // A panic indicates a violated backend invariant rather than a
            // page-level capability decline. Keep this render transactional,
            // then stop automatic pickup for the rest of the process so a
            // deterministic backend bug cannot tax every later page. Arcs
            // already held by parallel jobs remain valid; their own failures
            // are independently contained by the same CPU fallback seam.
            *slot = None;
            let _ = self.gpu_unavailable_reason.set(Arc::from(
                "GPU renderer panicked; automatic routing disabled for this process",
            ));
        } else if let Some(gpu) = slot.as_ref()
            && gpu.device_is_lost()
        {
            let gpu = Arc::clone(gpu);
            drop(slot);
            self.invalidate_gpu_after_loss(&gpu);
        }
    }

    /// Render through the configured policy while preserving CPU diagnostics
    /// and focused GPU measurements for production callers.
    pub fn render_to_host(
        &self,
        request: &RenderRequest,
    ) -> Result<ImageRenderResult, RenderError> {
        self.render_to_host_with(request, |cpu| cpu.render_to_host(request))
    }

    /// Render while preserving a caller-owned CPU worker context on fallback.
    ///
    /// This is intended for persistent raster pools such as the viewer: GPU
    /// eligible requests use the same policy and cache as [`Self::render_to_host`],
    /// while CPU/default/ineligible requests retain their worker-local font,
    /// coverage, and raster scratch.
    pub fn render_to_host_with_cpu_context(
        &self,
        request: &RenderRequest,
        context: &mut CpuWorkerContext,
    ) -> Result<ImageRenderResult, RenderError> {
        self.render_to_host_with(request, |cpu| cpu.render_with(request, context))
    }

    fn render_to_host_with(
        &self,
        request: &RenderRequest,
        render_cpu: impl FnOnce(&CpuBackend) -> Result<(HostPage, RenderStats), RenderError>,
    ) -> Result<ImageRenderResult, RenderError> {
        // Treat GPU routing as a transaction over the immutable request. No
        // GPU-side result becomes observable until a complete HostPage has
        // survived validation, submission, mapping, and readback. A typed
        // failure or panic abandons that attempt and executes the original
        // request from the beginning through the normative CPU renderer.
        let gpu_attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let shape_supported = match self.preference {
                ImageRendererPreference::Gpu => {
                    request_shape_supported_forced(&request.page, request)
                }
                ImageRendererPreference::Auto => request_shape_supported(&request.page, request),
                ImageRendererPreference::Cpu => false,
            };
            if !shape_supported {
                return None;
            }
            match self.preference {
                ImageRendererPreference::Auto => {
                    if self.gpu_unavailable_reason.get().is_some() {
                        None
                    } else {
                        let total_start = Instant::now();
                        let prepare_start = Instant::now();
                        match self.cpu.prepare_rgb_image_page_for_auto(request) {
                            Ok(Some(prepared)) => {
                                let prepare = prepare_start.elapsed();
                                match self.ensure_gpu() {
                                    Ok(Some(gpu)) => Some((
                                        Arc::clone(&gpu),
                                        gpu.render_prepared_to_host_measured(
                                            request,
                                            prepared,
                                            prepare,
                                            total_start,
                                        ),
                                    )),
                                    Ok(None) => None,
                                    Err(error) => {
                                        self.gpu_fallbacks.fetch_add(1, Ordering::Relaxed);
                                        eprintln!(
                                            "pdf-render-wgpu: automatic GPU initialization failed: \
                                             {error}"
                                        );
                                        None
                                    }
                                }
                            }
                            Ok(None) => None,
                            // The CPU entry point observes the same token and
                            // returns `Cancelled` without painting. Keeping the
                            // route transactional avoids a special partially
                            // prepared outcome here.
                            Err(RenderError::Cancelled) => None,
                            Err(error) => {
                                self.gpu_fallbacks.fetch_add(1, Ordering::Relaxed);
                                eprintln!("pdf-render-wgpu: automatic preparation failed: {error}");
                                None
                            }
                        }
                    }
                }
                ImageRendererPreference::Gpu => match self.ensure_gpu() {
                    Ok(Some(gpu)) => Some((Arc::clone(&gpu), gpu.render_to_host_measured(request))),
                    Ok(None) => None,
                    Err(error) => {
                        self.gpu_fallbacks.fetch_add(1, Ordering::Relaxed);
                        eprintln!("pdf-render-wgpu: GPU recovery failed: {error}");
                        None
                    }
                },
                ImageRendererPreference::Cpu => None,
            }
        }));
        match gpu_attempt {
            Ok(Some((_, Ok((host, stats))))) => {
                self.gpu_pages.fetch_add(1, Ordering::Relaxed);
                return Ok(ImageRenderResult {
                    host,
                    execution: ImageRenderExecution::Gpu(stats),
                });
            }
            Ok(Some((_, Err(RenderError::Cancelled)))) => return Err(RenderError::Cancelled),
            Ok(Some((gpu, Err(error)))) => {
                self.gpu_fallbacks.fetch_add(1, Ordering::Relaxed);
                if gpu.device_is_lost() || matches!(error, RenderError::GpuUnavailable(_)) {
                    self.invalidate_gpu_after_loss(&gpu);
                }
                eprintln!("pdf-render-wgpu: GPU page failed; rerunning on CPU: {error}");
            }
            Ok(None) => {}
            Err(payload) => {
                self.gpu_fallbacks.fetch_add(1, Ordering::Relaxed);
                self.gpu_panics.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "pdf-render-wgpu: GPU attempt panicked; rerunning complete request on CPU: {}",
                    pdf_render_api::panic_message(payload)
                );
                self.quarantine_gpu_after_panic();
            }
        }

        let cpu_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_cpu(self.cpu.as_ref())
        }));
        match cpu_result {
            Ok(Ok((host, stats))) => {
                self.cpu_pages.fetch_add(1, Ordering::Relaxed);
                Ok(ImageRenderResult {
                    host,
                    execution: ImageRenderExecution::Cpu(stats),
                })
            }
            Ok(Err(error)) => {
                self.cpu_failures.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
            Err(payload) => {
                self.cpu_failures.fetch_add(1, Ordering::Relaxed);
                Err(RenderError::Panic {
                    message: format!(
                        "CPU render panicked inside image-renderer policy: {}",
                        pdf_render_api::panic_message(payload)
                    ),
                })
            }
        }
    }

    fn render(&self, request: &RenderRequest) -> Result<RenderedPage, RenderError> {
        self.render_to_host(request)
            .map(|result| RenderedPage::Host(result.host))
    }
}

impl RenderBackend for ExperimentalImageRenderer {
    fn id(&self) -> BackendId {
        if self.current_gpu().is_some() {
            BackendId::Wgpu
        } else {
            BackendId::Cpu
        }
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.cpu.capabilities()
    }

    fn supports(&self, page: &CompiledPage, request: &RenderRequest) -> SupportLevel {
        if self.preference != ImageRendererPreference::Cpu
            && self.gpu_unavailable_reason.get().is_none()
            && match self.preference {
                ImageRendererPreference::Gpu => request_shape_supported_forced(page, request),
                ImageRendererPreference::Auto => request_shape_supported(page, request),
                ImageRendererPreference::Cpu => false,
            }
        {
            SupportLevel::Native
        } else {
            self.cpu.supports(page, request)
        }
    }

    fn submit(&self, request: RenderRequest) -> Result<RenderTicket, SubmitError> {
        let job_id = self.job_counter.fetch_add(1, Ordering::Relaxed);
        let (ticket, tx) = RenderTicket::new(job_id);
        let _ = tx.send(self.render(&request));
        Ok(ticket)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

    use super::*;
    use pdf_page_ir::{
        Color, FontId, FontResource, GlyphRun, GlyphRunId, ImageColorSpace, ImageIr, ImageSMask,
        InterpolationMode, MaskKind, Matrix, PageBounds, PageComplexity, Paint, PlacedGlyph, Rect,
        ResourceKey, TilingPattern,
    };
    use pdf_render_api::{
        AnnotationMode, OutputResidency, PageTransform, RenderLimits, RenderQuality,
    };

    fn image_page(samples: Vec<u8>, interpolation: InterpolationMode) -> CompiledPage {
        image_page_with(samples, interpolation, 8, ImageColorSpace::Rgb, None)
    }

    fn mixed_path_page() -> CompiledPage {
        let mut page = image_page(vec![32, 64, 160, 32, 64, 160], InterpolationMode::Nearest);
        page.paths = Arc::from([PathData {
            verbs: Arc::from([
                PathVerb::MoveTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::Close,
            ]),
            points: Arc::from([
                pdf_page_ir::Point { x: 0.25, y: 0.25 },
                pdf_page_ir::Point { x: 0.75, y: 0.25 },
                pdf_page_ir::Point { x: 0.50, y: 0.75 },
            ]),
        }]);
        page.paints = Arc::from([
            Paint::Solid(Color::BLACK),
            Paint::Solid(Color::from_rgb(0.0, 1.0, 0.0)),
        ]);
        let mut operations = page.operations.to_vec();
        operations.push(DisplayOp::FillPath {
            path: pdf_page_ir::PathId(0),
            paint: pdf_page_ir::PaintId(1),
            rule: pdf_page_ir::FillRule::NonZero,
            alpha: 1.0,
            blend: BlendMode::Normal,
        });
        page.operations = operations.into();
        page.features |= PageFeatures::BASIC_PATHS;
        page
    }

    fn path_only_page() -> CompiledPage {
        let mut page = mixed_path_page();
        let fill = page
            .operations
            .last()
            .cloned()
            .expect("mixed fixture has a fill");
        page.operations = Arc::from([DisplayOp::ConcatTransform(Matrix::scale(8.0, 8.0)), fill]);
        page.images = Arc::from([]);
        page.features = PageFeatures::BASIC_PATHS;
        page
    }

    fn mixed_text_clip_page() -> CompiledPage {
        let mut page = image_page(vec![32, 64, 160, 32, 64, 160], InterpolationMode::Nearest);
        page.paths = Arc::from([PathData {
            verbs: Arc::from([
                PathVerb::MoveTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::Close,
            ]),
            points: Arc::from([
                pdf_page_ir::Point { x: 0.0, y: 0.0 },
                pdf_page_ir::Point { x: 1.0, y: 0.0 },
                pdf_page_ir::Point { x: 1.0, y: 1.0 },
                pdf_page_ir::Point { x: 0.0, y: 1.0 },
            ]),
        }]);
        page.paints = Arc::from([
            Paint::Solid(Color::BLACK),
            Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0)),
        ]);
        page.fonts = Arc::from([FontResource {
            key: ResourceKey {
                object_number: 10,
                generation: 0,
                variant: 0,
            },
            program: Arc::from([]),
            face_index: 0,
            synthetic_shear: 0.0,
            synthetic_embolden_em: 0.0,
        }]);
        page.glyph_runs = Arc::from([GlyphRun {
            font: FontId(0),
            font_size: 1.0,
            transform: Matrix::IDENTITY,
            glyphs: Arc::from([PlacedGlyph {
                glyph: 65,
                x: 0.20,
                y: 0.20,
            }]),
            render_mode: 7,
        }]);
        let mut operations = page.operations.to_vec();
        operations.extend([
            DisplayOp::PushClipText {
                runs: Box::from([GlyphRunId(0)]),
            },
            DisplayOp::FillPath {
                path: pdf_page_ir::PathId(0),
                paint: pdf_page_ir::PaintId(1),
                rule: pdf_page_ir::FillRule::NonZero,
                alpha: 1.0,
                blend: BlendMode::Normal,
            },
        ]);
        page.operations = operations.into();
        page.features |= PageFeatures::BASIC_PATHS | PageFeatures::TEXT | PageFeatures::CLIPPING;
        page
    }

    fn blended_image_page(mode: BlendMode) -> CompiledPage {
        let mut page = image_page(vec![32, 96, 224, 224, 160, 32], InterpolationMode::Nearest);
        let mut source = page.images[0].clone();
        source.key = ResourceKey {
            object_number: 2,
            generation: 0,
            variant: 0,
        };
        source.samples = Some(Arc::from([224, 48, 80, 32, 208, 144]));
        page.images = Arc::from([page.images[0].clone(), source]);
        page.operations = Arc::from([
            DisplayOp::ConcatTransform(Matrix::scale(8.0, 8.0)),
            DisplayOp::DrawImage {
                image: pdf_page_ir::ImageId(0),
                paint: pdf_page_ir::PaintId(0),
                transform: Matrix::IDENTITY,
                alpha: 1.0,
                blend: BlendMode::Normal,
            },
            DisplayOp::DrawImage {
                image: pdf_page_ir::ImageId(1),
                paint: pdf_page_ir::PaintId(0),
                transform: Matrix::IDENTITY,
                alpha: 0.75,
                blend: mode,
            },
        ]);
        page.features |= PageFeatures::TRANSPARENCY;
        page
    }

    fn patterned_stencil_page(with_native_image: bool) -> CompiledPage {
        let mut page = image_page(vec![48, 96, 192, 192, 160, 48], InterpolationMode::Nearest);
        let background = page.images[0].clone();
        let mut stencil = background.clone();
        stencil.key = ResourceKey {
            object_number: 2,
            generation: 0,
            variant: 0,
        };
        stencil.width = 8;
        stencil.height = 8;
        stencil.is_stencil = true;
        stencil.bits_per_component = 1;
        stencil.color_space = ImageColorSpace::Gray;
        stencil.samples = Some(Arc::from([0x55; 8]));

        let mut cell = image_page(vec![0, 0, 0, 0, 0, 0], InterpolationMode::Nearest);
        cell.bounds.crop = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 4.0,
            y1: 4.0,
        };
        cell.operations = Arc::from([DisplayOp::FillPath {
            path: pdf_page_ir::PathId(0),
            paint: pdf_page_ir::PaintId(0),
            rule: pdf_page_ir::FillRule::NonZero,
            alpha: 1.0,
            blend: BlendMode::Normal,
        }]);
        cell.paths = Arc::from([PathData {
            verbs: Arc::from([
                PathVerb::MoveTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::Close,
            ]),
            points: Arc::from([
                pdf_page_ir::Point { x: 0.0, y: 0.0 },
                pdf_page_ir::Point { x: 2.0, y: 0.0 },
                pdf_page_ir::Point { x: 2.0, y: 2.0 },
                pdf_page_ir::Point { x: 0.0, y: 2.0 },
            ]),
        }]);
        cell.paints = Arc::from([Paint::Solid(Color::from_rgb(0.9, 0.15, 0.4))]);
        cell.images = Arc::from([]);
        cell.features = PageFeatures::BASIC_PATHS;

        page.paints = Arc::from([
            Paint::Solid(Color::BLACK),
            Paint::Pattern {
                tiling: pdf_page_ir::TilingId(0),
                matrix: Matrix::IDENTITY,
            },
        ]);
        page.tilings = Arc::from([TilingPattern {
            key: ResourceKey {
                object_number: 3,
                generation: 0,
                variant: 0,
            },
            uncolored: false,
            under_color: Color::BLACK,
            bbox: [0.0, 0.0, 4.0, 4.0],
            x_step: 4.0,
            y_step: 4.0,
            cell: Arc::new(cell),
        }]);
        page.images = if with_native_image {
            Arc::from([background, stencil])
        } else {
            Arc::from([stencil])
        };
        let stencil_id = u32::from(with_native_image);
        let mut operations = vec![DisplayOp::ConcatTransform(Matrix::scale(8.0, 8.0))];
        if with_native_image {
            operations.push(DisplayOp::DrawImage {
                image: pdf_page_ir::ImageId(0),
                paint: pdf_page_ir::PaintId(0),
                transform: Matrix::IDENTITY,
                alpha: 1.0,
                blend: BlendMode::Normal,
            });
        }
        operations.push(DisplayOp::DrawImage {
            image: pdf_page_ir::ImageId(stencil_id),
            paint: pdf_page_ir::PaintId(1),
            transform: Matrix::IDENTITY,
            alpha: 0.75,
            blend: BlendMode::Multiply,
        });
        page.operations = operations.into();
        page.features |=
            PageFeatures::STENCIL_MASKS | PageFeatures::PATTERNS | PageFeatures::TRANSPARENCY;
        page
    }

    fn image_page_with(
        samples: Vec<u8>,
        interpolation: InterpolationMode,
        bits_per_component: u8,
        color_space: ImageColorSpace,
        decode: Option<Arc<[[f32; 2]]>>,
    ) -> CompiledPage {
        let image = ImageIr {
            key: ResourceKey {
                object_number: 1,
                generation: 0,
                variant: 0,
            },
            width: 2,
            height: 1,
            is_stencil: false,
            interpolation,
            soft_mask: None,
            bits_per_component,
            color_space,
            decode,
            samples: Some(Arc::from(samples)),
            codec: None,
            codec_data: None,
            codec_parms: None,
            smask: None,
            mask: None,
            smask_in_data: 0,
            lowering_degraded: false,
        };
        CompiledPage {
            schema_version: pdf_page_ir::IR_SCHEMA_VERSION,
            bounds: PageBounds {
                crop: Rect {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 8.0,
                    y1: 8.0,
                },
                rotate: 0,
            },
            content_bounds: None,
            operations: Arc::from([
                DisplayOp::ConcatTransform(Matrix::scale(8.0, 8.0)),
                DisplayOp::DrawImage {
                    image: pdf_page_ir::ImageId(0),
                    paint: pdf_page_ir::PaintId(0),
                    transform: Matrix::IDENTITY,
                    alpha: 1.0,
                    blend: BlendMode::Normal,
                },
            ]),
            paths: Arc::from([]),
            paints: Arc::from([Paint::Solid(Color::BLACK)]),
            stroke_styles: Arc::from([]),
            glyph_runs: Arc::from([]),
            fonts: Arc::from([]),
            images: Arc::from([image]),
            masks: Arc::from([]),
            groups: Arc::from([]),
            shadings: Arc::from([]),
            tilings: Arc::from([]),
            features: PageFeatures::IMAGES,
            complexity: PageComplexity::default(),
        }
    }

    fn request(page: CompiledPage) -> RenderRequest {
        RenderRequest {
            page: Arc::new(page),
            transform: PageTransform {
                matrix: Matrix::IDENTITY,
            },
            crop: None,
            output_size: DeviceSize {
                width: 8,
                height: 8,
            },
            output_format: OutputFormat::Rgba8PremultipliedSrgb,
            background: Background::White,
            color_policy: RenderColorPolicy::Original,
            annotations: AnnotationMode::None,
            quality: RenderQuality::Normal,
            limits: RenderLimits::default(),
            residency: OutputResidency::HostRequired,
        }
    }

    fn minified_gray_page(bits_per_component: u8) -> CompiledPage {
        let mut page = image_page_with(
            if bits_per_component == 1 {
                vec![0x55; 32]
            } else {
                vec![0x55; 16 * 16]
            },
            InterpolationMode::Bilinear,
            bits_per_component,
            ImageColorSpace::Gray,
            None,
        );
        let mut image = page.images[0].clone();
        image.width = 16;
        image.height = 16;
        page.images = Arc::from([image]);
        page
    }

    fn pixel(page: &HostPage, x: usize, y: usize) -> [u8; 4] {
        let offset = y * page.stride + x * 4;
        page.pixels[offset..offset + 4].try_into().unwrap()
    }

    #[test]
    fn preflight_accepts_preparable_image_color_spaces_and_bpc() {
        let mut base_request = request(image_page(
            vec![255, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
        ));
        assert!(request_shape_supported(&base_request.page, &base_request));
        base_request.output_format = OutputFormat::Gray8;
        assert!(!request_shape_supported(&base_request.page, &base_request));
        assert_eq!(
            classify_gpu_eligibility(&base_request.page, &base_request).reasons,
            vec![GpuIneligibility::OutputFormat]
        );

        let gray_decode = request(image_page_with(
            vec![0, 255],
            InterpolationMode::Nearest,
            8,
            ImageColorSpace::Gray,
            Some(Arc::from([[1.0, 0.0]])),
        ));
        assert!(classify_gpu_eligibility(&gray_decode.page, &gray_decode).is_eligible());

        let indexed = request(image_page_with(
            vec![0b0100_0000],
            InterpolationMode::Nearest,
            1,
            ImageColorSpace::Indexed {
                base: Box::new(ImageColorSpace::Rgb),
                hival: 1,
                lookup: Arc::from([0, 0, 0, 255, 0, 0]),
            },
            None,
        ));
        assert!(classify_gpu_eligibility(&indexed.page, &indexed).is_eligible());

        let invalid_bpc = request(image_page_with(
            vec![0; 8],
            InterpolationMode::Nearest,
            17,
            ImageColorSpace::Cmyk,
            None,
        ));
        assert_eq!(
            classify_gpu_eligibility(&invalid_bpc.page, &invalid_bpc).reasons,
            vec![GpuIneligibility::ImageBitsPerComponent]
        );

        let mut oversized_page = image_page_with(
            vec![0],
            InterpolationMode::Nearest,
            1,
            ImageColorSpace::Gray,
            None,
        );
        let mut oversized_image = oversized_page.images[0].clone();
        oversized_image.width = 5_000;
        oversized_image.height = 5_000;
        oversized_page.images = Arc::from([oversized_image]);
        let oversized = request(oversized_page);
        assert_eq!(
            classify_gpu_eligibility(&oversized.page, &oversized).reasons,
            vec![GpuIneligibility::ImageRgbConversionBudget]
        );
    }

    #[test]
    fn cpu_preparation_converts_gray_indexed_cmyk_and_reuses_rgb_arc() {
        let backend = CpuBackend::default();

        let gray = request(image_page_with(
            vec![0, 255],
            InterpolationMode::Nearest,
            8,
            ImageColorSpace::Gray,
            Some(Arc::from([[1.0, 0.0]])),
        ));
        let first = backend
            .prepare_rgb_image_page(&gray)
            .unwrap()
            .expect("gray image is preparable");
        assert_eq!(&*first.images[0].samples, &[255, 255, 255, 0, 0, 0]);
        let warm = backend
            .prepare_rgb_image_page(&gray)
            .unwrap()
            .expect("gray image remains preparable");
        assert!(Arc::ptr_eq(
            &first.images[0].samples,
            &warm.images[0].samples
        ));

        let indexed = request(image_page_with(
            vec![0b0100_0000],
            InterpolationMode::Nearest,
            1,
            ImageColorSpace::Indexed {
                base: Box::new(ImageColorSpace::Rgb),
                hival: 1,
                lookup: Arc::from([0, 0, 0, 255, 0, 0]),
            },
            None,
        ));
        let indexed = backend
            .prepare_rgb_image_page(&indexed)
            .unwrap()
            .expect("indexed image is preparable");
        assert_eq!(&*indexed.images[0].samples, &[0, 0, 0, 255, 0, 0]);

        let cmyk = request(image_page_with(
            vec![0, 0, 0, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
            8,
            ImageColorSpace::Cmyk,
            None,
        ));
        let cmyk = backend
            .prepare_rgb_image_page(&cmyk)
            .unwrap()
            .expect("CMYK image is preparable");
        assert_eq!(&cmyk.images[0].samples[..3], &[255, 255, 255]);
        assert_eq!(&cmyk.images[0].samples[3..], &[35, 31, 32]);
    }

    #[test]
    fn cpu_preparation_normalizes_and_reuses_soft_mask_and_stencil_opacity() {
        let backend = CpuBackend::default();

        let mut soft_page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let mut soft_image = soft_page.images[0].clone();
        soft_image.smask = Some(Arc::new(ImageSMask {
            width: 2,
            height: 1,
            bits_per_component: 1,
            decode: Some(Arc::from([[1.0, 0.0]])),
            samples: Arc::from([0x80]),
            codec: None,
            codec_data: None,
            codec_parms: None,
        }));
        soft_page.images = Arc::from([soft_image]);
        let soft_request = request(soft_page);
        assert!(classify_gpu_eligibility(&soft_request.page, &soft_request).is_eligible());
        let soft = backend
            .prepare_rgb_image_page(&soft_request)
            .unwrap()
            .expect("image soft mask is preparable");
        let opacity = soft.images[0]
            .opacity
            .as_ref()
            .expect("soft mask becomes an opacity plane");
        assert_eq!(&*opacity.samples, &[0, 255]);
        assert!(opacity.box_filter);
        assert_eq!(soft.images[0].stencil_rgb, None);
        let warm = backend
            .prepare_rgb_image_page(&soft_request)
            .unwrap()
            .expect("soft mask remains preparable");
        assert!(Arc::ptr_eq(
            &opacity.samples,
            &warm.images[0].opacity.as_ref().unwrap().samples
        ));

        let mut stencil_page = image_page_with(
            vec![0x40],
            InterpolationMode::Nearest,
            1,
            ImageColorSpace::Gray,
            None,
        );
        let mut stencil_image = stencil_page.images[0].clone();
        stencil_image.is_stencil = true;
        stencil_page.images = Arc::from([stencil_image]);
        stencil_page.paints = Arc::from([Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))]);
        stencil_page.features |= PageFeatures::STENCIL_MASKS;
        let stencil_request = request(stencil_page);
        assert!(classify_gpu_eligibility(&stencil_request.page, &stencil_request).is_eligible());
        let stencil = backend
            .prepare_rgb_image_page(&stencil_request)
            .unwrap()
            .expect("solid stencil brush is preparable");
        assert!(stencil.images[0].samples.is_empty());
        assert_eq!(stencil.images[0].stencil_rgb, Some([255, 0, 0]));
        assert_eq!(
            &*stencil.images[0].opacity.as_ref().unwrap().samples,
            &[255, 0]
        );
        assert!(
            backend
                .prepare_rgb_image_page_for_auto(&stencil_request)
                .unwrap()
                .is_some(),
            "the ordinary bilevel CPU fast path does not cover stencils"
        );
    }

    #[test]
    fn cpu_prepares_and_caches_patterned_stencil_brushes() {
        let backend = CpuBackend::default();
        let pattern_only = request(patterned_stencil_page(false));
        let report = classify_gpu_eligibility(&pattern_only.page, &pattern_only);
        assert!(report.is_eligible(), "{:?}", report.reasons);

        let cold = backend
            .prepare_rgb_image_page(&pattern_only)
            .unwrap()
            .expect("forced GPU preparation accepts a patterned stencil");
        assert_eq!(cold.images.len(), 1);
        let brush = &cold.images[0];
        assert_eq!(brush.bounds.width, 8);
        assert_eq!(brush.bounds.height, 8);
        assert_eq!(brush.samples.len(), 8 * 8 * 3);
        assert_eq!(brush.opacity.as_ref().unwrap().samples.len(), 8 * 8);
        assert_eq!(brush.alpha, 191);
        assert_eq!(brush.blend, BlendMode::Multiply);
        assert!(brush.stencil_rgb.is_none());

        let warm = backend
            .prepare_rgb_image_page(&pattern_only)
            .unwrap()
            .expect("patterned stencil remains preparable");
        assert!(Arc::ptr_eq(
            &cold.images[0].samples,
            &warm.images[0].samples
        ));
        assert!(Arc::ptr_eq(
            &cold.images[0].opacity.as_ref().unwrap().samples,
            &warm.images[0].opacity.as_ref().unwrap().samples
        ));
        assert!(
            backend
                .prepare_rgb_image_page_for_auto(&pattern_only)
                .unwrap()
                .is_none(),
            "Auto avoids a GPU round trip when CPU already painted the only draw"
        );

        let mixed = request(patterned_stencil_page(true));
        assert!(
            backend
                .prepare_rgb_image_page_for_auto(&mixed)
                .unwrap()
                .is_some(),
            "a native image can amortize the bounded CPU pattern bridge"
        );
    }

    #[test]
    fn cpu_preparation_normalizes_color_key_and_separate_stencil_masks() {
        let backend = CpuBackend::default();

        let mut color_key_page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let mut color_key_image = color_key_page.images[0].clone();
        color_key_image.mask = Some(ImageMask::ColorKey(Arc::from([[255, 255], [0, 0], [0, 0]])));
        color_key_page.images = Arc::from([color_key_image]);
        let color_key_request = request(color_key_page);
        assert!(
            classify_gpu_eligibility(&color_key_request.page, &color_key_request).is_eligible()
        );
        let color_key = backend
            .prepare_rgb_image_page(&color_key_request)
            .unwrap()
            .expect("colour-key mask is preparable");
        let color_key_opacity = color_key.images[0]
            .opacity
            .as_ref()
            .expect("colour-key mask becomes an opacity plane");
        assert_eq!(&*color_key_opacity.samples, &[0, 255]);
        assert!(!color_key_opacity.box_filter);
        let color_key_warm = backend
            .prepare_rgb_image_page(&color_key_request)
            .unwrap()
            .expect("colour-key mask remains preparable");
        assert!(Arc::ptr_eq(
            &color_key_opacity.samples,
            &color_key_warm.images[0].opacity.as_ref().unwrap().samples
        ));

        let mut hard_stencil_page =
            image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let mut hard_stencil_image = hard_stencil_page.images[0].clone();
        hard_stencil_image.mask = Some(ImageMask::Stencil(Arc::new(ImageSMask {
            width: 2,
            height: 1,
            bits_per_component: 1,
            decode: None,
            samples: Arc::from([0x80]),
            codec: None,
            codec_data: None,
            codec_parms: None,
        })));
        hard_stencil_page.images = Arc::from([hard_stencil_image]);
        let hard_stencil_request = request(hard_stencil_page);
        assert!(
            classify_gpu_eligibility(&hard_stencil_request.page, &hard_stencil_request)
                .is_eligible()
        );
        let hard_stencil = backend
            .prepare_rgb_image_page(&hard_stencil_request)
            .unwrap()
            .expect("separate stencil mask is preparable");
        assert_eq!(
            &*hard_stencil.images[0]
                .opacity
                .as_ref()
                .expect("separate stencil becomes an opacity plane")
                .samples,
            &[0, 255]
        );
        assert!(!hard_stencil.images[0].opacity.as_ref().unwrap().box_filter);
    }

    #[test]
    fn automatic_preparation_keeps_only_minified_bilevel_images_on_cpu() {
        let backend = CpuBackend::default();
        let minified_bilevel = request(minified_gray_page(1));

        let forced = backend
            .prepare_rgb_image_page(&minified_bilevel)
            .unwrap()
            .expect("forced GPU policy keeps the full preparation surface");
        assert!(forced.images[0].footprint.iter().any(|axis| *axis > 1.0));
        assert!(
            backend
                .prepare_rgb_image_page_for_auto(&minified_bilevel)
                .unwrap()
                .is_none(),
            "automatic routing should decline before bilevel RGB expansion"
        );

        let magnified_bilevel = request(image_page_with(
            vec![0b0100_0000],
            InterpolationMode::Nearest,
            1,
            ImageColorSpace::Gray,
            None,
        ));
        assert!(
            backend
                .prepare_rgb_image_page_for_auto(&magnified_bilevel)
                .unwrap()
                .is_some(),
            "automatic routing should retain bilevel images near source resolution"
        );

        let minified_gray8 = request(minified_gray_page(8));
        assert!(
            backend
                .prepare_rgb_image_page_for_auto(&minified_gray8)
                .unwrap()
                .is_some(),
            "the packed-bilevel CPU fast path must not divert other image formats"
        );
    }

    #[test]
    fn cpu_preparation_keeps_constant_image_alpha() {
        let backend = CpuBackend::default();
        let mut page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let mut ops = page.operations.to_vec();
        let DisplayOp::DrawImage { alpha, .. } = &mut ops[1] else {
            panic!("image fixture draw moved");
        };
        *alpha = 0.5;
        page.operations = Arc::from(ops);
        let request = request(page);

        let report = classify_gpu_eligibility(&request.page, &request);
        assert!(report.is_eligible(), "{:?}", report.reasons);
        let prepared = backend
            .prepare_rgb_image_page(&request)
            .unwrap()
            .expect("constant-alpha image is preparable");
        assert_eq!(prepared.images[0].alpha, 128);
    }

    #[test]
    fn paints_all_pdf_image_blend_modes_when_wgpu_is_available() {
        let Ok(backend) = WgpuBackend::new() else {
            return;
        };
        let cpu = CpuBackend::default();
        let modes = [
            BlendMode::Normal,
            BlendMode::Multiply,
            BlendMode::Screen,
            BlendMode::Overlay,
            BlendMode::Darken,
            BlendMode::Lighten,
            BlendMode::ColorDodge,
            BlendMode::ColorBurn,
            BlendMode::HardLight,
            BlendMode::SoftLight,
            BlendMode::Difference,
            BlendMode::Exclusion,
            BlendMode::Hue,
            BlendMode::Saturation,
            BlendMode::Color,
            BlendMode::Luminosity,
        ];

        for mode in modes {
            let request = request(blended_image_page(mode));
            let report = classify_gpu_eligibility(&request.page, &request);
            assert!(report.is_eligible(), "{mode:?}: {:?}", report.reasons);
            let prepared = cpu
                .prepare_rgb_image_page(&request)
                .unwrap()
                .expect("blend page is preparable");
            assert_eq!(prepared.images[1].blend, mode);

            let expected = cpu.render_to_host(&request).unwrap().0;
            let actual = backend.render_to_host_measured(&request).unwrap().0;
            for (index, (&left, &right)) in
                expected.pixels.iter().zip(actual.pixels.iter()).enumerate()
            {
                assert!(
                    left.abs_diff(right) <= 1,
                    "{mode:?} byte {index}: CPU={left}, GPU={right}"
                );
            }
        }
    }

    #[test]
    fn paints_cpu_converted_image_spaces_when_wgpu_is_available() {
        let Ok(backend) = WgpuBackend::new() else {
            return;
        };

        let gray = request(image_page_with(
            vec![0, 255],
            InterpolationMode::Nearest,
            8,
            ImageColorSpace::Gray,
            Some(Arc::from([[1.0, 0.0]])),
        ));
        let (gray_page, cold) = backend.render_to_host_measured(&gray).unwrap();
        assert_eq!(pixel(&gray_page, 1, 4), [255, 255, 255, 255]);
        assert_eq!(pixel(&gray_page, 6, 4), [0, 0, 0, 255]);
        assert_eq!(cold.cache_misses, 1);
        let (_, warm) = backend.render_to_host_measured(&gray).unwrap();
        assert_eq!(warm.cache_hits, 1);
        assert_eq!(warm.uploaded_bytes, 0);

        let indexed = request(image_page_with(
            vec![0b0100_0000],
            InterpolationMode::Nearest,
            1,
            ImageColorSpace::Indexed {
                base: Box::new(ImageColorSpace::Rgb),
                hival: 1,
                lookup: Arc::from([0, 0, 0, 255, 0, 0]),
            },
            None,
        ));
        let (indexed_page, _) = backend.render_to_host_measured(&indexed).unwrap();
        assert_eq!(pixel(&indexed_page, 1, 4), [0, 0, 0, 255]);
        assert_eq!(pixel(&indexed_page, 6, 4), [255, 0, 0, 255]);

        let cmyk = request(image_page_with(
            vec![0, 0, 0, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
            8,
            ImageColorSpace::Cmyk,
            None,
        ));
        let (cmyk_page, _) = backend.render_to_host_measured(&cmyk).unwrap();
        assert_eq!(pixel(&cmyk_page, 1, 4), [255, 255, 255, 255]);
        assert_eq!(pixel(&cmyk_page, 6, 4), [35, 31, 32, 255]);
    }

    #[test]
    fn paints_soft_mask_and_solid_stencil_when_wgpu_is_available() {
        let Ok(backend) = WgpuBackend::new() else {
            return;
        };

        let mut soft_page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let mut soft_image = soft_page.images[0].clone();
        soft_image.smask = Some(Arc::new(ImageSMask {
            width: 2,
            height: 1,
            bits_per_component: 1,
            decode: None,
            samples: Arc::from([0x80]),
            codec: None,
            codec_data: None,
            codec_parms: None,
        }));
        soft_page.images = Arc::from([soft_image]);
        let soft_request = request(soft_page.clone());
        let (soft, cold) = backend.render_to_host_measured(&soft_request).unwrap();
        assert_eq!(pixel(&soft, 1, 4), [255, 0, 0, 255]);
        assert_eq!(pixel(&soft, 6, 4), [255, 255, 255, 255]);
        assert_eq!(cold.cache_misses, 2);
        assert_eq!(cold.uploaded_bytes, 8);
        let (_, warm) = backend.render_to_host_measured(&soft_request).unwrap();
        assert_eq!(warm.cache_hits, 2);

        let mut alpha_ops = soft_page.operations.to_vec();
        let DisplayOp::DrawImage { alpha, .. } = &mut alpha_ops[1] else {
            panic!("image fixture draw moved");
        };
        *alpha = 0.5;
        soft_page.operations = Arc::from(alpha_ops);
        let alpha_request = request(soft_page);
        assert!(classify_gpu_eligibility(&alpha_request.page, &alpha_request).is_eligible());
        let prepared = backend
            .preparer
            .prepare_rgb_image_page(&alpha_request)
            .unwrap()
            .expect("constant-alpha image with an /SMask is preparable");
        assert_eq!(prepared.images[0].alpha, 128);
        let (alpha_masked, _) = backend.render_to_host_measured(&alpha_request).unwrap();
        assert_eq!(pixel(&alpha_masked, 1, 4), [255, 127, 127, 255]);
        assert_eq!(pixel(&alpha_masked, 6, 4), [255, 255, 255, 255]);

        let mut stencil_page = image_page_with(
            vec![0x40],
            InterpolationMode::Nearest,
            1,
            ImageColorSpace::Gray,
            None,
        );
        let mut stencil_image = stencil_page.images[0].clone();
        stencil_image.is_stencil = true;
        stencil_page.images = Arc::from([stencil_image]);
        stencil_page.paints = Arc::from([Paint::Solid(Color::from_rgb(1.0, 0.0, 0.0))]);
        stencil_page.features |= PageFeatures::STENCIL_MASKS;
        let stencil_request = request(stencil_page);
        let (stencil, stats) = backend.render_to_host_measured(&stencil_request).unwrap();
        assert_eq!(pixel(&stencil, 1, 4), [255, 0, 0, 255]);
        assert_eq!(pixel(&stencil, 6, 4), [255, 255, 255, 255]);
        assert_eq!(stats.cache_misses, 1);
        assert_eq!(stats.uploaded_bytes, 2);
    }

    #[test]
    fn paints_patterned_stencil_brush_when_wgpu_is_available() {
        let Ok(backend) = WgpuBackend::new() else {
            return;
        };
        let request = request(patterned_stencil_page(true));
        let expected = CpuBackend::default().render_to_host(&request).unwrap().0;
        let (actual, cold) = backend.render_to_host_measured(&request).unwrap();

        let mut max_difference = 0u8;
        for (&cpu, &gpu) in expected.pixels.iter().zip(actual.pixels.iter()) {
            max_difference = max_difference.max(cpu.abs_diff(gpu));
        }
        assert!(
            max_difference <= 1,
            "patterned stencil CPU/GPU maximum byte difference: {max_difference}"
        );
        assert!(
            actual
                .pixels
                .chunks_exact(4)
                .any(|pixel| pixel[0] > pixel[1] && pixel[0] > pixel[2]),
            "the colored pattern cell paints through the stencil"
        );
        assert!(cold.cache_misses >= 3);

        let (_, warm) = backend.render_to_host_measured(&request).unwrap();
        assert!(warm.cache_hits >= 3);
        assert_eq!(warm.uploaded_bytes, 0);
    }

    #[test]
    fn prepares_and_paints_page_level_soft_mask_state() {
        let mut page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let image_draw = page.operations[1].clone();
        page.paths = Arc::from([PathData {
            verbs: Arc::from([
                PathVerb::MoveTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::Close,
            ]),
            points: Arc::from([
                pdf_page_ir::Point { x: 0.0, y: 0.0 },
                pdf_page_ir::Point { x: 4.0, y: 0.0 },
                pdf_page_ir::Point { x: 4.0, y: 8.0 },
                pdf_page_ir::Point { x: 0.0, y: 8.0 },
            ]),
        }]);
        page.paints = Arc::from([
            Paint::Solid(Color::BLACK),
            Paint::Solid(Color::from_rgb(1.0, 1.0, 1.0)),
        ]);
        page.operations = Arc::from([
            DisplayOp::BeginSoftMask {
                kind: MaskKind::Luminosity,
                transfer: None,
            },
            DisplayOp::FillPath {
                path: pdf_page_ir::PathId(0),
                paint: pdf_page_ir::PaintId(1),
                rule: pdf_page_ir::FillRule::NonZero,
                alpha: 1.0,
                blend: BlendMode::Normal,
            },
            DisplayOp::EndSoftMask,
            DisplayOp::ConcatTransform(Matrix::scale(8.0, 8.0)),
            image_draw,
        ]);
        page.features |= PageFeatures::SOFT_MASKS | PageFeatures::TRANSPARENCY;
        let masked_request = request(page.clone());

        let report = classify_gpu_eligibility(&masked_request.page, &masked_request);
        assert!(report.is_eligible(), "{:?}", report.reasons);
        assert_eq!(report.image_draws, 1);
        let prepared = CpuBackend::default()
            .prepare_rgb_image_page(&masked_request)
            .unwrap()
            .expect("page-level soft mask is preparable");
        let mask = prepared.images[0]
            .soft_mask
            .as_ref()
            .expect("active mask is attached to the image");
        assert_eq!(mask.bounds.x, 0);
        assert_eq!(mask.bounds.y, 0);
        assert_eq!(mask.bounds.width, 4);
        assert_eq!(mask.bounds.height, 8);
        assert_eq!(mask.outside, 0);
        assert!(mask.samples.iter().all(|&alpha| alpha == 255));

        if let Ok(backend) = WgpuBackend::new() {
            let (rendered, cold) = backend.render_to_host_measured(&masked_request).unwrap();
            assert_eq!(pixel(&rendered, 1, 4), [255, 0, 0, 255]);
            assert_eq!(pixel(&rendered, 6, 4), [255, 255, 255, 255]);
            assert_eq!(cold.cache_misses, 2);
            let (_, warm) = backend.render_to_host_measured(&masked_request).unwrap();
            assert_eq!(warm.cache_hits, 2);
        }

        let mut backdrop_page = page;
        let mut backdrop_ops = backdrop_page.operations.to_vec();
        backdrop_ops[0] = DisplayOp::BeginSoftMask {
            kind: MaskKind::LuminosityBc {
                backdrop: [128, 128, 128],
            },
            transfer: None,
        };
        backdrop_page.operations = Arc::from(backdrop_ops);
        let backdrop_request = request(backdrop_page);
        let prepared = CpuBackend::default()
            .prepare_rgb_image_page(&backdrop_request)
            .unwrap()
            .expect("/BC page-level soft mask is preparable");
        assert_eq!(prepared.images[0].soft_mask.as_ref().unwrap().outside, 128);
        if let Ok(backend) = WgpuBackend::new() {
            let (rendered, _) = backend.render_to_host_measured(&backdrop_request).unwrap();
            assert_eq!(pixel(&rendered, 1, 4), [255, 0, 0, 255]);
            assert_eq!(pixel(&rendered, 6, 4), [127, 127, 255, 255]);
        }
    }

    #[test]
    fn paints_color_key_and_separate_stencil_masks_when_wgpu_is_available() {
        let Ok(backend) = WgpuBackend::new() else {
            return;
        };

        let mut color_key_page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let mut color_key_image = color_key_page.images[0].clone();
        color_key_image.mask = Some(ImageMask::ColorKey(Arc::from([[255, 255], [0, 0], [0, 0]])));
        color_key_page.images = Arc::from([color_key_image]);
        let color_key_request = request(color_key_page);
        let (color_key, cold) = backend.render_to_host_measured(&color_key_request).unwrap();
        assert_eq!(pixel(&color_key, 1, 4), [255, 255, 255, 255]);
        assert_eq!(pixel(&color_key, 6, 4), [0, 0, 255, 255]);
        assert_eq!(cold.cache_misses, 2);
        assert_eq!(cold.uploaded_bytes, 8);
        let (_, warm) = backend.render_to_host_measured(&color_key_request).unwrap();
        assert_eq!(warm.cache_hits, 2);

        let mut hard_stencil_page =
            image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let mut hard_stencil_image = hard_stencil_page.images[0].clone();
        hard_stencil_image.mask = Some(ImageMask::Stencil(Arc::new(ImageSMask {
            width: 2,
            height: 1,
            bits_per_component: 1,
            decode: None,
            samples: Arc::from([0x80]),
            codec: None,
            codec_data: None,
            codec_parms: None,
        })));
        hard_stencil_page.images = Arc::from([hard_stencil_image]);
        let hard_stencil_request = request(hard_stencil_page);
        let (hard_stencil, stats) = backend
            .render_to_host_measured(&hard_stencil_request)
            .unwrap();
        assert_eq!(pixel(&hard_stencil, 1, 4), [255, 255, 255, 255]);
        assert_eq!(pixel(&hard_stencil, 6, 4), [0, 0, 255, 255]);
        assert_eq!(stats.cache_misses, 2);
        assert_eq!(stats.uploaded_bytes, 8);
    }

    #[test]
    fn rectangular_and_analytic_image_clips_are_prepared_and_painted() {
        let mut page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let draw = page.operations[1].clone();
        page.paths = Arc::from([PathData {
            verbs: Arc::from([
                PathVerb::MoveTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::Close,
            ]),
            points: Arc::from([
                pdf_page_ir::Point { x: 0.0, y: 0.0 },
                pdf_page_ir::Point { x: 0.5, y: 0.0 },
                pdf_page_ir::Point { x: 0.5, y: 1.0 },
                pdf_page_ir::Point { x: 0.0, y: 1.0 },
            ]),
        }]);
        page.operations = Arc::from([
            DisplayOp::Save,
            DisplayOp::ConcatTransform(Matrix::scale(8.0, 8.0)),
            DisplayOp::PushClip {
                path: pdf_page_ir::PathId(0),
                rule: pdf_page_ir::FillRule::NonZero,
            },
            draw.clone(),
            draw,
            DisplayOp::PopClip,
            DisplayOp::Restore,
        ]);
        page.features |= PageFeatures::CLIPPING;
        let clipped_request = request(page.clone());
        assert!(classify_gpu_eligibility(&clipped_request.page, &clipped_request).is_eligible());
        let prepared = CpuBackend::default()
            .prepare_rgb_image_page(&clipped_request)
            .unwrap()
            .expect("rectangular clip is represented by prepared image bounds");
        assert_eq!(prepared.images[0].bounds.width, 4);
        assert_eq!(prepared.images[0].bounds.height, 8);
        assert!(prepared.images[0].clip.is_none());

        if let Ok(backend) = WgpuBackend::new() {
            let (rendered, _) = backend.render_to_host_measured(&clipped_request).unwrap();
            assert_eq!(pixel(&rendered, 1, 4), [255, 0, 0, 255]);
            assert_eq!(pixel(&rendered, 6, 4), [255, 255, 255, 255]);
        }

        let mut analytic_page = page;
        analytic_page.paths = Arc::from([PathData {
            verbs: Arc::from([
                PathVerb::MoveTo,
                PathVerb::LineTo,
                PathVerb::LineTo,
                PathVerb::Close,
            ]),
            points: Arc::from([
                pdf_page_ir::Point { x: 0.0, y: 0.0 },
                pdf_page_ir::Point { x: 1.0, y: 0.0 },
                pdf_page_ir::Point { x: 0.5, y: 1.0 },
            ]),
        }]);
        let mut analytic_image = analytic_page.images[0].clone();
        analytic_image.smask = Some(Arc::new(ImageSMask {
            width: 1,
            height: 1,
            bits_per_component: 8,
            decode: None,
            samples: Arc::from([128]),
            codec: None,
            codec_data: None,
            codec_parms: None,
        }));
        analytic_page.images = Arc::from([analytic_image]);
        let analytic_request = request(analytic_page);
        assert!(classify_gpu_eligibility(&analytic_request.page, &analytic_request).is_eligible());
        let prepared = CpuBackend::default()
            .prepare_rgb_image_page(&analytic_request)
            .unwrap()
            .expect("analytic clip is exported as a device-space alpha plane");
        let clip = prepared.images[0]
            .clip
            .as_ref()
            .expect("triangle needs analytic coverage");
        assert_eq!(clip.bounds.width, 8);
        assert_eq!(clip.bounds.height, 8);
        assert!(clip.samples.iter().any(|&alpha| alpha == 0));
        assert!(clip.samples.iter().any(|&alpha| alpha == 255));
        assert!(Arc::ptr_eq(
            &clip.samples,
            &prepared.images[1].clip.as_ref().unwrap().samples
        ));

        if let Ok(backend) = WgpuBackend::new() {
            let (rendered, stats) = backend.render_to_host_measured(&analytic_request).unwrap();
            assert_eq!(pixel(&rendered, 1, 1), [255, 63, 63, 255]);
            assert_eq!(pixel(&rendered, 6, 6), [255, 255, 255, 255]);
            assert_eq!(stats.cache_misses, 3);
            assert_eq!(stats.cache_hits, 3);
        }
    }

    #[test]
    fn preflight_accepts_invisible_and_solid_visible_text() {
        let mut page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let image_ops = page.operations.to_vec();
        page.fonts = Arc::from([FontResource {
            key: ResourceKey {
                object_number: 10,
                generation: 0,
                variant: 0,
            },
            program: Arc::from([]),
            face_index: 0,
            synthetic_shear: 0.0,
            synthetic_embolden_em: 0.0,
        }]);
        page.glyph_runs = Arc::from([GlyphRun {
            font: FontId(0),
            font_size: 10.0,
            transform: Matrix::IDENTITY,
            glyphs: Arc::from([]),
            render_mode: 3,
        }]);
        let mut operations = image_ops.clone();
        operations.push(DisplayOp::DrawGlyphRun {
            run: GlyphRunId(0),
            paint: pdf_page_ir::PaintId(0),
            alpha: 1.0,
            blend: BlendMode::Normal,
            stroke: None,
        });
        page.operations = Arc::from(operations);
        page.features |= PageFeatures::TEXT;
        let invisible = request(page.clone());
        assert!(request_shape_supported(&invisible.page, &invisible));
        let invisible_report = classify_gpu_eligibility(&invisible.page, &invisible);
        assert!(invisible_report.is_eligible());
        assert_eq!(invisible_report.image_draws, 1);
        assert_eq!(invisible_report.ignored_text_draws, 1);

        page.glyph_runs = Arc::from([GlyphRun {
            font: FontId(0),
            font_size: 10.0,
            transform: Matrix::IDENTITY,
            glyphs: Arc::from([]),
            render_mode: 0,
        }]);
        let mut zero_alpha_operations = image_ops.clone();
        zero_alpha_operations.push(DisplayOp::DrawGlyphRun {
            run: GlyphRunId(0),
            paint: pdf_page_ir::PaintId(0),
            alpha: 0.0,
            blend: BlendMode::Normal,
            stroke: None,
        });
        page.operations = Arc::from(zero_alpha_operations);
        let zero_alpha = request(page.clone());
        assert!(request_shape_supported(&zero_alpha.page, &zero_alpha));

        let mut visible_operations = image_ops;
        visible_operations.push(DisplayOp::DrawGlyphRun {
            run: GlyphRunId(0),
            paint: pdf_page_ir::PaintId(0),
            alpha: 1.0,
            blend: BlendMode::Normal,
            stroke: None,
        });
        page.operations = Arc::from(visible_operations);
        let visible = request(page);
        assert!(request_shape_supported(&visible.page, &visible));
        assert!(
            CpuBackend::default()
                .prepare_rgb_image_page(&visible)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn prepares_and_paints_native_mixed_path_when_wgpu_is_available() {
        let request = request(mixed_path_page());
        let report = classify_gpu_eligibility(&request.page, &request);
        assert!(report.is_eligible(), "{:?}", report.reasons);
        let prepared = CpuBackend::default()
            .prepare_rgb_image_page(&request)
            .unwrap()
            .expect("solid mixed path is preparable");
        assert!(matches!(prepared.commands[0], PreparedGpuCommand::Image(0)));
        let PreparedGpuCommand::PathBatch(batch) = &prepared.commands[1] else {
            panic!("second command must preserve the batched path painter order");
        };
        let path = &batch.paths[0];
        assert_eq!(path.edge_count, 3);
        assert_eq!(batch.max_tile_depth, 1);

        let Ok(backend) = WgpuBackend::new() else {
            return;
        };
        let cpu = CpuBackend::default().render_to_host(&request).unwrap().0;
        let (gpu, stats) = backend.render_to_host_measured(&request).unwrap();
        assert_eq!(pixel(&gpu, 4, 3), [0, 255, 0, 255]);
        assert_eq!(pixel(&gpu, 0, 0), pixel(&cpu, 0, 0));
        assert_eq!(stats.image_draws, 1);
        assert_eq!(stats.path_draws, 1);
        assert_eq!(stats.path_batches, 1);
        assert_eq!(stats.path_dispatches, 1);
        assert_eq!(stats.path_edges, 3);
    }

    #[test]
    fn preparation_batches_path_runs_and_keeps_image_barriers() {
        let mut page = mixed_path_page();
        let image = page.operations[1].clone();
        let path = page.operations.last().cloned().unwrap();
        page.operations = Arc::from([
            page.operations[0].clone(),
            image.clone(),
            path.clone(),
            path,
            image,
        ]);
        let prepared = CpuBackend::default()
            .prepare_rgb_image_page(&request(page))
            .unwrap()
            .expect("mixed painter run is preparable");
        assert_eq!(prepared.commands.len(), 3);
        assert!(matches!(prepared.commands[0], PreparedGpuCommand::Image(0)));
        let PreparedGpuCommand::PathBatch(batch) = &prepared.commands[1] else {
            panic!("consecutive paths must share one batch");
        };
        assert_eq!(batch.paths.len(), 2);
        assert_eq!(batch.max_tile_depth, 2);
        assert!(matches!(prepared.commands[2], PreparedGpuCommand::Image(1)));
    }

    #[test]
    fn preparation_splits_a_tile_at_the_bounded_path_depth() {
        let mut page = path_only_page();
        let transform = page.operations[0].clone();
        let path = page.operations[1].clone();
        let mut operations = Vec::with_capacity(66);
        operations.push(transform);
        operations.extend(std::iter::repeat_n(path, 65));
        page.operations = operations.into();
        let prepared = CpuBackend::default()
            .prepare_rgb_image_page(&request(page))
            .unwrap()
            .expect("bounded path runs are preparable");
        assert_eq!(prepared.commands.len(), 2);
        let batches: Vec<_> = prepared
            .commands
            .iter()
            .map(|command| match command {
                PreparedGpuCommand::PathBatch(batch) => batch,
                _ => panic!("path-only page must contain only batches"),
            })
            .collect();
        assert_eq!(batches[0].paths.len(), 64);
        assert_eq!(batches[0].max_tile_depth, 64);
        assert_eq!(batches[1].paths.len(), 1);
    }

    #[test]
    fn forced_gpu_accepts_path_only_while_auto_retains_image_gate() {
        let request = request(path_only_page());
        let report = classify_gpu_eligibility(&request.page, &request);
        assert_eq!(report.reasons, vec![GpuIneligibility::NoImageDraw]);
        assert!(!request_shape_supported(&request.page, &request));
        assert!(request_shape_supported_forced(&request.page, &request));

        let cpu = CpuBackend::default();
        let prepared = cpu
            .prepare_rgb_image_page(&request)
            .unwrap()
            .expect("forced preparation accepts a native path-only page");
        assert!(prepared.images.is_empty());
        assert_eq!(prepared.commands.len(), 1);
        assert!(
            cpu.prepare_rgb_image_page_for_auto(&request)
                .unwrap()
                .is_none(),
            "automatic routing must retain the measured image/content gate"
        );

        let Ok(backend) = WgpuBackend::new() else {
            return;
        };
        let (gpu, stats) = backend.render_to_host_measured(&request).unwrap();
        assert_eq!(pixel(&gpu, 4, 3), [0, 255, 0, 255]);
        assert_eq!(stats.image_draws, 0);
        assert_eq!(stats.path_draws, 1);
        let (_, warm) = backend.render_to_host_measured(&request).unwrap();
        assert_eq!(warm.cache_hits, 1);
        assert_eq!(backend.path_upload_cache_telemetry().hits, 1);
    }

    #[test]
    fn text_clip_constrains_native_gpu_path() {
        let request = request(mixed_text_clip_page());
        assert!(classify_gpu_eligibility(&request.page, &request).is_eligible());
        let prepared = CpuBackend::default()
            .prepare_rgb_image_page(&request)
            .unwrap()
            .expect("text-clipped solid path is preparable");
        let PreparedGpuCommand::PathBatch(batch) = &prepared.commands[1] else {
            panic!("text-clipped fill must remain a native path batch");
        };
        let path = &batch.paths[0];
        assert!(
            path.clip.is_some(),
            "text clip exports exact alpha coverage"
        );

        let Ok(backend) = WgpuBackend::new() else {
            return;
        };
        let cpu = CpuBackend::default().render_to_host(&request).unwrap().0;
        let (gpu, stats) = backend.render_to_host_measured(&request).unwrap();
        assert_eq!(pixel(&gpu, 2, 3), pixel(&cpu, 2, 3));
        assert_eq!(pixel(&gpu, 0, 0), pixel(&cpu, 0, 0));
        assert_eq!(stats.path_draws, 1);
    }

    #[test]
    fn paints_rgb_image_when_wgpu_is_available() {
        let Ok(backend) = WgpuBackend::new() else {
            return;
        };
        let request = request(image_page(
            vec![255, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
        ));
        let (page, stats) = backend.render_to_host_measured(&request).unwrap();
        assert_eq!(pixel(&page, 1, 4), [255, 0, 0, 255]);
        assert_eq!(pixel(&page, 6, 4), [0, 0, 255, 255]);
        assert_eq!(stats.image_draws, 1);
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(stats.cache_misses, 1);
        assert_eq!(stats.uploaded_bytes, 6);
        assert_eq!(stats.reused_bytes, 0);
        assert_eq!(stats.readback_bytes, 8 * 8 * 4);

        let (_, warm_stats) = backend.render_to_host_measured(&request).unwrap();
        assert_eq!(warm_stats.cache_hits, 1);
        assert_eq!(warm_stats.cache_misses, 0);
        assert_eq!(warm_stats.uploaded_bytes, 0);
        assert_eq!(warm_stats.reused_bytes, 6);
        assert_eq!(
            backend.upload_cache_telemetry(),
            GpuUploadCacheTelemetry {
                hits: 1,
                misses: 1,
                inserts: 1,
                evictions: 0,
                entries: 1,
                resident_bytes: 8,
                budget_bytes: DEFAULT_UPLOAD_CACHE_BYTES,
            }
        );
    }

    #[test]
    fn upload_cache_is_bounded_and_changed_samples_miss() {
        let Ok(backend) =
            WgpuBackend::with_cpu_options_and_cache_budget_bytes(CpuBackendOptions::default(), 8)
        else {
            return;
        };
        let first = request(image_page(
            vec![255, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
        ));
        let changed = request(image_page(
            vec![0, 255, 0, 255, 255, 0],
            InterpolationMode::Nearest,
        ));

        let (_, first_stats) = backend.render_to_host_measured(&first).unwrap();
        let (changed_page, changed_stats) = backend.render_to_host_measured(&changed).unwrap();
        assert_eq!(first_stats.cache_misses, 1);
        assert_eq!(changed_stats.cache_misses, 1);
        assert_eq!(changed_stats.cache_hits, 0);
        assert_eq!(pixel(&changed_page, 1, 4), [0, 255, 0, 255]);

        let telemetry = backend.upload_cache_telemetry();
        assert_eq!(telemetry.entries, 1);
        assert_eq!(telemetry.resident_bytes, 8);
        assert_eq!(telemetry.evictions, 1);
    }

    #[test]
    fn warm_upload_cache_is_shared_across_concurrent_renders() {
        let Ok(backend) = WgpuBackend::new().map(Arc::new) else {
            return;
        };
        let request = request(image_page(
            vec![255, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
        ));
        let (_, cold) = backend.render_to_host_measured(&request).unwrap();
        assert_eq!(cold.cache_misses, 1);

        std::thread::scope(|scope| {
            let mut jobs = Vec::new();
            for _ in 0..4 {
                jobs.push(scope.spawn(|| {
                    let (page, stats) = backend.render_to_host_measured(&request).unwrap();
                    (pixel(&page, 6, 4), stats)
                }));
            }
            for job in jobs {
                let (sample, stats) = job.join().unwrap();
                assert_eq!(sample, [0, 0, 255, 255]);
                assert_eq!(stats.cache_hits, 1);
                assert_eq!(stats.cache_misses, 0);
            }
        });
        let telemetry = backend.upload_cache_telemetry();
        assert_eq!(telemetry.hits, 4);
        assert_eq!(telemetry.misses, 1);
        assert_eq!(telemetry.entries, 1);
    }

    #[test]
    fn cpu_preference_is_a_seamless_fallback_backend() {
        let renderer = ExperimentalImageRenderer::new(
            ImageRendererPreference::Cpu,
            CpuBackendOptions::default(),
        )
        .unwrap();
        let request = request(image_page(
            vec![255, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
        ));
        let mut context = CpuWorkerContext::new();
        let rendered = renderer
            .render_to_host_with_cpu_context(&request, &mut context)
            .unwrap();
        assert!(matches!(rendered.execution, ImageRenderExecution::Cpu(_)));
        assert_eq!(pixel(&rendered.host, 1, 4), [255, 0, 0, 255]);
        assert_eq!(
            renderer.telemetry(),
            ImageRendererTelemetry {
                gpu_pages: 0,
                cpu_pages: 1,
                gpu_fallbacks: 0,
                gpu_panics: 0,
                cpu_failures: 0,
                gpu_initializations: 0,
                gpu_recoveries: 0,
            }
        );
    }

    #[test]
    fn auto_decline_does_not_initialize_wgpu() {
        let renderer = ExperimentalImageRenderer::new(
            ImageRendererPreference::Auto,
            CpuBackendOptions::default(),
        )
        .unwrap();
        let request = request(minified_gray_page(1));

        assert!(!renderer.gpu_initialized());
        let rendered = renderer.render_to_host(&request).unwrap();
        assert!(matches!(rendered.execution, ImageRenderExecution::Cpu(_)));
        assert!(!renderer.gpu_initialized());
        assert_eq!(
            renderer.telemetry(),
            ImageRendererTelemetry {
                gpu_pages: 0,
                cpu_pages: 1,
                gpu_fallbacks: 0,
                gpu_panics: 0,
                cpu_failures: 0,
                gpu_initializations: 0,
                gpu_recoveries: 0,
            }
        );
    }

    #[test]
    fn auto_mixed_page_stays_cpu_until_path_speed_gate_passes() {
        let renderer = ExperimentalImageRenderer::new(
            ImageRendererPreference::Auto,
            CpuBackendOptions::default(),
        )
        .unwrap();
        let request = request(mixed_path_page());
        let expected = CpuBackend::default().render_to_host(&request).unwrap().0;

        assert!(request_shape_supported(&request.page, &request));
        let rendered = renderer.render_to_host(&request).unwrap();
        assert!(matches!(rendered.execution, ImageRenderExecution::Cpu(_)));
        assert_eq!(&*rendered.host.pixels, &*expected.pixels);
        assert!(!renderer.gpu_initialized());
        assert_eq!(renderer.telemetry().gpu_initializations, 0);
    }

    #[test]
    fn auto_pattern_only_page_stays_cpu_without_initializing_wgpu() {
        let renderer = ExperimentalImageRenderer::new(
            ImageRendererPreference::Auto,
            CpuBackendOptions::default(),
        )
        .unwrap();
        let request = request(patterned_stencil_page(false));
        let expected = CpuBackend::default().render_to_host(&request).unwrap().0;

        let rendered = renderer.render_to_host(&request).unwrap();
        assert!(matches!(rendered.execution, ImageRenderExecution::Cpu(_)));
        assert_eq!(&*rendered.host.pixels, &*expected.pixels);
        assert!(!renderer.gpu_initialized());
        assert_eq!(
            renderer.telemetry(),
            ImageRendererTelemetry {
                gpu_pages: 0,
                cpu_pages: 1,
                gpu_fallbacks: 0,
                gpu_panics: 0,
                cpu_failures: 0,
                gpu_initializations: 0,
                gpu_recoveries: 0,
            }
        );
    }

    #[test]
    fn cancellation_after_gpu_submission_returns_and_device_recovers() {
        let Ok(backend) = WgpuBackend::new().map(Arc::new) else {
            return;
        };
        let cancellation = CancellationToken::new();
        let mut cancelled_request = request(image_page(
            vec![255, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
        ));
        cancelled_request.limits.cancellation = Some(cancellation.clone());

        let (submitted_tx, submitted_rx) = std::sync::mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        backend.install_after_submit_hook(submitted_tx, resume_rx);
        let worker_backend = Arc::clone(&backend);
        let worker =
            std::thread::spawn(move || worker_backend.render_to_host_measured(&cancelled_request));

        submitted_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("GPU render did not reach submission");
        cancellation.cancel();
        resume_tx.send(()).unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Err(RenderError::Cancelled)
        ));

        let healthy_request = request(image_page(
            vec![0, 255, 0, 255, 255, 0],
            InterpolationMode::Nearest,
        ));
        let (healthy_page, _) = backend
            .render_to_host_measured(&healthy_request)
            .expect("device must remain usable after cancellation");
        assert_eq!(pixel(&healthy_page, 1, 4), [0, 255, 0, 255]);
    }

    #[test]
    fn injected_device_loss_falls_back_once_then_gpu_recovers() {
        let Ok(renderer) = ExperimentalImageRenderer::new(
            ImageRendererPreference::Gpu,
            CpuBackendOptions::default(),
        ) else {
            return;
        };
        let request = request(image_page(
            vec![255, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
        ));
        renderer
            .current_gpu()
            .expect("forced GPU preference initialized")
            .inject_device_loss_once();

        let fallback = renderer.render_to_host(&request).unwrap();
        assert!(matches!(fallback.execution, ImageRenderExecution::Cpu(_)));
        assert_eq!(pixel(&fallback.host, 6, 4), [0, 0, 255, 255]);
        assert_eq!(
            renderer.telemetry(),
            ImageRendererTelemetry {
                gpu_pages: 0,
                cpu_pages: 1,
                gpu_fallbacks: 1,
                gpu_panics: 0,
                cpu_failures: 0,
                gpu_initializations: 1,
                gpu_recoveries: 0,
            }
        );

        let recovered = renderer.render_to_host(&request).unwrap();
        assert!(matches!(recovered.execution, ImageRenderExecution::Gpu(_)));
        assert_eq!(pixel(&recovered.host, 6, 4), [0, 0, 255, 255]);
        assert_eq!(
            renderer.telemetry(),
            ImageRendererTelemetry {
                gpu_pages: 1,
                cpu_pages: 1,
                gpu_fallbacks: 1,
                gpu_panics: 0,
                cpu_failures: 0,
                gpu_initializations: 2,
                gpu_recoveries: 1,
            }
        );
    }

    #[test]
    fn gpu_policy_panic_reruns_the_complete_request_on_cpu() {
        let Ok(renderer) = ExperimentalImageRenderer::new(
            ImageRendererPreference::Gpu,
            CpuBackendOptions::default(),
        ) else {
            return;
        };
        let request = request(blended_image_page(BlendMode::Multiply));
        let expected = CpuBackend::default().render_to_host(&request).unwrap().0;
        renderer
            .current_gpu()
            .expect("forced GPU preference initialized")
            .inject_prepared_render_panic_once();

        let fallback = renderer.render_to_host(&request).unwrap();
        assert!(matches!(fallback.execution, ImageRenderExecution::Cpu(_)));
        assert_eq!(&*fallback.host.pixels, &*expected.pixels);
        assert_eq!(
            renderer.telemetry(),
            ImageRendererTelemetry {
                gpu_pages: 0,
                cpu_pages: 1,
                gpu_fallbacks: 1,
                gpu_panics: 1,
                cpu_failures: 0,
                gpu_initializations: 1,
                gpu_recoveries: 0,
            }
        );
    }

    #[test]
    fn auto_panic_falls_back_atomically_and_quarantines_gpu() {
        let renderer = ExperimentalImageRenderer::new(
            ImageRendererPreference::Auto,
            CpuBackendOptions::default(),
        )
        .unwrap();
        let request = request(blended_image_page(BlendMode::SoftLight));
        let expected = CpuBackend::default().render_to_host(&request).unwrap().0;

        let warm = renderer.render_to_host(&request).unwrap();
        if !matches!(warm.execution, ImageRenderExecution::Gpu(_)) {
            // Auto intentionally declines software adapters, so this invariant
            // is exercised only where automatic routing can really select GPU.
            return;
        }
        renderer
            .current_gpu()
            .expect("Auto initialized a hardware GPU")
            .inject_prepared_render_panic_once();

        let fallback = renderer.render_to_host(&request).unwrap();
        assert!(matches!(fallback.execution, ImageRenderExecution::Cpu(_)));
        assert_eq!(&*fallback.host.pixels, &*expected.pixels);
        assert!(!renderer.gpu_initialized());
        assert!(renderer.gpu_unavailable_reason().is_some());
        assert_eq!(
            renderer.telemetry(),
            ImageRendererTelemetry {
                gpu_pages: 1,
                cpu_pages: 1,
                gpu_fallbacks: 1,
                gpu_panics: 1,
                cpu_failures: 0,
                gpu_initializations: 1,
                gpu_recoveries: 0,
            }
        );

        let later = renderer.render_to_host(&request).unwrap();
        assert!(matches!(later.execution, ImageRenderExecution::Cpu(_)));
        assert_eq!(&*later.host.pixels, &*expected.pixels);
        assert_eq!(renderer.telemetry().cpu_pages, 2);
        assert_eq!(renderer.telemetry().gpu_fallbacks, 1);
    }

    #[test]
    fn parallel_auto_pages_all_finish_across_gpu_quarantine() {
        let renderer = ExperimentalImageRenderer::new(
            ImageRendererPreference::Auto,
            CpuBackendOptions::default(),
        )
        .unwrap();
        let request = request(image_page(
            vec![255, 0, 0, 0, 0, 255],
            InterpolationMode::Nearest,
        ));
        let expected = CpuBackend::default().render_to_host(&request).unwrap().0;

        let warm = renderer.render_to_host(&request).unwrap();
        if !matches!(warm.execution, ImageRenderExecution::Gpu(_)) {
            return;
        }
        renderer
            .current_gpu()
            .expect("Auto initialized a hardware GPU")
            .inject_prepared_render_panic_once();

        let barrier = Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            let mut jobs = Vec::new();
            let renderer = &renderer;
            let request = &request;
            for _ in 0..8 {
                let barrier = Arc::clone(&barrier);
                jobs.push(scope.spawn(move || {
                    barrier.wait();
                    renderer.render_to_host(request)
                }));
            }
            for job in jobs {
                let rendered = job
                    .join()
                    .expect("policy contains the injected backend panic")
                    .expect("every parallel page has either a GPU or CPU result");
                assert_eq!(&*rendered.host.pixels, &*expected.pixels);
            }
        });

        let telemetry = renderer.telemetry();
        assert_eq!(telemetry.gpu_pages + telemetry.cpu_pages, 9);
        assert_eq!(telemetry.gpu_fallbacks, 1);
        assert_eq!(telemetry.gpu_panics, 1);
        assert_eq!(telemetry.cpu_failures, 0);
        assert!(telemetry.cpu_pages >= 1);
        assert!(!renderer.gpu_initialized());
    }
}
