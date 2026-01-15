//! OpenJP2 JP2 encoder module

// Include the OpenJP2 implementation
#[path = "openjp2/src/lib.rs"]
pub mod lib;

// Re-export the main API
pub use lib::*;
