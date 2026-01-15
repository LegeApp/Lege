// src/error.rs
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ResizeError {
    #[error("Inconsistent image dimensions or format in batch")]
    InconsistentBatch,
    #[error("Input buffer is empty")]
    EmptyInput,
    #[error("Unsupported image format")]
    UnsupportedFormat,
    #[error("Invalid dimensions: width and height must be greater than 0")]
    InvalidDimensions,
    #[error("Backend not available: {0}")]
    BackendNotAvailable(&'static str),
    #[error("A backend-specific error occurred: {0}")]
    BackendError(String),
}

pub type Result<T> = std::result::Result<T, ResizeError>;
