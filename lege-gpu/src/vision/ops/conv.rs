use anyhow::{Context, Result, bail};

use super::common::f32_from_bytes;
use crate::vision::onnx::types::Conv2dPlan;
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params (19 x u32):
//   [0] total_out (N*cout*hout*wout)  [1] cin  [2] hin  [3] win
//   [4] cout  [5] cin/group  [6] kh  [7] kw
//   [8] hout  [9] wout
//   [10] stride_h  [11] stride_w
//   [12] pad_top  [13] pad_left
//   [14] dilation_h  [15] dilation_w
//   [16] use_bias  [17] cout/group  [18] n_batch
//
// Batch-aware naive convolution: the only conv kernel that handles N>1 (the
// optimized kernels assume N=1). Weights are shared across the batch.
pub(crate) const CONV2D_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }

    let cin           = params[1];
    let hout          = params[8];
    let wout          = params[9];
    let cout          = params[4];
    let cout_per_group= params[17];
    let cin_per_group = params[5];
    let kh            = params[6];
    let kw            = params[7];
    let hin           = params[2];
    let win           = params[3];
    let stride_h      = params[10];
    let stride_w      = params[11];
    let pad_top       = params[12];
    let pad_left      = params[13];
    let dilation_h    = params[14];
    let dilation_w    = params[15];

    let per_batch = cout * hout * wout;
    let n  = i / per_batch;
    let r  = i % per_batch;
    let co = r / (hout * wout);
    let oh = (r / wout) % hout;
    let ow = r % wout;
    let g  = co / cout_per_group;
    let in_batch_base = n * cin * hin * win;

    var sum = 0.0f;
    if params[16] != 0u { sum = bias[co]; }

    for (var ci_g: u32 = 0u; ci_g < cin_per_group; ci_g = ci_g + 1u) {
        let ci = g * cin_per_group + ci_g;
        for (var ky: u32 = 0u; ky < kh; ky = ky + 1u) {
            let ih_raw = i32(oh) * i32(stride_h) + i32(ky) * i32(dilation_h) - i32(pad_top);
            if ih_raw < 0 || u32(ih_raw) >= hin { continue; }
            let ih = u32(ih_raw);
            for (var kx: u32 = 0u; kx < kw; kx = kx + 1u) {
                let iw_raw = i32(ow) * i32(stride_w) + i32(kx) * i32(dilation_w) - i32(pad_left);
                if iw_raw < 0 || u32(iw_raw) >= win { continue; }
                let iw = u32(iw_raw);
                let in_idx = in_batch_base + ci * hin * win + ih * win + iw;
                let w_idx  = co * cin_per_group * kh * kw
                           + ci_g * kh * kw + ky * kw + kx;
                sum = sum + inp[in_idx] * wt[w_idx];
            }
        }
    }
    out[i] = sum;
}
"#;

pub(crate) async fn run_conv2d(
    ctx: &GpuContext,
    plan: &Conv2dPlan,
    inputs: &[Tensor],
) -> Result<Tensor> {
    if inputs.len() < 2 || inputs.len() > 3 {
        bail!("Conv2d expects 2-3 inputs (input, weight, optional bias)");
    }
    let x = &inputs[0];
    let w = &inputs[1];
    let bias_opt = inputs.get(2);

    if x.shape.len() != 4 || w.shape.len() != 4 {
        bail!("Conv2d: input and weight must be rank-4");
    }
    let n = x.shape[0];
    let cin = x.shape[1];
    let hin = x.shape[2];
    let win = x.shape[3];
    let cout = w.shape[0];
    let cin_per_group = w.shape[1];
    let kh = w.shape[2];
    let kw = w.shape[3];
    let group = plan.group as usize;
    if cin != cin_per_group * group || cout % group != 0 {
        bail!("Conv2d grouped channel mismatch");
    }
    let cout_per_group = cout / group;
    if let Some(b) = bias_opt {
        if b.shape != [cout] {
            bail!("Conv2d bias shape mismatch: {:?} vs [{cout}]", b.shape);
        }
    }

    let hout = conv_out_usize(
        hin,
        plan.pads[0],
        plan.pads[2],
        plan.dilations[0],
        kh,
        plan.strides[0],
    )?;
    let wout = conv_out_usize(
        win,
        plan.pads[1],
        plan.pads[3],
        plan.dilations[1],
        kw,
        plan.strides[1],
    )?;
    let num_out = n * cout * hout * wout;

    let bias_data: Vec<f32> = match bias_opt {
        Some(b) => b.data.clone(),
        None => vec![0.0f32; 1],
    };

    let params = [
        num_out as u32,
        cin as u32,
        hin as u32,
        win as u32,
        cout as u32,
        cin_per_group as u32,
        kh as u32,
        kw as u32,
        hout as u32,
        wout as u32,
        plan.strides[0] as u32,
        plan.strides[1] as u32,
        plan.pads[0] as u32,
        plan.pads[1] as u32,
        plan.dilations[0] as u32,
        plan.dilations[1] as u32,
        bias_opt.is_some() as u32,
        cout_per_group as u32,
        n as u32,
    ];

    let raw = dispatch_compute(
        ctx,
        CONV2D_WGSL,
        &[
            bytemuck::cast_slice(&x.data),
            bytemuck::cast_slice(&w.data),
            bytemuck::cast_slice(&bias_data),
        ],
        num_out * 4,
        bytemuck::cast_slice(&params),
        super::common::linear_grid(num_out.div_ceil(256)),
    )
    .await?;

    Tensor::new(vec![n, cout, hout, wout], f32_from_bytes(&raw))
}

// ── Tiled 1×1 conv as GEMM ────────────────────────────────────────────────────
//
// Tiles stored in column-major order to avoid 8-way shared memory bank conflicts.
//   tile_w[k * 16 + row_local]  — loaded as tile_w[lid.y * 16 + lid.x]
//   tile_x[col_local * 16 + k] — loaded as tile_x[lid.y * 16 + lid.x]
// Inner: acc += tile_w[k * 16 + lid.x] * tile_x[lid.y * 16 + k]
//
// Dispatch: (ceil(Cout/16), ceil(Hout*Wout/16), 1).
// params (9 x u32): [0]cout [1]cin [2]hout [3]wout [4]hin [5]win
//                   [6]stride_h [7]stride_w [8]use_bias
pub(crate) const CONV1X1_WGSL: &str = r#"
var<workgroup> tile_w: array<f32, 256>;
var<workgroup> tile_x: array<f32, 256>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hout     = params[2];
    let wout     = params[3];
    let hin      = params[4];
    let win      = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let use_bias = params[8];
    let hw_out   = hout * wout;

    // row = output channel, col = flattened output spatial index
    let row = wg.x * 16u + lid.x;
    let col = wg.y * 16u + lid.y;

    var acc = 0.0f;

    let num_tiles = (cin + 15u) / 16u;
    for (var t: u32 = 0u; t < num_tiles; t = t + 1u) {
        // Load tile_w: thread (lid.x=row_local, lid.y=k_local) stores tile_w[k * 16 + row]
        // Store as tile_w[lid.y * 16 + lid.x] so consecutive lid.x → consecutive banks
        let wt_col = t * 16u + lid.y;  // k-index = lid.y
        if row < cout && wt_col < cin {
            tile_w[lid.y * 16u + lid.x] = wt[row * cin + wt_col];
        } else {
            tile_w[lid.y * 16u + lid.x] = 0.0;
        }

        // Load tile_x: thread (lid.x=k_local, lid.y=col_local) stores tile_x[col * 16 + k]
        // Store as tile_x[lid.y * 16 + lid.x] so consecutive lid.x → consecutive banks
        let xi_row = t * 16u + lid.x;  // k-index = lid.x
        if xi_row < cin && col < hw_out {
            let oh = col / wout;
            let ow = col % wout;
            tile_x[lid.y * 16u + lid.x] = inp[xi_row * hin * win + oh * stride_h * win + ow * stride_w];
        } else {
            tile_x[lid.y * 16u + lid.x] = 0.0;
        }

        workgroupBarrier();

        // acc += tile_w[k * 16 + lid.x] * tile_x[lid.y * 16 + k]
        // tile_w access: consecutive lid.x across warp → consecutive banks ✓
        // tile_x access: same lid.y in warp → broadcast ✓
        for (var k: u32 = 0u; k < 16u; k = k + 1u) {
            acc = acc + tile_w[k * 16u + lid.x] * tile_x[lid.y * 16u + k];
        }

        workgroupBarrier();
    }

    if row < cout && col < hw_out {
        if use_bias != 0u { acc = acc + bias[row]; }
        let oh = col / wout;
        let ow = col % wout;
        out[row * hout * wout + oh * wout + ow] = acc;
    }
}
"#;

// ── Tiled 1×1 conv as GEMM, 2×2 register-blocked output tile ────────────────
//
// Workgroup covers 32 output channels × 32 flattened spatial positions using
// 16×16 threads. Each thread accumulates four outputs:
//   (row, col), (row, col+16), (row+16, col), (row+16, col+16).
//
// Dispatch: (ceil(Cout/32), ceil(Hout*Wout/32), 1).
// params match CONV1X1_WGSL.
pub(crate) const CONV1X1_BLOCK2X2_WGSL: &str = r#"
var<workgroup> tile_w: array<f32, 512>;
var<workgroup> tile_x: array<f32, 512>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

fn load_x(k: u32, col: u32, cin: u32, hw_out: u32, hout: u32, wout: u32, hin: u32, win: u32, stride_h: u32, stride_w: u32) -> f32 {
    if k >= cin || col >= hw_out {
        return 0.0;
    }
    let oh = col / wout;
    let ow = col % wout;
    return inp[k * hin * win + oh * stride_h * win + ow * stride_w];
}

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hout     = params[2];
    let wout     = params[3];
    let hin      = params[4];
    let win      = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let use_bias = params[8];
    let hw_out   = hout * wout;

    let row0 = wg.x * 32u + lid.x;
    let row1 = row0 + 16u;
    let col0 = wg.y * 32u + lid.y;
    let col1 = col0 + 16u;

    var acc00 = 0.0f;
    var acc01 = 0.0f;
    var acc10 = 0.0f;
    var acc11 = 0.0f;

    let num_tiles = (cin + 15u) / 16u;
    for (var t: u32 = 0u; t < num_tiles; t = t + 1u) {
        let k_w = t * 16u + lid.y;
        let w_base = lid.y * 32u + lid.x;
        if row0 < cout && k_w < cin {
            tile_w[w_base] = wt[row0 * cin + k_w];
        } else {
            tile_w[w_base] = 0.0;
        }
        if row1 < cout && k_w < cin {
            tile_w[w_base + 16u] = wt[row1 * cin + k_w];
        } else {
            tile_w[w_base + 16u] = 0.0;
        }

        let k_x = t * 16u + lid.x;
        let x_base = lid.y * 16u + lid.x;
        tile_x[x_base] = load_x(k_x, col0, cin, hw_out, hout, wout, hin, win, stride_h, stride_w);
        tile_x[x_base + 256u] = load_x(k_x, col1, cin, hw_out, hout, wout, hin, win, stride_h, stride_w);

        workgroupBarrier();

        for (var k: u32 = 0u; k < 16u; k = k + 1u) {
            let w0 = tile_w[k * 32u + lid.x];
            let w1 = tile_w[k * 32u + lid.x + 16u];
            let x0 = tile_x[lid.y * 16u + k];
            let x1 = tile_x[lid.y * 16u + k + 256u];
            acc00 = acc00 + w0 * x0;
            acc01 = acc01 + w0 * x1;
            acc10 = acc10 + w1 * x0;
            acc11 = acc11 + w1 * x1;
        }

        workgroupBarrier();
    }

    if row0 < cout {
        if use_bias != 0u {
            let b0 = bias[row0];
            acc00 = acc00 + b0;
            acc01 = acc01 + b0;
        }
        if col0 < hw_out {
            out[row0 * hout * wout + col0] = acc00;
        }
        if col1 < hw_out {
            out[row0 * hout * wout + col1] = acc01;
        }
    }
    if row1 < cout {
        if use_bias != 0u {
            let b1 = bias[row1];
            acc10 = acc10 + b1;
            acc11 = acc11 + b1;
        }
        if col0 < hw_out {
            out[row1 * hout * wout + col0] = acc10;
        }
        if col1 < hw_out {
            out[row1 * hout * wout + col1] = acc11;
        }
    }
}
"#;

// ── Tiled 1×1 conv as GEMM, 4×2 register-blocked output tile ────────────────
//
// Workgroup covers 64 output channels × 32 flattened spatial positions using
// 16×16 threads. Each thread accumulates eight outputs:
//   four output channels × two flattened spatial positions.
//
// Dispatch: (ceil(Cout/64), ceil(Hout*Wout/32), 1).
// params match CONV1X1_WGSL.
pub(crate) const CONV1X1_BLOCK4X2_WGSL: &str = r#"
var<workgroup> tile_w: array<f32, 1024>;
var<workgroup> tile_x: array<f32, 512>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

fn load_x_4x2(k: u32, col: u32, cin: u32, hw_out: u32, hout: u32, wout: u32, hin: u32, win: u32, stride_h: u32, stride_w: u32) -> f32 {
    if k >= cin || col >= hw_out {
        return 0.0;
    }
    let oh = col / wout;
    let ow = col % wout;
    return inp[k * hin * win + oh * stride_h * win + ow * stride_w];
}

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hout     = params[2];
    let wout     = params[3];
    let hin      = params[4];
    let win      = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let use_bias = params[8];
    let hw_out   = hout * wout;

    let row0 = wg.x * 64u + lid.x;
    let row1 = row0 + 16u;
    let row2 = row0 + 32u;
    let row3 = row0 + 48u;
    let col0 = wg.y * 32u + lid.y;
    let col1 = col0 + 16u;

    var acc00 = 0.0f;
    var acc01 = 0.0f;
    var acc10 = 0.0f;
    var acc11 = 0.0f;
    var acc20 = 0.0f;
    var acc21 = 0.0f;
    var acc30 = 0.0f;
    var acc31 = 0.0f;

    let num_tiles = (cin + 15u) / 16u;
    for (var t: u32 = 0u; t < num_tiles; t = t + 1u) {
        let k_w = t * 16u + lid.y;
        let w_base = lid.y * 64u + lid.x;
        if row0 < cout && k_w < cin {
            tile_w[w_base] = wt[row0 * cin + k_w];
        } else {
            tile_w[w_base] = 0.0;
        }
        if row1 < cout && k_w < cin {
            tile_w[w_base + 16u] = wt[row1 * cin + k_w];
        } else {
            tile_w[w_base + 16u] = 0.0;
        }
        if row2 < cout && k_w < cin {
            tile_w[w_base + 32u] = wt[row2 * cin + k_w];
        } else {
            tile_w[w_base + 32u] = 0.0;
        }
        if row3 < cout && k_w < cin {
            tile_w[w_base + 48u] = wt[row3 * cin + k_w];
        } else {
            tile_w[w_base + 48u] = 0.0;
        }

        let k_x = t * 16u + lid.x;
        let x_base = lid.y * 16u + lid.x;
        tile_x[x_base] = load_x_4x2(k_x, col0, cin, hw_out, hout, wout, hin, win, stride_h, stride_w);
        tile_x[x_base + 256u] = load_x_4x2(k_x, col1, cin, hw_out, hout, wout, hin, win, stride_h, stride_w);

        workgroupBarrier();

        for (var k: u32 = 0u; k < 16u; k = k + 1u) {
            let w0 = tile_w[k * 64u + lid.x];
            let w1 = tile_w[k * 64u + lid.x + 16u];
            let w2 = tile_w[k * 64u + lid.x + 32u];
            let w3 = tile_w[k * 64u + lid.x + 48u];
            let x0 = tile_x[lid.y * 16u + k];
            let x1 = tile_x[lid.y * 16u + k + 256u];
            acc00 = acc00 + w0 * x0;
            acc01 = acc01 + w0 * x1;
            acc10 = acc10 + w1 * x0;
            acc11 = acc11 + w1 * x1;
            acc20 = acc20 + w2 * x0;
            acc21 = acc21 + w2 * x1;
            acc30 = acc30 + w3 * x0;
            acc31 = acc31 + w3 * x1;
        }

        workgroupBarrier();
    }

    let plane = hout * wout;
    if row0 < cout {
        if use_bias != 0u {
            let b0 = bias[row0];
            acc00 = acc00 + b0;
            acc01 = acc01 + b0;
        }
        if col0 < hw_out { out[row0 * plane + col0] = acc00; }
        if col1 < hw_out { out[row0 * plane + col1] = acc01; }
    }
    if row1 < cout {
        if use_bias != 0u {
            let b1 = bias[row1];
            acc10 = acc10 + b1;
            acc11 = acc11 + b1;
        }
        if col0 < hw_out { out[row1 * plane + col0] = acc10; }
        if col1 < hw_out { out[row1 * plane + col1] = acc11; }
    }
    if row2 < cout {
        if use_bias != 0u {
            let b2 = bias[row2];
            acc20 = acc20 + b2;
            acc21 = acc21 + b2;
        }
        if col0 < hw_out { out[row2 * plane + col0] = acc20; }
        if col1 < hw_out { out[row2 * plane + col1] = acc21; }
    }
    if row3 < cout {
        if use_bias != 0u {
            let b3 = bias[row3];
            acc30 = acc30 + b3;
            acc31 = acc31 + b3;
        }
        if col0 < hw_out { out[row3 * plane + col0] = acc30; }
        if col1 < hw_out { out[row3 * plane + col1] = acc31; }
    }
}
"#;

// ── Tiled 1×1 conv as GEMM, 2×4 register-blocked output tile ────────────────
//
// Same eight outputs per thread as CONV1X1_BLOCK4X2_WGSL, but favors spatial
// width over output-channel width: 32 output channels × 64 flattened positions.
//
// Dispatch: (ceil(Cout/32), ceil(Hout*Wout/64), 1).
// params match CONV1X1_WGSL.
pub(crate) const CONV1X1_BLOCK2X4_WGSL: &str = r#"
var<workgroup> tile_w: array<f32, 512>;
var<workgroup> tile_x: array<f32, 1024>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

fn load_x_2x4(k: u32, col: u32, cin: u32, hw_out: u32, hout: u32, wout: u32, hin: u32, win: u32, stride_h: u32, stride_w: u32) -> f32 {
    if k >= cin || col >= hw_out {
        return 0.0;
    }
    let oh = col / wout;
    let ow = col % wout;
    return inp[k * hin * win + oh * stride_h * win + ow * stride_w];
}

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hout     = params[2];
    let wout     = params[3];
    let hin      = params[4];
    let win      = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let use_bias = params[8];
    let hw_out   = hout * wout;

    let row0 = wg.x * 32u + lid.x;
    let row1 = row0 + 16u;
    let col0 = wg.y * 64u + lid.y;
    let col1 = col0 + 16u;
    let col2 = col0 + 32u;
    let col3 = col0 + 48u;

    var acc00 = 0.0f;
    var acc01 = 0.0f;
    var acc02 = 0.0f;
    var acc03 = 0.0f;
    var acc10 = 0.0f;
    var acc11 = 0.0f;
    var acc12 = 0.0f;
    var acc13 = 0.0f;

    let num_tiles = (cin + 15u) / 16u;
    for (var t: u32 = 0u; t < num_tiles; t = t + 1u) {
        let k_w = t * 16u + lid.y;
        let w_base = lid.y * 32u + lid.x;
        if row0 < cout && k_w < cin {
            tile_w[w_base] = wt[row0 * cin + k_w];
        } else {
            tile_w[w_base] = 0.0;
        }
        if row1 < cout && k_w < cin {
            tile_w[w_base + 16u] = wt[row1 * cin + k_w];
        } else {
            tile_w[w_base + 16u] = 0.0;
        }

        let k_x = t * 16u + lid.x;
        let x_base = lid.y * 16u + lid.x;
        tile_x[x_base] = load_x_2x4(k_x, col0, cin, hw_out, hout, wout, hin, win, stride_h, stride_w);
        tile_x[x_base + 256u] = load_x_2x4(k_x, col1, cin, hw_out, hout, wout, hin, win, stride_h, stride_w);
        tile_x[x_base + 512u] = load_x_2x4(k_x, col2, cin, hw_out, hout, wout, hin, win, stride_h, stride_w);
        tile_x[x_base + 768u] = load_x_2x4(k_x, col3, cin, hw_out, hout, wout, hin, win, stride_h, stride_w);

        workgroupBarrier();

        for (var k: u32 = 0u; k < 16u; k = k + 1u) {
            let w0 = tile_w[k * 32u + lid.x];
            let w1 = tile_w[k * 32u + lid.x + 16u];
            let x0 = tile_x[lid.y * 16u + k];
            let x1 = tile_x[lid.y * 16u + k + 256u];
            let x2 = tile_x[lid.y * 16u + k + 512u];
            let x3 = tile_x[lid.y * 16u + k + 768u];
            acc00 = acc00 + w0 * x0;
            acc01 = acc01 + w0 * x1;
            acc02 = acc02 + w0 * x2;
            acc03 = acc03 + w0 * x3;
            acc10 = acc10 + w1 * x0;
            acc11 = acc11 + w1 * x1;
            acc12 = acc12 + w1 * x2;
            acc13 = acc13 + w1 * x3;
        }

        workgroupBarrier();
    }

    let plane = hout * wout;
    if row0 < cout {
        if use_bias != 0u {
            let b0 = bias[row0];
            acc00 = acc00 + b0;
            acc01 = acc01 + b0;
            acc02 = acc02 + b0;
            acc03 = acc03 + b0;
        }
        if col0 < hw_out { out[row0 * plane + col0] = acc00; }
        if col1 < hw_out { out[row0 * plane + col1] = acc01; }
        if col2 < hw_out { out[row0 * plane + col2] = acc02; }
        if col3 < hw_out { out[row0 * plane + col3] = acc03; }
    }
    if row1 < cout {
        if use_bias != 0u {
            let b1 = bias[row1];
            acc10 = acc10 + b1;
            acc11 = acc11 + b1;
            acc12 = acc12 + b1;
            acc13 = acc13 + b1;
        }
        if col0 < hw_out { out[row1 * plane + col0] = acc10; }
        if col1 < hw_out { out[row1 * plane + col1] = acc11; }
        if col2 < hw_out { out[row1 * plane + col2] = acc12; }
        if col3 < hw_out { out[row1 * plane + col3] = acc13; }
    }
}
"#;

// ── Tiled 1×1 conv, 2×4 tile, stride-1 same-size specialization ─────────────
//
// This is the hot YOLO 1×1 case: no padding/dilation, stride=1, H/W unchanged.
// It removes flattened-position division/modulo from input addressing.
//
// Dispatch: (ceil(Cout/32), ceil(H*W/64), 1).
// params: [0]cout [1]cin [2]plane [3]use_bias
pub(crate) const CONV1X1_BLOCK2X4_S1_WGSL: &str = r#"
var<workgroup> tile_w: array<f32, 512>;
var<workgroup> tile_x: array<f32, 1024>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let plane    = params[2];
    let use_bias = params[3];

    let row0 = wg.x * 32u + lid.x;
    let row1 = row0 + 16u;
    let col0 = wg.y * 64u + lid.y;
    let col1 = col0 + 16u;
    let col2 = col0 + 32u;
    let col3 = col0 + 48u;

    var acc00 = 0.0f;
    var acc01 = 0.0f;
    var acc02 = 0.0f;
    var acc03 = 0.0f;
    var acc10 = 0.0f;
    var acc11 = 0.0f;
    var acc12 = 0.0f;
    var acc13 = 0.0f;

    let num_tiles = (cin + 15u) / 16u;
    for (var t: u32 = 0u; t < num_tiles; t = t + 1u) {
        let k_w = t * 16u + lid.y;
        let w_base = lid.y * 32u + lid.x;
        if row0 < cout && k_w < cin {
            tile_w[w_base] = wt[row0 * cin + k_w];
        } else {
            tile_w[w_base] = 0.0;
        }
        if row1 < cout && k_w < cin {
            tile_w[w_base + 16u] = wt[row1 * cin + k_w];
        } else {
            tile_w[w_base + 16u] = 0.0;
        }

        let k_x = t * 16u + lid.x;
        let x_base = lid.y * 16u + lid.x;
        if k_x < cin && col0 < plane { tile_x[x_base] = inp[k_x * plane + col0]; } else { tile_x[x_base] = 0.0; }
        if k_x < cin && col1 < plane { tile_x[x_base + 256u] = inp[k_x * plane + col1]; } else { tile_x[x_base + 256u] = 0.0; }
        if k_x < cin && col2 < plane { tile_x[x_base + 512u] = inp[k_x * plane + col2]; } else { tile_x[x_base + 512u] = 0.0; }
        if k_x < cin && col3 < plane { tile_x[x_base + 768u] = inp[k_x * plane + col3]; } else { tile_x[x_base + 768u] = 0.0; }

        workgroupBarrier();

        for (var k: u32 = 0u; k < 16u; k = k + 1u) {
            let w0 = tile_w[k * 32u + lid.x];
            let w1 = tile_w[k * 32u + lid.x + 16u];
            let x0 = tile_x[lid.y * 16u + k];
            let x1 = tile_x[lid.y * 16u + k + 256u];
            let x2 = tile_x[lid.y * 16u + k + 512u];
            let x3 = tile_x[lid.y * 16u + k + 768u];
            acc00 = acc00 + w0 * x0;
            acc01 = acc01 + w0 * x1;
            acc02 = acc02 + w0 * x2;
            acc03 = acc03 + w0 * x3;
            acc10 = acc10 + w1 * x0;
            acc11 = acc11 + w1 * x1;
            acc12 = acc12 + w1 * x2;
            acc13 = acc13 + w1 * x3;
        }

        workgroupBarrier();
    }

    if row0 < cout {
        if use_bias != 0u {
            let b0 = bias[row0];
            acc00 = acc00 + b0;
            acc01 = acc01 + b0;
            acc02 = acc02 + b0;
            acc03 = acc03 + b0;
        }
        if col0 < plane { out[row0 * plane + col0] = acc00; }
        if col1 < plane { out[row0 * plane + col1] = acc01; }
        if col2 < plane { out[row0 * plane + col2] = acc02; }
        if col3 < plane { out[row0 * plane + col3] = acc03; }
    }
    if row1 < cout {
        if use_bias != 0u {
            let b1 = bias[row1];
            acc10 = acc10 + b1;
            acc11 = acc11 + b1;
            acc12 = acc12 + b1;
            acc13 = acc13 + b1;
        }
        if col0 < plane { out[row1 * plane + col0] = acc10; }
        if col1 < plane { out[row1 * plane + col1] = acc11; }
        if col2 < plane { out[row1 * plane + col2] = acc12; }
        if col3 < plane { out[row1 * plane + col3] = acc13; }
    }
}
"#;

// ── 1×1 conv as register-blocked GEMM, stride-1 same-size ───────────────────
//
// Classic 2D-blocked SGEMM. Computes C[M,N] = A[M,K] · B[K,N] where
//   A = weight [Cout, Cin]  (row-major, K contiguous)
//   B = input  [Cin, plane] (row-major, N=plane contiguous)
//   C = output [Cout, plane]
//
// Block tile BM×BN = 64×64, K-tile BK=16, micro-tile TM×TN = 4×4 per thread,
// 256 threads. Global loads are coalesced: for the dominant input (B) traffic,
// consecutive threads read consecutive spatial columns (contiguous memory),
// unlike the BLOCK2X4 kernels whose `inp[k*plane+col]` strided by `plane`.
//
// Dispatch: (ceil(Cout/64), ceil(plane/64), 1).
// params: [0]M(cout) [1]K(cin) [2]N(plane) [3]use_bias
pub(crate) const CONV1X1_GEMM_S1_WGSL: &str = r#"
const BM: u32 = 64u;
const BN: u32 = 64u;
const BK: u32 = 16u;

var<workgroup> As: array<f32, 1024>; // BK*BM, stored transposed As[k*BM + m]
var<workgroup> Bs: array<f32, 1024>; // BK*BN, stored Bs[k*BN + n]

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(
    @builtin(workgroup_id)           wg  : vec3<u32>,
    @builtin(local_invocation_index) tid : u32,
) {
    let M        = params[0];
    let K        = params[1];
    let N        = params[2];
    let use_bias = params[3];

    let bm = wg.x * BM;
    let bn = wg.y * BN;

    // Micro-tile origin for this thread within the block.
    let thread_row = tid / 16u;   // 0..15  → rows bm + thread_row*4 + (0..3)
    let thread_col = tid % 16u;   // 0..15  → cols bn + thread_col*4 + (0..3)

    // Coalesced load index decomposition.
    let inner_row_a = tid / BK;   // 0..15, A row within block
    let inner_col_a = tid % BK;   // 0..15, A col (k) within tile  → consecutive tid
    let inner_row_b = tid / BN;   // 0..3,  B row (k) within tile
    let inner_col_b = tid % BN;   // 0..63, B col (n) within block → consecutive tid

    var acc: array<f32, 16>;
    for (var i = 0u; i < 16u; i = i + 1u) { acc[i] = 0.0; }

    var bk = 0u;
    loop {
        if bk >= K { break; }

        // Load A tile (weight), transposed into As[k*BM + m]. Coalesced in k.
        for (var off = 0u; off < BM; off = off + 16u) {
            let row = inner_row_a + off;            // 0..63
            let gA_row = bm + row;
            let gA_col = bk + inner_col_a;
            var v = 0.0;
            if gA_row < M && gA_col < K {
                v = wt[gA_row * K + gA_col];
            }
            As[inner_col_a * BM + row] = v;
        }

        // Load B tile (input) into Bs[k*BN + n]. Coalesced in n (contiguous mem).
        for (var off = 0u; off < BK; off = off + 4u) {
            let krow = inner_row_b + off;           // 0..15
            let gB_row = bk + krow;
            let gB_col = bn + inner_col_b;
            var v = 0.0;
            if gB_row < K && gB_col < N {
                v = inp[gB_row * N + gB_col];
            }
            Bs[krow * BN + inner_col_b] = v;
        }

        workgroupBarrier();

        for (var k = 0u; k < BK; k = k + 1u) {
            let a0 = As[k * BM + thread_row * 4u + 0u];
            let a1 = As[k * BM + thread_row * 4u + 1u];
            let a2 = As[k * BM + thread_row * 4u + 2u];
            let a3 = As[k * BM + thread_row * 4u + 3u];
            let b0 = Bs[k * BN + thread_col * 4u + 0u];
            let b1 = Bs[k * BN + thread_col * 4u + 1u];
            let b2 = Bs[k * BN + thread_col * 4u + 2u];
            let b3 = Bs[k * BN + thread_col * 4u + 3u];
            acc[0u]  = acc[0u]  + a0 * b0;
            acc[1u]  = acc[1u]  + a0 * b1;
            acc[2u]  = acc[2u]  + a0 * b2;
            acc[3u]  = acc[3u]  + a0 * b3;
            acc[4u]  = acc[4u]  + a1 * b0;
            acc[5u]  = acc[5u]  + a1 * b1;
            acc[6u]  = acc[6u]  + a1 * b2;
            acc[7u]  = acc[7u]  + a1 * b3;
            acc[8u]  = acc[8u]  + a2 * b0;
            acc[9u]  = acc[9u]  + a2 * b1;
            acc[10u] = acc[10u] + a2 * b2;
            acc[11u] = acc[11u] + a2 * b3;
            acc[12u] = acc[12u] + a3 * b0;
            acc[13u] = acc[13u] + a3 * b1;
            acc[14u] = acc[14u] + a3 * b2;
            acc[15u] = acc[15u] + a3 * b3;
        }

        workgroupBarrier();
        bk = bk + BK;
    }

    for (var i = 0u; i < 4u; i = i + 1u) {
        let gm = bm + thread_row * 4u + i;
        if gm >= M { continue; }
        var b = 0.0;
        if use_bias != 0u { b = bias[gm]; }
        for (var j = 0u; j < 4u; j = j + 1u) {
            let gn = bn + thread_col * 4u + j;
            if gn < N {
                out[gm * N + gn] = acc[i * 4u + j] + b;
            }
        }
    }
}
"#;

// vec4 variant of the stride-1 1×1 GEMM. Requires K (cin) and N (plane) to be
// multiples of 4 so the input/weight/output buffers can be viewed as
// `array<vec4<f32>>`. Loading 128 bits per transaction cuts the global load
// instruction count and, by storing the B tile as `vec4`, removes the 2-way
// shared-memory bank conflict the scalar kernel has on the `thread_col*4` reads.
// Same params and dispatch as CONV1X1_GEMM_S1_WGSL; falls back to it otherwise.
pub(crate) const CONV1X1_GEMM_S1_VEC4_WGSL: &str = r#"
const BM: u32 = 64u;
const BN: u32 = 64u;
const BK: u32 = 16u;

var<workgroup> As: array<f32, 1024>;        // transposed As[k*BM + m], scalar
var<workgroup> Bs: array<vec4<f32>, 256>;   // Bs[k*16 + n4], 4 consecutive n per entry

@group(0) @binding(0) var<storage, read>       inp    : array<vec4<f32>>;
@group(0) @binding(1) var<storage, read>       wt     : array<vec4<f32>>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<vec4<f32>>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(
    @builtin(workgroup_id)           wg  : vec3<u32>,
    @builtin(local_invocation_index) tid : u32,
) {
    let M        = params[0];
    let K        = params[1];
    let N        = params[2];
    let use_bias = params[3];
    let K4 = K / 4u;
    let N4 = N / 4u;

    let bm  = wg.x * BM;
    let bn  = wg.y * BN;
    let bn4 = bn / 4u;            // BN is a multiple of 4

    let thread_row = tid / 16u;  // 0..15 → rows bm + thread_row*4 + (0..3)
    let thread_col = tid % 16u;  // 0..15 → 4 consecutive n at bn + thread_col*4

    // A load: 4 consecutive k (one vec4) per thread, scattered into transposed As.
    let a_m  = tid / 4u;         // 0..63  output row m within block
    let a_k4 = tid % 4u;         // 0..3   which vec4 of k

    // B load: one vec4 (4 consecutive n) per thread, straight into Bs.
    let b_k  = tid / 16u;        // 0..15  k within tile
    let b_n4 = tid % 16u;        // 0..15  vec4-n within block

    var acc: array<f32, 16>;
    for (var i = 0u; i < 16u; i = i + 1u) { acc[i] = 0.0; }

    var bk = 0u;
    loop {
        if bk >= K { break; }
        let bk4 = bk / 4u;

        // Load A (weight) vec4 of k, scatter transposed into As.
        let gm_a = bm + a_m;
        let gk4_a = bk4 + a_k4;
        var av = vec4<f32>(0.0, 0.0, 0.0, 0.0);
        if gm_a < M && gk4_a < K4 {
            av = wt[gm_a * K4 + gk4_a];
        }
        let kbase = a_k4 * 4u;
        As[(kbase + 0u) * BM + a_m] = av.x;
        As[(kbase + 1u) * BM + a_m] = av.y;
        As[(kbase + 2u) * BM + a_m] = av.z;
        As[(kbase + 3u) * BM + a_m] = av.w;

        // Load B (input) vec4 of n directly into Bs.
        let gk_b = bk + b_k;
        let gn4_b = bn4 + b_n4;
        var bv = vec4<f32>(0.0, 0.0, 0.0, 0.0);
        if gk_b < K && gn4_b < N4 {
            bv = inp[gk_b * N4 + gn4_b];
        }
        Bs[b_k * 16u + b_n4] = bv;

        workgroupBarrier();

        for (var k = 0u; k < BK; k = k + 1u) {
            let a0 = As[k * BM + thread_row * 4u + 0u];
            let a1 = As[k * BM + thread_row * 4u + 1u];
            let a2 = As[k * BM + thread_row * 4u + 2u];
            let a3 = As[k * BM + thread_row * 4u + 3u];
            let b  = Bs[k * 16u + thread_col];
            acc[0u]  = acc[0u]  + a0 * b.x;
            acc[1u]  = acc[1u]  + a0 * b.y;
            acc[2u]  = acc[2u]  + a0 * b.z;
            acc[3u]  = acc[3u]  + a0 * b.w;
            acc[4u]  = acc[4u]  + a1 * b.x;
            acc[5u]  = acc[5u]  + a1 * b.y;
            acc[6u]  = acc[6u]  + a1 * b.z;
            acc[7u]  = acc[7u]  + a1 * b.w;
            acc[8u]  = acc[8u]  + a2 * b.x;
            acc[9u]  = acc[9u]  + a2 * b.y;
            acc[10u] = acc[10u] + a2 * b.z;
            acc[11u] = acc[11u] + a2 * b.w;
            acc[12u] = acc[12u] + a3 * b.x;
            acc[13u] = acc[13u] + a3 * b.y;
            acc[14u] = acc[14u] + a3 * b.z;
            acc[15u] = acc[15u] + a3 * b.w;
        }

        workgroupBarrier();
        bk = bk + BK;
    }

    let n4_out = bn4 + thread_col;   // this thread's vec4-n column
    if n4_out >= N4 { return; }
    for (var i = 0u; i < 4u; i = i + 1u) {
        let gm = bm + thread_row * 4u + i;
        if gm >= M { continue; }
        var b = 0.0;
        if use_bias != 0u { b = bias[gm]; }
        let r = vec4<f32>(acc[i * 4u + 0u], acc[i * 4u + 1u], acc[i * 4u + 2u], acc[i * 4u + 3u])
              + vec4<f32>(b, b, b, b);
        out[gm * N4 + n4_out] = r;
    }
}
"#;

// ── Tiled 3×3 direct conv with shared-memory input patches ───────────────────
//
// Each workgroup processes one output channel and a 16×16 spatial output tile.
// For each input channel, the required input patch (with halo) is cooperatively
// loaded into shared memory — reducing global memory reads by ~7× vs naive.
//
// Dispatch: (ceil(Wout/16), ceil(Hout/16), Cout).
// params (16 x u32):
//   [0]cout [1]cin [2]hin [3]win [4]hout [5]wout [6]kh [7]kw
//   [8]stride_h [9]stride_w [10]pad_top [11]pad_left [12]dil_h [13]dil_w
//   [14]use_bias [15]cin_per_group
pub(crate) const CONV3X3_TILED_WGSL: &str = r#"
// Max patch: 34×34 = 1156 for 3×3 stride-2; fits in 1296-element buffer.
var<workgroup> smem: array<f32, 1296>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout        = params[0];
    let cin         = params[1];
    let hin         = params[2];
    let win         = params[3];
    let hout        = params[4];
    let wout        = params[5];
    let kh          = params[6];
    let kw          = params[7];
    let stride_h    = params[8];
    let stride_w    = params[9];
    let pad_top     = params[10];
    let pad_left    = params[11];
    let dil_h       = params[12];
    let dil_w       = params[13];
    let use_bias    = params[14];
    let cin_per_grp = params[15];

    let co = wg.z;
    // group index and input channel offset
    let cout_per_grp = cout / (cin / cin_per_grp);
    let g_idx        = co / cout_per_grp;
    let ci_start     = g_idx * cin_per_grp;

    let oh = wg.y * 16u + lid.y;
    let ow = wg.x * 16u + lid.x;

    var acc = 0.0f;
    if use_bias != 0u { acc = bias[co]; }

    // Input patch dimensions for this 16×16 output tile
    let patch_h   = 16u * stride_h + (kh - 1u) * dil_h;
    let patch_w   = 16u * stride_w + (kw - 1u) * dil_w;
    let patch_size = patch_h * patch_w;

    // Top-left input coordinate for this tile (signed: may be negative due to padding)
    let ih_base_i = i32(wg.y * 16u * stride_h) - i32(pad_top);
    let iw_base_i = i32(wg.x * 16u * stride_w) - i32(pad_left);

    let tid = lid.y * 16u + lid.x;  // 0..255

    for (var ci_local: u32 = 0u; ci_local < cin_per_grp; ci_local = ci_local + 1u) {
        let ci = ci_start + ci_local;

        // Cooperatively load the input patch (with halo) into shared memory.
        // 256 threads cover up to 1156 patch elements (5 rounds max).
        var pidx = tid;
        loop {
            if pidx >= patch_size { break; }
            let py = pidx / patch_w;
            let px = pidx % patch_w;
            let iy = ih_base_i + i32(py);
            let ix = iw_base_i + i32(px);
            if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
            } else {
                smem[py * patch_w + px] = 0.0;
            }
            pidx = pidx + 256u;
        }

        workgroupBarrier();

        if oh < hout && ow < wout {
            for (var ky: u32 = 0u; ky < kh; ky = ky + 1u) {
                for (var kx: u32 = 0u; kx < kw; kx = kx + 1u) {
                    let py = lid.y * stride_h + ky * dil_h;
                    let px = lid.x * stride_w + kx * dil_w;
                    let w_idx = co * cin_per_grp * kh * kw
                              + ci_local * kh * kw + ky * kw + kx;
                    acc = acc + smem[py * patch_w + px] * wt[w_idx];
                }
            }
        }

        workgroupBarrier();
    }

    if oh < hout && ow < wout {
        out[co * hout * wout + oh * wout + ow] = acc;
    }
}
"#;

// ── Tiled 3×3 conv computing four output channels per workgroup ──────────────
//
// This is for group=1 3×3 convolutions. It loads each input-channel patch once
// and reuses it for four output channels, reducing redundant global input reads
// versus CONV3X3_TILED_WGSL's one-output-channel workgroup.
//
// Dispatch: (ceil(Wout/16), ceil(Hout/16), ceil(Cout/4)).
// params (15 x u32):
//   [0]cout [1]cin [2]hin [3]win [4]hout [5]wout [6]stride_h [7]stride_w
//   [8]pad_top [9]pad_left [10]dil_h [11]dil_w [12]use_bias [13..14] unused
pub(crate) const CONV3X3_COBLOCK4_WGSL: &str = r#"
var<workgroup> smem: array<f32, 1296>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let pad_top  = params[8];
    let pad_left = params[9];
    let dil_h    = params[10];
    let dil_w    = params[11];
    let use_bias = params[12];

    let co0 = wg.z * 4u;
    let co1 = co0 + 1u;
    let co2 = co0 + 2u;
    let co3 = co0 + 3u;
    let oh = wg.y * 16u + lid.y;
    let ow = wg.x * 16u + lid.x;
    let valid_spatial = oh < hout && ow < wout;

    var acc0 = 0.0f;
    var acc1 = 0.0f;
    var acc2 = 0.0f;
    var acc3 = 0.0f;
    if use_bias != 0u {
        if co0 < cout { acc0 = bias[co0]; }
        if co1 < cout { acc1 = bias[co1]; }
        if co2 < cout { acc2 = bias[co2]; }
        if co3 < cout { acc3 = bias[co3]; }
    }

    let patch_h = 16u * stride_h + 2u * dil_h;
    let patch_w = 16u * stride_w + 2u * dil_w;
    let patch_size = patch_h * patch_w;
    let ih_base_i = i32(wg.y * 16u * stride_h) - i32(pad_top);
    let iw_base_i = i32(wg.x * 16u * stride_w) - i32(pad_left);
    let full_patch = ih_base_i >= 0 && iw_base_i >= 0
                  && ih_base_i + i32(patch_h) <= i32(hin)
                  && iw_base_i + i32(patch_w) <= i32(win);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        var pidx = tid;
        loop {
            if pidx >= patch_size { break; }
            let py = pidx / patch_w;
            let px = pidx - py * patch_w;
            let iy = ih_base_i + i32(py);
            let ix = iw_base_i + i32(px);
            if full_patch {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(ih_base_i) * win + u32(iw_base_i) + py * win + px];
            } else if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
            } else {
                smem[py * patch_w + px] = 0.0;
            }
            pidx = pidx + 256u;
        }

        workgroupBarrier();

        if valid_spatial {
            for (var ky: u32 = 0u; ky < 3u; ky = ky + 1u) {
                for (var kx: u32 = 0u; kx < 3u; kx = kx + 1u) {
                    let v = smem[(lid.y * stride_h + ky * dil_h) * patch_w + lid.x * stride_w + kx * dil_w];
                    let wk = ci * 9u + ky * 3u + kx;
                    if co0 < cout { acc0 = acc0 + v * wt[co0 * cin * 9u + wk]; }
                    if co1 < cout { acc1 = acc1 + v * wt[co1 * cin * 9u + wk]; }
                    if co2 < cout { acc2 = acc2 + v * wt[co2 * cin * 9u + wk]; }
                    if co3 < cout { acc3 = acc3 + v * wt[co3 * cin * 9u + wk]; }
                }
            }
        }

        workgroupBarrier();
    }

    if valid_spatial {
        let base = oh * wout + ow;
        let plane = hout * wout;
        if co0 < cout { out[co0 * plane + base] = acc0; }
        if co1 < cout { out[co1 * plane + base] = acc1; }
        if co2 < cout { out[co2 * plane + base] = acc2; }
        if co3 < cout { out[co3 * plane + base] = acc3; }
    }
}
"#;

// ── Tiled 3×3 conv computing eight output channels per workgroup ─────────────
//
// Same spatial tiling as CONV3X3_COBLOCK4_WGSL, but reuses each shared input
// patch for eight output channels. This halves the number of channel-block
// workgroups for the group=1 3×3 layers at the cost of more accumulator
// registers per thread.
//
// Dispatch: (ceil(Wout/16), ceil(Hout/16), ceil(Cout/8)).
pub(crate) const CONV3X3_COBLOCK8_WGSL: &str = r#"
var<workgroup> smem: array<f32, 1296>;
var<workgroup> wmem: array<f32, 72>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let pad_top  = params[8];
    let pad_left = params[9];
    let dil_h    = params[10];
    let dil_w    = params[11];
    let use_bias = params[12];

    let co0 = wg.z * 8u;
    let co1 = co0 + 1u;
    let co2 = co0 + 2u;
    let co3 = co0 + 3u;
    let co4 = co0 + 4u;
    let co5 = co0 + 5u;
    let co6 = co0 + 6u;
    let co7 = co0 + 7u;
    let oh = wg.y * 16u + lid.y;
    let ow = wg.x * 16u + lid.x;
    let valid_spatial = oh < hout && ow < wout;

    var acc0 = 0.0f;
    var acc1 = 0.0f;
    var acc2 = 0.0f;
    var acc3 = 0.0f;
    var acc4 = 0.0f;
    var acc5 = 0.0f;
    var acc6 = 0.0f;
    var acc7 = 0.0f;
    if use_bias != 0u {
        if co0 < cout { acc0 = bias[co0]; }
        if co1 < cout { acc1 = bias[co1]; }
        if co2 < cout { acc2 = bias[co2]; }
        if co3 < cout { acc3 = bias[co3]; }
        if co4 < cout { acc4 = bias[co4]; }
        if co5 < cout { acc5 = bias[co5]; }
        if co6 < cout { acc6 = bias[co6]; }
        if co7 < cout { acc7 = bias[co7]; }
    }

    let patch_h = 16u * stride_h + 2u * dil_h;
    let patch_w = 16u * stride_w + 2u * dil_w;
    let patch_size = patch_h * patch_w;
    let ih_base_i = i32(wg.y * 16u * stride_h) - i32(pad_top);
    let iw_base_i = i32(wg.x * 16u * stride_w) - i32(pad_left);
    let full_patch = ih_base_i >= 0 && iw_base_i >= 0
                  && ih_base_i + i32(patch_h) <= i32(hin)
                  && iw_base_i + i32(patch_w) <= i32(win);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        var pidx = tid;
        loop {
            if pidx >= patch_size { break; }
            let py = pidx / patch_w;
            let px = pidx - py * patch_w;
            let iy = ih_base_i + i32(py);
            let ix = iw_base_i + i32(px);
            if full_patch {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(ih_base_i) * win + u32(iw_base_i) + py * win + px];
            } else if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
            } else {
                smem[py * patch_w + px] = 0.0;
            }
            pidx = pidx + 256u;
        }

        if tid < 72u {
            let co_local = tid / 9u;
            let kk = tid - co_local * 9u;
            let co = co0 + co_local;
            if co < cout {
                wmem[tid] = wt[co * cin * 9u + ci * 9u + kk];
            } else {
                wmem[tid] = 0.0;
            }
        }

        workgroupBarrier();

        if valid_spatial {
            for (var ky: u32 = 0u; ky < 3u; ky = ky + 1u) {
                for (var kx: u32 = 0u; kx < 3u; kx = kx + 1u) {
                    let v = smem[(lid.y * stride_h + ky * dil_h) * patch_w + lid.x * stride_w + kx * dil_w];
                    let kk = ky * 3u + kx;
                    acc0 = acc0 + v * wmem[kk];
                    acc1 = acc1 + v * wmem[9u + kk];
                    acc2 = acc2 + v * wmem[18u + kk];
                    acc3 = acc3 + v * wmem[27u + kk];
                    acc4 = acc4 + v * wmem[36u + kk];
                    acc5 = acc5 + v * wmem[45u + kk];
                    acc6 = acc6 + v * wmem[54u + kk];
                    acc7 = acc7 + v * wmem[63u + kk];
                }
            }
        }

        workgroupBarrier();
    }

    if valid_spatial {
        let base = oh * wout + ow;
        let plane = hout * wout;
        if co0 < cout { out[co0 * plane + base] = acc0; }
        if co1 < cout { out[co1 * plane + base] = acc1; }
        if co2 < cout { out[co2 * plane + base] = acc2; }
        if co3 < cout { out[co3 * plane + base] = acc3; }
        if co4 < cout { out[co4 * plane + base] = acc4; }
        if co5 < cout { out[co5 * plane + base] = acc5; }
        if co6 < cout { out[co6 * plane + base] = acc6; }
        if co7 < cout { out[co7 * plane + base] = acc7; }
    }
}
"#;

// ── 3×3 s1/d1 conv: 2×2 spatial × 8 output channels per thread ───────────────
//
// Spatial register-blocking analog of the 1×1 GEMM win. Each thread computes a
// 2×2 spatial micro-tile for 8 output channels (32 accumulators), raising
// arithmetic intensity from 72 to 288 MACs per input channel between barriers.
// Specialized to stride-1, dilation-1 (the dominant 3×3 family). Each workgroup
// covers a 32×32 spatial tile (16×16 threads × 2×2) and 8 output channels.
//
// Dispatch: (ceil(Wout/32), ceil(Hout/32), ceil(Cout/8)).
// params (9 x u32): [0]cout [1]cin [2]hin [3]win [4]hout [5]wout
//                   [6]pad_top [7]pad_left [8]use_bias
pub(crate) const CONV3X3_CO8_SP2X2_S1D1_WGSL: &str = r#"
const PW: u32 = 34u;  // 32 spatial + 2 halo
var<workgroup> smem: array<f32, 1156>; // PW*PW
var<workgroup> wmem: array<f32, 72>;   // 8 channels * 9 taps

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let pad_top  = params[6];
    let pad_left = params[7];
    let use_bias = params[8];

    let co0 = wg.z * 8u;
    let oh_base = wg.y * 32u;
    let ow_base = wg.x * 32u;

    var acc00_0 = 0.0; var acc00_1 = 0.0; var acc00_2 = 0.0; var acc00_3 = 0.0;
    var acc00_4 = 0.0; var acc00_5 = 0.0; var acc00_6 = 0.0; var acc00_7 = 0.0;
    var acc01_0 = 0.0; var acc01_1 = 0.0; var acc01_2 = 0.0; var acc01_3 = 0.0;
    var acc01_4 = 0.0; var acc01_5 = 0.0; var acc01_6 = 0.0; var acc01_7 = 0.0;
    var acc10_0 = 0.0; var acc10_1 = 0.0; var acc10_2 = 0.0; var acc10_3 = 0.0;
    var acc10_4 = 0.0; var acc10_5 = 0.0; var acc10_6 = 0.0; var acc10_7 = 0.0;
    var acc11_0 = 0.0; var acc11_1 = 0.0; var acc11_2 = 0.0; var acc11_3 = 0.0;
    var acc11_4 = 0.0; var acc11_5 = 0.0; var acc11_6 = 0.0; var acc11_7 = 0.0;

    let ih_base_i = i32(oh_base) - i32(pad_top);
    let iw_base_i = i32(ow_base) - i32(pad_left);
    let full_patch = ih_base_i >= 0 && iw_base_i >= 0
                  && ih_base_i + i32(PW) <= i32(hin)
                  && iw_base_i + i32(PW) <= i32(win);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        // Cooperatively load the 34×34 input patch.
        var pidx = tid;
        loop {
            if pidx >= 1156u { break; }
            let py = pidx / PW;
            let px = pidx - py * PW;
            if full_patch {
                smem[pidx] = inp[ci * hin * win + (u32(ih_base_i) + py) * win + u32(iw_base_i) + px];
            } else {
                let iy = ih_base_i + i32(py);
                let ix = iw_base_i + i32(px);
                if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                    smem[pidx] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
                } else {
                    smem[pidx] = 0.0;
                }
            }
            pidx = pidx + 256u;
        }

        // Load 8×9 weight tile for this input channel.
        if tid < 72u {
            let co_local = tid / 9u;
            let kk = tid - co_local * 9u;
            let co = co0 + co_local;
            if co < cout {
                wmem[tid] = wt[co * cin * 9u + ci * 9u + kk];
            } else {
                wmem[tid] = 0.0;
            }
        }

        workgroupBarrier();

        // Each thread: 2×2 spatial block, top-left at (lid.y*2, lid.x*2).
        let py0 = lid.y * 2u;
        let px0 = lid.x * 2u;
        for (var ky = 0u; ky < 3u; ky = ky + 1u) {
            for (var kx = 0u; kx < 3u; kx = kx + 1u) {
                let kk = ky * 3u + kx;
                let v00 = smem[(py0 + ky) * PW + px0 + kx];
                let v01 = smem[(py0 + ky) * PW + px0 + 1u + kx];
                let v10 = smem[(py0 + 1u + ky) * PW + px0 + kx];
                let v11 = smem[(py0 + 1u + ky) * PW + px0 + 1u + kx];
                // Keep accumulators as named scalars. Even constant-index private
                // arrays lower to Function access chains in naga 29 SPIR-V; scalars
                // avoid feeding the driver local load/store traffic in this hot loop.
                let w0 = wmem[kk];
                let w1 = wmem[9u + kk];
                let w2 = wmem[18u + kk];
                let w3 = wmem[27u + kk];
                let w4 = wmem[36u + kk];
                let w5 = wmem[45u + kk];
                let w6 = wmem[54u + kk];
                let w7 = wmem[63u + kk];
                acc00_0 = acc00_0 + v00 * w0; acc01_0 = acc01_0 + v01 * w0; acc10_0 = acc10_0 + v10 * w0; acc11_0 = acc11_0 + v11 * w0;
                acc00_1 = acc00_1 + v00 * w1; acc01_1 = acc01_1 + v01 * w1; acc10_1 = acc10_1 + v10 * w1; acc11_1 = acc11_1 + v11 * w1;
                acc00_2 = acc00_2 + v00 * w2; acc01_2 = acc01_2 + v01 * w2; acc10_2 = acc10_2 + v10 * w2; acc11_2 = acc11_2 + v11 * w2;
                acc00_3 = acc00_3 + v00 * w3; acc01_3 = acc01_3 + v01 * w3; acc10_3 = acc10_3 + v10 * w3; acc11_3 = acc11_3 + v11 * w3;
                acc00_4 = acc00_4 + v00 * w4; acc01_4 = acc01_4 + v01 * w4; acc10_4 = acc10_4 + v10 * w4; acc11_4 = acc11_4 + v11 * w4;
                acc00_5 = acc00_5 + v00 * w5; acc01_5 = acc01_5 + v01 * w5; acc10_5 = acc10_5 + v10 * w5; acc11_5 = acc11_5 + v11 * w5;
                acc00_6 = acc00_6 + v00 * w6; acc01_6 = acc01_6 + v01 * w6; acc10_6 = acc10_6 + v10 * w6; acc11_6 = acc11_6 + v11 * w6;
                acc00_7 = acc00_7 + v00 * w7; acc01_7 = acc01_7 + v01 * w7; acc10_7 = acc10_7 + v10 * w7; acc11_7 = acc11_7 + v11 * w7;
            }
        }

        workgroupBarrier();
    }

    let plane = hout * wout;
    let py0_out = lid.y * 2u;
    let px0_out = lid.x * 2u;
    var b0 = 0.0; var b1 = 0.0; var b2 = 0.0; var b3 = 0.0;
    var b4 = 0.0; var b5 = 0.0; var b6 = 0.0; var b7 = 0.0;
    if use_bias != 0u {
        if co0 + 0u < cout { b0 = bias[co0 + 0u]; }
        if co0 + 1u < cout { b1 = bias[co0 + 1u]; }
        if co0 + 2u < cout { b2 = bias[co0 + 2u]; }
        if co0 + 3u < cout { b3 = bias[co0 + 3u]; }
        if co0 + 4u < cout { b4 = bias[co0 + 4u]; }
        if co0 + 5u < cout { b5 = bias[co0 + 5u]; }
        if co0 + 6u < cout { b6 = bias[co0 + 6u]; }
        if co0 + 7u < cout { b7 = bias[co0 + 7u]; }
    }
    let oh0 = oh_base + py0_out;
    let ow0 = ow_base + px0_out;
    if oh0 < hout && ow0 < wout {
        let obase = oh0 * wout + ow0;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc00_0 + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc00_1 + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc00_2 + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc00_3 + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc00_4 + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc00_5 + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc00_6 + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc00_7 + b7; }
    }
    let ow1 = ow0 + 1u;
    if oh0 < hout && ow1 < wout {
        let obase = oh0 * wout + ow1;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc01_0 + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc01_1 + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc01_2 + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc01_3 + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc01_4 + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc01_5 + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc01_6 + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc01_7 + b7; }
    }
    let oh1 = oh0 + 1u;
    if oh1 < hout && ow0 < wout {
        let obase = oh1 * wout + ow0;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc10_0 + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc10_1 + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc10_2 + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc10_3 + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc10_4 + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc10_5 + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc10_6 + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc10_7 + b7; }
    }
    if oh1 < hout && ow1 < wout {
        let obase = oh1 * wout + ow1;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc11_0 + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc11_1 + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc11_2 + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc11_3 + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc11_4 + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc11_5 + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc11_6 + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc11_7 + b7; }
    }
}
"#;

// Same compute tile as CONV3X3_CO8_SP2X2_S1D1_WGSL, but interior tiles load the
// 34×34 input patch through vec4 reads. With pad=1, the patch starts one column
// before the 32-wide output tile, so the aligned vec4 base is shifted left and
// the shared-memory row is padded to 37 floats. Edge tiles use scalar extraction
// from vec4 storage and keep the original unshifted coordinates.
pub(crate) const CONV3X3_CO8_SP2X2_S1D1_VEC4LOAD_WGSL: &str = r#"
const PW: u32 = 34u;
const PWV: u32 = 37u;
const PH: u32 = 34u;
var<workgroup> smem: array<f32, 1258>; // PH*PWV
var<workgroup> wmem: array<f32, 72>;   // 8 channels * 9 taps

@group(0) @binding(0) var<storage, read>       inp    : array<vec4<f32>>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

fn load_scalar(idx: u32) -> f32 {
    let v = inp[idx / 4u];
    let lane = idx & 3u;
    if lane == 0u { return v.x; }
    if lane == 1u { return v.y; }
    if lane == 2u { return v.z; }
    return v.w;
}

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let pad_top  = params[6];
    let pad_left = params[7];
    let use_bias = params[8];

    let co0 = wg.z * 8u;
    let oh_base = wg.y * 32u;
    let ow_base = wg.x * 32u;

    var acc = array<f32, 32>();

    let ih_base_i = i32(oh_base) - i32(pad_top);
    let iw_base_i = i32(ow_base) - i32(pad_left);
    let full_patch = ih_base_i >= 0 && iw_base_i >= 0
                  && ih_base_i + i32(PW) <= i32(hin)
                  && iw_base_i + i32(PW) <= i32(win);
    var x_shift = 0u;
    if full_patch {
        x_shift = u32(iw_base_i) & 3u;
    }
    let tid = lid.y * 16u + lid.x;
    let plane_in = hin * win;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        if full_patch {
            let aligned_x = u32(iw_base_i) - x_shift;
            var vidx = tid;
            loop {
                if vidx >= PH * 9u { break; }
                let py = vidx / 9u;
                let vx = vidx - py * 9u;
                let base = ci * plane_in + (u32(ih_base_i) + py) * win + aligned_x + vx * 4u;
                let v = inp[base / 4u];
                let dst = py * PWV + vx * 4u;
                smem[dst + 0u] = v.x;
                smem[dst + 1u] = v.y;
                smem[dst + 2u] = v.z;
                smem[dst + 3u] = v.w;
                vidx = vidx + 256u;
            }
        } else {
            var pidx = tid;
            loop {
                if pidx >= PW * PW { break; }
                let py = pidx / PW;
                let px = pidx - py * PW;
                let iy = ih_base_i + i32(py);
                let ix = iw_base_i + i32(px);
                if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                    smem[py * PWV + px] = load_scalar(ci * plane_in + u32(iy) * win + u32(ix));
                } else {
                    smem[py * PWV + px] = 0.0;
                }
                pidx = pidx + 256u;
            }
        }

        if tid < 72u {
            let co_local = tid / 9u;
            let kk = tid - co_local * 9u;
            let co = co0 + co_local;
            if co < cout {
                wmem[tid] = wt[co * cin * 9u + ci * 9u + kk];
            } else {
                wmem[tid] = 0.0;
            }
        }

        workgroupBarrier();

        let py0 = lid.y * 2u;
        let px0 = lid.x * 2u + x_shift;
        for (var ky = 0u; ky < 3u; ky = ky + 1u) {
            for (var kx = 0u; kx < 3u; kx = kx + 1u) {
                let kk = ky * 3u + kx;
                let v00 = smem[(py0 + ky) * PWV + px0 + kx];
                let v01 = smem[(py0 + ky) * PWV + px0 + 1u + kx];
                let v10 = smem[(py0 + 1u + ky) * PWV + px0 + kx];
                let v11 = smem[(py0 + 1u + ky) * PWV + px0 + 1u + kx];
                let w0 = wmem[kk];
                let w1 = wmem[9u + kk];
                let w2 = wmem[18u + kk];
                let w3 = wmem[27u + kk];
                let w4 = wmem[36u + kk];
                let w5 = wmem[45u + kk];
                let w6 = wmem[54u + kk];
                let w7 = wmem[63u + kk];
                acc[0]  = acc[0]  + v00 * w0; acc[8]  = acc[8]  + v01 * w0; acc[16] = acc[16] + v10 * w0; acc[24] = acc[24] + v11 * w0;
                acc[1]  = acc[1]  + v00 * w1; acc[9]  = acc[9]  + v01 * w1; acc[17] = acc[17] + v10 * w1; acc[25] = acc[25] + v11 * w1;
                acc[2]  = acc[2]  + v00 * w2; acc[10] = acc[10] + v01 * w2; acc[18] = acc[18] + v10 * w2; acc[26] = acc[26] + v11 * w2;
                acc[3]  = acc[3]  + v00 * w3; acc[11] = acc[11] + v01 * w3; acc[19] = acc[19] + v10 * w3; acc[27] = acc[27] + v11 * w3;
                acc[4]  = acc[4]  + v00 * w4; acc[12] = acc[12] + v01 * w4; acc[20] = acc[20] + v10 * w4; acc[28] = acc[28] + v11 * w4;
                acc[5]  = acc[5]  + v00 * w5; acc[13] = acc[13] + v01 * w5; acc[21] = acc[21] + v10 * w5; acc[29] = acc[29] + v11 * w5;
                acc[6]  = acc[6]  + v00 * w6; acc[14] = acc[14] + v01 * w6; acc[22] = acc[22] + v10 * w6; acc[30] = acc[30] + v11 * w6;
                acc[7]  = acc[7]  + v00 * w7; acc[15] = acc[15] + v01 * w7; acc[23] = acc[23] + v10 * w7; acc[31] = acc[31] + v11 * w7;
            }
        }

        workgroupBarrier();
    }

    let plane = hout * wout;
    let py0_out = lid.y * 2u;
    let px0_out = lid.x * 2u;
    var b0 = 0.0; var b1 = 0.0; var b2 = 0.0; var b3 = 0.0;
    var b4 = 0.0; var b5 = 0.0; var b6 = 0.0; var b7 = 0.0;
    if use_bias != 0u {
        if co0 + 0u < cout { b0 = bias[co0 + 0u]; }
        if co0 + 1u < cout { b1 = bias[co0 + 1u]; }
        if co0 + 2u < cout { b2 = bias[co0 + 2u]; }
        if co0 + 3u < cout { b3 = bias[co0 + 3u]; }
        if co0 + 4u < cout { b4 = bias[co0 + 4u]; }
        if co0 + 5u < cout { b5 = bias[co0 + 5u]; }
        if co0 + 6u < cout { b6 = bias[co0 + 6u]; }
        if co0 + 7u < cout { b7 = bias[co0 + 7u]; }
    }
    let oh0 = oh_base + py0_out;
    let ow0 = ow_base + px0_out;
    if oh0 < hout && ow0 < wout {
        let obase = oh0 * wout + ow0;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[0] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[1] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[2] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[3] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[4] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[5] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[6] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[7] + b7; }
    }
    let ow1 = ow0 + 1u;
    if oh0 < hout && ow1 < wout {
        let obase = oh0 * wout + ow1;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[8] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[9] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[10] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[11] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[12] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[13] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[14] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[15] + b7; }
    }
    let oh1 = oh0 + 1u;
    if oh1 < hout && ow0 < wout {
        let obase = oh1 * wout + ow0;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[16] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[17] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[18] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[19] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[20] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[21] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[22] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[23] + b7; }
    }
    if oh1 < hout && ow1 < wout {
        let obase = oh1 * wout + ow1;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[24] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[25] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[26] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[27] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[28] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[29] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[30] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[31] + b7; }
    }
}
"#;

// ── Tiled 5×5 conv computing four output channels per workgroup ──────────────
//
// Used by YOLO dilated blocks. Supports group=1, stride/pad/dilation variants.
// Dispatch: (ceil(Wout/16), ceil(Hout/16), ceil(Cout/4)).
pub(crate) const CONV5X5_COBLOCK4_WGSL: &str = r#"
var<workgroup> smem: array<f32, 1296>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let pad_top  = params[8];
    let pad_left = params[9];
    let dil_h    = params[10];
    let dil_w    = params[11];
    let use_bias = params[12];

    let co0 = wg.z * 4u;
    let co1 = co0 + 1u;
    let co2 = co0 + 2u;
    let co3 = co0 + 3u;
    let oh = wg.y * 16u + lid.y;
    let ow = wg.x * 16u + lid.x;
    let valid_spatial = oh < hout && ow < wout;

    var acc0 = 0.0f;
    var acc1 = 0.0f;
    var acc2 = 0.0f;
    var acc3 = 0.0f;
    if use_bias != 0u {
        if co0 < cout { acc0 = bias[co0]; }
        if co1 < cout { acc1 = bias[co1]; }
        if co2 < cout { acc2 = bias[co2]; }
        if co3 < cout { acc3 = bias[co3]; }
    }

    let patch_h = 16u * stride_h + 4u * dil_h;
    let patch_w = 16u * stride_w + 4u * dil_w;
    let patch_size = patch_h * patch_w;
    let ih_base_i = i32(wg.y * 16u * stride_h) - i32(pad_top);
    let iw_base_i = i32(wg.x * 16u * stride_w) - i32(pad_left);
    let full_patch = ih_base_i >= 0 && iw_base_i >= 0
                  && ih_base_i + i32(patch_h) <= i32(hin)
                  && iw_base_i + i32(patch_w) <= i32(win);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        var pidx = tid;
        loop {
            if pidx >= patch_size { break; }
            let py = pidx / patch_w;
            let px = pidx - py * patch_w;
            let iy = ih_base_i + i32(py);
            let ix = iw_base_i + i32(px);
            if full_patch {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(ih_base_i) * win + u32(iw_base_i) + py * win + px];
            } else if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
            } else {
                smem[py * patch_w + px] = 0.0;
            }
            pidx = pidx + 256u;
        }

        workgroupBarrier();

        if valid_spatial {
            for (var ky: u32 = 0u; ky < 5u; ky = ky + 1u) {
                for (var kx: u32 = 0u; kx < 5u; kx = kx + 1u) {
                    let v = smem[(lid.y * stride_h + ky * dil_h) * patch_w + lid.x * stride_w + kx * dil_w];
                    let wk = ci * 25u + ky * 5u + kx;
                    if co0 < cout { acc0 = acc0 + v * wt[co0 * cin * 25u + wk]; }
                    if co1 < cout { acc1 = acc1 + v * wt[co1 * cin * 25u + wk]; }
                    if co2 < cout { acc2 = acc2 + v * wt[co2 * cin * 25u + wk]; }
                    if co3 < cout { acc3 = acc3 + v * wt[co3 * cin * 25u + wk]; }
                }
            }
        }

        workgroupBarrier();
    }

    if valid_spatial {
        let base = oh * wout + ow;
        let plane = hout * wout;
        if co0 < cout { out[co0 * plane + base] = acc0; }
        if co1 < cout { out[co1 * plane + base] = acc1; }
        if co2 < cout { out[co2 * plane + base] = acc2; }
        if co3 < cout { out[co3 * plane + base] = acc3; }
    }
}
"#;

// ── Tiled 5×5 conv computing eight output channels per workgroup ─────────────
//
// Same as the Co4 5×5 path, but amortizes each shared input patch over eight
// output channels. This is aimed at the 96-channel dilated blocks that dominate
// the current profile.
//
// Dispatch: (ceil(Wout/16), ceil(Hout/16), ceil(Cout/8)).
pub(crate) const CONV5X5_COBLOCK8_WGSL: &str = r#"
var<workgroup> smem: array<f32, 1296>;
var<workgroup> wmem: array<f32, 200>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let pad_top  = params[8];
    let pad_left = params[9];
    let dil_h    = params[10];
    let dil_w    = params[11];
    let use_bias = params[12];

    let co0 = wg.z * 8u;
    let co1 = co0 + 1u;
    let co2 = co0 + 2u;
    let co3 = co0 + 3u;
    let co4 = co0 + 4u;
    let co5 = co0 + 5u;
    let co6 = co0 + 6u;
    let co7 = co0 + 7u;
    let oh = wg.y * 16u + lid.y;
    let ow = wg.x * 16u + lid.x;
    let valid_spatial = oh < hout && ow < wout;

    var acc0 = 0.0f;
    var acc1 = 0.0f;
    var acc2 = 0.0f;
    var acc3 = 0.0f;
    var acc4 = 0.0f;
    var acc5 = 0.0f;
    var acc6 = 0.0f;
    var acc7 = 0.0f;
    if use_bias != 0u {
        if co0 < cout { acc0 = bias[co0]; }
        if co1 < cout { acc1 = bias[co1]; }
        if co2 < cout { acc2 = bias[co2]; }
        if co3 < cout { acc3 = bias[co3]; }
        if co4 < cout { acc4 = bias[co4]; }
        if co5 < cout { acc5 = bias[co5]; }
        if co6 < cout { acc6 = bias[co6]; }
        if co7 < cout { acc7 = bias[co7]; }
    }

    let patch_h = 16u * stride_h + 4u * dil_h;
    let patch_w = 16u * stride_w + 4u * dil_w;
    let patch_size = patch_h * patch_w;
    let ih_base_i = i32(wg.y * 16u * stride_h) - i32(pad_top);
    let iw_base_i = i32(wg.x * 16u * stride_w) - i32(pad_left);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        var pidx = tid;
        loop {
            if pidx >= patch_size { break; }
            let py = pidx / patch_w;
            let px = pidx - py * patch_w;
            let iy = ih_base_i + i32(py);
            let ix = iw_base_i + i32(px);
            if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
            } else {
                smem[py * patch_w + px] = 0.0;
            }
            pidx = pidx + 256u;
        }

        if tid < 200u {
            let co_local = tid / 25u;
            let kk = tid - co_local * 25u;
            let co = co0 + co_local;
            if co < cout {
                wmem[tid] = wt[co * cin * 25u + ci * 25u + kk];
            } else {
                wmem[tid] = 0.0;
            }
        }

        workgroupBarrier();

        if valid_spatial {
            for (var ky: u32 = 0u; ky < 5u; ky = ky + 1u) {
                for (var kx: u32 = 0u; kx < 5u; kx = kx + 1u) {
                    let v = smem[(lid.y * stride_h + ky * dil_h) * patch_w + lid.x * stride_w + kx * dil_w];
                    let kk = ky * 5u + kx;
                    acc0 = acc0 + v * wmem[kk];
                    acc1 = acc1 + v * wmem[25u + kk];
                    acc2 = acc2 + v * wmem[50u + kk];
                    acc3 = acc3 + v * wmem[75u + kk];
                    acc4 = acc4 + v * wmem[100u + kk];
                    acc5 = acc5 + v * wmem[125u + kk];
                    acc6 = acc6 + v * wmem[150u + kk];
                    acc7 = acc7 + v * wmem[175u + kk];
                }
            }
        }

        workgroupBarrier();
    }

    if valid_spatial {
        let base = oh * wout + ow;
        let plane = hout * wout;
        if co0 < cout { out[co0 * plane + base] = acc0; }
        if co1 < cout { out[co1 * plane + base] = acc1; }
        if co2 < cout { out[co2 * plane + base] = acc2; }
        if co3 < cout { out[co3 * plane + base] = acc3; }
        if co4 < cout { out[co4 * plane + base] = acc4; }
        if co5 < cout { out[co5 * plane + base] = acc5; }
        if co6 < cout { out[co6 * plane + base] = acc6; }
        if co7 < cout { out[co7 * plane + base] = acc7; }
    }
}
"#;

// 5×5 stride-1 dilation-1 with 2×2 spatial register tiling × 8 output channels.
// Mirrors CONV3X3_CO8_SP2X2_S1D1_WGSL exactly, extended to a 5×5 kernel.
// Each thread accumulates 2×2 spatial × 8 ch = 32 values, 4× the arithmetic
// intensity vs the scalar-output CONV5X5_COBLOCK8_WGSL.
// Smem: 36×36 = 1296 floats (32 output + 4 halo = 36 each side for d1).
// Wmem: 8 channels × 25 taps = 200 floats.
// dispatch: (ceil(Wout/32), ceil(Hout/32), ceil(Cout/8)).
// params: [0]cout [1]cin [2]hin [3]win [4]hout [5]wout [6]pad_top [7]pad_left [8]use_bias
pub(crate) const CONV5X5_CO8_SP2X2_D1_WGSL: &str = r#"
const PW5: u32 = 36u;  // 32 spatial + 4 halo (5x5 d1)

var<workgroup> smem5: array<f32, 1296>;   // PW5 * PW5
var<workgroup> wmem5: array<f32, 200>;    // 8 channels * 25 taps

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let pad_top  = params[6];
    let pad_left = params[7];
    let use_bias = params[8];

    let co0 = wg.z * 8u;
    let oh_base = wg.y * 32u;
    let ow_base = wg.x * 32u;

    var acc = array<f32, 32>();

    let ih_base_i = i32(oh_base) - i32(pad_top);
    let iw_base_i = i32(ow_base) - i32(pad_left);
    let full_patch = ih_base_i >= 0 && iw_base_i >= 0
                  && ih_base_i + i32(PW5) <= i32(hin)
                  && iw_base_i + i32(PW5) <= i32(win);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        // Cooperatively load the 36×36 input patch.
        var pidx = tid;
        loop {
            if pidx >= 1296u { break; }
            let py = pidx / PW5;
            let px = pidx - py * PW5;
            if full_patch {
                smem5[pidx] = inp[ci * hin * win + (u32(ih_base_i) + py) * win + u32(iw_base_i) + px];
            } else {
                let iy = ih_base_i + i32(py);
                let ix = iw_base_i + i32(px);
                if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                    smem5[pidx] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
                } else {
                    smem5[pidx] = 0.0;
                }
            }
            pidx = pidx + 256u;
        }

        // Load 8×25 weight tile for this input channel.
        if tid < 200u {
            let co_local = tid / 25u;
            let kk = tid - co_local * 25u;
            let co = co0 + co_local;
            if co < cout {
                wmem5[tid] = wt[co * cin * 25u + ci * 25u + kk];
            } else {
                wmem5[tid] = 0.0;
            }
        }

        workgroupBarrier();

        // Each thread: 2×2 spatial block, top-left at (lid.y*2, lid.x*2).
        let py0 = lid.y * 2u;
        let px0 = lid.x * 2u;
        for (var ky = 0u; ky < 5u; ky = ky + 1u) {
            for (var kx = 0u; kx < 5u; kx = kx + 1u) {
                let kk = ky * 5u + kx;
                let v00 = smem5[(py0 + ky) * PW5 + px0 + kx];
                let v01 = smem5[(py0 + ky) * PW5 + px0 + 1u + kx];
                let v10 = smem5[(py0 + 1u + ky) * PW5 + px0 + kx];
                let v11 = smem5[(py0 + 1u + ky) * PW5 + px0 + 1u + kx];
                // Unrolled over 8 output channels so `acc` is statically indexed and
                // stays in registers (naga 29 spills a dynamically indexed private
                // array to local memory — ~12x slower; see CONV3X3_CO8_SP2X2).
                let w0 = wmem5[kk];
                let w1 = wmem5[25u + kk];
                let w2 = wmem5[50u + kk];
                let w3 = wmem5[75u + kk];
                let w4 = wmem5[100u + kk];
                let w5 = wmem5[125u + kk];
                let w6 = wmem5[150u + kk];
                let w7 = wmem5[175u + kk];
                acc[0]  = acc[0]  + v00 * w0; acc[8]  = acc[8]  + v01 * w0; acc[16] = acc[16] + v10 * w0; acc[24] = acc[24] + v11 * w0;
                acc[1]  = acc[1]  + v00 * w1; acc[9]  = acc[9]  + v01 * w1; acc[17] = acc[17] + v10 * w1; acc[25] = acc[25] + v11 * w1;
                acc[2]  = acc[2]  + v00 * w2; acc[10] = acc[10] + v01 * w2; acc[18] = acc[18] + v10 * w2; acc[26] = acc[26] + v11 * w2;
                acc[3]  = acc[3]  + v00 * w3; acc[11] = acc[11] + v01 * w3; acc[19] = acc[19] + v10 * w3; acc[27] = acc[27] + v11 * w3;
                acc[4]  = acc[4]  + v00 * w4; acc[12] = acc[12] + v01 * w4; acc[20] = acc[20] + v10 * w4; acc[28] = acc[28] + v11 * w4;
                acc[5]  = acc[5]  + v00 * w5; acc[13] = acc[13] + v01 * w5; acc[21] = acc[21] + v10 * w5; acc[29] = acc[29] + v11 * w5;
                acc[6]  = acc[6]  + v00 * w6; acc[14] = acc[14] + v01 * w6; acc[22] = acc[22] + v10 * w6; acc[30] = acc[30] + v11 * w6;
                acc[7]  = acc[7]  + v00 * w7; acc[15] = acc[15] + v01 * w7; acc[23] = acc[23] + v10 * w7; acc[31] = acc[31] + v11 * w7;
            }
        }

        workgroupBarrier();
    }

    let plane = hout * wout;
    let py0_out = lid.y * 2u;
    let px0_out = lid.x * 2u;
    var b0 = 0.0; var b1 = 0.0; var b2 = 0.0; var b3 = 0.0;
    var b4 = 0.0; var b5 = 0.0; var b6 = 0.0; var b7 = 0.0;
    if use_bias != 0u {
        if co0 + 0u < cout { b0 = bias[co0 + 0u]; }
        if co0 + 1u < cout { b1 = bias[co0 + 1u]; }
        if co0 + 2u < cout { b2 = bias[co0 + 2u]; }
        if co0 + 3u < cout { b3 = bias[co0 + 3u]; }
        if co0 + 4u < cout { b4 = bias[co0 + 4u]; }
        if co0 + 5u < cout { b5 = bias[co0 + 5u]; }
        if co0 + 6u < cout { b6 = bias[co0 + 6u]; }
        if co0 + 7u < cout { b7 = bias[co0 + 7u]; }
    }
    let oh0 = oh_base + py0_out;
    let ow0 = ow_base + px0_out;
    if oh0 < hout && ow0 < wout {
        let obase = oh0 * wout + ow0;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[0] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[1] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[2] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[3] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[4] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[5] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[6] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[7] + b7; }
    }
    let ow1 = ow0 + 1u;
    if oh0 < hout && ow1 < wout {
        let obase = oh0 * wout + ow1;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[8] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[9] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[10] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[11] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[12] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[13] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[14] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[15] + b7; }
    }
    let oh1 = oh0 + 1u;
    if oh1 < hout && ow0 < wout {
        let obase = oh1 * wout + ow0;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[16] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[17] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[18] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[19] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[20] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[21] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[22] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[23] + b7; }
    }
    if oh1 < hout && ow1 < wout {
        let obase = oh1 * wout + ow1;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[24] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[25] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[26] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[27] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[28] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[29] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[30] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[31] + b7; }
    }
}
"#;

// 5×5 stride-1 1×2 spatial tile × 8 output channels.
// Each thread accumulates 2 consecutive output columns × 8 channels = 16 values,
// doubling arithmetic intensity vs the scalar CONV5X5_COBLOCK8_WGSL while keeping
// 256 threads/workgroup for occupancy. Works for any dilation where
// (16 + 4*dil) * (32 + 4*dil) ≤ 1296, i.e., d≤3.
// Same params layout as CONV5X5_COBLOCK8_WGSL.
// dispatch: (ceil(Wout/32), ceil(Hout/16), ceil(Cout/8)).
pub(crate) const CONV5X5_CO8_SP1X2_WGSL: &str = r#"
var<workgroup> smem: array<f32, 1296>;  // max 28*44=1232 for d3
var<workgroup> wmem: array<f32, 200>;   // 8 * 25 taps

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let pad_top  = params[8];
    let pad_left = params[9];
    let dil_h    = params[10];
    let dil_w    = params[11];
    let use_bias = params[12];

    let co0 = wg.z * 8u;
    let oh  = wg.y * 16u + lid.y;    // 1 output row per thread
    let ow0 = wg.x * 32u + lid.x * 2u;  // first of 2 output cols
    let ow1 = ow0 + 1u;               // second output col
    let valid0 = oh < hout && ow0 < wout;
    let valid1 = oh < hout && ow1 < wout;

    // Accumulators: acc[0..7] for col0, acc[8..15] for col1.
    // Bias is folded in at store time so acc stays statically indexed and in
    // registers (naga 29 spills dynamically indexed private arrays to local
    // memory — ~12x slower; see CONV3X3_CO8_SP2X2).
    var acc = array<f32, 16>();

    let patch_h = 16u * stride_h + 4u * dil_h;
    let patch_w = 32u * stride_w + 4u * dil_w;
    let patch_size = patch_h * patch_w;
    let ih_base_i = i32(wg.y * 16u * stride_h) - i32(pad_top);
    let iw_base_i = i32(wg.x * 32u * stride_w) - i32(pad_left);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        // Cooperatively load the patch.
        var pidx = tid;
        loop {
            if pidx >= patch_size { break; }
            let py = pidx / patch_w;
            let px = pidx - py * patch_w;
            let iy = ih_base_i + i32(py);
            let ix = iw_base_i + i32(px);
            if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
            } else {
                smem[py * patch_w + px] = 0.0;
            }
            pidx = pidx + 256u;
        }

        // Load 8×25 weight tile.
        if tid < 200u {
            let co_local = tid / 25u;
            let kk = tid - co_local * 25u;
            let co = co0 + co_local;
            if co < cout {
                wmem[tid] = wt[co * cin * 25u + ci * 25u + kk];
            } else {
                wmem[tid] = 0.0;
            }
        }

        workgroupBarrier();

        if valid0 || valid1 {
            let sm_row_base = lid.y * stride_h;
            let sm_col0_base = lid.x * 2u * stride_w;
            for (var ky = 0u; ky < 5u; ky = ky + 1u) {
                for (var kx = 0u; kx < 5u; kx = kx + 1u) {
                    let kk = ky * 5u + kx;
                    let sm_row = sm_row_base + ky * dil_h;
                    let sm_col0 = sm_col0_base + kx * dil_w;
                    let v0 = smem[sm_row * patch_w + sm_col0];
                    let v1 = smem[sm_row * patch_w + sm_col0 + stride_w];
                    // Unrolled over 8 channels — constant acc indices keep it in registers.
                    let w0 = wmem[kk];
                    let w1 = wmem[25u + kk];
                    let w2 = wmem[50u + kk];
                    let w3 = wmem[75u + kk];
                    let w4 = wmem[100u + kk];
                    let w5 = wmem[125u + kk];
                    let w6 = wmem[150u + kk];
                    let w7 = wmem[175u + kk];
                    acc[0] = acc[0] + v0 * w0; acc[8]  = acc[8]  + v1 * w0;
                    acc[1] = acc[1] + v0 * w1; acc[9]  = acc[9]  + v1 * w1;
                    acc[2] = acc[2] + v0 * w2; acc[10] = acc[10] + v1 * w2;
                    acc[3] = acc[3] + v0 * w3; acc[11] = acc[11] + v1 * w3;
                    acc[4] = acc[4] + v0 * w4; acc[12] = acc[12] + v1 * w4;
                    acc[5] = acc[5] + v0 * w5; acc[13] = acc[13] + v1 * w5;
                    acc[6] = acc[6] + v0 * w6; acc[14] = acc[14] + v1 * w6;
                    acc[7] = acc[7] + v0 * w7; acc[15] = acc[15] + v1 * w7;
                }
            }
        }

        workgroupBarrier();
    }

    let plane = hout * wout;
    var b0 = 0.0; var b1 = 0.0; var b2 = 0.0; var b3 = 0.0;
    var b4 = 0.0; var b5 = 0.0; var b6 = 0.0; var b7 = 0.0;
    if use_bias != 0u {
        if co0 + 0u < cout { b0 = bias[co0 + 0u]; }
        if co0 + 1u < cout { b1 = bias[co0 + 1u]; }
        if co0 + 2u < cout { b2 = bias[co0 + 2u]; }
        if co0 + 3u < cout { b3 = bias[co0 + 3u]; }
        if co0 + 4u < cout { b4 = bias[co0 + 4u]; }
        if co0 + 5u < cout { b5 = bias[co0 + 5u]; }
        if co0 + 6u < cout { b6 = bias[co0 + 6u]; }
        if co0 + 7u < cout { b7 = bias[co0 + 7u]; }
    }
    if valid0 {
        let obase = oh * wout + ow0;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[0] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[1] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[2] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[3] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[4] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[5] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[6] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[7] + b7; }
    }
    if valid1 {
        let obase = oh * wout + ow1;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[8] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[9] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[10] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[11] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[12] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[13] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[14] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[15] + b7; }
    }
}
"#;

// 5×5 stride-1 2×1 spatial tile × 8 output channels.
// YOLO-specific trial for the 96-channel 128×128 d2/d3 dilated blocks. It mirrors
// CONV5X5_CO8_SP1X2_WGSL but computes two consecutive rows per thread instead of
// two consecutive columns. Patch footprint is 40×24 for d2 and 44×28 for d3.
// Same params layout as CONV5X5_COBLOCK8_WGSL.
// dispatch: (ceil(Wout/16), ceil(Hout/32), ceil(Cout/8)).
pub(crate) const CONV5X5_CO8_SP2X1_WGSL: &str = r#"
var<workgroup> smem: array<f32, 1296>;
var<workgroup> wmem: array<f32, 200>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let pad_top  = params[8];
    let pad_left = params[9];
    let dil_h    = params[10];
    let dil_w    = params[11];
    let use_bias = params[12];

    let co0 = wg.z * 8u;
    let oh0 = wg.y * 32u + lid.y * 2u;
    let oh1 = oh0 + 1u;
    let ow  = wg.x * 16u + lid.x;
    let valid0 = oh0 < hout && ow < wout;
    let valid1 = oh1 < hout && ow < wout;

    var acc = array<f32, 16>();

    let patch_h = 32u * stride_h + 4u * dil_h;
    let patch_w = 16u * stride_w + 4u * dil_w;
    let patch_size = patch_h * patch_w;
    let ih_base_i = i32(wg.y * 32u * stride_h) - i32(pad_top);
    let iw_base_i = i32(wg.x * 16u * stride_w) - i32(pad_left);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        var pidx = tid;
        loop {
            if pidx >= patch_size { break; }
            let py = pidx / patch_w;
            let px = pidx - py * patch_w;
            let iy = ih_base_i + i32(py);
            let ix = iw_base_i + i32(px);
            if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
            } else {
                smem[py * patch_w + px] = 0.0;
            }
            pidx = pidx + 256u;
        }

        if tid < 200u {
            let co_local = tid / 25u;
            let kk = tid - co_local * 25u;
            let co = co0 + co_local;
            if co < cout {
                wmem[tid] = wt[co * cin * 25u + ci * 25u + kk];
            } else {
                wmem[tid] = 0.0;
            }
        }

        workgroupBarrier();

        if valid0 || valid1 {
            let sm_row0_base = lid.y * 2u * stride_h;
            let sm_col_base = lid.x * stride_w;
            for (var ky = 0u; ky < 5u; ky = ky + 1u) {
                for (var kx = 0u; kx < 5u; kx = kx + 1u) {
                    let kk = ky * 5u + kx;
                    let sm_col = sm_col_base + kx * dil_w;
                    let sm_row0 = sm_row0_base + ky * dil_h;
                    let v0 = smem[sm_row0 * patch_w + sm_col];
                    let v1 = smem[(sm_row0 + stride_h) * patch_w + sm_col];
                    let w0 = wmem[kk];
                    let w1 = wmem[25u + kk];
                    let w2 = wmem[50u + kk];
                    let w3 = wmem[75u + kk];
                    let w4 = wmem[100u + kk];
                    let w5 = wmem[125u + kk];
                    let w6 = wmem[150u + kk];
                    let w7 = wmem[175u + kk];
                    acc[0] = acc[0] + v0 * w0; acc[8]  = acc[8]  + v1 * w0;
                    acc[1] = acc[1] + v0 * w1; acc[9]  = acc[9]  + v1 * w1;
                    acc[2] = acc[2] + v0 * w2; acc[10] = acc[10] + v1 * w2;
                    acc[3] = acc[3] + v0 * w3; acc[11] = acc[11] + v1 * w3;
                    acc[4] = acc[4] + v0 * w4; acc[12] = acc[12] + v1 * w4;
                    acc[5] = acc[5] + v0 * w5; acc[13] = acc[13] + v1 * w5;
                    acc[6] = acc[6] + v0 * w6; acc[14] = acc[14] + v1 * w6;
                    acc[7] = acc[7] + v0 * w7; acc[15] = acc[15] + v1 * w7;
                }
            }
        }

        workgroupBarrier();
    }

    let plane = hout * wout;
    var b0 = 0.0; var b1 = 0.0; var b2 = 0.0; var b3 = 0.0;
    var b4 = 0.0; var b5 = 0.0; var b6 = 0.0; var b7 = 0.0;
    if use_bias != 0u {
        if co0 + 0u < cout { b0 = bias[co0 + 0u]; }
        if co0 + 1u < cout { b1 = bias[co0 + 1u]; }
        if co0 + 2u < cout { b2 = bias[co0 + 2u]; }
        if co0 + 3u < cout { b3 = bias[co0 + 3u]; }
        if co0 + 4u < cout { b4 = bias[co0 + 4u]; }
        if co0 + 5u < cout { b5 = bias[co0 + 5u]; }
        if co0 + 6u < cout { b6 = bias[co0 + 6u]; }
        if co0 + 7u < cout { b7 = bias[co0 + 7u]; }
    }
    if valid0 {
        let obase = oh0 * wout + ow;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[0] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[1] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[2] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[3] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[4] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[5] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[6] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[7] + b7; }
    }
    if valid1 {
        let obase = oh1 * wout + ow;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[8] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[9] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[10] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[11] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[12] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[13] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[14] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[15] + b7; }
    }
}
"#;

// 3×3 stride-1 1×2 spatial tile × 8 output channels.
// Targets the dilated 3×3 blocks where the 2×2 spatial tile would exceed the
// 1296-float shared-memory budget at d=5. Patch for d=5 is 26×42=1092.
// params match CONV3X3_COBLOCK8_WGSL.
// dispatch: (ceil(Wout/32), ceil(Hout/16), ceil(Cout/8)).
pub(crate) const CONV3X3_CO8_SP1X2_WGSL: &str = r#"
var<workgroup> smem: array<f32, 1296>;
var<workgroup> wmem: array<f32, 72>;

@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(16, 16)
fn main(
    @builtin(workgroup_id)        wg : vec3<u32>,
    @builtin(local_invocation_id) lid: vec3<u32>,
) {
    let cout     = params[0];
    let cin      = params[1];
    let hin      = params[2];
    let win      = params[3];
    let hout     = params[4];
    let wout     = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let pad_top  = params[8];
    let pad_left = params[9];
    let dil_h    = params[10];
    let dil_w    = params[11];
    let use_bias = params[12];

    let co0 = wg.z * 8u;
    let oh  = wg.y * 16u + lid.y;
    let ow0 = wg.x * 32u + lid.x * 2u;
    let ow1 = ow0 + 1u;
    let valid0 = oh < hout && ow0 < wout;
    let valid1 = oh < hout && ow1 < wout;

    var acc = array<f32, 16>();

    let patch_h = 16u * stride_h + 2u * dil_h;
    let patch_w = 32u * stride_w + 2u * dil_w;
    let patch_size = patch_h * patch_w;
    let ih_base_i = i32(wg.y * 16u * stride_h) - i32(pad_top);
    let iw_base_i = i32(wg.x * 32u * stride_w) - i32(pad_left);
    let full_patch = ih_base_i >= 0 && iw_base_i >= 0
                  && ih_base_i + i32(patch_h) <= i32(hin)
                  && iw_base_i + i32(patch_w) <= i32(win);
    let tid = lid.y * 16u + lid.x;

    for (var ci: u32 = 0u; ci < cin; ci = ci + 1u) {
        var pidx = tid;
        loop {
            if pidx >= patch_size { break; }
            let py = pidx / patch_w;
            let px = pidx - py * patch_w;
            if full_patch {
                smem[py * patch_w + px] = inp[ci * hin * win + u32(ih_base_i) * win + u32(iw_base_i) + py * win + px];
            } else {
                let iy = ih_base_i + i32(py);
                let ix = iw_base_i + i32(px);
                if iy >= 0 && u32(iy) < hin && ix >= 0 && u32(ix) < win {
                    smem[py * patch_w + px] = inp[ci * hin * win + u32(iy) * win + u32(ix)];
                } else {
                    smem[py * patch_w + px] = 0.0;
                }
            }
            pidx = pidx + 256u;
        }

        if tid < 72u {
            let co_local = tid / 9u;
            let kk = tid - co_local * 9u;
            let co = co0 + co_local;
            if co < cout {
                wmem[tid] = wt[co * cin * 9u + ci * 9u + kk];
            } else {
                wmem[tid] = 0.0;
            }
        }

        workgroupBarrier();

        if valid0 || valid1 {
            let sm_row_base = lid.y * stride_h;
            let sm_col0_base = lid.x * 2u * stride_w;
            for (var ky = 0u; ky < 3u; ky = ky + 1u) {
                for (var kx = 0u; kx < 3u; kx = kx + 1u) {
                    let kk = ky * 3u + kx;
                    let sm_row = sm_row_base + ky * dil_h;
                    let sm_col0 = sm_col0_base + kx * dil_w;
                    let v0 = smem[sm_row * patch_w + sm_col0];
                    let v1 = smem[sm_row * patch_w + sm_col0 + stride_w];
                    let w0 = wmem[kk];
                    let w1 = wmem[9u + kk];
                    let w2 = wmem[18u + kk];
                    let w3 = wmem[27u + kk];
                    let w4 = wmem[36u + kk];
                    let w5 = wmem[45u + kk];
                    let w6 = wmem[54u + kk];
                    let w7 = wmem[63u + kk];
                    acc[0] = acc[0] + v0 * w0; acc[8]  = acc[8]  + v1 * w0;
                    acc[1] = acc[1] + v0 * w1; acc[9]  = acc[9]  + v1 * w1;
                    acc[2] = acc[2] + v0 * w2; acc[10] = acc[10] + v1 * w2;
                    acc[3] = acc[3] + v0 * w3; acc[11] = acc[11] + v1 * w3;
                    acc[4] = acc[4] + v0 * w4; acc[12] = acc[12] + v1 * w4;
                    acc[5] = acc[5] + v0 * w5; acc[13] = acc[13] + v1 * w5;
                    acc[6] = acc[6] + v0 * w6; acc[14] = acc[14] + v1 * w6;
                    acc[7] = acc[7] + v0 * w7; acc[15] = acc[15] + v1 * w7;
                }
            }
        }

        workgroupBarrier();
    }

    let plane = hout * wout;
    var b0 = 0.0; var b1 = 0.0; var b2 = 0.0; var b3 = 0.0;
    var b4 = 0.0; var b5 = 0.0; var b6 = 0.0; var b7 = 0.0;
    if use_bias != 0u {
        if co0 + 0u < cout { b0 = bias[co0 + 0u]; }
        if co0 + 1u < cout { b1 = bias[co0 + 1u]; }
        if co0 + 2u < cout { b2 = bias[co0 + 2u]; }
        if co0 + 3u < cout { b3 = bias[co0 + 3u]; }
        if co0 + 4u < cout { b4 = bias[co0 + 4u]; }
        if co0 + 5u < cout { b5 = bias[co0 + 5u]; }
        if co0 + 6u < cout { b6 = bias[co0 + 6u]; }
        if co0 + 7u < cout { b7 = bias[co0 + 7u]; }
    }
    if valid0 {
        let obase = oh * wout + ow0;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[0] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[1] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[2] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[3] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[4] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[5] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[6] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[7] + b7; }
    }
    if valid1 {
        let obase = oh * wout + ow1;
        if co0 + 0u < cout { out[(co0 + 0u) * plane + obase] = acc[8] + b0; }
        if co0 + 1u < cout { out[(co0 + 1u) * plane + obase] = acc[9] + b1; }
        if co0 + 2u < cout { out[(co0 + 2u) * plane + obase] = acc[10] + b2; }
        if co0 + 3u < cout { out[(co0 + 3u) * plane + obase] = acc[11] + b3; }
        if co0 + 4u < cout { out[(co0 + 4u) * plane + obase] = acc[12] + b4; }
        if co0 + 5u < cout { out[(co0 + 5u) * plane + obase] = acc[13] + b5; }
        if co0 + 6u < cout { out[(co0 + 6u) * plane + obase] = acc[14] + b6; }
        if co0 + 7u < cout { out[(co0 + 7u) * plane + obase] = acc[15] + b7; }
    }
}
"#;

// ── Depthwise 3×3 direct conv ────────────────────────────────────────────────
//
// One output element per invocation. This avoids the shared-memory/barrier
// overhead of the generic tiled 3×3 shader for depthwise layers, where each
// output channel only reads one input channel.
//
// params (11 x u32):
//   [0]num_out [1]channels [2]hin [3]win [4]hout [5]wout
//   [6]stride_h [7]stride_w [8]pad_top [9]pad_left [10]use_bias
pub(crate) const DEPTHWISE3X3_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       wt     : array<f32>;
@group(0) @binding(2) var<storage, read>       bias   : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let num_out = params[0];
    if i >= num_out { return; }

    let channels = params[1];
    let hin = params[2];
    let win = params[3];
    let hout = params[4];
    let wout = params[5];
    let stride_h = params[6];
    let stride_w = params[7];
    let pad_top = params[8];
    let pad_left = params[9];
    let use_bias = params[10];

    let c = i / (hout * wout);
    let rem = i - c * hout * wout;
    let oh = rem / wout;
    let ow = rem - oh * wout;

    var acc = 0.0f;
    if use_bias != 0u { acc = bias[c]; }

    for (var ky: u32 = 0u; ky < 3u; ky = ky + 1u) {
        let ih_i = i32(oh * stride_h + ky) - i32(pad_top);
        if ih_i < 0 || u32(ih_i) >= hin { continue; }
        let ih = u32(ih_i);
        for (var kx: u32 = 0u; kx < 3u; kx = kx + 1u) {
            let iw_i = i32(ow * stride_w + kx) - i32(pad_left);
            if iw_i < 0 || u32(iw_i) >= win { continue; }
            let iw = u32(iw_i);
            acc = acc + inp[c * hin * win + ih * win + iw] * wt[c * 9u + ky * 3u + kx];
        }
    }

    out[i] = acc;
}
"#;

pub(crate) fn is_conv1x1_gemm(plan: &Conv2dPlan, w_shape: &[usize]) -> bool {
    w_shape[2] == 1
        && w_shape[3] == 1
        && plan.group == 1
        && plan.pads == [0, 0, 0, 0]
        && plan.dilations == [1, 1]
}

pub(crate) fn is_conv3x3_tiled(plan: &Conv2dPlan, w_shape: &[usize]) -> bool {
    w_shape[2] == 3 && w_shape[3] == 3 && conv3x3_tile_patch_fits(plan)
}

pub(crate) fn conv3x3_tile_patch_fits(plan: &Conv2dPlan) -> bool {
    let patch_h = 16usize * plan.strides[0] as usize + 2usize * plan.dilations[0] as usize;
    let patch_w = 16usize * plan.strides[1] as usize + 2usize * plan.dilations[1] as usize;
    patch_h * patch_w <= 1296
}

/// 3×3 stride-1 dilation-1 conv via the 2×2-spatial × 8-channel register tile.
/// Mirrors the compiled-executor selection so gpu-diff can validate it.
pub(crate) async fn run_conv3x3_co8_sp2x2_s1d1(
    ctx: &GpuContext,
    plan: &Conv2dPlan,
    inputs: &[Tensor],
) -> Result<Tensor> {
    let x = &inputs[0];
    let w = &inputs[1];
    let bias_opt = inputs.get(2);
    let cin = x.shape[1];
    let hin = x.shape[2];
    let win = x.shape[3];
    let cout = w.shape[0];
    let hout = conv_out_usize(hin, plan.pads[0], plan.pads[2], 1, 3, 1)?;
    let wout = conv_out_usize(win, plan.pads[1], plan.pads[3], 1, 3, 1)?;
    let has_bias = bias_opt.is_some();
    let bias_data: Vec<f32> = match bias_opt {
        Some(b) => b.data.clone(),
        None => vec![0.0f32; 1],
    };
    let p = [
        cout as u32,
        cin as u32,
        hin as u32,
        win as u32,
        hout as u32,
        wout as u32,
        plan.pads[0] as u32,
        plan.pads[1] as u32,
        has_bias as u32,
    ];
    let raw = dispatch_compute(
        ctx,
        CONV3X3_CO8_SP2X2_S1D1_WGSL,
        &[
            bytemuck::cast_slice(&x.data),
            bytemuck::cast_slice(&w.data),
            bytemuck::cast_slice(&bias_data),
        ],
        cout * hout * wout * 4,
        bytemuck::cast_slice(&p),
        (
            wout.div_ceil(32) as u32,
            hout.div_ceil(32) as u32,
            cout.div_ceil(8) as u32,
        ),
    )
    .await?;
    Tensor::new(vec![1, cout, hout, wout], f32_from_bytes(&raw))
}

pub(crate) async fn run_conv3x3_tiled(
    ctx: &GpuContext,
    plan: &Conv2dPlan,
    inputs: &[Tensor],
) -> Result<Tensor> {
    let x = &inputs[0];
    let w = &inputs[1];
    let bias_opt = inputs.get(2);
    let cin = x.shape[1];
    let hin = x.shape[2];
    let win = x.shape[3];
    let cout = w.shape[0];
    let cin_per_group = w.shape[1];
    let kh = 3usize;
    let kw = 3usize;
    let group = plan.group as usize;
    let hout = conv_out_usize(
        hin,
        plan.pads[0],
        plan.pads[2],
        plan.dilations[0],
        kh,
        plan.strides[0],
    )?;
    let wout = conv_out_usize(
        win,
        plan.pads[1],
        plan.pads[3],
        plan.dilations[1],
        kw,
        plan.strides[1],
    )?;
    let num_out = cout * hout * wout;
    let has_bias = bias_opt.is_some();
    let bias_data: Vec<f32> = match bias_opt {
        Some(b) => b.data.clone(),
        None => vec![0.0f32; 1],
    };
    let cout_per_group = cout / group;
    let _ = cout_per_group;
    let p = [
        cout as u32,
        cin as u32,
        hin as u32,
        win as u32,
        hout as u32,
        wout as u32,
        kh as u32,
        kw as u32,
        plan.strides[0] as u32,
        plan.strides[1] as u32,
        plan.pads[0] as u32,
        plan.pads[1] as u32,
        plan.dilations[0] as u32,
        plan.dilations[1] as u32,
        has_bias as u32,
        cin_per_group as u32,
    ];
    let wg_x = wout.div_ceil(16) as u32;
    let wg_y = hout.div_ceil(16) as u32;
    let wg_z = cout as u32;
    let raw = dispatch_compute(
        ctx,
        CONV3X3_TILED_WGSL,
        &[
            bytemuck::cast_slice(&x.data),
            bytemuck::cast_slice(&w.data),
            bytemuck::cast_slice(&bias_data),
        ],
        num_out * 4,
        bytemuck::cast_slice(&p),
        (wg_x, wg_y, wg_z),
    )
    .await?;
    Tensor::new(vec![1, cout, hout, wout], f32_from_bytes(&raw))
}

/// Stride-1 same-size 1×1 conv via the register-blocked GEMM kernel.
/// Mirrors the compiled-executor selection so gpu-diff can validate it.
pub(crate) async fn run_conv1x1_gemm_s1(
    ctx: &GpuContext,
    plan: &Conv2dPlan,
    inputs: &[Tensor],
) -> Result<Tensor> {
    let x = &inputs[0];
    let w = &inputs[1];
    let bias_opt = inputs.get(2);
    let cin = x.shape[1];
    let hin = x.shape[2];
    let win = x.shape[3];
    let cout = w.shape[0];
    let plane = hin * win;
    let _ = plan;
    let has_bias = bias_opt.is_some();
    let bias_data: Vec<f32> = match bias_opt {
        Some(b) => b.data.clone(),
        None => vec![0.0f32; 1],
    };
    let p = [cout as u32, cin as u32, plane as u32, has_bias as u32];
    let gemm_wgsl = if cin % 4 == 0 && plane % 4 == 0 {
        CONV1X1_GEMM_S1_VEC4_WGSL
    } else {
        CONV1X1_GEMM_S1_WGSL
    };
    let raw = dispatch_compute(
        ctx,
        gemm_wgsl,
        &[
            bytemuck::cast_slice(&x.data),
            bytemuck::cast_slice(&w.data),
            bytemuck::cast_slice(&bias_data),
        ],
        cout * plane * 4,
        bytemuck::cast_slice(&p),
        (cout.div_ceil(64) as u32, plane.div_ceil(64) as u32, 1),
    )
    .await?;
    Tensor::new(vec![1, cout, hin, win], f32_from_bytes(&raw))
}

pub(crate) async fn run_conv1x1(
    ctx: &GpuContext,
    plan: &Conv2dPlan,
    inputs: &[Tensor],
) -> Result<Tensor> {
    let x = &inputs[0];
    let w = &inputs[1];
    let bias_opt = inputs.get(2);
    let cin = x.shape[1];
    let hin = x.shape[2];
    let win = x.shape[3];
    let cout = w.shape[0];
    let hout = conv_out_usize(hin, 0, 0, 1, 1, plan.strides[0])?;
    let wout = conv_out_usize(win, 0, 0, 1, 1, plan.strides[1])?;
    let num_out = cout * hout * wout;
    let has_bias = bias_opt.is_some();
    let bias_data: Vec<f32> = match bias_opt {
        Some(b) => b.data.clone(),
        None => vec![0.0f32; 1],
    };
    let p = [
        cout as u32,
        cin as u32,
        hout as u32,
        wout as u32,
        hin as u32,
        win as u32,
        plan.strides[0] as u32,
        plan.strides[1] as u32,
        has_bias as u32,
    ];
    let wg_x = cout.div_ceil(16) as u32;
    let wg_y = (hout * wout).div_ceil(16) as u32;
    let raw = dispatch_compute(
        ctx,
        CONV1X1_WGSL,
        &[
            bytemuck::cast_slice(&x.data),
            bytemuck::cast_slice(&w.data),
            bytemuck::cast_slice(&bias_data),
        ],
        num_out * 4,
        bytemuck::cast_slice(&p),
        (wg_x, wg_y, 1),
    )
    .await?;
    Tensor::new(vec![1, cout, hout, wout], f32_from_bytes(&raw))
}

pub(crate) fn conv_out_usize(
    inp: usize,
    pad0: i64,
    pad1: i64,
    dil: i64,
    k: usize,
    stride: i64,
) -> Result<usize> {
    let v = (inp as i64 + pad0 + pad1 - dil * (k as i64 - 1) - 1) / stride + 1;
    usize::try_from(v).context("conv output dimension is non-positive")
}
