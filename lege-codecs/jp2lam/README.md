# jp2lam

The canonical source is the [Lege monorepo](https://github.com/LegeApp/Lege/tree/main/lege-codecs/jp2lam).

JPEG 2000 Part 1 encoding in Rust, with focused JP2 decoding for document image workflows.

`jp2lam` writes unsigned 8–16-bit grayscale or sRGB images as JP2 files or raw J2K codestreams. It also includes a focused native decoder used for encoder conformance and Internet Archive-style JP2 page images.

The current goal is practical document-image interoperability first: a small Rust API, explicit unsupported-feature errors, and code organized around the JPEG 2000 standard rather than OpenJPEG internals.

## Highlights

- Encode unsigned 8–16-bit grayscale or sRGB input from owned images or borrowed planar/interleaved views.
- Write JP2 wrapper files or raw J2K codestreams.
- Use explicit lossless, quality, target-byte, total-bpp, or compression-ratio rate control.
- Bound high-resolution work with fixed/automatic tiling and RAM-to-temporary-file code-block spill.
- Label enumerated grayscale/sRGB explicitly or preserve a validated restricted ICC profile exactly.
- Decode Internet Archive-style JP2 page images into the crate's native `Image` model.
- Batch encode or decode folders whose images share dimensions, color model, and precision; encoded batches also share quality and format.
- Retain the legacy `quality` field while providing explicit `RateControl` and photographic presets.
- Optional PSNR/SSIM helper through `encode_with_psnr`.
- Optional CLI behind the `cli` feature.

## Install

```toml
[dependencies]
jp2lam = "0.3"
```

Minimum Rust version from the crate manifest:

```text
Rust 1.95+
```

## Quick Start

### Encode RGB To JP2

```rust
use jp2lam::{EncodeOptions, Image, OutputFormat, Preset};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let width = 800;
    let height = 600;
    let rgb = vec![128u8; width * height * 3];

    let image = Image::from_rgb_bytes(width as u32, height as u32, &rgb)?;
    let options = EncodeOptions::from_preset(Preset::PhotoHigh, OutputFormat::Jp2);

    let bytes = jp2lam::encode(&image, &options)?;
    std::fs::write("output.jp2", bytes)?;

    Ok(())
}
```

### Decode JP2

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read("page.jp2")?;
    let image = jp2lam::decode_jp2(&bytes)?;

    println!(
        "{}x{} {:?} components={}",
        image.width,
        image.height,
        image.colorspace,
        image.components.len()
    );

    Ok(())
}
```

### Decode Directly For A Renderer

Use a reusable `Jp2Decoder` when the destination will display a minified image.
The decoder omits unneeded high-resolution code-block reconstruction and writes
the requested packed 8-bit pixels directly:

```rust
use jp2lam::{
    DecodeConcurrency, DecodeOutputFormat, DecodeRequest, DecodeResolution, DecodeResult,
    Jp2Decoder,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = std::fs::read("page.jp2")?;
    let mut decoder = Jp2Decoder::new();
    let request = DecodeRequest {
        resolution: DecodeResolution::AtLeast {
            width: 800,
            height: 1100,
            quality_margin: 1.5,
        },
        output: DecodeOutputFormat::Rgb8,
        concurrency: DecodeConcurrency::Budgeted(2),
        ..Default::default()
    };

    let DecodeResult::Raster(raster) = decoder.decode(&bytes, &request)? else {
        unreachable!("a packed output format returns a raster");
    };
    render_rgb8(raster.width, raster.height, raster.stride, &raster.data);
    Ok(())
}

fn render_rgb8(_width: u32, _height: u32, _stride: usize, _pixels: &[u8]) {}
```

Reduced-resolution requests currently require a single-tile codestream.
Region-of-interest decoding is planned but is not implemented yet.

### Batch Encode Matching Pages

Use `BatchEncoder` when a folder or page stream comes from the same source and should keep one consistent image profile and encode configuration.

```rust
use jp2lam::{BatchEncoder, EncodeOptions, Image, OutputFormat};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let width = 2550;
    let height = 3300;
    let pages: Vec<Vec<u8>> = load_grayscale_pages();

    let options = EncodeOptions {
        quality: 85,
        format: OutputFormat::Jp2,
        ..Default::default()
    };
    let mut encoder = BatchEncoder::new(options);

    for (idx, page) in pages.iter().enumerate() {
        let image = Image::from_gray_bytes(width, height, page)?;
        let bytes = encoder.encode_one(&image)?;
        std::fs::write(format!("page_{idx:04}.jp2"), bytes)?;
    }

    Ok(())
}

fn load_grayscale_pages() -> Vec<Vec<u8>> {
    todo!("load pages from your own folder or pipeline")
}
```

`BatchDecoder` provides the matching decode path and rejects later items whose decoded image profile does not match the first page.

## Decoder Scope

The decoder is intentionally focused. It currently targets JP2 wrapper files and bare JPEG 2000 Part 1 codestreams that match the Internet Archive and PDF `/JPXDecode` page-image outputs tested during development:

- Unsigned 8–16-bit grayscale, RGB, CMYK, and palette-mapped images.
- Single or multi-tile codestreams, including multiple packet-boundary tile-parts per tile and Annex B.11 interleaving between tiles. Per-tile coding-style and quantization overrides remain unsupported.
- All five Part 1 progression orders, explicit precinct partitions, and SOP/EPH packet delimiters.
- MQ code-block styles including context reset, per-pass termination, vertical-causal contexts, predictable termination, and segmentation symbols.
- Common Archive.org `_jp2.zip` and extracted `*_jp2/` page-image layouts through the CLI.

This is not a universal JPEG 2000 or JPX decoder yet. Selective arithmetic bypass, arbitrary ICC profiles, multiple codestream boxes, and other out-of-scope features fail explicitly.

## Quality Guide

Prefer `RateControl` for new code. `Lossless` selects reversible 5/3; `Quality(0..=99)` selects calibrated photographic 9/7; exact-rate variants target the complete J2K codestream or JP2 file. The legacy `quality` field retains its compatibility meaning when `rate_control` is absent.

| Quality  | Use case                                                                    |
| -------: | --------------------------------------------------------------------------- |
|  `0..25` | Very small output, previews, stress testing                                 |
| `30..50` | Compact lossy output                                                        |
| `60..85` | Practical range for documents, screenshots, illustrations, and mixed images |
| `90..99` | High-fidelity lossy output                                                  |
|    `100` | Lossless output                                                             |

## Supported Input

Encoder input:

- Unsigned 8–16-bit grayscale.
- Unsigned 8–16-bit sRGB/RGB with explicit color description.
- Full-size, non-subsampled components.
- Images at least `2x2`.
- Borrowed `u8`/`u16` planar or interleaved input through `ImageView`.
- Interleaved RGB input through `Image::from_rgb_bytes`.
- Grayscale input through `Image::from_gray_bytes`.

Decoder input:

- JP2 files accepted by the focused decoder scope above.
- Complete in-memory byte slices through `decode_jp2`.
- Reader-backed input through `decode_from_reader`.

## CLI

The CLI is optional:

```bash
cargo run --release --features cli --bin jp2lam -- input.png
cargo run --release --features cli --bin jp2lam -- encode input.png q75
cargo run --release --features cli --bin jp2lam -- encode-dir pages_png/ pages_jp2/ q85
cargo run --release --features cli --bin jp2lam -- decode page.jp2 page.png
cargo run --release --features cli --bin jp2lam -- decode-dir book_jp2/ book_png/
cargo run --release --features cli --bin jp2lam -- decode-zip book_jp2.zip book_png/
```

The CLI reads normal image files through the optional `image` dependency, writes encoded JP2 output, and writes decoded pages as PNG files. `decode-zip` also walks nested ZIP entries and preserves the archive-relative output layout.

## Batch API

The batch API is for callers handling a sequence of images from one source. It does not require a folder abstraction in the library itself; external programs can walk their own directories and feed each page to `BatchEncoder` or `BatchDecoder`.

`BatchEncoder` checks dimensions, color model, component precision, component sampling, quality, and output format against the first image. `BatchDecoder` checks the decoded image profile against the first decoded page. The current implementation mainly centralizes validation and call shape; it is also the place to add future buffer reuse or shared setup without changing external callers.

Convenience helpers are available when all inputs are already in memory:

```rust
let encoded_pages = jp2lam::encode_batch(images.iter(), &options)?;
let decoded_pages = jp2lam::decode_batch(jp2_streams.iter().map(Vec::as_slice))?;
```

## Metrics

Use `encode_with_psnr` when you want encoded bytes plus internal PSNR/SSIM estimates:

```rust
use jp2lam::{EncodeOptions, Image, OutputFormat};

fn main() -> jp2lam::Result<()> {
    let width = 800;
    let height = 600;
    let gray = vec![240u8; width * height];

    let image = Image::from_gray_bytes(width as u32, height as u32, &gray)?;
    let options = EncodeOptions {
        quality: 75,
        format: OutputFormat::Jp2,
        ..Default::default()
    };

    let (bytes, metrics) = jp2lam::encode_with_psnr(&image, &options)?;

    println!("bytes: {}", bytes.len());
    println!("psnr: {:?}", metrics.psnr_db);
    println!("ssim: {:?}", metrics.ssim);

    Ok(())
}
```

## What It Does Internally

The encoder pipeline is organized around the standard JPEG 2000 stages:

1. Validate image and component metadata.
2. Prepare image samples.
3. Apply color transform where needed.
4. Run reversible 5/3 or irreversible 9/7 DWT.
5. Quantize lossy coefficients.
6. Encode code-block bit-planes with Tier-1 coding.
7. Select truncation points with PCRD.
8. Build packets and packet headers.
9. Write codestream markers.
10. Wrap the codestream in JP2 boxes when requested.

The crate also has an explicit Annex B geometry layer for tiles, tile-components, subbands, precincts, and code-blocks. Fixed and automatic tiling use reference-grid-aware DWT phase, and the production writer streams selected code-block payloads from bounded RAM/spill storage.

## Features

Default library build:

```toml
jp2lam = "0.1"
```

Optional CLI:

```toml
jp2lam = { version = "0.1", features = ["cli"] }
```

Available feature flags:

| Feature    | Purpose                                             |
| ---------- | --------------------------------------------------- |
| `cli`      | Enables the command-line tools and image-file input |
| `profile`  | Enables profiling hooks                             |
| `counters` | Exposes Tier-1 and stage-level memory high-water counters |

## Testing

```bash
cargo test
cargo test --features cli
```

## License

Dual-licensed under either:

- MIT
- Apache-2.0
