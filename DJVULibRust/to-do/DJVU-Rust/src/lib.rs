#![feature(portable_simd)]

//! A Rust library for encoding DjVu documents.
//!
//! This crate provides a high-level API for creating multi-page DjVu
//! documents from image data. The main entry point is the `DocumentEncoder`.

// Core modules
pub mod annotations;
pub mod doc;
pub mod encode;
// pub mod ffi; // TODO: FFI module not yet implemented
pub mod iff;
pub mod image;
pub mod utils;
pub mod validate;

// Public API exports
pub use doc::{DocumentEncoder, PageComponents, PageEncodeParams};
pub use utils::error::{DjvuError, Result};

// Constants
pub const DJVU_VERSION: &str = "0.1.0";

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::RgbImage;

    #[test]
    fn test_public_api_smoke_test() {
        // This is a simple smoke test to ensure the public API is usable.
        let mut encoder = DocumentEncoder::new();
        let page_components = PageComponents::new()
            .with_background(RgbImage::new(10, 10))
            .unwrap();
        encoder.add_page(page_components).unwrap();

        let mut buffer = Vec::new();
        let result = encoder.write_to(&mut buffer);
        assert!(result.is_ok());
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_version() {
        assert_eq!(DJVU_VERSION, "0.1.0");
    }
}
