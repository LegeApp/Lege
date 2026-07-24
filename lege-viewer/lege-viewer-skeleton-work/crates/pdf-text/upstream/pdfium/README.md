# Preserved PDFium Unicode sources

These files were copied byte-for-byte from PDFium commit
`33a80ac7c309a8686d1c34732eae8559a3a9ba82`:

- `core/fxcrt/fx_ucddata.inc`
- `core/fxcrt/fx_unicode.cpp`
- `core/fxcrt/fx_bidi.cpp`
- `core/fxcrt/fx_bidi.h`
- `core/fpdftext/unicodenormalizationdata.cpp`
- `core/fpdftext/unicodenormalizationdata.h`
- `LICENSE`

Do not convert the arrays by hand. `crates/pdf-text/build.rs` authenticates
the large source inputs with SHA-256, parses their C++ initializer/macro
syntax, and emits Rust arrays into Cargo's `OUT_DIR`. Updating PDFium means
copying the upstream files again, reviewing parser compatibility, and then
updating the recorded digests.
