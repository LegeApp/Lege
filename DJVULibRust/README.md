# DJVULibRust

[![Crates.io](https://img.shields.io/crates/v/djvu_encoder.svg)](https://crates.io/crates/djvu_encoder)
[![Documentation](https://docs.rs/djvu_encoder/badge.svg)](https://docs.rs/djvu_encoder)
[![Repository](https://img.shields.io/badge/github-LegeApp%2FDJVULibRust-blue)](https://github.com/LegeApp/DJVULibRust)

DJVULibRust is a Rust DjVu encoder focused on building valid single-page `DJVU`
and bundled multi-page `DJVM` documents from image data. It provides a high-level
builder API for normal document pipelines and lower-level page/component APIs for
custom encoders, experiments, and benchmarking.

The encoder is designed around page independence: pages can be built and encoded
out of order, encoded on worker threads, then inserted into the final document in
page-number order. This makes it a good fit for OCR and scanning pipelines where
image cleanup, OCR, and compression are naturally page-parallel.

## Features

- **IW44 photo/background encoding** for color or grayscale scanned pages.
- **JB2 bitonal foreground and mask support** for text and line art layers.
- **Layered page construction** with coordinate-based background, foreground,
  and mask placement.
- **Out-of-order page insertion** for pipelines that finish pages in arbitrary
  order.
- **Page-parallel encoding workflow** via `encode_page` plus `add_encoded_page`.
- **Optional internal rayon acceleration** for IW44 color-channel work.
- **Bundled multi-page DJVM assembly** with generated page directory metadata.
- **Single-page DJVU output** when the document has one page.
- **Hidden text layers** from OCR word boxes.
- **Hyperlink/annotation support** for rectangular clickable regions.
- **Advanced component API** for direct `PageComponents` and `PageEncodeParams`
  workflows.
- **Streaming-friendly document assembly** that avoids unnecessary full-page
  copies when bundling already-encoded page forms.

## Quick Start

Add the library from Git:

```toml
[dependencies]
djvu_encoder = { git = "https://github.com/LegeApp/DJVULibRust", features = ["rayon"] }
```

Create a simple multi-page document:

```rust
use djvu_encoder::{DjvuBuilder, PageBuilder, Pixel, Pixmap, Result};

fn white_page(width: u32, height: u32) -> Pixmap {
    Pixmap::from_pixel(width, height, Pixel::white())
}

fn main() -> Result<()> {
    let doc = DjvuBuilder::new(2)
        .with_dpi(300)
        .with_quality(90)
        .build();

    let page0 = PageBuilder::new(0, 2550, 3300)
        .with_background(white_page(2550, 3300))?
        .build()?;

    let page1 = PageBuilder::new(1, 2550, 3300)
        .with_background(white_page(2550, 3300))?
        .with_ocr_words(vec![
            ("Hello".to_string(), 100, 200, 180, 40),
            ("DjVu".to_string(), 300, 200, 160, 40),
        ])
        .build()?;

    doc.add_page(page1)?; // out of order is fine
    doc.add_page(page0)?;

    let bytes = doc.finalize()?;
    std::fs::write("output.djvu", bytes)?;
    Ok(())
}
```

## Image Types

- **`Pixmap`**: RGB/grayscale image data for IW44 backgrounds such as photos,
  scans, and page images.
- **`Bitmap`**: bilevel image data for JB2 foreground, masks, text, and line
  art.
- **`Pixel` and `GrayPixel`**: simple pixel primitives used by the image types.

## Parallel Page Encoding

`add_page` is convenient, but it performs the CPU-heavy compression immediately.
For parallel pipelines, split encoding from insertion:

```rust
use djvu_encoder::{DjvuBuilder, Page, Result};
use rayon::prelude::*;

fn encode_pages_parallel(pages: Vec<Page>) -> Result<Vec<u8>> {
    let doc = DjvuBuilder::new(pages.len())
        .with_dpi(300)
        .with_quality(90)
        .build();

    let encoded = pages
        .into_par_iter()
        .map(|page| doc.encode_page(page))
        .collect::<Result<Vec<_>>>()?;

    for page in encoded {
        doc.add_encoded_page(page)?;
    }

    doc.finalize()
}
```

This pattern keeps the expensive IW44/JB2 work on worker threads while the final
document collection remains deterministic and thread-safe.

## Encoding Controls

Use `PageEncodeParams` when you need direct control over IW44 and page settings:

```rust
use djvu_encoder::{DjvuBuilder, PageEncodeParams};

let params = PageEncodeParams {
    dpi: 300,
    use_iw44: true,
    color: true,
    slices: Some(100),
    decibels: None,
    bytes: None,
    db_frac: 0.35,
    lossless: false,
    quant_multiplier: Some(1.0),
    ..PageEncodeParams::default()
};

let doc = DjvuBuilder::new(10).with_params(params).build();
```

Important knobs:

- `slices`: controls IW44 progressive slice target. More slices usually means
  better quality and larger files.
- `decibels`: target IW44 quality by estimated SNR instead of slice count.
- `bytes`: target encoded size.
- `quant_multiplier`: tunes coefficient retention. Lower values keep more
  coefficients; higher values reduce size.
- `color`: choose color or grayscale IW44 output.
- `lossless`: enables lossless mode where supported by the page path.

## Building

Prerequisites:

- Rust with 2024 edition support.
- Cargo.

Basic build:

```bash
cargo build --release
```

With common feature flags:

```bash
# Enable Rayon-backed parallel work
cargo build --release --features rayon

# Enable experimental portable SIMD paths, requires nightly Rust
cargo build --release --features portable_simd

# Enable assembly-backed ZP coder paths where available
cargo build --release --features asm_zp

# Enable IW44 tracing for diagnostics
cargo build --release --features iw44-trace
```

## Benchmarks

The following benchmark uses 50 PNG pages from the `sahib`/Buddha Sahib test
set in the local comparison workspace. Each encoder received the same RGB page
inputs and produced a bundled multi-page DjVu document with one IW44 `BG44`
chunk per page, `slices = 100`, full chroma, `chroma_delay = 10`, and a release
build. Reconstruction quality is average RGB PSNR after decoding each `BG44`
chunk through the same decoder path.

DjVuLibre was measured through an in-process FFI wrapper around its C++ API
(`GPixmap`, `IW44Image::create_encode`, and `IW44Image::encode_chunk`), not by
spawning the `c44` command-line tool.

| Encoder | Pages | Seconds | Pages/sec | Output bytes | Avg PSNR |
| --- | ---: | ---: | ---: | ---: | ---: |
| djvu-rs | 50 | 5.822 | 8.588 | 6,946,698 | 22.63 dB |
| DJVULibRust | 50 | 4.470 | 11.186 | 5,717,668 | 37.28 dB |
| DjVuLibre | 50 | 3.978 | 12.570 | 5,717,678 | 37.28 dB |
| DJVULibRust page-parallel | 50 | 1.918 | 26.066 | 5,717,668 | 37.28 dB |

Notes:

- DJVULibRust serial output is effectively size-matched and quality-matched
  with DjVuLibre on this IW44 test.
- The page-parallel DJVULibRust run produced a bit-identical file to the serial
  DJVULibRust run while encoding 2.33x faster than serial and 2.07x faster than
  DjVuLibre on this machine.
- Compared with the tested djvu-rs fork, DJVULibRust produced a 17.7% smaller
  file and a 14.65 dB higher average PSNR at this setting.

Benchmark command:

```bash
cargo run --release --manifest-path iw44-bench-harness/Cargo.toml -- \
  --pages 50 --slices 100 --parallel
```

## Performance Model

DJVULibRust gets most of its practical throughput from page independence. A
document can encode page 37 before page 2, insert both safely, then finalize only
when every page slot is ready. This avoids a single global encoder bottleneck
and lets callers decide how aggressively to parallelize page preparation,
compression, OCR, and final insertion.

Within a page, the `rayon` feature can also parallelize selected IW44 color
channel work. That is separate from page-level parallelism and can be combined
with it when the workload and machine have enough cores.

## Feature Flags

| Feature | Description |
| --- | --- |
| `rayon` | Enables internal parallel work in IW44 encoding and supports parallel user pipelines. |
| `portable_simd` | Enables experimental portable SIMD code paths. Requires nightly Rust. |
| `asm_zp` | Enables assembly-backed ZP arithmetic coder paths where available. |
| `dev_asm_cmp` | Enables assembly-vs-Rust ZP comparison tests for development. |
| `iw44-trace` | Verbose IW44 tracing for debugging. |
| `debug-logging` | Extra encoder logging for diagnostics. |

## Current Scope

DJVULibRust is an encoder library. It is intended for applications that already
have image/OCR data in memory and need to assemble DjVu output. It does not try
to replace full command-line document processing suites; format conversion,
image cleanup, OCR, and batch orchestration are expected to live in the calling
application.

## Repository Layout

- `src/doc`: public document/page builder API and DJVM assembly.
- `src/encode/iw44`: IW44 image encoder.
- `src/encode/jb2`: JB2 bitonal encoder and symbol dictionary support.
- `src/iff`: IFF chunk writing and BZZ compression utilities.
- `src/annotations`: hidden text and annotation chunks.
- `examples`: small standalone encoding examples.
- `tests`: codec, document, and validation tests.

## Compatibility

The codebase targets Rust 2024 edition. It is written in portable Rust with
optional feature-gated acceleration paths, and is intended to build on Linux,
macOS, and Windows on common x86_64 and ARM64 targets.

## Related Projects

- [Lege](https://github.com/LegeApp/Lege): PDF-to-DjVu conversion project using
  this library.
- [DjVuLibre](http://djvu.sourceforge.net/): the reference open-source DjVu
  implementation used as a compatibility and benchmark point.

## License

This export does not currently include a `LICENSE` file or Cargo `license`
metadata. Add the intended license file before publishing to crates.io or
depending on this repository from downstream projects.
