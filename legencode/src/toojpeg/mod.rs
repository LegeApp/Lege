//! TooJpeg JPEG encoder module
//!
//! This module provides JPEG encoding functionality using the TooJpeg algorithm.

// Re-export the main API from the TooJpeg-rust implementation
pub use crate::toojpeg::toojpeg_rust::*;

// Include the TooJpeg-rust implementation as a submodule
#[path = "../TooJpeg-rust/src/lib.rs"]
mod toojpeg_rust;
