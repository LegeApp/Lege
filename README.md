<div align="center">
  <img src="Screenshot2.png" alt="Lege Interface" width="45%">
  <img src="page_0002-original.png" alt="Lege Processing" width="45%">
</div>

# Lege - 1.4.5
Releases are updated with every new version --> https://github.com/LegeApp/Lege/releases/

Lege is a document-processing program (CLI + desktop GUI) that converts scanned documents into reader-optimized **PDF** or **DjVu**, focusing on **better readability**, **smaller output size**, and **fast page turns** on e-ink devices. It uses optional layout-aware processing to detect image areas so that they can be excluded from the text binarization process, which makes the original scanned documents readable on e-ink readers with small file size.

There are 2 generally intended usages for the program; outputs of commercial book scanning utilities such as image folders of JPEG or PNG, and outputs of the Internet Archive in either PDF or JP2 zip or image folder, since the Internet Archive is the largest digital repository of scanned digital books and documents. If there is something old you want to read on e-ink, it is probably on Archive.org but it has yellowed aged page scans and the size of the book is 500MB. Lege is for those files. Further information is in the in-program documentation file.

---

## Interfaces

* **CLI**: guided interactive mode (no args) + direct command modes
* **GUI**: Freya desktop app using the same processing core; queue-based workflow with progress + cancel

---

## Quick start

### Build (from source)

```bash
git clone https://github.com/LegeApp/Lege.git
cd Lege
cargo build --release
```

You'll get:

* CLI: `target/release/lege`
* GUI: `target/release/lege-gui`

### Run

```bash
# simplest: optimized PDF output
lege input.pdf

# DjVu output (optionally with OCR)
lege input.pdf --output-format djvu --ocr

# process a page range
lege input.pdf --pages 10-50
```

the CLI also supports an interactive guided mode when run without arguments.

---

## Inputs and outputs

### Inputs

* **PDF files** (with optional page range selection)
* **Image-folder mode** for sequential page images (used for batch/page-image workflows)
* Debug modes for exporting rendered pages / crops (useful for model and pipeline inspection)

### Outputs

* **PDF**: mixed region encoding (compressed bi-level text + preserved image regions as overlays)
* **DjVu**: native Rust encoder with **JB2** (bi-level) + **IW44** (continuous-tone) layering

---

### External Dependencies

Lege requires several external files to be placed alongside the executables:

#### Required for all platforms:

**ONNX Models** (AI inference, loaded at runtime):
- `yolo-layout.onnx` — Layout detection (Linux production model)
- `paddle-layout.onnx` — Layout detection (Windows and macOS model)
- `paddle-rotate.onnx` — Page orientation detection
- `paddle-deskew.onnx` — Page deskew correction
- `sauvola.onnx` — Heavy neural binarization model (runs on CPU)

**Platform libraries:**

**Windows:**
- `pdfium.dll` — PDF rendering engine

**Linux:**
- `libpdfium.so` — PDF rendering engine
- `eng.traineddata` — Tesseract English language data (for OCR)

**macOS:**
- `libpdfium.dylib` — PDF rendering engine
- Tesseract language data (system installation)

> GPU inference (layout detection, deskew, rotation) runs through the native **WebGPU** backend built into Lege — no external GPU runtime libraries are required on any platform.

---

# Technical details

## High-level pipeline

Lege is an end-to-end document transformation system with distinct pipelines for PDF and DjVu output.

### Core stages

1. **Render pages** (PDF → images) using PDFium (with thread-safety guardrails).
2. **Layout inference** (optional): run an ONNX layout model at GPU speed via the native wgpu compute runtime; map detections into text-like vs image-like buckets.
3. **Region processing**

   * Text regions: binarize + encode with bi-level codecs (JBIG2 or CCITT4)
   * Image regions: preserve/encode separately; composite as overlays where applicable
   * Optional heavy neural binarization (Sauvola model on CPU) for degraded pages
   * Optional OCR integration at region or page level
4. **Assemble output**

   * PDF writer actor: ordered page finalize into a single PDF
   * DjVu writer actor: out-of-order page submission + multipage finalize

## PDF pipeline vs DjVu pipeline

### PDF pipeline (`src/pipeline/pdf_tokio_pipeline.rs`)

Implemented as a multi-stage async pipeline with bounded channels and configurable concurrency:

* render → inference → CPU page processing → ordered writer/finalizer
* supports page ranges and optional two-pass margin normalization

### DjVu pipeline (`src/pipeline/djvu_pipeline.rs`)

Separate pipeline to match DjVu constraints:

* similar render/inference conceptually
* produces DjVu page payloads submitted to a DjVu writer actor
* supports layered JB2/IW44 output, and optional hidden text

## GPU inference — native wgpu runtime

All AI model inference (layout detection, page rotation, deskew) runs through a **native WebGPU/wgpu compute runtime** compiled into Lege. ONNX models are parsed and lowered to WGSL compute shaders at startup; compiled kernel pipelines are cached per model resolution and reused across pages.

- **Windows**: DX12 backend via wgpu
- **Linux**: Vulkan backend via wgpu
- **macOS**: Metal backend via wgpu

No external inference runtime (ONNX Runtime, DirectML, etc.) is required. The GPU runtime lives in the `lege-gpu` crate (`lege-gpu/src/vision/`).

## Layout detection

Lege can run GPU-accelerated layout detection to segment a page into regions and apply different encoding strategies. When layout detection is disabled, Lege follows a uniform whole-page processing strategy.

## Binarization and image treatment

* **Text-like regions** are typically converted to **1-bit** (bi-level) using adaptive binarization logic in the encoding layer.
* **Image-like regions** can be preserved/encoded separately and overlaid onto the output (so photos/diagrams don't get crushed into 1-bit).
* **Heavy neural binarization** (optional): the Sauvola ONNX model runs on CPU with global instance-normalization statistics, giving high quality results on degraded or difficult pages without GPU tiling artifacts.

## OCR and text layers

OCR is optional:

* **Linux/macOS**: Tesseract (in-process via the `tesseract` Rust crate)
* **Windows**: WinRT OCR

Strategy:

* prefer bounded region OCR when layout segmentation is workable
* fall back to tiled or full-page OCR as needed
* when OCR is disabled, Lege can optionally reuse/extract text from PDFs that already have a text layer to synthesize a text overlay where possible

## Encoding formats and where they're used

Lege uses a dedicated encoding crate (`Legencode`) for in-memory processing and multiple output encoders, and a dedicated native DjVu encoder (`DJVULibRust`) for DjVu generation.

### Bi-level / "text compression" codecs

* **JBIG2** (via a Rust port under `Legencode`)
* **CCITT Group 4** (fax-style bi-level compression)

### Continuous-tone codecs

* **JPEG2000** (used for cover/photo regions in common paths)
* **DjVu IW44** (continuous-tone layer inside DjVu)

## Performance and operability features

* **Concurrent pipeline** with bounded channels/backpressure and adaptive per-job concurrency
* **Resident compiled GPU graphs**: model kernels are compiled once at startup and reused across all pages — no per-page GPU recompilation
* **Cancellation + progress tracking** shared by CLI and GUI
* Runtime dependency discovery (models/libs) via executable-adjacent paths, env vars, and platform fallback dirs

---

## Workspace layout

Lege is a Rust workspace with multiple crates:

* `src/` — main app + pipeline orchestration (CLI core)
* `lege-gpu/` — GPU compute: image resize, adaptive binarization, and the native wgpu ONNX inference runtime (`lege-gpu/src/vision/`)
* `Legencode/` — encoding + binarization + region utilities
* `DJVULibRust/` — native DjVu encoder crate
* `GUI/Freya/` — desktop GUI frontend

---

## License

AGPL-3.0. See `LICENSE`. Third-party licenses are documented under `docs/`.

---
