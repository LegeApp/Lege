# JPEG decode optimization review (pass 3) — 2026-07-20

Third optimization pass on the in-house `/DCTDecode` decoder in
`crates/pdf-image/src/jpeg/mod.rs`, starting from the pass-2 tree
(baseline MCU-row streaming pipeline + AVX2 IDCT). Raw paired measurements are
under `results/optimization-jpeg3-20260720/` (`metadata.txt` records the
baseline note, `rustc -vV`, and `lscpu`).

The baseline pdfium-diff binary was built from the CURRENT tree *before* this
pass's edits and copied aside; the optimized binary was rebuilt afterward. All
measurements are paired A/B on a 13th-gen i7-13700H.

## Attribution

The representative JPEG is the same 2758×3561 baseline 4:2:0 stream
(`to-sort/globalhistoryofm0000unse_1.pdf` page 0; Y 2×2, Cb/Cr 1×1; 9,821,238
pixels; 29,463,714 RGB output bytes). After pass 2 the decode-only cost was
dominated by two scalar hot loops the streaming pipeline still ran per output
row / per block: the integer YCbCr add-and-clamp colour conversion (a 64 KiB
green gather table) and the `(v + 128.5).clamp(0, 255)` band store. This pass
vectorizes both and adds a DC-only block shortcut.

## Changes (all kept — each wins, output byte-identical)

### 1. SIMD YCbCr→RGB colour conversion (`rgb_ycc_row_avx2`) — the big win

`assemble_row`'s `RowKind::Rgb(false)` path now dispatches (runtime
`is_x86_feature_detected!("avx2")`, scalar table path retained) to an AVX2
kernel for the two SIMD-friendly horizontal layouts, detected **once per image**
(`rgb_ycc_chroma_x`, O(w)): luma 1:1 with chroma either 1:1 (4:4:4) or 2:1
(4:2:2 / 4:2:0 — chroma index `x/2`, materialized gather-free by a
byte-duplication `pshufb`). Eight output pixels are processed per iteration; the
tail and all other samplings fall back to scalar.

The 64 KiB green table cannot be gathered lane-wise, so R/G/B are **recomputed
arithmetically**: each float sub-expression uses the exact operation order and
constants of `ycc_tables` with no FMA contraction, and the integer
add / level-shift / clamp mirrors `ycc_to_rgb` — including its two
saturated-edge `−1` green corrections (built as SIMD compare masks). This is
proven **byte-for-byte identical** to the scalar table path exhaustively over
all 16,777,216 `(Y, Cb, Cr)` triples by
`rgb_simd_tests::avx2_ycc_rgb_is_exhaustively_byte_exact`, plus a 2:1-chroma
duplication test.

### 2. SIMD band store (`store_band_block_avx2`)

The streaming pipeline's per-block `(v + 128.5).clamp(0, 255) as u8` store (8×8
IDCT samples → level-shifted bytes) is vectorized 8 lanes wide: add bias, clamp
`[0, 255]`, truncate toward zero (matching `.clamp(…) as u8` over the
already-in-range value), gather the low byte of each lane. Bit-identical to the
scalar reference, verified by `band_store_tests::avx2_band_store_matches_scalar`
(20k randomized rows including out-of-range clamp inputs).

### 3. DC-only block fast path

`decode_block_sequential` now returns whether it wrote any nonzero AC
coefficient. When a streaming block is DC-only (EOB / ZRL-only straight after
the DC term — common in the flat regions of scanned book pages), the pipeline
skips both the IDCT and the band store and fills the 8×8 band region with the
single constant `(coeffs[0]·dequant[0] + 128.5).clamp(0, 255)` byte. This is
exact: the inverse DCT of a DC-only block is that constant at every sample.
`dc_only_tests::dc_only_idct_is_flat_constant_and_matches_fast_path` proves it
across the full baseline DC range × several quant[0] values, for **both** IDCT
kernels (scalar and AVX2), and that the fast-path byte equals the band-store
byte.

## Correctness

Output is byte-for-byte unchanged on both metrics and both paths:

| Metric | Baseline (pass-2 tree) | This pass |
|---|---|---|
| Decode-only output hash (viewer 1.0) | `00b76c2f077f9c82` | `00b76c2f077f9c82` |
| Compiled page hash (sweep 2.0833) | `90d3173f5dca6b9b` | `90d3173f5dca6b9b` |
| PDFium severity | 0.000009712 | 0.000009712 |

Because both output hashes are unchanged, the pixels — and therefore any
pixel-derived severity — are provably identical; `pdfium_severity` reads
`9.71156784254987e-06` for both binaries.

`cargo test -p pdf-image` is all green: 31 (lib) + 9 + 7 + 7 tests, including
the exhaustive 16.7 M-triple scalar YCbCr test, the new exhaustive 16.7 M-triple
AVX2 YCbCr→RGB test, the DC-only equivalence test, the AVX2 band-store equality
test, the AVX2 IDCT bit-exactness test, and the streaming-vs-coefficient
equivalence fixtures.

## Paired results

### Decode-only (viewer page, scale 1.0) — 20 rows per binary

| Metric | Baseline | This pass | Change |
|---|---:|---:|---:|
| Median DCT decode | 73.926 ms | 42.093 ms | **43.1 % lower / 1.76×** |
| Mean DCT decode | 74.191 ms | 42.335 ms | 42.9 % lower |
| Output hash | `00b76c2f077f9c82` | `00b76c2f077f9c82` | unchanged |

### Compiled high-resolution page (sweep, scale 2.0833) — 10 rows per binary

| Metric | Baseline | This pass | Change |
|---|---:|---:|---:|
| Median compiled total | 234.492 ms | 208.707 ms | 11.0 % lower / 1.12× |
| Median DCT decode | 71.520 ms | 40.942 ms | **42.8 % lower / 1.75×** |
| Output hash | `90d3173f5dca6b9b` | `90d3173f5dca6b9b` | unchanged |
| PDFium severity | 0.000009712 | 0.000009712 | unchanged |
| Peak RSS (median) | 639.6 MiB | 639.5 MiB | ~unchanged |

The compiled *total* moves less than DCT decode because most of that wall time
is page rendering, not decode. Peak RSS is unchanged: this pass trades no memory
(all kernels operate on the bands / output the streaming path already held).

### Per-change attribution (decode-only, 16 runs each, one thermal window)

Isolated by rebuilding with each kernel's AVX2 dispatch selectively disabled.

| Build | Median DCT decode | Δ vs previous | vs baseline |
|---|---:|---:|---:|
| Baseline (pass-2: streaming + AVX2 IDCT) | 72.140 ms | — | — |
| + DC-only fast path (task 2) | 69.483 ms | −2.66 ms (−3.7 %) | −3.7 % |
| + SIMD band store (task 1, store) | 63.059 ms | −6.42 ms (−9.2 %) | −12.6 % |
| + SIMD YCbCr→RGB (task 1, colour) | 41.253 ms | −21.81 ms (−34.6 %) | **−42.8 %** |

All three win and are kept. The colour conversion dominates: on this 4:2:0 page
it was ~21.8 ms of scalar per-pixel table lookups + clamps, now vectorized. The
band store and DC-only shortcut are smaller but clearly positive and safe.

## What now dominates (task 3 — profiled, not started)

After this pass the ~41 ms decode-only cost is no longer colour conversion or
the level-shift store (both SIMD now) or the IDCT (AVX2 since pass 2, and
DC-only blocks now skip it entirely). What remains is inherently serial and
scalar: **Huffman entropy decoding** — the MSB-first `BitReader` (`fill` /
`peek8` / `decode`) walking the entropy stream one symbol at a time, plus the
`decode_block_sequential` run/size loop. This is the single largest remaining
component and the natural target for a pass 4.

Candidate directions (each would need the same byte-identity + paired-measure
discipline, and pass 2 already found a wider Huffman table *slower* — so this is
not a quick win):

1. Reduce per-symbol overhead in the bit reader — e.g. refill fewer times by
   widening the bit buffer to 64 bits, or branch-reduce the `0xFF` unstuffing
   in `fill` (the marker/stuffing check is per byte).
2. Batch the AC run/size decode (`decode_block_sequential`) to cut the
   per-coefficient `decode` + `read_bits` call overhead.

These are left for a dedicated pass rather than started here, per scope.

**Update 2026-07-21:** pass 4 landed in the production-readiness pass, taking
exactly these two directions (u64 bit buffer + batched AC run/size decode) for
a −7..16 % decode-time win across the JPEG classes.
