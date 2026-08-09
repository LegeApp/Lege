# MRC masked-draw fast path — 2026-08-08

MRC (mixed raster content) scans were the renderer's remaining high-latency
page class. The 2026-07-19 measurement pass attributed the MRC page to
"approximately half decode and half sampling/compositing"; with the production
`SharedImageCache` warm, the sampling half is *all* of it.

## What the measurement said

`pdfium-diff profile --mode compiled` on `Structures.pdf` p0 (2222×3191 JPX
foreground + JPX background + 1-bit JBIG2 `/SMask`, scale 2.0833333):

| Mode | Total | `render.image` |
|---|---:|---:|
| compiled | 235.4 ms | 234.4 ms |
| warm-decoded | 233.0 ms | 232.1 ms |
| decode-only (cold, full-res) | 239.4 ms | — (197.9 jpx + 41.5 jbig2) |

`compiled` and `warm-decoded` agree to within noise, so with the decode cache
warm the page is **99.6 % image shading**. The counters said why: of two draws
and 2,463,160 destination pixels, exactly half (`image.fast_rgb8_area_min_pixels
= 1231580`) took the axis-aligned area-minification fast path. The other
half — the masked foreground — did not, because every RGB8 fast path required
`img.smask.is_none() && img.mask.is_none()`.

An ablation that forced the masked draw onto the (incorrect, mask-ignoring)
fast path put the page at 56 ms, bounding the available win at ~4×. A second
ablation isolated the mask sampling itself at ~61 ms of the 234 ms, leaving
~173 ms in the generic per-pixel base sampling — i.e. the cost was the generic
loop's repeated setup, not the mask.

## What changed

`crates/pdf-render-cpu/src/exec.rs` — a new
`paint_axis_aligned_rgb8_area_min_masked`, the masked twin of
`paint_axis_aligned_rgb8_area_min_opaque`. Eligibility is the existing
`fast_rgb8_area` shape (Normal blend, no clip or soft mask, `alpha == 255`,
8 bpc RGB, no `/Decode`, minified, `inv.b == inv.c == 0`) with the mask
requirement lifted, plus `area_min_bilevel_mask` classifying the cut-out into
the two encodings MRC producers actually emit:

- **`AreaMinMask::Soft`** — a 1-bit grayscale `/SMask` coverage layer, box-
  filtered with the base image (Acrobat/Kakadu; `Structures.pdf`).
- **`AreaMinMask::Stencil`** — a hard `/Mask` over an `/ImageMask true` JBIG2
  bitmap, point-sampled all-or-nothing (ABBYY/LuraDocument; `Zen-essence.pdf`).

With `inv.b == inv.c == 0` the inverse map, the base image's two box-filter tap
ranges, and the mask's own per-axis lookup all depend only on the destination
column or row, so all four are prepared once per column and row instead of once
per pixel. What stays per pixel — the weighted box average, the mask's coverage
count or stencil test, the `/Decode` remap, and the source-over composite — is
the generic path's arithmetic term for term.

`crates/pdf-render-cpu/src/image.rs` — the shared bodies the two paths now
both call, so they cannot drift: `smask_footprint`, `smask_box_taps`,
`bilevel_smask_coverage`, `apply_smask_decode`, and
`stencil_col`/`stencil_row`/`stencil_hides_at`. `sample_smask` and
`stencil_hides` are rewritten over them and are behaviour-preserving.

### Edge anti-aliasing is kept, unlike the sibling paths

`fast_rgb8_area`, `fast_rgb8`, `fast_rgb8_bilinear`, and `fast_cmyk_area` all
skip `edge_coverage`, so they lose the A10 image-edge partial coverage on the
border ring. The first version of this path did the same and moved the page
hash (188 of 1,231,580 pixels differed, all on the outer border, max delta 32).

`edge_coverage` returns `None` — provably interior — exactly when the mapped
pixel square stays inside the unit square on both axes, and for an axis-aligned
placement that test *separates*. The interior is therefore a rectangle of whole
destination columns and rows: the prepared body runs there and the border band
keeps the generic per-pixel treatment. The draw is byte-identical, edges
included.

### `#[inline(never)]` is load-bearing

The new painter is large enough that LLVM inlined it into `paint_image` and
degraded the *sibling* fast paths' codegen: the unmasked `jpx-scan` page
regressed 22.6 → 31.1 ms in `render.image` with identical counters and an
identical code path. `#[inline(never)]` on a once-per-draw function restores it
(23.1 ms) and costs nothing.

## Results

Paired A/B, `pdfium-diff profile --mode compiled`, 9–15 runs per binary,
alternating binary order. Baseline is a `git worktree` build of `ef8b706`.
Host: i7-13700H, 20 logical cores.

| Page | Baseline | Optimized | Δ | Hash |
|---|---:|---:|---:|---|
| **mrc-jpx-jbig2** (`/Mask` stencil) | 144.16 ms | **12.63 ms** | **−91.2 %** | unchanged |
| **mrc-smask** (`Structures.pdf` p0, `/SMask`) | 228.86 ms | **69.39 ms** | **−69.7 %** | unchanged |
| jpeg-scan-viewer | 37.18 ms | 36.80 ms | −1.0 % | unchanged |
| jpeg-scan-sweep | 160.99 ms | 158.77 ms | −1.4 % | unchanged |
| jpx-scan | 23.35 ms | 23.28 ms | −0.3 % | unchanged |

The stencil page wins more because its mask test collapses to one bit read per
pixel, while the soft mask still box-filters its coverage.

The remaining corpus pages (`ccitt-bilevel`, `latin-text`, `cjk-cid-text`,
`type1-fonts`, `vector-diagram`, `transparency-group`, `soft-mask`,
`tiling-pattern`, `radial-shading`) live under a `pdfium-port-plan` root that is
not present on this host and were not re-measured; none of them reach
`paint_image`'s image draw path with a bilevel mask.

## Correctness gates

- **New equivalence test** `exec::rgb8_area_min_tests::masked_fast_path_matches_generic_shade`:
  the fast path must reproduce the generic `edge_coverage`/`shade`/
  `shade_clamped` body byte for byte, over fuzzed sources, six minifying
  footprints, three independent mask geometries (same as / coarser than / finer
  than the base), both mask encodings, and `/Decode [1 0]` polarity.
- **Byte-identity sweep**: 82 pages rendered by baseline and optimized `pdfr`
  (both MRC documents at pages 0–55, plus 25 assorted PDFs at pages 0/2/7/15) —
  **0 differ**.
- `cargo test -p pdf-render-cpu -p pdf-image -p pdf-content -p
  pdf-render-scheduler -p pdf-postprocess -p pdf-chaos-tests` — all pass.
- `cargo clippy -p pdf-render-cpu -p pdf-cli` — no warnings in the changed crate.
- Page hashes byte-identical on every measured page (see the table).

## Remaining opportunities

- **Colour-key `/Mask`.** `ImageMask::ColorKey` tests the base image's own
  samples, so it does not separate per axis and still takes the generic path.
  Rare on scans.
- **Non-minifying soft masks.** `sample_smask` point-samples when the mask does
  not minify; that shape is turned away here and keeps the generic path.
- **Gray8 and CMYK MRC foregrounds.** The path is RGB8-only, mirroring
  `fast_rgb8_area`. A CMYK MRC foreground would need the
  `cmyk_source_as_rgb8` conversion first, as `fast_cmyk_area` does.
- **Summed-area mask coverage.** `bilevel_smask_coverage` is O(footprint) per
  pixel. At the ~2.4×2.4 footprints these pages minify by, the existing
  `BilevelIntegral` would not pay for its build; a heavily minified mask
  (thumbnails, N-up) would.
- **Cold decode.** With the image cache cold the page still pays ~198 ms of JPX
  and ~42 ms of JBIG2. That is the decoders' problem, not the renderer's, and
  is unchanged by this pass.
