use anyhow::Result;

use super::common::f32_from_bytes;
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

pub(crate) const SIGMOID_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }
    out[i] = 1.0 / (1.0 + exp(-inp[i]));
}
"#;

// SiLU = x * sigmoid(x) = x / (1 + exp(-x))
// Same binding layout as SIGMOID_WGSL; used for fused Sigmoid+Mul patterns.
pub(crate) const SILU_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }
    let x = inp[i];
    out[i] = x / (1.0 + exp(-x));
}
"#;

// Vec4 variants: each thread processes 4 elements → 4× work per dispatch,
// 128-bit coalesced loads/stores, same pipeline layout.
// params[0] = len / 4  (caller ensures len % 4 == 0)
pub(crate) const SIGMOID_VEC4_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> out    : array<vec4<f32>>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }
    let x = inp[i];
    out[i] = vec4<f32>(1.0) / (vec4<f32>(1.0) + exp(-x));
}
"#;

pub(crate) const SILU_VEC4_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> out    : array<vec4<f32>>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }
    let x = inp[i];
    out[i] = x / (vec4<f32>(1.0) + exp(-x));
}
"#;

pub(crate) async fn run_sigmoid(ctx: &GpuContext, input: &Tensor) -> Result<Tensor> {
    let len = input.data.len();
    let params = [len as u32];
    let raw = dispatch_compute(
        ctx,
        SIGMOID_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        len * 4,
        bytemuck::cast_slice(&params),
        (len.div_ceil(256) as u32, 1, 1),
    )
    .await?;
    Tensor::new(input.shape.clone(), f32_from_bytes(&raw))
}
