# Recipe-PDF OCR accuracy fixture (2026-08-20)

The transcripts behind `@lege-ecosystem.evidence.ocr-v5-v6-recipe-20260820`, the
measurement that first showed PP-OCRv6 beating the embedded PP-OCRv5 engine.

They lived in `.agent/scratch/` until 2026-08-22, which meant a verified ledger
claim rested on files that scratch pruning is meant to delete. They are here now
so the numbers can be re-derived from the repository alone.

## Files

| File | What produced it |
|---|---|
| `reference.txt` | The pipeline's own native text extraction, **not** forced OCR — the ground truth |
| `ppocrv5-wgpu.txt` | Embedded PP-OCRv5 over the lege-gpu wgpu runtime |
| `ppocrv6-trt.txt` | PP-OCRv6-tiny over the TurboOCR TensorRT worker |

Source document: a two-page NYT Cooking recipe PDF, rendered at 300 DPI and
capped at 12 MP, with OCR forced for the two candidates. The PDF itself is not
redistributable and is not in the repository; that is why the transcripts are
kept rather than the input.

## Reproducing the recorded numbers

```sh
python3 lege-document-ocr/scripts/score_ocr_accuracy.py \
    --reference lege-document-ocr/scripts/benchmarks/recipe-2026-08-20/reference.txt \
    --candidate v5=lege-document-ocr/scripts/benchmarks/recipe-2026-08-20/ppocrv5-wgpu.txt \
    --candidate v6=lege-document-ocr/scripts/benchmarks/recipe-2026-08-20/ppocrv6-trt.txt
```

Expected, and what the ledger records:

```
reference: 2092 characters, 381 words
v5: 2092 characters, 381 words, CER 17.88%, WER 19.16%
v6: 2088 characters, 377 words, CER 12.00%, WER 14.17%
```

Treat these as frozen. Editing a transcript silently rewrites the evidence for a
recorded claim; a new measurement belongs in a new dated directory beside this
one.

## Scope

Two pages of one document, one language, one layout. It was enough to justify
moving to v6 and is not enough to tune thresholds against — in particular, the
v6 column here is the **TensorRT** path, so it says nothing about how the
now-embedded PP-OCRv6 weights score on the wgpu runtime.
