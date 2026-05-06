struct BinarizeParams {
    width: u32, height: u32, mode: u32, invert_output: u32,
    fixed_threshold: u32, sauvola_window: u32, bg_window: u32, otsu_threshold: u32,
    k_factor: f32, percentile_c: f32, padded_width: u32, padded_height: u32,
    integral_width: u32, sauvola_radius: u32, debug_mode: u32, _pad0: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read> gray_src: array<u32>; // 1 u32 per pixel (sRGB gray from linearize pass)
@group(0) @binding(2) var<storage, read_write> padded_gray: array<u32>;
@group(0) @binding(3) var<storage, read_write> padded_sq: array<f32>;

fn reflect_101(idx: i32, len: i32) -> i32 {
    if len <= 1 { return 0; }
    let period = 2 * len;
    var n = idx % period;
    if n < 0 { n = n + period; }
    if n >= len { return period - n - 1; }
    return n;
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let px = id.x;
    let py = id.y;
    if px >= params.padded_width || py >= params.padded_height { return; }

    let r = i32(params.sauvola_radius);
    let sx = u32(reflect_101(i32(px) - r, i32(params.width)));
    let sy = u32(reflect_101(i32(py) - r, i32(params.height)));

    let src_idx = sy * params.width + sx;
    let g = gray_src[src_idx] & 255u;
    let out_idx = py * params.padded_width + px;
    padded_gray[out_idx] = g;
    padded_sq[out_idx] = f32(g) * f32(g);
}
