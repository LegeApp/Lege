//! JPEG encoder module - wraps the TooJpeg-rust implementation

// Re-export the main types and functions from the TooJpeg subdirectory
pub use toojpeg_lib::*;

// Include the TooJpeg library directly
mod toojpeg_lib {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/TooJpeg-rust/src/lib.rs"));
}
