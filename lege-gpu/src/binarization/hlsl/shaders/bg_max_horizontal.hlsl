struct BgParams {
    uint width;
    uint height;
    uint bg_window;
    uint bg_radius;
};

cbuffer BgConstants : register(b0) {
    BgParams params;
};

StructuredBuffer<uint> grayBuffer   : register(t0);
RWStructuredBuffer<uint> bgTmp      : register(u0);

int reflect_101(int idx, int len) {
    if (idx < 0) return -idx - 1;
    if (idx >= len) return 2 * len - idx - 1;
    return idx;
}

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;
    uint y = id.y;

    if (x >= params.width || y >= params.height) {
        return;
    }

    uint m = 0u;

    for (int dx = -(int)params.bg_radius; dx <= (int)params.bg_radius; dx++) {
        int sx = reflect_101((int)x + dx, (int)params.width);
        uint v = grayBuffer[y * params.width + (uint)sx] & 255u;
        m = max(m, v);
    }

    bgTmp[y * params.width + x] = m;
}
