# TooJpeg-rust

[![Crates.io](https://img.shields.io/crates/v/toojpeg)](https://crates.io/crates/toojpeg)
[![Documentation](https://docs.rs/toojpeg/badge.svg)](https://docs.rs/toojpeg)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

A high-performance Rust port of the TooJpeg JPEG encoder. This crate provides a simple interface for encoding RGB(A) images to JPEG format with various quality and optimization settings.

## Features

- Fast JPEG encoding with optimized integer arithmetic
- Support for RGB, RGBA, and grayscale input formats
- Configurable quality settings (1-100)
- Baseline and progressive encoding
- Optimized Huffman tables
- Chroma subsampling (4:2:0) for smaller file sizes
- `no_std` support (with the `libm` crate for floating point)

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
toojpeg = "0.1"
```

### Basic Example

```rust
use toojpeg::{encode_jpeg, EncodeOptions, ImageFormat};
use std::fs::File;
use std::io::Write;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a simple 2x2 RGB image (red, green, blue, white)
    let pixels = vec![
        255, 0, 0,      // Red
        0, 255, 0,      // Green
        0, 0, 255,      // Blue
        255, 255, 255   // White
    ];

    let options = EncodeOptions {
        width: 2,
        height: 2,
        format: ImageFormat::RGB,
        quality: 90,
        ..Default::default()
    };

    let mut output = Vec::new();
    encode_jpeg(&pixels, options, &mut output)?;
    
    // Save to file
    let mut file = File::create("output.jpg")?;
    file.write_all(&output)?;
    
    Ok(())
}
```

## Performance

This implementation includes several optimizations over the original TooJpeg code:

- Integer arithmetic for color space conversion
- Optimized downsampling with proper rounding
- Precomputed quantization tables
- Improved cache locality in critical loops
- Unchecked array access in performance-critical sections

## License

Licensed under the [MIT License](LICENSE).
