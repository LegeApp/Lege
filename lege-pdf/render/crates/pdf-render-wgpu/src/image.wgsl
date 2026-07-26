struct Params {
    page_width: u32,
    page_height: u32,
    image_width: u32,
    image_height: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
    footprint_x: f32,
    footprint_y: f32,
    interpolation: u32,
    background: u32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> source: array<u32>;
@group(0) @binding(2) var<storage, read_write> page: array<u32>;

fn source_byte(offset: u32) -> u32 {
    let word = source[offset >> 2u];
    return (word >> ((offset & 3u) * 8u)) & 255u;
}

fn rgb_at(x: i32, y: i32) -> vec3<f32> {
    let sx = u32(clamp(x, 0, i32(params.image_width) - 1));
    let sy = u32(clamp(y, 0, i32(params.image_height) - 1));
    let offset = (sy * params.image_width + sx) * 3u;
    return vec3<f32>(
        f32(source_byte(offset)),
        f32(source_byte(offset + 1u)),
        f32(source_byte(offset + 2u)),
    );
}

fn pack_rgb(rgb: vec3<f32>) -> u32 {
    let color = vec3<u32>(clamp(rgb + vec3<f32>(0.5), vec3<f32>(0.0), vec3<f32>(255.0)));
    return color.r | (color.g << 8u) | (color.b << 16u) | 0xff000000u;
}

fn sample_nearest(fx: f32, fy: f32) -> vec3<f32> {
    return rgb_at(i32(round(fx)), i32(round(fy)));
}

fn sample_bilinear(fx: f32, fy: f32) -> vec3<f32> {
    let x0 = i32(floor(fx));
    let y0 = i32(floor(fy));
    let tx = fract(fx);
    let ty = fract(fy);
    let top = mix(rgb_at(x0, y0), rgb_at(x0 + 1, y0), tx);
    let bottom = mix(rgb_at(x0, y0 + 1), rgb_at(x0 + 1, y0 + 1), tx);
    return mix(top, bottom, ty);
}

fn sample_box(fx: f32, fy: f32) -> vec3<f32> {
    let half_x = max(params.footprint_x * 0.5, 0.5);
    let half_y = max(params.footprint_y * 0.5, 0.5);
    let left = max(fx + 0.5 - half_x, 0.0);
    let right = min(fx + 0.5 + half_x, f32(params.image_width));
    let top = max(fy + 0.5 - half_y, 0.0);
    let bottom = min(fy + 0.5 + half_y, f32(params.image_height));
    let lo_x = max(i32(floor(left)), 0);
    let hi_x = min(i32(ceil(right) - 1.0), i32(params.image_width) - 1);
    let lo_y = max(i32(floor(top)), 0);
    let hi_y = min(i32(ceil(bottom) - 1.0), i32(params.image_height) - 1);
    var sum = vec3<f32>(0.0);
    var total_weight = 0.0;
    for (var y = lo_y; y <= hi_y; y += 1) {
        let weight_y = max(0.0, min(bottom, f32(y + 1)) - max(top, f32(y)));
        for (var x = lo_x; x <= hi_x; x += 1) {
            let weight_x = max(0.0, min(right, f32(x + 1)) - max(left, f32(x)));
            let weight = weight_x * weight_y;
            sum += rgb_at(x, y) * weight;
            total_weight += weight;
        }
    }
    return sum / max(total_weight, 0.000001);
}

@compute @workgroup_size(16, 16, 1)
fn clear_page(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= params.page_width || id.y >= params.page_height) {
        return;
    }
    page[id.y * params.page_width + id.x] = params.background;
}

@compute @workgroup_size(16, 16, 1)
fn paint_image(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = params.x0 + i32(id.x);
    let y = params.y0 + i32(id.y);
    if (x >= params.x1 || y >= params.y1 || x < 0 || y < 0 ||
        x >= i32(params.page_width) || y >= i32(params.page_height)) {
        return;
    }

    let dx = f32(x) + 0.5;
    let dy = f32(y) + 0.5;
    let u = params.a * dx + params.c * dy + params.e;
    let v = params.b * dx + params.d * dy + params.f;
    if (u < 0.0 || u >= 1.0 || v < 0.0 || v >= 1.0) {
        return;
    }

    let fx = u * f32(params.image_width) - 0.5;
    let fy = (1.0 - v) * f32(params.image_height) - 0.5;
    var rgb: vec3<f32>;
    if (params.footprint_x > 1.0 || params.footprint_y > 1.0) {
        rgb = sample_box(fx, fy);
    } else if (params.interpolation == 1u) {
        rgb = sample_bilinear(fx, fy);
    } else {
        rgb = sample_nearest(fx, fy);
    }
    page[u32(y) * params.page_width + u32(x)] = pack_rgb(rgb);
}
