use anyhow::{Result, bail};

use super::common::{c_strides_raw, c_strides_u32, f32_from_bytes};
use crate::vision::reference::{self, Tensor};
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params (30 x u32):
//   [0] total_out [1] out_rank [2] lhs_rank [3] rhs_rank
//   [4] batch_rank [5] m [6] k [7] n
//   [8..13]  out_strides[0..5]
//   [14..19] lhs_batch_strides[0..5] (0 for broadcast)
//   [20] lhs_row_stride [21] lhs_k_stride
//   [22..27] rhs_batch_strides[0..5] (0 for broadcast)
//   [28] rhs_k_stride [29] rhs_col_stride
pub(crate) const MATMUL_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       lhs    : array<f32>;
@group(0) @binding(1) var<storage, read>       rhs    : array<f32>;
@group(0) @binding(2) var<storage, read_write> out    : array<f32>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }

    let out_rank = params[1];
    let batch_rank = params[4];
    let k = params[6];

    var remaining = i;
    var lhs_base : u32 = 0u;
    var rhs_base : u32 = 0u;
    var row : u32 = 0u;
    var col : u32 = 0u;

    for (var d: u32 = 0u; d < out_rank; d = d + 1u) {
        let coord = remaining / params[8u + d];
        remaining = remaining % params[8u + d];
        if d < batch_rank {
            lhs_base = lhs_base + coord * params[14u + d];
            rhs_base = rhs_base + coord * params[22u + d];
        } else if d == batch_rank {
            row = coord;
        } else {
            col = coord;
        }
    }

    var sum = 0.0f;
    for (var kk: u32 = 0u; kk < k; kk = kk + 1u) {
        let lhs_idx = lhs_base + row * params[20] + kk * params[21];
        let rhs_idx = rhs_base + kk * params[28] + col * params[29];
        sum = sum + lhs[lhs_idx] * rhs[rhs_idx];
    }
    out[i] = sum;
}
"#;

pub(crate) async fn run_matmul(ctx: &GpuContext, lhs: &Tensor, rhs: &Tensor) -> Result<Tensor> {
    if lhs.shape.len() < 2 || rhs.shape.len() < 2 {
        bail!("MatMul expects rank >= 2");
    }
    let m = lhs.shape[lhs.shape.len() - 2];
    let k = lhs.shape[lhs.shape.len() - 1];
    let rhs_k = rhs.shape[rhs.shape.len() - 2];
    let n = rhs.shape[rhs.shape.len() - 1];
    if k != rhs_k {
        bail!("MatMul contracting dimensions mismatch: {k} vs {rhs_k}");
    }

    let batch_shape = reference::broadcast_shape(
        &lhs.shape[..lhs.shape.len() - 2],
        &rhs.shape[..rhs.shape.len() - 2],
    )?;
    let mut out_shape = batch_shape.clone();
    out_shape.push(m);
    out_shape.push(n);
    if out_shape.len() > 6 {
        bail!(
            "MatMul GPU: output rank {} exceeds maximum 6",
            out_shape.len()
        );
    }

    let total_out = out_shape.iter().product::<usize>();
    let out_strides = c_strides_u32(&out_shape);
    let lhs_batch_strides =
        matmul_batch_strides_u32(&batch_shape, &lhs.shape[..lhs.shape.len() - 2], m * k)?;
    let rhs_batch_strides =
        matmul_batch_strides_u32(&batch_shape, &rhs.shape[..rhs.shape.len() - 2], k * n)?;

    let mut params = [0u32; 30];
    params[0] = total_out as u32;
    params[1] = out_shape.len() as u32;
    params[2] = lhs.shape.len() as u32;
    params[3] = rhs.shape.len() as u32;
    params[4] = batch_shape.len() as u32;
    params[5] = m as u32;
    params[6] = k as u32;
    params[7] = n as u32;
    params[8..14].copy_from_slice(&out_strides);
    params[14..20].copy_from_slice(&lhs_batch_strides);
    params[20] = k as u32;
    params[21] = 1;
    params[22..28].copy_from_slice(&rhs_batch_strides);
    params[28] = n as u32;
    params[29] = 1;

    let raw = dispatch_compute(
        ctx,
        MATMUL_WGSL,
        &[
            bytemuck::cast_slice(&lhs.data),
            bytemuck::cast_slice(&rhs.data),
        ],
        total_out * 4,
        bytemuck::cast_slice(&params),
        (total_out.div_ceil(256) as u32, 1, 1),
    )
    .await?;
    Tensor::new(out_shape, f32_from_bytes(&raw))
}

fn matmul_batch_strides_u32(
    out_batch: &[usize],
    in_batch: &[usize],
    matrix_elems: usize,
) -> Result<[u32; 6]> {
    if out_batch.len() > 6 {
        bail!(
            "MatMul GPU: batch rank {} exceeds maximum 6",
            out_batch.len()
        );
    }
    let pad = out_batch.len() - in_batch.len();
    let padded = (0..pad)
        .map(|_| 1usize)
        .chain(in_batch.iter().copied())
        .collect::<Vec<_>>();
    let padded_strides = c_strides_raw(&padded);
    let mut out = [0u32; 6];
    for dim in 0..out_batch.len() {
        out[dim] = if padded[dim] == 1 {
            0
        } else {
            (padded_strides[dim] * matrix_elems) as u32
        };
    }
    Ok(out)
}
