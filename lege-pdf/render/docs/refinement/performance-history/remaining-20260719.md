# Remaining profiling measurements — 2026-07-19

This completes the outstanding measurements from sections 1–4 of
`optimization-plan.md`. It is still pre-optimization: the decoded-image cache
is compiled only by the `profiling` feature and exists to measure warm
residency, not as a production cache.

Raw structured rows and focused flamegraphs are in
`results/remaining-20260719/`. Oracle-free page RSS rows are in
`results/rss-20260719/`. Existing DHAT profiles remain in
`results/baseline-20260719/`.

## What was added

- `warm-decoded`: populate profiling-only decoded residency once, then repeat
  CPU preparation and execution with codec-cache hits.
- `decode-only`: walk every unique codec payload reachable from the compiled
  page, including soft/hard masks and tiling cells, without geometry or raster
  work.
- `pipeline-profile`: structured full-document scheduler timing with worker
  counts, page success counts, true pre-reorder completion percentiles, and
  process RSS.
- Per-row Linux current RSS and high-water RSS.
- Optional retained PDFium differential metrics via
  `PDF_RENDERER_PDFIUM=/path/to/libpdfium.so`.
- Decode cache hits/misses, output codec formats, destination pixels, exact
  base-image source taps, minification/filter, color-space, mask, stencil, and
  affine classifications.
- Compilation scopes for page lookup, content gathering, resource resolution,
  interpretation, semantic finalization, and IR lowering, plus parsed-object
  and object-stream cache counters.
- Batch runner actions for the remaining modes and oracle-free RSS.

## Paired codec-versus-renderer result

All times below are same-build medians in milliseconds. `Saved` is compiled
render minus warm-decoded render. `Taps/dst` is the number of base-image
source texels interpreted per destination pixel. Small negative savings are
ordinary noise.

| Page class | Compiled | Warm decoded | Decode only | Saved | Saved % | Taps/dst |
|---|---:|---:|---:|---:|---:|---:|
| JPEG scan, viewer | 473.896 | 321.916 | 151.911 | 151.980 | 32.1% | 1.00 |
| JPEG scan, sweep | 1588.455 | 1432.637 | 150.601 | 155.818 | 9.8% | 1.00 |
| JPX scan | 523.678 | 223.110 | 291.791 | 300.568 | 57.4% | 13.42 |
| JPX/JBIG2 MRC | 597.972 | 305.145 | 311.575 | 292.827 | 49.0% | 5.00 |
| CCITT bilevel | 242.339 | 244.109 | 1.120 | -1.770 | -0.7% | 5.81 |
| Latin text | 25.747 | 25.043 | 0 | 0.704 | 2.7% | — |
| CJK CID text | 10.134 | 9.833 | 0 | 0.301 | 3.0% | — |
| Type 1 fonts | 111.385 | 110.830 | 0 | 0.555 | 0.5% | — |
| Vector diagram | 10.878 | 10.693 | 0 | 0.185 | 1.7% | — |
| Transparency group | 0.428 | 0.427 | 0 | 0.001 | 0.3% | — |
| Soft mask | 7.748 | 7.301 | 0 | 0.447 | 5.8% | 0.95 |
| Tiling pattern | 8.655 | 9.076 | 0 | -0.421 | -4.9% | — |
| Radial shading | 23.465 | 22.308 | 0 | 1.157 | 4.9% | — |

The decoded-cache invariant held in every sample: one hit for each JPEG, JPX,
and CCITT payload, three hits for MRC, and zero codec calls after warm-up.
Decode-only found one unique payload for each single-codec page and three for
MRC. The compiled-minus-warm delta closely tracks direct decode time.

The result is workload-specific rather than “codec-bound” in general:

- JPX is 57% codec decode, but its remaining renderer work still interprets
  13.4 source texels per output pixel.
- MRC is approximately half decode and half sampling/compositing, at 5.0
  source texels per output pixel.
- Viewer-scale JPEG spends 32% in DCT decode. At sweep scale the same decode
  remains about 151 ms but falls to 9.8%; destination-pixel rendering dominates.
- CCITT decode is only 1.1 ms. Nearly all of its 242 ms is the generic
  bilevel/minification sampling path.
- Type 1 remains a preparation problem unrelated to image codecs.

## Whole-document pipeline

The representative full-document control is the 74-page Latin-text document
at 2× with the default 7 compile and 13 render workers:

| Metric | Median |
|---|---:|
| Total | 633.8 ms |
| Throughput | 116.8 pages/s |
| Completion p50 | 129.7 ms |
| Completion p90 | 198.9 ms |
| Completion p99 | 630.0 ms |
| Successful pages | 74/74 |
| Process high-water RSS | 661.9 MiB |

Completion latency is captured when render workers send their result, before
the deterministic reorder buffer. Measuring at the public ordered callback
incorrectly made all percentiles cluster near total wall time.

## Memory

Oracle-free one-process captures give these notable high-water RSS values:

| Page class | Compiled RSS | Modeled peak |
|---|---:|---:|
| JPEG viewer | 166.7 MiB | 103.0 MiB |
| JPEG sweep | 417.0 MiB | 353.3 MiB |
| JPX scan | 171.3 MiB | 25.8 MiB |
| MRC | 154.6 MiB | 32.8 MiB |
| CCITT | 19.9 MiB | 15.8 MiB |
| Type 1 | 21.4 MiB | 14.8 MiB |

RSS confirms the DHAT conclusion: JPX and MRC decoder intermediates are the
large unmodeled peaks. The JPEG gap is mostly allocator/runtime overhead above
an otherwise accurate renderer-owned model. PDFium differential capture is
kept in separate processes because loading the oracle contaminates `VmHWM`.

## Retained differential scores

Every timing row retains output hashes and PDFium ink/gross/severity metrics.
The image controls are close to the oracle (severity from 0 to 0.0038), except
CCITT at 0.033. The larger existing correctness residuals remain Latin text
(0.089), vector (0.051), Type 1 (0.032), and radial shading (0.018). Performance
timings are valid, but those pages should not be treated as pixel-equivalent
to PDFium.

## Sampling-profile findings

- JPX compiled: 56% lowering/decode; 48% `jp2lam::decode_jp2`; 34% irreversible
  9/7 reconstruction; 37% image shading.
- JPX warm-decoded: 84% `PreparedImage::shade_with_taps`, 68% source-pixel
  interpretation. Decode disappears as intended.
- MRC compiled: 48% decode and 45% image shading, matching structured timing.
- Vector: 34% raster fill and 32% clip-mask construction; font outline
  extraction and lowering form a smaller tail.
- Whole document: 42% raster fill, 22% image shading, 22% preparation, and a
  visible repeated CFF outline-extraction/interpreter stack.

The focused artifacts are `jpx-compiled.svg`, `jpx-warm-decoded.svg`,
`mrc-compiled.svg`, `vector-compiled.svg`, and `whole-document.svg`.

## Measurement gate

Sections 1–4 now have cold, warm, compiled, prepared, warm-decoded,
decode-only, whole-document, hardware-counter, DHAT, RSS, differential, and
focused sampling data. The next work may select optimizations from measured
hotspots; no optimization has been made in this measurement pass.
