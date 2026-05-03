struct IntegralParams {
    uint padded_width;
    uint padded_height;
    uint integral_width;
    uint _pad0;
};

cbuffer IntegralConstants : register(b0) {
    IntegralParams params;
};

StructuredBuffer<uint> rowPrefix      : register(t0);
RWStructuredBuffer<uint> integralOut  : register(u0);

[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;

    if (x >= params.integral_width) {
        return;
    }

    integralOut[x] = 0u;

    uint running = 0u;
    for (uint y = 0u; y < params.padded_height; y++) {
        uint src_idx = (y + 1u) * params.integral_width + x;
        running += rowPrefix[src_idx];
        integralOut[(y + 1u) * params.integral_width + x] = running;
    }
}
