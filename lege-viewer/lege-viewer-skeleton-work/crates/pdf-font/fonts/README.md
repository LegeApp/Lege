# Bundled standard-14 faces — provenance and licence

These 14 OpenType files back non-embedded font substitution (fonts.md Font
Phase 3, `pdf-font/src/standard.rs`).

## Origin

They are PDFium's bundled **Foxit** fonts, taken from
`pdfium-reference-source/core/fxge/fontdata/chromefontdata/*.cpp`, where each
face is a C array of **bare CFF** bytes. PDFium hands those to FreeType, which
accepts bare CFF; our outline engine (Skrifa) reads SFNT only, so
`tools/foxit-fonts/extract.py` wraps each CFF in a minimal OpenType container
**once**, offline, and the result is committed here.

The wrapper adds no information of its own:

| table | source |
|---|---|
| `CFF ` | the original bytes, byte-for-byte |
| `hmtx` | advances read out of the charstrings |
| `head`/`hhea`/`OS/2` | the CFF `FontBBox`, `FontMatrix`, `ItalicAngle` |
| `maxp` | the CharStrings INDEX count |
| `cmap` | glyph names mapped through the AGL |
| `post` (2.0) | the CFF charset's glyph names |

Regenerate with:

```sh
python3 tools/foxit-fonts/extract.py <pdfium-source-root> crates/pdf-font/fonts
```

## Licence

Each source file carries:

> Copyright 2014 The PDFium Authors
> Use of this source code is governed by a BSD-style license that can be found
> in the LICENSE file.
> Original code copyright 2014 Foxit Software Inc. <http://www.foxitsoftware.com>

So the data is authored by Foxit and distributed by PDFium/Chromium under
PDFium's BSD-3 licence — the same basis on which Chrome ships it. Note that
PDFium's `LICENSE` does not call out font data separately from source code;
fonts.md flags exactly this ("verify licensing of any copied or bundled font
data separately from PDFium's source-code license"), and bundling these was a
deliberate project decision, taken because this engine is a semantic port of
PDFium and matching its substitution *and* its glyph shapes keeps differential
comparison against PDFium meaningful.

Swapping in differently-licensed faces (Liberation, URW Base 35, …) means
replacing these files and the `include_bytes!` paths in
`pdf-font/src/standard.rs`; nothing else depends on their provenance.

## Not included

`FoxitSansMM` and `FoxitSerifMM` are Type 1 Multiple Master faces, which
Skrifa cannot parse (Font Phase 5). PDFium interpolates them to fit unknown
fonts; we fall back to the nearest of the standard 14 instead. See
`DEFERRED.md`.
