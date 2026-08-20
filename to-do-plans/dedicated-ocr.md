# Recommendation

Do **not** replace PaddleOCR with TrOCR as the primary engine. The architecture in the uploaded code is leaving substantial performance on the table before model choice becomes the main issue.

The right direction is:

1. Keep a lightweight detector–recognizer system as the default.
2. Upgrade and properly benchmark PP-OCRv6 small and medium.
3. Redesign recognition around **cross-page, cross-book GPU batching**.
4. Add a canonical structured document representation before writing more exporters.
5. Offer separate **search**, **structured**, and **scientific** processing profiles.
6. Build the CLI and resumable batch processor first. Add a thin queue GUI later.

Your “thorough” method should become a **selective fallback**, not the standard path for every page.

This was a static review of the code-only archive. The model binaries and benchmark outputs were not included, so the performance conclusions below come from control flow, memory movement, and synchronization rather than measurements in this environment.

---

# The immediate issue: this is currently PP-OCRv5, not v6

The implementation in the archive explicitly identifies itself as PP-OCRv5 and embeds `ppocr-det.onnx` and `ppocr-rec.onnx`:

* `lege-ocr/src/engine_paddle.txt:1-14`
* `lege-ocr/src/engine_paddle.txt:24-30`

There is a `trocr` feature declared, but no corresponding implementation:

* `lege-ocr/Cargo.txt:44-48`

Either the archive is behind another branch, or “v6” is currently being used as the pipeline name rather than the actual model generation.

PP-OCRv6 was released on June 11, 2026. Paddle describes tiny, small, and medium tiers from 1.5 million to 34.5 million parameters, with one model covering 50 languages. Its published improvements over PP-OCRv5 are vendor benchmarks, so they should be treated as a reason to test it—not as proof it will be faster in `lege-gpu`. ([PaddlePaddle][1])

My model recommendation is:

* **PP-OCRv6 small:** default high-throughput engine.
* **PP-OCRv6 medium:** quality profile and retry engine.
* **Current PP-OCRv5:** retain temporarily as a regression baseline.
* **TrOCR:** optional specialist fallback, primarily for handwriting or unusually difficult individual lines.
* **Formula/table models:** invoked only for detected formula and table regions.
* **Document VLM:** optional external high-accuracy profile, not the normal path.

The official PP-OCRv6 small recognizer is only about 5.2 million parameters, although its CTC-plus-NRTR design means you must first inspect the exported graph and decide which output branch you intend to execute. ([Hugging Face][2])

---

# What is actually limiting the current OCR implementation

## 1. Recognition is one GPU inference per line

The recognizer uses fixed height 48 and width buckets of 160, 320, 480, 640, and 960:

* `lege-gpu/src/vision/api.txt:493-507`

But it then recognizes lines sequentially:

* `lege-gpu/src/vision/api.txt:766-770`
* `lege-ocr/src/engine_paddle.txt:131-176`

For a 500-page book with approximately 40 lines per page, that is around **20,000 separate recognition submissions**.

Even a very fast neural network will perform badly when surrounded by:

* one crop allocation,
* grayscale conversion,
* resize,
* normalization,
* padding,
* upload,
* GPU dispatch,
* synchronization,
* full output readback,
* CPU CTC decoding,

for every individual line.

The fundamental unit of inference must become a **batch of similarly sized lines**, not a line.

## 2. The recognizer and detector are effectively serialized

The recognizer owns a mutex-protected graph cache:

* `lege-gpu/src/vision/api.txt:540-575`

`run_preprocessed()` holds that cache lock while it runs and waits for the graph:

* `lege-gpu/src/vision/api.txt:610-645`

The detector does the same:

* `lege-gpu/src/vision/api.txt:840-845`
* `lege-gpu/src/vision/api.txt:919-953`

Meanwhile, `CompiledGraph` is explicitly single-flight:

* `lege-gpu/src/vision/runtime/compiled.txt:642-645`

Therefore, putting OCR calls behind more Tokio tasks or page workers does not create meaningful GPU concurrency. The workers eventually wait on the same locked graph.

Your layout pipeline already has the better pattern: sibling sessions with shared model/device state but independent activation and readback buffers:

* `lege-gpu/src/vision/api.txt:126-141`
* `lege-process/pipeline/inference.txt:19-33`
* `lege-process/pipeline/inference.txt:66-80`
* `lege-process/pipeline/inference.txt:99-153`

OCR should adopt that pattern after recognition batching is in place.

## 3. You read back the complete CTC matrix for every line

The CTC decoder receives an output shaped approximately as `[batch, timesteps, classes]` and performs CPU argmax across all classes:

* `lege-gpu/src/vision/decode/ctc.txt:1-7`
* `lege-gpu/src/vision/decode/ctc.txt:62-89`

The current dictionary expects 18,385 classes:

* `lege-ocr/src/engine_paddle.txt:296-301`
* `lege-gpu/src/vision/api.txt:650-659`

As an illustration, if a line has 40 output timesteps:

```text
40 × 18,385 × 4 bytes ≈ 2.94 MB per line
```

At 40 lines per page, that could mean roughly 118 MB of logits copied from GPU to CPU per page. The precise number depends on the model’s timestep count and selected width bucket, but the architectural problem remains.

Add a GPU postprocessing stage that produces only:

* top token ID per timestep,
* top score or logit,
* second-best score or margin,
* optionally blank probability,
* valid timestep count.

Then read back perhaps hundreds of bytes per line rather than megabytes. CTC collapse itself can remain on the CPU initially.

This is likely one of the highest-payoff changes in the entire OCR path.

## 4. Detection and recognition use the same downscaled page

The fast Paddle path caps the entire input to four million pixels and then uses the resulting image for both detection and recognition:

* `lege-ocr/src/engine_paddle.txt:202-224`

The detector subsequently limits its own long side to 960 pixels:

* `lege-gpu/src/vision/api.txt:798-800`

That means the recognizer may receive line crops from a page that was already unnecessarily reduced, even though detection only needed a small image.

Use two coordinate-linked representations:

* **Detection image:** approximately 960–1536 pixels on the long side.
* **Recognition source:** untouched source pixels, or a high-resolution 300-DPI-equivalent grayscale page.

Map detector polygons back to the high-resolution source and crop recognition lines there. This should improve recognition quality without making detection slower.

It may eliminate part of the perceived need for a more powerful recognition model.

## 5. The “fast” path is not yet a genuine GPU pipeline

The fast path still does CPU luminance conversion and page cleaning, constructs a host grayscale image, then invokes the detector and recognizer:

* `lege-process/ocr/fast.txt:46-130`

Each line subsequently becomes an allocated image and then a new tensor. The thorough path adds more full-page cloning and retries:

* `lege-process/ocr/slow.txt:95-181`
* `lege-ocr/src/lib.txt:267-405`
* `lege-ocr/src/lib.txt:408-579`

The first optimization does **not** need to be a complete GPU preprocessing rewrite. Start with:

* reusable page buffers,
* crop views rather than owned crop images,
* reusable batch tensors,
* a bounded line queue,
* batched upload,
* compact CTC readback.

Move crop/resize/normalize to compute shaders only after the batching benchmark shows the remaining CPU preparation cost.

## 6. A code comment says batching does not help, but the code contains a batching benchmark

The recognition code comments state that batching did not produce a speedup:

* `lege-gpu/src/vision/api.txt:498-500`
* `lege-gpu/src/vision/api.txt:766-767`

The same file contains an environment-gated benchmark for batches 4, 8, and 16:

* `lege-gpu/src/vision/api.txt:1439-1534`

There are no benchmark results in the archive. Do not let the old comment determine the architecture.

Batching can fail to help when:

* implementation still submits per item internally,
* preprocessing remains serial,
* output readback dominates,
* shapes cause inefficient kernels,
* graph construction is repeated,
* the tested model or GPU was underutilized for another reason.

The agent should re-run this from a clean baseline and report GPU utilization, submission count, readback bytes, and lines per second—not just elapsed time.

---

# The pipeline I would build

```text
Input discovery
    ↓
PDF page classification
    ↓
Direct scan extraction OR renderer
    ↓
Page normalization and quality classification
    ↓
Low-resolution text/layout detection
    ↓
High-resolution ROI mapping
    ↓
Cross-document recognition batching
    ↓
GPU CTC reduction
    ↓
Confidence and selective retry
    ↓
Document structure reconstruction
    ↓
Canonical document IR
    ↓
Searchable PDF / DOCX / HTML / JSON / ALTO / PAGE / LaTeX
```

## Stage 1: avoid rendering raster pages when possible

A raster book PDF commonly consists of one dominant image XObject per page. Rendering that page often means decoding the image and resampling or copying it into another surface before OCR sees it.

Your renderer already has image-only page analysis and prepared image paths:

* `lege-pdf/render/crates/pdf-render-cpu/src/lib.txt:214-222`
* `lege-pdf/render/crates/pdf-render-cpu/src/lib.txt:1249-1273`
* `lege-pdf/render/crates/pdf-render-wgpu/src/lib.txt:2428-2431`

Add a scan-specialized intake route:

1. Detect a page containing one dominant image with a simple transform.
2. Decode that source image directly.
3. Preserve its original pixel dimensions.
4. Record the image-to-page coordinate transform.
5. Fall back to the full renderer for mixed pages, clipping, annotations, masks, multiple images, or unusual transforms.

For scanned books, this may matter more than making the normal renderer another 20% faster.

## Stage 2: pipeline pages ahead of recognition

Detection should produce lightweight ROI records:

```rust
struct TextRoi {
    document_id: DocumentId,
    page_index: u32,
    region_id: RegionId,
    polygon_source_px: Polygon,
    orientation: TextOrientation,
    estimated_width: u32,
    language_hint: Option<Language>,
}
```

It should not immediately allocate one `GrayImage` per line.

A scheduler should collect ROIs from multiple pages and multiple books, group them by:

* recognition model,
* language/model alphabet,
* orientation,
* width bucket,
* preprocessing profile,

and then create batches.

Ten books are an advantage here. They provide enough lines to keep every bucket full.

## Stage 3: fixed recognition batch shapes

For the existing width buckets, test recognition batches:

```text
B = 1, 4, 8, 16, 32
W = 160, 320, 480, 640, 960
H = 48
```

Do not necessarily retain all 25 combinations. Based on telemetry, a likely practical set would be:

```text
B = 1, 8, 16 or 32
```

Use a small tail batch for the end of a job. In long batch processing, padding a nearly full batch is usually preferable to waiting for an exact shape.

The graph-cache key should eventually include at least:

```text
(model, dtype, batch size, width bucket)
```

Each active shape should have perhaps two independently owned execution sessions so that one can execute while another is uploading or returning compact results.

## Stage 4: split submission from completion

The current API uses the synchronous convenience path around `compiled.run()`. Build an OCR scheduler around:

```text
prepare → submit → retain in-flight handle → submit next session → collect completed
```

One graph remains single-flight, but a pool of two or three sibling graphs can overlap:

* CPU batch preparation,
* GPU execution,
* compact readback,
* CPU CTC collapse.

Do not hold a global model-cache mutex across execution. Lock only to locate or create the session pool, check out a session, and return it afterward.

## Stage 5: GPU CTC reduction

The first GPU postprocessor should perform:

1. Argmax over classes for every timestep.
2. Optionally top-two selection or log-sum-exp.
3. Write compact IDs and confidence values.
4. Ignore padded batch entries using a valid-count value.

Leave duplicate collapse and dictionary decoding on the CPU until profiling shows otherwise.

This also gives you a foundation for real recognition confidence.

At present, the Paddle engine assigns detector confidence to the recognized line, while word confidence is absent:

* `lege-ocr/src/engine_paddle.txt:164-171`

Those are different quantities. Store them separately:

```rust
struct RecognitionConfidence {
    detection: f32,
    mean_token: f32,
    minimum_token: f32,
    mean_margin: f32,
    blank_ratio: f32,
    abnormal_character_ratio: f32,
}
```

A retry policy can then use actual recognition evidence.

## Stage 6: selective quality routing

The normal printed-book path should be:

```text
natural grayscale → detection → recognition
```

Your own comments and code already indicate that hard binarization can damage Paddle results. Preserve binarization and heavier cleaning as alternatives, but invoke them only when:

* confidence is poor,
* the text contains abnormal character sequences,
* the crop is low contrast,
* the line is extremely narrow or small,
* the detector and recognizer disagree,
* a document-level language model flags an implausible result.

A sensible retry ladder is:

```text
1. PP-OCRv6 small, original grayscale
2. PP-OCRv6 small, alternate preprocessing
3. PP-OCRv6 medium, original grayscale
4. Specialist model only for unresolved crops
5. Mark for review rather than repeatedly spending GPU time
```

That will usually outperform running the maximum-quality path on every line.

---

# TrOCR is not the right default replacement

TrOCR is a recognition model, not a complete page OCR system. It combines an image Transformer with a text Transformer for wordpiece-level text generation. Microsoft’s published variants are 62 million, 334 million, and 558 million parameters. ([GitHub][3])

That creates several problems for your use case:

* It does not detect text lines.
* It does not recover page layout.
* It does not reconstruct tables.
* It does not understand chapter hierarchy.
* It does not produce mathematical structure.
* Its text-generation decoder introduces a token-generation loop rather than one compact CTC result.
* It is much larger than the PP-OCRv6 small recognizer.
* Your current `lege-gpu` operator set is designed around convolutional vision models and simpler recognition graphs.

The operator list in `lege-gpu/src/vision/onnx/types.txt:21-108` does not expose several pieces likely needed for an efficient TrOCR runtime, such as a complete Transformer decoding path, reusable key-value state, generalized gather/indexing, and a purpose-built autoregressive scheduler.

Implementing TrOCR well would therefore be a separate inference-runtime project. It may still be valuable for:

* handwriting,
* historical scripts,
* a customer-specific difficult corpus,
* a low-confidence line fallback.

But it should have to prove a meaningful CER improvement on your held-out dataset before becoming part of the normal pipeline.

---

# PP-OCRv6 migration should be an experiment, not a blind upgrade

The agent’s first PP-OCRv6 task should be:

1. Download the official ONNX small detector and recognizer.
2. Produce a complete operator and shape report.
3. Identify unsupported or incorrectly lowered operations.
4. Compare raw outputs against Paddle or ONNX Runtime on fixed inputs.
5. Compare decoded output on a representative line corpus.
6. Only then optimize it natively in `lege-gpu`.

PP-OCRv6 changes the backbone and recognition architecture relative to v5, so existing kernels that happen to run v5 are not sufficient proof of v6 compatibility. Official model cards describe the new LCNetV4, RepLKFPN, and LightSVTR-based architecture. ([Hugging Face][2])

Also test mixed precision separately for OCR. The current embedded initializers are stored as FP16 but are expanded to FP32:

* `lege-gpu/src/vision/onnx/attrs.txt:98-103`
* `lege-gpu/src/vision/onnx/attrs.txt:139-153`

That costs memory and bandwidth. A mixed-precision OCR path may be worthwhile even if FP16 was rejected for a different layout model. Use FP16 weights and activations where safe, with FP32 reductions where needed, and measure CER rather than assuming equivalence.

Quantization should come later. Batching, synchronization, and CTC readback are more fundamental.

PaddleOCR’s repository is Apache-2.0 licensed, but a commercial distribution should still record the license and provenance of each model, dictionary, test dataset, and copied implementation component separately. ([GitHub][4])

---

# Structured output requires a document model, not a better text format

The question “what text format preserves tables?” has no single answer because **tables are not plain text**. You need a structured document object model internally, followed by several exporters.

Your current abstraction is too narrow:

```rust
pub struct OcrResult {
    pub hocr: String,
    pub plain_text: String,
}
```

* `lege-ocr/src/types.txt:3-9`

Worse, the engine API itself returns hOCR:

* `lege-ocr/src/engine.txt:7-33`

That mixes inference with one particular serialization. A recognizer should return geometry, tokens, confidence, and provenance. hOCR should be an exporter.

The current document model is explicitly an EPUB stub:

* `lege-ocr/src/document.txt:1`

It has useful block categories, but `TextBlock` can only hold lines and spacing:

* `lege-ocr/src/document.txt:37-62`

It cannot represent:

* table rows and columns,
* merged cells,
* formula source,
* MathML or LaTeX,
* nested sections,
* figures and captions,
* explicit reading-order relationships,
* polygons,
* repeated headers and footers,
* alternatives and correction history,
* coordinate transforms,
* model provenance.

The current slow path also assumes that one detected region is approximately one paragraph:

* `lege-ocr/src/types.txt:54-63`
* `lege-ocr/src/lib.txt:189-233`

That will not survive serious book and business-document reconstruction.

## Proposed canonical document IR

The central model should resemble:

```rust
struct Document {
    source: SourceIdentity,
    metadata: DocumentMetadata,
    pages: Vec<Page>,
    outline: Vec<OutlineNode>,
    processing: ProcessingManifest,
}

struct Page {
    index: u32,
    source_size: Size,
    page_size: Size,
    source_to_page: Transform,
    image: PageImageRef,
    regions: Vec<Region>,
    reading_order: Vec<RegionId>,
}

struct Region {
    id: RegionId,
    kind: RegionKind,
    polygon: Polygon,
    confidence: RegionConfidence,
    content: RegionContent,
    provenance: Provenance,
}

enum RegionContent {
    Text(TextBlock),
    Table(Table),
    Formula(Formula),
    Figure(Figure),
    Separator,
    Unknown,
}
```

A table must have explicit cells:

```rust
struct TableCell {
    row: u32,
    column: u32,
    row_span: u32,
    column_span: u32,
    polygon: Polygon,
    blocks: Vec<TextBlock>,
    is_header: bool,
    confidence: f32,
}
```

A formula should retain both the semantic result and the evidence:

```rust
struct Formula {
    latex: Option<String>,
    mathml: Option<String>,
    display: FormulaDisplay,
    source_crop: ImageRef,
    confidence: f32,
}
```

Text lines and words should include:

* raw recognized text,
* normalized text,
* polygon or bbox,
* recognition confidence,
* language,
* alternatives where useful,
* whether geometry was detector-derived or CTC-estimated.

That last distinction matters because your current word boxes are inferred from CTC timestep positions rather than independently localized words:

* `lege-gpu/src/vision/api.txt:703-758`

Persist this IR after each page or small page group. That gives you:

* crash recovery,
* resume,
* re-export without rerunning OCR,
* manual correction later,
* deterministic testing,
* search indexing,
* downstream API integration.

JSON, CBOR, or SQLite could all work. I would start with versioned Serde JSON for inspectability, then add a SQLite job/index layer around it.

---

# Outputs businesses are likely to expect

No one output should be treated as the universal result.

| Output              | Purpose                               | Layout/structure behavior                                                                                  |
| ------------------- | ------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Searchable PDF      | Default document result               | Preserves original visual page exactly; invisible text enables search/copy                                 |
| PDF/A               | Archival variant                      | Valuable for regulated or long-term repositories; requires real conformance work                           |
| DOCX                | Editable business document            | Real headings, paragraphs, tables, styles, and equations; visual layout is reconstructed rather than exact |
| HTML5 package       | Open structured document              | Strongest general semantic export for tables, headings, CSS layout, links, and MathML                      |
| Canonical JSON      | API, search, RAG, audit               | Preserves all geometry, structure, confidence, and provenance                                              |
| ALTO XML            | Libraries and archives                | Standard OCR text plus physical page layout                                                                |
| PAGE XML            | OCR research and advanced interchange | Rich regions, lines, words, glyphs, reading order, and dewarping information                               |
| XLSX/CSV sidecars   | Extracted tables                      | Useful companion output, not a representation of the complete book                                         |
| LaTeX project       | Scientific documents                  | Best for editable formulas and academic structure, but not a general office output                         |
| Plain text/Markdown | Convenience                           | Useful for indexing and inspection; insufficient as the canonical result                                   |
| RTF                 | Avoid                                 | Little reason to add it ahead of DOCX and HTML                                                             |

## Searchable PDF should be the first product output

This is the most defensible default for scanned books because it preserves the exact page appearance and adds searchability. That is also the core behavior of established local tools such as OCRmyPDF and NAPS2. ([OCRmyPDF][5])

You already have most of the relevant writer machinery:

* `lege-pdf/write/src/artifact.txt:149-180`
* `lege-pdf/write/src/text.txt:14-37`

That should be shipped before an ambitious reflowable-document exporter.

Start with normal searchable PDF. Treat PDF/A as a separate milestone with external conformance validation rather than merely changing metadata and claiming archival compliance.

## DOCX is not obsolete

DOCX remains the sensible editable business result. WordprocessingML has proper paragraph and character styles and proper row/column/cell table structures. Office Math provides structured equations, and current Microsoft 365 can import and export MathML. ([Microsoft Learn][6])

The DOCX exporter should aim for **semantic reconstruction**:

* document title,
* heading levels,
* paragraphs,
* lists,
* page breaks where meaningful,
* true tables,
* captions,
* footnotes,
* equations.

Do not try to reproduce every raster coordinate with floating text boxes. That produces an uneditable imitation of the PDF rather than a useful Word document.

## HTML should be the strongest open structured output

A packaged HTML export can preserve:

* heading hierarchy,
* paragraphs,
* semantic tables,
* inline and display formulas through MathML,
* page anchors,
* figures,
* captions,
* links back to source page coordinates,
* optional CSS approximating the source layout.

It is also easier to debug than DOCX and can serve as an intermediate validation representation for the DOCX exporter.

## ALTO and PAGE XML serve different specialist customers

ALTO is a standardized XML format for OCR text and page layout and is commonly associated with METS metadata in library and archival workflows. ([The Library of Congress][7])

PAGE XML represents regions, lines, words, glyphs, reading order, text content, and related document-image information. It is richer for OCR evaluation, correction tools, and layout research. ([GitHub][8])

They are not normal office-user outputs, but adding them over a good canonical IR should be straightforward and may materially increase the product’s usefulness to digitization projects.

---

# Tables and mathematics need specialist branches

Do not expect a text recognizer, whether Paddle or TrOCR, to recover these correctly from ordinary lines.

## Tables

The structured profile should:

1. Detect table regions.
2. Classify wired versus wireless tables if useful.
3. Run a table structure model.
4. Produce rows, columns, cells, and spans.
5. OCR text within cell geometry.
6. Export the result to HTML, DOCX, JSON, and optionally XLSX.

Paddle’s table structure modules are specifically intended to turn table images into editable structures such as HTML and expose lightweight SLANet variants as well as more expensive models. ([PaddlePaddle][9])

PP-StructureV3 is modular rather than one indivisible model. It combines layout analysis, text, tables, formulas, reading order, and formatting, and its modules can be run independently. That is a good conceptual source for your architecture even if you do not adopt the Python pipeline. ([PaddlePaddle][10])

## Mathematics

A formula detector should route formula regions to a formula recognizer. The output should contain:

* LaTeX,
* optionally MathML,
* display versus inline classification,
* source crop,
* confidence,
* page geometry.

Paddle’s formula module explicitly targets LaTeX or MathML output and offers smaller PP-FormulaNet variants as well as larger models. ([PaddlePaddle][11])

LaTeX should therefore be an output of a **scientific profile**, not the default representation for all documents.

Inline formulas remain harder than isolated display equations. They may require mixed text/formula segmentation within a line or a more expensive document model on selected pages.

---

# Processing profiles

I would expose three user-visible profiles rather than “fast” and “thorough.”

## Search

Goal: maximum throughput and searchable documents.

```text
direct image extraction or render
→ orientation/light cleanup
→ text detection
→ PP-OCRv6 small
→ selective retries
→ searchable PDF
→ plain text + JSON sidecar
```

No full document-layout model unless the page obviously requires reading-order analysis.

## Structured

Goal: editable and machine-readable documents.

```text
search pipeline
+ layout regions
+ heading/paragraph reconstruction
+ multi-column reading order
+ table recognition
+ repeated header/footer handling
→ DOCX + HTML + canonical JSON
```

## Scientific

Goal: tables, formulas, complex technical layouts.

```text
structured pipeline
+ formula-region recognition
+ inline-formula handling
+ more aggressive medium-model retries
+ optional document-VLM fallback
→ HTML/MathML + LaTeX project + DOCX + JSON
```

This profile will not normally match search-profile throughput, and that distinction should be explicit.

PaddleOCR-VL-1.6 is a 0.9-billion-parameter document VLM used with a layout model and is intended for text, formula, and table understanding. It is a possible optional complex-document backend, not a replacement for the lightweight path. ([PaddlePaddle][1])

The even newer HPD-Parsing release claims high document-parsing throughput, but its local setup currently depends on a customized vLLM runtime and CUDA 12.8 or later. That makes it a useful benchmark or optional NVIDIA sidecar, not a natural core backend for a small cross-platform `lege-gpu` application. ([GitHub][12])

---

# Crate boundaries

The current `lege-process` crate still describes and models the e-ink conversion product, while `lege-ocr` mixes:

* engine abstraction,
* platform engines,
* Paddle integration,
* segmentation,
* reading order,
* hOCR,
* document stubs.

I would not turn `lege-process` into the business OCR application. Keep the casual e-ink/viewer product and business OCR product as separate binaries over common libraries.

A reasonable eventual shape is:

```text
lege-docir
    Canonical structured document model and versioned serialization.
    No GPU, renderer, GUI, or exporter dependencies.

lege-preprocess
    Page quality analysis, grayscale normalization, deskew/dewarp,
    crop transforms, scan-specific cleanup.

lege-ocr
    Backend-neutral detection/recognition traits, batching types,
    confidence calculations, retry policy.

lege-ocr-paddle
    PP-OCRv5/v6 model adapters, detector postprocessing,
    recognition dictionaries, CTC integration.

lege-layout
    Region classification, reading order, paragraph reconstruction,
    headings, repeated headers/footers, table/formula routing.

lege-export
    Searchable PDF, DOCX, HTML, JSON, ALTO, PAGE, LaTeX,
    table XLSX/CSV companions.

lege-batch
    Persistent job queue, checkpoints, input discovery,
    output policy, error isolation, progress events.

lege-ocr-cli
    Thin CLI binary over lege-batch.

lege-ocr-app
    Later minimal native GUI over the same batch API.
```

Existing crates remain:

```text
lege-gpu
    Shared WGPU inference runtime.

lege-pdf
    Parsing, direct scan extraction, rendering, and writing.

lege-process
    E-ink conversion product; consumes OCR when requested.

lege-viewer
    Casual integrated renderer/processor/viewer.
```

Do not perform this entire split before measuring the recognizer. The first useful extraction should be:

1. `lege-docir`
2. `lege-batch`
3. `lege-export`

Then adjust the Paddle backend boundary when the batched API is known.

The new backend trait should accept batches and return typed inference output. It should not return hOCR:

```rust
trait TextRecognizer {
    fn recognize_batch(
        &self,
        batch: RecognitionBatch<'_>,
    ) -> Result<Vec<RecognizedLine>>;
}
```

hOCR becomes just one `lege-export` implementation.

---

# CLI and job behavior

The first product form should be a CLI resembling:

```bash
lege-ocr batch /incoming \
  --recursive \
  --profile search \
  --format searchable-pdf,json,text \
  --output /processed \
  --resume \
  --device auto \
  --language auto \
  --on-error continue
```

For a business batch tool, the important behavior is not command-line sophistication. It is operational reliability:

* retain one process and one loaded GPU model across all books,
* interleave pages from multiple books to fill recognition batches,
* checkpoint every completed page,
* resume after termination,
* use source hashes and processing-profile hashes,
* skip outputs already generated with the same profile,
* write outputs atomically,
* isolate failures to one page or one document,
* create a low-confidence review report,
* record model/version/configuration provenance,
* provide deterministic names and collision handling,
* expose machine-readable progress,
* support cancellation without corrupting completed work.

A useful output directory might be:

```text
book-name/
    book-name.searchable.pdf
    book-name.docx
    book-name.html/
    book-name.lege.json
    book-name.txt
    tables/
        table-0001.xlsx
        table-0002.csv
    assets/
        figures/
        formulas/
    qa.json
    processing-manifest.json
```

The first GUI should only expose:

* files/folders,
* profile,
* output formats,
* destination,
* queue,
* progress,
* warnings/failures,
* open-output actions.

A document-library organizer is a separate application concern and should not delay the batch engine.

---

# Is one or two minutes per book realistic?

It depends on page count, page complexity, hardware, and profile.

|   Book size | Required rate for two minutes |
| ----------: | ----------------------------: |
|   300 pages |                   2.5 pages/s |
|   500 pages |                  4.17 pages/s |
| 1,000 pages |                  8.33 pages/s |

At 40 text lines per page, a 500-page book requires approximately **167 recognized lines per second**, plus detection, preprocessing, decoding, structure, and output.

Your earlier layout benchmark around 2.38 pages/s would already take roughly 210 seconds for 500 pages before text recognition. Therefore:

* the **search profile must avoid the full layout model**;
* the structured profile must be marketed with a separate rate;
* batching across books is essential;
* the product claim should be pages per minute on specified hardware, not “a book” without qualification.

A defensible performance statement would eventually look like:

```text
250 pages/minute on RTX <model>,
search profile,
clean 300-DPI English book scans,
warm model,
searchable-PDF output.
```

That is measurable. “Any scanned book in one minute” is not.

---

# Benchmark plan for the agent

The agent should not begin by replacing models. The first assignment should produce a performance and quality Pareto report.

## Corpus

Use representative subsets of:

* clean modern book scans,
* skewed or warped scans,
* faded and bleed-through pages,
* two-column academic pages,
* tables,
* mathematical notation,
* mixed-language pages,
* a small handwriting set if TrOCR is being considered.

Ground truth does not need to cover every page. Carefully transcribed representative lines and regions are enough to compare iterations.

## Measurements

Record:

* pages per second,
* recognized lines per second,
* detector time,
* crop/preprocess time,
* recognizer GPU time,
* submission/wait time,
* readback bytes,
* CPU CTC time,
* output-writing time,
* GPU utilization,
* VRAM peak,
* queue occupancy,
* cold versus warm startup,
* p50 and p95 page latency,
* CER and WER,
* detector recall/Hmean,
* reading-order errors,
* table structure score such as TEDS or GriTS,
* normalized formula exact match.

## Experiment order

### A. Establish current PP-OCRv5 baseline

Measure the current fast and thorough paths without architectural changes.

### B. Recognition batching

Test:

```text
B = 1, 4, 8, 16, 32
```

Run the same line set, same preprocessing, same decoder.

### C. Compact GPU CTC output

Compare full logits readback against top-ID/top-score readback.

### D. Session pool

Compare one graph, two sibling graphs, and four sibling graphs after batching exists.

### E. Low-resolution detection, high-resolution recognition

Sweep detector long side and source DPI independently:

```text
detector: 960 / 1280 / 1536
recognition source: 200 / 300 / 400 DPI equivalent
```

### F. PP-OCRv6 import

Run model/operator reports, numerical parity, then v5 versus v6 small versus v6 medium.

### G. Preprocessing ablation

Compare original grayscale, background normalization, deskew, dewarp, adaptive thresholding, and combinations. Do not assume the most processed image is best.

### H. Selective retry policy

Measure how many lines are retried, quality recovered, and time added.

### I. Structure models

Evaluate tables and formulas only after the fast text path is stable.

The migration gate should be strict:

* no v6 default until native output matches an authoritative runtime,
* no TrOCR integration until it demonstrates a significant difficult-line advantage,
* no “quality” profile without a held-out accuracy corpus,
* no “book in two minutes” claim without a stated page count, profile, and GPU.

---

# Product positioning

Fully local OCR already exists. OCRmyPDF and NAPS2 provide local searchable-PDF workflows, while ABBYY sells high-volume conversion to PDF, PDF/A, Word, and other formats. Enterprise OCR systems also expose raw structured output such as XML or JSON. ([OCRmyPDF][5])

Therefore, “local OCR without a subscription” is useful but not enough by itself. The stronger differentiation is:

* unusually high GPU throughput,
* no upload and no per-page fee,
* cross-platform native deployment,
* robust batch and watched-folder operation,
* searchable PDF plus real structured exports,
* tables and mathematics only when required,
* resumability and page-level failure isolation,
* confidence and review reports,
* exact source-coordinate provenance,
* stable CLI/API integration,
* re-export without rerunning OCR.

The renderer is part of that advantage, but the business product should be sold as a **local document-conversion pipeline**, not merely a fast PDF renderer with OCR attached.

# Final direction

The strongest first version is:

```text
Raster PDF
→ direct embedded-image extraction where possible
→ low-resolution Paddle detection
→ high-resolution line crops
→ batched PP-OCRv6-small recognition
→ GPU CTC reduction
→ confidence-based medium-model retry
→ canonical document IR
→ searchable PDF + JSON + text
```

Then add:

```text
structured layout → DOCX + HTML + table exports
```

and finally:

```text
formula routing → LaTeX/MathML scientific output
```

The existing Paddle architecture is not yet optimized enough to justify abandoning it. Fixing batching, synchronization, recognition-source resolution, and CTC readback is much more likely to produce the required throughput than replacing it with TrOCR.

[1]: https://paddlepaddle.github.io/PaddleOCR/main/en/index.html "https://paddlepaddle.github.io/PaddleOCR/main/en/index.html"
[2]: https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec "https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec"
[3]: https://github.com/microsoft/unilm/blob/master/trocr/README.md "https://github.com/microsoft/unilm/blob/master/trocr/README.md"
[4]: https://github.com/PaddlePaddle/PaddleOCR/blob/main/LICENSE "https://github.com/PaddlePaddle/PaddleOCR/blob/main/LICENSE"
[5]: https://ocrmypdf.readthedocs.io/ "https://ocrmypdf.readthedocs.io/"
[6]: https://learn.microsoft.com/en-us/office/open-xml/word/working-with-wordprocessingml-tables "https://learn.microsoft.com/en-us/office/open-xml/word/working-with-wordprocessingml-tables"
[7]: https://www.loc.gov/standards/alto/description.html "https://www.loc.gov/standards/alto/description.html"
[8]: https://github.com/PRImA-Research-Lab/PAGE-XML "https://github.com/PRImA-Research-Lab/PAGE-XML"
[9]: https://paddlepaddle.github.io/PaddleOCR/main/en/version3.x/module_usage/table_structure_recognition.html "https://paddlepaddle.github.io/PaddleOCR/main/en/version3.x/module_usage/table_structure_recognition.html"
[10]: https://paddlepaddle.github.io/PaddleOCR/main/en/version3.x/pipeline_usage/PP-StructureV3.html "https://paddlepaddle.github.io/PaddleOCR/main/en/version3.x/pipeline_usage/PP-StructureV3.html"
[11]: https://paddlepaddle.github.io/PaddleOCR/main/en/version3.x/module_usage/formula_recognition.html "https://paddlepaddle.github.io/PaddleOCR/main/en/version3.x/module_usage/formula_recognition.html"
[12]: https://github.com/PADDLEPADDLE/PADDLEOCR "https://github.com/PADDLEPADDLE/PADDLEOCR"
