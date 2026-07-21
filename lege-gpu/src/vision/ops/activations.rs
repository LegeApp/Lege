//! Pointwise activation kernels: Relu, HardSwish, HardSigmoid.
//! Same binding layout as `sigmoid.rs`: one input, one output, a params buffer.

pub(crate) const RELU_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    out[i] = max(0.0, inp[i]);
}
"#;

// HardSwish(x) = x * clamp(x / 6 + 0.5, 0, 1)  (ONNX fixed alpha=1/6, beta=0.5).
pub(crate) const HARDSWISH_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let x = inp[i];
    out[i] = x * clamp(x / 6.0 + 0.5, 0.0, 1.0);
}
"#;

// HardSigmoid(x) = clamp(alpha*x + beta, 0, 1).
// params[0] = len, params[1] = bitcast(alpha), params[2] = bitcast(beta).
pub(crate) const HARDSIGMOID_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let alpha = bitcast<f32>(params[1]);
    let beta  = bitcast<f32>(params[2]);
    out[i] = clamp(alpha * inp[i] + beta, 0.0, 1.0);
}
"#;

pub(crate) const SQRT_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    out[i] = sqrt(inp[i]);
}
"#;

// Pow(x, e) for a constant scalar exponent. WGSL `pow` is undefined for x < 0,
// so we reconstruct the sign for integer exponents (matches Rust's powf, which
// the reference uses). params[1] = bitcast(exponent).
pub(crate) const POW_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let x = inp[i];
    let e = bitcast<f32>(params[1]);
    if x >= 0.0 {
        out[i] = pow(x, e);
    } else if e == round(e) {
        // Integer exponent: |x|^e with sign from odd/even parity.
        let mag = pow(-x, e);
        let odd = (e - 2.0 * floor(e * 0.5)) != 0.0;
        out[i] = select(mag, -mag, odd);
    } else {
        out[i] = pow(x, e);
    }
}
"#;
