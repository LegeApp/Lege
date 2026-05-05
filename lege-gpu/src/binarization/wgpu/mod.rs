use std::mem::size_of;
use std::sync::mpsc;

use crate::binarization::{
    BinarizationMode, BinarizationParams, BinarizeParamsStd140, GpuBinarizationError, Result,
};

// ---------------------------------------------------------------------------
// GPU context
// ---------------------------------------------------------------------------

struct WgpuContext {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl WgpuContext {
    async fn new_async() -> Result<Self> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| {
                GpuBinarizationError::Initialization(format!("request_adapter failed: {e:?}"))
            })?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("lege-wgpu-binarize-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits {
                    max_storage_buffers_per_shader_stage: 8,
                    ..wgpu::Limits::downlevel_defaults()
                },
                memory_hints: wgpu::MemoryHints::Performance,
                ..Default::default()
            })
            .await
            .map_err(|e| {
                GpuBinarizationError::Initialization(format!("request_device failed: {e}"))
            })?;

        Ok(Self {
            _instance: instance,
            _adapter: adapter,
            device,
            queue,
        })
    }

    fn new() -> Result<Self> {
        pollster::block_on(Self::new_async())
    }
}

// ---------------------------------------------------------------------------
// Buffer set — compute/work buffers only; readback is managed separately.
// ---------------------------------------------------------------------------

struct WgpuBuffers {
    params: wgpu::Buffer,

    // Image-sized — all 1 u32 per pixel (pixel_count * 4 bytes)
    gpu_src: wgpu::Buffer,    // RGBA-packed input (R | G<<8 | B<<16 | A<<24)
    gray_buf: wgpu::Buffer,   // gray output of linearize pass (consumed by all binarize shaders)
    gpu_dst: wgpu::Buffer,    // u32-per-pixel binary output before packing
    packed_dst: wgpu::Buffer, // ceil(pixel_count/4) × u32 — packed binary for readback
    bg_tmp: wgpu::Buffer,     // bg_max_h output
    bg: wgpu::Buffer,         // bg_max_v output

    // Padded-sized (padded_width × padded_height × element)
    padded_gray: wgpu::Buffer, // u32
    padded_sq: wgpu::Buffer,   // f32

    // Integral-sized (integral_width × integral_height × element).
    // row_prefix is one row shorter but uses the same allocation.
    row_prefix: wgpu::Buffer,    // u32
    row_prefix_sq: wgpu::Buffer, // f32
    integral: wgpu::Buffer,      // u32
    integral_sq: wgpu::Buffer,   // f32

    img_bytes: usize, // capacity: gpu_src / gray_buf / gpu_dst / bg_tmp / bg (all 1 u32/px)
    padded_bytes: usize, // capacity: padded_gray / padded_sq
    integral_bytes: usize, // capacity: row_prefix / row_prefix_sq / integral / integral_sq
}

// ---------------------------------------------------------------------------
// Main binarizer
// ---------------------------------------------------------------------------

pub struct WgpuBinarizer {
    ctx: WgpuContext,

    // sRGB→linear LUT (256 f32 = 1024 bytes), uploaded once at construction.
    srgb_lut: wgpu::Buffer,

    // Pre-pass: RGBA → linear-luma → sRGB gray (writes gray_buf consumed by everything below).
    linearize_pipeline: wgpu::ComputePipeline,
    linearize_bgl: wgpu::BindGroupLayout,

    // Single-pass fixed-threshold pipeline (from existing binarize_adaptive.wgsl, mode=0).
    fixed_pipeline: wgpu::ComputePipeline,
    fixed_bgl: wgpu::BindGroupLayout,

    // Multi-pass adaptive pipelines.
    build_padded_pipeline: wgpu::ComputePipeline,
    build_padded_bgl: wgpu::BindGroupLayout,

    integral_rows_pipeline: wgpu::ComputePipeline,
    integral_rows_bgl: wgpu::BindGroupLayout,

    integral_cols_pipeline: wgpu::ComputePipeline,
    integral_cols_bgl: wgpu::BindGroupLayout,

    bg_max_h_pipeline: wgpu::ComputePipeline,
    bg_max_v_pipeline: wgpu::ComputePipeline,
    bg_max_bgl: wgpu::BindGroupLayout, // shared by both bg_max pipelines

    final_pipeline: wgpu::ComputePipeline,
    final_bgl: wgpu::BindGroupLayout,

    pack_pipeline: wgpu::ComputePipeline,
    pack_bgl: wgpu::BindGroupLayout,

    buffers: Option<WgpuBuffers>,

    // Readback buffer — sized independently so batch mode can hold N pages at once.
    readback: Option<wgpu::Buffer>,
    readback_cap: usize,

    // Persistent CPU staging for RGB→RGBA expansion before write_buffer.
    // Reused across pages to avoid per-page allocation.
    upload_staging: Vec<u8>,
}

/// Source pixel layout passed to the GPU binarizer.
#[derive(Clone, Copy)]
pub enum SourceFormat {
    /// 1 byte per pixel — splatted to (G, G, G, 0) before upload so the linearize
    /// pass produces a stable round-trip (gray in → gray out, since R=G=B implies
    /// luma = LUT[gray] re-encoded ≈ identity within u8 quantization).
    Gray,
    /// 3 bytes per pixel: [R, G, B, R, G, B, ...]. Padded to RGBA on upload.
    Rgb,
}

// ---------------------------------------------------------------------------
// Helper: build a BindGroupLayout from a slice of (binding, read_only) pairs.
// Binding 0 is always the params uniform; bindings 1..N are storage buffers.
// ---------------------------------------------------------------------------

fn make_bgl(
    device: &wgpu::Device,
    label: &str,
    storage_bindings: &[(u32, bool)], // (binding index, read_only)
) -> wgpu::BindGroupLayout {
    let mut entries = vec![wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }];
    for &(binding, read_only) in storage_bindings {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        });
    }
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &entries,
    })
}

fn make_pipeline(
    device: &wgpu::Device,
    label: &str,
    bgl: &wgpu::BindGroupLayout,
    shader: &wgpu::ShaderModule,
    entry_point: &str,
) -> wgpu::ComputePipeline {
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[bgl],
        immediate_size: 0,
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pl),
        module: shader,
        entry_point: Some(entry_point),
        compilation_options: Default::default(),
        cache: None,
    })
}

fn srgb_to_linear_lut() -> [f32; 256] {
    let mut lut = [0.0f32; 256];
    for i in 0..256 {
        let c = i as f32 / 255.0;
        lut[i] = if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    lut
}

impl WgpuBinarizer {
    pub fn new() -> Result<Self> {
        let ctx = WgpuContext::new()?;
        let dev = &ctx.device;

        // --- sRGB→linear LUT (constant, uploaded once) ---
        let lut_data = srgb_to_linear_lut();
        let srgb_lut = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-srgb-lut"),
            size: std::mem::size_of_val(&lut_data) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue
            .write_buffer(&srgb_lut, 0, bytemuck::cast_slice(&lut_data));

        // --- Linearize pre-pass (RGBA → sRGB gray) ---
        let linearize_shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-wgpu-linearize-shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("shaders/adaptive_linearize.wgsl").into(),
            ),
        });
        let linearize_bgl = make_bgl(
            dev,
            "lege-wgpu-linearize-bgl",
            &[(1, true), (2, false), (3, true)],
        );
        let linearize_pipeline = make_pipeline(
            dev,
            "lege-wgpu-linearize",
            &linearize_bgl,
            &linearize_shader,
            "main",
        );

        // --- Fixed threshold (existing single-pass shader) ---
        let fixed_shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-wgpu-fixed-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/binarize_adaptive.wgsl").into()),
        });
        let fixed_bgl = make_bgl(dev, "lege-wgpu-fixed-bgl", &[(1, true), (2, false)]);
        let fixed_pipeline =
            make_pipeline(dev, "lege-wgpu-fixed", &fixed_bgl, &fixed_shader, "main");

        // --- Build padded ---
        let bp_shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-wgpu-build-padded-shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("shaders/adaptive_build_padded.wgsl").into(),
            ),
        });
        let build_padded_bgl = make_bgl(
            dev,
            "lege-wgpu-build-padded-bgl",
            &[(1, true), (2, false), (3, false)],
        );
        let build_padded_pipeline = make_pipeline(
            dev,
            "lege-wgpu-build-padded",
            &build_padded_bgl,
            &bp_shader,
            "main",
        );

        // --- Integral rows ---
        let ir_shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-wgpu-integral-rows-shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("shaders/adaptive_integral_rows.wgsl").into(),
            ),
        });
        let integral_rows_bgl = make_bgl(
            dev,
            "lege-wgpu-integral-rows-bgl",
            &[(1, true), (2, true), (3, false), (4, false)],
        );
        let integral_rows_pipeline = make_pipeline(
            dev,
            "lege-wgpu-integral-rows",
            &integral_rows_bgl,
            &ir_shader,
            "main",
        );

        // --- Integral cols ---
        let ic_shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-wgpu-integral-cols-shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("shaders/adaptive_integral_cols.wgsl").into(),
            ),
        });
        let integral_cols_bgl = make_bgl(
            dev,
            "lege-wgpu-integral-cols-bgl",
            &[(1, true), (2, true), (3, false), (4, false)],
        );
        let integral_cols_pipeline = make_pipeline(
            dev,
            "lege-wgpu-integral-cols",
            &integral_cols_bgl,
            &ic_shader,
            "main",
        );

        // --- Background max (shared shader, two entry points) ---
        let bg_shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-wgpu-bg-max-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/adaptive_bg_max.wgsl").into()),
        });
        let bg_max_bgl = make_bgl(dev, "lege-wgpu-bg-max-bgl", &[(1, true), (2, false)]);
        let bg_max_h_pipeline =
            make_pipeline(dev, "lege-wgpu-bg-max-h", &bg_max_bgl, &bg_shader, "main_h");
        let bg_max_v_pipeline =
            make_pipeline(dev, "lege-wgpu-bg-max-v", &bg_max_bgl, &bg_shader, "main_v");

        // --- Final adaptive pass ---
        let fin_shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-wgpu-final-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/adaptive_final.wgsl").into()),
        });
        let final_bgl = make_bgl(
            dev,
            "lege-wgpu-final-bgl",
            &[(1, true), (2, true), (3, true), (4, true), (5, false)],
        );
        let final_pipeline = make_pipeline(dev, "lege-wgpu-final", &final_bgl, &fin_shader, "main");

        // --- Pack output pass ---
        let pack_shader = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-wgpu-pack-shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("shaders/adaptive_pack_output.wgsl").into(),
            ),
        });
        let pack_bgl = make_bgl(dev, "lege-wgpu-pack-bgl", &[(1, true), (2, false)]);
        let pack_pipeline = make_pipeline(dev, "lege-wgpu-pack", &pack_bgl, &pack_shader, "main");

        Ok(Self {
            ctx,
            srgb_lut,
            linearize_pipeline,
            linearize_bgl,
            fixed_pipeline,
            fixed_bgl,
            build_padded_pipeline,
            build_padded_bgl,
            integral_rows_pipeline,
            integral_rows_bgl,
            integral_cols_pipeline,
            integral_cols_bgl,
            bg_max_h_pipeline,
            bg_max_v_pipeline,
            bg_max_bgl,
            final_pipeline,
            final_bgl,
            pack_pipeline,
            pack_bgl,
            buffers: None,
            readback: None,
            readback_cap: 0,
            upload_staging: Vec::new(),
        })
    }

    /// Build RGBA bytes from input pixels and upload to gpu_src.
    /// Reuses `self.upload_staging` to avoid per-call allocation.
    fn upload_source_pixels(&mut self, pixels: &[u8], pixel_count: usize, fmt: SourceFormat) {
        let rgba_bytes = pixel_count * 4;
        if self.upload_staging.len() < rgba_bytes {
            self.upload_staging.resize(rgba_bytes, 0);
        }
        let dst = &mut self.upload_staging[..rgba_bytes];
        match fmt {
            SourceFormat::Gray => {
                // Splat gray → (g, g, g, 0). Linearize round-trips gray to gray ± 1 LSB.
                dst.chunks_exact_mut(4)
                    .zip(pixels.iter())
                    .for_each(|(out, &g)| {
                        out[0] = g;
                        out[1] = g;
                        out[2] = g;
                        out[3] = 0;
                    });
            }
            SourceFormat::Rgb => {
                dst.chunks_exact_mut(4)
                    .zip(pixels.chunks_exact(3))
                    .for_each(|(out, p)| {
                        out[0] = p[0];
                        out[1] = p[1];
                        out[2] = p[2];
                        out[3] = 0;
                    });
            }
        }
        let bufs = self.buffers.as_ref().unwrap();
        self.ctx.queue.write_buffer(&bufs.gpu_src, 0, dst);
    }

    pub fn binarize_gray_raw(
        &mut self,
        gray: &[u8],
        params: &BinarizationParams,
    ) -> Result<Vec<u8>> {
        params.validate_gray(gray)?;
        match params.mode {
            BinarizationMode::FixedThreshold => self.fixed_threshold(gray, params),
            BinarizationMode::Adaptive => self.adaptive(gray, params),
        }
    }

    /// Binarize with a callback that receives the mapped data directly, avoiding an extra copy.
    pub fn binarize_gray_raw_with<F, R>(
        &mut self,
        gray: &[u8],
        params: &BinarizationParams,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        params.validate_gray(gray)?;
        match params.mode {
            BinarizationMode::FixedThreshold => self.fixed_threshold_with(gray, params, f),
            BinarizationMode::Adaptive => self.adaptive_with(gray, params, f),
        }
    }

    /// Binarize from RGB bytes (3 bytes/pixel). The GPU linearize pre-pass applies an
    /// sRGB→linear LUT, computes BT.709 luma in linear space, and re-encodes to sRGB
    /// gray — preserving perceptual contrast that integer luma drops.
    pub fn binarize_rgb_raw_with<F, R>(
        &mut self,
        rgb: &[u8],
        params: &BinarizationParams,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let expected = params.width as usize * params.height as usize * 3;
        if rgb.len() != expected {
            return Err(GpuBinarizationError::BufferSizeMismatch {
                expected,
                actual: rgb.len(),
            });
        }
        match params.mode {
            BinarizationMode::FixedThreshold => self.fixed_threshold_rgb_with(rgb, params, f),
            BinarizationMode::Adaptive => self.adaptive_rgb_with(rgb, params, f),
        }
    }

    fn fixed_threshold_rgb_with<F, R>(
        &mut self,
        rgb: &[u8],
        params: &BinarizationParams,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let pixel_count = params.width as usize * params.height as usize;
        self.ensure_buffers(pixel_count, 0, 0)?;
        let gpu_params = self.build_params(params, 0, 0, 0, 0);
        {
            let bufs = self.buffers.as_ref().unwrap();
            self.ctx
                .queue
                .write_buffer(&bufs.params, 0, bytemuck::bytes_of(&gpu_params));
        }
        self.upload_source_pixels(rgb, pixel_count, SourceFormat::Rgb);
        let mut enc = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lege-wgpu-fixed-rgb-enc"),
            });
        self.encode_fixed_passes(&mut enc, params);
        self.with_readback_data(enc, pixel_count, f)
    }

    fn adaptive_rgb_with<F, R>(
        &mut self,
        rgb: &[u8],
        params: &BinarizationParams,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let w = params.width as usize;
        let h = params.height as usize;
        let radius = (params.adaptive.sauvola_window / 2) as usize;
        let pw = w + 2 * radius;
        let ph = h + 2 * radius;
        let iw = pw + 1;
        let ih = ph + 1;
        if (pw as u64) * (ph as u64) * 255 > u32::MAX as u64 {
            return Err(GpuBinarizationError::Unsupported(
                "image too large for GPU adaptive binarization (integral would overflow u32)"
                    .into(),
            ));
        }
        let pixel_count = w * h;
        let padded_bytes = pw * ph * size_of::<u32>();
        let integral_bytes = iw * ih * size_of::<u32>();
        self.ensure_buffers(pixel_count, padded_bytes, integral_bytes)?;
        let gpu_params = self.build_params(params, pw as u32, ph as u32, iw as u32, radius as u32);
        {
            let bufs = self.buffers.as_ref().unwrap();
            self.ctx
                .queue
                .write_buffer(&bufs.params, 0, bytemuck::bytes_of(&gpu_params));
        }
        self.upload_source_pixels(rgb, pixel_count, SourceFormat::Rgb);
        let mut enc = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lege-wgpu-adaptive-rgb-enc"),
            });
        self.encode_adaptive_passes(&mut enc, params, pw, ph, iw);
        self.with_readback_data(enc, pixel_count, f)
    }

    // -----------------------------------------------------------------------
    // Batch binarization: N pages, one poll, one map.
    //
    // Pages may have different sizes. Work buffers are sized to max page in the
    // batch; the readback buffer grows to hold all packed results.
    // wgpu flushes staged write_buffer data before each encoder's commands, so
    // sequential write+submit pairs correctly deliver per-page params/src even
    // though the same GPU buffer is reused.
    // -----------------------------------------------------------------------

    pub fn binarize_batch(
        &mut self,
        pages: &[(&[u8], &BinarizationParams)],
    ) -> Result<Vec<Vec<u8>>> {
        if pages.is_empty() {
            return Ok(vec![]);
        }

        // --- Pass 1: validate + compute buffer sizes and per-page readback offsets ---
        let mut max_img_bytes = 0usize;
        let mut max_padded_bytes = 0usize;
        let mut max_integral_bytes = 0usize;
        let mut rb_offsets: Vec<usize> = Vec::with_capacity(pages.len());
        let mut rb_cursor = 0usize;

        for &(gray, params) in pages {
            params.validate_gray(gray)?;
            let px = params.width as usize * params.height as usize;
            max_img_bytes = max_img_bytes.max(px); // u8 data: one byte per pixel

            if params.mode == BinarizationMode::Adaptive {
                let r = (params.adaptive.sauvola_window / 2) as usize;
                let pw = params.width as usize + 2 * r;
                let ph = params.height as usize + 2 * r;
                if (pw as u64) * (ph as u64) * 255 > u32::MAX as u64 {
                    return Err(GpuBinarizationError::Unsupported(
                        "image too large for GPU adaptive binarization (integral overflow)".into(),
                    ));
                }
                max_padded_bytes = max_padded_bytes.max(pw * ph * size_of::<u32>());
                let iw = pw + 1;
                let ih = ph + 1;
                max_integral_bytes = max_integral_bytes.max(iw * ih * size_of::<u32>());
            }

            rb_offsets.push(rb_cursor);
            rb_cursor += px.div_ceil(4) * size_of::<u32>();
        }
        let total_rb = rb_cursor;

        // --- Pass 2: allocate / grow buffers ---
        self.ensure_buffers(max_img_bytes, max_padded_bytes, max_integral_bytes)?;
        self.ensure_readback(total_rb);

        // --- Pass 3: per-page write → encode → submit (no poll yet) ---
        for (i, &(gray, params)) in pages.iter().enumerate() {
            let pixel_count = params.width as usize * params.height as usize;
            let packed_words = pixel_count.div_ceil(4) as u32;
            let packed_bytes = packed_words as usize * size_of::<u32>();
            let rb_offset = rb_offsets[i] as u64;

            match params.mode {
                BinarizationMode::FixedThreshold => {
                    let gpu_params = self.build_params(params, 0, 0, 0, 0);
                    {
                        let bufs = self.buffers.as_ref().unwrap();
                        self.ctx.queue.write_buffer(
                            &bufs.params,
                            0,
                            bytemuck::bytes_of(&gpu_params),
                        );
                    }
                    self.upload_source_pixels(gray, pixel_count, SourceFormat::Gray);

                    let mut enc = self
                        .ctx
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                    self.encode_fixed_passes(&mut enc, params);
                    self.encode_pack_copy_submit(enc, packed_words, packed_bytes, rb_offset);
                }
                BinarizationMode::Adaptive => {
                    let r = (params.adaptive.sauvola_window / 2) as usize;
                    let pw = params.width as usize + 2 * r;
                    let ph = params.height as usize + 2 * r;
                    let iw = pw + 1;

                    let gpu_params =
                        self.build_params(params, pw as u32, ph as u32, iw as u32, r as u32);
                    {
                        let bufs = self.buffers.as_ref().unwrap();
                        self.ctx.queue.write_buffer(
                            &bufs.params,
                            0,
                            bytemuck::bytes_of(&gpu_params),
                        );
                    }
                    self.upload_source_pixels(gray, pixel_count, SourceFormat::Gray);

                    let mut enc = self
                        .ctx
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                    self.encode_adaptive_passes(&mut enc, params, pw, ph, iw);
                    self.encode_pack_copy_submit(enc, packed_words, packed_bytes, rb_offset);
                }
            }
        }

        // --- Pass 4: single poll for all N page submissions ---
        self.ctx
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GpuBinarizationError::Execution(format!("device poll failed: {e:?}")))?;

        // --- Pass 5: single map of the full readback buffer ---
        let rb = self.readback.as_ref().unwrap();
        let slice = rb.slice(0..total_rb as u64);
        let (tx, rx) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.ctx
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| {
                GpuBinarizationError::Execution(format!("device poll (map) failed: {e:?}"))
            })?;
        rx.recv()
            .map_err(|_| GpuBinarizationError::Execution("map_async channel failed".into()))?
            .map_err(|e| GpuBinarizationError::Execution(format!("map_async failed: {e:?}")))?;

        let data = slice.get_mapped_range();
        let results: Vec<Vec<u8>> = pages
            .iter()
            .enumerate()
            .map(|(i, &(_, params))| {
                let px = params.width as usize * params.height as usize;
                let packed_bytes = px.div_ceil(4) * size_of::<u32>();
                let start = rb_offsets[i];
                let mut page_data = data[start..start + packed_bytes].to_vec();
                page_data.truncate(px);
                page_data
            })
            .collect();
        drop(data);
        rb.unmap();

        Ok(results)
    }

    // -----------------------------------------------------------------------
    // Single-page paths (thin wrappers around the shared encoding helpers).
    // -----------------------------------------------------------------------

    fn fixed_threshold(&mut self, gray: &[u8], params: &BinarizationParams) -> Result<Vec<u8>> {
        self.fixed_threshold_with(gray, params, |data| {
            let mut out = data.to_vec();
            out.truncate(params.width as usize * params.height as usize);
            out
        })
    }

    fn fixed_threshold_with<F, R>(
        &mut self,
        gray: &[u8],
        params: &BinarizationParams,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let pixel_count = params.width as usize * params.height as usize;
        let img_bytes = pixel_count; // u8 data: one byte per pixel

        self.ensure_buffers(img_bytes, 0, 0)?;

        let gpu_params = self.build_params(params, 0, 0, 0, 0);
        {
            let bufs = self.buffers.as_ref().unwrap();
            self.ctx
                .queue
                .write_buffer(&bufs.params, 0, bytemuck::bytes_of(&gpu_params));
        }
        self.upload_source_pixels(gray, pixel_count, SourceFormat::Gray);

        let mut enc = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lege-wgpu-fixed-enc"),
            });
        self.encode_fixed_passes(&mut enc, params);
        self.with_readback_data(enc, pixel_count, f)
    }

    fn adaptive(&mut self, gray: &[u8], params: &BinarizationParams) -> Result<Vec<u8>> {
        self.adaptive_with(gray, params, |data| {
            let mut out = data.to_vec();
            out.truncate(params.width as usize * params.height as usize);
            out
        })
    }

    fn adaptive_with<F, R>(&mut self, gray: &[u8], params: &BinarizationParams, f: F) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let w = params.width as usize;
        let h = params.height as usize;
        let radius = (params.adaptive.sauvola_window / 2) as usize;
        let pw = w + 2 * radius;
        let ph = h + 2 * radius;
        let iw = pw + 1;
        let ih = ph + 1;

        if (pw as u64) * (ph as u64) * 255 > u32::MAX as u64 {
            return Err(GpuBinarizationError::Unsupported(
                "image too large for GPU adaptive binarization (integral would overflow u32)"
                    .into(),
            ));
        }

        let pixel_count = w * h;
        let img_bytes = pixel_count; // u8 data: one byte per pixel
        let padded_bytes = pw * ph * size_of::<u32>();
        let integral_bytes = iw * ih * size_of::<u32>();

        self.ensure_buffers(img_bytes, padded_bytes, integral_bytes)?;

        let gpu_params = self.build_params(params, pw as u32, ph as u32, iw as u32, radius as u32);
        {
            let bufs = self.buffers.as_ref().unwrap();
            self.ctx
                .queue
                .write_buffer(&bufs.params, 0, bytemuck::bytes_of(&gpu_params));
        }
        self.upload_source_pixels(gray, pixel_count, SourceFormat::Gray);

        let mut enc = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lege-wgpu-adaptive-enc"),
            });
        self.encode_adaptive_passes(&mut enc, params, pw, ph, iw);
        self.with_readback_data(enc, pixel_count, f)
    }

    // -----------------------------------------------------------------------
    // Pass-encoding helpers — take &self so they can be called while buffers
    // are also borrowed, and produce no side effects beyond filling the encoder.
    // -----------------------------------------------------------------------

    // Pre-pass: read RGBA from gpu_src, write sRGB-perceptual gray to gray_buf.
    // Must run before any pass that reads gray_buf (build_padded, bg_max_h, final, fixed).
    fn encode_linearize_pass(&self, enc: &mut wgpu::CommandEncoder, params: &BinarizationParams) {
        let bufs = self.buffers.as_ref().unwrap();
        let bg = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.linearize_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: bufs.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: bufs.gpu_src.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: bufs.gray_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.srgb_lut.as_entire_binding(),
                    },
                ],
            });
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.linearize_pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(params.width.div_ceil(16), params.height.div_ceil(16), 1);
    }

    fn encode_fixed_passes(&self, enc: &mut wgpu::CommandEncoder, params: &BinarizationParams) {
        self.encode_linearize_pass(enc, params);
        let bufs = self.buffers.as_ref().unwrap();
        let bg = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.fixed_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: bufs.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: bufs.gray_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: bufs.gpu_dst.as_entire_binding(),
                    },
                ],
            });
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.fixed_pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(params.width.div_ceil(16), params.height.div_ceil(16), 1);
    }

    fn encode_adaptive_passes(
        &self,
        enc: &mut wgpu::CommandEncoder,
        params: &BinarizationParams,
        pw: usize,
        ph: usize,
        iw: usize,
    ) {
        // Pass 0: RGBA → linearize → sRGB gray (gray_buf), consumed by all passes below.
        self.encode_linearize_pass(enc, params);

        let bufs = self.buffers.as_ref().unwrap();
        let cx_w = params.width.div_ceil(16);
        let cx_h = params.height.div_ceil(16);
        let cpw = (pw as u32).div_ceil(16);
        let cph = (ph as u32).div_ceil(16);

        // Pass 1: build reflected-padded image (reads gray_buf produced by linearize).
        {
            let bg = self
                .ctx
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.build_padded_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: bufs.params.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: bufs.gray_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: bufs.padded_gray.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: bufs.padded_sq.as_entire_binding(),
                        },
                    ],
                });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.build_padded_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(cpw, cph, 1);
        }

        // Pass 2: horizontal integral prefix sums.
        {
            let bg = self
                .ctx
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.integral_rows_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: bufs.params.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: bufs.padded_gray.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: bufs.padded_sq.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: bufs.row_prefix.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: bufs.row_prefix_sq.as_entire_binding(),
                        },
                    ],
                });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.integral_rows_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(1, (ph as u32).div_ceil(64), 1);
        }

        // Pass 3: vertical accumulation to complete 2-D integrals.
        {
            let bg = self
                .ctx
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.integral_cols_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: bufs.params.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: bufs.row_prefix.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: bufs.row_prefix_sq.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: bufs.integral.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: bufs.integral_sq.as_entire_binding(),
                        },
                    ],
                });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.integral_cols_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups((iw as u32).div_ceil(64), 1, 1);
        }

        // Pass 4: background max filter — horizontal (reads gray_buf, not gpu_src).
        {
            let bg = self
                .ctx
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.bg_max_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: bufs.params.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: bufs.gray_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: bufs.bg_tmp.as_entire_binding(),
                        },
                    ],
                });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.bg_max_h_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(cx_w, cx_h, 1);
        }

        // Pass 5: background max filter — vertical.
        {
            let bg = self
                .ctx
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.bg_max_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: bufs.params.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: bufs.bg_tmp.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: bufs.bg.as_entire_binding(),
                        },
                    ],
                });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.bg_max_v_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(cx_w, cx_h, 1);
        }

        // Pass 6: final — Sauvola + Otsu + AND fusion (reads gray_buf for source pixel).
        {
            let bg = self
                .ctx
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.final_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: bufs.params.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: bufs.gray_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: bufs.integral.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: bufs.integral_sq.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: bufs.bg.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 5,
                            resource: bufs.gpu_dst.as_entire_binding(),
                        },
                    ],
                });
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.final_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(cx_w, cx_h, 1);
        }
    }

    // Encode the pack pass, copy packed_dst → readback[rb_offset..], and submit.
    // Caller must call ensure_readback(rb_offset + packed_bytes) before this.
    fn encode_pack_copy_submit(
        &self,
        enc: wgpu::CommandEncoder,
        packed_words: u32,
        packed_bytes: usize,
        rb_offset: u64,
    ) {
        let bufs = self.buffers.as_ref().unwrap();
        let rb = self.readback.as_ref().unwrap();

        let pack_bg = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.pack_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: bufs.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: bufs.gpu_dst.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: bufs.packed_dst.as_entire_binding(),
                    },
                ],
            });

        let mut enc = enc;
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pack_pipeline);
            pass.set_bind_group(0, &pack_bg, &[]);
            pass.dispatch_workgroups(packed_words.div_ceil(256), 1, 1);
        }
        enc.copy_buffer_to_buffer(&bufs.packed_dst, 0, rb, rb_offset, packed_bytes as u64);
        self.ctx.queue.submit(Some(enc.finish()));
    }

    // Submit, poll, map readback[0..packed_bytes], process with callback.
    // The callback receives the mapped data directly, avoiding an extra copy.
    fn with_readback_data<F, R>(
        &mut self,
        enc: wgpu::CommandEncoder,
        pixel_count: usize,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let packed_words = pixel_count.div_ceil(4) as u32;
        let packed_bytes = packed_words as usize * size_of::<u32>();

        self.ensure_readback(packed_bytes);
        self.encode_pack_copy_submit(enc, packed_words, packed_bytes, 0);

        let rb = self.readback.as_ref().unwrap();
        let slice = rb.slice(0..packed_bytes as u64);
        let (tx, rx) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.ctx
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GpuBinarizationError::Execution(format!("device poll failed: {e:?}")))?;
        rx.recv()
            .map_err(|_| GpuBinarizationError::Execution("map_async channel failed".into()))?
            .map_err(|e| GpuBinarizationError::Execution(format!("map_async failed: {e:?}")))?;
        let data = slice.get_mapped_range();
        let result = f(&data[..packed_bytes.min(data.len())]);
        drop(data);
        rb.unmap();
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Buffer management
    // -----------------------------------------------------------------------

    fn build_params(
        &self,
        p: &BinarizationParams,
        padded_width: u32,
        padded_height: u32,
        integral_width: u32,
        sauvola_radius: u32,
    ) -> BinarizeParamsStd140 {
        BinarizeParamsStd140 {
            width: p.width,
            height: p.height,
            mode: match p.mode {
                BinarizationMode::FixedThreshold => 0,
                BinarizationMode::Adaptive => 1,
            },
            invert_output: p.invert_output as u32,
            fixed_threshold: p.fixed_threshold as u32,
            sauvola_window: p.adaptive.sauvola_window,
            bg_window: p.adaptive.bg_window,
            otsu_threshold: p.adaptive.otsu_threshold as u32,
            k_factor: p.k_factor,
            percentile_c: p.adaptive.percentile_c as f32,
            padded_width,
            padded_height,
            integral_width,
            sauvola_radius,
            debug_mode: p.debug_mode,
            _pad2: 0,
            _pad3: 0,
            _pad4: 0,
            _pad5: 0,
            _pad6: 0,
            _pad7: 0,
        }
    }

    fn ensure_buffers(
        &mut self,
        img_bytes: usize, // pixel_count: one u8 per pixel (packed 4-per-u32 in gpu_src)
        padded_bytes: usize,
        integral_bytes: usize,
    ) -> Result<()> {
        let pixel_count = img_bytes;
        // All image-sized buffers are 1 u32 per pixel (gpu_src is RGBA packed; gray_buf,
        // gpu_dst, bg_tmp, bg are 1 byte of value per u32). pixel_count * 4 bytes each.
        let img_al = align_up((pixel_count * size_of::<u32>()).max(1), 256);
        let packed_bytes = pixel_count.div_ceil(4) * size_of::<u32>();
        let packed_al = align_up(packed_bytes.max(1), 256);
        let pad_al = align_up(padded_bytes.max(1), 256);
        let int_al = align_up(integral_bytes.max(1), 256);
        let par_al = align_up(size_of::<BinarizeParamsStd140>(), 256);

        let needs_new = match &self.buffers {
            Some(b) => b.img_bytes < img_al || b.padded_bytes < pad_al || b.integral_bytes < int_al,
            None => true,
        };
        if !needs_new {
            return Ok(());
        }

        let dev = &self.ctx.device;

        let mk = |label: &str, size: usize, usage: wgpu::BufferUsages| {
            dev.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size as u64,
                usage,
                mapped_at_creation: false,
            })
        };

        let src_usage = wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE;
        let dst_usage = wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::STORAGE;
        let rw_usage = wgpu::BufferUsages::STORAGE;

        self.buffers = Some(WgpuBuffers {
            params: mk(
                "lege-wgpu-binarize-params",
                par_al,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            ),
            gpu_src: mk("lege-wgpu-binarize-src", img_al, src_usage),
            gray_buf: mk("lege-wgpu-binarize-gray", img_al, rw_usage),
            gpu_dst: mk("lege-wgpu-binarize-dst", img_al, dst_usage),
            packed_dst: mk("lege-wgpu-binarize-packed-dst", packed_al, dst_usage),
            bg_tmp: mk("lege-wgpu-binarize-bg-tmp", img_al, rw_usage),
            bg: mk("lege-wgpu-binarize-bg", img_al, rw_usage),
            padded_gray: mk("lege-wgpu-binarize-padded-gray", pad_al, rw_usage),
            padded_sq: mk("lege-wgpu-binarize-padded-sq", pad_al, rw_usage),
            row_prefix: mk("lege-wgpu-binarize-row-pfx", int_al, rw_usage),
            row_prefix_sq: mk("lege-wgpu-binarize-row-pfx-sq", int_al, rw_usage),
            integral: mk("lege-wgpu-binarize-integral", int_al, rw_usage),
            integral_sq: mk("lege-wgpu-binarize-integral-sq", int_al, rw_usage),
            img_bytes: img_al,
            padded_bytes: pad_al,
            integral_bytes: int_al,
        });
        Ok(())
    }

    fn ensure_readback(&mut self, needed: usize) {
        let al = align_up(needed.max(1), 256);
        if self.readback_cap >= al {
            return;
        }
        self.readback = Some(self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-binarize-readback"),
            size: al as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        }));
        self.readback_cap = al;
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}
