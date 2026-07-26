# CCITT and Type 1/bare-CFF optimization pass — 2026-07-20

This pass starts from image-sampling commit
`8e12132` and addresses priorities 3 and 5 from
`optimization-20260720.md`:

1. remaining CCITT bilevel minification/compositing work;
2. repeated Type 1 and bare-CFF font preparation.

Raw focused and paired measurements are under
`results/optimization-ccitt-type1-20260720/`.

## CCITT: prepared axis-aligned bilevel filtering

The prior pass still performed device-to-source affine and footprint
calculation inside each destination-pixel call. The new path applies when an
image is:

- one bit per component;
- axis aligned and minified;
- backed by the prepared two-entry Gray/Indexed/Tint lookup table;
- free of resource masks and soft masks.

It now:

- prepares the inclusive source X box once per destination column;
- prepares the inclusive source Y box once per destination row;
- counts packed source bits directly for each box;
- mixes the two prepared RGBA entries without generic color dispatch;
- writes fully opaque pixels directly and retains exact source-over behavior
  for clip-edge coverage;
- detects path clip masks that are entirely opaque and can be treated as
  rectangular.

The CCITT corpus page sent 1,964,690 painted pixels through the new path.

### Paired CCITT result

Two alternating-order batches produced 30 compiled-page rows per binary.
Both binaries used the same corpus page and retained PDFium comparison.

| Metric | `8e12132` | Optimized | Change |
|---|---:|---:|---:|
| Median compiled total | 111.226 ms | 70.935 ms | **1.57× faster** |
| Mean compiled total | 112.490 ms | 70.882 ms | **37.0% lower** |
| Median `render.image` | 105.211 ms | 65.156 ms | **1.61× faster** |
| Output hash | `533b4e49e12ab195` | `533b4e49e12ab195` | unchanged |
| PDFium severity | 0.032997166 | 0.032997166 | unchanged |

Against the original 2026-07-19 measurement of 242.339 ms, the two image
sampling passes together reduce the CCITT page to about 71 ms, or **3.42×**.

## Type 1 and bare CFF: repeated-page parsed-program residency

Added profiling split Type 1 lowering into font parsing, outline extraction,
and outline emission. It showed that a representative first preparation spent
about 129 ms parsing 19 embedded font programs, while extracting 901 glyph
outlines and emitting their flattened geometry together took roughly 1.5 ms.

The CPU worker now retains parsed programs when:

- the next request uses the exact same `CompiledPage`; and
- the program is native Type 1 or a bare CFF that required SFNT wrapping.

Ordinary TrueType and already-wrapped OpenType programs are not retained. A
worker moving to another page discards the page-local map, avoiding a
document-sweep cache that grows with every page.

### Paired Type 1 result

Two alternating-order batches produced 40 compiled-page rows per binary.

| Metric | `8e12132` | Optimized | Change |
|---|---:|---:|---:|
| Median compiled total | 156.601 ms | 12.840 ms | **12.20× faster** |
| Median lowering | 145.294 ms | 1.866 ms | **77.9× faster** |
| Warm optimized median, excluding first render | — | 12.821 ms | — |
| First optimized render median | — | 165.514 ms | cold parse retained |
| Cached programs | — | 19 | expected |
| Output hash | `4cd8639fb0f5077c` | `4cd8639fb0f5077c` | unchanged |
| PDFium severity | 0.031789505 | 0.031789505 | unchanged |

This is intentionally a repeated-render optimization. The first request still
performs the same parser work and seeds the worker-local cache.

## Whole-document control

The 74-page Latin-text document was run 20 times per binary with both
execution orders alternated. This document does not repeatedly render the
same compiled page, so the page-local Type 1/bare-CFF cache should remain
effectively dormant.

| Metric | `8e12132` | Optimized | Change |
|---|---:|---:|---:|
| Median total | 868.429 ms | 846.090 ms | 2.6% lower |
| Mean total | 856.972 ms | 848.005 ms | 1.0% lower |
| Median throughput | 85.22 pages/s | 87.46 pages/s | 2.6% higher |
| Median peak RSS | 532.1 MiB | 487.7 MiB | 8.3% lower |
| Successful pages | 74/74 | 74/74 | unchanged |

The absolute whole-document values varied with machine load, so the
alternating paired comparison is the relevant control. It shows no throughput
or memory regression from the repeated-page font cache.

## Remaining renderer-side work

- CCITT still spends about 65 ms in image execution. The next step would be a
  row-span bilevel compositor or a separable horizontal box-filter buffer, but
  the remaining absolute opportunity is now much smaller.
- Type 1 first-render latency is unchanged. Reducing it requires moving parsed
  font ownership earlier than CPU request preparation or adding a
  document-scoped resource cache.
- Already-wrapped OpenType/CFF outline extraction across different pages is
  deliberately not cached here; doing so needs a document-scoped identity and
  bounded glyph cache rather than a worker cache that retains every page.
