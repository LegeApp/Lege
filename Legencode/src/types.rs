//! Shared types for color processing and binarization
use anyhow::Result;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Configuration error: {0}")]
    ConfigError(#[from] config::ConfigError),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Invalid page range: {0}")]
    InvalidPageRange(String),

    #[error("Invalid format option: {0}")]
    InvalidFormatOption(String),

    #[error("Invalid binarization method: {0}")]
    InvalidBinarizationMethod(String),
}

pub type AppResult<T> = Result<T, AppError>;

/// These are derived from `BinarizationConfig` for per-image operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinarizationOptions {
    pub invert: bool,
    pub invert_input: bool, // Invert input grayscale before thresholding (for inverted docs)
    pub k_factor: f32,
    pub use_heavy_duty: bool,
    pub patch_percentage: f32,
    pub no_patch: bool, // Debug option to disable patching
    pub use_fixed_threshold: bool,
    pub fixed_threshold: u8,
}

impl Default for BinarizationOptions {
    fn default() -> Self {
        Self {
            invert: false,
            invert_input: false,
            k_factor: 0.2,
            use_heavy_duty: false,
            patch_percentage: 5.0,
            no_patch: false,
            use_fixed_threshold: false,
            fixed_threshold: 200,
        }
    }
}

/// Configuration structure for binarization algorithms
#[derive(Clone, Debug)]
pub struct BinarizationConfig {
    /// Sauvola k factor (controls sensitivity, higher = darker output).
    pub k_factor: f32,
    /// Invert output (black on white vs. white on black).
    pub invert: bool,
    /// Invert input grayscale before thresholding (for inverted documents).
    pub invert_input: bool,
    /// Use heavy duty Sauvola (requires sauvola.onnx model).
    pub use_heavy_duty: bool,
    /// Patch percentage for heavy duty processing.
    pub patch_percentage: f32,
    /// Debug option to disable patching (process entire image).
    pub no_patch: bool,
    /// Use a fixed global threshold instead of Sauvola/Otsu fusion.
    pub use_fixed_threshold: bool,
    /// Fixed threshold value (0-255) applied after linearization.
    pub fixed_threshold: u8,
}

impl Default for BinarizationConfig {
    fn default() -> Self {
        Self {
            k_factor: 0.2,
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
