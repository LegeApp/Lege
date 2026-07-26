//! Experimental WGPU PDF render backend.
//!
//! The first production-shaped slice paints decoded opaque images prepared as
//! RGB8 into one GPU-resident page surface and reads that surface back once.
//! All other PDF paint remains on the CPU backend until its semantics are
//! implemented here. Image decoding, color conversion, and request lowering
//! are shared with the normative CPU renderer through its narrow
//! [`pdf_render_cpu::PreparedRgbImagePage`] seam.

use std::collections::{BTreeSet, HashMap};
#[cfg(test)]
use std::sync::atomic::AtomicU8;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use lege_gpu::compute::{AdapterInfo, SharedGpuContext, wgpu};
use pdf_page_ir::{BlendMode, CompiledPage, DeviceSize, DisplayOp, ImageColorSpace, PageFeatures};
use pdf_render_api::{
    BackendCapabilities, BackendId, Background, CancellationToken, HostPage, OutputFormat,
    PostprocessCapabilities, RenderBackend, RenderColorPolicy, RenderError, RenderRequest,
    RenderTicket, RenderedPage, SubmitError, SupportLevel, UnsupportedFeature,
};
use pdf_render_cpu::{
    CpuBackend, CpuBackendOptions, CpuWorkerContext, MAX_PREPARED_RGB_CONVERSION_BYTES,
    PreparedRgbImage, PreparedRgbImagePage, RenderStats,
};
use wgpu::util::DeviceExt;

const WORKGROUP_EDGE: u32 = 16;
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
    _pad0: u32,
    _pad1: u32,
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
            _pad0: 0,
            _pad1: 0,
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
            _pad0: 0,
            _pad1: 0,
        }
    }
}

struct PipelineSet {
    layout: wgpu::BindGroupLayout,
    clear: wgpu::ComputePipeline,
    paint: wgpu::ComputePipeline,
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
        let dummy_source = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pdf-render-wgpu-dummy-source"),
            contents: &[0, 0, 0, 0],
            usage: wgpu::BufferUsages::STORAGE,
        });
        Self {
            layout,
            clear: make_pipeline("pdf-render-wgpu-clear", "clear_page"),
            paint: make_pipeline("pdf-render-wgpu-paint-image", "paint_image"),
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
    fn new(image: &PreparedRgbImage) -> Self {
        Self {
            data_ptr: image.samples.as_ptr() as usize,
            data_len: image.samples.len(),
            width: image.width,
            height: image.height,
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

    fn get(&self, image: &PreparedRgbImage) -> Option<Arc<wgpu::Buffer>> {
        let key = UploadCacheKey::new(image);
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
        if !Arc::ptr_eq(&entry.samples, &image.samples) {
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

    fn insert(&self, image: &PreparedRgbImage, buffer: Arc<wgpu::Buffer>, source_len: usize) {
        let key = UploadCacheKey::new(image);
        let charge = source_len.next_multiple_of(4);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.clock = state.clock.wrapping_add(1);
        let clock = state.clock;
        if let Some(previous) = state.entries.insert(
            key,
            UploadCacheEntry {
                buffer,
                samples: Arc::clone(&image.samples),
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

/// Experimental decoded-image GPU backend.
pub struct WgpuBackend {
    context: SharedGpuContext,
    adapter: AdapterInfo,
    pipelines: Arc<PipelineSet>,
    preparer: Arc<CpuBackend>,
    upload_cache: Arc<GpuUploadCache>,
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

    pub fn upload_cache_telemetry(&self) -> GpuUploadCacheTelemetry {
        self.upload_cache.telemetry()
    }

    /// Drop all cached decoded-image device buffers.
    pub fn clear_upload_cache(&self) {
        self.upload_cache.clear();
    }

    #[cfg(test)]
    fn inject_device_loss_once(&self) {
        self.test_fault.store(1, Ordering::Release);
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
        self.render_to_host_measured_with_policy(request, false)?
            .ok_or(RenderError::Unsupported(request.page.features))
    }

    fn render_to_host_measured_for_auto(
        &self,
        request: &RenderRequest,
    ) -> Result<Option<(HostPage, GpuRenderStats)>, RenderError> {
        self.render_to_host_measured_with_policy(request, true)
    }

    fn render_to_host_measured_with_policy(
        &self,
        request: &RenderRequest,
        automatic_routing: bool,
    ) -> Result<Option<(HostPage, GpuRenderStats)>, RenderError> {
        let total_start = Instant::now();
        if !request_shape_supported(&request.page, request) {
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
        let prepared = if automatic_routing {
            self.preparer.prepare_rgb_image_page_for_auto(request)?
        } else {
            self.preparer.prepare_rgb_image_page(request)?
        };
        let Some(prepared) = prepared else {
            return Ok(None);
        };
        let prepare = prepare_start.elapsed();
        validate_prepared(&prepared)?;

        let gpu_start = Instant::now();
        let (host, transfer) = self.execute(
            &prepared,
            request.background,
            request.limits.cancellation.as_ref(),
        )?;
        let gpu_and_readback = gpu_start.elapsed();
        let readback_bytes = prepared.size.width as u64 * prepared.size.height as u64 * 4;
        Ok(Some((
            host,
            GpuRenderStats {
                prepare,
                gpu_and_readback,
                total: total_start.elapsed(),
                image_draws: prepared.images.len() as u32,
                cache_hits: transfer.cache_hits,
                cache_misses: transfer.cache_misses,
                uploaded_bytes: transfer.uploaded_bytes,
                reused_bytes: transfer.reused_bytes,
                readback_bytes,
            },
        )))
    }

    fn execute(
        &self,
        prepared: &PreparedRgbImagePage,
        background: Background,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(HostPage, ExecuteTransferStats), RenderError> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.execute_inner(prepared, background, cancellation)
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
            prepared.size.width.div_ceil(WORKGROUP_EDGE),
            prepared.size.height.div_ceil(WORKGROUP_EDGE),
        );

        let mut transfer = ExecuteTransferStats::default();
        for image in &prepared.images {
            check_cancelled(cancellation)?;
            if image.bounds.width == 0 || image.bounds.height == 0 {
                continue;
            }
            let source_len = image.width as usize * image.height as usize * 3;
            if source_len > max_binding {
                return Err(RenderError::LimitExceeded(
                    "GPU image exceeds max storage buffer binding size",
                ));
            }
            let source_buffer = if let Some(buffer) = self.upload_cache.get(image) {
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
                self.upload_cache
                    .insert(image, Arc::clone(&buffer), source_len);
                transfer.cache_misses += 1;
                transfer.uploaded_bytes += source_len as u64;
                buffer
            };
            let params = ShaderParams::image(prepared.size, image);
            encode_dispatch(
                device,
                &mut encoder,
                &self.pipelines,
                &self.pipelines.paint,
                &params,
                source_buffer.as_ref(),
                &page_buffer,
                image.bounds.width.div_ceil(WORKGROUP_EDGE),
                image.bounds.height.div_ceil(WORKGROUP_EDGE),
            );
        }

        check_cancelled(cancellation)?;
        encoder.copy_buffer_to_buffer(&page_buffer, 0, &readback, 0, page_bytes as u64);
        queue.submit(std::iter::once(encoder.finish()));
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
                .wait()
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
        match op {
            DisplayOp::Save
            | DisplayOp::Restore
            | DisplayOp::ConcatTransform(_)
            | DisplayOp::BeginPaintOrigin(_)
            | DisplayOp::EndPaintOrigin => {}
            DisplayOp::DrawImage {
                image,
                alpha,
                blend,
                ..
            } => {
                image_draws += 1;
                let Some(resource) = page.images.get(image.index()) else {
                    reasons.insert(GpuIneligibility::MissingImageResource);
                    continue;
                };
                if *blend != BlendMode::Normal {
                    reasons.insert(GpuIneligibility::ImageBlend);
                }
                if (*alpha - 1.0).abs() > f32::EPSILON {
                    reasons.insert(GpuIneligibility::ImageAlpha);
                }
                if resource.is_stencil {
                    reasons.insert(GpuIneligibility::ImageStencil);
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
                if !direct_rgb8
                    && converted_rgb_bytes
                        .is_none_or(|bytes| bytes > MAX_PREPARED_RGB_CONVERSION_BYTES)
                {
                    reasons.insert(GpuIneligibility::ImageRgbConversionBudget);
                }
                if resource.soft_mask.is_some() || resource.smask.is_some() {
                    reasons.insert(GpuIneligibility::ImageSoftMask);
                }
                if resource.mask.is_some() {
                    reasons.insert(GpuIneligibility::ImageHardMask);
                }
                if resource.smask_in_data != 0 {
                    reasons.insert(GpuIneligibility::ImageInDataAlpha);
                }
            }
            DisplayOp::DrawGlyphRun {
                run, alpha, stroke, ..
            } => {
                let non_painting = page.glyph_runs.get(run.index()).is_some_and(|glyph_run| {
                    let mode = glyph_run.render_mode;
                    let paints_fill = matches!(mode, 0 | 2 | 4 | 6) && *alpha > 0.0;
                    let paints_stroke = matches!(mode, 1 | 2 | 5 | 6)
                        && stroke.as_ref().is_some_and(|value| value.alpha > 0.0);
                    !paints_fill && !paints_stroke
                });
                if non_painting {
                    ignored_text_draws += 1;
                } else {
                    reasons.insert(GpuIneligibility::VisibleText);
                }
            }
            DisplayOp::PushClip { .. } | DisplayOp::PushClipText { .. } | DisplayOp::PopClip => {
                reasons.insert(GpuIneligibility::Clip);
            }
            DisplayOp::FillPath { .. } | DisplayOp::StrokePath { .. } => {
                reasons.insert(GpuIneligibility::Path);
            }
            DisplayOp::BeginTransparencyGroup { .. } | DisplayOp::EndTransparencyGroup => {
                reasons.insert(GpuIneligibility::TransparencyGroup);
            }
            DisplayOp::ApplySoftMask { .. }
            | DisplayOp::BeginSoftMask { .. }
            | DisplayOp::EndSoftMask
            | DisplayOp::ClearSoftMask => {
                reasons.insert(GpuIneligibility::SoftMaskState);
            }
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
            features: PageFeatures::IMAGES | PageFeatures::CODEC_MASK,
            resident_surfaces: false,
            postprocess: PostprocessCapabilities::NONE,
        }
    }

    fn supports(&self, page: &CompiledPage, request: &RenderRequest) -> SupportLevel {
        if request_shape_supported(page, request) {
            SupportLevel::Native
        } else {
            SupportLevel::Unsupported(UnsupportedFeature {
                missing: page.features,
                detail: "experimental WGPU backend supports opaque image-only pages prepared as RGB8",
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
    gpu: Option<WgpuBackend>,
    preference: ImageRendererPreference,
    gpu_unavailable_reason: Option<Arc<str>>,
    job_counter: AtomicU64,
    gpu_pages: AtomicU64,
    cpu_pages: AtomicU64,
    gpu_fallbacks: AtomicU64,
}

impl std::fmt::Debug for ExperimentalImageRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExperimentalImageRenderer")
            .field(
                "gpu_adapter",
                &self.gpu.as_ref().map(WgpuBackend::adapter_info),
            )
            .field("preference", &self.preference)
            .field("gpu_unavailable_reason", &self.gpu_unavailable_reason)
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
        let (gpu, gpu_unavailable_reason) = match preference {
            ImageRendererPreference::Cpu => (None, None),
            ImageRendererPreference::Gpu => {
                (Some(WgpuBackend::with_preparer(Arc::clone(&cpu))?), None)
            }
            ImageRendererPreference::Auto => match WgpuBackend::with_preparer(Arc::clone(&cpu)) {
                Ok(gpu) if gpu.is_hardware_gpu() => (Some(gpu), None),
                Ok(gpu) => (
                    None,
                    Some(Arc::from(format!(
                        "software adapter {:?} is not eligible for automatic rendering",
                        gpu.adapter_info()
                    ))),
                ),
                Err(error) => (None, Some(Arc::from(error.to_string()))),
            },
        };
        Ok(Self {
            cpu,
            gpu,
            preference,
            gpu_unavailable_reason,
            job_counter: AtomicU64::new(0),
            gpu_pages: AtomicU64::new(0),
            cpu_pages: AtomicU64::new(0),
            gpu_fallbacks: AtomicU64::new(0),
        })
    }

    pub fn gpu_adapter_info(&self) -> Option<&AdapterInfo> {
        self.gpu.as_ref().map(WgpuBackend::adapter_info)
    }

    pub fn gpu_unavailable_reason(&self) -> Option<&str> {
        self.gpu_unavailable_reason.as_deref()
    }

    pub fn telemetry(&self) -> ImageRendererTelemetry {
        ImageRendererTelemetry {
            gpu_pages: self.gpu_pages.load(Ordering::Relaxed),
            cpu_pages: self.cpu_pages.load(Ordering::Relaxed),
            gpu_fallbacks: self.gpu_fallbacks.load(Ordering::Relaxed),
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
        if let Some(gpu) = &self.gpu
            && request_shape_supported(&request.page, request)
        {
            let gpu_result = match self.preference {
                ImageRendererPreference::Auto => gpu.render_to_host_measured_for_auto(request),
                ImageRendererPreference::Gpu | ImageRendererPreference::Cpu => {
                    gpu.render_to_host_measured(request).map(Some)
                }
            };
            match gpu_result {
                Ok(Some((host, stats))) => {
                    self.gpu_pages.fetch_add(1, Ordering::Relaxed);
                    return Ok(ImageRenderResult {
                        host,
                        execution: ImageRenderExecution::Gpu(stats),
                    });
                }
                Ok(None) => {}
                Err(RenderError::Cancelled) => return Err(RenderError::Cancelled),
                Err(_) => {
                    self.gpu_fallbacks.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.cpu_pages.fetch_add(1, Ordering::Relaxed);
        render_cpu(self.cpu.as_ref()).map(|(host, stats)| ImageRenderResult {
            host,
            execution: ImageRenderExecution::Cpu(stats),
        })
    }

    fn render(&self, request: &RenderRequest) -> Result<RenderedPage, RenderError> {
        self.render_to_host(request)
            .map(|result| RenderedPage::Host(result.host))
    }
}

impl RenderBackend for ExperimentalImageRenderer {
    fn id(&self) -> BackendId {
        if self.gpu.is_some() {
            BackendId::Wgpu
        } else {
            BackendId::Cpu
        }
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.cpu.capabilities()
    }

    fn supports(&self, page: &CompiledPage, request: &RenderRequest) -> SupportLevel {
        if self
            .gpu
            .as_ref()
            .is_some_and(|_| request_shape_supported(page, request))
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
        Color, FontId, GlyphRun, GlyphRunId, ImageColorSpace, ImageIr, InterpolationMode, Matrix,
        PageBounds, PageComplexity, Paint, Rect, ResourceKey,
    };
    use pdf_render_api::{
        AnnotationMode, OutputResidency, PageTransform, RenderLimits, RenderQuality,
    };

    fn image_page(samples: Vec<u8>, interpolation: InterpolationMode) -> CompiledPage {
        image_page_with(samples, interpolation, 8, ImageColorSpace::Rgb, None)
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
    fn preflight_accepts_invisible_ocr_but_not_visible_text() {
        let mut page = image_page(vec![255, 0, 0, 0, 0, 255], InterpolationMode::Nearest);
        let image_ops = page.operations.to_vec();
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
        assert!(!request_shape_supported(&visible.page, &visible));
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
            .gpu
            .as_ref()
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
            }
        );
    }
}
