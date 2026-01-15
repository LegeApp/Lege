use crate::{streamline::JpegSettings, EncodingError, Result};
use toojpeg::{encode_jpeg, EncodeOptions, ImageFormat as TooJpegFormat};

pub fn encode(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    settings: &JpegSettings,
) -> Result<Vec<u8>> {
    if channels != 1 && channels != 3 {
        return Err(EncodingError::InvalidInput(
            "JPEG requires 1 (grayscale) or 3 (RGB) channels".to_string(),
        ));
    }
    let expected_len = width as usize * height as usize * channels as usize;
    if input.len() < expected_len {
        return Err(EncodingError::InvalidInput(
            "Input buffer too small for JPEG dimensions".to_string(),
        ));
    }

    let toojpeg_format = if channels == 1 {
        TooJpegFormat::Gray
    } else {
        TooJpegFormat::RGB
    };

    let options = EncodeOptions {
        width,
        height,
        format: toojpeg_format,
        quality: settings.quality,
        baseline: settings.baseline,
        optimized: settings.optimized,
        downsample: settings.downsample,
    };

    let mut output = Vec::new();
    encode_jpeg(input, options, &mut output)
        .map_err(|e| EncodingError::EncoderError(e.to_string()))?;
    Ok(output)
}
