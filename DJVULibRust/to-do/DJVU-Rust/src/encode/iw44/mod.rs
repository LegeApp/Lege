// src/encode/iw44/mod.rs

//! IW44 wavelet-based image encoder implementation
//!
//! This module provides the IW44 (Incremental Wavelet 44) encoding functionality
//! for DjVu image compression.

pub mod codec;
pub mod coeff_map;
pub mod constants;
pub mod encoder;
pub mod masking;
pub mod transform;
pub mod zigzag;
#[cfg(test)]
mod tests;

// Re-export commonly used types and functions
pub use codec::*;
pub use coeff_map::*;
pub use constants::*;
pub use encoder::*;
pub use masking::*;
pub use zigzag::{get_zigzag_loc, get_zigzag_loc_checked, ZIGZAG_LOC};
