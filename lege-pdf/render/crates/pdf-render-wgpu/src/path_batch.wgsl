struct BatchParams {
    page_width: u32,
    page_height: u32,
    tile_count: u32,
    samples: u32,
    dispatch_width: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

struct PathDescriptor {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    geometry_base: u32,
    rgb: u32,
    alpha: u32,
    blend_mode: u32,
    even_odd: u32,
    clip_x: i32,
    clip_y: i32,
    clip_width: u32,
    clip_height: u32,
    clip_offset: u32,
    has_clip: u32,
    soft_x: i32,
    soft_y: i32,
    soft_width: u32,
    soft_height: u32,
    soft_offset: u32,
    soft_outside: u32,
    has_soft_mask: u32,
}

struct PathTile {
    x: u32,
    y: u32,
    path_offset: u32,
    path_count: u32,
}

@group(0) @binding(0) var<uniform> params: BatchParams;
@group(0) @binding(1) var<storage, read> paths: array<PathDescriptor>;
@group(0) @binding(2) var<storage, read> geometry: array<u32>;
@group(0) @binding(3) var<storage, read> tiles: array<PathTile>;
@group(0) @binding(4) var<storage, read> tile_paths: array<u32>;
@group(0) @binding(5) var<storage, read> masks: array<u32>;
@group(0) @binding(6) var<storage, read_write> page: array<u32>;

var<workgroup> coverage: array<atomic<u32>, 64>;

fn mask_byte(offset: u32) -> u32 {
    let word = masks[offset >> 2u];
    return (word >> ((offset & 3u) * 8u)) & 255u;
}

fn sample_clip(path: PathDescriptor, x: i32, y: i32) -> f32 {
    if (path.has_clip == 0u) {
        return 255.0;
    }
    let local_x = x - path.clip_x;
    let local_y = y - path.clip_y;
    if (local_x < 0 || local_y < 0 ||
        local_x >= i32(path.clip_width) || local_y >= i32(path.clip_height)) {
        return 0.0;
    }
    let offset = path.clip_offset + u32(local_y) * path.clip_width + u32(local_x);
    return f32(mask_byte(offset));
}

fn sample_soft_mask(path: PathDescriptor, x: i32, y: i32) -> f32 {
    if (path.has_soft_mask == 0u) {
        return 255.0;
    }
    let local_x = x - path.soft_x;
    let local_y = y - path.soft_y;
    if (local_x < 0 || local_y < 0 ||
        local_x >= i32(path.soft_width) || local_y >= i32(path.soft_height)) {
        return f32(path.soft_outside);
    }
    let offset = path.soft_offset + u32(local_y) * path.soft_width + u32(local_x);
    return f32(mask_byte(offset));
}

fn path_rgb(path: PathDescriptor) -> vec3<f32> {
    return vec3<f32>(
        f32(path.rgb & 255u),
        f32((path.rgb >> 8u) & 255u),
        f32((path.rgb >> 16u) & 255u),
    );
}

fn pack_rgb(rgb: vec3<f32>) -> u32 {
    let color = vec3<u32>(clamp(rgb + vec3<f32>(0.5), vec3<f32>(0.0), vec3<f32>(255.0)));
    return color.r | (color.g << 8u) | (color.b << 16u) | 0xff000000u;
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
    if (mode == 1u) { return b * s; }
    if (mode == 2u) { return screen(b, s); }
    if (mode == 3u) { return hard_light(s, b); }
    if (mode == 4u) { return min(b, s); }
    if (mode == 5u) { return max(b, s); }
    if (mode == 6u) {
        if (b == 0.0) { return 0.0; }
        if (s >= 1.0) { return 1.0; }
        return min(1.0, b / (1.0 - s));
    }
    if (mode == 7u) {
        if (b >= 1.0) { return 1.0; }
        if (s <= 0.0) { return 0.0; }
        return 1.0 - min(1.0, (1.0 - b) / s);
    }
    if (mode == 8u) { return hard_light(b, s); }
    if (mode == 9u) { return soft_light(b, s); }
    if (mode == 10u) { return abs(b - s); }
    if (mode == 11u) { return b + s - 2.0 * b * s; }
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
        let swap = lo; lo = mid; mid = swap;
    }
    if (c[mid] > c[hi]) {
        let swap = mid; mid = hi; hi = swap;
    }
    if (c[lo] > c[mid]) {
        let swap = lo; lo = mid; mid = swap;
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
    if (mode == 12u) { return set_lum(set_sat(cs, sat(cb)), lum(cb)); }
    if (mode == 13u) { return set_lum(set_sat(cb, sat(cs)), lum(cb)); }
    if (mode == 14u) { return set_lum(cs, lum(cb)); }
    return set_lum(cb, lum(cs));
}

fn blended_source_over(
    rgb: vec3<f32>,
    alpha: f32,
    destination: u32,
    mode: u32,
) -> u32 {
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
    let blended = blend_color(cb, cs, mode);
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

fn edge_component(base: u32, edge: u32, component: u32) -> f32 {
    return bitcast<f32>(geometry[base + 4u + edge * 4u + component]);
}

@compute @workgroup_size(8, 8, 1)
fn paint_path_batch(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(local_invocation_id) local: vec3<u32>,
    @builtin(local_invocation_index) lane: u32,
) {
    let tile_index = group.y * params.dispatch_width + group.x;
    if (tile_index >= params.tile_count) {
        return;
    }
    let tile = tiles[tile_index];
    let x = i32(tile.x + local.x);
    let y = i32(tile.y + local.y);
    let valid_pixel = x >= 0 && y >= 0 &&
        x < i32(params.page_width) && y < i32(params.page_height);
    var destination = 0u;
    if (valid_pixel) {
        destination = page[u32(y) * params.page_width + u32(x)];
    }
    let samples = clamp(params.samples, 1u, 8u);

    for (var tile_path = 0u; tile_path < tile.path_count; tile_path += 1u) {
        atomicStore(&coverage[lane], 0u);
        workgroupBarrier();

        let path_index = tile_paths[tile.path_offset + tile_path];
        let path = paths[path_index];
        let subrow_count = samples * 8u;
        if (lane < subrow_count) {
            var winding: array<i32, 64>;
            let first_pixel = u32(clamp(path.x0 - i32(tile.x), 0, 8));
            let final_pixel = u32(clamp(path.x1 - i32(tile.x), 0, 8));
            let first_sample = first_pixel * samples;
            let final_sample = final_pixel * samples;
            for (var sx = first_sample; sx < final_sample; sx += 1u) {
                winding[sx] = 0i;
            }
            let py = f32(tile.y) + (f32(lane) + 0.5) / f32(samples);
            if (py >= f32(path.y0) && py < f32(path.y1)) {
                let base = path.geometry_base;
                let band_count = geometry[base + 1u];
                let offsets_base = geometry[base + 2u];
                let indices_base = geometry[base + 3u];
                let band = min(
                    u32(max(0.0, floor((py - f32(path.y0)) / 16.0))),
                    band_count - 1u,
                );
                let begin = geometry[base + offsets_base + band];
                let end = geometry[base + offsets_base + band + 1u];
                for (var reference = begin; reference < end; reference += 1u) {
                    let edge = geometry[base + indices_base + reference];
                    let x0 = edge_component(base, edge, 0u);
                    let y0 = edge_component(base, edge, 1u);
                    let x1 = edge_component(base, edge, 2u);
                    let y1 = edge_component(base, edge, 3u);
                    let upward = y0 <= py && y1 > py;
                    let downward = y1 <= py && y0 > py;
                    if (!upward && !downward) {
                        continue;
                    }
                    let crossing_x = x0 + (py - y0) * (x1 - x0) / (y1 - y0);
                    let delta = select(-1i, 1i, upward);
                    for (var sx = first_sample; sx < final_sample; sx += 1u) {
                        let px = f32(tile.x) + (f32(sx) + 0.5) / f32(samples);
                        if (crossing_x > px) {
                            winding[sx] += delta;
                        }
                    }
                }
                for (var sx = first_sample; sx < final_sample; sx += 1u) {
                    let inside = select(
                        winding[sx] != 0i,
                        (abs(winding[sx]) % 2i) != 0i,
                        path.even_odd != 0u,
                    );
                    if (inside) {
                        let pixel_x = sx / samples;
                        let pixel_y = lane / samples;
                        atomicAdd(&coverage[pixel_y * 8u + pixel_x], 1u);
                    }
                }
            }
        }
        workgroupBarrier();

        if (valid_pixel && x >= path.x0 && x < path.x1 && y >= path.y0 && y < path.y1) {
            let covered = atomicLoad(&coverage[lane]);
            if (covered != 0u) {
                let coverage_alpha = 255.0 * f32(covered) / f32(samples * samples);
                let alpha = f32(path.alpha) * coverage_alpha *
                    sample_clip(path, x, y) * sample_soft_mask(path, x, y) /
                    (255.0 * 255.0 * 255.0);
                let rgb = path_rgb(path);
                if (path.blend_mode == 0u) {
                    destination = source_over(rgb, alpha, destination);
                } else {
                    destination = blended_source_over(rgb, alpha, destination, path.blend_mode);
                }
            }
        }
        workgroupBarrier();
    }

    if (valid_pixel) {
        page[u32(y) * params.page_width + u32(x)] = destination;
    }
}
