Your thesis is directionally right, but I would phrase it more narrowly:

**For book-page OCR with mature engines, most avoidable errors come from page conditioning, segmentation, scale, skew, layout confusion, and bad region selection—not from the recognizer being intrinsically bad.**

That is a strong basis for either a Lege submodule or a standalone “OCR preprocessor.” But I would not make the claim absolute. Tesseract/WinOCR still have model limitations: historical fonts, damaged type, vertical text, mixed scripts, equations, tables, and old print can exceed what preprocessing can fix. Also, Tesseract does have hOCR-style positional output; hOCR commonly includes page/paragraph/line/word spans with `bbox` metadata, and Tesseract is one of the engines that can emit it. ([Wikipedia][1]) The issue is not “no coordinates,” but whether the coordinates are reliable enough after your chosen preprocessing path.

## The idea is good, but the core unit should be “text region → line images → OCR → coordinate reconstruction”

The best version of this is not “OCR preprocessor” in the vague sense. It is a **page-to-structured-text pipeline**:

```text
PDF/image page
→ high-res grayscale render
→ layout detections
→ text-region filtering
→ low-res or mid-res binary analysis image
→ paragraph/region line segmentation
→ high-res grayscale line crops
→ OCR line-by-line
→ coordinate remapping
→ hOCR / ALTO / PDF text overlay / EPUB document model
```

This aligns closely with your “smallest amount of text at highest useful resolution” principle. OCR engines are often hurt by being asked to solve too many problems at once: page layout, columns, ornaments, page numbers, footnotes, noise, headers, skew, uneven lighting, and text recognition. You want the model to do only the last part.

## What Lege already has

### 1. Page rendering and coordinate scale foundation

Lege already creates a high-resolution page image and a separate inference image. `RenderedPageData` carries both `high_res_image` and `inference_image`, plus original page dimensions in points.  The source stage builds the inference image from the high-res page only when layout detection is enabled, otherwise it shares the high-res image. 

That is exactly the architecture you need: one image for detection/analysis, one image for final OCR crops.

### 2. Layout detection regions

Your detection type already has class ID, optional class name, confidence, bbox, and context fields.  The class system already knows document labels such as `paragraph_title`, `text`, `number`, `abstract`, `content`, `figure_title`, `formula`, `table`, `reference`, `doc_title`, `footnote`, `header`, `footer`, `aside_text`, etc. 

That is more useful than a generic text mask. For EPUB, `doc_title`, `paragraph_title`, `footnote`, `header`, `footer`, `number`, `table`, and `text` should not all be treated the same.

### 3. Text-region classification

Lege already has a `LabelClassifier` that classifies detections as text-like or image-like. `should_process_with_ocr()` currently returns true for text-like detections. 

This is the obvious entry point for your proposed pipeline: instead of sending whole regions directly to OCR, send each region to a new line-segmentation module.

### 4. Existing region-based OCR and HOCR stitching

Lege already has region OCR. It extracts regions from a page image, OCRs them, strips HOCR down to the body, offsets `bbox` coordinates back into page space, and finalizes the page-level hOCR.  

The current `perform_region_based_ocr()` takes layout detections, filters text regions, extracts each bbox, OCRs it, and stitches the adjusted hOCR back together.  It also falls back to tiled OCR and then full-page OCR when the stitched result is empty. 

So your new module should not be “add region OCR.” It should be **replace region OCR with line-aware region OCR**.

### 5. Binarization and content bounds

Lege already uses binarization to compute content bounds for margin handling. `compute_pixel_bounds_for_margin()` binarizes an RGB page, normalizes the mask, and calls `calculate_content_bounds_from_binary_mask()`. 

This can be reused conceptually for line segmentation: binary image for geometry; grayscale image for recognition.

## What is missing

### 1. A dedicated line segmentation module

This is the main missing piece.

I would add:

```text
src/ocr_preprocess/
  mod.rs
  line_segment.rs
  region_normalize.rs
  coordinate.rs
  hocr.rs
  epub_layout.rs
  engine.rs
```

Core data types:

```rust
pub struct TextRegion {
    pub page_index: usize,
    pub region_id: usize,
    pub class_name: Option<String>,
    pub bbox_highres: Rect,
    pub confidence: f32,
}

pub struct TextLineCrop {
    pub page_index: usize,
    pub region_id: usize,
    pub line_index: usize,
    pub bbox_highres: Rect,
    pub image_gray: GrayImage,
    pub baseline: Option<Baseline>,
    pub indent_px: u32,
}

pub struct OcrLineResult {
    pub text: String,
    pub confidence: Option<f32>,
    pub words: Vec<OcrWord>,
    pub bbox_highres: Rect,
}
```

The segmentation module should not depend on Tesseract or WinOCR. It should output line crops and coordinates. OCR engine integration should be separate.

### 2. Robust line finding, beyond “draw horizontal lines”

Your proposed horizontal whitespace method is good as a first baseline, but it can go wrong in predictable cases:

Thin ascenders/descenders can bridge rows. Large drop caps can span multiple lines. Italics and skew can smear rows diagonally. Underlines, table rules, and page damage can create false connectors. Footnotes may have tighter leading. Multi-column detections may accidentally contain two columns. Old scans may have bleed-through and speckles.

Better line segmentation approach:

1. Take the layout bbox.
2. Expand it slightly, maybe 2–4 percent.
3. Binarize only that region.
4. Clean noise with light morphology or connected-component filtering.
5. Compute horizontal projection: count black pixels per y row.
6. Smooth the projection.
7. Find low-density valleys, not necessarily full blank rows.
8. Split at valleys with minimum line-height constraints.
9. Merge tiny fragments.
10. Estimate baseline per line if possible.
11. Map each line crop back to the high-res grayscale image.

So the principle should be “projection-profile line segmentation with connected-component cleanup,” not literally “draw lines until one goes all the way through.” A completely empty row is common in clean scans, but not guaranteed.

### 3. Grayscale OCR path

Current `perform_ocr_on_binarized()` explicitly calls `run_ocr(..., true)` and comments that the data is binarized.  Your new design needs:

```rust
perform_ocr_on_grayscale_line(
    gray: Vec<u8>,
    width: usize,
    height: usize,
    language: &str,
    psm: OcrSegmentationMode,
) -> Result<OcrLineResult>
```

For Tesseract, line mode should use a page segmentation mode equivalent to “single text line.” For WinOCR, you may be constrained by its API, but you can still feed it a small grayscale bitmap.

This is also where your “binoculars” analogy needs one constraint: **too much scale can hurt.** The target should not be “highest possible resolution,” but “high enough x-height.” Very large line crops may slow the engine or distort learned priors. A practical target is to normalize line height or estimated x-height into a sane band, then benchmark.

### 4. Coordinate model

You will need a formal coordinate transformation layer.

At minimum:

```rust
struct CoordinateMap {
    highres_width: u32,
    highres_height: u32,
    analysis_width: u32,
    analysis_height: u32,
    scale_x: f32,
    scale_y: f32,
}
```

Functions:

```rust
analysis_to_highres(rect) -> rect
highres_to_pdf_points(rect, original_width_pts, original_height_pts) -> rect
line_local_to_page(rect, line_crop_origin) -> rect
```

Lege already does bbox offsetting for hOCR stitching with `adjust_hocr_offsets()`.  But line-level OCR needs more: region-local → line-local → page-pixel → PDF-point coordinate mapping.

### 5. Reading-order reconstruction

For EPUB, OCR text is not enough. You need a document model.

A practical model:

```rust
PageText {
    blocks: Vec<TextBlock>
}

TextBlock {
    class_name: BlockKind,
    bbox: Rect,
    lines: Vec<TextLine>,
    indent_px: u32,
    spacing_before_px: u32,
    spacing_after_px: u32,
}

TextLine {
    text: String,
    bbox: Rect,
    words: Vec<Word>,
}
```

Then build reading order:

1. Remove headers/footers/page numbers unless the user wants them.
2. Sort blocks by column, then y.
3. Detect columns using x-clustering.
4. Group nearby text detections into paragraph blocks.
5. Use first-line indent and vertical spacing to infer paragraph breaks.
6. Use label class to map `doc_title`/`paragraph_title` to headings.
7. Treat footnotes separately.
8. Preserve tables as non-flowable blocks initially.

This is the part that makes EPUB hard. OCR accuracy can be high and EPUB quality still poor if reading order and paragraph reconstruction are wrong.

## Where your idea is strongest

The strongest part is **line-level OCR from high-res grayscale crops**.

That specifically attacks three problems:

First, it avoids the OCR engine’s layout analysis. You already have better external layout signals.

Second, it avoids making OCR interpret giant mixed-content pages.

Third, it gives you direct line coordinates, making hOCR/PDF overlays and EPUB text reconstruction easier.

This is better than only doing “better binarization,” because you are changing the problem shape given to the OCR engine.

## Where I would be cautious

### “OCR engines were trained on grayscale, not binarized text”

This is plausibly true for some engines/models and not universally true as a practical statement. Tesseract historically does internal thresholding and layout analysis, and its quality can improve or worsen depending on whether you feed grayscale, binary, or carefully binarized input. The right design is to support both:

```text
line crop source:
- grayscale
- contrast-normalized grayscale
- binarized
- inverted-corrected grayscale
```

Then benchmark. Do not assume grayscale always wins.

### “Connected component analysis can’t go wrong”

It can go wrong, but it is still the right baseline. Treat line segmentation as a scored algorithm with fallbacks. Example confidence checks:

```text
- Are line heights plausible?
- Are line bboxes non-overlapping?
- Does OCR return plausible text length?
- Are too many lines empty?
- Are there abnormal vertical gaps?
- Does detected region width suggest two columns?
```

When confidence is low, fallback to region-level OCR or tiled OCR, which Lege already has. 

### “Confluence/jury system”

Useful, but not first. It can become expensive and can create false confidence. I would only add it after you have a deterministic line pipeline and an evaluation set.

The best early “jury” is not multiple OCR engines. It is multiple preprocessing variants on the same line:

```text
A: grayscale normalized
B: grayscale sharpened/contrast stretched
C: adaptive binary
D: fixed threshold, if clean scan
```

Then choose based on OCR confidence, dictionary plausibility, and character-set constraints.

## Recommended first implementation target

Do not start with EPUB. Start with a debug mode:

```text
lege ocr-prep-debug input.pdf --page 12 --out debug/
```

Output:

```text
debug/page_001/
  detections.json
  region_00_binary.png
  region_00_projection.csv
  region_00_lines.png
  line_00_gray.png
  line_00_ocr.json
  page.hocr
  page_overlay_debug.svg
```

This will make the hard part visible. You want to see where segmentation fails before wiring it into production.

Then add a real mode:

```text
lege input.pdf --ocr-line-preprocess --ocr-output hocr
lege input.pdf --ocr-line-preprocess --epub-output book.epub
```

## Suggested Lege mapping by step

### Your step 1: layout detection

Corresponding Lege pieces:

* `Detection` type: bbox, class, confidence. 
* `LabelInfo`/`LabelClassifier`: document class names and text/image decisions. 
* `RenderedPageData`: high-res and inference image pair. 

Missing:

* OCR-specific block taxonomy.
* Reading-order sort.
* Column detection.
* Confidence scoring for whether layout detections are good enough for OCR segmentation.

### Your step 2: segmentation and high-res line OCR

Corresponding Lege pieces:

* `extract_region_from_image()` already extracts rectangular regions from flat grayscale buffers. 
* Region-based OCR already stitches region output into page hOCR. 
* Existing binarization can produce masks for geometry. 

Missing:

* Region-local binarization for line geometry.
* Horizontal projection / connected-component line segmentation.
* High-res grayscale line crop extraction.
* OCR mode for single-line grayscale.
* Line-local to page-global hOCR remapping.
* Debug visualization.

### Your step 3: EPUB paragraph mapping

Corresponding Lege pieces:

* Layout labels already distinguish title/body/footnote/header/footer/table-ish classes. 
* Detections already carry bboxes, so indentation and spacing can be inferred. 

Missing:

* `DocumentTextModel`.
* Reading order.
* Paragraph grouping.
* Hyphenation repair.
* Heading inference.
* Footnote handling.
* Table handling.
* EPUB writer.

### Your step 4: grid fallback, multi-pass OCR, jury/spellcheck

Corresponding Lege pieces:

* Tiling fallback exists conceptually. 
* Region fallback exists when region OCR produces nothing. 

Missing:

* Grid-based “layout missed text” detector.
* OCR confidence normalization across engines.
* Spellcheck/language-model correction.
* Variant-ranking framework.
* Evaluation harness.

## Submodule vs standalone program

I would implement it as a **Lege submodule first**, not standalone.

Reason: the hard dependencies are already in Lege: page rendering, layout detection, binarization, bbox scaling, OCR invocation, hOCR stitching, and PDF output. A standalone program would immediately need to recreate or depend on most of Lege.

But design it with standalone boundaries:

```text
ocr_preprocess_core:
  pure image/geometry/OCR-prep logic

lege integration:
  Pdfium rendering
  Paddle layout
  Tesseract/WinOCR adapters
  PDF/EPUB output
  CLI flags
```

Later, if it proves useful independently, extract `ocr_preprocess_core` into a crate.

## Practical MVP

I would define MVP as:

```text
Given one page and existing layout detections:
1. Find text-like detections.
2. For each text detection, segment lines on a binary analysis crop.
3. Extract grayscale high-res line crops.
4. Run OCR in single-line mode.
5. Reconstruct page hOCR with global coordinates.
6. Compare against current region-based OCR.
```

Do not include EPUB in MVP. EPUB is the second product built on top of this.

Success metric:

```text
Line OCR should reduce:
- missing words
- merged lines
- column-order errors
- header/footer contamination
- false text from images
- bad hOCR bbox placement
```

Only after that should you build:

```text
PageText → ChapterText → EPUB
```

## My overall take

This is one of the more natural next modules for Lege because it reuses the exact infrastructure Lege already has. You are not proposing a separate OCR engine; you are proposing an **OCR orchestration layer**. That is a better fit for Lege than training models.

The main adjustment I would make is to treat line segmentation as probabilistic and fallible. Build it with debug outputs and fallbacks from the beginning. Your current region/tile/full-page fallback structure is already the right philosophy; replace the first stage with a better line-aware stage, not with a brittle all-or-nothing segmentation pass.

[1]: https://en.wikipedia.org/wiki/HOCR?utm_source=chatgpt.com "HOCR"

# Plan: `lege-ocr` — Slow OCR Submodule

## Context

The existing `src/ocr/` subsystem (fast path) uses region-based → tiling → full-page fallback OCR on binarized input. It produces hOCR overlays adequate for document search but not for accurate text extraction — estimated ~85% accuracy. The goal is a new "slow" OCR path targeting ~99.9% accuracy by feeding OCR engines only clean, line-level, high-res grayscale crops instead of large mixed-content regions. Output: reliable hOCR now; EPUB later.

The key insight from OCR.md: the bottleneck is not the OCR engine but the problem shape handed to it — layout confusion, scale, skew, and mixed content. Lege already owns layout detection, binarization, coordinate mapping, and region extraction; this module wires them together at line granularity.

---

## New workspace crate: `lege-ocr/`

A standalone workspace member. Does **not** depend on the root `lege` crate. Can be built and tested independently via `cargo build -p lege-ocr` / `cargo test -p lege-ocr`.

```
lege-ocr/
  Cargo.toml
  src/
    lib.rs            — public API: OcrPipeline, SlowOcrPage
    types.rs          — TextRegion, TextLineCrop, OcrLineResult, OcrWord
    coordinate.rs     — CoordinateMap + analysis_to_highres / highres_to_pdf
    segment.rs        — horizontal projection-profile line segmentation
    normalize.rs      — region expand, binarize, morphological cleanup
    engine.rs         — OcrEngine trait + TesseractEngine / WinOcrEngine / TrOcrEngine stubs
    hocr.rs           — assemble per-line results into page-level hOCR
    document.rs       — PageText / TextBlock / TextLine doc model (EPUB prep, not wired yet)
    debug.rs          — debug dump: PNG, CSV, JSON, SVG outputs
  src/bin/
    lege_ocr_debug.rs — standalone tester binary (no lege dependency)
```

### `lege-ocr/Cargo.toml` dependencies
- `image` — GrayImage, RgbImage, crop/buffer ops
- `lege-gpu` (path dep) — `binarization::try_binarize_gray`, `resize::resize_bytes`
- `tesseract` (same version as root, feature-gated `tesseract-backend`)
- `serde`, `serde_json` — debug output serialization
- `rayon` — parallel line OCR
- Optional: `trocr` feature stub (no-op until a viable Rust binding exists)

---

## Core types (`types.rs`)

```rust
pub struct TextRegion {
    pub page_index: usize,
    pub region_id: usize,
    pub class_name: Option<String>,   // "text", "paragraph_title", "footnote", etc.
    pub bbox_highres: [u32; 4],       // [x1,y1,x2,y2] in high-res pixel space
    pub confidence: f32,
}

pub struct TextLineCrop {
    pub region_id: usize,
    pub line_index: usize,
    pub bbox_highres: [u32; 4],       // page-global high-res coords
    pub image_gray: image::GrayImage, // high-res grayscale crop
    pub baseline_y: Option<u32>,      // estimated baseline in crop-local y
}

pub struct OcrLineResult {
    pub text: String,
    pub confidence: Option<f32>,
    pub words: Vec<OcrWord>,
    pub bbox_highres: [u32; 4],
}

pub struct OcrWord {
    pub text: String,
    pub bbox_highres: [u32; 4],
    pub confidence: Option<f32>,
}

pub struct SlowOcrPage {
    pub page_index: usize,
    pub hocr: String,
    pub regions: Vec<TextRegion>,
    pub lines: Vec<OcrLineResult>,    // flat; each carries its bbox
}
```

---

## Coordinate layer (`coordinate.rs`)

```rust
pub struct CoordinateMap {
    pub highres_width: u32,
    pub highres_height: u32,
    pub analysis_width: u32,
    pub analysis_height: u32,
    pub pdf_width_pts: f32,
    pub pdf_height_pts: f32,
}

impl CoordinateMap {
    pub fn analysis_to_highres(&self, rect: [f32;4]) -> [u32;4]
    pub fn highres_to_pdf_pts(&self, rect: [u32;4]) -> [f32;4]
    pub fn line_local_to_page(&self, rect: [u32;4], region_origin: [u32;2]) -> [u32;4]
}
```

Mirrors the existing `map_bbox_infer_to_page` / `full_infer_bbox_to_final_page` in `src/pipeline/policies.rs`, but expressed without pipeline config coupling.

---

## Line segmentation algorithm (`segment.rs`)

Given a binary analysis-res image of one text region:

1. Expand bbox by 3% on each side (clamped to page bounds)
2. Extract region from binarized analysis image
3. Compute horizontal projection: `black_px_per_row[y]` = count of set bits in row y
4. Smooth projection with a 5-row Gaussian kernel
5. Find valley centers: rows where smoothed projection < 15% of local max AND gap ≥ min_line_height (default 8px at analysis res)
6. Split region at each valley center; enforce min line height (merge lines shorter than threshold upward)
7. Filter: skip lines where black pixel count < 5 (empty / noise)
8. Map each line bbox back through `CoordinateMap::analysis_to_highres`
9. Extract high-res grayscale crop from `RenderedPageData::high_res_image`
10. Optionally estimate baseline (bottom of dominant connected component in bottom 30% of line)

Confidence checks (for fallback trigger):
- Line heights plausible (5px–200px at analysis res)
- Lines non-overlapping
- No line has zero OCR result when region has visible ink
- Too many empty lines (>50%) → fallback to region OCR

---

## OCR engine trait (`engine.rs`)

```rust
pub trait OcrEngine: Send + Sync {
    fn ocr_line(&self, image: &image::GrayImage, lang: &str) -> anyhow::Result<OcrLineResult>;
    fn ocr_region(&self, image: &image::GrayImage, lang: &str) -> anyhow::Result<Vec<OcrLineResult>>;
    fn name(&self) -> &'static str;
}
```

**`TesseractEngine`** (Linux/macOS default, Windows fallback):
- PSM 7 (single line) for `ocr_line`
- PSM 6 (block of uniform text) for `ocr_region`
- Grayscale input preferred; caller controls binarization before passing
- Reuses `tesseract` crate v0.14 already in workspace

**`WinOcrEngine`** (Windows primary):
- Wraps existing `src/ocr/winocr.rs` logic
- Crops passed as grayscale; internal BGRA conversion retained
- PSM equivalent: single-line mode via WinRT `RecognizeAsync`

**`TrOcrEngine`** (feature `trocr`, initially a no-op stub):
- Trait impl that returns `Err(NotImplemented)`
- Slot reserved for a Rust TrOCR binding once one matures
- Selection heuristic placeholder: prefer TrOCR when estimated x-height ≥ 24px

---

## hOCR assembly (`hocr.rs`)

- Build `<span class="ocr_line" title="bbox x1 y1 x2 y2">` elements from `OcrLineResult`
- Build `<span class="ocrx_word">` from `OcrWord`
- Wrap in `<div class="ocr_page">` with page dimensions
- Reuses the bbox-offset pattern already in `src/ocr/ocr.rs` (`adjust_hocr_offsets`, `finalize_hocr`), but all coordinates are already in page-global space so no offset arithmetic needed at stitch time

---

## Debug mode (`debug.rs` + `src/bin/lege_ocr_debug.rs`)

Binary usage:
```
lege-ocr-debug input.png [--detections detections.json] --out debug/
lege-ocr-debug input.pdf --page 12 --out debug/
```

Outputs under `debug/page_NNN/`:
- `detections.json` — raw layout detections fed in
- `region_NNN_binary.png` — binarized analysis crop
- `region_NNN_projection.csv` — y, raw_count, smoothed_count
- `region_NNN_lines.png` — binarized crop annotated with line split boundaries
- `line_NNN_NNN_gray.png` — high-res grayscale line crop
- `line_NNN_NNN_ocr.json` — `OcrLineResult` serialized
- `page.hocr` — assembled page hOCR
- `page_overlay.svg` — page image + bbox overlays (regions, lines, words)

The binary can accept a pre-rendered PNG (no PDF/pdfium dependency) so it builds without the full Lege dependency tree.

---

## Integration into root `lege` crate

1. Add `lege-ocr` as path dependency in root `Cargo.toml`
2. Add `src/ocr/slow.rs` — thin adapter:
   ```rust
   pub async fn perform_slow_ocr(
       high_res: &RgbImage,
       analysis: &RgbImage,
       binarized: &[u8],
       detections: &[Detection],
       coord_map: CoordinateMap,
       config: &PipelineConfig,
   ) -> Result<String>  // returns hOCR string
   ```
   Converts `lege::Detection` → `lege_ocr::TextRegion`, calls `lege_ocr::OcrPipeline::process_page`, returns assembled hOCR
3. In `src/pipeline/pdf_tokio_pipeline.rs`, branch on `config.slow_ocr_enabled()`:
   - `true` → call `perform_slow_ocr` (new)
   - `false` → call `perform_ocr` (existing, unchanged)
4. Add `--slow-ocr` CLI flag (long-only, default off)
5. Add `slow_ocr_enabled: bool` to `PipelineConfig`

`lege-gpu` is already a dependency of `lege`; `lege-ocr` adds no new heavy transitive deps.

---

## Workspace change

Add `"lege-ocr"` to `[workspace] members` in root `Cargo.toml`. Keep it out of `default-members` initially (opt-in build).

---

## What is explicitly deferred

- EPUB output (`document.rs` stubs exist but are not wired)
- TrOCR runtime integration (trait stub only)
- Reading-order reconstruction / column sorting
- Paragraph grouping / hyphenation repair
- Evaluation harness / ground-truth comparison

---

## Verification

1. `cargo build -p lege-ocr` — standalone build, no pdfium, no freya
2. `cargo test -p lege-ocr` — unit tests for `segment.rs` (projection valleys), `coordinate.rs` (round-trip), `hocr.rs` (bbox encoding)
3. `lege-ocr-debug sample_page.png --out debug/` — inspect debug outputs manually
4. `cargo run -- input.pdf --slow-ocr` — full pipeline run; compare resulting hOCR against existing fast-path output side by side
5. Visual check: open output PDF, verify text-selection layer aligns with glyphs

---

## Implementation order

1. `lege-ocr/` crate scaffold + `types.rs` + `coordinate.rs`
2. `segment.rs` (projection-profile segmentation, unit-tested on synthetic binary images)
3. `engine.rs` trait + `TesseractEngine` / `WinOcrEngine` impls
4. `hocr.rs` assembly
5. `OcrPipeline::process_page` wiring all of the above
6. `debug.rs` + `lege_ocr_debug` binary
7. Root integration (`src/ocr/slow.rs`, pipeline branch, CLI flag)
8. Confidence-check fallbacks (region OCR on low-confidence segmentation)