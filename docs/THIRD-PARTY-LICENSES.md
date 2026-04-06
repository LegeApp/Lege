# Lege License and Third-Party Notices

This file summarizes the current license position of Lege and the main third-party
components that are distributed with, linked by, or required by the program.

It is intentionally concise. Lege itself is released under a strong copyleft
license, but third-party components still retain their own licenses and notices.

## Lege license

The Lege project itself is licensed under the GNU Affero General Public License,
version 3. See the top-level [LICENSE](../LICENSE) file for the full text.

This means:

- Lege is not an MIT- or Apache-licensed application.
- More permissive licenses used by dependencies do not change Lege's own license.
- Third-party notices still matter and are preserved below.

## Main third-party components

The entries below reflect the current codebase and packaging layout.

| Component | Role in Lege | Current license | Notes |
| --- | --- | --- | --- |
| Freya | Desktop GUI framework | MIT | Current GUI frontend used by `lege-gui`. |
| Legencode | Local encoding crate | MIT | Local crate used for JPEG, JBIG2, CCITT4, and related image processing helpers. |
| toojpeg | Local JPEG encoder crate | MIT | Local Rust port of TooJpeg. |
| jbig2enc-rust | Local JBIG2 encoder crate | MIT OR Apache-2.0 | Current local JBIG2 encoder. |
| ort | ONNX Runtime Rust binding | MIT OR Apache-2.0 | Local fork/configured copy used for inference runtime integration. |
| pdfium-render | PDF rendering wrapper | MIT OR Apache-2.0 | Rust wrapper around PDFium. |
| fast_image_resize | Image resize library | MIT OR Apache-2.0 | Used in image processing paths. |
| lopdf | PDF parsing and writing support | MIT | Used in PDF processing/output code. |
| rayon | Parallel CPU work helper | MIT OR Apache-2.0 | Used in CPU-heavy processing paths. |
| rfd | Native file dialogs | MIT | Used by the GUI. |

## OCR components

Current OCR usage:

- Linux and macOS:
  - Rust crate: `tesseract`
  - Runtime engine: system Tesseract installation
- Windows:
  - Windows OCR / WinRT APIs
  - No Tesseract runtime is required on Windows

Notes:

- The Rust `tesseract` crate is MIT-licensed.
- The Linux package configuration currently depends on `tesseract-ocr`.
- `eng.traineddata` may be distributed with packaged builds for convenience.

## Bundled runtime libraries and assets

Depending on platform and packaging target, Lege may distribute or expect these
runtime assets alongside the binaries:

- PDFium shared library
- ONNX Runtime shared libraries
- ONNX model files used for layout detection, deskew/orientation, and heavy binarization
- Tesseract language data such as `eng.traineddata`

These assets should be treated separately from ordinary Rust crate dependencies.
They may have their own upstream licenses, notices, attribution requirements, or
redistribution conditions.

## Models and data

Current packaged model/data assets referenced by the build configuration include:

- `yolo-layout.onnx`
- `paddle-rotate.onnx`
- `paddle-deskew.onnx`
- `sauvola.onnx`
- `eng.traineddata`

This document intentionally does not claim a single uniform license for all
models or data files. Their provenance and license terms can differ from one
another and should be tracked based on the exact upstream source used for the
distributed asset.

For release packaging, verify and preserve the relevant notices for:

- model source repository or paper release
- converted ONNX artifact provenance
- Tesseract traineddata source
- any bundled native shared libraries

## Research citations and good-faith attribution

The items below are kept as acknowledgements and citations in good faith.
They are not included here because Lege is legally required to paste them in
full, but because the project intentionally wants to credit the work that
influenced or powers parts of the binarization pipeline.

### Heavy Sauvola model

The `sauvola.onnx` heavy binarization model is attributed to:

Li, Deng and Wu, Yue and Zhou, Yicong.
"SauvolaNet: Learning Adaptive Sauvola Network for Degraded Document Binarization."
In: The 16th International Conference on Document Analysis and Recognition
(ICDAR), 2021, pp. 538-553.
DOI: <https://doi.org/10.1007/978-3-030-86337-1_36>

### Adaptive binarization inspiration

Lege's lighter adaptive binarization path also keeps a good-faith citation to
the project that influenced the earlier binarization approach:

- <https://github.com/rahimnathwani/binarize-pdf>

This is an acknowledgement of inspiration and prior work, not a statement that
Lege is currently shipping that project as a direct dependency.


## Additional verification items

The following areas should be kept under review as the release process evolves:

- the exact license/provenance of each bundled ONNX model
- the exact redistribution basis for bundled PDFium binaries
- the exact redistribution basis for bundled ONNX Runtime binaries
- whether `eng.traineddata` is bundled or required from the host system
- the local `djvu_encoder` crate metadata, which should declare its own license explicitly
