struct Params {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    format: u32,
    filter_mode: u32,
    p0: u32,
    p1: u32,
    p2: u32,
    p3: u32,
    p4: u32,
    p5: u32,
    p6: u32,
    p7: u32,
    p8: u32,
    p9: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> src_pixels: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst_pixels: array<u32>;
@group(0) @binding(3) var<storage, read> tone_lut: array<u32>;
@group(0) @binding(4) var<storage, read_write> aux: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read_write> resize_mid: array<u32>;

fn channel(pixel: u32, index: u32) -> u32 {
    return (pixel >> (index * 8u)) & 255u;
}

fn pack_rgba(r: u32, g: u32, b: u32, a: u32) -> u32 {
    return r | (g << 8u) | (b << 16u) | (a << 24u);
}

fn workgroups_index(gid: vec3<u32>) -> u32 {
    return gid.x;
}

@compute @workgroup_size(256)
fn crop(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.dst_width * params.dst_height;
    if index >= count {
        return;
    }
    let x = index % params.dst_width;
    let y = index / params.dst_width;
    let source = (y + params.p1) * params.src_width + x + params.p0;
    dst_pixels[index] = src_pixels[source];
}

@compute @workgroup_size(256)
fn to_gray(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.src_width * params.src_height;
    if index >= count {
        return;
    }
    if params.format == 1u {
        dst_pixels[index] = src_pixels[index] & 255u;
        return;
    }
    let pixel = src_pixels[index];
    let alpha = channel(pixel, 3u);
    let paper = 255u - alpha;
    let r = channel(pixel, 0u) + paper;
    let g = channel(pixel, 1u) + paper;
    let b = channel(pixel, 2u) + paper;
    var gray: u32;
    if params.p0 != 0u {
        gray = (r + g + b + 1u) / 3u;
    } else {
        gray = (2126u * r + 7152u * g + 722u * b + 5000u) / 10000u;
    }
    dst_pixels[index] = min(gray, 255u);
}

@compute @workgroup_size(256)
fn tone(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.src_width * params.src_height;
    if index >= count {
        return;
    }
    let pixel = src_pixels[index];
    if params.format == 1u {
        dst_pixels[index] = tone_lut[pixel & 255u];
        return;
    }
    let alpha = channel(pixel, 3u);
    if alpha == 0u {
        dst_pixels[index] = pixel;
        return;
    }
    var mapped: array<u32, 3>;
    for (var c = 0u; c < 3u; c = c + 1u) {
        let straight = min((channel(pixel, c) * 255u + alpha / 2u) / alpha, 255u);
        mapped[c] = (tone_lut[straight] * alpha + 127u) / 255u;
    }
    dst_pixels[index] = pack_rgba(mapped[0], mapped[1], mapped[2], alpha);
}

fn kernel_support(filter_mode: u32) -> f32 {
    switch filter_mode {
        case 1u: { return 0.5; }
        case 2u: { return 1.0; }
        case 3u: { return 2.0; }
        case 4u: { return 3.0; }
        default: { return 0.5; }
    }
}

fn kernel_eval(filter_mode: u32, raw_t: f32) -> f32 {
    let t = abs(raw_t);
    switch filter_mode {
        case 1u: {
            return select(0.0, 1.0, t <= 0.5);
        }
        case 2u: {
            return max(1.0 - t, 0.0);
        }
        case 3u: {
            if t < 1.0 {
                return 1.5 * t * t * t - 2.5 * t * t + 1.0;
            }
            if t < 2.0 {
                return -0.5 * t * t * t + 2.5 * t * t - 4.0 * t + 2.0;
            }
            return 0.0;
        }
        case 4u: {
            if t < 0.000001 {
                return 1.0;
            }
            if t < 3.0 {
                let pt = 3.14159265358979323846 * t;
                return 3.0 * (sin(pt) / pt) * (sin(pt / 3.0) / (pt / 3.0));
            }
            return 0.0;
        }
        default: {
            return select(0.0, 1.0, t <= 0.5);
        }
    }
}

fn nearest_index(dst: u32, src_len: u32, dst_len: u32) -> u32 {
    let center = (f32(dst) + 0.5) * f32(src_len) / f32(dst_len);
    return min(u32(center), src_len - 1u);
}

@compute @workgroup_size(256)
fn resize_nearest(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.dst_width * params.dst_height;
    if index >= count {
        return;
    }
    let x = index % params.dst_width;
    let y = index / params.dst_width;
    let sx = nearest_index(x, params.src_width, params.dst_width);
    let sy = nearest_index(y, params.src_height, params.dst_height);
    dst_pixels[index] = src_pixels[sy * params.src_width + sx];
}

@compute @workgroup_size(256)
fn resize_horizontal(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.dst_width * params.src_height;
    if index >= count {
        return;
    }
    let x = index % params.dst_width;
    let y = index / params.dst_width;
    let ratio = f32(params.src_width) / f32(params.dst_width);
    let stretch = max(ratio, 1.0);
    let support = kernel_support(params.filter_mode) * stretch;
    let center = (f32(x) + 0.5) * ratio;
    let overlap = params.filter_mode == 1u;
    var lo: i32;
    var hi: i32;
    if overlap {
        lo = i32(floor(center - support));
        hi = i32(ceil(center + support)) - 1;
    } else {
        lo = i32(floor(center - support + 0.5));
        hi = i32(ceil(center + support - 0.5));
    }
    lo = clamp(lo, 0, i32(params.src_width) - 1);
    hi = clamp(hi, lo, i32(params.src_width) - 1);
    var accum = vec4<f32>(0.0);
    var weight_sum = 0.0;
    for (var source_x = lo; source_x <= hi; source_x = source_x + 1) {
        var weight: f32;
        if overlap {
            let left = max(center - support, f32(source_x));
            let right = min(center + support, f32(source_x) + 1.0);
            weight = max(right - left, 0.0);
        } else {
            weight = kernel_eval(
                params.filter_mode,
                (f32(source_x) + 0.5 - center) / stretch,
            );
        }
        let pixel = src_pixels[y * params.src_width + u32(source_x)];
        accum.x = accum.x + weight * f32(channel(pixel, 0u));
        if params.format == 0u {
            accum.y = accum.y + weight * f32(channel(pixel, 1u));
            accum.z = accum.z + weight * f32(channel(pixel, 2u));
            accum.w = accum.w + weight * f32(channel(pixel, 3u));
        }
        weight_sum = weight_sum + weight;
    }
    if weight_sum <= 0.00000011920929 {
        let source_x = min(u32(round(center - 0.5)), params.src_width - 1u);
        let pixel = src_pixels[y * params.src_width + source_x];
        accum = vec4<f32>(
            f32(channel(pixel, 0u)),
            f32(channel(pixel, 1u)),
            f32(channel(pixel, 2u)),
            f32(channel(pixel, 3u)),
        );
        weight_sum = 1.0;
    }
    let base = index * 4u;
    resize_mid[base] = bitcast<u32>(accum.x / weight_sum);
    resize_mid[base + 1u] = bitcast<u32>(accum.y / weight_sum);
    resize_mid[base + 2u] = bitcast<u32>(accum.z / weight_sum);
    resize_mid[base + 3u] = bitcast<u32>(accum.w / weight_sum);
}

@compute @workgroup_size(256)
fn resize_vertical(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.dst_width * params.dst_height;
    if index >= count {
        return;
    }
    let x = index % params.dst_width;
    let y = index / params.dst_width;
    let ratio = f32(params.src_height) / f32(params.dst_height);
    let stretch = max(ratio, 1.0);
    let support = kernel_support(params.filter_mode) * stretch;
    let center = (f32(y) + 0.5) * ratio;
    let overlap = params.filter_mode == 1u;
    var lo: i32;
    var hi: i32;
    if overlap {
        lo = i32(floor(center - support));
        hi = i32(ceil(center + support)) - 1;
    } else {
        lo = i32(floor(center - support + 0.5));
        hi = i32(ceil(center + support - 0.5));
    }
    lo = clamp(lo, 0, i32(params.src_height) - 1);
    hi = clamp(hi, lo, i32(params.src_height) - 1);
    var accum = vec4<f32>(0.0);
    var weight_sum = 0.0;
    for (var source_y = lo; source_y <= hi; source_y = source_y + 1) {
        var weight: f32;
        if overlap {
            let left = max(center - support, f32(source_y));
            let right = min(center + support, f32(source_y) + 1.0);
            weight = max(right - left, 0.0);
        } else {
            weight = kernel_eval(
                params.filter_mode,
                (f32(source_y) + 0.5 - center) / stretch,
            );
        }
        let base = (u32(source_y) * params.dst_width + x) * 4u;
        accum.x = accum.x + weight * bitcast<f32>(resize_mid[base]);
        accum.y = accum.y + weight * bitcast<f32>(resize_mid[base + 1u]);
        accum.z = accum.z + weight * bitcast<f32>(resize_mid[base + 2u]);
        accum.w = accum.w + weight * bitcast<f32>(resize_mid[base + 3u]);
        weight_sum = weight_sum + weight;
    }
    var out = vec4<u32>(
        u32(clamp(floor(accum.x / weight_sum + 0.5), 0.0, 255.0)),
        u32(clamp(floor(accum.y / weight_sum + 0.5), 0.0, 255.0)),
        u32(clamp(floor(accum.z / weight_sum + 0.5), 0.0, 255.0)),
        u32(clamp(floor(accum.w / weight_sum + 0.5), 0.0, 255.0)),
    );
    if params.format == 0u {
        out.x = min(out.x, out.w);
        out.y = min(out.y, out.w);
        out.z = min(out.z, out.w);
        dst_pixels[index] = pack_rgba(out.x, out.y, out.z, out.w);
    } else {
        dst_pixels[index] = out.x;
    }
}

@compute @workgroup_size(256)
fn otsu_histogram(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let total = params.src_width * params.src_height;
    if index < total {
        let value = src_pixels[index] & 255u;
        atomicAdd(&aux[value], 1u);
    }
}

fn find_otsu_threshold() -> u32 {
    let total = params.src_width * params.src_height;
    if total == 0u {
        return 127u;
    }
    var sum_all = 0.0;
    for (var bin = 0u; bin < 256u; bin = bin + 1u) {
        sum_all = sum_all + f32(bin) * f32(atomicLoad(&aux[bin]));
    }
    var weight0 = 0u;
    var sum0 = 0.0;
    var best_threshold = 127u;
    var best_variance = -1.0;
    for (var threshold = 0u; threshold < 255u; threshold = threshold + 1u) {
        let count = atomicLoad(&aux[threshold]);
        weight0 = weight0 + count;
        if weight0 == 0u {
            continue;
        }
        let weight1 = total - weight0;
        if weight1 == 0u {
            break;
        }
        sum0 = sum0 + f32(threshold * count);
        let mean0 = sum0 / f32(weight0);
        let mean1 = (sum_all - sum0) / f32(weight1);
        let delta = mean0 - mean1;
        let variance = f32(weight0) * f32(weight1) * delta * delta;
        if variance > best_variance {
            best_variance = variance;
            best_threshold = threshold;
        }
    }
    return best_threshold;
}

@compute @workgroup_size(1)
fn otsu_find(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x == 0u {
        atomicStore(&aux[256], find_otsu_threshold());
    }
}

@compute @workgroup_size(256)
fn otsu_apply(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.src_width * params.src_height;
    if index >= count {
        return;
    }
    dst_pixels[index] = select(
        0u,
        255u,
        (src_pixels[index] & 255u) > atomicLoad(&aux[256]),
    );
}

fn add_u64(left: vec2<u32>, right: vec2<u32>) -> vec2<u32> {
    let low = left.x + right.x;
    let carry = select(0u, 1u, low < left.x);
    return vec2<u32>(low, left.y + right.y + carry);
}

fn sub_u64(left: vec2<u32>, right: vec2<u32>) -> vec2<u32> {
    let borrow = select(0u, 1u, left.x < right.x);
    return vec2<u32>(left.x - right.x, left.y - right.y - borrow);
}

fn integral_value(x: u32, y: u32, lane: u32) -> vec2<u32> {
    let base = (y * params.src_width + x) * 4u + lane;
    return vec2<u32>(resize_mid[base], resize_mid[base + 1u]);
}

fn integral_rectangle(
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
    lane: u32,
) -> vec2<u32> {
    var value = integral_value(x1, y1, lane);
    if x0 > 0u && y0 > 0u {
        value = add_u64(value, integral_value(x0 - 1u, y0 - 1u, lane));
    }
    if x0 > 0u {
        value = sub_u64(value, integral_value(x0 - 1u, y1, lane));
    }
    if y0 > 0u {
        value = sub_u64(value, integral_value(x1, y0 - 1u, lane));
    }
    return value;
}

fn u64_to_f32(value: vec2<u32>) -> f32 {
    return f32(value.y) * 4294967296.0 + f32(value.x);
}

@compute @workgroup_size(256)
fn integral_rows(@builtin(global_invocation_id) gid: vec3<u32>) {
    let y = workgroups_index(gid);
    if y >= params.src_height {
        return;
    }
    var sum = vec2<u32>(0u);
    var square_sum = vec2<u32>(0u);
    for (var x = 0u; x < params.src_width; x = x + 1u) {
        let value = src_pixels[y * params.src_width + x] & 255u;
        sum = add_u64(sum, vec2<u32>(value, 0u));
        square_sum = add_u64(square_sum, vec2<u32>(value * value, 0u));
        let base = (y * params.src_width + x) * 4u;
        resize_mid[base] = sum.x;
        resize_mid[base + 1u] = sum.y;
        resize_mid[base + 2u] = square_sum.x;
        resize_mid[base + 3u] = square_sum.y;
    }
}

@compute @workgroup_size(256)
fn integral_columns(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = workgroups_index(gid);
    if x >= params.src_width {
        return;
    }
    var sum = vec2<u32>(0u);
    var square_sum = vec2<u32>(0u);
    for (var y = 0u; y < params.src_height; y = y + 1u) {
        let base = (y * params.src_width + x) * 4u;
        sum = add_u64(sum, vec2<u32>(resize_mid[base], resize_mid[base + 1u]));
        square_sum = add_u64(
            square_sum,
            vec2<u32>(resize_mid[base + 2u], resize_mid[base + 3u]),
        );
        resize_mid[base] = sum.x;
        resize_mid[base + 1u] = sum.y;
        resize_mid[base + 2u] = square_sum.x;
        resize_mid[base + 3u] = square_sum.y;
    }
}

fn local_sauvola_threshold(index: u32) -> f32 {
    let x = index % params.src_width;
    let y = index / params.src_width;
    let window = max(params.p0, 3u) | 1u;
    let half = window / 2u;
    let x0 = select(0u, x - half, x >= half);
    let y0 = select(0u, y - half, y >= half);
    let x1 = min(x + half, params.src_width - 1u);
    let y1 = min(y + half, params.src_height - 1u);
    let count = (x1 - x0 + 1u) * (y1 - y0 + 1u);
    let sum = u64_to_f32(integral_rectangle(x0, x1, y0, y1, 0u));
    let square_sum = u64_to_f32(integral_rectangle(x0, x1, y0, y1, 2u));
    let mean = sum / f32(count);
    let variance = max(square_sum / f32(count) - mean * mean, 0.0);
    let deviation = sqrt(variance);
    let k = bitcast<f32>(params.p1);
    return mean * (1.0 + k * (deviation / 128.0 - 1.0));
}

@compute @workgroup_size(256)
fn sauvola(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.src_width * params.src_height;
    if index >= count {
        return;
    }
    let threshold = local_sauvola_threshold(index);
    dst_pixels[index] = select(
        0u,
        255u,
        f32(src_pixels[index] & 255u) > threshold,
    );
}

@compute @workgroup_size(256)
fn fuse_thresholds(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.src_width * params.src_height;
    if index >= count {
        return;
    }
    let local_threshold = local_sauvola_threshold(index);
    let global_weight = bitcast<f32>(params.p2);
    let threshold =
        global_weight * f32(atomicLoad(&aux[256])) +
        (1.0 - global_weight) * local_threshold;
    dst_pixels[index] = select(
        0u,
        255u,
        f32(src_pixels[index] & 255u) > threshold,
    );
}

const BAYER4: array<u32, 16> = array<u32, 16>(
    0u, 8u, 2u, 10u,
    12u, 4u, 14u, 6u,
    3u, 11u, 1u, 9u,
    15u, 7u, 13u, 5u,
);

@compute @workgroup_size(256)
fn dither(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = workgroups_index(gid);
    let count = params.src_width * params.src_height;
    if index >= count {
        return;
    }
    let value = src_pixels[index] & 255u;
    if params.p0 == 0u {
        dst_pixels[index] = select(0u, 255u, value >= 128u);
        return;
    }
    let x = index % params.src_width;
    let y = index / params.src_width;
    let matrix_value = BAYER4[(y & 3u) * 4u + (x & 3u)];
    let threshold = ((matrix_value * 2u + 1u) * 255u) / 32u;
    dst_pixels[index] = select(0u, 255u, value > threshold);
}

@compute @workgroup_size(1)
fn floyd_steinberg(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x != 0u {
        return;
    }
    let row_slots = params.src_width + 2u;
    var current_base = 0u;
    var next_base = row_slots;
    for (var y = 0u; y < params.src_height; y = y + 1u) {
        for (var x = 0u; x < params.src_width; x = x + 1u) {
            let index = y * params.src_width + x;
            let slot = x + 1u;
            let corrected =
                i32(src_pixels[index] & 255u) +
                bitcast<i32>(atomicLoad(&aux[current_base + slot])) / 16;
            let output = select(0, 255, corrected >= 128);
            dst_pixels[index] = u32(output);
            let error = corrected - output;
            let right = current_base + slot + 1u;
            atomicStore(
                &aux[right],
                bitcast<u32>(bitcast<i32>(atomicLoad(&aux[right])) + error * 7),
            );
            let down_left = next_base + slot - 1u;
            atomicStore(
                &aux[down_left],
                bitcast<u32>(bitcast<i32>(atomicLoad(&aux[down_left])) + error * 3),
            );
            let down = next_base + slot;
            atomicStore(
                &aux[down],
                bitcast<u32>(bitcast<i32>(atomicLoad(&aux[down])) + error * 5),
            );
            let down_right = next_base + slot + 1u;
            atomicStore(
                &aux[down_right],
                bitcast<u32>(bitcast<i32>(atomicLoad(&aux[down_right])) + error),
            );
        }
        let old_current = current_base;
        current_base = next_base;
        next_base = old_current;
        for (var slot = 0u; slot < row_slots; slot = slot + 1u) {
            atomicStore(&aux[next_base + slot], 0u);
        }
    }
}

fn packed_byte(byte_index: u32) -> u32 {
    let stride = (params.src_width + 7u) / 8u;
    let row = byte_index / stride;
    let byte_in_row = byte_index % stride;
    var output = 0u;
    for (var bit = 0u; bit < 8u; bit = bit + 1u) {
        let x = byte_in_row * 8u + bit;
        if x < params.src_width {
            let value = src_pixels[row * params.src_width + x] & 255u;
            if value < 128u {
                output = output | (128u >> bit);
            }
        }
    }
    return output;
}

@compute @workgroup_size(256)
fn pack_monochrome(@builtin(global_invocation_id) gid: vec3<u32>) {
    let word_index = workgroups_index(gid);
    let stride = (params.src_width + 7u) / 8u;
    let byte_count = stride * params.src_height;
    let word_count = (byte_count + 3u) / 4u;
    if word_index >= word_count {
        return;
    }
    var output = 0u;
    for (var byte = 0u; byte < 4u; byte = byte + 1u) {
        let byte_index = word_index * 4u + byte;
        if byte_index < byte_count {
            output = output | (packed_byte(byte_index) << (byte * 8u));
        }
    }
    dst_pixels[word_index] = output;
}
