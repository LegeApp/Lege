struct BgParams {
    uint width;
    uint height;
    uint bg_window;
    uint bg_radius;
};

cbuffer BgConstants : register(b0) {
    BgParams params;
};

StructuredBuffer<uint> bgTmp       : register(t0);
RWStructuredBuffer<uint> bgBuffer  : register(u0);

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

    for (int dy = -(int)params.bg_radius; dy <= (int)params.bg_radius; dy++) {
        int sy = reflect_101((int)y + dy, (int)params.height);
        uint v = bgTmp[(uint)sy * params.width + x] & 255u;
        m = max(m, v);
    }

    bgBuffer[y * params.width + x] = m;
}
