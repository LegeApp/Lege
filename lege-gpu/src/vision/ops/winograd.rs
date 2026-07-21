//! Winograd F(2×2, 3×3) convolution helpers.
//!
//! F(2,3) computes a 2×2 output tile from a 4×4 input tile and a 3×3 filter with
//! 16 element-wise multiplies instead of 36 (4 outputs × 9 taps) — a 2.25×
//! reduction in multiplies, at the cost of small input/weight/output transforms.
//!
//! Per single channel:
//!   U = G · g · Gᵀ          (4×4 transformed weight, `g` is the 3×3 filter)
//!   V = Bᵀ · d · B          (4×4 transformed input tile, `d` is the 4×4 patch)
//!   Y = Aᵀ · (U ⊙ V) · A    (2×2 output tile)
//! Multi-channel accumulates the Hadamard product in transform space before the
//! output transform: `Y = Aᵀ · (Σ_c U_c ⊙ V_c) · A`.
//!
//! Transform matrices for F(2,3) (standard Winograd / Lavin-Gray):
//!   Bᵀ = [[1,0,-1,0],[0,1,1,0],[0,-1,1,0],[0,1,0,-1]]
//!   G  = [[1,0,0],[1/2,1/2,1/2],[1/2,-1/2,1/2],[0,0,1]]
//!   Aᵀ = [[1,1,1,0],[0,1,-1,-1]]
//!
//! This module is the numeric source of truth: the GPU weight transform reuses
//! [`weight_transform_f23`] at graph-prep time, and the input/output transform
//! WGSL must match [`input_transform_f23`] / [`output_transform_f23`].

#[cfg(test)]
use anyhow::Result;

#[cfg(test)]
use super::common::f32_from_bytes;
#[cfg(test)]
use crate::vision::reference::Tensor;
#[cfg(test)]
use crate::vision::runtime::device::{GpuContext, dispatch_compute};

// ── GPU input transform: x[1,Cin,H,W] → V[16, Cin, P] ───────────────────────
//
// P = ceil(H/2)*ceil(W/2) output tiles (pad=1, the standard F(2,3) layout, keeps
// H,W). One thread per (ci, tile): gather the 4×4 input patch, apply V = Bᵀ·d·B,
// scatter the 16 transform-space values to V[e*Cin*P + ci*P + tile] (ξ-major so
// each ξ slab is a contiguous Cin×P matrix for the batched GEMM).
// params: [0]cin [1]h [2]w [3]ntw [4]nth
pub(crate) const WINOGRAD_INPUT_TRANSFORM_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> outv   : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cin = params[0];
    let h   = params[1];
    let w   = params[2];
    let ntw = params[3];
    let nth = params[4];
    let p = ntw * nth;
    let idx = gid.x;
    if idx >= cin * p { return; }

    let ci = idx / p;
    let tile = idx - ci * p;
    let ty = tile / ntw;
    let tx = tile - ty * ntw;
    let base = ci * h * w;

    // Gather the 4×4 input patch (top-left at 2*ty-1, 2*tx-1; zero-pad).
    var d: array<f32, 16>;
    for (var py = 0u; py < 4u; py = py + 1u) {
        let ih = i32(2u * ty + py) - 1;
        for (var px = 0u; px < 4u; px = px + 1u) {
            let iw = i32(2u * tx + px) - 1;
            var val = 0.0;
            if ih >= 0 && ih < i32(h) && iw >= 0 && iw < i32(w) {
                val = inp[base + u32(ih) * w + u32(iw)];
            }
            d[py * 4u + px] = val;
        }
    }

    // V = Bᵀ·d·B, fully unrolled. t = Bᵀ·d (per column c), then V[i] = t[i]·B.
    let obase = ci * p + tile;
    let stride = cin * p;
    var t0: array<f32, 4>;
    var t1: array<f32, 4>;
    var t2: array<f32, 4>;
    var t3: array<f32, 4>;
    for (var c = 0u; c < 4u; c = c + 1u) {
        let d0 = d[0u * 4u + c];
        let d1 = d[1u * 4u + c];
        let d2 = d[2u * 4u + c];
        let d3 = d[3u * 4u + c];
        t0[c] = d0 - d2;
        t1[c] = d1 + d2;
        t2[c] = d2 - d1;
        t3[c] = d1 - d3;
    }
    // V[i][n]: n0=t0-t2, n1=t1+t2, n2=t2-t1, n3=t1-t3 (per row i)
    outv[0u  * stride + obase] = t0[0] - t0[2];
    outv[1u  * stride + obase] = t0[1] + t0[2];
    outv[2u  * stride + obase] = t0[2] - t0[1];
    outv[3u  * stride + obase] = t0[1] - t0[3];
    outv[4u  * stride + obase] = t1[0] - t1[2];
    outv[5u  * stride + obase] = t1[1] + t1[2];
    outv[6u  * stride + obase] = t1[2] - t1[1];
    outv[7u  * stride + obase] = t1[1] - t1[3];
    outv[8u  * stride + obase] = t2[0] - t2[2];
    outv[9u  * stride + obase] = t2[1] + t2[2];
    outv[10u * stride + obase] = t2[2] - t2[1];
    outv[11u * stride + obase] = t2[1] - t2[3];
    outv[12u * stride + obase] = t3[0] - t3[2];
    outv[13u * stride + obase] = t3[1] + t3[2];
    outv[14u * stride + obase] = t3[2] - t3[1];
    outv[15u * stride + obase] = t3[1] - t3[3];
}
"#;

// ── GPU output transform: M[16, Cout, P] → y[1, Cout, H, W] ──────────────────
//
// One thread per (co, tile): read the 16 transform-space accumulators, apply
// Y = Aᵀ·m·A, scatter the 2×2 output (bounds-checked) plus bias.
// params: [0]cout [1]h [2]w [3]ntw [4]nth [5]use_bias
pub(crate) const WINOGRAD_OUTPUT_TRANSFORM_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       m      : array<f32>;
@group(0) @binding(1) var<storage, read>       bias   : array<f32>;
@group(0) @binding(2) var<storage, read_write> outy   : array<f32>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cout = params[0];
    let h    = params[1];
    let w    = params[2];
    let ntw  = params[3];
    let nth  = params[4];
    let use_bias = params[5];
    let p = ntw * nth;
    let idx = gid.x;
    if idx >= cout * p { return; }

    let co = idx / p;
    let tile = idx - co * p;
    let ty = tile / ntw;
    let tx = tile - ty * ntw;

    let mbase = co * p + tile;
    let stride = cout * p;
    var mm: array<f32, 16>;
    for (var e = 0u; e < 16u; e = e + 1u) {
        mm[e] = m[e * stride + mbase];
    }

    // s = Aᵀ·m (rows 0,1), then Y = s·A.
    var s0: array<f32, 4>;
    var s1: array<f32, 4>;
    for (var c = 0u; c < 4u; c = c + 1u) {
        let m0 = mm[0u * 4u + c];
        let m1 = mm[1u * 4u + c];
        let m2 = mm[2u * 4u + c];
        let m3 = mm[3u * 4u + c];
        s0[c] = m0 + m1 + m2;
        s1[c] = m1 - m2 - m3;
    }
    var b = 0.0;
    if use_bias != 0u { b = bias[co]; }
    let y00 = s0[0] + s0[1] + s0[2] + b;
    let y01 = s0[1] - s0[2] - s0[3] + b;
    let y10 = s1[0] + s1[1] + s1[2] + b;
    let y11 = s1[1] - s1[2] - s1[3] + b;

    let oh0 = 2u * ty;
    let ow0 = 2u * tx;
    let cbase = co * h * w;
    if oh0 < h && ow0 < w { outy[cbase + oh0 * w + ow0] = y00; }
    if oh0 < h && ow0 + 1u < w { outy[cbase + oh0 * w + ow0 + 1u] = y01; }
    if oh0 + 1u < h && ow0 < w { outy[cbase + (oh0 + 1u) * w + ow0] = y10; }
    if oh0 + 1u < h && ow0 + 1u < w { outy[cbase + (oh0 + 1u) * w + ow0 + 1u] = y11; }
}
"#;

// ── Batched 16-GEMM in transform space: M[ξ] = U[ξ]·V[ξ] ────────────────────
//
// 16 independent GEMMs, one per transform position ξ = wg.z. For each ξ:
//   U[ξ] is Cout×Cin (A), V[ξ] is Cin×P (B), M[ξ] is Cout×P (C).
// Identical register-blocked SGEMM as CONV1X1_GEMM_S1, with ξ-strided global
// offsets (A += ξ·Cout·Cin, B += ξ·Cin·P, C += ξ·Cout·P). No bias here — the
// output transform adds it. params: [0]M(cout) [1]K(cin) [2]N(p).
// dispatch: (ceil(Cout/64), ceil(P/64), 16).
pub(crate) const WINOGRAD_BATCHED_GEMM_WGSL: &str = r#"
const BM: u32 = 64u;
const BN: u32 = 64u;
const BK: u32 = 16u;

var<workgroup> As: array<f32, 1024>; // BK*BM, transposed As[k*BM + m]
var<workgroup> Bs: array<f32, 1024>; // BK*BN, Bs[k*BN + n]

@group(0) @binding(0) var<storage, read>       u_in   : array<f32>;
@group(0) @binding(1) var<storage, read>       v_in   : array<f32>;
@group(0) @binding(2) var<storage, read_write> m_out  : array<f32>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(
    @builtin(workgroup_id)           wg  : vec3<u32>,
    @builtin(local_invocation_index) tid : u32,
) {
    let M = params[0];
    let K = params[1];
    let N = params[2];
    let xi = wg.z;

    let a_off = xi * M * K;   // U[ξ] base
    let b_off = xi * K * N;   // V[ξ] base
    let c_off = xi * M * N;   // M[ξ] base

    let bm = wg.x * BM;
    let bn = wg.y * BN;

    let thread_row = tid / 16u;
    let thread_col = tid % 16u;

    let inner_row_a = tid / BK;
    let inner_col_a = tid % BK;
    let inner_row_b = tid / BN;
    let inner_col_b = tid % BN;

    var acc: array<f32, 16>;
    for (var i = 0u; i < 16u; i = i + 1u) { acc[i] = 0.0; }

    var bk = 0u;
    loop {
        if bk >= K { break; }

        for (var off = 0u; off < BM; off = off + 16u) {
            let row = inner_row_a + off;
            let gA_row = bm + row;
            let gA_col = bk + inner_col_a;
            var val = 0.0;
            if gA_row < M && gA_col < K {
                val = u_in[a_off + gA_row * K + gA_col];
            }
            As[inner_col_a * BM + row] = val;
        }

        for (var off = 0u; off < BK; off = off + 4u) {
            let krow = inner_row_b + off;
            let gB_row = bk + krow;
            let gB_col = bn + inner_col_b;
            var val = 0.0;
            if gB_row < K && gB_col < N {
                val = v_in[b_off + gB_row * N + gB_col];
            }
            Bs[krow * BN + inner_col_b] = val;
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
            acc[0u]  = acc[0u]  + a0 * b0; acc[1u]  = acc[1u]  + a0 * b1;
            acc[2u]  = acc[2u]  + a0 * b2; acc[3u]  = acc[3u]  + a0 * b3;
            acc[4u]  = acc[4u]  + a1 * b0; acc[5u]  = acc[5u]  + a1 * b1;
            acc[6u]  = acc[6u]  + a1 * b2; acc[7u]  = acc[7u]  + a1 * b3;
            acc[8u]  = acc[8u]  + a2 * b0; acc[9u]  = acc[9u]  + a2 * b1;
            acc[10u] = acc[10u] + a2 * b2; acc[11u] = acc[11u] + a2 * b3;
            acc[12u] = acc[12u] + a3 * b0; acc[13u] = acc[13u] + a3 * b1;
            acc[14u] = acc[14u] + a3 * b2; acc[15u] = acc[15u] + a3 * b3;
        }

        workgroupBarrier();
        bk = bk + BK;
    }

    for (var i = 0u; i < 4u; i = i + 1u) {
        let gm = bm + thread_row * 4u + i;
        if gm >= M { continue; }
        for (var j = 0u; j < 4u; j = j + 1u) {
            let gn = bn + thread_col * 4u + j;
            if gn < N {
                m_out[c_off + gm * N + gn] = acc[i * 4u + j];
            }
        }
    }
}
"#;

/// Run the batched 16-GEMM: `u` flat `[16*Cout*Cin]`, `v` flat `[16*Cin*P]`
/// → M flat `[16*Cout*P]`.
#[cfg(test)]
pub(crate) async fn run_batched_gemm(
    ctx: &GpuContext,
    u: &[f32],
    v: &[f32],
    cout: usize,
    cin: usize,
    p: usize,
) -> Result<Vec<f32>> {
    let params = [cout as u32, cin as u32, p as u32];
    let raw = dispatch_compute(
        ctx,
        WINOGRAD_BATCHED_GEMM_WGSL,
        &[bytemuck::cast_slice(u), bytemuck::cast_slice(v)],
        16 * cout * p * 4,
        bytemuck::cast_slice(&params),
        (cout.div_ceil(64) as u32, p.div_ceil(64) as u32, 16),
    )
    .await?;
    Ok(f32_from_bytes(&raw))
}

/// Run the GPU input transform, returning V flat `[16*Cin*P]` (ξ-major).
#[cfg(test)]
pub(crate) async fn run_input_transform(
    ctx: &GpuContext,
    x: &Tensor,
) -> Result<(Vec<f32>, usize, usize, usize)> {
    let cin = x.shape[1];
    let h = x.shape[2];
    let w = x.shape[3];
    let ntw = w.div_ceil(2);
    let nth = h.div_ceil(2);
    let p = ntw * nth;
    let params = [cin as u32, h as u32, w as u32, ntw as u32, nth as u32];
    let total = cin * p;
    let raw = dispatch_compute(
        ctx,
        WINOGRAD_INPUT_TRANSFORM_WGSL,
        &[bytemuck::cast_slice(&x.data)],
        16 * total * 4,
        bytemuck::cast_slice(&params),
        (total.div_ceil(256) as u32, 1, 1),
    )
    .await?;
    Ok((f32_from_bytes(&raw), p, ntw, nth))
}

/// Run the GPU output transform on `m` flat `[16*Cout*P]` → y `[1,Cout,H,W]`.
#[cfg(test)]
pub(crate) async fn run_output_transform(
    ctx: &GpuContext,
    m: &[f32],
    bias: &[f32],
    cout: usize,
    h: usize,
    w: usize,
    use_bias: bool,
) -> Result<Tensor> {
    let ntw = w.div_ceil(2);
    let nth = h.div_ceil(2);
    let p = ntw * nth;
    let params = [
        cout as u32,
        h as u32,
        w as u32,
        ntw as u32,
        nth as u32,
        use_bias as u32,
    ];
    let total = cout * p;
    let raw = dispatch_compute(
        ctx,
        WINOGRAD_OUTPUT_TRANSFORM_WGSL,
        &[bytemuck::cast_slice(m), bytemuck::cast_slice(bias)],
        cout * h * w * 4,
        bytemuck::cast_slice(&params),
        (total.div_ceil(256) as u32, 1, 1),
    )
    .await?;
    Tensor::new(vec![1, cout, h, w], f32_from_bytes(&raw))
}

/// Bᵀ, row-major 4×4.
const BT: [[f32; 4]; 4] = [
    [1.0, 0.0, -1.0, 0.0],
    [0.0, 1.0, 1.0, 0.0],
    [0.0, -1.0, 1.0, 0.0],
    [0.0, 1.0, 0.0, -1.0],
];

/// G, row-major 4×3.
const G: [[f32; 3]; 4] = [
    [1.0, 0.0, 0.0],
    [0.5, 0.5, 0.5],
    [0.5, -0.5, 0.5],
    [0.0, 0.0, 1.0],
];

/// Aᵀ, row-major 2×4.
const AT: [[f32; 4]; 2] = [[1.0, 1.0, 1.0, 0.0], [0.0, 1.0, -1.0, -1.0]];

/// Transform a 3×3 filter `g` (row-major) into the 4×4 Winograd weight `U = G·g·Gᵀ`.
/// Returned row-major 4×4 (16 floats). Computed once per (cout,cin) at prep time.
pub(crate) fn weight_transform_f23(g: &[f32; 9]) -> [f32; 16] {
    // tmp(4×3) = G(4×3) · g(3×3)
    let mut tmp = [[0.0f32; 3]; 4];
    for i in 0..4 {
        for j in 0..3 {
            let mut s = 0.0;
            for k in 0..3 {
                s += G[i][k] * g[k * 3 + j];
            }
            tmp[i][j] = s;
        }
    }
    // U(4×4) = tmp(4×3) · Gᵀ(3×4)  (Gᵀ[k][n] = G[n][k])
    let mut u = [0.0f32; 16];
    for i in 0..4 {
        for n in 0..4 {
            let mut s = 0.0;
            for k in 0..3 {
                s += tmp[i][k] * G[n][k];
            }
            u[i * 4 + n] = s;
        }
    }
    u
}

/// Transform a 4×4 input tile `d` (row-major) into `V = Bᵀ·d·B`. Row-major 4×4.
pub(crate) fn input_transform_f23(d: &[f32; 16]) -> [f32; 16] {
    // tmp(4×4) = Bᵀ · d
    let mut tmp = [0.0f32; 16];
    for i in 0..4 {
        for j in 0..4 {
            let mut s = 0.0;
            for k in 0..4 {
                s += BT[i][k] * d[k * 4 + j];
            }
            tmp[i * 4 + j] = s;
        }
    }
    // V = tmp · B   (B[k][n] = Bᵀ[n][k])
    let mut v = [0.0f32; 16];
    for i in 0..4 {
        for n in 0..4 {
            let mut s = 0.0;
            for k in 0..4 {
                s += tmp[i * 4 + k] * BT[n][k];
            }
            v[i * 4 + n] = s;
        }
    }
    v
}

/// Output transform `Y = Aᵀ·m·A` of a 4×4 transform-space tile `m`. Row-major 2×2.
pub(crate) fn output_transform_f23(m: &[f32; 16]) -> [f32; 4] {
    // tmp(2×4) = Aᵀ(2×4) · m(4×4)
    let mut tmp = [0.0f32; 8];
    for i in 0..2 {
        for j in 0..4 {
            let mut s = 0.0;
            for k in 0..4 {
                s += AT[i][k] * m[k * 4 + j];
            }
            tmp[i * 4 + j] = s;
        }
    }
    // Y(2×2) = tmp(2×4) · A(4×2)   (A[k][n] = Aᵀ[n][k])
    let mut y = [0.0f32; 4];
    for i in 0..2 {
        for n in 0..2 {
            let mut s = 0.0;
            for k in 0..4 {
                s += tmp[i * 4 + k] * AT[n][k];
            }
            y[i * 2 + n] = s;
        }
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic pseudo-random fill in [-1, 1).
    fn fill(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed.wrapping_add(0x9E3779B97F4A7C15);
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    /// Direct 3×3 stride-1 pad-1 multi-channel conv (NCHW), the oracle.
    fn direct_conv(
        x: &[f32],
        w: &[f32],
        bias: &[f32],
        cin: usize,
        cout: usize,
        h: usize,
        wid: usize,
    ) -> Vec<f32> {
        let mut out = vec![0.0f32; cout * h * wid];
        for co in 0..cout {
            for oh in 0..h {
                for ow in 0..wid {
                    let mut acc = bias[co];
                    for ci in 0..cin {
                        for ky in 0..3 {
                            let ih = oh as isize + ky as isize - 1;
                            if ih < 0 || ih as usize >= h {
                                continue;
                            }
                            for kx in 0..3 {
                                let iw = ow as isize + kx as isize - 1;
                                if iw < 0 || iw as usize >= wid {
                                    continue;
                                }
                                let xv = x[ci * h * wid + ih as usize * wid + iw as usize];
                                let wv = w[(co * cin + ci) * 9 + ky * 3 + kx];
                                acc += xv * wv;
                            }
                        }
                    }
                    out[co * h * wid + oh * wid + ow] = acc;
                }
            }
        }
        out
    }

    /// Winograd F(2,3) conv using the transform helpers, same layout as direct_conv.
    fn winograd_conv(
        x: &[f32],
        w: &[f32],
        bias: &[f32],
        cin: usize,
        cout: usize,
        h: usize,
        wid: usize,
    ) -> Vec<f32> {
        // Precompute U[cout][cin][16].
        let mut u_all = vec![0.0f32; cout * cin * 16];
        for co in 0..cout {
            for ci in 0..cin {
                let mut g = [0.0f32; 9];
                g.copy_from_slice(&w[(co * cin + ci) * 9..(co * cin + ci) * 9 + 9]);
                let u = weight_transform_f23(&g);
                u_all[(co * cin + ci) * 16..(co * cin + ci) * 16 + 16].copy_from_slice(&u);
            }
        }
        let nth = h.div_ceil(2);
        let ntw = wid.div_ceil(2);
        let mut out = vec![0.0f32; cout * h * wid];
        for ty in 0..nth {
            for tx in 0..ntw {
                // Top-left output coord of this 2×2 tile; input tile starts at -1 (pad).
                let oh0 = ty * 2;
                let ow0 = tx * 2;
                // Accumulate M = Σ_c U_c ⊙ V_c per cout.
                let mut m = vec![[0.0f32; 16]; cout];
                for ci in 0..cin {
                    // Gather 4×4 input patch (pad with zeros).
                    let mut d = [0.0f32; 16];
                    for py in 0..4 {
                        let ih = oh0 as isize + py as isize - 1;
                        for px in 0..4 {
                            let iw = ow0 as isize + px as isize - 1;
                            if ih >= 0 && (ih as usize) < h && iw >= 0 && (iw as usize) < wid {
                                d[py * 4 + px] = x[ci * h * wid + ih as usize * wid + iw as usize];
                            }
                        }
                    }
                    let v = input_transform_f23(&d);
                    for co in 0..cout {
                        let u = &u_all[(co * cin + ci) * 16..(co * cin + ci) * 16 + 16];
                        for e in 0..16 {
                            m[co][e] += u[e] * v[e];
                        }
                    }
                }
                for co in 0..cout {
                    let y = output_transform_f23(&m[co]);
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let oh = oh0 + dy;
                            let ow = ow0 + dx;
                            if oh < h && ow < wid {
                                out[co * h * wid + oh * wid + ow] = y[dy * 2 + dx] + bias[co];
                            }
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn winograd_gpu_input_transform_matches_cpu() {
        pollster::block_on(async {
            let ctx = match GpuContext::shared().await {
                Ok(c) => c,
                Err(_) => return,
            };
            let (cin, h, w) = (5usize, 10usize, 11usize);
            let x = Tensor::new(vec![1, cin, h, w], fill(cin * h * w, 7)).unwrap();
            let (v, p, ntw, _nth) = run_input_transform(&ctx, &x).await.unwrap();
            // CPU oracle per (ci, tile).
            let mut max_diff = 0.0f32;
            for ci in 0..cin {
                for ty in 0..(h.div_ceil(2)) {
                    for tx in 0..ntw {
                        let tile = ty * ntw + tx;
                        let mut d = [0.0f32; 16];
                        for py in 0..4 {
                            let ih = (2 * ty + py) as isize - 1;
                            for px in 0..4 {
                                let iw = (2 * tx + px) as isize - 1;
                                if ih >= 0 && (ih as usize) < h && iw >= 0 && (iw as usize) < w {
                                    d[py * 4 + px] =
                                        x.data[ci * h * w + ih as usize * w + iw as usize];
                                }
                            }
                        }
                        let vc = input_transform_f23(&d);
                        for e in 0..16 {
                            let got = v[e * cin * p + ci * p + tile];
                            max_diff = max_diff.max((got - vc[e]).abs());
                        }
                    }
                }
            }
            assert!(max_diff < 1e-4, "input transform max diff {max_diff}");
        });
    }

    #[test]
    fn winograd_gpu_output_transform_matches_cpu() {
        pollster::block_on(async {
            let ctx = match GpuContext::shared().await {
                Ok(c) => c,
                Err(_) => return,
            };
            let (cout, h, w) = (4usize, 10usize, 11usize);
            let ntw = w.div_ceil(2);
            let nth = h.div_ceil(2);
            let p = ntw * nth;
            let m = fill(16 * cout * p, 8);
            let bias = fill(cout, 9);
            let y = run_output_transform(&ctx, &m, &bias, cout, h, w, true)
                .await
                .unwrap();
            let mut max_diff = 0.0f32;
            for co in 0..cout {
                for ty in 0..nth {
                    for tx in 0..ntw {
                        let tile = ty * ntw + tx;
                        let mut mm = [0.0f32; 16];
                        for e in 0..16 {
                            mm[e] = m[e * cout * p + co * p + tile];
                        }
                        let yc = output_transform_f23(&mm);
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let oh = 2 * ty + dy;
                                let ow = 2 * tx + dx;
                                if oh < h && ow < w {
                                    let got = y.data[co * h * w + oh * w + ow];
                                    let want = yc[dy * 2 + dx] + bias[co];
                                    max_diff = max_diff.max((got - want).abs());
                                }
                            }
                        }
                    }
                }
            }
            assert!(max_diff < 1e-4, "output transform max diff {max_diff}");
        });
    }

    #[test]
    fn winograd_gpu_end_to_end_matches_direct_conv() {
        pollster::block_on(async {
            let ctx = match GpuContext::shared().await {
                Ok(c) => c,
                Err(_) => return,
            };
            // Channel counts ≥64 so the 64×64 GEMM tiles are exercised.
            let (cin, cout, h, w) = (96usize, 128usize, 28usize, 31usize);
            let x = Tensor::new(vec![1, cin, h, w], fill(cin * h * w, 11)).unwrap();
            let wt = fill(cout * cin * 9, 12);
            let bias = fill(cout, 13);

            // Prep-time weight transform: U[ξ][co][ci] = weight_transform(g_co_ci)[ξ].
            let mut u = vec![0.0f32; 16 * cout * cin];
            for co in 0..cout {
                for ci in 0..cin {
                    let mut g = [0.0f32; 9];
                    g.copy_from_slice(&wt[(co * cin + ci) * 9..(co * cin + ci) * 9 + 9]);
                    let ut = weight_transform_f23(&g);
                    for e in 0..16 {
                        u[e * cout * cin + co * cin + ci] = ut[e];
                    }
                }
            }

            // 3-pass GPU Winograd.
            let (v, p, _ntw, _nth) = run_input_transform(&ctx, &x).await.unwrap();
            let m = run_batched_gemm(&ctx, &u, &v, cout, cin, p).await.unwrap();
            let y = run_output_transform(&ctx, &m, &bias, cout, h, w, true)
                .await
                .unwrap();

            // Direct-conv oracle.
            let direct = direct_conv(&x.data, &wt, &bias, cin, cout, h, w);
            let max_diff = y
                .data
                .iter()
                .zip(&direct)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                max_diff < 2e-3,
                "GPU Winograd 3-pass vs direct conv max abs diff {max_diff} too large"
            );
        });
    }

    #[test]
    fn winograd_f23_matches_direct_conv() {
        let (cin, cout, h, w) = (6usize, 8usize, 10usize, 11usize);
        let x = fill(cin * h * w, 1);
        let wt = fill(cout * cin * 9, 2);
        let bias = fill(cout, 3);
        let direct = direct_conv(&x, &wt, &bias, cin, cout, h, w);
        let wino = winograd_conv(&x, &wt, &bias, cin, cout, h, w);
        let max_diff = direct
            .iter()
            .zip(&wino)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_diff < 1e-4,
            "Winograd F(2,3) vs direct conv max abs diff {max_diff} too large"
        );
    }
}
