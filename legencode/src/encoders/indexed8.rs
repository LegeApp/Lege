//! Encoder for 8-bit indexed (paletted) images with multiple quantization options.

use crate::colorquant_integration::{quantize_color_to_indexed8, quantize_grayscale_to_indexed8};
use crate::{EncodingError, Result};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use imagequant::Attributes as ImagequantAttributes;
use std::io::Write;

/// Quantization method for indexed8 encoding
#[derive(Debug, Clone, Copy)]
pub enum QuantizationMethod {
    /// Use imagequant library (default, good quality)
    Imagequant,
    /// Use optimized NeuQuant for color images
    NeuQuantColor {
        sample_factor: i32,
        max_colors: usize,
    },
    /// Use optimized NeuQuant for grayscale images  
    NeuQuantGray {
        sample_factor: i32,
        max_colors: usize,
    },
}

impl Default for QuantizationMethod {
    fn default() -> Self {
        // Use imagequant as the default - it's faster and higher quality for most use cases
        QuantizationMethod::Imagequant
    }
}

/// Encodes an RGB, RGBA, or grayscale image into an 8-bit indexed format.
///
/// This format uses a 256-color palette and is suitable for embedding in PDFs.
/// The output is a single `Vec<u8>` containing the 768-byte palette followed by the pixel data.
///
/// # Arguments
/// * `input` - The raw image data (grayscale, RGB, or RGBA).
/// * `width` - Image width in pixels.
/// * `height` - Image height in pixels.
/// * `channels` - Number of channels (1 for grayscale, 3 for RGB, or 4 for RGBA).
///
/// # Returns
/// A `Result` containing the combined palette and pixel data or an `EncodingError`.
pub fn encode(input: &[u8], width: u32, height: u32, channels: u8) -> Result<Vec<u8>> {
    if channels != 1 && channels != 3 && channels != 4 {
        return Err(EncodingError::UnsupportedChannels {
            format: "Indexed8",
            channels,
        });
    }
    if width == 0 || height == 0 {
        return Err(EncodingError::InvalidDimensions {
            format: "Indexed8",
            width,
            height,
        });
    }
    let expected_len = width as usize * height as usize * channels as usize;
    if input.len() != expected_len {
        return Err(EncodingError::InputBufferTooSmall);
    }

    // Handle grayscale input
    if channels == 1 {
        // Use optimized grayscale quantization with default parameters
        return quantize_grayscale_to_indexed8(
            input, width, height, 256, // max_colors
            1,   // sample_factor
        );
    }

    // Convert input pixels to imagequant's RGBA format for color images
    let pixels_rgba: Vec<imagequant::RGBA> = if channels == 3 {
        input
            .chunks_exact(3)
            .map(|c| imagequant::RGBA {
                r: c[0],
                g: c[1],
                b: c[2],
                a: 255,
            })
            .collect()
    } else {
        input
            .chunks_exact(4)
            .map(|c| imagequant::RGBA {
                r: c[0],
                g: c[1],
                b: c[2],
                a: c[3],
            })
            .collect()
    };

    // Configure imagequant to generate a 256-color palette.
    let mut liq = ImagequantAttributes::new();
    liq.set_max_colors(256)
        .map_err(|e| EncodingError::Quantization(format!("set_max_colors failed: {:?}", e)))?;

    let mut img_liq = liq
        .new_image(pixels_rgba, width as usize, height as usize, 0.0)
        .map_err(|e| EncodingError::Quantization(format!("new_image failed: {:?}", e)))?;

    let mut res = liq
        .quantize(&mut img_liq)
        .map_err(|e| EncodingError::Quantization(format!("quantize failed: {:?}", e)))?;

    let (palette_rgba, indexed_pixel_data) = res
        .remapped(&mut img_liq)
        .map_err(|e| EncodingError::Quantization(format!("remapped failed: {:?}", e)))?;

    // Convert the RGBA palette to an RGB byte vector.
    let mut palette_bytes_rgb = Vec::with_capacity(palette_rgba.len() * 3);
    for color in palette_rgba {
        palette_bytes_rgb.push(color.r);
        palette_bytes_rgb.push(color.g);
        palette_bytes_rgb.push(color.b);
    }

    // The PDF specification for Indexed color spaces requires a full 256-entry palette.
    // We pad the palette with black if imagequant returns fewer than 256 colors.
    if palette_bytes_rgb.len() < 256 * 3 {
        palette_bytes_rgb.resize(256 * 3, 0); // Pad with black
    }

    // Compress pixel data using Flate (Zlib) at maximum level
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(&indexed_pixel_data)
        .map_err(|e| EncodingError::Compression(e.to_string()))?;
    let compressed_pixel_data = encoder
        .finish()
        .map_err(|e| EncodingError::Compression(e.to_string()))?;

    // Combine the palette and compressed pixel data into a single buffer.
    // The calling context (e.g., PDF writer) must know that the pixel data is compressed
    // and that the first 768 bytes are the palette.
    let mut output = Vec::with_capacity(palette_bytes_rgb.len() + compressed_pixel_data.len());
    output.extend_from_slice(&palette_bytes_rgb);
    output.extend_from_slice(&compressed_pixel_data);

    Ok(output)
}

/// Encodes an RGB or RGBA image into an 8-bit indexed format with optimized quantization.
///
/// This format uses a 256-color palette and is suitable for embedding in PDFs.
/// The output is a single `Vec<u8>` containing the 768-byte palette followed by the pixel data.
///
/// **Recommendation:** Use `QuantizationMethod::Imagequant` for best performance and quality
/// with color images. For grayscale content, consider JPEG encoding instead as indexed8
/// provides no space savings for grayscale (still uses RGB palette internally).
///
/// # Arguments
/// * `input` - The raw RGB or RGBA image data.
/// * `width` - Image width in pixels.
/// * `height` - Image height in pixels.
/// * `channels` - Number of channels (must be 3 for RGB or 4 for RGBA).
/// * `method` - Quantization method to use.
///
/// # Returns
/// A `Result` containing the combined palette and pixel data or an `EncodingError`.
pub fn encode_with_method(
    input: &[u8],
    width: u32,
    height: u32,
    channels: u8,
    method: QuantizationMethod,
) -> Result<Vec<u8>> {
    match method {
        QuantizationMethod::Imagequant => {
            // Use the original imagequant implementation
            encode(input, width, height, channels)
        }
        QuantizationMethod::NeuQuantColor {
            sample_factor,
            max_colors,
        } => {
            // Use optimized NeuQuant for color
            quantize_color_to_indexed8(input, width, height, channels, max_colors, sample_factor)
        }
        QuantizationMethod::NeuQuantGray {
            sample_factor,
            max_colors,
        } => {
            // Convert to grayscale first, then quantize
            if channels != 3 && channels != 4 {
                return Err(EncodingError::UnsupportedChannels {
                    format: "NeuQuantGray",
                    channels,
                });
            }

            // Convert RGB/RGBA to grayscale
            let mut grayscale = Vec::with_capacity((width * height) as usize);
            for chunk in input.chunks_exact(channels as usize) {
                // Use standard luminance formula
                let gray =
                    (0.299 * chunk[0] as f32 + 0.587 * chunk[1] as f32 + 0.114 * chunk[2] as f32)
                        .round() as u8;
                grayscale.push(gray);
            }

            quantize_grayscale_to_indexed8(&grayscale, width, height, max_colors, sample_factor)
        }
    }
}
