# Unifying `lege-process`, `lege-viewer`, and a file manager into one application

Feasibility and aesthetics assessment — 2026-07-26

Scope: is it practical to fold the Freya processing GUI into the from-scratch
viewer, drop Freya, and leave room for an integrated file manager — all three
sharing the renderer and the existing `lege` processing library, with per-page
processing and preview?

Verdict up front: **feasible, and the architecture is already most of the way
there.** The merge is not blocked by the processing side, the renderer, or the
GPU. It is blocked by exactly one thing: **the viewer has no retained UI layer.**
Its chrome is hardcoded pixel arithmetic that will not carry a 45-field settings
surface. That gap is real but bounded, and — this is the important finding — the
thing that fills it already exists in the Freya fork, decoupled from Freya.

---

## 1. What the audit found

### 1.1 The two GUIs are far less entangled than they look

`lege-process/GUI/Freya/src` is 8,637 lines. Framework coupling:

| Module | Lines | `freya` references |
|---|---:|---:|
| `app.rs` | 3,567 | many |
| `widgets.rs` | 345 | 1 |
| `main.rs` | 105 | 3 |
| `sanzowada.rs` | 1,361 | **0** |
| `worker_process.rs` | 1,116 | **0** |
| `backend.rs` | 565 | **0** |
| `appearance.rs` | 430 | **0** |
| `models.rs` | 421 | **0** |
| `gui_text.rs` | 217 | **0** |
| `settings.rs` | 109 | **0** |
| `colors.rs` | 110 | **0** |
| `logging.rs`, `version.rs` | 157 | **0** |

**4,620 lines (53%) are framework-independent and move across unchanged.** That
includes the entire Sanzo Wada theme system, `ProcessingOptions` (~45 fields),
the CLI-argument translation layer, the subprocess supervisor, the i18n text
table, and settings persistence. Only `app.rs` + `widgets.rs` + `main.rs`
(~4,017 lines) are actually Freya view code.

### 1.2 The GUI barely uses Freya's widget library

Widget-type usage across all of `app.rs`:

```
TooltipArea  29    Button  16    Input  7    ScrollView  4    Popup  ~5
```

Everything else — `compact_choice_button`, `bool_tile`, `compact_checkbox_row`,
`panel_card`, `progress_stage_card`, `settings_row`, the theme chooser, the
queue rows, the log viewer — is already custom-drawn on top of `rect` + `label`
+ flexbox. The design is homegrown; Freya is supplying **layout, text, painting,
and five widgets**, not a look.

That reframes "drop Freya" from *rewrite the GUI* to *replace four services*:
flexbox layout, text shaping/paint, a vector rasterizer, and five controls.

### 1.3 Three of those four services are already vendored, Freya-independent

In `freya-main/crates`:

| Crate | Lines | External deps | Freya deps |
|---|---:|---|---|
| `freya-render-api` | 363 | **none** | none |
| `freya-render-tiny-skia` | 1,032 | `tiny-skia` | render-api only |
| `torin` (layout engine) | 8,036 | `euclid`, `rustc-hash`, `itertools`, `tracing` | **none** |
| `ragnarok` (event routing) | 1,803 | `euclid`, `rustc-hash`, `itertools` | **none** |
| `freya-text-cosmic` | 338 | `cosmic-text` | render-api only |

`freya-core` (14,338) and `freya-components` (14,650) are the parts you are
dropping. The drawing/layout/event substrate is ~11.6k lines with four trivial
dependencies and **no coupling to Dioxus, Skia, or Freya's reactive model.**

And note `lege-gui` already builds with `default-features = false,
features = ["cpu-renderer"]` — **the production GUI is already rendering through
`freya-render-tiny-skia`, not Skia.** Dropping Freya loses no rendering
capability at all. There is nothing to preserve by special effort; the tiny-skia
renderer is simply a crate you keep and the rest is a crate you stop depending on.

### 1.4 The viewer's UI layer is the actual gap

`lege-viewer` is 14,207 lines and genuinely good where it counts — the document
engine, conductor, tile cache, scroll model, text/search/selection, and the
dual software/wgpu presenter are all real. But the UI layer is a placeholder:

`src/scene.rs` — the entire drawing vocabulary:

```rust
pub enum SceneCommand {
    Solid      { rect: RectI, clip: RectI, color: u32 },
    AlphaSolid { rect: RectI, clip: RectI, color: u32 },
    Image      { tile: Arc<TileSurface>, .. },
    Surface    { surface: Arc<SceneSurface>, .. },
}
```

Axis-aligned solid fills and blits. No paths, no rounded rectangles, no strokes,
no gradients, no anti-aliasing, no transforms.

`src/app.rs:112` — the entire hit-testing system:

```rust
fn toolbar_action_at(x: f64) -> Option<ToolbarAction> {
    match x {
        x if x < 34.0  => Some(ToolbarAction::ZoomOut),
        x if x < 68.0  => Some(ToolbarAction::ZoomIn),
        x if x < 146.0 => Some(ToolbarAction::FitWidth),
        ...
    }
}
```

Magic-number x-ranges, hand-synchronized with magic-number paint offsets 2,000
lines away in `render_chrome_surfaces`. This works for nine toolbar buttons. It
does not work for `ProcessingOptions`' ~45 fields, a queue list, six popups, a
progress card, a log viewer, and a file-manager pane. `app.rs` is already 3,344
lines and ~40% of it is chrome bookkeeping.

**This is the crux: the merge is gated on giving the viewer a layout + widget
layer, not on the merge itself.** Attempting to port the settings dashboard onto
the current chrome model would produce something unmaintainable within a week.

### 1.5 The processing side is ready

- `lege` is already a **library** (`core/lib.rs`) with a broad public surface,
  not just a binary.
- It already depends on `lege-pdf-read` (renderer-backed) — **pdfium is gone**.
  The viewer and the processor already sit on the same document stack. No dual
  parsing, no format skew.
- `lege-gpu` exposes a process-wide shared `GpuContext` via `OnceLock`
  (`vision/runtime/device.rs:223`). One wgpu device can serve the viewer's
  presenter, Sauvola binarization, resize, and YOLO inference. Today the
  subprocess model initializes the GPU **twice**; merging removes that.
- The per-page seam already exists as a pure function:
  `process_page_cpu_work(input: PageProcessingInput) -> Result<PageProcessingOutput>`
  (`pdf_tokio_pipeline.rs:623`). It is private, but it takes a struct and returns
  a struct. Making it `pub` is close to the whole API work for per-page preview.

### 1.6 The one real constraint on per-page processing

`PageProcessingInput` carries `margin_analysis: Option<Arc<DocumentMarginAnalysis>>`,
produced by `perform_document_analysis` — a **document-scoped Phase-1 low-res
pass over every page** that also builds the detection cache. Crop/center margins
are a whole-document decision by design; that is why Lege's output is consistent
across a book and Scantailor's is not.

So "process pages one by one" has two honest modes:

- **Draft preview** — `margin_analysis: None`, which falls back to
  `identity_margin_correction()` (`:538`). Instant, but crop/centering differs
  from the final output. Misleading if presented as final.
- **Exact preview** — run Phase 1 first (cheap, low-res, already cancellable),
  then every per-page preview is bit-identical to what the job will emit.

Recommendation: **run Phase 1 on open, in the background, and show draft
previews until it lands.** The user sees a page immediately, and it silently
becomes exact. This is not a workaround; it matches how Lege actually thinks
about documents, and it is the thing that keeps the merged app from drifting
toward Scantailor.

---

## 2. Recommended shape

### 2.1 Keep the subprocess boundary for *jobs*; go in-process for *previews*

The current architecture spawns `lege --gui-worker` per queue item and streams
JSON progress back (`worker_process.rs`, `lege-ipc`). Do not throw that away.
It buys crash isolation on adversarial PDFs, real cancellation via process kill,
and it keeps the Tokio multi-thread pipeline runtime out of the winit event
loop — a genuine impedance mismatch you do not want to litigate.

Split the two duties:

- **Batch jobs** (the 2–3-click path, the queue): subprocess, unchanged. This is
  the workflow that must not regress.
- **Live single-page preview**: in-process, on the shared `GpuContext`, calling
  `process_page_cpu_work` directly. Previews are interactive, bounded, and
  cheap; a subprocess round-trip per settings tweak would feel dead.

### 2.2 Build one small UI kit — vendored, not adopted

Move into the workspace as a new crate (say `lege-ui`):

- `freya-render-api` (363 lines, zero deps) — this becomes the drawing
  vocabulary the viewer is missing: rounded rects, paths, strokes, brushes,
  clips, affine transforms, image sampling.
- `freya-render-tiny-skia` (1,032 lines) — the CPU rasterizer, already the
  production path today.
- `torin` (8,036 lines) — flexbox-ish layout, so controls stop carrying literal
  pixel offsets.
- `ragnarok` (1,803 lines) — optional; adopt if hit-testing/gesture routing
  proves non-trivial, skip if a simple retained hit-tree suffices.
- Text: the viewer already drives `cosmic-text` directly in `ui.rs`. Keep that;
  `freya-text-cosmic` is redundant.

Then reimplement exactly five controls — button, text input, scroll view,
tooltip, popup — plus the custom cards the GUI already defines itself. Budget
roughly 2,500–4,000 lines for the kit plus controls.

**Integration is cheap.** Render all chrome with tiny-skia into a single Pixmap
and hand it to the viewer's existing `SceneCommand::Surface` path. The
compositor, damage tracking, scroll reuse, and both presenters stay untouched.
You are adding a chrome producer, not rewriting the frame pipeline.

Deliberately: this is an application UI kit, not a general toolkit. Same rule
`viewer.md` already sets for the viewer, and it is the right one.

### 2.3 Three surfaces, one window, one document engine

```
┌──────────────────────────────────────────────────────────────┐
│ toolbar: open · process ▸ · view controls · palette · search │
├────────────┬─────────────────────────────────┬───────────────┤
│  left rail │        document canvas          │  right panel  │
│            │   (existing viewer, untouched)  │   collapsed   │
│  files /   │                                 │   by default  │
│  outline / │   before | after split preview  │   settings    │
│  queue     │                                 │   when opened │
└────────────┴─────────────────────────────────┴───────────────┘
```

The file manager is not a third application — it is a **left-rail mode**
alongside outline and queue, backed by `backend.rs`, which already has
`get_pdf_files_in_directory`, `get_image_files_in_directory`, `is_pdf_file`,
`is_zip_file`, `calculate_path_size`, `truncate_path`, and
`open_folder_in_explorer`. Thumbnails come free from the viewer's existing
thumbnail tile tier. This is the cheapest of the three merges and should be
built last, after the kit proves itself on the settings panel.

### 2.4 Protecting the 2–3-click path

This is the part worth being stubborn about. Concretely:

- **Default posture is viewer.** Open a file, it reads. Nothing about processing
  is on screen.
- **One primary action.** "Process" with the current profile is a single click
  from the toolbar. Settings live behind a disclosure, not in the path.
- **Settings panel is a drawer, not a stage.** Collapsed by default; opening it
  does not change what the canvas is doing.
- **Preview is passive.** It updates when settings change; it is never a step
  the user must complete. No "click to preview," no per-page confirmation, no
  wizard.
- **No page-level required interaction, ever.** Per-page preview must stay a
  verification affordance. The moment a page needs a decision before the job can
  run, you are Scantailor.

A useful test to hold the line: *the click count from "app open" to "job
running" must stay at ≤3 for the default path, and no feature ships that raises
it.* Everything else in this document is negotiable; that is not.

---

## 3. Aesthetics assessment

**The two halves currently look nothing alike, and the viewer is the weaker one.**

The processing GUI has a considered visual identity: the Sanzo Wada palette
system (1,361 lines of historical color pairings), light/dark rows, live theme
preview, card-based panels, tooltips, staged progress. The viewer has three
hardcoded themes (`light`/`night`/`warm`), flat `u32` fills, 1px hairline
borders, no anti-aliasing, no corner radii, no elevation, and a toolbar that is
literally the string `"−   +    Fit width    Fit page"` blitted at x=10, y=10.

The good news: **the direction of travel is one-way and favorable.**

- `sanzowada.rs` + `colors.rs` + `appearance.rs` are pure data and pure
  functions with zero framework references. They port as-is and immediately
  upgrade the viewer from three themes to the full palette system.
- Adopting `freya-render-api` + tiny-skia gives the viewer anti-aliasing,
  rounded rects, strokes, and gradients on day one — the exact primitives its
  current chrome lacks and the settings panel requires.
- The viewer's `ThemeMetrics`/`ThemeColors` split is already the right shape;
  it just needs to be repointed at the Sanzo Wada source and widened (border
  radius, elevation, focus ring, accent, disabled states).

Aesthetic risks to name honestly:

1. **Two visual languages in one window.** A document canvas wants recessive,
   low-chroma, high-contrast-on-paper chrome. A settings dashboard wants
   legible, dense, differentiated controls. Resolve by *zone*: canvas chrome
   stays quiet and desaturated; the settings drawer is allowed to be denser and
   more colorful, but shares type scale, spacing units, and radii.
2. **The palette is the identity — do not lose it.** Sanzo Wada is the most
   distinctive thing about Lege's look. It should survive the merge as the
   theming source for the *whole* app, viewer chrome included.
3. **Theme count vs. reading modes.** The viewer's `ColorMode`
   (Original/Night/WarmPaper) affects *page rendering*; the GUI's themes affect
   *chrome*. These are different axes and must stay separate in the UI, or users
   will expect night mode to invert scans.
4. **Text quality is the tell.** `ui.rs` currently blends glyphs with a
   hand-rolled `blend_xrgb` and no gamma correction or subpixel positioning.
   At settings-panel density that will read as visibly cheap next to the current
   GUI. Budget real time for text rendering quality; it is the single highest
   perceived-quality-per-line-of-code item in the whole project.

Overall: the merged application can look **better than either half does today**,
because the viewer contributes the interaction model and the process GUI
contributes the visual system, and neither currently has both.

---

## 4. Risks and how they rank

| # | Risk | Severity | Mitigation |
|---|---|---|---|
| 1 | Viewer has no layout/widget layer; porting 45 settings onto pixel-offset chrome | **High** | Build `lege-ui` from vendored render-api + tiny-skia + torin *before* porting any panel |
| 2 | `app.rs` already 3,344 lines; merge triples chrome logic in one file | **High** | Split chrome into modules *first*; treat this as a precondition, not cleanup |
| 3 | Tokio pipeline runtime vs. winit event loop | Medium | Keep batch jobs subprocess; in-process previews on a dedicated runtime |
| 4 | Per-page preview ≠ final output before Phase 1 completes | Medium | Background Phase 1 on open; label drafts; upgrade silently |
| 5 | Feature creep toward Scantailor | Medium | Hard ≤3-click rule on the default path; preview stays passive |
| 6 | Text rendering quality gap | Medium | Gamma-correct blending + subpixel positioning; do it early |
| 7 | `tests/architecture.rs` (1,395 lines) encodes viewer boundaries | Low | Extend the contracts; they are an asset, keep them enforcing the layering |
| 8 | Crash isolation lost if previews go in-process | Low | Preview one page at a time, catch panics at the page boundary — the viewer already does this |
| 9 | Freya fork removal breaks the music-sheet GUI too | Low-Med | `lege-music-gui` is a separate Freya consumer; decide whether it migrates, stays on Freya, or is retired |

Risks 1 and 2 are the ones that decide whether this succeeds. They are both
"do the boring structural work first" risks, which is the good kind.

---

## 5. Sequencing

Ordered so that each step is independently valuable and the project is never in
a broken half-merged state.

1. **Split `lege-viewer/src/app.rs`.** Chrome, input routing, and document state
   into separate modules. No behavior change. Precondition for everything else.
2. **Create `lege-ui`.** Vendor `freya-render-api`, `freya-render-tiny-skia`,
   `torin` into the workspace. Route the viewer's existing chrome through it via
   `SceneCommand::Surface`. Ship it — the viewer alone gets anti-aliasing,
   rounded corners, and real layout. *Freya is still present; nothing is dropped
   yet.*
3. **Port the theme system.** `sanzowada.rs`, `colors.rs`, `appearance.rs` into
   `lege-ui`. Widen `ThemeColors`/`ThemeMetrics`. Both GUIs now share a palette.
4. **Build the five controls.** Button, input, scroll view, tooltip, popup —
   driven by the viewer's own needs (search field, outline list, link peek)
   before the settings panel needs them.
5. **Port the settings drawer.** `ProcessingOptions` UI onto `lege-ui`. Wire to
   the existing subprocess worker. **At this point `lege-gui` can be retired and
   Freya dropped from the workspace.**
6. **Make `process_page_cpu_work` public; add per-page preview.** Background
   Phase 1 on open, draft→exact upgrade, before/after split view.
7. **File manager as a left-rail mode.** On `backend.rs` + existing thumbnail
   tiles.
8. **Consolidate the GPU context.** One `GpuContext` for presenter, binarization,
   resize, inference. Do this once previews are in-process and the win is real.

Steps 1–5 deliver "one application, Freya gone" without touching the pipeline.
Steps 6–8 deliver what the merge is actually *for*.

---

## 6. Bottom line

Feasible, and better-positioned than it looks from the outside. The shared
renderer is done, pdfium is gone, `lege` is already a library, the GPU context
is already shareable, the per-page seam is already a pure function, and 53% of
the process GUI has no framework coupling at all. Dropping Freya costs almost
nothing because the GUI already runs on the fork's tiny-skia renderer and barely
touches Freya's widgets.

The one genuine piece of new construction is a small application UI kit — and
even that is mostly vendoring ~11.6k lines that are already in the tree and
already Freya-independent, plus five controls.

The thing most likely to go wrong is not technical. It is letting per-page
preview grow into per-page *approval*. Keep the preview passive, keep the
default path at three clicks, and the merged application is strictly better than
the three separate ones.
