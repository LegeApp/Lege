use std::error::Error;
use std::fmt;
use std::io;

/// Main error type for the DjVu encoder library.
#[derive(Debug)]
pub enum DjvuError {
    /// An I/O error occurred
    Io(io::Error),
    /// An invalid argument was provided
    InvalidArg(String),
    /// An invalid operation was attempted
    InvalidOperation(String),
    /// A validation error occurred
    ValidationError(String),
    /// A stream processing error occurred
    Stream(String),
    /// A custom error with a message
    Custom(String),
    /// An encoding/decoding error occurred
    EncodingError(String),
}

impl fmt::Display for DjvuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DjvuError::Io(err) => write!(f, "I/O error: {}", err),
            DjvuError::InvalidArg(msg) => write!(f, "Invalid argument: {}", msg),
            DjvuError::InvalidOperation(msg) => write!(f, "Invalid operation: {}", msg),
            DjvuError::ValidationError(msg) => write!(f, "Validation error: {}", msg),
            DjvuError::Stream(msg) => write!(f, "Stream error: {}", msg),
            DjvuError::Custom(msg) => write!(f, "Error: {}", msg),
            DjvuError::EncodingError(msg) => write!(f, "Encoding error: {}", msg),
        }
    }
}

impl Error for DjvuError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            DjvuError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for DjvuError {
    fn from(err: io::Error) -> Self {
        DjvuError::Io(err)
    }
}

impl From<crate::encode::jb2::error::Jb2Error> for DjvuError {
    fn from(err: crate::encode::jb2::error::Jb2Error) -> Self {
        DjvuError::EncodingError(err.to_string())
    }
}

impl From<crate::encode::zc::ZCodecError> for DjvuError {
    fn from(err: crate::encode::zc::ZCodecError) -> Self {
        DjvuError::EncodingError(err.to_string())
    }
}

/// A specialized `Result` type for DjVu encoding operations.
pub type Result<T> = std::result::Result<T, DjvuError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display() {
        let io_error = io::Error::new(io::ErrorKind::NotFound, "file not found");
        assert_eq!(
            DjvuError::Io(io_error).to_string(),
            "I/O error: file not found"
        );

        assert_eq!(
            DjvuError::InvalidArg("test".to_string()).to_string(),
            "Invalid argument: test"
        );

        assert_eq!(
            DjvuError::InvalidOperation("test".to_string()).to_string(),
            "Invalid operation: test"
        );

        assert_eq!(
            DjvuError::ValidationError("test".to_string()).to_string(),
            "Validation error: test"
        );

        assert_eq!(
            DjvuError::Stream("test".to_string()).to_string(),
            "Stream error: test"
        );

        assert_eq!(
            DjvuError::Custom("test".to_string()).to_string(),
            "Error: test"
        );
    }
}
