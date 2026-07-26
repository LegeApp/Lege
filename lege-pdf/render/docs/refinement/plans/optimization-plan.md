# PDF Renderer Profiling and Optimization Plan

## 1. Current assessment

The performance results in `DEFERRED.md` establish two separate scoreboards:

* Single-page latency is roughly **4–6× slower than PDFium**.
* Whole-document throughput is roughly **1.65–2.34× faster**, using 7 compile workers and 13 render workers on the measured machine.

That confirms the concurrency architecture is working. The optimization pass should preserve it and concentrate on single-page work.

The evidence does **not** yet support concluding that JP2 decoding is the main problem:

* The MRC page is image-heavy and runs at 142 ms versus PDFium’s 34 ms.
* The text page still runs at 39 ms versus PDFium’s 6.6 ms.

The text result demonstrates that a substantial non-codec deficit exists. The MRC deficit may also be partly renderer-side: decoded samples still pass through a costly generic sampler, color conversion, minification, masking, and compositing path.

The current timers cannot resolve this because:

* `RenderStats::lower` wraps all of `prepared::lower()`.
* `prepared::lower()` performs geometry preparation, font work, codec decode, mask decode, decoded-row repacking, tiling preparation, and image sampler construction.
* `RenderStats::raster` combines path coverage, image sampling, clipping, blending, groups, soft masks, shadings, and tiling.
* `Surface::into_output()` is outside all timers.
* Compilation and document opening have no structured timing.
* `surface_bytes` records only the main surface, not decoded images, clip masks, tiling buffers, soft masks, or transparency-group surfaces.

The first optimization task should therefore be attribution, not SIMD.

---

## 2. Required benchmark structure

Create one benchmark driver that can execute the same page in several progressively narrower modes.

### 2.1 Benchmark modes

Implement these modes separately:

1. **Cold end-to-end**

   * Start process or discard all state.
   * Open document.
   * Compile page.
   * Render page.
   * Materialize output.
   * Do not write PPM or another output file during the timed region.

2. **Warm document**

   * Open the document once.
   * Reuse the snapshot.
   * Compile and render selected pages.
   * Measures normal viewer and document-sweep behavior.

3. **Compiled-page render**

   * Compile once to `CompiledPage`.
   * Repeatedly render the same IR.
   * Isolates backend preparation, decoding, and execution.

4. **Prepared-page execution**

   * Lower once to `CpuPreparedPage`.
   * Repeatedly allocate or reset a surface and execute it.
   * Isolates rasterization, sampling, and compositing.

5. **Warm-decoded render**

   * Retain decoded image resources.
   * Repeat CPU preparation and execution without re-running DCT, JPX, JBIG2, or CCITT decoding.
   * This is the decisive codec-versus-renderer comparison.

6. **Decode-only**

   * Call each codec directly on the actual embedded payload.
   * Record compressed bytes, decoded pixels, decoded bytes, output format, and wall time.

7. **Whole-document pipeline**

   * Existing compile/render pipeline with worker-count sweeps.
   * Measure throughput, queue wait, memory pressure, and tail latency.

These modes should use the same PDF, page, transform, output dimensions, background, annotation policy, hinting policy, and output format.

### 2.2 Benchmark page classes

Select a small permanent performance corpus rather than profiling random sweep pages:

* Single JPEG scanned page.
* Single JPX scanned page.
* MRC page with JPX background, JPX foreground, and JBIG2 soft mask.
* CCITT bilevel page.
* Ordinary Latin text page.
* CJK or glyph-heavy page.
* Type 1 or bare CFF page.
* Vector/path-heavy page.
* Transparency-group page.
* Soft-mask page.
* Tiling-pattern page.
* Shading-heavy page.
* One unusually slow real-world page from the sweep.
* One high-resolution page at each intended output scale.

Run at the actual production sizes, including the sweep resolution and common viewer resolutions. Synthetic rectangle pages should remain as microbenchmarks, but they cannot stand in for image, text, transparency, or pattern workloads.

### 2.3 Machine-readable results

Write one CSV or JSON row per page and run, containing:

* Commit and dirty-tree state.
* Rust compiler version.
* CPU model and enabled target features.
* Build profile and `RUSTFLAGS`.
* Page class and source identifier.
* Output dimensions and pixels.
* Cold or warm mode.
* Each stage duration.
* Total wall time.
* Allocation count and allocated bytes where available.
* Peak live bytes and process RSS.
* Hardware-counter results.
* Output hash.
* Differential score against the retained baseline and PDFium.

Report median, p90, and p99 rather than only aggregate pages per second.

---

## 3. Instrumentation to add

### 3.1 Compilation statistics

Add a `CompileStats` result or observer around `PageCompiler` with at least:

* Page lookup and inherited page attributes.
* Content-stream gathering.
* General stream-filter decode.
* Resource dictionary resolution.
* Image resource construction.
* Font resolution and parsing.
* Content tokenization.
* Operator interpretation.
* Semantic-page construction.
* Semantic-to-`CompiledPage` lowering.
* Objects resolved.
* Object-cache hits and misses.
* Object streams inflated.
* Decoded stream bytes.
* Recovery events.

Do not put an `Instant` around every operator. Use coarse scopes and counters.

### 3.2 CPU preparation statistics

Split the existing `lower` duration into:

* Path transformation and flattening.
* Stroke expansion.
* Glyph program parsing.
* Glyph outline extraction.
* Glyph hinting.
* Glyph flattening.
* Clip preparation.
* Tiling enumeration.
* Shading preparation.
* DCT decode.
* JPX decode.
* JBIG2 decode.
* CCITT decode.
* Soft-mask and hard-mask decode.
* Stride repacking.
* Image sampler construction.

Codec attribution can initially be implemented at the registry seam without changing the external codec crates. Wrap `ImageCodec::decode()` calls and aggregate by `StreamFilter`.

### 3.3 Execution statistics

Split execution into:

* Solid path coverage.
* Clip-mask construction.
* Opaque fills.
* Constant-alpha blends.
* Per-pixel masked blends.
* Separable and non-separable blend modes.
* Image sampling.
* Image compositing.
* Soft-mask rendering and derivation.
* Transparency-group rendering and compositing.
* Tiling-pattern rendering.
* Shading evaluation.
* Final RGBA-to-output conversion.

Add image-specific counters:

* Image draws.
* Encoded images decoded.
* Decode-cache hits and misses.
* Source pixels decoded.
* Destination pixels visited.
* Source sample taps.
* Minified versus magnified pixels.
* Nearest, bilinear, and area-average usage.
* Gray, RGB, CMYK, Indexed, TintLut, stencil, and masked-image counts.
* Axis-aligned versus general-affine draws.
* Fast-path hit rates.

The source-sample-tap count is especially important. A page may spend less time decoding JPX than repeatedly interpreting and converting its decoded samples during area averaging.

### 3.4 Memory statistics

Correct the memory accounting before tuning worker counts.

The current scheduler estimate uses output-format bytes plus `estimated_peak_bytes`, but the CPU backend always allocates an internal RGBA surface. A Gray8 output is therefore budgeted below its actual main-surface cost. The estimate also needs to account for:

* Decoded image samples.
* Interleaved JPX output.
* Repacked image rows.
* Main RGBA surface.
* Gray8 conversion output.
* Clip masks.
* Soft masks.
* Transparency-group surfaces.
* Tiling surfaces and fill masks.
* Prepared geometry.
* Cached resources.

Track both estimated and observed peak bytes so the estimate can be calibrated from real pages.

---

## 4. Profiling workflow

Continue using flamegraphs, but add three complementary forms of measurement.

### Sampling profiles

Produce separate profiles for:

* One JPX page.
* One MRC page.
* One text page.
* One vector page.
* One full-document render.
* A repeated warm-decoded render.

`cargo-flamegraph` remains suitable for broad hotspot discovery. Samply provides an interactive call tree and timeline through the Firefox profiler interface and supports the major desktop platforms.

Use a profiling build with:

* Release optimization.
* Line-table debug information.
* Forced frame pointers where useful for reliable unwinding.
* The same CPU target settings as the benchmark build.

### Hardware counters

For every representative page, collect at least:

* Cycles.
* Instructions.
* Instructions per cycle.
* Branches and branch misses.
* Cache references and cache misses.
* Page faults.
* Context switches.
* CPU migrations.

`perf` exposes hardware counters such as instructions, cache misses, and branch mispredictions, allowing a distinction between excessive instruction count and memory-bound execution.

This distinction will guide the implementation:

* High instruction count points toward generic per-pixel work, repeated lowering, or excessive flattening.
* Low IPC and high cache misses point toward image layouts, large temporary buffers, random lookup, or cache-unfriendly coverage structures.
* High branch misses point toward generic sample-format dispatch inside pixel loops.

### Allocation profiling

Use `dhat-rs` or an external heap profiler on the representative pages. Record:

* Total allocations.
* Total allocated bytes.
* Peak live bytes.
* Allocation call sites.
* Allocation counts per rendered page.

`dhat-rs` can profile heap behavior and also supports tests that assert allocation counts and peak heap use, making it suitable for later regression gates.

### Deterministic microbenchmarks

Use instruction-count or Callgrind benchmarks only for isolated kernels after the main profiles identify them:

* `PreparedImage` 8-bit RGB sampling.
* Box minification.
* Gray/RGB/CMYK conversion.
* `fill_opaque`.
* `blend_const`.
* `blend_mask`.
* Coverage accumulation.
* Clip-mask intersection.
* RGBA-to-Gray conversion.

Iai-Callgrind is useful here because it can give stable instruction and cache measurements suitable for CI, but it should not replace real wall-time document benchmarks.

---

## 5. Likely hotspots visible in the current code

These are hypotheses to test, not conclusions.

### 5.1 Generic image sampling

`pdf-render-cpu/src/exec.rs::paint_image()` visits every destination pixel and calls `PreparedImage::shade()`.

For each pixel, the generic path may perform:

* A floating-point inverse matrix transformation.
* Unit-square boundary checks.
* Coordinate conversion.
* Minification-mode selection.
* Multiple calls to `pixel()`.
* Recalculation of component count, row bit stride, and maximum sample value.
* Bit-offset arithmetic.
* Bounds-checked sample reads.
* `/Decode` remapping.
* Color-space dispatch.
* CMYK or palette conversion.
* Hard-mask sampling.
* Soft-mask sampling.
* Alpha multiplication.
* Destination compositing.

For minified scans, `area_average()` repeats much of this work for every source texel touched by every output pixel.

This means “image decoding” currently consists of two very different costs:

1. Codestream decoding in jp2lam, the JPEG decoder, JBIG2, or CCITT.
2. Renderer-side interpretation and resampling of the decoded samples.

The warm-decoded benchmark must separate them.

### 5.2 JPX adaptation overhead

`pdf-image/src/jpx.rs` receives planar `i32` component data from jp2lam, allocates a new interleaved 8-bit buffer, then scales and interleaves every sample with scalar loops.

This work belongs to the renderer adapter rather than the JP2 codestream decoder. Time it separately before opening a jp2lam optimization project.

Longer-term options include:

* Asking jp2lam for the exact target precision and layout.
* Preserving planar output for a planar resizer.
* SIMD scaling and interleaving.
* Fusing interleave, precision conversion, and initial resize.

### 5.3 Repeated image decoding

`ImageIr` already has a `ResourceKey` and explicitly describes backend caches as the owner of resource residency, but the CPU backend currently decodes codec images inside request lowering with no resource cache.

Add a bounded decoded-resource cache keyed by:

* Document identity.
* `ResourceKey`.
* Codec kind and relevant parameters.
* Any decode-affecting image semantics.

Use two cache levels:

* **Decoded-resource cache:** encoded payload to packed source samples. Reusable across output sizes and repeated page renders.
* **Prepared/resampled cache:** source samples to a destination-specific representation. Keyed additionally by destination geometry, transform class, interpolation, masks, and quality.

The second cache should be conservative because high-resolution images can consume substantial memory. Both caches must participate in the scheduler memory budget.

### 5.4 Scalar-only kernel dispatch

`KernelSet::select()` always returns the scalar kernels. This is a legitimate future target, but SIMD should follow sampler specialization and algorithmic improvements.

Good initial SIMD kernels are:

* Opaque RGBA fills.
* Constant-alpha source-over.
* Coverage-mask source-over.
* RGBA-to-Gray conversion.
* 8-bit Gray/RGB expansion.
* JPX planar-to-interleaved conversion.
* Soft-mask multiplication.
* Box-filter row accumulation.

The general affine image sampler and analytic edge rasterizer are poor first SIMD targets because their control flow remains complex.

### 5.5 Coverage and curve expansion

The rasterizer uses floating-point edge accumulation and splits an edge across integer columns inside each covered scanline. This can become expensive for text and vector pages.

Before replacing it:

* Count edges, edge-row intersections, `accumulate()` calls, and column-split iterations.
* Separate glyph-generated edges from ordinary paths.
* Measure coverage time independently of compositing.

Likely improvements are:

1. Adaptive device-error curve flattening.
2. Reuse of glyph outlines.
3. Fixed-point active-edge-table rasterization.
4. Only then, SIMD or wider span processing.

The fixed `CURVE_SEGMENTS` and `OUTLINE_SEGMENTS` values can generate too much geometry for small curves while still being insufficient for unusually large curves. Adaptive flattening should reduce work and improve consistency simultaneously.

### 5.6 Glyph extraction and flattening

A font program is cached once per font during one `prepared::lower()` call, which is good. Glyph outlines themselves are not cached:

* Repeated glyph IDs are extracted repeatedly.
* Repeated characters are flattened repeatedly.
* The fixed eight-segment subdivision is applied to every quadratic and cubic.

Introduce caches in stages:

* Unhinted font-unit outline cache keyed by font resource and glyph ID.
* Hinted outline cache keyed by font, glyph ID, and quantized ppem.
* Optional flattened-shape cache keyed by outline, scale/rotation class, and tolerance.

Translation should remain outside the cache so one shape can be placed at many glyph origins.

### 5.7 Tiling-pattern execution

`render_tiling()` currently:

* Re-runs `prepared::lower()` for every tile instance.
* Allocates clip and soft-mask vectors for each instance.
* Allocates a fill-sized mask.
* Copies each pattern row with `to_vec()` before compositing it.

This is a clear localized optimization target when tiling pages appear in the profile:

* Lower the cell once in cell-local coordinates.
* Translate prepared geometry or apply an execution origin per instance.
* Reuse mask vectors.
* Remove the per-row `to_vec()` by splitting borrows safely or exposing a direct surface-composite operation.
* Reuse the pattern surface and fill mask from worker scratch.

### 5.8 Surface and mask allocation

`CpuWorkerContext` currently retains only `RasterKernel` and `KernelSet`. It should eventually own reusable scratch for:

* Main surfaces where dimensions permit.
* Clip-mask vectors.
* Soft-mask stacks.
* Temporary Alpha8 buffers.
* Tiling fill masks.
* Group surfaces grouped into size classes.
* Prepared-page vectors or arenas.

Do not retain arbitrarily large buffers forever. Apply a maximum retained capacity or trim after anomalously large pages.

### 5.9 Compile-worker cache lifetime

`ParseContext` is designed to be worker-owned and contains parsed-object and decompressed-object-stream caches. However, `RenderScheduler::render_range()` invokes a generic closure without providing persistent worker state. The scheduler test constructs a new `ParseContext` and `PageCompiler` for every page, and the API encourages production code to do the same.

Refactor the compile stage around a worker factory, for example conceptually:

* Construct one `CompileWorkerContext` per compile thread.
* Store its `ParseContext`, `PageCompiler`, and temporary buffers.
* Pass `&mut CompileWorkerContext` into the page request builder.

This preserves the intended worker-local design while allowing object, object-stream, font, and allocation reuse across pages assigned to the same worker.

Only after measuring that should shared once-published document object caches be considered.

### 5.10 Source opening

The CLI currently uses `std::fs::read()` and `OwnedBytesSource`, despite `MmapSource` already existing.

Use `MmapSource` for local benchmark and production files. Keep `OwnedBytesSource` for tests and externally supplied in-memory documents. This is unlikely to explain warm page rendering, but it removes file-copy cost and makes cold/open benchmarks representative.

### 5.11 Unused configuration

`CpuBackendOptions::threads`, `tile_size`, and the Rayon dependency are currently not active in execution.

Do not tune these values yet. Either:

* Remove or clearly label them as inactive until tiled rendering exists, or
* Implement them only after profiling shows that page-level parallelism leaves unacceptable latency on unusually large pages.

Nested Rayon parallelism could oversubscribe the existing compile/render pools, so it must be coordinated with the scheduler rather than enabled independently.

---

## 6. Optimization sequence

### Phase A — Establish the baseline

1. Add the representative page corpus.
2. Reintroduce the `pdfium-diff bench` tooling into the working checkout.
3. Add cold, warm, compiled, prepared, warm-decoded, and decode-only modes.
4. Add structured stage timings and counters.
5. Produce baseline JSON/CSV, flamegraphs, hardware counters, and allocation profiles.
6. Store output hashes and differential metrics.

Exit gate: at least 90–95% of every representative page’s wall time is attributable to named stages.

### Phase B — Low-risk structural work

Implement and measure individually:

1. Persistent compile-worker contexts.
2. Mmap input for local files.
3. Timing of output conversion.
4. Correct observed peak-memory tracking.
5. Reusable masks, vectors, and moderately sized surfaces.
6. Removal of the tiling per-row copy.
7. A dedicated benchmark build profile.

Test build-profile variants rather than assuming they help:

* Existing release settings.
* `codegen-units = 1`.
* Thin LTO.
* Fat LTO.
* Native CPU targeting for local builds.
* Portable baseline plus runtime SIMD dispatch for distributed binaries.

Keep the profiling build separate from the fastest benchmark build when frame-pointer or debug settings affect optimization.

### Phase C — Image fast paths

Implement in this order:

1. Hoist all sampler invariants into `PreparedImage`.
2. Replace generic color-space matching with prepared sampler variants.
3. Add direct byte-aligned 8-bit Gray and RGB access.
4. Add dedicated packed Mono1/stencil access.
5. Add direct axis-aligned, Normal-blend, no-clip/no-mask paths.
6. Replace per-pixel matrix multiplication with row starts and coordinate increments.
7. Add a proper axis-aligned minification path:

   * Horizontal and vertical separable box filtering, or
   * Another exact-enough area resizer consistent with the differential gate.
8. Fuse resize, color conversion, mask application, and compositing where appropriate.
9. Add the bounded decoded-resource cache.
10. Add SIMD only to the surviving hot loops.

A scanned page should not pass through the fully generic affine sampler when it is simply an axis-aligned image scaled to the output page.

### Phase D — Text and vector pages

1. Add glyph outline caches.
2. Introduce adaptive curve flattening.
3. Measure the resulting edge-count reduction.
4. Specialize common coverage cases.
5. Prototype fixed-point active-edge-table coverage.
6. Add SIMD span kernels.
7. Optimize blend modes only when a representative transparency page identifies them as material.

### Phase E — Complex content

Address independently:

* Tiling lower-once/translate-many.
* Reusable transparency-group surfaces.
* Soft-mask derivation and compositing.
* Shading coordinate increments instead of a complete inverse transform per pixel.
* Reduced allocation and f32 work in non-Normal blend modes.

These should not delay the main scanned-page and text-page wins.

### Phase F — Codec projects

Only begin separate jp2lam, JPEG, JBIG2, or CCITT optimization work after the decode-only measurements are available.

For each codec, report:

* Codestream decode time.
* Adapter conversion/repacking time.
* Renderer sampling time.
* Source and destination pixels.
* Bytes allocated.
* Instructions per decoded pixel.
* Cache-miss behavior.

This will identify whether jp2lam itself is slow, whether the planar-to-interleaved adapter is slow, or whether the renderer spends most of its time resampling already decoded pixels.

---

## 7. Correctness and regression gates

Every optimization must retain:

* Existing unit and integration tests.
* Byte-identical output where the algorithm is intended to remain identical.
* Differential scores within an explicitly recorded tolerance where sampling or flattening changes.
* No new blank, destroyed, or missing-ink pages.
* Determinism across worker counts.
* Cancellation behavior.
* Memory-limit enforcement.
* Output consistency between repeated runs.

Use two performance scoreboards:

### Single-page latency

Report by page class:

* Cold total.
* Warm total.
* Compile.
* Codec decode.
* CPU preparation.
* Execute.
* Output conversion.
* Peak memory.
* Allocation count.

### Whole-document throughput

Report:

* Pages per second.
* p50, p90, and p99 page completion time.
* Compile and render worker utilization.
* Queue wait.
* Reorder-buffer depth.
* Peak in-flight memory.
* Results for several compile/render partitions.

Do not accept a latency improvement that damages document throughput through excessive shared locking or cache contention.

---

## 8. Initial hypotheses and decisive experiments

### Hypothesis 1: JPX decoding dominates MRC pages

Experiment:

* Compare compiled-page render against warm-decoded render.
* Time jp2lam decode and JPX interleave separately.

Interpretation:

* Large difference: codec or adapter work dominates.
* Small difference: image sampling/compositing dominates.

### Hypothesis 2: Generic minification dominates scanned pages

Experiment:

* Record source taps, destination pixels, and image-raster time.
* Add a temporary axis-aligned pre-resized image and compare.

Interpretation:

* A major improvement confirms the generic area-average path as the target.

### Hypothesis 3: Coverage and glyph flattening dominate text pages

Experiment:

* Separate glyph extraction, flattening, edge generation, coverage, and blending.
* Record repeated glyph IDs and generated edge counts.

Interpretation:

* Extraction-heavy: add outline cache.
* Edge-heavy: adaptive flattening.
* Coverage-heavy: active-edge/fixed-point work.
* Blend-heavy: span SIMD.

### Hypothesis 4: Compilation repeats document work

Experiment:

* Compare a new `ParseContext` per page against persistent compile-worker contexts.
* Record object and object-stream cache hit rates.

Interpretation:

* Improved compilation confirms that the current callback interface defeats intended cache reuse.

### Hypothesis 5: Allocations and large temporary surfaces hurt complex pages

Experiment:

* Run heap profiling on tiling, soft-mask, and transparency pages.
* Compare observed peak memory with scheduler estimates.

Interpretation:

* High churn justifies worker-local surface and mask pools before arithmetic optimization.

---

## 9. Missing material from the uploaded archive

> **Historical note (2026-07-20):** this section described the incomplete
> review ZIP this plan was originally written against, not the repository.
> The working tree is complete: `jp2lam` and `jbig2enc-rust` live in the
> adjacent Lege ecosystem checkout, the corpus and `tools/pdfium-diff`
> exist, and the parent design documents sit next to this repo. The list is
> retained only to explain the plan's original evidence limits.

The following items are required for direct empirical work but were not present in the ZIP:

1. `jp2lam`, referenced through `../../jp2lam`.
2. `jbig2enc-rust`, referenced through `../../jbig2enc-rust`.
3. The actual rendering corpus; only `corpus/README.md` is included.
4. The `tools/` directory and `pdfium-diff bench` implementation referenced by the documentation.
5. The parent design documents named in `README.md`:

   * `pdfium-concurrency-rust-port.md`
   * `expanded-rust-pdfrender-plan.md`
   * `skeleton-blueprint.md`
6. Raw benchmark outputs behind the 4–6× and 1.65–2.34× summaries.
7. The exact PDFium build, render flags, output sizes, hinting settings, and comparison command.
8. The benchmark machine’s CPU model, compiler flags, power/governor state, and operating system details.

The repository also lacks the stage-level instrumentation, allocation baseline, hardware-counter baseline, permanent performance corpus, and machine-readable regression reports described above.

Because the external path dependencies are absent, the uploaded checkout cannot be compiled or profiled as supplied. Static inspection is sufficient to design the optimization pass, but not to rank codec decode against sampling, compilation, and coverage empirically.

---

## 10. Recommended first implementation milestone

The first milestone should contain no major optimization. It should add:

1. `CompileStats`.
2. Expanded `RenderStats`.
3. Per-codec timing at the codec-registry seam.
4. Image sample-tap and fast-path counters.
5. Cold, warm, compiled, prepared, warm-decoded, and decode-only benchmarks.
6. CSV or JSON output.
7. Persistent compile-worker contexts.
8. Correct peak-memory observation.
9. A 12–20-page performance corpus.
10. A script that produces:

    * Renderer timings.
    * PDFium timings.
    * Differential scores.
    * Flamegraphs.
    * `perf stat` results.
    * Allocation reports.

After that milestone, one profiling run should unambiguously select among:

* jp2lam optimization,
* the JPX adapter,
* the generic image sampler,
* text/glyph lowering,
* analytic coverage,
* repeated compilation,
* or allocation and memory behavior.

That is the point at which an optimization pass becomes straightforward rather than speculative.
