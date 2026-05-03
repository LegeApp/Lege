use std::collections::HashMap;
use std::mem::size_of;
use std::sync::mpsc;

use bytemuck::{Pod, Zeroable};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterType {
    Bilinear,
    Bell,
    Lanczos3,
}

impl FilterType {
    fn shader_source(self) -> &'static str {
        match self {
            Self::Bilinear => include_str!("wgpu/shaders/bilinear_resize.wgsl"),
            Self::Bell => include_str!("wgpu/shaders/bell_resize.wgsl"),
            Self::Lanczos3 => include_str!("wgpu/shaders/lanczos3_resize.wgsl"),
        }
    }

    fn thread_group_size(self) -> (u32, u32, u32) {
        match self {
            Self::Bilinear => (16, 16, 1),
            Self::Bell => (16, 16, 1),
            Self::Lanczos3 => (8, 8, 1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderMode {
    Clamp = 0,
    Reflect = 1,
    Wrap = 2,
    Constant = 3,
}

impl Default for BorderMode {
    fn default() -> Self {
        Self::Clamp
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseCase {
    PaddleX,
    OCR,
    MarginCalculation,
}

impl UseCase {
    pub fn default_parameters(self, src_width: u32, src_height: u32) -> ResizeParameters {
        match self {
            Self::PaddleX => ResizeParameters {
                src_width,
                src_height,
                dst_width: 640,
                dst_height: 640,
                filter: FilterType::Bilinear,
                border_mode: BorderMode::Clamp,
                border_value: 0.0,
                channel_count: 3,
                no_srgb: false,
            },
            Self::OCR => {
                let aspect_ratio = src_height as f32 / src_width as f32;
                let dst_width = 1200;
                let dst_height = (dst_width as f32 * aspect_ratio).round() as u32;
                ResizeParameters {
                    src_width,
                    src_height,
                    dst_width,
                    dst_height,
                    filter: FilterType::Bell,
                    border_mode: BorderMode::Clamp,
                    border_value: 0.0,
                    channel_count: 3,
                    no_srgb: false,
                }
            }
            Self::MarginCalculation => {
                let aspect_ratio = src_height as f32 / src_width as f32;
                let dst_width = 1024;
                let dst_height = (dst_width as f32 * aspect_ratio).round() as u32;
                ResizeParameters {
                    src_width,
                    src_height,
                    dst_width,
                    dst_height,
                    filter: FilterType::Bilinear,
                    border_mode: BorderMode::Clamp,
                    border_value: 0.0,
                    channel_count: 3,
                    no_srgb: false,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversionType {
    Rgba8ToNchwF32,
}

impl ConversionType {
    fn shader_source(self) -> &'static str {
        match self {
            Self::Rgba8ToNchwF32 => include_str!("wgpu/shaders/rgba8_to_nchw_f32.wgsl"),
        }
    }
}

#[derive(Debug)]
pub enum WgpuResizeError {
    InvalidDimensions { width: u32, height: u32 },
    InvalidChannelCount(u32),
    BufferSizeMismatch { expected: usize, actual: usize },
    Initialization(String),
    Shader(String),
    Execution(String),
}

impl std::fmt::Display for WgpuResizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDimensions { width, height } => {
                write!(f, "Invalid dimensions: {}x{}", width, height)
            }
            Self::InvalidChannelCount(ch) => write!(f, "Invalid channel count: {}", ch),
            Self::BufferSizeMismatch { expected, actual } => {
                write!(
                    f,
                    "Buffer size mismatch: expected {}, got {}",
                    expected, actual
                )
            }
            Self::Initialization(msg) => write!(f, "Initialization failed: {}", msg),
            Self::Shader(msg) => write!(f, "Shader error: {}", msg),
            Self::Execution(msg) => write!(f, "Execution failed: {}", msg),
        }
    }
}

impl std::error::Error for WgpuResizeError {}

type Result<T> = std::result::Result<T, WgpuResizeError>;

#[derive(Debug, Clone)]
pub struct ResizeParameters {
    pub src_width: u32,
    pub src_height: u32,
    pub dst_width: u32,
    pub dst_height: u32,
    pub filter: FilterType,
    pub border_mode: BorderMode,
    pub border_value: f32,
    pub channel_count: u32,
    pub no_srgb: bool,
}

impl ResizeParameters {
    pub fn new(src_width: u32, src_height: u32, dst_width: u32, dst_height: u32) -> Self {
        Self {
            src_width,
            src_height,
            dst_width,
            dst_height,
            filter: FilterType::Bilinear,
            border_mode: BorderMode::Clamp,
            border_value: 0.0,
            channel_count: 3,
            no_srgb: false,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.src_width == 0 || self.src_height == 0 {
            return Err(WgpuResizeError::InvalidDimensions {
                width: self.src_width,
                height: self.src_height,
            });
        }
        if self.dst_width == 0 || self.dst_height == 0 {
            return Err(WgpuResizeError::InvalidDimensions {
                width: self.dst_width,
                height: self.dst_height,
            });
        }
        if !matches!(self.channel_count, 1 | 3 | 4) {
            return Err(WgpuResizeError::InvalidChannelCount(self.channel_count));
        }
        Ok(())
    }

    fn src_buffer_size(&self) -> usize {
        (self.src_width * self.src_height * self.channel_count) as usize
    }

    fn dst_buffer_size(&self) -> usize {
        (self.dst_width * self.dst_height * self.channel_count) as usize
    }
}

#[derive(Clone)]
pub struct WgpuResizeOutput {
    pub buffer: wgpu::Buffer,
    pub width: u32,
    pub height: u32,
    pub channel_count: u32,
    pub bytes_per_pixel: u32,
    pub size_in_bytes: usize,
}

struct WgpuContext {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

struct WgpuPipelineState {
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
}

struct WgpuResizeBuffers {
    gpu_src: wgpu::Buffer,
    gpu_dst: wgpu::Buffer,
    readback: wgpu::Buffer,
    params: wgpu::Buffer,
    src_capacity: usize,
    dst_capacity: usize,
}

struct WgpuConvertBuffers {
    gpu_dst_nchw: wgpu::Buffer,
    readback: wgpu::Buffer,
    params: wgpu::Buffer,
    dst_capacity_bytes: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ResizeParamsStd140 {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    scale_x: f32,
    scale_y: f32,
    offset_x: f32,
    offset_y: f32,
    border_mode: u32,
    border_value: f32,
    channel_count: u32,
    no_srgb: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ConvertParamsStd140 {
    width: u32,
    height: u32,
    batch_size: u32,
    _pad: u32,
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn expand_to_rgba(src_data: &[u8], channel_count: u32, pixel_count: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    match channel_count {
        4 => rgba.extend_from_slice(src_data),
        3 => {
            for px in src_data.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        1 => {
            for &g in src_data {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        _ => unreachable!("Unsupported channel count"),
    }
    rgba
}

fn compact_from_rgba(gpu_out: &[u8], channel_count: u32, dst_size: usize) -> Vec<u8> {
    let mut out = vec![0u8; dst_size];
    match channel_count {
        4 => out.copy_from_slice(gpu_out),
        3 => {
            for (i, dst_px) in out.chunks_exact_mut(3).enumerate() {
                let s = i * 4;
                dst_px[0] = gpu_out[s];
                dst_px[1] = gpu_out[s + 1];
                dst_px[2] = gpu_out[s + 2];
            }
        }
        1 => {
            for (i, dst_px) in out.iter_mut().enumerate() {
                *dst_px = gpu_out[i * 4];
            }
        }
        _ => unreachable!("Unsupported channel count"),
    }
    out
}

impl WgpuContext {
    async fn new_async(_verbose: bool) -> Result<Self> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| {
                WgpuResizeError::Initialization(format!("request_adapter failed: {e:?}"))
            })?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("lege-wgpu-resize-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
                ..Default::default()
            })
            .await
            .map_err(|e| WgpuResizeError::Initialization(format!("request_device failed: {e}")))?;

        Ok(Self {
            _instance: instance,
            _adapter: adapter,
            device,
            queue,
        })
    }

    fn new(verbose: bool) -> Result<Self> {
        pollster::block_on(Self::new_async(verbose))
    }
}

pub struct WgpuResizer {
    ctx: WgpuContext,
    resize_pipelines: HashMap<FilterType, WgpuPipelineState>,
    resize_buffers: Option<WgpuResizeBuffers>,
    convert_pipelines: HashMap<ConversionType, WgpuPipelineState>,
    convert_buffers: Option<WgpuConvertBuffers>,
    verbose: bool,
}

impl WgpuResizer {
    pub fn new() -> Result<Self> {
        let verbose = std::env::var("LEGE_WGPU_RESIZE_VERBOSE").ok().as_deref() == Some("1");
        let ctx = WgpuContext::new(verbose)?;
        let mut resizer = Self {
            ctx,
            resize_pipelines: HashMap::new(),
            resize_buffers: None,
            convert_pipelines: HashMap::new(),
            convert_buffers: None,
            verbose,
        };
        resizer.initialize_pipelines()?;
        Ok(resizer)
    }

    pub fn resize(&mut self, src_data: &[u8], params: &ResizeParameters) -> Result<Vec<u8>> {
        params.validate()?;

        let src_size = params.src_buffer_size();
        if src_data.len() != src_size {
            return Err(WgpuResizeError::BufferSizeMismatch {
                expected: src_size,
                actual: src_data.len(),
            });
        }

        let pixel_count_src = (params.src_width as usize) * (params.src_height as usize);
        let pixel_count_dst = (params.dst_width as usize) * (params.dst_height as usize);
        let src_rgba = expand_to_rgba(src_data, params.channel_count, pixel_count_src);
        let gpu_src_size = pixel_count_src * 4;
        let gpu_dst_size = pixel_count_dst * 4;

        let gpu_out = self
            .dispatch_resize(params, &src_rgba, gpu_src_size, gpu_dst_size, true)?
            .ok_or_else(|| WgpuResizeError::Execution("missing readback data".to_string()))?;

        Ok(compact_from_rgba(
            &gpu_out,
            params.channel_count,
            params.dst_buffer_size(),
        ))
    }

    pub fn resize_to_gpu(
        &mut self,
        src_data: &[u8],
        params: &ResizeParameters,
    ) -> Result<WgpuResizeOutput> {
        params.validate()?;
        if src_data.len() != params.src_buffer_size() {
            return Err(WgpuResizeError::BufferSizeMismatch {
                expected: params.src_buffer_size(),
                actual: src_data.len(),
            });
        }

        let pixel_count_src = (params.src_width as usize) * (params.src_height as usize);
        let pixel_count_dst = (params.dst_width as usize) * (params.dst_height as usize);
        let src_rgba = expand_to_rgba(src_data, params.channel_count, pixel_count_src);
        let gpu_src_size = pixel_count_src * 4;
        let gpu_dst_size = pixel_count_dst * 4;

        self.dispatch_resize(params, &src_rgba, gpu_src_size, gpu_dst_size, false)?;
        let buffers = self.resize_buffers.as_ref().ok_or_else(|| {
            WgpuResizeError::Execution("resize buffers not initialized".to_string())
        })?;

        Ok(WgpuResizeOutput {
            buffer: buffers.gpu_dst.clone(),
            width: params.dst_width,
            height: params.dst_height,
            channel_count: 4,
            bytes_per_pixel: 4,
            size_in_bytes: gpu_dst_size,
        })
    }

    pub fn convert_to_nchw_f32_on_gpu(
        &mut self,
        input: &WgpuResizeOutput,
        batch_size: usize,
    ) -> Result<wgpu::Buffer> {
        if input.channel_count != 4 || input.bytes_per_pixel != 4 {
            return Err(WgpuResizeError::Execution(
                "WGPU conversion expects RGBA8 GPU resize output".to_string(),
            ));
        }

        self.ensure_convert_pipeline(ConversionType::Rgba8ToNchwF32)?;
        let dst_bytes =
            batch_size * 3 * input.width as usize * input.height as usize * size_of::<f32>();
        self.ensure_convert_buffers(dst_bytes)?;

        let buffers = self.convert_buffers.as_ref().ok_or_else(|| {
            WgpuResizeError::Execution("converter buffers not initialized".to_string())
        })?;
        let pipeline = self
            .convert_pipelines
            .get(&ConversionType::Rgba8ToNchwF32)
            .ok_or_else(|| WgpuResizeError::Shader("converter pipeline cache miss".to_string()))?;

        let convert_params = ConvertParamsStd140 {
            width: input.width,
            height: input.height,
            batch_size: batch_size as u32,
            _pad: 0,
        };
        self.ctx
            .queue
            .write_buffer(&buffers.params, 0, bytemuck::bytes_of(&convert_params));

        let bind_group = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lege-wgpu-convert-bind-group"),
                layout: &pipeline.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffers.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: input.buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: buffers.gpu_dst_nchw.as_entire_binding(),
                    },
                ],
            });

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lege-wgpu-convert-encoder"),
            });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("lege-wgpu-convert-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            let groups_x = input.width.div_ceil(16);
            let groups_y = input.height.div_ceil(16);
            pass.dispatch_workgroups(groups_x, groups_y, batch_size as u32);
        }

        self.ctx.queue.submit(Some(encoder.finish()));

        if self.verbose {
            log::debug!(
                "WGPU RGBA8->NCHW conversion dispatched: {}x{} batch {}",
                input.width,
                input.height,
                batch_size
            );
        }

        Ok(buffers.gpu_dst_nchw.clone())
    }

    pub fn convert_to_nchw_f32_then_readback(
        &mut self,
        input: &WgpuResizeOutput,
        batch_size: usize,
    ) -> Result<Vec<f32>> {
        let _output = self.convert_to_nchw_f32_on_gpu(input, batch_size)?;
        let buffers = self.convert_buffers.as_ref().ok_or_else(|| {
            WgpuResizeError::Execution("converter buffers not initialized".to_string())
        })?;
        let dst_bytes =
            batch_size * 3 * input.width as usize * input.height as usize * size_of::<f32>();

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lege-wgpu-convert-readback-encoder"),
            });
        encoder.copy_buffer_to_buffer(
            &buffers.gpu_dst_nchw,
            0,
            &buffers.readback,
            0,
            dst_bytes as u64,
        );
        self.ctx.queue.submit(Some(encoder.finish()));

        let bytes = self.read_mapped_bytes(&buffers.readback, dst_bytes)?;
        let mut out = Vec::with_capacity(bytes.len() / size_of::<f32>());
        for chunk in bytes.chunks_exact(size_of::<f32>()) {
            out.push(f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        Ok(out)
    }

    fn initialize_pipelines(&mut self) -> Result<()> {
        for filter in [FilterType::Bilinear, FilterType::Bell, FilterType::Lanczos3] {
            self.ensure_pipeline(filter)?;
        }

        for conversion in [ConversionType::Rgba8ToNchwF32] {
            self.ensure_convert_pipeline(conversion)?;
        }

        if self.verbose {
            log::debug!("WGPU resize backend initialized with eager pipeline compilation");
        }

        Ok(())
    }

    fn ensure_pipeline(&mut self, filter: FilterType) -> Result<()> {
        if self.resize_pipelines.contains_key(&filter) {
            return Ok(());
        }

        let module = self
            .ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("lege-wgpu-resize-shader"),
                source: wgpu::ShaderSource::Wgsl(filter.shader_source().into()),
            });

        let bind_group_layout =
            self.ctx
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lege-wgpu-resize-bgl"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage { read_only: true },
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage { read_only: false },
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });

        let pipeline_layout =
            self.ctx
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lege-wgpu-resize-pipeline-layout"),
                    bind_group_layouts: &[&bind_group_layout],
                    immediate_size: 0,
                });

        let pipeline = self
            .ctx
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("lege-wgpu-resize-pipeline"),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            });

        self.resize_pipelines.insert(
            filter,
            WgpuPipelineState {
                pipeline,
                bind_group_layout,
            },
        );
        Ok(())
    }

    fn ensure_buffers(&mut self, src_bytes: usize, dst_bytes: usize) -> Result<()> {
        let src_aligned = align_up(src_bytes, 256);
        let dst_aligned = align_up(dst_bytes, 256);
        let params_aligned = align_up(std::mem::size_of::<ResizeParamsStd140>(), 256);

        let needs_new = match &self.resize_buffers {
            Some(b) => b.src_capacity < src_aligned || b.dst_capacity < dst_aligned,
            None => true,
        };
        if !needs_new {
            return Ok(());
        }

        let device = &self.ctx.device;

        let gpu_src = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-resize-gpu-src"),
            size: src_aligned as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });

        let gpu_dst = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-resize-gpu-dst"),
            size: dst_aligned as u64,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-resize-readback"),
            size: dst_aligned as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-resize-params"),
            size: params_aligned as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.resize_buffers = Some(WgpuResizeBuffers {
            gpu_src,
            gpu_dst,
            readback,
            params,
            src_capacity: src_aligned,
            dst_capacity: dst_aligned,
        });

        Ok(())
    }

    fn ensure_convert_pipeline(&mut self, conversion: ConversionType) -> Result<()> {
        if self.convert_pipelines.contains_key(&conversion) {
            return Ok(());
        }

        let module = self
            .ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("lege-wgpu-convert-shader"),
                source: wgpu::ShaderSource::Wgsl(conversion.shader_source().into()),
            });

        let bind_group_layout =
            self.ctx
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lege-wgpu-convert-bgl"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage { read_only: true },
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 2,
                            visibility: wgpu::ShaderStages::COMPUTE,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage { read_only: false },
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });

        let pipeline_layout =
            self.ctx
                .device
                .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lege-wgpu-convert-pipeline-layout"),
                    bind_group_layouts: &[&bind_group_layout],
                    immediate_size: 0,
                });

        let pipeline = self
            .ctx
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("lege-wgpu-convert-pipeline"),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            });

        self.convert_pipelines.insert(
            conversion,
            WgpuPipelineState {
                pipeline,
                bind_group_layout,
            },
        );
        Ok(())
    }

    fn ensure_convert_buffers(&mut self, dst_bytes: usize) -> Result<()> {
        let dst_aligned = align_up(dst_bytes, 256);
        let params_aligned = align_up(size_of::<ConvertParamsStd140>(), 256);
        let needs_new = match &self.convert_buffers {
            Some(b) => b.dst_capacity_bytes < dst_aligned,
            None => true,
        };
        if !needs_new {
            return Ok(());
        }

        let device = &self.ctx.device;

        let gpu_dst_nchw = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-convert-gpu-dst"),
            size: dst_aligned as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-convert-readback"),
            size: dst_aligned as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-convert-params"),
            size: params_aligned as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.convert_buffers = Some(WgpuConvertBuffers {
            gpu_dst_nchw,
            readback,
            params,
            dst_capacity_bytes: dst_aligned,
        });

        Ok(())
    }

    fn dispatch_resize(
        &mut self,
        params: &ResizeParameters,
        src_rgba: &[u8],
        gpu_src_size: usize,
        gpu_dst_size: usize,
        readback: bool,
    ) -> Result<Option<Vec<u8>>> {
        self.ensure_pipeline(params.filter)?;
        self.ensure_buffers(gpu_src_size, gpu_dst_size)?;

        let buffers = self.resize_buffers.as_ref().ok_or_else(|| {
            WgpuResizeError::Execution("resize buffers not initialized".to_string())
        })?;
        let pipeline = self
            .resize_pipelines
            .get(&params.filter)
            .ok_or_else(|| WgpuResizeError::Shader("pipeline cache miss".to_string()))?;

        let resize_params = ResizeParamsStd140 {
            src_width: params.src_width,
            src_height: params.src_height,
            dst_width: params.dst_width,
            dst_height: params.dst_height,
            scale_x: params.src_width as f32 / params.dst_width as f32,
            scale_y: params.src_height as f32 / params.dst_height as f32,
            offset_x: 0.0,
            offset_y: 0.0,
            border_mode: params.border_mode as u32,
            border_value: params.border_value / 255.0,
            channel_count: 4,
            no_srgb: if params.no_srgb { 1 } else { 0 },
        };

        self.ctx.queue.write_buffer(&buffers.gpu_src, 0, src_rgba);
        self.ctx
            .queue
            .write_buffer(&buffers.params, 0, bytemuck::bytes_of(&resize_params));

        let bind_group = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lege-wgpu-resize-bind-group"),
                layout: &pipeline.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffers.params.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: buffers.gpu_src.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: buffers.gpu_dst.as_entire_binding(),
                    },
                ],
            });

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lege-wgpu-resize-encoder"),
            });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("lege-wgpu-resize-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            let (tgx, tgy, _) = params.filter.thread_group_size();
            let groups_x = params.dst_width.div_ceil(tgx);
            let groups_y = params.dst_height.div_ceil(tgy);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
        }

        if readback {
            encoder.copy_buffer_to_buffer(
                &buffers.gpu_dst,
                0,
                &buffers.readback,
                0,
                gpu_dst_size as u64,
            );
        }

        self.ctx.queue.submit(Some(encoder.finish()));

        if self.verbose {
            log::debug!(
                "WGPU resize dispatched: {}x{} -> {}x{}",
                params.src_width,
                params.src_height,
                params.dst_width,
                params.dst_height
            );
        }

        if !readback {
            return Ok(None);
        }

        self.read_mapped_bytes(&buffers.readback, gpu_dst_size)
            .map(Some)
    }

    fn read_mapped_bytes(&self, buffer: &wgpu::Buffer, size: usize) -> Result<Vec<u8>> {
        let slice = buffer.slice(0..size as u64);
        let (tx, rx) = mpsc::channel();

        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });

        self.ctx
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| WgpuResizeError::Execution(format!("device poll failed: {e:?}")))?;

        rx.recv()
            .map_err(|_| WgpuResizeError::Execution("map_async channel failed".to_string()))?
            .map_err(|e| WgpuResizeError::Execution(format!("map_async failed: {e:?}")))?;

        let data = slice.get_mapped_range();
        let mut out = vec![0u8; size];
        out.copy_from_slice(&data[..size]);
        drop(data);
        buffer.unmap();
        Ok(out)
    }
}
