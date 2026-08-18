# GPU rendering of decoded PDF images

Status: **experimental renderer wired into `pdfr render` and viewer tiles; CPU-default**
Opened: 2026-07-26

## Why this comes before postprocess promotion

Recent renderer profiles identify `render.image`—sampling and compositing
already-decoded JPEG image data—as a large upstream hotspot. GPU postprocess is
now experimentally fast, but it runs after this cost and cannot remove it.
The former `pdf-render-wgpu` stub now implements this narrow slice; decoded
image painting remains the smallest renderer class likely to produce a
material gain without first porting the complete vector compositor.

This must be a page-level resident renderer, not a helper called from the CPU
pixel loop. Uploading an image, reading the page back after every image, and
resuming CPU painter order would spend the gain on PCIe transfers. The initial
backend therefore owns the destination surface for the whole eligible page,
uploads each decoded image once, submits all image draws in painter order, and
performs one final readback.

## Implemented first slice (2026-07-26)

1. Reuses `lege-gpu::compute::SharedGpuContext`; it never creates a second
   device and exposes the same adapter detection/metadata.
2. Extracts the backend-neutral part of CPU image preparation so both renderers
   receive decoded samples, device bounds/inverse transform, footprint,
   and interpolation through `PreparedRgbImagePage`, without exposing CPU
   executor operations or raster internals.
3. Routes image-only pages, including the common searchable-scan shape where
   the image is accompanied only by provably non-painting OCR text (`Tr 3` or
   zero alpha). Visible text, text clipping, visible page paths, shadings, and
   transparency groups make preflight decline the page to CPU.
4. The native prepared image shape is:
   - display-ready RGB8 (decoded RGB8 is zero-copy; Gray, Indexed, CMYK,
     `/Decode`, and other CPU-supported spaces are converted once and cached);
   - all 16 PDF image blend modes with opaque or constant-alpha image draws;
   - image-attached `/SMask`, JPX `/SMaskInData`, explicit colour-key/separate
     stencil `/Mask`, and solid-colour `/ImageMask` stencils are supported
     through an independent opacity plane;
   - patterned `/ImageMask` brushes cross as a CPU-prepared bounded RGB+alpha
     plane; glyph-outline text clips remain unsupported, while rectangular
     and analytic path clips are supported;
   - nearest, bilinear, and minification box sampling;
   - axis-aligned and general affine placement.
5. Keeps a resident premultiplied RGBA8 page surface, preserves display-list
   order, and read back once for the existing `HostPage` contract.
6. `ExperimentalImageRenderer` provides `Cpu` / forced `Gpu` /
   hardware-only `Auto` selection, whole-request CPU fallback, and routing
   telemetry. `LEGE_PDF_IMAGE_RENDERER=cpu|gpu|auto` is CPU-default.
7. Initialization, preparation, validation, shader, polling, and readback
   failures are typed and isolated. Cancellation does not rerun on CPU.
8. Retains decoded-image device buffers in a thread-safe 128 MiB LRU. Identity
   is the immutable decoded `Arc<[u8]>` allocation plus dimensions: the CPU
   decoded-image cache returns the same allocation on a resource revisit,
   changed content naturally receives a new identity, and retaining the Arc
   prevents pointer reuse. Per-render and lifetime telemetry report hits,
   misses, uploads, reuse, evictions, entries, and resident bytes.
9. Exposes a structured, stable GPU-eligibility report. Production preflight
   delegates to the same classifier used by the corpus tool, so measurement
   and routing policy cannot drift. `eligibility-census` accepts either a
   sweep `results.csv` or a direct PDF, compiles a deterministic page sample,
   and decode-confirms every statically eligible page without initializing
   WGPU.
10. Checks cancellation before allocation, between image draws, before
    submission, and while polling an already-submitted readback at 1 ms
    granularity. Cancellation never retries on CPU. A deterministic
    after-submit test verifies `Cancelled` and successful reuse of the same
    device by the next render.
11. A one-shot injected device-loss-class failure verifies whole-request CPU
    fallback telemetry, backend/pipeline invalidation, and lazy recreation on
    the next page. The shared device now installs wgpu's driver-loss callback,
    records its reason, and lives in a replaceable process-wide slot; a lost
    device is therefore replaced rather than being trapped forever in a
    `OnceLock`. Actual driver-induced loss remains a platform stress item.
12. The production full-page `pdfr render` command now constructs
    `ExperimentalImageRenderer`, so `LEGE_PDF_IMAGE_RENDERER=cpu|gpu|auto`
    controls a real caller. The command reports the selected CPU/GPU route and
    GPU image/upload/readback counters. CPU remains the unset default, and
    unsupported pages or recoverable GPU failures retain whole-request CPU
    fallback.
13. The production viewer raster worker uses the same policy renderer while
    preserving its caller-owned `CpuWorkerContext`. Eligible tiles share the
    renderer's decoded upload cache across raster workers; default,
    ineligible, and fallback tiles retain the existing worker-local CPU font,
    coverage, and raster scratch. `pdf_tile_profile` measures the exact
    production worker path and exposes aggregate routing telemetry.
14. Viewer initialization is lazy and process-shared per document. `open`
    validates only the environment policy; the first final raster job
    initializes the renderer inside the conductor's background raster pool
    through a shared `OnceLock`. Text-first structural tiles do not touch the
    cell, so they can appear while WGPU initializes. All raster workers then
    share the resulting renderer, device, and upload cache.
15. Non-RGB8 preparation reuses the CPU renderer's exact source-pixel color
    semantics and stores converted RGB in a document-scoped 64 MiB LRU. The
    cache holds a weak source identity plus the full color/decode descriptor,
    preventing stale pointer aliasing without pinning a second decoded copy.
    Conversions larger than 64 MiB decline before allocation and remain on the
    CPU's destination-driven sampler.
16. `Auto` now performs the static check and CPU-side prepared-page
    classification before initializing WGPU. A CPU-routed page never enumerates
    adapters; an eligible page hands the already-prepared allocation to the
    newly created backend rather than preparing twice. Telemetry separately
    reports GPU initializations and recoveries.
17. Image-attached soft masks and solid-colour stencil brushes use a normalized
    alpha8 plane prepared and cached by the CPU semantics layer. WGPU samples
    mask geometry independently from RGB, box-filters minified masks, and
    composites masked pixels source-over into the resident premultiplied page.
    RGB and opacity uploads share the bounded device-buffer cache. Opaque
    images retain their overwrite fast path.
18. Explicit colour-key and separate stencil `/Mask` values reuse the opacity
    plane without expanding the GPU shader vocabulary. Colour keys are tested
    against raw components before `/Decode`; separate stencil streams preserve
    hard-mask polarity after codec decoding. Both stay binary under
    minification rather than taking the coverage box filter used by soft masks
    and image stencils.
19. All 16 PDF image blend modes cross the prepared seam and execute in WGSL.
    Separable and non-separable formulas match the CPU compositor, including
    premultiplied source-over with image alpha and every independent mask/clip
    coverage. A real RTX regression checks every output byte within one level
    of CPU for a two-image overlap fixture.
20. GPU routing is transactional over the immutable request. No GPU result is
    exposed before a complete validated readback; typed failures and panics
    abandon it and repaint the original request from the beginning on CPU.
    `Auto` quarantines its GPU after a panic so a deterministic backend bug
    cannot catch every later page in the same loop. Telemetry separates GPU
    panics and CPU failures, and a real-hardware fault test proves the fallback
    page byte-identical to direct CPU output and later pages CPU-routed. An
    eight-way parallel fault test proves every in-flight page still finishes
    across the quarantine boundary.
21. Patterned `/ImageMask` brushes use the normative CPU tiling executor to
    produce one bounded straight-RGB plus alpha plane, then reuse the existing
    GPU image compositor for painter order, constant alpha, every blend mode,
    and active page soft masks. Pattern cells may therefore retain arbitrary
    CPU-covered paths, text, images, groups, and nested patterns without
    broadening WGPU into mixed-content rendering. The planes live in a 64 MiB
    request-shape LRU and retain stable Arc identity for GPU upload reuse.
    `Auto` declines a pattern-only page because CPU has already done its only
    paint; it uses the bridge when another native image draw can amortize the
    transfer/readback. Degraded nested cell draws decline to full CPU so their
    diagnostics remain visible.

The focused synthetic scan benchmark is
`cargo run --release -p pdf-render-wgpu --example image-profile`. On an RTX
4060 Laptop GPU via Vulkan, decoded RGB8 2400×3200 → RGBA8 1200×1600 measured
after the upload cache landed:

- CPU total median: 30.386 ms;
- GPU cold total: 9.277 ms, including a 21.97 MiB upload;
- GPU preparation median: 0.002 ms;
- GPU warm paint/readback median: 1.984 ms;
- GPU warm total median: 1.986 ms (15.30×);
- CPU/GPU RGB mean absolute difference 0.014, maximum 1, across 1.35% of
  channels. Exact parity is not a promotion gate, but this confirms the
  fractional-overlap minifier is also quantitatively close.

The real-PDF harness is
`cargo run --release -p pdf-render-wgpu --example dct-profile -- FILE PAGE SCALE RUNS`.
Two sweep-15 DCT scan fixtures on the same adapter measured:

- `Goebbels Diaries.pdf` p97, 503×801 DCT → 1006×1602: cold
  **109.921 ms CPU vs 20.504 ms GPU (5.36×)**; warm
  **100.489 ms vs 1.462 ms (68.73×)**; maximum RGB difference one.
- `globalhistoryofm0000unse_1.pdf` p134, 25.72 MiB decoded upload →
  2578×3487: cold **674.480 ms CPU vs 177.728 ms GPU (3.79×)**; warm
  **589.873 ms vs 16.973 ms (34.75×)**; GPU and CPU output were byte-exact.

Both PDFs contain hundreds to thousands of invisible OCR operations. The
focused run therefore also validated that accepting non-painting searchable
text materially expands scan coverage without approximating visible content.

The eligibility harness is:

```sh
cargo run --release -p pdf-render-wgpu --example eligibility-census -- \
  RESULTS_CSV_OR_PDF SAMPLE_PAGES SCALE OUT_CSV
```

A deterministic 240-page sample from sweep 15 found 136 pages with at least
one image draw. After prepared color-space expansion, 47/240 pages overall,
or **47/136 image-bearing pages (34.6%)**, were statically and
decode-confirmed eligible—up from 23/136 (16.9%)—with no static/preparation
drift. Blocker counts overlap because one page can exercise several features:
visible text 167, paths 111, clips 93, soft-mask state 34, and 15 images whose
full RGB conversion would exceed the explicit 64 MiB guard. The sample also
contained 58 pages with 23,461 provably non-painting text draws.

The original pure-scan test book,
`buddhasahibsmenw0000alle_1.pdf`, is the complementary coverage fixture:
**all 356/356 pages** were statically and decode-confirmed eligible at scale
2, while safely ignoring 135,094 invisible OCR glyph draws. On page 180 at
scale 1 (1673×2734, 13.09 MiB decoded RGB upload), the RTX 4060/Vulkan run
measured **344.469 ms CPU vs 47.765 ms cold GPU (7.21×)** and
**293.167 ms vs 4.222 ms warm GPU (69.43×)**. Output was byte-exact.

The production `pdfr render` integration was also exercised on page 180 at its
fixed 150-DPI output (3486×5696). Including document open, JPEG decode, WGPU
startup, rendering, and PPM output, the CPU route took **1.53 s** and the cold
GPU route took **0.53 s (2.9× end-to-end)**. The GPU selected the RTX 4060
through the shared detector and reported one 13.09 MiB upload plus a 75.75 MiB
readback. CPU/GPU output differed only at resampling level: normalized MAE
0.001363, RMSE 0.002643, and PSNR 51.56 dB.

The viewer-specific harness is:

```sh
LEGE_PDF_IMAGE_RENDERER=gpu cargo run --release -p lege-gui \
  --example pdf_tile_profile -- FILE PAGE ZOOM_BUCKET WARM_PASSES TILE_COUNT
```

On the same book/page and RTX 4060, 12 visible 256×256 tiles at zoom bucket 0
(scale 1) took **92.734 ms CPU vs 71.636 ms cold GPU (1.29×)** and
**50.725 ms vs 21.261 ms warm (2.39×)**. All 96 measured requests routed to
GPU with no fallback, and CPU/GPU aggregate pixel checksums were identical.
At bucket 2 (scale 2), CPU was 90.152/51.243 ms cold/warm and GPU was
71.063/20.609 ms (1.27×/2.49×); resampling output was not byte-identical at
that zoom, which is not a parity gate.

Parallel page scheduling has also been validated through the production
viewer worker contract:

```sh
LEGE_PDF_IMAGE_RENDERER=auto cargo run --release -p lege-gui \
  --example pdf_parallel_profile -- \
  FILE FIRST_PAGE PAGE_COUNT ZOOM_BUCKET TILES_PER_PAGE THREADS PASSES
```

`TILES_PER_PAGE=0` requests one whole-page raster per page. On pages 180–187
of the pure JPEG scan book at scale 1, warm GPU time fell from **354.022 ms
with one worker to 76.023 ms with eight workers (4.66×)**. Eight CPU workers
took **461.894 ms**, making the parallel GPU path **6.08× faster**. The workers
share a device and hardware submission queue, but each owns an independent
page buffer, command encoder, readback, and cancellation-aware mapping wait;
there is no page-wide execution mutex or sequential software dispatcher.
All 40 GPU requests completed without fallback and every pass had the same
CPU/GPU checksum.

The same measurement exposes a routing boundary rather than a concurrency
failure: for 32 visible 256×256 tiles, eight GPU workers took **132.161 ms**
and eight CPU workers **130.303 ms**. The GPU still scaled 1.71× against its
one-worker result, but transfer/setup overhead erased the device advantage.
Keep `Auto` conservative for small tile sets until batching or a resident
presenter handoff changes that cost model; whole-page parallel rendering is
already decisively viable.

Those cold GPU figures excluded adapter discovery because the first
implementation initialized during `PdfEngine::open` (159–226 ms). After
moving initialization into the background raster pool, open measured
**6.359 ms** and the first 12-tile set, now including the one-time adapter
cost, measured **220.661 ms**; warm rendering remained **20.459 ms**.

Prepared-space corpus checks add two useful performance boundaries:

- a non-RGB DCT page (`Stari srpski zapisi i natpisi.pdf` p48) took
  **82.025 ms CPU vs 17.841 ms GPU warm** for 12 scale-1 viewer tiles
  (4.60× GPU gain);
- a packed 1-bit CCITT page (`Byzantium and the Slavs.pdf` p0) took
  **14.454 ms CPU vs 19.341 ms GPU warm** at scale 1, but
  **31.810 ms CPU vs 17.310 ms GPU** at scale 4. The CPU popcount/box-filter
  path wins when heavily minified; the GPU wins as the destination approaches
  source resolution. A follow-up scale-2 viewer measurement remained
  CPU-favorable at **16.697 ms vs 20.162 ms GPU warm**. `Auto` therefore keeps
  1-bit images on CPU whenever either source footprint axis exceeds one texel
  per destination pixel, declining before RGB expansion/upload. At or above
  source resolution it retains GPU routing. Forced `Gpu` deliberately ignores
  this performance policy and remains useful for experimentation.

The prepare-first startup path was then measured through the same production
viewer harness. On the CCITT fixture at scale 2, `Auto` performed **zero GPU
initializations** and the cold 12-tile set fell from **156.868 ms to
29.501 ms** (26.752 ms warm). At scale 4 it initialized exactly once, routed
all 48 requests to GPU, and measured 285.207 ms cold including discovery and
17.095 ms warm. The same eligible case also passed on the integrated Intel
Iris Xe Vulkan adapter: one initialization, no fallback, 302.060 ms cold and
19.529 ms warm. NVIDIA and Intel produced the same aggregate checksum.

## Remaining work before promotion

1. Validate actual driver-induced device loss/recreation and Windows DX12.
   Injected failure/fallback, shared-context replacement, after-submit
   cancellation, and both NVIDIA discrete and Intel integrated Linux Vulkan
   adapters are covered. Follow
   [`WINDOWS-DX12-GPU-VALIDATION.md`](../handoffs/WINDOWS-DX12-GPU-VALIDATION.md).
2. Expand masks/clips and mixed content in the measured order below.
   Gray/Indexed/CMYK preparation, image `/SMask`, explicit hard `/Mask`, and
   solid and patterned stencil brushes, image path clips, page-level soft-mask
   state, constant image alpha, and all image blend modes are complete. The
   Forced GPU now also paints ordered mixed images, solid paths/strokes,
   visible solid text, and CPU-derived exact text-clip planes. This is not yet
   an `Auto` coverage expansion: GPU-native clip-outline generation and a
   batched path raster architecture remain performance work.
3. Measure first-final-tile latency under real interactive scheduling on more
   documents and refine the small-tile `Auto` threshold. Parallel whole-page
   throughput is no longer an open risk. The prepare-first path avoids WGPU
   entirely for CPU-routed pages; a later idle prewarm may improve time to the
   first eligible final scan tile without returning work to the UI thread.

## Expansion order

After the image-only slice wins its benchmark:

1. ~~resource soft masks~~ — complete 2026-07-26, including packed/codec
   masks, `/Decode`, independent minification footprints, cached alpha8
   preparation/upload, and source-over composition;
2. ~~color-key and separate hard stencil `/Mask`~~ — complete 2026-07-26,
   including raw-component matching, codec-decoded stencil polarity, cached
   alpha8 preparation/upload, and binary minification sampling;
3. ~~solid-colour `/ImageMask` stencil brushes~~ — complete 2026-07-26;
4. ~~patterned stencil brushes~~ — complete 2026-07-27 through a bounded,
   cached CPU pattern-plane bridge; pattern-only pages remain CPU-routed in
   `Auto`, while image pages can preserve GPU residency;
5. ~~rectangular and analytic path clip masks~~ — rectangular bounds lowering
   completed 2026-07-26; arbitrary paths completed 2026-07-27 by exporting
   the CPU rasterizer's exact nested anti-aliased device mask and multiplying
   it with image opacity in WGSL. Text clips now use the same exact prepared
   alpha plane in forced mixed mode; GPU-native clip-outline coverage remains
   deferred as an optimization;
6. ~~constant image alpha and all PDF image blend modes~~ — complete
   2026-07-27, including multiplication with image opacity, analytic clips,
   and page-level soft masks plus separable/non-separable blend composition;
7. ~~Gray/Indexed/CMYK prepared uploads~~ — complete 2026-07-26, including
   `/Decode`, packed low-bpc samples, caching, cancellation, and a 64 MiB
   conversion guard;
8. mixed CPU/GPU page content — forced-mode semantic prototype completed
   2026-07-27 without readback boundaries; keep `Auto` closed until a batched
   coverage implementation beats CPU by at least 1.2× on the selected mixed
   corpus;
9. direct resident handoff to GPU postprocess and the viewer presenter.

The 240-page sweep-15 sample moved from **47 to 50 decode-confirmed eligible
pages**, or **36.8% of the 136 image-bearing pages**, with static/prepared
counts still exact. The three newly admitted pages include a solid CCITT
stencil and two JPX+JBIG2 MRC soft-mask pages. On RTX 4060/Vulkan,
`Appian Roman History.pdf` p196 at viewer scale 2 measured **129.424 ms CPU
vs 16.446 ms GPU warm (7.87×)**. The solid-stencil fixture at scale 4 measured
**65.613 ms CPU vs 20.128 ms GPU warm (3.26×)**. Its result also corrected the
automatic-routing policy: stencil minification no longer inherits the
ordinary bilevel CPU preference because stencils do not use that CPU popcount
fast path.

The same sample then moved from **50 to 51 decode-confirmed eligible pages**
(**37.5%** of image-bearing pages) after explicit hard `/Mask` support, again
with no static/prepared drift. `Argentine Democracy.pdf` p108 at viewer scale
2 measured **220.742 ms CPU vs 22.511 ms GPU warm (9.81×)** with all 60
requests GPU-routed and no fallback. The sample's other hard-mask page remains
ineligible for its independent visible-text and clip requirements.

Rectangular clip admission moved the sample from **51 to 60
decode-confirmed eligible pages**, or **44.1% of the 136 image-bearing
pages**, with static and prepared counts still identical. Clip blockers fell
from 93 to 19; the remaining cases are analytic/text clips or overlap other
unsupported content. Nine real pages became fully eligible.
`The Image of Edessa.pdf` p86 automatically routed all seven whole-page
requests to GPU without fallback and measured **63.777 ms CPU vs 1.746 ms GPU
warm**. A focused real-GPU test verifies the rectangular clip interior and
exterior.

Analytic path clips then moved the same sample from **60 to 63
decode-confirmed eligible pages**, or **46.3%** of image-bearing pages, with
static and prepared counts again identical. Standalone clip blockers fell
from 19 to zero; sixteen clipped pages still carry independent unsupported
content. The preparation seam exports a bounded device-space alpha8 mask from
the normative CPU rasterizer, including nested path intersections and PDF fill
rules. Images sharing one clip share its allocation/upload, and WGSL multiplies
clip coverage with any image opacity plane.

The true analytic fixture, `Byzantium, Latin Romania and the Mediterranean.pdf`
p290, measured **93.463 ms CPU vs 5.349 ms GPU warm whole-page (17.47×)** and
**93.839 ms vs 24.359 ms** for twelve viewer tiles (3.85×), with automatic
routing and no fallback. Its full 1241×1755 CPU/GPU renders had normalized
RMSE **0.000131** (about 77.6 dB PSNR); visual differences were ordinary scan
resampling rather than clip geometry.

Page-level soft-mask state is now admitted without duplicating general PDF
painting in WGSL. The CPU prepared-stream executor renders each bounded mask
group in isolation and derives its final Alpha/Luminosity plane, including
`/BC`, `/TR`, nested masks, and arbitrary mask content. The main image walker
tracks the balanced mask stack and attaches the active device-space plane to
each image. A separate WGPU binding multiplies it with image-resource opacity
and analytic clipping. Derived planes use a bounded 64 MiB document-session
cache, preserving allocation identity so page revisits hit the GPU upload
cache. An empty real mask now correctly remains zero coverage rather than
aliasing `/SMask /None`.

The sweep-15 sample moved **63 → 64** decode-confirmed eligible pages
(**47.1%** of 136 image-bearing pages), still with exact static/prepared
agreement. The census now reports active page-level masks separately and found
none among those 64 pages: the new `The Ashgate Research Companion to Imperial
Germany.pdf` p0 case contains `/SMask /None`, not a live mask. It measured
**71.641 ms CPU vs 0.993 ms GPU warm (72.15×)** at scale 2. A focused
RTX 4060/Vulkan regression therefore supplies the meaningful active-mask
validation: path-derived luminosity coverage, warm image+mask upload hits, and
nonzero `/BC` outside coverage all pass. A future broader corpus pass should
locate a real image-only active-mask visual oracle.

The suggested Braudel volume is an unusually strong image-resource mask
oracle, but not a page-level graphics-state mask oracle: all **632/632** pages
are decode-confirmed GPU-eligible, none has an active page `/SMask`, and its
MRC foregrounds instead carry image `/SMask` resources. Page 0 contains two
2222×3191 JPX images, with the second masked; page 300 similarly combines a
699×1017 JPX background with a 2099×3055 masked JPX foreground. On RTX
4060/Vulkan, page 0 measured **181.673 ms CPU warm vs 1.524 ms GPU warm
(119.20×)** and page 300 **153.356 ms vs 1.268 ms (120.97×)**. RGB differences
were small (mean absolute 0.017 and 0.034 respectively), and both revisits hit
all three cached uploads: the two image planes plus the mask.

Constant Normal-blend image alpha now crosses the prepared seam as alpha8 and
is multiplied in WGSL with image opacity, analytic clip coverage, and active
page-soft-mask coverage before source-over composition. CPU-only preparation
and RTX 4060/Vulkan shader regressions cover a 50%-alpha image combined with an
image `/SMask`. The deterministic sweep-15 sample remains **64/136 (47.1%)**
because it contains no page blocked solely by constant image alpha; this closes
a known semantic gap without overstating corpus coverage.

All 16 PDF image blend modes now remain GPU-eligible. The WGSL compositor
implements the CPU renderer's separable and non-separable formulas and keeps
the old Normal/opaque overwrite fast path. On RTX 4060/Vulkan, a focused
two-image overlap regression passed every mode with every byte within one
level of CPU. The deterministic sweep-15 census remains **64/136 (47.1%)**:
it contains no page blocked solely by image blend mode, so this is another
semantic closure rather than a sampled coverage increase. The Normal-image
hot path did not regress materially: Braudel p0 measured **182.396 ms CPU vs
1.561 ms GPU warm (116.83×)** after the expanded shader.

`Auto` now treats every GPU attempt as all-or-nothing. Preparation, backend
initialization, validation, submission, mapping, readback, and unexpected
panics can never expose a partial page; any non-cancellation failure repaints
the immutable original request through CPU. A real RTX injected-panic test
proved byte-identical CPU fallback. Auto then quarantines the panicking GPU
backend for the process and sends subsequent pages directly to CPU, while
device-loss-class failures retain the existing lazy recreation path.

Patterned `/ImageMask` brushes now remain eligible through a deliberately
narrow hybrid seam. The CPU tiling executor rasterizes the pattern cell through
the stencil into a bounded straight-RGB plus alpha plane; WGPU then applies
page ordering, constant alpha, blend modes, and any active page soft mask. A
64 MiB LRU preserves both plane allocations, so revisits hit the GPU upload
cache. An RTX 4060/Vulkan fixture combining a native RGB image with a colored
tiling stencil under Multiply at 75% alpha matched CPU within one byte and
reused all three uploads warm. `Auto` keeps a pattern-only page on CPU, because
sending a page whose sole paint was already rasterized through GPU cannot be a
win. The sweep-15 sample remains **64/136 (47.1%)** because it contains no page
blocked solely by a patterned stencil.

## Forced mixed content and text clips

The 2026-07-27 focused prototype generalizes the prepared seam to an ordered
`Image | Path` stream. Solid fill/stroke paths and real visible glyph outlines
are rasterized in WGSL and composite between image commands using the same
alpha and 16-mode blend implementation. Draft path coverage is 4×4; Normal is
8×8. Exact nested path/text clip coverage and page soft masks currently cross
as CPU-derived alpha planes. This keeps clip semantics complete while the
native path rasterizer is evaluated; it does not claim that clip geometry is
already GPU-native.

GPU path work is bounded in three layers:

- flattened device contours are simplified to 0.1-pixel tolerance;
- edges are indexed into 16-row Y bands;
- glyph outlines lower in batches of at most eight, avoiding both a
  page-wide edge search and one dispatch per glyph.

Prepared mixed pages live in a bounded 64 MiB request-shape cache. Consecutive
paths are now grouped across an 8×8 active-tile worklist; images remain hard
painter-order barriers. A batch is split at 64 paths per tile, 4,096 total
paths, or a 64 MiB packed-component bound. The bounded device cache uses stable
prepared-batch identity and owns the complete descriptor, geometry, tile-list,
and deduplicated clip/soft-mask atlas upload. Statistics expose logical paths,
actual batch dispatches, active tiles, tile references/depth, packed bytes,
upload reuse, and resident path-cache bytes. Forced mode also accepts
image-free solid path pages. Auto continues to require an image and explicitly
declines every page containing a path batch.

The correctness fixtures pass on RTX 4060/Vulkan: ordered image then path,
forced path-only paint, and a text clip constraining a native GPU path. The
full crate suite passes 30/30, including atomic CPU fallback, panic quarantine,
device-loss recovery, cancellation after submission, and concurrent jobs.

The GPU-native coverage batch is implemented. One 8×8 workgroup owns one
active tile, cooperatively calculates edge crossings for its subpixel rows,
accumulates shared coverage, and composites all tile paths in painter order
while keeping the destination pixel in a register. On
`The-Flower-of-Chinese-Buddhism.pdf` p99 at 354×592, 411 logical path draws now
become **one dispatch** over 2,143 active tiles and 4,331 ordered tile
references. RTX 4060/Vulkan warm time fell from **101.357 ms to 5.914 ms**
(about 17.1×), with RGB MAE still 0.990 and maximum error 41. The measured CPU
median was 4.784 ms, so GPU is 0.81× CPU rather than the required 1.2× win.
This clears the batching implementation target but not the Auto promotion
gate; mixed paths remain forced-GPU-only pending broader corpus evidence and
another focused optimization.

## Validation

- Use the recent JPEG/image hotspot fixtures and sweep-15 scan pages, not only
  synthetic quads.
- Compare semantic image placement, crop, orientation, edge coverage, alpha,
  and scan tone. Byte identity is useful but not a gate when the GPU result is
  visually/quantitatively as good or better.
- Record decode, upload, GPU paint, readback, and total page time separately.
- Require an end-to-end win including decode and transfers; a shader-only
  speedup does not count.
- Stress repeated pages, repeated image resources, concurrent page jobs,
  cancellation, device loss, GPU panics, and whole-request CPU fallback.
- Validate Linux Vulkan first, then Windows DX12 before automatic selection.

## Integration policy

The backend is opt-in and experimental. Both the production full-page
`pdfr render` command and the viewer tile engine select it through
`LEGE_PDF_IMAGE_RENDERER`; `Auto` uses the same discrete/integrated hardware
test as `lege-gpu`, while an unset variable still selects CPU. Once real-page
coverage, stability, and non-blocking initialization gates pass, the default
can switch to `Auto` without changing either render contract. All ineligible
pages and every recoverable GPU failure continue to use CPU from the beginning
of the request. Broader WGPU rendering grows by truthful capability expansion,
never by silently approximating an unsupported page.
