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

fn sinc(x: f32) -> f32 {
    if (abs(x) < 1e-5) {
        return 1.0;
    }
    let pix = 3.141592653589793 * x;
    return sin(pix) / pix;
}

fn lanczos3_weight(x: f32) -> f32 {
    let ax = abs(x);
    if (ax >= 3.0) {
        return 0.0;
    }
    return sinc(x) * sinc(x / 3.0);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dx = gid.x;
    let dy = gid.y;

    if (dx >= params.dst_width || dy >= params.dst_height) {
        return;
    }

    let src_x = (f32(dx) + 0.5) * params.scale_x - 0.5 + params.offset_x;
    let src_y = (f32(dy) + 0.5) * params.scale_y - 0.5 + params.offset_y;

    let base_x = i32(floor(src_x));
    let base_y = i32(floor(src_y));

    var accum = vec4<f32>(0.0);
    var weight_sum = 0.0;

    for (var j = -2; j <= 3; j = j + 1) {
        for (var i = -2; i <= 3; i = i + 1) {
            let sx = f32(base_x + i);
            let sy = f32(base_y + j);
            let wx = lanczos3_weight(src_x - sx);
            let wy = lanczos3_weight(src_y - sy);
            let w = wx * wy;

            let c = maybe_to_linear(sample_pixel_i(base_x + i, base_y + j));
            accum = accum + c * w;
            weight_sum = weight_sum + w;
        }
    }

    let outc = maybe_to_srgb(accum / max(weight_sum, 1e-8));
    let out_idx = dy * params.dst_width + dx;
    dst_pixels[out_idx] = pack_rgba8(outc);
}
