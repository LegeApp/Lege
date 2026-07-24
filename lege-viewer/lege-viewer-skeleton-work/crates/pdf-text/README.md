# pdf-text

`pdf-text` extracts owned UTF-16 text and geometry from a
`pdf_content::SemanticPage`. It is independent of rasterization and does not
depend on any render backend.

```rust
let text_page = pdf_text::TextPage::build(
    &semantic_page,
    &pdf_text::TextPageOptions::default(),
);
let utf16 = text_page.all_text_utf16(); // parity-safe, including malformed PDF data
let display = text_page.all_text();     // convenience: invalid UTF-16 becomes U+FFFD
let words = text_page.words();          // exact unions of glyph boxes
```

The default retains hidden optional-content text, matching PDFium, but
excludes annotation appearances, soft-mask content, tiling cells, and nested
Type 3 CharProc text. Options can include annotations or soft masks and can
force overall RTL order or disable compatibility normalization.

## Lege migration

The old rendered-segment flow maps as follows:

- page text → `all_text_utf16()` or `all_text()`
- early text presence check → `has_text()`
- PDFium-style text-object rectangles → `rects(0, char_count())`
- text clipped to a rectangle → `text_in_rect(rect)`
- word text and geometry → `words()`

`words()` unions actual character boxes. A caller should not split a large
segment rectangle at spaces and interpolate by character count; that is
incorrect for proportional fonts.

## Development oracle

`tools/pdfium-text-diff` compares the public text, character, tight/loose
box, and rectangle surfaces with a dynamically loaded PDFium build. Internal
fields that PDFium does not expose are covered by focused Rust tests.
