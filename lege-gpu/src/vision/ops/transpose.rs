// params layout (16 x u32):
//   [0] num_elems  [1] rank  [2] _pad  [3] _pad
//   [4..9]  out_strides[0..5]
//   [10..15] in_perm_strides[0..5]  (input_stride[perm[d]])
pub(crate) const TRANSPOSE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let rank = params[1];

    var remaining = i;
    var in_idx : u32 = 0u;
    for (var d: u32 = 0u; d < rank; d = d + 1u) {
        let s     = params[4u + d];
        let coord = remaining / s;
        remaining  = remaining % s;
        in_idx = in_idx + coord * params[10u + d];
    }
    out[i] = inp[in_idx];
}
"#;
