# Lege Viewer Development Stages

Status: authoritative roadmap  
Last reconciled: 2026-07-25

This document is the single source of truth for the viewer's development
stages, their order, and their completion criteria. The older planning
documents remain design references:

- `viewer.md` defines the original platform, event-loop, software-presentation,
  scrolling, and diagnostic constraints.
- `fundamentals-plan.md` defines the reader-first behavior and the line,
  anchor, visibility, and request models.
- `blank-slate-architecture.md` defines the tight renderer/viewer integration,
  tile/conductor architecture, and renderer-aware feature ideas.
- `expanded-advice.md` and `seek-scan-optimization.md` define and record the
  seek/scan optimization work.
- `README.md` and `IMPLEMENTATION_MAP.md` describe the implementation as it
  exists.

Those documents used overlapping “phase” numbers and were written at
different points in the design. Their feature descriptions remain useful, but
their build orders are historical where they conflict with this file.

## Roadmap rules

1. A required stage must deliver reader-visible capability, necessary
   reliability, or an architectural prerequisite for either.
2. A performance mechanism is promoted into the required path only after a
   representative benchmark attributes enough time to the problem it solves.
3. CPU rasterization and software presentation remain the correctness
   references. Optional acceleration may not make document support or viewer
   behavior backend-dependent.
4. The renderer and viewer are developed together. Semantic pages, text,
   compiled IR, links, destinations, and raster tiles are shared working
   artifacts rather than a black-box bitmap boundary.
5. “Usable” means release-build behavior on real documents. Synthetic and
   unit tests are gates, not substitutes for corpus and hands-on testing.
6. A stage is substantially complete when its core behavior and failure paths
   work. Cosmetic polish can remain unless the cosmetic defect obscures state
   or interferes with reading.

## Current position

Stages 0 through 5 and the first seek/scan optimization pass are substantially
complete. Stage 6 is in progress: the movement model, the pointer and touch
gestures that feed it, and trace replay are done; page snapping and the
scrollbar-track refinements are not.

| Stage | Result | Status |
|---|---|---|
| 0 | Workspace and architecture contracts | Complete |
| 1 | Usable real-document viewing core | Complete |
| 2 | Scheduling, caching, and presentation core | Complete |
| 3 | Text and reader tools | Complete |
| Optimization A | Predictive seek/scan and adaptive viewing | Complete |
| 4 | Semantic navigation and link peek | Complete |
| 5 | Renderer-aware display | Complete |
| 6 | Movement and interaction completion | In progress |
| 7 | Opening, recovery, and document reliability | Planned |
| 8 | Accessibility and platform integration | Planned |
| 9 | Product polish and optional reader extensions | Later |

GPU compositing and GPU page rendering are deliberately absent from the
required-stage column. Their placement is defined under “Optional,
evidence-gated acceleration.”

## Completed foundation

### Stage 0 — Workspace and architecture contracts

Delivered:

- the viewer as a member of the Lege workspace beside `lege-pdf/render`;
- a single application-specific crate rather than a general widget toolkit;
- backend-neutral scene and presenter boundaries;
- document, viewport, layout, tile, cache, conductor, and text seams;
- the software presenter as the permanent reference path;
- synthetic-document and architecture-test scaffolding.

Exit condition: the renderer can evolve alongside the viewer without copying
renderer source into the viewer or reducing it to a finished-bitmap service.

### Stage 1 — Usable real-document viewing core

Delivered:

- a `winit` event loop and on-demand redraw;
- real PDF opening, page discovery, and continuous virtual layout;
- asynchronous compilation and rasterization;
- 256×256 tile display with generation tracking and stale-result rejection;
- fit-width/fit-page viewing, scrolling, paging, zoom, and fullscreen;
- a 10,000-page synthetic reference document;
- crash isolation at the page boundary.

Exit condition: a real document opens, renders, and can be read and navigated
without the UI thread waiting for renderer work.

### Stage 2 — Scheduling, caching, and presentation core

Delivered:

- visible-first viewport intent and cancellable worker queues;
- draft, final, neighboring-scale, thumbnail, structural, and skeleton
  fallback tiers;
- bounded CPU/GPU memory accounting and navigation-aware eviction;
- damage tracking, scroll reuse, and immutable frame tile snapshots;
- a software presenter and an automatically-falling-back WGPU presenter;
- reading anchors, history, scrollbar dragging, and hover page previews.

The existence of the WGPU presenter is an implementation fact, not a
requirement imposed on later stages. Stage 2's required result is the
backend-neutral scene plus a correct software presenter. The WGPU path may
remain the default on machines where it works because current interaction is
good, while software remains a fully supported fallback and test oracle.

Exit condition: rendering, caching, movement, and presentation are independent
enough that slow page work never stalls direct manipulation and presenter
failure does not prevent reading.

### Stage 3 — Text and reader tools

Delivered:

- one retained semantic/text/compiled artifact set per page;
- a compact, bounded whole-document search index;
- cancellable search with result navigation and document-map marks;
- renderer-backed selection, word/line selection, multi-page copy, and native
  clipboard integration;
- embedded and synthesized contents;
- glyph-backed viewer chrome;
- text-line paging and geometric fallback paging.

Exit condition: search, selection, copy, contents navigation, and paging use a
shared text/line substrate rather than independent approximations.

### Optimization A — Predictive seek/scan and adaptive viewing

Delivered:

- navigation-mode tracking for forward, backward, jump, skimming, and idle
  behavior;
- visible-first work, directional final-quality prefetch, a wider preview
  ring, and low-priority document sweep;
- structural previews for cold text pages;
- preview-only scrollbar skimming and high-quality refinement after motion
  settles;
- line-continuity Page Up/Down with notional page rows when text is absent;
- automatic proportional fit that scales pages up or down for the current
  viewport while retaining centered alignment.

This is a cross-stage optimization checkpoint rather than a new feature
family. Its governing references are `expanded-advice.md` and
`seek-scan-optimization.md`.

Exit condition: sequential reading and long seeks usually land on prepared
content, cold destinations show the best truthful fallback available, and
direct manipulation is not held for final raster.

### Stage 4 — Semantic navigation and link peek

Delivered:

- one-pass PDF link annotation extraction;
- direct, named, name-tree, and legacy internal destinations;
- crop- and rotation-correct internal target placement;
- safe external HTTP, HTTPS, and mail links;
- internal jump history;
- pointer feedback;
- predictive target warming and delayed link preview rendered off-screen.

Exit condition: internal and external links behave as document navigation,
with internal destinations ready or visibly progressing before the reader
commits to the jump.

## Required next stages

### Stage 5 — Renderer-aware display

Status: substantially complete (2026-07-25).

Implemented through compiled-IR painted bounds, anchored page-local crop
layout, crop/palette-aware tile and preview identities, renderer-lowered Night
and Warm Paper policies, toolbar/keyboard controls, and viewer-wide persisted
settings. GPU page rendering and GPU-specific recoloring remain outside this
stage.

Purpose: deliver the first remaining features that specifically justify
developing the viewer and renderer without a black-box boundary.

Implement in this order:

1. Content-aware margin trim.
   - Derive each page's content extent from compiled IR or renderer geometry,
     not a screenshot heuristic.
   - Fit the content box while keeping the reading anchor stable.
   - Handle mixed page sizes, rotations, empty pages, annotations, and
     intentionally full-bleed pages.
   - Include trim state and effective crop in layout, raster, preview, and
     cache identities.
2. Renderer-level night and warm-paper policies.
   - Transform vector/text fill and stroke colors before raster output.
   - Preserve images by default; any image dimming is an explicit independent
     policy.
   - Preserve text antialiasing, transparency semantics, links, selection,
     search highlights, and chrome contrast.
   - Include color policy in raster and preview cache identities.
3. Persistence and transitions.
   - Keep the selected display policy stable across resize, zoom, navigation,
     and document reopen as appropriate.
   - Reuse old truthful tiles during transition only when their visibly
     different policy cannot be mistaken for final output.

Exit gate:

- trim and color policy are derived through renderer data, not post-hoc whole
  frame image manipulation;
- changing either takes effect without blocking the UI thread;
- anchors survive mode, resize, and zoom changes;
- no stale tile with the wrong crop or color policy is promoted as exact;
- representative vector, text, image, transparency, rotated, empty, and
  scanned pages remain correct.

### Stage 6 — Movement and interaction completion

Status: in progress (2026-09-04).

Purpose: complete the one-scroll-model interaction vocabulary without changing
the already-good direct paging and settle behavior.

Done:

- **Drag-to-pan and middle-mouse autoscroll.** The middle button presses into
  a 1:1 grab-pan; released without moving inside 400 ms it promotes to
  autoscroll anchored at the press, steered by pointer displacement through a
  dead zone and a quadratic speed curve. The anchor is drawn on the canvas —
  an invisible mode that moves the document on its own is not acceptable — and
  is cancelled by any wheel, any other click, focus loss, or a DPI change,
  because a physical-pixel anchor stops meaning anything after a scale change.
- **Configurable wheel scaling.** `MovementTuning` carries the wheel line
  distance, a single scale over every wheel and touchpad delta, and the
  kinetic preference. It is persisted in the viewer settings file (version 2;
  version 1 files load with defaults) and sanitized on load, since the file is
  hand-editable and a zero or a NaN there would freeze every scroll. Drag and
  touch panning stay 1:1 and deliberately ignore the scale.
- **Kinetic movement.** A released drag or touch flick launches momentum at
  the model's time-smoothed velocity, decays it exponentially, and stops at
  the document edge or below a visible-motion threshold. It is off by default
  for pointer drags — momentum on a mouse reads as lag — and always on for
  touch, where its absence reads as a broken surface. Any direct input
  cancels it immediately. A long stall integrates at most one slow frame, so
  a suspended machine cannot teleport the document.
- **Touch panning** through `WindowEvent::Touch`, one finger at a time.
- **Deterministic trace recording and replay.** Every movement source funnels
  through one recording helper, so a trace is complete by construction.
  `LEGE_INPUT_TRACE=<path>` writes a JSON trace on exit; replay steps a
  simulated clock at the event loop's own 8 ms animation cadence rather than
  reading wall time, so a trace containing a fling reproduces the same final
  position on any machine. Paging is explicitly not recordable: its meaning
  lives in line sets, not in the scroll model.

Remaining:

- page snapping as an opt-in command rather than a second layout mode;
- refined track click and thumb capture behavior;
- fine movement commands beyond the existing arrow-key line step.

This stage should be scoped by actual personal use. Features that add no value
to the intended viewer can be omitted rather than implemented for parity.

Exit gate:

- every movement source feeds the same `f64` scroll model — met: pointer,
  wheel, touch, autoscroll, momentum and the scrollbar thumb all apply
  `ScrollCommand` to the one model;
- Page Up/Down behavior established in Optimization A is unchanged — met:
  `PageStep` is still resolved by `paging.rs` and was not touched;
- direct manipulation remains immediate during raster activity;
- replayable traces produce the same final anchor and viewport — met for the
  scroll model; anchor-level replay is not yet asserted.

### Stage 7 — Opening, recovery, and document reliability

Purpose: make the usable core dependable across the documents it claims to
open before spending time on visual polish.

Implement:

- encrypted-document password flow using the renderer's existing security
  support;
- clear handling of malformed, partially supported, and page-local failures;
- file reload/reopen behavior that invalidates generations and caches safely;
- empty, zero-page, huge-page, mixed-size, rotated, image-only, and
  transparency-heavy corpus cases;
- actionable status for degraded lowering or unsupported content;
- release-build soak tests for seek, zoom, resize, suspend/resume, and repeated
  open/close.

Exit gate:

- a bad page cannot crash or silently blank the document;
- encrypted documents can be opened without command-line workarounds;
- reload and failure recovery cannot display stale content from another
  generation;
- the release corpus passes without UI-thread stalls or unbounded memory.

### Stage 8 — Accessibility and platform integration

Implement only through narrow platform boundaries:

- accessibility tree and screen-reader roles;
- complete keyboard traversal and focus indication;
- native file-open integration and file associations;
- window position/state restoration and recent-document integration;
- platform clipboard/IME edge cases not covered by the reader core.

Exit gate: the document, controls, search, selection, links, contents, and
status can be operated without a pointer, and platform integration does not
leak into document or rendering policy.

### Stage 9 — Product polish and optional reader extensions

This is intentionally after the functional and reliability stages:

- visual chrome refinement, tooltips, menus, splitters, and theme polish;
- optional OCR for genuinely image-only pages;
- optional reflow, e-ink profile, DjVu provider, annotations overlay, or
  printing work;
- any additional reading feature supported by actual use.

Each extension needs its own scope and exit gate. Stage 9 is not permission to
grow a generic GUI toolkit or add modes without a reader need.

## Benchmark finding: what GPU work can and cannot address

The renderer corpus under
`../lege-pdf/render/corpus/perf/results/` and its consolidated records under
`../lege-pdf/render/docs/refinement/performance-history/` measure page
compilation, decode, sampling, glyph/path raster, masks, and page-surface
composition. They do **not** contain a controlled WGPU-versus-softbuffer
viewer presentation benchmark.

The viewer has `FrameMetrics` fields for compose/present time and GPU resource
diagnostics, but there is no saved representative A/B series in the current
test records. Therefore neither GPU compositing nor software presentation can
honestly be declared faster from the existing benchmark corpus.

The measured renderer attribution does establish where page-readiness time
has been going:

| Representative, pre-optimization | Compiled | Warm decoded | Decode share | Main implication |
|---|---:|---:|---:|---|
| JPEG scan, viewer scale | 473.896 ms | 321.916 ms | 32.1% | Decode matters, but most time remained in destination rendering |
| JPEG scan, sweep scale | 1588.455 ms | 1432.637 ms | 9.8% | Raster/sampling dominated overwhelmingly |
| JPX scan | 523.678 ms | 223.110 ms | 57.4% | Both codec work and many-tap image shading mattered |
| JPX/JBIG2 MRC | 597.972 ms | 305.145 ms | 49.0% | Roughly half decode, half sampling/composition |
| CCITT bilevel | 242.339 ms | 244.109 ms | effectively 0% | Generic minification and mask composition dominated |
| Latin text | 25.747 ms | 25.043 ms | 2.7% | Glyph work, not image decode |
| Type 1 text | 111.385 ms | 110.830 ms | 0.5% | Font preparation and glyph work |
| Vector diagram | 10.878 ms | 10.693 ms | 1.7% | Fills, paths, and clipping |

Later CPU-side changes produced large improvements in exactly those attributed
areas:

- glyph caching improved the Latin representative from 20.54 ms to 3.17 ms
  and Type 1 from 9.62 ms to 4.78 ms;
- production minified JPX fell from 142.66 ms to 66.76 ms and MRC from
  181.29 ms to 100.31 ms after decode/integration work;
- CCITT row-band mask composition fell from 48.74 ms to 30.16 ms;
- another JPEG decoder pass cut decode by 42.8%, but total compiled time by
  only 11%, confirming the remaining page-render cost;
- whole-document glyph caching raised throughput from 131.7 to 145.1 pages/s,
  while the remaining gap to PDFium's measured 238 pages/s was attributed to
  non-text work such as the page-zero image and compile pipeline.

The consolidated evidence for those figures is:

- [`remaining-20260719.md`](../lege-pdf/render/docs/refinement/performance-history/remaining-20260719.md)
  for the paired decode attribution and whole-document baseline;
- [`optimization-glyphcache-20260720.md`](../lege-pdf/render/docs/refinement/performance-history/optimization-glyphcache-20260720.md)
  for text, Type 1, and whole-document glyph-cache results;
- [`optimization-jpx-integration-20260720.md`](../lege-pdf/render/docs/refinement/performance-history/optimization-jpx-integration-20260720.md)
  for JPX/MRC decode integration;
- [`optimization-renderer3-20260720.md`](../lege-pdf/render/docs/refinement/performance-history/optimization-renderer3-20260720.md)
  for CCITT mask composition and document font caching;
- [`optimization-jpeg3-20260720.md`](../lege-pdf/render/docs/refinement/performance-history/optimization-jpeg3-20260720.md)
  for the later JPEG decoder attribution.

Here “composition” in the CCITT and MRC records means compositing image or mask
content **inside the CPU page renderer**. It is not the viewer's final act of
placing completed page tiles into a window. A GPU screen compositor cannot
remove JPEG entropy decoding, JPX decoding, font parsing, glyph outline work,
CPU path rasterization, or CPU image sampling that has already occurred before
a tile is uploaded.

This confirms the proposed roadmap correction:

- GPU compositing is not a required viewer feature and is not a demonstrated
  page-rendering optimization.
- GPU rendering is not automatically faster. It is potentially useful for
  sufficiently large, parallel raster workloads, but dispatch, tile upload,
  synchronization, fallback, and readback can erase gains—especially while
  codecs and font preparation remain on the CPU.
- A GPU compositor becomes strategically stronger if page output is already
  GPU-resident, because that can avoid a CPU-tile upload. That possible future
  pairing does not justify making either backend a present requirement.

## Optional, evidence-gated acceleration

### G1 — GPU compositor: implemented option, no required stage

The current WGPU compositor can be retained and maintained because it already
provides fractional transforms, temporary zoom scaling, overlays, and an atlas.
It must not gate any required stage, remove the software path, or be described
as accelerating PDF rendering without a controlled result.

Revisit or expand it only if a release-build A/B benchmark shows one of these
on target hardware:

- software compose/present p95 consumes a material part of a 60/120/144 Hz
  frame budget;
- presentation copies or scaling dominate interaction frames;
- overlay composition causes missed frames;
- a future renderer produces GPU-resident page tiles and avoiding readback plus
  re-upload gives an end-to-end win.

The A/B must use the same document, viewport, input trace, damage sequence,
quality tier, and cache state. Report input-to-first-pixels,
input-to-exact-pixels, compose p50/p95/p99, present p50/p95/p99, CPU time,
uploads, bytes moved, power where practical, and fallback correctness.

If no meaningful improvement appears, the WGPU path remains an optional
implementation rather than receiving additional roadmap work.

### G2 — GPU page renderer: post-reliability research track

GPU page rendering belongs after Stage 7, not between reader features, unless a
specific earlier stage exposes a measured blocker that only it can solve.

Before implementation:

1. Re-profile the release renderer on the actual viewer corpus and target
   viewport/tile sizes.
2. Separate decode, compile/font preparation, image sampling, glyph masks,
   path fill/stroke, clipping, transparency, and final page composition.
3. Choose only GPU-suitable kernels with enough repeated parallel work.
4. Include upload, synchronization, fallback, and any readback in the
   end-to-end comparison.
5. Require pixel/correctness parity and retain CPU execution for unsupported
   operations and devices.

Likely experimental candidates are large image resampling, repeated mask
operations, fills/clips, or a backend-resident tile pipeline. JPEG/JPX codec
work, PDF interpretation, font parsing, and small irregular pages should not
be assumed to benefit. A hybrid backend is acceptable only if measured
end-to-end latency or throughput improves without weakening determinism.

Promotion rule: GPU rendering becomes a supported accelerator only after it
wins on representative release workloads and never becomes a prerequisite for
correct viewing. Until then it is research, not Stage 5 or Stage 6 work.

## Continuous performance work

Performance refinement is not deferred until the optional GPU tracks. Small,
measured CPU and integration improvements may be made between any required
stages when they address a demonstrated user-visible delay.

The current priority order is:

1. preserve prediction, cancellation, and off-screen preparation behavior;
2. reduce renderer work actually attributed by the corpus;
3. reduce duplicate compile/decode/raster work across tile requests;
4. consider tile-run or band rendering where it removes repeated setup;
5. measure viewer composition/presentation before changing presenter policy;
6. investigate GPU kernels only after the remaining CPU attribution justifies
   them.

Every optimization must report release-build end-to-end behavior and preserve
the exactness, fallback, and memory gates of the stage it touches.
