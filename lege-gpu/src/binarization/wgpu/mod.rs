use std::mem::size_of;
use std::sync::mpsc;

use crate::binarization::{
    BinarizationMode, BinarizationParams, BinarizeParamsStd140, GpuBinarizationError, Result,
};

struct WgpuContext {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

struct WgpuBuffers {
    gpu_src: wgpu::Buffer,
    gpu_dst: wgpu::Buffer,
    readback: wgpu::Buffer,
    params: wgpu::Buffer,
    src_capacity: usize,
    dst_capacity: usize,
}

pub struct WgpuBinarizer {
    ctx: WgpuContext,
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    buffers: Option<WgpuBuffers>,
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
                required_limits: wgpu::Limits::downlevel_defaults(),
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

impl WgpuBinarizer {
    pub fn new() -> Result<Self> {
        let ctx = WgpuContext::new()?;

        let shader = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("lege-wgpu-binarize-shader"),
                source: wgpu::ShaderSource::Wgsl(
                    include_str!("shaders/binarize_adaptive.wgsl").into(),
                ),
            });

        let bind_group_layout =
            ctx.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lege-wgpu-binarize-bgl"),
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
                                ty: wgpu::BindingType::Storage { read_only: true },
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

        let pipeline_layout = ctx
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("lege-wgpu-binarize-pipeline-layout"),
                bind_group_layouts: &[&bind_group_layout],
                immediate_size: 0,
            });

        let pipeline = ctx
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("lege-wgpu-binarize-pipeline"),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            });

        Ok(Self {
            ctx,
            pipeline,
            bind_group_layout,
            buffers: None,
        })
    }

    pub fn binarize_gray_raw(
        &mut self,
        gray: &[u8],
        params: &BinarizationParams,
    ) -> Result<Vec<u8>> {
        params.validate_gray(gray)?;
        match params.mode {
            BinarizationMode::FixedThreshold | BinarizationMode::Adaptive => {
                self.binarize(gray, params)
            }
        }
    }

    fn binarize(&mut self, gray: &[u8], params: &BinarizationParams) -> Result<Vec<u8>> {
        let src_words: Vec<u32> = gray.iter().map(|&v| v as u32).collect();
        let pixel_count = params.width as usize * params.height as usize;
        let src_bytes = src_words.len() * size_of::<u32>();
        let dst_bytes = pixel_count * size_of::<u32>();
        self.ensure_buffers(src_bytes, dst_bytes)?;
        let buffers = self
            .buffers
            .as_ref()
            .ok_or_else(|| GpuBinarizationError::Execution("buffers not initialized".into()))?;

        let sauvola_radius = params.adaptive.sauvola_window / 2;
        let padded_width = params.width + 2 * sauvola_radius;
        let padded_height = params.height + 2 * sauvola_radius;
        let integral_width = padded_width + 1;

        let gpu_params = BinarizeParamsStd140 {
            width: params.width,
            height: params.height,
            mode: match params.mode {
                BinarizationMode::FixedThreshold => 0,
                BinarizationMode::Adaptive => 1,
            },
            invert_output: params.invert_output as u32,
            fixed_threshold: params.fixed_threshold as u32,
            sauvola_window: params.adaptive.sauvola_window,
            bg_window: params.adaptive.bg_window,
            otsu_threshold: params.adaptive.otsu_threshold as u32,
            k_factor: params.k_factor,
            percentile_c: params.adaptive.percentile_c as f32,
            padded_width,
            padded_height,
            integral_width,
            sauvola_radius,
            debug_mode: params.debug_mode,
            _pad2: 0,
            _pad3: 0,
            _pad4: 0,
            _pad5: 0,
            _pad6: 0,
            _pad7: 0,
        };
        self.ctx
            .queue
            .write_buffer(&buffers.gpu_src, 0, bytemuck::cast_slice(&src_words));
        self.ctx
            .queue
            .write_buffer(&buffers.params, 0, bytemuck::bytes_of(&gpu_params));

        let bind_group = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lege-wgpu-binarize-bind-group"),
                layout: &self.bind_group_layout,
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
                label: Some("lege-wgpu-binarize-encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("lege-wgpu-binarize-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(params.width.div_ceil(16), params.height.div_ceil(16), 1);
        }
        encoder.copy_buffer_to_buffer(&buffers.gpu_dst, 0, &buffers.readback, 0, dst_bytes as u64);
        self.ctx.queue.submit(Some(encoder.finish()));

        let bytes = self.read_mapped_bytes(&buffers.readback, dst_bytes)?;
        let words: &[u32] = bytemuck::cast_slice(&bytes);
        Ok(words.iter().take(pixel_count).map(|&v| v as u8).collect())
    }

    fn ensure_buffers(&mut self, src_bytes: usize, dst_bytes: usize) -> Result<()> {
        let src_aligned = align_up(src_bytes, 256);
        let dst_aligned = align_up(dst_bytes, 256);
        let params_aligned = align_up(size_of::<BinarizeParamsStd140>(), 256);
        let needs_new = match &self.buffers {
            Some(b) => b.src_capacity < src_aligned || b.dst_capacity < dst_aligned,
            None => true,
        };
        if !needs_new {
            return Ok(());
        }

        let gpu_src = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-binarize-src"),
            size: src_aligned as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let gpu_dst = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-binarize-dst"),
            size: dst_aligned as u64,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let readback = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-binarize-readback"),
            size: dst_aligned as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let params = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lege-wgpu-binarize-params"),
            size: params_aligned as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.buffers = Some(WgpuBuffers {
            gpu_src,
            gpu_dst,
            readback,
            params,
            src_capacity: src_aligned,
            dst_capacity: dst_aligned,
        });
        Ok(())
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
            .map_err(|e| GpuBinarizationError::Execution(format!("device poll failed: {e:?}")))?;
        rx.recv()
            .map_err(|_| GpuBinarizationError::Execution("map_async channel failed".into()))?
            .map_err(|e| GpuBinarizationError::Execution(format!("map_async failed: {e:?}")))?;
        let data = slice.get_mapped_range();
        let mut out = vec![0u8; size];
        out.copy_from_slice(&data[..size]);
        drop(data);
        buffer.unmap();
        Ok(out)
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}
