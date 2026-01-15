//! TooJpeg JPEG encoder module

// Include all the TooJpeg implementation files
#[path = "TooJpeg-rust/src/lib.rs"]
pub mod lib;

// Re-export the main API
pub use lib::*;
