# Image sampling optimization pass — 2026-07-20

This pass starts from profiling baseline commit
`d164ebccf5ca39eb85117c89f39d87f739c264a0` and attacks the largest shared
renderer hotspot identified in `remaining-20260719.md`: generic per-tap image
sampling.

Raw focused measurements are in `results/optimization-20260720/`.

## Changes

- Added an opaque, unmasked, axis-aligned RGB8 nearest-neighbor blit.
  - Source X indices are prepared once per draw.
  - Decoded RGB bytes copy directly into the premultiplied RGBA surface.
  - Per-pixel affine application, generic sample dispatch, float conversion,
    alpha math, and source-over math are skipped when they are provably
    unnecessary.
- Added direct RGB8 byte accumulation for area minification.
- Added prepared sample-to-RGBA lookup tables for 1/2/4-bpc
  Gray/Indexed/Tint images.
  - `/Decode`, palette lookup, tint lookup, and color conversion move out of
    the source-tap loop.
  - The table is deliberately not built for ordinary 8-bit grayscale images,
    avoiding repeated preparation cost on text-heavy documents.
- Added packed 1-bit range counting for bilevel area averages.
- Added `image.fast_rgb8_nearest_pixels` profiling attribution.

All generic, masked, affine, high-bit-depth, interpolated, and unsupported
cases retain the existing fallback.

## Focused results

Times are same-machine medians in milliseconds. Baseline values are from
`results/remaining-20260719/`; optimized values are from
`results/optimization-20260720/`.

| Page class | Mode | Baseline total | Optimized total | Total speedup | Baseline image | Optimized image | Image speedup |
|---|---|---:|---:|---:|---:|---:|---:|
| JPEG sweep | compiled | 1588.455 | 337.880 | 4.70× | 1341.629 | 81.840 | 16.39× |
| JPEG sweep | warm decoded | 1432.637 | 171.118 | 8.37× | 1341.151 | 79.352 | 16.90× |
| JPX scan | compiled | 523.606 | 363.738 | 1.44× | 222.318 | 36.816 | 6.04× |
| JPX scan | warm decoded | 223.043 | 35.014 | 6.37× | 222.620 | 34.494 | 6.45× |
| JPX/JBIG2 MRC | compiled | 597.630 | 374.584 | 1.60× | 304.190 | 55.501 | 5.48× |
| JPX/JBIG2 MRC | warm decoded | 305.129 | 55.792 | 5.47× | 304.240 | 54.701 | 5.56× |
| CCITT bilevel | compiled | 242.339 | 89.828 | 2.70× | 238.004 | 85.030 | 2.80× |
| CCITT bilevel | warm decoded | 244.109 | 87.508 | 2.79× | 241.143 | 84.128 | 2.87× |

The JPEG sweep fast path handled all 42,629,574 destination pixels. The MRC
page used it for its 1,573,000-pixel unmasked layer while retaining the
general path for the masked/minified layer.

Decode-only timings varied upward by roughly 9–14% in this capture, but this
patch does not alter any codec or decode-only path. The renderer conclusions
therefore use compiled versus warm-decoded timing and the directly attributed
`render.image` duration.

## Correctness

All four focused page hashes are byte-for-byte unchanged:

| Page class | Output hash | PDFium severity |
|---|---|---:|
| JPEG sweep | `90d3173f5dca6b9b` | 0.000009712 |
| JPX scan | `7936df7b287ba862` | 0.000042228 |
| JPX/JBIG2 MRC | `066c3477e187b89c` | 0.003815003 |
| CCITT bilevel | `533b4e49e12ab195` | 0.032997166 |

The full workspace test suite passes with all features enabled.

## Whole-document control

The 74-page Latin-text control is noisy enough that one three-run batch
misleadingly looked slower. A paired A/B check used separate release binaries
for baseline and optimized code, 14 runs each, alternating both execution
orders. Raw rows are:

- `whole-document-paired-base.jsonl`
- `whole-document-paired-current.jsonl`

| Metric | Baseline | Optimized | Change |
|---|---:|---:|---:|
| Median total | 643.331 ms | 628.052 ms | -2.4% |
| Mean total | 645.579 ms | 632.434 ms | -2.0% |
| Median throughput | 115.0 pages/s | 117.8 pages/s | +2.4% |
| Median peak RSS | 523.0 MiB | 489.7 MiB | -6.4% |
| Successful pages | 74/74 | 74/74 | unchanged |

This preserves the scheduler/throughput control while materially improving
the image-heavy single-page workloads.

## New priority order

1. **JPX decode** now dominates the JPX compiled result; renderer image work
   fell from 222 ms to about 35 ms.
2. **JPX/JBIG2 decode and decoded intermediate memory** now dominate MRC;
   renderer image work fell from 304 ms to about 55 ms.
3. **CCITT masked minification/compositing** remains about 84–85 ms despite the
   2.8× image-stage gain. The next renderer-side experiment should combine an
   axis-aligned box-filter plan with opaque-clip detection or row-span
   compositing.
4. **JPEG decode plus surface/output materialization** now outweigh image
   sampling at sweep scale.
5. **Repeated Type 1/CFF preparation** remains the largest non-image target and
   was intentionally untouched in this pass.
