//! JPEG encoding through vstroebel's `jpeg-encoder` crate.
//!
//! The decoder retains two standard DCT lookup tables here because its IDCT
//! implementation uses them directly.

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

/// Zigzag position to natural (row-major) position.
pub(crate) const ZIGZAG_INV: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// AAN DCT per-frequency scale factors used by the decoder's dequantization.
pub(crate) const AAN_SCALE_FACTORS: [f32; 8] = [
    1.0,
    1.387039845,
    1.306562965,
    1.175875602,
    1.0,
    0.785694958,
    0.541196100,
    0.275899379,
];

/// Encode RGB or grayscale pixels as a baseline JPEG.
pub fn write_jpeg(
    pixels: &[u8],
    width: u16,
    height: u16,
    is_rgb: bool,
    quality: u8,
    downsample: bool,
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut encoder = Encoder::new(&mut output, quality.clamp(1, 100));
    encoder.set_sampling_factor(if downsample && is_rgb {
        SamplingFactor::F_2_2
    } else {
        SamplingFactor::F_1_1
    });
    encoder
        .encode(
            pixels,
            width,
            height,
            if is_rgb {
                ColorType::Rgb
            } else {
                ColorType::Luma
            },
        )
        .map_err(|error| error.to_string())?;
    Ok(output)
}
