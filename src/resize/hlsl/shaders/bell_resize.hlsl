// Bell filter resize compute shader
// Implements Bell (also known as B-spline or quadratic) filtering for higher quality resizing

struct ResizeParams {
    uint src_width;
    uint src_height;
    uint dst_width;
    uint dst_height;
    float scale_x;
    float scale_y;
    float offset_x;
    float offset_y;
    uint border_mode;
    float border_value;
    uint channel_count;
    uint no_srgb;
};

cbuffer ResizeConstants : register(b0) {
    ResizeParams params;
};

ByteAddressBuffer srcBuffer : register(t0);
RWByteAddressBuffer dstBuffer : register(u0);

// Bell filter kernel (support = 1.5)
float bell_filter(float x) {
    x = abs(x);
    if (x < 0.5f) {
        return 0.75f - x * x;
    } else if (x < 1.5f) {
        x = 1.5f - x;
        return 0.5f * x * x;
    }
    return 0.0f;
}

// Utility functions
float srgb_to_linear(float srgb) {
    return (srgb <= 0.04045f) ? (srgb / 12.92f) : pow((srgb + 0.055f) / 1.055f, 2.4f);
}

float linear_to_srgb(float lin_val) {
    return (lin_val <= 0.0031308f) ? (lin_val * 12.92f) : (1.055f * pow(lin_val, 1.0f / 2.4f) - 0.055f);
}

float4 sample_border(int x, int y) {
    switch (params.border_mode) {
        case 0: // Clamp
            x = clamp(x, 0, (int)params.src_width - 1);
            y = clamp(y, 0, (int)params.src_height - 1);
            break;
        case 1: // Reflect
            if (x < 0) x = -x - 1;
            if (x >= (int)params.src_width) x = 2 * (int)params.src_width - x - 1;
            if (y < 0) y = -y - 1;
            if (y >= (int)params.src_height) y = 2 * (int)params.src_height - y - 1;
            break;
        case 2: // Wrap
            x = x % (int)params.src_width;
            if (x < 0) x += (int)params.src_width;
            y = y % (int)params.src_height;
            if (y < 0) y += (int)params.src_height;
            break;
        case 3: // Constant
            if (x < 0 || x >= (int)params.src_width || y < 0 || y >= (int)params.src_height) {
                return float4(params.border_value, params.border_value, params.border_value, 1.0f);
            }
            break;
    }

    // Always treat as RGBA8
    uint pixel_offset = (y * params.src_width + x) * 4;
    uint packed = srcBuffer.Load(pixel_offset);
    uint r =  packed        & 0xFF;
    uint g = (packed >> 8) & 0xFF;
    uint b = (packed >>16) & 0xFF;
    uint a = (packed >>24) & 0xFF;
    return float4(r / 255.0f, g / 255.0f, b / 255.0f, a / 255.0f);
}

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint dst_x = id.x;
    uint dst_y = id.y;

    if (dst_x >= params.dst_width || dst_y >= params.dst_height) {
        return;
    }

    // Calculate source coordinates
    float src_x = (dst_x + params.offset_x) * params.scale_x;
    float src_y = (dst_y + params.offset_y) * params.scale_y;

    // Bell filter has support of 1.5, so we need to sample a 3x3 neighborhood
    int center_x = (int)floor(src_x);
    int center_y = (int)floor(src_y);

    float4 result = float4(0, 0, 0, 0);
    float weight_sum = 0.0f;

    // Sample 3x3 neighborhood
    for (int dy = -1; dy <= 1; dy++) {
        for (int dx = -1; dx <= 1; dx++) {
            int sample_x = center_x + dx;
            int sample_y = center_y + dy;

            float weight_x = bell_filter(src_x - sample_x);
            float weight_y = bell_filter(src_y - sample_y);
            float weight = weight_x * weight_y;

            if (weight > 0.0f) {
                float4 sample = sample_border(sample_x, sample_y);

                // Convert to linear space for proper filtering unless disabled
                if (params.no_srgb == 0) {
                    sample.rgb = float3(
                        srgb_to_linear(sample.r),
                        srgb_to_linear(sample.g),
                        srgb_to_linear(sample.b)
                    );
                }

                result += weight * sample;
                weight_sum += weight;
            }
        }
    }

    // Normalize by weight sum
    if (weight_sum > 0.0f) {
        result /= weight_sum;
    }

    // Convert back to sRGB unless disabled
    if (params.no_srgb == 0) {
        result.rgb = float3(
            linear_to_srgb(result.r),
            linear_to_srgb(result.g),
            linear_to_srgb(result.b)
        );
    }

    // Clamp to valid range
    result = saturate(result);

    // Write result
    // Always write packed RGBA8 (A forced to 255)
    uint dst_offset = (dst_y * params.dst_width + dst_x) * 4;
    uint r = (uint)round(result.r * 255.0f);
    uint g = (uint)round(result.g * 255.0f);
    uint b = (uint)round(result.b * 255.0f);
    uint a = 255u;
    uint packed = (a << 24) | (b << 16) | (g << 8) | r;
    dstBuffer.Store(dst_offset, packed);
}