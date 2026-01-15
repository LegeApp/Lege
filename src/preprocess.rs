// src/preprocess.rs

/// Transposes an image from HWC (Height, Width, Channel) to NCHW (Number, Channel, Height, Width) layout.
/// This implementation is for a single image (N=1).
pub fn hwc_to_nchw(src_data: &[f32], width: usize, height: usize, channels: usize) -> Vec<f32> {
    let mut dest = Vec::with_capacity(src_data.len());

    // Convert from HWC to NCHW format
    for ch in 0..channels {
        for y in 0..height {
            for x in 0..width {
                let src_idx = y * width * channels + x * channels + ch;
                dest.push(src_data[src_idx]);
            }
        }
    }
    dest
}
