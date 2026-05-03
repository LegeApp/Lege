// Correctness-first DirectX compute shader for Lege light binarization:
// fixed threshold or adaptive Sauvola + background-normalized Otsu + AND fusion.

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

StructuredBuffer<uint> grayBuffer : register(t0);
RWStructuredBuffer<uint> dstBuffer : register(u0);

int reflect_101(int idx, int len) {
    if (idx < 0) {
        return -idx - 1;
    }
    if (idx >= len) {
        return 2 * len - idx - 1;
    }
    return idx;
}

uint load_gray_reflect(int x, int y) {
    int rx = reflect_101(x, (int)params.width);
    int ry = reflect_101(y, (int)params.height);
    uint idx = (uint)ry * params.width + (uint)rx;
    return grayBuffer[idx] & 255u;
}

uint apply_output_invert(uint v) {
    if (params.invert_output != 0u) {
        return 255u - v;
    }
    return v;
}

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;
    uint y = id.y;

    if (x >= params.width || y >= params.height) {
        return;
    }

    uint idx = y * params.width + x;
    uint g_u = grayBuffer[idx] & 255u;

    if (params.mode == 0u) {
        uint out_fixed = (g_u > params.fixed_threshold) ? 255u : 0u;
        dstBuffer[idx] = apply_output_invert(out_fixed);
        return;
    }

    float g = (float)g_u;

    uint win = params.sauvola_window;
    int r = (int)(win / 2u);

    float sum = 0.0f;
    float sum_sq = 0.0f;

    [loop]
    for (int dy = -r; dy <= r; dy++) {
        [loop]
        for (int dx = -r; dx <= r; dx++) {
            float v = (float)load_gray_reflect((int)x + dx, (int)y + dy);
            sum += v;
            sum_sq += v * v;
        }
    }

    float area = (float)(win * win);
    float mean = sum / area;
    float var = (sum_sq / area) - mean * mean;
    float stddev = sqrt(max(var, 0.0f));

    float sauvola_thr = mean * (1.0f + params.k_factor * (stddev / 127.0f - 1.0f));
    uint sauvola_bin = (g > sauvola_thr) ? 255u : 0u;

    uint bg_win = params.bg_window;
    int br = (int)(bg_win / 2u);
    uint bg = 0u;

    [loop]
    for (int dy2 = -br; dy2 <= br; dy2++) {
        [loop]
        for (int dx2 = -br; dx2 <= br; dx2++) {
            uint v2 = load_gray_reflect((int)x + dx2, (int)y + dy2);
            bg = max(bg, v2);
        }
    }

    float normalized_f = params.percentile_c / max((float)bg, 0.000001f) * g;
    uint normalized = (uint)clamp(normalized_f, 0.0f, 255.0f);

    uint otsu_bin = (normalized > params.otsu_threshold) ? 255u : 0u;
    uint fused = (sauvola_bin != 0u && otsu_bin != 0u) ? 255u : 0u;

    dstBuffer[idx] = apply_output_invert(fused);
}
