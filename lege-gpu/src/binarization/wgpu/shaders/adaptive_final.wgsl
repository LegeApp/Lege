// Final adaptive binarization pass.
// Reads precomputed integral images and background estimate from prior passes.
// Computes Sauvola (via O(1) integral lookup), background-normalized Otsu, AND-fuses them.
//
// Debug modes (params.debug_mode):
//   0 = fused binary (default, with invert_output)
//   1 = Sauvola binary only
//   2 = Otsu binary only
//   3 = background estimate (grayscale, no invert)
//   4 = normalized grayscale (no invert)
//   5 = local mean grayscale
//   6 = local stddev grayscale
//   7 = x-coordinate ramp (stride/transpose check)
//   8 = y-coordinate ramp
//   9 = all white (readback/pipeline check)

struct BinarizeParams {
    width: u32, height: u32, mode: u32, invert_output: u32,
    fixed_threshold: u32, sauvola_window: u32, bg_window: u32, otsu_threshold: u32,
    k_factor: f32, percentile_c: f32, padded_width: u32, padded_height: u32,
    integral_width: u32, sauvola_radius: u32, debug_mode: u32, _pad0: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read> gray_src: array<u32>; // 1 u32 per pixel (sRGB gray from linearize pass)
@group(0) @binding(2) var<storage, read> integral_buf: array<u32>;
@group(0) @binding(3) var<storage, read> integral_sq_buf: array<f32>;
@group(0) @binding(4) var<storage, read> bg_buf: array<u32>;
@group(0) @binding(5) var<storage, read_write> dst: array<u32>;

fn apply_invert(v: u32) -> u32 {
    if params.invert_output != 0u { return 255u - v; }
    return v;
}

// SAT box-sum for the u32 integral. Uses wrapping arithmetic; the final window
// sum is always small enough to be correct modulo 2^32.
fn integral_rect_u32(x0: u32, y0: u32, x1: u32, y1: u32) -> u32 {
    let iw = params.integral_width;
    let d = integral_buf[(y1 + 1u) * iw + (x1 + 1u)];
    let b = integral_buf[(y1 + 1u) * iw + x0];
    let c = integral_buf[y0 * iw + (x1 + 1u)];
    let a = integral_buf[y0 * iw + x0];
    return d + a - b - c;
}

fn integral_rect_f32(x0: u32, y0: u32, x1: u32, y1: u32) -> f32 {
    let iw = params.integral_width;
    let d = integral_sq_buf[(y1 + 1u) * iw + (x1 + 1u)];
    let b = integral_sq_buf[(y1 + 1u) * iw + x0];
    let c = integral_sq_buf[y0 * iw + (x1 + 1u)];
    let a = integral_sq_buf[y0 * iw + x0];
    return d + a - b - c;
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x = id.x;
    let y = id.y;
    if x >= params.width || y >= params.height { return; }

    let idx = y * params.width + x;

    // Cheap debug modes that skip algorithm work.
    if params.debug_mode == 9u { dst[idx] = 255u; return; }
    if params.debug_mode == 7u { dst[idx] = x & 255u; return; }
    if params.debug_mode == 8u { dst[idx] = y & 255u; return; }

    let g_u = gray_src[idx] & 255u;
    let g = f32(g_u);

    // Sauvola via integral images.
    // The padded image places original pixel (x,y) at padded (x+r, y+r).
    // The window [x, x+2r] x [y, y+2r] in padded coords maps directly to
    // integral lookup corners (x0=x, y0=y, x1=x+2r, y1=y+2r).
    let r = params.sauvola_radius;
    let x1 = x + 2u * r;
    let y1 = y + 2u * r;

    let sum_u = integral_rect_u32(x, y, x1, y1);
    let sum_sq = integral_rect_f32(x, y, x1, y1);
    let area = f32(params.sauvola_window * params.sauvola_window);
    let mean = f32(sum_u) / area;
    let variance = (sum_sq / area) - mean * mean;
    let stddev = sqrt(max(variance, 0.0));
    let sauvola_thr = mean * (1.0 + params.k_factor * (stddev / 127.0 - 1.0));
    let sauvola_bin = select(0u, 255u, g > sauvola_thr);

    // Background normalization + Otsu threshold.
    let bg = max(bg_buf[idx] & 255u, 1u);
    let normalized = u32(clamp(params.percentile_c / f32(bg) * g, 0.0, 255.0));
    let otsu_bin = select(0u, 255u, normalized > params.otsu_threshold);

    let fused = select(0u, 255u, sauvola_bin != 0u && otsu_bin != 0u);

    if params.debug_mode == 0u {
        dst[idx] = apply_invert(fused);
    } else if params.debug_mode == 1u {
        dst[idx] = apply_invert(sauvola_bin);
    } else if params.debug_mode == 2u {
        dst[idx] = apply_invert(otsu_bin);
    } else if params.debug_mode == 3u {
        dst[idx] = bg;
    } else if params.debug_mode == 4u {
        dst[idx] = normalized;
    } else if params.debug_mode == 5u {
        dst[idx] = u32(clamp(mean, 0.0, 255.0));
    } else if params.debug_mode == 6u {
        dst[idx] = u32(clamp(stddev, 0.0, 255.0));
    } else {
        dst[idx] = apply_invert(fused);
    }
}
