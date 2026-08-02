//! JBIG2 Encoder in Rust
//!
//! This crate provides functionality to encode binary images into the JBIG2 format.
//! It supports both standalone JBIG2 files and PDF-embedded fragments with proper
//! global dictionary handling.

#![allow(missing_docs)]
#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_mut)]

// Re-export commonly used types
pub use ndarray::Array2;

/// Errors that can occur during JBIG2 encoding
#[derive(Debug)]
pub enum Jbig2Error {
    /// Input buffer size mismatch
    BufferSizeMismatch {
        expected: usize,
        actual: usize,
        width: u32,
        height: u32,
        ratio: f64,
    },

    /// Buffer too small for operation
    BufferTooSmall { expected: usize, actual: usize },

    /// Array shape error during conversion
    ArrayShapeError { source: ndarray::ShapeError },

    /// Encoding failed
    EncodingFailed { message: String },

    /// Dictionary creation failed
    DictionaryFailed { message: String },

    /// Packed binary data detected when unpacked expected
    PackedDataDetected,

    /// Stream count mismatch
    StreamCountMismatch { expected: usize, actual: usize },

    /// Segment writing failed
    SegmentWriteFailed {
        segment_type: String,
        message: String,
    },
}

impl std::fmt::Display for Jbig2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Jbig2Error::BufferSizeMismatch {
                expected,
                actual,
                width,
                height,
                ratio,
            } => {
                write!(
                    f,
                    "Input buffer size mismatch: expected {}, got {} for {}x{} image (ratio: {:.3})",
                    expected, actual, width, height, ratio
                )
            }
            Jbig2Error::BufferTooSmall { expected, actual } => {
                write!(f, "Buffer too small: expected {}, got {}", expected, actual)
            }
            Jbig2Error::ArrayShapeError { source } => {
                write!(f, "Array shape error: {}", source)
            }
            Jbig2Error::EncodingFailed { message } => {
                write!(f, "Encoding failed: {}", message)
            }
            Jbig2Error::DictionaryFailed { message } => {
                write!(f, "Dictionary creation failed: {}", message)
            }
            Jbig2Error::PackedDataDetected => {
                write!(
                    f,
                    "Input appears to be packed binary data (1 bit per pixel), but JBIG2 encoder expects unpacked data (1 byte per pixel)"
                )
            }
            Jbig2Error::StreamCountMismatch { expected, actual } => {
                write!(
                    f,
                    "Expected {} stream(s) for single image encoding, got {}",
                    expected, actual
                )
            }
            Jbig2Error::SegmentWriteFailed {
                segment_type,
                message,
            } => {
                write!(f, "Failed to write {} segment: {}", segment_type, message)
            }
        }
    }
}

impl std::error::Error for Jbig2Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Jbig2Error::ArrayShapeError { source } => Some(source),
            _ => None,
        }
    }
}

impl From<ndarray::ShapeError> for Jbig2Error {
    fn from(source: ndarray::ShapeError) -> Self {
        Jbig2Error::ArrayShapeError { source }
    }
}

pub use crate::jbig2arith::Jbig2ArithCoder;
#[cfg(feature = "symboldict")]
pub use crate::jbig2cc::{BBox, CC, CCImage, Run, analyze_page, extract_symbols_for_jbig2};
pub use crate::jbig2enc::{PdfSplitOutput, encode_document};
pub use crate::jbig2structs::Jbig2Config;

use crate::jbig2enc::Jbig2Encoder;
use log::info;
use std::env;

const JBIG2_THRESHOLD_DEF: f32 = 0.92;
const JBIG2_WEIGHT_DEF: f32 = 0.5;

#[derive(Debug, Clone)]
pub struct Jbig2PdfSplitResult {
    pub global_segments: Option<Vec<u8>>,
    pub page_streams: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Jbig2Context {
    config: Jbig2Config,
    threshold: f32,
    weight: f32,
    pdf_mode: bool,
}

impl Default for Jbig2Context {
    fn default() -> Self {
        Self {
            config: Jbig2Config::default(),
            threshold: JBIG2_THRESHOLD_DEF,
            weight: JBIG2_WEIGHT_DEF,
            pdf_mode: false,
        }
    }
}

impl Jbig2Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_pdf_mode(pdf_mode: bool) -> Self {
        Self {
            config: Jbig2Config::default(),
            threshold: JBIG2_THRESHOLD_DEF,
            weight: JBIG2_WEIGHT_DEF,
            pdf_mode,
        }
    }

    pub fn with_config(config: Jbig2Config, pdf_mode: bool) -> Self {
        Self {
            config,
            threshold: JBIG2_THRESHOLD_DEF,
            weight: JBIG2_WEIGHT_DEF,
            pdf_mode,
        }
    }

    pub fn with_lossless_config(pdf_mode: bool) -> Self {
        Self {
            config: Jbig2Config::lossless(),
            threshold: JBIG2_THRESHOLD_DEF,
            weight: JBIG2_WEIGHT_DEF,
            pdf_mode,
        }
    }

    pub fn get_pdf_mode(&self) -> bool {
        self.pdf_mode
    }

    pub fn get_symbol_mode(&self) -> bool {
        self.config.symbol_mode
    }

    pub fn get_dpi(&self) -> u32 {
        if self.config.generic.dpi == 0 {
            300
        } else {
            self.config.generic.dpi
        }
    }
}

pub fn encode_single_image(
    input: &[u8],
    width: u32,
    height: u32,
    pdf_mode: bool,
) -> Result<Jbig2PdfSplitResult, Jbig2Error> {
    let bitimage = validate_and_build_bitimage(input, width, height)?;
    encode_single_bitimage(bitimage, Jbig2Context::with_pdf_mode(pdf_mode))
}

pub fn encode_single_image_lossless(
    input: &[u8],
    width: u32,
    height: u32,
    pdf_mode: bool,
) -> Result<Jbig2PdfSplitResult, Jbig2Error> {
    let bitimage = validate_and_build_bitimage(input, width, height)?;
    encode_single_bitimage(bitimage, Jbig2Context::with_lossless_config(pdf_mode))
}

pub fn encode_single_image_with_config(
    input: &[u8],
    width: u32,
    height: u32,
    context: Jbig2Context,
) -> Result<Jbig2PdfSplitResult, Jbig2Error> {
    let bitimage = validate_and_build_bitimage(input, width, height)?;
    encode_single_bitimage(bitimage, context)
}

fn validate_and_build_bitimage(
    input: &[u8],
    width: u32,
    height: u32,
) -> Result<crate::jbig2sym::BitImage, Jbig2Error> {
    let expected_len = (width as usize) * (height as usize);
    if input.len() != expected_len {
        let ratio = if expected_len > 0 {
            input.len() as f64 / expected_len as f64
        } else {
            0.0
        };
        return Err(Jbig2Error::BufferSizeMismatch {
            expected: expected_len,
            actual: input.len(),
            width,
            height,
            ratio,
        });
    }

    let looks_packed = width > 0
        && height > 0
        && input.len() == (height as usize) * (width as usize).div_ceil(8)
        && input.iter().any(|&b| b != 0 && b != 1);
    if looks_packed {
        return Err(Jbig2Error::PackedDataDetected);
    }

    crate::jbig2sym::binary_pixels_to_bitimage(input, width as usize, height as usize)
        .map_err(|message| Jbig2Error::EncodingFailed { message })
}

fn encode_single_bitimage(
    bitimage: crate::jbig2sym::BitImage,
    context: Jbig2Context,
) -> Result<Jbig2PdfSplitResult, Jbig2Error> {
    let mut config = context.config.clone();
    config.want_full_headers = !context.pdf_mode;
    if !config.symbol_mode {
        config.refine = false;
        config.text_refine = false;
    }

    let mut encoder = Jbig2Encoder::new(&config);
    encoder
        .add_page_bitimage(bitimage)
        .map_err(|e| Jbig2Error::EncodingFailed {
            message: e.to_string(),
        })?;

    if context.pdf_mode {
        let split = encoder
            .flush_pdf_split()
            .map_err(|e| Jbig2Error::EncodingFailed {
                message: e.to_string(),
            })?;

        Ok(Jbig2PdfSplitResult {
            global_segments: split.global_segments,
            page_streams: split.page_streams,
        })
    } else {
        let output = encoder.flush().map_err(|e| Jbig2Error::EncodingFailed {
            message: e.to_string(),
        })?;
        Ok(Jbig2PdfSplitResult {
            global_segments: None,
            page_streams: vec![output],
        })
    }
}

pub fn init_logging() {
    let default_level = if cfg!(debug_assertions) {
        "debug"
    } else {
        "info"
    };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .format_timestamp_millis()
        .init();

    info!(
        "JBIG2 library logging initialized. RUST_LOG={}",
        env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string())
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_pdf_api_matches_canonical_single_pass_api() {
        let pixels: Vec<u8> = (0..32 * 16)
            .map(|i| u8::from(i % 11 == 0 || i % 17 == 0))
            .collect();

        let legacy = super::encode_single_image(&pixels, 32, 16, true).unwrap();
        let canonical = crate::encode_single_image(&pixels, 32, 16, true).unwrap();

        assert_eq!(legacy.global_segments, canonical.global_data);
        assert_eq!(legacy.page_streams, vec![canonical.page_data]);
    }
}
