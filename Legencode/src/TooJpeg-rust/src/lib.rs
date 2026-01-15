//! A Rust port of the TooJpeg JPEG encoder with performance optimizations.
//!
//! This library provides a simple interface for encoding RGB(A) images to JPEG format
//! with various quality and optimization settings.

#![allow(missing_docs)]
#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

mod toojpeg;

pub use toojpeg::{write_jpeg, BitCode, BitWriter, I16, I32, U16, U8};

/// Image format options for the JPEG encoder
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// RGB format (3 bytes per pixel)
    RGB,
    /// RGBA format (4 bytes per pixel, alpha is ignored)
    RGBA,
    /// Grayscale format (1 byte per pixel)
    Gray,
}

/// JPEG encoding options
#[derive(Debug, Clone, Copy)]
pub struct EncodeOptions {
    /// Image width in pixels
    pub width: u32,
    /// Image height in pixels
    pub height: u32,
    /// Image format (RGB, RGBA, or Grayscale)
    pub format: ImageFormat,
    /// Quality from 1 (worst) to 100 (best)
    pub quality: u8,
    /// Whether to use baseline DCT encoding (true) or progressive (false)
    pub baseline: bool,
    /// Whether to use optimized Huffman tables
    pub optimized: bool,
    /// Whether to downsample chroma channels (4:2:0 subsampling)
    pub downsample: bool,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            format: ImageFormat::RGB,
            quality: 90,
            baseline: true,
            optimized: true,
            downsample: true,
        }
    }
}

/// Encode an image to JPEG format
///
/// # Arguments
/// * `pixels` - The image pixel data in the format specified by `options.format`
/// * `options` - Encoding options including dimensions, format, and quality
/// * `output` - A writer that implements `std::io::Write` to receive the JPEG data
///
/// # Returns
/// `Result<(), &'static str>` indicating success or an error message
pub fn encode_jpeg<W: std::io::Write>(
    pixels: &[u8],
    options: EncodeOptions,
    output: &mut W,
) -> Result<(), &'static str> {
    // Input validation
    let bytes_per_pixel = match options.format {
        ImageFormat::RGB => 3,
        ImageFormat::RGBA => 4,
        ImageFormat::Gray => 1,
    };

    let expected_len = (options.width * options.height * bytes_per_pixel) as usize;
    if pixels.len() < expected_len {
        return Err("Input buffer too small for specified dimensions and format");
    }

    // Input validation
    let bytes_per_pixel = match options.format {
        ImageFormat::RGB => 3,
        ImageFormat::RGBA => 4,
        ImageFormat::Gray => 1,
    };

    let expected_len = (options.width as usize)
        .checked_mul(options.height as usize)
        .and_then(|x| x.checked_mul(bytes_per_pixel));

    if expected_len.map_or(true, |len| pixels.len() < len) {
        return Err("Input buffer too small for specified dimensions and format");
    }

    // Convert to the format expected by write_jpeg
    let is_rgb = matches!(options.format, ImageFormat::RGB | ImageFormat::RGBA);
    let quality = options.quality.clamp(1, 100) as u8;

    // Create a BitWriter for the output
    let mut writer = BitWriter::new(|byte| {
        output
            .write_all(&[byte])
            .map_err(|_| "Failed to write output")
    });

    // Call the low-level write_jpeg function
    write_jpeg(
        &mut writer,
        pixels,
        options.width as u16,
        options.height as u16,
        is_rgb,
        quality,
        options.downsample,
        None, // comment
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_encode_rgb() {
        // Create a simple 2x2 RGB image (red, green, blue, white)
        let pixels = vec![
            255, 0, 0, // Red
            0, 255, 0, // Green
            0, 0, 255, // Blue
            255, 255, 255, // White
        ];

        let options = EncodeOptions {
            width: 2,
            height: 2,
            format: ImageFormat::RGB,
            quality: 90,
            ..Default::default()
        };

        let mut output = Vec::new();
        encode_jpeg(&pixels, options, &mut output).unwrap();

        // Basic validation of JPEG output
        assert!(output.len() > 100); // Should be at least 100 bytes
        assert_eq!(&output[0..2], [0xFF, 0xD8]); // JPEG SOI marker
    }
}
