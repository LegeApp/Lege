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
   zero alpha). Visible text, text clipping, paths, shadings, groups, clips,
   and soft masks make preflight decline the page to CPU.
4. The native prepared image shape is:
   - display-ready RGB8 (decoded RGB8 is zero-copy; Gray, Indexed, CMYK,
     `/Decode`, and other CPU-supported spaces are converted once and cached);
   - Normal blend, opaque image/page;
   - no `/SMask`, hard `/Mask`, stencil, or non-rectangular clip;
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
    fallback telemetry and a return to GPU on the next page. This exercises
    the policy boundary without destroying Lege's process-wide shared device;
    actual driver device-loss/recreation remains a platform stress item.
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
LEGE_PDF_IMAGE_RENDERER=gpu cargo run --release -p lege-viewer \
  --example pdf_tile_profile -- FILE PAGE ZOOM_BUCKET WARM_PASSES TILE_COUNT
```

On the same book/page and RTX 4060, 12 visible 256×256 tiles at zoom bucket 0
(scale 1) took **92.734 ms CPU vs 71.636 ms cold GPU (1.29×)** and
**50.725 ms vs 21.261 ms warm (2.39×)**. All 96 measured requests routed to
GPU with no fallback, and CPU/GPU aggregate pixel checksums were identical.
At bucket 2 (scale 2), CPU was 90.152/51.243 ms cold/warm and GPU was
71.063/20.609 ms (1.27×/2.49×); resampling output was not byte-identical at
that zoom, which is not a parity gate.

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

## Remaining work before promotion

1. Validate actual driver device loss/recreation, more Linux Vulkan adapters,
   then Windows DX12. Injected failure/fallback and after-submit cancellation
   are covered.
2. Expand masks/clips and mixed content in the measured order below.
   Gray/Indexed/CMYK preparation is complete; the next coverage gates are
   mixed visible text/vector content, clips, and soft-mask state rather than
   another unknown-gap sweep.
3. Measure first-final-tile latency under real interactive scheduling on more
   adapters. The current lazy path keeps document open and text-first tiles
   responsive, but a later idle prewarm may improve time to the first final
   scan tile without returning work to the UI thread.
4. Before `Auto` becomes the default, add a footprint-aware bilevel routing
   threshold. The same CCITT fixture loses at scale 1 and wins at scale 4, so
   a blanket format exclusion would discard a real high-zoom gain.

## Expansion order

After the image-only slice wins its benchmark:

1. resource soft masks and color-key masks;
2. rectangular and then analytic clip masks;
3. image alpha and non-Normal blends;
4. ~~Gray/Indexed/CMYK prepared uploads~~ — complete 2026-07-26, including
   `/Decode`, packed low-bpc samples, caching, cancellation, and a 64 MiB
   conversion guard;
5. mixed CPU/GPU page content only after the WGPU backend can paint the
   intervening vector operations without readback boundaries;
6. direct resident handoff to GPU postprocess and the viewer presenter.

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
  cancellation, device loss, and whole-request CPU fallback.
- Validate Linux Vulkan first, then Windows DX12 before automatic selection.

## Integration policy

The backend is opt-in and experimental. Both the production full-page
`pdfr render` command and the viewer tile engine select it through
`LEGE_PDF_IMAGE_RENDERER`; `Auto` uses the same discrete/integrated hardware
test as `lege-gpu`, while an unset variable still selects CPU. Once real-page
coverage, stability, and non-blocking initialization gates pass, the default
can switch to `Auto` without changing either render contract. All ineligible
pages continue to use CPU. Broader WGPU rendering grows by truthful capability
expansion, never by silently approximating an unsupported page.
