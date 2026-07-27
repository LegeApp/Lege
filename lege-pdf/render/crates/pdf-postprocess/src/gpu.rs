use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use lege_gpu::compute::{AdapterInfo, SharedGpuContext, wgpu};
use pdf_render_api::{
    HostPage, OutputFormat, PostprocessCapabilities, PostprocessOperations, RenderedPage,
};
use wgpu::util::DeviceExt;

use crate::{
    CpuPostprocess, DitherSpec, PostprocessBackend, PostprocessError, PostprocessGraph,
    PostprocessOp, PostprocessOutput, ResizeFilter,
};

const WORKGROUP_SIZE: u32 = 256;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuParams {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    format: u32,
    filter_mode: u32,
    p0: u32,
    p1: u32,
    p2: u32,
    p3: u32,
    p4: u32,
    p5: u32,
    p6: u32,
    p7: u32,
    p8: u32,
    p9: u32,
}

impl GpuParams {
    fn new(width: u32, height: u32, format: OutputFormat) -> Self {
        Self {
            src_width: width,
            src_height: height,
            dst_width: width,
            dst_height: height,
            format: format_code(format),
            filter_mode: 0,
            p0: 0,
            p1: 0,
            p2: 0,
            p3: 0,
            p4: 0,
            p5: 0,
            p6: 0,
            p7: 0,
            p8: 0,
            p9: 0,
        }
    }
}

fn format_code(format: OutputFormat) -> u32 {
    match format {
        OutputFormat::Rgba8PremultipliedSrgb => 0,
        OutputFormat::Gray8 => 1,
    }
}

fn filter_code(filter: ResizeFilter) -> u32 {
    match filter {
        ResizeFilter::Nearest => 0,
        ResizeFilter::Box => 1,
        ResizeFilter::Bilinear => 2,
        ResizeFilter::CatmullRom => 3,
        ResizeFilter::Lanczos3 => 4,
    }
}

fn div_ceil(value: u32, divisor: u32) -> u32 {
    value.div_ceil(divisor)
}

struct PipelineSet {
    layout: wgpu::BindGroupLayout,
    crop: wgpu::ComputePipeline,
    to_gray: wgpu::ComputePipeline,
    tone: wgpu::ComputePipeline,
    resize_nearest: wgpu::ComputePipeline,
    resize_horizontal: wgpu::ComputePipeline,
    resize_vertical: wgpu::ComputePipeline,
    integral_rows: wgpu::ComputePipeline,
    integral_columns: wgpu::ComputePipeline,
    otsu_histogram: wgpu::ComputePipeline,
    otsu_find: wgpu::ComputePipeline,
    otsu_apply: wgpu::ComputePipeline,
    sauvola: wgpu::ComputePipeline,
    fuse_thresholds: wgpu::ComputePipeline,
    dither: wgpu::ComputePipeline,
    floyd_steinberg: wgpu::ComputePipeline,
    pack_monochrome: wgpu::ComputePipeline,
    dummy_lut: wgpu::Buffer,
}

impl PipelineSet {
    fn new(context: &SharedGpuContext) -> Result<Self, PostprocessError> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Self::build(context.device())
        }));
        match result {
            Ok(pipelines) => Ok(pipelines),
            Err(payload) => Err(PostprocessError::Failed(format!(
                "GPU postprocess shader/pipeline initialization panicked: {}",
                pdf_render_api::panic_message(payload)
            ))),
        }
    }

    fn build(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pdf-postprocess-layout"),
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
                storage_entry(1, true),
                storage_entry(2, false),
                storage_entry(3, true),
                storage_entry(4, false),
                storage_entry(5, false),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pdf-postprocess-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pdf-postprocess-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/postprocess.wgsl").into()),
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
        let dummy_lut = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("pdf-postprocess-dummy-lut"),
            contents: bytemuck::cast_slice(&(0u32..256).collect::<Vec<_>>()),
            usage: wgpu::BufferUsages::STORAGE,
        });
        Self {
            layout,
            crop: make_pipeline("pdf-postprocess-crop", "crop"),
            to_gray: make_pipeline("pdf-postprocess-gray", "to_gray"),
            tone: make_pipeline("pdf-postprocess-tone", "tone"),
            resize_nearest: make_pipeline("pdf-postprocess-nearest", "resize_nearest"),
            resize_horizontal: make_pipeline(
                "pdf-postprocess-resize-horizontal",
                "resize_horizontal",
            ),
            resize_vertical: make_pipeline("pdf-postprocess-resize-vertical", "resize_vertical"),
            integral_rows: make_pipeline("pdf-postprocess-integral-rows", "integral_rows"),
            integral_columns: make_pipeline("pdf-postprocess-integral-columns", "integral_columns"),
            otsu_histogram: make_pipeline("pdf-postprocess-otsu-histogram", "otsu_histogram"),
            otsu_find: make_pipeline("pdf-postprocess-otsu-find", "otsu_find"),
            otsu_apply: make_pipeline("pdf-postprocess-otsu-apply", "otsu_apply"),
            sauvola: make_pipeline("pdf-postprocess-sauvola", "sauvola"),
            fuse_thresholds: make_pipeline("pdf-postprocess-fuse-thresholds", "fuse_thresholds"),
            dither: make_pipeline("pdf-postprocess-dither", "dither"),
            floyd_steinberg: make_pipeline("pdf-postprocess-floyd-steinberg", "floyd_steinberg"),
            pack_monochrome: make_pipeline("pdf-postprocess-pack-monochrome", "pack_monochrome"),
            dummy_lut,
        }
    }
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

struct GpuBuffers {
    first: wgpu::Buffer,
    second: wgpu::Buffer,
    resize_mid: wgpu::Buffer,
    aux: wgpu::Buffer,
    readback: wgpu::Buffer,
    pixel_capacity: usize,
    mid_capacity: usize,
    aux_capacity: usize,
    readback_capacity: usize,
}

impl GpuBuffers {
    fn new(
        device: &wgpu::Device,
        pixel_capacity: usize,
        mid_capacity: usize,
        aux_capacity: usize,
        readback_capacity: usize,
    ) -> Self {
        let storage = |label: &'static str, bytes: usize| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes.max(4) as u64,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        Self {
            first: storage("pdf-postprocess-first", pixel_capacity * 4),
            second: storage("pdf-postprocess-second", pixel_capacity * 4),
            resize_mid: storage("pdf-postprocess-resize-mid", mid_capacity * 16),
            aux: storage("pdf-postprocess-aux", aux_capacity * 4),
            readback: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pdf-postprocess-readback"),
                size: readback_capacity.max(4) as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            pixel_capacity,
            mid_capacity,
            aux_capacity,
            readback_capacity,
        }
    }

    fn fits(&self, layout: &GraphLayout) -> bool {
        self.pixel_capacity >= layout.max_pixels
            && self.mid_capacity >= layout.max_mid_pixels
            && self.aux_capacity >= layout.aux_u32s
            && self.readback_capacity >= layout.max_readback_bytes
    }
}

#[derive(Debug)]
struct GraphLayout {
    max_pixels: usize,
    max_mid_pixels: usize,
    aux_u32s: usize,
    max_readback_bytes: usize,
    final_width: u32,
    final_height: u32,
    final_format: OutputFormat,
    packed: bool,
    packed_stride: usize,
    result_bytes: usize,
}

fn checked_pixels(width: u32, height: u32) -> Result<usize, PostprocessError> {
    (width as usize)
        .checked_mul(height as usize)
        .ok_or(PostprocessError::InvalidParams(
            "postprocess dimensions overflow address space",
        ))
}

fn analyze(source: &HostPage, graph: &PostprocessGraph) -> Result<GraphLayout, PostprocessError> {
    if !CpuPostprocess.supports(graph) {
        return Err(PostprocessError::InvalidParams(
            "postprocess graph shape is invalid",
        ));
    }
    let bpp = source.format.bytes_per_pixel();
    let row_bytes = (source.width as usize)
        .checked_mul(bpp)
        .ok_or(PostprocessError::FormatMismatch("source row overflow"))?;
    let height = source.height as usize;
    if source.width == 0 || source.height == 0 {
        return Err(PostprocessError::FormatMismatch("empty source page"));
    }
    if source.stride < row_bytes {
        return Err(PostprocessError::FormatMismatch(
            "source stride shorter than a pixel row",
        ));
    }
    let needed = source
        .stride
        .checked_mul(height - 1)
        .and_then(|bytes| bytes.checked_add(row_bytes))
        .ok_or(PostprocessError::FormatMismatch("source size overflow"))?;
    if source.pixels.len() < needed {
        return Err(PostprocessError::FormatMismatch(
            "source pixel buffer shorter than stride × height",
        ));
    }

    let mut width = source.width;
    let mut current_height = source.height;
    let mut format = source.format;
    let mut max_pixels = checked_pixels(width, current_height)?;
    let mut max_mid_pixels = 1usize;
    let mut aux_u32s = 257usize;
    let mut packed = false;
    for op in &graph.ops {
        match op {
            PostprocessOp::Crop(spec) => {
                let x1 = spec
                    .x
                    .checked_add(spec.width)
                    .ok_or(PostprocessError::InvalidParams("crop overflow"))?;
                let y1 = spec
                    .y
                    .checked_add(spec.height)
                    .ok_or(PostprocessError::InvalidParams("crop overflow"))?;
                if x1 > width || y1 > current_height {
                    return Err(PostprocessError::InvalidParams(
                        "crop rectangle exceeds the surface",
                    ));
                }
                width = spec.width;
                current_height = spec.height;
            }
            PostprocessOp::Resize(spec) => {
                max_mid_pixels = max_mid_pixels.max(checked_pixels(spec.width, current_height)?);
                width = spec.width;
                current_height = spec.height;
            }
            PostprocessOp::ConvertToGray(_) => format = OutputFormat::Gray8,
            PostprocessOp::ApplyToneCurve(_) => {}
            PostprocessOp::Otsu(_)
            | PostprocessOp::Sauvola(_)
            | PostprocessOp::FuseThresholds(_)
            | PostprocessOp::Dither(_)
            | PostprocessOp::PackMonochrome => {
                if format != OutputFormat::Gray8 {
                    return Err(PostprocessError::FormatMismatch(
                        "operation requires a Gray8 surface (insert ConvertToGray first)",
                    ));
                }
                if matches!(op, PostprocessOp::Dither(DitherSpec::FloydSteinberg)) {
                    aux_u32s = aux_u32s.max((width as usize + 2).checked_mul(2).ok_or(
                        PostprocessError::InvalidParams("Floyd-Steinberg scratch size overflow"),
                    )?);
                }
                if matches!(op, PostprocessOp::PackMonochrome) {
                    packed = true;
                }
                if matches!(
                    op,
                    PostprocessOp::Sauvola(_) | PostprocessOp::FuseThresholds(_)
                ) {
                    max_mid_pixels = max_mid_pixels.max(checked_pixels(width, current_height)?);
                }
            }
        }
        max_pixels = max_pixels.max(checked_pixels(width, current_height)?);
    }
    let final_pixels = checked_pixels(width, current_height)?;
    let packed_stride = (width as usize).div_ceil(8);
    let result_bytes = if packed {
        packed_stride.checked_mul(current_height as usize).ok_or(
            PostprocessError::InvalidParams("packed output size overflow"),
        )?
    } else {
        final_pixels.checked_mul(format.bytes_per_pixel()).ok_or(
            PostprocessError::InvalidParams("postprocess output size overflow"),
        )?
    };
    // Gray surfaces still occupy one u32 per pixel on the GPU.
    let final_transfer = if packed {
        result_bytes.next_multiple_of(4)
    } else {
        final_pixels * 4
    };
    Ok(GraphLayout {
        max_pixels,
        max_mid_pixels,
        aux_u32s,
        max_readback_bytes: final_transfer.max(4),
        final_width: width,
        final_height: current_height,
        final_format: format,
        packed,
        packed_stride,
        result_bytes,
    })
}

struct GpuSession {
    buffers: Option<GpuBuffers>,
}

impl GpuSession {
    fn new() -> Self {
        Self { buffers: None }
    }

    fn ensure_buffers(&mut self, device: &wgpu::Device, layout: &GraphLayout) {
        if self
            .buffers
            .as_ref()
            .is_some_and(|buffers| buffers.fits(layout))
        {
            return;
        }
        self.buffers = Some(GpuBuffers::new(
            device,
            layout.max_pixels,
            layout.max_mid_pixels,
            layout.aux_u32s,
            layout.max_readback_bytes,
        ));
    }

    fn execute(
        &mut self,
        context: &SharedGpuContext,
        pipelines: &PipelineSet,
        source: &HostPage,
        graph: &PostprocessGraph,
        layout: &GraphLayout,
    ) -> Result<GpuExecution, PostprocessError> {
        let started = Instant::now();
        self.ensure_buffers(context.device(), layout);
        let buffers = self
            .buffers
            .as_ref()
            .ok_or_else(|| PostprocessError::Failed("GPU buffers were not created".to_owned()))?;
        let upload = pack_source(source)?;
        context
            .queue()
            .write_buffer(&buffers.first, 0, bytemuck::cast_slice(&upload));

        let mut encoder =
            context
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("pdf-postprocess-command-encoder"),
                });
        let mut first_is_current = true;
        let mut width = source.width;
        let mut height = source.height;
        let mut format = source.format;

        for op in &graph.ops {
            let (current, next) = if first_is_current {
                (&buffers.first, &buffers.second)
            } else {
                (&buffers.second, &buffers.first)
            };
            match op {
                PostprocessOp::Crop(spec) => {
                    let mut params = GpuParams::new(width, height, format);
                    params.dst_width = spec.width;
                    params.dst_height = spec.height;
                    params.p0 = spec.x;
                    params.p1 = spec.y;
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.crop,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        div_ceil(spec.width * spec.height, WORKGROUP_SIZE),
                    );
                    width = spec.width;
                    height = spec.height;
                    first_is_current = !first_is_current;
                }
                PostprocessOp::Resize(spec) => {
                    let mut params = GpuParams::new(width, height, format);
                    params.dst_width = spec.width;
                    params.dst_height = spec.height;
                    params.filter_mode = filter_code(spec.filter);
                    if spec.filter == ResizeFilter::Nearest {
                        encode_dispatch(
                            context.device(),
                            &mut encoder,
                            pipelines,
                            &pipelines.resize_nearest,
                            &params,
                            current,
                            next,
                            &pipelines.dummy_lut,
                            buffers,
                            div_ceil(spec.width * spec.height, WORKGROUP_SIZE),
                        );
                    } else {
                        encode_dispatch(
                            context.device(),
                            &mut encoder,
                            pipelines,
                            &pipelines.resize_horizontal,
                            &params,
                            current,
                            next,
                            &pipelines.dummy_lut,
                            buffers,
                            div_ceil(spec.width * height, WORKGROUP_SIZE),
                        );
                        encode_dispatch(
                            context.device(),
                            &mut encoder,
                            pipelines,
                            &pipelines.resize_vertical,
                            &params,
                            current,
                            next,
                            &pipelines.dummy_lut,
                            buffers,
                            div_ceil(spec.width * spec.height, WORKGROUP_SIZE),
                        );
                    }
                    width = spec.width;
                    height = spec.height;
                    first_is_current = !first_is_current;
                }
                PostprocessOp::ConvertToGray(spec) => {
                    let mut params = GpuParams::new(width, height, format);
                    params.p0 = u32::from(spec.flat_weights);
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.to_gray,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        div_ceil(width * height, WORKGROUP_SIZE),
                    );
                    format = OutputFormat::Gray8;
                    first_is_current = !first_is_current;
                }
                PostprocessOp::ApplyToneCurve(curve) => {
                    let lut: Vec<u32> = curve.lut.iter().map(|&value| u32::from(value)).collect();
                    let lut_buffer =
                        context
                            .device()
                            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                label: Some("pdf-postprocess-tone-lut"),
                                contents: bytemuck::cast_slice(&lut),
                                usage: wgpu::BufferUsages::STORAGE,
                            });
                    let params = GpuParams::new(width, height, format);
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.tone,
                        &params,
                        current,
                        next,
                        &lut_buffer,
                        buffers,
                        div_ceil(width * height, WORKGROUP_SIZE),
                    );
                    first_is_current = !first_is_current;
                }
                PostprocessOp::Otsu(_) => {
                    let params = GpuParams::new(width, height, format);
                    encoder.clear_buffer(&buffers.aux, 0, Some(257 * 4));
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.otsu_histogram,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        div_ceil(width * height, WORKGROUP_SIZE),
                    );
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.otsu_find,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        1,
                    );
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.otsu_apply,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        div_ceil(width * height, WORKGROUP_SIZE),
                    );
                    first_is_current = !first_is_current;
                }
                PostprocessOp::Sauvola(spec) => {
                    let mut params = GpuParams::new(width, height, format);
                    params.p0 = spec.window;
                    params.p1 = spec.k.to_bits();
                    encode_integral(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &params,
                        current,
                        next,
                        buffers,
                    );
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.sauvola,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        div_ceil(width * height, WORKGROUP_SIZE),
                    );
                    first_is_current = !first_is_current;
                }
                PostprocessOp::FuseThresholds(spec) => {
                    let mut params = GpuParams::new(width, height, format);
                    params.p0 = spec.window;
                    params.p1 = spec.k.to_bits();
                    params.p2 = spec.global_weight.to_bits();
                    encode_integral(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &params,
                        current,
                        next,
                        buffers,
                    );
                    encoder.clear_buffer(&buffers.aux, 0, Some(257 * 4));
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.otsu_histogram,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        div_ceil(width * height, WORKGROUP_SIZE),
                    );
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.otsu_find,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        1,
                    );
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.fuse_thresholds,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        div_ceil(width * height, WORKGROUP_SIZE),
                    );
                    first_is_current = !first_is_current;
                }
                PostprocessOp::Dither(spec) => {
                    let mut params = GpuParams::new(width, height, format);
                    let (pipeline, groups) = match spec {
                        DitherSpec::None => {
                            params.p0 = 0;
                            (&pipelines.dither, div_ceil(width * height, WORKGROUP_SIZE))
                        }
                        DitherSpec::Bayer4 => {
                            params.p0 = 1;
                            (&pipelines.dither, div_ceil(width * height, WORKGROUP_SIZE))
                        }
                        DitherSpec::FloydSteinberg => {
                            encoder.clear_buffer(&buffers.aux, 0, None);
                            (&pipelines.floyd_steinberg, 1)
                        }
                    };
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        pipeline,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        groups,
                    );
                    first_is_current = !first_is_current;
                }
                PostprocessOp::PackMonochrome => {
                    let params = GpuParams::new(width, height, format);
                    let words = (layout.result_bytes as u32).div_ceil(4);
                    encode_dispatch(
                        context.device(),
                        &mut encoder,
                        pipelines,
                        &pipelines.pack_monochrome,
                        &params,
                        current,
                        next,
                        &pipelines.dummy_lut,
                        buffers,
                        div_ceil(words, WORKGROUP_SIZE),
                    );
                    first_is_current = !first_is_current;
                }
            }
        }

        let current = if first_is_current {
            &buffers.first
        } else {
            &buffers.second
        };
        let transfer_bytes = if layout.packed {
            layout.result_bytes.next_multiple_of(4)
        } else {
            checked_pixels(layout.final_width, layout.final_height)? * 4
        };
        encoder.copy_buffer_to_buffer(current, 0, &buffers.readback, 0, transfer_bytes as u64);
        let submission = context.queue().submit(std::iter::once(encoder.finish()));

        let slice = buffers.readback.slice(..transfer_bytes as u64);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        context
            .wait_for_submission(submission)
            .map_err(|error| PostprocessError::Failed(format!("GPU poll failed: {error}")))?;
        receiver
            .recv()
            .map_err(|_| PostprocessError::Failed("GPU map callback disconnected".to_owned()))?
            .map_err(|error| {
                PostprocessError::Failed(format!("GPU readback mapping failed: {error}"))
            })?;
        let mapped = slice.get_mapped_range().map_err(|error| {
            PostprocessError::Failed(format!("GPU readback range failed: {error}"))
        })?;
        let output = decode_output(&mapped, layout)?;
        drop(mapped);
        buffers.readback.unmap();

        Ok(GpuExecution {
            output,
            uploaded_bytes: (upload.len() * 4) as u64,
            readback_bytes: transfer_bytes as u64,
            elapsed: started.elapsed(),
        })
    }
}

fn encode_dispatch(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &PipelineSet,
    pipeline: &wgpu::ComputePipeline,
    params: &GpuParams,
    source: &wgpu::Buffer,
    destination: &wgpu::Buffer,
    lut: &wgpu::Buffer,
    buffers: &GpuBuffers,
    groups: u32,
) {
    let params_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("pdf-postprocess-params"),
        contents: bytemuck::bytes_of(params),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("pdf-postprocess-bind-group"),
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
                resource: lut.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: buffers.aux.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: buffers.resize_mid.as_entire_binding(),
            },
        ],
    });
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some("pdf-postprocess-pass"),
        timestamp_writes: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, &bind_group, &[]);
    pass.dispatch_workgroups(groups.max(1), 1, 1);
}

fn encode_integral(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    pipelines: &PipelineSet,
    params: &GpuParams,
    source: &wgpu::Buffer,
    destination: &wgpu::Buffer,
    buffers: &GpuBuffers,
) {
    encode_dispatch(
        device,
        encoder,
        pipelines,
        &pipelines.integral_rows,
        params,
        source,
        destination,
        &pipelines.dummy_lut,
        buffers,
        div_ceil(params.src_height, WORKGROUP_SIZE),
    );
    encode_dispatch(
        device,
        encoder,
        pipelines,
        &pipelines.integral_columns,
        params,
        source,
        destination,
        &pipelines.dummy_lut,
        buffers,
        div_ceil(params.src_width, WORKGROUP_SIZE),
    );
}

fn pack_source(source: &HostPage) -> Result<Vec<u32>, PostprocessError> {
    let count = checked_pixels(source.width, source.height)?;
    let bpp = source.format.bytes_per_pixel();
    let row_bytes = source.width as usize * bpp;
    let mut upload = Vec::with_capacity(count);
    for y in 0..source.height as usize {
        let row = &source.pixels[y * source.stride..y * source.stride + row_bytes];
        match source.format {
            OutputFormat::Gray8 => upload.extend(row.iter().map(|&value| u32::from(value))),
            OutputFormat::Rgba8PremultipliedSrgb => {
                upload.extend(
                    row.chunks_exact(4)
                        .map(|pixel| u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]])),
                );
            }
        }
    }
    Ok(upload)
}

fn decode_output(
    mapped: &[u8],
    layout: &GraphLayout,
) -> Result<PostprocessOutput, PostprocessError> {
    if layout.packed {
        let bits: Arc<[u8]> = Arc::from(mapped[..layout.result_bytes].to_vec().into_boxed_slice());
        return Ok(PostprocessOutput::PackedMono {
            width: layout.final_width,
            height: layout.final_height,
            stride: layout.packed_stride,
            bits,
        });
    }
    let pixels = checked_pixels(layout.final_width, layout.final_height)?;
    let data = match layout.final_format {
        OutputFormat::Rgba8PremultipliedSrgb => mapped[..pixels * 4].to_vec(),
        OutputFormat::Gray8 => mapped
            .chunks_exact(4)
            .take(pixels)
            .map(|word| word[0])
            .collect(),
    };
    Ok(PostprocessOutput::Page(RenderedPage::Host(HostPage {
        width: layout.final_width,
        height: layout.final_height,
        stride: layout.final_width as usize * layout.final_format.bytes_per_pixel(),
        format: layout.final_format,
        pixels: Arc::from(data.into_boxed_slice()),
    })))
}

struct SessionPool {
    sessions: Mutex<Vec<GpuSession>>,
    available: Condvar,
}

impl SessionPool {
    fn new() -> Self {
        let count = std::env::var("LEGE_GPU_POSTPROCESS_SESSIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(2)
            .clamp(1, 4);
        Self {
            sessions: Mutex::new((0..count).map(|_| GpuSession::new()).collect()),
            available: Condvar::new(),
        }
    }

    fn checkout(&self) -> SessionLease<'_> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while sessions.is_empty() {
            sessions = self
                .available
                .wait(sessions)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        SessionLease {
            pool: self,
            session: sessions.pop(),
        }
    }
}

struct SessionLease<'a> {
    pool: &'a SessionPool,
    session: Option<GpuSession>,
}

impl std::ops::Deref for SessionLease<'_> {
    type Target = GpuSession;

    fn deref(&self) -> &Self::Target {
        self.session
            .as_ref()
            .unwrap_or_else(|| unreachable!("GPU postprocess lease is empty"))
    }
}

impl std::ops::DerefMut for SessionLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.session
            .as_mut()
            .unwrap_or_else(|| unreachable!("GPU postprocess lease is empty"))
    }
}

impl Drop for SessionLease<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let mut sessions = self
                .pool
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.push(session);
            self.pool.available.notify_one();
        }
    }
}

pub(crate) struct GpuExecution {
    pub(crate) output: PostprocessOutput,
    pub(crate) uploaded_bytes: u64,
    pub(crate) readback_bytes: u64,
    #[allow(dead_code)]
    pub(crate) elapsed: Duration,
}

/// Experimental resident WGPU executor for the complete postprocess graph.
pub struct WgpuPostprocess {
    context: SharedGpuContext,
    adapter: AdapterInfo,
    pipelines: Arc<PipelineSet>,
    pool: SessionPool,
}

impl std::fmt::Debug for WgpuPostprocess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgpuPostprocess")
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

impl WgpuPostprocess {
    pub fn new() -> Result<Self, PostprocessError> {
        let context = SharedGpuContext::get().map_err(|error| {
            PostprocessError::Failed(format!("GPU postprocess initialization failed: {error}"))
        })?;
        let adapter = context.adapter_info();
        let pipelines = Arc::new(PipelineSet::new(&context)?);
        Ok(Self {
            context,
            adapter,
            pipelines,
            pool: SessionPool::new(),
        })
    }

    pub fn adapter_info(&self) -> &AdapterInfo {
        &self.adapter
    }

    pub fn is_hardware_gpu(&self) -> bool {
        self.adapter.is_hardware_gpu()
    }

    pub(crate) fn execute_measured(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<GpuExecution, PostprocessError> {
        let layout = analyze(source, graph)?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.pool
                .checkout()
                .execute(&self.context, &self.pipelines, source, graph, &layout)
        }));
        match result {
            Ok(execution) => execution,
            Err(payload) => Err(PostprocessError::Failed(format!(
                "GPU postprocess execution panicked: {}",
                pdf_render_api::panic_message(payload)
            ))),
        }
    }
}

impl PostprocessBackend for WgpuPostprocess {
    fn capabilities(&self) -> PostprocessCapabilities {
        PostprocessCapabilities {
            operations: PostprocessOperations::all(),
            resident_execution: true,
        }
    }

    fn supports(&self, graph: &PostprocessGraph) -> bool {
        CpuPostprocess.supports(graph)
    }

    fn execute(
        &self,
        source: &HostPage,
        graph: &PostprocessGraph,
    ) -> Result<PostprocessOutput, PostprocessError> {
        Ok(self.execute_measured(source, graph)?.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CropSpec, FusionSpec, GraySpec, OtsuSpec, ResizeSpec, SauvolaSpec, ToneCurve};

    fn page(width: u32, height: u32, format: OutputFormat, pixels: Vec<u8>) -> HostPage {
        HostPage {
            width,
            height,
            stride: width as usize * format.bytes_per_pixel(),
            format,
            pixels: Arc::from(pixels.into_boxed_slice()),
        }
    }

    fn output_bytes(output: &PostprocessOutput) -> Vec<u8> {
        match output {
            PostprocessOutput::Page(RenderedPage::Host(page)) => page.pixels.to_vec(),
            PostprocessOutput::PackedMono { bits, .. } => bits.to_vec(),
            PostprocessOutput::Page(RenderedPage::Resident(_)) => {
                unreachable!("postprocess executor returns host output")
            }
        }
    }

    #[test]
    fn discrete_graph_matches_cpu_when_wgpu_is_available() {
        let Ok(gpu) = WgpuPostprocess::new() else {
            return;
        };
        let source = page(
            6,
            5,
            OutputFormat::Rgba8PremultipliedSrgb,
            (0..30)
                .flat_map(|index| {
                    let alpha = 128 + (index % 128) as u8;
                    [
                        ((index * 7) as u8).min(alpha),
                        ((index * 5) as u8).min(alpha),
                        ((index * 3) as u8).min(alpha),
                        alpha,
                    ]
                })
                .collect(),
        );
        let graph = PostprocessGraph {
            ops: vec![
                PostprocessOp::Crop(CropSpec {
                    x: 1,
                    y: 1,
                    width: 4,
                    height: 3,
                }),
                PostprocessOp::ApplyToneCurve(ToneCurve::invert()),
                PostprocessOp::ConvertToGray(GraySpec::default()),
                PostprocessOp::Dither(DitherSpec::Bayer4),
                PostprocessOp::PackMonochrome,
            ],
        };
        let cpu = CpuPostprocess.execute(&source, &graph).unwrap();
        let actual = gpu.execute(&source, &graph).unwrap();
        assert_eq!(output_bytes(&actual), output_bytes(&cpu));
    }

    #[test]
    fn threshold_graphs_match_cpu_when_wgpu_is_available() {
        let Ok(gpu) = WgpuPostprocess::new() else {
            return;
        };
        let gray: Vec<u8> = (0..63)
            .map(|index| ((index * 47 + index / 3 * 11) & 255) as u8)
            .collect();
        let source = page(9, 7, OutputFormat::Gray8, gray);
        for op in [
            PostprocessOp::Otsu(OtsuSpec::default()),
            PostprocessOp::Sauvola(SauvolaSpec { window: 5, k: 0.3 }),
            PostprocessOp::FuseThresholds(FusionSpec {
                global_weight: 0.35,
                window: 5,
                k: 0.25,
            }),
            PostprocessOp::Dither(DitherSpec::FloydSteinberg),
        ] {
            let graph = PostprocessGraph { ops: vec![op] };
            let cpu = CpuPostprocess.execute(&source, &graph).unwrap();
            let actual = gpu.execute(&source, &graph).unwrap();
            assert_eq!(output_bytes(&actual), output_bytes(&cpu));
        }
    }

    #[test]
    fn smooth_resize_is_within_one_lsb_when_wgpu_is_available() {
        let Ok(gpu) = WgpuPostprocess::new() else {
            return;
        };
        let source = page(
            8,
            6,
            OutputFormat::Gray8,
            (0..48).map(|index| (index * 13) as u8).collect(),
        );
        for filter in [
            ResizeFilter::Box,
            ResizeFilter::Bilinear,
            ResizeFilter::CatmullRom,
            ResizeFilter::Lanczos3,
        ] {
            let graph = PostprocessGraph {
                ops: vec![PostprocessOp::Resize(ResizeSpec {
                    width: 5,
                    height: 9,
                    filter,
                })],
            };
            let cpu = output_bytes(&CpuPostprocess.execute(&source, &graph).unwrap());
            let actual = output_bytes(&gpu.execute(&source, &graph).unwrap());
            assert_eq!(actual.len(), cpu.len());
            assert!(
                actual
                    .iter()
                    .zip(&cpu)
                    .all(|(&left, &right)| left.abs_diff(right) <= 1),
                "{filter:?} exceeded the one-LSB parity gate"
            );
        }
    }
}
