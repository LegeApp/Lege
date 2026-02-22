# OCR + DJVU Findings (Linux Path) - 2026-02

## Summary
Recent fixes confirmed that DJVU hidden text now emits selectable OCR content instead of raw hOCR markup. The next quality gap is OCR character substitution (for example `Furopean` vs `European`, `genceations` vs `generations`) caused by input quality + OCR model limits, not by the text-layer container itself.

This document captures:
- what was fixed,
- why the issue happened,
- what to improve next release,
- a practical rollout plan with low regression risk.

## What Was Fixed

### 1. DJVU hidden-text extraction from hOCR
Problem: hOCR parsing for DJVU word extraction was too strict and could mis-handle real-world hOCR spans/attributes.

Fix location: `src/djvu.rs`
- `parse_hocr_to_words(...)` was updated to:
  - accept mixed quotes/newlines and flexible attribute ordering,
  - parse `title='bbox ...; x_wconf ...'` payloads,
  - strip nested tags from text,
  - decode common HTML entities,
  - clamp coordinates to page bounds before encoding.

Representative pattern now used:
```rust
let word_re = Regex::new(
    r#"(?is)<span[^>]*class=['\"][^'\"]*\bocrx_word\b[^'\"]*['\"][^>]*title=['\"]([^'\"]*)['\"][^>]*>(.*?)</span>"#
)?;
```

Result: pasted output from DJVU now contains recognized text content instead of HTML payload blocks.

### 2. CLI readability and duplicate status lines
Fix locations: `src/main.rs`
- Interactive prompt/info/highlight color set to terminal default (empty ANSI prefix), improving readability on light terminal themes.
- Duplicate non-stage status triplets are suppressed to avoid repeated lines such as duplicated initialization blocks.

### 3. Linux OCR throughput guardrails
Fix locations:
- `src/ocr/ocr.rs`
- `src/pipeline/pdf_tokio_pipeline.rs`
- `src/pipeline/djvu_pipeline.rs`

Changes:
- OCR semaphore is adaptive on Linux (`available_parallelism().clamp(2, 8)`).
- Added `should_use_region_ocr(...)` to avoid pathological fan-out of many tiny region OCR calls.
- PDF and DJVU pipelines now use this heuristic before choosing region OCR vs tiled/full-page OCR.

## Why OCR Accuracy Is Still Imperfect
The remaining errors are mostly OCR recognition errors, not container/encoding errors.

Primary contributors:
1. Binarization damage before OCR
- Thin serifs, ligatures, punctuation and tight kerning can degrade with aggressive thresholding.
- Typical confusion pairs: `f/e`, `rn/m`, `l/i`, `cl/d`, `c/e`.

2. OCR input scale
- Current Tesseract path in `src/ocr/tesseract.rs` has a conservative **downscale** path for very large images.
- There is no dedicated **upsample-for-OCR** path for low x-height text.

3. Region OCR overhead/noise
- Many tiny regions can reduce effective OCR quality and throughput compared with coherent strips/pages.

## Recommended Next Release Plan

### Phase 1 (high ROI, low risk): OCR-only input preprocessing
Goal: improve recognition without changing output rendering pipeline.

Proposed additions in `src/ocr/tesseract.rs`:
- optional OCR-only upscale for low-height text lines/pages,
- optional OCR on grayscale pre-binarization signal (where available),
- retain current binarized image for visual page encoding.

Pseudo-example:
```rust
if estimated_x_height < 14 {
    // OCR-only upscale, preserve final document image pipeline unchanged
    ocr_input = upscale_lanczos(ocr_input, 1.25);
}
```

### Phase 2: confidence-gated correction
Use hOCR confidence (`x_wconf`) to constrain corrections:
- only attempt corrections below configurable threshold,
- prioritize single-edit confusions,
- avoid high-confidence token rewrites.

Pseudo-example:
```rust
if word_conf < 70 {
    candidate = confusion_map_rewrite(word);
    if dictionary.contains(candidate) {
        use candidate;
    }
}
```

### Phase 3 (optional): opt-in SymSpell mode
- Add `--ocr-correct` (or config key) as explicit user choice.
- Keep default conservative to avoid proper-noun/archaic-word overcorrection.

## Suggested User-Facing Controls
- `ocr_preprocess_mode = conservative | quality`
- `ocr_region_strategy = auto | region | tiled | full_page`
- `ocr_correction = off | conservative | aggressive`
- `ocr_confidence_threshold = <int>`

## Validation Strategy
1. Build a small corpus (good print, degraded print, historical scans).
2. Measure CER/WER before and after each phase.
3. Verify no regression in:
- output file validity (Okular/djvulibre open + select),
- throughput on Linux WebGPU path,
- PDF text-layer behavior.

## Key Code References
- DJVU hOCR parsing: `src/djvu.rs`
- Tesseract OCR path: `src/ocr/tesseract.rs`
- OCR orchestration + heuristics: `src/ocr/ocr.rs`
- PDF OCR callsite: `src/pipeline/pdf_tokio_pipeline.rs`
- DJVU OCR callsite: `src/pipeline/djvu_pipeline.rs`
- CLI progress/formatting: `src/main.rs`

## Conclusion
The pipeline is now functionally correct for DJVU selectable OCR text. The remaining gap is OCR recognition quality. The safest next release path is:
1. improve OCR input quality,
2. add confidence-gated corrections,
3. keep aggressive spell-correction opt-in.

