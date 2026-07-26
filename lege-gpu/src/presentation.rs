//! Shared wgpu window-presentation core.
//!
//! This module deliberately knows nothing about PDFs or viewer state. Callers
//! stream ordered solid and image quads into a frame; the compositor owns the
//! surface, pipelines, bounded texture-array atlas, and submission policy.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytemuck::{Pod, Zeroable};
use thiserror::Error;
use wgpu::util::DeviceExt;
use winit::window::Window;

const TILE_EXTENT: u32 = 256;
const LAYERS_PER_BANK: u32 = 256;
const BYTES_PER_BANK: u64 = TILE_EXTENT as u64 * TILE_EXTENT as u64 * LAYERS_PER_BANK as u64 * 4;
const DEFAULT_MAX_BANKS: usize = 4;

const SHADER: &str = r#"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv_layer: vec3<f32>,
    @location(2) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: i32,
    @location(2) color: vec4<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = vec4<f32>(input.position, 0.0, 1.0);
    output.uv = input.uv_layer.xy;
    output.layer = i32(input.uv_layer.z);
    output.color = input.color;
    return output;
}

@fragment
fn fs_solid(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}

@group(0) @binding(0)
var tile_atlas: texture_2d_array<f32>;
@group(0) @binding(1)
var tile_sampler: sampler;

@fragment
fn fs_image(input: VertexOutput) -> @location(0) vec4<f32> {
    let sampled = textureSample(tile_atlas, tile_sampler, input.uv, input.layer);
    return vec4<f32>(sampled.rgb, 1.0);
}
"#;

/// Collision-free caller-provided identity for one atlas image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageKey(pub [u64; 4]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sampling {
    Nearest,
    Linear,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    fn right(self) -> f32 {
        self.x + self.width
    }

    fn bottom(self) -> f32 {
        self.y + self.height
    }

    fn intersection(self, other: Self) -> Option<Self> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        (x1 > x0 && y1 > y0).then_some(Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ImageSource<'a> {
    pub key: ImageKey,
    pub revision: u64,
    pub width: u32,
    pub height: u32,
    pub stride_pixels: usize,
    pub pixels: &'a [u32],
}

#[derive(Debug, Clone, Copy)]
pub struct ImageQuad<'a> {
    pub source: ImageSource<'a>,
    pub destination: Rect,
    pub clip: Rect,
    pub sampling: Sampling,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresentationStats {
    pub frames_presented: u64,
    pub frames_skipped: u64,
    pub atlas_uploads: u64,
    pub atlas_upload_bytes: u64,
    pub atlas_resident_images: usize,
    pub atlas_bytes: u64,
    pub draw_calls: u32,
    pub vertices: u32,
}

#[derive(Debug, Clone)]
pub struct PresentationConfig {
    pub max_atlas_banks: usize,
    pub desired_maximum_frame_latency: u32,
}

impl Default for PresentationConfig {
    fn default() -> Self {
        Self {
            max_atlas_banks: DEFAULT_MAX_BANKS,
            desired_maximum_frame_latency: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentOutcome {
    Presented,
    Skipped,
}

#[derive(Debug, Error)]
pub enum PresentationError {
    #[error("failed to create a GPU presentation surface: {0}")]
    CreateSurface(String),
    #[error("no surface-compatible GPU adapter is available: {0}")]
    Adapter(String),
    #[error("failed to create a GPU device: {0}")]
    Device(String),
    #[error("the GPU device was lost or reported an uncaptured error")]
    DeviceLost,
    #[error("the selected adapter cannot present to this surface")]
    UnsupportedSurface,
    #[error("invalid presentation size")]
    InvalidSize,
    #[error("invalid image dimensions or stride")]
    InvalidImage,
    #[error("the GPU tile atlas is full for the current frame")]
    AtlasFull,
    #[error("the presentation surface was lost")]
    SurfaceLost,
    #[error("GPU presentation validation failed")]
    Validation,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    uv_layer: [f32; 3],
    color: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchKind {
    Solid,
    Image { bank: usize, sampling: Sampling },
}

#[derive(Debug, Clone, Copy)]
struct DrawBatch {
    kind: BatchKind,
    first_vertex: u32,
    vertex_count: u32,
}

#[derive(Debug)]
struct AtlasBank {
    texture: wgpu::Texture,
    nearest_bind_group: wgpu::BindGroup,
    linear_bind_group: wgpu::BindGroup,
    slots: Vec<Option<ImageKey>>,
}

#[derive(Debug, Clone, Copy)]
struct ResidentImage {
    bank: usize,
    layer: u32,
    revision: u64,
    width: u32,
    height: u32,
    last_used_frame: u64,
}

#[allow(missing_debug_implementations)]
pub struct GpuCompositor {
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    adapter_name: String,
    backend_name: String,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    width: u32,
    height: u32,
    max_atlas_banks: usize,
    tile_bind_group_layout: wgpu::BindGroupLayout,
    nearest_sampler: wgpu::Sampler,
    linear_sampler: wgpu::Sampler,
    solid_pipeline: wgpu::RenderPipeline,
    image_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
    vertices: Vec<Vertex>,
    batches: Vec<DrawBatch>,
    banks: Vec<AtlasBank>,
    residents: HashMap<ImageKey, ResidentImage>,
    frame_number: u64,
    clear_color: wgpu::Color,
    stats: PresentationStats,
    device_failed: Arc<AtomicBool>,
}

impl GpuCompositor {
    pub fn new(
        window: Arc<Window>,
        width: u32,
        height: u32,
        config: PresentationConfig,
    ) -> Result<Self, PresentationError> {
        if width == 0 || height == 0 {
            return Err(PresentationError::InvalidSize);
        }
        pollster::block_on(Self::new_async(window, width, height, config))
    }

    async fn new_async(
        window: Arc<Window>,
        width: u32,
        height: u32,
        config: PresentationConfig,
    ) -> Result<Self, PresentationError> {
        let instance = crate::wgpu_setup::create_instance();
        let surface = instance
            .create_surface(window)
            .map_err(|error| PresentationError::CreateSurface(error.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await
            .map_err(|error| PresentationError::Adapter(error.to_string()))?;
        let info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("lege-presentation"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|error| PresentationError::Device(error.to_string()))?;
        let device_failed = Arc::new(AtomicBool::new(false));
        let lost_flag = device_failed.clone();
        device.set_device_lost_callback(move |_reason, _message| {
            lost_flag.store(true, Ordering::Release);
        });
        let error_flag = device_failed.clone();
        device.on_uncaptured_error(Arc::new(move |_error| {
            error_flag.store(true, Ordering::Release);
        }));
        let mut surface_config = surface
            .get_default_config(&adapter, width, height)
            .ok_or(PresentationError::UnsupportedSurface)?;
        if let Some(srgb_format) = surface
            .get_capabilities(&adapter)
            .formats
            .into_iter()
            .find(|format| format.is_srgb())
        {
            surface_config.format = srgb_format;
        }
        surface_config.present_mode = wgpu::PresentMode::AutoVsync;
        surface_config.desired_maximum_frame_latency =
            config.desired_maximum_frame_latency.clamp(1, 3);
        surface.configure(&device, &surface_config);

        let tile_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("lege-presentation-tile-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2Array,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("lege-presentation-nearest"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("lege-presentation-linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lege-presentation-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 8,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 20,
                    shader_location: 2,
                },
            ],
        };
        let solid_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lege-presentation-solid-pipeline-layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let image_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lege-presentation-image-pipeline-layout"),
            bind_group_layouts: &[Some(&tile_bind_group_layout)],
            immediate_size: 0,
        });
        let target = Some(wgpu::ColorTargetState {
            format: surface_config.format,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        });
        let buffers = [Some(vertex_layout)];
        let targets = [target];
        let solid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lege-presentation-solid-pipeline"),
            layout: Some(&solid_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &buffers,
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_solid"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("lege-presentation-image-pipeline"),
            layout: Some(&image_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &buffers,
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_image"),
                compilation_options: Default::default(),
                targets: &targets,
            }),
            multiview_mask: None,
            cache: None,
        });
        let vertex_capacity = 4096;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-presentation-vertices"),
            size: (vertex_capacity * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            _instance: instance,
            surface,
            adapter_name: info.name,
            backend_name: format!("{:?}", info.backend),
            device,
            queue,
            surface_config,
            width,
            height,
            max_atlas_banks: config.max_atlas_banks.clamp(1, DEFAULT_MAX_BANKS),
            tile_bind_group_layout,
            nearest_sampler,
            linear_sampler,
            solid_pipeline,
            image_pipeline,
            vertex_buffer,
            vertex_capacity,
            vertices: Vec::with_capacity(vertex_capacity),
            batches: Vec::with_capacity(64),
            banks: Vec::new(),
            residents: HashMap::new(),
            frame_number: 0,
            clear_color: wgpu::Color::BLACK,
            stats: PresentationStats::default(),
            device_failed,
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), PresentationError> {
        if width == 0 || height == 0 {
            self.width = width;
            self.height = height;
            return Ok(());
        }
        self.width = width;
        self.height = height;
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
        Ok(())
    }

    pub fn begin_frame(&mut self, clear_xrgb: u32) {
        self.frame_number = self.frame_number.wrapping_add(1).max(1);
        self.vertices.clear();
        self.batches.clear();
        self.clear_color = xrgb_to_color(clear_xrgb);
        self.stats.draw_calls = 0;
        self.stats.vertices = 0;
    }

    pub fn push_solid(&mut self, destination: Rect, clip: Rect, xrgb: u32) {
        self.push_solid_argb(destination, clip, 0xff00_0000 | (xrgb & 0x00ff_ffff));
    }

    pub fn push_solid_argb(&mut self, destination: Rect, clip: Rect, argb: u32) {
        let Some(clipped) = destination.intersection(clip).and_then(|rect| {
            rect.intersection(Rect {
                x: 0.0,
                y: 0.0,
                width: self.width as f32,
                height: self.height as f32,
            })
        }) else {
            return;
        };
        let color = argb_to_array(argb);
        self.push_quad_vertices(clipped, [0.0, 0.0], [0.0, 0.0], 0.0, color);
        self.extend_batch(BatchKind::Solid, 6);
    }

    pub fn push_image(&mut self, quad: ImageQuad<'_>) -> Result<(), PresentationError> {
        let Some(clipped) = quad.destination.intersection(quad.clip).and_then(|rect| {
            rect.intersection(Rect {
                x: 0.0,
                y: 0.0,
                width: self.width as f32,
                height: self.height as f32,
            })
        }) else {
            return Ok(());
        };
        let resident = self.ensure_resident(quad.source)?;
        let offset_x = (clipped.x - quad.destination.x) / quad.destination.width;
        let offset_y = (clipped.y - quad.destination.y) / quad.destination.height;
        let extent_x = clipped.width / quad.destination.width;
        let extent_y = clipped.height / quad.destination.height;
        let texel_max_x = resident.width as f32 / TILE_EXTENT as f32;
        let texel_max_y = resident.height as f32 / TILE_EXTENT as f32;
        let half_u = 0.5 / TILE_EXTENT as f32;
        let half_v = 0.5 / TILE_EXTENT as f32;
        let uv0 = [
            (offset_x * texel_max_x).clamp(half_u, (texel_max_x - half_u).max(half_u)),
            (offset_y * texel_max_y).clamp(half_v, (texel_max_y - half_v).max(half_v)),
        ];
        let uv1 = [
            ((offset_x + extent_x) * texel_max_x).clamp(half_u, (texel_max_x - half_u).max(half_u)),
            ((offset_y + extent_y) * texel_max_y).clamp(half_v, (texel_max_y - half_v).max(half_v)),
        ];
        self.push_quad_vertices(clipped, uv0, uv1, resident.layer as f32, [1.0; 4]);
        self.extend_batch(
            BatchKind::Image {
                bank: resident.bank,
                sampling: quad.sampling,
            },
            6,
        );
        Ok(())
    }

    pub fn present(&mut self) -> Result<PresentOutcome, PresentationError> {
        if self.device_failed.load(Ordering::Acquire) {
            return Err(PresentationError::DeviceLost);
        }
        if self.width == 0 || self.height == 0 {
            self.stats.frames_skipped = self.stats.frames_skipped.saturating_add(1);
            return Ok(PresentOutcome::Skipped);
        }
        self.ensure_vertex_capacity();
        if !self.vertices.is_empty() {
            self.queue
                .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&self.vertices));
        }

        let (surface_texture, reconfigure_after) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => (texture, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(texture) => (texture, true),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.stats.frames_skipped = self.stats.frames_skipped.saturating_add(1);
                return Ok(PresentOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(texture)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => (texture, false),
                    _ => return Err(PresentationError::SurfaceLost),
                }
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(PresentationError::Validation);
            }
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("lege-presentation-encoder"),
            });
        {
            let color_attachment = Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(self.clear_color),
                    store: wgpu::StoreOp::Store,
                },
            });
            let attachments = [color_attachment];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lege-presentation-pass"),
                color_attachments: &attachments,
                ..Default::default()
            });
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            for batch in &self.batches {
                match batch.kind {
                    BatchKind::Solid => pass.set_pipeline(&self.solid_pipeline),
                    BatchKind::Image { bank, sampling } => {
                        pass.set_pipeline(&self.image_pipeline);
                        let bind_group = match sampling {
                            Sampling::Nearest => &self.banks[bank].nearest_bind_group,
                            Sampling::Linear => &self.banks[bank].linear_bind_group,
                        };
                        pass.set_bind_group(0, bind_group, &[]);
                    }
                }
                let end = batch.first_vertex + batch.vertex_count;
                pass.draw(batch.first_vertex..end, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(surface_texture);
        if reconfigure_after {
            self.surface.configure(&self.device, &self.surface_config);
        }
        self.stats.frames_presented = self.stats.frames_presented.saturating_add(1);
        self.stats.draw_calls = self.batches.len() as u32;
        self.stats.vertices = self.vertices.len() as u32;
        self.stats.atlas_resident_images = self.residents.len();
        self.stats.atlas_bytes = self.banks.len() as u64 * BYTES_PER_BANK;
        Ok(PresentOutcome::Presented)
    }

    pub fn stats(&self) -> PresentationStats {
        self.stats
    }

    fn ensure_resident(
        &mut self,
        source: ImageSource<'_>,
    ) -> Result<ResidentImage, PresentationError> {
        if source.width == 0
            || source.height == 0
            || source.width > TILE_EXTENT
            || source.height > TILE_EXTENT
            || source.stride_pixels < source.width as usize
            || source.pixels.len() < source.stride_pixels.saturating_mul(source.height as usize)
        {
            return Err(PresentationError::InvalidImage);
        }
        if let Some(existing) = self.residents.get_mut(&source.key) {
            existing.last_used_frame = self.frame_number;
            let resident = *existing;
            if resident.revision != source.revision
                || resident.width != source.width
                || resident.height != source.height
            {
                self.upload(resident.bank, resident.layer, source);
                if let Some(updated) = self.residents.get_mut(&source.key) {
                    updated.revision = source.revision;
                    updated.width = source.width;
                    updated.height = source.height;
                    return Ok(*updated);
                }
            }
            return Ok(resident);
        }

        let (bank, layer) = self.allocate_slot()?;
        self.banks[bank].slots[layer as usize] = Some(source.key);
        let resident = ResidentImage {
            bank,
            layer,
            revision: source.revision,
            width: source.width,
            height: source.height,
            last_used_frame: self.frame_number,
        };
        self.residents.insert(source.key, resident);
        self.upload(bank, layer, source);
        Ok(resident)
    }

    fn allocate_slot(&mut self) -> Result<(usize, u32), PresentationError> {
        for (bank_index, bank) in self.banks.iter().enumerate() {
            if let Some(layer) = bank.slots.iter().position(Option::is_none) {
                return Ok((bank_index, layer as u32));
            }
        }
        if self.banks.len() < self.max_atlas_banks {
            let index = self.banks.len();
            self.create_bank();
            return Ok((index, 0));
        }
        let candidate = self
            .residents
            .iter()
            .filter(|(_, resident)| resident.last_used_frame != self.frame_number)
            .min_by_key(|(_, resident)| resident.last_used_frame)
            .map(|(key, resident)| (*key, *resident));
        let Some((key, resident)) = candidate else {
            return Err(PresentationError::AtlasFull);
        };
        self.residents.remove(&key);
        self.banks[resident.bank].slots[resident.layer as usize] = None;
        Ok((resident.bank, resident.layer))
    }

    fn create_bank(&mut self) {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lege-presentation-tile-atlas"),
            size: wgpu::Extent3d {
                width: TILE_EXTENT,
                height: TILE_EXTENT,
                depth_or_array_layers: LAYERS_PER_BANK,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("lege-presentation-tile-atlas-view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let nearest_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lege-presentation-nearest-atlas"),
            layout: &self.tile_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.nearest_sampler),
                },
            ],
        });
        let linear_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lege-presentation-linear-atlas"),
            layout: &self.tile_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
                },
            ],
        });
        self.banks.push(AtlasBank {
            texture,
            nearest_bind_group,
            linear_bind_group,
            slots: vec![None; LAYERS_PER_BANK as usize],
        });
    }

    fn upload(&mut self, bank: usize, layer: u32, source: ImageSource<'_>) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.banks[bank].texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: 0,
                    y: 0,
                    z: layer,
                },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(source.pixels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((source.stride_pixels * 4) as u32),
                rows_per_image: Some(source.height),
            },
            wgpu::Extent3d {
                width: source.width,
                height: source.height,
                depth_or_array_layers: 1,
            },
        );
        self.stats.atlas_uploads = self.stats.atlas_uploads.saturating_add(1);
        self.stats.atlas_upload_bytes = self
            .stats
            .atlas_upload_bytes
            .saturating_add(u64::from(source.width) * u64::from(source.height) * 4);
    }

    fn push_quad_vertices(
        &mut self,
        rect: Rect,
        uv0: [f32; 2],
        uv1: [f32; 2],
        layer: f32,
        color: [f32; 4],
    ) {
        let left = rect.x / self.width as f32 * 2.0 - 1.0;
        let right = rect.right() / self.width as f32 * 2.0 - 1.0;
        let top = 1.0 - rect.y / self.height as f32 * 2.0;
        let bottom = 1.0 - rect.bottom() / self.height as f32 * 2.0;
        let vertex = |position: [f32; 2], uv: [f32; 2]| Vertex {
            position,
            uv_layer: [uv[0], uv[1], layer],
            color,
        };
        self.vertices.extend_from_slice(&[
            vertex([left, top], [uv0[0], uv0[1]]),
            vertex([left, bottom], [uv0[0], uv1[1]]),
            vertex([right, top], [uv1[0], uv0[1]]),
            vertex([right, top], [uv1[0], uv0[1]]),
            vertex([left, bottom], [uv0[0], uv1[1]]),
            vertex([right, bottom], [uv1[0], uv1[1]]),
        ]);
    }

    fn extend_batch(&mut self, kind: BatchKind, vertex_count: u32) {
        if let Some(last) = self.batches.last_mut()
            && last.kind == kind
        {
            last.vertex_count += vertex_count;
            return;
        }
        self.batches.push(DrawBatch {
            kind,
            first_vertex: self.vertices.len() as u32 - vertex_count,
            vertex_count,
        });
    }

    fn ensure_vertex_capacity(&mut self) {
        if self.vertices.len() <= self.vertex_capacity {
            return;
        }
        self.vertex_capacity = self.vertices.len().next_power_of_two();
        self.vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("lege-presentation-vertices"),
                contents: &vec![0; self.vertex_capacity * std::mem::size_of::<Vertex>()],
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            });
    }
}

fn xrgb_to_array(xrgb: u32) -> [f32; 4] {
    let linear = |value: u32| {
        let srgb = value as f32 / 255.0;
        if srgb <= 0.04045 {
            srgb / 12.92
        } else {
            ((srgb + 0.055) / 1.055).powf(2.4)
        }
    };
    [
        linear((xrgb >> 16) & 0xff),
        linear((xrgb >> 8) & 0xff),
        linear(xrgb & 0xff),
        1.0,
    ]
}

fn argb_to_array(argb: u32) -> [f32; 4] {
    let mut color = xrgb_to_array(argb);
    color[3] = ((argb >> 24) & 0xff) as f32 / 255.0;
    color
}

fn xrgb_to_color(xrgb: u32) -> wgpu::Color {
    let [r, g, b, a] = xrgb_to_array(xrgb);
    wgpu::Color {
        r: f64::from(r),
        g: f64::from(g),
        b: f64::from(b),
        a: f64::from(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_use_half_open_intersection() {
        let left = Rect {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        let touching = Rect {
            x: 10.0,
            y: 0.0,
            width: 5.0,
            height: 5.0,
        };
        assert_eq!(left.intersection(touching), None);
        assert_eq!(
            left.intersection(Rect {
                x: 8.0,
                y: 7.0,
                width: 5.0,
                height: 5.0,
            }),
            Some(Rect {
                x: 8.0,
                y: 7.0,
                width: 2.0,
                height: 3.0,
            })
        );
    }

    #[test]
    fn atlas_bank_size_is_bounded_and_explicit() {
        assert_eq!(BYTES_PER_BANK, 64 * 1024 * 1024);
        assert_eq!(DEFAULT_MAX_BANKS as u64 * BYTES_PER_BANK, 256 * 1024 * 1024);
    }

    #[test]
    fn solid_colors_are_linearized_for_an_srgb_surface() {
        let [red, green, blue, alpha] = xrgb_to_array(0x80_00_00);
        assert!((red - 0.215_86).abs() < 0.000_1);
        assert_eq!(green, 0.0);
        assert_eq!(blue, 0.0);
        assert_eq!(alpha, 1.0);
    }

    #[test]
    fn alpha_solids_retain_source_alpha() {
        let color = argb_to_array(0x80_ff_00_00);
        assert!((color[3] - 128.0 / 255.0).abs() < f32::EPSILON);
    }
}
