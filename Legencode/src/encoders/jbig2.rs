use crate::streamline::Jbig2Settings;
use crate::{EncodingError, Result};
use jbig2enc_rust::{encode_single_image, encode_single_image_lossless, Jbig2EncodeResult};

/// Normalize binary data from various formats to 0/1 per byte for JBIG2 encoding
/// 
/// Accepts:
/// - channels=0: Already normalized 0/1 data (pass through)
/// - channels=1: Grayscale 0/255 data (normalize to 0/1)
fn normalize_binary_data(input: &[u8], width: u32, height: u32, channels: u8) -> Result<Vec<u8>> {
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| EncodingError::InvalidInput("Dimensions too large".to_string()))?;
    
    match channels {
        0 => {
            // Already normalized 0/1 data - verify and use as-is
            if input.len() < expected_len {
                return Err(EncodingError::InputBufferTooSmall);
            }
            Ok(input[..expected_len].to_vec())
        }
        1 => {
            // Grayscale 0/255 data - normalize to 0/1
            if input.len() < expected_len {
                return Err(EncodingError::InputBufferTooSmall);
            }
            
            // Check if already normalized (all values are 0 or 1)
            let already_binary = input[..expected_len].iter().all(|&b| b <= 1);
            if already_binary {
                return Ok(input[..expected_len].to_vec());
            }
            
            // Normalize: >128 = white (1), <=128 = black (0)
            Ok(input[..expected_len]
                .iter()
                .map(|&b| if b > 128 { 1 } else { 0 })
                .collect())
        }
        _ => Err(EncodingError::UnsupportedChannels {
            format: "JBIG2",
            channels,
        }),
    }
}

pub fn encode(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    settings: &Jbig2Settings,
) -> Result<Jbig2EncodeResult> {
    // Normalize input data to 0/1 format required by jbig2enc_rust
    let normalized = normalize_binary_data(input, width, height, channels)?;

    // Choose encoding function based on symbol_mode setting
    if settings.symbol_mode {
        // Use normal symbol dictionaries (original behavior)
        encode_single_image(&normalized, width, height, settings.pdf_fragment_mode)
            .map_err(|e| EncodingError::EncoderError(e.to_string()))
    } else {
        // Use the lossless encoding function that creates proper JBIG2 segments
        // This includes end-of-page and end-of-file segments required for PDF embedding
        encode_single_image_lossless(&normalized, width, height, settings.pdf_fragment_mode)
            .map_err(|e| EncodingError::EncoderError(e.to_string()))
    }
}
