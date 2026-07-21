# lege-pdf-write implementation plan

Mid-level plan to realize the shape in `../lopdf-replacement.md`: a typed,
append-only, image-oriented PDF emitter that (A) replaces every lopdf use in
Lege — which is exactly `src/accumulator.rs`, the only consumer — and (B) does
nothing more. The inputs are fully known: Lege's own encoders and OCR engines
produce them. There is no unknown-PDF handling anywhere in this crate.

```text
lege-pdf/
├── render/   pdf-renderer workspace moves here (own workspace, excluded)
└── write/    lege-pdf-write (this plan; member of the Lege root workspace)
```

---

## 1. Scope: the complete input vocabulary

Everything the writer will ever be asked to emit, taken from the current
`accumulator.rs`. This list is closed — a request outside it is a bug in the
caller, not a feature request for the writer.

**Image payloads** (already-encoded bytes, never re-encoded):

| Today's stringly tag | Becomes | PDF dictionary essentials |
|---|---|---|
| `"jpeg"` / `"jpeg-gray"` | `Jpeg { color: Rgb \| Gray }` | DCTDecode, DeviceRGB/DeviceGray, 8 bpc, Interpolate true |
| `"jp2"` / `"jp2-gray"` | `Jpx { color }` | JPXDecode, DeviceRGB/DeviceGray, 8 bpc, Interpolate true |
| `"jbig2"` | `Jbig2 { globals: None, image_mask: false }` | JBIG2Decode, DeviceGray, 1 bpc, Interpolate false, Decode [0 1] |
| `Jbig2ImageWithGlobals` | `Jbig2 { globals: Some(id), image_mask: false }` | + DecodeParms /JBIG2Globals ref; globals deduped via registry |
| `Jbig2Mask` | `Jbig2 { image_mask: true }` | ImageMask true, no ColorSpace, Decode [0 1], painted after `0 g` |
| `"ccitt"` / `"ccitt4"` | `CcittGroup4` | CCITTFaxDecode, K -1, Columns, Rows, BlackIs1 true, EndOfBlock false, EncodedByteAlign false, Decode [1 0], 1 bpc, Interpolate false |
| `"indexed8"` | `Indexed8 { palette, indices }` | [/Indexed /DeviceRGB 255 ref], palette split out as its own stream (today: first 768 bytes of the blob — the artifact type makes the split explicit instead) |

**Page content operators** — the entire set, forever:
`q Q g cm Do` (images) and `BT ET Tr Tc Tw Tz Tf Tm Tj` (invisible text).

**Document-level objects**: page dicts + MediaBox, page tree, catalog,
Info dict, OutputIntent (GTS_PDFA1 / sRGB strings), outline tree with
source→output page mapping (reflow), embedded OCR font graph, identity
ToUnicode CMap, classic xref + trailer.

**Non-goals (permanent)**: no parsing, no object resolution, no editing of
existing PDFs, no encryption, no incremental update, no filter decoding, no
text extraction, no font subsetting (see §7), no linearization (see §2), no
transparency/graphics beyond the operator set above. Object streams and xref
streams are deferred behind profiling (M4) and may never happen — for
image-dominated output they compress only the small dictionaries.

---

## 2. "Linearization" — what it is and why we will never do it

It is **not** sRGB linearization. It is *Linearized PDF* (PDF 32000-1, Annex F),
aka "Fast Web View": a file organization where page 1's objects, plus special
*hint tables*, are placed at the physical front of the file so a viewer
fetching the PDF over HTTP byte-ranges can display the first page before the
download finishes.

Why the source doc rates it "very high" difficulty:

1. The hint tables encode the **byte offset and length of every object and
   every page's object group** — you cannot know these until the whole file is
   written, so writing them at the *front* forces either a full two-pass write
   or write-then-patch with self-referential offset arithmetic (the hint
   stream's own length changes the offsets it records).
2. It prescribes a specific physical object order (first-page objects first,
   shared objects grouped), which is the exact opposite of our
   **append-only, arrival-order** design — supporting both would poison the
   architecture the whole crate exists to enable.
3. Almost nothing validates it; broken hint tables silently degrade to normal
   reading, so bugs hide.

Why it's worthless for Lege: linearization only pays off for **remote
byte-range streaming**. Lege's output is written to local disk and side-loaded
onto an e-ink reader, which has the entire file before opening it. Decision:
**out of scope permanently**, not "deferred". Delete it from mental load.

---

## 3. Crate boundary and shared types with `render/`

Per the source doc: render and write share *compact value types*, not
internals. The renderer workspace's leaf crate `pdf-page-ir` has the geometry:
`Matrix { a,b,c,d,e,f: f64 }`, `Rect { x0,y0,x1,y1: f64 }`, `Point`, re-exported
at its crate root. NOTE (verified): the crate is *not* literally
dependency-free — it pulls `bitflags` and its `geom` module ships alongside the
whole `CompiledPage` IR, so depending on it drags in more than geometry.
`geom.rs` itself is std/core only. This is why the writer keeps its own geom for
now (below) and we revisit at M5 — likely by factoring a truly-leaf `pdf-geom`
crate rather than depending on `pdf-page-ir`.

Sequencing so write doesn't block on the render move:

- **Now**: `write/src/types.rs` defines its own minimal `Affine` (6×f64,
  fields `a,b,c,d,e,f` to mirror `pdf-page-ir::Matrix` exactly),
  `PdfRect`, `ObjectId { num: u32, gen: u16 }` (offsets are `u64` everywhere —
  the doc's §8 warns lopdf's u32 offsets bite on large scanned books), and
  `ResourceName` (small fixed formatting like `Im12`, `F0` — no heap Vec per
  name). Field layout deliberately mirrors `pdf-page-ir::geom`.
- **After the render move**: replace `types.rs` geometry with re-exports from
  `pdf-page-ir` (one commit, no semantic change). The writer must never
  depend on any render crate other than `pdf-page-ir`.
- All coordinates entering the writer are already in **PDF user space,
  bottom-left origin**. The top-left→bottom-left flip (`page.height - y - h`)
  and the raster-pixel→point scaling stay in Lege's pipeline adapter, not in
  the writer. The writer applies matrices verbatim.

f32→f64: lopdf's `Object::Real(f32)` narrowing goes away; the writer formats
f64 with a shortest-representation real formatter (fixed precision, no
exponent notation — PDF reals don't allow `e` syntax).

---

## 4. Architecture

### 4.1 Data flow (write-on-arrival, no reorder buffer)

```text
pipeline workers (parallel, out of order)
    │  PdfPageArtifact { index, media_box, elements, text_layer, resident_bytes }
    ▼
bounded mpsc channel  ←— capacity + memory permits = backpressure
    │
    ▼
DocumentWriter (single task, owns PdfSink)
    ├─ on artifact arrival: allocate arrival-order ObjectIds, write image
    │  streams (zero-copy from Arc<[u8]>), content stream, page dict;
    │  record page ObjectId in WrittenPageSlots[logical_index]; release permit
    └─ finalize(): balanced page tree, catalog, outlines, Info/OutputIntent,
       classic xref, trailer, startxref, flush+sync
```

- **Arrival-order IDs** (doc §12's recommendation): object numbering varies
  run-to-run with worker timing; pixels and logical page order are still
  deterministic. Byte-stable output is explicitly not a requirement.
- **WrittenPageSlots**: `Box<[Option<ObjectId>]>` sized from the known page
  count (`Box<[AtomicU32]>` unnecessary — only the writer task touches it).
  Replaces `Arc<Mutex<BTreeMap<usize, Page>>>` entirely.
- **Backpressure**: the artifact's exact `resident_bytes()` (unique data only;
  shared resources budgeted once by the registry) is acquired by the worker
  before encoding and released by the writer after the bytes hit the
  `BufWriter`. A slow disk throttles encoding. This replaces the racy
  `load/add/store` counter and the process-global `TOTAL_ACCUMULATED_MEMORY`.
- **Failure**: writer error ⇒ drop the receiver ⇒ every producer's `send`
  fails ⇒ pipeline aborts with the writer's error, no hang.

### 4.2 The artifact contract (write/src/artifact.rs)

As in doc §10, adapted to what Lege actually sends today:

```rust
pub struct PdfPageArtifact {
    pub index: u32,                       // logical page index
    pub media_box: PdfRect,
    pub elements: Box<[PdfImageElement]>, // draw order = paint order (bg before mask)
    pub text_layer: Option<PreparedTextLayer>,
}
pub struct PdfImageElement {
    pub transform: Affine,                // full cm matrix; today always scale+translate
    pub image: PdfImageResource,          // the closed enum from §1
}
pub struct PreparedTextLayer {            // PDF-space, ready to emit
    pub runs: Box<[TextRun]>,             // { text: String, x, y, size: f64 }
    pub font: TextFont,                   // Embedded(SharedResourceId) | HelveticaFallback
}
```

`PreparedTextLayer` is the OCR boundary: **hOCR parsing, line grouping,
dedup_adjacent_repeats, baseline math, and the Y flip all stay in Lege.**
(Correction from a verification pass: `parse_hocr` is NOT shared with
`src/djvu.rs` — djvu has its own independent `parse_hocr_to_words`. The one
external consumer of `accumulator::parse_hocr` is `src/pipeline/epub_pipeline.rs:292`.
So the helpers relocate to a neutral `src/hocr.rs` for the EPUB path plus the
new text adapter.) A new small adapter in Lege (`src/hocr.rs` + a
pipeline-side converter) turns `HocrLine`s + page height → `TextRun`s exactly as
`emit_invisible_text` positions them today (x, `page_height − (y + h)`,
size = word height clamped ≥ 1.0, trailing space joined onto non-final words).
The writer does zero XML and zero heuristics.

### 4.3 Emission fidelity

M1/M2 are **byte-semantics parity ports** of `accumulator.rs`'s dictionaries
and operators (the table in §1, the font graph in §7). Fidelity is verified
differentially (§9), not re-derived. Two deliberate non-parity changes:

- Palette split for Indexed8 is typed instead of a 768-byte prefix convention.
- Content streams: always Flate-compress when a text layer is present (same
  as today), never otherwise (same as today) — but through a streaming zlib
  writer into a reusable buffer, not encode-all-then-compress.

Known lopdf-era bugs that must NOT be replicated: the ineffective
`use_xref_streams(false)` path (our output mode is structural — M1 has only
classic xref, so the bug class is unrepresentable); the PDF/A-1.4 vs
object-stream contradiction (profiles, §8); memory races and `into_pages()`
cloning (gone with the accumulator itself).

---

## 5. Module map (write/src/), with build order

Stubs already exist with doc headers. Sizes from doc §19.

| # | Module | Responsibility | Est. lines |
|---|---|---|---|
| 1 | `types.rs` | ObjectId, Affine, PdfRect, ResourceName, WriteError | 150–250 |
| 2 | `serialize.rs` | real/int formatting, name & string escaping, dict/array framing, `PdfValue<'a>` for cold paths | 400–600 |
| 3 | `content.rs` | typed ContentWriter: the 14 operators, nothing else | 250–400 |
| 4 | `sink.rs` | append-only PdfSink: BufWriter, u64 offsets by object number, `begin_obj/end_obj`, StreamBody {Shared, Owned} zero-copy | 400–600 |
| 5 | `xref.rs` | classic xref table + trailer + startxref | 250–400 |
| 6 | `artifact.rs` | §4.2 types + exact `resident_bytes()` | 200–300 |
| 7 | `images.rs` | one typed writer per codec (§1 table), incl. globals wiring | 600–900 |
| 8 | `resources.rs` | `SharedResourceId → ObjectId` registry; write-once semantics | 250–400 |
| 9 | `pages.rs` | page dict emission, WrittenPageSlots, balanced page tree (flat < 256 pages, fan-out above), catalog | 300–500 |
| 10 | `font.rs` | embedded font graph parity port (§7) | 300–500 |
| 11 | `text.rs` | PreparedTextLayer → BT…ET via ContentWriter | 150–300 |
| 12 | `outline.rs` | bookmark tree linking (First/Last/Prev/Next/Parent/Count), GoTo dests from slots | 300–500 |
| 13 | `meta.rs` | Info dict, OutputIntent, `PdfProfile` (§8) | 200–400 |
| 14 | `writer.rs` | DocumentWriter: channel intake, per-page commit, finalize ordering | 400–700 |

≈ 4.2–6.7k lines including unit tests — in line with the doc's estimate once
tests are counted. Modules 1–5 have no internal dependencies on 6–14 and are
pure functions over bytes: build and unit-test them first.

---

## 6. Milestones

Status legend: ✅ done · ◑ partial/blocked.

### M0 — scaffold ✅
Folder structure, crate skeleton in the workspace, this plan.

### M1 — image-only documents ✅ (the big win)
Modules 1–9 + `writer.rs`. All seven codec dictionaries unit-tested against the
accumulator fidelity table. Validated end-to-end: the `wrap_jpeg` example
produces a PDF that `pdfinfo` parses and `pdftoppm` renders with pixel stats
matching the source JPEG; the JBIG2 path is validated through the real pipeline
(§ M3). (`qpdf`/`mupdf` unavailable in this environment — poppler used instead;
the peak-RSS-vs-lopdf comparison is left as a follow-up benchmark.)

### M2 — OCR text layer + outlines + metadata ✅
`font.rs` (glyphless Type0/Identity-H graph + identity ToUnicode), `text.rs`
(invisible-text emission, UTF-16BE incl. surrogate pairs), `outline.rs`
(First/Last/Prev/Next/Parent/Count + `[page /Fit]` dests, UTF-16 BOM titles),
`meta.rs` (`PdfProfile`, Info, OutputIntent). Validated: `pdftotext` extracts
the emitted text (Helvetica path standalone; embedded-glyphless path through the
real pipeline OCR run, including a CJK glyph and digits), and content streams
are FlateDecode-compressed and decode in poppler.

### M3 — pipeline integration & accumulator retirement ✅ (goal A met)
`spawn_pdf_writer_actor` (`helper_functions.rs`) now owns a `DocumentWriter` and
writes pages in **arrival order** (reorder buffer removed); the pipeline still
sends `accumulator::Page`, converted by the new `src/pdf_artifact.rs` adapter
(+ globals registered by content hash). hOCR parsing moved to `src/hocr.rs` (for
`epub_pipeline` and the adapter). `accumulator.rs` is gutted to plain DTOs;
`StreamingPdfBuilder`/`PageAccumulator`/`assemble_pdf`/`create_pdf_in_memory`
deleted. **lopdf is removed from `Cargo.toml`** entirely (no dev-oracle kept).
Validated end-to-end on a real 3-page scan for both `--text-format jbig2` and
`--ocr`.

### M4 — honest profiles, optional compression (measure first) ✅ (decision recorded)
`PdfProfile { Pdf14, Pdf17, PdfA1b }` seam is built and drives version + PDF/A
metadata. **Decision:** object streams / xref streams are NOT implemented —
output is image-dominated (e.g. ~50 KB for a 3-page JBIG2 scan) and object
streams only compress the small dictionaries, so the measured value is
negligible (matches the doc's expectation). The actor emits **Pdf17**, so the
output makes **no false PDF/A claims** (the old code claimed PDF/A-1b via
Keywords/OutputIntent yet never emitted them — `has_ocr` was hardcoded `false`).
The `PdfA1b` path exists and emits OutputIntent/MarkInfo/Info, but is documented
as **not fully conformant** (no XMP packet, no embedded ICC / `/DestOutputProfile`);
turning it on is a future opt-in that must add those first.

### M5 — cleanup ◑ (lopdf done; geom blocked on render move)
lopdf is fully out of the build. The `types.rs` → `pdf-page-ir` geom
reconciliation is **blocked on the `render/` move** (still a placeholder); until
then the writer keeps its own geom, with field names/types mirroring
`pdf-page-ir::Matrix`/`Rect` so the future swap stays mechanical.

---

## 7. OCR: why the doc calls it hard, and why parity is not

The doc's "moderate–high" rating (§18) covers making Unicode OCR text
*generally correct* — subsetting, width arrays, non-BMP planes, vertical
scripts, PDF/A font conformance. **None of that binds us at parity**, because
the current scheme is a deliberate invisible-text hack that we port verbatim:

- Text renders in mode `Tr 3` (invisible). Glyph *rendering* correctness is
  moot — only extraction and search matter.
- Encoding is Identity-H with text encoded as UTF-16BE code units, so
  CID = code unit; `CIDToGIDMap /Identity` maps those to arbitrary glyphs of
  the embedded font (wrong glyphs, invisible anyway); the **identity ToUnicode
  CMap** maps them straight back to Unicode for extractors. `DW 1000`, no `/W`
  array — extraction doesn't need widths.
- The font program is embedded whole (`FontFile2` + `Length1`), memoized once
  per document, and only when a page actually has text. The Helvetica `F1`
  non-embedded fallback (WINDOWS_1252-encoded) exists for the no-font-asset
  case and is ported as-is. Correction from a verification pass: the embedded
  font is already a **glyphless ~1 KB TTF** (`unicode_font.rs:43-53`,
  `glyphless_font::build_glyphless_ttf()`), not the ~1 MB system font this plan
  first assumed. `FontMetrics` (`units_per_em, ascent, descent, cap_height,
  italic_angle, bbox`) maps directly to the FontDescriptor.

So M2 is a mechanical port of a known object graph (~10 dictionaries/streams)
plus the text operator emission — the doc's own "preserve the current font
behavior exactly and differential-test" advice. The genuinely hard OCR work
(surrogate pairs render as two CIDs but extract correctly via identity CMap —
verify with a test; real width arrays) is **deferred and optional**. Font
**subsetting is moot**: the program is already a minimal glyphless font, so the
per-page memoization is a micro-optimization kept only because it is cheap and
correct, not a memory lever.

Caveat to carry into M2 tests: extractors that ignore ToUnicode and trust
Identity-H still get correct text (CID = Unicode); extractors that trust
ToUnicode also get correct text. Both paths must be in the differential suite
(pdftotext -layout, mupdf draw -tt, pdfium GetText).

## 8. PDF/A honesty (M4, but decided now)

The current output *claims* PDF/A-1b (Keywords, OutputIntent) but is not
conformant: no XMP metadata stream, no embedded ICC profile behind the
OutputIntent (`/DestOutputProfile` missing), object streams enabled on a
"1.4" file, non-embedded Helvetica. M1–M3 reproduce the current claims
verbatim (parity), M4 makes the profile explicit: either implement real
PDF/A-1b (XMP packet, embedded sRGB ICC, all fonts embedded ⇒ drop the F1
fallback under that profile) or stop claiming it. Decision deferred to M4 but
the `PdfProfile` seam is built in M1 so nothing scatters.

## 9. Testing strategy

- **Unit**: serializer escaping/real-formatting tables (steal lopdf's test
  vectors — MIT), content-writer golden bytes, xref layout, resident_bytes.
- **Differential (the workhorse, M1–M3)**: run the same Lege job through the
  lopdf path and the new writer; compare (a) `qpdf --check` clean, (b)
  rasterized pages SSIM ≈ 1.0 via pdfium, (c) extracted text equality, (d)
  outline structure via `mutool show`. Keep the lopdf path callable behind a
  `--legacy-writer` debug flag until M5.
- **Adversarial inputs the pipeline actually produces**: 0-element blank
  pages, pages with only a mask, duplicate page submission (today's silent
  BTreeMap overwrite becomes an explicit error), missing pages at finalize
  (today's hang-diagnosis via `missing_indices` becomes a finalize error
  listing the gaps), 2000+ page docs (page-tree fan-out, >4 GiB offsets on
  the sink's u64 path with a synthetic sparse test).
- **Concurrency**: loom-style or stress test that writer failure unblocks all
  producers; permit accounting sums to zero at exit.

## 10. Guardrails ("do no more than that")

- No `pub` API for constructing arbitrary PDF object graphs; the crate's
  public surface is `PdfPageArtifact`, `DocumentWriter`, `PdfProfile`,
  registry handles, and errors. `serialize.rs`/`sink.rs` are `pub(crate)`.
- The image enum and operator set are closed; extending them requires editing
  this plan first.
- No feature flags for hypothetical outputs (no encryption, no tagging, no
  attachments). If the renderer needs shared types, they go in `pdf-page-ir`,
  not here.
