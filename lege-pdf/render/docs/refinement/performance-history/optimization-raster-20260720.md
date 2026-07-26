# Phase A — RasterKernel rewrite (per-edge signed-area deposit) — 2026-07-20

Implements Phase A of `PLAN-TEXT-GAP.md`: the coverage-kernel rewrite proven
out by the fontdue evaluation. Scope was `crates/pdf-render-cpu/src/raster.rs`
only (+ its tests). No other crate touched; no git state changed. Raw paired
rows are under `results/optimization-raster-20260720/` (`metadata.txt` included).

## What changed (`crates/pdf-render-cpu/src/raster.rs`)

The old kernel rescanned **every edge against every scanline** of the fill band
(`for y { for e in edges { clip; deposit } }`) — O(edges × rows) — even though
the file header already claimed the font-rs single-walk method.

- **A1 — per-edge area deposit.** Each edge is now walked **once**, over only the
  rows it crosses, depositing signed area into a **path-bbox-local 2D
  accumulation buffer** (`stride = bbox_width + 2`, `bbox_height` rows). A
  per-row prefix-sum then converts to coverage. The buffer is device-clamped to
  the fill's own bounding box, not the device plane, and only touched cells are
  reset between fills.
- **A2 — vertical-edge fast path.** Edges with `dxdy == 0` skip the x-walk and
  deposit a single precomputed column split per row (fontdue's `v_lines`
  analogue).
- **A3 — AVX2 coverage conversion.** `std::arch` runtime dispatch
  (`is_x86_feature_detected!`), `#[target_feature(enable="avx2")]`, scalar
  reference retained, `get_unchecked`/pointer ops only over caller-validated
  ranges (house style, per `pdf-image/src/jpeg/mod.rs`). It is **bit-for-bit
  identical** to the scalar path. **Disabled by default** (`AVX2_MIN_SPAN =
  usize::MAX`) — see "AVX2: a measured negative" below.
- **Cache/routing guard.** A large-but-sparse local window (`> 8 Ki cells` with
  `< cells/8` edges) is routed to the retained per-scanline reference path
  (`fill_scanline_ref`), whose single reused O(device-width) buffer has better
  cache locality when the 2D window would be big and mostly empty.

The public interface (`RasterKernel::fill(points, subpaths, width, height, rule,
row)` and the `last_edges/last_rows/last_covered` counters) is unchanged, so
`exec.rs` and `mask.rs` are untouched.

## Determinism: byte-identity ACHIEVED (strongest gate)

The rewrite is a pure loop reorganization: for every accumulation cell the
operands **and their order** (edge-index order) are preserved, the per-pixel
area math (`deposit_local`) is copied verbatim from the reference
(`deposit_partial`), and the coverage byte comes from one shared
`coverage_byte()`. So the fast path, the reference path, and the AVX2 conversion
all produce **identical bytes**.

- Unit gate `raster::tests::fast_path_matches_reference_bit_exact` fuzzes random
  multi-subpath geometry across 6 widths × 6 heights × both fill rules and
  asserts the fast path equals the reference **byte-for-byte**.
- `raster::tests::avx2_convert_matches_scalar_bit_exact` asserts AVX2 == scalar
  over ±8 signed-area values, both rules, SIMD body + scalar tail.
- **Every gated page's output hash is unchanged** between bin-base and bin-opt
  (table below). No fallback (±1) gate was needed — the strongest gate holds.

## Per-page results (compiled, scale 2.0, 2 alternating batches/binary)

`render.path` == `render.execute` (scan-convert + blend). Medians (ms).

| page | metric | bin-base | bin-opt | speedup |
|---|---|---:|---:|---:|
| **latin-text** (2,689 glyphs) | render.path | 16.852 | 13.131 | **1.28×** |
| | benchmark.total | 22.767 | 19.092 | 1.19× |
| | output hash | `77e58820675dc4b5` | `77e58820675dc4b5` | **identical** ✓ |
| | PDFium severity | 0.0893807 | 0.0893807 | **identical** ✓ (baseline 0.089) |
| **type1-fonts** | render.path | 4.611 | 3.536 | **1.30×** |
| | output hash | `4cd8639fb0f5077c` | `4cd8639fb0f5077c` | identical ✓ |
| | severity | 0.0317895 | 0.0317895 | identical ✓ (baseline 0.032) |
| **vector-diagram** (regression watch) | render.path | 8.765 | 8.176 | **1.07×** |
| | output hash | `6bdd66dbcdd3fa34` | `6bdd66dbcdd3fa34` | identical ✓ |
| | severity | 0.0512654 | 0.0512654 | identical ✓ (baseline 0.051) |
| **cjk-cid-text** | render.path | 6.862 | 6.804 | 1.01× (neutral) |
| | render.path (min) | 6.239 | 6.204 | — |
| | output hash | `61a2b5bd1db0243e` | `61a2b5bd1db0243e` | identical ✓ |
| | severity | 3.543e-05 | 3.543e-05 | identical ✓ |

- **latin-text** (the primary target): `render.path` **16.85 → 13.13 ms**
  (−3.7 ms), materially below the plan's ~16.8 ms figure; whole `benchmark.total`
  22.8 → 19.1 ms.
- **vector-diagram** (the Phase-A regression watch, 34% raster fill): improved
  1.07×, no regression.
- **cjk-cid-text**: only 4 glyphs / 400 edges in a single **wide, spread-out**
  fill command — not raster-bound (`render.path` is dominated by ~6 ms of fixed
  per-command cost). An early version regressed it ~18% (a 4-pass pass-2) then
  ~3% (2D-buffer cache cost on the wide sparse window); the cache/routing guard
  sends that command to the scanline reference, returning it to **baseline
  (neutral)**. Because both paths are byte-identical, routing only ever trades
  speed, never correctness — it cannot regress below baseline.

## Whole-document control (Buddhist_Ethics, 74 pages, pipeline-profile ×4)

| metric | bin-base | bin-opt |
|---|---:|---:|
| benchmark.total (median) | 561.09 ms | **521.35 ms** (1.076×) |
| throughput | 131.9 pages/s | **141.9 pages/s** |
| page latency p50 | 129.68 ms | **111.45 ms** (1.16×) |
| page latency p90 | 194.74 ms | 170.62 ms |
| peak RSS | 654.2 MiB | 606.1 MiB |
| succeeded pages | 74/74 | 74/74 |

The bbox-local buffer keeps RSS bounded (in fact slightly lower here); all 74
pages render on both binaries.

## Kernel micro-bench (pure fill, outlines+flatten cached; median ns/glyph)

3,000 draws over 75 distinct ASCII glyphs, real Skrifa outlines + the renderer's
8-seg flattener. `new-scalar` is the shipped kernel (AVX2 off); `new-avx2` forces
AVX2 on every span; fontdue's full rasterize is the reference point.

| font | px | fontdue | old-kernel | new-scalar | new-avx2 | new-scalar / old |
|---|---:|---:|---:|---:|---:|---:|
| FreeSans (glyf) | 24 | 335 | 2820 | **2271** | 2385 | **1.24×** |
| | 48 | 699 | 5920 | **3944** | 4018 | **1.50×** |
| | 96 | 1770 | 11805 | **9168** | 9974 | **1.29×** |
| FoxitSans (CFF) | 24 | 338 | 2358 | **1671** | 1908 | **1.41×** |
| | 48 | 760 | 4825 | **3514** | 3682 | **1.37×** |
| | 96 | 1821 | 11261 | **8585** | 9361 | **1.31×** |
| NimbusSans (CFF) | 24 | 352 | 2411 | **1862** | 1995 | **1.29×** |
| | 48 | 760 | 5259 | **3807** | 3521 | **1.38×** |
| | 96 | 1901 | 11305 | **8459** | 9534 | **1.34×** |

The A1+A2 rewrite is **1.24–1.50×** on the pure scan-conversion kernel, which is
where the per-page `render.path` gains come from.

## AVX2 (A3): a measured negative, delivered and gated off

The bit-exact AVX2 conversion is implemented, dispatched, and verified — but the
micro-bench (`new-avx2` column, slower than `new-scalar` in 8/9 cases) and the
page A/B both show it **net-negative** for glyph and vector fills. Root cause:
the kernel is deposit- and **sequential-prefix-sum**-bound, not convert-bound. A
bit-exact SIMD prefix-sum is impossible (SIMD reorders the running sum → last-bit
drift), so SIMD can only touch the *convert*, which forces a separate prefix-sum
write-back pass; that pass costs more than the SIMD convert saves. It is retained
(runtime-dispatched, scalar reference, unit-tested bit-exact) behind
`AVX2_MIN_SPAN = usize::MAX` so a future convert-bound workload can lower one
constant, without shipping a regression today.

## Success bar vs outcome

The plan hoped "3–8× on the scan-conversion share." That figure was the gap to
**fontdue's whole rasterize**, which also pre-flattens at load, uses a tuned
unsafe-SIMD deposit, and writes bbox-tight buffers — those are Phase B (glyph
coverage cache) and Phase C (pre-flatten) levers, not Phase A. A1 alone (removing
the edges×rows rescan) is worth **1.24–1.50×** on the kernel because many glyph
edges (stems) span most of the glyph height regardless, so the eliminated waste
is the short curve edges. That lands as **1.28× latin / 1.30× type1 / 1.07×
vector** on `render.path` and **1.076× / +18 ms p50** whole-document — a real,
byte-identical win, with the larger multiple deferred to Phase B's cache.

## Gates

- `cargo test -p pdf-render-cpu`: **28/28 lib tests pass** (incl. both bit-exact
  gates). (`font_cache_tests::cheap_parses_are_not_retained` is a pre-existing
  parse-timing test that can flake under concurrent machine load; passes in
  isolation and is unrelated to raster.rs.)
- `cargo check --workspace --all-features`: clean for the change. The only
  remaining warnings are pre-existing, in external path-deps (jp2lam,
  jbig2enc-rust); `raster.rs` adds none.
- Git untouched (only `raster.rs` modified in-tree; results dir untracked).
