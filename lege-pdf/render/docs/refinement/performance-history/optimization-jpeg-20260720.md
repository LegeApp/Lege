# JPEG decode optimization review — 2026-07-20

This pass reviews and optimizes the in-house `/DCTDecode` implementation in
`crates/pdf-image/src/jpeg/`, starting from commit `891d55b`.

Raw structured and paired measurements are under
`results/optimization-jpeg-20260720/`.

## Attribution

The representative JPEG is a 2758×3561 baseline 4:2:0 stream:

- Y sampling: 2×2;
- Cb/Cr sampling: 1×1;
- 9,821,238 decoded pixels;
- 29,463,714 RGB output bytes.

Temporary stage timing attributed about 111–113 ms of a roughly 155 ms decode
to final component upsampling, YCbCr conversion, and RGB assembly. The old
`sample()` path performed two integer divisions per component per output pixel,
and the color conversion performed floating-point multiplication and clamping
for every pixel.

## Changes

### Prepared component addressing

- Source X indices are computed once per component and output column.
- Source row offsets are computed once per component and output row.
- The inner RGB/CMYK assembly loops perform direct plane indexing.
- Output components are stored directly instead of copying temporary
  three-/four-byte slices.

This removes six integer divisions per RGB output pixel on a subsampled image.

### Byte-exact integer YCbCr conversion

- R and B use 256-entry signed-delta tables.
- G uses a 65,536-entry Cb/Cr signed-delta table.
- The final conversion is integer add-and-clamp.
- Two explicitly handled saturated-edge cases preserve the original sequential
  `f32` rounding behavior.

An exhaustive unit test compares all 16,777,216 `(Y, Cb, Cr)` combinations
against the previous floating-point formula and requires byte-for-byte
identity.

The table storage is approximately 129 KiB and is initialized once with
`OnceLock`.

## Paired results

Two alternating-order batches compared release binaries at `891d55b` and the
optimized tree.

### Decode-only

Twenty rows per binary:

| Metric | `891d55b` | Optimized | Change |
|---|---:|---:|---:|
| Median total | 187.498 ms | 121.653 ms | **35.1% lower / 1.54× faster** |
| Mean total | 187.938 ms | 121.419 ms | **35.4% lower** |
| Median DCT decode | 184.867 ms | 119.329 ms | **35.4% lower** |
| Output hash | `90d3173f5dca6b9b` | `90d3173f5dca6b9b` | unchanged |
| PDFium severity | 0.000009712 | 0.000009712 | unchanged |

### Compiled high-resolution page

Ten rows per binary:

| Metric | `891d55b` | Optimized | Change |
|---|---:|---:|---:|
| Median compiled total | 373.734 ms | 316.154 ms | **15.4% lower / 1.18× faster** |
| Mean compiled total | 378.617 ms | 315.552 ms | **16.7% lower** |
| Median DCT decode | 180.513 ms | 117.086 ms | **35.1% lower** |
| Output hash | `90d3173f5dca6b9b` | `90d3173f5dca6b9b` | unchanged |

The permanent focused capture measured decode-only medians of approximately
120–123 ms for both viewer and sweep scale. Decode time remains independent of
render scale because both render requests decode the same embedded JPEG.

## Remaining JPEG opportunities

### 1. Baseline MCU-row streaming

The corpus image is baseline SOF0, but the decoder currently:

1. stores every block's 64 `i16` coefficients;
2. performs a separate whole-image IDCT pass into component planes;
3. performs a third pass to upsample and assemble RGB.

For this image, coefficient storage is roughly 30 MiB, component planes roughly
15 MiB, and RGB output roughly 29 MiB. A baseline-only MCU-row pipeline could
decode, IDCT, upsample, color-convert, and emit rows without retaining all
coefficients or all component planes. Progressive JPEG would retain the
existing full-coefficient path.

This is the largest remaining combined CPU and memory opportunity.

### 2. SIMD IDCT and color conversion

The floating-point AAN IDCT is scalar. AVX2/NEON row/column kernels and
vectorized integer YCbCr conversion are natural isolated kernels, with the
scalar implementation retained as the reference.

### 3. Wider Huffman fast table

The entropy reader uses an eight-bit prefix table and extends codes of length
9–16 one bit at a time. Measuring a 10–12-bit table would trade a few KiB per
table for fewer slow-path branches.

### 4. Sparse/DC-only block paths

The IDCT already collapses all-zero AC columns, but not whole DC-only blocks.
A measured direct constant-block path may help scans with large flat regions;
it was not retained in this pass because it did not improve the representative
wall-time measurement.
