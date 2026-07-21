//! Perceptual quality optimization for JPEG 2000 encoding
//!
//! This module implements perceptual models to improve rate-distortion
//! optimization based on human visual perception.

pub mod contrast_mask;
pub mod taubman_masking;

pub use contrast_mask::{
    ContrastMask, ContrastMaskMap, ContrastMaskParams, SourceRect, average_mask_for_source_rect,
    build_contrast_mask_map_from_luma_u8, contrast_mask_for_luma_block8x8,
};
