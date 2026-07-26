# Lege Viewer — Fundamentals Plan (Reader-First Core)

> **Roadmap note (2026-07-25):** This remains the authority for reader-first
> mechanics, but not for current stage numbering or order. See `STAGES.md` for
> the reconciled roadmap and current position.

This document extends `viewer.md`. That document defines the platform
foundation (winit + softbuffer, retained framebuffer, damage tracking, thread
ownership, phased build-up). This one defines the **reader-first mechanics**
that the foundation exists to serve, resolves the open decisions needed to
start building, and grounds the renderer bridge in the actual API of the
renderer crates (`lege-pdf/render`).

Where the two documents overlap, this one is authoritative.

---

# 1. Reader-first pillars

Every architectural decision below traces to one of these five pillars. When a
future feature or refactor is proposed, it must serve one of them or it is out
of scope.

**P1 — Never lose your place.** Paging, zooming, resizing, and window changes
must preserve the reader's position *in the text*, not a pixel offset.
Fast skimming must be recoverable: the reader can always re-anchor visually.

**P2 — Always know where you are.** It must be obvious at a glance which page
is shown, how much of it is visible versus cut off, and where the viewport
sits in the whole document — without moving anything.

**P3 — Motion fidelity.** Input maps to movement exactly: no quantization of
wheel/touchpad deltas, no animation interposed on direct manipulation, no lag
between thumb and pointer. `f64` scroll positions from day one (viewer.md §11).

**P4 — Never blank, never tear.** Every presented frame shows the best
available content for every visible page. A white void is a bug. The renderer
being ~4× faster than pdfium is the enabler; the viewer's job is a request
policy and fallback ladder that never leaves it waiting on screen.

**P5 — Feature minimalism.** One layout mode: continuous vertical scroll,
fit-width by default. Plus a fullscreen toggle. No facing pages, no
single-page-flip mode, no horizontal book mode. Anything achievable by the one
mode plus zoom is not a second mode.

---

# 2. Coordinate spaces and the document layout

Three spaces, named consistently everywhere:

```text
page space      PDF user space of one page (y-up, points). Text geometry
                (pdf-text) and render transforms live here.
document space  y-down, logical pixels at zoom=1.0. All pages stacked
                vertically with a fixed inter-page gap. The scroll model,
                line index, and page placements live here.
device space    y-down physical pixels of the window framebuffer.
                document → device is (translate by -scroll, scale by
                zoom·dpi_scale), snapped per the placement rules of
                viewer.md §11.
```

The `PageLayoutIndex` of viewer.md §10 (placements + `page_starts_y` +
binary search) is retained unchanged. Two additions:

```rust
pub struct PagePlacement {
    pub page: PageIndex,
    pub bounds: RectF,          // document space
    pub page_to_doc: Affine,    // page space (y-up pts) → document space
}
```

- `page_to_doc` is stored per page so text geometry and render transforms are
  derived from one matrix, not re-derived ad hoc (rotation folds in here).
- The layout index is rebuilt only on zoom-mode change or page-size discovery,
  never during scrolling.

**Layout policy (P2, P5):** default is fit-width — the page fills the canvas
width minus a minimal fixed margin, which maximizes visible page area.
Zooming beyond fit-width enables horizontal scrolling; zooming below is
allowed but the page stays horizontally centered. "Fit page" exists as a
command (not a mode): it just sets zoom so the current page's height fits.

---

# 3. The line index — foundation for paging and anchoring

This is the load-bearing new subsystem. It powers P1's paging and anchoring
and must be designed before the scroll commands that consume it.

## 3.1 What it is

Per page, an ordered list of text-line boxes:

```rust
pub struct LineBox {
    /// Document-space rect of the line (already through page_to_doc).
    pub bounds: RectF,
    /// Baseline y in document space (for tie-breaking and debug overlay).
    pub baseline_y: f64,
    /// First/last char index into the page's TextPage (future: selection,
    /// search, read-aloud).
    pub char_range: (usize, usize),
}

pub struct PageLineIndex {
    /// Sorted by bounds.top ascending (document space).
    pub lines: Box<[LineBox]>,
    pub status: LineIndexStatus,   // Pending | Ready | NoText
}
```

## 3.2 How it is built

`pdf-text::TextPage::build(&SemanticPage, ...)` yields `CharInfo` with
`origin: Point`, `char_box: Rect`, `matrix`, and `text_object: TextRunId`
— all in page space. Line clustering:

1. Group chars by identical baseline under the text matrix: project each
   char's `origin` onto the matrix's up-vector; chars whose projected
   baseline differs by < ~20% of the nominal line height join the same line.
   (Start with the simple axis-aligned case — cluster on `origin.y` with
   tolerance — and only generalize to rotated text if the corpus demands it.)
2. Within a line, merge `char_box`es into one rect; split into separate
   `LineBox`es only on column gaps larger than ~2× median char width
   (multi-column pages: each column's line is its own box — paging then
   works per *visual* line, which is what a reader tracks).
3. Discard empty/whitespace-only lines. Transform to document space via
   `page_to_doc`, sort by top.

This runs on the **compile/worker side**, not the UI thread: the bridge
worker that compiles a page already holds the `SemanticPage`; it builds the
`TextPage`, derives the `PageLineIndex`, and ships only the index (a few KB
per page) to the UI. The `TextPage` itself is retained worker-side keyed by
page for later search/selection — the UI never touches it.

## 3.3 Availability policy (P4 applies to data too)

- Line indices are prefetched with the same directional overscan policy as
  page surfaces (viewer.md §10) plus the current page ±2 unconditionally.
- A paging command that arrives before the index is ready **does not block**:
  it falls back to geometric paging (§4.3) immediately. The index is a
  quality upgrade, never a synchronization point.
- `NoText` pages (scanned images, pure-figure pages) permanently use the
  geometric fallback. Later, DjVu/OCR word boxes or PAL layout lines can
  populate the same `PageLineIndex` shape — the paging code never knows the
  difference.

---

# 4. Scroll commands, reader semantics

The `ScrollModel` / `ScrollCommand` split of viewer.md §11 stands. This
section pins down the semantics of the commands that make reading work.

## 4.1 PageDown / PageUp — line-continuity paging (P1)

**PageDown:** find the *last fully visible* line in the viewport; scroll so
that line's **top edge aligns with the viewport top** (snapped to a whole
device pixel). The line the reader just finished is now the first line —
exactly one line of visual overlap, so the eye re-anchors instantly.

**PageUp:** find the *first fully visible* line; scroll so its **bottom edge
aligns with the viewport bottom**. Symmetric: the line that was first is now
last.

Precise definitions:

- A line is *fully visible* iff `line.bounds` ⊆ viewport (document space,
  with a 1-physical-pixel tolerance so rounding never demotes a line).
- Candidate lines come from all pages intersecting the viewport — the line
  index is queried per page and lines merge across page boundaries in
  document-space order. Paging across a page gap is seamless.
- If **no** line is fully visible (viewport inside a huge figure, or between
  pages): geometric fallback (§4.3).
- If the found line is the *only* fully visible line (a line taller than
  ~the viewport, or extreme zoom): still page by it, but clamp the scroll
  delta to at most `viewport_height` so paging always makes progress and
  never jumps more than one screen.
- Clamp at document ends. A PageDown that cannot fill the viewport scrolls
  to the exact bottom (no bounce, no overshoot).

Motion: instant by default. If an animated variant is ever added it must be
short (<120 ms), interruptible, and off by default — paging is for reading
rhythm, and animation delays re-anchoring (P3).

## 4.2 Fine scrolling (P3)

- Wheel/touchpad deltas accumulate in `f64`, applied directly; the fractional
  remainder carries between frames (viewer.md §11 precision rules).
- Arrow keys: a `FineStep` of one *median line height* of the current page
  (from the line index; fallback: a configurable fixed logical distance).
  This makes arrow-key reading advance by actual lines of the actual
  document, not an arbitrary 40 px.
- No smooth-scroll easing on wheel input by default. Kinetic/inertial motion
  exists only for touchpad flings where the platform delivers momentum
  events — never synthesized on top of discrete wheels.

## 4.3 Geometric fallback paging

When no line data applies: scroll by `viewport_height − overlap`, where
`overlap = clamp(8% of viewport_height, 24 px, 96 px logical)`. This is the
behavior of a good conventional viewer and the floor we never fall below.

## 4.4 The reading anchor (P1)

```rust
pub struct ReadingAnchor {
    pub page: PageIndex,
    /// Page-space y of the anchor point (top visible line's baseline,
    /// else the page-space point at the viewport top).
    pub page_y: f64,
    /// Where that point sat in the viewport, 0.0 = top.
    pub viewport_fraction: f64,
}
```

Recomputed whenever scrolling settles. Consumed — the anchor point is
restored to the same viewport fraction — on: zoom change, fit-width/fit-page,
window resize, DPI/monitor change, sidebar toggle, fullscreen toggle, and
layout rebuild after page-size discovery. This is the mechanism that makes
"zoom in, zoom out, still on the same sentence" true, which is the single
biggest lose-your-place failure in existing viewers.

---

# 5. Page-visibility model (P2)

The reader must always see, without interaction:

1. **Page boundaries are unmistakable.** Pages are drawn on a distinct canvas
   background with a fixed inter-page gap (~8 logical px) and a 1 px page
   border. A page edge crossing the viewport reads instantly as "cut off
   here".
2. **Current-page readout.** *Current page* := the page containing the
   viewport's vertical midpoint (stable, no flicker at boundaries — midpoint
   crossings are rare and meaningful). The status strip shows
   `Page 12 of 340 · 78%` where the percentage is the visible fraction of
   the current page's height. Updated per settled frame; cheap (two
   subtractions from the layout index).
3. **In-scrollbar page geography.** The scrollbar track *is* the document
   map: page-boundary ticks (§6) plus the thumb extent showing exactly what
   fraction of the document is visible. No separate minimap widget.

Rejected for minimalism (P5): per-page progress rings, floating "page x/y"
overlays that appear during scroll (the status strip is always visible and in
a fixed location — glanceable beats transient).

---

# 6. The scrollbar (P2, P3)

One custom vertical scrollbar, always visible when the document overflows
(no auto-hiding overlay by default — position awareness beats 12 px of
width). Horizontal scrollbar appears only when zoomed past fit-width.

## 6.1 Model

- Track maps linearly to document-space height. Thumb size =
  `viewport_height / content_height`, with a minimum thumb of 24 logical px;
  below the minimum, thumb position uses the standard compensated mapping so
  the thumb still reaches both ends exactly.
- **Absolute drag**: on thumb drag the thumb tracks the pointer exactly,
  every frame, via pointer capture (viewer.md §18). The document view
  updates live during the drag ("live scrub") — the fallback ladder (§7)
  guarantees something meaningful is shown for every page flown past.
- Track click: jump the viewport one screen toward the click
  (page-step semantics, reusing §4 paging), not teleport-to-position —
  teleporting on a misclick is a lose-your-place hazard. Click-and-hold
  repeats. Shift+click teleports for those who want it.
- Page-boundary ticks on the track when the document has ≤ ~200 pages
  (above that they merge into noise; the preview popup takes over).

## 6.2 Hover preview (the macOS-style page preview)

Hovering the track (after ~150 ms) or dragging the thumb shows a popup
adjacent to the pointer:

```text
┌──────────────┐
│  [thumbnail] │   page thumbnail at track position
│   Page 137   │   page number (+ nearest outline entry, later)
└──────────────┘
```

- The popup is chrome-layer damage only; it never invalidates the canvas.
- Thumbnails come from a dedicated **thumbnail cache**: `Draft`-quality
  renders at a fixed small width (~160 logical px), `Rgba8`, own byte budget
  (~32 MiB), populated by a background sweep at *lowest* scheduler priority
  plus an *elevated*-priority ring around the hover/drag position
  (hover position ± 5 pages), since that is where the pointer will land.
- Until a thumbnail exists, the popup shows the page number on a paper-color
  placeholder — the popup never blocks and never shows white (P4).
- Moving along the track re-targets the elevated ring; renders for departed
  positions are cancelled via `CancellationToken`.

Thumbnails are the seed of the later sidebar page list — same cache, second
consumer.

---

# 7. Blank-free rendering: the fallback ladder and request policy (P4)

## 7.1 What the renderer actually provides (confirmed API)

From `pdf-render-api` / `pdf-render-scheduler`:

- `RenderRequest { page: Arc<CompiledPage>, transform, crop: Option<DeviceRect>, output_size, output_format, background, annotations, quality, limits, residency }`
- `OutputFormat::Rgba8PremultipliedSrgb` (or `Gray8`); `Background::White`;
  `AnnotationMode::StaticAppearances` is the default (correct for a viewer).
- `RenderQuality::Draft` — reduced AA, fast image path: the preview tier.
- `RenderLimits.cancellation: CancellationToken` — cooperative cancel checked
  at operation boundaries.
- `HostPage { width, height, stride, format, pixels: Arc<[u8]> }` — immutable,
  refcounted: exactly the `Arc<PageSurface>` shape viewer.md §13 assumes.
- `RenderScheduler` (compile pool → render pool, memory permits, reordering)
  and `RenderTicket` (mpsc-backed, `try_wait`) for async submission.
- Compile and render are **separate stages**: `Arc<CompiledPage>` is the
  scale-independent artifact. Compiling is the expensive step; re-rendering
  a compiled page at a new scale is cheap. `crop` renders a sub-rect.

## 7.2 Surface tiers and the ladder

Per visible page, the compositor picks the best of:

```text
1. Exact    — current zoom bucket, Normal quality        → 1:1 blit
2. Stale    — other zoom bucket, Normal quality          → scaled blit
3. Preview  — Draft quality at ~0.5× current bucket      → scaled blit
4. Thumb    — thumbnail-cache entry                      → scaled blit
5. Skeleton — paper-colored rect, page border, centered
              page number                                → painter prims
```

Tier 5 is styled so that even the worst case reads as "a page that is
coming", not a rendering failure. With the renderer's speed, tiers 3–5
should be visible only during fast scrubbing of cold regions.

## 7.3 Request policy

Driven by scroll state + direction, evaluated per settled frame and per
overscan change (never per scroll pixel):

```text
priority 1  visible pages missing Preview           (Draft, fast)
priority 2  visible pages missing Exact             (Normal)
priority 3  overscan pages (directional) Preview
priority 4  overscan pages Exact
priority 5  hover-ring thumbnails (§6.2)
priority 6  background thumbnail sweep
```

- **Compile-ahead:** `Arc<CompiledPage>` is cached in its own budgeted cache
  (compiled pages are the expensive artifact and scale-independent —
  keeping them makes zoom re-render and tile render cheap). Compile requests
  extend one overscan ring further than render requests.
- **Viewport generations** (viewer.md §13) tag every request; completions
  carrying a stale generation for a *worse* tier than what is displayed are
  dropped; a stale-generation *Exact* for a still-visible page is kept
  (it is still the best stale tier).
- **Cancellation:** when a page leaves the extended overscan ring or the
  zoom bucket changes, its in-flight requests are cancelled through
  `CancellationToken`. The renderer checks at operation boundaries, so
  cancellation is prompt but not instant — the policy must tolerate late
  completions (they arrive as valid updates and are tier-compared normally).
- **Zoom:** during a zoom gesture, no render requests at all — geometry
  updates immediately, existing surfaces scale in the compositor
  (viewer.md §13). On settle: Exact requests for visible pages under a new
  generation.
- **Tiling at high zoom:** when `page_extent × zoom` exceeds a surface-size
  threshold (~1.5× a 4K frame), request only the viewport-intersecting
  region via `crop`, in fixed 512 px device-space tiles, keyed
  `(page, bucket, tile)`. Below the threshold whole-page surfaces keep the
  cache simple. This bounds worst-case surface memory at any zoom.

## 7.4 Pixel format bridging

Renderer output is `Rgba8PremultipliedSrgb`; softbuffer wants `0RGB` u32.
Pages render over `Background::White`, so alpha is 255 everywhere and the
conversion is a pure channel swizzle. Convert **once at surface ingestion**
(worker side, before the UI sees it) into the viewer's native `Vec<u32>`
surface so every subsequent blit is a straight row copy with no per-frame
conversion. `Gray8` (future e-ink/low-memory mode) expands in the same
ingestion step.

## 7.5 Tear-free presentation

- All composition happens into the retained `WindowBuffer`; softbuffer
  presents with explicit damage rects. Presentation is a single buffer
  handoff — the window never shows a half-composed frame.
- Platform honesty: Wayland and macOS are compositor-synced; X11 without a
  compositor may tear on present regardless of what we do — accepted, not
  worked around (viewer.md §24 lists the escalation path: replace the
  presenter, not the UI).
- The scroll-blit fast path (viewer.md §12) keeps steady-scroll damage to
  two strips + scrollbar; full-canvas repaints happen only on tier upgrades
  of visible pages, which are themselves damage-bounded to the page's rect.

---

# 8. Renderer bridge — concrete shape

One `DocumentSession` per open document, living on the worker side of the
`EventLoopProxy` boundary (viewer.md §5–6):

```rust
pub struct DocumentSession {
    snapshot: Arc<DocumentSnapshot>,        // pdf-document
    scheduler: RenderScheduler,             // pdf-render-scheduler
    compiled: CompiledPageCache,            // Arc<CompiledPage>, budgeted
    text: TextPageStore,                    // worker-side, for line index + search
    // channels: requests in (from UI), ViewerEvent updates out
}
```

Updates flowing UI-ward (through the coalescing queue + `Wake` event of
viewer.md §6):

```rust
pub enum SessionUpdate {
    PageGeometry { page: PageIndex, size_pts: SizeF },   // drives layout index
    Surface(PageSurfaceUpdate),                          // tiered, keyed
    Thumbnail { page: PageIndex, surface: Arc<ViewerSurface> },
    LineIndex { page: PageIndex, index: Arc<PageLineIndex> },
    Outline(OutlineUpdate),
    Error(BackgroundError),
}
```

Requests flowing worker-ward are the priority classes of §7.3 plus
`CompileAhead(range)`, `BuildLineIndex(range)`, and `Cancel(generation)`.

Open handling: `DocumentSnapshot::open` / `open_with_password`; page count
and page sizes stream in as `PageGeometry` (layout index starts with
estimated uniform sizes from page 1 and refines — the scrollbar mapping
adjusts without moving the reading anchor).

## 8.1 DjVu and the provider boundary

The viewer never names PDF types outside the bridge. The bridge implements:

```rust
pub trait DocumentProvider: Send {
    fn page_count(&self) -> usize;
    fn request_page_size(&self, page: PageIndex);
    fn request_surface(&self, req: SurfaceRequest);   // tier, bucket, crop
    fn request_line_index(&self, page: PageIndex);
    fn request_thumbnail(&self, page: PageIndex);
    fn cancel_generation(&self, gen: u64);
}
```

`PdfProvider` (above) is the first implementation. **Reality check:**
`djvulibrust` today contains JB2/IW44 *encoders* only — a DjVu decode path
is a real future project, not a wiring task. The trait exists now so that
project changes nothing in the viewer; DjVu OCR word boxes map naturally
onto `PageLineIndex`, so line-paging will work on DjVu from day one of that
integration.

---

# 9. Fullscreen (P5's one concession)

- `F11` / `⌘⌃F` toggles winit borderless fullscreen on the current monitor.
- Toolbar, sidebar, and status strip hide; the scrollbar remains (P2 — the
  document map must survive fullscreen). Reveal chrome on pointer-to-top-edge
  with a short delay, hide again on leave.
- The reading anchor (§4.4) restores position across the toggle; the layout
  reflows for the new canvas size through the normal resize path — no
  special fullscreen layout mode.

---

# 10. Build order — fundamentals track

This refines viewer.md §21's phases 0–5 with the subsystems above. Phases
0–2 (contracts, window + presenter, painter) are unchanged. From Phase 3:

**Phase 3 — synthetic virtual document** *(viewer.md §21.3, extended)*
Add to the existing scope:
- Geometric fallback paging (§4.3) as the first PageUp/PageDown.
- Synthetic *line boxes* on synthetic pages, and line-continuity paging
  (§4.1) driven by them — the paging algorithm is fully testable without a
  renderer, including page-gap crossing, no-line fallback, and end clamping.
- Reading anchor + restore across synthetic resize/zoom.
- Scrollbar with absolute drag, track paging, ticks, and the preview popup
  showing synthetic placeholder thumbnails.
- Status readout with visible-percentage.
- Exit gates additionally require: paging is line-exact against synthetic
  ground truth; anchor restore error ≤ 1 physical px across zoom round-trip;
  thumb drag latency (input → present) within one frame at 120 Hz.

**Phase 4 — renderer bridge** *(viewer.md §21.4, extended)*
- `DocumentProvider` trait + `PdfProvider` over `DocumentSnapshot` +
  `RenderScheduler`.
- Surface tiers, ladder compositor selection, request policy, generations,
  cancellation, format conversion at ingestion.
- Compiled-page cache with byte budget; unified budget accounting with the
  surface + thumbnail caches.
- Exit gates: cold-open of a 500-page PDF paints first content < 150 ms;
  scrubbing the full scrollbar of a warm document never presents tier 5;
  cancelled work provably stops (scheduler queue depth returns to zero after
  a scrub burst).

**Phase 4.5 — real line index** *(new)*
- Worker-side `TextPage` build + line clustering (§3.2), `LineIndex`
  updates, prefetch policy, fallback interplay.
- Arrow-key fine step from median line height.
- Debug overlay rendering line boxes (reuses the damage/diagnostics overlay
  of viewer.md §20) — clustering cannot be tuned blind.
- Exit gates: line-paging correct on a 20-document check corpus including
  two-column academic PDFs, scanned+OCR hybrids (fallback engages), rotated
  pages; index build never appears in UI-thread profiles.

**Phase 5 — zoom & navigation** *(viewer.md §21.5, unchanged scope)* with
the reading anchor as the restore mechanism for every operation listed
there, plus fullscreen (§9).

Everything later (chrome, text/search UI, kinetic polish, semantic overlays,
GPU presenter) proceeds as in viewer.md. Nothing in this document requires
reordering it.

---

# 11. Decisions resolved (previously open)

| Decision | Resolution |
|---|---|
| Layout modes | One: continuous vertical, fit-width default. Fit-page is a command, not a mode. |
| PageUp/Down semantics | Line-continuity paging (§4.1); geometric fallback `viewport − clamp(8%, 24..96 px)`. |
| Current page definition | Page containing viewport vertical midpoint. |
| Scrollbar style | Always-visible custom bar; absolute thumb drag; track click = page step (Shift = teleport). |
| Preview popup data | Dedicated Draft-quality thumbnail cache, ~160 px wide, own budget, hover-ring priority. |
| Pixel path | `Rgba8PremultipliedSrgb` over white → swizzle to `0RGB` u32 once at ingestion. |
| High-zoom memory | Whole-page surfaces below ~1.5× 4K frame; 512 px `crop` tiles above. |
| DjVu | Behind `DocumentProvider` now; decode path is a separate future project (djvulibrust is encode-only today). |
| Paging animation | None by default; instant re-anchor. |
| Wheel easing | None; raw deltas, `f64` accumulation, platform momentum only. |

---

# 12. Risks to watch

- **Line clustering quality** is the only algorithmically uncertain piece
  (multi-column, drop caps, math, marginalia). Mitigated by: worker-side
  isolation, the geometric fallback always working, the debug overlay, and
  Phase 4.5's check corpus. Ship fallback-only if clustering stalls; the
  architecture doesn't change.
- **Softbuffer throughput at 4K/120 Hz** — the scroll-blit path plus damage
  presentation is the design answer; viewer.md §24 defines the measured
  criteria for escalating to a native/GPU presenter. Measure in Phase 3,
  not later.
- **Page-size streaming vs. layout stability** — refining page sizes moves
  document-space coordinates under the reader. The anchor (§4.4) must be
  applied on every layout rebuild from geometry updates, and the scrollbar
  mapping must interpolate, or early scrolling in a large cold document will
  visibly jump. Treat as a first-class Phase 4 test case, not an edge case.
