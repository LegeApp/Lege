The roadmap below treats the CPU renderer as the reference implementation and makes WGPU an optional backend introduced only after parsing, page compilation, rendering semantics, and concurrency are stable.

:::writing{variant="document" id="48317"}
# Roadmap: Concurrent Rust PDF Renderer with a Replaceable WGPU Backend

## 1. Project objective

Build a read-only Rust PDF rendering engine that:

1. Opens a PDF once and exposes an immutable document snapshot.
2. Compiles different pages concurrently.
3. Renders different pages concurrently through a CPU backend.
4. Defines rendering semantics independently of either CPU or GPU implementation.
5. Allows a WGPU renderer to replace the CPU raster backend without changing parsing, page interpretation, scheduling, or application-level page processing.
6. Retains the CPU backend permanently as:
   - The compatibility fallback.
   - The reference implementation.
   - The differential-testing oracle.
   - The backend for unsupported GPU features or unavailable GPUs.
7. Eventually allows rendered pages and postprocessing intermediates to remain GPU-resident, while preserving a compatibility API that returns ordinary CPU pixel buffers.

The WGPU backend is not an initial implementation target. The architecture must accommodate it from the beginning without requiring GPU abstractions, GPU dependencies, or asynchronous GPU lifecycle handling in the parser and CPU renderer.

---

# 2. Architectural invariants

These rules should be established before substantial implementation begins.

## 2.1 PDF semantics must not depend on the raster backend

The following layers must not import `wgpu`, CPU rasterizer implementation types, operating-system graphics APIs, or backend-specific pixel surfaces:

- PDF byte-source access
- Syntax parsing
- Cross-reference handling
- Object resolution
- Encryption
- Page-tree resolution
- Content-stream interpretation
- Font encoding and CMap interpretation
- Resource resolution
- Graphics-state evaluation
- Page display-list construction

The result of page interpretation must be a backend-neutral compiled page.

```text
PDF bytes
   ↓
DocumentSnapshot
   ↓
PageCompiler
   ↓
CompiledPage
   ├── CPU lowering → CPU renderer
   └── GPU lowering → WGPU renderer
```

## 2.2 Rendering must not mutate the document

Rendering page 20 must not add derived state to:

- PDF dictionaries
- Indirect objects
- Page objects
- Font dictionaries
- Image stream objects
- Shared resource dictionaries

Canonical PDF objects should be immutable after publication.

Derived data belongs in:

- A shared immutable resource cache
- A backend-specific resource cache
- A worker-local cache
- A page render session
- A temporary render arena

## 2.3 The CPU backend defines correctness

The initial CPU backend should be treated as the normative implementation of the engine’s rendering semantics.

The GPU backend is accepted feature by feature only when it matches the CPU backend within predefined tolerances. It should not become the only implementation of any PDF feature until the CPU implementation and test coverage exist.

## 2.4 Backend selection occurs after page compilation

The parser should not decide whether a page will use the CPU or GPU.

`CompiledPage` should include a feature summary:

```rust
bitflags::bitflags! {
    pub struct PageFeatures: u64 {
        const BASIC_PATHS              = 1 << 0;
        const TEXT                     = 1 << 1;
        const IMAGES                   = 1 << 2;
        const CLIPPING                 = 1 << 3;
        const TRANSPARENCY             = 1 << 4;
        const SOFT_MASKS               = 1 << 5;
        const PATTERNS                  = 1 << 6;
        const SHADINGS                  = 1 << 7;
        const TYPE3_FONTS               = 1 << 8;
        const ICC_COLOR                 = 1 << 9;
        const NONSEPARABLE_BLEND_MODES = 1 << 10;
        const OVERPRINT                 = 1 << 11;
    }
}
```

The scheduler asks a backend whether it can render that compiled page.

```rust
pub trait RenderBackend {
    fn capabilities(&self) -> BackendCapabilities;

    fn supports(
        &self,
        page: &CompiledPage,
        request: &RenderRequest,
    ) -> SupportLevel;
}
```

Initial fallback granularity should be an entire page:

```text
GPU supports every required feature → GPU page render
GPU lacks one required feature      → CPU page render
```

Do not begin with partial CPU/GPU rendering of the same page. Crossing the CPU/GPU boundary within a page introduces readbacks, synchronization, surface conversion, and compositing consistency problems.

## 2.5 The common API must not expose WGPU types

No public common-renderer API should require:

- `wgpu::Device`
- `wgpu::Texture`
- `wgpu::Buffer`
- `wgpu::CommandEncoder`
- WGPU bind groups
- Shader-specific resource identifiers

The WGPU crate can expose an optional advanced integration API, but the normal renderer API should use backend-neutral requests, handles, and results.

---

# 3. Proposed workspace structure

```text
pdf-renderer/
├── crates/
│   ├── pdf-source/
│   ├── pdf-syntax/
│   ├── pdf-object/
│   ├── pdf-structure/
│   ├── pdf-security/
│   ├── pdf-document/
│   ├── pdf-font/
│   ├── pdf-image/
│   ├── pdf-color/
│   ├── pdf-content/
│   ├── pdf-page-ir/
│   ├── pdf-render-api/
│   ├── pdf-render-scheduler/
│   ├── pdf-render-cpu/
│   ├── pdf-render-wgpu/
│   ├── pdf-postprocess/
│   ├── pdf-test-support/
│   └── pdf-cli/
├── fuzz/
├── corpus/
├── shaders/
└── tools/
```

## Dependency direction

```text
pdf-source
    ↓
pdf-syntax
    ↓
pdf-object
    ↓
pdf-structure
    ↓
pdf-document
    ↓
pdf-content ──────── pdf-font / pdf-image / pdf-color
    ↓
pdf-page-ir
    ↓
pdf-render-api
    ├── pdf-render-cpu
    ├── pdf-render-wgpu
    └── pdf-render-scheduler
```

Neither renderer backend should be a dependency of `pdf-page-ir`.

The CPU renderer must build and work when the WGPU crate and feature are disabled.

---

# 4. Core data model

## 4.1 Random-access source

Use immutable positional reads, not shared seeking.

```rust
pub trait PdfSource: Send + Sync {
    fn len(&self) -> u64;

    fn read_exact_at(
        &self,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<(), SourceError>;

    fn slice(
        &self,
        range: std::ops::Range<u64>,
    ) -> Result<SourceSlice, SourceError>;
}
```

Implementations:

- `MmapSource`
- `OwnedBytesSource`
- `FileReadAtSource`
- Optional chunked/network source later

All page workers may read the source simultaneously.

## 4.2 Immutable document snapshot

```rust
pub struct DocumentSnapshot {
    source: Arc<dyn PdfSource>,
    structure: Arc<DocumentStructure>,
    pages: Arc<PageIndex>,
    objects: Arc<ObjectRepository>,
    security: Option<Arc<SecurityContext>>,
    limits: Arc<DocumentLimits>,
}
```

The snapshot should become read-only after document opening and structural recovery.

Document opening performs all mutations needed to establish a stable interpretation:

- Locate headers and trailers.
- Read xref tables and streams.
- Merge incremental revisions.
- Establish current indirect-object locations.
- Identify compressed objects.
- Resolve the catalog.
- Build the page index.
- Establish encryption state.
- Record structural recovery decisions.

## 4.3 Object repository

Canonical parsed objects should be immutable and identified by object number and generation.

```rust
pub struct ObjectId {
    pub number: u32,
    pub generation: u16,
}

pub struct ObjectRepository {
    slots: Box<[ObjectSlot]>,
}

struct ObjectSlot {
    state: ObjectState,
}
```

The mature implementation may use once-publication:

```rust
struct ObjectSlot {
    value: OnceLock<Result<Arc<PdfObject>, Arc<ObjectError>>>,
}
```

However, the first concurrent implementation may use worker-local parsed-object caches over a shared immutable structural index. That is simpler and should be benchmarked before introducing shared concurrent object initialization.

Recommended progression:

1. Shared bytes, independent worker object caches.
2. Shared decompressed stream bytes.
3. Shared immutable parsed objects.
4. Shared expensive derived resources where measurements justify them.

## 4.4 Explicit parse context

Parsing limits and recursion state should not be global.

```rust
pub struct ParseContext {
    recursion: Vec<ObjectId>,
    decoded_bytes_used: usize,
    objects_visited: usize,
    local_cache: LocalObjectCache,
    scratch: ParseScratch,
}
```

Each page-compilation worker owns one context.

---

# 5. Backend-neutral page representation

The display-list intermediate representation is the central compatibility boundary.

## 5.1 Separate semantic compilation from raster lowering

Use three representations rather than one:

```text
PDF operators
    ↓
SemanticPage
    ↓
CompiledPage
    ├── CpuPreparedPage
    └── GpuPreparedPage
```

### `SemanticPage`

Contains validated PDF-level operations and resolved resources but may retain high-level PDF concepts.

### `CompiledPage`

Contains explicit painter-order operations suitable for any backend.

### Backend-prepared page

Contains tessellation, atlas references, packed buffers, tile bins, or other backend-specific acceleration data.

The common `CompiledPage` must not contain backend-specific prepared data.

## 5.2 Suggested compiled operations

```rust
pub enum DisplayOp {
    Save,
    Restore,

    ConcatTransform(Matrix),

    PushClip {
        path: PathId,
        rule: FillRule,
    },
    PopClip,

    FillPath {
        path: PathId,
        paint: PaintId,
        rule: FillRule,
        alpha: f32,
        blend: BlendMode,
    },

    StrokePath {
        path: PathId,
        paint: PaintId,
        style: StrokeStyleId,
        alpha: f32,
        blend: BlendMode,
    },

    DrawGlyphRun {
        run: GlyphRunId,
        paint: PaintId,
        alpha: f32,
        blend: BlendMode,
    },

    DrawImage {
        image: ImageId,
        transform: Matrix,
        interpolation: InterpolationMode,
        alpha: f32,
        blend: BlendMode,
    },

    BeginTransparencyGroup {
        group: TransparencyGroupId,
    },

    EndTransparencyGroup,

    ApplySoftMask {
        mask: SoftMaskId,
    },

    DrawShading {
        shading: ShadingId,
        transform: Matrix,
    },
}
```

Painter order must remain explicit. Optimizers may batch or reorder only when equivalence is proven.

## 5.3 Resource tables

```rust
pub struct CompiledPage {
    pub bounds: PageBounds,
    pub operations: Arc<[DisplayOp]>,
    pub paths: Arc<[PathData]>,
    pub paints: Arc<[Paint]>,
    pub stroke_styles: Arc<[StrokeStyle]>,
    pub glyph_runs: Arc<[GlyphRun]>,
    pub images: Arc<[ImageResource]>,
    pub masks: Arc<[MaskResource]>,
    pub groups: Arc<[TransparencyGroup]>,
    pub shadings: Arc<[ShadingResource]>,
    pub features: PageFeatures,
    pub complexity: PageComplexity,
}
```

Large resource bytes can remain in document-level `Arc` storage. The page resource table should use stable handles rather than duplicate data.

## 5.4 Precision policy

Retain PDF geometry and transformations in `f64` through semantic interpretation.

A portable WGPU backend generally should not depend on shader `f64`. During GPU lowering:

1. Divide the output into pages or tiles.
2. Translate geometry into tile-local coordinates.
3. Convert the smaller relative coordinates to `f32`.
4. Preserve transforms and clipping at sufficient precision.
5. Detect pages whose coordinate range cannot be represented safely.
6. Fall back to CPU when precision limits are exceeded.

The CPU renderer may continue using `f64`, fixed-point arithmetic, or a controlled mixture.

---

# 6. Common rendering API

## 6.1 Render request

```rust
pub struct RenderRequest {
    pub page: Arc<CompiledPage>,
    pub transform: PageTransform,
    pub crop: Option<DeviceRect>,
    pub output_size: DeviceSize,
    pub output_format: OutputFormat,
    pub background: Background,
    pub annotation_mode: AnnotationMode,
    pub quality: RenderQuality,
    pub limits: RenderLimits,
    pub output_residency: OutputResidency,
}
```

```rust
pub enum OutputResidency {
    HostRequired,
    BackendPreferred,
}
```

During the CPU-only stages, both modes return host-resident data.

The first WGPU implementation should support `HostRequired` by rendering on the GPU and performing asynchronous readback. This establishes drop-in compatibility.

Only after correctness is stable should `BackendPreferred` return a GPU-resident surface.

## 6.2 Job-based backend contract

Use a job interface rather than requiring all backends to behave synchronously.

```rust
pub trait RenderBackend: Send + Sync {
    fn id(&self) -> BackendId;

    fn capabilities(&self) -> BackendCapabilities;

    fn supports(
        &self,
        page: &CompiledPage,
        request: &RenderRequest,
    ) -> SupportLevel;

    fn submit(
        &self,
        request: RenderRequest,
    ) -> Result<RenderTicket, SubmitError>;
}
```

A CPU backend can fulfill the ticket through its thread pool. A WGPU backend can fulfill it when command submission and readback complete.

```rust
pub struct RenderTicket {
    job_id: RenderJobId,
    receiver: RenderResultReceiver,
}
```

The renderer should also expose a blocking convenience method:

```rust
pub fn render_blocking(
    backend: &dyn RenderBackend,
    request: RenderRequest,
) -> Result<RenderedPage, RenderError>;
```

Existing CPU-oriented applications can use the blocking API without knowing that a GPU backend may execute asynchronously.

## 6.3 Host-compatible result

```rust
pub struct HostPage {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub format: OutputFormat,
    pub pixels: Arc<[u8]>,
}
```

The initial stable API should return `HostPage`.

Later, add a separate advanced result:

```rust
pub enum RenderedPage {
    Host(HostPage),
    Resident(ResidentPage),
}
```

`ResidentPage` should be an opaque common handle, not a public `wgpu::Texture`.

```rust
pub struct ResidentPage {
    backend: BackendId,
    handle: BackendSurfaceHandle,
    metadata: SurfaceMetadata,
}
```

The owning backend provides:

- Postprocessing
- Readback
- Release
- Device-loss handling

This keeps the original host-returning API intact.

---

# 7. CPU-first implementation roadmap

## Phase 0: test corpus and observability

### Tasks

1. Establish a versioned rendering corpus containing:
   - Simple paths
   - Text
   - Embedded fonts
   - Images
   - Clipping
   - Transparency
   - Soft masks
   - Patterns
   - Shadings
   - Type 3 fonts
   - Color-space cases
   - Malformed but commonly accepted files
   - Large and adversarial pages

2. Build a reference-render tool that can invoke PDFium externally and save:
   - Rendered page
   - Dimensions
   - Rendering flags
   - PDFium version
   - Timing
   - Failure status

3. Define image comparison metrics:
   - Exact pixel match where applicable
   - Maximum per-channel difference
   - Mean absolute difference
   - Changed-pixel percentage
   - SSIM
   - Edge-weighted difference
   - Binary output disagreement after thresholding

4. Add structured traces:
   - Objects parsed
   - Streams decompressed
   - Font programs loaded
   - Images decoded
   - Display operations produced
   - Paths and segments
   - Transparency groups
   - Peak page memory
   - Parse, compile, and raster time

5. Add deterministic page hashes for:
   - Semantic operations
   - Compiled display list
   - Resource tables
   - CPU raster output

### Exit gate

The project can generate reproducible reference renders and compare future outputs automatically.

---

## Phase 1: document structure and immutable snapshot

### Tasks

Implement:

- Random-access sources
- Lexer and primitive parser
- Cross-reference tables
- Cross-reference streams
- Incremental updates
- Object streams
- Trailer resolution
- Catalog resolution
- Page-tree indexing
- Basic encryption
- Structural limits
- Cycle detection
- Error taxonomy
- Recovery reporting

Document opening should produce an immutable `DocumentSnapshot`.

### Concurrency requirement

Multiple threads must be able to:

- Query page metadata.
- Read independent object bytes.
- Resolve independent page roots.
- Create worker-local parse contexts.

No document-wide mutable cursor or parser may remain.

### Exit gate

Six threads can enumerate and structurally resolve six pages from the same snapshot under ThreadSanitizer-compatible native dependencies, with deterministic results and no document-wide rendering mutex.

---

## Phase 2: content interpreter and semantic page

### Tasks

Implement the PDF graphics-state machine:

- Save and restore
- Current transformation matrix
- Path construction
- Fill and stroke parameters
- Text state
- Text matrices
- Clipping
- Color state
- Resource lookup
- XObject invocation
- Recursion protection
- Content-stream arrays
- Inline images

Create `SemanticPage` without performing pixel rasterization.

### Validation

Provide a display-list dump tool:

```text
page 4
  save
  concat [1 0 0 1 20 40]
  set-fill DeviceRGB(0.2, 0.3, 0.4)
  fill path#17 nonzero
  draw-glyph-run run#4
  restore
```

Compare semantic dumps across repeated runs and worker counts.

### Exit gate

Supported pages compile concurrently into deterministic semantic representations without invoking any raster backend.

---

## Phase 3: stable compiled-page IR

### Tasks

1. Resolve inherited and indirect graphics resources.
2. Convert implicit PDF state changes into explicit display operations.
3. Intern paths, paints, transforms, glyph runs, and resource handles.
4. Record explicit clip-stack changes.
5. Represent transparency groups as explicit scoped operations or graph nodes.
6. Compute page feature flags.
7. Compute complexity estimates:
   - Operation count
   - Path segment count
   - Estimated tile coverage
   - Image pixels
   - Glyph count
   - Transparency surface requirements
8. Serialize a debug form of `CompiledPage`.
9. Add an IR schema version.

### Restrictions

The IR must contain no:

- CPU bitmap pointers
- Tessellation-library handles
- Glyph atlas positions
- WGPU objects
- Shader-specific layouts
- Thread-local cache references

### Exit gate

The same `CompiledPage` can be consumed by two simple independent test backends, such as:

- A debug SVG-like backend
- A minimal CPU bitmap backend

This demonstrates that the IR is not tied to one rasterizer.

---

## Phase 4: CPU raster backend foundation

### Components

```text
pdf-render-cpu
├── surface
├── tile
├── path
├── stroke
├── coverage
├── clip
├── image
├── text
├── blend
├── transparency
└── color
```

### Initial rendering model

Use a tiled CPU renderer even if the first version renders whole pages.

Suggested logical tile size:

- 256×256 for small working sets
- 512×512 as a likely general default
- Configurable after benchmarking

The CPU backend should lower `CompiledPage` into tile operation lists while preserving painter order.

### Worker-local state

```rust
pub struct CpuWorkerContext {
    path_scratch: PathScratch,
    coverage_scratch: CoverageScratch,
    blend_scratch: BlendScratch,
    image_scratch: ImageScratch,
    font_context: FontExecutionContext,
    tile_pool: TileBufferPool,
}
```

Avoid a global scratch allocator or font execution context.

### Initial feature order

1. Solid fills
2. Clipping
3. Strokes
4. Image drawing
5. Basic text
6. Alpha
7. Blend modes
8. Transparency groups
9. Soft masks
10. Patterns and shadings
11. Complex color

### Exit gate

The CPU backend renders the initial supported corpus from `CompiledPage` and matches reference images within defined thresholds.

---

## Phase 5: parallel page scheduling

### Architecture

```text
DocumentSnapshot
       │
Page compilation pool
       │
CompiledPage queue
       │
CPU render pool
       │
Completed-page reorder buffer
       │
Downstream processing
```

Compilation and rasterization should use separate bounded queues so that:

- Slow rendering does not block parsing.
- Parsing cannot run arbitrarily far ahead.
- Memory remains bounded.
- Pages can finish out of order but be emitted in order when required.

### Memory control

Define permits for:

- Compiled-page memory
- Decoded images
- Font resources
- CPU tile surfaces
- Full output pages
- In-flight downstream pages

A job must reserve its estimated working memory before rendering.

### Scheduling API

```rust
pub struct RenderScheduler {
    compiler_pool: WorkerPool,
    backend: Arc<dyn RenderBackend>,
    memory_budget: MemoryBudget,
    reorder: ReorderBuffer,
}
```

### Exit gate

On a representative multipage document:

- Six pages can compile simultaneously.
- Six pages can render simultaneously, subject to memory limits.
- Page outputs are deterministic across worker counts.
- No global PDF lock serializes the pipeline.
- Cancellation safely stops queued and active jobs.
- Failures on one page do not corrupt other page jobs.

---

## Phase 6: CPU feature completeness and stabilization

Before implementing WGPU, complete enough of the CPU backend that the GPU backend has a stable target.

### Required stabilization areas

- Text rendering and font substitution policy
- CMaps and CID fonts
- Type 3 fonts
- Image masks
- Soft masks
- Transparency groups
- Blend modes
- Patterns
- Axial and radial shadings
- ICC conversion
- DeviceCMYK policy
- Annotation appearance streams
- Malformed-resource behavior
- Resource-recursion limits
- Deterministic antialiasing policy
- Output color and alpha semantics

### Canonical surface contract

Freeze:

- Channel order
- Premultiplication behavior
- Transfer function
- Background application
- Rounding rules
- Clip-edge behavior
- Pixel-center convention
- Image interpolation policy
- Path antialiasing policy

For example:

```rust
pub enum OutputFormat {
    Rgba8PremultipliedSrgb,
    Gray8,
    Gray16,
}
```

Do not permit the CPU and GPU backends to silently use different alpha or color-space conventions.

### Exit gate

The CPU renderer is usable independently and the public `pdf-render-api` is considered stable enough that a new backend should not require parser or application changes.

---

# 8. Preparing for WGPU without implementing it

This work should happen during the CPU stages but must not require WGPU.

## 8.1 Add backend capability negotiation

```rust
pub struct BackendCapabilities {
    pub formats: OutputFormatSet,
    pub max_surface_size: DeviceSize,
    pub page_features: PageFeatures,
    pub resident_surfaces: bool,
    pub postprocess: PostprocessCapabilities,
}
```

## 8.2 Add page feature preflight

The page compiler should summarize all required features before rendering.

This allows an automatic policy:

```rust
match gpu.supports(page, request) {
    SupportLevel::Native => gpu.submit(request),
    SupportLevel::Unsupported(_) => cpu.submit(request),
}
```

## 8.3 Preserve operation boundaries

Do not flatten everything into CPU-specific span lists inside `CompiledPage`.

The GPU backend will need access to:

- Original paths
- Transforms
- Clip operations
- Draw ordering
- Images
- Glyph positions
- Transparency boundaries
- Blend modes

CPU-specific spans and tiles belong in `CpuPreparedPage`.

## 8.4 Separate decoded semantics from backend residency

An image resource should conceptually contain:

```rust
pub struct ImageResource {
    pub descriptor: ImageDescriptor,
    pub source: ImageSource,
    pub decode: DecodeParameters,
    pub color_space: ColorSpaceId,
    pub mask: Option<ImageMask>,
}
```

It should not permanently contain only a CPU RGBA bitmap. A backend-specific cache may hold:

- CPU decoded image
- GPU texture
- Compressed source bytes
- Multiple resolution variants

## 8.5 Establish resource identities

Use stable resource keys derived from:

- Document identity
- Object ID
- Decode parameters
- Color transform
- Output resolution where relevant

The same key can index independent CPU and GPU caches.

---

# 9. WGPU implementation roadmap

WGPU supports command encoders that record render passes, compute passes, and resource transfers before producing command buffers for queue submission. This permits the backend to assemble complete page or tile workloads before submitting them. 

## Phase 7: isolated WGPU backend skeleton

### Scope

Create `pdf-render-wgpu` behind a Cargo feature:

```toml
[features]
default = ["cpu"]
cpu = ["dep:pdf-render-cpu"]
wgpu = ["dep:pdf-render-wgpu"]
```

### Components

```text
pdf-render-wgpu
├── instance
├── adapter
├── device
├── scheduler
├── pipelines
├── shaders
├── resources
├── surface_pool
├── staging
├── lowering
└── diagnostics
```

WGPU provides a cross-platform Rust abstraction over native graphics backends including Vulkan, Metal, D3D12, and OpenGL. 

### First milestone

Do not render PDFs yet.

Implement:

- Adapter selection
- Device creation
- Queue ownership
- Capability reporting
- Device-loss reporting
- Bounded texture allocation
- Upload buffers
- Readback buffers
- Command submission
- Completion tracking
- Surface pooling
- A synthetic colored-rectangle job

### Exit gate

The WGPU backend accepts a synthetic backend-neutral render request, renders an offscreen image, reads it back asynchronously, and returns the same `HostPage` type as the CPU backend.

---

## Phase 8: basic PDF GPU renderer

### Initial supported subset

- Solid path fills
- Rectangular and path clipping
- Affine transforms
- Basic alpha
- Images
- CPU-generated glyph masks
- Normal blend mode
- Opaque page background

### Recommended initial design

Use CPU tessellation and conventional WGPU render pipelines:

```text
Compiled path
    ↓
CPU tessellation
    ↓
GPU vertex/index buffers
    ↓
Render pass
    ↓
Page or tile texture
```

Text should initially use:

```text
Font parser / rasterizer on CPU
    ↓
Glyph mask atlas upload
    ↓
GPU glyph quads
```

This keeps font compatibility work in the established CPU font layer and limits the first GPU task to compositing already-generated glyph masks.

### No GPU-specific parser changes

`CompiledPage` remains unchanged. The WGPU backend produces:

```rust
pub struct GpuPreparedPage {
    operation_batches: Vec<GpuOperationBatch>,
    vertex_data: UploadData,
    index_data: UploadData,
    resource_uploads: Vec<ResourceUpload>,
    tile_plan: GpuTilePlan,
}
```

### Page-level fallback

The GPU backend reports support only for pages entirely within its implemented subset.

### Exit gate

The WGPU backend renders basic pages and returns ordinary host pixel buffers through the same public API as the CPU backend.

---

## Phase 9: differential backend conformance

For every GPU-supported feature:

1. Render through CPU.
2. Render through WGPU.
3. Compare:
   - Dimensions
   - Alpha behavior
   - Background
   - Edge coverage
   - Color
   - Clip boundaries
   - Image sampling
   - Glyph positioning
4. Test on:
   - Vulkan
   - D3D12
   - Metal
   - At least one integrated GPU
   - At least one discrete GPU
   - Software or fallback adapters where practical

### Tolerance classes

Use different thresholds for:

- Flat-color interiors
- Antialiased edges
- Text edges
- Image interpolation
- Transparency
- Final monochrome result

For the e-ink pipeline, compare not only RGBA pixels but the final binarized page. Small antialiasing differences may be irrelevant if they produce identical or near-identical 1-bit output.

### Exit gate

GPU-supported pages remain within documented tolerances across supported platforms and adapters.

---

## Phase 10: clipping, transparency, and masks

Implement in increasing complexity:

1. Nested hard clips
2. Alpha masks
3. Isolated transparency groups
4. Soft masks
5. Standard separable blend modes
6. Knockout groups
7. Nonseparable blend modes
8. Luminosity masks

Use offscreen textures acquired from a bounded transient surface pool.

```rust
struct GpuSurfacePool {
    available: HashMap<SurfaceClass, Vec<TextureLease>>,
    bytes_in_use: AtomicUsize,
    byte_limit: usize,
}
```

A texture lease must remain alive until submitted commands using it have completed.

### Group bounds

Calculate conservative device-space group bounds before allocation.

Avoid allocating a full-page intermediate for a group that affects only a small area.

### Exit gate

Transparency-heavy pages either:

- Render correctly on the GPU within the VRAM budget, or
- Are rejected by preflight and sent to the CPU backend before GPU work begins.

---

## Phase 11: GPU tiling and multi-page residency

Do not model the GPU as six independent render workers.

Use:

```text
Multiple CPU page compilers
             ↓
One WGPU scheduler
             ↓
Bounded page/tile slots
             ↓
One device and associated queue
```

A WGPU queue executes submitted command buffers, and command buffers are one-use submission objects. 

### GPU job state

```rust
enum GpuJobState {
    WaitingForResources,
    Preparing,
    ReadyToSubmit,
    Submitted(SubmissionId),
    ReadbackPending,
    Complete,
    Failed,
}
```

### Tile plan

```rust
pub struct GpuTilePlan {
    pub tile_size: DeviceSize,
    pub tiles: Vec<GpuTile>,
    pub border: u32,
}
```

Each tile contains an operation list preserving original painter order.

```rust
pub struct GpuTile {
    pub output_rect: DeviceRect,
    pub padded_rect: DeviceRect,
    pub operation_indices: Range<u32>,
}
```

### Bounded in-flight work

Control separately:

- Compiled pages waiting for GPU
- Resource uploads
- Resident images
- Glyph atlases
- Tile surfaces
- Transparency surfaces
- Readback buffers
- Completed host pages

### Exit gate

Several pages can be concurrently:

- Waiting on resource upload
- Being rendered
- Waiting on GPU completion
- Undergoing readback
- Entering downstream CPU processing

No stage performs a synchronous wait after every page.

---

# 10. Drop-in integration stages

## Stage A: strict host-output compatibility

Both backends return:

```rust
HostPage
```

GPU execution is invisible to the caller.

This is the required first integration stage.

Advantages:

- Existing CPU postprocessing is unchanged.
- Backend comparisons are straightforward.
- GPU bugs cannot contaminate later GPU-only stages.
- Backend selection can be switched at runtime.

Disadvantage:

- Every GPU-rendered page requires readback.

GPU buffers must not be mapped while the GPU still uses them; readback therefore needs completion-aware asynchronous staging rather than immediate mapping. 

## Stage B: optional resident surface

Add an optional advanced path:

```rust
RenderRequest {
    output_residency: OutputResidency::BackendPreferred,
    ..
}
```

GPU returns an opaque resident surface. CPU returns a host-backed resident surface.

The normal `HostRequired` API remains unchanged.

## Stage C: backend-neutral postprocessing graph

Define operations such as:

```rust
pub enum PostprocessOp {
    Resize(ResizeSpec),
    ConvertToGray(GraySpec),
    ApplyToneCurve(ToneCurve),
    Otsu(OtsuSpec),
    Sauvola(SauvolaSpec),
    FuseThresholds(FusionSpec),
    Dither(DitherSpec),
    PackMonochrome,
}
```

Both CPU and GPU implementations consume the same operation graph.

```rust
pub trait PostprocessBackend {
    fn supports(&self, graph: &PostprocessGraph) -> bool;

    fn execute(
        &self,
        source: SurfaceHandle,
        graph: &PostprocessGraph,
    ) -> Result<SurfaceHandle, PostprocessError>;
}
```

## Stage D: minimal readback

The eventual GPU path becomes:

```text
PDF display list
    ↓
GPU rasterization
    ↓
GPU grayscale
    ↓
GPU resize
    ↓
GPU binarization
    ↓
GPU 1-bit packing
    ↓
small host readback
    ↓
CCITT4/JBIG2/DJVU encoding
```

The CPU path remains:

```text
PDF display list
    ↓
CPU rasterization
    ↓
CPU grayscale
    ↓
CPU resize
    ↓
CPU binarization
    ↓
CPU packing
    ↓
encoding
```

Both paths use the same application-level pipeline description.

---

# 11. Runtime backend policy

```rust
pub enum BackendPreference {
    Cpu,
    Gpu,
    Auto,
}
```

`Auto` should consider:

- GPU availability
- Adapter type
- Supported output format
- Page feature set
- Page dimensions
- Page complexity
- Available VRAM budget
- Current GPU queue depth
- Whether another workload is using the GPU
- Expected readback cost
- Known driver or backend exclusions

Example policy:

```text
Simple tiny page                → CPU
Large image-heavy page          → GPU
Unsupported blend mode          → CPU
GPU queue saturated             → CPU or wait, according to policy
GPU device lost                 → CPU
Resident postprocessing enabled → Prefer GPU
Host output immediately needed  → Compare estimated CPU and GPU cost
```

Initially, keep `Auto` conservative:

```text
GPU only when support is complete and page size exceeds a threshold.
Otherwise CPU.
```

---

# 12. Resource caching strategy

## 12.1 Canonical resource cache

Stores immutable decoded semantics:

- Parsed font programs
- CMaps
- Image metadata
- Decompressed stream bytes
- ICC profiles
- Paths and shadings

## 12.2 CPU resource cache

Stores:

- CPU image rasters
- Glyph masks
- Tessellated paths where reusable
- Color-conversion tables

## 12.3 GPU resource cache

Stores:

- Image textures
- Glyph atlases
- Geometry buffers
- Gradient lookup textures
- Color-conversion lookup textures
- Pipeline variants

## 12.4 Cache-state machine

```rust
enum ResourceState<T> {
    Missing,
    Preparing,
    Resident(Arc<T>),
    Failed(Arc<ResourceError>),
}
```

GPU resources additionally need:

- Submission-lifetime tracking
- Eviction eligibility
- Device-generation identity
- Device-loss invalidation

A GPU cache entry from a lost device must never be reused after device recreation.

## 12.5 No cross-backend cache dependency

The CPU backend must continue functioning when:

- The WGPU feature is absent.
- GPU initialization fails.
- The GPU device is lost.
- The GPU cache is exhausted.
- A page exceeds GPU limits.

---

# 13. Error and fallback model

Separate failures into:

```rust
pub enum RenderError {
    Document(DocumentError),
    PageCompilation(PageCompilationError),
    UnsupportedFeature(UnsupportedFeature),
    ResourceLimit(ResourceLimitError),
    CpuBackend(CpuRenderError),
    GpuUnavailable(GpuUnavailableError),
    GpuDeviceLost,
    GpuOutOfMemory,
    GpuValidation(GpuValidationError),
    Readback(ReadbackError),
    Cancelled,
}
```

Automatic fallback is appropriate for:

- GPU unavailable
- Unsupported GPU page feature
- GPU device lost before rendering
- GPU surface-size limit
- Recoverable GPU allocation failure

Automatic fallback should be configurable for:

- GPU validation failure
- GPU shader failure
- Output mismatch detected in validation mode

Do not silently fall back after a GPU backend produces potentially incorrect output. Failures after rendering starts should be reported and the page rerun on CPU only when the failure is explicit and recoverable.

---

# 14. Testing requirements

## 14.1 Parser tests

- Unit tests for tokens and objects
- Incremental update tests
- Object stream tests
- Xref recovery tests
- Cyclic reference tests
- Encryption tests
- Fuzzing
- Allocation-limit tests

## 14.2 Page compiler tests

- Graphics-state transition tests
- Clip-stack tests
- Resource inheritance tests
- Matrix tests
- Text positioning tests
- Recursion tests
- Stable IR snapshot tests

## 14.3 CPU renderer tests

- Per-operation pixel tests
- Tile-boundary tests
- Transparency tests
- Color tests
- Font tests
- Determinism tests
- Worker-count equivalence

## 14.4 GPU renderer tests

- Shader unit fixtures
- Buffer-layout validation
- Tile-bin validation
- Cross-adapter output tests
- Device-loss tests
- VRAM exhaustion tests
- Readback-stride tests
- CPU/GPU differential tests

## 14.5 Concurrency tests

Repeatedly render:

- The same page on multiple threads
- Different pages sharing the same fonts
- Different pages sharing the same object stream
- Different pages sharing the same image
- Pages with recursive forms
- Pages cancelled during compilation
- Pages cancelled during GPU submission
- Pages completing out of order

Verify:

- No deadlock
- No cache corruption
- No output nondeterminism
- No use-after-free
- Bounded memory
- Correct page ordering

## 14.6 Validation mode

Provide a development mode:

```rust
BackendPreference::GpuValidated {
    compare_against_cpu: true,
    mismatch_action: MismatchAction::SaveArtifacts,
}
```

On mismatch, save:

- CPU output
- GPU output
- Difference image
- Compiled-page dump
- Backend capabilities
- Adapter and driver metadata
- Shader configuration
- Render request
- Resource identifiers

---

# 15. Performance instrumentation

Measure each layer independently.

## Document and compilation

- Source read bytes
- Objects parsed
- Streams decoded
- Page compile time
- Resource resolution time
- Display operation count
- Compiled-page bytes

## CPU renderer

- Preparation time
- Tessellation time
- Tile binning
- Raster time
- Blend time
- Peak worker memory
- Cache hit rate

## GPU renderer

- CPU lowering time
- Upload bytes
- Upload time
- Command-recording time
- Queue wait
- GPU execution time where timestamp queries are available
- Readback bytes
- Readback latency
- Resident texture bytes
- Transient surface peak
- Cache hit rate
- Number of submissions
- Number of render passes
- Number of compute passes

## End-to-end

- Pages per second
- CPU utilization
- GPU utilization
- Peak system RAM
- Peak VRAM
- Downstream encoder utilization
- Time with CPU workers idle
- Time with GPU idle
- Readback contribution

Do not judge the WGPU backend solely by raster time. Its intended value includes freeing CPU capacity and avoiding CPU-resident intermediate pages.

---

# 16. Recommended implementation order

The practical order should be:

1. Corpus, reference renderer, comparison tooling.
2. Immutable source and document structure.
3. Concurrent object and page resolution.
4. Semantic content interpreter.
5. Backend-neutral compiled-page IR.
6. Basic CPU renderer.
7. CPU page concurrency and bounded scheduling.
8. CPU rendering fidelity.
9. Freeze rendering contracts and canonical surface semantics.
10. Add backend capability and page preflight APIs.
11. Build isolated WGPU lifecycle and readback backend.
12. Add basic paths, images, and glyph atlases to WGPU.
13. Introduce page-level automatic fallback.
14. Run continuous CPU/GPU differential testing.
15. Add clipping and ordinary transparency.
16. Add GPU tiling and bounded multi-page residency.
17. Add difficult transparency, masks, patterns, and shadings.
18. Introduce opaque resident surfaces.
19. Port postprocessing operations to the shared backend-neutral graph.
20. Read back only final grayscale or packed monochrome output.
21. Optimize GPU resource sharing and batching only after profiling.

---

# 17. Definition of completion

The design goal is reached when all of the following are true:

1. One document snapshot can feed multiple concurrent page compilers.
2. The CPU backend can render multiple pages simultaneously without a document-wide lock.
3. The parser, page compiler, and application do not import WGPU.
4. CPU and WGPU consume the same `CompiledPage`.
5. The application can select CPU, GPU, or automatic mode without changing its processing code.
6. Unsupported GPU pages are identified before submission and routed to CPU.
7. GPU-rendered pages can initially return the same host bitmap type as CPU pages.
8. CPU/GPU differential tests run automatically.
9. The CPU renderer remains available as a permanent fallback.
10. Multiple GPU page or tile jobs can remain in flight under a bounded VRAM budget.
11. GPU-resident postprocessing is optional and does not break the original host-output API.
12. Device loss or GPU initialization failure does not prevent CPU rendering.
13. The final pipeline can keep rasterization, resizing, grayscale conversion, binarization, and packing in GPU memory when supported.
14. The scheduler measurably frees CPU time for PDF compilation, OCR, encoding, or other pipeline stages.

The most important implementation boundary is:

```text
CompiledPage is the rendering contract.

CPU and WGPU are consumers of that contract, not participants in PDF parsing.
```

That boundary permits the entire CPU renderer and concurrent document engine to be completed and deployed before the first WGPU shader is written.
:::

A repository-specific version would replace the proposed crates and interfaces with the project’s actual module boundaries while retaining the same phase gates and dependency direction.