//! Perceptual quality optimization for JPEG 2000 encoding
//!
//! This module implements perceptual models to improve rate-distortion
//! optimization based on human visual perception.

pub mod contrast_mask;
pub mod taubman_masking;

#[cfg(test)]
pub use contrast_mask::ContrastMask;
pub use contrast_mask::{
    ContrastMaskMap, ContrastMaskParams, SourceRect, average_mask_for_source_rect,
    build_contrast_mask_map_from_luma_u8,
};
