// params (6 x u32):
//   [0] num_out  [1] channels  [2] hin  [3] win  [4] hout  [5] wout
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
    let ch   = i / (hout * wout);
    let oh   = (i / wout) % hout;
    let ow   = i % wout;

    let hin = params[2];
    let win = params[3];

    // Asymmetric: floor(oh * hin / hout). Integer division gives floor.
    let ih = oh * hin / hout;
    let iw = ow * win / wout;

    out[i] = inp[ch * hin * win + ih * win + iw];
}
"#;
