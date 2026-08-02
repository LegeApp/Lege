// params (8 x u32):
//   [0] num_out  [1] N*C planes  [2] hin  [3] win  [4] hout  [5] wout
//   [6] scale_h f32 bits  [7] scale_w f32 bits
pub(crate) const RESIZE_NEAREST_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       inp    : array<f32>;
@group(0) @binding(1) var<storage, read_write> out    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= params[0] { return; }

    let hout = params[4];
    let wout = params[5];
    let plane = i / (hout * wout);
    let oh   = (i / wout) % hout;
    let ow   = i % wout;

    let hin = params[2];
    let win = params[3];

    // ONNX asymmetric coordinates use the model's declared scales, not the
    // effective input/output ratio after the output dimension was floored.
    let scale_h = bitcast<f32>(params[6]);
    let scale_w = bitcast<f32>(params[7]);
    let ih = min(u32(floor(f32(oh) / scale_h)), hin - 1u);
    let iw = min(u32(floor(f32(ow) / scale_w)), win - 1u);

    out[i] = inp[plane * hin * win + ih * win + iw];
}
"#;
