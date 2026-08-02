// Parallel column-wise accumulation completing the 2-D integral image.
// One 256-lane workgroup owns one column and scans it in 256-element tiles.

struct BinarizeParams {
    width: u32, height: u32, mode: u32, invert_output: u32,
    fixed_threshold: u32, sauvola_window: u32, bg_window: u32, otsu_threshold: u32,
    k_factor: f32, percentile_c: f32, padded_width: u32, padded_height: u32,
    integral_width: u32, sauvola_radius: u32, debug_mode: u32, _pad0: u32,
}

@group(0) @binding(0) var<uniform> params: BinarizeParams;
@group(0) @binding(1) var<storage, read> row_prefix_in: array<u32>;
@group(0) @binding(2) var<storage, read> row_prefix_sq_in: array<u32>;
@group(0) @binding(3) var<storage, read_write> integral_out: array<u32>;
@group(0) @binding(4) var<storage, read_write> integral_sq_out: array<u32>;

var<workgroup> scan_gray: array<u32, 256>;
var<workgroup> scan_sq: array<u32, 256>;
var<workgroup> carry_gray: u32;
var<workgroup> carry_sq: u32;

@compute @workgroup_size(256, 1, 1)
fn main(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(local_invocation_id) local: vec3<u32>,
) {
    let x = group.x;
    let lane = local.x;
    if x >= params.integral_width { return; }

    if lane == 0u {
        integral_out[x] = 0u;
        integral_sq_out[x] = 0u;
        carry_gray = 0u;
        carry_sq = 0u;
    }
    workgroupBarrier();

    var tile = 0u;
    loop {
        if tile >= params.padded_height { break; }
        let y = tile + lane;
        if y < params.padded_height {
            let src = y * params.integral_width + x;
            scan_gray[lane] = row_prefix_in[src];
            scan_sq[lane] = row_prefix_sq_in[src];
        } else {
            scan_gray[lane] = 0u;
            scan_sq[lane] = 0u;
        }
        workgroupBarrier();

        var offset = 1u;
        loop {
            if offset >= 256u { break; }
            var add_gray = 0u;
            var add_sq = 0u;
            if lane >= offset {
                add_gray = scan_gray[lane - offset];
                add_sq = scan_sq[lane - offset];
            }
            workgroupBarrier();
            scan_gray[lane] = scan_gray[lane] + add_gray;
            scan_sq[lane] = scan_sq[lane] + add_sq;
            workgroupBarrier();
            offset = offset * 2u;
        }

        if y < params.padded_height {
            integral_out[(y + 1u) * params.integral_width + x] =
                carry_gray + scan_gray[lane];
            integral_sq_out[(y + 1u) * params.integral_width + x] =
                carry_sq + scan_sq[lane];
        }
        workgroupBarrier();
        if lane == 0u {
            let remaining = params.padded_height - tile;
            let last = min(remaining, 256u) - 1u;
            carry_gray = carry_gray + scan_gray[last];
            carry_sq = carry_sq + scan_sq[last];
        }
        workgroupBarrier();
        tile = tile + 256u;
    }
}
