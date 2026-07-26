# JPX decoder integration — 2026-07-20

Integrating the optimized `jp2lam` request API into the renderer's `/JPXDecode`
codec. JPX decode was the renderer's #1 measured hotspot (jpx-scan ~292 ms and
mrc ~302 ms decode-only in the 2026-07-19 sweep). Two phases:

- **Phase 1** — direct packed 8-bit output, removing the renderer-side
  full-image interleave pass and the planar `i32` intermediates. Hash-preserving.
- **Phase 2** — reduced-resolution decode for minified draws, driven by a
  device-footprint hint plumbed from the renderer through the codec seam.

## What changed

- `crates/pdf-image/src/jpx.rs` — rewritten over the new API. Inspects the
  header, then for all-8-bit-component streams requests packed `Gray8`/`Rgb8`/
  `Cmyk8` directly (`jp2lam::Jp2Decoder` session, thread-local per worker,
  retaining Tier-1 scratch and a cached Rayon pool). Streams with any non-8-bit
  component fall back to `NativePlanarI32` + the historical
  `v >> (precision - 8)` interleave (the two paths scale >8-bit samples
  differently; the fallback keeps bytes bit-exact with the legacy adapter).
- `crates/pdf-image/src/lib.rs` — `DecodeParameters` gains
  `target_size: Option<(u32, u32)>` (a hint, not a limit; non-scaling codecs
  ignore it).
- `crates/pdf-render-cpu/src/prepared.rs` — `lower_image` computes the draw's
  device footprint (`codec_target_size`, the unclamped device bbox of the placed
  unit square) before decode and passes it to the base image, its soft mask, and
  any stencil mask (they share the placement CTM). The profiling `DecodeCacheKey`
  gains the hint so a reduced decode for one scale is never served for another.

### Direct-path support (checked against jp2lam)

jp2lam's `decode_packed_direct` handles single-tile, non-palette streams whose
container colour space matches the requested format; multi-tile/palette streams
fall back inside jp2lam to native decode + `pack_image_8bit`. For **8-bit**
components both jp2lam paths and the legacy renderer interleave compute the same
byte (`(sample + 2^(p-1) + 0.5).clamp(0, 2^p-1)`, then identity for p=8), so the
adapter routes every 8-bit stream through the packed request regardless of tile
layout and stays byte-identical. Only non-8-bit precision diverges, and that is
exactly the case routed to the native fallback.

## Measurement protocol

Paired A/B, `tools/pdfium-diff` release. The **baseline** binary is the current
working tree (which already contains other agents' jp2lam + JPEG work) with only
this task's `jpx.rs` restored to its pre-integration legacy form — the `lib.rs`
and `prepared.rs` plumbing is inert to the legacy codec, which ignores
`params`. This isolates *this task's* marginal effect on top of the other
agents' in-tree work. No git state was touched (see
`results/optimization-jpx-integration-20260720/metadata.txt`).

Pages (corpus root `/mnt/Samsung980_1TB`, scale 2.0833333): **jpx-scan**
(`…/Axel Boethius- Etruscan and Early Roman Architecture.pdf` p1, RGB JPX) and
**mrc-jpx-jbig2** (`to-sort/Zen-essence.pdf` p0, MRC: two JPX foregrounds +
JBIG2 mask). 12 runs per mode, `--mode decode-only` and `--mode compiled`,
alternating binary order. Whole-document control: the 74-page JPX-free Latin
document via `pipeline-profile` (16 runs/binary, order alternated). Host:
i7-13700H, 20 logical cores.

`--mode compiled` is the production render path (real per-draw footprints →
Phase-2 reduction engages). `--mode decode-only` decodes payloads full-resolution
in a single-threaded warm loop (no footprint), isolating Phase 1 and exposing the
concurrency tradeoff below.

## Phase 1 — hash preservation

With the reduction hint disabled (`PDF_RENDERER_JPX_REDUCE=0`, a measurement
switch on the shipping binary), the compiled render output hash is
**byte-identical** to the required values:

| Page | Required hash | Measured (reduction off) |
|---|---|---|
| jpx-scan | `7936df7b287ba862` | `7936df7b287ba862` ✓ |
| mrc | `066c3477e187b89c` | `066c3477e187b89c` ✓ |

Phase 1 (packed output + native fallback) changes no pixels.

## Phase 2 — eligibility policy and margin

The device footprint is passed to jp2lam's `AtLeast { width, height,
quality_margin }`, which selects the largest wavelet reduction whose reduced
dimensions still meet or exceed `footprint × margin` on **both** axes. This is
inherently conservative: magnified and near-1:1 draws never reduce (a level
would drop below the destination). Soft/stencil masks reuse the base draw's
footprint.

**Margin choice — 1.0, chosen by measurement.** The task's estimate of 13.42
source texels per destination pixel did not hold: the profiled pages minify only
~2.4–2.9× on the binding axis, so at most **one** wavelet level is
quality-available. Margin sweep (compiled mode, decoded-pixel count shows the
reduction, severity is the correctness evidence):

| margin | jpx-scan reduce | jpx-scan sev | mrc reduce | mrc sev |
|---:|---|---:|---|---:|
| 1.00 | r=1 (6.56M→1.64M px) | 0.000096 | yes (13.29M→8.57M px) | 0.003781 |
| 1.10 | r=1 | 0.000096 | none | 0.003815 |
| 1.20 | r=1 | 0.000096 | none | 0.003815 |
| 1.35 | none | 0.000042 | none | 0.003815 |

Margin **1.0** is the principled floor — never decode below one texel per
destination pixel — and, because reduction is discrete (each level halves), the
decoded image still lands between 1× and 2× the destination in practice (real
supersample headroom). It engages one level on **both** hotspot pages. Severity
stays far under the 0.005 budget on both, and on the demanding MRC scan it is
*below* its full-resolution baseline (the reduced decode aligns better with
PDFium's own downsampling). Larger margins forego the MRC reduction for no
severity gain. `quality_margin` is overridable via `PDF_RENDERER_JPX_MARGIN`.

### Correctness (compiled, reduction on — pixels intentionally change)

| Page | Baseline severity | Optimized severity | Budget | Hash (changes) |
|---|---:|---:|---:|---|
| jpx-scan | 0.000042228 | **0.000096367** | ≤ 0.005 ✓ | `7936…`→`e046caf17a3d75bf` |
| mrc | 0.003815003 | **0.003780674** | ≤ 0.005 ✓ | `066c…`→`f2dc38c07f3ed0c4` |

Hashes change when reduction engages — expected; severity is the correctness
evidence.

## Performance (paired medians)

### Compiled — the production path (Phase 1 + Phase 2)

| Page | Metric | Baseline | Optimized | Δ |
|---|---|---:|---:|---:|
| jpx-scan | jpx decode | 108.05 ms | 39.87 ms | **−63.1%** |
| jpx-scan | bench total | 142.66 ms | 66.76 ms | **−53.2%** |
| jpx-scan | peak RSS | 143.6 MiB | 65.8 MiB | **−54.2%** |
| mrc | jpx decode | 121.31 ms | 46.15 ms | **−62.0%** |
| mrc | bench total | 181.29 ms | 100.31 ms | **−44.7%** |
| mrc | peak RSS | 139.2 MiB | 50.8 MiB | **−63.5%** |

The RSS drop confirms the elimination of the planar-`i32` intermediates and the
renderer-side interleave buffer; the decode-time drop is Phase 2 reduction plus
Phase 1 packing.

### Decode-only — full-resolution, single-threaded (the concurrency tradeoff)

| Page | Baseline | Optimized | Δ |
|---|---:|---:|---:|
| jpx-scan | 102.82 ms | 141.65 ms | +37.8% |
| mrc | 119.50 ms | 144.59 ms | +21.0% |

At **full** resolution the optimized path is slower: baseline `decode_jp2()`
runs Tier-1 on jp2lam's global Rayon pool (all cores), whereas the codec now
bounds each decode (`Budgeted(2)`, below) to protect the parallel render
scheduler from oversubscription. This is a deliberate throughput-over-latency
tradeoff on the rare full-resolution JPX draw; in production these
scanned-book pages minify and take the reduction path, where the compiled
numbers above apply.

### Concurrency policy — `Budgeted(2)`

Render workers already parallelise across draws, so each decode is bounded
rather than grabbing the machine. Sweep (jpx decode median):

| concurrency | jpx decode-only | jpx compiled | mrc compiled |
|---|---:|---:|---:|
| Serial | 164.9 ms | 48.0 ms | 53.9 ms |
| **Budgeted(2)** | 147.7 ms | **38.5 ms** | **46.6 ms** |
| Budgeted(4) | 132.0 ms | 35.8 ms | 47.2 ms |

`Budgeted(2)` recovers most of a single decode's internal parallelism (compiled
jpx-scan 48→38.5 ms vs Serial) at ≤2× thread cap; `Budgeted(4)` gains little
more and slightly regresses mrc. Overridable via `PDF_RENDERER_JPX_CONCURRENCY`.

### Whole-document control (74-page Latin, JPX-free)

| Metric | Baseline | Optimized |
|---|---:|---:|
| Median total | 605.15 ms | 594.99 ms |
| Median peak RSS | 667.1 MiB | 691.9 MiB |
| Pages | 74/74 | 74/74 |

The document has no JPX, so the thread-local decoder is never constructed;
totals and RSS differ only within run-to-run noise. No scheduler or memory
regression.

## Correctness gates

- `cargo test -p pdf-image` — all pass (jpx_codec extended with packed-Gray8,
  16-bit native-fallback bit-exactness, and a reduced-decode-via-`target_size`
  test).
- `cargo check --workspace --all-features` — clean.
- Phase-1 hashes byte-identical; Phase-2 severity ≤ 0.005 on both pages.

## Remaining opportunities

- **Full-resolution decode latency.** For a JPX shown near 1:1, bounded
  concurrency is slower than the baseline global pool. A load-aware budget (more
  threads when the render pool is idle) would recover it without oversubscription.
- **ROI / region decode.** jp2lam's `DecodeRegion` is not implemented
  (`region: None` only). Once it lands, clipped JPX draws could decode just the
  visible precincts — a further decode and RSS win for partially-clipped scans.
- **Multi-tile packed direct.** jp2lam's zero-copy interleaver is single-tile;
  multi-tile 8-bit streams fall back to native + pack. Extending it removes one
  more full-image copy for tiled scans.
- **Second reduction level.** These pages only minify ~2.5×. Pages placed
  smaller (thumbnails, N-up) would reach r=2 automatically under the same
  margin-1.0 policy.
