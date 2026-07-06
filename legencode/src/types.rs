//! Shared types for color processing and binarization
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum AppError {
    ConfigError(config::ConfigError),
    IoError(std::io::Error),
    InvalidConfig(String),
    InvalidPageRange(String),
    InvalidFormatOption(String),
    InvalidBinarizationMethod(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::ConfigError(e) => write!(f, "Configuration error: {}", e),
            AppError::IoError(e) => write!(f, "IO error: {}", e),
            AppError::InvalidConfig(s) => write!(f, "Invalid configuration: {}", s),
            AppError::InvalidPageRange(s) => write!(f, "Invalid page range: {}", s),
            AppError::InvalidFormatOption(s) => write!(f, "Invalid format option: {}", s),
            AppError::InvalidBinarizationMethod(s) => {
                write!(f, "Invalid binarization method: {}", s)
            }
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AppError::ConfigError(e) => Some(e),
            AppError::IoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<config::ConfigError> for AppError {
    fn from(e: config::ConfigError) -> Self {
        AppError::ConfigError(e)
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::IoError(e)
    }
}

pub type AppResult<T> = Result<T, AppError>;

/// Default Sauvola k-factor. Lower values produce more ink (darker output)
/// in flat/low-variance regions. 0.05 balances stroke preservation vs background
/// noise across clean-white and yellowed paper scans.
pub const DEFAULT_K_FACTOR: f32 = 0.05;

/// These are derived from `BinarizationConfig` for per-image operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinarizationOptions {
    pub invert: bool,
    pub invert_input: bool, // Invert input grayscale before thresholding (for inverted docs)
    pub k_factor: f32,
    pub use_heavy_duty: bool,
    /// No-op: the heavy (ONNX) path is always whole-image now; patching was removed.
    /// Retained only for config/serde compatibility. See `BinarizationConfig`.
    pub patch_percentage: f32,
    /// No-op: see `patch_percentage`.
    pub no_patch: bool,
    pub use_fixed_threshold: bool,
    pub fixed_threshold: u8,
    /// When true, skip GPU binarization fast paths and use the CPU path.
    /// Used to dodge a black-background regression seen in layout-mode runs
    /// where the GPU path occasionally produced an inverted base layer.
    #[serde(default)]
    pub disable_gpu: bool,
}

impl Default for BinarizationOptions {
    fn default() -> Self {
        Self {
            invert: false,
            invert_input: false,
            k_factor: DEFAULT_K_FACTOR,
            use_heavy_duty: false,
            patch_percentage: 5.0,
            no_patch: false,
            use_fixed_threshold: false,
            fixed_threshold: 200,
            disable_gpu: false,
        }
    }
}

/// Configuration structure for binarization algorithms
#[derive(Clone, Debug)]
pub struct BinarizationConfig {
    /// Sauvola k factor (controls sensitivity). With `thr = μ(1 + k(σ/127 − 1))`,
    /// higher k *lowers* the threshold in flat/low-σ regions → more paper, i.e.
    /// LIGHTER output (less ink); lower k darkens flat regions.
    pub k_factor: f32,
    /// Invert output (black on white vs. white on black).
    pub invert: bool,
    /// Invert input grayscale before thresholding (for inverted documents).
    pub invert_input: bool,
    /// Use heavy duty Sauvola (requires sauvola.onnx model).
    pub use_heavy_duty: bool,
    /// No-op: the heavy (ONNX) path is always whole-image now; patching was removed.
    /// Retained only for config/serde compatibility.
    pub patch_percentage: f32,
    /// No-op: see `patch_percentage`.
    pub no_patch: bool,
    /// Use a fixed global threshold instead of Sauvola/Otsu fusion.
    pub use_fixed_threshold: bool,
    /// Fixed threshold value (0-255) applied after linearization.
    pub fixed_threshold: u8,
}

impl Default for BinarizationConfig {
    fn default() -> Self {
        Self {
            k_factor: DEFAULT_K_FACTOR,
            invert: false,
            invert_input: false,
            use_heavy_duty: false,
            patch_percentage: 5.0,
            no_patch: false,
            use_fixed_threshold: false,
            fixed_threshold: 200,
        }
    }
}
