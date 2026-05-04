// Row-wise prefix sums over the padded image.
// One invocation per padded row; each loops across padded_width.
// Output layout: row_prefix[y * integral_width + (x+1)] = sum of padded[y][0..x].
// Column 0 is always 0 (sentinel).

struct BinarizeParams {
    width: u32, height: u32, mode: u32, invert_output: u32,
    fixed_threshold: u32, sauvola_window: u32, bg_window: u32, otsu_threshold: u32,
    k_factor: f32, percentile_c: f32, padded_width: u32, padded_height: u32,
    integral_width: u32, sauvola_radius: u32, debug_mode: u32, _pad0: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read> padded_gray_in: array<u32>;
@group(0) @binding(2) var<storage, read> padded_sq_in: array<f32>;
@group(0) @binding(3) var<storage, read_write> row_prefix: array<u32>;
@group(0) @binding(4) var<storage, read_write> row_prefix_sq: array<f32>;

@compute @workgroup_size(1, 64, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let y = id.y;
    if y >= params.padded_height { return; }

    let row_base = y * params.integral_width;
    row_prefix[row_base] = 0u;
    row_prefix_sq[row_base] = 0.0;

    var running_u: u32 = 0u;
    var running_sq: f32 = 0.0;
    for (var x: u32 = 0u; x < params.padded_width; x = x + 1u) {
        let src_idx = y * params.padded_width + x;
        running_u = running_u + (padded_gray_in[src_idx] & 255u);
        running_sq = running_sq + padded_sq_in[src_idx];
        row_prefix[row_base + x + 1u] = running_u;
        row_prefix_sq[row_base + x + 1u] = running_sq;
    }
}
