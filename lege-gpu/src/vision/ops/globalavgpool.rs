//! GlobalAveragePool: mean over all spatial dims per (batch, channel).
//! NCHW input [N, C, d0, d1, ...] -> [N, C, 1, 1, ...]. One invocation per (n, c)
//! averages the contiguous spatial plane that follows it in row-major layout.



// params[0] = num_planes (N*C), params[1] = plane size (product of spatial dims).
pub(crate) const GLOBALAVGPOOL_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }
    let plane = params[1];
    let base = i * plane;
    var acc = 0.0;
    for (var p: u32 = 0u; p < plane; p = p + 1u) {
        acc = acc + inp[base + p];
    }
    out[i] = acc / f32(plane);
}
"#;

