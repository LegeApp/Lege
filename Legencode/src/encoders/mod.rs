//! Module for format-specific encoders.

pub mod ccitt4;
// pub mod indexed8;  // Removed - depends on deleted colorquant modules
pub mod jbig2;
pub mod jpeg;
#[cfg(feature = "jp2-openjp2")]
pub mod jp2;
