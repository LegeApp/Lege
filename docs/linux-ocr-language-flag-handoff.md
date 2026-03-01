# Linux OCR Language Flag Handoff

Date: 2026-03-01

## What was implemented

A hidden/advanced direct CLI flag was added for Linux OCR language selection:

- `--language <tesseract_code>` in direct CLI mode only (not interactive prompts).
- Low-visibility help mention under an `ADVANCED` section in `--help` output.
- Language code normalization/validation: lowercase, allowed charset `[a-z0-9_]`.
- Default language remains `eng` when the flag is not provided.

## Main behavior

- Example: `lege book.pdf 1-10 1440x1920 --language jpn`
- Lege now requests `jpn.traineddata` (or `<code>.traineddata`) and fails fast if missing.
- OCR output handling remains language-agnostic; Lege consumes Tesseract output as before.
- On non-Linux builds, using `--language` returns an explicit error.

## Files changed

- `src/main.rs`
  - Parses/removes hidden `--language` from direct CLI args.
  - Keeps flag out of interactive flow.
  - Passes language override into simple processing config.
- `src/cli_text.json`
  - Adds minimal `ADVANCED` help mention for `--language`.
- `src/pipeline/config.rs`
  - Adds `ocr_language: String` to `PipelineConfig`.
  - Adds `ocr_language()` getter and `set_ocr_language()` setter.
  - Adds validation for language code format.
- `src/ocr/mod.rs`
  - Extends `run_ocr(...)` to accept `language`.
  - Adds language-aware availability check:
    - `check_tesseract_availability_for_language(lang)`
  - Adds language-aware tessdata path resolution:
    - `get_tessdata_path_for_language(lang)`
  - Adds language data search through:
    - `TESSDATA_PREFIX`
    - executable dir (+ `tessdata/`)
    - cwd (+ `tessdata/`)
    - standard system tessdata dirs
- `src/ocr/tesseract.rs`
  - Replaces hardcoded `eng` with selected language.
- `src/ocr/ocr.rs`
  - Threads language parameter through OCR orchestration methods.
- `src/pipeline/pdf_tokio_pipeline.rs`
  - Passes `config.ocr_language()` into OCR path.
- `src/pipeline/djvu_pipeline.rs`
  - Passes `config.ocr_language()` into OCR path.
- `src/progress.rs`
  - Preflight now validates selected language data (`<code>.traineddata`) on Linux/macOS.
  - Error includes missing filename and search context.

## Linux verification steps (to run later)

1. Build/check:

```bash
cargo check
```

2. Verify default behavior still works:

```bash
lege sample.pdf
```

3. Verify explicit English:

```bash
lege sample.pdf --language eng
```

4. Verify non-3-letter code support:

```bash
lege sample.pdf --language chi_sim
```

5. Verify fail-fast for missing data:

```bash
lege sample.pdf --language jpn
# Expect clear error unless jpn.traineddata is discoverable
```

6. Verify invalid code rejection:

```bash
lege sample.pdf --language ja-p
# Expect validation error
```

7. Verify help visibility is subtle:

```bash
lege --help
```

## Notes

- Tesseract language files must be named `<code>.traineddata`.
- This feature intentionally does not alter interactive mode UX.
- Existing OCR remains unchanged when `--language` is not used.
