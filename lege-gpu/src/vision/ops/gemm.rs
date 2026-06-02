//! ONNX Gemm: Y = alpha * A' * B' + beta * C, where A'/B' are A/B optionally
//! transposed. A is [M,K] (or [K,M] if transA), B is [K,N] (or [N,K] if transB),
//! C is unidirectionally broadcastable to [M,N] (we support len N, M*N, or 1).
//! 2D only — the fused MatMul+Add classifier heads these models use.

use anyhow::{Result, bail};

use super::common::f32_from_bytes;
use crate::vision::reference::Tensor;
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// params: [M, N, K, transA, transB, has_bias, bias_len, alpha_bits, beta_bits]
pub(crate) const GEMM_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       a      : array<f32>;
@group(0) @binding(1) var<storage, read>       b      : array<f32>;
@group(0) @binding(2) var<storage, read>       c      : array<f32>;
@group(0) @binding(3) var<storage, read_write> out    : array<f32>;
@group(0) @binding(4) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let m = params[0];
    let n = params[1];
    let k = params[2];
    if i >= m * n { return; }
    let trans_a  = params[3];
    let trans_b  = params[4];
    let has_bias = params[5];
    let bias_len = params[6];
    let alpha = bitcast<f32>(params[7]);
    let beta  = bitcast<f32>(params[8]);
    let row = i / n;
    let col = i % n;
    var acc = 0.0;
    for (var kk: u32 = 0u; kk < k; kk = kk + 1u) {
        var av: f32;
        if trans_a == 1u { av = a[kk * m + row]; } else { av = a[row * k + kk]; }
        var bv: f32;
        if trans_b == 1u { bv = b[col * k + kk]; } else { bv = b[kk * n + col]; }
        acc = acc + av * bv;
    }
    var y = alpha * acc;
    if has_bias == 1u {
        var bi: u32 = i;
        if bias_len == 1u { bi = 0u; } else if bias_len == n { bi = col; }
        y = y + beta * c[bi];
    }
    out[i] = y;
}
"#;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_gemm(
    ctx: &GpuContext,
    a: &Tensor,
    b: &Tensor,
    bias: Option<&Tensor>,
    alpha: f32,
    beta: f32,
    trans_a: bool,
    trans_b: bool,
) -> Result<Tensor> {
    if a.shape.len() != 2 || b.shape.len() != 2 {
        bail!("Gemm expects 2D A and B, got {:?} {:?}", a.shape, b.shape);
    }
    let (m, ka) = if trans_a {
        (a.shape[1], a.shape[0])
    } else {
        (a.shape[0], a.shape[1])
    };
    let (kb, n) = if trans_b {
        (b.shape[1], b.shape[0])
    } else {
        (b.shape[0], b.shape[1])
    };
    if ka != kb {
        bail!("Gemm contracting dims mismatch: {ka} vs {kb}");
    }
    let dummy = [0.0f32];
    let bias_data = bias.map(|t| t.data.as_slice()).unwrap_or(&dummy);
    let params = [
        m as u32,
        n as u32,
        ka as u32,
        trans_a as u32,
        trans_b as u32,
        bias.is_some() as u32,
        bias.map(|t| t.data.len()).unwrap_or(0) as u32,
        alpha.to_bits(),
        beta.to_bits(),
    ];
    let raw = dispatch_compute(
        ctx,
        GEMM_WGSL,
        &[
            bytemuck::cast_slice(&a.data),
            bytemuck::cast_slice(&b.data),
            bytemuck::cast_slice(bias_data),
        ],
        m * n * 4,
        bytemuck::cast_slice(&params),
        ((m * n).div_ceil(256) as u32, 1, 1),
    )
    .await?;
    Tensor::new(vec![m, n], f32_from_bytes(&raw))
}
