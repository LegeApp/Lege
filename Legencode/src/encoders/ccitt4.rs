use crate::{EncodingError, Result};
use fax::{encoder::Encoder, slice_bits, Color, VecWriter};

/// Normalize and bit-pack binary data for CCITT4 encoding
///
/// Accepts:
/// - channels=0: Already bit-packed PBM data (pass through)
/// - channels=1: Grayscale 0/255 data (normalize to 0/1, then bit-pack)
fn prepare_pbm_data(input: &[u8], width: u32, height: u32, channels: u8) -> Result<Vec<u8>> {
    let bytes_per_row = ((width as usize) + 7) / 8;
    let expected_len = bytes_per_row
        .checked_mul(height as usize)
        .ok_or_else(|| EncodingError::InvalidInput("Dimensions too large".to_string()))?;

    match channels {
        0 => {
            // Already bit-packed PBM data - verify size and use as-is
            if input.len() < expected_len {
                return Err(EncodingError::InvalidInput(format!(
                    "Input buffer too small for CCITT4 dimensions: have {}, need {}",
                    input.len(),
                    expected_len
                )));
            }
            Ok(input[..expected_len].to_vec())
        }
        1 => {
            // Grayscale 0/255 data - normalize and bit-pack
            let pixel_count = (width as usize)
                .checked_mul(height as usize)
                .ok_or_else(|| EncodingError::InvalidInput("Dimensions too large".to_string()))?;

            if input.len() < pixel_count {
                return Err(EncodingError::InputBufferTooSmall);
            }

            // Allocate bit-packed buffer
            let mut pbm_data = vec![0u8; expected_len];

            // Normalize and bit-pack: >128 = white (0), <=128 = black (1)
            for y in 0..(height as usize) {
                let row_offset = y * bytes_per_row;
                for x in 0..(width as usize) {
                    let pixel_idx = y * (width as usize) + x;
                    let is_black = input[pixel_idx] <= 128;

                    if is_black {
                        let byte_index = row_offset + x / 8;
                        let bit_index = 7 - (x % 8);
                        pbm_data[byte_index] |= 1 << bit_index;
                    }
                }
            }

            Ok(pbm_data)
        }
        _ => Err(EncodingError::UnsupportedChannels {
            format: "CCITT4",
            channels,
        }),
    }
}

// Encode CCITT Group 4 from binary data
// Accepts:
// - channels=0: Pre-packed PBM data (1bpp, bit=1 => black, bit=0 => white)
// - channels=1: Grayscale binary data (0/255 per byte, will normalize and bit-pack)
pub fn encode(input: &[u8], width: u32, height: u32, channels: u8) -> Result<Vec<u8>> {
    // Normalize and bit-pack input data
    let pbm_data = prepare_pbm_data(input, width, height, channels)?;
    
    let bytes_per_row = ((width as usize) + 7) / 8;
    let writer = VecWriter::new();
    let mut encoder = Encoder::new(writer);

    for y in 0..height as usize {
        let start = y * bytes_per_row;
        let line = &pbm_data[start..start + bytes_per_row];

        // Map each bit to Color across actual width (ignore padding bits on the right)
        let colors = slice_bits(line).take(width as usize).map(|bit| {
            if bit {
                Color::Black
            } else {
                Color::White
            }
        });

        encoder
            .encode_line(colors, width as u16)
            .map_err(|e| EncodingError::EncoderError(e.to_string()))?;
    }

    let encoded_writer = encoder
        .finish()
        .map_err(|e| EncodingError::EncoderError(e.to_string()))?;
    Ok(encoded_writer.finish())
}
