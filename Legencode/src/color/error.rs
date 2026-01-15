//! Error types for color processing operations

use thiserror::Error;

/// Color processing error types
#[derive(Error, Debug)]
pub enum ColorOpsError {
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Processing error: {0}")]
    ProcessingError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Result type for color operations
pub type Result<T> = std::result::Result<T, ColorOpsError>;
