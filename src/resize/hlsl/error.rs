use thiserror::Error;

/// Result type for resizer operations
pub type Result<T> = std::result::Result<T, ResizerError>;

/// Errors that can occur during resizing operations
#[derive(Error, Debug)]
pub enum ResizerError {
    #[error("DirectX 12 initialization failed: {0}")]
    D3D12InitializationFailed(String),

    #[error("DirectX 12 operation failed: HRESULT {0:08X}")]
    D3D12OperationFailed(i32),

    #[error("Shader compilation failed: {0}")]
    ShaderCompilationFailed(String),

    #[error("Invalid image dimensions: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },

    #[error("Invalid channel count: {0} (must be 1, 3, or 4)")]
    InvalidChannelCount(u32),

    #[error("Buffer size mismatch: expected {expected}, got {actual}")]
    BufferSizeMismatch { expected: usize, actual: usize },

    #[error("Unsupported filter type: {0}")]
    UnsupportedFilter(String),

    #[error("Resource allocation failed: {0}")]
    ResourceAllocationFailed(String),

    #[error("Command queue execution failed: {0}")]
    CommandExecutionFailed(String),

    #[error("GPU timeout: operation took too long")]
    GpuTimeout,

    #[error("Invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("Windows API error: {0}")]
    WindowsApiError(windows::core::Error),
}

impl ResizerError {
    /// Convert Windows HRESULT to ResizerError
    pub fn from_hresult(hr: i32) -> Self {
        Self::D3D12OperationFailed(hr)
    }

    /// Create an error from a Windows core error
    pub fn from_windows_error(error: windows::core::Error) -> Self {
        Self::WindowsApiError(error)
    }
}

impl From<windows::core::Error> for ResizerError {
    fn from(error: windows::core::Error) -> Self {
        ResizerError::WindowsApiError(error)
    }
}
