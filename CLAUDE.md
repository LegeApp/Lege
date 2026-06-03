# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

Lege is a fully-automatic PDF processor for E-Ink ebook readers. It renders PDF pages, runs YOLO layout detection to classify regions (text, image, table, noise), then re-encodes each page with region-aware compression (JBIG2 for text, JPEG2000/JPEG for images) and writes a new PDF or DJVU file. There is both a CLI (`lege`) and a GUI (`lege-gui`) binary.

## Workspace layout

| Crate | Path | Purpose |
|---|---|---|
| `lege` | `.` (root) | CLI binary + core processing library |
| `lege-gpu` | `lege-gpu/` | wgpu GPU compute: binarization, resize, custom ONNX runtime |
| `Legencode` | `legencode/` | Pure-Rust image encoders: JPEG, JPEG2000, JBIG2, CCITT4 |
| `jbig2enc-rust` | `legencode/src/jbig2enc-rust/` | JBIG2 encoder sub-crate |
| `TooJpeg-rust` | `legencode/src/TooJpeg-rust/` | JPEG encoder sub-crate |
| `lege-gui` | `GUI/Freya/` | Freya/Skia GUI frontend |
| `djvu_encoder` | `djvulibrust/` | Pure-Rust DJVU encoder |

The workspace `default-members` are `.` and `GUI/Freya` — building the workspace builds both the CLI and the GUI.

## Build commands

```sh
# Standard release build (CLI + GUI)
cargo build --release

# Development build (incremental, 256 codegen units)
cargo build

# Debug-logging build (perf tracing, no LTO) — use alias from .cargo/config.toml
cargo build-debug-logging          # alias for: cargo build --profile debug-fast --features debug-logging
cargo run-debug-logging -- <args>  # alias for: cargo run --profile debug-fast --features debug-logging -- <args>

# Run CLI in dev mode
cargo run -- <pdf-file>

# Linux .deb package
cargo deb

# Single crate build
cargo build -p lege-gpu
```

Copy `.cargo/config.example.toml` to `.cargo/config.toml` and fill in local patch paths when working on `freya` or `jp2lam` checkouts.

## Running tests

```sh
# All workspace tests
cargo test

# Tests for a specific crate
cargo test -p djvu_encoder
cargo test -p Legencode

# Single test by name
cargo test -p djvu_encoder jb2_pipeline

# Tests with debug-logging output visible
cargo test --features debug-logging -- --nocapture
```

## Features

| Feature | Effect |
|---|---|
| `debug-logging` | Enables `perf_log!`, `info_log!`, `warn_log!` macros; activates per-region timing |
| `jp2-lam` (default) | Enables JPEG2000 encoding via the `jp2lam` library |
| `static` | Statically links pdfium-render |
| `profiling` | Inherits `debug-logging`; use with `--profile profiling` for flamegraph-compatible builds |
| `debug-layers` | Propagated to `lege-gpu` for wgpu validation layer diagnostics |

## Architecture: PDF processing pipeline

The main processing flow lives in `src/pipeline/`. The two concrete pipeline variants are:

- **`pdf_tokio_pipeline.rs`** — PDF → PDF pipeline (Tokio async, parallel stages)
- **`djvu_pipeline.rs`** — PDF → DJVU pipeline

Both pipelines share the same stage structure:

```
PdfiumPageSource (render pages at high-res + 1024×1024 inference size)
  → inference_stage_parallel (YOLO layout detection via InferenceActor)
  → page processing (classify_page, region masking, binarization, encoding)
  → spawn_pdf_writer_actor (assembles output PDF/DJVU with hOCR text layer)
```

Key files per concern:

| Concern | File |
|---|---|
| Pipeline orchestration | `src/pipeline/pdf_tokio_pipeline.rs`, `djvu_pipeline.rs` |
| Config / shared types | `src/pipeline/config.rs` |
| YOLO inference actor (batching) | `src/pipeline/inference.rs` |
| Page classification & blank detection | `src/pipeline/page_analysis.rs` |
| Region/resize policies | `src/pipeline/policies.rs` |
| Margin analysis | `src/margin.rs` |
| Page rendering (pdfium) | `src/pagerender.rs` |
| Content categories | `src/types.rs` — `ContentCategory` enum drives all downstream decisions |

## Layout detection

`engine.rs` is a thin `include!("engine_yolo_linux.rs")` redirect; the real code is in `src/engine_yolo_linux.rs`. It wraps `lege_gpu::vision::LayoutDetector` which runs a custom ONNX-graph inference on wgpu (DX12 on Windows, Vulkan on Linux, Metal on macOS).

`ContentCategory` (in `src/types.rs`) is the single source of truth for what a detection means downstream — never use raw YOLO class IDs outside of `types.rs`.

## GPU backend (`lege-gpu`)

- **`vision/`** — Custom ONNX runtime: loads `.onnx` protobuf, folds constants, lowers ops to wgpu compute shaders. No `ort`/`tract` dependency.
- **`binarization/`** — wgpu Sauvola binarization kernel.
- **`resize/`** — wgpu image resize (letterbox + direct).

WGSL shaders are either embedded at compile time (`include_str!`) or, with the `no-include-shaders` feature, loaded at runtime.

## Encoding (`legencode`)

`legencode/src/streamline.rs` — the `Streamline::encode` method is the single dispatch point for all encoders. Choose encoder via `EncodingSettings` variants: `Jpeg`, `Jp2`, `Jbig2`, `Ccitt4`.

JBIG2 has two modes (`Jbig2Mode`): `Symbol` (symbol-substitution, best for clean text) and `Generic` (bitplane, required when `ContentCategory::Abandon` is present on a page).

## Runtime assets

ONNX models are found at runtime via `runtime_asset_path` / `runtime_asset_path_if_exists` (in `src/pipeline/config.rs` and `legencode/src/lib.rs`). Search order: exe directory → `models/` subdirectory → `LEGE_DATA_DIR` / `LEGE_ASSET_DIR` env vars → `/usr/share/lege/models` (Linux). Models required: `yolo-layout.onnx`, `paddle-rotate.onnx`, `paddle-deskew.onnx`, `sauvola.onnx`.

## Platform notes

- **Windows**: Uses WinRT OCR (`src/ocr/winocr.rs`), Direct3D12 wgpu backend, Windows API for system dirs.
- **Linux/macOS**: Uses Tesseract OCR, Vulkan/Metal wgpu backend. `libpdfium.so` must be on `LD_LIBRARY_PATH` or next to the binary.
- `src/windows_dirs.rs` / `src/app_dirs.rs` — platform-specific config/data directory resolution.

## Freya fork

The workspace patches `https://github.com/LegeApp/freya.git` to a local path (`../freya-main/`). If that checkout is absent the GUI won't build. The `rfd` (file dialog) crate is also patched to a local clone (`../../../clones/rfd`).

## Debug tracing

- `LEGE_BBOX_TRACE=1` — prints bbox/region/placement lines to stderr.
- `LEGE_RAYON_THREADS=N` — override Rayon thread count (default: CPU count − 1).
- `perf_log!(start, "label")` — zero-cost timing macro; only active with `debug-logging` feature.
