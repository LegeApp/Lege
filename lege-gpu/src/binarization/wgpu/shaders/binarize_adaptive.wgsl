// Fixed-threshold single-pass shader (mode == 0).
// Source is packed u32: 4 pixels per word, little-endian bytes.
// Destination is u32-per-pixel (0 or 255); packed down by adaptive_pack_output.wgsl.

struct BinarizeParams {
    width: u32,
    height: u32,
    mode: u32,
    invert_output: u32,
    fixed_threshold: u32,
    sauvola_window: u32,
    bg_window: u32,
    otsu_threshold: u32,
    k_factor: f32,
    percentile_c: f32,
    padded_width: u32,
    padded_height: u32,
    integral_width: u32,
    sauvola_radius: u32,
    debug_mode: u32,
    _pad2: u32,
    _pad3: u32,
    _pad4: u32,
    _pad5: u32,
    _pad6: u32,
    _pad7: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read>       src_pixels: array<u32>; // 1 u32 per pixel (sRGB gray from linearize pass)
@group(0) @binding(2) var<storage, read_write> dst_pixels: array<u32>; // 1 u32 per pixel (0 or 255)

fn load_pixel(idx: u32) -> u32 {
    return src_pixels[idx] & 255u;
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    if (x >= params.width || y >= params.height) {
        return;
    }

    let idx = y * params.width + x;
    let g_u = load_pixel(idx);

    var out: u32;
    if (params.invert_output != 0u) {
        out = select(255u, 0u, g_u > params.fixed_threshold);
    } else {
        out = select(0u, 255u, g_u > params.fixed_threshold);
    }

    dst_pixels[idx] = out;
}
