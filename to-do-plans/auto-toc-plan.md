# Automatic Table of Contents: Plan

Goal: on any document, silently synthesize a navigable table of contents from
layout-detected chapter/section titles — always on, no knob, invisible unless
the user opens their reader's navigation panel. Plus: preserve pre-existing
bookmarks from the source document (advice only for the renderer-facing part,
since the renderer API is about to change).

---

## 1. What "bookmarks"/"TOC" mean in PDF and DjVu — and whether they render

**PDF has two distinct things:**

1. **The document outline** (`/Outlines` tree in the catalog — colloquially
   "bookmarks"). A tree of `{title, destination, children}` nodes. It is pure
   navigation metadata: nothing is drawn on any page; viewers show it in a
   sidebar/menu, and e-ink readers use it for their chapter list, chapter-skip
   buttons, and "% through chapter" progress display. This is the mechanism
   the feature should target.
2. **A rendered TOC page** — actual page content, optionally with link
   annotations (`/Annots` + GoTo actions) so lines are tappable. This *is* a
   rendering operation and it changes the document's pagination.

**DjVu has the direct equivalent of (1):** the `NAVM` chunk in the `DJVM`
container — a compressed bookmark list of `{title, dest, children}` with
`#p0007.djvu`-style destinations. Same properties: no visual change, shown by
the viewer's navigation UI.

**Answer to "is bookmarking a rendering operation or a user-facing choice?"**
The outline/NAVM is neither — it is *writer-side document metadata*, emitted
at finalize alongside the xref/DIRM, entirely orthogonal to rasterization and
encoding. It costs a few KB, alters no page, and needs no user decision —
which is exactly why it fits the "always on, unobtrusive" requirement. A
rendered TOC page is the only variant that is a rendering operation, and *it*
is a user-facing choice (it inserts a page, shifts page numbers, and imposes
styling). Recommendation: outline/NAVM always; rendered TOC page not built
now (a possible later opt-in, e.g. for readers with poor outline UI — but the
target e-ink readers all surface outlines).

---

## 2. What already exists (audit results)

The situation is far better than expected:

### PDF outline writing is complete and battle-ready
`lege-pdf/write/src/outline.rs` implements the full `/Outlines` tree
(First/Last/Prev/Next/Parent/Count linking, UTF-16BE titles, per-node
`/Dest [page /Fit]`, unresolvable nodes dropped). Input is
`OutlineItem { title, page_index (0-based output), children }` via
`DocumentWriter::set_bookmarks` (`writer.rs:95`), resolved against the page
slot table at finalize. **A synthesized TOC only needs to construct
`OutlineItem`s** — zero new writer code for PDF.

### Source-bookmark preservation already exists for PDF→PDF — with holes
- Extraction: `extract_bookmarks_from_bytes` (`src/pagerender.rs:822`) walks
  pdfium's bookmark tree → `Vec<OwnedBookmarkNode { title, source_page,
  children }>` (`pagerender.rs:771-777`).
- Transport: `PdfWriterHandle::send_bookmarks` →
  `WriterMessage::SetBookmarks { bookmarks, source_to_output }` → converted by
  `bookmarks_to_outline` (`helper_functions.rs:1290-1311`) at finalize.
- Wired in the main PDF pipeline (`pdf_tokio_pipeline.rs:3163-3178`) and in
  reflow (`reflow_pipeline.rs:479-497`, with a real source→output map).

**Why the user feedback "bookmarks are not preserved" can still be true:**
1. **DjVu output drops bookmarks entirely** — no `SetBookmarks` equivalent in
   `DjvuWriterActor`, no outline field in the manifest.
2. **Page-range runs corrupt/lose them** — the main pipeline sends an
   **identity** map (`HashMap::new()`, `pdf_tokio_pipeline.rs:3174`) even when
   a page range shifts output indices by `−page_start`; out-of-range entries
   are silently dropped *with their whole subtrees*.
3. Named-destination-only bookmarks may come back unresolved from pdfium
   (`source_page = usize::MAX`) and get dropped subtree-and-all.
4. The bookmark task is a **detached** spawn; if extraction loses the race
   with finalize or errors, the outline is silently absent.

First implementation step is therefore verification: run a bookmarked PDF
through PDF→PDF full-document and confirm whether passthrough works today, so
the fix lands where the actual breakage is.

### The DjVu side is "commented out", not missing
`../djvulibrust` contains a complete NAVM implementation — `Bookmark`,
`DjVmNav` with the binary NAVM encoder (`src/doc/djvu_dir.rs:705-797`),
`PageCollection::set_navigation` — but the container assembler has NAVM
writing deliberately disabled (`encoder.rs:57-67, 177-186`), and the
DJVM offset math **already reserves a `nav_chunk_size` slot** (currently 0).
Re-enabling is uncommenting + threading, not new format work.

### Detection: the model already sees titles, but the info is thrown away
- PP-DocLayout-M's 23 classes include `paragraph_title` (id 0), `doc_title`
  (11), plus caption classes and — notably — `content` (id 5) = a *printed
  TOC page* (`src/types.rs:54-78`). All of these collapse into
  `ContentCategory::Text`, and `ContentElement` (`src/accumulator.rs:104`)
  carries no class/confidence — **title identity is erased before the
  writer**. `Detection` retains `class_name`, `confidence`, bbox
  (`src/engine_impl.rs:9-17`).
- The doc-wide `detection_cache` exists but is only populated in the two-pass
  margin modes (`pdf_tokio_pipeline.rs:2957-2977`); not a reliable vehicle.
- OCR is already region-guided by detections, and the EPUB path proves the
  exact association we need: `BlockKind::from_class_name` maps
  `doc_title`→`Title`, `paragraph_title`→`SectionHeading` with structured
  lines/words/bboxes (`lege-ocr/src/document.rs:17-60`), and
  `is_geometric_chapter_start` (`epub_pipeline.rs:569`) already implements a
  blank-gap chapter heuristic (`CHAPTER_BLANK_GAP_RATIO = 0.20`).
- In the PDF/DjVu pipelines the per-region structure is flattened to one hOCR
  string, but `src/hocr.rs::parse_hocr` round-trips it to words+bboxes in the
  same page-pixel space as the (output-space) detections, so "text inside
  this title bbox" is a cheap intersection.

---

## 3. Feature design

### 3.1 Candidate capture (in the process stage, where everything coexists)

In `process_page_cpu_work` / `process_single_djvu_page`, after detections are
in output space and hOCR exists, emit per page:

```rust
pub struct TocCandidate {
    pub page_index: usize,        // output space (see §3.4)
    pub kind: TitleKind,          // DocTitle | ParagraphTitle
    pub confidence: f32,          // detection confidence
    pub bbox: [f32; 4],           // output page pixels
    pub text: String,             // hOCR words intersected with bbox
    pub line_height: f32,         // median word height inside bbox
    pub page_height: f32,         // for position normalization
}
```

Carried as `Vec<TocCandidate>` on `ProcessedPage` (PDF) and
`DjvuBinarizedData` (DjVu) — tiny, text-only, no raster payload. The writer
forwarder accumulates them document-wide (mirroring how `hocr_pages` is
accumulated for EPUB).

Candidates cost nothing extra: detections and hOCR are already computed. When
OCR is disabled, `text` is empty and the candidate is still usable with a
fallback title (see 3.3).

### 3.2 Doc-wide verification (the "solid" part — runs once, at finalize)

A pure function `fn build_outline(candidates, total_pages, body_stats) ->
Vec<OutlineItem>` in a new `src/toc.rs`, unit-testable in isolation. Scoring,
not hard gates — each candidate accumulates evidence; a conservative
threshold decides inclusion; ambiguity resolves toward *emitting nothing*.

Signals (boosts/penalties):
- **Relative size**: candidate `line_height` vs the document's body-text
  median line height (computed from all hOCR lines). Chapter heads in real
  books are reliably taller than body text; same-size "titles" are usually
  false positives (running heads, emphasized lines).
- **Position & isolation**: bbox top within the upper portion of the page;
  vertical whitespace below the title before the next text (reuse the EPUB
  blank-gap idea) — chapter openings have air around them.
- **Text shape** (boost only, never required — must stay language-neutral):
  short (≲ 12 words); matches `Chapter/Part/Book/Kapitel/Chapitre/§ + number`,
  bare Roman/Arabic numerals, or ALL-CAPS/heading capitalization; and if a
  numbering sequence is detected across candidates, monotonicity strongly
  boosts conforming candidates and demotes violators.
- **Repetition kill**: identical (normalized) text appearing on ≥3 pages is a
  running header, not a chapter — discard all instances. (Complements the
  model's own `header`/`footer` classes, which are already excluded.)
- **Density sanity**: expected chapters scale with page count. Cap at ~1
  entry per page hard, and if >1 in ~3 pages sustained, keep only the
  strongest per window — a book is not 40% chapter openings. If fewer than 2
  entries survive, emit nothing (a 1-entry TOC is noise).
- **Confidence floor**: require detection confidence well above the 0.2
  pipeline default (start ~0.5, tune on the corpus).

Hierarchy: `doc_title` and the largest `paragraph_title` size-cluster become
level 0; a clearly separated smaller size-cluster becomes level 1 (children of
the preceding level-0 entry). At most two levels — deeper synthetic trees are
guesswork.

Title text: candidate hOCR text, whitespace-normalized, truncated ~120 chars.
If OCR was off or empty: `"Chapter N"`-style fallback **only** when a
numbering sequence was confidently detected; otherwise skip the candidate —
"Page 217" entries are exactly the obtrusive noise the feature must not make.

Stretch (later phase): the model's `content` class detects *printed TOC
pages*. When present, OCR that page and cross-validate entry count/titles
against the synthesized outline — a strong document-level confirmation signal,
and potentially a superior TOC source by itself (titles as the author wrote
them, page numbers resolvable via printed-page-number OCR).

### 3.3 Merge policy — how "always on" stays unobtrusive

Precedence at finalize:

1. **Source outline exists and survives remapping** (≥1 resolved entry) →
   preserve it verbatim; **do not synthesize.** An author's TOC is ground
   truth; silently replacing or interleaving it would be obtrusive.
2. **No source outline** → attach the synthesized outline if it passed
   verification.
3. **Neither** → no `/Outlines` / no NAVM at all (writer already emits nothing
   for an empty list — the document is byte-wise indistinguishable from
   today's output).

This ordering is *why* no on/off switch is needed: the feature can only ever
add navigation where none existed, and its failure mode is absence. (A single
escape-hatch env var, e.g. `LEGE_NO_AUTO_TOC=1`, for debugging is cheap
insurance and satisfies "shouldn't have to be turned on or off" as a default,
not a legal prohibition.)

Required plumbing fix: `SetBookmarks` is currently last-write-wins
(`helper_functions.rs:1244`), and the source-bookmark task is detached. Make
finalize wait on both inputs (source bookmarks, synthesized candidates) and
run the merge policy in one place.

### 3.4 Page-index correctness

`OutlineItem.page_index` is 0-based **output** space. Blank pages are stomped
white but never removed (1:1 count), so:
- Full-document PDF→PDF: identity.
- Page-range runs: subtract `page_start` — fixing the existing identity-map
  bug for preserved bookmarks at the same time (`pdf_tokio_pipeline.rs:3174`).
- Reflow: use the existing `src_to_out` placement map for preserved
  bookmarks; synthesized candidates are born in output space already.

### 3.5 Destination quality (small, high-polish option)

The outline currently emits `/Dest [page /Fit]`. Since candidates carry the
title bbox, extend `OutlineItem` with an optional `top: Option<f32>` (PDF user
space) and emit `/XYZ null top null` so tapping a chapter lands *at the
title*, not just the page top. Trivial in `outline.rs`; page-level `/Fit`
stays the fallback for preserved bookmarks (source position data is a
new-renderer question, §5).

---

## 4. Output wiring per format

### PDF
Nothing new in the writer beyond the optional `/XYZ` destination. Work is:
candidate capture, `src/toc.rs`, the merge-at-finalize rework, page-range map
fix.

### DjVu (new, but mostly reconnection)
1. Manifest schema **v2**: add optional document-level
   `outline: Vec<OutlineEntry { title, page_index, children }>` to both
   `Manifest` structs (`src/djvu.rs:261`, `djvu-encoder.rs:145`); bump
   `MANIFEST_SCHEMA_VERSION` on both sides. Optional field keeps v1 manifests
   readable if desired.
2. `DjvuWriterActor`: accept a `SetOutline` message (mirror of PDF's
   `SetBookmarks`), run the same merge policy, serialize into the manifest at
   finalize.
3. djvulibrust: map manifest outline → existing `Bookmark`/`DjVmNav`
   (`dest: format!("#p{:04}.djvu", page+1)` matching the disabled example),
   call `set_navigation`, re-enable the NAVM block in `assemble_djvm`
   (`encoder.rs:57-67, 177-186`) and thread `nav` through
   `DjvuBuilder::finalize` / `PageCollection::collect_pages`. The offset math
   already carries `nav_chunk_size`.
4. This same channel is what finally gives DjVu **preserved** source
   bookmarks too — the current silent drop is half of the user's complaint.

### EPUB (later, unification)
EPUB already self-builds a TOC from its geometric heuristic. Once `src/toc.rs`
exists, `assemble_chapters` can consume the same scored candidates so all
three formats agree on what a chapter is. Not needed for v1.

---

## 5. Preserving source bookmarks under the new renderer (advice only)

The good news from the audit: **everything downstream of extraction is
renderer-agnostic.** The entire preservation feature hangs off one seam:

```rust
fn extract_bookmarks(&self) -> Vec<OwnedBookmarkNode>
// OwnedBookmarkNode { title: String, source_page: usize, children: Vec<..> }
```

Advice for the new renderer integration:

1. **Keep this exact contract** — flat, owned, resolved-to-page-index tree.
   The transport (`SetBookmarks`), remapping (`bookmarks_to_outline`), and
   emission (`outline.rs`) need no changes when the renderer swaps.
2. **Outline extraction is document parsing, not rendering.** It walks
   `catalog → /Outlines → First/Next` dicts; no rasterization is involved. So
   it does not *have* to live in the renderer at all — if the new renderer
   doesn't expose bookmarks on day one, a small standalone reader (natural
   home: a `lege-pdf/read` sibling of `lege-pdf/write`) can own it, and the
   pipeline seam stays identical. Don't block renderer selection on bookmark
   support.
3. **The hard 20% is destination resolution**, which pdfium currently does for
   free: outline items may target pages via direct `/Dest` arrays, **named
   destinations** (`/Dests` name tree lookups), or `/A` GoTo actions — all
   three occur in the wild and must resolve to a page index. This is the
   checklist item to test against the new renderer.
4. Two current behaviors worth *changing* at the same time, whoever owns
   extraction: unresolvable nodes drop **their entire subtree**
   (`bookmarks_to_outline`) — better to promote resolvable children; and
   consider carrying the destination's Y-offset so preserved bookmarks can
   also use `/XYZ` targets (§3.5).
5. The page-range identity-map bug (§3.4) is pipeline-side and can be fixed
   now, independent of the renderer swap.

---

## 6. Implementation phases

- **Phase 0 — verify the complaint.** Round-trip bookmarked PDFs (full doc,
  page range, PDF→DjVu) and record exactly where preservation fails today.
  Assemble a small test corpus for TOC synthesis: novels, scanned books with
  printed TOCs, music scores, papers — including documents with *no* chapters
  (the must-emit-nothing cases).
- **Phase 1 — plumbing fixes** (small, immediately shippable): page-range
  source→output map; merge-at-finalize instead of last-wins `SetBookmarks`;
  un-detach the bookmark task; promote children of unresolvable nodes.
- **Phase 2 — candidate capture + `src/toc.rs`** with the scoring heuristics,
  behind the merge policy; PDF output only. Unit tests on synthetic candidate
  sets + corpus snapshot tests (assert the emitted outline per document).
- **Phase 3 — DjVu NAVM**: manifest v2, `SetOutline`, djvulibrust
  re-enablement. Both preserved and synthesized outlines arrive together.
- **Phase 4 — polish**: `/XYZ` destinations, printed-TOC (`content` class)
  cross-validation, EPUB unification.

## 7. Risks

- **False positives are the reputation risk** — a wrong TOC is worse than no
  TOC. Hence: scoring with conservative thresholds, emit-nothing default,
  never overriding an existing outline, ≥2-entry minimum, corpus snapshots as
  the regression net.
- **OCR quality gates title text.** Garbled titles read as broken. Mitigate:
  per-word `x_wconf` is available in hOCR — drop candidates whose words are
  low-confidence rather than emitting mojibake.
- **Music-sheet edition**: scores have `paragraph_title`-like headings
  (movement titles) — likely a *feature* there, but verify on score scans
  before shipping the shared default.
- **Manifest schema bump** must land in lockstep with the encoder binary
  (existing version handshake rejects mismatches — fail-fast already built).
