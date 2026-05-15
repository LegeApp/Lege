// Bilinear resize compute shader
// Optimized for high-performance image resizing with bilinear filtering

struct ResizeParams {
    uint src_width;
    uint src_height;
    uint dst_width;
    uint dst_height;
    float scale_x;
    float scale_y;
    float offset_x;
    float offset_y;
    uint border_mode;  // 0=clamp, 1=reflect, 2=wrap, 3=constant
    float border_value;
    uint channel_count;  // 1, 3, or 4
    uint no_srgb;        // 1 = do not convert to/from linear
};

cbuffer ResizeConstants : register(b0) {
    ResizeParams params;
};

ByteAddressBuffer srcBuffer : register(t0);
RWByteAddressBuffer dstBuffer : register(u0);

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

    // Always treat as RGBA8 (4 bytes per pixel)
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
    float src_x = ((float)dst_x + 0.5f) * params.scale_x - 0.5f + params.offset_x;
    float src_y = ((float)dst_y + 0.5f) * params.scale_y - 0.5f + params.offset_y;

    // Bilinear interpolation
    int x0 = (int)floor(src_x);
    int y0 = (int)floor(src_y);
    int x1 = x0 + 1;
    int y1 = y0 + 1;

    float fx = src_x - x0;
    float fy = src_y - y0;

    // Sample four corners
    float4 c00 = sample_border(x0, y0);
    float4 c10 = sample_border(x1, y0);
    float4 c01 = sample_border(x0, y1);
    float4 c11 = sample_border(x1, y1);

    // Convert to linear space for proper interpolation unless disabled
    if (params.no_srgb == 0) {
        c00.rgb = float3(srgb_to_linear(c00.r), srgb_to_linear(c00.g), srgb_to_linear(c00.b));
        c10.rgb = float3(srgb_to_linear(c10.r), srgb_to_linear(c10.g), srgb_to_linear(c10.b));
        c01.rgb = float3(srgb_to_linear(c01.r), srgb_to_linear(c01.g), srgb_to_linear(c01.b));
        c11.rgb = float3(srgb_to_linear(c11.r), srgb_to_linear(c11.g), srgb_to_linear(c11.b));
    }

    // Bilinear interpolation
    float4 top = lerp(c00, c10, fx);
    float4 bottom = lerp(c01, c11, fx);
    float4 result = lerp(top, bottom, fy);

    // Convert back to sRGB unless disabled
    if (params.no_srgb == 0) {
        result.rgb = float3(linear_to_srgb(result.r), linear_to_srgb(result.g), linear_to_srgb(result.b));
    }

    // Clamp to valid range
    result = saturate(result);

    // Write result
    // Always write RGBA8 packed into a single 32-bit value
    uint dst_offset = (dst_y * params.dst_width + dst_x) * 4;
    uint r = (uint)round(result.r * 255.0f);
    uint g = (uint)round(result.g * 255.0f);
    uint b = (uint)round(result.b * 255.0f);
    uint a = 255u; // Opaque alpha
    uint packed = (a << 24) | (b << 16) | (g << 8) | r;
    dstBuffer.Store(dst_offset, packed);
}
