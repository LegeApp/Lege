# Opaque axis-aligned area-minification fast path (RGB8 + CMYK) — 2026-07-20

Closes the whole-document Latin bottleneck found by heads-up profiling: page 0
of `Buddhist_Ethics.pdf` (the `latin-text` corpus doc) spent `render.image` =
420 ms of a 475 ms `render.total` on a single large cover image being
area-minified onto the page (`image.source_sample_taps` 10.67 M for 1.12 M
destination pixels ≈ 9.5 taps/px). The renderer had a fast path for opaque
axis-aligned RGB8 *nearest* (magnified/1:1) and for bilevel SAT box-filtering,
but **no fast path for an opaque axis-aligned image being area-minified** — that
case fell into the generic per-destination-pixel `shade()` loop.

Raw paired rows: `results/optimization-rgb8-areamin-20260720/` (+ `metadata.txt`).
Baseline = clean HEAD `5e31d9a`; no git state touched.

## Root cause

`exec.rs`'s `fast_rgb8` gate requires `footprint ≤ 1` (nearest, magnified). A
minified opaque axis-aligned image (footprint > 1) fails it, is not bilevel, and
lands in the generic `else` loop, which per destination pixel calls
`img.shade()` → `area_average()` with the full setup, `average_rgba`, mask/smask
dispatch, and per-pixel source-over. **Correction to the task brief:** the page-0
cover is **CMYK**, not RGB8 (`image.cmyk_draws = 1`). That matters: the generic
`area_average` has a fast byte-sum branch *only for RGB8*; a CMYK source falls to
the slowest branch, calling `pixel()` — which does `cmyk_to_rgb` in f32 — for
**every one of the 10.67 M taps**. That per-tap colour conversion is the 420 ms.

## What changed

Two dispatch gates + fast paths in `crates/pdf-render-cpu/src/exec.rs`, plus a
source converter in `image.rs`, all sharing one box-average core:

- **`fast_rgb8_area`** — the area-minified twin of `fast_rgb8`: same eligibility
  (opaque, unmasked, no soft/hard mask, not stencil, bpc 8, no `/Decode`, RGB,
  axis-aligned) but `footprint > 1` on at least one axis. Off-diagonal inverse
  terms are required **exactly zero** (`inv.b == 0.0 && inv.c == 0.0`) so the
  per-column `u = inv.a·dx + inv.e` and per-row `v = inv.d·dy + inv.f` reproduce
  the generic per-pixel `inv.apply` bit-for-bit (adding `inv.c·dy = 0.0` is the
  identity on a finite float — a page-placed scaled image inverts to exactly-zero
  off-diagonals). Interpolation is intentionally *not* checked: minification
  ignores it (`shade` area-averages regardless of the Nearest/Bilinear hint), so
  a minified Bilinear draw is also served.
- **`fast_cmyk_area`** — the same, for CMYK. The source is converted to a packed
  RGB8 buffer **once** (`image.rs::cmyk_source_as_rgb8`, each pixel through the
  *identical* `cmyk_to_rgb` + `to_u8` as the generic `pixel()`), then handed to
  the same box-average core. This is byte-identical to the generic
  convert-each-tap-then-average — each source pixel yields the same RGB triple —
  but the conversion is hoisted out of the per-destination-pixel loop into one
  tight pass, and the averaging becomes plain byte sums. Bounded by a 64-Mpx cap
  (192 MiB); above it the generic path runs unchanged.

Both fast paths prepare per-column source X boxes and per-row source Y boxes
once (like the shipped CCITT SAT path), then for each destination pixel sum the
inclusive source RGB box and write it opaque — no per-pixel float shade
dispatch, `average_rgba`, mask test, or source-over blend (opaque source-over is
a copy). The box bounds, the uniform-weight sum, and the integer-truncating
divide mirror `area_average` exactly.

### SAT vs direct

I used **direct row-band accumulation**, not a summed-area table. For this page
the footprint is ~3×3 (9.5 taps/px), and the source pixel count ≈ the tap count,
so a 3-channel `u32` SAT would cost about as much to *build* as the direct sum
costs in total, while adding ~120 MiB and random-access lookups. Direct
streaming of the source rows wins here. (The CMYK conversion, being once per
source pixel, is the dominant residual cost regardless of SAT vs direct — see
below.)

## HARD GATE — byte-identical output

Page 0, `pdfium-diff profile 2.0 Buddhist_Ethics.pdf 0 … --mode compiled`:

| | baseline `5e31d9a` | optimized |
|---|---|---|
| Output hash | `d367047299c4b294` | `d367047299c4b294` ✓ |
| PDFium severity | 0.009583492 | 0.009583492 ✓ |
| `image.source_sample_taps` | 10 670 610 | 10 670 610 (identical) |
| fast path fired | — | `image.fast_cmyk_area_min_pixels = 1 119 738` |

Byte-identical, severity unchanged. Backed by a fuzz test (`rgb8_area_min_tests`)
asserting both the RGB8 and CMYK fast paths reproduce the generic
`shade()`-based area-average + opaque write, pixel-for-pixel and painted-count,
over randomized images across six minifying footprints.

## Performance

### Page 0 (compiled, paired, 24 runs/binary, alternating order)

| Metric | baseline | optimized | Change |
|---|---:|---:|---:|
| `render.image` | 417.1 ms | 132.3 ms | **3.15× faster** |
| `render.total` | 468.2 ms | 187.0 ms | **2.50× faster** |
| peak RSS | 54 MiB | 68 MiB | +14 MiB (transient RGB8 convert buffer) |

The residual 132 ms is the CMYK→RGB conversion of the source pixels, which
byte-identity forbids reducing (averaging-before-converting is a *different*
result for the non-linear CMYK→sRGB map — the rounding trap the brief warned of).
It is now a single tight pass instead of 10.67 M per-tap conversions wrapped in
the generic machinery.

### Whole-document Latin (74 pages) — the goal

Heads-up `pdfium-diff bench …/libpdfium.so 2.0 Buddhist_Ethics.pdf 10` and paired
`pipeline-profile` (18 runs/binary):

| Metric | baseline | optimized |
|---|---:|---:|
| page-0 bench ratio vs PDFium | 4.7× slower | 1.7× slower |
| whole-doc throughput (pipeline median) | 131.1 pages/s | **305.0 pages/s** |
| whole-doc throughput (bench, single) | 128.5 pages/s | 277.5 pages/s |
| vs PDFium (~203–240 pages/s here) | 1.58× slower | **1.36× faster** |
| median document total | 564.6 ms | 242.6 ms |
| peak RSS | 617 MiB | 645 MiB (+28 MiB, transient) |

Whole-document Latin crosses PDFium's throughput and clears the ~240 pages/s
target: **131 → 305 pages/s (2.33×)**. The +28 MiB peak is the transient
per-draw RGB8 conversion buffer for the page-0 cover (bounded, freed after the
draw).

## Other corpus image pages — checked, no regression

All hashes unchanged; none newly hit the area paths on the profiled pages, for
principled reasons:

| Page | scale | space | why | new path? | hash |
|---|---|---|---|---|---|
| jpeg-scan-viewer | 1.0 | RGB | magnified/1:1 → nearest fast path | no | `00b76c2f077f9c82` unchanged |
| jpeg-scan-sweep | 2.083 | RGB | magnified → nearest fast path | no | `90d3173f5dca6b9b` unchanged |
| mrc-jpx-jbig2 | 2.083 | RGB | minified layer is **masked** (MRC) → generic | no | `f2dc38c07f3ed0c4` unchanged |

The `fast_rgb8_area` path is correct, tested, and ready for opaque *minified*
RGB scans (common in real documents), but the profiled corpus RGB pages happen
to be magnified (→ nearest) or masked (→ generic). The `fast_cmyk_area` path is
the one that fires on the target and delivers the win.

## Gates

- `cargo test -p pdf-render-cpu` — 111 passed, 0 failed (adds `rgb8_area_min_tests`:
  RGB8 and CMYK byte-identity fuzz).
- `cargo check --workspace --all-features` — clean.
- Page-0 hash `d367047299c4b294` / severity 0.009583492 byte-identical; other
  image pages' hashes unchanged.

## Summary

An opaque axis-aligned area-minification fast path (RGB8 direct + CMYK
convert-once-then-box-average), byte-identical to the generic area-average, cuts
the Latin cover page's `render.image` 3.15× and lifts whole-document Latin
throughput **131 → 305 pages/s**, past PDFium. The page-0 image was CMYK (not
RGB8 as briefed); the CMYK variant is what closes the bottleneck, and the RGB8
variant is in place for minified RGB scans.
