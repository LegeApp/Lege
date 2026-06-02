use anyhow::{Result, bail};

use super::common::linear_grid;
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, map_readback, storage_bgl_entries};

// params layout (8 x u32):
//   [0] num_elems (this slice)
//   [1] inner_stride  [2] local_axis_size  [3] axis_offset  [4] total_axis_size
pub(crate) const CONCAT_SLICE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       src    : array<f32>;
@group(0) @binding(1) var<storage, read_write> dst    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }

    let inner_stride    = params[1];
    let local_axis_size = params[2];
    let axis_offset     = params[3];
    let total_axis      = params[4];

    let inner_idx      = i % inner_stride;
    let local_axis_idx = (i / inner_stride) % local_axis_size;
    let outer_idx      = i / (inner_stride * local_axis_size);

    let dst_idx = outer_idx * (total_axis * inner_stride)
                + (axis_offset + local_axis_idx) * inner_stride
                + inner_idx;
    dst[dst_idx] = src[i];
}
"#;

pub(crate) async fn run_concat(ctx: &GpuContext, axis: usize, inputs: &[Tensor]) -> Result<Tensor> {
    use crate::vision::wgpu::util::DeviceExt;

    if inputs.is_empty() {
        bail!("Concat needs at least one input");
    }
    let rank = inputs[0].shape.len();
    if axis >= rank {
        bail!("Concat axis {axis} out of range for rank {rank}");
    }

    let mut out_shape = inputs[0].shape.clone();
    out_shape[axis] = 0;
    for inp in inputs {
        if inp.shape.len() != rank {
            bail!("Concat rank mismatch");
        }
        for d in 0..rank {
            if d == axis {
                out_shape[d] += inp.shape[d];
            } else if inp.shape[d] != out_shape[d] {
                bail!("Concat dimension mismatch at dim {d}");
            }
        }
    }

    let total_elems = out_shape.iter().product::<usize>();
    let total_axis = out_shape[axis];
    let inner_stride: usize = out_shape[axis + 1..].iter().product();

    let shader = ctx
        .device
        .create_shader_module(crate::vision::wgpu::ShaderModuleDescriptor {
            label: Some("concat slice"),
            source: crate::vision::wgpu::ShaderSource::Wgsl(CONCAT_SLICE_WGSL.into()),
        });
    let bgl =
        ctx.device
            .create_bind_group_layout(&crate::vision::wgpu::BindGroupLayoutDescriptor {
                label: Some("concat slice bgl"),
                entries: &storage_bgl_entries(&[true, false, true]),
            });
    let pipeline =
        ctx.device
            .create_compute_pipeline(&crate::vision::wgpu::ComputePipelineDescriptor {
                label: Some("concat slice pipeline"),
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

    let out_buf = ctx
        .device
        .create_buffer(&crate::vision::wgpu::BufferDescriptor {
            label: Some("concat output"),
            size: (total_elems * 4) as u64,
            usage: crate::vision::wgpu::BufferUsages::STORAGE
                | crate::vision::wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
    let readback = ctx
        .device
        .create_buffer(&crate::vision::wgpu::BufferDescriptor {
            label: Some("concat readback"),
            size: (total_elems * 4) as u64,
            usage: crate::vision::wgpu::BufferUsages::MAP_READ
                | crate::vision::wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

    let mut encoder =
        ctx.device
            .create_command_encoder(&crate::vision::wgpu::CommandEncoderDescriptor {
                label: Some("concat"),
            });

    let mut axis_offset: u32 = 0;
    for inp in inputs {
        let local_elems = inp.data.len();
        let local_axis = inp.shape[axis] as u32;

        let params: [u32; 8] = [
            local_elems as u32,
            inner_stride as u32,
            local_axis,
            axis_offset,
            total_axis as u32,
            0,
            0,
            0,
        ];

        let src_buf =
            ctx.device
                .create_buffer_init(&crate::vision::wgpu::util::BufferInitDescriptor {
                    label: Some("concat src"),
                    contents: bytemuck::cast_slice(&inp.data),
                    usage: crate::vision::wgpu::BufferUsages::STORAGE,
                });
        let params_buf =
            ctx.device
                .create_buffer_init(&crate::vision::wgpu::util::BufferInitDescriptor {
                    label: Some("concat params"),
                    contents: bytemuck::cast_slice(&params),
                    usage: crate::vision::wgpu::BufferUsages::STORAGE,
                });
        let bg = ctx
            .device
            .create_bind_group(&crate::vision::wgpu::BindGroupDescriptor {
                label: Some("concat slice bg"),
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

        {
            let mut pass =
                encoder.begin_compute_pass(&crate::vision::wgpu::ComputePassDescriptor {
                    label: Some("concat slice pass"),
                    timestamp_writes: None,
                });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bg, &[]);
            let (gx, gy, gz) = linear_grid(local_elems.div_ceil(256));
            pass.dispatch_workgroups(gx, gy, gz);
        }

        axis_offset += local_axis;
    }

    encoder.copy_buffer_to_buffer(&out_buf, 0, &readback, 0, (total_elems * 4) as u64);
    ctx.queue.submit(Some(encoder.finish()));

    let raw = map_readback(&ctx.device, &readback, total_elems * 4).await?;
    Tensor::new(out_shape, bytemuck::cast_slice(&raw).to_vec())
}
