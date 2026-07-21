// params: [0] row_count [1] dim [2] inner
pub(crate) const SOFTMAX_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let row_index = gid.x;
    if row_index >= params[0] { return; }

    let dim = params[1];
    let inner = params[2];
    let outer_index = row_index / inner;
    let inner_index = row_index % inner;
    let base = outer_index * dim * inner + inner_index;

    var max_value = -3.4028234663852886e38f;
    for (var d: u32 = 0u; d < dim; d = d + 1u) {
        max_value = max(max_value, inp[base + d * inner]);
    }

    var sum = 0.0f;
    for (var d: u32 = 0u; d < dim; d = d + 1u) {
        let value = exp(inp[base + d * inner] - max_value);
        out[base + d * inner] = value;
        sum = sum + value;
    }
    for (var d: u32 = 0u; d < dim; d = d + 1u) {
        out[base + d * inner] = out[base + d * inner] / sum;
    }
}
"#;
