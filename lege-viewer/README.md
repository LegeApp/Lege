# Lege Viewer

Lege Viewer is the reader-first native PDF viewer for the Lege ecosystem. It
is one application-specific Rust crate rather than a reusable GUI toolkit, and
it consumes the semantic, text, compiled-IR, and raster APIs of
`lege-pdf/render` directly.

The authoritative development order and current stage are in
[`STAGES.md`](STAGES.md). The older phase documents are retained as design
references; their numbering is historical where it conflicts with that
roadmap.

## Stage 5 renderer-aware display

The viewing loop, production presentation path, and first document-reading
tools are operational:

- `winit` application loop with redraw-on-demand and suspend/resume for
  zero-sized windows;
- one retained, viewer-specific frame scene shared by both presenters;
- WGPU presentation by default, with automatic in-session fallback to the
  `softbuffer` reference presenter if GPU initialization or presentation fails;
- a generic `lege-gpu` compositor with fractional quads, effective clips,
  nearest/linear sampling, ordered batches, and sRGB-correct output;
- a lazy, bounded tile atlas: up to four 64 MiB texture-array banks, exact
  tile revision tracking, current-frame pinning, and LRU eviction;
- damage tracking and presenter-owned scroll reuse;
- continuous virtual layout for both real PDFs and a 10,000-page synthetic
  reference document;
- asynchronous compile and raster worker pools with viewport priorities,
  cancellation, generation tracking, panic quarantine, and bounded updates;
- navigation-mode prediction (`forward`, `backward`, `jump`, `idle`) with a
  140 ms settle transition, visible-first compilation, bounded exact-quality
  next-page rendering, and a wider whole-page preview ring;
- off-screen final tiles for likely sequential destinations, plus structural
  text-geometry tiles for cold text PDFs so a random seek has a
  layout-correct fallback while full rasterization completes;
- 256×256 CPU-rendered tiles with final/draft/neighboring-bucket/thumbnail/
  structural/skeleton fallback;
- fit-width and fit-page layout, bucketed zoom, wheel and line-continuity
  paging, reading anchors, scrollbar dragging, and hover thumbnails;
- one-pass retention of `SemanticPage`, `TextPage`, `CompiledPage`, and viewer
  line geometry;
- low-priority whole-document text compilation that never blocks visible-page
  work, with live indexing progress and foreground-only queue diagnostics;
- a compact per-page UTF-16 search index governed by the shared memory
  arbiter, with anonymous temporary-file spill after 64 MiB;
- cancellable background case-insensitive search, stable 10,000-result
  limiting, next/previous navigation, visible-page highlights, and scrollbar
  result marks;
- renderer-backed character hit testing, drag/word/line selection,
  edge-autoscroll, multi-page copy, and native clipboard integration;
- embedded PDF outlines with direct, legacy named, name-tree, and GoTo
  destinations, including crop/rotation-correct target positions;
- PDF link annotations extracted in one document-level pass, with direct and
  named internal destinations, safe HTTP/HTTPS/mail links, pointer feedback,
  and browser-style back/forward history for internal jumps;
- predictive link hover: the target page is warmed immediately and a delayed
  off-screen-rendered page preview appears beside the pointer with the target
  position marked;
- a virtual, wheel-scrollable contents sidebar; documents without embedded
  outlines receive a heading-derived contents list after indexing, with a
  page-list fallback;
- glyph-backed toolbar, search, contents, and status text using the renderer's
  bundled font program, with retained surface caching;
- alpha search/selection overlays in both presenters, with linear-light
  software blending to match WGPU composition;
- immutable per-frame tile snapshots, retained scene/tile scratch capacity,
  GPU atlas accounting in the shared memory arbiter, and optional GPU
  diagnostics.
- cache promotion that removes superseded draft/structural tiles and refreshes
  eviction distance after every viewport replan.
- content-aware margin trim derived from compiled display-list geometry, with
  conservative padding, per-axis full-bleed fallback, mixed-size/rotation
  support, and anchored progressive relayout as background compilation
  discovers pages;
- renderer-level Night (`#252525` paper, `#D8D1C4` text) and Warm Paper
  (`#F2E8D2`) policies for vector/text/shading/stencil paint while ordinary
  images retain source color;
- crop-and-palette variant identities across exact tiles, fallback tiles, and
  canonical previews, preventing stale pixels from being promoted as current;
- persisted viewer-wide trim and palette settings;
- an on-canvas password prompt for encrypted documents, distinguishing "needs
  a password" from a corrupt file and from an encryption scheme the renderer
  cannot decrypt, on both the toolbar and the command-line open paths;
- page-local failure placeholders that name the page and the reason, with a
  bounded retry so a permanently broken page is set aside rather than
  re-requested on every replan.

The PDF engine and both presenters are enabled in the default build. CPU
rasterization remains the default. The decoded-image WGPU renderer can also
produce eligible scan tiles experimentally; other tiles retain seamless CPU
fallback, and either presenter can display both routes.

## Run

Open a PDF:

```text
cargo run -p lege-viewer -- document.pdf
```

`--presenter auto` is the default. The explicit modes are useful for
certification and diagnosis:

```text
cargo run -p lege-viewer -- --presenter gpu document.pdf
cargo run -p lege-viewer -- --presenter software document.pdf
```

`gpu` is strict and exits with a controlled error if GPU presentation cannot
continue. `auto` preserves the open document, viewport, and session state when
it switches to software.

PDF image rendering has a separate experimental policy:

```text
LEGE_PDF_IMAGE_RENDERER=gpu cargo run -p lege-viewer -- document.pdf
LEGE_PDF_IMAGE_RENDERER=auto cargo run -p lege-viewer -- document.pdf
```

The unset/default value is `cpu`. This policy affects eligible decoded-image
tiles only; it is independent of `--presenter`, and unsupported content falls
back to the existing CPU raster worker. GPU discovery is deferred to the
background raster pool; opening the document and producing text-first
structural tiles do not wait for adapter initialization.

Run the deterministic synthetic reference:

```text
cargo run -p lege-viewer -- --synthetic
cargo run -p lege-viewer -- --synthetic 25000
```

Set `LEGE_VIEWER_GPU_DIAGNOSTICS=1` to print the selected adapter/backend,
atlas bytes and occupancy, uploads, draw/vertex counts, compose/present time,
and conductor queue depths. Other values do not enable the diagnostic stream.

Current keyboard controls are PageUp/PageDown or Space, arrow keys, Home/End,
`+`/`-` zoom, `W` fit width, `P` fit page, `S` snap to the nearest page
boundary, `B` contents sidebar, `M` margin
trim, `N` Original/Night/Warm Paper cycle, Alt+Left/
Alt+Right history, Ctrl+F search, F3/Shift+F3 search navigation, Ctrl+C copy,
and F11 fullscreen. Arrow keys step one text line; Shift makes the step a
quarter and Control triples it, and Left/Right pan horizontally once a zoom has
pushed the page past the canvas. On the scrollbar, a held track click repeats,
and shift-click or middle-click centres the thumb under the pointer and keeps
it there while the button is down. Search editing supports IME composition, selection,
cut/copy/paste, and replacement. The Options popup contains the
Original/Night/Warm/Earth/Sea appearance choices. The Process toolbar section
opens a resizable processing card with explicit Run, Current page, and Profile
controls; it runs the whole document by default and cancels a running worker
with Stop.

Startup uses automatic zoom: one resolution-aware step above full-page fit,
capped by fit-width. It grows and shrinks with the canvas and display scale.
Pages narrower than the canvas are centered in automatic, fit-page, and manual
zoom modes; painting, text hit-testing, and selection share the same centered
document origin.

## Verify

```text
cargo test -p lege-viewer --all-targets
cargo clippy -p lege-viewer --all-targets --no-deps -- -D warnings
cargo test -p lege-gpu --features presentation presentation::tests
```

The tests cover deterministic headless composition, the 10,000-page layout,
tile fallback and generations, directional prediction, off-screen final-tile
completion, cache promotion, rapid viewport replanning and queue drainage,
progressive indexing, background search, index spill, search offset stability,
selection, PDF outline destination forms, PDF semantic/text/IR compilation,
CPU tile rendering, cancellation, immutable tile snapshots, scene identity,
atlas geometry, clipping, alpha blending, and color conversion. The software
compositor remains the deterministic golden-frame oracle. Windows smoke
verification should exercise both strict `gpu` and strict `software` modes
against a real PDF.

## Deliberately deferred

- true text/vector-first display-list filtering (the current structural tier
  uses compiled text geometry) and renderer-side in-place tile upgrades;
- renderer tile-run batching;
- broader chrome polish and accessibility;
- OCR-backed search for image-only documents and more sophisticated
  synthesized-outline scoring;
- GPU offscreen image-difference certification, cross-platform visual
  certification, and accessibility API exposure.

The types and ownership seams for these features remain in place. Link
extraction deliberately lives in `pdf-document`, while scheduling and display
remain viewer-owned; the renderer/viewer pair therefore stays integrated
without duplicating PDF object-tree parsing in the application.
