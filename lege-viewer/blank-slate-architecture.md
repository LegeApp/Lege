# Lege Viewer — Blank-Slate Architecture

> **Roadmap note (2026-07-25):** This remains the main architecture reference,
> but its build-order section is historical. `STAGES.md` supersedes the
> promotion of GPU compositing and is authoritative for current stages.

**Premise:** a viewer and renderer designed together, by one team, with no
black-box boundary between them. Every existing viewer except Acrobat farms
rendering out to a third-party engine and therefore consumes exactly one
artifact: finished bitmaps. That single fact explains most of what is wrong
with PDF viewers — they are bitmap browsers with chrome. When the renderer is
yours, the viewer can consume the document's *structure* at every stage, and
that changes the architecture, the feature set, and what "fast" means.

This document is the blank-slate design: what I would build from the
beginning. It supersedes `fundamentals-plan.md` where they conflict; §12
lists the divergences explicitly.

---

# 1. Themes

## 1.1 The renderer is a document engine, not a bitmap server

The single organizing idea. The renderer's pipeline is:

```text
bytes → DocumentSnapshot → SemanticPage → CompiledPage (IR) → raster
```

A black-box viewer sees only the last arrow. A tightly-integrated viewer
subscribes to **every stage**:

| Artifact | When available | What it enables |
|---|---|---|
| Page tree + sizes | ~immediately | Layout, scrollbar geometry |
| Outline, links, dests | ~immediately | Navigation, link peek |
| `SemanticPage` | at compile, **before any raster** | Text geometry, line boxes, selection, search — *not OCR, not raster-dependent* |
| `CompiledPage` IR | at compile | Re-raster at any scale/crop for free; IR-level recolor; content-extent analysis |
| Raster tiles | streamed | Progressive display |
| Image decode completion | streamed | Text-first two-pass display (§4.3) |

Everything interesting in this document falls out of that table.

## 1.2 Latency is the product

The spec, treated as hard requirements with p99 measurement, not vibes:

```text
input → photon (scroll, pan, thumb drag)   ≤ 1 display frame
keypress → paged view                       ≤ 1 display frame
open → first readable text                  ≤ 150 ms (cold, 500-page doc)
zoom settle → crisp text                    ≤ 100 ms
scrub any distance → recognizable page      always (tiered, never blank)
idle CPU                                    ~0
```

The reader's scroll never waits on the renderer; the renderer races the
reader and almost always wins, because it is ~4× faster than pdfium and is
fed viewport intent (§3.4) rather than guessing.

## 1.3 The document is the interface

Chrome is subordinate: one toolbar, one sidebar, one status strip, one
scrollbar that doubles as the document map. No feature ships if it can be
expressed through the document surface itself (link peek > link panel;
scrollbar marks > search-results window). Fullscreen hides everything but
the page and the map.

## 1.4 Progressive fidelity, honestly presented

The viewer always shows the best truth it has and never fakes completeness:
a page appears as skeleton → text-first raster → full raster, visibly
converging. Blank white is forbidden; so is pretending a draft is final
(draft tiles get no special badge — they are simply replaced within
milliseconds — but a page that *failed* shows an explicit error badge, never
a silent blank; the renderer's `catch_unwind` page boundary and
`lowering_degraded` flags make failure states known, and we surface them).

## 1.5 Text is the substrate

Paging, anchoring, selection, search, outline synthesis, and accessibility
are all views over one per-page text/line dataset that the compile stage
produces as a by-product. Build that substrate once, first-class, and a
dozen features become thin.

---

# 2. What I would NOT build from scratch

Blank slate means nothing is sacred, not that everything is rebuilt. The
test: *does owning this layer improve reading?*

- **Windowing/input (winit): keep.** Wayland + X11 + Win32 + AppKit event
  handling, IME, DPI, monitors — a permanent standards-maintenance project
  with zero reader-visible upside. Winit is the platform boundary and
  nothing more; every pixel and every behavior above it is ours.
- **Buffer presentation (softbuffer): keep, as one of two presenters.** It
  is ~3k lines solving exactly one narrow problem (shared-memory buffers +
  damage on four platforms). Vendorable if it ever stalls. What I *would*
  change from the earlier plans: the GPU compositor is not a "phase 10
  maybe" — it is the planned steady-state presenter (§5), with software as
  the reference implementation and fallback.
- **Text shaping for UI chrome:** reuse the renderer's font/glyph machinery
  (it already rasterizes glyphs better than a UI toolkit needs). A shaping
  library only if/when complex-script UI labels matter.
- **Everything else — compositor, scroll model, scrollbar, tiles, caches,
  chrome, search, selection — is built here.** These are exactly the layers
  where reader-first decisions live, and where frameworks impose their own
  opinions.

---

# 3. System architecture

## 3.1 Process and thread model

One process. Roles, not thread-per-feature:

```text
UI thread          winit loop · input · scroll model · compositor ·
                   present · owns all UI state, no locks, no waiting
conductor thread   the scheduler brain: owns priorities, budgets,
                   request lifecycle; the only writer to work queues
compile pool       parse → SemanticPage → {CompiledPage, TextPage,
                   LineSet, links, extents} — one pass, N workers
raster pool        CompiledPage → tiles (band-ordered, cancellable)
io/decode          image decode (feeds SharedImageCache), file watch,
                   prefetch reads
```

The renderer's existing `RenderScheduler` (compile pool → render pool,
memory permits, reordering) *is* most of this; the conductor is the
viewer-side owner that feeds it and arbitrates across documents, thumbnails,
and text indexing. Renderer worker panics are already caught per page
(`RenderError::Panic`); the viewer quarantines a panicking page (error
badge, no retry storm) — one bad page never takes the app down.

## 3.2 Data flow: one loop, two directions

```text
        viewport intent (arc-swap snapshot, written per frame)
  UI ─────────────────────────────────────────────────► conductor
   ◄───────────────────────────────────────────────── UI
        coalescing update queue + one Wake user-event
        (tiles, line sets, geometry, outline, search hits)
```

- **Down:** the UI publishes a `ViewportIntent` snapshot every frame it
  changes: scroll position, velocity, direction, zoom bucket, visible +
  overscan page ranges, hover position on the scrollbar. Lock-free
  (arc-swap); workers read the *current* intent at every operation
  boundary — a raster job checks "is my page still near the viewport?"
  mid-job and aborts itself. Cancellation is thus continuous and adaptive,
  not just token-triggered.
- **Up:** workers push typed updates into a bounded coalescing queue; the
  first producer flips a wake flag and sends one winit user event; the UI
  drains everything on wake. No event storms, no per-tile wakeups.

## 3.3 Memory: one arbiter

A single budget arbiter owns all caches — compiled pages, raster tiles,
thumbnails, the renderer's image cache, text/line sets, search index.
Eviction scores by `reproduction_cost × distance_from_viewport ÷ bytes`,
with floors (never evict the last representation of a visible page; never
evict line sets for the current chapter). Whole-app memory is one number in
the diagnostics overlay, and it stays inside a configurable envelope.

## 3.4 The conductor's priority ladder

Recomputed on every intent change (not every scroll pixel):

```text
1. visible tiles missing any raster        (draft quality)
2. visible tiles missing final raster
3. compile of pages entering overscan      (compile-ahead ring is wider
                                            than the raster ring — IR is
                                            the expensive, reusable part)
4. overscan tiles, weighted by scroll direction and velocity
   (at high velocity, skip finals — drafts only — the reader is flying)
5. scrollbar hover ring thumbnails
6. background: thumbnail sweep, full-text index, outline synthesis
```

Velocity-aware degradation is a tight-integration exclusive: the conductor
knows the reader is skimming at 4 pages/sec and stops wasting raster time on
finals that will scroll away before they finish.

---

# 4. The surface model: tiles, tiers, and text-first raster

## 4.1 Tiles are the only currency

Every rastered pixel lives in a fixed-size tile (256×256 device px),
keyed `(doc, page, zoom_bucket, tile_xy, tier)`. No whole-page surfaces,
ever. Why tiles-first rather than pages-with-tiling-at-high-zoom:

- Uniform memory units — the arbiter and caches deal in one shape.
- Progressive display for free — tiles land individually; a page fills in
  rather than popping in whole.
- Raster order can follow exposure: the band about to scroll on-screen
  rasters first (§4.2).
- They are exactly GPU texture atlas entries when the GPU presenter lands
  (§5) — no model change.
- High zoom needs no special case; only viewport-intersecting tiles exist.

Zoom buckets at power-of-√2 steps; between buckets the compositor scales
the nearest tier (GPU: free; CPU: cheap bilinear on the visible strip
only), and re-raster on settle makes it crisp.

## 4.2 Band-ordered raster

Within a page raster job, tiles are produced in scroll-exposure order
(bottom-up when scrolling down, top-down when scrolling up), streamed to the
UI as completed. The strip about to be revealed is statistically already
present — this, plus the scroll-blit path, is what makes "never shows a
blank strip" real rather than aspirational.

## 4.3 Two-pass, text-first raster (the flagship integration feature)

The rasterizer walks the `CompiledPage` display list twice:

```text
pass 1  vector fills, strokes, and TEXT — no images, no shadings
        → tiles marked tier=TextFirst, streamed immediately
pass 2  images (as decodes complete) and shadings composited in
        → tiles upgraded to tier=Final
```

A scanned-figure-heavy page becomes *readable* in the time it takes to
raster glyphs (a few ms), while a 40 MB JPEG2000 decodes in the background
and fades in when ready. No black-box viewer can do this — they get the
bitmap when everything is done or nothing is. For image-*only* pages
(scans), pass 1 has nothing, so the draft tier (low-res full raster,
`RenderQuality::Draft`) covers the gap instead.

Fallback ladder per tile, compositor picks the best available:

```text
Final → TextFirst → Draft → neighboring-bucket scaled → thumbnail scaled
→ skeleton (paper color, page border, page number — styled, not blank)
```

## 4.4 What the CPU rasterizer needs added

Two renderer-side work items this architecture asks for (both natural
extensions, neither speculative):

1. **Tile-run rendering:** render a `RenderRequest` whose `crop` is a tile
   list with a completion stream, instead of one rect per request — avoids
   per-tile job overhead. (Today: `crop` exists per-request; batching is an
   optimization.)
2. **Pass-split execution:** a display-list filter that defers image/shading
   ops to a second pass with a completion callback per image. The IR is
   already explicit enough for this (`ImageIr` ops are discrete).

---

# 5. Presentation: software reference, GPU steady-state

> **Current roadmap correction:** `STAGES.md` retains WGPU as an implemented
> optional presenter, not a required steady-state destination. The renderer
> corpus attributes page-readiness cost to decode, sampling, glyph/path work,
> and in-page composition, and contains no controlled presenter A/B. This
> section records the earlier design rationale, not a current performance
> conclusion.

The earlier plans treated a GPU presenter as a distant maybe. Blank slate,
I invert that: **design for the GPU compositor as the destination**, ship
the software path first as the reference implementation, keep both forever.

- **Software presenter** (softbuffer): composites tiles into the retained
  buffer, scroll-blit fast path, damage rects. Proves correctness, runs
  anywhere, golden-image testable. This is the reference semantics.
- **GPU compositor** (wgpu, already a workspace competency via lege-gpu):
  tiles upload once into an atlas; per frame the GPU draws visible tile
  quads + chrome. What it buys, concretely:
  - **Fractional scroll placement** — no integer-snap during motion, so
    slow precision scrolling is perfectly smooth instead of stepping.
  - **Vsync-paced 120–165 Hz** with compositor-grade frame pacing; the CPU
    per frame does ~nothing (tile list + uniforms).
  - **Free scaling** between zoom buckets → continuous pinch zoom that
    never shows nearest-neighbor shimmer.
  - Cheap overlays (selection, search marks, crossfades on tier upgrade).
- Both implement one `Presenter` trait; tiles are the shared currency, so
  the compositor above the trait doesn't know which is active.

The decision rule stands from the earlier plan — replace/augment the
presenter, never the UI — but the GPU path is scheduled work (right after
the renderer bridge), not deferred indefinitely.

---

# 6. The text and line substrate — and the OCR concern, retired

## 6.1 Why line-paging does NOT depend on OCR

The worry: line-by-line PageUp/PageDown might depend on OCR rather than the
initial raster. It doesn't, on either class of document:

- **Born-digital PDFs (the overwhelming majority):** text geometry comes
  from the *compile stage*. `pdf-text::TextPage` builds from the
  `SemanticPage` — the same artifact the raster is lowered from — so
  per-character positions exist **before the first pixel is rastered**.
  Line boxes are a cheap clustering over that. Zero OCR, zero raster
  dependency, zero added latency.
- **Scanned PDFs / DjVu:** no text objects, but line *geometry* (which is
  all paging needs — not characters, just "where are the line bands")
  falls out of the raster itself via **projection-profile segmentation**:
  binarize the draft tile column, sum ink per row, find the valleys.
  ~1 ms per page on the draft raster we already produce, and it runs in
  pass 2 of the same worker. This is classic document-analysis, not OCR.
  For Lege-processed books it's even simpler: the lege pipeline's
  JBIG2/OCR word boxes ride along in the file and map directly.

## 6.2 The LineSource ladder

```rust
pub enum LineSource {
    ContentStream,   // compile-time, exact       (born-digital)
    InkProfile,      // draft-raster projection    (scans, ~1 ms)
    Embedded,        // OCR/hOCR boxes already in the file
    None,            // geometric paging fallback
}
```

One `PageLineSet { lines: Box<[LineBox]>, source: LineSource }` per page,
produced worker-side, a few KB, prefetched like tiles. Paging semantics
(last-fully-visible-line → viewport top, and the symmetric PageUp),
the reading anchor, and arrow-key line stepping all consume `PageLineSet`
and never know the source. **The geometric fallback
(`viewport − clamp(8%, 24..96 px)`) is always instant** — a paging keypress
never waits on any of this; a late-arriving line set simply upgrades the
next keypress.

## 6.3 What else the substrate feeds

- **Selection & copy:** char quads in reading order (TextPage already
  orders them), rendered as an overlay; copy uses extracted text with
  hyphenation-aware joining (`CharType::Hyphen` is already classified).
- **Search:** background full-text index (per-page UTF-16 is a compile
  by-product); incremental find-as-you-type; hits are char-range → quads
  overlays, plus **hit marks on the scrollbar track** — the document map
  shows where in the book the matches are (editor-style).
- **Outline synthesis:** when a PDF has no bookmarks (most don't), cluster
  font-size/weight outliers across pages — the SemanticPage knows every
  run's font and size — into a synthesized outline, labeled as such. The
  sidebar is never uselessly empty.
- **Accessibility (later, principled):** the same substrate is exactly what
  AccessKit needs; a reader app that can't be read aloud is incomplete.

---

# 7. Scroll, paging, and navigation

The scroll model, command set, precision rules, reading anchor, and
line-continuity paging semantics of `fundamentals-plan.md` §4 carry over
unchanged — they were designed reader-first and nothing here contradicts
them. Deltas and additions:

- **Fractional placement in motion** on the GPU presenter (§5): the
  software path snaps to device pixels during motion (carrying remainders);
  GPU presents true fractional offsets. Settled positions snap on both.
- **Position history:** every *jump* (link click, outline click, search
  hit, page-number entry, Shift+scrollbar teleport) pushes the reading
  anchor onto a back/forward stack. `Alt+←/→` like a browser. Skimming by
  scroll never pollutes the stack — only jumps do. This is the other half
  of "never lose your place."
- **Link peek:** hovering an internal link renders the *target region*
  (cheap: compile-ahead + crop raster of a few tiles) in a popup —
  footnotes, citations, and figures readable **without leaving your
  place**. For a research-reading tool this is the killer feature, and it
  is pure tight-integration: a black-box viewer cannot afford a speculative
  render of an arbitrary rect.
- **Scrollbar** as specified before (absolute drag, live scrub, hover
  preview thumbnails, page ticks ≤200 pages, track click = page step,
  Shift = teleport) plus search-hit marks (§6.3).

---

# 8. Features that exist because the renderer is ours

The honest sales pitch of the integration, gathered in one place:

1. **Text-first progressive pages** (§4.3) — readable in milliseconds while
   images stream in.
2. **True night mode / recolor at the IR level.** Remap fill and stroke
   colors in the display list (dark paper, warm text) and re-raster;
   images pass through untouched or gently dimmed. Every black-box viewer
   does raster inversion, which destroys images and antialiasing. Ours is
   a color-policy pass over `CompiledPage` — crisp text on any theme.
   (The IR and `pdf-color` policy layer make this a contained feature.)
3. **Content-aware margin trim.** Compute the ink extent per page from the
   display list (exact, not raster heuristics), offer one-key "trim
   margins" that re-fits the *content box* to width — enormous on
   letter-size academic PDFs read on monitors. The reading anchor makes it
   position-stable.
4. **Link peek / footnote peek** (§7) — speculative crop renders.
5. **Velocity-aware scheduling** (§3.4) — the renderer knows how you read.
6. **Synthesized outlines** (§6.3) — structure recovered from typography.
7. **Instant crisp zoom** — compiled-page cache + crop raster means zoom
   settle is a ~few-tile re-raster, not a whole-page job.
8. **Honest failure surfaces** — per-page panic quarantine and degraded-
   lowering flags surface as badges, not silent blanks or crashes.

Each is small *because* of the architecture; none is feasible over a
bitmap-server boundary.

---

# 9. User-facing feature boundary

## Core (the product)

- Open PDF (incl. encrypted — full password support exists in the
  renderer); continuous vertical fit-width layout; zoom (cursor-anchored,
  fit-width/fit-page commands); fullscreen.
- Line-continuity paging, line arrow-stepping, reading anchor, back/forward
  history.
- The scrollbar/document-map with previews, ticks, and search marks.
- Search (incremental, whole-doc), selection/copy, outline sidebar
  (real or synthesized), internal + external links, link peek.
- Night mode (IR recolor), margin trim.
- Status readout: page x of y · % visible.
- Diagnostics overlay (frame timing, tiers shown, cache MB, queue depths).

## Deliberately absent (P5 from the fundamentals plan, unchanged)

Facing-page/book modes, per-viewer annotation *editing* (static annotation
appearances render; authoring is Lege-pipeline territory), forms, JS,
tabbed MDI, print-production tooling, plugin systems, cloud anything.
A second window is `lege-viewer file.pdf` again.

## Later, behind existing seams

- **DjVu**: a second `DocumentProvider`. Note honestly: `djvulibrust` is
  encode-only today — the decode path (JB2, IW44, ZP-coder decode) is a
  real project. When it lands, DjVu's layered format maps *beautifully*
  onto this architecture: the JB2 text mask is pass 1, the IW44 background
  is pass 2 — text-first display is native to the format. Embedded OCR
  boxes feed `LineSource::Embedded`.
- Text reflow mode (lege-process has reflow machinery); e-ink profile
  (Gray8 end-to-end, no animation); AccessKit; annotations as an overlay
  document, never mutating the source file.

---

# 10. Testing and determinism (built in, not bolted on)

- The compositor is a pure function of (tile cache state, viewport, theme)
  → golden-frame tests across platforms on the software presenter.
- Input traces (winit events) are recordable and replayable; scroll-feel
  regressions become diffable numbers (position curves), not vibes.
- The renderer side already has the determinism harness, chaos tests, and
  fuzzing; the viewer inherits the same standard: a corpus of layout-nasty
  documents (two-column, rotated, scanned, huge-image, 10k-page synthetic)
  with per-phase gates, and the frame-metrics overlay from the first
  prototype (`fundamentals-plan` §10 / viewer.md §20 gates all still
  apply).

---

# 11. Build order (delta view)

> **Historical order:** the reconciled required stages are in `STAGES.md`.
> In particular, the GPU compositor entry below no longer gates search,
> selection, outline, renderer-aware display, or any other reader feature.

Phases 0–3 of the earlier plans stand (contracts → window/presenter →
painter → synthetic 10k-page document with scrollbar, paging, anchor).
Then:

```text
4   Renderer bridge: DocumentProvider, tiles, tiers, conductor,
    band order, budgets, cancellation           ← tiles-first from day one
4.5 Text substrate: compile-side TextPage/LineSet, line paging on real
    docs, ink-profile lines for scans           ← retire the OCR worry here
5   Zoom, navigation, history, link peek, fullscreen
6   GPU compositor (promoted: right after the bridge is proven)
7   Search + selection + outline (all substrate consumers)
8   Night mode, margin trim (IR-level features)
9+  Chrome polish, DjVu decode project, reflow, accessibility
```

The renderer-side asks (tile-run rendering, pass-split raster, IR recolor
hook) should be scheduled alongside phases 4, 4.5, and 8 respectively —
they are the three places the integration wants the renderer to move
toward the viewer.

---

# 12. Divergences from the earlier plans

| Topic | fundamentals-plan.md | This document |
|---|---|---|
| Surface currency | Whole-page surfaces; crop tiles only above a size threshold | Tiles universally (256², bucket-keyed) |
| Raster progression | Whole-surface tiers | Band-ordered tile streaming + two-pass text-first raster |
| GPU presenter | Deferred (viewer.md phase 10) | Planned steady-state, built right after the bridge |
| Line data | pdf-text clustering, geometric fallback | Same, plus ink-profile source for scans; explicit `LineSource` ladder; OCR ruled out entirely |
| Scheduling | Priority classes per settled frame | Conductor + continuously-published `ViewportIntent`; velocity-aware degradation; mid-job self-cancel |
| Renderer changes | None assumed | Three scoped asks: tile-run rendering, pass-split raster, IR recolor |
| Feature ceiling | Paging/scrollbar/preview fundamentals | Adds history, link peek, search marks, synthesized outline, night mode, margin trim — all substrate/IR consumers |

What did *not* change: winit + softbuffer as the platform floor, the
one-crate no-widget-framework discipline, the scroll model and precision
rules, the reading anchor, line-continuity paging semantics, the
never-blank ladder, single layout mode, and every performance gate. The
blank slate confirmed those; the genuinely new ground is the surface model
(tiles + text-first raster), the conductor, the promoted GPU compositor,
and the family of features that only exist because the renderer is ours.
