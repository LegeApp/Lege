You are **partly describing something the pipeline already does**, but the custom renderer enables a substantially deeper integration.

The current code already has one limited form of fusion: when there is no cover mutation, no detected image region, no crop normalization, no OCR consumer, and no JPEG base layer, binarization is deferred so the encoder can consume its result without storing an extra full-page binary buffer.  The callback form also lets the GPU binarizer hand mapped output directly to the encoder rather than allocating another `Vec<u8>`.

That is **binarization-to-encoding fusion**.

What is newly possible is eliminating the earlier and much larger boundary:

```text
PDF renderer
    ↓
full-page RGB
    ↓
grayscale conversion
    ↓
binarization
```

With Lege-render, the pipeline can instead choose the raster product appropriate to each page and each downstream consumer.

## The present pipeline

The effective current path is approximately:

```text
PDFium
  ↓
high-resolution RGB page
  ├── smaller inference image
  ├── crop/margin/resize operations
  ├── optional OCR RGB image
  ├── RGB image-region crops
  └── full-page binarization input
          ↓
      RGB → grayscale
          ↓
      Sauvola/Otsu/fixed threshold
          ↓
      1 byte per binary pixel
          ↓
      JBIG2/CCITT/DjVu encoding
```

When image regions exist, the code clones the full RGB page, paints detected image boxes white, and binarizes that separate RGB copy so figure content does not affect threshold statistics. 

The adaptive CPU path then performs:

```text
RGB8
  ↓
linear RGB f32
  ↓
Gray8
  ↓
Sauvola + background-normalized Otsu
  ↓
binary u8
```

The linearization stage allocates three `f32` values per pixel before reducing them to one grayscale byte.  The binarizer then allocates its grayscale and binary working planes; Sauvola additionally constructs two `u64` integral images.

At 12 megapixels:

| Plane                     | Approximate size |
| ------------------------- | ---------------: |
| RGB8                      |            36 MB |
| Gray8                     |            12 MB |
| Binary as current `u8`    |            12 MB |
| Packed Mono1              |           1.5 MB |
| Linear RGB `f32`          |           144 MB |
| Two `u64` integral planes |   roughly 192 MB |

Those buffers do not all necessarily peak simultaneously, but the memory traffic is considerable.

## What should change

The correct design is not merely “let the renderer output binary.”

It is:

> Let the renderer produce a set of typed page products according to a page-specific output plan.

```rust
pub enum RasterFormat {
    Rgb8,
    Gray8,
    LinearGray8,
    Mono8,
    Mono1,
}

pub struct PageOutputPlan {
    pub analysis: Option<AnalysisTarget>,
    pub base: BaseTarget,
    pub regions: Vec<RegionTarget>,
    pub ocr: Option<OcrTarget>,
}
```

The renderer should not assume that every page has one full-size destination surface.

# 1. Direct grayscale output is the largest practical opportunity

For ordinary text and scanned pages, Lege-render should support:

```rust
RenderSurfaceFormat::Gray8
```

The compositor converts PDF paint colors, decoded images, masks, and coverage directly into the chosen grayscale representation.

That eliminates:

* The 3-byte-per-pixel RGB output.
* The subsequent RGB-to-gray pass.
* The 12-byte-per-pixel linear RGB transient.
* Several cache-unfriendly full-page memory reads and writes.

The binarization API should first be changed from:

```rust
fn binarize_rgb(rgb: &[u8], ...)
```

to a primary API of:

```rust
fn binarize_gray(
    gray: &[u8],
    width: usize,
    height: usize,
    options: &BinarizationOptions,
) -> BinaryPage;
```

The existing RGB entry point becomes a compatibility wrapper:

```rust
fn binarize_rgb(...) {
    let gray = convert_rgb_to_gray(...);
    binarize_gray(&gray, ...)
}
```

Most of the actual binarization algorithm already operates on grayscale; RGB is only its input adapter. The adaptive implementation receives a `gray: &[u8]` and performs Sauvola, background normalization, Otsu, and fusion from that plane.

That makes direct Gray8 rendering an immediate architectural fit.

## Correctness limitation

Direct grayscale is not universally identical to:

```text
render full color → convert final pixels to gray
```

It is safe or readily made equivalent for common pages using:

* Normal source-over compositing
* Opaque or normally transparent text and paths
* Images converted through the correct color pipeline
* DeviceGray content
* Neutral black/white document content

Use a conservative RGB fallback for pages involving:

* Non-normal blend modes
* Transparency groups with non-gray blend spaces
* DeviceN or Separation interactions
* Overprint behavior
* Difficult ICC transformations
* Color-dependent soft-mask behavior
* Other cases where color composition and grayscale conversion do not commute

`CompiledPage::features` should decide this before rendering:

```rust
pub enum GrayRenderEligibility {
    Exact,
    AcceptableForBilevel,
    RequiresColorFallback,
}
```

For the Lege-process use case, “acceptable for bilevel” may cover more pages than exact grayscale viewer output because the final result is thresholded anyway. It should still be explicit policy, not accidental behavior.

# 2. Direct binary output is possible, but narrower

A renderer cannot generally decide the final binary value while processing each drawing command.

Consider:

* Antialiased glyph edges
* Multiple overlapping translucent objects
* Soft masks
* A gray image behind black text
* Adaptive thresholding based on neighboring pixels

Thresholding each operation independently would produce a different result from compositing the final page and thresholding that result.

The correct conceptual sequence remains:

```text
PDF painting
    ↓
final luminance or coverage
    ↓
binarization
```

The integration improvement is that those stages can share storage and scheduling.

## Fixed threshold

Fixed threshold can be highly fused:

```text
render one completed band into Gray8
    ↓
threshold and pack that band
    ↓
send packed rows to encoder
    ↓
reuse band buffer
```

No full RGB page, full Gray8 page, or one-byte-per-pixel binary page is necessary.

## Adaptive Sauvola/Otsu

Your current adaptive algorithm is not a simple one-pass threshold:

* It computes a whole-page grayscale histogram.
* It calculates a 30th-percentile normalization constant.
* It creates a background-normalized plane.
* It calculates Otsu on that normalized distribution.
* It computes local mean and variance through large integral images.
* It fuses the Sauvola and Otsu results.

Therefore, adaptive output cannot normally be finalized during the original painting operation.

There are three implementation levels:

### Level A — recommended first

```text
renderer → Gray8 page → adaptive binarizer → packed Mono1
```

This already removes RGB and linear-RGB intermediates.

### Level B — lower-memory adaptive path

Use a two-pass or banded binarizer:

1. Render Gray8 while accumulating global histogram information.
2. Retain Gray8 in a compact page plane or temporary spool.
3. Process bands with sufficient vertical halo.
4. Emit packed binary rows.
5. Release completed grayscale bands.

This requires rewriting the integral-image and background-normalization implementation around rolling windows or band-local integrals.

### Level C — renderer-integrated postprocess sink

The renderer emits completed bands into an adaptive postprocess object:

```rust
trait CompletedBandSink {
    fn consume_band(
        &mut self,
        y: u32,
        gray: &[u8],
    ) -> Result<(), ProcessError>;
}
```

This is still logically postprocessing, but it avoids a hard full-page API boundary.

The heavy Sauvola model is even less streamable because its global instance normalization requires the entire input region at once. Your comments already explicitly recognize this whole-region requirement.  That path should continue to materialize at least a full Gray8 or model-input surface unless the model itself changes.

# 3. Layout detection does not require high-resolution RGB output

The detector creates an ordering dependency:

```text
render something
    ↓
detect image regions
    ↓
separate base and image regions
```

But the first render does not need to be the final high-resolution page.

The optimal architecture is a two-render plan over one compiled page:

```text
Compile PDF page once
        ↓
low-resolution analysis render
        ↓
PAL detections + margin decisions
        ↓
construct final output plan
        ↓
selective final render
```

## Pass A: analysis

Render only what PAL requires:

```rust
pub struct AnalysisTarget {
    pub width: u32,
    pub height: u32,
    pub format: AnalysisFormat,
}
```

Possibly:

* Low-resolution RGB if the model relies on color
* Low-resolution grayscale if model testing shows equivalence
* No full high-resolution output
* No expensive exact antialiasing beyond what the model needs

Your current inference stage already treats the inference image separately from the high-resolution page image, so this is a natural extension rather than a new pipeline concept. 

## Pass B: final products

After detections are available:

```text
Base page:
    Gray8 or Mono1
    final crop/scale
    detected image regions excluded

Each detected image region:
    RGB8, Gray8, or Mono1 as requested

OCR:
    separate Gray8/RGB target only when needed
```

This removes the need for one high-resolution RGB page to serve every consumer.

# 4. Exclude image regions during rendering instead of cloning and painting white

The current pipeline creates a masked RGB copy and fills image rectangles with white before binarization. 

Once detections are known, the final base target can be initialized white and rendered with exclusion rectangles:

```rust
pub struct BaseTarget {
    pub format: RasterFormat,
    pub exclusion_regions: Arc<[DeviceRect]>,
}
```

The renderer can reject spans that fall inside those regions:

```text
draw command
    ↓
clip against normal page clip
    ↓
subtract image exclusion regions
    ↓
composite remaining spans
```

Or, for a simpler first version:

1. Render Gray8 normally.
2. Fill excluded Gray8 rectangles with white.
3. Binarize.

Even that avoids an additional full-size RGB clone.

The more integrated form renders only the nonexcluded destination area.

This preserves the current pipeline’s semantics: detected image regions are blanked from the base and later covered with independently encoded overlays.

# 5. Render detected image regions directly

The current code extracts each detected region from the full adjusted RGB page:

```rust
process_image_region(
    adjusted_image.as_raw(),
    ...
)
```



With Lege-render, each final region can instead be a clipped render request:

```rust
pub struct RegionTarget {
    pub page_rect: PageRect,
    pub output_size: PixelSize,
    pub format: RasterFormat,
}
```

```text
CompiledPage
    ↓ region clip
RGB8 region surface
    ↓ JPEG/JP2 encoding
```

or:

```text
CompiledPage
    ↓ region clip
Gray8 region surface
    ↓ Gray JP2 / halftone / dithering
```

This means the full page can remain Gray8 while only a photograph or figure region is rendered in RGB.

That is one of the strongest new opportunities.

## Semantic image detection can augment PAL

Because the renderer sees the page IR, it already knows:

* Image XObject bounds
* Image masks
* Image transforms
* Form XObjects containing images
* Clipping paths
* Painting order
* Resolution and color space

This does not entirely replace PAL. PAL may detect:

* Figures assembled from several image objects
* Vector illustrations
* Tables
* Diagrams
* Composite regions
* Captions or semantic figure groupings

But it gives the detector strong prior information:

```rust
pub struct PageAnalysisHints {
    pub image_xobjects: Arc<[ImagePlacement]>,
    pub vector_density_regions: Arc<[PageRect]>,
    pub native_text_regions: Arc<[PageRect]>,
}
```

PAL detections and renderer-derived placements can then be merged.

For simple scanned PDFs containing one page-sized image, Lege-process may not need layout inference merely to establish that the page is an image.

# 6. Remove Gray → RGB → Gray conversions in image-region processing

There is a clear compatibility artifact in the current region code.

For Gray JP2, halftone, Stucki, and clustered-dot modes, the region is reduced to grayscale or bilevel, then expanded to RGB triplets because the existing pipeline expects RGB.  

The processing stage then immediately reconstructs a grayscale vector using:

```rust
let grayscale_data: Vec<u8> =
    region_data.chunks(3).map(|rgb| rgb[0]).collect();
```



That is exactly the sort of boundary the new integration should eliminate.

Use typed outputs:

```rust
pub enum RegionPixels {
    Rgb8(RgbSurface),
    Gray8(GraySurface),
    Mono8(Mono8Surface),
    Mono1(MonoBitmap),
}
```

Then:

* `None` returns `Rgb8`
* `GrayJp2` returns `Gray8`
* `Halftone` returns `Gray8`
* `Stucki` returns `Mono1` or temporarily `Mono8`
* `Ccitt4ClusteredDot4x4` returns `Mono1`

Encoders consume the native representation.

# 7. Stop using one byte per binary pixel as the permanent format

The current binary plane is `Vec<u8>` with values `0` or `255`, and packing happens later. 

That representation is convenient for existing OCR and mutation code, but it costs eight times the necessary memory.

Introduce:

```rust
pub struct MonoBitmap {
    pub width: u32,
    pub height: u32,
    pub stride_words: u32,
    pub words: Vec<u32>,
}
```

Use MSB-first packed rows, matching the planned JBIG2 decoder and CCITT conventions.

Provide operations needed by the pipeline:

```rust
impl MonoBitmap {
    fn fill_rect_white(&mut self, rect: RectI);
    fn copy_region(&self, rect: RectI) -> MonoBitmap;
    fn merge_region(&mut self, origin: PointI, source: &MonoBitmap);
    fn as_packed_bytes(&self) -> PackedRows<'_>;
}
```

Some consumers may still need `Mono8` temporarily:

* Existing OCR code
* Existing heavy model adapters
* Debug image output

Convert only at those boundaries.

## Encoder constraints

Different encoders have different streaming opportunities:

| Encoder                    | Likely direct row/band consumption      |
| -------------------------- | --------------------------------------- |
| CCITT Group 4              | Yes                                     |
| JBIG2 generic region       | Potentially yes                         |
| JBIG2 symbol mode          | Usually needs full bitmap/random access |
| DjVu JB2 symbol processing | Usually needs page-wide analysis        |
| PBM output                 | Yes                                     |
| OCR                        | Often easier with Gray8 or Mono8        |

Therefore, packed full-page storage remains useful even when direct streaming is unavailable.

# 8. MRC can become substantially cleaner

The current grayscale mode produces:

* `cleaned_gray`
* A separate one-byte-per-pixel ink mask
* The adjusted RGB page remains available
* The MRC encoder later consumes both planes. 

The new path should be:

```text
renderer → Gray8 final page
             ├── clean-gray → Gray8 background
             └── adaptive/fixed mask → Mono1 ink mask
```

The RGB page is unnecessary unless:

* A detected color image region must be preserved
* OCR specifically requires RGB
* The page triggers the color-fallback compositor
* It is a preserved color cover

The same final Gray8 plane can drive both cleaning and mask generation.

# 9. Crop, resize, and margin transforms should move into rendering

The current pipeline adjusts the rendered `RgbImage`, rescales detections, and may retain a separate high-resolution OCR image.

With a custom renderer:

```text
source page
    ↓ page-to-output affine transform
final crop, scale, centering, rotation
    ↓
destination pixels
```

Do not:

```text
render large RGB
    ↓
crop RGB
    ↓
resize RGB
```

For one-pass layouts, calculate the output transform before final rendering.

For two-pass document margin analysis:

1. Produce low-resolution analysis surfaces.
2. Determine document-wide margins.
3. Construct each final page transform.
4. Render final output once at its final dimensions.

Detection boxes and native text coordinates use the same affine transform.

# 10. OCR should request its own surface

The current slow OCR path receives either the retained high-resolution RGB image or the adjusted RGB image plus binary data. 

That should become an explicit request:

```rust
pub enum OcrSurfaceRequest {
    Gray8 {
        scale: RenderScale,
    },
    Rgb8 {
        scale: RenderScale,
    },
    Binary {
        scale: RenderScale,
    },
}
```

Test which OCR methods actually require three independent color channels.

Many OCR workflows only need luminance. In that case, eliminating RGB is straightforward. Where an OCR model requires RGB input, render RGB only when OCR is enabled and only at the required resolution.

Native PDF text extraction should require no raster surface at all.

# Recommended final pipeline

```text
                    ┌────────────────────────┐
PDF bytes ─────────▶│ CompiledPage           │
                    └───────────┬────────────┘
                                │
                     low-res analysis render
                                │
                    PAL + semantic page hints
                                │
                    margins + image detections
                                │
                    construct PageOutputPlan
                                │
          ┌─────────────────────┼─────────────────────┐
          │                     │                     │
   base render              region renders       OCR render
 Gray8 / Mono1           RGB8 / Gray8 / Mono1    only if needed
          │                     │                     │
   clean/binarize          encode immediately       OCR
          │
 packed Mono1
          │
 JBIG2 / CCITT / DjVu
```

For a simple no-layout text page:

```text
CompiledPage
    ↓ direct Gray8
adaptive binarizer
    ↓ packed Mono1
encoder
```

For a fixed-threshold simple page:

```text
CompiledPage
    ↓ Gray8 bands
threshold + pack
    ↓
streaming encoder
```

For a page with photographs:

```text
low-resolution analysis render
    ↓ detect photographs
Gray8 base render excluding photo boxes
    +
RGB region renders for photo boxes
    ↓
bilevel base + JPEG/JP2 overlays
```

For a preserved cover:

```text
RGB render or direct source-image extraction
    ↓
JPEG/JP2 encode
```

# Implementation order

## Phase 1: make grayscale a first-class binarizer input

Add:

```rust
binarize_gray_raw()
binarize_gray_into()
binarize_gray_packed()
```

Retain RGB wrappers.

This can be done before Lege-render is ready.

## Phase 2: typed image planes

Replace ambiguous `Vec<u8>` region data with:

```rust
Rgb8
Gray8
Mono8
Mono1
```

Remove all Gray→RGB→Gray compatibility conversions.

## Phase 3: Lege-render Gray8 target

Implement a grayscale CPU target and compare:

```text
direct Gray8 render
```

against:

```text
RGB render → current grayscale conversion
```

Use ordinary scanned, text, vector, transparency, and color-management corpora.

## Phase 4: page feature classifier and fallback

Classify pages into:

```text
direct gray exact
direct gray suitable for binarization
RGB fallback
```

No risky PDF feature should silently use direct grayscale.

## Phase 5: move output geometry into the render request

Apply crop, resize, rotation, standardization, and centering through the page-to-device matrix.

## Phase 6: two-pass analysis/final plan

Render a small PAL surface first, then request:

* Gray base
* Selective RGB regions
* Optional OCR surface

Remove the high-resolution full-page RGB requirement.

## Phase 7: packed binary output

Make `MonoBitmap` the ordinary bilevel representation.

Adapt JBIG2, CCITT, and DjVu encoders to consume it directly.

## Phase 8: banded fusion

After profiling, add:

* Fixed-threshold render-to-encoder streaming
* Rolling adaptive binarization
* Direct CCITT row emission
* Potential JBIG2 generic-region row integration

Do not start here; direct Gray8 will capture most of the architectural gain with far less risk.

## Final judgment

You are using similar words for **two different depths of integration**:

* The current deferred path already fuses **binarization with encoding** in selected cases.
* Lege-render enables fusion of **rendering, color reduction, region separation, geometry adjustment, binarization, and encoding policy**.

The primary target should be **direct Gray8 rendering**, not universal direct binary rendering.

Direct Gray8 is broadly applicable, removes the largest RGB and linear-RGB costs, remains compatible with adaptive binarization, and allows selective RGB rendering only where color is genuinely needed. Direct Mono1 should be a specialized path for simple pages and streamable threshold modes.
