//! Shared tensor index utilities used across op implementations.

/// Standard C-order strides for a shape (product of all trailing dims).
pub(crate) fn c_strides_raw(shape: &[usize]) -> Vec<usize> {
    let rank = shape.len();
    let mut s = vec![1usize; rank];
    for d in (0..rank.saturating_sub(1)).rev() {
        s[d] = s[d + 1] * shape[d + 1];
    }
    s
}

/// C-order strides packed into a fixed [u32; 6] (extra slots set to 1).
pub(crate) fn c_strides_u32(shape: &[usize]) -> [u32; 6] {
    let raw = c_strides_raw(shape);
    let mut out = [1u32; 6];
    for (d, &s) in raw.iter().enumerate() {
        out[d] = s as u32;
    }
    out
}

/// Broadcast strides for `in_shape` broadcasting into `out_shape`.
/// Dims that are broadcast (size 1) get stride 0.
pub(crate) fn broadcast_strides_u32(out_shape: &[usize], in_shape: &[usize]) -> [u32; 6] {
    let rank = out_shape.len();
    let pad = rank - in_shape.len();
    let padded: Vec<usize> = (0..pad)
        .map(|_| 1usize)
        .chain(in_shape.iter().copied())
        .collect();
    let raw = c_strides_raw(&padded);
    let mut out = [0u32; 6];
    for d in 0..rank {
        out[d] = if padded[d] == 1 { 0 } else { raw[d] as u32 };
    }
    out
}

#[cfg(test)]
pub(crate) fn f32_from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytemuck::cast_slice(bytes).to_vec()
}

/// Max workgroups per dispatch dimension (wgpu/Vulkan guarantee).
const MAX_GROUPS_PER_DIM: u32 = 65535;

/// Splits a 1D workgroup count into a 2D grid `[gx, gy, 1]` that respects the
/// per-dimension limit. Flat kernels recover the linear group index with
/// `gid.y * num_workgroups.x * WG + gid.x` (WG = workgroup_size.x), so a slight
/// over-allocation on the last row is harmless given the `i >= n` guard.
pub(crate) fn linear_grid(groups: usize) -> (u32, u32, u32) {
    let groups = (groups as u32).max(1);
    if groups <= MAX_GROUPS_PER_DIM {
        (groups, 1, 1)
    } else {
        (MAX_GROUPS_PER_DIM, groups.div_ceil(MAX_GROUPS_PER_DIM), 1)
    }
}
