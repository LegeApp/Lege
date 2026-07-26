# PLAN-TEXT-EXTRACTION — port `CPDF_TextPage` into a `pdf-text` crate

Status: implemented and validated on 2026-07-23. The focused differential
oracle remains a diagnostic tool because PDFium parity is evidence, not the
definition of correctness. Language: ASD-STE100 Simplified Technical English,
Issue 9.

Companion document: `Lege-ecosystem/to-do-plans/unified-renderer-integration-plan.md`.
That plan removes PDFium from Lege. This plan closes the last gap that blocks
the removal: Lege reads the native text layer of a PDF through PDFium, and
this engine has no equivalent.

This is a **blocking item for the production move-over.** Rendering parity is
not enough. If this crate does not exist, Lege loses the native text layer,
and every text page falls back to optical character recognition.

---

## 1. Why this matters

Lege builds the searchable text layer of its output. When the source PDF
already contains text, Lege reads that text and its word boxes, and writes
them as hOCR. This is faster than optical character recognition and it is
exact, because the words come from the document, not from a model.

Three Lege call sites depend on it:

| Lege call | Use |
|---|---|
| `has_text_layer(page)` | Decide between the native text layer and optical character recognition |
| `extract_page_text(page)` | Plain page text, used by the DjVu path |
| `extract_positioned_text_words(page, w, h)` | Word boxes, used to build hOCR |

`build_hocr_from_positioned_words` sorts these words into lines and writes
the hOCR. So the accuracy of each word box decides the quality of the text
layer of the output document.

---

## 2. What Lege calls today, down to the PDFium function

The chain is short, and it ends in one class.

```
lege pagerender.rs
  page.text().all()                → FPDFText_GetText
                                   → CPDF_TextPage::GetAllPageText

  page.text().segments()           → FPDFText_CountRects(0, -1)
                                   → CPDF_TextPage::GetRectArray(0, all)
  segment.bounds()                 → FPDFText_GetRect
  segment.text()                   → FPDFText_GetBoundedText
                                   → CPDF_TextPage::GetTextByRect

  push_segment_words(...)          → Lege splits the segment text at spaces
                                     and interpolates the x edges by
                                     character count.
```

So PDFium does the hard part: it turns the content stream into a character
list with boxes, and it inserts the spaces and the line breaks that the
content stream does not contain. PDFium then groups the characters into
rectangles by text object. Lege only cuts those rectangles at the spaces.

**Lege's last step is weak.** It divides the rectangle width by the character
count, so a proportional font gives wrong word edges. Section 6.4 replaces it
with exact word boxes. This is an improvement, not a port.

Reference files in `../pdfium-reference-source/`:

- `core/fpdftext/cpdf_textpage.cpp` (1586 lines) — the whole algorithm.
- `core/fpdftext/cpdf_textpage.h` (200 lines) — the data model.
- `core/fpdftext/unicodenormalizationdata.cpp` (8275 lines) — ligature data.
- `core/fxcrt/fx_bidi.cpp` and `fx_bidi.h` (173 lines) — the bidirectional
  segmenter.
- `core/fxcrt/fx_unicode.cpp` and `fx_ucddata.inc` (65536 lines) — the
  character property table, which gives the bidirectional class and the
  mirror character.
- `core/fpdfapi/page/cpdf_textobject.*` — the text object and its items.

---

## 3. The algorithm to port

Read this section as the specification. Every constant here is a real
constant in the source, and each one changes the output.

### 3.1 The data model

```
CharInfo {
    char_type: Normal | Generated | NotUnicode | Hyphen | Piece
    char_code: u32
    unicode:   char
    origin:    Point      // device space
    char_box:  Rect       // tight box, device space
    loose_char_box: Rect  // font-bbox box, device space
    matrix:    Matrix     // text matrix * form matrix
    text_object: id       // identity of the source text object
}
```

The page holds an ordered character list and a parallel text buffer. It also
holds `char_indices_`, which maps a character index to a text index and back.
The map skips the characters that do not print.

### 3.2 The order of operations

1. `FindTextlineFlowOrientation` decides whether the page flows horizontally
   or vertically. It paints the rectangle of every text object into a
   horizontal mask and a vertical mask, both one entry per point of page
   size. Rules, in order: if the vertical extent is below two line heights,
   the page is horizontal; if the horizontal extent is below two line
   heights, the page is vertical; if the horizontal mask is more than 80 per
   cent filled, the page is horizontal; then the larger filled fraction wins;
   equal fractions give "unknown".
2. Walk the page objects in painter order. A form object recurses with
   `form_matrix = form.matrix * parent_matrix`.
3. Each text object goes into a **sort buffer**, not straight into the
   character list. `ProcessTextObject(obj, form_matrix, holder, iter)` does
   this:
   - Skip the object when its rectangle width is below `kSizeEpsilon`
     (0.01).
   - Skip the object when `IsSameAsPreTextObject` finds a near-duplicate in
     the previous five text objects. This removes fake-bold text that is
     drawn two times.
   - Compare the display-space Y of this object against the last buffered
     object. When the difference is more than `2 * threshold`, where
     `threshold = max(prev_width, this_width) / 4`, flush the whole buffer in
     order and start a new buffer.
   - Otherwise insert the object into the buffer by descending X, so the
     buffer holds one visual line in reading order.
4. After the walk, process every buffered object, then call `CloseTempLine`.
5. Build `char_indices_`.

### 3.3 Per text object

`ProcessTextObject(TransformedTextObject)`:

1. Skip when the rectangle width is below `kSizeEpsilon`.
2. `PreMarkedContent` looks for an `/ActualText` entry in the marked-content
   dictionaries. It answers `Pass`, `Done` (the same dictionary as the
   previous object, so skip the object), or `Delay` (synthesize the
   characters from the `/ActualText` string).
3. When a previous object exists, `ProcessInsertObject` answers `None`,
   `Space`, `LineBreak`, or `Hyphen`. A `LineBreak` resets `curline_rect_` to
   this object's rectangle; anything else adds this rectangle to it.
4. `ProcessGenerateCharacter` acts on that answer. See Section 3.5.
5. On `Delay`, `ProcessMarkedContent` spreads the `/ActualText` characters
   evenly across the object rectangle, one equal step for each character, and
   pushes them as `Piece` characters.
6. Compute `bR2L` with the bidirectional classifier over the object's
   characters. When `bR2L` is true **and** the determinant `a*d - b*c` of the
   text matrix is negative, reverse the characters that this object appends.
7. `ProcessTextObjectItems` appends the characters.

### 3.4 `ProcessTextObjectItems` — the character loop

```
base_space = CalculateBaseSpace(obj, matrix)
           + CalculateBaseSpaceAdjustment(obj, matrix)
```

- `CalculateBaseSpace` returns 0 when the character spacing is 0 or when the
  object has fewer than three items. Otherwise it starts at the transformed
  character spacing and lowers it by the kerning of every `TJ` adjustment.
  It returns 0 when the result is negative, or when the object has exactly
  three items and at least one is an adjustment.
- `CalculateBaseSpaceAdjustment` returns the transformed character spacing
  with the sign reversed, and 0 inside the band of 0.001.

Then, for each item:

- A `TJ` adjustment carries the code `0xffffffff`. It sets
  `spacing = -font_size_h * origin.x / 1000`, but only when the last
  character in the buffer is not a space.
- Subtract `base_space` from `spacing`.
- When `spacing` is non-zero, the item is not the first, and
  `spacing >= CalculateSpaceThreshold(...)`, append a **generated space**.
  The generated space is a `Generated` character with an empty box at the
  item origin.
- `CalculateSpaceThreshold` takes the width of the font's own space
  character, scaled by the horizontal font size and divided by 1000. It sets
  the threshold to zero when that value is more than one third of the
  horizontal font size, and halves it in every other case. When the threshold
  is zero, it falls back to the width of the current character, passed
  through `NormalizeThreshold(w, 300, 500, 700)` and scaled.
- `NormalizeThreshold(t, t1, t2, t3)` divides `t` by 2, 4, 5, or 6, depending
  on which of the three bounds `t` is below.
- Build the character box from the font character bounding box, scaled by
  `font_size / 1000` and offset by the item origin. Two repairs follow: a box
  with almost no height becomes one font size high; a box with almost no
  width becomes as wide as the character advance. Then transform the box by
  the matrix.
- **Duplicate suppression.** Look back over at most seven characters. When a
  previous character has the same character code, the same font, and an
  origin within `0.07 * font_size` (transformed along X) in both axes, do not
  append the Unicode. This is the second half of the fake-bold repair. When
  the duplicate is the first item of the object and the buffer ends in a
  space, remove that space.
- A character with no Unicode becomes a `NotUnicode` character, and the text
  buffer receives `0xfffe` as its placeholder.

### 3.5 Space, line break, and hyphen between objects

`ProcessInsertObject` decides. In order:

1. Get the writing mode from `GetTextObjectWritingMode`. That function
   normalizes the vector from the first to the last character origin and
   compares both components against 0.0872, which is about five degrees. An
   object with one character, or an ambiguous vector, uses the page
   orientation from Section 3.2.
2. Horizontal mode: `EndHorizontalLine` returns true when both rectangles are
   taller than 4.5 and the two rectangles do not overlap vertically. Vertical
   mode: `EndVerticalLine` returns true when both rectangles are wider than
   `0.1 * font_size` and this rectangle does not overlap the current line
   rectangle horizontally. A true answer gives `Hyphen` or `LineBreak`.
3. Transform this object's position into the **previous object's text
   space**. In horizontal mode, a line break also follows when the previous
   rectangle is empty and more than 5 high, or when `pos.y > threshold * 2`
   or `pos.y < threshold * -3`, together with `|pos.y| >= 1` or
   `|pos.y| > |pos.x|`.
4. That test has a carve-out for pages that draw one line as several objects
   in a strange order. When the display matrix is close to the identity
   flip (`a > 0.9`, `b < 0.1`, `c < 0.1`, `d < -0.9`), the text matrix has
   almost no skew, and either object's position falls inside the band
   `[0, 1000]` of the other object's vertical extent, the line break is
   cancelled.
5. One-character objects that hold a hyphen give `Hyphen`.
6. A leading or trailing space in either object gives `None`.
7. Otherwise compute `threshold2` from the larger of the two character
   widths through `NormalizeThreshold(w, 400, 700, 800)`, scale it by the
   font size, transform it when the new character is wider, and divide by
   1000. **Two magic values follow:** when `threshold2` lands inside the
   narrow band around 1.4880 or around 1.3900, multiply it by 1.5. Keep this
   rule. It is not an accident; it repairs a common font family.
8. `GenerateSpace(pos, last_pos, this_width, last_width, threshold)` gives
   the final answer. It returns false when the gap is inside the threshold,
   true when the position difference is larger than `threshold + last_width`,
   true for the negative-position case, and true when the difference is
   larger than the sum of the two widths.

`ProcessGenerateCharacter` then acts:

- `Space` appends a generated space to the temporary buffer.
- `LineBreak` closes the temporary line, then appends `\r` and `\n` to the
  main buffer, but only when the main buffer is not empty.
- `Hyphen` removes the trailing spaces of the temporary buffer, removes the
  last character from the text buffer, marks that character as `Hyphen`, sets
  its Unicode to `0x2`, and appends `0xfffe`. A one-character object that is
  itself a hyphen stops the object from being processed at all.

`GenerateCharInfo` places a generated character at the previous origin plus
the previous character advance.

### 3.6 `CloseTempLine` — bidirectional reordering

1. Remove every second space in a run of spaces.
2. Run the bidirectional segmenter over the line.
3. A right-to-left segment, or a neutral segment inside a right-to-left run,
   appends its characters in reverse order, each one mirrored by
   `GetMirrorChar`, and each one normalized.
4. A left-to-right segment appends in order. Only the ligature block
   `U+FB00` to `U+FB06` is normalized on this path.
5. A normalized character becomes a `Piece` character, and one source
   character can produce several output characters.

The segmenter is not the full Unicode bidirectional algorithm. It is a
one-pass classifier that groups characters into left, left-weak, right, and
neutral segments. The overall direction becomes right when the right segments
are at least as many as the left segments. Port the PDFium version. A
standard implementation gives different output and breaks the oracle test.

### 3.7 The output queries

- `GetPageText(start, count)` reads the text buffer through the index map.
- `GetRectArray(start, count)` walks the characters, skips `Generated`
  characters and boxes below `kSizeEpsilon` in either axis, and unions the
  boxes of every run of characters that share one text object. Each run gives
  one rectangle.
- `GetTextByRect(rect)` collects every character whose box intersects the
  rectangle. It inserts `\r\n` when the origin Y changes and the previous
  character was outside, and it collapses spaces.
- `GetCharLooseBounds` returns the box built from the font bounding box.
  Vertical CID fonts use the vertical origin and the vertical width. This box
  includes the accents, which the tight box does not.

---

## 4. Gap analysis against this workspace

### 4.1 What already exists and fits

| Need | Where | Note |
|---|---|---|
| Text objects in painter order | `SemanticPage.ops`, `SemanticOp::ShowText(TextRunId)` | Painter order is preserved |
| Text object identity | `TextRunId` | Replaces the PDFium object pointer |
| Character codes, not glyphs | `TextElement::Show(Vec<u8>)` | Exactly what the port needs |
| `TJ` adjustments | `TextElement::Adjust(f64)` | The `0xffffffff` item of PDFium |
| Text state | `TextRun`: `font_size`, `char_spacing`, `word_spacing`, `horizontal_scale`, `rise`, `render_mode`, `text_matrix` | Complete |
| Code to CID decoding and advances | `pdf_font::FontMetrics::decode` | Simple, Identity, CMap, and vertical forms |
| Font identity for the `/ToUnicode` lookup | `SemFont.object: Option<ObjectId>` | The font dictionary is reachable |
| CID CMap parsing | `pdf_font::cmap` | `cidrange` and `cidchar` |
| Object access and resolution | `DocumentSnapshot`, `ParseContext` | No new parser needed |
| Page geometry and rotation | `PageBounds` | Gives the display matrix |

### 4.2 What is missing and must be built

Each row is real work. None of it is optional for parity.

| Missing | Where it must go | Size |
|---|---|---|
| `/ToUnicode` CMap parsing (`bfchar`, `bfrange`) | `pdf-font` | Small. `pdf-syntax` gives the tokens. |
| Code to Unicode fallback chain | `pdf-font` | Medium. See Section 5.2. |
| Font bounding box accessor | `pdf-font::FontProgram` | Small. Skrifa supplies it. |
| Per-character bounding box accessor | `pdf-font::FontProgram` | Small. Needed for every character box. |
| Bidirectional class table and mirror table | new `pdf-text` data module | Generated. See Section 5.3. |
| Ligature normalization table | new `pdf-text` data module | Generated. See Section 5.3. |
| The PDFium bidirectional segmenter | `pdf-text` | Small. 102 lines of C++. |
| Marked content and `/ActualText` | `pdf-content` | **Medium and invasive.** See Section 5.4. |
| The whole `CPDF_TextPage` algorithm | `pdf-text` | Large. Sections 3.2 to 3.7. |
| Word grouping | `pdf-text` | Small, and new. See Section 6.4. |

### 4.3 The one invasive item

`SemanticOp` has no marked-content operations. The interpreter reads `BMC`
and `BDC`, but it keeps only the optional-content visibility, and it drops
the dictionaries. So `/ActualText` cannot reach a text consumer today.

`/ActualText` is how a PDF states the real text of a region whose glyphs do
not carry it. Ligature-heavy typesetting and tagged PDFs use it. Without it,
those regions extract as mojibake or as nothing.

Two choices:

- **Preferred.** Add an `actual_text: Option<Arc<str>>` field to `TextRun`,
  filled by the interpreter from the innermost enclosing marked-content
  dictionary. This is additive, it costs one pointer per run, and it does not
  change the operation vocabulary. Rendering ignores the field.
- Alternative. Add `BeginMarkedContent` and `EndMarkedContent` operations.
  This is closer to PDFium but it changes the operation list, and every
  consumer must skip the new operations.

Take the preferred choice unless a second consumer needs the spans.

---

## 5. Design

### 5.1 A new crate: `pdf-text`

```text
crates/pdf-text
    lib.rs        TextPage, TextPageOptions, CharInfo, TextWord
    build.rs      the Section 3 algorithm over a SemanticPage
    bidi.rs       the PDFium bidirectional segmenter
    tables/       generated: bidi class, mirror, normalization
    unicode.rs    the code-to-Unicode resolver
    query.rs      page text, rectangles, text in rectangle, words
```

Dependency direction: `pdf-text` depends on `pdf-content`, `pdf-font`,
`pdf-document`, `pdf-object`, and `pdf-geom`. **No render crate depends on
`pdf-text`.** Text extraction is a document-layer product, not a rendering
product, so it must never enter the render path.

Public API:

```rust
pub struct TextPageOptions {
    /// Force the overall direction to right-to-left.
    pub rtl: bool,
    /// Page space to device space. Identity gives PDF user space.
    pub display_matrix: Matrix,
    /// Apply the ligature normalization tables.
    pub normalize: bool,
}

pub struct TextPage { /* char list, text buffer, index map */ }

impl TextPage {
    pub fn build(
        page: &SemanticPage,
        unicode: &UnicodeResolver,
        options: &TextPageOptions,
    ) -> TextPage;

    pub fn char_count(&self) -> usize;
    pub fn char_info(&self, index: usize) -> &CharInfo;
    pub fn loose_bounds(&self, index: usize) -> Rect;

    pub fn all_text(&self) -> String;
    pub fn text(&self, start: usize, count: usize) -> String;
    pub fn text_in_rect(&self, rect: Rect) -> String;

    /// PDFium `GetRectArray`. Kept for the oracle test.
    pub fn rects(&self, start: usize, count: usize) -> Vec<Rect>;

    /// New. See Section 6.4.
    pub fn words(&self) -> Vec<TextWord>;

    /// Cheap answer for "does this page carry text?".
    pub fn has_text(&self) -> bool;
}

pub struct TextWord {
    pub text: String,
    pub bbox: Rect,
    pub first_char: usize,
    pub char_count: usize,
}
```

`TextPage::build` takes a `SemanticPage`, so a caller that already compiled
the page for rendering pays nothing more. `TextPage` is `Send + Sync` and
holds no borrow of the snapshot.

### 5.2 The code-to-Unicode resolver

`UnicodeResolver` maps one character code of one font to a string. Build it
one time for each font of the page and cache it for the document, next to the
other document-scoped caches.

The order of attempts, which follows PDFium:

1. The font's `/ToUnicode` CMap, when present. Parse `bfchar` and `bfrange`.
   Handle the array form of `bfrange`, and the surrogate pairs.
2. A composite font with a predefined CMap: the registry, ordering, and the
   `Adobe-*-UCS2` table. `pdf-font` already carries these tables for the four
   CJK registries.
3. A simple font: the encoding, then the glyph name, then the Adobe glyph
   list. `pdf-font::encoding` and `agl_table` already hold this.
4. The embedded font program `cmap` table, reversed.
5. No answer. The character becomes a `NotUnicode` character with the
   placeholder `0xfffe`, exactly as PDFium does.

Write down which step answered. The differential test in Section 7 needs it
to explain a difference.

### 5.3 The generated tables

Three tables come from PDFium. Do not type them by hand, and do not commit a
one-time translation of them. Copy the upstream source files byte-for-byte
into `crates/pdf-text/upstream/pdfium/` with `cp`. A `build.rs` parser reads
those preserved sources and emits Rust into `OUT_DIR` on each build. The
crate uses `include!(concat!(env!("OUT_DIR"), ...))` to compile the result.
The build script checks a recorded source digest and fails when an upstream
format change would make the translation ambiguous.

| Table | Source | Emitted size |
|---|---|---|
| Bidirectional class | `core/fxcrt/fx_ucddata.inc`, field `bd` | `[u8; 65536]`, 64 KiB |
| Mirror characters | same file, field `mirror`, plus `kFXTextLayoutBidiMirror` | About 1 KiB |
| Ligature normalization | `core/fpdftext/unicodenormalizationdata.cpp` | About 64 KiB |

**Licence.** PDFium is under a BSD three-clause licence. This workspace is
Apache-2.0. A generated table is a derived work. Add the PDFium copyright
notice to the generated files and to a `THIRD-PARTY-NOTICES` file at the
workspace root. Do this in the same commit as the tables.

A phased option: the left-to-right path normalizes only `U+FB00` to
`U+FB06`, which is seven entries. Phase 2 can ship those seven entries and
carry the full table in Phase 4 with the right-to-left work. Record the
divergence when you take this option.

### 5.4 Threading

`TextPage::build` takes `&SemanticPage` and `&UnicodeResolver`, and it
returns an owned value. It holds no global state, so it obeys the one rule
of this workspace. Many threads build many pages of one snapshot at the same
time.

The `UnicodeResolver` cache is document-scoped and shared, like
`SharedFontProgramCache`. Use the same once-publication pattern.

---

### 5.5 Code-review amendments (2026-07-23)

This section is authoritative where it conflicts with earlier sections. The
review compared this plan with the current `pdf-content`, `pdf-font`, and
PDFium public text APIs.

1. **Use a canonical page matrix for the extraction algorithm.** PDFium's
   `CPDF_TextPage` uses `CPDF_Page::GetDisplayMatrix()`, which normalizes the
   crop box and page rotation but does not use the requested render bitmap
   size. A caller-selected pixel matrix would make spaces and reading order
   change with output resolution. Build and compare in canonical rotated
   page space. Transform the completed character and word boxes to pixels
   only at the Lege/API boundary.
2. **Resolve Unicode while the document is available.** A `SemanticPage`
   owns no `DocumentSnapshot`, while `/ToUnicode` is commonly an indirect
   stream. The content compiler must resolve each font's owned Unicode map
   and attach it to `SemFont`. `TextPage::build` then needs only the owned
   `SemanticPage`; it must not accept a resolver that secretly requires the
   document. The document-scoped cache lives in the compiler/document layer.
3. **Preserve both character code and CID/GID.** `FontMetrics::decode`
   currently exposes a field named `glyph`, which is a character code for a
   simple font and a CID for a composite font. `/ToUnicode` is keyed by the
   original variable-width character code, not by the CID or GID. Extend the
   decoded item (or add a lossless decoder) with the original code and byte
   length, the CID, and the final GID. Do not use one integer for all three.
4. **Retain extraction-only Type 3 text.** The interpreter currently replaces
   Type 3 `Tj`/`TJ` operations with CharProc geometry and emits no `TextRun`.
   It must retain a non-painting semantic text run in addition to the existing
   geometry. Lowering ignores that run for rendering. Type 3 font metadata
   must retain `/FontBBox`, `/FontMatrix`, encoding, widths, and available
   `d0`/`d1` bounds for extraction.
5. **Separate extraction presence from render visibility.** The interpreter
   currently drops text in a hidden optional-content span. PDFium text
   extraction operates on page text objects rather than the render-time OCG
   decision. Retain the run and its visibility flag; rendering skips hidden
   runs, while the PDFium-compatible extractor includes them by default.
   Conversely, exclude annotation appearances and soft-mask content from
   page text by default. The existing paint-origin markers provide the scope
   needed for this filter. Add explicit options before permitting either
   category.
6. **Give `/ActualText` a span identity.** `Option<Arc<str>>` is insufficient:
   PDFium emits `/ActualText` once for consecutive text objects that share
   the same marked-content parameter dictionary. Store an owned
   `ActualTextSpanId` together with decoded UTF-16 text on each run. The
   interpreter allocates one ID per active marked-content parameter scope and
   propagates the innermost applicable span.
7. **Keep an exact UTF-16 surface.** The PDFium oracle and PDF strings are
   UTF-16 based, mappings can yield multiple code units, and malformed files
   can contain values that Rust `char` cannot represent exactly. Store the
   comparison text as `Vec<u16>` and expose `all_text_utf16()` /
   `text_utf16()` as the parity API. `String` methods are convenience,
   explicitly using a documented replacement policy. `CharInfo::unicode`
   is a numeric value, not a Rust `char`.
8. **Limit the oracle to public observable data.** The public PDFium API can
   provide Unicode, generated/hyphen/Unicode-map-error flags, font size,
   tight and loose boxes, and text-object identity. It cannot expose every
   internal `CharType`, source character code, origin, or matrix. The harness
   records the public fields and assigns stable per-page object ordinals from
   `FPDFText_GetTextObject`; internal-only fields are verified by focused
   tests, not claimed as oracle observations.
9. **Do not assume an outline box equals a PDF character box.** Tight and
   loose boxes use PDF font metrics, vertical origins, text rise, horizontal
   scaling, text and form matrices, and PDFium repair rules. `FontProgram`
   outline bounds are one input and a fallback, not the complete accessor.
   Add explicit font-bbox and per-code metric APIs with Type 1, CID, vertical,
   standard-font, missing-program, and Type 3 behavior.
10. **Treat each text-show operation as a stable object.** `TextRunId`
    supplies identity only if one run is emitted for each PDF text object and
    is not coalesced during lowering. Keep the semantic runs uncoalesced.
    Form transforms are reconstructed from the semantic `Save`/`Concat`/
    `Restore` scopes; tests must cover nested forms and reused forms.

These amendments also change phase order: the semantic retention work in
items 2–6 is a prerequisite of the first character-parity gate, not work that
can wait until T5.

---

## 6. Phases

### Phase T0 — The oracle harness

Goal: measure a difference before you write the algorithm.

Tasks:

1. Add `tools/pdfium-text-diff`, next to `tools/pdfium-diff`. It loads
   `libpdfium` with `dlopen`, exactly as the render oracle does, and it is
   dev-only and out of workspace.
2. For each corpus page it records, from both sides:
   - the full page text, character for character;
   - the character count, and for each character the Unicode value, the tight
     box, the loose box, and the character type;
   - the rectangle array from `GetRectArray`;
   - the text of each rectangle.
3. It reports four numbers per page: the character-level edit distance of the
   page text, the count of characters whose box differs by more than 0.01,
   the rectangle count difference, and the count of rectangles whose text
   differs.
4. Run it over the whole corpus with PDFium on both sides, to prove the
   harness itself is stable.

Exit gate: the harness runs over the corpus and reports zero difference when
both sides are PDFium.

### Phase T1 — Characters without spaces

Goal: build the character list, with correct Unicode and correct boxes.

Tasks:

1. Add the `/ToUnicode` parser and the resolver of Section 5.2 to
   `pdf-font`.
2. Add the font bounding box and character bounding box accessors to
   `FontProgram`.
3. Add `pdf-text` with the character loop of Section 3.4, but with no space
   generation, no line breaks, no hyphens, and no bidirectional work. Walk
   the `SemanticPage` operations and keep the transform stack, so each text
   run gets its full matrix.
4. Port the duplicate suppression of Section 3.4. It is part of the
   character loop, not an extra.

Exit gate: on the Latin corpus, every character has the same Unicode value as
PDFium and the tight boxes agree within 0.01. Word order and spaces are still
wrong, and that is expected at this phase.

### Phase T2 — Spaces, line breaks, and hyphens

Goal: the page text matches PDFium for left-to-right documents.

Tasks: port Sections 3.2, 3.3, and 3.5 in full. This is the text object sort
buffer, `ProcessInsertObject`, and `ProcessGenerateCharacter`. Keep every
constant. Add a unit test for each constant, with a hand-built input that
crosses the threshold in both directions.

Exit gate: on the Latin corpus, the page-text edit distance is zero on at
least 99 per cent of pages, and the harness explains every remaining page.

### Phase T3 — Rectangles, loose boxes, and the queries

Goal: the query surface matches PDFium.

Tasks: port Section 3.7 — `GetRectArray`, `GetTextByRect`, the index map, and
the loose bounds, including the vertical CID case.

Exit gate: the rectangle arrays match in count and in geometry within 0.01 on
the Latin corpus, and the text of each rectangle matches.

### Phase T4 — Bidirectional text and normalization

Goal: right-to-left and ligature documents match.

Tasks: generate the three tables of Section 5.3, port the segmenter of
`fx_bidi.cpp`, and port `CloseTempLine` and both `AddCharInfoBy*Direction`
functions. Add the mirror-inverse reversal of Section 3.3 step 6.

Exit gate: on the Arabic and Hebrew corpus documents, the page text matches
PDFium. Add corpus documents if the current corpus has none.

### Phase T5 — Marked content and `/ActualText`

Goal: tagged documents extract their stated text.

Tasks: add the `actual_text` field to `TextRun` in `pdf-content` per
Section 4.3, then port `PreMarkedContent` and `ProcessMarkedContent`.

Exit gate: the tagged corpus documents match PDFium. Rendering output does
not change at all, which the render oracle proves.

### Phase T6 — Words, and the Lege surface

Goal: give Lege what it needs, and give it better than PDFium.

See Section 6.4 for the design of `words()`.

Tasks:

1. Build `words()` from the character list.
2. Add `has_text()` as a cheap early answer.
3. Add `pdfr text <file.pdf> <page>` to `pdf-cli`, with options for plain
   text, the character dump, the rectangles, and the words.
4. Write the integration note for Lege: which call maps to which function.

Exit gate: `pdfr text` prints all four forms. On a proportional-font page,
the word boxes are visibly tighter than the boxes that Lege's interpolation
produces. Lege can delete `push_segment_words`.

### Phase T7 — Hardening

Tasks: add a `pdf-text` fuzz target that builds a `TextPage` from any input
document. Add the crate to the `pdf-chaos-tests` never-panic gate. Keep the
`unwrap_used`, `expect_used`, and `panic` lints at deny, as every crate here
does.

Exit gate: the fuzz target runs one hour with no crash. The chaos gate
passes.

### 6.4 The word grouping design

PDFium has no word concept. Lege builds words by cutting a rectangle at the
spaces and interpolating the edges by character count. That is wrong for
every proportional font: a page of "Illinois will" gives boxes that do not
sit under their words.

Because `pdf-text` owns the character list, it can do this exactly:

1. Walk the character list in order.
2. Break a word at a space character, at a generated space, at a line break,
   and at a change of text object when the gap is larger than the space
   threshold of Section 3.4.
3. Do not break at a hyphen character. Keep the hyphen with the word, and
   mark the word as continued, because the following line completes it.
4. The word box is the union of the tight boxes of its characters. Skip the
   `Generated` characters, which have empty boxes.
5. Skip a word whose text is empty after the placeholder characters are
   removed.

Report the character range of each word, so a caller can go back to the
character data.

This is the only place in the plan where the port deliberately does not copy
PDFium. Write the reason in the code.

---

## 7. Verification

The oracle is the rule, not the test suite. Three levels:

1. **Unit tests per constant.** Every threshold of Section 3 gets a test that
   crosses it in both directions with a hand-built page.
2. **The differential harness of Phase T0** over the whole corpus, at every
   phase. Record the numbers in a CSV, as the render sweeps do. A phase that
   makes an earlier number worse is a regression.
3. **Determinism.** Build the same page from four worker counts and compare
   the output byte for byte, exactly as the render tests do.

Accept these differences and write them down:

- A font whose Unicode comes from a different step of the fallback chain than
  PDFium chose. Record the step in the harness output and judge each case.
- Floating point differences below 0.01 in a box edge.
- The word boxes, which are better by design.

Do not accept a difference in the page text of a Latin document.

---

## 8. Open decisions

Answer these before Phase T1 starts.

1. **The display matrix.** PDFium builds character boxes in device space
   through `CPDF_Page::GetDisplayMatrix`. Lege asks for boxes in the pixel
   space of the rendered page. Decide whether `TextPage` holds device space
   (one matrix at build time, as PDFium does) or PDF user space (the caller
   transforms). PDF user space is cleaner, but the line-break carve-out of
   Section 3.5 step 4 reads the display matrix directly, so the build needs
   it either way. **Recommendation: build in device space with a caller-given
   matrix, exactly as PDFium does, because the algorithm depends on it.**
2. **The full normalization table or the seven ligatures.** Section 5.3.
3. **The `/ActualText` shape.** Section 4.3.
4. Whether `has_text()` needs a fast path that stops at the first character,
   or whether a full build is cheap enough. Measure in Phase T6.

---

## 9. Out of scope

- Text search (`CPDF_TextPageFind`). Lege does not use it.
- Text selection geometry beyond `GetRectArray`.
- The full Unicode bidirectional algorithm. Section 3.6 gives the reason.
- Structure-tree reading order (`/StructTreeRoot`). It is a better source of
  reading order than the geometry, but PDFium does not use it, and parity
  comes first. Record it as a later opportunity.
