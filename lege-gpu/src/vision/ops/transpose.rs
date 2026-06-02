use anyhow::{Result, bail};

use super::common::{c_strides_raw, c_strides_u32, f32_from_bytes, linear_grid};
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params layout (16 x u32):
//   [0] num_elems  [1] rank  [2] _pad  [3] _pad
//   [4..9]  out_strides[0..5]
//   [10..15] in_perm_strides[0..5]  (input_stride[perm[d]])
pub(crate) const TRANSPOSE_WGSL: &str = r#"
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
    var in_idx : u32 = 0u;
    for (var d: u32 = 0u; d < rank; d = d + 1u) {
        let s     = params[4u + d];
        let coord = remaining / s;
        remaining  = remaining % s;
        in_idx = in_idx + coord * params[10u + d];
    }
    out[i] = inp[in_idx];
}
"#;

pub(crate) async fn run_transpose(
    ctx: &GpuContext,
    perm: &[usize],
    input: &Tensor,
) -> Result<Tensor> {
    let rank = input.shape.len();
    if perm.len() != rank {
        bail!("Transpose perm rank mismatch");
    }
    if rank > 6 {
        bail!("Transpose GPU: rank {rank} exceeds maximum 6");
    }

    let out_shape: Vec<usize> = perm.iter().map(|&p| input.shape[p]).collect();
    let num_elems = out_shape.iter().product::<usize>();
    let out_s = c_strides_u32(&out_shape);

    let in_s = c_strides_raw(&input.shape);
    let mut in_perm_s = [0u32; 6];
    for d in 0..rank {
        in_perm_s[d] = in_s[perm[d]] as u32;
    }

    let mut params = [0u32; 16];
    params[0] = num_elems as u32;
    params[1] = rank as u32;
    params[4..10].copy_from_slice(&out_s);
    params[10..16].copy_from_slice(&in_perm_s);

    let raw = dispatch_compute(
        ctx,
        TRANSPOSE_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        num_elems * 4,
        bytemuck::cast_slice(&params),
        linear_grid(num_elems.div_ceil(256)),
    )
    .await?;

    Tensor::new(out_shape, f32_from_bytes(&raw))
}
