//! Structural kernels used by the sauvola binarization model: single-axis
//! inclusive CumSum (integral images), single-axis ReduceSum, and the
//! SpaceToDepth/DepthToSpace block shuffles (DCR mode, the ONNX default).
//!
//! All four treat tensors as flat row-major buffers and gather per element, so
//! they are rank- and layout-agnostic; correctness, not speed, is the goal
//! (the production ORT path is 1-3 s/page).

use anyhow::{Result, bail};

use super::common::{f32_from_bytes, linear_grid};
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// One thread per (outer, inner) line; each scans the axis sequentially.
// params: [num_lines = outer*inner, axis_dim, inner].
pub(crate) const CUMSUM_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let t = gid.y * ng.x * 256u + gid.x;
    if t >= params[0] { return; }
    let axis_dim = params[1];
    let inner    = params[2];
    let o = t / inner;
    let i = t % inner;
    var acc = 0.0;
    for (var k: u32 = 0u; k < axis_dim; k = k + 1u) {
        let idx = (o * axis_dim + k) * inner + i;
        acc = acc + inp[idx];
        out[idx] = acc;
    }
}
"#;

// One thread per output element; sums the reduced axis.
// params: [num_out = outer*inner, axis_dim, inner].
pub(crate) const REDUCESUM_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let t = gid.y * ng.x * 256u + gid.x;
    if t >= params[0] { return; }
    let axis_dim = params[1];
    let inner    = params[2];
    let o = t / inner;
    let i = t % inner;
    var acc = 0.0;
    for (var k: u32 = 0u; k < axis_dim; k = k + 1u) {
        acc = acc + inp[(o * axis_dim + k) * inner + i];
    }
    out[t] = acc;
}
"#;

// SpaceToDepth (DCR): [N,C,H,W] -> [N,C*b*b,H/b,W/b].
// out channel oc decodes to (bh, bw, c); input pulls from (oh*b+bh, ow*b+bw).
// params: [num_out, C, H, W, b].
pub(crate) const SPACE_TO_DEPTH_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let t = gid.y * ng.x * 256u + gid.x;
    if t >= params[0] { return; }
    let c_in = params[1];
    let h_in = params[2];
    let w_in = params[3];
    let b    = params[4];
    let oc_n = c_in * b * b;
    let oh_n = h_in / b;
    let ow_n = w_in / b;

    var r = t;
    let ow = r % ow_n; r = r / ow_n;
    let oh = r % oh_n; r = r / oh_n;
    let oc = r % oc_n;
    let n  = r / oc_n;

    let bh = oc / (b * c_in);
    let rem = oc % (b * c_in);
    let bw = rem / c_in;
    let c  = rem % c_in;

    let ih = oh * b + bh;
    let iw = ow * b + bw;
    out[t] = inp[((n * c_in + c) * h_in + ih) * w_in + iw];
}
"#;

// DepthToSpace (DCR): [N,C,H,W] -> [N,C/(b*b),H*b,W*b].
// input channel ic = b0*b*Cout + b1*Cout + oc for output spatial parity (b0,b1).
// params: [num_out, C, H, W, b].
pub(crate) const DEPTH_TO_SPACE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let t = gid.y * ng.x * 256u + gid.x;
    if t >= params[0] { return; }
    let c_in = params[1];
    let h_in = params[2];
    let w_in = params[3];
    let b    = params[4];
    let oc_n = c_in / (b * b);
    let oh_n = h_in * b;
    let ow_n = w_in * b;

    var r = t;
    let ow = r % ow_n; r = r / ow_n;
    let oh = r % oh_n; r = r / oh_n;
    let oc = r % oc_n;
    let n  = r / oc_n;

    let h  = oh / b;
    let b0 = oh % b;
    let w  = ow / b;
    let b1 = ow % b;
    let ic = b0 * b * oc_n + b1 * oc_n + oc;
    out[t] = inp[((n * c_in + ic) * h_in + h) * w_in + w];
}
"#;

/// Splits a flat row-major tensor at `axis` into (outer, axis_dim, inner).
fn axis_split(shape: &[usize], axis: usize) -> (usize, usize, usize) {
    let outer = shape[..axis].iter().product::<usize>();
    let axis_dim = shape[axis];
    let inner = shape[axis + 1..].iter().product::<usize>();
    (outer, axis_dim, inner)
}

pub(crate) async fn run_cumsum(ctx: &GpuContext, input: &Tensor, axis: usize) -> Result<Tensor> {
    if axis >= input.shape.len() {
        bail!(
            "CumSum axis {axis} out of range for rank {}",
            input.shape.len()
        );
    }
    let (outer, axis_dim, inner) = axis_split(&input.shape, axis);
    let lines = outer * inner;
    let params = [lines as u32, axis_dim as u32, inner as u32];
    let raw = dispatch_compute(
        ctx,
        CUMSUM_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        input.data.len() * 4,
        bytemuck::cast_slice(&params),
        linear_grid(lines.div_ceil(256)),
    )
    .await?;
    Tensor::new(input.shape.clone(), f32_from_bytes(&raw))
}

pub(crate) async fn run_reduce_sum(
    ctx: &GpuContext,
    input: &Tensor,
    axis: usize,
    keepdims: bool,
) -> Result<Tensor> {
    if axis >= input.shape.len() {
        bail!(
            "ReduceSum axis {axis} out of range for rank {}",
            input.shape.len()
        );
    }
    let (outer, axis_dim, inner) = axis_split(&input.shape, axis);
    let num_out = outer * inner;
    let mut out_shape = input.shape.clone();
    if keepdims {
        out_shape[axis] = 1;
    } else {
        out_shape.remove(axis);
    }
    let params = [num_out as u32, axis_dim as u32, inner as u32];
    let raw = dispatch_compute(
        ctx,
        REDUCESUM_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        num_out * 4,
        bytemuck::cast_slice(&params),
        linear_grid(num_out.div_ceil(256)),
    )
    .await?;
    Tensor::new(out_shape, f32_from_bytes(&raw))
}

pub(crate) async fn run_space_to_depth(
    ctx: &GpuContext,
    input: &Tensor,
    blocksize: usize,
) -> Result<Tensor> {
    let s = &input.shape;
    if s.len() != 4 {
        bail!("SpaceToDepth expects rank-4 input, got {s:?}");
    }
    if s[2] % blocksize != 0 || s[3] % blocksize != 0 {
        bail!("SpaceToDepth: H/W not divisible by blocksize {blocksize}");
    }
    let out_shape = vec![
        s[0],
        s[1] * blocksize * blocksize,
        s[2] / blocksize,
        s[3] / blocksize,
    ];
    let num_out = out_shape.iter().product::<usize>();
    let params = [
        num_out as u32,
        s[1] as u32,
        s[2] as u32,
        s[3] as u32,
        blocksize as u32,
    ];
    let raw = dispatch_compute(
        ctx,
        SPACE_TO_DEPTH_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        num_out * 4,
        bytemuck::cast_slice(&params),
        linear_grid(num_out.div_ceil(256)),
    )
    .await?;
    Tensor::new(out_shape, f32_from_bytes(&raw))
}

pub(crate) async fn run_depth_to_space(
    ctx: &GpuContext,
    input: &Tensor,
    blocksize: usize,
) -> Result<Tensor> {
    let s = &input.shape;
    if s.len() != 4 {
        bail!("DepthToSpace expects rank-4 input, got {s:?}");
    }
    if s[1] % (blocksize * blocksize) != 0 {
        bail!(
            "DepthToSpace: C not divisible by blocksize^2 {}",
            blocksize * blocksize
        );
    }
    let out_shape = vec![
        s[0],
        s[1] / (blocksize * blocksize),
        s[2] * blocksize,
        s[3] * blocksize,
    ];
    let num_out = out_shape.iter().product::<usize>();
    let params = [
        num_out as u32,
        s[1] as u32,
        s[2] as u32,
        s[3] as u32,
        blocksize as u32,
    ];
    let raw = dispatch_compute(
        ctx,
        DEPTH_TO_SPACE_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        num_out * 4,
        bytemuck::cast_slice(&params),
        linear_grid(num_out.div_ceil(256)),
    )
    .await?;
    Tensor::new(out_shape, f32_from_bytes(&raw))
}
