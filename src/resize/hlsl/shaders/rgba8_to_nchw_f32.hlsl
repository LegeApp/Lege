struct ConversionParams {
    uint width;
    uint height;
};

cbuffer ConversionConstants : register(b0) {
    ConversionParams params;
};

ByteAddressBuffer srcRgba8Buffer : register(t0);
RWByteAddressBuffer dstNchwF32Buffer : register(u0);

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;
    uint y = id.y;
    uint batch_index = id.z;

    if (x >= params.width || y >= params.height) {
        return;
    }

    uint src_pixel_offset_bytes = (y * params.width + x) * 4;
    uint packed_rgba = srcRgba8Buffer.Load(src_pixel_offset_bytes);

    // RGBA to BGR (model expects BGR)
    float b = ((packed_rgba >> 16) & 0xFF) / 255.0f;
    float g = ((packed_rgba >> 8) & 0xFF) / 255.0f;
    float r = ( packed_rgba        & 0xFF) / 255.0f;

    uint channel_size_pixels = params.width * params.height;
    uint batch_offset_floats = batch_index * 3 * channel_size_pixels;
    uint pixel_index_in_channel = y * params.width + x;

    uint b_channel_offset_floats = batch_offset_floats;
    uint g_channel_offset_floats = batch_offset_floats + channel_size_pixels;
    uint r_channel_offset_floats = batch_offset_floats + 2 * channel_size_pixels;

    dstNchwF32Buffer.Store( (b_channel_offset_floats + pixel_index_in_channel) * 4, asuint(b) );
    dstNchwF32Buffer.Store( (g_channel_offset_floats + pixel_index_in_channel) * 4, asuint(g) );
    dstNchwF32Buffer.Store( (r_channel_offset_floats + pixel_index_in_channel) * 4, asuint(r) );
}
