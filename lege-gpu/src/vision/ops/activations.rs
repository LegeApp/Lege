//! Pointwise activation kernels: Relu, HardSwish, HardSigmoid.
//! Same binding layout as `sigmoid.rs`: one input, one output, a params buffer.

use anyhow::Result;

use super::common::{f32_from_bytes, linear_grid};
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

pub(crate) const RELU_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    out[i] = max(0.0, inp[i]);
}
"#;

// HardSwish(x) = x * clamp(x / 6 + 0.5, 0, 1)  (ONNX fixed alpha=1/6, beta=0.5).
pub(crate) const HARDSWISH_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let x = inp[i];
    out[i] = x * clamp(x / 6.0 + 0.5, 0.0, 1.0);
}
"#;

// HardSigmoid(x) = clamp(alpha*x + beta, 0, 1).
// params[0] = len, params[1] = bitcast(alpha), params[2] = bitcast(beta).
pub(crate) const HARDSIGMOID_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let alpha = bitcast<f32>(params[1]);
    let beta  = bitcast<f32>(params[2]);
    out[i] = clamp(alpha * inp[i] + beta, 0.0, 1.0);
}
"#;

pub(crate) const SQRT_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    out[i] = sqrt(inp[i]);
}
"#;

// Pow(x, e) for a constant scalar exponent. WGSL `pow` is undefined for x < 0,
// so we reconstruct the sign for integer exponents (matches Rust's powf, which
// the reference uses). params[1] = bitcast(exponent).
pub(crate) const POW_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let x = inp[i];
    let e = bitcast<f32>(params[1]);
    if x >= 0.0 {
        out[i] = pow(x, e);
    } else if e == round(e) {
        // Integer exponent: |x|^e with sign from odd/even parity.
        let mag = pow(-x, e);
        let odd = (e - 2.0 * floor(e * 0.5)) != 0.0;
        out[i] = select(mag, -mag, odd);
    } else {
        out[i] = pow(x, e);
    }
}
"#;

pub(crate) async fn run_sqrt(ctx: &GpuContext, input: &Tensor) -> Result<Tensor> {
    run_simple(ctx, SQRT_WGSL, input, &[input.data.len() as u32]).await
}

pub(crate) async fn run_pow(ctx: &GpuContext, input: &Tensor, exponent: f32) -> Result<Tensor> {
    let params = [input.data.len() as u32, exponent.to_bits()];
    run_simple(ctx, POW_WGSL, input, &params).await
}

pub(crate) async fn run_relu(ctx: &GpuContext, input: &Tensor) -> Result<Tensor> {
    run_simple(ctx, RELU_WGSL, input, &[input.data.len() as u32]).await
}

pub(crate) async fn run_hardswish(ctx: &GpuContext, input: &Tensor) -> Result<Tensor> {
    run_simple(ctx, HARDSWISH_WGSL, input, &[input.data.len() as u32]).await
}

pub(crate) async fn run_hardsigmoid(
    ctx: &GpuContext,
    input: &Tensor,
    alpha: f32,
    beta: f32,
) -> Result<Tensor> {
    let params = [input.data.len() as u32, alpha.to_bits(), beta.to_bits()];
    run_simple(ctx, HARDSIGMOID_WGSL, input, &params).await
}

async fn run_simple(
    ctx: &GpuContext,
    wgsl: &str,
    input: &Tensor,
    params: &[u32],
) -> Result<Tensor> {
    let len = input.data.len();
    let raw = dispatch_compute(
        ctx,
        wgsl,
        &[bytemuck::cast_slice(&input.data)],
        len * 4,
        bytemuck::cast_slice(params),
        linear_grid(len.div_ceil(256)),
    )
    .await?;
    Tensor::new(input.shape.clone(), f32_from_bytes(&raw))
}
