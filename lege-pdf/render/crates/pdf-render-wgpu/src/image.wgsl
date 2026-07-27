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
    mask_width: u32,
    mask_height: u32,
    mask_footprint_x: f32,
    mask_footprint_y: f32,
    has_opacity: u32,
    is_stencil: u32,
    stencil_rgb: u32,
    opacity_box_filter: u32,
    clip_x: i32,
    clip_y: i32,
    clip_width: u32,
    clip_height: u32,
    has_clip: u32,
    image_alpha: u32,
    blend_mode: u32,
    _pad2: u32,
    soft_x: i32,
    soft_y: i32,
    soft_width: u32,
    soft_height: u32,
    has_soft_mask: u32,
    soft_outside: u32,
    _pad3: u32,
    _pad4: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> source: array<u32>;
@group(0) @binding(2) var<storage, read_write> page: array<u32>;
@group(0) @binding(3) var<storage, read> opacity: array<u32>;
@group(0) @binding(4) var<storage, read> clip_mask: array<u32>;
@group(0) @binding(5) var<storage, read> page_soft_mask: array<u32>;

fn source_byte(offset: u32) -> u32 {
    let word = source[offset >> 2u];
    return (word >> ((offset & 3u) * 8u)) & 255u;
}

fn opacity_byte(offset: u32) -> u32 {
    let word = opacity[offset >> 2u];
    return (word >> ((offset & 3u) * 8u)) & 255u;
}

fn clip_byte(offset: u32) -> u32 {
    let word = clip_mask[offset >> 2u];
    return (word >> ((offset & 3u) * 8u)) & 255u;
}

fn soft_mask_byte(offset: u32) -> u32 {
    let word = page_soft_mask[offset >> 2u];
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

fn opacity_at(x: i32, y: i32) -> f32 {
    let sx = u32(clamp(x, 0, i32(params.mask_width) - 1));
    let sy = u32(clamp(y, 0, i32(params.mask_height) - 1));
    return f32(opacity_byte(sy * params.mask_width + sx));
}

fn sample_opacity(u: f32, v: f32) -> f32 {
    if (params.has_opacity == 0u) {
        return 255.0;
    }
    let fx = u * f32(params.mask_width) - 0.5;
    let fy = (1.0 - v) * f32(params.mask_height) - 0.5;
    if (params.opacity_box_filter == 0u ||
        (params.mask_footprint_x <= 1.0 && params.mask_footprint_y <= 1.0)) {
        let x = i32(floor(u * f32(params.mask_width)));
        let y = i32(floor((1.0 - v) * f32(params.mask_height)));
        return opacity_at(x, y);
    }

    let half_x = max(params.mask_footprint_x * 0.5, 0.5);
    let half_y = max(params.mask_footprint_y * 0.5, 0.5);
    let left = max(fx + 0.5 - half_x, 0.0);
    let right = min(fx + 0.5 + half_x, f32(params.mask_width));
    let top = max(fy + 0.5 - half_y, 0.0);
    let bottom = min(fy + 0.5 + half_y, f32(params.mask_height));
    let lo_x = max(i32(floor(left)), 0);
    let hi_x = min(i32(ceil(right) - 1.0), i32(params.mask_width) - 1);
    let lo_y = max(i32(floor(top)), 0);
    let hi_y = min(i32(ceil(bottom) - 1.0), i32(params.mask_height) - 1);
    var sum = 0.0;
    var total_weight = 0.0;
    for (var y = lo_y; y <= hi_y; y += 1) {
        let weight_y = max(0.0, min(bottom, f32(y + 1)) - max(top, f32(y)));
        for (var x = lo_x; x <= hi_x; x += 1) {
            let weight_x = max(0.0, min(right, f32(x + 1)) - max(left, f32(x)));
            let weight = weight_x * weight_y;
            sum += opacity_at(x, y) * weight;
            total_weight += weight;
        }
    }
    return sum / max(total_weight, 0.000001);
}

fn sample_clip(x: i32, y: i32) -> f32 {
    if (params.has_clip == 0u) {
        return 255.0;
    }
    let local_x = x - params.clip_x;
    let local_y = y - params.clip_y;
    if (local_x < 0 || local_y < 0 ||
        local_x >= i32(params.clip_width) || local_y >= i32(params.clip_height)) {
        return 0.0;
    }
    return f32(clip_byte(u32(local_y) * params.clip_width + u32(local_x)));
}

fn sample_soft_mask(x: i32, y: i32) -> f32 {
    if (params.has_soft_mask == 0u) {
        return 255.0;
    }
    let local_x = x - params.soft_x;
    let local_y = y - params.soft_y;
    if (local_x < 0 || local_y < 0 ||
        local_x >= i32(params.soft_width) || local_y >= i32(params.soft_height)) {
        return f32(params.soft_outside);
    }
    return f32(soft_mask_byte(u32(local_y) * params.soft_width + u32(local_x)));
}

fn stencil_color() -> vec3<f32> {
    return vec3<f32>(
        f32(params.stencil_rgb & 255u),
        f32((params.stencil_rgb >> 8u) & 255u),
        f32((params.stencil_rgb >> 16u) & 255u),
    );
}

fn source_over(rgb: vec3<f32>, alpha: f32, destination: u32) -> u32 {
    let a = u32(clamp(alpha + 0.5, 0.0, 255.0));
    if (a == 0u) {
        return destination;
    }
    if (a == 255u) {
        return pack_rgb(rgb);
    }
    let straight = vec3<u32>(clamp(rgb + vec3<f32>(0.5), vec3<f32>(0.0), vec3<f32>(255.0)));
    let dst = vec4<u32>(
        destination & 255u,
        (destination >> 8u) & 255u,
        (destination >> 16u) & 255u,
        (destination >> 24u) & 255u,
    );
    let inverse = 255u - a;
    let premultiplied = (straight * vec3<u32>(a) + vec3<u32>(127u)) / vec3<u32>(255u);
    let out_rgb = min(
        premultiplied + (dst.rgb * vec3<u32>(inverse) + vec3<u32>(127u)) / vec3<u32>(255u),
        vec3<u32>(255u),
    );
    let out_alpha = min(a + (dst.a * inverse + 127u) / 255u, 255u);
    return out_rgb.r | (out_rgb.g << 8u) | (out_rgb.b << 16u) | (out_alpha << 24u);
}

fn screen(b: f32, s: f32) -> f32 {
    return b + s - b * s;
}

fn hard_light(b: f32, s: f32) -> f32 {
    if (s <= 0.5) {
        return b * (2.0 * s);
    }
    return screen(b, 2.0 * s - 1.0);
}

fn soft_light(b: f32, s: f32) -> f32 {
    if (s <= 0.5) {
        return b - (1.0 - 2.0 * s) * b * (1.0 - b);
    }
    var d: f32;
    if (b <= 0.25) {
        d = ((16.0 * b - 12.0) * b + 4.0) * b;
    } else {
        d = sqrt(b);
    }
    return b + (2.0 * s - 1.0) * (d - b);
}

fn separable_component(b: f32, s: f32, mode: u32) -> f32 {
    if (mode == 1u) {
        return b * s;
    }
    if (mode == 2u) {
        return screen(b, s);
    }
    if (mode == 3u) {
        return hard_light(s, b);
    }
    if (mode == 4u) {
        return min(b, s);
    }
    if (mode == 5u) {
        return max(b, s);
    }
    if (mode == 6u) {
        if (b == 0.0) {
            return 0.0;
        }
        if (s >= 1.0) {
            return 1.0;
        }
        return min(1.0, b / (1.0 - s));
    }
    if (mode == 7u) {
        if (b >= 1.0) {
            return 1.0;
        }
        if (s <= 0.0) {
            return 0.0;
        }
        return 1.0 - min(1.0, (1.0 - b) / s);
    }
    if (mode == 8u) {
        return hard_light(b, s);
    }
    if (mode == 9u) {
        return soft_light(b, s);
    }
    if (mode == 10u) {
        return abs(b - s);
    }
    if (mode == 11u) {
        return b + s - 2.0 * b * s;
    }
    return s;
}

fn lum(c: vec3<f32>) -> f32 {
    return 0.3 * c.r + 0.59 * c.g + 0.11 * c.b;
}

fn sat(c: vec3<f32>) -> f32 {
    return max(c.r, max(c.g, c.b)) - min(c.r, min(c.g, c.b));
}

fn clip_color(input: vec3<f32>) -> vec3<f32> {
    var c = input;
    let l = lum(c);
    let n = min(c.r, min(c.g, c.b));
    let x = max(c.r, max(c.g, c.b));
    if (n < 0.0) {
        c = vec3<f32>(l) + (c - vec3<f32>(l)) * l / (l - n);
    }
    if (x > 1.0) {
        c = vec3<f32>(l) + (c - vec3<f32>(l)) * (1.0 - l) / (x - l);
    }
    return c;
}

fn set_lum(c: vec3<f32>, l: f32) -> vec3<f32> {
    return clip_color(c + vec3<f32>(l - lum(c)));
}

fn set_sat(input: vec3<f32>, s: f32) -> vec3<f32> {
    var c = input;
    var lo = 0u;
    var mid = 1u;
    var hi = 2u;
    if (c[lo] > c[mid]) {
        let swap = lo;
        lo = mid;
        mid = swap;
    }
    if (c[mid] > c[hi]) {
        let swap = mid;
        mid = hi;
        hi = swap;
    }
    if (c[lo] > c[mid]) {
        let swap = lo;
        lo = mid;
        mid = swap;
    }
    if (c[hi] > c[lo]) {
        c[mid] = (c[mid] - c[lo]) * s / (c[hi] - c[lo]);
        c[hi] = s;
    } else {
        c[mid] = 0.0;
        c[hi] = 0.0;
    }
    c[lo] = 0.0;
    return c;
}

fn blend_color(cb: vec3<f32>, cs: vec3<f32>, mode: u32) -> vec3<f32> {
    if (mode <= 11u) {
        return vec3<f32>(
            separable_component(cb.r, cs.r, mode),
            separable_component(cb.g, cs.g, mode),
            separable_component(cb.b, cs.b, mode),
        );
    }
    if (mode == 12u) {
        return set_lum(set_sat(cs, sat(cb)), lum(cb));
    }
    if (mode == 13u) {
        return set_lum(set_sat(cb, sat(cs)), lum(cb));
    }
    if (mode == 14u) {
        return set_lum(cs, lum(cb));
    }
    return set_lum(cb, lum(cs));
}

fn blended_source_over(rgb: vec3<f32>, alpha: f32, destination: u32) -> u32 {
    let a_s = clamp(alpha / 255.0, 0.0, 1.0);
    if (a_s <= 0.0) {
        return destination;
    }
    let dst = vec4<f32>(
        f32(destination & 255u),
        f32((destination >> 8u) & 255u),
        f32((destination >> 16u) & 255u),
        f32((destination >> 24u) & 255u),
    ) / 255.0;
    let cs = clamp(rgb / 255.0, vec3<f32>(0.0), vec3<f32>(1.0));
    let da = dst.a;
    var cb = vec3<f32>(0.0);
    if (da > 0.0) {
        cb = dst.rgb / da;
    }
    let blended = blend_color(cb, cs, params.blend_mode);
    let mixed = (1.0 - da) * cs + da * blended;
    let out_rgb = a_s * mixed + (1.0 - a_s) * dst.rgb;
    let out_alpha = a_s + (1.0 - a_s) * da;
    let out = vec4<u32>(clamp(
        vec4<f32>(out_rgb, out_alpha) * 255.0 + vec4<f32>(0.5),
        vec4<f32>(0.0),
        vec4<f32>(255.0),
    ));
    return out.r | (out.g << 8u) | (out.b << 16u) | (out.a << 24u);
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
    if (params.is_stencil == 1u) {
        rgb = stencil_color();
    } else if (params.footprint_x > 1.0 || params.footprint_y > 1.0) {
        rgb = sample_box(fx, fy);
    } else if (params.interpolation == 1u) {
        rgb = sample_bilinear(fx, fy);
    } else {
        rgb = sample_nearest(fx, fy);
    }
    let page_index = u32(y) * params.page_width + u32(x);
    if (params.blend_mode == 0u && params.image_alpha == 255u &&
        params.has_opacity == 0u &&
        params.has_clip == 0u && params.has_soft_mask == 0u) {
        page[page_index] = pack_rgb(rgb);
    } else {
        let alpha = f32(params.image_alpha) * sample_opacity(u, v) *
            sample_clip(x, y) * sample_soft_mask(x, y) /
            (255.0 * 255.0 * 255.0);
        if (params.blend_mode == 0u) {
            page[page_index] = source_over(rgb, alpha, page[page_index]);
        } else {
            page[page_index] = blended_source_over(rgb, alpha, page[page_index]);
        }
    }
}

fn edge_component(edge: u32, component: u32) -> f32 {
    return bitcast<f32>(source[4u + edge * 4u + component]);
}

fn path_covered_samples(x: i32, y: i32, samples: u32) -> u32 {
    var covered = 0u;
    var winding: array<i32, 8>;
    let band_count = source[1u];
    let offsets_base = source[2u];
    let indices_base = source[3u];
    for (var sy = 0u; sy < samples; sy += 1u) {
        for (var sx = 0u; sx < samples; sx += 1u) {
            winding[sx] = 0i;
        }

        let py = f32(y) + (f32(sy) + 0.5) / f32(samples);
        let band = min(
            u32(max(0.0, floor((py - f32(params.y0)) / 16.0))),
            band_count - 1u,
        );
        let begin = source[offsets_base + band];
        let end = source[offsets_base + band + 1u];
        for (var reference = begin; reference < end; reference += 1u) {
            let edge = source[indices_base + reference];
            let x0 = edge_component(edge, 0u);
            let y0 = edge_component(edge, 1u);
            let x1 = edge_component(edge, 2u);
            let y1 = edge_component(edge, 3u);
            let upward = y0 <= py && y1 > py;
            let downward = y1 <= py && y0 > py;
            if (!upward && !downward) {
                continue;
            }
            let crossing_x = x0 + (py - y0) * (x1 - x0) / (y1 - y0);
            let delta = select(-1i, 1i, upward);
            for (var sx = 0u; sx < samples; sx += 1u) {
                let px = f32(x) + (f32(sx) + 0.5) / f32(samples);
                if (crossing_x > px) {
                    winding[sx] += delta;
                }
            }
        }
        for (var sx = 0u; sx < samples; sx += 1u) {
            let inside = select(
                winding[sx] != 0i,
                (abs(winding[sx]) % 2i) != 0i,
                params.opacity_box_filter != 0u,
            );
            if (inside) {
                covered += 1u;
            }
        }
    }
    return covered;
}

@compute @workgroup_size(8, 8, 1)
fn paint_path(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = params.x0 + i32(id.x);
    let y = params.y0 + i32(id.y);
    if (x >= params.x1 || y >= params.y1 || x < 0 || y < 0 ||
        x >= i32(params.page_width) || y >= i32(params.page_height)) {
        return;
    }

    let samples = clamp(params.interpolation, 1u, 8u);
    let covered = path_covered_samples(x, y, samples);
    if (covered == 0u) {
        return;
    }

    let coverage = 255.0 * f32(covered) / f32(samples * samples);
    let alpha = f32(params.image_alpha) * coverage *
        sample_clip(x, y) * sample_soft_mask(x, y) /
        (255.0 * 255.0 * 255.0);
    let page_index = u32(y) * params.page_width + u32(x);
    let rgb = stencil_color();
    if (params.blend_mode == 0u) {
        page[page_index] = source_over(rgb, alpha, page[page_index]);
    } else {
        page[page_index] = blended_source_over(rgb, alpha, page[page_index]);
    }
}
