# Plan: source-preserving searchable PDF export

Status: proposed, not implemented.
Written after a full 472-page OCR run of an Internet Archive scan
(`buddhisminchines0000gern_1.pdf`) that failed at export with
`PreservingPdfOverlayUnsupported` and had to be re-run as `--pdf-mode rasterize`.

## 1. What preserve mode is supposed to mean

For a scanned book the correct searchable PDF is the **original page images, byte
for byte, with an invisible OCR text layer drawn over them**. Nothing is
re-encoded, so there is no second generation of JPEG loss and the output is
roughly the size of the input.

That is what `SearchablePdfPolicy::PreserveSource` promises and what every
comparable tool (OCRmyPDF, Acrobat, ABBYY) does by default.

## 2. What actually happens today

`crates/export/src/lib.rs:292` — `SearchablePdfExporter::export`:

```rust
let all_native = request.document.pages.iter().all(|page| {
    matches!(page.source_kind, PageSourceKind::NativeText | PageSourceKind::Hybrid)
});
match request.searchable_pdf_policy {
    SearchablePdfPolicy::PreserveSource if all_native => { /* byte-copy the source */ }
    SearchablePdfPolicy::PreserveSource => return Err(ExportError::PreservingPdfOverlayUnsupported),
    SearchablePdfPolicy::Rasterize => rasterized_searchable_pdf(request, &path)?,
}
```

So there are exactly two behaviours and neither is an overlay:

- **all pages native/hybrid** → copy the source file unchanged. No OCR text is
  added at all. For `Hybrid` pages whose untrusted native text was *replaced* by
  OCR (`crates/pipeline/src/lib.rs:373`), that silently discards the OCR result
  and ships the bad text.
- **any scanned page** → hard error.

`rasterized_searchable_pdf` (`crates/export/src/lib.rs:318`) re-renders every
page to RGB, re-encodes it as quality-90 JPEG, and writes it through
`lege_pdf_write::DocumentWriter`. It works, but it is a second generation of
lossy encoding and it inflated this book from **292 MB to 814 MB**.

It also drops document-level state: it never calls `writer.set_metadata` or
`writer.set_bookmarks` (`lege-pdf/write/src/writer.rs:91,98`), so title, author
and outline are lost. Verified on the run above — the source has
`title="Buddhism in Chinese society…"`, `author="Gernet, Jacques"`; the exported
PDF has `title=null`, `author=null`.

## 3. Root cause

The blocker is **not** the writer. `lege-pdf-write` already accepts
pre-encoded image payloads and emits them without touching them
(`lege-pdf/write/src/artifact.rs`, `PdfImageResource`):
`Jpeg`/`Jpx`/`Jbig2`/`CcittGroup4`/`Indexed8`, placed under an arbitrary
`Affine`, with an optional `PreparedTextLayer`. That is precisely the shape a
preserve-mode page needs, and `rasterized_searchable_pdf` already builds exactly
that artifact — it just feeds it a freshly re-encoded JPEG instead of the
original one.

What is missing is a **read-side accessor that hands the export the original
encoded image stream and its placement**. The nearest thing that exists is
`lege_pdf_read::DirectScanImage` (`lege-pdf/read/src/text.rs:43`), produced by
`direct_scan_image` at `text.rs:110`. The pipeline already uses it as an OCR
intake fast path (`crates/pipeline/src/lib.rs:348`).

Its predicate is far too strict to reuse:

- the page must consist of *exactly* `Save`/`Restore`/`Concat`/`DrawImage` and
  nothing else — **any text-drawing operator rejects the page**;
- codec must be `Dct`, 8 bpc, gray or RGB;
- no mask, no soft mask, `/Rotate` must be 0;
- the drawn area must match the crop box within 2%.

The first rule alone disqualifies the entire class of documents this feature
exists for. An Internet Archive scan is a full-page JPEG **plus an invisible
ABBYY text layer**, so `direct_scan_image` returns `None` and the pipeline
classifies it `Rendered`. In the run above **all 472 pages came back
`"source_kind": "rendered"`**, even though `pdf_images` confirms each page is one
full-page `DCTDecode` RGB image covering the media box exactly
(page 60: image 1855×3043, `transform [1855,0,0,3043,0,0]`, media box
`[0,0,1855,3043]`, one draw). The existing text layer is irrelevant here — we are
replacing it.

Consequence: the OCR intake also needlessly re-rendered all 472 pages instead of
decoding the embedded JPEG. Fixing the predicate helps both paths.

## 4. Approach

Two candidate designs.

**A. Incremental update.** Append a new revision to the original bytes: new
content stream, font, page objects, xref section. Byte-exact preservation of
everything — annotations, forms, optional content, structure tree.

Rejected for now. `lege-pdf-write` states its contract in
`lege-pdf/write/src/lib.rs:1`: *"No parser, no editing, no encryption, **no
incremental update**, no linearization."* Incremental update needs a writer that
can parse and reference existing objects, which is a different crate with a
different plan. It is also unsafe on this corpus: this book's xref had to be
rebuilt on open (`"cross-reference table rebuilt by full object scan"`), and
appending a revision to a file whose xref chain is already broken produces a
document that only tolerant readers open.

**B. Passthrough rebuild.** Extract each page's original encoded image stream
and its placement matrix, and rebuild the document through the existing
`DocumentWriter` with those exact bytes plus our text layer.

**Recommended.** No re-encoding, output size ≈ input size, uses the writer as
designed, no change to its stated contract, and it repairs malformed sources as a
side effect. It does not preserve annotations or structure — acceptable, since
scanned books have none (this one: `annotations.total = 0`, no AcroForm, no
outline).

Adopt B now. Revisit A only if a real document needs annotation/form fidelity.

## 5. Work items

### 5.1 `lege-pdf/read` — expose the original page image

New public accessor, sibling to `direct_scan_image` but permissive and
placement-aware:

```rust
pub struct PreservedPageImage {
    pub codec: PreservedCodec,        // Dct | Jpx | CcittG4 | Jbig2 | Flate | Raw
    pub data: Arc<[u8]>,              // already-encoded stream, untouched
    pub width: u32,
    pub height: u32,
    pub color: PreservedColor,        // Gray | Rgb | Indexed{palette} | Bilevel
    pub bits_per_component: u8,
    pub black_is_one: bool,           // CCITT/JBIG2 polarity
    pub placement: Affine,            // the page's own `cm`, not a re-derived one
    pub media_box: PdfRect,
    pub rotate: i32,
}

pub fn preserved_page_image(session: &RenderSession, page: u32)
    -> Result<Option<PreservedPageImage>, ReadError>;
```

Predicate — accept when the page paints exactly one image and nothing else
*visible*:

- ignore all text-showing operators and text state entirely (we are replacing the
  text layer, so its presence must not disqualify the page) — this is the change
  that unblocks the archive-scan class;
- ignore `Save`/`Restore`/clip that does not reduce the painted image area;
- reject a second `DrawImage`, any path fill/stroke, any shading;
- require an axis-aligned matrix (`b`, `c` ≈ 0); allow negative `a`/`d` (flips)
  by carrying them in `placement`;
- **drop the 2% coverage rule** — the export honours the real matrix, so partial
  coverage or slight overflow is fine as long as it is the only mark on the page;
- carry `/Rotate` through rather than rejecting it.

Keep `direct_scan_image` as a thin wrapper over the new function, restricted to
`Dct` + gray/RGB, so the OCR intake fast path gains the text-layer-tolerant
predicate without further change. Expect this alone to move this book's 472 pages
from `Rendered` to `ScannedImage` and cut intake time.

### 5.2 `lege-pdf/write` — close two small gaps

- **`Flate` image variant.** `PdfImageResource` has no Flate/raw arm, and
  `Indexed8` is written with no `/Filter` at all
  (`lege-pdf/write/src/images.rs:142`). Flate-encoded scans are common enough
  that passthrough without it will fall back to rasterizing for no good reason.
  Add `Flate { data, width, height, color, bits_per_component, decode_parms }`
  emitting `/Filter /FlateDecode` with the source's `/DecodeParms` copied
  verbatim. Adding a variant is explicitly a plan change under
  `artifact.rs:1` — update `lege-pdf/PLAN.md` §4.2/§4.3 in the same commit.
- **Page rotation.** The writer emits no `/Rotate`
  (no match in `lege-pdf/write/src/`). Do *not* add it: bake rotation into the
  page's `Affine` and swap the media box in the export. Cheaper, and it keeps
  the text layer and the image in one coordinate system.

### 5.3 `crates/export` — implement the overlay

Restructure `SearchablePdfExporter::export` around one shared rebuild routine
that takes a per-page image source:

```
for page in document.pages:
    match preserved_page_image(session, page.index):
        Some(img) -> PdfPageArtifact { image: passthrough(img), transform: img.placement, .. }
        None      -> per policy: rasterize this page, or fail
    artifact.text_layer = page_text_layer(page, view)   // unchanged, already correct
```

`page_text_layer` (`crates/export/src/lib.rs:392`) needs no change: it scales
`source_size` → `page_size_points`, which stays valid regardless of how the image
got there. Note that for `ScannedImage` pages `source_size` is the
*downscaled-for-OCR* size (`limit_gray_pixels`, `crates/pipeline/src/lib.rs:352`),
not the JPEG's own size — that is fine and must stay that way, since the text
coordinates were produced in that space.

Also in this routine, for every policy:

- `writer.set_metadata(...)` from `lege_pdf_read::extract_metadata`;
- `writer.set_bookmarks(...)` from `lege_pdf_read::extract_outline`.

This fixes the metadata loss in rasterize mode too.

### 5.4 Policy and CLI semantics

Three policies instead of two:

| policy | behaviour |
|---|---|
| `preserve` | passthrough every page; **fail** if any page cannot be preserved, naming the page and the reason |
| `prefer-source` | passthrough where possible, rasterize only the pages that cannot be — report the count | 
| `rasterize` | today's behaviour, unchanged |

`prefer-source` becomes the CLI default (`--pdf-mode`,
`cli/src/main.rs:130`), because it is what a user asking for a searchable PDF
wants and it never fails a 472-page job at the last step.

Replace `ExportError::PreservingPdfOverlayUnsupported` with a variant that says
*which* page and *why* (`unsupported codec X`, `multiple images`, `rotated
matrix`), and suggests `--pdf-mode prefer-source`. The current message tells the
user to switch to `rasterize`, which is the worst of the three options.

Also fix the all-native byte-copy arm: route native/hybrid documents through the
same rebuild so corrections and OCR-replaced text reach the output. A verbatim
copy is only correct when no page's text was replaced and no correction was
applied; make that the explicit condition rather than `all_native`.

## 6. Tests

Extend `crates/export/src/lib.rs` tests (they already have `PdfBuilder` and use
`lege_pdf_read` to read results back — `lib.rs:1180`):

1. **archive-scan shape** — page = full-page JPEG + invisible text layer.
   Assert `preserved_page_image` returns `Some`, the exported page's image stream
   is **byte-identical** to the input's, and `page_text` returns the OCR text.
   This is the regression test for the bug that motivated the plan.
2. **size sanity** — exported bytes ≤ 1.15 × source bytes for a preserved page.
3. **mixed document** — one preservable page + one vector page; `preserve` errors
   naming page 2, `prefer-source` succeeds and rasterizes only page 2.
4. **metadata/outline carried** in both `prefer-source` and `rasterize`.
5. **rotation** — `/Rotate 90` page renders upright with correctly placed text.
6. **native+corrections** — a native page whose text was corrected must export the
   corrected text, not the source bytes.

End-to-end check on the real file: re-run this book with `--pdf-mode preserve`
and expect a ~300 MB output whose page 60 image stream matches source object 248,
against the 814 MB rasterized baseline.

## 7. Out of scope

Annotations, AcroForms, optional content, structure tree/tagging, PDF/A, and
incremental update. If any of those become requirements, that is design A and a
new writer capability, not an extension of this plan.
