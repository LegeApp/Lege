// sRGB-aware RGB→gray pre-conversion pass.
// Reads RGBA-packed source (1 pixel per u32: R | G<<8 | B<<16 | A<<24, A unused),
// applies a 256-entry sRGB→linear LUT, computes BT.709 linear-luma, re-encodes
// to sRGB, and writes 1-u32-per-pixel gray (0..255) to gray_dst.
//
// Running this once before the binarization passes keeps the bg_max separable
// max filter cheap: each output pixel reads bg_window pre-converted gray
// values, not bg_window RGB triples requiring LUT + pow() per read.
//
// To switch to "min-of-channel" gray (better for colored-ink documents like
// faded red stamps or blue carbon copies), replace the luma line with:
//   let y_srgb = min(min(r_lin, g_lin), b_lin);
// and skip linear_to_srgb (since we'd want the darkest channel directly).

struct BinarizeParams {
    width: u32, height: u32, mode: u32, invert_output: u32,
    fixed_threshold: u32, sauvola_window: u32, bg_window: u32, otsu_threshold: u32,
    k_factor: f32, percentile_c: f32, padded_width: u32, padded_height: u32,
    integral_width: u32, sauvola_radius: u32, debug_mode: u32, _pad0: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read> rgba_src: array<u32>;
@group(0) @binding(2) var<storage, read_write> gray_dst: array<u32>;
@group(0) @binding(3) var<storage, read> srgb_lut: array<f32>;

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 { return 12.92 * c; }
    return 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    if x >= params.width || y >= params.height { return; }
    let idx = y * params.width + x;

    let w = rgba_src[idx];
    let r_u = w & 0xFFu;
    let g_u = (w >> 8u) & 0xFFu;
    let b_u = (w >> 16u) & 0xFFu;

    let r_lin = srgb_lut[r_u];
    let g_lin = srgb_lut[g_u];
    let b_lin = srgb_lut[b_u];

    // BT.709 linear luma → re-encode to sRGB-perceptual gray.
    let y_lin = 0.2126 * r_lin + 0.7152 * g_lin + 0.0722 * b_lin;
    let y_srgb = linear_to_srgb(y_lin);

    gray_dst[idx] = u32(clamp(y_srgb * 255.0 + 0.5, 0.0, 255.0));
}
