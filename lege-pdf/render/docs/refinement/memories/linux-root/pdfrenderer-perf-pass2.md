---
name: pdfrenderer-perf-pass2
description: "2026-07-20 second optimization pass: jp2lam decoder 2.3x + JPX renderer integration with reduced decode, JPEG MCU streaming + AVX2; remaining perf items"
metadata: 
  node_type: memory
  type: project
  originSessionId: 41d8e742-4f3f-47ba-bf78-9b522cb5ecfe
  modified: 2026-07-20T08:25:53.442Z
---

Second perf pass (2026-07-20, three Opus agents, all landed UNCOMMITTED in
working trees of `jp2lam` and `pdfium-port-plan/pdf-renderer`):

1. **jp2lam decoder** — Tier-1/dequant fusion (`CoefficientPlane::Integer/Real`,
   no full i32 plane), bounded parallel Tier-1 (`BlockPlanes` disjoint-rect
   unsafe writer, Serial ≡ Budgeted byte-identical test), multi-tile reduced
   decode. Fixture full decode 28→~11.5 ms. Legacy hash `40b78d4e95b72745`
   unchanged; 265 lib tests. Doc: `llm-docs/decode-optimization-results-2026-07-20-phase2.md`.
2. **JPEG** — baseline MCU-row streaming (byte-identical by shared
   `decode_block_sequential`/`assemble_row`; progressive keeps coeff path) +
   bit-exact AVX2 IDCT. DCT decode −18%, peak RSS −42 MiB. 12-bit Huffman table
   measured 2.1% SLOWER — dropped (cache pressure). Doc:
   `corpus/perf/optimization-jpeg2-20260720.md`.
3. **JPX integration** — `jpx.rs` now uses `Jp2Decoder` thread-local session,
   packed Gray8/Rgb8/Cmyk8 for all-8-bit streams (hash-identical), native+shift
   fallback for >8-bit. `DecodeParameters.target_size` hint from `lower_image`
   CTM (`codec_target_size`, unclamped bbox) drives `AtLeast` reduced decode,
   margin 1.0, `Budgeted(2)`. jpx-scan decode 108→40 ms, RSS −54%; mrc 121→46 ms,
   severity ≤ baselineish (0.000096 / 0.003781, budget 0.005). Env switches:
   `PDF_RENDERER_JPX_REDUCE=0`, `PDF_RENDERER_JPX_MARGIN`,
   `PDF_RENDERER_JPX_CONCURRENCY`. Known intentional tradeoff: synthetic
   decode-only mode regresses (+21–38%) because per-decode concurrency is
   bounded instead of using the global rayon pool. Doc:
   `corpus/perf/optimization-jpx-integration-20260720.md`.

**Pass 3 (same day, three more agents):** jp2lam region decode (bit-exact,
margins 5/3:±2 9/7:±5; multi-tile tile-skip scales with area, single-tile only
0.886× — windowed DWT deferred) + multi-tile packed-direct (269 tests). JPEG
AVX2 YCbCr row assembly + band store + DC-only blocks → decode 42 ms
(cumulative 187→42 ms, 4.4×; Huffman entropy now dominates → pass-4 target:
64-bit bit buffer, batched AC). Renderer: CCITT SAT box filter (hash-identical,
render.image 44.6→28.2 ms), document-scoped font cache (128-bit content hash —
pointer identity got 0 cross-page hits; 48 MiB sharded LRU; ≥400 µs parse
retention gate; only wins on repeated/multi-page Type 1 docs), load-aware JPX
budget clamp(cores/in_flight,2,8) (−9% single-page, inert under load). Docs:
`optimization-{jpeg3,renderer3}-20260720.md`, jp2lam phase3 doc.

**Heads-up vs PDFium** (`pdfium-diff bench`, 2026-07-20, was 3–4× slower/page,
2× faster/doc): per-page single-threaded — JPEG scan 0.6× (FASTER), MRC 1.2×,
JPX 1.6×, CCITT 1.6×, Latin text 4.9×, Type1 first render 11.6× (parse-bound).
Whole-document — 5.8–11.2× FASTER on image docs (JPEG 808pp 35.6 vs 3.2
pages/s), but 1.56× slower on the 74-page Latin text doc (140.6 vs 220 pages/s).
**Next frontier is text/font pages**: raster fill + per-page glyph prep
(Latin pages run 3–10× PDFium; whole-doc profile was 42% raster fill, 22%
prep). Plan committed: `pdf-renderer/PLAN-TEXT-GAP.md` — Phase A RasterKernel
rewrite (current kernel is O(edges×rows), fontdue benchmarks prove 6–8×
headroom in the same algorithm class), Phase B PDFium-style glyph coverage
cache + integer origin snapping, Phase C flatten/composite cleanup.
**fontdue evaluated and REJECTED** (no arbitrary transforms — geometry
pre-flattened at fixed scale; no public outline-feed API; SFNT-only via
ttf-parser, no bare CFF/Type 1; experimental) — hand-roll, porting its
edge-walk-once/vertical-edge/SIMD techniques.

**Phase A DONE** (master afda1ec): RasterKernel per-edge area deposit replaces
O(edges×rows) rescan; BYTE-IDENTICAL (bit-exact tests), latin render.path
16.85→13.13 ms, whole-doc 561→521 ms. AVX2 convert built but gated off
(deposit-bound, net-negative). **Phase B DONE** (master 5e31d9a): glyph
coverage-bitmap cache (PDFium CFX_GlyphCache) — 32 MiB sharded LRU, key =
font-hash+gid+quantized axis-aligned map+subpixel phase+hinting; sub-pixel
4×4 bucketing NOT full snapping (snapping regressed severity). latin 20.5→3.2
ms (6.5×), per-glyph 9.3→1.03 µs, whole-doc 131.7→145.1 pages/s, 95.8% hit
rate, severity within noise, cache-off byte-identical to baseline (I verified
independently). Type3/clip/stroke-modes excluded; only render-mode-0 cached.

**Post-A+B heads-up:** Latin body pages now 1.5–2.8× PDFium/page (was 6–10×);
whole-doc still 1.67× behind (144 vs 240 pages/s) — but the blocker is NOW
**page 0 = 475 ms, 88% image area-min SAMPLING** (render.image 420 ms; DCT
decode only 52 ms; 10.7M taps / 1.1M dst px @ ~39 ns/tap — looks like the fast
RGB8 area-accumulate path is NOT engaging). **Phase C (text prep) is low-value
now** — B's coverage cache subsumed C1's flatten caching on the warm path.
Real next target = page-0 image sampling, not Phase C (Phase C SKIPPED by
user decision — measured low-value).

**Page-0 fix DONE** (master c64baeb): root cause was NOT RGB8 — the cover is
CMYK, and the generic area_average byte-sums only RGB8 so CMYK ran per-tap
cmyk_to_rgb (f32) over 10.67M taps = the 420 ms. Added fast_rgb8_area +
fast_cmyk_area (footprint>1 opaque axis-aligned twins of the nearest path);
CMYK converts source to RGB8 once then box-averages, byte-identical (fuzz test
asserts fast==generic shade pixel-for-pixel; page-0 hash d367047299c4b294
unchanged). Direct row-band accumulate, not SAT (footprint ~3x3). Page-0
render.image 417→132 ms. **Whole-doc Latin 131→316 pages/s — 1.58× slower →
1.49× FASTER than PDFium (I independently re-benched: 316 vs 213).** Body
pages now 1–3.2× PDFium/page. GOAL CLEARED. jp2lam master 1ca41b3; nothing
pushed. See [[pdfrenderer-corpus-gaps]],
[[pdfrenderer-native-first]], [[project_jp2lam]].
