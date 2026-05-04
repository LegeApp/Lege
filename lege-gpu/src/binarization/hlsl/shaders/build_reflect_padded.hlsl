struct PadParams {
    uint width;
    uint height;
    uint padded_width;
    uint padded_height;
    uint radius;
    uint _pad0;
    uint _pad1;
    uint _pad2;
};

cbuffer PadConstants : register(b0) {
    PadParams params;
};

StructuredBuffer<uint> grayBuffer       : register(t0);
RWStructuredBuffer<uint> paddedGray     : register(u0);
RWStructuredBuffer<uint> paddedGraySq   : register(u1);

int reflect_101(int idx, int len) {
    if (idx < 0) return -idx - 1;
    if (idx >= len) return 2 * len - idx - 1;
    return idx;
}

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint px = id.x;
    uint py = id.y;

    if (px >= params.padded_width || py >= params.padded_height) {
        return;
    }

    int sx = reflect_101((int)px - (int)params.radius, (int)params.width);
    int sy = reflect_101((int)py - (int)params.radius, (int)params.height);

    uint src_idx = (uint)sy * params.width + (uint)sx;
    uint g = grayBuffer[src_idx] & 255u;
    uint out_idx = py * params.padded_width + px;

    paddedGray[out_idx] = g;
    paddedGraySq[out_idx] = g * g;
}
