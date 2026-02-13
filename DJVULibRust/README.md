# DjVu Encoder

[![Crates.io](https://img.shields.io/crates/v/djvu_encoder.svg)](https://crates.io/crates/djvu_encoder)
[![Documentation](https://docs.rs/djvu_encoder/badge.svg)](https://docs.rs/djvu_encoder)
[![License](https://img.shields.io/crates/l/djvu_encoder.svg)](https://github.com/yourusername/djvu_encoder/blob/main/LICENSE)

A high-performance Rust library for encoding DjVu documents with a modern builder API.

## Overview

DjVu Encoder provides a thread-safe, coordinate-based API for creating multi-page DjVu documents from image data. It supports out-of-order page processing, automatic layer masking, and optional parallel encoding for high-throughput workflows.

## Key Features

- **Coordinate-based layers**: Position image layers at specific coordinates with automatic JB2/IW44 masking
- **Out-of-order processing**: Add pages in any sequence, even in parallel
- **Thread-safe**: Safe concurrent access from multiple threads
- **High performance**: Optimized encoding with planned SIMD and assembly ZP coder support
- **Memory efficient**: Stream processing with minimal memory footprint
- **Builder pattern**: Intuitive API for complex document construction

## Quick Start

```rust
use djvu_encoder::{DjvuBuilder, PageBuilder};

let doc = DjvuBuilder::new(10)
    .with_dpi(300)
    .with_quality(90)
    .build();

// Add pages (out-of-order supported)
for i in 0..10 {
    let page = PageBuilder::new(i, 2480, 3508)  // A4 @ 300dpi
        .with_background(load_pixmap(i))
        .with_foreground(load_bitmap(i), 50, 100)
        .build()?;
    doc.add_page(page)?;
}

let djvu_bytes = doc.finalize()?;
std::fs::write("output.djvu", djvu_bytes)?;
```

## Image Formats

- **Pixmap**: RGB/grayscale images for IW44 background layers (photos, scans)
- **Bitmap**: Bilevel images for JB2 foreground layers (text, graphics)

## Building

### Prerequisites

- Rust 1.70+ (2024 edition)
- Cargo

### Basic Build

```bash
cargo build --release
```

### With Features

```bash
# Enable parallel processing
cargo build --release --features rayon

# Enable SIMD optimizations (planned - not yet finalized)
cargo build --release --features portable_simd

# Enable assembly ZP coder (planned - x86_64 only, not yet finalized)
cargo build --release --features asm_zp

# Enable IW44 debug tracing
cargo build --release --features iw44-trace
```

### Feature Flags

| Feature | Description | Default |
|---------|-------------|---------|
| `rayon` | Parallel encoding using Rayon | Disabled |
| `portable_simd` | SIMD optimizations for encoding (planned - not finalized) | Disabled |
| `asm_zp` | Assembly-optimized ZP arithmetic coder (planned - not finalized) | Disabled |
| `iw44-trace` | Verbose IW44 encoding debug output | Disabled |
| `dev_asm_cmp` | Assembly vs Rust ZP comparison tests | Disabled |

## Modules

- **`doc`**: Main builder API (`DjvuBuilder`, `PageBuilder`)
- **`encode`**: Core encoders
  - `iw44`: Wavelet compression for color/grayscale images
  - `jb2`: Bilevel compression for text/graphics
  - `zc`: Arithmetic coding backend
- **`iff`**: DjVu file format (IFF) handling
- **`image`**: Image types (`Pixel`, `Pixmap`, `Bitmap`) and operations
- **`annotations`**: DjVu annotation support
- **`utils`**: Error handling and utilities

## Performance

- **Parallel processing**: Rayon-based multi-threading
- **Memory efficient**: Stream-based encoding with minimal allocations
- **Planned optimizations**: SIMD acceleration and assembly ZP coder (not yet finalized)

## Compatibility

- **Rust**: 1.70+ (2024 edition)
- **Platforms**: Linux, macOS, Windows
- **Architecture**: x86_64, ARM64

## License

Licensed under the GNU General Public License version 3 (GPL-3.0). See [LICENSE](LICENSE) for details.

## Contributing

Contributions welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## Related Projects

- [Lege](https://github.com/yourusername/lege): PDF to DjVu converter using this library
- [DjVuLibre](http://djvu.sourceforge.net/): Reference DjVu implementation</content>
<parameter name="filePath">D:\Rust-projects\Lege\DJVULibRust\README.md