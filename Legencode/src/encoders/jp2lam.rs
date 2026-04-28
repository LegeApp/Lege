//! JPEG 2000 encoder using jp2lam (pure Rust, no external C libraries).

use crate::EncodingError;

/// Encode RGB bytes as a JP2 file.
///
/// `data` must be interleaved sRGB, `width * height * 3` bytes.
/// `quality` is 0–100; 100 = lossless.
pub fn encode_rgb(
    data: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, EncodingError> {
    let image = jp2lam::Image::from_rgb_bytes(width, height, data)
        .map_err(|e| EncodingError::InvalidInput(e.to_string()))?;
    let options = jp2lam::EncodeOptions {
        quality,
        format: jp2lam::OutputFormat::Jp2,
    };
    jp2lam::encode(&image, &options).map_err(|e| EncodingError::EncoderError(e.to_string()))
}

/// Encode grayscale bytes as a JP2 file.
///
/// `data` must be 1 byte per pixel, `width * height` bytes.
/// `quality` is 0–100; 100 = lossless.
pub fn encode_gray(
    data: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, EncodingError> {
    let image = jp2lam::Image::from_gray_bytes(width, height, data)
        .map_err(|e| EncodingError::InvalidInput(e.to_string()))?;
    let options = jp2lam::EncodeOptions {
        quality,
        format: jp2lam::OutputFormat::Jp2,
    };
    jp2lam::encode(&image, &options).map_err(|e| EncodingError::EncoderError(e.to_string()))
}
