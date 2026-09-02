//! Lege's in-process JPEG, JP2, JBIG2, and CCITT4 encoding support.
//!
//! This crate provides a single interface for encoding images in multiple formats,
//! with all operations performed in memory for integration into larger applications.

use std::error::Error;
use std::fmt;
pub mod prelude;

// Main encoding manager
pub mod streamline;

// Raster text -> per-document glyph font (the `glyphfont` text format).
pub mod glyphfont;
pub mod vectorize;

// Encoder modules
pub mod encoders;

#[cfg(feature = "jp2-lam")]
pub mod jp2;
#[cfg(not(feature = "jp2-lam"))]
pub mod jp2 {
    use super::{EncodingError, Result};

    pub fn encode_gray_document(
        _data: &[u8],
        _width: u32,
        _height: u32,
        _quality: u8,
    ) -> Result<Vec<u8>> {
        Err(EncodingError::EncoderError(
            "JPEG2000 support requires the jp2-lam feature".to_string(),
        ))
    }
}

// JPEG encoder adapter (vstroebel/jpeg-encoder).
pub mod jpeg;

// Colorquant modules removed - were only used by deleted indexed8 encoder

/// Result type used throughout the crate
pub type Result<T> = std::result::Result<T, EncodingError>;

/// Unified error type for all encoding operations
#[derive(Debug)]
pub enum EncodingError {
    /// Invalid input parameters
    InvalidInput(String),
    /// Encoder-specific error
    EncoderError(String),
    /// I/O related error
    IoError(String),
    /// Memory allocation error
    MemoryError(String),
    /// Error during image quantization
    Quantization(String),
    /// Error during data compression
    Compression(String),
    /// Input buffer is too small for the specified dimensions
    InputBufferTooSmall,
    /// Invalid image dimensions for a given format
    InvalidDimensions {
        format: &'static str,
        width: u32,
        height: u32,
    },
    /// Unsupported number of channels for a given format
    UnsupportedChannels { format: &'static str, channels: u8 },
}

impl fmt::Display for EncodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodingError::InvalidInput(msg) => write!(f, "Invalid input: {}", msg),
            EncodingError::EncoderError(msg) => write!(f, "Encoder error: {}", msg),
            EncodingError::IoError(msg) => write!(f, "I/O error: {}", msg),
            EncodingError::MemoryError(msg) => write!(f, "Memory error: {}", msg),
            EncodingError::Quantization(msg) => write!(f, "Quantization error: {}", msg),
            EncodingError::Compression(msg) => write!(f, "Compression error: {}", msg),
            EncodingError::InputBufferTooSmall => {
                write!(f, "Input buffer is too small for the specified dimensions")
            }
            EncodingError::InvalidDimensions {
                format,
                width,
                height,
            } => {
                write!(f, "Invalid dimensions for {}: {}x{}", format, width, height)
            }
            EncodingError::UnsupportedChannels { format, channels } => {
                write!(
                    f,
                    "Unsupported number of channels for {}: {}. Expected 3 (RGB) or 4 (RGBA).",
                    format, channels
                )
            }
        }
    }
}

impl Error for EncodingError {}

impl From<Box<dyn Error>> for EncodingError {
    fn from(err: Box<dyn Error>) -> Self {
        EncodingError::EncoderError(err.to_string())
    }
}

impl From<std::io::Error> for EncodingError {
    fn from(err: std::io::Error) -> Self {
        EncodingError::IoError(err.to_string())
    }
}

impl From<&'static str> for EncodingError {
    fn from(err: &'static str) -> Self {
        EncodingError::EncoderError(err.to_string())
    }
}

impl From<String> for EncodingError {
    fn from(err: String) -> Self {
        EncodingError::EncoderError(err)
    }
}

impl From<anyhow::Error> for EncodingError {
    fn from(err: anyhow::Error) -> Self {
        EncodingError::EncoderError(err.to_string())
    }
}

/// Diagnostic string for the active JBIG2 encoder acceleration paths.
pub fn active_jbig2_backend_info() -> String {
    "jbig2enc-rust (symbol dictionary + parallel)".to_string()
}

pub use streamline::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_conversion() {
        let err: EncodingError = "test error".into();
        assert!(matches!(err, EncodingError::EncoderError(_)));
    }
}
