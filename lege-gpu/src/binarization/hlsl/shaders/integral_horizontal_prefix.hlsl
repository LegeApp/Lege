struct IntegralParams {
    uint padded_width;
    uint padded_height;
    uint integral_width;
    uint _pad0;
};

cbuffer IntegralConstants : register(b0) {
    IntegralParams params;
};

StructuredBuffer<uint> src             : register(t0);
RWStructuredBuffer<uint> rowPrefix     : register(u0);

[numthreads(1, 64, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint y = id.y;
    if (y >= params.padded_height) {
        return;
    }

    uint integral_y = y + 1u;
    uint row_base = integral_y * params.integral_width;

    rowPrefix[row_base] = 0u;

    uint running = 0u;
    for (uint x = 0u; x < params.padded_width; x++) {
        running += src[y * params.padded_width + x];
        rowPrefix[row_base + x + 1u] = running;
    }
}
