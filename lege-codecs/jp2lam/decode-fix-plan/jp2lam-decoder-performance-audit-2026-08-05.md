# `jp2lam` JPEG 2000 decoder performance audit

**Date:** 2026-08-05  
**Scope:** the decoder in the supplied `jp2lam` source tree, with emphasis on PDF `/JPXDecode` images and source/output sizes around 3000×4000 pixels and above.  
**Goal:** identify current structural, algorithmic, integration, parallelism, memory, and code-generation gaps; rank the work that is likely to improve end-to-end PDF rendering rather than only isolated kernels.

---

## Executive verdict

The JPEG 2000 decoder is now a credible primary bottleneck in JPX-heavy pages. The renderer-side image sampling pass has already reduced warm-decoded JPX rendering to roughly **35 ms**, while the same page still spends roughly **292–331 ms** in JPX decode. Earlier memory profiling also attributed about **150 MiB of unmodeled peak memory** to JPX coefficient and reconstruction intermediates. The source audit explains both results.

The decoder is not primitive. It already has:

- native reduced-resolution decode;
- code-block filtering for regions;
- packed output modes;
- fused Tier-1 dequantization;
- bounded Tier-1 parallelism through a decoder-owned Rayon pool;
- SIMD abstractions for DWT, color transforms, and dequantization;
- support for the packet structures and code-block modes commonly encountered in PDF and archival JP2 files.

The remaining problem is that the optimized pieces are joined by several scaling failures:

1. **The public defaults select the slowest shape:** full resolution, planar `i32`, serial execution.
2. **The convenience APIs rebuild scratch and/or a thread pool per image**, while only one Tier-1 scratch object is retained by `Jp2Decoder`.
3. **Irreversible 9/7 DWT has severe allocation and memory-copy amplification:** a scratch allocation per parallel row and a full active-plane temporary at every vertical synthesis level.
4. **Nonzero tile origins force scalar phase-aware gather/scatter DWT**, even for normally aligned tiles whose phase is equivalent to origin zero.
5. **Packed output is not actually end-to-end packed:** RGB reconstruction creates separate full-size 8-bit component planes and then performs another scalar interleave pass.
6. **Common renderer formats can silently fall off the direct path.** `Rgba8` without an encoded alpha plane falls back to full planar reconstruction; sYCC and palette cases also fall back.
7. **Single-tile ROI still reconstructs and allocates most of the full image.** Existing measurements show only an approximately 11% speedup for a quarter-image ROI.
8. **Tier-1 parallel work is scheduled by block count/order, not estimated cost**, and parallel worker scratch is not persistently owned by the decoder session.
9. **Tier-2 creates a large amount of small metadata and segment allocation**, including packet records that are unnecessary when profiling is disabled.
10. **SIMD dispatch is incomplete at the architecture-specific level.** The x86 AVX2 module is a shell; the current acceleration is primarily portable `wide` SIMD.

The highest-return sequence is therefore:

> **verify the renderer is reaching the reduced packed session API → repair 9/7 memory behavior → fix aligned nonzero-origin DWT dispatch → fuse color/finalization/output → improve Tier-1 scheduling and scratch → implement genuinely windowed/line-based reconstruction → add architecture-specific SIMD/PGO → consider a GPU-resident backend.**

A multi-second or 10–20 second decode at approximately 12 megapixels should be treated as a configuration, repeated-work, fallback, debug-build, or pathological-stream failure—not as an unavoidable JPEG 2000 cost. A local OpenJPEG 2.5.3 control decoded a 4485×2791 irreversible 9/7 image in approximately **88 ms grayscale** or **263 ms RGB** with four threads on the five-core benchmark host. That is not an apples-to-apples score against the renderer corpus, but it establishes the correct order of magnitude.

---

## 1. What “large render” means for this problem

There are three sizes that must be recorded separately:

1. **Codestream/source size** — the JPEG 2000 component grids and number of coded samples.
2. **Requested image size in device pixels** — the transformed, clipped footprint of the PDF image on the destination surface.
3. **Page/output surface size** — for example a 3000×4000 rendered page.

JPEG 2000 decode cost is primarily determined by source samples, components, retained resolutions, coded passes/bytes, layers, and the selected region. It should not automatically scale with the full page surface. A 6000×8000 JPX image placed into a 900×1200 box should normally be decoded at an appropriate wavelet reduction level. Conversely, a 3000×4000 page-filling source may legitimately require almost full reconstruction.

The renderer should therefore derive `DecodeResolution::AtLeast` from the **clipped device-space image footprint**, not simply from page dimensions, source dimensions, or a fixed “full” policy. A source-space ROI should also be projected from the PDF clip and image matrix before decode.

---

## 2. Evidence from the existing renderer profiles

The supplied project history already isolates JPX decode from renderer sampling:

| Workload | Relevant observation |
|---|---:|
| JPX scan, original stage attribution | approximately 331 ms JPX decode and 245 ms image execution |
| JPX scan after image-sampling optimization | approximately 364 ms compiled total, but approximately 35 ms warm-decoded total |
| Direct decode-only measurement | approximately 292 ms on the representative JPX stream |
| MRC page | roughly half codec decode and half renderer work before the sampling optimization |
| JPX memory | roughly 171 MiB process high-water RSS versus approximately 26 MiB modeled renderer ownership |
| Allocation profile | approximately 278 MiB allocated, about 20,686 allocations, and approximately 176 MiB actual peak for the JPX case |
| Hardware counters | approximately 59% cache-miss ratio in the measured JPX process |

This means the old conclusion—“JPX is partly decode and partly generic sampling”—has changed. Sampling has been attacked successfully. Codec decode is now the dominant remaining JPX stage.

The memory and cache evidence is particularly important. It points toward full-plane traffic and allocation behavior, not merely slow floating-point arithmetic.

---

## 3. Independent control benchmark

I built a small OpenJPEG 2.5.3 C harness and encoded the supplied `lear.png` as 4485×2791, one-tile, six-resolution, irreversible 9/7 JP2 at approximately 20:1 compression. The host exposes five AMD EPYC cores and AVX2/AVX-512. Times are medians of five runs; “pack” is a deliberately generic scalar checksum/conversion loop and should not be treated as an optimized renderer output path.

### Full-resolution decode

| Components | Threads | OpenJPEG decode | Generic pack | Total |
|---|---:|---:|---:|---:|
| Gray | 1 | 185.9 ms | 22.7 ms | 209.2 ms |
| Gray | 2 | 137.4 ms | 23.4 ms | 170.9 ms |
| Gray | 4 | **88.0 ms** | 22.4 ms | **110.6 ms** |
| Gray | 8 | 82.1 ms | 23.0 ms | 105.8 ms |
| RGB | 1 | 560.0 ms | 100.7 ms | 660.9 ms |
| RGB | 2 | 364.2 ms | 99.1 ms | 463.4 ms |
| RGB | 4 | **262.9 ms** | 100.6 ms | **363.8 ms** |
| RGB | 8 | 204.5 ms | 103.6 ms | 307.0 ms |

Eight threads oversubscribe the five-core host. The extra improvement reflects scheduling and host variability, not a recommended renderer policy.

### Reduced-resolution decode, four threads

| Components | Reduction | Output dimensions | Decode | Generic pack | Total |
|---|---:|---:|---:|---:|---:|
| Gray | 1 level | 2243×1396 | 37.2 ms | 5.7 ms | 43.1 ms |
| Gray | 2 levels | 1122×698 | 12.8 ms | 1.4 ms | 14.4 ms |
| RGB | 1 level | 2243×1396 | 103.4 ms | 25.7 ms | 129.5 ms |
| RGB | 2 levels | 1122×698 | 34.6 ms | 6.2 ms | 41.1 ms |

### Peak RSS, single representative run

| Components | Reduction | Peak RSS |
|---|---:|---:|
| Gray | full | 53.4 MiB |
| Gray | 1 | 17.4 MiB |
| Gray | 2 | 8.2 MiB |
| RGB | full | 153.1 MiB |
| RGB | 1 | 45.5 MiB |
| RGB | 2 | 18.1 MiB |

The control benchmark supports four conclusions:

- large Part 1 JPEG 2000 decode is not intrinsically a multi-second operation;
- native wavelet reduction is a very large latency and memory lever;
- component count matters greatly;
- full-resolution RGB JPEG 2000 can still be memory-heavy in a mature decoder, so the correct target is not “zero memory,” but elimination of avoidable duplication and copies.

The exact control files, harness, and raw results accompany this report.

---

## 4. P0: verify the renderer is not bypassing the optimized decoder

### 4.1 Dangerous defaults

`src/decode/mod.rs:196–205` defines:

```rust
DecodeRequest {
    resolution: DecodeResolution::Full,
    output: DecodeOutputFormat::NativePlanarI32,
    region: None,
    concurrency: DecodeConcurrency::Serial,
    ...
}
```

Those are compatibility defaults, but they are hostile defaults for a renderer. Any call built from `DecodeRequest::default()` without explicitly overriding all three critical fields receives full resolution, planar data, and one decoder thread.

The legacy `decode_jp2()` path at `src/decode/mod.rs:327–341` also constructs fresh scratch and requests full planar output. `decode_jp2_request()` at lines 369–376 constructs a new Rayon pool and new scratch on every call. A renderer should use a persistent `Jp2Decoder` (`lines 236–277`) and an explicit request.

### 4.2 Stats mode changes the program being measured

`decode_jp2_with_stats()` enables internal stats. In `src/decode/t1.rs`, stats-enabled Tier-1 is intentionally serialized. Do not use that path as the production benchmark or to infer real parallel scaling. Use coarse external stage timers or low-overhead thread-local counters that do not alter scheduling.

### 4.3 Reader API is inappropriate for resident PDF stream bytes

The reader-based API around `src/decode/mod.rs:1818–1860` can spill through a temporary file/mapping before calling the request API. PDF stream bytes are already resident. The PDF renderer should call the slice/session path directly.

### 4.4 Common output-format fallback

`decode_packed_direct()` accepts only exact Gray, sRGB, or CMYK matches (`src/decode/mod.rs:515–523`). More importantly, `Rgba8` without an encoded alpha component returns `None` (`lines 524–529`), causing a full planar decode followed by generic packing.

A normal PDF renderer commonly owns a four-byte RGBX/BGRA/RGBA destination even when the source has no alpha. That should not require an encoded fourth plane. Add renderer-oriented formats such as:

```rust
Rgbx8,   // fixed 255 in X
Bgrx8,
Bgra8,   // encoded alpha if present, otherwise 255
```

or, preferably, a `decode_into()` API with caller-specified channel order, alpha fill, stride, and premultiplication.

### 4.5 Required telemetry before optimization

Emit one compact line per unique JPX decode during a diagnostic run:

```text
object=17/0 source=4485x2791 comps=3 precision=8 transform=9/7
layers=1 tiles=1 codeblocks=... codeword_bytes=...
request=2243x1396 reduce=1 roi=none output=bgra8
threads=3 backend=wide direct_packed=true fallback=none
cache=miss parse_ms=... t2_ms=... t1_ms=... dwt_ms=... final_ms=... total_ms=...
```

Also record:

- debug versus release build;
- `JP2LAM_PRIMITIVES` selection;
- whether stats were enabled;
- decoder-session reuse;
- whether the PDF image cache hit;
- repeated decodes of the same object in one page/render;
- whether the output request caused a direct-path fallback;
- outer render-worker and inner codec-worker counts.

This instrumentation will immediately distinguish genuine kernel cost from wrong API selection, repeated decode, or oversubscription.

---

## 5. P1: irreversible 9/7 DWT is the largest structural gap

Lossy JP2 files normally use irreversible 9/7. The current implementation has two independent large-image defects.

### 5.1 Scratch allocation per parallel row

At `src/dwt/irrev97.rs:871–877`, every row handled by `par_chunks_mut()` allocates:

```rust
let mut local_scratch = vec![0.0f32; active_width];
```

For a 3000×4000 component at the largest level, this means thousands of heap allocations and approximately 48 MB of cumulative row-scratch allocation for that level alone. Across levels and three components, the traffic can approach hundreds of megabytes even though only a few worker scratch lines are needed concurrently.

Do not replace this blindly with Rayon `for_each_init()` and assume one initializer per worker. Rayon initializes per job, and jobs may outnumber workers. Instead, use explicit coarse chunks or a persistent indexed scratch pool.

A low-risk first implementation is:

```rust
let workers = rayon::current_num_threads().max(1);
let rows_per_job = active_height.div_ceil(workers * 2).max(1);

data[..active_height * stride]
    .par_chunks_mut(rows_per_job * stride)
    .for_each(|rows| {
        let mut scratch = vec![0.0f32; active_width];
        for row in rows.chunks_mut(stride) {
            inverse_97_1d_with_scratch(
                &mut row[..active_width],
                &mut scratch,
                use_wide,
            );
        }
    });
```

That reduces allocation count from O(rows × levels × components) to roughly O(jobs × levels × components). The production version should borrow scratch from the persistent decoder session rather than allocate it per level.

### 5.2 Full active-plane temporary per vertical level

`interleave_rows()` at `src/dwt/irrev97.rs:963–982` allocates:

```rust
let mut tmp = vec![0.0f32; active_width * active_height];
```

It then copies every active row into the temporary and copies every row back. This happens at every synthesis level. On a 3000×4000 component:

- one full `f32` plane is **45.8 MiB**;
- three component planes are **137.3 MiB**;
- the largest vertical temporary adds another **45.8 MiB**;
- coefficient planes plus that temporary already reach approximately **183.1 MiB**, before Tier-2, Tier-1 scratch, output, allocator overhead, or the other components’ finalization buffers;
- summing active areas over five levels produces about **61.0 MiB of temporary allocation per component**, or roughly **183 MiB for RGB**, solely for this vertical interleave temporary category.

This matches the measured JPX allocation/RSS excess and cache-miss behavior.

### 5.3 Port the banded 5/3 design to 9/7

The reversible 5/3 implementation already shows the right architecture. `src/dwt/rev53.rs:932–1009` processes vertical synthesis in fixed column bands, uses scratch proportional to `band_width × active_height`, and copies one band back. The irreversible path should use the same spatial blocking model.

Recommended design:

1. Choose a measured band width, initially 64, 128, or 256 columns.
2. Gather/deinterleave only that band into contiguous scratch.
3. Perform scaling and four lifting steps over contiguous rows in the band.
4. Copy the reconstructed band back.
5. Schedule independent bands across workers when their total work exceeds a threshold.
6. Retain one maximum band scratch per decoder worker/session.
7. Add phase-aware variants for odd-origin tiles and ROI boundaries.

The previous negative result for “vertical parallelism” does not invalidate this. The project notes show that the attempted variants parallelized fine-grained row-pair operations or used full snapshots. Banded columns are coarser independent jobs, improve locality, and eliminate the full-plane temporary; the benefit is not merely thread count.

### 5.4 Large-image memory target

For 3000×4000 RGB, a sensible CPU decoder target after this change is approximately:

- three coefficient/reconstruction planes: 137.3 MiB if all components remain live;
- bounded DWT scratch: a few MiB per active worker, not 45.8 MiB per level;
- final packed destination: 34.3 MiB for RGB or 45.8 MiB for BGRA;
- no separate full 8-bit component staging planes;
- bounded Tier-1/Tier-2 metadata and code-block scratch.

Further reduction requires component-sequential or line-based reconstruction, discussed later.

---

## 6. P1: nonzero tile origins unnecessarily disable optimized DWT

`tile_local_header()` preserves an absolute tile origin in `src/decode/mod.rs:1427–1444`. Reconstruction then chooses the optimized DWT backend only when both origins are exactly zero:

- reversible 5/3: `src/decode/reconstruct.rs:831–859`;
- irreversible 9/7: `src/decode/reconstruct.rs:902–930`.

Every nonzero-origin tile is sent to `inverse_97_2d_in_place_at()` or the 5/3 equivalent. The 9/7 phase-aware path (`src/dwt/irrev97.rs:689–725`) is scalar, disables wide SIMD, gathers every column into a line buffer, transforms it, and scatters it back.

That is correct for genuinely odd phase. It is unnecessarily slow for the common case where the tile origin is aligned to the decomposition lattice. If `x0` and `y0` are divisible by `2^levels`, all synthesis phases are equivalent to the origin-zero path.

An immediate routing fix is conceptually:

```rust
fn common_even_phase(x0: usize, y0: usize, levels: u8) -> bool {
    let Some(alignment) = 1usize.checked_shl(u32::from(levels)) else {
        return false;
    };
    x0.is_multiple_of(alignment) && y0.is_multiple_of(alignment)
}
```

Then use the optimized backend when `common_even_phase(...)` is true, regardless of absolute origin. A more general version should inspect the already-computed phase steps and route to the common backend whenever every step is even.

This patch is low risk if validated against OpenJPEG for:

- tile zero;
- aligned nonzero tile origins;
- odd origins in X, Y, and both;
- partial edge tiles;
- 5/3 and 9/7;
- every supported reduction level.

The long-term fix remains a SIMD/banded phase-aware DWT so genuinely odd tiles are not condemned to scalar gather/scatter.

---

## 7. P1: “packed direct” still creates full planar staging

The direct API avoids constructing the crate’s native `Image`, but color reconstruction still creates large intermediate planes.

In `src/decode/reconstruct.rs`:

- color components are reconstructed into separate centered `f32` or `i32` planes;
- inverse ICT/RCT runs over those planes;
- each component is converted into a new `Vec<u8>`;
- `reconstruct_interleaved_u8()` then performs a scalar nested interleave into the final destination.

For 3000×4000 RGB:

- three 8-bit component planes consume **34.3 MiB**;
- packed RGB consumes another **34.3 MiB**;
- approximately **68.7 MiB** are live across staging plus output, with another complete memory pass.

### 7.1 Fuse finalization

Implement transform-specific final kernels:

```text
9/7 Y/Cb/Cr f32
    → inverse ICT
    → level shift
    → precision scaling/clamp
    → RGB/BGR/BGRA/RGBX store

5/3 Y/Db/Dr i32
    → inverse RCT
    → level shift
    → precision scaling/clamp
    → packed store
```

Process cache-sized row chunks and write directly into the caller destination. SIMD is useful here because the work is regular and independent after DWT barriers.

### 7.2 Decode directly into renderer memory

Add an API resembling:

```rust
pub struct DecodeTarget<'a> {
    pub data: &'a mut [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: PixelFormat,
    pub premultiplied: bool,
}

pub fn decode_into(
    &mut self,
    bytes: &[u8],
    request: &DecodeRequest,
    target: DecodeTarget<'_>,
) -> Result<DecodeMetadata>;
```

For multi-tile streams, reconstruct each tile directly into its canvas rectangle. The current multi-tile packed path reconstructs a temporary tile raster and copies it into the output, while multi-tile iteration itself is serial. Direct destination writes remove the copy and make independent tiles schedulable under a global work budget.

### 7.3 Add direct sYCC output

The packed direct gate requires sRGB for `Rgb8`; sYCC therefore falls back. Current sYCC reconstruction also allocates expanded chroma planes and performs per-pixel coordinate arithmetic. Add specialized direct 4:2:0, 4:2:2, and 4:4:4 kernels that replicate/interpolate chroma and perform YCbCr→RGB directly into the packed destination, without full expanded chroma planes.

---

## 8. P1/P2: Tier-1 work scheduling and hot-loop structure

Tier-1 remains a major arithmetic/branch component after DWT allocation is repaired. The current code has several correct but expensive structures.

### 8.1 Persistent parallel scratch

`Jp2Decoder` retains one `Tier1Scratch`, which benefits the serial path. The parallel path uses Rayon `try_for_each_init()` (`src/decode/t1.rs` around lines 414–416). That initializer is per Rayon job, not guaranteed once per worker, and the storage is not retained across decode calls.

Create a decoder-owned worker scratch pool containing:

- coefficient scratch for the maximum supported code-block;
- padded flag/context storage;
- MQ input sentinel buffer if adopted;
- dequant/output row scratch;
- DWT line/band scratch.

Assign stable scratch slots to decoder pool workers or lease them from a bounded lock-free/mutex pool. The pool should grow lazily to observed maxima and obey the decode memory budget.

### 8.2 Schedule by estimated cost, not block count

A `.par_iter()` over code-blocks distributes block count, but work varies with:

- codeword bytes;
- coding passes/segments;
- block area;
- band/resolution;
- style flags;
- number of quality layers retained.

Build compact `BlockJob` records after Tier-2 and estimate cost, for example:

```rust
cost = codeword_bytes
     + pass_count * PASS_WEIGHT
     + block_area * OUTPUT_WEIGHT;
```

Sort/bucket largest jobs first or use weighted chunks. Batch tiny blocks until they reach a minimum byte/pass budget. This reduces the long tail where one worker finishes a dense LL or highly refined block after the others go idle.

### 8.3 Precompute block parameters

`block_params()` recomputes geometry, quantization, style, and offsets per block. Tier-2 already knows most of this. Put the finalized values directly in `BlockJob` so the hot loop does not repeatedly perform checked arithmetic, lookups, and branching.

### 8.4 Tighten the write/dequant path

`BlockPlanes::write_block` validates and computes row bounds repeatedly, then performs scalar dequant copy. Validate the destination rectangle once when constructing the job. Use a tight row kernel—safe slices outside the inner loop, then SIMD dequant/store where profitable.

### 8.5 Stripe-oriented state

JPEG 2000 Tier-1 scans four-row stripes. The current row-major coefficient/flag organization leads to strided accesses across four rows and branch-heavy neighbor updates. A padded stripe-major scratch layout can:

- make the four samples contiguous;
- remove most boundary checks through a one-cell halo;
- expose direct context masks;
- reduce `FlagGrid::mark_significant()` neighbor-update branches;
- reduce remapping from packed flag bits to context indices.

This is a more promising Tier-1 redesign than trying to SIMD one MQ stream. MQ arithmetic is serial within a codeword segment; parallelism comes from independent code-blocks.

### 8.6 Remove the final sign pass

The decoder currently applies signs in a separate full block scan. Store or materialize signed magnitudes when the sign symbol is decoded, or fold sign application into final block write/dequantization.

### 8.7 MQ micro-optimization only after attribution

Potential low-level changes include:

- force-inline the decode/exchange/renormalize helpers after checking generated assembly;
- add sentinel padding so `bytein()` avoids repeated bounds-checked `get()` calls;
- replace repeated pass division/modulo with a small explicit pass-state machine;
- test table/CLZ-assisted renormalization;
- specialize the dominant code-block style, leaving the general path for RESET, TERMALL, VSC, PTERM, and segmentation-symbol combinations.

Each must be bit-exact and benchmarked. Branch predictor and instruction-cache regressions are plausible.

---

## 9. P2: Tier-2 metadata and allocation churn

Tier-2 is not expected to dominate a simple one-layer scan, but it can matter on streams with many precincts, layers, tile-parts, or small code-blocks. The current implementation creates avoidable objects:

- `DecodedTilePackets` retains packet records plus code-blocks;
- `Contribution` owns separate `Vec`s for segment passes and lengths;
- common non-TERMALL contributions allocate a one-element pass vector and length vector;
- precinct state stores a `Vec<usize>` of block indices even when the indices form a contiguous range;
- packet progression clones the codestream header;
- packet/order vectors are not always reserved from known packet counts;
- environment tracing state is queried in packet processing;
- multi-layer code-block data can be concatenated into a new owned buffer at finish;
- ROI filtering occurs after substantial contribution/chunk construction.

Recommended changes:

1. Replace the common one-segment case with inline fields or `SmallVec<[T; 1 or 2]>`.
2. Store `Range<usize>` for contiguous precinct block membership.
3. Use dense global block IDs instead of tuple-keyed `HashMap` lookup where possible.
4. Retain packet records only when profiling/debug output requests them; otherwise keep counters.
5. Reserve packet, order, contribution, and merged-block storage from header-derived counts.
6. Cache trace/debug environment selection once.
7. Pass the ROI retention predicate into merging so discarded code-blocks never accumulate segment/chunk objects.
8. Consider a scatter-gather MQ reader for multi-layer chunks, avoiding concatenation when the common single-chunk borrowed case is not available.
9. Add a maximum quality-layer request for preview rendering. A viewer rarely needs every refinement layer for the first frame.
10. Exploit TLM/PLT when present to skip tile-parts/packets rather than merely parse through them.

These changes matter most after the DWT/full-plane issues are removed.

---

## 10. P2: current ROI is not genuinely windowed

The decoder computes conservative subband windows and filters Tier-1 blocks, which is useful. But for a single-tile stream it still allocates the full coefficient plane, runs DWT over the full reduced tile, finalizes the full output, and then crops. The project’s own phase-3 measurement found only about an **11% speedup** for a quarter-image single-tile ROI. Multi-tile ROI performs better because entire tiles can be skipped.

A true ROI path requires windowed inverse synthesis:

1. Convert the clipped full-resolution source ROI to the selected reduced grid.
2. Expand it by the exact 5/3 or 9/7 support halo at every retained level.
3. Allocate only the necessary coefficient/subband windows.
4. Run row- and column-limited IDWT over those windows.
5. Finalize directly into the ROI destination.

OpenJPEG has long implemented sub-tile decoding where Tier-1, IDWT, MCT, and allocation depend on the requested window. OpenHTJ2K now exposes line-based and row-callback decode paths with no intermediate W×H output buffer, as well as row/column-limited viewport reconstruction. These are the right architectural references.

Do not enable ROI indiscriminately. For regions covering most of the image, halo setup and fragmented work can cost more than a full decode. Benchmark thresholds such as 50%, 70%, and 85% coverage by transform/component type.

The current region API also parses once to derive region geometry and then enters a path that parses again. Cache a parsed codestream plan so region selection does not duplicate container/header/Tier-2 setup.

---

## 11. Parallelism policy for a PDF renderer

The decoder cannot choose thread count in isolation. The renderer already runs pages/images concurrently. Nested unrestricted Rayon work can reduce document throughput and increase tail latency.

Use a global CPU-work permit system:

- **Outer scheduler:** pages and independent images.
- **Inner decoder:** 1–4 workers depending on image size/complexity and available permits.
- **No nested unbounded parallel regions.** Flatten work across components, tiles, code-blocks, and DWT bands where possible.

Suggested policy:

| Situation | Internal decoder workers |
|---|---:|
| Many pages already runnable | 1 |
| One large page-filling JPX blocks first paint | 2–4 |
| Small image or low reduced resolution | 1 |
| Large grayscale single image and otherwise idle | 3–4 |
| RGB with component-level reconstruction available | 3–4, flattened rather than nested |

Existing phase-2 measurements already show diminishing returns beyond four workers. The local OpenJPEG control shows the same general shape.

Component-level DWT parallelism is valuable for RGB/CMYK, but it should share the same work queue with band jobs. Do not run “three parallel components, each launching parallel rows/bands” without a single global budget.

---

## 12. SIMD, architecture dispatch, build, and PGO

### 12.1 Current state

The release profile already uses fat LTO and one codegen unit. The portable `wide` backend provides real vectorization, but `src/simd/x86/mod.rs` detects AVX2 while its architecture-specific setup is effectively empty. `JP2LAM_PRIMITIVES=avx2` therefore does not yet mean a comprehensive hand-tuned AVX2 backend.

Project notes also show that SIMD results have been workload-sensitive: one early default-wide configuration regressed wall time, while later same-build comparisons showed improvements. Continue to benchmark scalar versus wide on large decode-only and renderer workloads rather than assuming “auto” is always best.

### 12.2 Order of work

Do architecture-specific SIMD after fixing layout and memory traffic. Otherwise AVX2 merely accelerates arithmetic between large allocations and copies.

Then implement and measure:

- AVX2/FMA 9/7 horizontal and banded vertical lifting;
- AVX2 5/3 lifting;
- inverse ICT/RCT plus packed output;
- dequant/store;
- sYCC upsample/color output;
- NEON equivalents for ARM;
- AVX-512 only as a measured optional path, because frequency throttling and small kernels can erase theoretical width gains.

Use function multiversioning or build variants (`x86-64-v3`, native) rather than forcing a nonportable baseline.

### 12.3 PGO

Profile-guided optimization is likely worthwhile after the pipeline stabilizes because Tier-1/MQ contains many branches and common coding-style paths. Train on a PDF-specific corpus covering:

- archival grayscale scans;
- RGB page images;
- 5/3 and 9/7;
- one and several layers;
- single and multi-tile;
- common and uncommon code-block styles;
- reduced and full decode;
- sYCC and palette fallbacks.

Inspect generated assembly and inlining before and after PGO; do not use PGO as a substitute for the structural fixes.

---

## 13. GPU path: useful, but only with renderer integration

NVIDIA’s current nvJPEG2000 library offloads every decode stage except Tier-2 to Pascal-or-newer GPUs, supports 5/3 and 9/7, multiple tiles, up to four components, common chroma subsampling, resolution/tile decode, partial decode, RGB output controls, and reusable decode state. On an RTX 4060, it is a viable experimental control or optional backend.

There are two materially different GPU strategies:

### 13.1 nvJPEG2000 backend

Advantages:

- mature GPU Tier-1, DWT, and color pipeline;
- asynchronous decode to device buffers;
- partial/tile/resolution controls;
- reusable state.

Costs/constraints:

- CUDA/NVIDIA-only distribution and licensing considerations;
- CPU Tier-2 remains;
- unsupported streams still require a CPU fallback;
- image/tile origin restrictions must be checked against PDF corpus;
- transferring decoded pixels back to CPU can erase much of the gain.

It makes most sense when the decoded result remains on the GPU and feeds the existing wgpu rendering/compositing path through an interop or upload strategy.

### 13.2 Hybrid native path

Keep Tier-2 and MQ/Tier-1 on CPU initially, then upload dequantized coefficient bands and perform:

- 9/7 or 5/3 IDWT;
- ICT/RCT or sYCC conversion;
- scaling/minification;
- clipping and compositing;

directly into the GPU render target. This targets the decoder’s largest memory-bandwidth stage and avoids a full decoded CPU raster. It also preserves the native Rust packet/Tier-1 work.

The hybrid path is attractive only after the CPU banded/windowed design establishes correct data boundaries. Do not add a GPU round trip merely to reproduce a CPU packed raster.

---

## 14. Cache architecture

A renderer-owned image cache should distinguish:

1. **Parsed JP2 plan:** container/header, tile-parts, packet geometry, quantization, component metadata, and reusable block-job plan.
2. **Decoded raster:** keyed by PDF object/generation, output format, reduction level, palette policy, color-space policy, and ROI semantics.
3. **GPU texture/coefficient state:** keyed by the same decode identity plus device/context generation.

Avoid caching arbitrary tiny ROIs as independent full entries; use tile/strip granularity or promote to a larger canonical region. Do not let a cache key that includes page scale cause the same source to be reparsed and Tier-1 decoded repeatedly when a reusable higher-resolution raster already exists.

For progressive viewing, retain lower-resolution decode immediately and refine only when the settled viewport requires it. JPEG 2000 resolution levels are a natural preview pyramid; use them rather than decoding full and downsampling.

---

## 15. Memory planning and resource accounting

`DecodeLimits::max_working_bytes` uses a coarse estimate (`component_samples × 20` in the current code). The estimate does not accurately model:

- selected reduction;
- ROI/window coverage;
- output format and staging;
- number of live components;
- 9/7 full-plane vertical temporary;
- parallel worker scratch;
- Tier-2 metadata/segment storage;
- multi-tile temporary raster copies.

Replace it with a decode plan computed before large allocation:

```text
coefficient/reconstruction planes
+ DWT band/line scratch × active workers
+ Tier-1 scratch × active workers
+ Tier-2 block/packet/segment state
+ output destination or tile staging
+ color/palette/chroma staging
+ safety margin
```

The plan should expose expected and peak bytes for diagnostics and acquire tokens from a process-wide codec memory budget. This prevents several 12 MP RGB images from simultaneously allocating hundreds of MiB inside otherwise independent page workers.

The stats struct currently leaves counters such as MQ symbols and allocated bytes incomplete, and its peak-scratch heuristic is not tied to actual ownership. Add allocator/scratch high-water counters at the owning buffers rather than estimating from pixels after the fact.

---

## 16. Recommended renderer API and policy

A renderer-specific constructor should make the fast path explicit and difficult to bypass:

```rust
fn make_pdf_jpx_request(
    source_bounds_in_device_pixels: DeviceRect,
    source_roi: Option<DecodeRegion>,
    pdf_has_indexed_colorspace: bool,
    codec_threads: usize,
) -> DecodeRequest {
    DecodeRequest {
        resolution: DecodeResolution::AtLeast {
            width: source_bounds_in_device_pixels.width().ceil().max(1.0) as u32,
            height: source_bounds_in_device_pixels.height().ceil().max(1.0) as u32,
            quality_margin: 1.25,
        },
        output: DecodeOutputFormat::Rgb8, // replace with direct Bgra8/Rgbx8
        region: source_roi,
        concurrency: DecodeConcurrency::Budgeted(codec_threads.max(1)),
        ignore_container_palette: pdf_has_indexed_colorspace,
        limits: DecodeLimits::default(),
    }
}
```

Operational rules:

- own a persistent `Jp2Decoder` per render worker or in a small decoder pool;
- never use `decode_jp2()` for the PDF hot path;
- never rely on output/resolution/concurrency defaults;
- use resident PDF stream bytes, not the reader/tempfile path;
- calculate target dimensions from the transformed and clipped image, not the page;
- use a 1.25–1.5 quality margin, measured against visual/differential output;
- request the renderer-native pixel order and stride;
- acquire codec CPU and memory permits;
- log every fallback reason;
- cache parsed plans and decoded results by PDF object identity and decode semantics.

---

## 17. Implementation roadmap

### Phase 0 — one diagnostic pass

1. Add the one-line decoder telemetry.
2. Confirm release mode, backend, actual request, reduction, region, direct path, worker count, and cache behavior in the PDF renderer.
3. Add counters for repeated decode of identical PDF object bytes within one render and across zooms.
4. Benchmark the representative JPX scan and MRC pages with 1/2/4 decoder workers under the normal outer scheduler.

**Exit condition:** every measured decode has an explained request/path and stage split.

### Phase 1 — low-risk structural patches

1. Add renderer-native RGBX/BGRA formats so no-alpha images stay on the direct path.
2. Make the PDF integration use persistent `Jp2Decoder` sessions.
3. Replace per-row 9/7 scratch allocation with coarse chunk/session scratch.
4. Route aligned nonzero tile origins through the optimized DWT backend.
5. Reserve obvious Tier-2 vectors and suppress packet-record retention when stats are off.

**Expected effect:** lower allocation count, lower tail latency, major multi-tile improvement where origins are aligned, and removal of accidental planar fallbacks.

### Phase 2 — 9/7 banded reconstruction

1. Port the 5/3 vertical band design to 9/7.
2. Reuse per-worker band scratch.
3. Add coarse independent band scheduling.
4. Add phase-aware banded kernels.
5. Remove the full active-plane `interleave_rows()` temporary.

**Expected effect:** the largest memory/RSS reduction and a substantial large-image speedup through lower copy traffic and better locality.

### Phase 3 — fused packed output

1. Fuse inverse ICT/RCT, shift, scale, clamp, and channel packing.
2. Add direct sYCC 4:2:0/4:2:2/4:4:4 output.
3. Add `decode_into()` with caller stride/channel order.
4. Write multi-tile output directly into the final canvas.
5. Parallelize components/tiles through one flattened decoder work budget.

**Expected effect:** remove approximately 34 MiB of RGB 8-bit staging at 12 MP and one or more full output passes.

### Phase 4 — Tier-1/Tier-2 refinement

1. Build compact prevalidated `BlockJob`s.
2. Weighted largest-first/bucketed scheduling.
3. Persistent worker scratch.
4. Tight SIMD dequant/store.
5. Stripe-major padded flags/coefficient state.
6. Eliminate final sign sweep.
7. Inline common Tier-2 segment representation and dense block IDs.
8. Add quality-layer limit.

**Expected effect:** lower branch/object overhead and better core utilization once DWT no longer dominates memory traffic.

### Phase 5 — true windowed/line-based decode

1. Windowed subband allocation.
2. Row- and column-limited IDWT.
3. Strip/row callback finalization into the renderer or GPU upload.
4. Parsed-plan cache.
5. ROI coverage threshold policy.

**Expected effect:** large gains for clipped/zoomed images and lower peak memory; this is the architectural endpoint for a viewer.

### Phase 6 — ISA/PGO/GPU

1. AVX2/FMA and NEON specializations.
2. PGO with PDF JPX corpus.
3. Optional nvJPEG2000 control/backend on NVIDIA.
4. Hybrid CPU Tier-1 + GPU IDWT/color/composite path.

---

## 18. Correctness and performance gates

### 18.1 Differential correctness

Use OpenJPEG as the primary oracle:

- reversible 5/3: maximum absolute sample difference **0**;
- irreversible 9/7: retain the project’s existing corpus tolerance—normally around 1–2, with specifically documented deeper-level cases permitted only after mean/error-distribution review;
- packed output must match the planar path under the same PDF color/palette policy;
- ROI output must be byte-identical to cropping a full decode at the same reduction;
- reductions must match the corresponding OpenJPEG reduced output.

Test matrix:

- 1, 2, 3, and 4 components;
- 1/2/4/8/12/16-bit precision where supported;
- 5/3 and 9/7;
- no MCT, RCT, ICT, sYCC 4:2:0/4:2:2/4:4:4;
- full, reduce 1, reduce 2, and `AtLeast`;
- single tile, regular aligned multi-tile, odd-origin tile, partial edge tile;
- LRCP/RLCP/RPCL/PCRL/CPRL;
- explicit precincts, SOP/EPH, tile-parts;
- one and several layers;
- common style, RESET, TERMALL, VSC, PTERM, segmentation symbols;
- palette ignored/applied according to PDF semantics;
- alpha and no alpha into RGB, RGBX, RGBA, BGRA;
- ROI coverage 5%, 25%, 50%, 75%, 95%;
- malformed/truncated streams and resource ceilings.

### 18.2 Performance scoreboard

Record p50 and p95 for:

- parse/Tier-2;
- Tier-1 MQ;
- block write/dequant;
- horizontal DWT;
- vertical DWT;
- color/finalization/pack;
- total decode;
- renderer image execution;
- total page render;
- allocations and allocated bytes;
- peak RSS/live working bytes;
- cache misses and IPC where available;
- CPU time as well as wall time;
- outer-render throughput and page p99 under concurrency.

Normalize by:

- source megapixels;
- retained output megapixels;
- component count;
- compressed codeword bytes;
- pass/layer count;
- ROI coverage.

### 18.3 Concrete control targets on this host

For the supplied 4485×2791 20:1 9/7 control image, OpenJPEG 2.5.3 produced approximately:

| Case | Decode target |
|---|---:|
| Gray full, 4 threads | 88 ms |
| Gray reduce 1, 4 threads | 37 ms |
| Gray reduce 2, 4 threads | 13 ms |
| RGB full, 4 threads | 263 ms |
| RGB reduce 1, 4 threads | 103 ms |
| RGB reduce 2, 4 threads | 35 ms |

Do not require `jp2lam` to beat those immediately; the streams and output semantics differ. Use them as an order-of-magnitude control and run exact same-stream comparisons once the Rust toolchain is available.

Structural acceptance gates:

- 9/7 row scratch allocations scale with coarse jobs/workers, not rows;
- no full active-plane vertical temporary in the banded path;
- aligned nonzero tiles use the optimized/SIMD DWT path;
- RGB/BGRA direct output does not allocate three full 8-bit component planes;
- no temporary per-tile raster when writing an ordinary multi-tile canvas;
- stats do not serialize production decode;
- no fresh Rayon pool on each renderer image;
- single-tile ROI eventually allocates/reconstructs proportional to ROI plus halo, not full tile;
- no whole-document throughput regression from nested codec threading.

---

## 19. Fast diagnosis of severe 3000×4000+ outliers

If a decode takes seconds rather than hundreds of milliseconds, check in this order:

1. Debug/unoptimized build or assertions/instrumentation enabled.
2. Compatibility `decode_jp2()` or default request: full + planar + serial.
3. `decode_jp2_request()` rebuilding a pool/session for every image.
4. Stats path serializing Tier-1.
5. Full-resolution decode despite a small device footprint.
6. RGB/RGBA request falling off the direct path because no alpha exists.
7. sYCC/palette/color-space fallback to planar conversion.
8. Same PDF image object decoded repeatedly because cache identity is wrong.
9. Reader path spilling resident bytes to a temporary file.
10. Outer page workers × inner codec workers oversubscribing CPU and memory.
11. Multi-tile nonzero origins forcing scalar phase-aware DWT.
12. High layer/pass/codeword complexity; record bytes and passes, not dimensions alone.
13. Many tiny tiles/code-blocks causing Tier-2/Tier-1 task and allocation overhead.
14. Memory pressure/page faults caused by concurrent full-plane RGB decodes.
15. Unsupported code-block style or color case invoking an external/slow fallback.

---

## 20. What should not be done first

- Do not begin with hand-optimizing `MqDecoder` instruction by instruction.
- Do not add more fine-grained Rayon regions inside existing component/tile/page parallelism.
- Do not substitute a faster allocator for eliminating full-plane temporaries.
- Do not implement ROI as “decode full, crop,” which is already the current limitation.
- Do not GPU-decode and immediately copy a full 12 MP raster back to CPU unless measurements prove the transfer still wins.
- Do not change 9/7 arithmetic or rounding for speed without exact OpenJPEG differential tests.
- Do not rely on the README as the source of current capability; it is already stale relative to implemented multi-tile reduction/ROI support.

---

## 21. Final assessment

There is substantial optimization headroom, but it is concentrated rather than diffuse.

The immediate likely gains are not speculative:

- **API/integration correction** can remove accidental full/serial/planar work entirely.
- **Coarse row scratch and banded 9/7** directly remove the allocation/copy behavior that matches the measured JPX memory and cache profile.
- **Aligned-origin dispatch** can restore the optimized DWT for most regular multi-tile streams with a small, testable change.
- **Fused final output** removes tens of MiB and a full-image pass at 12 MP.
- **Persistent weighted Tier-1 scheduling** addresses the next major compute tail without pretending MQ itself is vector-friendly.
- **Windowed/line-based reconstruction** is the correct endpoint for a tightly integrated PDF viewer, because it lets the renderer pay for only the visible resolution and region.

The target should be two-tiered:

1. **CPU production path:** robust, portable, reduced/ROI-aware, banded, low-allocation, direct-to-renderer output, and competitive with OpenJPEG on the same streams.
2. **GPU viewer path:** keep coefficients or decoded rows on-device and fuse IDWT/color/sampling/compositing, using nvJPEG2000 as a control or optional NVIDIA backend rather than forcing it into the portable core.

The decoder is already far enough along that replacing it wholesale would discard useful PDF-specific semantics and recent correctness work. The better course is to repair the four structural hotspots first, benchmark exact same streams against OpenJPEG, and retain a mature fallback/control decoder for unsupported or pathological codestreams while the native path closes the remaining gap.

---

## Audit limitations

The supplied source was inspected statically and cross-checked against the existing project benchmark documents. This environment did not contain a Rust compiler or Cargo, so I could not build this exact `jp2lam` snapshot for a same-stream side-by-side benchmark. The OpenJPEG control benchmark is reproducible and included, but its scores are not a direct ratio against `jp2lam` until the same JP2 files, output format, worker budget, and machine are used in both decoders.
