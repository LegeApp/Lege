//! Module for format-specific encoders.

pub mod ccitt4;
// pub mod indexed8;  // Removed - depends on deleted colorquant modules
pub mod jbig2;
#[cfg(feature = "jp2-openjp2")]
pub mod jp2;
#[cfg(feature = "jp2-lam")]
pub mod jp2lam;
pub mod jpeg;
