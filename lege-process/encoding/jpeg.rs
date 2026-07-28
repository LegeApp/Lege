//! Adapter for the `jpeg-encoder` crate used by Lege's encoding pipeline.

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use std::io::Write;

/// Input pixel layout for JPEG encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Rgb,
    Rgba,
    Gray,
}

/// JPEG encoding options.
#[derive(Debug, Clone, Copy)]
pub struct EncodeOptions {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub quality: u8,
    /// Encode a baseline stream when true, or a progressive stream when false.
    pub baseline: bool,
    /// Generate image-specific optimized Huffman tables.
    pub optimized: bool,
    /// Use 4:2:0 chroma subsampling for color images.
    pub downsample: bool,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            format: ImageFormat::Rgb,
            quality: 90,
            baseline: true,
            optimized: true,
            downsample: true,
        }
    }
}

/// Encode an image with vstroebel's `jpeg-encoder`.
pub fn encode_jpeg<W: Write>(
    pixels: &[u8],
    options: EncodeOptions,
    output: &mut W,
) -> Result<(), String> {
    let width =
        u16::try_from(options.width).map_err(|_| "JPEG width exceeds 65535 pixels".to_string())?;
    let height = u16::try_from(options.height)
        .map_err(|_| "JPEG height exceeds 65535 pixels".to_string())?;
    let color_type = match options.format {
        ImageFormat::Rgb => ColorType::Rgb,
        ImageFormat::Rgba => ColorType::Rgba,
        ImageFormat::Gray => ColorType::Luma,
    };

    let mut encoder = Encoder::new(output, options.quality.clamp(1, 100));
    encoder.set_progressive(!options.baseline);
    encoder.set_optimized_huffman_tables(options.optimized);
    encoder.set_sampling_factor(
        if options.downsample && options.format != ImageFormat::Gray {
            SamplingFactor::F_2_2
        } else {
            SamplingFactor::F_1_1
        },
    );
    encoder
        .encode(pixels, width, height, color_type)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_position(jpeg: &[u8], marker: u8) -> Option<usize> {
        jpeg.windows(2).position(|bytes| bytes == [0xff, marker])
    }

    fn encode_rgb(baseline: bool, downsample: bool) -> Vec<u8> {
        let pixels = vec![128; 16 * 16 * 3];
        let mut output = Vec::new();
        encode_jpeg(
            &pixels,
            EncodeOptions {
                width: 16,
                height: 16,
                format: ImageFormat::Rgb,
                quality: 80,
                baseline,
                optimized: true,
                downsample,
            },
            &mut output,
        )
        .unwrap();
        output
    }

    #[test]
    fn baseline_and_progressive_modes_are_forwarded() {
        let baseline = encode_rgb(true, true);
        assert!(marker_position(&baseline, 0xc0).is_some());
        assert!(marker_position(&baseline, 0xc2).is_none());

        let progressive = encode_rgb(false, true);
        assert!(marker_position(&progressive, 0xc2).is_some());
    }

    #[test]
    fn chroma_sampling_mode_is_forwarded() {
        let sampling_byte = |jpeg: &[u8]| {
            let sof = marker_position(jpeg, 0xc0).unwrap();
            jpeg[sof + 11]
        };
        assert_eq!(sampling_byte(&encode_rgb(true, false)), 0x11);
        assert_eq!(sampling_byte(&encode_rgb(true, true)), 0x22);
    }
}
