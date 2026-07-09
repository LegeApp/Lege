//! Structural kernels used by the sauvola binarization model: single-axis
//! inclusive CumSum (integral images), single-axis ReduceSum, and the
//! SpaceToDepth/DepthToSpace block shuffles (DCR mode, the ONNX default).
//!
//! All four treat tensors as flat row-major buffers and gather per element, so
//! they are rank- and layout-agnostic; correctness, not speed, is the goal
//! (the production ORT path is 1-3 s/page).



// One thread per (outer, inner) line; each scans the axis sequentially.
// params: [num_lines = outer*inner, axis_dim, inner].
pub(crate) const CUMSUM_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let t = gid.y * ng.x * 256u + gid.x;
    if t >= params[0] { return; }
    let axis_dim = params[1];
    let inner    = params[2];
    let o = t / inner;
    let i = t % inner;
    var acc = 0.0;
    for (var k: u32 = 0u; k < axis_dim; k = k + 1u) {
        let idx = (o * axis_dim + k) * inner + i;
        acc = acc + inp[idx];
        out[idx] = acc;
    }
}
"#;

// One thread per output element; sums the reduced axis.
// params: [num_out = outer*inner, axis_dim, inner].
pub(crate) const REDUCESUM_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let t = gid.y * ng.x * 256u + gid.x;
    if t >= params[0] { return; }
    let axis_dim = params[1];
    let inner    = params[2];
    let o = t / inner;
    let i = t % inner;
    var acc = 0.0;
    for (var k: u32 = 0u; k < axis_dim; k = k + 1u) {
        acc = acc + inp[(o * axis_dim + k) * inner + i];
    }
    out[t] = acc;
}
"#;

// SpaceToDepth (DCR): [N,C,H,W] -> [N,C*b*b,H/b,W/b].
// out channel oc decodes to (bh, bw, c); input pulls from (oh*b+bh, ow*b+bw).
// params: [num_out, C, H, W, b].
pub(crate) const SPACE_TO_DEPTH_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let t = gid.y * ng.x * 256u + gid.x;
    if t >= params[0] { return; }
    let c_in = params[1];
    let h_in = params[2];
    let w_in = params[3];
    let b    = params[4];
    let oc_n = c_in * b * b;
    let oh_n = h_in / b;
    let ow_n = w_in / b;

    var r = t;
    let ow = r % ow_n; r = r / ow_n;
    let oh = r % oh_n; r = r / oh_n;
    let oc = r % oc_n;
    let n  = r / oc_n;

    let bh = oc / (b * c_in);
    let rem = oc % (b * c_in);
    let bw = rem / c_in;
    let c  = rem % c_in;

    let ih = oh * b + bh;
    let iw = ow * b + bw;
    out[t] = inp[((n * c_in + c) * h_in + ih) * w_in + iw];
}
"#;

// DepthToSpace (DCR): [N,C,H,W] -> [N,C/(b*b),H*b,W*b].
// input channel ic = b0*b*Cout + b1*Cout + oc for output spatial parity (b0,b1).
// params: [num_out, C, H, W, b].
pub(crate) const DEPTH_TO_SPACE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let t = gid.y * ng.x * 256u + gid.x;
    if t >= params[0] { return; }
    let c_in = params[1];
    let h_in = params[2];
    let w_in = params[3];
    let b    = params[4];
    let oc_n = c_in / (b * b);
    let oh_n = h_in * b;
    let ow_n = w_in * b;

    var r = t;
    let ow = r % ow_n; r = r / ow_n;
    let oh = r % oh_n; r = r / oh_n;
    let oc = r % oc_n;
    let n  = r / oc_n;

    let h  = oh / b;
    let b0 = oh % b;
    let w  = ow / b;
    let b1 = ow % b;
    let ic = b0 * b * oc_n + b1 * oc_n + oc;
    out[t] = inp[((n * c_in + ic) * h_in + h) * w_in + w];
}
"#;

