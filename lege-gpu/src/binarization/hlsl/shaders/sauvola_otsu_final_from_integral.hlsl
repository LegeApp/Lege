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

    uint padded_width;
    uint padded_height;
    uint integral_width;
    uint sauvola_radius;

    uint debug_mode;
    uint _pad2;
    uint _pad3;
    uint _pad4;
    uint _pad5;
    uint _pad6;
    uint _pad7;
};

cbuffer BinarizeConstants : register(b0) {
    BinarizeParams params;
};

StructuredBuffer<uint> grayBuffer     : register(t0);
StructuredBuffer<uint> integral       : register(t1);
StructuredBuffer<float> integralSq    : register(t2);
StructuredBuffer<uint> bgBuffer       : register(t3);
RWStructuredBuffer<uint> dstBuffer    : register(u0);

uint apply_output_invert(uint v) {
    return (params.invert_output != 0u) ? (255u - v) : v;
}

uint integral_sum(
    StructuredBuffer<uint> ii,
    uint x0,
    uint y0,
    uint x1,
    uint y1
) {
    uint iw = params.integral_width;

    uint d = ii[(y1 + 1u) * iw + (x1 + 1u)];
    uint b = ii[(y1 + 1u) * iw + x0];
    uint c = ii[y0 * iw + (x1 + 1u)];
    uint a = ii[y0 * iw + x0];

    return d + a - b - c;
}

float integral_sum_f32(
    StructuredBuffer<float> ii,
    uint x0,
    uint y0,
    uint x1,
    uint y1
) {
    uint iw = params.integral_width;

    float d = ii[(y1 + 1u) * iw + (x1 + 1u)];
    float b = ii[(y1 + 1u) * iw + x0];
    float c = ii[y0 * iw + (x1 + 1u)];
    float a = ii[y0 * iw + x0];

    return d + a - b - c;
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

    uint r = params.sauvola_radius;

    uint x0 = x;
    uint y0 = y;
    uint x1 = x + 2u * r;
    uint y1 = y + 2u * r;

    uint sum_u = integral_sum(integral, x0, y0, x1, y1);
    float sum_sq = integral_sum_f32(integralSq, x0, y0, x1, y1);

    float area = (float)(params.sauvola_window * params.sauvola_window);
    float mean = (float)sum_u / area;
    float var = (sum_sq / area) - mean * mean;
    float stddev = sqrt(max(var, 0.0f));

    float sauvola_thr =
        mean * (1.0f + params.k_factor * (stddev / 127.0f - 1.0f));

    uint sauvola_bin = ((float)g_u > sauvola_thr) ? 255u : 0u;

    uint bg = max(bgBuffer[idx] & 255u, 1u);
    float normalized_f = params.percentile_c / (float)bg * (float)g_u;
    uint normalized = (uint)clamp(normalized_f, 0.0f, 255.0f);

    uint otsu_bin = (normalized > params.otsu_threshold) ? 255u : 0u;
    uint fused = (sauvola_bin != 0u && otsu_bin != 0u) ? 255u : 0u;

    if (params.debug_mode == 0u) {
        dstBuffer[idx] = apply_output_invert(fused);
    } else if (params.debug_mode == 1u) {
        dstBuffer[idx] = apply_output_invert(sauvola_bin);
    } else if (params.debug_mode == 2u) {
        dstBuffer[idx] = apply_output_invert(otsu_bin);
    } else if (params.debug_mode == 3u) {
        dstBuffer[idx] = (uint)clamp((float)bg, 0.0f, 255.0f);
    } else if (params.debug_mode == 4u) {
        dstBuffer[idx] = normalized;
    } else {
        dstBuffer[idx] = apply_output_invert(fused);
    }
}
