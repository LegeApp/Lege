The main adjustment is to stop treating Phase 4 as a “reference rasterizer.” It should be the **production scalar CPU renderer**. The scalar kernels are the reference implementation for later SIMD kernels, but the surrounding renderer should be designed for speed from the first commit.

The existing plan has the right upper boundary: `CompiledPage` is backend-neutral, resource references are compact `u32` handles, and semantic geometry remains `f64` until backend lowering. Those decisions give the CPU backend freedom to build a much more specialized execution representation without contaminating the IR. 

## Recommended architecture

```text
CompiledPage + RenderRequest
            │
            ▼
      CpuPageLowerer
  transform, cull, classify,
  flatten, stroke, bind resources
            │
            ▼
      CpuPreparedPage
  compact commands + prepared geometry
  clip graph + layer graph + bounds
            │
            ▼
       RasterPlanner
  DirectPage | HorizontalBands | Tiles
            │
            ▼
       RegionExecutor
  coverage generation → spans
            │
            ▼
        KernelSet
  fill / composite / sample / mask
            │
            ▼
     CpuSurface / HostPage
```

The critical boundary is:

> `CompiledPage` describes what the PDF paints. `CpuPreparedPage` describes how this particular render request should execute on a CPU.

Do not execute `DisplayOp` directly in the hot loop.

## 1. Add a request-specific CPU lowering stage

`CompiledPage` should remain reusable across output sizes and future GPU backends. The CPU representation should be generated per render request because page scale, rotation, crop, output format, clipping, and device dimensions affect nearly every optimization.

```rust
pub struct CpuPreparedPage {
    pub size: DeviceSize,
    pub surface_kind: CpuSurfaceKind,

    pub commands: Box<[CpuCommand]>,
    pub command_bounds: Box<[DeviceRect]>,

    pub paths: Box<[PreparedPath]>,
    pub strokes: Box<[PreparedStroke]>,
    pub images: Box<[PreparedImage]>,
    pub glyph_runs: Box<[PreparedGlyphRun]>,

    pub clips: Box<[PreparedClip]>,
    pub layers: Box<[PreparedLayer]>,

    pub complexity: CpuComplexity,
}
```

The lowerer should do the following once:

1. Compose the PDF-to-device transform.
2. Cull objects outside the crop and output bounds.
3. Classify common operations into fast paths.
4. Resolve all resource handles to dense CPU-side indexes.
5. Convert colors into the chosen internal surface representation.
6. Flatten curves in device space.
7. Expand strokes in device space.
8. Calculate exact or conservative device-space bounds.
9. Build clip and transparency-layer relationships.
10. Produce scheduling cost estimates.

No dictionary lookup, `Arc` clone, resource hash lookup, font lookup, or transform composition should occur in the pixel executor.

### Compact CPU commands

Do not carry a large Rust enum containing arbitrary payloads through the hot loop. Lower it to a compact command header and typed payload arenas.

Conceptually:

```rust
#[repr(C)]
pub struct CpuCommand {
    pub kind: u8,
    pub flags: u8,
    pub reserved: u16,
    pub payload: u32,
    pub clip: u32,
    pub layer: u32,
}
```

The exact layout should be benchmarked, but the objective is a predictable 16- or 24-byte record. Bounds can remain in a parallel array because binning needs them, while execution often does not.

This preserves the plan’s existing typed-handle model while removing large enums and pointer chasing from raster execution. 

## 2. Do not make tiling mandatory

The plan currently describes the CPU rasterizer as tiled.  That should mean **tile-capable**, not **always tiled**.

Because your primary concurrency source is different pages, mandatory page tiling can make per-page performance worse by adding:

* Command binning
* Repeated clip setup
* Additional command references
* Tile-edge handling
* More surface bookkeeping
* Worse behavior for full-page paths and images

Use an adaptive execution plan:

```rust
pub enum RasterPlan {
    DirectPage,
    HorizontalBands {
        band_height: u16,
    },
    Tiles {
        tile_width: u16,
        tile_height: u16,
        index: TileCommandIndex,
    },
}
```

### Direct-page mode

Use for ordinary pages when several pages are available to render concurrently.

This should be the primary mode for matching PDFium per-page performance:

* One page worker
* One output surface
* Sequential PDF painter order
* Scanline/span rasterization
* No tile index
* No internal thread synchronization

### Horizontal-band mode

Use when a full page is large enough that temporary masks or group surfaces become problematic.

A band might be 32–128 scanlines high. It preserves scanline locality while bounding scratch memory. It is generally cheaper than square tiling for wide document pages.

### Tile mode

Use for:

* Very large pages
* Documents with only one or two expensive pages
* Complex local transparency
* Low-latency single-page rendering
* Cases where memory limits prohibit full-width bands

Tiles should be region executions of the same prepared page, not a second raster architecture.

The planner should make this decision from measured cost, available page parallelism, output bytes, and transient-surface estimates. Avoid fixed policy such as “all pages use 256×256 tiles.”

## 3. Port PDFium’s raster algorithm, not its object architecture

The lowest-risk route to PDFium-class performance is to port the intent and scalar algorithms of its AGG rendering path:

* Device-space path conversion
* Stroke expansion
* AGG-style analytic scanline antialiasing
* Coverage spans
* Rect-or-mask clipping
* Specialized scanline compositors
* Separate image and text paths

Current PDFium still uses an AGG scanline-AA rasterizer and dispatches generated spans into destination-format-specific compositors. Its clip representation also distinguishes the cheap rectangular case from a bitmap-mask case. ([Pdfium Git Repositories][1])

That is a good performance architecture. What you should not port is PDFium’s mutable device/document ownership model.

The Phase 4 path engine should therefore be:

```text
Path commands
    ↓
device-space flattening
    ↓
stroke expansion, when needed
    ↓
fixed-point edges/cells
    ↓
analytic scanline coverage
    ↓
contiguous coverage spans
    ↓
specialized compositor
```

Do not start with:

* Supersampling the entire tile
* Converting every path into triangles
* Rendering every operation through a generic scene library
* Allocating a mask image for every path

Those approaches can work, but they are less likely to match PDFium’s scalar per-page performance.

## 4. Make spans the CPU hot-path unit

The executor should not blend one abstract pixel at a time. Coverage generation should emit contiguous spans:

```rust
pub struct CoverageSpan {
    pub x: u32,
    pub len: u32,
    pub coverage_offset: u32,
}
```

Possible span forms:

* Fully covered constant span
* Constant partial-coverage span
* Per-pixel coverage span
* Masked span

The renderer then selects a kernel once per span:

```text
opaque solid + full coverage
solid + coverage
solid + coverage + clip mask
image row + coverage
glyph mask + solid
general blend fallback
```

This is where most future SIMD value will reside.

PDFium’s current scalar path similarly resolves destination format and compositing behavior outside the innermost logic rather than running one universal pixel formula for every case. ([Pdfium Git Repositories][1])

## 5. Build fast paths before general correctness paths

The common cases should not pass through the most general compositor.

### Essential initial classifications

```rust
enum CpuDrawClass {
    OpaqueRect,
    AlphaRect,
    SolidPath,
    SolidPathWithClip,
    GlyphRun,
    ImageCopy,
    ImageAxisScale,
    ImageAffine,
    GeneralPaint,
}
```

High-value fast paths include:

* Integer-aligned opaque rectangle: direct row fill
* Fractional axis-aligned rectangle: specialized AA rectangle
* Opaque solid span: overwrite without destination read
* Solid normal source-over: integer premultiplied blend
* Rectangular clipping: adjust pointers and lengths
* Image translation without scaling: row copy or row composite
* Axis-aligned image scaling: incremental scanline sampler
* One-bit image mask with solid color: mask compositor
* Repeated glyph run with one transform: batch setup once
* Empty or fully opaque clip: eliminate clip-mask access

A general path should exist, but it should not define the performance of ordinary generated PDFs.

## 6. Use optimized internal surface formats

Do not force every intermediate into one public format.

```rust
pub enum CpuSurfaceKind {
    Bgrx8Opaque,
    Bgra8Premultiplied,
    Gray8,
    Alpha8,
}
```

### Main page surface

Prefer:

* `Bgrx8Opaque` when the requested background is opaque and no retained alpha is required
* `Bgra8Premultiplied` when transparency must survive
* `Gray8` for a validated grayscale fast path
* `Alpha8` for clips, glyph masks, and soft masks

Premultiplied alpha makes the normal source-over path straightforward and matches the eventual GPU representation well. Complex PDF blend modes can use a slower specialized path that temporarily derives straight color where required.

A direct `Gray8` renderer could be particularly valuable for your e-ink pipeline, but it should initially be restricted to pages whose color and blend features make grayscale rendering equivalent to color rendering followed by conversion. Do not apply it blindly to nonseparable blend modes, unusual color spaces, or transparency groups.

PDFium’s scanline compositor similarly has distinct paths for grayscale, masks, RGB, and alpha-bearing destinations. ([Pdfium Git Repositories][2])

### Surface memory

Use:

* 64-byte-aligned base allocation
* Stride padded to at least 64 bytes
* Explicit stride in all APIs
* Dirty bounds for partial initialization
* Pooled `Alpha8` and BGRA transient surfaces
* Bounds-limited group surfaces

Do not require tightly packed rows internally.

## 7. Design a scalar kernel ABI now

SIMD readiness does not mean arranging every structure as speculative SoA data. It means isolating contiguous arithmetic kernels from orchestration.

```rust
pub struct KernelSet {
    pub clear_u32: ClearU32Fn,
    pub fill_solid_opaque: FillSolidOpaqueFn,
    pub blend_solid_mask: BlendSolidMaskFn,
    pub blend_image_row: BlendImageRowFn,
    pub multiply_masks: MultiplyMasksFn,
    pub expand_mono_mask: ExpandMonoMaskFn,
    pub convert_rgb_to_gray: ConvertRgbToGrayFn,
}
```

Example scalar boundary:

```rust
pub type BlendSolidMaskFn = unsafe fn(
    dst: *mut u32,
    coverage: *const u8,
    len: usize,
    premul_color: u32,
);
```

The safe wrapper validates lengths and aliasing. The kernel receives contiguous buffers and performs no allocation, resource lookup, clipping decision, or paint dispatch.

Later:

```text
KernelSet::scalar()
KernelSet::sse41()
KernelSet::avx2()
KernelSet::avx512()
KernelSet::neon()
```

Runtime selection happens once when constructing the CPU backend or worker context.

Do not call a function pointer per pixel. Call it once per sufficiently long span. Very short spans can use an inline scalar helper to avoid dispatch overhead.

### Scalar implementation rules

Write the scalar kernels so they are easy to translate later:

* Simple counted loops
* Contiguous input and destination
* No iterator adapters in measured inner loops
* No closure calls
* No branch on pixel format inside the loop
* No branch on blend mode inside the loop
* No bounds checks repeated per channel
* Integer alpha math with a shared `mul_div_255` primitive
* Separate prefix, vector body, and suffix conceptual regions
* No hidden buffer aliasing

A small, audited amount of `unsafe` should be allowed in `pdf-render-cpu::memory` and `pdf-render-cpu::kernels`. Keep the public backend safe. Trying to forbid unsafe permanently will make later SIMD, aligned allocation, and bounds-check elimination unnecessarily difficult.

## 8. Treat clipping as a persistent graph

Use a dense clip ID for every distinct clip state:

```rust
pub struct PreparedClip {
    pub parent: Option<ClipId>,
    pub bounds: DeviceRect,
    pub kind: PreparedClipKind,
}

pub enum PreparedClipKind {
    Rect(DeviceRect),
    Path(PreparedPathId, FillRule),
}
```

At execution:

1. Intersect all rectangular ancestors arithmetically.
2. Generate an `Alpha8` mask only when a nonrectangular clip is actually used.
3. Generate it only for the executing region.
4. Cache the result by `(ClipId, RegionId)` during that page execution.
5. Multiply masks with a dedicated mask kernel.

Do not generate full-page masks during lowering.

Do not use a hash map in the draw loop. Clip IDs are dense, so use indexed vectors and region-local cache slots.

## 9. Keep transparency in bounded layers

Represent transparency as a layer hierarchy in `CpuPreparedPage`, even if Phase 4 initially implements only the simpler cases.

Each prepared layer should have:

```rust
pub struct PreparedLayer {
    pub bounds: DeviceRect,
    pub isolated: bool,
    pub knockout: bool,
    pub blend_mode: BlendMode,
    pub opacity: u8,
    pub soft_mask: Option<SoftMaskId>,
    pub command_range: Range<u32>,
}
```

The executor should allocate the smallest safe surface for the intersection of:

* Layer bounds
* Current clip bounds
* Current region bounds

A group occupying 300×200 pixels should never allocate a page-sized surface.

Use worker-local transient surface pools. A shared global surface pool is likely to introduce lock contention and unpredictable retained memory.

## 10. Make image rendering a fused scanline operation

Avoid Hayro’s current pattern of resizing into a new allocation, expanding to RGBA, and then feeding the result into a general compositor.

The CPU image architecture should retain codec-native representations:

```text
Mono1
Gray8
Gray16
Rgb8
Rgba8
Cmyk8
Indexed8
```

Then select an image pipeline:

```text
decode row / source row
        ↓
sample and transform
        ↓
apply Decode array
        ↓
color conversion
        ↓
mask or soft mask
        ↓
blend directly into destination span
```

The common cases deserve distinct implementations:

1. Direct translation
2. Axis-aligned nearest sampling
3. Axis-aligned bilinear sampling
4. Separable high-quality downscale
5. General affine sampling

Do not create an entire transformed image unless reuse or filter semantics justify it.

Shared document cache:

* Decoded source image
* Palette
* ICC/profile interpretation
* Decode metadata

Worker-local state:

* Sampling coefficients
* Scanline buffers
* Temporary converted row

This is both faster now and an obvious SIMD target later.

## 11. Separate font assets from glyph execution

Use three layers:

```text
Shared FontProgram
    immutable font bytes/tables/outlines

PreparedFontInstance
    size, font matrix, synthetic style, render mode

Worker-local GlyphCache
    masks or prepared device-space geometry
```

The common text path should process a `PreparedGlyphRun`, not issue one generic draw command per glyph.

```rust
pub struct PreparedGlyphRun {
    pub font_instance: FontInstanceId,
    pub glyphs: Range<u32>,
    pub paint: PreparedPaintId,
    pub clip: ClipId,
    pub transform_class: GlyphTransformClass,
}
```

Classify transforms:

* Axis-aligned translation and scale
* Axis-aligned with fractional positioning
* General affine
* Type 3 display-list glyph

Cache glyph masks only where the cache key remains compact and reuse is likely. Cache immutable outlines more aggressively. Arbitrary transformed glyph bitmaps can otherwise cause cache explosion.

PDFium also keeps text and Type 3 handling as specialized render paths rather than treating every glyph as an ordinary arbitrary page path. ([Pdfium Git Repositories][3])

## 12. Use one physical CPU pool

The blueprint defines logical compile and render stages with bounded queues and memory permits.  Keep those logical stages, but do not automatically create two full-size Rayon pools.

Two pools of `N` workers can produce `2N` CPU-bound threads and degrade:

* Cache locality
* Scheduler behavior
* Memory bandwidth
* Tail latency

Use either:

* One physical work-stealing pool beneath both logical stages, or
* An explicitly partitioned pool based on profiling

The scheduler should generally favor page-level tasks:

```text
compile page 10
render page 7
compile page 11
render page 8
```

Inside the CPU backend, one page should normally remain single-worker. Only split into bands or tiles when the scheduler knows there are insufficient independent pages or the page is exceptionally expensive.

Do not call `par_iter()` from inside a task already running in the render pool without an explicit nested-parallelism policy.

## 13. Worker context should own all reusable scratch

```rust
pub struct CpuWorkerContext {
    pub kernels: &'static KernelSet,

    pub edge_arena: Vec<Edge>,
    pub cell_arena: Vec<Cell>,
    pub span_arena: Vec<CoverageSpan>,
    pub coverage: AlignedBuffer<u8>,

    pub image_row: AlignedBuffer<u8>,
    pub mask_row: AlignedBuffer<u8>,

    pub clip_cache: ClipExecutionCache,
    pub glyph_cache: WorkerGlyphCache,
    pub transient_surfaces: TransientSurfacePool,
}
```

After warm-up, ordinary page rendering should perform almost no small heap allocation.

Rules:

* Reuse capacities with `clear()`.
* Never shrink worker scratch during normal rendering.
* Track touched ranges instead of clearing full buffers.
* Do not zero an entire tile mask when only a small span was written.
* Put upper bounds on retained worker memory.
* Release unusually large allocations after pathological pages.
* Keep all hot-path structures worker-local.

The blueprint’s rule that state must live in the immutable snapshot, explicit caches, or worker contexts is exactly right. 

## 14. Keep hash maps and synchronization out of execution

By the time `RegionExecutor` starts:

* Images have dense indexes.
* Fonts have dense indexes.
* Clips have dense indexes.
* Paints have dense indexes.
* Layers have dense indexes.
* Paths have dense indexes.
* Kernels are selected.
* Output format is selected.

The hot loop should use indexed arrays and slices.

Shared caches should use once-publication for expensive immutable resources. Page execution should never lock a document-wide cache merely to retrieve an already prepared object.

## 15. Build an adaptive page classifier

During CPU lowering, generate:

```rust
pub struct CpuComplexity {
    pub command_count: u32,
    pub path_segments: u64,
    pub glyph_count: u64,
    pub source_image_pixels: u64,
    pub estimated_covered_pixels: u64,
    pub complex_clip_count: u32,
    pub layer_count: u32,
    pub largest_layer_pixels: u64,
    pub features: CpuFeatureSet,
}
```

Use this to select:

* Surface format
* Direct/band/tile mode
* Tile or band size
* Eager versus lazy geometry preparation
* Whether to build a tile index
* Whether to parallelize within the page
* Memory permits required

This is preferable to static presets.

## 16. Phase 4 implementation order

### 4A. Benchmark and memory contract

Before path rendering:

* Pin a PDFium build and flags.
* Define the exact page transform and pixel format.
* Benchmark open, compile, lower, and raster separately.
* Add allocation-count and bytes-zeroed counters.
* Add p50, p90, and p99 corpus reporting.
* Add one-page and whole-document tests.

### 4B. Surface and scalar kernels

Implement:

* Aligned surfaces
* Padded strides
* Clear
* Opaque fill
* Solid source-over
* `Alpha8` mask multiplication
* Surface subviews
* Transient pool

Benchmark these independently against memory-bandwidth expectations.

### 4C. Rectangles and rectangular clipping

This proves:

* Lowering
* Fast-path classification
* Span dispatch
* Opaque and alpha surfaces
* Clip arithmetic

No general path rasterizer yet.

### 4D. General fills

Port the AGG-style scalar coverage generator:

* Device-space flattening
* Fixed-point edges
* Nonzero and even-odd winding
* Scanline cells
* Coverage spans
* Solid compositing

### 4E. Strokes

Add:

* Caps
* Joins
* Miter limits
* Dashes
* Hairline behavior
* Text-stroke policy

Expand once during lowering, not separately per region.

### 4F. Complex clips

Add lazy region-local clip masks and clip caching.

### 4G. Images

Implement the common transform classes before general affine sampling.

### 4H. Text

Add prepared glyph runs, immutable outline caching, and worker-local mask caching.

### 4I. Transparency

Add bounded layers, soft masks, ordinary blend modes, then knockout and unusual modes.

### 4J. Adaptive region planning

Only after direct-page rendering is competitive should bands and tiles be enabled automatically.

## 17. Performance gates

Do not use only a single average.

Track at least four classes:

| Corpus class                  | Primary risk                        |
| ----------------------------- | ----------------------------------- |
| Generated text/vector PDFs    | Interpretation and path overhead    |
| Scanned PDFs                  | Decode, scale, and memory bandwidth |
| Mixed office documents        | Glyphs, images, clips               |
| Complex graphics/transparency | Masks, layers, blending             |

For every class record:

* CPU page-lowering time
* Raster time
* Resource decode time
* Allocations
* Bytes allocated
* Bytes cleared
* Bytes copied
* Peak scratch
* Peak output/transient memory
* Commands
* Generated path edges
* Coverage pixels
* Composited pixels
* Clip-mask pixels
* Worker scaling

The two required scoreboards should be separate:

```text
Per-page:
one page, one worker, warm and cold resource caches

Whole-document:
same document, 1 / 2 / 4 / 6 / N workers
```

A useful Phase 4 standard is not necessarily “beat PDFium on every page before SIMD.” It is:

1. No architectural bottleneck is hiding behind allocation or generic dispatch.
2. Common text/vector pages are near PDFium scalar performance.
3. Image pages are limited primarily by decoding and memory bandwidth.
4. Whole-document throughput scales until memory bandwidth becomes dominant.
5. Scalar span kernels account for a clearly isolated proportion of runtime that SIMD can later improve.
6. SIMD can be added by replacing `KernelSet`, not rewriting rendering orchestration.

## Most important changes to the operating plan

I would formally change this description:

```text
pdf-render-cpu
    reference rasterizer (tiled)
```

to:

```text
pdf-render-cpu
    production scalar raster engine
    adaptive direct-page / band / tile execution
    scalar reference kernels with replaceable SIMD KernelSet
```

And define Phase 4’s central implementation rule as:

> **Port PDFium’s mature scanline and compositor algorithms into a new request-lowered, worker-local, backend-specific architecture.**

That gives the strongest chance of matching PDFium per page. At the same time, adaptive page-level scheduling provides the second victory condition: exceeding serialized PDFium whole-document throughput even where individual pages remain moderately slower.

[1]: https://pdfium.googlesource.com/pdfium/%2B/40a40484c7651c17d1abc6d9440079815581dae2/core/fxge/agg/fx_agg_driver.cpp "core/fxge/agg/fx_agg_driver.cpp - pdfium - Git at Google"
[2]: https://pdfium.googlesource.com/pdfium/%2B/4cf91142a765388828b1672b0efb004e1dc3ed75/core/fxge/dib/cfx_scanlinecompositor.cpp?utm_source=chatgpt.com "core/fxge/dib/cfx_scanlinecompositor.cpp - pdfium - Git at Google"
[3]: https://pdfium.googlesource.com/pdfium/%2B/refs/heads/main/core/fpdfapi/render/cpdf_renderstatus.h?utm_source=chatgpt.com "core/fpdfapi/render/cpdf_renderstatus.h - pdfium - Git at Google"
