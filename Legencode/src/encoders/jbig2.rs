use crate::streamline::{Jbig2Mode, Jbig2Settings};
use crate::{EncodingError, Result};
use jbig2enc_rust::{
    Jbig2Config, Jbig2Context, Jbig2EncodeResult, encode_single_image_lossless,
    encode_single_image_with_config,
};
use std::borrow::Cow;

/// Normalize binary data from various formats to 0/1 per byte for JBIG2 encoding
///
/// Accepts:
/// - channels=0: Already normalized 0/1 data (pass through)
/// - channels=1: Grayscale 0/255 data (normalize to 0/1)
fn normalize_binary_data<'a>(
    input: &'a [u8],
    width: u32,
    height: u32,
    channels: u8,
) -> Result<Cow<'a, [u8]>> {
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| EncodingError::InvalidInput("Dimensions too large".to_string()))?;

    match channels {
        0 => {
            // Already normalized 0/1 data - verify and use as-is
            if input.len() < expected_len {
                return Err(EncodingError::InputBufferTooSmall);
            }
            Ok(Cow::Borrowed(&input[..expected_len]))
        }
        1 => {
            // Grayscale 0/255 data - normalize to 0/1
            if input.len() < expected_len {
                return Err(EncodingError::InputBufferTooSmall);
            }

            // Check if already normalized (all values are 0 or 1)
            let already_binary = input[..expected_len].iter().all(|&b| b <= 1);
            if already_binary {
                return Ok(Cow::Borrowed(&input[..expected_len]));
            }

            // Normalize: >128 = white (1), <=128 = black (0)
            Ok(Cow::Owned(
                input[..expected_len]
                    .iter()
                    .map(|&b| if b > 128 { 1 } else { 0 })
                    .collect(),
            ))
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
    
    // Debug: Log input characteristics
    log::debug!("JBIG2 encode: {}x{}, {} bytes, mode={:?}", width, height, normalized.len(), settings.mode);

    match settings.mode {
        Jbig2Mode::Generic => encode_single_image_lossless(
            normalized.as_ref(),
            width,
            height,
            settings.pdf_fragment_mode,
        )
        .map_err(|e: jbig2enc_rust::Jbig2Error| EncodingError::EncoderError(e.to_string())),
        Jbig2Mode::Symbol => encode_single_image_with_config(
            normalized.as_ref(),
            width,
            height,
            Jbig2Context::with_config(Jbig2Config::text(), settings.pdf_fragment_mode),
        )
        .map_err(|e: jbig2enc_rust::Jbig2Error| {
            log::error!("JBIG2 encoding failed: {:?}", e);
            EncodingError::EncoderError(e.to_string())
        }),
        Jbig2Mode::SymUnify => encode_single_image_with_config(
            normalized.as_ref(),
            width,
            height,
            Jbig2Context::with_config(Jbig2Config::text_symbol_unify(), settings.pdf_fragment_mode),
        )
        .map_err(|e: jbig2enc_rust::Jbig2Error| {
            log::error!("JBIG2 encoding failed: {:?}", e);
            EncodingError::EncoderError(e.to_string())
        }),
    }
}
