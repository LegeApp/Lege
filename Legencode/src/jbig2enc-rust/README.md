# jbig2enc-rust

A Rust port of the jbig2enc library with enhanced features for JBIG2 encoding. This library provides functionality to encode binary images into the JBIG2 format, supporting both standalone JBIG2 files and PDF-embedded fragments with proper global dictionary handling.

## Features

- **JBIG2 Encoding**: Full implementation of the JBIG2 specification for compressing binary images
- **Symbol Dictionary Support**: Advanced symbol matching using code adapted from djvulibre for improved compression of text documents
- **Spec Halftone Support**: Enhanced halftone encoding using Stucki dithering algorithm
- **PDF Fragment Support**: Ability to create separate global and page data for proper PDF embedding
- **Connected Component Analysis**: Optional feature for advanced symbol extraction and analysis
- **Lossless and Lossy Modes**: Configurable encoding options for different use cases

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
jbig2enc-rust = "0.2.0"
```

## Usage

### Basic Encoding

```rust
use jbig2enc_rust::{encode_single_image, Jbig2Config};

// Example binary image data (0/1 values)
let input = vec![0, 1, 0, 1, 1, 0, 1, 0]; // Example data
let width = 2;
let height = 4;

// Create default configuration
let config = Jbig2Config::default();

// Encode the image
let result = encode_single_image(&input, width, height, false)?;
```

### PDF Mode Encoding

For PDF embedding, use PDF mode to get separate global and page data:

```rust
use jbig2enc_rust::{encode_single_image, Jbig2Config, Jbig2Context};

let input = vec![0, 1, 0, 1, 1, 0, 1, 0];
let width = 2;
let height = 4;

// Create context with PDF mode enabled
let ctx = Jbig2Context::with_pdf_mode(true);

// Encode for PDF embedding
let result = encode_single_image(&input, width, height, true)?;
// result.global_data contains global dictionary (if any)
// result.page_data contains page-specific data
```

### Advanced Configuration

```rust
use jbig2enc_rust::Jbig2Config;

// Create custom configuration
let mut config = Jbig2Config::default();
config.symbol_mode = true;  // Enable symbol dictionary mode
config.dpi = 300;           // Set DPI
config.want_full_headers = true; // Include full file headers

// For lossless encoding
let lossless_config = Jbig2Config::lossless();
```

## License Notice

⚠️ **Important Licensing Information**:

- When built **without** symbol dictionary support: Licensed under **Apache 2.0** (same as original jbig2enc)
- When built **with** symbol dictionary support (using the `cc-analysis` feature): Technically **GPL-3.0** due to the djvulibre-derived code

The symbol matching code is adapted from djvulibre, which is GPL-licensed. If you enable the `cc-analysis` feature, your resulting binary may be subject to GPL licensing terms.

## Features

Enable optional features in your `Cargo.toml`:

```toml
[dependencies.jbig2enc-rust]
version = "0.2.0"
features = ["cc-analysis", "tracing"]
```

Available features:
- `cc-analysis`: Enables connected component analysis for advanced symbol extraction (GPL license applies)
- `tracing`: Enables detailed logging and tracing for debugging
- `trace_encoder`: Enables encoder-specific tracing
- `line_verify`: Line verification functionality
- `default`: Includes basic tracing functionality

## Architecture

The library is organized into several key modules:

- `jbig2enc`: Main encoder logic and document handling
- `jbig2arith`: Arithmetic coding implementation
- `jbig2sym`: Symbol and bitmap handling
- `jbig2structs`: JBIG2 data structures and configuration
- `jbig2halftone`: Halftone encoding with Stucki dithering
- `jbig2cc`: Connected component analysis (when enabled)
- `jbig2comparator`: Symbol comparison algorithms

## Testing

Run the test suite:

```bash
cargo test
```

Some tests require the `jbig2dec` external decoder to be available in your PATH for round-trip validation.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request. For major changes, please open an issue first to discuss what you would like to change.

## Acknowledgments

- Based on the original jbig2enc library
- Symbol matching code adapted from djvulibre
- Halftone encoding based on "Lossy Compression of Stochastic Halftones with JBIG2" by M. Valliappan et al.
- Implements the JBIG2 specification (ISO/IEC 14492)

## References

- JBIG2 Specification: ISO/IEC 14492:2001
- Original jbig2enc: https://github.com/agl/jbig2enc
- djvulibre: https://github.com/djvuzone/djvulibre