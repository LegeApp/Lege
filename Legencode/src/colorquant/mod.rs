//! Color quantization functionality for image processing

pub mod grayscale;

// Re-export public items from the grayscale module
pub use grayscale::{
    load_png_grayscale, quantize_to_1bit, CliArgs, NeuQuantGray, PngCompressionArg,
};
