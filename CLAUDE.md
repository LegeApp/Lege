# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

Lege is a fully-automatic PDF processor for E-Ink ebook readers. It renders PDF pages, runs YOLO layout detection to classify regions (text, image, table, noise), then re-encodes each page with region-aware compression (JBIG2 for text, JPEG2000/JPEG for images) and writes a new PDF or DJVU file. There is both a CLI (`lege`) and a GUI (`lege-gui`) binary.

## Workspace layout

| Crate | Path | Purpose |
|---|---|---|
| Ecosystem workspace | `Cargo.toml` (root) | Virtual workspace, shared dependencies/profiles/patches, and build aliases |
| `lege` | `lege-process/` | CLI binary + core processing library |
| Process crate roots | `lege-process/core/{lib,main}.rs` | Rust library and binary roots |
| Process modules | `lege-process/{pipeline,encoding,color,colorquant,ocr,reflow,resize}/` | Major processing subsystems wired from `core/lib.rs` with `#[path]` |
| `lege-gpu` | `lege-gpu/` | wgpu GPU compute: binarization, resize, custom ONNX runtime |
| `lege-ocr` | `lege-ocr/` | Shared OCR library + `lege-ocr-debug` |
| `lege-pdf-write` | `lege-pdf/write/` | Typed append-only PDF writer |
| Future renderer | `lege-pdf/render/` | Reserved for the renderer's independent workspace |
| `jbig2enc-rust` | `lege-codecs/jbig2enc-rust/` | In-tree JBIG2 encoder/decoder (patched into `lege`) |
| `jp2lam` | `lege-codecs/jp2lam/` | In-tree JPEG2000 codec (patched into `lege`) |
| `djvu_encoder` | `lege-codecs/djvulibrust/` | Standalone AGPL `djvu-encoder`; invoked as a subprocess, never linked |
| `lege-gui` | `lege-process/GUI/Freya/` | Freya/Skia GUI frontend |
| `lege-music-gui` | `lege-process/GUI/musicsheet/` | Sheet Music Edition GUI |
| Shared assets/tools | `lege-misc/` | Icons, packaging, scripts, docs, dev utilities, and `xtask` |

The workspace default members are `lege-process` and
`lege-process/GUI/Freya`, so plain `cargo build` builds the CLI and main GUI.
The vendored Freya workspace and standalone codec projects retain their own
build graphs; root patches make the processor use the in-tree linked codecs.

## Build commands

```sh
# Standard release build (CLI + GUI)
cargo build --release

# Development build (incremental, 256 codegen units)
cargo build

# Fast release-optimized build for iteration/troubleshooting (PREFER THIS over
# --release when testing changes): the debug-fast profile inherits release but
# disables fat LTO, which alone takes up to ~15 minutes on Windows.
# Binary lands in target/debug-fast/.
cargo build --profile debug-fast -p lege --bin lege

# Debug-logging build (perf tracing, no LTO) — use alias from .cargo/config.toml
cargo build-debug-logging          # alias for: cargo build --profile debug-fast --features debug-logging
cargo run-debug-logging -- <args>  # alias for: cargo run --profile debug-fast --features debug-logging -- <args>

# Run CLI in dev mode
cargo run -- <pdf-file>

# Linux .deb package
cargo deb

# Single crate build
cargo build -p lege-gpu

# Convenient aliases for each sub-program
cargo process
cargo gui
cargo music-gui
cargo gpu
cargo ocr
cargo pdf-write
cargo jbig2
cargo jp2
cargo djvu

# Check every ecosystem workspace member
cargo ecosystem-check
```

Normal root Cargo commands already use the in-tree `jp2lam` and
`jbig2enc-rust` sources through root `[patch]` entries. The compatibility helper
is now `lege-misc/scripts/cargo-local.sh`.

## Important 

Do not treat repo-wide `cargo fmt` output as unwanted churn. If `cargo fmt` modifies files outside the files directly edited by the agent, keep those formatting changes unless the user explicitly asks for a minimal diff.

Never run `git checkout --`, `git restore`, `git reset`, or any other command that discards uncommitted changes unless the user explicitly requests it. If the working tree contains unexpected modified files, report them in the final summary instead of reverting them.

Before any destructive git operation, stop and ask. The only exception is deleting temporary files created by the agent itself, such as logs under `/tmp`.

## Running tests

```sh
# All workspace tests
cargo test

# Tests for a specific crate
cargo test -p djvu_encoder
cargo test -p lege

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

The main processing flow lives in `lege-process/pipeline/`. The two concrete pipeline variants are:

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
| Pipeline orchestration | `lege-process/pipeline/pdf_tokio_pipeline.rs`, `djvu_pipeline.rs` |
| Config / shared types | `lege-process/pipeline/config.rs` |
| YOLO inference actor (batching) | `lege-process/pipeline/inference.rs` |
| Page classification & blank detection | `lege-process/pipeline/page_analysis.rs` |
| Region/resize policies | `lege-process/pipeline/policies.rs` |
| Margin analysis | `lege-process/core/margin.rs` |
| Page rendering (pdfium) | `lege-process/core/pagerender.rs` |
| Content categories | `lege-process/core/types.rs` — `ContentCategory` drives downstream decisions |

## Layout detection

`lege-process/core/engine.rs` is a thin `include!("engine_impl.rs")` redirect to
`lege-process/core/engine_impl.rs` (the `LayoutEngine` wrapping
`lege_gpu::vision::LayoutDetector`, PP-DocLayout PicoDet on wgpu).

`ContentCategory` (in `lege-process/core/types.rs`) is the single source of truth for what a detection means downstream — never use raw layout class IDs outside of `types.rs`.

## GPU backend (`lege-gpu`)

- **`vision/`** — Custom ONNX runtime: loads `.onnx` protobuf, folds constants, lowers ops to wgpu compute shaders. No `ort`/`tract` dependency.
- **`binarization/`** — wgpu Sauvola binarization kernel.
- **`resize/`** — wgpu image resize (letterbox + direct).

WGSL shaders are either embedded at compile time (`include_str!`) or, with the `no-include-shaders` feature, loaded at runtime.

## Encoding

`lege-process/encoding/streamline.rs` contains the single dispatch point for all
encoders. Choose an encoder with the `EncodingSettings` variants `Jpeg`,
`Jp2Lam`, `Jbig2`, and `Ccitt4`.

JBIG2 has two modes (`Jbig2Mode`): `Symbol` (symbol-substitution, best for clean text) and `Generic` (bitplane, required when `ContentCategory::Abandon` is present on a page).

## DjVu encoding (arms-length GPL subprocess)

The DjVu encoder (`djvu_encoder` crate in `djvulibrust/`) is GPL. Lege does **not**
link it as a library — it is a separate program invoked over the command line so
the proprietary build stays at arms length. `lege-process/core/djvu.rs` is a subprocess driver:
it composes each page's layers (bilevel ink mask, IW44 background canvas, OCR word
boxes), writes them to the work dir as ordinary files (PNG masks/backgrounds, JSON
word boxes) plus a neutral `manifest.json`, then spawns `djvu-encoder
encode-document --manifest … --output … --progress-json`, streaming progress back
into the `ProgressTracker`. Both `djvu_pipeline.rs` and `reflow_pipeline.rs` use
this via `spawn_djvu_writer_actor`.

- Build the encoder: `cargo build -p djvu_encoder --features cli --bin djvu-encoder`
  (behind the `cli` feature; the library build stays lean). It also has a simple
  standalone mode: `djvu-encoder encode PAGE.png… -o out.djvu [--photo|--bilevel]`.
- Resolution order (`djvu::resolve_encoder_path`): `--djvu-encoder-path` flag →
  `LEGE_DJVU_ENCODER` env → next to the `lege` executable → `PATH`. DJVU jobs
  fail fast at preflight if it is not found (PDF output is unaffected).
- Manifest schema version is `MANIFEST_SCHEMA_VERSION` (1); keep the driver in
  `lege-process/core/djvu.rs` and the reader in
  `lege-codecs/djvulibrust/src/bin/djvu-encoder.rs` in sync.

## Runtime assets

ONNX models are found at runtime via `runtime_asset_path` /
`runtime_asset_path_if_exists` in `lege-process/pipeline/config.rs`. Search order: exe
directory → `models/` subdirectory → `LEGE_DATA_DIR` / `LEGE_ASSET_DIR` env
vars → `/usr/share/lege/models` (Linux). Models required:
`yolo-layout.onnx`, `sauvola.onnx`.

## Platform notes

- **Windows**: Uses WinRT OCR (`lege-process/ocr/winocr.rs`), Direct3D12 wgpu backend, Windows API for system dirs.
- **Linux/macOS**: Uses Tesseract OCR, Vulkan/Metal wgpu backend. `libpdfium.so` must be on `LD_LIBRARY_PATH` or next to the binary.
- `lege-process/core/windows_dirs.rs` / `app_dirs.rs` — platform-specific config/data directory resolution.

## Freya fork

The workspace patches `https://github.com/LegeApp/freya.git` to a local path (`../freya-main/`). If that checkout is absent the GUI won't build. The `rfd` (file dialog) crate is also patched to a local clone (`../../../clones/rfd`).

## Debug tracing

- `LEGE_BBOX_TRACE=1` — prints bbox/region/placement lines to stderr.
- `LEGE_RAYON_THREADS=N` — override Rayon thread count (default: CPU count − 1).
- `perf_log!(start, "label")` — zero-cost timing macro; only active with `debug-logging` feature.
