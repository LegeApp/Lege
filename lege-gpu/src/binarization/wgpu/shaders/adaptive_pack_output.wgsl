// Pack u32-per-pixel output (values 0 or 255) into byte-per-pixel packed buffer.
// Each thread processes 4 consecutive pixels → 1 output u32 (little-endian bytes).
// Reuses the existing BinarizeParams uniform so no separate uniform buffer is needed.

struct BinarizeParams {
    width: u32, height: u32, mode: u32, invert_output: u32,
    fixed_threshold: u32, sauvola_window: u32, bg_window: u32, otsu_threshold: u32,
    k_factor: f32, percentile_c: f32, padded_width: u32, padded_height: u32,
    integral_width: u32, sauvola_radius: u32, debug_mode: u32, _pad0: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read> src: array<u32>;        // one u32 per pixel, value 0 or 255
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;  // 4 pixels packed per u32

@compute @workgroup_size(256, 1, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let pixel_count = params.width * params.height;
    let base = id.x * 4u;
    if base >= pixel_count { return; }
    var packed = 0u;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let pi = base + i;
        if pi < pixel_count {
            packed |= (src[pi] & 255u) << (i * 8u);
        }
    }
    dst[id.x] = packed;
}
