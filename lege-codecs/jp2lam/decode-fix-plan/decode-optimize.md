# jp2lam JPEG 2000 Decoder Optimization Plan

> Implementation update (2026-07-20): stage profiling, encoder-SIMD reuse,
> single-tile copy removal, Tier-1 scratch reuse, native reduced-resolution
> decode, direct packed renderer output, reusable decoder sessions, and
> request-scoped worker budgets are implemented. See
> `llm-docs/decode-optimization-results-2026-07-20.md` for measurements,
> correctness gates, and the remaining work.

## 1. Objective

Optimize jp2lam’s decoder for two related but distinct uses:

1. **General full-resolution JP2/J2K decoding**

   * Preserve the existing `decode_jp2()` behavior and output.
   * Reduce latency, allocations, copying, and peak memory.
   * Retain 8–16-bit grayscale, sRGB, CMYK, palette, multi-tile, multi-tile-part, and Part 1 progression support.

2. **PDF-renderer image decoding**

   * Decode only the resolution actually needed by the rendered page.
   * Produce the renderer’s preferred packed 8-bit format directly.
   * Avoid constructing several full-resolution planar intermediate images that the renderer immediately scales and interleaves.

The second path is likely to produce the largest practical gain. The profiled JPX image is minified by an effective source-to-destination pixel ratio of approximately 13.4:1; the MRC image is approximately 5:1. Decoding the complete highest-resolution wavelet representation is therefore unnecessary for these render requests.

---

## 2. Current decoder pipeline

The current top-level path is:

```text
JP2/J2K parse
    ↓
codestream and tile-part framing
    ↓
Tier-2 packet-header decode and codeword assembly
    ↓
Tier-1 MQ code-block decode
    ↓
copy each decoded code-block into full component coefficient planes
    ↓
dequantize i32 coefficient planes into new f32 planes
    ↓
inverse 9/7 or 5/3 DWT
    ↓
inverse component transform
    ↓
convert reconstructed f32/i32 samples into new i32 output planes
    ↓
construct a temporary tile Image
    ↓
copy the tile into a separately preallocated full-image Image
    ↓
renderer converts planar i32 output to packed 8-bit pixels
```

This is functionally clear, but it creates more full-image representations than the final consumer needs.

For an irreversible single-tile grayscale image, several full-sized buffers can coexist:

* Final destination `Vec<i32>` allocated by `empty_decoded_image()`.
* Tier-1 full component coefficient `Vec<i32>`.
* Reconstruction `Vec<f32>`.
* Finalized tile `Vec<i32>`.
* Per-code-block magnitudes, signs, flags, and final coefficient vectors.
* Renderer-side packed/interleaved output.

For RGB and CMYK, this multiplies across three or four components. This matches the measured JPX and MRC RSS gaps: JPX process high-water RSS was about 171 MiB despite a modeled renderer peak of only about 26 MiB.

---

# Part I — Establish decoder-internal attribution

## 3. Add `Jp2DecodeStats`

Do not start by optimizing `MqDecoder` merely because entropy decoding is normally expensive. The current profile identifies irreversible reconstruction, but Tier-1, dequantization, DWT, finalization, and copying are still combined too broadly.

Add optional low-overhead decoder statistics:

```rust
pub struct Jp2DecodeStats {
    pub container_parse_ns: u64,
    pub codestream_parse_ns: u64,
    pub tile_plan_ns: u64,

    pub tier2_setup_ns: u64,
    pub tier2_packet_headers_ns: u64,
    pub tier2_merge_ns: u64,
    pub tier2_concat_ns: u64,

    pub tier1_total_ns: u64,
    pub tier1_mq_ns: u64,
    pub tier1_significance_ns: u64,
    pub tier1_refinement_ns: u64,
    pub tier1_cleanup_ns: u64,
    pub tier1_block_output_ns: u64,

    pub dequantize_ns: u64,
    pub dwt_horizontal_ns: u64,
    pub dwt_vertical_ns: u64,
    pub dwt_level_ns: Vec<u64>,
    pub inverse_mct_ns: u64,
    pub finalize_ns: u64,
    pub tile_stitch_ns: u64,
    pub output_pack_ns: u64,

    pub packets: u64,
    pub packet_header_bytes: u64,
    pub codeword_bytes: u64,
    pub codeblocks: u64,
    pub mq_symbols: u64,
    pub significance_passes: u64,
    pub refinement_passes: u64,
    pub cleanup_passes: u64,

    pub coefficient_pixels: u64,
    pub reconstructed_pixels: u64,
    pub output_pixels: u64,

    pub allocated_bytes: u64,
    pub peak_scratch_bytes: u64,
}
```

Exact MQ timing around every symbol would distort the result. Use sampled counters or wrap whole code-block passes. Symbol counts can be incremented in profiling builds only.

## 4. Required profiles

Capture separate flamegraphs and hardware counters for:

* Existing grayscale irreversible JPX PDF image.
* Existing MRC foreground/background JPX streams separately.
* Large Archive.org grayscale JP2.
* Large Archive.org RGB JP2.
* Reversible 5/3 image.
* Irreversible 9/7 image.
* Single-tile and multi-tile images.
* One multi-layer or multi-precinct stream.
* Full-resolution decode.
* One-level reduced decode.
* Two-level reduced decode.
* Packed renderer-target decode.

The first report must divide the approximately 320–330 ms current decode into:

```text
parse
Tier-2
Tier-1
dequantization
horizontal DWT
vertical DWT
MCT
final conversion
tile/output copies
```

Exit condition: at least 95% of `decode_jp2()` wall time is assigned to named decoder stages.

---

# Part II — Add a renderer-oriented decoding API

## 5. Replace the single fixed-output entry point internally

Keep:

```rust
pub fn decode_jp2(bytes: &[u8]) -> Result<Image>
```

as a compatibility wrapper, but implement it through a new request API:

```rust
pub struct DecodeRequest {
    pub resolution: DecodeResolution,
    pub output: DecodeOutputFormat,
    pub region: Option<DecodeRegion>,
    pub concurrency: DecodeConcurrency,
}

pub enum DecodeResolution {
    Full,
    ReduceLevels(u8),
    AtLeast {
        width: u32,
        height: u32,
        quality_margin: f32,
    },
}

pub enum DecodeOutputFormat {
    NativePlanarI32,
    Gray8,
    Rgb8,
    Cmyk8,
}

pub enum DecodeConcurrency {
    Serial,
    Budgeted(usize),
}

pub struct DecodedRaster {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: DecodeOutputFormat,
    pub data: Vec<u8>,
}
```

Also introduce a reusable decoder object:

```rust
pub struct Jp2Decoder {
    scratch: DecodeScratch,
}

impl Jp2Decoder {
    pub fn decode(
        &mut self,
        bytes: &[u8],
        request: &DecodeRequest,
    ) -> Result<DecodeResult>;
}
```

This gives PDF render workers a place to retain bounded scratch allocations without creating global synchronization.

## 6. Resolution selection in the PDF renderer

The renderer should calculate the image’s destination footprint before JPX decode.

For an axis-aligned image:

```text
required_width  = destination pixel width
required_height = destination pixel height
```

Select the largest JPEG 2000 reduction level whose decoded dimensions remain at least:

```text
required dimension × quality margin
```

Start with a quality margin of approximately 1.25–1.5. A stricter mode can use 2.0.

For the measured workloads:

* A 13.4:1 source/destination area ratio has a linear ratio near 3.7:1.
* A 5:1 area ratio has a linear ratio near 2.2:1.

A one-level reduction, halving both dimensions, should remain above destination resolution for both pages while reducing reconstructed pixels by roughly fourfold. The JPX scan may sometimes permit a second level, depending on its exact dimensions and quality margin.

For rotated, sheared, masked, or perspective-like affine draws, derive a conservative maximum pixel footprint or retain full-resolution decode initially.

---

# Part III — Implement native reduced-resolution decoding

## 7. Tier-2 behavior

A reduced-resolution decoder still needs to parse packet headers sufficiently to locate packet bodies and preserve tag-tree state, but it need not retain or Tier-1-decode high-resolution code-block data.

Define:

```rust
let highest_required_resolution =
    decomposition_levels - reduce_levels;
```

Tier-2 should:

1. Parse every necessary packet header.
2. Maintain inclusion, zero-bitplane, and `Lblock` state for correctness.
3. Compute every packet body length.
4. Retain codeword contributions only for resolutions at or below the selected output resolution.
5. Advance over bodies belonging only to discarded high-resolution bands without storing their codeword segments.
6. Avoid concatenating discarded multi-layer codewords.
7. Avoid constructing `DecodedCodeBlock` objects for discarded bands.

This keeps the existing packet parser valid while reducing retained compressed data, Tier-1 work, coefficient storage, and DWT work.

The current Tier-2 decoder builds all packet positions eagerly and sorts them, allocates a contribution vector per packet, stores merged blocks in a `HashMap`, and can concatenate multi-chunk codeword segments. Those operations should be measured, but they are secondary unless Tier-2 attribution proves otherwise.

## 8. Reduced reconstruction geometry

Reconstruction must use the dimensions and reference-grid phase associated with the selected resolution, not simply divide the full dimensions by a power of two.

Implement a tested helper:

```rust
fn reduced_component_bounds(
    full_x0: u32,
    full_y0: u32,
    full_x1: u32,
    full_y1: u32,
    reduce_levels: u8,
) -> ReducedBounds;
```

Use the same phase-aware geometry already used for subbands and tile origins.

For reduction level `r`:

* Retain the LL band and detail bands only through resolution `L-r`.
* Run only `L-r` inverse DWT synthesis levels.
* Return exact reduced reference-grid bounds.
* Preserve odd origins and odd dimensions.
* Test tile boundaries where ceil-division changes dimensions.

Do not implement reduced decode as full reconstruction followed by resizing. The point is to omit high-resolution Tier-1 and DWT work.

---

# Part IV — Remove full-plane duplication

## 9. Eliminate temporary full-image allocation for single-tile images

The current `decode_jp2()` allocates the final image before decoding and then creates a complete temporary tile image, which is copied into it.

Add an immediate single-tile path:

```rust
if tile_count == 1 && tile covers the full output {
    return reconstructed_tile;
}
```

This removes one complete output-sized allocation and one complete component copy for the common case.

For multi-tile images, reconstruct directly into the final destination or into a caller-provided tile destination. Do not create a complete temporary `Image` for each tile and then copy it.

## 10. Replace per-code-block result allocation

The Tier-1 implementation currently allocates:

* `Vec<u32>` magnitudes.
* `Vec<u8>` signs.
* `FlagGrid`.
* A final `Vec<i32>` coefficients.
* Then copies that coefficient vector into the full tile plane.

The separate sign vector appears unnecessary because significance and sign information are already retained in `FlagGrid`, and the coefficient can be maintained as signed data.

Refactor toward:

```rust
struct CodeBlockScratch {
    coefficients: Vec<i32>,
    flags: FlagGrid,
}
```

or, preferably, direct output:

```rust
fn decode_codeblock_into(
    ...,
    destination: &mut PlaneViewMut<'_, i32>,
    scratch: &mut Tier1Scratch,
)
```

For the reversible path, write decoded coefficients directly into the full `i32` component plane.

For the irreversible path, decode into one reusable code-block-sized `i32` scratch buffer and immediately dequantize-copy it into the destination `f32` plane.

That removes:

* Full irreversible `i32` tile coefficient planes.
* Per-block final coefficient allocations.
* Per-block copy into the full `i32` plane.
* The later full-plane dequantization pass.

The maximum normal JPEG 2000 code-block is small enough that one scratch allocation per decoding worker can be reused.

## 11. Fuse Tier-1 output and dequantization

The current irreversible flow is:

```text
decode all blocks into full i32 plane
allocate full f32 plane
walk every subband and dequantize i32 → f32
```

Change it to:

```text
allocate final f32 wavelet plane
for each code-block:
    Tier-1 decode into reusable local i32 scratch
    multiply by that block band’s quantization step
    write f32 values directly into their final subband coordinates
```

The quantization step is already known while iterating decoded blocks.

This should remove one full component plane and one complete image traversal.

Retain a separate reversible path:

```text
Tier-1 decode directly into full i32 wavelet plane
inverse 5/3 in place
```

## 12. Produce packed output directly

The PDF renderer ultimately needs 8-bit output. It should not receive three or four planar `Vec<i32>` components only to scale and interleave them later.

For grayscale irreversible output:

```text
inverse 9/7 into f32 plane
center, round, clamp, and write Gray8 directly
```

For sRGB with ICT:

```text
inverse 9/7 into three f32 planes
apply ICT
center, round, clamp, and interleave RGB8 in one pass
```

For CMYK:

```text
reconstruct planes
apply optional MCT to first three
center, clamp, and interleave CMYK8
```

Add SIMD implementations for the final ICT/clamp/interleave operation once it is isolated.

Keep `NativePlanarI32` for callers that genuinely require the old model. The compatibility `decode_jp2()` path may still construct it.

## 13. Reusable bounded scratch

`Jp2Decoder` should retain:

* Tier-2 contribution scratch.
* Tier-1 code-block coefficient scratch.
* `FlagGrid` storage.
* DWT horizontal and vertical line buffers.
* Temporary transpose blocks if used.
* Packet-position or progression iterator state.
* Component-plane allocations where safe.

Apply retention limits:

```rust
const MAX_RETAINED_CODEBLOCK_BYTES: usize = ...;
const MAX_RETAINED_DWT_SCRATCH_BYTES: usize = ...;
```

An anomalously large image must not permanently inflate every render worker.

---

# Part V — Optimize irreversible 9/7 reconstruction

## 14. Treat 9/7 as the first arithmetic kernel target

The earlier flamegraph attributed about 34% of the original JPX page to irreversible 9/7 reconstruction. With renderer sampling reduced to approximately 35 ms, reconstruction is likely now the largest individual decoder stage.

Split 9/7 measurement into:

* Dequantization.
* Horizontal lifting by level.
* Vertical lifting by level.
* Boundary extension.
* Temporary copying or transposition.
* Final ICT and clamp.

## 15. Horizontal lifting kernels

Horizontal rows are naturally contiguous. Implement specialized kernels for:

* Even-length, even-origin rows.
* Odd-length rows.
* Odd reference-grid phase.
* Very short boundary-only rows.
* Interior bulk with separate boundary handling.

Move boundary calculations outside the bulk loop. The inner lifting loop should not branch per sample to select symmetric-extension behavior.

Add scalar and AVX2 implementations first. AVX-512 or NEON can follow through the existing primitive-dispatch structure.

Maintain a portable scalar fallback.

## 16. Vertical lifting layout

Vertical DWT is likely less cache-friendly because samples are separated by the full image stride.

Test three approaches:

### Approach A — Strided vertical kernels

Process several adjacent columns together so each cache line supplies useful samples for multiple independent columns.

### Approach B — Tiled transpose

For each working-resolution rectangle:

1. Transpose a block of columns into contiguous scratch.
2. Run the horizontal lifting kernel over the transposed rows.
3. Transpose back.

Use moderately sized tiles chosen from benchmark results.

### Approach C — Column gather into reusable lines

Copy one or several columns into aligned line buffers, transform contiguously, then scatter them back.

Benchmark rather than assume which method wins. For wide scan images, blocked transpose will probably provide the best SIMD and cache behavior, but its extra copying may not win on small tiles or low resolutions.

## 17. Level-local operation

Do not run kernels over unused full-image regions.

At each inverse DWT level:

* Compute the exact active LL/detail rectangle.
* Process only that rectangle.
* Use phase-aware dimensions.
* Skip empty or one-sample axes.
* Specialize low-resolution levels, which are too small for wide SIMD or parallel scheduling.

## 18. Fuse operations where useful

Potential fusions to benchmark:

* Tier-1 output plus dequantization.
* Last inverse DWT level plus final sample centering.
* ICT plus clamp plus interleave.
* Gray finalization plus direct `u8` output.

Do not fuse so aggressively that the native full-precision API becomes impossible. Keep the packed-output path separate from the compatibility path.

## 19. Fixed-point 9/7 is a later experiment

A fixed-point inverse 9/7 implementation may improve speed and reproducibility, but it will probably change some output samples.

Do not make it the initial optimization.

Only test it after:

* Floating-point DWT is cache-efficient.
* SIMD is active.
* Redundant planes and copies are removed.
* The final PDF differential gate is established.

Treat fixed-point as an optional backend selected through a quality mode until its output behavior is fully characterized.

---

# Part VI — Optimize Tier-1 and MQ decoding

## 20. Specialize the common code-block style

The general Tier-1 loop carries runtime decisions for:

* Vertical-causal contexts.
* Context reset.
* Segmentation symbols.
* Per-pass termination.
* Other code-block style flags.

Archive and PDF scans usually use a narrower common style.

Select one implementation before entering the sample loops:

```rust
match style_class {
    DefaultMq => decode_default_mq(...),
    VerticalCausal => decode_vertical_causal(...),
    ResetContexts => decode_reset(...),
    Generic => decode_generic(...),
}
```

The default path should have no per-sample `vertical_causal` branch and no per-pass checks for rare features.

## 21. Remove redundant state and traversals

Validate the following experimentally:

1. **Separate `signs` array**

   * Sign is already stored in the flag grid.
   * Remove it or maintain signed coefficients directly.

2. **`clear_visited_all()` after cleanup**

   * Cleanup already clears visited state for every handled sample.
   * Add an invariant test proving no visited bits remain after cleanup.
   * Remove the full-grid clearing pass if redundant.

3. **Final magnitude/sign conversion**

   * Maintain signed `i32` values during decoding or apply signs directly while writing the block.
   * Avoid allocating and walking a separate final coefficient vector.

4. **Repeated index calculation**

   * Use row or stripe bases rather than repeatedly calculating `y * width + x`.

The existing Tier-1 code scans each bitplane through significance, refinement, and cleanup passes and performs dynamic neighbour/context queries for individual coefficients.

## 22. Use a stripe-oriented scratch layout

JPEG 2000 Tier-1 processes four-row stripes. The current coefficient vectors are row-major while the inner scan repeatedly visits the four vertical samples at each `x`.

Benchmark a code-block-local stripe layout:

```text
[x0 row0, x0 row1, x0 row2, x0 row3,
 x1 row0, x1 row1, x1 row2, x1 row3, ...]
```

This may improve locality for:

* Magnitude updates.
* Sign access.
* Flag access.
* Cleanup run-mode processing.

Convert or scatter to the component plane only once after the block is decoded. Because code-block scratch is small, the conversion cost may be lower than repeated strided access across every pass.

## 23. Optimize `FlagGrid`

The implementing agent needs the actual `FlagGrid` implementation before choosing changes. Measure:

* `neighbour_mask`.
* `is_significant`.
* `is_visited`.
* `mark_significant`.
* Cardinal sign context.
* Visited clearing.
* Stripe-clean tests.

Likely improvements include:

* Padded borders to remove neighbour bounds checks.
* Compact flag words containing significance, sign, visited, and refinement history.
* Direct precomputed context lookup from a compact neighbour mask.
* Separate default and vertical-causal masks.
* Inline hot accessors.
* Stripe-wide clean tests using one or two integer loads.

Do not use SIMD merely to update four coefficients at once while MQ decoding remains serial. The better Tier-1 parallelization unit is the code-block.

## 24. Optimize `MqDecoder` only after direct attribution

The MQ arithmetic state is inherently serial within one codeword segment. SIMD is unlikely to help materially inside one MQ stream.

Inspect and optimize:

* `decode_with_ctx()` inlining.
* Context state representation.
* MPS/LPS transition lookup.
* Renormalization loop.
* Byte input and `0xFF` stuffing handling.
* Bounds checks.
* Decoder initialization and context copying between segments.

Potential implementation changes:

* Store context state and MPS in one compact byte.
* Use static transition tables indexed by compact state.
* Inline the common MPS path.
* Use `leading_zeros()` or a small renormalization lookup to consume several shifts where valid.
* Add a safe padded/sentinel input representation to reduce repeated end checks.
* Avoid copying the complete context table between segments when contexts are retained.
* Specialize one-segment, non-reset code-blocks.

Every change requires direct code-block bit-exact tests. A desynchronized MQ decoder can produce plausible but corrupted images.

## 25. Tier-1 code-block parallelism

After reducing per-block allocation, decode independent code-blocks concurrently.

Use a decoder-owned or caller-budgeted pool, not the global Rayon pool implicitly.

Recommended policy:

```rust
DecodeConcurrency::Serial
DecodeConcurrency::Budgeted(n)
```

Parallelize only when:

* The compressed image exceeds a work threshold.
* There are enough non-empty code-blocks.
* The caller grants more than one thread.
* Estimated intermediate memory remains below the request budget.

Each worker should own:

* `Tier1Scratch`.
* `FlagGrid`.
* MQ context storage.
* Code-block coefficient scratch.

For the irreversible path, workers write dequantized blocks into disjoint component-plane rectangles. Encapsulate any unsafe disjoint-write mechanism in one audited abstraction.

Do not parallelize tiny code-block jobs individually. Batch jobs by component, band, or accumulated compressed-byte cost.

---

# Part VII — Controlled reconstruction parallelism

## 26. Component-level parallelism

Before inverse MCT, grayscale/RGB/CMYK component reconstruction is independent.

For RGB and CMYK:

* Decode component code-blocks concurrently.
* Run component inverse DWT concurrently.
* Synchronize only before RCT/ICT.

This is coarse-grained and should be easier to control than spawning tasks for every row.

## 27. DWT row and column parallelism

Within one large component:

* Parallelize horizontal row batches.
* Parallelize vertical column blocks or transpose tiles.
* Use a minimum work threshold.
* Reuse one scratch area per worker.

Avoid nested unbounded parallelism:

```text
code-block parallelism
    plus
component parallelism
    plus
DWT row parallelism
```

must share one decode budget. A simple scoped work scheduler or token budget is preferable to independent Rayon calls.

## 28. Renderer integration policy

The PDF renderer is already document-parallel. Hidden codec threading could reduce whole-document throughput through oversubscription.

Use:

* Serial or one-thread JPX decoding when many render workers are active.
* Two to four threads for latency-sensitive single-page rendering.
* More threads only for very large standalone images.
* A shared CPU permit system if dynamic allocation is needed.

Benchmark both:

* Single-page JPX latency.
* Whole-document mixed-codec throughput.

A single-page improvement is not acceptable if it substantially damages the renderer’s existing document-level advantage.

---

# Part VIII — Tier-2 cleanup

## 29. Replace eager packet-position sorting where worthwhile

`build_packet_positions()` currently constructs every packet position, pairs it with a five-element sort key, sorts the full vector, and then discards the keys.

Replace this eventually with progression-specific iterators:

```rust
enum PacketProgressionIter {
    Lrcp(...),
    Rlcp(...),
    Rpcl(...),
    Pcrl(...),
    Cprl(...),
}
```

LRCP and RLCP are straightforward nested odometers. Position-based orders need careful precinct-coordinate iteration.

Do this only if Tier-2 setup is measurable. The parser is not the present leading target.

## 30. Replace the merged-block `HashMap`

The full band and code-block geometry is known when `TilePacketDecoder` is constructed.

Assign each block a flat stable ID and store merge state in:

```rust
Vec<Option<MergedBlock>>
```

instead of:

```rust
HashMap<(band_index, block_index), MergedBlock>
```

Benefits:

* No hashing.
* Fewer allocations.
* Deterministic direct indexing.
* No separate order vector if Tier-1 can consume blocks in geometry order.

Preserve first-inclusion ordering only if another consumer actually depends on it. Tier-1 reconstruction itself should not.

## 31. Reuse per-packet contribution storage

`push_tile_part()` creates a fresh `Vec<Contribution>` for every packet.

Retain it as decoder scratch:

```rust
self.contribution_scratch.clear();
```

Likewise reuse temporary segment-length vectors where possible.

For the common single-layer case, retain borrowed codeword slices directly and avoid segment chunk structures that exist only to support contribution concatenation across layers.

These are useful allocation reductions, but remain below reduced-resolution decode, DWT, and Tier-1 work in priority.

---

# Part IX — Advanced region decoding

## 32. Add region-of-interest decode only after resolution reduction

A PDF image is often clipped or only partly visible. JPEG 2000 precincts and code-blocks make partial spatial decode possible, but inverse DWT requires support samples beyond the exact output rectangle.

A later `DecodeRegion` implementation should:

1. Transform the page clip into source-image coordinates.
2. Expand the requested rectangle by the inverse DWT support halo at every retained level.
3. Select intersecting precincts and code-blocks.
4. Parse but discard unrelated packet contributions.
5. Reconstruct only the expanded region.
6. Crop to the requested rectangle after synthesis.

This is substantially more complex than reduced-resolution decode. Do not combine both changes in the first implementation.

---

# Part X — Benchmark and correctness gates

## 33. Permanent decoder corpus

Create a jp2lam-specific corpus containing:

* Grayscale 8-bit irreversible 9/7.
* RGB 8-bit irreversible 9/7 with ICT.
* CMYK.
* Reversible 5/3.
* 12-bit and 16-bit samples.
* Palette-mapped image.
* Single and multiple tiles.
* Multiple tile-parts.
* All progression orders.
* Explicit precincts.
* Multiple quality layers.
* SOP and EPH.
* Context reset.
* Per-pass termination.
* Vertical causal.
* Segmentation symbols.
* Odd dimensions and odd origins.
* Very small images.
* Corrupt and truncated streams.
* Existing non-conformant Kakadu tile-part cases.

The current decoder already contains independent ImageMagick crop checks and Archive.org directory tests. Retain and expand those rather than relying only on jp2lam round trips.

## 34. Full-resolution correctness

For the legacy `decode_jp2()` path require:

* Existing output hashes unchanged where current output is known correct.
* Full test suite passes.
* Independent decoder crop comparisons remain within existing tolerances.
* No change in accepted or rejected feature scope unless intentional.
* Exact reversible 5/3 output.
* Stable output across thread counts.

## 35. Reduced-resolution correctness

Compare against an independent decoder invoked at the same JPEG 2000 reduction level.

Also compare:

* Reduced dimensions.
* Tile origins.
* Odd-size behavior.
* Component alignment.
* PDF page output against full-resolution decode followed by renderer resampling.
* PDFium differential severity.

Reduced-resolution output will not necessarily produce a byte-identical final rendered page. Gate it perceptually and retain a full-resolution fallback.

Use conservative fallback conditions initially:

* General affine transforms.
* Unsupported component subsampling.
* Ambiguous color metadata.
* Very small destination images where phase errors are conspicuous.
* Any stream form not represented in the reduction corpus.

## 36. Performance scoreboard

Record, per image:

| Metric                      | Required |
| --------------------------- | -------- |
| Full decode median          | Yes      |
| Reduced decode median       | Yes      |
| Tier-2 time                 | Yes      |
| Tier-1 time                 | Yes      |
| Dequantization time         | Yes      |
| Horizontal DWT time         | Yes      |
| Vertical DWT time           | Yes      |
| MCT/finalization time       | Yes      |
| Peak RSS                    | Yes      |
| Allocated bytes             | Yes      |
| Allocation count            | Yes      |
| Code-block count            | Yes      |
| MQ symbols                  | Yes      |
| Output pixels               | Yes      |
| Source coefficients decoded | Yes      |
| Threads used                | Yes      |
| Output hash/differential    | Yes      |

Use direct OpenJPEG or PDFium decoding as a comparative control, but compare identical output resolution and format.

---

# Part XI — Recommended implementation order

## Milestone 1 — Decoder attribution

Implement:

* `Jp2DecodeStats`.
* Stage timers.
* Allocation profiling.
* Direct JPX payload benchmark.
* Separate Tier-1, dequantization, DWT, finalization, and copy measurements.

No decoder algorithm change.

## Milestone 2 — Remove obvious duplication

Implement:

* Single-tile direct-return path.
* Reusable decoder scratch.
* Per-packet contribution-vector reuse.
* Per-code-block scratch reuse.
* Remove the separate sign vector if tests confirm it is redundant.
* Remove redundant visited clearing if the invariant holds.

Expected result:

* Lower allocation count.
* Lower peak memory.
* Modest speed improvement.
* Byte-identical output.

## Milestone 3 — Fused Tier-1 output

Implement:

* Direct reversible block output into `i32` component planes.
* Irreversible block decode into reusable scratch.
* Fused dequantization into final `f32` component planes.
* Remove full irreversible `i32` component planes.

Expected result:

* Major intermediate-memory reduction.
* One less full-image traversal.
* Lower cache pressure.

## Milestone 4 — Reduced-resolution decode

Implement:

* `DecodeRequest`.
* Resolution selection.
* Tier-2 discard of unneeded high-resolution contributions.
* Partial Tier-1 block selection.
* Reduced inverse DWT geometry.
* PDF renderer integration with conservative eligibility.

This is the highest-leverage milestone for the measured PDF workloads.

## Milestone 5 — 9/7 kernel optimization

Implement:

* Detailed level and axis profiling.
* Boundary-specialized lifting.
* SIMD horizontal kernels.
* Cache-efficient vertical implementation.
* Component and row/column parallel scheduling.
* Fused final ICT/clamp/interleave.

Expected result:

* Largest full-resolution arithmetic improvement.
* Benefits both general decoding and reduced decode.

## Milestone 6 — Tier-1/MQ optimization

Implement:

* Default-style specialization.
* Compact flags and coefficients.
* MQ hot-path improvements.
* Code-block batching and controlled parallelism.
* Direct-index Tier-2 merge state if still material.

## Milestone 7 — Packed renderer output

Implement:

* `Gray8`, `Rgb8`, and `Cmyk8` output targets.
* Direct finalization into packed buffers.
* Removal of renderer-side planar scaling/interleaving.
* Updated decoder memory estimates.

## Milestone 8 — ROI decode

Implement only after reduced-resolution decode is stable and measured.

---

# 37. Initial target results

Use the current measured values as the first scoreboard:

| Workload      | Current compiled | Current warm decoded | Approximate decode delta |
| ------------- | ---------------: | -------------------: | -----------------------: |
| JPX scan      |         363.7 ms |              35.0 ms |                 328.7 ms |
| JPX/JBIG2 MRC |         374.6 ms |              55.8 ms |                 318.8 ms |

First practical targets:

### Full-resolution path

* JPX decode below 220 ms without internal parallelism.
* JPX decode below 150–180 ms with a modest thread budget.
* At least 40–50% reduction in decoder-owned peak intermediate memory.
* Byte-identical legacy output.

### Resolution-aware renderer path

* JPX page compiled total below 150–200 ms initially.
* Longer-term target near 100 ms when one-level reduction is available.
* MRC compiled total below 180–220 ms.
* No material deterioration in PDFium differential severity.
* No whole-document throughput regression.

These are targets rather than assumptions. The actual attainable result depends on how much of current reconstruction time disappears through reduced-resolution decoding and whether Tier-1 or vertical 9/7 remains dominant afterward.

---

# 38. Material needed by the implementing agent

The supplied files expose the decoder orchestration, Tier-1 loops, Tier-2 packet handling, and reconstruction pipeline. The exact low-level optimization work also requires:

* `mq.rs` and its state/transition tables.
* `tier1/flags.rs`.
* `tier1/helpers.rs`.
* The inverse 9/7 and inverse 5/3 DWT implementations.
* Current SIMD primitive definitions and runtime dispatch.
* The JPX PDF payloads used by the focused benchmark.
* Raw `jpx-compiled` flamegraph or profile data.
* DHAT call-site report for the JPX decode.
* Existing jp2lam benchmark/build configuration.
* The renderer’s current JPX adapter and packed-output conversion.

Without those files, the architectural priorities are clear, but an agent cannot safely specify exact MQ, flag-grid, or DWT kernel patches.

---

# 39. Core recommendation

The optimization should not begin as “add SIMD to the decoder.”

The correct sequence is:

```text
measure decoder substages
    ↓
decode only the required JPEG 2000 resolution
    ↓
remove duplicate full-image planes and copies
    ↓
fuse Tier-1 output with dequantization
    ↓
optimize cache behavior and SIMD in inverse 9/7
    ↓
specialize and parallelize Tier-1 code-block decoding
    ↓
produce packed renderer output directly
```

For the PDF renderer, reduced-resolution decoding is probably more important than making the existing full-resolution decode loop incrementally faster. It reduces Tier-1 work, DWT work, intermediate memory, and later sampling work simultaneously.
