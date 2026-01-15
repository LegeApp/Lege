//! JBIG2 encoder module - wraps the jbig2enc-rust implementation

// Include the main jbig2 modules
#[path = "jbig2enc-rust/src/jbig2.rs"]
pub mod jbig2_main;

#[path = "jbig2enc-rust/src/jbig2enc.rs"]
pub mod jbig2enc;

#[path = "jbig2enc-rust/src/jbig2structs.rs"]
pub mod jbig2structs;

#[path = "jbig2enc-rust/src/jbig2arith.rs"]
pub mod jbig2arith;

#[path = "jbig2enc-rust/src/jbig2comparator.rs"]
pub mod jbig2comparator;

#[path = "jbig2enc-rust/src/jbig2lutz.rs"]
pub mod jbig2lutz;

#[path = "jbig2enc-rust/src/jbig2pdf.rs"]
pub mod jbig2pdf;

#[path = "jbig2enc-rust/src/jbig2shared.rs"]
pub mod jbig2shared;

#[path = "jbig2enc-rust/src/jbig2sym.rs"]
pub mod jbig2sym;

// Re-export the main types and functions we need
pub use jbig2_main::{encode_rois, Jbig2Context};

// Re-export error handling
pub use crate::EncodingError;
