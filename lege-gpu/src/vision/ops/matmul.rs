

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

