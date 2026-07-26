# Lege Ecosystem

This directory is the Rust workspace root for the Lege application family.
The layout separates applications, shared compute/OCR/PDF libraries, codecs,
and project tooling so the processor and native PDF viewer/manager can
share the renderer and lower-level crates without becoming one monolithic app.

## Folder contract

```text
Lege-ecosystem/
├── Cargo.toml                 ecosystem workspace + shared dependency policy
├── .cargo/config.toml         short build/check commands
├── lege-process/              existing PDF processing application family
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
│       ├── Freya/             main `lege-gui` application
│       ├── musicsheet/        `lege-music-gui`
│       └── rfd/               vendored file dialog crate
├── lege-gpu/                  shared GPU compute and ONNX runtime
├── lege-ocr/                  shared OCR library and debug program
├── lege-pdf/
│   ├── read/                  document intake/read seam
│   ├── write/                 shared typed PDF writer
│   └── render/                native document and rendering engine
├── lege-viewer/               reader-first native PDF viewer
├── lege-codecs/
│   ├── jbig2enc-rust/         JBIG2 codec project
│   ├── jp2lam/                JPEG2000 codec project
│   └── djvulibrust/           standalone DjVu encoder project
├── lege-misc/                 assets, docs, packaging, scripts, and xtask
└── freya-main/                vendored Freya workspace
```

`lege-pdf/render` and `lege-viewer` are direct workspace members and evolve
together through retained semantic, text, compiled-IR, and tiled-raster APIs.
The codec projects keep independent build graphs, while the root workspace
uses the in-tree JBIG2 and JPEG2000 sources.

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

# Run the processor CLI
cargo process-run -- path/to/input.pdf
cargo viewer-run -- path/to/input.pdf

# Packaging task help
cargo xtask-help
```

Build profiles and shared dependency versions belong in the root
`Cargo.toml`, not in application member manifests.
