// params (28 x u32):
//   [0] num_elems  [1] rank  [2..3] _pad
//   [4..9]  out_strides[0..5]
//   [10..15] in_start[0..5]
//   [16..21] in_step[0..5]
//   [22..27] in_strides[0..5]
pub(crate) const SLICE_WGSL: &str = r#"
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
    var in_idx: u32 = 0u;
    for (var d: u32 = 0u; d < rank; d = d + 1u) {
        let out_stride = params[4u + d];
        let coord      = remaining / out_stride;
        remaining       = remaining % out_stride;
        let in_coord   = params[10u + d] + coord * params[16u + d];
        in_idx = in_idx + in_coord * params[22u + d];
    }
    out[i] = inp[in_idx];
}
"#;
