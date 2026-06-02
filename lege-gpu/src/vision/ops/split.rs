use anyhow::{Result, bail};

use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, map_readback, storage_bgl_entries};

pub(crate) const SPLIT_SLICE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       src    : array<f32>;
@group(0) @binding(1) var<storage, read_write> dst    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }

    let inner_stride    = params[1];
    let local_axis_size = params[2];
    let axis_offset     = params[3];
    let total_axis      = params[4];

    let inner_idx      = i % inner_stride;
    let local_axis_idx = (i / inner_stride) % local_axis_size;
    let outer_idx      = i / (inner_stride * local_axis_size);

    let src_idx = outer_idx * (total_axis * inner_stride)
                + (axis_offset + local_axis_idx) * inner_stride
                + inner_idx;
    dst[i] = src[src_idx];
}
"#;

pub(crate) async fn run_split(
    ctx: &GpuContext,
    axis: usize,
    sizes: &[i64],
    input: &Tensor,
) -> Result<Vec<Tensor>> {
    use crate::vision::wgpu::util::DeviceExt;

    if axis >= input.shape.len() {
        bail!("Split axis {axis} out of range");
    }
    let total_axis = input.shape[axis];
    let inner_stride: usize = input.shape[axis + 1..].iter().product();

    let shader = ctx
        .device
        .create_shader_module(crate::vision::wgpu::ShaderModuleDescriptor {
            label: Some("split slice"),
            source: crate::vision::wgpu::ShaderSource::Wgsl(SPLIT_SLICE_WGSL.into()),
        });
    let bgl =
        ctx.device
            .create_bind_group_layout(&crate::vision::wgpu::BindGroupLayoutDescriptor {
                label: Some("split slice bgl"),
                entries: &storage_bgl_entries(&[true, false, true]),
            });
    let pipeline =
        ctx.device
            .create_compute_pipeline(&crate::vision::wgpu::ComputePipelineDescriptor {
                label: Some("split slice pipeline"),
                layout: Some(&ctx.device.create_pipeline_layout(
                    &crate::vision::wgpu::PipelineLayoutDescriptor {
                        label: None,
                        bind_group_layouts: &[Some(&bgl)],
                        immediate_size: 0,
                    },
                )),
                module: &shader,
                entry_point: Some("main"),
                compilation_options: crate::vision::wgpu::PipelineCompilationOptions::default(),
                cache: None,
            });

    let src_buf = ctx
        .device
        .create_buffer_init(&crate::vision::wgpu::util::BufferInitDescriptor {
            label: Some("split src"),
            contents: bytemuck::cast_slice(&input.data),
            usage: crate::vision::wgpu::BufferUsages::STORAGE,
        });

    let mut results = Vec::new();
    let mut axis_offset: u32 = 0;

    for &size in sizes {
        let local_axis = size as usize;
        let outer: usize = input.shape[..axis].iter().product();
        let num_elems = outer * local_axis * inner_stride;

        let out_buf = ctx
            .device
            .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                label: Some("split out"),
                size: (num_elems * 4) as u64,
                usage: crate::vision::wgpu::BufferUsages::STORAGE
                    | crate::vision::wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
        let readback = ctx
            .device
            .create_buffer(&crate::vision::wgpu::BufferDescriptor {
                label: Some("split readback"),
                size: (num_elems * 4) as u64,
                usage: crate::vision::wgpu::BufferUsages::MAP_READ
                    | crate::vision::wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

        let params: [u32; 8] = [
            num_elems as u32,
            inner_stride as u32,
            local_axis as u32,
            axis_offset,
            total_axis as u32,
            0,
            0,
            0,
        ];
        let params_buf =
            ctx.device
                .create_buffer_init(&crate::vision::wgpu::util::BufferInitDescriptor {
                    label: Some("split params"),
                    contents: bytemuck::cast_slice(&params),
                    usage: crate::vision::wgpu::BufferUsages::STORAGE,
                });
        let bg = ctx
            .device
            .create_bind_group(&crate::vision::wgpu::BindGroupDescriptor {
                label: Some("split bg"),
                layout: &bgl,
                entries: &[
                    crate::vision::wgpu::BindGroupEntry {
                        binding: 0,
                        resource: src_buf.as_entire_binding(),
                    },
                    crate::vision::wgpu::BindGroupEntry {
                        binding: 1,
                        resource: out_buf.as_entire_binding(),
                    },
                    crate::vision::wgpu::BindGroupEntry {
                        binding: 2,
                        resource: params_buf.as_entire_binding(),
                    },
                ],
            });

        let mut encoder =
            ctx.device
                .create_command_encoder(&crate::vision::wgpu::CommandEncoderDescriptor {
                    label: Some("split"),
                });
        {
            let mut pass =
                encoder.begin_compute_pass(&crate::vision::wgpu::ComputePassDescriptor {
                    label: Some("split pass"),
                    timestamp_writes: None,
                });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(num_elems.div_ceil(256) as u32, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&out_buf, 0, &readback, 0, (num_elems * 4) as u64);
        ctx.queue.submit(Some(encoder.finish()));

        let raw = map_readback(&ctx.device, &readback, num_elems * 4).await?;
        let mut out_shape = input.shape.clone();
        out_shape[axis] = local_axis;
        results.push(Tensor::new(out_shape, bytemuck::cast_slice(&raw).to_vec())?);

        axis_offset += local_axis as u32;
    }

    Ok(results)
}
