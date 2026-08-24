# Lege Ecosystem

This directory is the Rust workspace root for the Lege application family.
The layout separates applications, shared compute/OCR/PDF libraries, codecs,
and project tooling so the processor and native PDF viewer/manager can
share the renderer and lower-level crates without becoming one monolithic app.

## Folder contract

```text
Lege/
├── Cargo.toml                 ecosystem workspace + shared dependency policy
├── .cargo/config.toml         short build/check commands
├── lege-process/              PDF processing CLI, library, and legacy GUIs
│   ├── core/                  `lege` library and CLI roots + core services
│   ├── pipeline/              processing pipelines
│   ├── encoding/              encoder dispatch/adapters
│   ├── color/                 binarization and color processing
│   ├── colorquant/            color quantization
│   ├── ocr/                   processor-facing OCR integration
│   ├── reflow/                raster reflow
│   ├── resize/                processor-facing resize helpers
│   ├── lege-ipc/              CLI/GUI IPC types
│   └── GUI/
│       ├── Freya/             `lege-gui`: default desktop frontend
│       ├── musicsheet/        `lege-music-gui`
│       └── rfd/               vendored file dialog crate
├── lege-viewer/               native `lege-viewer`: reader + processing workspace
├── lege-document-ocr/         business-grade OCR product
│   ├── crates/                document IR, pipeline, batch, and export libraries
│   ├── cli/                   `lege-ocr` batch CLI
│   └── installer-winsafe/     native Windows installer and uninstaller
├── lege-android/              JNI library and Gradle Android application
├── lege-gpu/                  shared GPU compute and ONNX runtime
├── lege-ocr/                  shared OCR library and debug program
├── lege-pdf/
│   ├── agent/                 structured, agent-facing `lege-pdf` CLI
│   ├── read/                  document intake/read seam
│   ├── write/                 shared typed PDF writer
│   ├── render/                native document and rendering engine
│   └── pdf-integrity/         standalone forensic-triage workspace
├── lege-codecs/
│   ├── jbig2enc-rust/         canonical JBIG2 codec source
│   ├── jp2lam/                canonical JPEG 2000 codec source
│   └── djvulibrust/           canonical DjVu codec source
├── lege-misc/                 assets, docs, packaging, scripts, and xtask
└── freya-main/                vendored upstream UI workspace
```

`lege-pdf/render` and `lege-viewer` are direct workspace members and evolve
together through retained semantic, text, compiled-IR, and tiled-raster APIs.
The codec projects keep independent build graphs, while the root workspace
uses the in-tree JBIG2 and JPEG2000 sources.

## Canonical codec source

`lege-codecs/djvulibrust`, `lege-codecs/jbig2enc-rust`, and
`lege-codecs/jp2lam` are the only maintained source locations for Lege's
codecs. Their former standalone GitHub repositories are redirect-only archive
pages; use this repository for source, issues, and development. Rust consumers
should use published crate versions where available rather than a moving Git
dependency.

## Common commands

Run these from the ecosystem root:

```sh
# Default members: processor CLI + main GUI
cargo build

# Individual applications
cargo process
cargo gui
cargo music-gui
cargo viewer
cargo document-ocr

# Shared crates
cargo gpu
cargo ocr
cargo pdf-write
cargo ipc

# Standalone codecs
cargo jbig2
cargo jp2
cargo djvu

# Checks
cargo process-check
cargo gui-check
cargo music-gui-check
cargo gpu-check
cargo ocr-check
cargo pdf-write-check
cargo ecosystem-check
cargo fmt-all
cargo fmt-all-check
# For pdf-render workspace crates specifically, run package-scoped format checks, e.g.
cargo fmt --package pdf-read --package pdf-text -- --check

# Run the processor CLI
cargo process-run -- path/to/input.pdf
cargo viewer-run -- path/to/input.pdf
cargo document-ocr-run batch <input.pdf-dir> --output <out-dir> --workers 4

# Packaging task help
cargo xtask-help
```

Build profiles and shared dependency versions belong in the root
`Cargo.toml`, not in application member manifests.

## License

The top-level Lege workspace and application packages are AGPL-3.0-only. See
`LICENSE` and `NOTICE`. Independently usable codec and vendored third-party
subtrees retain the licenses declared in their own manifests and license files.
