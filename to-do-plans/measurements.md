# Phase 0 measurements

Canonical requirements: [unified-renderer-integration-plan.md](unified-renderer-integration-plan.md),
Phase 0. This file is the durable baseline ledger for Phases 1a–1d and later
renderer integration. Do not replace an old measurement: add a dated run so
before/after comparisons retain the same inputs and commands.

## Status

As of 2026-07-23, the renderer sweeps are paused and Lege integration work
has resumed. Another agent still owns the renderer workspace. Lege does not
modify that workspace.

- [x] `--debug-runtime-stats` flag and machine-readable report implemented.
- [x] 250 ms thread/RSS time series implemented on Linux.
- [x] Rayon width and blocking-pool entry/current/peak counters implemented.
- [x] Render, inference, processing, encode, OCR, and writer gauges implemented.
- [x] Stage active-wall and summed-job timing implemented.
- [x] Three baseline inputs selected and characterized without rendering them.
- [x] Initial bookmark failure list recorded from current-code inspection.
- [x] Build the instrumented debug-fast binary after the external renderer tree is coherent.
- [x] Record the three conversion baselines — 2026-07-23, see below.
- [x] Run the bookmark preservation matrix — 2026-07-23, PDF paths only.
- [ ] Fill every required corpus slot with a reviewed, redistributable fixture or
      a stable local-only path.

**These are not pre-integration baselines.** The Phase 0 runs were never made
before the work started, and pdfium was removed in Phase 3, so no "before"
number exists to compare against for any mode that involves layout or optical
character recognition. The rows below are the *current* code measured on the
Phase 0 inputs and commands. They are the reference point for later phases, not
a before/after pair. The only genuine before/after in this file is the Phase 3
no-layout comparison, which was taken while both engines were still present.

The external renderer tree became coherent later on 2026-07-23.
`cargo build --profile debug-fast -p lege --bin lege --offline` now passes.
This proves the integration build, but it is not a release baseline run and
does not fill the performance rows below.

## Runtime report contract

`lege ... --debug-runtime-stats` writes exactly one line beginning with
`LEGE_RUNTIME_STATS ` to stderr at process exit. The remainder is schema-1
JSON containing:

- elapsed wall time, peak threads, peak RSS in KiB, and Rayon pool size;
- blocking-pool entries, current/peak use, active-wall time, and summed job time;
- the same current/peak/timing fields for each pipeline stage;
- 250 ms samples of threads, RSS, blocking use, and stage in-flight counts.

`active_wall_seconds` is the union of intervals in which at least one job in
the stage was active. `summed_job_seconds` sums all concurrent jobs. For the
layout-mode share of wall time, use `active_wall_percent`; stage percentages
may overlap because the pipeline runs concurrently.

The report goes to stderr so `--gui-worker` stdout remains valid NDJSON.
Collection is dormant when the flag is absent.

## Baseline host and build

Fill these fields once sweep 11 is complete and before the first measured run.

| Field | Baseline value |
|---|---|
| Date/time | 2026-07-23 19:57 +08:00 |
| Git commit plus dirty-tree description | `b8f7451`, working tree dirty (74 modified paths: Phases 1–8 uncommitted) |
| `rustc -Vv` | 1.97.1 (8bab26f4f 2026-07-14), x86_64-unknown-linux-gnu, LLVM 22.1.6 |
| Cargo profile/features | `debug-fast` (inherits release, no fat LTO), default features |
| CPU / logical cores | 13th Gen Intel Core i7-13700H / 20 |
| RAM | 31 GiB total, ~19 GiB available at run time |
| GPU and driver | NVIDIA GeForce RTX 4060 Laptop GPU, driver 580.159.03, Vulkan |
| OS / kernel | Linux 6.17.0-40-generic |
| Lege binary SHA-256 | `53ac4f57ec08dc25fb18727ebace5fb1b19fb523a0d00a5290667bd9e16240ce` |
| Renderer sweep/result used | `lege-pdf/render` at the Phase 8 state |

The profile is `debug-fast`, not `release`, because that is the profile every
phase from 1b onward was measured on; switching now would break comparability
with the Phase 3–8 rows for a build-time saving only.

Use a quiescent host. Preserve `/usr/bin/time -v` stderr and the
`LEGE_RUNTIME_STATS` line in a dated results directory. Run each case at least
twice; record the second run unless the two differ by more than 5%, in which
case run a third time and record all three.

## Selected baseline inputs

These files were only inspected with `pdfinfo`; no renderer or Lege conversion
was invoked.

| Case | Stable local input | Characterization | SHA-256 |
|---|---|---|---|
| Small clean PDF | `/mnt/Samsung980_1TB/to-sort/jbig99paper.pdf` | 6 pages, 495,264 bytes, PDF 1.2, unencrypted | `e0fb996e9905ef05818ce7079da4e95a01a398a161f8383ec67a2402a6e29deb` |
| Large scanned book + slow OCR | `/mnt/Samsung980_1TB/to-sort/crusadeswholesto0000lamb_1.pdf` | 886 pages, 578,638,454 bytes, PDF 1.3, unencrypted | `379fbd2b8ef51bc8c29def56751585a5b0f090288a4df9e9d935b90595b644c4` |
| DjVu output job | `/mnt/Samsung980_1TB/to-sort/risefallofconfed01daviuoft.pdf` | 782 pages, 74,013,712 bytes, PDF 1.5, unencrypted | `b4426498d43666c576c54937f75abc42d4740c66000d9216345fb54e993d9d3e` |

The large-scan case is the memory/thread stress baseline. The DjVu case uses a
different public-domain scan so PDF and DjVu paths cannot accidentally share a
document-specific fast path.

## Reproducible commands

Set `LEGE_BIN` to the exact release binary being measured and create a fresh
dated directory outside the source tree. Do not overwrite output from an older
phase.

```bash
LEGE_BIN=/absolute/path/to/lege
RESULTS=/tmp/lege-phase0-YYYYMMDD
mkdir -p "$RESULTS"

/usr/bin/time -v "$LEGE_BIN" \
  /mnt/Samsung980_1TB/to-sort/jbig99paper.pdf all 1200 \
  --text-format ccitt4 --output "$RESULTS/small" \
  --debug-runtime-stats \
  2>"$RESULTS/small.stderr"

/usr/bin/time -v "$LEGE_BIN" \
  /mnt/Samsung980_1TB/to-sort/crusadeswholesto0000lamb_1.pdf all 1200 \
  --text-format ccitt4 --ocr --ocr-mode best --output "$RESULTS/large-ocr" \
  --debug-runtime-stats \
  2>"$RESULTS/large-ocr.stderr"

/usr/bin/time -v "$LEGE_BIN" \
  /mnt/Samsung980_1TB/to-sort/risefallofconfed01daviuoft.pdf all 1200 \
  --text-format djvu --output "$RESULTS/djvu" \
  --debug-runtime-stats \
  2>"$RESULTS/djvu.stderr"
```

Before running, confirm the current CLI's positional target syntax with
`lege --help`; if it changed, update the commands here before recording data.

## Baseline results

Recorded 2026-07-23. Each case ran once; the run-twice rule was set aside for
the optical-character-recognition case by decision, and that case is a
**120-page slice** (`100-219`), not the whole 886-page book. The reason is that
the renderer turned out to be effectively bit-exact against pdfium, so the risk
the full run was meant to catch — render drift degrading recognition — does not
justify the machine time. The other two cases are complete documents.

| Case | Peak threads | Peak RSS KiB | Wall s | User CPU s | System CPU s | CPU % | Rayon | Blocking peak | Output SHA-256 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| Small clean PDF, 6 pages | 34 | 462,800 | 1.30 | 1.00 | 0.39 | 107 | 19 | 4 | `a5683f4dcc39b0aa1bd73dca447aa04c9f336b18e605f51876328db8e6862b32` |
| Large scan + best OCR, 120-page slice | 34 | 2,304,772 | 325.09 | 278.50 | 4.99 | 87 | 19 | 4 | `b4b3a0cc53b392693a481532a4deae49f20bf891307e03fc9fdce6810ba2a567` |
| DjVu output, 782 pages | 66 | 1,752,196 | 69.45 | 320.61 | 13.72 | 481 | 19 | 4 | `a633b80f518e2231071411732a14a024fbf89397fc1d9d1f628d81cb8cfa18af` |

Output sizes: 273,118 bytes; 594,140 bytes; 15,188,168 bytes.

Two things stand out and are worth carrying into Phase 9. Optical character
recognition is 99.0% of the wall time on the recognition case and only 87% of
one core is busy, so that path is latency-bound inside the engine, not
throughput-bound on the machine. The DjVu case is the opposite: 481% CPU, with
render and inference overlapping almost completely.

An unrelated defect surfaced on the first attempt: `--output <dir>` where the
directory does not exist fails with `PDF writer actor has stopped` rather than
naming the missing directory. The writer task's own error is only logged under
the `debug-logging` feature, so the release message is the downstream channel
error. Not fixed here; recorded for a later pass.

### Layout-stage wall share

Copy `active_wall_seconds`, `active_wall_percent`, `summed_job_seconds`, and
`peak` from the small clean layout-mode report. Repeat for the large scan when
it is practical; the small case is the canonical comparison row.

Stage percentages overlap, because the pipeline runs the stages concurrently.

| Case/stage | Active wall s | Wall % | Summed job s | Peak in flight |
|---|---:|---:|---:|---:|
| Small / render | 0.252 | 20.1 | 0.524 | 4 |
| Small / inference | 0.149 | 11.9 | 0.242 | 2 |
| Small / processing | 0.045 | 3.6 | 0.045 | 1 |
| Small / encode | 0.111 | 8.8 | 0.124 | 3 |
| Small / OCR | 0.038 | 3.0 | 0.050 | 2 |
| Small / writer | 0.002 | 0.2 | 0.002 | 1 |
| OCR slice / render | 8.400 | 2.6 | 8.751 | 4 |
| OCR slice / inference | 3.629 | 1.1 | 3.674 | 2 |
| OCR slice / processing | 2.705 | 0.8 | 2.833 | 4 |
| OCR slice / encode | 0.198 | 0.1 | 0.198 | 1 |
| OCR slice / OCR | 321.819 | 99.0 | 1258.724 | 4 |
| OCR slice / writer | 0.074 | 0.0 | 0.074 | 1 |
| DjVu / render | 64.922 | 93.7 | 328.119 | 19 |
| DjVu / inference | 63.981 | 92.3 | 210.701 | 6 |
| DjVu / processing | 4.343 | 6.3 | 4.645 | 3 |
| DjVu / encode | 3.602 | 5.2 | 3.602 | 1 |
| DjVu / OCR | 8.512 | 12.3 | 9.622 | 3 |
| DjVu / writer | 3.006 | 4.3 | 3.006 | 1 |

The small-document row is dominated by fixed startup: 1.30 s of wall time
against 0.60 s of summed stage work.

## Corpus manifest

Local-only inputs may remain outside Git, but each must be identified by an
immutable SHA-256 before it becomes a gate fixture. Avoid personal documents.

| Required class | Fixture | Status / expected property |
|---|---|---|
| Bookmarked PDF | `lege-codecs/jbig2enc-rust/T-REC-T.88-200002-S!!PDF-E.pdf` | In-tree, 164 pages, 70 entries, two levels, direct and named destinations. Verified round-trip 2026-07-23 |
| Named destinations only | Generated by `named_destination_fixture()` in `lege-process/tests/renderer_read_parity.rs` | Direct, name-tree, and legacy catalog `/Dests` entries resolve to the same pages as PDFium |
| Encrypted PDF | PENDING | Record password handling and permissions; use a purpose-built fixture |
| Scanned book with printed contents | `crusadeswholesto0000lamb_1.pdf` | Selected; printed-TOC pages and outline status still need review |
| No chapters | `jbig99paper.pdf` | Selected paper; verify source outline is empty |
| Music score | `Lege-ecosystem/Pop & Rock .pdf` | Selected 72-page score; Phase 2 document/text parity passes all pages |
| Paper | `jbig99paper.pdf` | Selected, 6 pages |
| Large region-heavy scan | `risefallofconfed01daviuoft.pdf` | Selected; verify it exercises image regions before using as Phase 1c gate |

## Bookmark preservation matrix

These are current-code findings, not conversion observations. Replace
`PENDING RUN` with the exact observed result and output hashes after sweep 11.

Observed 2026-07-23 on `lege-codecs/jbig2enc-rust/T-REC-T.88-200002-S!!PDF-E.pdf`
(164 pages, 70 outline entries, two levels, direct and named destinations).

| Path | Current-code risk | Observed 2026-07-23 |
|---|---|---|
| PDF → PDF, full document | Bookmark extraction is detached and can lose the race with writer finalization. `SetBookmarks` is last-write-wins. | **Pass.** 70 source entries → 70 output entries, same two-level nesting, `/Fit` destinations, page indexes unchanged. Extraction is awaited, and the last-write-wins slot became an explicit merge. |
| PDF → PDF, page range | Main pipeline sends an empty map, which is interpreted as identity; source page indexes are not shifted and out-of-range nodes are not filtered correctly. | **Pass.** Range `40-79` emits 8 entries, every index shifted by −39. Parents outside the range are dropped and their in-range children promoted to the top level, so no subtree is lost with its parent. |
| PDF → DjVu, full document | DjVu writer has no bookmark/NAVM transport in the current pipeline. | **Still fails.** Unchanged: no transport exists yet. This is the deferred DjVu half of Phase 7. |
| Named-destination-only PDF → PDF | Unresolved destinations become `usize::MAX`; the node and its entire subtree are dropped. | **Pass.** `extract_outline` resolves direct arrays, `/Names` name-tree entries and the legacy catalog `/Dests`, and promotes the children of an unresolvable node. Covered by `outline_resolves_all_destination_forms_and_promotes_children` in `lege-pdf/read/tests/document_read.rs`. |

For every bookmarked fixture, record:

1. source outline as a stable title/page/tree snapshot;
2. full-document PDF output outline;
3. a page range that removes at least one parent but retains a resolvable child;
4. DjVu `NAVM` output inspected with `djvused`;
5. whether titles, nesting, and destination output indexes match.

## Phase 1 comparison rule

Phases 1a–1d must reuse the exact input hashes, binary feature set, target,
and command lines above. Phase 1a must lower peak threads and scanned-book RSS
without changing output bytes. Phase 1b and Phase 1c use the corpus hashes for
byte-identity checks. Phase 1d is complete only when the PDF full-document and
page-range bookmark rows pass.

### Phase 1b grayscale API evidence — 2026-07-23

`binarize_gray` is now the primary implementation. The RGB compatibility
wrapper retains the old integer Rec.601 fixed/GPU conversion, linear-light
BT.709 adaptive CPU conversion, and post-GPU-failure retry.

The opt-in byte-equivalence test used all pages from:

- `jbig99paper.pdf` — 6 pages;
- `crusadeswholesto0000lamb_1.pdf` — 886 pages;
- `risefallofconfed01daviuoft.pdf` — 782 pages;
- `Pop & Rock .pdf` — 72 pages.

For every page, the renderer produced a 160-pixel-high RGB raster. The test
compared the new wrapper with an embedded copy of the pre-refactor CPU
algorithm in both adaptive and fixed-threshold modes. All 3,492 comparisons
were byte-identical. Focused deterministic tests also cover input/output
inversion, callback fallback, and PBM packing. A real-GPU wrapper comparison
passes.

Verification:

```text
cargo test -p lege --lib color::binarization::tests --offline
  12 passed (the corpus test is ignored by default)

WGPU_REQUIRE_REAL_GPU=1 LEGE_RUN_GPU_TESTS=1 \
  cargo test -p lege --lib \
  color::binarization::tests::rgb_wrapper_matches_legacy_gpu_gray_path \
  --offline -- --nocapture
  1 passed

LEGE_BINARIZATION_PARITY_CORPUS=<four paths> \
  cargo test -p lege --lib \
  color::binarization::tests::rgb_wrappers_are_byte_identical_to_legacy_cpu_on_pdf_corpus \
  --offline -- --ignored --nocapture --test-threads=1
  1 passed; 3,492 byte comparisons; 447.48 s
```

### Phase 2 document and hOCR evidence — 2026-07-23

`lege-process/tests/renderer_read_parity.rs` uses PDFium 0.9.2 only as the
temporary oracle. On each page it compares page count, displayed geometry,
outline shape, normalized page text, word reconstruction, and word boxes.
It now also sends renderer words and PDFium-era positioned words through the
same production `build_hocr_from_positioned_words` function. The test
extracts the visible hOCR text and requires the renderer hOCR to lose no more
text than the PDFium-era hOCR, relative to each engine's native page-text
result.

The three Phase 0 baselines pass all 1,674 pages:

- `jbig99paper.pdf` — 6 pages;
- `crusadeswholesto0000lamb_1.pdf` — 886 pages;
- `risefallofconfed01daviuoft.pdf` — 782 pages.

The deterministic `named_destination_fixture()` also passes. Both engines
produce the same three-entry outline and resolve a direct destination, a
`/Dests` name-tree destination, and a legacy catalog `/Dests` destination to
the same page indexes.

The external renderer fix clears the T.88 cover-page blocker. The Lege
normalizer reports a one-character multiset delta on page 0, inside its
four-character tolerance. A full-document diagnostic reaches page 29 before
finding a different oracle-quality disagreement: PDFium maps Symbol-font
minus codes to `ñ`, while the renderer uses the font-program cmap and emits
the correct signs. The general parity threshold was not weakened for that
PDFium defect.

Verification:

```text
cargo test -p lege --test renderer_read_parity --offline
  3 passed; 2 ignored

LEGE_PDFIUM_PATH=<libpdfium.so> cargo test -p lege \
  --test renderer_read_parity --offline \
  named_destinations_match_pdfium -- --ignored --nocapture
  1 passed

LEGE_PDFIUM_PATH=<libpdfium.so> \
LEGE_PDF_PARITY_CORPUS=<three baseline paths> \
  cargo test -p lege --test renderer_read_parity --offline \
  renderer_read_matches_pdfium_on_corpus -- --ignored --nocapture
  1 passed; 1,674 pages; hOCR comparison passed on every page
```

### Phase 3 temporary raster switch — 2026-07-23

The debug-fast binary was run twice on `jbig99paper.pdf`, with identical CLI
options except for `LEGE_RENDER_ENGINE`:

```text
lege jbig99paper.pdf all 1200 --text-format ccitt4 --no-layout
```

| Engine | Result | Pages / size | Output bytes | Extracted-text SHA-256 |
|---|---|---|---:|---|
| `lege` | pass without loading PDFium | 6 / 927×1200 | 304,185 | `d92d56cb2f8da6aa2f41f8ce3f237fe91be3fe2508ae51ec78acca2c2ffbbafa` |
| `pdfium` | pass with packaged 0.9.2 oracle | 6 / 927×1200 | 293,557 | `d92d56cb2f8da6aa2f41f8ce3f237fe91be3fe2508ae51ec78acca2c2ffbbafa` |

All six output pages were rendered through Poppler at 72 DPI and inspected.
The Lege and PDFium results have the same layout and content. Pixel metrics
differ because the two rasterizers antialias differently and the adaptive
binarizer moves those edge pixels across its threshold. This is the expected
reason that Phase 3 compares end products instead of requiring identical
rasters.

Verification:

```text
cargo check -p lege --offline
  pass

cargo build --profile debug-fast -p lege --bin lege --offline
  pass

cargo test -p lege --lib pagerender::render_engine_tests --offline
  1 passed
```

### Phase 3 final raster cutover — 2026-07-23

After the temporary-switch comparison, Lege was reduced to one
`PdfRenderer` backed by one shared `RenderSession` per document. PDFium,
its runtime discovery, feature flags, dependency, and installer assets were
then removed.

#### Complete no-layout end products

| Baseline | Pages / geometry | Renderer bytes / time | PDFium bytes / time | Text result |
|---|---|---:|---:|---|
| `jbig99paper.pdf` | 6 / 927×1200 | 304,185 | 293,557 | identical SHA-256 `d92d56cb2f8da6aa2f41f8ce3f237fe91be3fe2508ae51ec78acca2c2ffbbafa` |
| `crusadeswholesto0000lamb_1.pdf` | 886 / 778×1200 | 3,521,646 / ~12.3 s | 3,522,845 / ~49.5 s | identical SHA-256 `14263bb9308ff1bf59a567bd220a90ffa1fb114579befd726fa2752624296ffb` |
| `risefallofconfed01daviuoft.pdf` to DjVu | 782 / four matching dimension groups | 15,224,166 / ~28.6 s | 15,319,088 / ~128.9 s | identical SHA-256 `23817a7589e23da633f82ba9cbf4f0b7ef25cf55bbe2d92abc92f94895879064` |

The full Crusades comparison sampled output pages 1, 40, 311, 475, 709, and
886. Pages 40 through 886 were pixel-identical; the cover measured 42.99 dB
PSNR. The DjVu outputs have the same geometry distribution: one 728×1200
page, 94 745×1200 pages, 186 758×1200 pages, and 501 782×1200 pages.

#### Layout and OCR

`tools/pdfium-diff` produced exact renderer/PDFium PNG pairs for 14 pages:
three JBIG paper pages, six Crusades pages, and five region-heavy pages.
Processing the two 14-page folders with layout enabled produced 24 detected
overlay images in each output with the same per-page counts. Corresponding
overlay dimensions differ by only a few pixels, consistent with the expected
edge-antialiasing differences.

Best-OCR samples on pages 1–3 of every baseline produce searchable output.
The canonical layout-enabled samples yielded:

| Baseline | Renderer text / words | Packaged PDFium text / words |
|---|---:|---:|
| JBIG paper | 40,502 bytes / 1,337 words | 48,665 bytes / 1,919 words |
| Crusades | 212 chars / 32 words | 195 chars / 29 words |
| Region-heavy | 41 chars / 8 words | 3 chars / 0 words |

These figures are diagnostics rather than an identical-text gate because OCR
segmentation legitimately changes with raster edges. The renderer JBIG
whole-document no-layout OCR had a higher normalized sequence match to the
source native text (0.6269) than the PDFium result (0.5368). During this
comparison, an unrelated pipeline defect was fixed: slow OCR previously
returned no hOCR when layout produced zero text-like detections. It now falls
back to one whole-page OCR region, as fast OCR already did.

#### Behavior and removal audit

- Password integration tests pass for revisions 2, 3, and 6 with correct,
  wrong, and missing passwords.
- Rotation geometry and text-coordinate tests pass.
- A generated annotation appearance-stream fixture confirms annotations
  paint by default.
- `Cargo.lock` and `cargo tree -p lege --offline` contain no
  `pdfium-render`.
- The built executable has no PDFium dynamic dependency or symbol according
  to `ldd` and `nm -D`.
- Linux, AppImage, and macOS packaging no longer require, copy, or sign a
  PDFium library.
- `lege-process/rust-toolchain.toml` now matches the workspace's Rust 1.97.1
  pin, which is the minimum required by the integrated renderer crates.
- A post-removal JBIG smoke output is byte-identical to the pre-removal
  renderer-selected output.

The functional Phase 3 cutover and quality gate pass. Whole-process peak
threads remain about 138 on the Crusades job and 198 on the DjVu job because
the existing Tokio blocking pool and codec schedulers are still independent.
The plan intentionally assigns the blocking-pool cap to Phase 8 after its
audit; Phase 4 also removes stage scheduler fan-out. This cross-phase metric
is recorded as deferred, not treated as a renderer regression.

### Phase 4 and Phase 5 combined implementation — 2026-07-23

The PDF pipeline now runs one page-owned job from render to writer handoff.
The old PDF render, inference, process, and forwarder channels are removed.
Each job takes a MiB host-memory permit before render. Renderer parser and
raster contexts are worker-local and document-tagged. Cancellation reaches
the renderer and has checkpoints between the major page stages.

The layout detector now uses K single-flight sessions on one shared WGPU
device and queue. Sibling sessions share immutable model-weight buffers.
Binarization and resize also use that device. One `gpu-poll` thread performs
blocking device polling. The old inference actor/pool and `GPU_BINARIZER`
mutex are removed.

#### K-session layout comparison

Command shape:

```text
WGPU_REQUIRE_REAL_GPU=1 LEGE_GPU_SESSIONS=<K> \
LEGE_VRAM_BUDGET_MB=2048 \
target/debug-fast/lege "Pop & Rock .pdf" 1-72 1200 \
  --text-format ccitt4 --output <directory>
```

Hardware: NVIDIA GeForce RTX 4060 Laptop GPU, Vulkan.

| K | Pages | Wall time | Throughput | Peak RSS |
|---:|---:|---:|---:|---:|
| 2 | 72 | 8.46 s | 8.51 pages/s | 1,310,764 KiB |
| 3 | 72 | 8.59 s | 8.38 pages/s | 1,487,068 KiB |
| 4 | 72 | 8.57 s | 8.40 pages/s | 1,415,920 KiB |

K=2 is the saturation point on this workload. K=3 and K=4 do not improve
throughput, so K=2 remains the default. All runs completed every page without
session starvation. A separate K=4 run sampled with `nvidia-smi` at 100 ms
intervals peaked at 716 MiB for the Lege process, below the configured
2,048 MiB VRAM budget.

The short six-page `jbig99paper.pdf` smoke run also completed at K=2, K=3,
and K=4. After host-memory admission was added, the final rebuilt binary
completed 6/6 pages with `LEGE_MEMORY_BUDGET_MB=1024` and
`LEGE_VRAM_BUDGET_MB=2048`.

Verification:

```text
cargo test -p lege-gpu --lib --offline
  29 passed

cargo test -p lege-pdf-read --test document_read --offline
  8 passed, including pre-cancelled render <100 ms

cargo test -p lege --lib --offline -- --test-threads=1
  150 passed; 1 ignored

cargo build --profile debug-fast -p lege --bin lege --offline
  pass
```

Phase 4 still needs the long optical-character-recognition baseline for the
full exit gate: resident memory versus the selected host-memory budget, wall
time versus Phase 0, and cancellation latency during an already-running long
kernel.

### Phase 6 page output plan and Phase 8 diet — 2026-07-23

Binary: `target/debug-fast/lege`, built from the Phase 6/8 tree.
Hardware: the Phase 0 host; NVIDIA GeForce RTX 4060 Laptop GPU, Vulkan.
DjVu runs use `lege-codecs/djvulibrust/target/release/djvu-encoder`.

#### Phase 6 — common bilevel page through the page output plan

```text
WGPU_REQUIRE_REAL_GPU=1 /usr/bin/time -v target/debug-fast/lege \
  <input> <range> 1200 --text-format ccitt4 --output <directory>
```

| Case | Pages | Wall time | Peak RSS | Output bytes |
|---|---:|---:|---:|---:|
| `jbig99paper.pdf` | 6 | 1.16 s | 484,088 KiB | 273,118 |
| `risefallofconfed01daviuoft.pdf` 30-149 | 120 | 13.94 s | 1,440,352 KiB | 3,220,851 |

The six-page product is 273,118 bytes against the 304,185 bytes of the Phase 3
renderer cutover on the same input, with the same 6 pages and the same
927×1200 geometry. Every page carries ink (0.42% on the title page, 4.97% to
10.47% on the text pages), so the smaller product is not a blank-page
regression.

#### Phase 8 item 8 — bounded reflow

```text
WGPU_REQUIRE_REAL_GPU=1 /usr/bin/time -v target/debug-fast/lege \
  risefallofconfed01daviuoft.pdf <range> 1200 \
  --text-format ccitt4 --reflow --output <directory>
```

| Source pages | Wall time | User+sys CPU | Peak RSS | Output bytes |
|---:|---:|---:|---:|---:|
| 30 | 24.88 s | 29.61 s | 609,136 KiB | 3,815,432 |
| 120 | 74.41 s | 100.34 s | 626,084 KiB | 18,996,621 |

Peak resident memory is flat against a four-fold increase in document length
(+2.8%), which is the point of the change: the old design held every rendered
source page, grayscale and RGB, until the last output page was composed.

#### Phase 8 item 7 — cancellation and subprocess safety

All runs use `risefallofconfed01daviuoft.pdf` or `jbig99paper.pdf` with
`--text-format djvu`, and measure the interval from `kill -TERM` to whole
process exit. The DjVu temp base is counted before and after each run.

| Case | Latency to process exit | Work directories left | Orphan encoder |
|---|---:|---:|---|
| SIGTERM during page processing | 0.310 s | 0 | none |
| SIGTERM while the encoder child ran | 0.297 s | 0 | none |
| Normal completion, 6-page DjVu | — | 0 | none |

The encoder-phase case uses a stand-in encoder that stays alive, so the child
kill path is exercised deliberately. Before this change the same case left the
job directory behind, because `DjvuOrchestrator::cleanup` did nothing: the
host's DjVu temp base held 137 MiB of directories from earlier runs.

Two DjVu conversions of `jbig99paper.pdf` produce a byte-identical 107,566-byte
document, so the cleanup and encoder-control changes do not alter the product.

Verification:

```text
cargo build --profile debug-fast -p lege --bin lege --offline
  pass

cargo test -p lege --lib --offline -- --test-threads=1
  167 passed; 1 ignored

cargo test -p lege-pdf-read --offline
  pass
```

New tests: four compose-window tests (bounded residency, a spanning output
page, no re-render of a resident page, color kept only for figure pages) and
three DjVu cleanup tests (managed directory removed whole, unmanaged directory
keeps files it does not own, guard cleans up when a job unwinds).

Still open for the Phase 8 exit gate: resident memory and total CPU time
against the Phase 0 baseline on all three documents, which needs the same long
optical-character-recognition run Phase 4 still owes.

### Phase 4 and Phase 8 gate evidence — 2026-07-23

The three baseline conversions above close the measurement half of both gates.
What follows covers the two gate clauses that do not need a pre-integration
"before": resident memory against the configured byte budget, and cancellation
latency.

#### Host-memory admission

`LEGE_MEMORY_BUDGET_MB` sets the MiB-denominated admission semaphore each page
takes a permit from before it renders.

| Case | Budget | Wall s | Peak RSS KiB |
|---|---:|---:|---:|
| OCR slice, 120 pages | default (8,192) | 325.1 | 2,304,772 |
| OCR slice, 120 pages | 1,024 | 367.6 | 2,322,516 |
| `risefallofconfed01daviuoft.pdf` 1-200, no OCR | 8,192 | 25.4 | 1,385,508 |
| `risefallofconfed01daviuoft.pdf` 1-200, no OCR | 64 | 99.7 | 745,300 |

The semaphore works, but at the default budget it is not what bounds resident
memory. A 1200-pixel page is estimated at about 15 MiB, so even a 1,024 MiB
budget admits far more pages than `page_concurrency` ever runs — dropping the
budget from 8,192 to 1,024 changes peak resident memory by 0.8%, which is
noise. Squeeze the budget to 64 MiB and admission becomes the binding
constraint: peak resident memory falls 46% and wall time rises 3.9×.

So the Phase 4 clause "resident memory is about the byte budget" holds in the
sense that resident memory responds to the budget monotonically and the
mechanism is real, but not in the sense that resident memory sits *at* the
budget. On this hardware the default budget is far above the concurrency
ceiling, and page concurrency is what actually caps memory. Two honest
consequences: the default budget is doing no work on a 20-core, 31 GiB host,
and the per-page estimate (`width × height × 16`) is the knob that would have
to change for it to.

#### Cancellation latency

Interval from `kill -TERM` to whole-process exit, measured with the kill landing
mid-run.

| Case | Samples | Latency s |
|---|---:|---|
| Slow OCR in flight (crusades slice, `--ocr --ocr-mode best`) | 4 | 0.359, 0.361, 0.370, 0.374 |
| Layout only, no OCR (risefall, ccitt4) | 2 | 0.317, 0.330 |
| DjVu, page processing (Phase 8 section above) | 1 | 0.310 |
| DjVu, encoder child running (Phase 8 section above) | 1 | 0.297 |

**The Phase 4 gate of under 100 ms is not met at process-exit granularity, and
the cause is not the kernel it was written about.** A run with no optical
character recognition at all exits in 0.32 s; adding a recognition call that is
in flight when the signal arrives costs about 40 ms more. The floor is fixed
process teardown — dominated by wgpu/Vulkan device destruction — not by waiting
for a long kernel to finish. Cancellation itself reaches the pipeline promptly:
every run printed `Processing cancelled by SIGTERM`, left no orphan process, and
left no work directory.

Two readings of the gate, both worth stating. If it means "the pipeline stops
promptly", it is met. If it means "the process is gone in 100 ms", it is not,
and closing it would mean shortening GPU teardown rather than adding more
cancellation checkpoints. Nothing here was tuned to make the number look
better.

### Phase 7 automatic table of contents — PDF output — 2026-07-23

Binary: `target/debug-fast/lege`, rebuilt from the Phase 7 tree. Command shape:

```text
[LEGE_TOC_DEBUG=1] WGPU_REQUIRE_REAL_GPU=1 target/debug-fast/lege \
  <input> <range> 1200 --text-format ccitt4 [--ocr] --output <directory>
```

`LEGE_TOC_DEBUG=1` prints every scored candidate to stderr. Emitted outlines
were read back out of the product with a direct object scan.

#### Corpus results

| Document | Pages run | Source outline | Text source | Emitted |
|---|---:|---|---|---|
| `T-REC-T.88-200002-S!!PDF-E.pdf` (in-tree) | 164 | 70 entries, 2 levels | native | **Preserved, 70/70**, same nesting, `/Fit` |
| Same, range `40-79` | 40 | 70 entries | native | **Preserved, 8 entries**, indexes −39, orphaned children promoted |
| `jbig99paper.pdf` | 6 | none | native | Nothing — a paper with no chapters |
| `Pop & Rock .pdf` | 72 | none | OCR | Nothing — 0 candidates. The music-score risk did not materialize |
| `crusadeswholesto0000lamb_1.pdf` `1-120` | 120 | none | OCR | Nothing — front matter suppressed |
| Same, `100-139` | 40 | none | OCR | Nothing — 6 candidates, all running heads, best score 0.93 |
| `risefallofconfed01daviuoft.pdf` `1-200` | 200 | none | OCR | Nothing — 5 candidates, 1 survivor, below the two-entry floor |
| `Studies_in_the_Language_of_Zu_tang_ji.pdf` `1-120` | 120 | none | OCR | **3 entries**: PREFACE p23, 1. INTRODUCTION p29, and 1.4.3 nested under it at p52 |
| `ASSOCIATION AGREEMENT.pdf` `1-120` | 120 | none | native | **3 entries**: Article 91 p50, Article 121 p66, Sub-section 7 p114 |
| Zu tang ji with `LEGE_NO_AUTO_TOC=1` | 120 | none | OCR | Nothing — the escape hatch works |

Both synthesized outlines are correct as far as they go, and both are sparse
relative to the structure actually present. That is the intended trade: on
`Studies_in_the_Language_of_Zu_tang_ji.pdf` a further nine genuine headings
scored 1.5–1.9 against a 2.0 threshold, so lowering the threshold would enrich
the outline — and would also lower the wall that keeps the failures below out.
The threshold is `SCORE_THRESHOLD` in `core/toc.rs` if that trade is ever
revisited.

#### The three signals added after the first corpus run

The first version of the scoring emitted outlines that would have been worse
than none. Each fix is a response to observed output, not a precaution.

| Failure | Observed | After |
|---|---|---|
| Running head with the printed folio | `crusades 100-139` emitted 8 entries, every one a running head: "THE COMING OF THE IRON MEN 91", "… 93", "ALEXIS AND BOHEMUND 97". The repetition kill never fired because the folio makes each instance textually unique | Comparison key ignores a leading or trailing page number. Same run: no outline, best score 0.93 |
| The document's own printed contents page | `risefall 1-200` emitted "PART IV." from the book's contents leaf. Short, well positioned, starts with a chapter word — 4.01 points | Sub-body-size is now a hard rejection, not a penalty. That candidate is set at 0.85× body height and never reaches scoring |
| Two increasing numbers read as chapter numbering | The same run emitted "92 RISE AND FALL OF THE CONFEDERATE GOVERNMENT." — a running head whose folio 92 chained with an unrelated "PART IV." to collect the 1.5-point sequence boost, scoring 2.51 | A sequence needs three numbers stepping by at most three. Same candidate now scores 0.01 |

Verification:

```text
cargo test -p lege --lib --offline -- --test-threads=1
  189 passed; 1 ignored     (167 before Phase 7)

cargo test -p lege-pdf-write --offline     49 passed
cargo test -p lege-pdf-read --offline      12 passed
cargo ecosystem-check                      clean
cargo build --profile debug-fast -p lege --bin lege --offline   pass
```

New tests: ten in `core/toc.rs` (clean chapter book, plain running header,
folio-bearing running header, printed contents page, front matter, two-level
nesting, density cap, low word confidence, number parsing, capture from hOCR),
two in `core/hocr.rs` (`x_wconf` present and absent), and four in
`pipeline/helper_functions.rs` (source outline wins, synthesized fills the gap,
neither emits nothing, page-range shift with child promotion).

Deferred: the DjVu half — manifest schema 2, `SetOutline` on the DjVu writer,
and the NAVM re-enable in djvulibrust. DjVu output still carries no outline at
all, preserved or synthesized, which is the one remaining row of the bookmark
preservation matrix.

### Phase 9 profile — 2026-07-23

Phase 9's own instruction is to profile before starting any of its three items.
The profile says **none of the three is justified**, and points somewhere else
entirely.

Binary: `target/profiling/lege` (`--profile profiling --features profiling`).
`perf record -F 499` on the whole process; `kernel.perf_event_paranoid` was
lowered to 1 for the session. The host is a hybrid P/E-core i7-13700H, so perf
emits a `cpu_core` and a `cpu_atom` table — the shares below weight the two by
their event counts, which is why they sum to 100%.

#### Where the cycles actually go

| Bucket | Scanned book, ccitt4 | Scanned book, jbig2 | Born-digital text, ccitt4 | Born-digital text, jbig2 |
|---|---:|---:|---:|---:|
| Input JPEG2000 decode (`jp2lam::decode`/`dwt`) | 28.3% | 27.9% | 2.1% | 1.9% |
| Input JBIG2 decode | 22.4% | 22.1% | — | — |
| Renderer raster + parse | 28.1% | 27.5% | 19.8% | 17.3% |
| **CCITT encode (output)** | **0.6%** | — | **5.9%** | — |
| **JBIG2 encode (output)** | **0.1%** | **2.3%** | — | **18.1%** |
| **JP2 encode (output, image regions)** | **0.8%** | **0.8%** | **11.3%** | **9.4%** |
| **Binarization, CPU side** | **0.1%** | **0.1%** | **1.6%** | **1.8%** |
| GPU driver | 0.2% | 0.2% | 3.2% | 3.0% |
| memmove/memset/libm/malloc/scheduling | 16.5% | 16.0% | 47.4% | 40.9% |

Scanned book = `risefallofconfed01daviuoft.pdf 1-200`, 325–330e9 cycles.
Born-digital text = the 164-page T.88 specification, 26–30e9 cycles, 4.50 s.

#### Verdict on the three Phase 9 items

- **Fixed-threshold band streaming** — targets binarization and the full-page
  buffer traffic. CPU-side binarization is 0.1% of cycles on scanned books and
  1.6–1.8% on born-digital text; the work itself is on the GPU. **Declined.**
- **Rolling adaptive binarization with band-local integrals** — same target,
  same 1.8% ceiling. **Declined.**
- **Direct CCITT row emission and JBIG2 generic-region row integration** — the
  only one with a real number behind it. On the T.88 specification JBIG2 encode
  is 18.1% of cycles, and it *is* generic-region mode
  (`encode_generic_region_inner`, 13.8%), so the item applies. But row
  integration does not make the arithmetic coder faster; it removes the
  intermediate packed plane, which is `binary_pixels_to_bitimage` at 3.7% plus
  some of the memory traffic. And that document's wall time is
  inference-bound — layout inference holds 73.7% of active wall against
  encode's 27.6% — so removing 4% of cycles would not move it. On the scanned
  book, the audience workload, CCITT encode is 0.6% and JBIG2 encode 0.1%.
  **Declined**, with the note that it becomes worth revisiting if layout
  inference ever stops being the critical path on born-digital documents.

#### What the profile found instead

`__powisf2` — the compiler-runtime helper for `powi` — was **31.4% of all CPU
cycles** on a 40-page recognition run. Its caller is `f16_bits_to_f32`, which
widened each fp16 model weight with `2.0f32.powi(exp - 15)`.

The cost is not a one-time model load. Absolute cycles in that helper scale
with page count: 3.57e9 for one page, 128.77e9 for forty. The reason is
structural: `Detector` and `RecRecognizer` keep the raw `ModelProto` and call
`PreparedGraph::from_model_with_input_dims` whenever an input size is not in
their small graph cache, and every rebuild re-widens every weight. A scanned
book whose pages vary in size misses that cache constantly.

Fixed by rewriting `f16_bits_to_f32` as bit-field widening — the exponent
rebiased from 15 to 127, the mantissa shifted by 13. A test compares the new
implementation against the old formula across all 65,536 half values and
requires bit-identical results.

| 40-page recognition run, both under perf | Before | After |
|---|---:|---:|
| Total cycles | 409.8e9 | 230.7e9 |
| `__powisf2` | 128.77e9 (31.4%) | 0 |
| fp16 widening collect | 78.35e9 (19.1%) | 22.52e9 (9.8%) |
| Wall time | 104.83 s | 80.32 s |

**44% fewer CPU cycles and 23% less wall time**, from a change to one function.
Output is unchanged: same 194,169-byte product and byte-identical extracted
text before and after.

Still on the table, not done: the remaining 9.8% is the re-widening itself —
allocating and converting the same weights again on every graph rebuild. Caching
the widened constants on `Detector`/`RecRecognizer` so a new input size reuses
them would remove it. That is a change to how `lege-gpu` holds models, which is
larger than the Phase 9 mandate, so it is recorded here rather than taken.

Verification:

```text
cargo test -p lege-gpu --lib --offline      31 passed  (2 new fp16 tests)
cargo test -p lege --lib --offline -- --test-threads=1   189 passed; 1 ignored
cargo ecosystem-check                       clean
```
