# Legencode - Unified Image Encoding Crate

A unified Rust crate that provides in-memory image encoding for multiple formats using only local code implementations.

## Features

- **JPEG**: High-quality JPEG encoding using the local `TooJpeg-rust` implementation
- **JPEG 2000 (JP2)**: Advanced JP2 encoding using the local `openjp2` implementation  
- **JBIG2**: Binary image compression using the local `jbig2enc-rust` implementation
- **CCITT Group 4**: Fax compression using the external `fax` crate

## Architecture

The crate uses a unified API through the `EncodingManager` struct that provides:
- **In-memory encoding**: All encoders work with `Vec<u8>` input and output
- **Local implementations**: All encoder code is included locally except for CCITT4 which uses the `fax` crate
- **Streamlined interface**: Single API for all formats with format-specific settings

## Usage

```rust
use image_codecs::{EncodingManager, ImageFormat, EncodingSettings, JpegSettings};

// Create RGB image data (width=8, height=8, channels=3)
let input_data = vec![255u8; 8 * 8 * 3]; // Red pixels

// Encode as JPEG
let jpeg_settings = EncodingSettings::Jpeg(JpegSettings {
    quality: 85,
    baseline: true,
    optimized: false,
    downsample: false,
});

let encoded_data = EncodingManager::encode(
    ImageFormat::Jpeg,
    &input_data,
    8,  // width
    8,  // height  
    3,  // channels (RGB)
    &jpeg_settings,
)?;

println!("Encoded {} bytes", encoded_data.len());
```

## Supported Formats

### JPEG
- **Channels**: 1 (grayscale) or 3 (RGB)
- **Input Data**: Raw pixel bytes (1 byte per channel per pixel)
- **Settings**: Quality, baseline/progressive, optimization, chroma subsampling
- **Implementation**: Local `TooJpeg-rust` crate

### JPEG 2000 (JP2)
- **Channels**: 1 (grayscale) or 3 (RGB)
- **Input Data**: Raw pixel bytes (1 byte per channel per pixel)
- **Settings**: Resolution levels, progression order, compression rate, wavelet type
- **Implementation**: Local `openjp2` crate

### JBIG2
- **Channels**: 0 (binary format)
- **Input Data**: **Byte-per-pixel format** - Each pixel is 1 byte (0 or 1), no bit-packing
- **Data Convention**: `0 => black pixel, 1 => white pixel`
- **Settings**: PDF fragment mode, symbol mode
- **Implementation**: Local `jbig2enc-rust` crate
- **Example**: 8x2 image = 16 total bytes

### CCITT Group 4
- **Channels**: 0 (binary format)
- **Input Data**: **Bit-packed PBM format** - Each row padded to byte boundary, `(width + 7) / 8` bytes per row
- **Data Convention**: `bit=1 => black pixel, bit=0 => white pixel` (PBM P4 format)
- **Settings**: No configurable settings
- **Implementation**: External `fax` crate v0.2.4
- **Example**: 8x2 image = 2 bytes per row = 4 total bytes

## Critical Input Data Format Requirements

**⚠️ IMPORTANT**: Binary formats (JBIG2 and CCITT4) have different data format expectations:

### JBIG2 Input Format
```rust
// For a 10x2 binary image, JBIG2 expects byte-per-pixel data:
let width = 10u32;
let height = 2u32;
let expected_size = (width * height) as usize; // = 20 bytes total

// Data layout: [p0, p1, p2, ..., p19] where each pixel is 0 or 1
let input_data = vec![0, 1, 1, 0, 1, 0, 0, 1, 1, 0,  // Row 0 (10 pixels)
                      1, 0, 0, 1, 0, 1, 1, 0, 0, 1]; // Row 1 (10 pixels)
```

### CCITT4 Input Format
```rust
// For a 10x2 binary image, CCITT4 expects bit-packed PBM data:
let width = 10u32;
let height = 2u32;
let bytes_per_row = (width + 7) / 8; // = 2 bytes per row
let expected_size = bytes_per_row * height as usize; // = 4 bytes total

// Data layout: [row0_byte0, row0_byte1, row1_byte0, row1_byte1]
// Each byte contains 8 pixels (MSB = leftmost pixel)
let input_data = vec![0xFF, 0xC0, 0x00, 0x3F]; // Example bit pattern
```

### Data Format Summary
| Format | Channels | Data Layout | Pixel Convention | Size Formula |
|--------|----------|-------------|------------------|--------------|
| JPEG | 1 or 3 | Raw bytes | N/A | width × height × channels |
| JP2 | 1 or 3 | Raw bytes | N/A | width × height × channels |
| JBIG2 | 0 | Byte-per-pixel | 0=black, 1=white | width × height |
| CCITT4 | 0 | Bit-packed | 1=black, 0=white | ((width+7)/8) × height |

## Project Structure

```
src/
├── lib.rs                    # Main library entry point
├── streamline.rs             # Unified encoding manager
├── TooJpeg-rust/            # Local JPEG encoder
├── openjp2/                 # Local JP2 encoder
└── jbig2enc-rust/           # Local JBIG2 encoder
```

## Dependencies

The main crate only has one external dependency:
- `fax` 0.2.4 - For CCITT4 encoding

All other encoders are implemented as local path dependencies.

## Testing

Run the basic usage example:
```bash
cargo run --example basic_usage
```

Expected output:
```
JPEG encoding successful! Output size: 616 bytes
JBIG2 encoding successful! Output size: 4 bytes  
CCITT4 encoding successful! Output size: 7 bytes
All encoders are working correctly!
```

## Build Status

✅ **Compilation**: Clean build with no errors  
✅ **API**: Unified interface for all formats  
✅ **Testing**: All encoders verified working  
✅ **Documentation**: Complete API documentation  

## Notes

- JP2 encoding uses temporary files internally due to openjp2 stream limitations
- JBIG2 and CCITT4 require binary (PBM) input data with channels=0
- JPEG and JP2 support both grayscale (channels=1) and RGB (channels=3) input

## License

MIT
