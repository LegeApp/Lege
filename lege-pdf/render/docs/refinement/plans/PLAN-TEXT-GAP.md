# PLAN-TEXT-GAP — closing the Latin-text gap vs PDFium

> **CLOSED (2026-07-21):** Phases A and B landed previously, Phase C was
> skipped by decision, and the goal is cleared. Retained as provenance.

Date: 2026-07-20. Inputs: PDFium source review (pdfium-reference-source),
fontdue crate evaluation with benchmarks, and focused latin-text profiling.
Baseline: heads-up bench shows text pages 3–10× slower than PDFium
(Buddhist_Ethics body pages: ours ~10–31 ms, PDFium ~2–5 ms; whole doc
140.6 vs 220 pages/s). Image-heavy classes are already at 0.6–1.6× per page
and 5.8–11.2× faster per document, so text is the last large gap.

## Measured shape of the problem (latin-text page 10, 2,689 glyphs)

| Stage | Time | Share |
|---|---:|---:|
| render.execute / render.path (scan-convert + blend) | 16.8 ms | 67% |
| render.lower (outline extract 3.0 + flatten/emit 2.4) | 6.3 ms | 25% |
| output/surface | 2.2 ms | 8% |

Per-glyph budget: **9.3 µs ours vs ~1–1.8 µs PDFium** — and our raster fill
alone is 6.2 µs. perf self-time: 59% scan conversion, ~7% edge building,
~6% CFF charstring interpretation, 2% blend. Every glyph occurrence redoes
outline-extract → flatten (fixed 8 seg/curve) → edge build → scan convert;
there is no glyph-level reuse of any artifact.

## Why PDFium is fast (from its source)

1. `CFX_GlyphCache` — rendered coverage bitmaps cached per unique glyph;
   key = quantized CTM a,b,c,d (×10000), dest width, AA mode, substitution
   params; pen translation deliberately excluded. FreeType runs once per
   unique glyph; each occurrence is two map probes + an alpha blend.
2. Integer-pixel origin snapping (`FXSYS_roundf`) with ±1 px spacing fixup
   (`AdjustGlyphSpace`) so all occurrences share one bitmap.
3. Whole-run accumulation into one bitmap, flushed once per run.
4. Size/rotation escape hatch: |a|+|b| > 50 or printer → outline path.
References: core/fxge/cfx_glyphcache.cpp, cfx_renderdevice.cpp:1082–1292.

## fontdue verdict: NOT adopted; techniques copied

Benchmarks (i7-13700H, 75 distinct glyphs, cache-miss cost): fontdue full
rasterize **~0.33 µs/glyph** at 24 px vs our fill kernel alone 2.1–2.8 µs
(6–8×) — proof the algorithm class (signed-area accumulation, which we
already use) supports much more speed. Rejected because: public API is
uniform-px only and geometry is pre-flattened at load, so rotation/skew/
non-uniform scale are architecturally impossible (PDF requires them); no
public outline-feed entry point (`Raster::draw` is pub(crate)), so it cannot
rasterize our Skrifa-extracted outlines; SFNT-wrapped-only parsing via
ttf-parser (no bare CFF/Type 1) would duplicate our font stack; crate
self-labels experimental. Its speed sources — walk each edge once, vertical-
edge fast path, flatten-once, SIMD accumulate/prefix-sum — are all portable
to our kernel. Keep the clone as reference only.

## Plan

### Phase A — RasterKernel rewrite (helps ALL fills, not just text)

crates/pdf-render-cpu/src/raster.rs. The current kernel rescans every edge
on every scanline (O(edges × rows)); the file header cites font-rs but the
implementation doesn't realize it.

- A1: per-edge area deposit — walk each edge once, touching only crossed
  pixels, into the accumulation grid; prefix-sum sweep to coverage.
- A2: vertical-edge fast path (fontdue v_lines analog).
- A3: SIMD (AVX2 runtime-dispatch, scalar reference retained — house style)
  for accumulate + prefix-sum; `get_unchecked` only with clamped inputs.
- Determinism protocol: f32 deposit reordering can shift last-bit coverage,
  so byte-identical hashes are NOT promised here. Gate like the JPX reduced
  decode: per-page PDFium severity must not worsen materially
  (latin 0.089, vector 0.051 baselines), plus fixed-fixture coverage-diff
  tests bounding any pixel delta to ±1 coverage step. If a variant CAN stay
  byte-identical (deposit order made deterministic), prefer it.
- Expected: 3–8× on the 59% scan-conversion share; also speeds the vector
  class (34% raster fill) and the Phase-B cache-miss path.

### Phase B — glyph coverage-bitmap cache + origin snapping (the PDFium win)

- B1: cache keyed by FontProgram content identity (reuse the 128-bit hash
  from SharedFontProgramCache) + glyph id + quantized a,b,c,d (×10000)
  + ppem + hinting state (hinted glyphs also snap origin-y — key MUST
  separate them) + fill rule + AA mode. Value: bbox-tight u8 coverage +
  bearing. Document-scoped, sharded LRU bounded ~32 MiB (same pattern as
  the font-program cache). Populate on miss via the Phase-A kernel; on hit,
  blit through kernels::blend_mask (coverage is color-agnostic — solid
  fills only today, prepared.rs:917 already bails on pattern text).
- B2: integer-pixel origin snapping + spacing fixup (AdjustGlyphSpace
  analog) so occurrences share bitmaps; without it the key would need
  sub-pixel phases and hit rates collapse. Output pixels shift by design —
  gate via severity (expect PDFium parity to improve, since PDFium snaps).
- B3: exclusions — Type 3 (already diverted via show_type3), clip modes
  4–7 (need exact outlines), stroke-relevant modes 1/2/5/6 (currently
  WRONGLY filled — do not bake that defect into the cache; fix or exclude),
  and a size/rotation cap mirroring PDFium's |a|+|b| escape to the outline
  path so huge glyphs don't blow the LRU.
- Expected: ~95%+ hit rate on body text (few hundred unique glyph×size vs
  ~2,700 occurrences) — this is the mechanism that collapses 9.3 µs/glyph
  toward probe+blend.

### Phase C — prep + composite cleanup

- C1: cache design-space flattened outlines per (font, gid), rescale on
  material scale change instead of re-extracting + 8-seg re-flattening every
  draw (kills most of the 25% lower share for cache misses and unmapped
  glyphs).
- C2: batch a glyph run into one accumulation region, single blend per run
  (PDFium mechanism #4; ~2 ms/page today, cheap alongside B).
- C3: adaptive flattening tolerance (device-space error target) replacing
  fixed OUTLINE_SEGMENTS=8 — fewer edges at body sizes, better quality at
  display sizes.

### Measurement gates (every phase)

Paired A/B binaries; latin-text pages 10 + 20, cjk-cid-text, type1-fonts,
vector-diagram (Phase A regression watch), whole-document control, and the
heads-up bench (target: body pages ≤2× PDFium per page; whole-doc faster
than PDFium on the Latin ebook). RSS: cache growth bounded and reported.

### Sequencing

A → B → C. A is standalone and lowest-risk; B depends on A only for miss
cost; C is polish. One agent per phase, review + gates between phases.

Target end state: per-glyph ≤2 µs (body pages ~25 ms → ~5 ms), whole-doc
Latin from 140 pages/s past PDFium's 220.
