# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What Lege Does

Lege is a document processing application that transforms scanned PDFs and image folders into reader-optimized output for e-ink devices. It runs a multi-stage pipeline: render PDF pages via PDFium → optional ONNX layout detection → region-aware binarization/encoding → assemble output PDF or DjVu. Both a CLI and a Freya-based desktop GUI are provided.

## Workspace Structure

| Crate | Path | Role |
|---|---|---|
| `lege` | `src/` | Core pipeline engine + CLI entry point |
| `lege-gui` | `GUI/Freya/` | Desktop GUI (Freya/Skia) |
| `legencode` | `Legencode/` | Encoding and binarization (JBIG2, CCITT4, JPEG, JP2) |
| `djvu_encoder` | `DJVULibRust/` | Native Rust DjVu encoder (JB2 + IW44 codecs) |

Local forks in the workspace: `ort/` (ONNX Runtime), `windows-rs-71/` (Windows API bindings).

## Commands

```bash
# Build
cargo build                          # debug build
cargo build --release                # optimized release
cargo build --features debug-logging # enable perf/debug macros
cargo build --profile profiling --features debug-logging  # flamegraph profiling

# GUI only
cargo build -p lege-gui --release

# Test
cargo test

# Lint / format
cargo clippy
cargo fmt
```

## Pipeline Architecture

**PDF output path** (`src/pipeline/pdf_tokio_pipeline.rs`):
1. **Render** — PDFium renders pages to high-res RGB + 640×640 inference image
2. **Inference** — ONNX layout model detects text/image regions (optional, layout detection flag)
3. **Process** — Region-aware: text regions → adaptive binarization → JBIG2/CCITT4; image regions → JPEG/IW44
4. **Accumulate + finalize** — In-order page assembly into output PDF

**DjVu output path** uses a separate `src/pipeline/djvu_pipeline.rs` that submits pages out-of-order to a DjVu writer actor producing JB2 + optional IW44 layers.

`PipelineConfig` (`src/pipeline/config.rs`) is the central runtime control struct passed through all stages.

## Key Architectural Decisions

- **Two separate pipelines**: PDF and DjVu are independent despite similar stages — DjVu has format-specific ordering and layer constraints.
- **Inference image is square (640×640)**: Layout model requires fixed-size input; detections are mapped back to original page coordinates.
- **Region processing requires layout mode**: In no-layout (whole-page) mode, dithering and per-region encoding are disabled.
- **Adaptive concurrency**: `AdaptiveConcurrency` in `src/pipeline/runtime_limits.rs` adjusts worker count based on free RAM; semaphores gate encoding to prevent memory exhaustion.
- **`libc::_exit()` on CLI exit**: Bypasses atexit handlers that can segfault during WebGPU/Vulkan teardown on Linux.
- **Asset discovery order**: exe directory → parent dirs + unix share paths → `LEGE_DATA_DIR`/`LEGE_ASSET_DIR` env vars → system paths (`/usr/lib/lege`, etc.) → cwd. Required assets: `*.onnx` models, `libpdfium.*`, `libonnxruntime.*`, `eng.traineddata` (Linux OCR).

## Platform Differences

| Feature | Linux | Windows | macOS |
|---|---|---|---|
| GPU inference | WebGPU via ONNX Runtime (`engine_yolo_linux.rs`) | DirectML | CoreML in staged macOS engine (`macos-files/macos-engine.rs`) |
| Layout model | YOLO (`engine_yolo_linux.rs`) | PaddleX (`engine.rs`) | PaddleX in staged macOS engine |
| OCR | In-process libtesseract via `tesseract` crate | WinRT OCR | In-process libtesseract in staged macOS code |
| Resize shaders | WGSL/WebGPU (`src/resize/wgpu.rs`, `src/resize/wgpu/shaders/`) | HLSL/DX12 compiled at build time (dxc.exe) | WGSL/WebGPU path exists in shared resize module |

Notes:
- The active resize module compiles the WGPU backend on Linux/macOS and attempts it before `fast_image_resize` CPU fallback. The WGSL shaders under `src/resize/wgpu/shaders/` are active code, not unused assets.
- Linux OCR recognition uses the `tesseract` Rust crate and libtesseract in-process. The current availability check still shells out to `tesseract --version`; do not confuse that probe with the OCR execution path.
- macOS is currently staged outside the main workspace under `macos-files/`; that staged engine attempts CoreML first and falls back to CPU.

## Feature Flags

- `jp2-lam` (default) — JPEG2000 support via pure-Rust jp2lam
- `debug-logging` — enables `perf_log!`, `info_println!`, `warning` macros
- `profiling` — release + debug symbols profile (defined in Cargo.toml)

## Encoding Dispatch (Legencode)

`Legencode/src/streamline.rs` is the unified encoder entry point. It detects image type (bilevel/grayscale/RGB) and routes to: JBIG2 (`jbig2enc-rust`), CCITT Group 4 (`fax` crate), JPEG (`TooJpeg-rust`), or JPEG2000 (`jp2lam`). Returns `EncodingResult::Standard` or `EncodingResult::JbigWithGlobals`.

## Debugging Tools

- `LEGE_BBOX_TRACE=1` — stderr output for layout bounding boxes and region encoding decisions
- `--pdf-to-png` CLI flag — export rendered pages to PNG for inspection
- `--png-folder <dir>` — process an image folder as document pages (bypasses PDF rendering)
- `--crop-areas` — export detected regions as images (layout detection QA)

## GUI ↔ Backend Bridge

`GUI/Freya/src/backend.rs` contains `gui_options_to_pipeline_config()` which converts `ProcessingOptions` (GUI state, `GUI/Freya/src/models.rs`) into `PipelineConfig`. When adding new pipeline options, both sides need updating.

## CLI Text

CLI-facing strings live in `src/cli_text.json` (loaded via `src/text_loader.rs`). GUI strings are in `GUI/Freya/src/gui_text.rs`.
