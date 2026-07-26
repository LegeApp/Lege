# PDFium text differential oracle

This development-only crate compares `pdf-text` with PDFium through PDFium's
public text API. It is deliberately outside the workspace and dynamically
loads PDFium, so the renderer never acquires a PDFium dependency.

```sh
cargo run --manifest-path tools/pdfium-text-diff/Cargo.toml --release -- \
  /path/to/libpdfium.so document.pdf [zero-based-page]
```

The CSV reports UTF-16 edit distance, character-count difference, public
per-character field mismatches, tight- and loose-box mismatches above 0.01
page units, rectangle-count difference, and rectangle text mismatches.
Character source codes, matrices, and internal `CharType` values are tested
inside `pdf-text`; PDFium's public API does not expose them.
