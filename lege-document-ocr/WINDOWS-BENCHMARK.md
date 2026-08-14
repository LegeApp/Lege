# Windows OCR benchmark — 2026-08-14

This is a first-pass local measurement, not a release quality gate. It records
the relative behavior of the current Windows Runtime and embedded PP-OCRv5/WGPU
backends on one printed English book.

## Host and corpus

- Windows 11 Pro 10.0.26200
- Intel Core i7-13700H, 14 cores / 20 logical processors, 31.7 GiB RAM
- NVIDIA GeForce RTX 4060 Laptop GPU, 8 GiB, driver 610.88
- Rust 1.97.1, optimized `release` build
- Source: `D:\to-sort\socialjusticeini00qutb_1.pdf`
- Full sample: source pages 21–220, exactly 200 pages
- Pilot: the first 10 pages of that sample
- Search profile, raw text, spellcheck disabled, JSON and text exports

The source PDF has an existing OCR text layer. Runs used `--force-ocr` so Lege
actually recognized the page images. Accuracy is an edit-distance proxy against
that existing text layer after Unicode normalization, case folding,
dehyphenation, and punctuation/whitespace normalization. The reference is not
manually corrected ground truth, so the figures are useful for relative
comparison only.

## Results

| Run | Pages | Time | Pages/s | CER proxy | WER proxy |
|---|---:|---:|---:|---:|---:|
| Windows OCR, 40 MP cap, one document/worker | 10 | 14.12 s | 0.708 | 1.60% | 5.71% |
| Windows OCR, 12 MP cap, one document/worker | 10 | 7.06 s | 1.418 | 2.82% | 6.86% |
| Paddle/WGPU, 40 MP cap, one document/worker | 10 | 32.45 s | 0.308 | 1.54% | 5.68% |
| Paddle/WGPU, 12 MP cap, one document/worker | 10 | 28.54 s | 0.350 | 1.65% | 6.33% |
| Windows OCR, 12 MP, one 200-page document | 200 | 184.15 s | 1.086 | 1.14% | 4.49% |
| Windows OCR, 12 MP, 4 × 50-page inputs, 4 workers | 200 | 42.16 s | 4.744 | 1.14% | 4.49% |
| Windows OCR, 12 MP, 8 × 25-page inputs, 8 workers | 200 | 49.33 s | 4.054 | — | — |
| Paddle/WGPU, 12 MP, 4 × 50-page inputs, 4 workers | 200 | 727.01 s | 0.275 | 1.11% | 5.88% |

The Windows single-document and four-worker runs produced identical normalized
text on all 200 pages. Five outputs were empty in both backends: three reference
pages were blank, and the other two reference strings contained only `i` and
`imk i j`, so there was no meaningful missed content signal.

### TensorRT integration follow-up

The native PP-OCRv6-tiny/TensorRT worker was integrated after the first-pass
table above. A like-for-like release run on the same 10-page, 12 MP sample took
5.030 s end to end (1.99 pages/s), including cached engine startup, PDF render,
OCR, and JSON/text export. A WinOCR fallback run immediately afterward took
9.961 s (1.00 pages/s), so TensorRT delivered 1.98x the end-to-end throughput in
that paired run. A warmed repeat completed in 2.786 s; use the 5.030 s figure as
the more conservative result.

A page-local NFKC/case/dehyphenation/punctuation/whitespace edit-distance proxy
against the embedded text layer measured 1.90% CER / 9.14% WER for TensorRT and
3.06% CER / 9.46% WER for WinOCR. This follow-up normalization is not identical
to the earlier table's script, so compare the paired follow-up values to one
another rather than combining them with the earlier absolute values.

## Interpretation

In the original CPU-versus-WGPU pass, Windows Runtime OCR was the correct
throughput choice: with four active documents it was 17.3 times faster than
Paddle/WGPU at the same 12 MP raster cap. The later TensorRT integration is now
the preferred Windows auto candidate because it was 1.98 times faster than
WinOCR in the paired single-document release run. Auto retains WinOCR as the
startup-only fallback. This does not establish a universal accuracy ordering;
a manually transcribed, layout-diverse corpus is still required.

The 12 MP cap is a real quality/performance control. On the pilot it doubled
Windows throughput relative to 40 MP, at a 1.22-point CER cost. Paddle gained
only 14% from the lower cap. Use 12 MP for high-throughput search ingestion and
retain 40 MP for a quality-oriented pass until a broader corpus sets a better
threshold.

Four document workers were best on this 20-thread host. Eight workers were 15%
slower than four. The CLI currently parallelizes documents, not pages within one
document; the 4.37× win required splitting the 200-page sample into four inputs.

## First-pass speed work indicated by the measurement

1. Add bounded intra-document page scheduling so one large PDF can obtain the
   four-worker result without external PDF splitting. Preserve ordered DocIR
   assembly and per-page checkpoints.
2. Broaden the direct-scan intake path for dominant-image pages with hidden text
   layers. Every measured page was classified as `rendered`, so the pipeline
   decoded and resampled a 12–40 MP raster instead of using the source scan. Any
   broader gate must still reject visible overlays, masks, clipping, and complex
   transforms.
3. Keep Paddle at one producer until its detector and recognition graph caches
   use sibling execution sessions. The current cache mutexes cover GPU execution;
   four document workers reduced rather than increased Paddle throughput.
4. Add phase telemetry for intake/decode, render, detection, recognition,
   retry, and export. Current end-to-end timing proves the priority order but
   cannot attribute the remaining cost precisely.
5. Extend the PP-OCRv6 TensorRT comparison beyond the tiny English model and
   this embedded-text proxy before selecting the final production tier.

## Reproduction command

After preparing equivalent 50-page inputs in `D:\benchmark\shards`:

```powershell
cargo build -p lege-document-ocr-cli --release

.\target\release\lege-ocr.exe batch D:\benchmark\shards `
  --recursive `
  --output D:\benchmark\out `
  --profile search `
  --backend winocr-legacy `
  --language eng `
  --format json,text `
  --text-view raw `
  --force-ocr `
  --max-page-pixels 12000000 `
  --workers 4 `
  --no-spellcheck `
  --on-error stop `
  --json-progress
```

Omit `--force-ocr` in production. It exists to evaluate OCR against an embedded
reference layer; normal runs should preserve trustworthy native text.
