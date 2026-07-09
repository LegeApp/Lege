

// params layout (8 x u32):
//   [0] num_elems (this slice)
//   [1] inner_stride  [2] local_axis_size  [3] axis_offset  [4] total_axis_size
pub(crate) const CONCAT_SLICE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read>       src    : array<f32>;
@group(0) @binding(1) var<storage, read_write> dst    : array<f32>;
@group(0) @binding(2) var<storage, read>       params : array<u32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(num_workgroups) ng: vec3<u32>) {
    let i = gid.y * ng.x * 256u + gid.x;
    if i >= params[0] { return; }

    let inner_stride    = params[1];
    let local_axis_size = params[2];
    let axis_offset     = params[3];
    let total_axis      = params[4];

    let inner_idx      = i % inner_stride;
    let local_axis_idx = (i / inner_stride) % local_axis_size;
    let outer_idx      = i / (inner_stride * local_axis_size);

    let dst_idx = outer_idx * (total_axis * inner_stride)
                + (axis_offset + local_axis_idx) * inner_stride
                + inner_idx;
    dst[dst_idx] = src[i];
}
"#;

