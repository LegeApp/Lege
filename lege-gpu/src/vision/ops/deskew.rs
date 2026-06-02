use anyhow::{Context, Result, bail};

use super::common::{broadcast_strides_u32, c_strides_u32, f32_from_bytes, linear_grid};
use crate::vision::onnx::types::PadMode;
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params:
// [0] len [1] rank
// [4..9] out_strides [10..15] input_strides [16..21] slope_strides
pub(crate) const PRELU_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       slope  : array<f32>;
@group(0) @binding(2) var<storage, read_write> out    : array<f32>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let rank = params[1];
    var rem = i;
    var in_idx = 0u;
    var slope_idx = 0u;
    for (var d = 0u; d < rank; d = d + 1u) {
        let coord = rem / params[4u + d];
        rem = rem % params[4u + d];
        in_idx = in_idx + coord * params[10u + d];
        slope_idx = slope_idx + coord * params[16u + d];
    }
    let x = inp[in_idx];
    out[i] = select(x, x * slope[slope_idx], x < 0.0);
}
"#;

// params:
// [0] len [1] rank [2] mode(0 constant, 1 reflect) [3] value_bits
// [4..9] out_shape [10..15] in_shape [16..21] pads_begin
// [22..27] out_strides [28..33] in_strides
pub(crate) const PAD_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

fn reflect_index(coord_raw: i32, dim: i32) -> i32 {
    if dim <= 1 { return 0; }
    var c = coord_raw;
    loop {
        if c < 0 {
            c = -c;
        } else if c >= dim {
            c = 2 * dim - 2 - c;
        } else {
            break;
        }
    }
    return c;
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let rank = params[1];
    let mode = params[2];
    let constant_value = bitcast<f32>(params[3]);
    var rem = i;
    var in_idx = 0u;
    var outside = false;
    for (var d = 0u; d < rank; d = d + 1u) {
        let coord = rem / params[22u + d];
        rem = rem % params[22u + d];
        let raw = i32(coord) - i32(params[16u + d]);
        let dim = i32(params[10u + d]);
        var in_coord = raw;
        if raw < 0 || raw >= dim {
            if mode == 1u {
                in_coord = reflect_index(raw, dim);
            } else {
                outside = true;
            }
        }
        in_idx = in_idx + u32(in_coord) * params[28u + d];
    }
    out[i] = select(inp[in_idx], constant_value, outside);
}
"#;

// params: [0] total [1] n [2] c [3] hin [4] win [5] hout [6] wout [7] align
pub(crate) const RESIZE_LINEAR_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

fn src_coord(o: u32, in_size: u32, out_size: u32, align: u32) -> f32 {
    if align != 0u {
        if out_size <= 1u { return 0.0; }
        return f32(o) * f32(in_size - 1u) / f32(out_size - 1u);
    }
    return (f32(o) + 0.5) * f32(in_size) / f32(out_size) - 0.5;
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let c = params[2];
    let hin = params[3];
    let win = params[4];
    let hout = params[5];
    let wout = params[6];
    let align = params[7];

    let ow = i % wout;
    let oh = (i / wout) % hout;
    let ch = (i / (hout * wout)) % c;
    let n = i / (c * hout * wout);

    let y = clamp(src_coord(oh, hin, hout, align), 0.0, f32(hin - 1u));
    let x = clamp(src_coord(ow, win, wout, align), 0.0, f32(win - 1u));
    let y0 = u32(floor(y));
    let x0 = u32(floor(x));
    let y1 = min(y0 + 1u, hin - 1u);
    let x1 = min(x0 + 1u, win - 1u);
    let wy = y - f32(y0);
    let wx = x - f32(x0);

    let base = ((n * c + ch) * hin) * win;
    let v00 = inp[base + y0 * win + x0];
    let v01 = inp[base + y0 * win + x1];
    let v10 = inp[base + y1 * win + x0];
    let v11 = inp[base + y1 * win + x1];
    let top = v00 * (1.0 - wx) + v01 * wx;
    let bot = v10 * (1.0 - wx) + v11 * wx;
    out[i] = top * (1.0 - wy) + bot * wy;
}
"#;

// params: [0] total [1] n [2] c [3] hin [4] win [5] hout [6] wout [7] align
pub(crate) const GRID_SAMPLE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       grid   : array<f32>;
@group(0) @binding(2) var<storage, read_write> out    : array<f32>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

fn unnormalize(v: f32, size: u32, align: u32) -> f32 {
    if align != 0u {
        return ((v + 1.0) * 0.5) * f32(size - 1u);
    }
    return ((v + 1.0) * f32(size) - 1.0) * 0.5;
}

fn sample(inp_idx_base: u32, hin: u32, win: u32, y: i32, x: i32) -> f32 {
    if y < 0 || x < 0 || y >= i32(hin) || x >= i32(win) {
        return 0.0;
    }
    return inp[inp_idx_base + u32(y) * win + u32(x)];
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let c = params[2];
    let hin = params[3];
    let win = params[4];
    let hout = params[5];
    let wout = params[6];
    let align = params[7];

    let ow = i % wout;
    let oh = (i / wout) % hout;
    let ch = (i / (hout * wout)) % c;
    let n = i / (c * hout * wout);
    let grid_base = ((n * hout + oh) * wout + ow) * 2u;
    let x = unnormalize(grid[grid_base], win, align);
    let y = unnormalize(grid[grid_base + 1u], hin, align);

    let x0 = i32(floor(x));
    let y0 = i32(floor(y));
    let x1 = x0 + 1;
    let y1 = y0 + 1;
    let wx = x - f32(x0);
    let wy = y - f32(y0);
    let base = ((n * c + ch) * hin) * win;
    let v00 = sample(base, hin, win, y0, x0);
    let v01 = sample(base, hin, win, y0, x1);
    let v10 = sample(base, hin, win, y1, x0);
    let v11 = sample(base, hin, win, y1, x1);
    let top = v00 * (1.0 - wx) + v01 * wx;
    let bot = v10 * (1.0 - wx) + v11 * wx;
    out[i] = top * (1.0 - wy) + bot * wy;
}
"#;

pub(crate) async fn run_prelu(ctx: &GpuContext, input: &Tensor, slope: &Tensor) -> Result<Tensor> {
    if input.shape.len() > 6 {
        bail!("PRelu rank exceeds 6");
    }
    let len = input.data.len();
    let mut p = [0u32; 22];
    p[0] = len as u32;
    p[1] = input.shape.len() as u32;
    p[4..10].copy_from_slice(&c_strides_u32(&input.shape));
    p[10..16].copy_from_slice(&c_strides_u32(&input.shape));
    p[16..22].copy_from_slice(&broadcast_strides_u32(&input.shape, &slope.shape));
    let raw = dispatch_compute(
        ctx,
        PRELU_WGSL,
        &[
            bytemuck::cast_slice(&input.data),
            bytemuck::cast_slice(&slope.data),
        ],
        len * 4,
        bytemuck::cast_slice(&p),
        linear_grid(len.div_ceil(256)),
    )
    .await?;
    Tensor::new(input.shape.clone(), f32_from_bytes(&raw))
}

pub(crate) fn run_unsqueeze(input: &Tensor, axes: &[usize]) -> Result<Tensor> {
    let mut shape = input.shape.clone();
    let mut axes = axes.to_vec();
    axes.sort_unstable();
    for axis in axes {
        shape.insert(axis.min(shape.len()), 1);
    }
    Tensor::new(shape, input.data.clone())
}

pub(crate) fn run_squeeze(input: &Tensor, axes: &[usize]) -> Result<Tensor> {
    let shape = input
        .shape
        .iter()
        .enumerate()
        .filter_map(|(axis, dim)| (!axes.contains(&axis)).then_some(*dim))
        .collect::<Vec<_>>();
    Tensor::new(shape, input.data.clone())
}

pub(crate) async fn run_pad(
    ctx: &GpuContext,
    pads: &[i64],
    mode: &PadMode,
    value: f32,
    input: &Tensor,
) -> Result<Tensor> {
    let rank = input.shape.len();
    if rank > 6 || pads.len() != rank * 2 {
        bail!("Pad expects rank <= 6 and 2*rank pads");
    }
    let out_shape = (0..rank)
        .map(|axis| usize::try_from(input.shape[axis] as i64 + pads[axis] + pads[axis + rank]))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Pad output dim must be non-negative")?;
    let len = out_shape.iter().product::<usize>();
    let mode_u32 = match mode {
        PadMode::Constant => 0u32,
        PadMode::Reflect => 1u32,
    };
    let mut p = [0u32; 34];
    p[0] = len as u32;
    p[1] = rank as u32;
    p[2] = mode_u32;
    p[3] = value.to_bits();
    for axis in 0..rank {
        p[4 + axis] = out_shape[axis] as u32;
        p[10 + axis] = input.shape[axis] as u32;
        p[16 + axis] = pads[axis] as u32;
    }
    p[22..28].copy_from_slice(&c_strides_u32(&out_shape));
    p[28..34].copy_from_slice(&c_strides_u32(&input.shape));
    let raw = dispatch_compute(
        ctx,
        PAD_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        len * 4,
        bytemuck::cast_slice(&p),
        linear_grid(len.div_ceil(256)),
    )
    .await?;
    Tensor::new(out_shape, f32_from_bytes(&raw))
}

pub(crate) async fn run_resize_linear(
    ctx: &GpuContext,
    sizes: &[i64],
    align_corners: bool,
    input: &Tensor,
) -> Result<Tensor> {
    if input.shape.len() != 4 || sizes.len() != 4 {
        bail!("ResizeLinear expects rank-4 input and sizes");
    }
    let out_shape = sizes
        .iter()
        .map(|&d| usize::try_from(d).context("ResizeLinear size must be non-negative"))
        .collect::<Result<Vec<_>>>()?;
    let len = out_shape.iter().product::<usize>();
    let p = [
        len as u32,
        input.shape[0] as u32,
        input.shape[1] as u32,
        input.shape[2] as u32,
        input.shape[3] as u32,
        out_shape[2] as u32,
        out_shape[3] as u32,
        align_corners as u32,
    ];
    let raw = dispatch_compute(
        ctx,
        RESIZE_LINEAR_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        len * 4,
        bytemuck::cast_slice(&p),
        linear_grid(len.div_ceil(256)),
    )
    .await?;
    Tensor::new(out_shape, f32_from_bytes(&raw))
}

pub(crate) async fn run_grid_sample(
    ctx: &GpuContext,
    align_corners: bool,
    input: &Tensor,
    grid: &Tensor,
) -> Result<Tensor> {
    if input.shape.len() != 4 || grid.shape.len() != 4 || grid.shape[3] != 2 {
        bail!("GridSample expects NCHW input and NHW2 grid");
    }
    let out_shape = vec![input.shape[0], input.shape[1], grid.shape[1], grid.shape[2]];
    let len = out_shape.iter().product::<usize>();
    let p = [
        len as u32,
        input.shape[0] as u32,
        input.shape[1] as u32,
        input.shape[2] as u32,
        input.shape[3] as u32,
        grid.shape[1] as u32,
        grid.shape[2] as u32,
        align_corners as u32,
    ];
    let raw = dispatch_compute(
        ctx,
        GRID_SAMPLE_WGSL,
        &[
            bytemuck::cast_slice(&input.data),
            bytemuck::cast_slice(&grid.data),
        ],
        len * 4,
        bytemuck::cast_slice(&p),
        linear_grid(len.div_ceil(256)),
    )
    .await?;
    Tensor::new(out_shape, f32_from_bytes(&raw))
}
