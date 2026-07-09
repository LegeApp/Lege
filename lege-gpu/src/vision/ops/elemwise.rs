

// params layout (22 x u32):
//   [0] num_elems  [1] rank  [2] op(0=add,1=mul,2=sub,3=div)  [3] _pad
//   [4..9]  out_strides[0..5]
//   [10..15] in0_broadcast_strides[0..5]
//   [16..21] in1_broadcast_strides[0..5]
pub(crate) const ELEMENTWISE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       in0    : array<f32>;
@group(0) @binding(1) var<storage, read>       in1    : array<f32>;
@group(0) @binding(2) var<storage, read_write> out    : array<f32>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i        = gid.y * ng.x * 256u + gid.x;
    let n        = params[0];
    if i >= n { return; }
    let rank     = params[1];
    let op       = params[2];

    var remaining = i;
    var a_idx : u32 = 0u;
    var b_idx : u32 = 0u;
    for (var d: u32 = 0u; d < rank; d = d + 1u) {
        let s    = params[4u + d];
        let c    = remaining / s;
        remaining = remaining % s;
        a_idx = a_idx + c * params[10u + d];
        b_idx = b_idx + c * params[16u + d];
    }

    let a = in0[a_idx];
    let b = in1[b_idx];
    switch op {
        case 0u:      { out[i] = a + b; }
        case 1u:      { out[i] = a * b; }
        case 2u:      { out[i] = a - b; }
        case 3u:      { out[i] = a / b; }
        default:      { out[i] = max(a, b); }
    }
}
"#;

// Vec4 same-shape variant: both inputs have identical shape as output, no broadcasting.
// params[0] = num_elems/4, params[1] = op_code.  Caller ensures num_elems % 4 == 0.
pub(crate) const ELEMENTWISE_SAME_VEC4_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       in0    : array<vec4<f32>>;
@group(0) @binding(1) var<storage, read>       in1    : array<vec4<f32>>;
@group(0) @binding(2) var<storage, read_write> out    : array<vec4<f32>>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i  = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let op = params[1];
    let a  = in0[i];
    let b  = in1[i];
    switch op {
        case 0u:  { out[i] = a + b; }
        case 1u:  { out[i] = a * b; }
        case 2u:  { out[i] = a - b; }
        case 3u:  { out[i] = a / b; }
        default:  { out[i] = max(a, b); }
    }
}
"#;

