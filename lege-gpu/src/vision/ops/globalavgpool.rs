//! GlobalAveragePool: mean over all spatial dims per (batch, channel).
//! NCHW input [N, C, d0, d1, ...] -> [N, C, 1, 1, ...]. One invocation per (n, c)
//! averages the contiguous spatial plane that follows it in row-major layout.

use anyhow::{Result, bail};

use super::common::f32_from_bytes;
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params[0] = num_planes (N*C), params[1] = plane size (product of spatial dims).
pub(crate) const GLOBALAVGPOOL_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }
    let plane = params[1];
    let base = i * plane;
    var acc = 0.0;
    for (var p: u32 = 0u; p < plane; p = p + 1u) {
        acc = acc + inp[base + p];
    }
    out[i] = acc / f32(plane);
}
"#;

pub(crate) async fn run_global_avg_pool(ctx: &GpuContext, input: &Tensor) -> Result<Tensor> {
    if input.shape.len() < 2 {
        bail!("GlobalAveragePool expects rank >= 2, got {:?}", input.shape);
    }
    let num_planes = input.shape[0] * input.shape[1];
    let plane = input.shape[2..].iter().product::<usize>().max(1);
    let mut out_shape = input.shape.clone();
    for dim in out_shape.iter_mut().skip(2) {
        *dim = 1;
    }
    let params = [num_planes as u32, plane as u32];
    let raw = dispatch_compute(
        ctx,
        GLOBALAVGPOOL_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        num_planes * 4,
        bytemuck::cast_slice(&params),
        (num_planes.div_ceil(256) as u32, 1, 1),
    )
    .await?;
    Tensor::new(out_shape, f32_from_bytes(&raw))
}
