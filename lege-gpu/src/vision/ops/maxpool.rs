// params (14 x u32):
//   [0] num_elems  [1] channels  [2] hin  [3] win
//   [4] hout  [5] wout  [6] kh  [7] kw
//   [8] stride_h  [9] stride_w  [10] pad_top  [11] pad_left
//   [12] dilation_h  [13] dilation_w
pub(crate) const MAXPOOL2D_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }

    let hout = params[4];
    let wout = params[5];
    let ch   = i / (hout * wout);
    let oh   = (i / wout) % hout;
    let ow   = i % wout;

    let hin        = params[2];
    let win        = params[3];
    let kh         = params[6];
    let kw         = params[7];
    let stride_h   = params[8];
    let stride_w   = params[9];
    let pad_top    = params[10];
    let pad_left   = params[11];
    let dilation_h = params[12];
    let dilation_w = params[13];

    var max_val = -3.402823466e+38f;

    for (var ky: u32 = 0u; ky < kh; ky = ky + 1u) {
        let ih_raw = i32(oh) * i32(stride_h) + i32(ky) * i32(dilation_h) - i32(pad_top);
        if ih_raw < 0 || u32(ih_raw) >= hin { continue; }
        let ih = u32(ih_raw);
        for (var kx: u32 = 0u; kx < kw; kx = kx + 1u) {
            let iw_raw = i32(ow) * i32(stride_w) + i32(kx) * i32(dilation_w) - i32(pad_left);
            if iw_raw < 0 || u32(iw_raw) >= win { continue; }
            let iw = u32(iw_raw);
            let v = inp[ch * hin * win + ih * win + iw];
            if v > max_val { max_val = v; }
        }
    }
    out[i] = max_val;
}
"#;

// Same params layout as MaxPool. count_include_pad=0: average over the count of
// in-bounds window elements (matches the reference and ONNX default).
pub(crate) const AVGPOOL2D_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }

    let hout = params[4];
    let wout = params[5];
    let ch   = i / (hout * wout);
    let oh   = (i / wout) % hout;
    let ow   = i % wout;

    let hin        = params[2];
    let win        = params[3];
    let kh         = params[6];
    let kw         = params[7];
    let stride_h   = params[8];
    let stride_w   = params[9];
    let pad_top    = params[10];
    let pad_left   = params[11];
    let dilation_h = params[12];
    let dilation_w = params[13];

    var sum   = 0.0f;
    var count = 0u;

    for (var ky: u32 = 0u; ky < kh; ky = ky + 1u) {
        let ih_raw = i32(oh) * i32(stride_h) + i32(ky) * i32(dilation_h) - i32(pad_top);
        if ih_raw < 0 || u32(ih_raw) >= hin { continue; }
        let ih = u32(ih_raw);
        for (var kx: u32 = 0u; kx < kw; kx = kx + 1u) {
            let iw_raw = i32(ow) * i32(stride_w) + i32(kx) * i32(dilation_w) - i32(pad_left);
            if iw_raw < 0 || u32(iw_raw) >= win { continue; }
            let iw = u32(iw_raw);
            sum = sum + inp[ch * hin * win + ih * win + iw];
            count = count + 1u;
        }
    }
    out[i] = select(0.0f, sum / f32(count), count > 0u);
}
"#;
