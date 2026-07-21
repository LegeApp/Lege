// params:
// [0] len [1] rank
// [4..9] out_strides [10..15] input_strides [16..21] slope_strides
pub(crate) const PRELU_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       slope  : array<f32>;
@group(0) @binding(2) var<storage, read_write> out    : array<f32>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let rank = params[1];
    var rem = i;
    var in_idx = 0u;
    var slope_idx = 0u;
    for (var d = 0u; d < rank; d = d + 1u) {
        let coord = rem / params[4u + d];
        rem = rem % params[4u + d];
        in_idx = in_idx + coord * params[10u + d];
        slope_idx = slope_idx + coord * params[16u + d];
    }
    let x = inp[in_idx];
    out[i] = select(x, x * slope[slope_idx], x < 0.0);
}
"#;

// params:
// [0] len [1] rank [2] mode(0 constant, 1 reflect) [3] value_bits
// [4..9] out_shape [10..15] in_shape [16..21] pads_begin
// [22..27] out_strides [28..33] in_strides
pub(crate) const PAD_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

fn reflect_index(coord_raw: i32, dim: i32) -> i32 {
    if dim <= 1 { return 0; }
    var c = coord_raw;
    loop {
        if c < 0 {
            c = -c;
        } else if c >= dim {
            c = 2 * dim - 2 - c;
        } else {
            break;
        }
    }
    return c;
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let rank = params[1];
    let mode = params[2];
    let constant_value = bitcast<f32>(params[3]);
    var rem = i;
    var in_idx = 0u;
    var outside = false;
    for (var d = 0u; d < rank; d = d + 1u) {
        let coord = rem / params[22u + d];
        rem = rem % params[22u + d];
        let raw = i32(coord) - i32(params[16u + d]);
        let dim = i32(params[10u + d]);
        var in_coord = raw;
        if raw < 0 || raw >= dim {
            if mode == 1u {
                in_coord = reflect_index(raw, dim);
            } else {
                outside = true;
            }
        }
        in_idx = in_idx + u32(in_coord) * params[28u + d];
    }
    out[i] = select(inp[in_idx], constant_value, outside);
}
"#;

// params: [0] total [1] n [2] c [3] hin [4] win [5] hout [6] wout [7] align
pub(crate) const RESIZE_LINEAR_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

fn src_coord(o: u32, in_size: u32, out_size: u32, align: u32) -> f32 {
    if align != 0u {
        if out_size <= 1u { return 0.0; }
        return f32(o) * f32(in_size - 1u) / f32(out_size - 1u);
    }
    return (f32(o) + 0.5) * f32(in_size) / f32(out_size) - 0.5;
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let c = params[2];
    let hin = params[3];
    let win = params[4];
    let hout = params[5];
    let wout = params[6];
    let align = params[7];

    let ow = i % wout;
    let oh = (i / wout) % hout;
    let ch = (i / (hout * wout)) % c;
    let n = i / (c * hout * wout);

    let y = clamp(src_coord(oh, hin, hout, align), 0.0, f32(hin - 1u));
    let x = clamp(src_coord(ow, win, wout, align), 0.0, f32(win - 1u));
    let y0 = u32(floor(y));
    let x0 = u32(floor(x));
    let y1 = min(y0 + 1u, hin - 1u);
    let x1 = min(x0 + 1u, win - 1u);
    let wy = y - f32(y0);
    let wx = x - f32(x0);

    let base = ((n * c + ch) * hin) * win;
    let v00 = inp[base + y0 * win + x0];
    let v01 = inp[base + y0 * win + x1];
    let v10 = inp[base + y1 * win + x0];
    let v11 = inp[base + y1 * win + x1];
    let top = v00 * (1.0 - wx) + v01 * wx;
    let bot = v10 * (1.0 - wx) + v11 * wx;
    out[i] = top * (1.0 - wy) + bot * wy;
}
"#;

// params: [0] total [1] n [2] c [3] hin [4] win [5] hout [6] wout [7] align
pub(crate) const GRID_SAMPLE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read>       grid   : array<f32>;
@group(0) @binding(2) var<storage, read_write> out    : array<f32>;
@group(0) @binding(3) var<storage, read>       params : array<u32>;

fn unnormalize(v: f32, size: u32, align: u32) -> f32 {
    if align != 0u {
        return ((v + 1.0) * 0.5) * f32(size - 1u);
    }
    return ((v + 1.0) * f32(size) - 1.0) * 0.5;
}

fn sample(inp_idx_base: u32, hin: u32, win: u32, y: i32, x: i32) -> f32 {
    if y < 0 || x < 0 || y >= i32(hin) || x >= i32(win) {
        return 0.0;
    }
    return inp[inp_idx_base + u32(y) * win + u32(x)];
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }
    let c = params[2];
    let hin = params[3];
    let win = params[4];
    let hout = params[5];
    let wout = params[6];
    let align = params[7];

    let ow = i % wout;
    let oh = (i / wout) % hout;
    let ch = (i / (hout * wout)) % c;
    let n = i / (c * hout * wout);
    let grid_base = ((n * hout + oh) * wout + ow) * 2u;
    let x = unnormalize(grid[grid_base], win, align);
    let y = unnormalize(grid[grid_base + 1u], hin, align);

    let x0 = i32(floor(x));
    let y0 = i32(floor(y));
    let x1 = x0 + 1;
    let y1 = y0 + 1;
    let wx = x - f32(x0);
    let wy = y - f32(y0);
    let base = ((n * c + ch) * hin) * win;
    let v00 = sample(base, hin, win, y0, x0);
    let v01 = sample(base, hin, win, y0, x1);
    let v10 = sample(base, hin, win, y1, x0);
    let v11 = sample(base, hin, win, y1, x1);
    let top = v00 * (1.0 - wx) + v01 * wx;
    let bot = v10 * (1.0 - wx) + v11 * wx;
    out[i] = top * (1.0 - wy) + bot * wy;
}
"#;
