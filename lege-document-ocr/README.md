# Lege Document OCR

`lege-ocr` is the batch-oriented PDF OCR and document-conversion product in the
Lege workspace. It is separate from the e-ink conversion application and reuses
the Lege PDF renderer/writer and OCR runtime through typed APIs.

## Current pipeline

The CLI fingerprints each source and configuration, classifies every page from
one semantic PDF compilation, keeps trustworthy native text, and renders pages
that require OCR at a bounded 300-DPI-equivalent resolution. Paddle uses a
low-resolution detector with high-resolution recognition crops and batches the
line recognizer. Low-confidence lines receive one bounded alternate-contrast
retry. Every newly completed page is atomically checkpointed in versioned DocIR;
SQLite WAL state makes `--resume` recover only missing or invalid pages.

Batch workers process documents concurrently while a shared bounded scheduler
coalesces recognition crops across documents by line count and pixel budget.
The recognition head performs top-two reduction on the GPU and reads back one
compact record per timestep instead of the complete class-logit tensor.

Raw recognition is never overwritten. Optional conservative English correction
only applies decisive, frequency-supported missing-space repairs; riskier
edit-distance spelling changes require an explicit opt-in. Proper nouns,
uppercase tokens, exact dictionary words, and an API allowlist are protected.

## Build and run

```bash
cargo build -p lege-document-ocr-cli --release

target/release/lege-ocr batch /incoming \
  --recursive \
  --output /processed \
  --profile search \
  --format searchable-pdf,json,text,html,docx \
  --pdf-mode rasterize \
  --workers 4 \
  --gpu-batch-lines 64 \
  --resume \
  --on-error continue
```

PowerShell uses the same executable with Windows paths:

```powershell
cargo build -p lege-document-ocr-cli --release

.\target\release\lege-ocr.exe batch D:\incoming `
  --recursive `
  --output D:\processed `
  --profile search `
  --backend auto `
  --format json,text `
  --workers 4 `
  --resume `
  --no-spellcheck
```

For controlled speed/accuracy evaluation, `--force-ocr` deliberately ignores a
trustworthy embedded text layer so that it can be used as a reference transcript.
`--render-dpi` (72–600, default 300) exposes the main search-profile
speed/accuracy tradeoff. These switches are not needed for ordinary production
runs, where native text should be preserved.

See [WINDOWS-BENCHMARK.md](WINDOWS-BENCHMARK.md) for the measured Windows
backend, raster-size, and worker-count tradeoffs on a 200-page scanned-book
sample.

On Windows, `--backend auto` discovers the native PP-OCRv6/TensorRT worker and
runs a real detector/recognizer inference probe before any document starts. If
that startup probe succeeds, the complete batch uses `tensorrt-paddle`. If no
runtime is found or the probe fails, the complete batch uses Windows Runtime
OCR. The selected backend is included in the resumable configuration hash and
printed before processing begins. A runtime failure after processing starts
fails the affected job; it never switches that job to a different OCR engine.

Use `--backend tensorrt-paddle` to test or require the GPU path. This selection
fails closed instead of falling back. `--backend winocr-legacy` selects the CPU
fallback explicitly. The future Windows AI/NPU adapter has a reserved
`windows-ai` selection, but unsupported builds reject it. On Linux,
`--backend paddle` selects the Paddle/Lege WGPU backend.

The Windows TensorRT test script builds the worker, performs an actual CUDA
inference preflight, builds the release CLI, and runs a fail-closed PDF job:

```powershell
git clone https://github.com/aiptimizer/TurboOCR.git `
  .\lege-document-ocr\turboocr

.\lege-document-ocr\scripts\test_windows_tensorrt.ps1 `
  -Pdf .\.agent\scratch\ocr-benchmark\social-justice-pages-0021-0030.pdf
```

Add `-SkipBuild` after the binaries exist. The default development paths are
`D:\TensorRT`, `D:\cuda`, and the vcpkg OpenCV installation; override the
corresponding parameters for another layout. OpenCV is used only for CPU image
decode/conversion in this worker, so a CUDA-enabled OpenCV build is neither
required nor used for inference. See
[`turboocr/docs/build/windows.md`](turboocr/docs/build/windows.md) for cold-cache
behavior and lower-level probe commands.

The embedded Paddle assets are the compatibility PP-OCRv5 models. A v6 or
customer model can be installed with `--model-pack DIRECTORY`; assets are not
accepted unless their BLAKE3 checksums match the pack manifest.

## Outputs

Implemented exporters are searchable PDF, canonical JSON, UTF-8 text, packaged
HTML, DOCX, Markdown, ALTO XML, PAGE XML, hOCR, LaTeX, CSV table sidecar, and
XLSX table sidecar. JSON export also writes a processing manifest and a
low-confidence/correction QA report. PDF/A is deliberately rejected because the
current PDF writer does not yet produce externally validated PDF/A.

Searchable PDF has an explicit visual-fidelity policy:

- `--pdf-mode preserve` byte-copies a source whose pages already contain
  trustworthy native text. It rejects scanned pages until a true incremental
  source-object overlay writer exists.
- `--pdf-mode rasterize` opts into rebuilding visual pages as high-quality JPEG
  plus an invisible positioned text layer.

## Model-pack manifest

`manifest.json` schema version 1:

```json
{
  "schema_version": 1,
  "provider": "PaddlePaddle",
  "name": "PP-OCRv6-small",
  "version": "vendor-export-version",
  "generation": "v6",
  "license": "Apache-2.0",
  "source": "https://github.com/PaddlePaddle/PaddleOCR",
  "detector": { "path": "det.onnx", "blake3": "..." },
  "recognizer": { "path": "rec.onnx", "blake3": "..." },
  "dictionary": { "path": "dict.txt", "blake3": "..." },
  "layout": { "path": "doclayout.onnx", "blake3": "..." },
  "table": {
    "name": "table-structure-model",
    "model": { "path": "table.onnx", "blake3": "..." },
    "dictionary": { "path": "table-tokens.txt", "blake3": "..." },
    "input_width": 488,
    "input_height": 488,
    "blank_index": 0
  },
  "formula": {
    "name": "formula-to-latex-model",
    "model": { "path": "formula.onnx", "blake3": "..." },
    "dictionary": { "path": "latex-tokens.txt", "blake3": "..." },
    "input_width": 384,
    "input_height": 384,
    "end_token": "</s>"
  },
  "languages": ["eng"]
}
```

The manifest is provenance, not proof of runtime compatibility. A new graph
must still pass numerical parity and corpus accuracy gates before becoming the
default.

## Known production gates

The Windows AI NPU adapter remains deferred until its supported hardware and
deployment requirements are selected; Windows Runtime OCR remains the supported
Windows path. The existing performance corpus will be wired into automated
quality/performance gates once its location and provenance are linked.

Model-pack-driven PP-DocLayout, table-structure, and formula-to-LaTeX adapters
are implemented, and specialist output retains underlying OCR evidence in
DocIR. No specialist weights are bundled: a pack must provide checksum-pinned,
runtime-compatible prepared ONNX graphs and token dictionaries. Table and
formula graphs currently use the generic fixed-shape token contract: one
`[1, sequence, classes]` output decoded with compact GPU top-two readback. The
table adapter reconstructs the row/column spans described by the structure
tokens and assigns the retained OCR blocks to those cells; models that expose
separate cell-coordinate tensors need a dedicated adapter before use. True
incremental text overlay for scanned source PDFs and externally validated PDF/A
output remain open gates; use explicit rasterization for searchable scans.

## Correction dictionary

Pass `--dictionary words.txt`, where each non-comment row is `word frequency`;
spaces or tabs are accepted, as is a UTF-8 BOM. The dictionary file is included
in the configuration hash so changing it creates a distinct resumable job. Use
only a dictionary whose license permits commercial redistribution.

The tested English dictionary is vendored at
`lege-document-ocr/third_party/symspell/frequency_dictionary_en_82_765.txt`.
Its SymSpell, Google Books Ngram, and SCOWL attribution is retained in the
adjacent `NOTICE.md` and `LICENSE-SYMSPELL.txt` files. From the workspace root:

```powershell
.\target\release\lege-ocr.exe batch D:\incoming `
  --output D:\processed `
  --dictionary .\lege-document-ocr\third_party\symspell\frequency_dictionary_en_82_765.txt
```

Conservative correction inserts one missing space only when both resulting
words are known and the frequency-weighted split is decisive. Exact dictionary
words, proper-name-like title case, acronyms, plausible derived words, and
ambiguous splits remain unchanged. Raw OCR and correction provenance are always
retained. Edit-distance spelling candidates are disabled by default and are
only indexed and applied with `--apply-spelling-edits`, because isolated OCR
line fragments make those edits materially less precise. Avoiding that large
delete index also keeps conservative startup and memory costs lower.
`--no-spellcheck` disables the entire stage; without a configured dictionary
the pipeline records a warning and preserves recognition unchanged.
