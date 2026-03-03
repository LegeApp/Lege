# Lege: Technical Project Overview (LLM-Ready)

## Scope of this overview
This document summarizes the implemented functionality and architecture of the `Lege` workspace for README reconstruction.

Included: all project folders and code paths except `ort/` and `windows-rs-71/`.
Excluded by request: ONNX Runtime fork internals (`ort`) and local Windows bindings fork (`windows-rs-71`).

## What the program does
Lege is a Rust document-processing application that transforms scanned PDFs (and page-image folders) into reader-optimized output, with an emphasis on e-ink readability and file-size reduction.

At runtime, it combines:
- PDF rasterization and optional text-layer reuse.
- ML layout detection (to separate text-like and image-like regions).
- Region-aware binarization/encoding.
- Optional OCR text-layer generation.
- Output assembly as either optimized PDF or native DjVu.

Primary end-user interfaces:
- CLI (`src/main.rs`) with guided interactive flow and direct command modes.
- Desktop GUI (`GUI/Dioxus`) using the same processing core.

## User-visible feature set

### Input handling
- PDF input processing with optional page range selection.
- Image-folder input mode for sequential page images.
- Debug/data-generation modes for exporting rendered pages and cropped regions.

### Output modes
- PDF output with mixed content encoding (binary text base + image overlays where applicable).
- DjVu output via internal native Rust encoder (`DJVULibRust`) with IW44 and JB2 layering.

### Content processing features
- Layout-aware processing using ONNX inference (PaddleX-style document detector).
- Optional layout-disabled path that treats full pages more uniformly.
- Adaptive text binarization (Sauvola/Otsu-style logic via `Legencode`), plus optional no-binarization mode.
- Optional dithering behaviors for image/halftone handling.
- Optional OCR layer insertion:
  - Linux/macOS: Tesseract backend.
  - Windows: WinRT OCR backend.
- Margin workflows:
  - No margin adjustment.
  - Standardize-and-center.
  - Crop-and-resize.
- Optional deskew / orientation correction path.
- Target-size/device profile support (named e-ink presets and custom dimensions).

### Workflow and operability features
- Concurrent tokio pipeline with distinct stages and backpressure.
- Cancellation signaling and progress tracking shared by CLI and GUI.
- Runtime dependency/model discovery via executable-adjacent paths, environment variables, and platform fallback directories.
- System status reporting (OCR availability, hardware/resource checks, provider state).

## End-to-end architecture

### 1) Entry and configuration
- `src/main.rs` parses direct CLI args or launches an interactive wizard when no args are provided.
- `PipelineConfig` (`src/pipeline/config.rs`) is the central runtime control object for:
  - Output format and encoding policy.
  - OCR and language selection.
  - Layout detection and margin mode.
  - Deskew, batching, concurrency, retries, and page range.

### 2) Rendering and analysis
- PDF rendering is done through PDFium (`src/pagerender.rs`) with thread-safety guardrails.
- Each page gets a high-resolution render and a lower-resolution inference image.
- Optional document-wide low-res prepass computes baseline margin bounds for more consistent multi-page output.

### 3) Inference and classification
- `src/engine.rs` runs layout model inference and detection parsing/NMS.
- `src/types.rs` maps detection classes into text-like vs image-like behavior buckets.
- Inference execution uses actor/pool abstractions (`src/pipeline/inference.rs`) and platform-specific provider selection (`src/gpu.rs`).

### 4) Region processing
- Text-oriented regions are binarized and prepared for highly compressed binary encoders.
- Image-oriented regions can be preserved/encoded separately and overlaid.
- OCR is performed region-first when feasible, with tile/full-page fallback if region strategy is unsuitable.
- When OCR is disabled, PDF text extraction can be used to synthesize an HOCR-style text layer when available.

### 5) Output assembly
- PDF path:
  - Staged page processing feeds a writer actor (`src/pipeline/helper_functions.rs`, `src/accumulator.rs`).
  - Pages are buffered/ordered and finalized into a single PDF.
- DjVu path:
  - Separate standalone pipeline (`src/pipeline/djvu_pipeline.rs`) produces `PageData` and submits pages to a DjVu writer actor.
  - `src/djvu.rs` orchestrates page encoding into JB2/IW44 layers and optional hidden text.

## Pipeline implementations

### PDF pipeline (`src/pipeline/pdf_tokio_pipeline.rs`)
Implemented as multi-stage async processing with bounded channels and configurable concurrency:
- Render stage.
- Inference stage.
- CPU-intensive page processing stage.
- Ordered writer/finalizer stage.

Notable behavior:
- Supports page-range filtering.
- Can perform two-pass margin normalization.
- Handles cover-page policy separately from interior pages.
- Supports mixed region encoding (binary base + image overlays).
- Integrates OCR/text-layer synthesis into page assembly.

### DjVu pipeline (`src/pipeline/djvu_pipeline.rs`)
Implemented separately from PDF path to preserve DjVu-specific constraints:
- Render and inference stages similar in concept to PDF path.
- Binarization/text extraction stage prepares DjVu page payloads.
- Submission/writer stage appends out-of-order pages and finalizes multipage DjVu.

DjVu-specific behavior from `src/djvu.rs`:
- Normal mode: JB2-style binary text layer with optional IW44 image/background regions.
- No-binarization mode: full-page IW44 encoding.
- Optional hidden text from HOCR parsing.
- Region white-out logic to avoid text bleed where image regions are composited.

## OCR subsystem

### Backend selection
- `src/ocr/mod.rs` routes to platform-specific backends.
- `src/ocr/tesseract.rs` handles Tesseract invocation and language-data discovery.
- `src/ocr/winocr.rs` implements Windows OCR integration.

### OCR strategy
- Attempts bounded region OCR first when layout segmentation is manageable.
- Falls back to tiled OCR or full-page OCR when needed.
- Corrects and stitches OCR coordinates when images are resized/downsampled.
- Exposes availability checks and diagnostics for runtime status reporting.

## Encoding and image-processing stack

### `Legencode` crate role
`Legencode` is the in-memory encoding and image-processing layer used by the main app:
- Unified encoding management API (`streamline.rs`).
- Encoders for:
  - JBIG2 (`src/jbig2enc-rust`).
  - CCITT Group 4 (`fax` crate-based path).
  - JPEG (`src/TooJpeg-rust` integration).
- Binarization and color-processing utilities (`src/color`, `src/encoders`, `src/image_types.rs`).
- Region operations and dithering helpers used by page processing.

### About JP2/JPEG2000 in this codebase
- The repository contains substantial JPEG2000/openjp2 code under `Legencode/src/openjp2` and JP2 configuration logic in `streamline.rs`.
- In current main pipeline code paths, active region/cover encoding branches primarily use JPEG, CCITT4, or JBIG2 with fallback behavior.
- Inference from current entry-path matches: JP2 appears partially legacy/experimental in top-level processing UX and not the primary active path for main PDF/DjVu production.

## Native DjVu encoder crate (`DJVULibRust`)
`DJVULibRust` provides the `djvu_encoder` crate consumed by the main app.

Core capabilities:
- Thread-safe `DjvuBuilder` / `PageBuilder` APIs for page assembly.
- Multipage DjVu document finalization (including DJVM packaging flows).
- JB2 encoding for bi-level layers.
- IW44 encoding for continuous-tone layers.
- Chunk-level IFF/DjVu composition (INFO, Sjbz, BG44, FGbz, TXTz, ANTz, etc.).
- Hidden text and annotation/hyperlink chunk support.
- Tests and examples for low-level codec and document assembly scenarios.

## GUI application (`GUI/Dioxus`)
The Dioxus desktop front-end wraps the same backend processing engine and exposes it with queue-oriented UX.

Implemented GUI capabilities:
- Add individual PDFs and folder-based inputs (including page-image folders).
- Maintain a queue and process items serially with progress and status text.
- Start/cancel processing and reflect cancellation state.
- Configure output format (PDF/DjVu) and processing options (OCR, layout behavior, margins, compatibility mode, target dimensions).
- Select device presets via shared target profile data.
- Save/load/reset settings to user data directory JSON.
- Show OCR-detection prompt for incoming PDFs with existing text layer.
- Open output folders in system file explorer.
- Windows-only Microsoft Store in-app consumable donation integration (`store_iap.rs`).

## CLI surface and modes

### Standard CLI modes
- Guided interactive workflow (no args).
- Direct conversion command path (`lege <file.pdf> [page-range] [target]`).
- Help/status/license/target-list commands.

### Advanced/debug modes
- PDF to PNG export mode (`--pdf-to-png`).
- Layout crop-debug dataset generation (`--crop-areas`).
- PNG/image-folder processing mode (`--png-folder`).
- Linux OCR language override flag (`--language <tesseract_code>`).

## Progress, cancellation, and observability
- `src/progress.rs` defines status lifecycle models used by both CLI and GUI.
- Tracker APIs report stage transitions, page counts, and completion summaries.
- Processing log support (`src/processing_log.rs`) records option snapshots and output context.
- Cancellation propagation uses explicit shutdown signals across async tasks.

## Runtime assets and dependency discovery
- Runtime search helpers in `src/lib.rs` and pipeline config resolve required files (models, OCR data, dynamic libraries) from:
  - executable-local directories,
  - configured environment variables (`LEGE_DATA_DIR`, `LEGE_ASSET_DIR`, etc.),
  - platform fallback directories.
- PDFium and OCR assets are discovered/validated at startup or first-use checks.

## Folder-by-folder technical map (excluding `ort` and `windows-rs-71`)

### `/src`
Main CLI application and processing engine.
- Pipeline orchestration (`pipeline/*`), rendering, inference, OCR, margins, deskew, resizing, progress, and output assembly.
- Includes specialized debug modes (`pdf_to_png`, `pnginference`) and DjVu orchestration (`djvu.rs`).

### `/Legencode`
Workspace image-encoding and preprocessing library.
- Unified encoding API, binarization/color tooling, and local codec subprojects (`jbig2enc-rust`, `TooJpeg-rust`).
- Contains vendored/ported `openjp2` sources and JP2-related modules.

### `/DJVULibRust`
Native Rust DjVu encoder crate (`djvu_encoder`).
- Low-level codecs, chunk assembly, multipage document builder, annotations/text support, tests/examples.

### `/GUI/Dioxus`
Desktop GUI crate.
- Dioxus app state, backend bridge to `lege` pipeline, persistent settings, theming/styles, queue processing UI.

### `/docs`
Project documentation and analysis artifacts.
- Product/technical notes, OCR-language handoff documentation, third-party licensing docs.

### `/assets`
Branding/application assets (icons and static media used by app/packaging).

### `/dev-misc`
Developer and release utility scripts/tools.
- Build orchestration scripts, release helpers, cleanup/copy tools.

### `/scripts`
Repo utility script(s), including staged-file allowlist sync helper.

### `/macos-files`
macOS-focused auxiliary snapshots/config experiments (`macos-Cargo.toml`, `macos-engine.rs`).
- Appears support/porting reference material rather than primary runtime path.

### Root-level build/config files
- `Cargo.toml`, `build.rs`, `Cargo.lock`, desktop entry, license/readme, and packaging metadata.

### Dev-environment folders
- `.vscode`, `.vs`: editor/IDE metadata; not part of runtime functionality.

## Behavior and design characteristics worth preserving in a rewritten README
- The project is not just an encoder library; it is an end-to-end document transformation system with both CLI and GUI fronts.
- The two output backends (PDF and DjVu) are architecturally distinct at pipeline level.
- The strongest technical differentiators are region-aware processing, mixed encoding strategies, and native Rust DjVu generation.
- OCR is optional and backend-dependent by platform; text-layer reuse from source PDFs is also part of the flow.
- The workspace is multi-crate by design (`lege`, `Legencode`, `djvu_encoder`, GUI app), with shared configuration/progress abstractions.

## Caveats and inferred points
- Inferred from current code-path inspection: JP2/JPEG2000 support is present in repository modules but is not the dominant active branch in top-level processing code compared with JPEG/CCITT4/JBIG2 and DjVu IW44/JB2 paths.
- Inferred from folder contents: some subtrees (e.g., `macos-files`, legacy/to-do content in `DJVULibRust`) appear auxiliary or historical and not on the primary execution path.
