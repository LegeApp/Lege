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

// Module declarations
pub mod jbig2;
pub mod jbig2arith;
#[cfg(feature = "symboldict")]
pub mod jbig2cc;
pub mod jbig2classify;
pub mod jbig2comparator;
pub mod jbig2context;
pub mod jbig2cost;
pub mod jbig2enc;
pub mod jbig2halftone;
pub mod jbig2shared;
pub mod jbig2structs;
pub mod jbig2sym;
pub mod jbig2unify;

// Re-export the main encode functions and config
pub use crate::jbig2arith::Jbig2ArithCoder;
#[cfg(feature = "symboldict")]
pub use jbig2cc::{BBox, CC, CCImage, Run, analyze_page, extract_symbols_for_jbig2};
pub use jbig2enc::{PdfSplitOutput, encode_document};
pub use jbig2structs::Jbig2Config;

use jbig2enc::Jbig2Encoder;
use jbig2sym::binary_pixels_to_bitimage;
use std::env;

/// Result of JBIG2 encoding with separate global and page data for PDF embedding
#[derive(Debug, Clone)]
pub struct Jbig2EncodeResult {
    /// Global dictionary data (if any) - should be stored as separate PDF object
    pub global_data: Option<Vec<u8>>,
    /// Page-specific data - the actual image stream
    pub page_data: Vec<u8>,
}

/// Context for JBIG2 encoding operations
#[derive(Debug, Clone)]
pub struct Jbig2Context {
    /// The underlying configuration
    config: Jbig2Config,
    pdf_mode: bool,
}

impl Default for Jbig2Context {
    fn default() -> Self {
        Self {
            config: Jbig2Config::default(),
            pdf_mode: false,
        }
    }
}

impl Jbig2Context {
    /// Create a new context with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new context with specified PDF mode
    pub fn with_pdf_mode(pdf_mode: bool) -> Self {
        Self {
            config: Jbig2Config::default(),
            pdf_mode,
        }
    }

    /// Create a new context with custom configuration
    pub fn with_config(config: Jbig2Config, pdf_mode: bool) -> Self {
        Self { config, pdf_mode }
    }

    /// Create a new context with lossless configuration (no symbol dictionaries)
    /// This is useful for PDF embedding when symbol dictionaries cause display issues
    pub fn with_lossless_config(pdf_mode: bool) -> Self {
        Self {
            config: Jbig2Config::lossless(),
            pdf_mode,
        }
    }

    /// Get the PDF mode setting
    pub fn get_pdf_mode(&self) -> bool {
        self.pdf_mode
    }

    /// Get the symbol mode setting
    pub fn get_symbol_mode(&self) -> bool {
        self.config.symbol_mode
    }

    /// Get the DPI setting
    pub fn get_dpi(&self) -> u32 {
        if self.config.generic.dpi == 0 {
            300
        } else {
            self.config.generic.dpi
        }
    }
}

/// Main encoding function that handles both standalone and PDF fragment modes
///
/// This function encodes a single binary image into JBIG2 format. When PDF mode is enabled,
/// it returns separate global dictionary and page data that can be properly embedded in PDF.
///
/// # Arguments
/// * `input` - Binary image data (0/1 values)
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `pdf_mode` - Whether to create PDF fragments (true) or standalone file (false)
///
/// # Returns
/// A `Jbig2EncodeResult` containing separate global and page data for PDF embedding,
/// or combined data for standalone files.
pub fn encode_single_image(
    input: &[u8],
    width: u32,
    height: u32,
    pdf_mode: bool,
) -> Result<Jbig2EncodeResult, Jbig2Error> {
    let bitimage = validate_and_build_bitimage(input, width, height)?;
    encode_single_bitimage(bitimage, Jbig2Context::with_pdf_mode(pdf_mode))
}

/// Encodes a single binary image into JBIG2 format with custom configuration.
///
/// This function allows specifying custom JBIG2 configurations for symbol encoding.
///
/// # Arguments
/// * `input` - Binary image data (0/1 values)
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `context` - JBIG2 encoding context with configuration
///
/// # Returns
/// A `Jbig2EncodeResult` containing separate global and page data for PDF embedding,
/// or combined data for standalone files.
pub fn encode_single_image_with_config(
    input: &[u8],
    width: u32,
    height: u32,
    context: Jbig2Context,
) -> Result<Jbig2EncodeResult, Jbig2Error> {
    let bitimage = validate_and_build_bitimage(input, width, height)?;
    encode_single_bitimage(bitimage, context)
}

/// Encodes a single binary image into JBIG2 format using lossless configuration.
///
/// This function forces symbol_mode = false to create standalone JBIG2 streams
/// without global dictionaries, which can resolve PDF display issues.
///
/// # Arguments
/// * `input` - Binary image data (0/1 values)
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `pdf_mode` - Whether to create PDF fragments (true) or standalone file (false)
///
/// # Returns
/// A `Jbig2EncodeResult` containing page data without global dictionary
pub fn encode_single_image_lossless(
    input: &[u8],
    width: u32,
    height: u32,
    pdf_mode: bool,
) -> Result<Jbig2EncodeResult, Jbig2Error> {
    let bitimage = validate_and_build_bitimage(input, width, height)?;
    encode_single_bitimage(bitimage, Jbig2Context::with_lossless_config(pdf_mode))
}

/// Encodes a multi-page document for PDF embedding, sharing one symbol
/// dictionary across every page.
///
/// PDF readers expect multi-page JBIG2 content as a single `JBIG2Globals`
/// object (the shared symbol dictionary) referenced by each page's separate
/// `/JBIG2Decode` stream. Producing that layout requires every page to be
/// planned and serialized by the *same* encoder instance so that the symbol
/// indices referenced by each page's text regions line up with the shared
/// dictionary's symbol table — building the dictionary and the page streams
/// from independent encoders silently desyncs those indices and produces
/// undecodable pages (solid black/white, decoder errors, etc.).
///
/// # Arguments
/// * `images` - the page images, in document order
/// * `config` - encoder configuration; `symbol_mode` controls whether a
///   shared global dictionary is produced at all (when disabled, every page
///   is encoded as an independent generic region and `global_segments` is
///   `None`)
///
/// # Returns
/// A [`PdfSplitOutput`] with the optional global dictionary segment plus one
/// page stream per input image, ready to embed directly.
pub fn encode_document_pdf_split(
    images: &[Array2<u8>],
    config: &Jbig2Config,
) -> Result<PdfSplitOutput, Jbig2Error> {
    let mut enc_config = config.clone();
    enc_config.want_full_headers = false;
    if !enc_config.symbol_mode {
        enc_config.refine = false;
        enc_config.text_refine = false;
    }

    let mut encoder = Jbig2Encoder::new(&enc_config);
    for image in images {
        encoder
            .add_page(image)
            .map_err(|e| Jbig2Error::EncodingFailed {
                message: e.to_string(),
            })?;
    }

    encoder
        .flush_pdf_split()
        .map_err(|e| Jbig2Error::EncodingFailed {
            message: e.to_string(),
        })
}

fn validate_and_build_bitimage(
    input: &[u8],
    width: u32,
    height: u32,
) -> Result<jbig2sym::BitImage, Jbig2Error> {
    let expected_len = width as usize * height as usize;
    if input.len() < expected_len {
        let packed_size = (width as usize * height as usize).div_ceil(8);
        if input.len() == packed_size {
            return Err(Jbig2Error::PackedDataDetected);
        }

        return Err(Jbig2Error::BufferSizeMismatch {
            expected: expected_len,
            actual: input.len(),
            width,
            height,
            ratio: input.len() as f64 / expected_len as f64,
        });
    }

    binary_pixels_to_bitimage(&input[..expected_len], width as usize, height as usize)
        .map_err(|message| Jbig2Error::EncodingFailed { message })
}

fn encode_single_bitimage(
    bitimage: jbig2sym::BitImage,
    ctx: Jbig2Context,
) -> Result<Jbig2EncodeResult, Jbig2Error> {
    let mut enc_config = ctx.config.clone();
    enc_config.want_full_headers = !ctx.get_pdf_mode();
    if !enc_config.symbol_mode {
        enc_config.refine = false;
        enc_config.text_refine = false;
    }

    let mut encoder = Jbig2Encoder::new(&enc_config);
    encoder
        .add_page_bitimage(bitimage)
        .map_err(|e| Jbig2Error::EncodingFailed {
            message: e.to_string(),
        })?;

    if ctx.get_pdf_mode() {
        // PDF embedding must use the same planning/serialization as tests/pdf_variants:
        // one encoder, flush_pdf_split() → globals stream + page stream with matching
        // segment references. The previous approach (dict_only().flush_dict() on one
        // encoder + flush() on a second) paired unrelated streams and broke decoders
        // (solid black/white pages, etc.).
        let split = encoder
            .flush_pdf_split()
            .map_err(|e| Jbig2Error::EncodingFailed {
                message: e.to_string(),
            })?;
        let n = split.page_streams.len();
        if n != 1 {
            return Err(Jbig2Error::StreamCountMismatch {
                expected: 1,
                actual: n,
            });
        }
        let page_data = split.page_streams.into_iter().next().unwrap();
        Ok(Jbig2EncodeResult {
            global_data: split.global_segments,
            page_data,
        })
    } else {
        let page_data = encoder.flush().map_err(|e| Jbig2Error::EncodingFailed {
            message: e.to_string(),
        })?;
        Ok(Jbig2EncodeResult {
            global_data: None,
            page_data,
        })
    }
}

/// Get the version string for the crate
pub fn get_version() -> String {
    let enc_version = option_env!("JBIG2ENC_VERSION").unwrap_or("unknown");
    format!(
        "jbig2-rs {}, jbig2enc {}",
        env!("CARGO_PKG_VERSION"),
        enc_version
    )
}

/// Get the build information string
pub fn get_build_info() -> String {
    let build_ts = option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or("unknown");
    let build_type = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!("{} (built with {})", build_ts, build_type)
}
