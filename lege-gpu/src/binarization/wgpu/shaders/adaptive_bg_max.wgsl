// Separable max-filter for background estimation.
// Two entry points share the same bind group layout so a single BindGroupLayout works for both.
//   main_h: horizontal pass  (reads u32-per-pixel gray_buf, writes u32-per-pixel bg_tmp)
//   main_v: vertical pass    (reads u32-per-pixel bg_tmp,   writes u32-per-pixel bg)
// Both inputs are 1 u32 per pixel (gray, 0..255) — gray_buf is produced by the
// linearize pass; bg_tmp by main_h.

struct BinarizeParams {
    width: u32, height: u32, mode: u32, invert_output: u32,
    fixed_threshold: u32, sauvola_window: u32, bg_window: u32, otsu_threshold: u32,
    k_factor: f32, percentile_c: f32, padded_width: u32, padded_height: u32,
    integral_width: u32, sauvola_radius: u32, debug_mode: u32, _pad0: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read>       bg_input:  array<u32>;
@group(0) @binding(2) var<storage, read_write> bg_output: array<u32>;

fn reflect_101(idx: i32, len: i32) -> i32 {
    if len <= 1 { return 0; }
    let period = 2 * len;
    var n = idx % period;
    if n < 0 { n = n + period; }
    if n >= len { return period - n - 1; }
    return n;
}

// Both inputs are 1 u32 per pixel (gray, 0..255) — gray_buf for main_h, bg_tmp for main_v.
fn read_gray(idx: u32) -> u32 {
    return bg_input[idx] & 255u;
}

@compute @workgroup_size(16, 16, 1)
fn main_h(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    if x >= params.width || y >= params.height { return; }

    let r: i32 = i32(params.bg_window / 2u);
    var mx: u32 = 0u;
    for (var dx: i32 = -r; dx <= r; dx = dx + 1) {
        let sx = u32(reflect_101(i32(x) + dx, i32(params.width)));
        let src_idx = y * params.width + sx;
        let v = read_gray(src_idx);
        mx = max(mx, v);
    }
    bg_output[y * params.width + x] = mx;
}

@compute @workgroup_size(16, 16, 1)
fn main_v(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    if x >= params.width || y >= params.height { return; }

    let r: i32 = i32(params.bg_window / 2u);
    var mx: u32 = 0u;
    for (var dy: i32 = -r; dy <= r; dy = dy + 1) {
        let sy = u32(reflect_101(i32(y) + dy, i32(params.height)));
        let v = read_gray(sy * params.width + x);
        mx = max(mx, v);
    }
    bg_output[y * params.width + x] = mx;
}
