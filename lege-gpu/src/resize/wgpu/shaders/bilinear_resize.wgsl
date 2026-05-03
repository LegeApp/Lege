struct ResizeParams {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    scale_x: f32,
    scale_y: f32,
    offset_x: f32,
    offset_y: f32,
    border_mode: u32,
    border_value: f32,
    channel_count: u32,
    no_srgb: u32,
}

@group(0) @binding(0) var<uniform> params: ResizeParams;
@group(0) @binding(1) var<storage, read> src_pixels: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst_pixels: array<u32>;

fn unpack_rgba8(p: u32) -> vec4<f32> {
    let r = f32((p >> 0u) & 0xffu) / 255.0;
    let g = f32((p >> 8u) & 0xffu) / 255.0;
    let b = f32((p >> 16u) & 0xffu) / 255.0;
    let a = f32((p >> 24u) & 0xffu) / 255.0;
    return vec4<f32>(r, g, b, a);
}

fn pack_rgba8(v: vec4<f32>) -> u32 {
    let c = clamp(v, vec4<f32>(0.0), vec4<f32>(1.0));
    let r = u32(round(c.r * 255.0));
    let g = u32(round(c.g * 255.0));
    let b = u32(round(c.b * 255.0));
    let a = u32(round(c.a * 255.0));
    return r | (g << 8u) | (b << 16u) | (a << 24u);
}

fn srgb_to_linear(x: f32) -> f32 {
    if (x <= 0.04045) {
        return x / 12.92;
    }
    return pow((x + 0.055) / 1.055, 2.4);
}

fn linear_to_srgb(x: f32) -> f32 {
    if (x <= 0.0031308) {
        return x * 12.92;
    }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

fn maybe_to_linear(c: vec4<f32>) -> vec4<f32> {
    if (params.no_srgb != 0u) {
        return c;
    }
    return vec4<f32>(
        srgb_to_linear(c.r),
        srgb_to_linear(c.g),
        srgb_to_linear(c.b),
        c.a,
    );
}

fn maybe_to_srgb(c: vec4<f32>) -> vec4<f32> {
    if (params.no_srgb != 0u) {
        return c;
    }
    return vec4<f32>(
        linear_to_srgb(c.r),
        linear_to_srgb(c.g),
        linear_to_srgb(c.b),
        c.a,
    );
}

fn clamp_coord(x: i32, maxv: i32) -> i32 {
    return clamp(x, 0, maxv - 1);
}

fn reflect_coord(x: i32, maxv: i32) -> i32 {
    if (maxv <= 1) {
        return 0;
    }
    let period = 2 * maxv - 2;
    var t = x % period;
    if (t < 0) {
        t = t + period;
    }
    if (t >= maxv) {
        t = period - t;
    }
    return t;
}

fn wrap_coord(x: i32, maxv: i32) -> i32 {
    var t = x % maxv;
    if (t < 0) {
        t = t + maxv;
    }
    return t;
}

fn sample_pixel_i(x: i32, y: i32) -> vec4<f32> {
    let w = i32(params.src_width);
    let h = i32(params.src_height);

    if (params.border_mode == 3u && (x < 0 || x >= w || y < 0 || y >= h)) {
        let v = params.border_value;
        return vec4<f32>(v, v, v, 1.0);
    }

    var sx: i32;
    if (params.border_mode == 1u) {
        sx = reflect_coord(x, w);
    } else if (params.border_mode == 2u) {
        sx = wrap_coord(x, w);
    } else {
        sx = clamp_coord(x, w);
    }

    var sy: i32;
    if (params.border_mode == 1u) {
        sy = reflect_coord(y, h);
    } else if (params.border_mode == 2u) {
        sy = wrap_coord(y, h);
    } else {
        sy = clamp_coord(y, h);
    }

    let idx = u32(sy * w + sx);
    return unpack_rgba8(src_pixels[idx]);
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dx = gid.x;
    let dy = gid.y;

    if (dx >= params.dst_width || dy >= params.dst_height) {
        return;
    }

    let src_x = (f32(dx) + 0.5) * params.scale_x - 0.5 + params.offset_x;
    let src_y = (f32(dy) + 0.5) * params.scale_y - 0.5 + params.offset_y;

    let x0 = i32(floor(src_x));
    let y0 = i32(floor(src_y));
    let x1 = x0 + 1;
    let y1 = y0 + 1;

    let tx = src_x - floor(src_x);
    let ty = src_y - floor(src_y);

    let c00 = maybe_to_linear(sample_pixel_i(x0, y0));
    let c10 = maybe_to_linear(sample_pixel_i(x1, y0));
    let c01 = maybe_to_linear(sample_pixel_i(x0, y1));
    let c11 = maybe_to_linear(sample_pixel_i(x1, y1));

    let cx0 = mix(c00, c10, tx);
    let cx1 = mix(c01, c11, tx);
    let outc = maybe_to_srgb(mix(cx0, cx1, ty));

    let out_idx = dy * params.dst_width + dx;
    dst_pixels[out_idx] = pack_rgba8(outc);
}
