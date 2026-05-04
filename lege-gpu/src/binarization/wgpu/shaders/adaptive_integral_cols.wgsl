// Column-wise accumulation to complete the 2-D integral image.
// One invocation per integral column; each loops down padded_height rows.
// After this pass: integral[(y+1)*iw + x] = sum of padded[0..y][0..x-1].
// Row 0 is zeroed as the sentinel row.

struct BinarizeParams {
    width: u32, height: u32, mode: u32, invert_output: u32,
    fixed_threshold: u32, sauvola_window: u32, bg_window: u32, otsu_threshold: u32,
    k_factor: f32, percentile_c: f32, padded_width: u32, padded_height: u32,
    integral_width: u32, sauvola_radius: u32, debug_mode: u32, _pad0: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read> row_prefix_in: array<u32>;
@group(0) @binding(2) var<storage, read> row_prefix_sq_in: array<f32>;
@group(0) @binding(3) var<storage, read_write> integral_out: array<u32>;
@group(0) @binding(4) var<storage, read_write> integral_sq_out: array<f32>;

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    if x >= params.integral_width { return; }

    // Zero the sentinel row 0 for this column.
    integral_out[x] = 0u;
    integral_sq_out[x] = 0.0;

    var running_u: u32 = 0u;
    var running_sq: f32 = 0.0;
    for (var y: u32 = 0u; y < params.padded_height; y = y + 1u) {
        let src_idx = y * params.integral_width + x;
        running_u = running_u + row_prefix_in[src_idx];
        running_sq = running_sq + row_prefix_sq_in[src_idx];
        integral_out[(y + 1u) * params.integral_width + x] = running_u;
        integral_sq_out[(y + 1u) * params.integral_width + x] = running_sq;
    }
}
