# JPEG decode optimization review (pass 2) — 2026-07-20

Second optimization pass on the in-house `/DCTDecode` decoder in
`crates/pdf-image/src/jpeg/mod.rs`, starting from commit `1bb3579` (the first
pass, "prepared component addressing + byte-exact integer YCbCr", which took the
decode-only median from 187 → 122 ms).

Raw paired measurements are under `results/optimization-jpeg2-20260720/`
(`metadata.txt` records the baseline commit, `rustc -vV`, and `lscpu`).

## Attribution

The representative JPEG is the same 2758×3561 baseline 4:2:0 stream
(`to-sort/globalhistoryofm0000unse_1.pdf` page 0; Y 2×2, Cb/Cr 1×1; 9,821,238
pixels; 29,463,714 RGB output bytes). Before this pass the decoder ran three
whole-image passes:

1. entropy-decode **all** blocks into `i16` coefficient storage (~30 MiB);
2. a separate whole-image IDCT into per-component sample planes (~15 MiB);
3. a third pass to upsample + colour-convert + assemble RGB (~29 MiB output).

The scalar float AAN IDCT was the dominant remaining CPU cost after pass 1, and
the coefficient + plane buffers were pure intermediate memory.

## Changes

### 1. Baseline MCU-row streaming pipeline (CPU + memory)

`Decoder::scan_sequential_streaming` reconstructs a single interleaved (or
single-component 1×1) sequential scan **one MCU row at a time**: it entropy-
decodes and IDCTs that row's blocks into a small per-component sample *band*
(only `v·8` rows tall), then upsamples + colour-converts + emits the output rows
immediately. It never allocates the full coefficient or plane buffers.

- Coefficient storage is now **lazy** (`ensure_coeffs`): only the paths that
  genuinely revisit blocks — progressive scans, and non-interleaved sequential
  scans — materialize it. `parse_sof` no longer allocates it eagerly.
- Eligibility (`can_stream`): `!progressive && ns == ncomp` and, for a single
  component, `h == v == 1`. Restart markers (DRI/RSTn) resync exactly as in the
  coefficient path. Progressive and non-interleaved sequential JPEGs keep the
  full coefficient path unchanged.
- The per-pixel colour/assembly code is now shared: both the streaming path and
  the whole-image `assemble` call one `assemble_row` (driven by an
  `output_layout`-derived `RowKind`), and both call one `decode_block_sequential`
  block decoder, so the two paths are guaranteed byte-identical by construction.

### 2. AVX2 IDCT (CPU)

`idct_block` dispatches (via `is_x86_feature_detected!("avx2")`, scalar
`idct_block_scalar` retained as reference/fallback) to `idct_block_avx2`: the 8
columns become the 8 SIMD lanes for the column pass, an 8×8 transpose, the 8
rows become the lanes for the row pass, and a transpose back. Each lane runs the
*exact* scalar op sequence — same multiply/add/subtract order, no FMA
contraction — and the transposes are pure data movement, so the result is
bit-for-bit identical to the scalar kernel. This is verified by
`idct_tests::avx2_matches_scalar_bit_exact` (5,000 random blocks, `f32`
bit-equality).

### 3. Wider Huffman fast table — measured and **dropped**

A 12-bit primary lookup table (vs the existing 8-bit) was implemented and
measured: it came out **2.1 % slower** on decode-only (85.7 vs 84.0 ms median),
the larger 8 KiB-per-table footprint costing more in cache than it saved in
slow-path branches. Reverted; the 8-bit table stands.

## Correctness

Output is byte-for-byte unchanged on both metrics and both paths:

| Metric | Baseline `1bb3579` | This pass |
|---|---|---|
| Decode-only output hash (viewer 1.0) | `00b76c2f077f9c82` | `00b76c2f077f9c82` |
| Compiled page hash (sweep 2.0833) | `90d3173f5dca6b9b` | `90d3173f5dca6b9b` |
| PDFium severity | 0.000009712 | 0.000009712 |

- `idct_tests::avx2_matches_scalar_bit_exact` — AVX2 == scalar, bit-exact.
- `streaming_tests` — the streaming and coefficient paths produce identical
  bytes on a baseline **4:2:0 stream with mid-row restart markers**
  (`base420_restart.jpg`, new fixture, restart interval 4 over `mcus_x == 5`),
  4:2:2, restart grayscale, and CMYK/YCCK fixtures. The test forces the
  coefficient path via the `Decoder::allow_stream` hook.
- The exhaustive 16,777,216-combination YCbCr byte-exactness test and all other
  `pdf-image` tests pass (`cargo test -p pdf-image`: 27 + 9 + 7 + 4 green).
- `cargo check --workspace --all-features` is clean (the SIMD `unsafe` carries
  justified `#[allow(unsafe_code)]`, matching the workspace `unsafe_code = warn`
  policy).

## Paired results

Paired A/B, alternating first-runner across two batches, on a 13th-gen i7-13700H.

### Decode-only (viewer page, scale 1.0) — 20 rows per binary

| Metric | Baseline | This pass | Change |
|---|---:|---:|---:|
| Median total | 105.777 ms | 86.074 ms | **18.6 % lower / 1.23×** |
| Mean total | 106.284 ms | 85.467 ms | 19.6 % lower |
| Median DCT decode | 103.263 ms | 84.718 ms | **18.0 % lower / 1.22×** |
| Output hash | `00b76c2f077f9c82` | `00b76c2f077f9c82` | unchanged |

### Compiled high-resolution page (sweep, scale 2.0833) — 10 rows per binary

| Metric | Baseline | This pass | Change |
|---|---:|---:|---:|
| Median compiled total | 283.723 ms | 271.935 ms | 4.2 % lower / 1.04× |
| Median DCT decode | 104.519 ms | 86.555 ms | **17.2 % lower / 1.21×** |
| Output hash | `90d3173f5dca6b9b` | `90d3173f5dca6b9b` | unchanged |
| PDFium severity | 0.000009712 | 0.000009712 | unchanged |
| Peak RSS (median) | 652.2 MiB | 609.9 MiB | **−42.3 MiB** |

The compiled *total* moves less than DCT decode because most of that wall time
is page rendering, not decode; DCT decode is the metric this pass targets.

### Kernel attribution (decode-only, 16 runs each, one thermal window)

| Build | Median DCT decode | vs baseline |
|---|---:|---:|
| Baseline (no streaming, scalar IDCT) | 110.24 ms | — |
| Streaming + scalar IDCT | 95.36 ms | −13.5 % |
| Streaming + AVX2 IDCT (this pass) | 85.38 ms | **−22.6 %** |

Streaming alone gives −13.5 % CPU plus the memory win; the AVX2 IDCT adds a
further −10.5 % (≈10 ms) over the scalar-streaming build. Both kernels win and
are kept. (Absolute medians drift a few ms between runs with CPU frequency; the
paired same-window deltas are the reliable figures.)

## Peak-RSS note

The ~45 MiB of decode intermediates (coefficients + planes) the streaming path
eliminates shows cleanly as **−42.3 MiB** in the compiled-sweep peak RSS. The
decode-only viewer harness shows only ~5 MiB because in that configuration the
process peak is reached by other pipeline allocations rather than during JPEG
decode; the compiled measurement isolates the decode footprint.

## Remaining JPEG opportunities

1. **Vectorized colour conversion.** The integer YCbCr add-and-clamp still runs
   scalar with a 64 KiB green gather table; a shuffle/gather-free SIMD variant
   (or splitting the green table) may help now that IDCT is faster. Not attempted
   this pass — the gather table makes a byte-identical SIMD kernel non-trivial.
2. **NEON IDCT** for aarch64 (the AVX2 kernel is x86-only; scalar fallback runs
   elsewhere).
3. **Streaming for non-interleaved sequential scans** (currently coefficient-
   path) — rare in the wild, lower priority.
4. **Sparse/DC-only block fast path** in the IDCT — measured neutral in pass 1;
   the AVX2 kernel already handles DC-only columns at full lane width.
