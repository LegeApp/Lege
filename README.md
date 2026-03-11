<div align="center">
  <img src="Screenshot 2026-02-18 105750.png" alt="Lege Interface" width="45%" style="margin-right: 2%">
  <img src="page_0002-original.png" alt="Lege Processing" width="45%">
</div>

# Lege

> **Fully automatic PDF and image processor optimized for E-Ink ebook readers**

[![License](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
[![Build Status](https://img.shields.io/badge/build-passing-brightgreen.svg)]()
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey.svg)]()

Lege (pronounced *Layg-ay*, from the Latin verb *legere* - "to read") is a document processing tool that transforms scanned PDFs and image collections into optimized files for E-Ink readers. Using machine learning-based layout detection, adaptive binarization, and specialized compression algorithms, Lege produces smaller files with improved readability.

---

## ≡ƒîƒ Key Features

### Output Formats
- **PDF Output** - Universal compatibility with all readers
- **DjVu Output** - Superior compression and faster loading for compatible readers (recommended for Kobo + KoReader)
- **OCR Text Layer** - Optional searchable text using Tesseract (Linux/macOS) or Windows OCR

### Processing Features
- **Layout Detection** - PaddleX machine learning model identifies and preserves 21 different layout elements (text, images, tables, equations, etc.)
- **Adaptive Binarization** - Sauvola/Otsu fusion algorithm converts text to 1-bit while preserving image quality
- **Image Encoding** - Automatic format selection per region:
  - **JBIG2** for text and dithered images (custom halftone dithering)
  - **CCITT Group 4** for original images and legacy compatibility
  - **JPEG/JP2** for cover images and photo regions
- **Dithering** - Bayer dithering for CCITT4, Stucki for JBIG2 halftones
- **Margin Correction** - Automatic cropping and centering based on layout detection or algorithmic analysis
- **Deskew & Auto-Rotate** - Machine learning-based page correction for scanned documents
- **Target Profiles** - Pre-configured output dimensions for popular E-Ink devices (Kindle, Kobo, reMarkable, etc.)

### User Interfaces
- **CLI** - Fast, scriptable command-line interface with rich progress display
- **GUI** - Modern Dioxus-based graphical interface for desktop use

### Input Support
- **PDF files** - Direct PDF processing with text layer preservation
- **Image folders** - Batch processing of sequentially numbered image files (PNG, JPEG, TIFF)

---

## ≡ƒÅù∩╕Å Architecture

Lege is built as a modular Rust workspace with three main components:

### `/src` - Main CLI Application
The core processing engine and command-line interface. Includes:
- Pipeline orchestration (Tokio-based async processing)
- PDF rendering (PDFium integration)
- Page processing and encoding
- OCR integration (platform-specific)
- Progress tracking and logging

### `/Legencode` - Unified Encoding Library
In-memory image encoding library with pure Rust implementations:
- **JPEG** - via local `TooJpeg-rust` implementation
- **JPEG 2000** - via local `openjp2` bindings
- **JBIG2** - via local `jbig2enc-rust` port (symbol and generic modes)
- **CCITT Group 4** - via `fax` crate
- **PNG** - via `png` crate
- Adaptive binarization and color processing modules

### `/DJVULibRust` - Native DjVu Encoder
**Major recent addition** - Pure Rust DjVu encoder replacing the legacy DjVuLibre dependency:
- Thread-safe coordinate-based API
- IW44 wavelet compression for color/grayscale backgrounds
- JB2 compression for bilevel text/graphics layers
- Out-of-order page processing support
- Zero external dependencies (previously required DjVuLibre C library)

### `/GUI/Dioxus` - Desktop GUI
Cross-platform desktop interface built with Dioxus framework:
- File browser integration
- Real-time progress display
- Settings persistence
- Payment integration (Microsoft Store builds)

---

## ≡ƒÜÇ Quick Start

### Prerequisites

**Required:**
- Rust 1.70+ (2024 edition recommended)
- Cargo build system

**Platform-specific:**
- **Windows**: DirectX 12 for DirectML acceleration
- **Linux**: Vulkan drivers for WebGPU acceleration
- **macOS**: Metal support for CoreML acceleration

### Build

```bash
# Clone the repository
git clone https://github.com/LegeApp/Lege.git
cd Lege

# Build release binaries (CLI + GUI)
cargo build --release

# CLI binary will be at: target/release/lege
# GUI binary will be at: target/release/lege-gui
```

### External Dependencies

Lege requires several external files to be placed alongside the executables:

#### Required for all platforms:

**ONNX Models** (AI inference):
- `paddle-layout.onnx` - Layout detection (21 element types)
- `paddle-rotate.onnx` - Page orientation detection
- `paddle-deskew.onnx` - Page deskew correction
- `sauvola.onnx` - Adaptive binarization

**Platform-specific GPU libraries:**

**Windows:**
- `DirectML.dll` - DirectML acceleration provider
- `onnxruntime.dll` - ONNX Runtime main library
- `onnxruntime_providers_shared.dll` - Shared provider library
- `pdfium.dll` - PDF rendering engine

**Linux:**
- `libonnxruntime.so` - ONNX Runtime
- `libonnxruntime_providers_shared.so` - Provider library
- `libwebgpu_dawn.so` - WebGPU/Vulkan backend
- `libpdfium.so` - PDF rendering engine
- `eng.traineddata` - Tesseract English language data (for OCR)

**macOS:**
- `libonnxruntime.dylib` - ONNX Runtime
- `libpdfium.dylib` - PDF rendering engine
- Tesseract language data (system installation)

#### Reference Build

All required external files are maintained in the Microsoft Store build directory:
```
D:\Lege\Lege-MSIX
```

This directory contains a complete working set of all dependencies for the Windows build. Linux and macOS builds require platform-specific equivalents.

---

## ≡ƒôû Usage

### CLI Examples

```bash
# Basic usage - process PDF to optimized PDF
lege input.pdf

# Output as DjVu with OCR layer
lege input.pdf --output-format djvu --ocr

# Process specific page range
lege input.pdf --pages 10-50

# Target specific device (e.g., Kobo Libra 2)
lege input.pdf --profile kobo-libra-2

# Process image folder
lege /path/to/images --output output.pdf

# Custom target dimensions
lege input.pdf --target-height 1920

# Disable layout detection (treat entire page as text)
lege input.pdf --no-layout-detection

# Use original images (no dithering)
lege input.pdf --no-dither
```

### GUI Usage

Launch the GUI:
```bash
./lege-gui
```

The GUI provides an intuitive interface for:
- File selection (drag & drop support)
- Output format selection (PDF/DjVu)
- OCR toggle
- Layout detection settings
- Margin correction options
- Target device profiles
- Real-time progress tracking

---

## ≡ƒöº Advanced Configuration

### Binarization Methods

Lege supports multiple binarization strategies:

- **Adaptive (Default)** - Sauvola/Otsu fusion, best for most documents
- **Threshold** - Simple threshold at specified level
- **None** - No binarization (preserve original pixel data)

### Image Processing Modes

- **Dithered** - Reduces images to 1-bit with error diffusion (Stucki for JBIG2, Bayer for CCITT4)
- **Original** - Preserves image quality in detected image regions

### Cover Page Handling

- First page automatically treated as cover (encoded as JPEG or JP2)
- Cover processing disabled when using page ranges
- Configurable cover format: JPEG (smaller) or JP2 (better quality)

### Margin Correction

- **Center** - Equalizes margins around content
- **Crop** - Removes maximum margin while preserving aspect ratio
- **Algorithm-based** - Works without layout detection
- **Forced margin** - Override automatic detection

---

## ≡ƒôª Distribution Packages

### Windows
- **Microsoft Store MSIX** - Fully packaged with all dependencies
- **Standalone EXE** - Requires manual DLL placement

### Linux
- **Debian Package (.deb)** - Installs to `/usr/lib/lege` with wrapper scripts
- **Standalone Binary** - Requires `libonnxruntime`, `libpdfium`, `libwebgpu_dawn`, and Tesseract

### macOS
- **Standalone Binary** - Requires Tesseract system installation

---

## ≡ƒÅ¢∩╕Å Technical Details

### Core Technologies

- **Language**: Rust (edition 2024)
- **PDF Rendering**: PDFium (via `pdfium-render` crate)
- **ML Inference**: ONNX Runtime with DirectML (Windows), WebGPU (Linux), CoreML (macOS)
- **GUI Framework**: Dioxus 0.6
- **Async Runtime**: Tokio
- **Image Processing**: Custom implementations + `fast_image_resize`
- **Compression**: JBIG2, CCITT4, JPEG, JPEG 2000, IW44 (DjVu)

### Performance Optimizations

- **Parallel Processing** - Rayon-based multi-threaded pipeline
- **GPU Acceleration** - Platform-specific ML inference
- **LTO & Optimization** - Fat LTO, opt-level 3 in release builds
- **Memory Efficiency** - Streaming compression, minimal allocations
- **Custom Allocator** - Optional mimalloc support (commented out for compatibility)

### Major Recent Changes

**DjVuLibRust Integration** - The most significant recent update replaced the external DjVuLibre C library dependency with a pure Rust implementation (`DJVULibRust`). This provides:
- Elimination of complex C library bundling
- Better cross-platform compatibility
- Thread-safe concurrent page encoding
- Modern builder API
- Smaller binary sizes
- Easier maintenance and debugging

---

## ≡ƒñ¥ Contributing

Contributions are welcome! Please note:

- Follow Rust 2024 edition conventions
- Run `cargo clippy` and `cargo fmt` before submitting
- Test on multiple platforms if possible
- Update documentation for user-facing changes

---

## ≡ƒôä License

Lege is licensed under the **GNU General Public License v3.0** (GPL-3.0).

See [LICENSE](LICENSE) for full license text.

Third-party licenses are documented in `docs/THIRD-PARTY-LICENSES.md`.

---

## ≡ƒÖÅ Acknowledgments

### External Libraries & Resources

- **PaddleX** - Layout detection models
- **Sauvola/Otsu** - Binarization algorithms ([paper reference](https://arxiv.org/abs/1904.06098))
- **PDFium** - Google's PDF rendering engine
- **ONNX Runtime** - Microsoft's ML inference engine
- **Tesseract** - OCR engine (Google)
- **DjVu Format** - AT&T Labs (lizardtech)
- **JBIG2 Specification** - ITU-T T.88 / ISO/IEC 14492

### Rust Ecosystem

- **jbig2enc** - Original C implementation (ports to `jbig2enc-rust`)
- **TooJpeg** - Stephan Brumme's compact JPEG encoder (ported to Rust)
- **OpenJPEG** - JPEG 2000 reference implementation
- **fax** - Pure Rust CCITT4 encoder

---

## ≡ƒöù Links

- **Homepage**: [https://www.legeapp.com](https://www.legeapp.com)
- **Repository**: [https://github.com/LegeApp/Lege](https://github.com/LegeApp/Lege)
- **Documentation**: See `docs/Documentation.md` for detailed feature walkthrough
- **Issues**: [GitHub Issues](https://github.com/LegeApp/Lege/issues)

---

## ≡ƒôÜ Related Projects

- **Calibre** - E-book library management (recommended companion tool)
- **KoReader** - Open-source E-Ink reader firmware (best for DjVu support)
- **Internet Archive Downloader** - [elementdavv/internet_archive_downloader](https://github.com/elementdavv/internet_archive_downloader)

---

**Made with Γ¥ñ∩╕Å for the E-Ink reading community**
