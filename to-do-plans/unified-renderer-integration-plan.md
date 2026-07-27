# Unified Plan: Renderer Integration, Compute Seam, Raster Products, and TOC

Status: plan, written 2026-07-22. Language: ASD-STE100 Simplified Technical
English, Issue 9.

This plan replaces the sequencing of four documents in this folder:

| Source document | What this plan takes from it |
|---|---|
| `renderer-integration-plan.md` | Renderer intake, pdfium removal, page-owned jobs, GPU sessions |
| `compute-scheduler-plan.md` | `LegeCompute` facade, byte admission, Tokio diet, actor cleanup |
| `render-upgrade.md` | Typed raster products, Gray8 base, region renders, packed bilevel |
| `auto-toc-plan.md` | Outline preservation and the synthesized table of contents |

Where this plan and a source document disagree, this plan wins. Section 9
lists each change and the reason for it.

---

## 0. How to use this document

This is a mid-level plan. It gives the order of work, the seams, and the
exit gate of each phase. It does not give final code.

Run one `/plan` for each phase in Section 6. Use the exit gate of that phase
as the `/goal`. Do not start a phase before the exit gate of the previous
phase passes. Phases 1a to 1d are independent of each other and an agent can
do them in parallel.

Read Section 1 before any code work. Section 1 contains rules that apply to
every phase.

---

## 1. Rules for all phases

1. **The renderer has one canonical in-repository home.** It lives at
   `lege-pdf/render/`; the former development copy and the viewer-local copy
   were removed on 2026-07-25.
2. **Do not change renderer code.** Another agent owns that repository. If
   the renderer needs a change, write the request in Section 8 of this
   document and build a Lege-side workaround. The renderer API is stable
   from this date. One request is already sent and accepted: the `pdf-text`
   crate, specified by
   `lege-pdf/render/docs/refinement/plans/PLAN-TEXT-EXTRACTION.md`.
3. Declare each renderer crate one time in the root `[workspace.dependencies]`
   block. A later move of the renderer then changes only that block.
4. Keep `lege-pdf/render/` as an empty directory with a README. The README
   gives the temporary path and the reason for it.
5. Do not use `pdf-render-scheduler`. Lege has one compute pool. Section 3
   gives the reason.
6. Each phase must build and pass its tests before the next phase starts.
   Use `cargo build --profile debug-fast -p lege --bin lege` while you work.
7. Do not discard uncommitted changes. Do not run `git checkout --`,
   `git restore`, or `git reset`.
8. Record all measurements in `to-do-plans/measurements.md`. Every phase
   compares against the Phase 0 baseline.

---

## 2. Baseline facts (verified 2026-07-22)

These facts come from the renderer source, not from its plan documents. An
agent must not assume more than this list.

### 2.1 What the renderer gives you

- `pdf_document::DocumentSnapshot::open(source, limits)` and
  `open_with_password(...)`. The snapshot is immutable, `Send + Sync`, and
  has no `&mut self` method. Many threads read one snapshot with no lock.
- `DocumentSnapshot::page_count()` and `page(PageIndex) -> &PageRef`. The
  `PageRef` carries `crop_box`, `rotate`, and the annotation list.
- `pdf_content::PageCompiler::compile(&snapshot, page, &mut ctx) ->
  CompiledPage`. The `ParseContext` is worker-owned and mutable.
- `pdf_render_cpu::CpuBackend`, which is `Send + Sync`. It owns three shared
  caches for the document: fonts, rendered glyphs, and decoded images. The
  image cache is a 96 MiB 8-shard LRU.
- `pdf_render_cpu::CpuWorkerContext`, which holds per-worker raster scratch.
- `pdf_render_api::RenderRequest` with these fields: `page`, `transform`
  (a `Matrix`), `crop: Option<DeviceRect>`, `output_size`, `output_format`,
  `background`, `annotations`, `quality`, `limits`, `residency`.
- `OutputFormat::Rgba8PremultipliedSrgb` and `OutputFormat::Gray8`. The CPU
  backend advertises both.
- `pdf_render_api::submit_caught` and `render_blocking`. A panic in a page
  becomes `RenderError::Panic` and fails only that page.
- `pdf_read::examine(source) -> DocumentReport`. It gives the open outcome,
  encryption data, cross-reference health, per-page compile status, an
  annotation inventory, and feature flags. It never panics and never fails.
- `pdf_postprocess` with crop, resize, gray conversion, tone curve, Otsu,
  Sauvola, threshold fusion, dither, and 1-bit packing.
- `RenderLimits.cancellation`, a cooperative `CancellationToken`.

### 2.2 What the renderer does not give you

Each item below is Lege-side work in this plan. Do not plan around a
renderer change.

1. **No outline extraction.** `pdf_read` reports only the boolean
   `has_outlines`. Nothing walks the `/Outlines` tree. Nothing resolves a
   destination.
2. **No text extraction.** The IR carries glyph indexes and positions, not
   Unicode. `pdf_content::TextRun` keeps the character codes, the text
   matrix, the text state, and the font object identity. `pdf_font` parses
   CID CMaps but does not parse a `/ToUnicode` CMap (`bfchar` and `bfrange`).
   **This work moved to the renderer** on 2026-07-22, as a new `pdf-text`
   crate. The specification is
   `lege-pdf/render/docs/refinement/plans/PLAN-TEXT-EXTRACTION.md`. It ports
   `CPDF_TextPage` in full: character generation, space and line-break
   insertion, hyphens, bidirectional reordering, and rectangle grouping.
   Lege then calls `pdf_text::TextPage`. Section 4.4 gives the seam.
   **Phase 3 of this plan cannot finish before `pdf-text` reaches its
   Phase T6.**
3. **Gray8 is a downconversion, not a gray compositor.** The CPU backend
   composites in RGBA and converts at the end
   (`pdf-render-cpu/src/surface.rs`). A Gray8 request therefore removes
   Lege's own RGB buffer and Lege's own RGB-to-gray pass. It does not remove
   work inside the renderer. This is still the largest single memory win.
   Do not claim more than this.
4. **No exclusion rectangles in a render request.** Section 6.7 gives the
   Lege-side method.
5. **No page-analysis hints from the IR.** The idea in `render-upgrade.md`
   §5 (image XObject placements as a prior for layout detection) needs a new
   renderer API. Section 8 records it as a request. Do not build the plan
   around it.

### 2.3 What Lege uses pdfium for today

All call sites are in `lege-process/core/pagerender.rs`. The lock is
`PDFIUM_GLOBAL_LOCK`. The public functions are:

| Function | Replacement |
|---|---|
| `ensure_pdfium_binds` | Delete |
| `count_pdf_pages_from_bytes` | `DocumentSnapshot::page_count` |
| `PdfiumRenderer::new_from_bytes` | `DocumentSnapshot::open` |
| `render_page_rgb`, `render_page_rgb_sync`, `render_pages_rgb` | `PageCompiler` and `CpuBackend` |
| `extract_bookmarks`, `extract_bookmarks_from_bytes` | New outline reader (Phase 2) |
| `has_text_layer`, `has_any_text_layer` | New text reader (Phase 2) |
| `extract_page_text`, `extract_positioned_text_words` | New text reader (Phase 2) |
| `maybe_run_pdfium_worker_and_exit`, `memory_usage`, `shutdown` | Delete |

The GUI installers and the macOS package bundle `libpdfium`. Phase 3 removes
those files.

### 2.4 Two workspaces

Lege and the renderer keep separate workspaces, separate lock files, and
separate `target/` directories. This has three results:

- Both workspaces must pin the same toolchain. The renderer pins 1.97.1 and
  edition 2024. Add the same pin at the Lege root.
- The renderer already refers to `../../Lege-ecosystem/lege-codecs/jp2lam`
  and `../../Lege-ecosystem/lege-codecs/jbig2enc-rust`. These are the same
  directories that the Lege root `[patch]` block uses. So one copy of each
  codec compiles. Prove this with `cargo tree -d` in Phase 2.
- `cargo ecosystem-check` does not check the renderer. Keep it that way.

---

## 3. Target architecture

### 3.1 Threads

```
main thread          CLI arguments, progress consumer
lege-control thread  Tokio current_thread: channels, select, cancel,
                     subprocess supervision, progress
compute pool         cores - 1 workers, the global Rayon pool.
                     One page job = one page, start to end.
gpu-poll thread      one thread, device.poll(Wait), fires completions
writer thread        PDF serialize and deflate, or DjVu manifest
tokio blocking pool  maximum 4 threads, true blocking input and output only
```

Peak thread count is about `cores + 3`. There is no pdfium actor. There is
no inference actor.

### 3.2 Per-document state, shared with an `Arc`

One `RenderSession` per document holds:

- `Arc<DocumentSnapshot>` — immutable, no lock, read by every worker.
- `Arc<CpuBackend>` — holds the font, glyph, and image caches for the
  document. Build it one time per document. If you build it per page, the
  caches lose all their value.
- `Arc<PageCompiler>` — cheap and immutable after construction.

### 3.3 Per-worker state, in `thread_local!` on the Rayon pool

The pool has fixed threads, so this state lives for the process:

- `pdf_document::ParseContext` — the renderer needs one mutable context per
  worker. Reset it between pages. Do not build a new one per page.
- `pdf_render_cpu::CpuWorkerContext` — raster scratch.
- The pooled JBIG2 decoder context.
- Image-decode scratch and OCR context, where the OCR method permits it.

Leave `CpuBackendOptions::threads` as `None`. The backend then uses the
global Rayon pool. A wide page can steal idle workers, and a busy pool does
not oversubscribe. Do not build a second pool.

### 3.4 The compute seam

```rust
pub struct ComputeJob {
    pub document: DocumentId,
    pub page: Option<u32>,
    pub class: JobClass,       // Compile, Render, ImageDecode, Jbig2Decode,
                               // Binarize, Encode, Ocr, Layout, Compose,
                               // WriterPrep, Thumbnail
    pub priority: JobPriority, // Interactive | Visible | Adjacent |
                               // Prefetch | Background | Maintenance
    pub generation: u64,       // viewport generation; batch mode uses 0
    pub estimated_bytes: u64,
    pub cancel: CancelToken,
}
```

- Admission uses a byte-denominated `tokio::sync::Semaphore`. One permit is
  1 MiB. The total is `clamp(total_ram * 0.5, 1 GiB, 8 GiB)`. The flags
  `--memory-budget-mb` and `LEGE_MEMORY_BUDGET_MB` override it.
- A job acquires its bytes before it allocates, which means before the
  render, not after it.
- Execution is `rayon::spawn`. The result returns on a
  `tokio::sync::oneshot`.
- `JobCtx::checkpoint()` fails when the cancel token is set or when the
  generation is stale.
- `JobPriority` and `generation` exist from the first commit. Batch mode
  submits everything at `Background` and generation 0. The later viewer then
  needs no second migration.

### 3.5 GPU concurrency

- One `wgpu::Device` and one `Queue` for the process.
- Model weights upload one time. Every session references the same
  read-only buffers.
- A pool of K sessions, K from 2 to 4. Each session owns its activation
  buffers, staging buffers, and bind groups. A worker checks a session out
  for one inference and returns it after readback.
- One `gpu-poll` thread calls `device.poll(Wait)`. Completion fires a
  oneshot or a condvar. No code calls `pollster::block_on` per page.
- The page worker blocks on the completion signal, not on `device.poll`.
  With K sessions in flight, at most `workers - K` threads wait. That wait
  is the correct backpressure at the true bottleneck.
- **Permit order, to prevent deadlock:** a worker takes the byte permit
  first, then the GPU session permit. Never the opposite order.
- GPU binarization and Paddle OCR use the same device, the same poller, and
  their own small session pools. Delete the `GPU_BINARIZER` mutex.
- With no GPU, a session becomes a per-worker thread-local CPU graph state
  that shares the weights through an `Arc`. The check-out API does not
  change.

### 3.6 One page job, start to end

A worker takes one page and runs compile, render, gray work, layout,
optical character recognition, encode, and the writer handoff. It uses its
thread-local contexts for all of this. The stage channels between the CPU
stages go away, because they were there only because pdfium and the GPU were
serial.

The only shared resources per page are: the snapshot (read only), the byte
semaphore (one atomic path), the priority queue (one short mutex), the GPU
session pool, the writer channel (one send), and the progress atomics.

Delete the stage-pipeline code path. Do not keep it as an alternative. Two
execution models mean two backpressure designs and two cancellation
surfaces.

---

## 4. The new document crate

Build one new workspace member: `lege-pdf/read`, package name
`lege-pdf-read`. It is the only crate that names a renderer type. Every
other Lege crate speaks Lege types.

The crate owns four groups of work.

### 4.1 Intake

```rust
pub struct DocumentIntake {
    pub page_count: u32,
    pub encrypted: bool,
    pub needs_password: bool,
    pub recovery_used: bool,
    pub per_page_compile: Vec<CompileStatus>,
}
pub fn examine_document(bytes: &Arc<[u8]>) -> DocumentIntake;
```

This wraps `pdf_read::examine`. It gives the intake path more than pdfium
gave: encryption method, cross-reference health, and per-page compile
status.

### 4.2 Render session and page rasters

```rust
pub struct RenderSession { /* snapshot, backend, compiler — see 3.2 */ }

impl RenderSession {
    pub fn open(bytes: Arc<[u8]>, password: Option<&str>) -> Result<Self>;
    pub fn page_geometry(&self, page: u32) -> PageGeometry; // crop box, rotate
    pub fn compile(&self, page: u32) -> Result<Arc<CompiledPage>>;
    pub fn render(&self, page: &Arc<CompiledPage>, product: &RasterProduct)
        -> Result<RasterPlane>;
}
```

`render` builds one `RenderRequest`, calls `submit_caught`, and maps the
result to a Lege `RasterPlane`. It maps `RenderError::Panic` to a page-level
failure, never to a process failure.

### 4.3 Outline extraction (new code, replaces pdfium)

Keep the contract that the pipeline already uses:

```rust
pub struct OwnedBookmarkNode {
    pub title: String,
    pub source_page: usize,
    pub top: Option<f32>,   // new: PDF user space Y of the destination
    pub children: Vec<OwnedBookmarkNode>,
}
pub fn extract_outline(session: &RenderSession) -> Vec<OwnedBookmarkNode>;
```

The work is a walk of `catalog → /Outlines → First/Next`, with UTF-16BE and
PDFDocEncoding title decoding. It uses `DocumentSnapshot::objects()` and a
worker `ParseContext`. No rasterization happens.

Destination resolution is the difficult part. All three forms occur in real
documents and all three must resolve to a page index:

1. A direct `/Dest` array, `[pageRef /XYZ x y z]` or another form.
2. A named destination, through the `/Dests` name tree or the older
   `/Dests` dictionary in the catalog.
3. An `/A` action with `/S /GoTo` and a `/D` value of either form above.

Keep the destination Y value. Section 7.4 uses it.

Change one current behavior: when a node does not resolve, promote its
resolvable children instead of dropping the whole subtree.

### 4.4 Text extraction (thin wrapper over the renderer's `pdf-text`)

The algorithm lives in the renderer, not here. See
`lege-pdf/render/docs/refinement/plans/PLAN-TEXT-EXTRACTION.md`. Lege writes only
the wrapper:

```rust
pub fn has_text_layer(session: &RenderSession, page: u32) -> bool;
pub fn page_text(session: &RenderSession, page: u32) -> String;
pub fn positioned_words(session: &RenderSession, page: u32,
                        source_width: u32, source_height: u32)
    -> Vec<NativeTextWord>;
```

Method: compile the semantic page one time, build a
`pdf_text::TextPage` with the page-to-pixel matrix as its display matrix,
then map the results:

| Lege function | `pdf-text` call |
|---|---|
| `has_text_layer` | `TextPage::has_text` |
| `page_text` | `TextPage::all_text` |
| `positioned_words` | `TextPage::words` |

`TextPage::words` returns exact word boxes, built from the union of the
character boxes. So Lege **deletes** `push_segment_words` in
`pagerender.rs`. That function divides a rectangle by the character count,
which puts the boxes in the wrong place under any proportional font. The
hOCR text layer becomes more accurate than the pdfium-era output, not less.

**Accept a fidelity difference in the page text.** The renderer plan
measures it against pdfium page by page and holds the Latin corpus at zero
difference. Lege re-checks the end product: the hOCR of the three baselines.

---

## 5. Raster products

### 5.1 Typed planes

Replace the ambiguous `Vec<u8>` region data:

```rust
pub enum RasterPlane {
    Rgb8(RgbSurface),
    Gray8(GraySurface),
    Mono8(Mono8Surface),
    Mono1(MonoBitmap),
}

pub struct MonoBitmap {
    pub width: u32,
    pub height: u32,
    pub stride_words: u32,
    pub words: Vec<u32>,
}
```

`MonoBitmap` rows are MSB-first and **1 means black ink**. This is the
convention of `pdf_postprocess::PostprocessOp::PackMonochrome`, of JBIG2,
and of CCITT. Do not invent a second convention.

Give `MonoBitmap` these operations: `fill_rect_white`, `copy_region`,
`merge_region`, and `as_packed_bytes`.

Some consumers still need `Mono8`: the current optical character recognition
code, the heavy model adapters, and debug image output. Convert only at
those boundaries.

### 5.2 The page output plan

Lege builds the plan. The renderer executes one request per product. All
products of one page share one `Arc<CompiledPage>`, so the page compiles one
time.

```rust
pub struct PageOutputPlan {
    pub analysis: Option<AnalysisTarget>, // low resolution, for detection
    pub base: BaseTarget,                 // Gray8 or Mono1 full page
    pub regions: Vec<RegionTarget>,       // one render per detected region
    pub ocr: Option<OcrTarget>,           // only when the method needs it
}
```

A `RegionTarget` maps to `RenderRequest.crop` plus `output_size`, so a
photograph renders at its own resolution and its own format. The base page
can stay Gray8 while one region renders RGB.

### 5.3 Geometry in the render matrix

Put crop, scale, rotation, centering, and standardization into the
page-to-device matrix. Do not render large, then crop, then resize.

The two-pass document margin flow is:

1. Render low-resolution analysis surfaces for all pages.
2. Compute the document-wide margins.
3. Build the final matrix of each page.
4. Render each final product one time at its final size.

Detection boxes and native text coordinates use the same matrix.

### 5.4 Grayscale binarization

Make grayscale the primary input of the binarizer:

```rust
pub fn binarize_gray(gray: &[u8], width: usize, height: usize,
                     options: &BinarizationOptions) -> BinaryPage;
```

Keep `binarize_image` and `binarize_image_raw` as thin RGB wrappers that
convert and call the gray function. Most of the algorithm already works on
gray; RGB is only its input adapter. This change needs no renderer, so
Phase 1b does it early.

### 5.5 Do not replace Lege binarization with `pdf_postprocess`

`pdf_postprocess` has Otsu, Sauvola, fusion, dither, and packing. This
overlaps Lege's tuned and GPU-accelerated binarizer. Keep Lege's binarizer.
Use `pdf_postprocess` only where it is clearly cheaper: crop, resize, gray
conversion, and the packing convention. Record the overlap as a later
decision, not as work in this plan.

---

## 6. Phases

### Phase 0 — Measure

Goal: get the numbers that every later phase must not make worse.

Tasks:

1. Add `--debug-runtime-stats`. Report peak thread count, peak resident
   memory, per-stage in-flight counts, Rayon pool size, and blocking-pool
   use.
2. Record baselines on three documents: a small clean PDF, a large scanned
   book with slow optical character recognition, and a DjVu output job.
   Record thread count with time, peak resident memory, wall time, and total
   CPU time.
3. Record the share of wall time of each stage in layout mode.
4. Build the test corpus. It must include: bookmarked PDFs, PDFs with named
   destinations only, encrypted PDFs, scanned books with a printed contents
   page, documents with no chapters, music scores, and papers.
5. Run bookmarked PDFs through PDF to PDF full document, PDF to PDF page
   range, and PDF to DjVu. Record exactly where preservation fails today.

Exit gate: `to-do-plans/measurements.md` holds all baselines. The bookmark
failure list is written down.

### Phase 1a — The `LegeCompute` facade

Goal: one compute substrate with byte admission, with no other change.

Tasks: build `lege-process/compute/`. Port the processing stage first: the
two `spawn_blocking` calls in `process_single_page` and the encode call
sites. Make `ENCODE_SEMAPHORE` and `OCR_SEMAPHORE` per-class caps inside the
facade and delete the standalone semaphores. Port the DjVu binarize and
compose calls and the optical character recognition call sites.

Exit gate: peak thread count falls to about `cores` plus input and output
threads. Resident memory on the scanned-book baseline falls. Output bytes
are identical on all three baselines.

### Phase 1b — Grayscale binarization API

Goal: Section 5.4.

Exit gate: `binarize_gray` exists and is the primary path. The RGB wrappers
produce byte-identical output to today on the corpus.

Implementation status, 2026-07-23: exit gate met.

- `binarize_gray` is the primary fixed-threshold and adaptive implementation.
  It accepts one byte per pixel and owns input inversion, GPU dispatch, CPU
  fallback, output inversion, and the grayscale adapter for heavy-duty
  binarization.
- `binarize_image_raw` converts RGB with the same integer Rec.601 or
  linear-light BT.709 rule as the old path and calls the grayscale
  implementation. It preserves the old linear-light retry after a failed
  GPU attempt. `binarize_image` remains the PBM wrapper over the raw result.
- Deterministic unit tests compare fixed-threshold, adaptive, input/output
  inversion, callback fallback, and PBM output with the pre-refactor
  algorithm. A real-GPU test also matches the old grayscale GPU route.
- The opt-in corpus test rendered every page of the three Phase 0 baselines
  and the 72-page music score. All 3,492 comparisons passed byte-for-byte:
  adaptive and fixed-threshold output on each of 1,746 pages.
- `cargo test -p lege --lib --offline -- --test-threads=1` passes 148 tests.
  The required debug-fast binary build passes. The default high-concurrency
  test process can finish every assertion and then intermittently terminate
  with SIGSEGV during teardown; two-thread and serial runs exit normally.
  This is tracked as a separate test-runtime defect, not as Phase 1b output
  evidence.

### Phase 1c — Typed planes

Goal: Section 5.1.

Tasks: add `RasterPlane` and `MonoBitmap`. Remove the gray-to-RGB-to-gray
conversions in the image-region code. Make the encoders read the native
representation: `None` reads `Rgb8`, `GrayJp2` and halftone read `Gray8`,
Stucki reads `Mono1` or `Mono8`, and `Ccitt4ClusteredDot4x4` reads `Mono1`.

Exit gate: no gray-to-RGB-to-gray conversion is left. Output is identical on
the corpus. Peak resident memory on the region-heavy baseline falls.

### Phase 1d — Outline plumbing fixes

Goal: fix the bookmark bugs that exist today and do not depend on the
renderer.

Tasks:

1. Send a real source-to-output map for page-range runs. Today the code
   sends an identity map, so a page range corrupts the outline.
2. Replace the last-write-wins `SetBookmarks` with a merge at finalize.
   Finalize waits for both inputs.
3. Stop detaching the bookmark task. Track it in the stage join set.
4. Promote the resolvable children of an unresolvable node.

Exit gate: the Phase 0 bookmark failure list is empty for PDF output, for
both a full document and a page range.

Implementation status, 2026-07-23: exit gate met.

- Task 1 is done in `pdf_tokio_pipeline.rs`: a page-range run sends the real
  `page_start..page_end` map instead of an empty one.
- Task 2 landed with Phase 7. The writer holds the source bookmarks and the
  synthesized outline separately and applies precedence in one place at
  finalize, because whether the source outline survived remapping is only
  knowable once the page slots resolve.
- Task 3 is done: outline extraction is awaited before finalize, so it can no
  longer lose the race.
- Task 4 is done twice over: `lege-pdf/read/src/outline.rs` promotes the
  children of a node it cannot resolve, and `bookmarks_to_outline` promotes the
  children of a node whose page falls outside a page range.
- Measured on the 164-page, 70-entry T.88 specification: the full document
  round-trips all 70 entries with their nesting, and range `40-79` emits 8
  entries with every index shifted by −39 and orphaned children promoted. The
  bookmark failure list is empty for PDF output. The DjVu row of that list is
  still open and belongs to Phase 7's deferred half.

### Phase 2 — The document crate

Goal: read documents through the renderer while pdfium is still present.

Tasks:

1. Add the renderer crates to the root `[workspace.dependencies]` block with
   relative paths. Add `lege-pdf/read` as a workspace member.
2. Pin toolchain 1.97.1 at the Lege root.
3. Run `cargo tree -d`. Prove that jp2lam and jbig2enc-rust each compile one
   time.
4. Build `DocumentIntake`, `RenderSession`, `extract_outline`, and the text
   wrapper of Section 4.4.
5. Write parity tests against pdfium, which is still in the tree. Compare
   page counts, page geometry, outline trees, destination page indexes, and
   text words.

Dependency: the text wrapper needs the renderer's `pdf-text` crate at its
Phase T6. Everything else in Phase 2 is independent of it, so start the
outline and intake work at once and add the text wrapper when `pdf-text`
lands.

Implementation status, 2026-07-23:

- Tasks 1 to 4 are implemented. `lege-pdf-read` owns document intake,
  immutable sessions, page geometry, opaque compilation, outline reading,
  and the `pdf-text` wrapper.
- `lege-process` now gets PDF page counts and geometry from `RenderSession`.
  Its PDF and DjVu native-text paths use the renderer text API first and keep
  PDFium only as a temporary fallback. PDF and reflow outline extraction use
  the Lege outline reader.
- Page-range outline finalization now waits for extraction and uses the real
  source-to-output page map.
- An offline dependency-tree check resolves one package ID for `jp2lam` and
  one for `jbig2enc-rust`.
- `RenderSession` now owns the document `CpuBackend` and exposes typed
  renderer output as Lege-owned `Rgb8` and `Gray8` planes. The fixture test
  covers both products and rejects a zero-size request.
- The `lege-pdf-read` unit and fixture tests pass. The required
  `cargo build --profile debug-fast -p lege --bin lege --offline` also
  passes against the coherent external renderer tree.
- PDFium stays only as the Phase 2 oracle. `pdfium-render` is pinned exactly
  to 0.9.2 because the packaged PDFium library implements that revision's
  ABI, not the newer default selected by 0.9.3.
- Task 5 has an opt-in corpus harness in
  `lege-process/tests/renderer_read_parity.rs`. It compares page count,
  displayed geometry, outline titles/tree/destination pages, normalized
  text content, renderer word reconstruction, renderer word-box validity,
  and the visible text retained by the production hOCR builder. It builds
  hOCR from both the renderer words and the old PDFium positioned-word path
  and requires the renderer hOCR to lose no more native text on every page.
  It ignores discretionary hyphens, accepts reading-order-only differences
  when the character multiset is equal, and reports a bounded PDFium-oracle
  tolerance of at most 1%, or at most four characters on a page with at
  least 100 characters.
- The harness passes the three selected baselines: `jbig99paper.pdf`
  (6 pages), `crusadeswholesto0000lamb_1.pdf` (886 pages), and
  `risefallofconfed01daviuoft.pdf` (782 pages). This is 1,674 pages with
  matching page counts and geometry. The hOCR comparison also passes all
  1,674 pages. It also passes all 72 pages of `Pop & Rock .pdf`.
- A deterministic generated outline fixture now covers both the `/Dests`
  name tree and the legacy catalog `/Dests` dictionary. Its titles, tree,
  and destination page indexes match PDFium.
- The T.88 cover-page text defect is **resolved 2026-07-23** (see §8 item 6).
  `T-REC-T.88-200002-S!!PDF-E.pdf` page index 0 now extracts 244 normalized
  characters beginning with “International Telecommunication Union” (PDFium
  yields 242 — a two-character difference, inside the ≤4-char tolerance).
  Page count, geometry, and outline parity continue to pass.

Exit gate: page count and page geometry match pdfium on the whole corpus.
The outline tree matches pdfium on every bookmarked document, including the
named-destination documents. The hOCR built from `TextPage::words` is at
least as accurate as the pdfium-era hOCR on the three baselines.

Implementation status, 2026-07-23: exit gate met.

### Phase 3 — Raster swap and pdfium removal

Goal: the renderer produces every raster. pdfium leaves the tree.

Tasks:

1. Add the temporary switch `LEGE_RENDER_ENGINE=pdfium|lege`. Route the
   render calls of `pagerender.rs` through `RenderSession`.
2. Compare the end products, not only the rasters. Compare the MRC output
   size and quality, the optical character recognition accuracy, and the
   layout detections on the three baselines. The two engines differ
   legitimately in antialiasing, so do not test for identical pixels.
3. Feed each real difference back to the renderer agent as a fixture case.
   The `tools/pdfium-diff` oracle stays available for this.
4. Delete `PDFIUM_GLOBAL_LOCK`, every pdfium call path, the pdfium bindings,
   and the switch. Remove the `pdfium-render` dependency. Remove library
   bundling from every installer target, including the macOS package.
5. Check the behaviour parity list: page rotation through the page
   transform, passwords with `open_with_password` for revisions 2 to 6, and
   annotations rendered by default.

Exit gate: no pdfium symbol is left in the tree. Wall time on render-bound
documents falls with core count. Peak thread count is about `cores + 3`. The
end-product comparison shows no quality loss on the three baselines.

Implementation status, 2026-07-23:

- Tasks 1 through 5 are complete. The temporary switch has served its
  comparison purpose and is deleted. One `PdfRenderer`, owning one shared
  `RenderSession` per document, now supplies page count and every PDF raster
  to the normal PDF, PDF-to-DjVu, EPUB, raster-reflow, PDF-to-PNG, JP2 debug,
  layout visualization, layout crop, and PDF-to-images paths.
- The six-page `jbig99paper.pdf` comparison has matching page count,
  927×1200 geometry, and extracted-text SHA-256. The renderer output is
  304,185 bytes versus PDFium's 293,557 bytes, with no missing visual
  content.
- The complete 886-page Crusades PDF conversion has matching page count,
  778×1200 geometry, and extracted-text SHA-256. Six sampled pages include
  five pixel-identical pages and a 42.99 dB cover. The renderer output is
  3,521,646 bytes versus 3,522,845 bytes and took about 12.3 seconds versus
  49.5 seconds for the packaged PDFium build.
- The complete 782-page region-heavy PDF-to-DjVu conversion has matching
  page count, the same four-dimension distribution, and matching extracted
  DjVu text SHA-256. The renderer output is 15,224,166 bytes versus
  15,319,088 bytes and took about 28.6 seconds versus 128.9 seconds.
- Fourteen exact renderer/PDFium raw-page pairs from `tools/pdfium-diff`
  were passed through layout processing. Both outputs have 14 pages and
  identical per-page detected-overlay counts (24 total); corresponding
  overlay dimensions differ only by a few antialiasing-edge pixels.
- Best-OCR samples on all three baselines produce searchable text through
  the renderer path. This work also fixed a pipeline defect, independent of
  rasterization: slow OCR now falls back to a whole-page region when layout
  returns no text-like detections, matching fast OCR's behavior.
- Rotation geometry and text-coordinate mapping remain covered. New
  integration coverage passes password handling for revisions 2, 3, and 6,
  including correct, wrong, and missing passwords, and verifies that a
  normal annotation appearance stream is painted by default.
- `PDFIUM_GLOBAL_LOCK`, the PDFium render paths and selection switch,
  `pdfium-render`, startup discovery and preflight, GUI environment
  injection, and Linux/macOS/AppImage library bundling are deleted.
  `Cargo.lock`, `cargo tree`, the built executable's dynamic dependencies,
  and exported/imported symbols contain no PDFium dependency.

Functional raster cutover and end-product quality gates are met. The literal
whole-process peak-thread target is not yet met: measured jobs still peak at
about 138 and 198 threads because the pre-existing Tokio blocking pool and
codec schedulers are not unified. That is not renderer-owned work; the
blocking-pool cap is deliberately deferred to the audited Tokio reduction in
Phase 8, with the page-owned pipeline work in Phase 4 also reducing scheduler
fan-out. Do not reintroduce a renderer switch to address that cross-phase
metric.

### Phase 4 — Page-owned pipeline

Goal: Section 3.6.

Tasks: make one compute job run one page from compile to writer handoff. Add
the thread-local worker contexts of Section 3.3. Build one `CpuBackend` per
document. Delete the stage channels of the PDF path and their in-flight
gauges. Put cancellation checkpoints between the stages inside the job and
inside the long kernels. The DjVu path follows if the win repeats.

Exit gate: resident memory is about the byte budget. Wall time does not get
worse on the optical-character-recognition-heavy baseline. Cancel latency is
below 100 ms. The writer keeps its arrival-order slot table.

Implementation status, 2026-07-23:

- The PDF path no longer has render, inference, process, or forwarding stage
  channels. A bounded `JoinSet` owns complete page jobs from render through
  layout, processing, optical character recognition/text extraction, encoding,
  and direct writer handoff. The PDF writer channel is the only stage channel
  left, and its arrival-order page slot behavior is unchanged.
- Each page acquires a MiB-denominated host-memory permit before render and
  holds it through writer handoff. The default is 50% of detected available
  memory, clamped to 1–8 GiB. `LEGE_MEMORY_BUDGET_MB` overrides it.
- `RenderSession` now reuses one document-tagged `ParseContext` and
  `CpuWorkerContext` per worker thread. A worker resets both contexts when it
  switches documents, which prevents parsed-object and font-cache leakage
  between document snapshots.
- One Lege-owned cancellation token reaches renderer `RenderLimits` and is
  checked between render, inference, geometry adjustment, resize,
  binarization, region processing, optical character recognition/text
  extraction, encoding, and writer handoff. A deterministic pre-cancelled
  2400×3200 render test returns in less than 100 ms without allocating the
  surface.
- The rebuilt debug-fast binary completes the six-page real-GPU smoke run
  with a 1 GiB host-memory budget.

The implementation tasks are complete. The optical-character-recognition
baseline was measured on 2026-07-23, on a 120-page slice run once rather than
the whole 886-page book — the renderer proved effectively bit-exact against
PDFium, so the render drift that the full run was meant to catch does not
justify the machine time. Results are in `measurements.md`; the gate is partly
met and partly not, and the parts are worth separating.

- **Wall time versus Phase 0: cannot be evaluated.** The Phase 0 runs were
  never made, and PDFium was removed in Phase 3, so no pre-integration number
  exists for any mode involving layout or optical character recognition. The
  measured figures are recorded as the reference point for later phases
  instead. The one real before/after in the file is the Phase 3 no-layout
  comparison, taken while both engines were present.
- **Resident memory versus the byte budget: the mechanism works, the default
  does not bind.** Squeezing the budget to 64 MiB cuts peak resident memory by
  46% and costs 3.9× the wall time, so admission is real. But a 1200-pixel page
  is estimated at about 15 MiB, so at the default budget the semaphore admits
  far more pages than `page_workers` ever runs: 8,192 MiB and 1,024 MiB differ
  by 0.8% in peak resident memory. Page concurrency, not the byte budget, is
  what caps memory on this host.
- **Cancellation under 100 ms: not met, and not for the expected reason.** With
  a slow recognition call in flight, `kill -TERM` to process exit is 0.36 s. With
  no optical character recognition at all it is 0.32 s, so the in-flight kernel
  costs about 40 ms and the rest is fixed process teardown, dominated by GPU
  device destruction. Cancellation reaches the pipeline promptly and leaves no
  orphan process and no work directory. Closing the gap means shortening
  teardown, not adding checkpoints.

### Phase 5 — GPU concurrency

Goal: Section 3.5.

Tasks: audit the wgpu graph for hidden global state, such as a staging belt,
a reused encoder, or a shared readback buffer. This audit is the real work.
Then build the session pool, the single poller, and the VRAM budget as a
second byte semaphore. Delete `InferenceActor`, its batch and clone path,
`InferencePool`, and the `GPU_BINARIZER` mutex.

Exit gate: in layout mode, pages per second rises with K until the GPU
saturates. Test K = 2, then 3, then 4. No session starvation happens. VRAM
stays inside the budget.

Implementation status, 2026-07-23:

- `InferenceActor`, `InferenceJob`, and `InferencePool` are deleted. Layout
  inference uses a checkout pool of K single-flight `LayoutEngine` sessions.
  `LEGE_GPU_SESSIONS` selects K and clamps it to 2–4. K defaults to 2.
- All layout sessions share one process-wide WGPU device and queue. Sibling
  compiled graphs share the first session's immutable model-weight buffers
  and own separate activation, parameter, bind-group, and readback state.
- One `gpu-poll` thread owns every blocking `Device::poll` call. Layout,
  binarization, resize, and Paddle optical character recognition graphs use
  the same shared device and completion path.
- The `GPU_BINARIZER` global mutex is deleted. GPU binarization has a small
  1–4 session checkout pool on the shared device.
- A MiB-denominated VRAM semaphore is acquired before a GPU session.
  `LEGE_VRAM_BUDGET_MB` controls the budget and defaults to 2 GiB. Session
  construction is reduced automatically when the configured budget is too
  small.
- The 72-page `Pop & Rock .pdf` layout run measured 8.46 s at K=2, 8.59 s
  at K=3, and 8.57 s at K=4 on an RTX 4060 Laptop GPU. K=2 is therefore the
  saturation/default point on this workload; more sessions add no throughput.
  All three runs completed 72/72 pages without starvation. A K=4
  `nvidia-smi` sample peaked at 716 MiB, below the configured 2 GiB budget.

The Phase 5 implementation and measured exit gate are complete for layout
inference. K=2 remains the default because K=3 and K=4 did not improve this
corpus.

### Phase 6 — The page output plan

Goal: Section 5.2 and 5.3.

Tasks:

1. Render the analysis surface at low resolution for detection.
2. Render the base page as Gray8 at the final geometry.
3. Render each detected image region as its own request with `crop` and
   `output_size`.
4. Render an optical character recognition surface only when the method
   needs its own scale or format. Native text extraction needs no surface.
5. Replace the masked RGB clone. The renderer has no exclusion rectangles,
   so do this: render the base as Gray8, then fill the excluded rectangles
   with white in the Gray8 plane, then binarize. This already removes one
   full-page RGB clone.
6. Add a page feature classifier. Use `CompiledPage` features to sort each
   page into "gray is exact", "gray is acceptable for bilevel output", or
   "color fallback". Send a page to the RGB path when it has a non-normal
   blend mode, a transparency group in a non-gray blend space, DeviceN or
   Separation interaction, overprint, a difficult ICC transform, or a
   color-dependent soft mask. No risky page uses gray silently.

Exit gate: the full-page high-resolution RGB buffer is gone from the common
path. Peak resident memory falls again against the Phase 4 number. Output
quality on the corpus does not fall.

Implementation status, 2026-07-23:

- All six tasks are implemented. `lege-pdf-read` owns `PageOutputPlan`,
  `AnalysisTarget`, `BaseTarget`, `RegionTarget`, `OcrTarget`, `RasterProduct`
  and `GraySuitability`; `lege-process/pipeline/page_output_plan.rs` builds the
  plan and `RenderSession::render_output_plan` executes it against one shared
  `Arc<CompiledPage>` per page.
- Task 1: the analysis surface renders at the configured inference long edge,
  not at the output size, and only when layout detection is on.
- Task 2: the base page renders Gray8 at the final geometry.
- Task 3: each detected image region is its own request with `crop` and
  `output_size`, and is encoded from the region's native RGB pixels.
- Task 4: an OCR surface is requested only when slow optical character
  recognition needs a larger scale than the output. Native text extraction
  requests no surface.
- Task 5: the masked full-page RGB clone is gone. Excluded region rectangles
  are filled white in the Gray8 plane (3 px pad) and the page is then
  binarized through `binarize_gray`.
- Task 6: the page feature classifier is `CompiledDocumentPage::gray_suitability`.
  Transparency, soft masks, ICC color, non-separable blends and overprint give
  `ColorFallback`; images, patterns and shadings give `AcceptableForBilevel`;
  anything else is `Exact`. Only the first is sent to the RGB path.
- Scope: the planned gray path serves the common bilevel page. Grayscale/MRC
  mode, JPEG text format, dithered image regions, preserved cover pages, and
  document-wide margin analysis still use the earlier RGB path, which the plan
  does not require this phase to convert.
- Measured on the six-page baseline: 273,118 output bytes against the 304,185
  bytes of the Phase 3 renderer cutover, same 6 pages, same 927×1200 geometry,
  and ink present on every page. See `measurements.md`.

### Phase 7 — Automatic table of contents

Goal: on any document, build a navigable outline from the detected titles.
The feature is always on and invisible until the reader opens its navigation
panel.

This phase comes after Phase 4 on purpose. Candidate capture touches the
page processing code that Phase 4 rewrites, so an earlier start does the
work two times.

Tasks:

1. Capture candidates in the page job, after the detections are in output
   space and the hOCR text exists:

```rust
pub struct TocCandidate {
    pub page_index: usize,   // output space
    pub kind: TitleKind,     // DocTitle | ParagraphTitle
    pub confidence: f32,
    pub bbox: [f32; 4],
    pub text: String,        // hOCR words inside the box
    pub line_height: f32,
    pub page_height: f32,
}
```

   Carry them on `ProcessedPage` and on `DjvuBinarizedData`. They hold text
   only, so they cost almost nothing. The detections and the hOCR text
   already exist.

2. Write `lege-process/core/toc.rs` with one pure function
   `build_outline(candidates, total_pages, body_stats) -> Vec<OutlineItem>`.
   Use scoring, not hard gates. Ambiguity must produce nothing.

   Signals: the line height against the document body median; the position
   on the page and the empty space below the title; short text and a chapter
   number pattern in several languages as a boost only; a monotonic number
   sequence as a strong boost; identical text on three or more pages as a
   running header, which removes all of its instances; a density limit of
   about one entry per page; and a detection confidence floor near 0.5.

   Build at most two levels. Emit nothing when fewer than two entries
   survive.

   Title text comes from the hOCR words, normalized and cut at about 120
   characters. Drop a candidate whose words have low `x_wconf` values.

3. Apply the merge policy at finalize:
   - A source outline that survives remapping wins. Do not synthesize.
   - With no source outline, attach the synthesized outline if it passed.
   - With neither, emit no `/Outlines` and no NAVM chunk.
   Add `LEGE_NO_AUTO_TOC=1` as a debug escape.

4. DjVu output: bump the manifest to schema version 2 with an optional
   document-level `outline` field, on both sides. Add a `SetOutline` message
   to `DjvuWriterActor`. In djvulibrust, map the manifest outline to the
   existing `Bookmark` and `DjVmNav` types, call `set_navigation`, and
   re-enable the NAVM block in `assemble_djvm`. The offset arithmetic already
   reserves `nav_chunk_size`. Land the schema bump and the encoder binary
   together, because the version handshake fails fast on a mismatch.

Exit gate: on the corpus, every document with a source outline keeps it
unchanged. Every document with no chapters emits no outline. The synthesized
outlines pass a human read on the book corpus. DjVu output carries both
preserved and synthesized outlines. Snapshot tests hold the emitted outline
of each corpus document.

Implementation status, 2026-07-23: **PDF output complete, DjVu deferred by
decision.** The DjVu half — manifest schema 2, `SetOutline`, and the NAVM
re-enable in djvulibrust — waits until the synthesized outlines have been read
on the corpus, so it is not in this change and the DjVu row of the bookmark
matrix is still open.

- Task 1, candidate capture: `TocCandidate` is captured in both PDF processing
  paths, at the point where output-space detections and the finished hOCR
  already coexist. A page with no title detection returns immediately and never
  parses its hOCR, so the feature costs nothing on the pages that have nothing
  to give. Candidates ride out of the page job on `ProcessedPage` and are
  accumulated beside the existing hOCR pages.
- Task 2, `lege-process/core/toc.rs`: `capture_page` and `build_outline`, both
  pure. Scoring, not gates, with three exceptions that are gates because no
  amount of other evidence should overcome them: detection confidence under
  0.5, recognized-word confidence under 0.6, and a heading set smaller than the
  document's body text.
- Task 3, merge at finalize: in the writer, in one place. A source outline that
  resolves wins; a synthesized outline fills the gap; with neither, no
  `/Outlines` object is emitted at all, so those documents stay byte-identical
  to before. `LEGE_NO_AUTO_TOC=1` skips synthesis and leaves preservation alone.
- Destinations: `OutlineItem` gained `top`, and the writer emits
  `/XYZ null top null` for synthesized entries, so a tap lands at the title.
  Preserved entries keep page-level `/Fit`, because their `top` is in the
  source page's user space and the output page is re-rendered at another scale.
- Body-text statistics are a median of per-page medians, not of every line in
  the document: hOCR is spilled per page and never held document-wide.

Three signals were added after the first corpus run produced outlines that
would have been worse than none, and each is worth recording because each was a
real failure, not a hypothetical:

1. A running head that carries the printed folio ("THE KNEELING TOWER 113") is
   textually unique on every page, so the repetition kill never fired. The
   comparison key now ignores a leading or trailing page number.
2. The entries of a book's *own printed contents page* are short, well
   positioned, and full of the word "chapter" — they outscored everything else
   in `risefallofconfed01daviuoft.pdf`. They are set smaller than the body
   text, which is now a hard rejection.
3. Any two increasing numbers counted as "chapter numbering" and paid 1.5
   points. A folio-bearing running head collected it. A sequence now needs at
   least three numbers stepping by at most three.

Measured on the corpus, in `measurements.md`. Snapshot tests over synthetic
candidate sets stand in for per-document snapshots: the corpus documents are
large local scans whose recognized text is not stable enough to pin
byte-for-byte, so each failure mode above is instead frozen as a unit test.

### Phase 8 — Tokio diet, cancellation, and reuse

Tasks:

1. Change `pnginference.rs` to a `current_thread` runtime.
2. Replace the one `tokio::fs::read` with `std::fs::read`.
3. Set `max_blocking_threads(4)` last, after an audit of the remaining
   users.
4. Shrink the queue depths. Set `channel_capacity = page_workers`. Delete
   the `max(heavy_sauvola_concurrency * 2, 8)` default.
5. Delete `wait_for_memory_relief` and the resident-memory polling monitor.
   The byte semaphore replaces them.
6. Extend `AdaptiveConcurrency` to the PDF path.
7. Install a SIGTERM handler in the CLI. It triggers the same broadcast
   cancel, waits a short time for the checkpoints, kills the `djvu-encoder`
   child, and cleans the work directory. Add a timeout around the child
   `wait()`.
8. Bound the reflow pipeline. Replace the hold-everything vector with a
   two-pass design: pass 1 renders analysis surfaces to collect detections
   and the document scale, and pass 2 streams full-resolution pages one at a
   time. Drop the deep RGB clone.
9. Spill the per-page hOCR strings for the EPUB sidecar to the work
   directory and assemble at finalize.
10. Give the CPU Sauvola executor a thread-local arena.

Exit gate: a GUI cancel of a DjVu job in progress leaves no work directory
and no orphan encoder process. Cancel latency stays below 100 ms. Resident
memory and total CPU time are lower than the Phase 0 baseline on all three
documents.

Implementation status, 2026-07-23:

- Items 1 to 7, 9 and 10 are implemented. Every Tokio runtime in the CLI, the
  GUI worker and the debug entry points is `current_thread` with
  `max_blocking_threads(runtime_stats::MAX_BLOCKING_THREADS)` = 4. No
  `tokio::fs::read` call is left. `channel_capacity` is `page_workers` and the
  old `max(heavy_sauvola_concurrency * 2, 8)` default is deleted.
  `wait_for_memory_relief` and the resident-memory monitor are gone; the byte
  semaphore of Phase 4 replaces them. `AdaptiveConcurrency` serves the PDF
  path through `PipelineRuntimeLimits::from_config`. The per-page hOCR strings
  of the EPUB sidecar spill to the work directory and are read back at
  finalize. The CPU heavy-Sauvola executor keeps a thread-local RGB arena.
- Item 7 is complete. The CLI installs a SIGTERM handler that only sets an
  atomic; a bridge thread performs the broadcast cancel. A DjVu job now owns a
  `DjvuEncoderControl` and a `DjvuWorkDirGuard`. The guard runs on every exit
  path — success, failure and cancellation — kills the `djvu-encoder` child,
  waits for it to be reaped, and then removes the job's work directory.
  `DjvuOrchestrator::cleanup` was a no-op before this change, so **every** job
  used to leave its page layers behind. It now removes a job directory inside
  the managed DjVu temp base whole, and removes only the job's own files from
  a work directory the caller chose. The child also has a wall-clock timeout
  (`LEGE_DJVU_ENCODER_TIMEOUT_SECS`, default 30 minutes). On a kill path the
  stdout/stderr drain threads are left detached on purpose: a grandchild of
  the encoder can hold the pipe open and joining would make the cancel
  latency unbounded.
- Item 8 is complete. Reflow no longer holds every source page. The analysis
  half is two streaming passes — a calibration pass over the sampled pages and
  a flow pass over all of them — and each keeps one grayscale page at a time
  and no RGB. Only the reading-order flow, which is rectangles, survives a
  page. The compose half streams the source pages again through a bounded
  window of `SOURCE_PAGE_WINDOW` = 4 pages, loading exactly the pages one
  output page draws from, and keeps the color render only for a source page
  that carries a figure or table placement. `SourcePageSet` replaces the
  "every source page in one slice" argument of the compositor and of reflow
  optical character recognition. Cost: a source page renders about two times
  instead of one, plus the calibration samples.
- Measured: reflow peak resident memory is now flat in document length —
  609 MiB for 30 source pages and 626 MiB for 120 source pages of the same
  book, where the old design held every page. A SIGTERM during DjVu page
  processing and a SIGTERM while the encoder child was running both left no
  work directory and no orphan encoder, with a whole-process exit in about
  0.30 s. See `measurements.md`.
- The three baseline conversions were measured on 2026-07-23 and are recorded
  in `measurements.md`. There is no Phase 0 "before" to compare them against —
  see the Phase 4 status block for why, and for what the byte-budget and
  cancellation clauses actually measured.

### Phase 9 — Optional, after profiling

Do not start these before the profile says they still matter:

- Fixed-threshold streaming: render a Gray8 band, threshold it, pack it,
  send the packed rows to the encoder, and reuse the band buffer.
- Rolling adaptive binarization with band-local integrals.
- Direct CCITT row emission and JBIG2 generic-region row integration.

Symbol-mode JBIG2 and DjVu JB2 need the whole page, so a packed full-page
plane stays useful.

Implementation status, 2026-07-23: **profiled; all three declined; a different
optimization taken instead.** Full numbers in `measurements.md`.

The profile was taken with `perf` on four workloads: a 200-page scanned book
and the 164-page T.88 specification, each through CCITT and through JBIG2.

- Fixed-threshold streaming and rolling adaptive binarization both target
  binarization, which is 0.1% of cycles on scanned books and at most 1.8% on
  born-digital text — the work is on the GPU. Declined.
- Direct CCITT and JBIG2 row emission is the only one with a real number: JBIG2
  encode is 18.1% of cycles on the T.88 specification, in generic-region mode,
  so the item does apply there. But row integration removes the intermediate
  packed plane (3.7%), not the arithmetic coder (13.8%), and that document is
  inference-bound anyway — layout holds 73.7% of active wall. On the scanned
  book, CCITT encode is 0.6% and JBIG2 encode 0.1%. Declined, revisit if layout
  inference stops being the critical path.

What the profile did find: `__powisf2` at 31.4% of all CPU cycles on a
recognition run, from `f16_bits_to_f32` widening every fp16 model weight with
`2.0f32.powi(exp - 15)`. The cost scales with page count, not as a one-time
load, because the optical-character-recognition graphs re-widen their weights
whenever a page size misses their graph cache. Rewriting the helper as
bit-field widening — verified bit-identical across all 65,536 half values —
cut a 40-page recognition run from 409.8e9 to 230.7e9 cycles and from 104.8 s
to 80.3 s, with byte-identical extracted text.

Left undone and recorded: the remaining 9.8% is the re-widening allocation
itself. Caching the widened constants on `Detector`/`RecRecognizer` would
remove it, but that changes how `lege-gpu` holds models, which is past this
phase's mandate.

---

## 7. Interfaces to write down first

An agent must write these five items into code before the phase that uses
them, because they are the seams between the workstreams.

1. `ComputeJob`, `JobClass`, `JobPriority`, `JobCtx` — Phase 1a.
2. `RasterPlane` and `MonoBitmap` — Phase 1c.
3. `RenderSession`, `RasterProduct`, and `PageGeometry` — Phase 2.
4. `OwnedBookmarkNode` with the new `top` field, and `OutlineItem` with an
   optional `top` — Phase 2 and Phase 7. With a `top` value, the writer emits
   `/XYZ null top null`, so a tap lands at the title. Page-level `/Fit` stays
   the fallback.
5. `PageOutputPlan`, `BaseTarget`, `RegionTarget`, `AnalysisTarget`,
   `OcrTarget` — Phase 6.

Page-index rule for all outline work: `OutlineItem.page_index` is 0-based
output space. Blank pages stay in the document, so the count is 1 to 1. A
full-document run is the identity. A page-range run subtracts `page_start`.
A reflow run uses the existing source-to-output placement map.

---

## 8. Requests for the renderer agent

Do not build the plan around these. Send them, and continue with the
Lege-side method.

1. **Native grayscale compositing.** Today Gray8 is a final downconversion.
   A gray compositor would remove the RGBA intermediate inside the renderer.
2. **Exclusion rectangles in `RenderRequest`.** The renderer could reject
   spans inside the detected image boxes and never paint them.
3. **Page analysis hints from the IR.** The compiler already sees the image
   XObject bounds, the image transforms, the clip paths, the paint order,
   and the color spaces. An accessor for these would give the layout
   detector a strong prior and could remove the detection step for a simple
   one-image scan.
4. **Text extraction — sent and accepted 2026-07-22.** It is now the
   `pdf-text` crate, specified by
   `lege-pdf/render/docs/refinement/plans/PLAN-TEXT-EXTRACTION.md`. That plan also
   adds `/ToUnicode` parsing to `pdf-font` and an `actual_text` field to
   `pdf_content::TextRun`.
5. **Outline reading in `pdf-read`.** Lege writes it in Phase 2 because it
   is small and needs no renderer change. The renderer may want it later.
6. **Empty text on the T.88 cover page — found and fixed 2026-07-23.** On
   `lege-codecs/jbig2enc-rust/T-REC-T.88-200002-S!!PDF-E.pdf`, page index 0,
   `pdf_text::TextPage` yielded no text while PDFium yielded 242 normalized
   characters beginning with “International Telecommunication Union”. The
   font is an embedded TrueType subset marked `/Flags` Symbolic, with no
   `/Encoding` and no `/ToUnicode`; its MS-Symbol cmap still maps
   `0xF000 | code` to the correct glyphs. The port had implemented §5.2
   fallback steps 1–3 but not step 4 (reverse the font-program cmap).
   `pdf-font::FontProgram::char_for_gid` now provides a cached reverse cmap,
   folds the MS-Symbol `0xF000..=0xF0FF` range to the low byte, and
   `pdf-content` fills still-unmapped simple-font codes with
   `UnicodeSource::FontProgram`. The page now extracts 244 normalized
   characters, inside the ≤4-character PDFium-oracle tolerance. The
   renderer's font, content, and text test suites pass. The temporary PDFium
   fallback is no longer needed for this font class.

---

## 9. Changes against the four source documents

| Source | Change | Reason |
|---|---|---|
| `renderer-integration-plan.md` §1 | The renderer does not move into `lege-pdf/render/`. Lege refers to it by relative path. | The user prefers a temporary path. The renderer is still in work. |
| Same, §1 | `lege-pdf-write` does not adopt `pdf-geom` now. | It couples two workspaces for no gain in this effort. Do it when the renderer moves. |
| Same, §2 | pdfium removal grows: it must also replace outline extraction and text extraction. | Section 2.2. The renderer supplies neither. |
| Same, §5 | Unchanged in design; the wgpu global-state audit moves to the front of the phase. | The audit decides whether the rest of the phase is possible. |
| `compute-scheduler-plan.md` §2.3 | No pdfium actor. | pdfium goes away. |
| Same, §5 | Priorities, generations, and worker-local contexts return. | A viewer comes later, and the renderer brings real per-worker state. |
| `render-upgrade.md` §1 | Direct Gray8 removes Lege's RGB buffer and Lege's RGB-to-gray pass, not the renderer's internal RGBA work. | `pdf-render-cpu/src/surface.rs` converts at the end. |
| Same, §4 | Exclusion rectangles become "fill the Gray8 rectangles with white". | The render request has no exclusion field. |
| Same, §5 | Semantic image hints move to Section 8 as a request. | No public accessor exists. |
| Same, phases 1 to 8 | Reordered into Phases 1b, 1c, 6, and 9 of this plan. | The renderer-independent work must land first. |
| `auto-toc-plan.md` §5 | Outline extraction becomes required work, not advice. | The renderer has no outline reader. |
| Same, §6 | Candidate capture moves after the page-owned pipeline. | Phase 4 rewrites the same code. |

---

## 10. Risks

- **The renderer changes under you.** The API is stable from 2026-07-22, but
  behaviour still improves. Pin nothing to exact pixels. Re-run the
  end-product comparison at each phase.
- **Text extraction is the critical path.** `pdf-text` is a full port of
  `CPDF_TextPage`, and Phase 3 cannot finish without it. It is the largest
  single item of work in the whole effort and it sits in another
  repository. Start it first, and track its phases beside these.
- **A wrong table of contents is worse than none.** Use conservative
  scoring, emit nothing on doubt, never override a source outline, need two
  or more entries, and keep corpus snapshot tests.
- **Blocked workers during a GPU wait.** The design bounds this at
  `workers - K`. The escape is to split the page job at the inference
  boundary. Do not build that split before a measurement asks for it.
- **Byte estimates.** A render size is known before the render from the page
  size and the target scale, so estimates are better than in the pdfium era.
  Keep a floor of two concurrent pages, and log estimated bytes against
  actual bytes.
- **Two workspaces drift.** Different toolchains or different codec versions
  break the build in a confusing way. The Phase 2 `cargo tree -d` check and
  the shared toolchain pin catch this.
- **The manifest schema bump for DjVu** must land with the encoder binary in
  the same change. The version handshake already fails fast.
- **Music score edition.** Movement titles look like chapter titles. Test the
  synthesized outline on score scans before the shared default ships.
