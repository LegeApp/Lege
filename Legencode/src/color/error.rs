//! Error types for color processing operations

/// Color processing error types
#[derive(Debug)]
pub enum ColorOpsError {
    InvalidInput(String),
    ProcessingError(String),
    IoError(std::io::Error),
}

impl std::fmt::Display for ColorOpsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColorOpsError::InvalidInput(s) => write!(f, "Invalid input: {}", s),
            ColorOpsError::ProcessingError(s) => write!(f, "Processing error: {}", s),
            ColorOpsError::IoError(e) => write!(f, "IO error: {}", e),
        }
    }
}

impl std::error::Error for ColorOpsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ColorOpsError::IoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ColorOpsError {
    fn from(e: std::io::Error) -> Self {
        ColorOpsError::IoError(e)
    }
}

/// Result type for color operations
pub type Result<T> = std::result::Result<T, ColorOpsError>;
