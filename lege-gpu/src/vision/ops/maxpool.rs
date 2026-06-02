use anyhow::{Result, bail};

use super::common::f32_from_bytes;
use super::conv::conv_out_usize;
use crate::vision::onnx::types::Pool2dPlan;
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params (14 x u32):
//   [0] num_elems  [1] channels  [2] hin  [3] win
//   [4] hout  [5] wout  [6] kh  [7] kw
//   [8] stride_h  [9] stride_w  [10] pad_top  [11] pad_left
//   [12] dilation_h  [13] dilation_w
pub(crate) const MAXPOOL2D_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }

    let hout = params[4];
    let wout = params[5];
    let ch   = i / (hout * wout);
    let oh   = (i / wout) % hout;
    let ow   = i % wout;

    let hin        = params[2];
    let win        = params[3];
    let kh         = params[6];
    let kw         = params[7];
    let stride_h   = params[8];
    let stride_w   = params[9];
    let pad_top    = params[10];
    let pad_left   = params[11];
    let dilation_h = params[12];
    let dilation_w = params[13];

    var max_val = -3.402823466e+38f;

    for (var ky: u32 = 0u; ky < kh; ky = ky + 1u) {
        let ih_raw = i32(oh) * i32(stride_h) + i32(ky) * i32(dilation_h) - i32(pad_top);
        if ih_raw < 0 || u32(ih_raw) >= hin { continue; }
        let ih = u32(ih_raw);
        for (var kx: u32 = 0u; kx < kw; kx = kx + 1u) {
            let iw_raw = i32(ow) * i32(stride_w) + i32(kx) * i32(dilation_w) - i32(pad_left);
            if iw_raw < 0 || u32(iw_raw) >= win { continue; }
            let iw = u32(iw_raw);
            let v = inp[ch * hin * win + ih * win + iw];
            if v > max_val { max_val = v; }
        }
    }
    out[i] = max_val;
}
"#;

pub(crate) async fn run_maxpool2d(
    ctx: &GpuContext,
    plan: &Pool2dPlan,
    input: &Tensor,
) -> Result<Tensor> {
    if input.shape.len() != 4 {
        bail!("MaxPool2d: expected rank-4 input");
    }
    let n = input.shape[0];
    if n != 1 {
        bail!("MaxPool2d GPU: batch must be 1 (got {n})");
    }
    let channels = input.shape[1];
    let hin = input.shape[2];
    let win = input.shape[3];
    let kh = plan.kernel_shape[0] as usize;
    let kw = plan.kernel_shape[1] as usize;
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
    let num_out = channels * hout * wout;

    let params = [
        num_out as u32,
        channels as u32,
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
    ];
    let raw = dispatch_compute(
        ctx,
        MAXPOOL2D_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        num_out * 4,
        bytemuck::cast_slice(&params),
        (num_out.div_ceil(256) as u32, 1, 1),
    )
    .await?;
    Tensor::new(vec![1, channels, hout, wout], f32_from_bytes(&raw))
}
