// Pack the u32-per-pixel binarize output (each word holds 0 or 255 in its LSB)
// into a byte-per-pixel packed buffer (4 pixels per output u32). Mirrors the
// WGPU `adaptive_pack_output.wgsl` shader so the HLSL backend can do the same
// zero-copy readback path.

struct BinarizeParams {
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

    uint _pad0;
    uint _pad1;
};

cbuffer BinarizeConstants : register(b0) {
    BinarizeParams params;
};

StructuredBuffer<uint> srcBuffer : register(t0);     // one u32 per pixel, value 0 or 255
RWStructuredBuffer<uint> dstBuffer : register(u0);   // 4 pixels packed per u32

[numthreads(256, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint pixel_count = params.width * params.height;
    uint base = id.x * 4u;
    if (base >= pixel_count) {
        return;
    }
    uint packed = 0u;
    [unroll]
    for (uint i = 0u; i < 4u; i++) {
        uint pi = base + i;
        if (pi < pixel_count) {
            packed |= (srcBuffer[pi] & 255u) << (i * 8u);
        }
    }
    dstBuffer[id.x] = packed;
}
