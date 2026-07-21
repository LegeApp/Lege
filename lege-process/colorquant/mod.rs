//! Reusable color quantization helpers for debug PNG outputs.

pub mod neuquant;

pub use neuquant::{PngQuantizationOptions, write_quantized_rgb_png};
