use anyhow::{Result, bail};

use super::common::f32_from_bytes;
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params (6 x u32):
//   [0] num_out  [1] channels  [2] hin  [3] win  [4] hout  [5] wout
pub(crate) const RESIZE_NEAREST_WGSL: &str = r#"
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

    let hin = params[2];
    let win = params[3];

    // Asymmetric: floor(oh * hin / hout). Integer division gives floor.
    let ih = oh * hin / hout;
    let iw = ow * win / wout;

    out[i] = inp[ch * hin * win + ih * win + iw];
}
"#;

pub(crate) async fn run_resize_nearest(
    ctx: &GpuContext,
    scales: &[f32],
    input: &Tensor,
) -> Result<Tensor> {
    if input.shape.len() != 4 {
        bail!("ResizeNearest: expected rank-4 input");
    }
    if scales.len() != 4 {
        bail!("ResizeNearest: expected 4 scales");
    }
    let channels = input.shape[1];
    let hin = input.shape[2];
    let win = input.shape[3];
    let hout = (hin as f32 * scales[2]).round() as usize;
    let wout = (win as f32 * scales[3]).round() as usize;
    let num_out = channels * hout * wout;

    let params = [
        num_out as u32,
        channels as u32,
        hin as u32,
        win as u32,
        hout as u32,
        wout as u32,
    ];
    let raw = dispatch_compute(
        ctx,
        RESIZE_NEAREST_WGSL,
        &[bytemuck::cast_slice(&input.data)],
        num_out * 4,
        bytemuck::cast_slice(&params),
        (num_out.div_ceil(256) as u32, 1, 1),
    )
    .await?;
    Tensor::new(vec![1, channels, hout, wout], f32_from_bytes(&raw))
}
