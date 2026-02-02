//! A unified image encoding library supporting JPEG, JP2, JBIG2, and CCITT4 formats.
//!
//! This crate provides a single interface for encoding images in multiple formats,
//! with all operations performed in memory for integration into larger applications.

#![allow(missing_docs)]
#![allow(dead_code)] // TODO: Remove once fully implemented

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

pub mod prelude;

// Re-export debug logging functions
#[cfg(feature = "debug-logging")]
pub use streamline::{clear_debug_log, get_debug_log_messages, log_debug_message};

// Main encoding manager
pub mod streamline;

// Encoder modules
pub mod encoders;

// Color processing and binarization modules
pub mod color;

// Shared types
pub mod types;

// Custom image types (replaces image crate dependency)
pub mod image_types;

// Colorquant modules removed - were only used by deleted indexed8 encoder

/// Result type used throughout the crate
pub type Result<T> = std::result::Result<T, EncodingError>;

fn runtime_search_directories() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push_unique = |path: PathBuf| {
        if !dirs.iter().any(|p| p == &path) {
            dirs.push(path);
        }
    };

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(dir) = exe_path.parent() {
            push_unique(dir.to_path_buf());
            push_unique(dir.join("models"));
            push_unique(dir.join("tessdata"));

            if let Some(parent) = dir.parent() {
                push_unique(parent.to_path_buf());
                push_unique(parent.join("share/lege"));
                push_unique(parent.join("share/lege/models"));
                push_unique(parent.join("share/lege/tessdata"));
                push_unique(parent.join("lib/lege"));
            }
        }
    }

    for env_key in ["LEGE_DATA_DIR", "LEGE_ASSET_DIR"] {
        if let Ok(value) = std::env::var(env_key) {
            push_unique(PathBuf::from(value));
        }
    }

    if let Ok(ld_path) = std::env::var("LD_LIBRARY_PATH") {
        for segment in ld_path.split(':').filter(|s| !s.is_empty()) {
            push_unique(PathBuf::from(segment));
        }
    }

    for fallback in [
        "/usr/lib/lege",
        "/usr/local/lib/lege",
        "/usr/share/lege",
        "/usr/share/lege/models",
    ] {
        push_unique(PathBuf::from(fallback));
    }

    if let Ok(cwd) = std::env::current_dir() {
        push_unique(cwd);
    }

    push_unique(PathBuf::from("."));
    dirs
}

pub(crate) fn runtime_asset_path_if_exists(file_name: &str) -> Option<PathBuf> {
    runtime_search_directories()
        .into_iter()
        .map(|dir| dir.join(file_name))
        .find(|candidate| candidate.is_file())
}

pub(crate) fn runtime_asset_path(file_name: &str) -> PathBuf {
    runtime_asset_path_if_exists(file_name).unwrap_or_else(|| PathBuf::from(file_name))
}

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

pub mod jbig2;
pub use jbig2::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_conversion() {
        let err: EncodingError = "test error".into();
        assert!(matches!(err, EncodingError::EncoderError(_)));
    }
}
