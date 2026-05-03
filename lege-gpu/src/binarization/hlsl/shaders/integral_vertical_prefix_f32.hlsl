struct IntegralParams {
    uint padded_width;
    uint padded_height;
    uint integral_width;
    uint _pad0;
};

cbuffer IntegralConstants : register(b0) {
    IntegralParams params;
};

StructuredBuffer<float> rowPrefix      : register(t0);
RWStructuredBuffer<float> integralOut  : register(u0);

[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;
    if (x >= params.integral_width) {
        return;
    }

    // Required zero border row for summed-area lookup.
    integralOut[x] = 0.0f;

    float running = 0.0f;
    for (uint y = 0u; y < params.padded_height; y++) {
        uint src_idx = (y + 1u) * params.integral_width + x;
        running += rowPrefix[src_idx];
        integralOut[(y + 1u) * params.integral_width + x] = running;
    }
}
