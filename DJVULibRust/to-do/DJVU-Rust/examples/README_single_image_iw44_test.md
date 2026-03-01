# Single Image IW44 Test

This example demonstrates how to encode a single JPEG image to IW44 format using the Rust DjVu encoder, equivalent to what `c44.exe` (the DjVuLibre C++ encoder) does.

## How to Run

1. Place a JPEG file named `test.jpeg` in the current directory (f:\EncodingTester\DJVU-rust-port\Rust\)
2. Run the example:
   ```bash
   cargo run --example single_image_iw44_test
   ```

## What it Does

The example loads the input JPEG image and encodes it to IW44 format using four different quality settings:

1. **High Quality** (45 dB, Full chrominance) - Best quality, largest file
2. **Medium Quality** (35 dB, Full chrominance) - Good quality/size balance
3. **Low Quality** (25 dB, Half chrominance) - Lower quality, smaller file
4. **Very Low Quality** (15 dB, Half chrominance) - Minimal quality, smallest file

Each encoding produces a separate `.iw4` file that contains the IW44-encoded image data in proper IFF format.

## Output Files

The example generates four files:
- `high_quality_test_output.iw4`
- `medium_quality_test_output.iw4`  
- `low_quality_test_output.iw4`
- `very_low_quality_test_output.iw4`

## IW44 Format Details

The generated files use the proper IW44 format:
- `FORM:BM44` container for the entire file
- Multiple `BM44` chunks for progressive encoding
- Each chunk contains slices that progressively refine the image quality
- ZP (Zero-tree Prediction) compression for efficient entropy coding

## Comparison with c44.exe

This Rust implementation should produce similar compression ratios and quality levels as the original DjVuLibre `c44.exe` tool. The progressive encoding allows for streaming and partial decoding of images.

## Debug Output

The example provides detailed debug information showing:
- YCbCr color space conversion
- Wavelet coefficient analysis
- Progressive slice encoding progress
- Compression statistics

This helps understand how the IW44 encoder processes the image data internally.
