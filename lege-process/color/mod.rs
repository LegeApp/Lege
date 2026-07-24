//! Color Operations Module
//!
//! Image processing library for binarization, grayscale processing, and dithering.
//! Focused on CPU-based 1-bit document processing.

// Private modules - not exposed in public API
mod error;
pub mod linearize;
mod types;

// Public modules - only keeping essential modules for 1-bit processing
pub mod binarization;
pub mod color_processing;

// Re-export necessary types and traits
// Re-export key types from top-level types module
pub use error::{ColorOpsError, Result};
pub use types::{BinarizationConfig, BinarizationOptions, DEFAULT_K_FACTOR};

// Export essential binarization utilities
pub use binarization::{
    HeavySauvolaProcessor, binarize_gray, binarize_image, binarize_image_raw,
    compute_adaptive_gpu_constants, odd_background_window, sauvola_window_for,
};

// Export color processing utilities
pub use color_processing::{ImageRegionDitherMode, process_image_region};

// GPU-related convenience functions removed - using direct binarization and dithering functions instead
