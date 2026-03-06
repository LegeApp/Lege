struct ConvertParams {
    width: u32,
    height: u32,
    batch_size: u32,
    _pad: u32,
};

@group(0) @binding(0) var<uniform> params: ConvertParams;
@group(0) @binding(1) var<storage, read> src_pixels: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst_tensor: array<f32>;

fn unpack_rgba8(p: u32) -> vec4<f32> {
    let r = f32((p >> 0u) & 0xFFu) / 255.0;
    let g = f32((p >> 8u) & 0xFFu) / 255.0;
    let b = f32((p >> 16u) & 0xFFu) / 255.0;
    let a = f32((p >> 24u) & 0xFFu) / 255.0;
    return vec4<f32>(r, g, b, a);
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x;
    let y = gid.y;
    let batch = gid.z;

    if (x >= params.width || y >= params.height || batch >= params.batch_size) {
        return;
    }

    let hw = params.width * params.height;
    let plane_offset = y * params.width + x;
    let pixel_index = plane_offset;
    let pixel = unpack_rgba8(src_pixels[pixel_index]);

    let batch_base = batch * 3u * hw;
    let b_idx = batch_base + plane_offset;
    let g_idx = batch_base + hw + plane_offset;
    let r_idx = batch_base + (2u * hw) + plane_offset;

    dst_tensor[b_idx] = pixel.b;
    dst_tensor[g_idx] = pixel.g;
    dst_tensor[r_idx] = pixel.r;
}
