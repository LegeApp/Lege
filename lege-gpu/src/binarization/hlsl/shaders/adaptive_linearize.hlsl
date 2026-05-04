// sRGB-aware RGB→gray pre-conversion.
// Mirror of adaptive_linearize.wgsl on the WGPU backend.
//
// Reads RGBA-packed source (1 pixel per uint: R | G<<8 | B<<16 | A<<24),
// applies a 256-entry sRGB→linear LUT, computes BT.709 linear luma, re-encodes
// to sRGB-perceptual gray, and writes 1-uint-per-pixel gray (0..255) to gray_dst.
//
// Running this once before the binarization passes keeps the bg_max separable
// max filter cheap: each output pixel reads bg_window pre-converted gray values.

cbuffer BinarizeConstants : register(b0) {
    uint width;
    uint height;
    uint mode;
    uint invert_output;
    uint fixed_threshold;
    uint sauvola_window;
    uint bg_window;
    uint otsu_threshold;
    float k_factor;
    float percentile_c;
    uint padded_width;
    uint padded_height;
    uint integral_width;
    uint sauvola_radius;
    uint debug_mode;
    uint _pad0;
    uint _pad2;
    uint _pad3;
    uint _pad4;
    uint _pad5;
    uint _pad6;
    uint _pad7;
};

StructuredBuffer<uint>   rgba_src : register(t0);   // 1 pixel/uint: R | G<<8 | B<<16 | A<<24
StructuredBuffer<float>  srgb_lut : register(t1);   // 256 entries: sRGB -> linear curve
RWStructuredBuffer<uint> gray_dst : register(u0);   // 1 uint per pixel (0..255)

float linear_to_srgb(float c) {
    return (c <= 0.0031308f) ? (12.92f * c) : (1.055f * pow(c, 1.0f / 2.4f) - 0.055f);
}

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= width || id.y >= height) return;
    uint idx = id.y * width + id.x;

    uint w = rgba_src[idx];
    uint r_u = (w >>  0) & 0xFFu;
    uint g_u = (w >>  8) & 0xFFu;
    uint b_u = (w >> 16) & 0xFFu;

    float r_lin = srgb_lut[r_u];
    float g_lin = srgb_lut[g_u];
    float b_lin = srgb_lut[b_u];

    // BT.709 linear luma -> re-encode to sRGB-perceptual gray.
    float y_lin  = 0.2126f * r_lin + 0.7152f * g_lin + 0.0722f * b_lin;
    float y_srgb = linear_to_srgb(y_lin);

    gray_dst[idx] = (uint)clamp(y_srgb * 255.0f + 0.5f, 0.0f, 255.0f);
}
