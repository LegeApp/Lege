struct BinarizeParams {
    width: u32,
    height: u32,
    mode: u32,
    invert_output: u32,
    fixed_threshold: u32,
    sauvola_window: u32,
    bg_window: u32,
    otsu_threshold: u32,
    k_factor: f32,
    percentile_c: f32,
    pad0: u32,
    pad1: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read> src_pixels: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst_pixels: array<u32>;

fn reflect_101(idx: i32, len: i32) -> i32 {
    if (idx < 0) {
        return -idx - 1;
    }
    if (idx >= len) {
        return 2 * len - idx - 1;
    }
    return idx;
}

fn load_gray_reflect(x: i32, y: i32) -> u32 {
    let rx = reflect_101(x, i32(params.width));
    let ry = reflect_101(y, i32(params.height));
    let idx = u32(ry) * params.width + u32(rx);
    return src_pixels[idx] & 255u;
}

fn apply_output_invert(v: u32) -> u32 {
    if (params.invert_output != 0u) {
        return 255u - v;
    }
    return v;
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    if (x >= params.width || y >= params.height) {
        return;
    }

    let idx = y * params.width + x;
    let g_u = src_pixels[idx] & 255u;

    if (params.mode == 0u) {
        let out_fixed = select(0u, 255u, g_u > params.fixed_threshold);
        dst_pixels[idx] = apply_output_invert(out_fixed);
        return;
    }

    let g = f32(g_u);

    // 1. Direct Sauvola local stats
    let win = params.sauvola_window;
    let r = i32(win / 2u);

    var sum: f32 = 0.0;
    var sum_sq: f32 = 0.0;

    for (var dy = -r; dy <= r; dy = dy + 1) {
        for (var dx = -r; dx <= r; dx = dx + 1) {
            let v = f32(load_gray_reflect(i32(x) + dx, i32(y) + dy));
            sum = sum + v;
            sum_sq = sum_sq + v * v;
        }
    }

    let area = f32(win * win);
    let mean = sum / area;
    let var = (sum_sq / area) - mean * mean;
    let stddev = sqrt(max(var, 0.0));

    let sauvola_thr = mean * (1.0 + params.k_factor * (stddev / 127.0 - 1.0));
    let sauvola_bin = select(0u, 255u, g > sauvola_thr);

    // 2. Direct reflected square dilation (background max)
    let bg_win = params.bg_window;
    let br = i32(bg_win / 2u);
    var bg: u32 = 0u;

    for (var dy2 = -br; dy2 <= br; dy2 = dy2 + 1) {
        for (var dx2 = -br; dx2 <= br; dx2 = dx2 + 1) {
            let v2 = load_gray_reflect(i32(x) + dx2, i32(y) + dy2);
            bg = max(bg, v2);
        }
    }

    // 3. Background normalization
    let normalized_f = params.percentile_c / max(f32(bg), 0.000001) * g;
    let normalized = u32(clamp(normalized_f, 0.0, 255.0));

    // 4. Otsu threshold
    let otsu_bin = select(0u, 255u, normalized > params.otsu_threshold);

    // 5. AND fusion
    let fused = select(0u, 255u, sauvola_bin != 0u && otsu_bin != 0u);

    dst_pixels[idx] = apply_output_invert(fused);
}
