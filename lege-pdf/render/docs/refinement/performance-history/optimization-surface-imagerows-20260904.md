# Page-surface reuse + load-aware image row fan-out — 2026-09-04

Two costs that nine earlier passes never attributed, because both live *outside*
the per-pixel sampling maths those passes optimized:

1. **`render.surface`** — allocating and painting the page buffer. On a
   sweep-resolution scan this was **50 ms of an 87 ms page**, 59 % of the whole
   render, and no prior document mentions it.
2. **Serial image row loops.** `paint_axis_aligned_rgb8_bilinear_opaque` had
   fanned its destination rows over the rayon pool since the JPEG pass; every
   *other* image loop — area-minification (opaque, masked, CMYK), the bilevel
   summed-area path, and the generic per-pixel fallback — stayed single
   threaded, so an already-tight per-pixel kernel ran on one core while
   nineteen sat idle.

Everything here is a scheduling/allocation change. No arithmetic was touched;
every page's output hash is unchanged.

## Measurement

`pdfium-diff profile --mode compiled`, release + `bench-profiling`, host
i7-13700H (20 logical cores). **Baseline is a separate binary** built from a
`git worktree` of `HEAD` (`b3e8ecce`) — not an env-gated arm — so the pre-existing
bilinear fan-out is present in the baseline exactly as it ships. (Another agent
advanced `feat/printing` to `c186c671` mid-session; that range touches only
`lege-codecs/djvulibrust` and `.akr/`, so `lege-pdf/render` at `b3e8ecce` *is*
current HEAD and the pairing stands.) Runs are paired
and interleaved (A/B/B/A over 4 batches, 8–40 timed runs per page per arm), and
the tables report **min / median** in ms. The machine carried other agents'
work throughout, which is why min is quoted alongside median.

The 13-page `corpus/perf/pages.tsv` set is mostly missing on this host (the
`pdfium-port-plan/renderer-corpus/` root does not exist), so the corpus was
rebuilt from what resolves: the two `to-sort/` corpus entries, the in-repo
`corpus/shadings/`, and eight further `to-sort/` documents chosen by a
counter-driven sweep of 60 documents × 2 pages for *distinct* slow classes
(bilevel SAT, generic stencil, area-min, CMYK area-min).

## Results — whole page, baseline binary vs optimized binary

| page | class | base min/med | opt min/med | speedup | hash |
|---|---|---:|---:|---:|---|
| `globalhistoryofm` p0 @2.083 | jpeg scan, sweep | 86.98 / 92.62 | **38.80 / 41.22** | **2.24×** | identical |
| `globalhistoryofm` p0 @1.0 | jpeg scan, viewer | 20.40 / 22.14 | **9.09 / 10.27** | **2.24×** | identical |
| `Levy-SanctionsSouthAfrica` p3 | bilevel SAT | 72.51 / 76.41 | **15.03 / 17.76** | **4.82×** | identical |
| `Finnegans-Wake` p0 | bilevel SAT | 46.95 / 49.64 | **9.98 / 11.69** | **4.70×** | identical |
| `lossless-and-lossy…` p3 | bilevel SAT + text | 59.06 / 63.16 | **12.81 / 14.14** | **4.61×** | identical |
| `Zen-essence` p0 @2.083 | MRC (JPX + JBIG2 stencil) | 10.19 / 11.34 | **2.76 / 3.45** | **3.69×** | identical |
| `Graphics Gems III` p60 | minified gray stencil (generic loop) | 8.24 / 9.29 | **2.27 / 2.72** | **3.64×** | identical |
| `04-Timo_Kunkel` p3 | RGB8 area-min + vector | 29.12 / 31.25 | **7.57 / 8.69** | **3.84×** | identical |
| `rosiesmenu3` p3 | CMYK/RGB area-min + heavy paths | 271.11 / 281.52 | **109.27 / 112.75** | **2.48×** | identical |
| `Hu Shih` p40 | latin text | 1.09 / 1.26 | 1.10 / 1.24 | 1.00× | identical |
| `10.1016@j.mehy…` p2 | vector, 8,886 edges | 1.96 / 2.20 | 1.97 / 2.21 | 1.00× | identical |
| `mesh-type6-coons` p0 | Coons-patch shading | 3.35 / 3.81 | 3.36 / 3.78 | 1.00× | identical |

Peak RSS is unchanged on every page (largest delta 1 MB of 1,009 MB).

---

## Change 1 — the page surface is reused across renders

### What the measurement said

`render.surface` is one call: `Surface::new(w, h, background)`. It is a fresh
`Arc<[u8]>` of `w·h·4` bytes filled with the background byte. On the sweep-scale
scan (5746×7419 → 170 MB) it measured **50 ms**; at viewer scale (2758×3561 →
39 MB) **12 ms**; on a 1224×1584 text page **0.10 ms**.

The discontinuity is glibc's dynamic `mmap` threshold, which never rises past
32 MiB. Under it a freed block is recycled by the allocator and the refill is a
resident memset; over it every render `mmap`s fresh anonymous pages, and the
fill takes a minor fault plus a kernel page-zero on each one before it can write
a byte. A standalone probe on this host:

| 170 MB buffer | time |
|---|---:|
| fresh alloc + `fill(0xFF)` | 49–54 ms |
| reused alloc + `fill(0xFF)` | 3.3–3.6 ms |
| `vec![0u8; n]` never touched | 2–10 µs |

So ~93 % of `render.surface` on a large page was page-fault and kernel-zero
work, not the fill.

### What changed

- `surface.rs` — `Surface::new_recycled(w, h, background, recycled)`;
  `Surface::new` is now that with `None`. A recycled buffer is accepted only
  when its length matches *exactly* and `Arc::get_mut` succeeds, so a buffer the
  previous consumer still holds is dropped rather than written through. The
  background is repainted by `paint_background`, which writes byte-for-byte what
  `filled_arc`/`repeated_rgba_arc` would have allocated.
- `surface.rs` — `into_output_recycling(format, pool)` parks the buffer.
  For RGBA it parks a **second `Arc` on the returned page**, so the pool pins
  nothing the consumer still owns; reuse is granted only after the consumer
  drops it. For `Gray8` the RGBA buffer is not the returned page, so it is
  parked outright.
- `exec.rs` — `CpuWorkerContext` gains `surface_buffer: Option<Arc<[u8]>>`: at
  most one buffer, always the most recent page's size (a differently sized page
  drops it), and nothing at all until a page has been rendered. That is one page
  surface per worker — the same allocation the worker was already sized for.
- `lib.rs` — both `render_with` and `execute_prepared_profiled` take from and
  park into the context. `PDF_RENDERER_SURFACE_REUSE=off` disables it for
  same-binary A/B.

### Result (measured alone, same binary, `off` vs `on`, 3 interleaved batches)

| page | metric | off | on | Δ |
|---|---|---:|---:|---:|
| jpeg-scan-sweep | `render.surface` | 49.58 | **3.34** | −93 % |
| | `benchmark.total` | 86.92 | **40.08** | **2.17×** |
| jpeg-scan-viewer | `render.surface` | 11.23 | **0.76** | −93 % |
| | `benchmark.total` | 20.41 | **9.13** | **2.24×** |
| every other page | — | — | — | within noise |

Whole-document peak RSS on a 640-page scan: 1,102 → 1,092 MB (retaining the
buffer costs nothing net, because the allocator was returning it to the kernel
and taking it straight back).

---

## Change 2 — every image row loop fans out, load-aware

### What the measurement said

Per painted destination pixel, on the same host:

| path | ns/px | parallel before? |
|---|---:|---|
| `fast_rgb8_bilinear` (4 taps) | 0.82 | yes |
| `fast_rgb8_area_min` (4 taps) | 12.4 | no |
| generic loop, minified gray stencil | 102 | no |
| bilevel SAT (`Levy` p3, 25.8 taps) | 51 | no |

A 15× gap between two four-tap kernels is not arithmetic; it is nineteen idle
cores.

### What changed (`exec.rs`)

Four loops were restructured into a `paint_row(y, …, row) -> counters` closure
driven by `surface.rows_mut_abs(y0, y1)` and either `par_chunks_mut` or
`chunks_exact_mut`:

- `area_min_box_average_opaque` — the shared core of `fast_rgb8_area` and
  `fast_cmyk_area`.
- `paint_axis_aligned_rgb8_area_min_masked` — the MRC path.
- `paint_binary_box_sat` — the bilevel summed-area painter. Its `(weight,
  weighted ones)` mix memo becomes per row; the memo is a pure function of its
  key, so a per-row memo changes only how often the division repeats, never the
  colour it yields.
- the generic per-pixel `else` arm of `paint_image` — the fallback that carries
  stencils, colour-key masks, rotated placements, clipped and soft-masked draws,
  and every non-Normal blend.

Each destination row reads only shared, immutable state and writes only its own
row; the counters are summed with `reduce`/`fold` over `u64`, which is exact.
The pixel arithmetic is untouched.

### Change 2b — the bilevel summed-area table is filled in parallel

Parallelizing the SAT *painter* alone only bought 1.46× on `Levy` p3
(70.2 → 48.2 ms), so the build was instrumented directly:

```
SATBUILD 4350x5950 entries=25892801 alloc=9µs loop=44.9ms   (render.image = 52 ms)
```

`BilevelIntegral::build` was **44 of the 52 ms**: 25.9 M `u32` = 103 MB, written
once, most of it first-touch page faults — the same cost as change 1, in a
different allocation.

`BilevelIntegral::fill_parallel` fills it in three passes: every block of rows
sums from zero in parallel; the block-boundary carries are chained once (one row
of `stride` adds per block, ~40 blocks); every block but the first adds its carry
back, in parallel. Integer addition is associative, so each cell ends as the same
sum of the same `u32` terms. The serial walk is retained as the reference and
still runs below `BILEVEL_SAT_PAR_MIN_ENTRIES` (1 Mi entries).

Gate: `exec::bilevel_sat_tests::parallel_sat_fill_matches_serial_bit_exact` — the
parallel fill must equal the serial walk **element for element**, over eight
geometries that fall short of, land on and overrun the block boundary, and over
both a full-image and an offset sub-window.

### Incremental attribution (same binary, `PDF_RENDERER_IMAGE_ROW_PAR` off/on)

| step | page | metric | serial | parallel | speedup |
|---|---|---|---:|---:|---:|
| opaque area-min | image-vector-mix | `render.image` | 22.31 | **2.82** | 7.9× |
| + masked area-min | mrc-jpx-jbig2 | `render.image` | 10.07 | **2.37** | 4.3× |
| + generic loop | stencil-generic | `render.image` | 7.25 | **0.94** | 7.8× |
| + SAT painter only | bilevel-ccitt-a | total | 70.16 | 48.19 | 1.46× |
| + parallel SAT build | bilevel-ccitt-a | total | 69.39 | **15.40** | **4.51×** |
| | bilevel-ccitt-b | total | 45.25 | **9.81** | **4.61×** |

---

## The measured negative, and the fix

Fanning out unconditionally **cost 31 % of whole-document throughput.**
`pipeline-profile`, `Finnegans-Wake` (640 bilevel pages, scale 1.0, 7 compile /
13 render workers), 6 runs per arm, interleaved:

| | median total | peak RSS |
|---|---:|---:|
| fan-out off | 4,702 ms | 1,060 MB |
| fan-out always on | 6,019 ms (**−31 %**) | 1,176 MB |

Under the scheduler every core already carries another page, so 13 render
workers plus a 20-thread rayon pool oversubscribe the box: the same work,
scheduled worse.

So the fan-out is now **load-aware**, in the shape of the `JPX_IN_FLIGHT` policy
from `optimization-renderer3-20260720.md`. `exec::execute` holds an RAII
`PageInFlight` guard over one top-level page execution; the row loops fan out
only while `PAGES_IN_FLIGHT <= 1` — the viewer / single-page case, which is
exactly where latency is the product. The pre-existing bilinear fan-out was put
under the same policy, since it had the same problem and nobody had measured it.

`PDF_RENDERER_IMAGE_ROW_PAR` overrides: unset/`auto` = load-aware, `off` = never,
`on`/`always` = always.

### Whole-document control after the fix (baseline binary vs optimized binary)

`Finnegans-Wake`, 640 pages, scale 1.0, 14 runs per arm, interleaved:

| | min | p25 | median | peak RSS | pages |
|---|---:|---:|---:|---:|---:|
| baseline `b3e8ecce` | 4,365 ms | 4,731 | 4,786 | 1,102 MB | 640/640 |
| optimized | 4,496 ms | 4,727 | **4,784** | 1,092 MB | 640/640 |

Neutral: the medians and p25 agree to 0.1 %, the min difference is one lucky
baseline run out of fourteen.

## Correctness gates

- **Byte-identity sweep.** 70 randomly chosen documents (`to-sort/` + the in-repo
  shading corpus) × pages 0/1/4 at scale 2.0, rendered by the baseline and the
  optimized binary: **148 pages compared, 148 identical, 0 differ** (62
  page/document combinations do not exist and were skipped).
- **Every measured page's output hash is unchanged** (tables above).
- **New unit gates**
  - `exec::bilevel_sat_tests::parallel_sat_fill_matches_serial_bit_exact`.
  - `surface::tests::a_recycled_surface_is_byte_identical_to_a_fresh_one` — for
    White, Transparent and Solid, a recycled dirty buffer must end byte-identical
    to a fresh allocation, in the same allocation.
  - `surface::tests::recycling_declines_a_shared_or_mismatched_buffer` — a buffer
    another handle still owns is never written through, and a wrong-length one is
    never adopted.
  - `surface::tests::into_output_parks_the_buffer_for_the_next_render` — parked
    but not reusable while the consumer holds it; reusable once dropped; the
    `Gray8` path parks the RGBA buffer outright.
- `cargo test -p pdf-render-cpu` — **84 lib tests + 13 integration suites pass**,
  0 failed.
- `cargo clippy -p pdf-render-cpu --all-targets` — 0 errors; the 11 lib / 17
  lib-test warnings are the same pre-existing set, at the same locations, as
  before this pass (`prepared.rs`, `raster.rs`, `mask.rs`, `attribution.rs`, and
  `exec.rs`'s pre-existing "manual checked division").
- `cargo check -p pdf-cli -p pdf-render-scheduler -p pdf-postprocess
  -p pdf-chaos-tests --all-targets` — clean.
- `cargo fmt -p pdf-render-cpu` — clean (its diff was confined to this pass's own
  code).

## Remaining opportunities

- **`render.path` is now the slowest thing on the corpus.** `rosiesmenu3` p0
  spends **169 ms** and p3 **76 ms** in `render.path`; `0130175455` p0/p3 spend
  22–28 ms; `Book Cover first (1)` p0 spends 58 ms. Path coverage, glyph blits,
  shading spans, tiling and group compositing are all still serial and were not
  attributed in this pass. That is the natural next target.
- **Small masked draws stay serial.** The fan-out threshold is 65,536
  destination pixels. `Booking Confirmation_1658103130102` p0 spends **208 ms**
  in `render.image` across 23 resource-masked draws averaging ~38 k pixels each
  — every one below the threshold, and ~250 ns per painted pixel in the generic
  loop. Either the threshold should count a *draw's* work rather than its
  pixels, or that shape needs its own prepared path.
- **The bilevel SAT still materialises ~100 MB.** After the parallel fill it is
  ~10 ms, but the table is only ever differenced across rows inside one
  destination pixel's box (≈5 rows here). A per-destination-row band integral —
  two L2-resident arrays instead of a 100 MB table — would remove the allocation
  entirely and is fully row-parallel. It needs `weighted_ones` reworked against
  band-local indices, so it is a real change, not a reordering.
- **The non-SAT bilevel popcount fallback** (`paint_axis_aligned_binary_box`'s
  tail, taken only when the source region exceeds `MAX_ENTRIES`) is still
  serial. No corpus page reaches it, so it was left alone rather than changed
  unmeasured.
- **Whole-document mode gets none of the fan-out, by design.** Per-page latency
  under the scheduler is unchanged; only single-page latency improved. If a
  future scheduler wanted both, it would have to shrink the render-worker count
  and let rayon own the cores, which is a scheduler decision, not a renderer one.
- **`area_min_box_average_opaque`'s per-tap `weight_at`** is still a two-branch
  select inside the box loop, and `wy * tx.weight_at(col)` recomputes the column
  weight once per source row. Splitting the box into first / interior / last
  runs would be bit-identical, but at the 2×2 footprints these pages minify by
  there is nothing to amortise; it only pays for heavily minified sources.
- **Not re-measured:** every corpus page under the absent
  `pdfium-port-plan/renderer-corpus/` root (`ccitt-bilevel`, `latin-text`,
  `cjk-cid-text`, `type1-fonts`, `vector-diagram`, `transparency-group`,
  `soft-mask`, `tiling-pattern`, `radial-shading`). Stand-ins for the text,
  vector and shading classes were measured and are neutral; the transparency,
  soft-mask and tiling classes have no stand-in here at all.
