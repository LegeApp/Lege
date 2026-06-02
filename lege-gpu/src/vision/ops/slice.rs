use anyhow::{Result, bail};

use super::common::{c_strides_u32, f32_from_bytes, linear_grid};
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params (28 x u32):
//   [0] num_elems  [1] rank  [2..3] _pad
//   [4..9]  out_strides[0..5]
//   [10..15] in_start[0..5]
//   [16..21] in_step[0..5]
//   [22..27] in_strides[0..5]
pub(crate) const SLICE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let rank = params[1];

    var remaining = i;
    var in_idx: u32 = 0u;
    for (var d: u32 = 0u; d < rank; d = d + 1u) {
        let out_stride = params[4u + d];
        let coord      = remaining / out_stride;
        remaining       = remaining % out_stride;
        let in_coord   = params[10u + d] + coord * params[16u + d];
        in_idx = in_idx + in_coord * params[22u + d];
    }
    out[i] = inp[in_idx];
}
"#;

pub(crate) async fn run_slice(
    ctx: &GpuContext,
    axes: &[usize],
    starts: &[i64],
    ends: &[i64],
    steps: &[i64],
    input: &Tensor,
) -> Result<Tensor> {
    let rank = input.shape.len();
    if rank > 6 {
        bail!("Slice GPU: rank {rank} exceeds maximum 6");
    }

    let mut out_shape = input.shape.clone();
    let mut in_start_arr = [0u32; 6];
    let mut in_step_arr = [1u32; 6];

    for (&axis, (&start_raw, (&end_raw, &step_raw))) in axes
        .iter()
        .zip(starts.iter().zip(ends.iter().zip(steps.iter())))
    {
        let dim = input.shape[axis];
        let start = normalize_slice_idx(start_raw, dim);
        let end = normalize_slice_idx(end_raw, dim);
        let step = step_raw as usize;
        out_shape[axis] = end.saturating_sub(start).div_ceil(step);
        in_start_arr[axis] = start as u32;
        in_step_arr[axis] = step as u32;
    }

    let num_out = out_shape.iter().product::<usize>();
    if num_out == 0 {
        return Tensor::new(out_shape, Vec::new());
    }

    let out_s = c_strides_u32(&out_shape);
    let in_s = c_strides_u32(&input.shape);

    let mut p = [0u32; 28];
    p[0] = num_out as u32;
    p[1] = rank as u32;
    p[4..10].copy_from_slice(&out_s);
    p[10..16].copy_from_slice(&in_start_arr);
    p[16..22].copy_from_slice(&in_step_arr);
    p[22..28].copy_from_slice(&in_s);

    let raw = dispatch_compute(
        ctx,
        SLICE_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        num_out * 4,
        bytemuck::cast_slice(&p),
        linear_grid(num_out.div_ceil(256)),
    )
    .await?;

    Tensor::new(out_shape, f32_from_bytes(&raw))
}

fn normalize_slice_idx(idx: i64, dim: usize) -> usize {
    if idx < 0 {
        (idx + dim as i64).clamp(0, dim as i64) as usize
    } else {
        (idx as usize).min(dim)
    }
}
