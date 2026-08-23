# Detector `limit_side` sweep on the wgpu path (2026-08-22)

Why `det_limit_side` in `lege-gpu/src/vision/api.rs` returns 1280 for PP-OCRv6
and 960 for v5.

Unlike the sibling `recipe-2026-08-20/` fixture, this one measures the **wgpu**
path with the embedded PP-OCRv6 `small` weights, and its reference is a
hand transcription of the page rather than a second extraction pipeline — so the
error rates here are far lower and are not comparable with that fixture's.

## Files

| File | What produced it |
|---|---|
| `reference.txt` | Hand transcription of `lege-process/page_0002-original.png` (961x1400) |
| `ppocrv6-wgpu-limit960.txt` | Embedded v6 on wgpu, long side capped at 960 |
| `ppocrv6-wgpu-limit1280.txt` | Same, capped at 1280 — the shipped value |
| `ppocrv6-wgpu-limit1600.txt` | Same, capped at 1600 — above the page's own 1400, so no resize at all |

## Result

```
reference: 1596 characters, 249 words
limit-960:  CER 0.25%, WER 2.81%   7.28s
limit-1280: CER 0.25%, WER 2.41%   8.31s
limit-1600: CER 0.31%, WER 3.21%   8.23s
```

1280 wins on WER and ties on CER, so that is what ships. Note that 1600 leaves
the page unresized and scores **worst**: the downscale appears to suppress scan
noise, so more pixels is not automatically better.

## Read this before trusting it

The margin is about one word on a 249-word page. Two specific limits:

- **One page, one layout, one language.** Not enough to tune against.
- **The page's long side is only 1400 px**, so 960 vs 1280 is a 0.69x vs 0.91x
  downscale. A 300 DPI scan is roughly 3300 px, where the same two caps mean
  0.29x vs 0.39x — a far larger difference this fixture cannot speak to. If the
  cap is ever revisited, measure it on genuine 300 DPI input.

The residual errors are also mostly not a resolution problem. At every cap the
same lost inter-word spaces appear — `strangeforetaste`, `bea` — on the lines
crossing the vertical shading boundary in this scan, where detection merges
adjacent boxes. Fixing those means detection or box-splitting work, not more
pixels.

## Reproducing

```sh
LEGE_OCR_PROBE_MODELS=$PWD/lege-ocr/assets \
LEGE_OCR_PROBE_PAGE=$PWD/lege-process/page_0002-original.png \
LEGE_OCR_PROBE_OUT=/tmp/probe.txt \
  cargo test -p lege-ocr --features paddle-ocr --test model_generation_probe \
    probe_models_read_a_page -- --nocapture

python3 lege-document-ocr/scripts/score_ocr_accuracy.py \
    --reference lege-document-ocr/scripts/benchmarks/book-page-wgpu-2026-08-22/reference.txt \
    --candidate probe=/tmp/probe.txt
```

Needs a real GPU adapter. To re-sweep, change the v6 arm of `det_limit_side` and
rebuild between runs.
