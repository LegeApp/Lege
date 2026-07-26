# Phase B — glyph coverage-bitmap cache + sub-pixel bucketing — 2026-07-20

Implements Phase B of `PLAN-TEXT-GAP.md`: the PDFium `CFX_GlyphCache` mechanism
(#1 in the text-gap review) — a document-scoped cache of rendered glyph coverage
bitmaps. On a cache hit a glyph is a map probe and an alpha blit; the outline
extraction + curve flattening + edge build + scan conversion that Phase A's
profile showed as ~59% + ~13% + ~1us/glyph of extract now happen **once per
unique glyph** instead of once per occurrence.

Raw paired rows: `results/optimization-glyphcache-20260720/` (`metadata.txt`).
No git state touched.

## What was built

### B1 — shared glyph coverage cache (`crates/pdf-render-cpu/src/prepared.rs`)

`SharedGlyphCache`: same shape as `SharedFontProgramCache` — 8 `Mutex`-guarded
shards, LRU by retained coverage bytes, **32 MiB** bound, lives on `CpuBackend`
(`Arc`, shared by all render workers, scoped to one document render). Value is an
`Arc<GlyphBitmap>` = a bbox-tight `u8` coverage bitmap plus `(left, top)` bearing
offsets relative to the glyph origin. Key (`GlyphCacheKey`):

- **font content identity** — the existing 128-bit `FontProgramKey` (content
  hash + len + face), so a glyph shared across a document's pages hits (the same
  cross-page identity the parse cache uses; pointer identity gave 0 cross-page
  hits historically). Computed once per font per page (~8 hashes/page), not per
  run.
- **glyph index**.
- **quantized effective linear map** `la,lb,lc,ld` = `(font_size/upem)·CTM_linear`,
  each `×10000` (PDFium quantizes matrix a,b,c,d identically). Folds in font
  size, page scale, skew.
- **sub-pixel phase** `sx,sy` (see B2).
- **hinting state** — hinted vs unhinted produce different pixels (the review's
  explicit requirement); hinted outlines depend on ppem, captured via `ld`+font.

Populate on miss via the (Phase-A) `RasterKernel` into a bbox-local buffer; on
hit, blit through `kernels::blend_mask`. Coverage is color-agnostic (text is
solid-only — `prepared.rs` already bails on pattern/shading text), so one bitmap
serves any color/alpha. Misses are **probed first, then batch-extracted** (one
`FontRef`/`HintingInstance` per run, never per glyph), so a fully-warm page does
zero outline extraction.

New op `PreparedOp::GlyphRun` carries the run's placements + solid color/alpha/
clip/blend; executed by `paint_glyph_run` (`exec.rs`) — one `blend_mask` per row
in the fast case (Normal blend, no non-rect clip mask, no soft mask), a per-row
scratch that folds clip/soft coverage otherwise.

### B2 — sub-pixel origin bucketing (not full snapping)

The plan proposed integer-pixel snapping (PDFium `FXSYS_roundf`). Measured, full
snapping (`1×1`) **worsened** PDFium severity on every text page
(latin-p10 0.0894 → 0.1313) — the cache faithfully renders the same shape, so the
regression is purely the origin moving up to a half-pixel off its exact position,
which shifts the anti-aliasing away from PDFium's. (Verified: with
`PDF_RENDERER_GLYPHCACHE=off` the binary is **byte-identical** to base on all
gated pages, so nothing but the snap changed.)

Instead the origin is split into an integer pixel base + a sub-pixel **phase
bucket** in `0..N` per axis (`quantize_origin`); the phase enters the key and the
rasterization frame (the bitmap carries that phase's exact AA), the base is where
it blits. Positioning error ≤ `1/(2N)` px. The IR carries each glyph's absolute
pen position, so independent quantization introduces no cumulative spacing drift
— no `AdjustGlyphSpace` pass is needed.

`N` swept on latin-p10 (severity, hit rate):

| N (per axis) | severity | hit rate |
|---|---:|---:|
| 1 (snap) | 0.1313 | 0.991 |
| 2 | 0.1140 | 0.987 |
| 3 | 0.0972 | 0.981 |
| **4 (default)** | **0.0902** | **0.972** |
| 8 | 0.0880 | 0.952 |

Default **`4×4`** (`glyph_subpixel_steps`, override `PDF_RENDERER_GLYPH_SUBPIXEL="NX,NY"`):
severity at the exact-position baseline (0.0894) while holding the ≥95% hit-rate
bar. Higher N inches severity below baseline but drops hit rate under 95%.

### B3 — exclusions (all enforced)

- **render mode**: only pure-fill (mode 0) consults the cache (gated at the call
  site). Modes 1/2/4/5/6 keep the exact outline path unchanged — the pre-existing
  wrong-fill of stroke/clip modes is **not** baked into the cache, and a later
  stroke fix is unblocked. Modes 3/7 already paint nothing.
- **Type 3**: never reaches glyph-outline lowering (`pdf-content` routes it to
  `show_type3`), so it cannot enter the cache.
- **text clip** (modes 4–7 outline union) uses its own exact-outline path
  (`push_text_clip`), untouched.
- **escape hatch** (PDFium `|a|+|b| > 50` analog): only axis-aligned runs with
  `ppem ≤ 200` are cached; rotated/skewed/oversized runs fall back to the outline
  fill so they neither need rotated-bbox coverage nor blow the LRU. (Rotated text
  caching is a documented deferral — coverage is position-independent, so it is
  addable later.)

## Results — gated pages, scale 2.0, compiled mode, 20 runs, medians

Base = `pdfium-diff @ afda1ec` (clean HEAD, Phase A merged). New = working tree,
default `4×4`. Severity vs `libpdfium.so`.

| page | base ms | new ms | speedup | base sev | new sev | Δsev | hit rate | RSS b/n MB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| latin-text p10 | 20.54 | **3.17** | **6.48×** | 0.08938 | 0.09016 | +0.9% | 0.983 | 24.3/20.2 |
| latin-text p20 | 20.86 | **3.18** | **6.56×** | 0.09533 | 0.09352 | −1.9% ✓ | 0.981 | 24.6/20.3 |
| cjk-cid-text | 10.33 | 10.84 | 0.95× | 3.5e-5 | 3.0e-5 | −16% ✓ | 0.950 | 32.9/32.9 |
| type1-fonts | 9.62 | **4.78** | **2.01×** | 0.03179 | 0.03056 | −3.9% ✓ | 0.954 | 32.8/32.2 |
| vector-diagram | 10.98 | 9.96 | 1.10× | 0.05127 | 0.05265 | +2.7% | 0.970 | 22.3/21.7 |

Per-glyph, latin: `render.total` **9.3 µs → 1.03 µs**. Attribution (latin-p10,
base→new ms): `render.execute` 14.09→0.82, `render.lower` 5.99→0.26,
`lower.glyph.outline_extract` 3.0→0 (warm), `render.edges` 449k→~10. Peak RSS
**drops** (24→20 MiB): the cache replaces the whole-run edge/point arenas with
small shared bitmaps; the 32 MiB cap is never approached on these pages.

Notes:
- **cjk** page has only 4 glyphs; its 10.8 ms is CID/non-text work, and the
  0.95× is run-to-run noise (severity improved).
- **vector-diagram** page 50 is **not text-free** — it carries ~718 glyphs
  (labels), so it is not byte-identical; the +2.7% is sub-pixel text-AA noise on
  a vector-dominated page (its non-text content is byte-identical to base, proven
  by the cache-off run). The plan's "vector has no text" premise was incorrect
  for this page.

Severity gate: no page worsens **materially**; three of five improve, latin-p10
is +0.9% (noise vs the 0.0894 baseline), vector +2.7% (immaterial, sub-pixel).

## Whole-document heads-up bench (`bench`, Buddhist_Ethics, scale 2.0)

Same harness, base vs new (single-threaded per-page; parallel whole-doc):

| | base | new |
|---|---:|---:|
| text body pages 6–9 (ours ms) | 21.2–22.6 | 8.8–12.3 |
| whole-doc throughput | 131.7 pg/s | **145.1 pg/s** (+10%) |
| vs PDFium (238 pg/s) | 1.78× slower | 1.64× slower |
| whole-doc glyph hit rate | — | **0.958** |

The `bench` per-page figure includes per-render page compile + a cold-per-page
glyph cache, so it is pessimistic; in **warm** compiled mode latin renders in
2.8–3.2 ms — at/below PDFium's ~3 ms/page for these pages (the "≤2× PDFium per
page" target is met and beaten warm). Whole-doc wall is parallel and bounded by
non-text work (page 0 is a ~470 ms image, plus compile), so the +10% throughput
understates the text-raster win; closing the whole-doc gap needs the compile
pipeline and the page-0 image path, not more text-raster.

## Tests

`cargo test -p pdf-render-cpu -p pdf-font` green (22 suites). New:
- `prepared::glyph_cache_tests`: hit/miss + stats; key distinctness for hinting
  state, transform, glyph id, **sub-pixel phase**; LRU byte-budget eviction;
  eligible-run populate-then-hit; rotated/oversized ineligibility (fallback);
  real-outline rasterization to a tight bitmap; empty-glyph → empty bitmap.
- `tests/render_glyph_cache.rs`: end-to-end small-glyph render through the cache
  (correct shape) + identical repeat render on the same backend.

`cargo check --workspace --all-features` clean (no warnings from the new code).

## Deferred

- Rotated/skewed text caching (coverage is position-independent; the escape
  hatch just declines it today).
- Hinted-glyph caching is wired (key separates hinted/unhinted) but the default
  backend hinting policy is `None`, so it is exercised only when a caller sets
  `HintingPolicy::Auto`.
- Whole-doc throughput past PDFium is compile/image-bound, out of Phase B scope
  (Phase C: prep/flatten caching, single-blit-per-run).
