# pdfium-diff — multi-renderer differential harness

The renderer under development is always the baseline. Six independent
references can be compiled and selected separately: PDFium, hayro, Poppler,
MuPDF, Ghostscript, and PDF.js. The native/JavaScript controls run in child
processes so a malformed PDF cannot take the controller with it.

## Build the optional controls (Linux)

Normal Cargo builds never compile C/C++ or run npm. Provision only the controls
you need, then enable the corresponding Rust adapters:

```sh
./setup-renderers.sh --engines hayro,poppler,mupdf,ghostscript
# PDF.js additionally requires Node >= 22.13:
./setup-renderers.sh --engines pdfjs

cargo build --release --features renderer-hayro,renderer-poppler,renderer-mupdf,renderer-ghostscript
# Or compile every adapter:
cargo build --release --features all-renderers
```

PDFium remains a user-supplied shared library. Set
`PDF_RENDERER_PDFIUM_LIB=/path/to/libpdfium.so`; the other defaults are written
under `../.renderer-bin/` by the setup script. Every location can be overridden
with `--renderer-path NAME=PATH` or `PDF_RENDERER_<NAME>_BIN`.

Hayro is deliberately built from `../hayro`, not the 1.3 GB corpus checkout.
Its upstream `render` example supplies the standard/CJK font resolver required
for a meaningful comparison.

## Targeted adjudication

Page numbers are zero-based. This writes each original PNG plus a labeled
two-row sheet (renderers above, differences from ours below):

```sh
cargo run --release --features all-renderers -- \
  render issue18466.pdf --pages 0 --scale 2 --reference all --out renderer-diff-out

# Add diagnostic PDF-object attribution planes and category counts:
cargo run --release --features all-renderers -- \
  render issue18466.pdf --pages 0 --scale 2 --reference all \
  --attribution --out renderer-diff-out

# Any subset is valid:
cargo run --release --features renderer-hayro,renderer-mupdf -- \
  render file.pdf --pages 0,2-4 --reference hayro,mupdf --out renderer-diff-out
```

The tool records dimensions rather than resizing a renderer to agree with
another. Contact sheets white-pad smaller rasters at the same pixel scale.
Dimension disagreement is itself a finding.

With `--attribution`, `attribution.csv` reports gross-difference pixels by
both the topmost paint leaf (`path`, `shading`, `tiling-pattern`, `text`,
`image`) and its innermost containing construct (`page-content`, Form XObject,
annotation appearance, tiling cell, or Type 3 glyph). It records both category
area and difference share. Pixels painted only by a control are explicitly
`unattributed`; they are not guessed from the control's final raster. The
auxiliary pass respects geometric coverage and clips, but deliberately ignores
alpha, blend and soft-mask effects, so its categories are diagnostic leads,
not proof of cause.

## Multi-renderer corpus sweep

```sh
cargo run --release --features all-renderers -- \
  sweep <file.pdf|dir>... --scale 1 --reference all --out renderer-diff-out

# Attribution is opt-in and independently resumable:
cargo run --release --features all-renderers -- \
  sweep <file.pdf|dir>... --scale 1 --reference all --attribution \
  --out renderer-diff-out
```

`results.csv` is long-form: one row per `file/page/reference`. Resume keys are
therefore `file|page|reference`, so a failed engine does not discard successful
controls. By default sweeps retain metrics only; add `--dump` to write sheets
for suspect or dimension-mismatched pages. The multi-renderer CSV is a clean
break from the legacy PDFium-only schema and refuses to append to it.

`PDF_RENDERER_ENGINE_TIMEOUT` controls the per-engine timeout in seconds
(default 180). A timeout, crash, or missing PNG is recorded for that engine and
the remaining controls continue.

The metrics are ours-versus-each-reference and are triage signals, not votes.
Several engines can share the same malformed-input fallback and agree on the
wrong image; adjudicate against PDF semantics when the controls cluster.

## Optional cross-renderer timing sample

Timing never runs during an ordinary sweep. Run it alone (200 pages by
default), or request one timing pass before a sweep:

```sh
cargo run --release --features all-renderers -- \
  benchmark <file.pdf|dir>... --samples 200 --scale 1 --reference all \
  --out renderer-diff-out

cargo run --release --features all-renderers -- \
  sweep <file.pdf|dir>... --timing-sample 200 --scale 1 --reference all \
  --out renderer-diff-out
```

`timing.csv` compares identical sampled pages in cold-document batches. The
measurement intentionally includes PDF open/compile, renderer process startup,
rendering, PNG encoding, and output reads. It therefore answers operational
throughput for this adjudication workflow, not isolated hot raster-kernel
speed. Runs are sequential and intended as rough estimates; avoid concurrent
sweeps and repeat the sample when small differences matter.

## Legacy PDFium-only and profiling commands

The roadmap calls PDFium **"the differential-testing oracle"** (line 18) and
reserves `tools/` for "PDFium reference-render comparison tooling". This is
that tool.

```sh
cargo run --release -- <libpdfium.so> <scale> <file.pdf|dir>…
```

It renders sampled pages with both engines at the same pixel grid and reports
where we disagree, worst first.

Orchestration (2026-07-21): the controller dispatches *chunks* of files to a
pool of worker processes (`PDFIUM_DIFF_WORKERS`, default cores/2; chunk size
`PDFIUM_DIFF_CHUNK`, default 32) — one exe+pdfium load per chunk, workers
never read results.csv (the controller owns the done-set and merges each
worker's private CSV fragment on completion; rows and resume keys are
byte-compatible with older sweeps). A worker whose per-file progress marker
stalls for `PDFIUM_DIFF_TIMEOUT` seconds (default 180) is killed; the
in-flight file gets the terminal `page=-1` row and the rest of the chunk is
redispatched. Measured on a 50-file smoke: 3× wall-clock over the serial
process-per-file controller at 7-way concurrency; scales with the pool. Suspect pages are written to
`pdfium-diff-out/` as `ours | pdfium | difference` triptychs. `collect_pdfs`
recurses each directory (skipping `node_modules`/`.git`/`target`).

## The full sweep — three corpus roots

`./run-sweep.sh` runs the whole corpus (day-plan "sweep 3"); `CORPUS_ROOT`
defaults to `/mnt/Samsung980_1TB` (Linux; use `D:` on Windows). The roots:

| root | files | notes |
|---|---:|---|
| `$ROOT/Pol was right again` | 10,712 | main archive.org corpus |
| `$ROOT/to-sort` | 494 | loose intake |
| `$ROOT/Rust-projects/pdfium-port-plan/renderer-corpus` | 2,826 | pdfbox + pdf.js + hayro hand-picked regression PDFs |
| **total** | **14,032** | |

The first two are the 2026-07-18 baseline's 11,206 files (their `file|page` keys
are unchanged, so resume/dedup keeps prior rows byte-identical); renderer-corpus
is purely additive. Sanity-check wiring without a render:
`cargo run -- --count <dir>…`. Other modes:
`--rerun-failures <prior.csv> <failed|blank|destroyed|all> <lib> <scale>`
re-grades only a prior sweep's drop-class documents.

## Structured profiling

The tool also consumes the libraries' opt-in `profiling` feature:

```sh
cargo run --release -- profile 2.0833333 /absolute/path/page.pdf 0 10 profile.jsonl
```

It emits JSONL rows for cold end-to-end, warm-document, compiled-page,
warm-decoded, decode-only, and prepared-page execution modes. Add
`--mode cold|warm|compiled|warm-decoded|decode-only|prepared` to
run exactly one mode (especially useful for a focused flamegraph). Rows include named stage durations,
codec/repacking and image-sampling counters where applicable, output hash,
process RSS/high-water RSS, and renderer-owned peak bytes. Set
`PDF_RENDERER_PDFIUM=/path/to/libpdfium.so` to retain PDFium differential
metrics in every row without including the oracle render in the timed region.
`run-profile.sh` runs the same command; `run-perf-stat.sh` captures
Linux hardware counters; `run-flamegraph.sh` uses `cargo-flamegraph` when it is
installed. `../../corpus/perf/manifest.json` records the intended page classes;
the licensed PDFs remain external to this repository.

Whole-document scheduler runs use:

```sh
cargo run --release -- pipeline-profile 2.0 /absolute/path/document.pdf 3 pipeline.jsonl
./run-flamegraph.sh pipeline-profile 2.0 /absolute/path/document.pdf 1 pipeline.jsonl
```

Heap attribution is also feature-gated and uses `dhat-rs`:

```sh
./run-dhat.sh page.dhat.json 2.0 /absolute/path/page.pdf 0 1 page.jsonl --mode compiled
```

This enables the `dhat-heap` feature only for the profiling binary, installs
the DHAT global allocator, and writes the call-site profile to the requested
path. Normal library and tool builds retain the system allocator.

The permanent representative corpus can be run as a batch:

```sh
PDF_RENDERER_CORPUS_ROOT=/mnt/Samsung980_1TB \
./run-corpus-profile.sh structured ../../corpus/perf/results/local/structured
./run-corpus-profile.sh remaining ../../corpus/perf/results/local/remaining
./run-corpus-profile.sh rss ../../corpus/perf/results/local/rss
./run-corpus-profile.sh perf ../../corpus/perf/results/local/perf
./run-corpus-profile.sh dhat ../../corpus/perf/results/local/dhat
```

Set `PDF_RENDERER_PERF_IDS=id-one,id-two` to rerun a subset. The page list,
scales, and sample counts live in `../../corpus/perf/pages.tsv`.

Each document is rendered in its own worker process. If malformed input causes
PDFium or another native dependency to abort, the controller records a
`page=-1` terminal row in `results.csv` with the worker error and continues to
the next PDF. Those rows are retained on a resume so one known-bad file cannot
block a later unattended sweep. PDFium open and per-page render failures are
also written to the CSV rather than being silently skipped.

## Why it exists

Hand-written tests only cover the failures we already imagined. Every real
document tried during development exposed a bug that no synthetic test had
caught — a blank page, placement boxes, silently substituted fonts. PDFium
spent years absorbing those cases; borrowing it as an oracle is how we get
the same coverage without the same years.

## Reading the metrics

The engines are *not* expected to match pixel for pixel — anti-aliasing,
hinting and glyph rasterization all differ legitimately. So the metrics
separate noise from signal:

| metric | meaning |
|---|---|
| `inkΔ` | disagreement in how much of the page is marked at all |
| `gross` | fraction of pixels differing by more than 48/255 |
| `ours-ink` / `ref-ink` | page coverage, each engine |

`inkΔ` is the one that matters: no amount of AA difference makes a page
blank, fill with boxes, or lose an image. Ranking is `inkΔ * 10 + gross`, so
real defects sort above rasterization differences. A page where *we* fail and
PDFium succeeds is reported with a note — the worst outcome, never silently
dropped.

`suspect` is intentionally more sensitive than the compatibility bar. A
thresholded-ink disagreement now requires continuous-darkness corroboration:
`(inkΔ > 0.01 && continuous_inkΔ > 0.003) || gross > 0.05`. This preserves
blank/missing-content detection while suppressing scan pages where two
renderers lay down the same total darkness with different smoothing. Severity
ranking remains based on thresholded `inkΔ` plus `gross`; inspect
distinct-document representatives rather than treating the raw suspect count
as a regression score.

The 2026-07-18 full sweep is the current baseline: 65,990 sampled rows from
11,206 files, with 21 compiler failures (down from 1,571) and 3,229 rows over
the 0.05 bar. See `../../DEFERRED.md` for the by-cause ranking and named
regression fixtures. To make an on-demand triptych for one of those fixtures:

```sh
PDFIUM_DIFF_DUMP=1 cargo run --release -- <libpdfium.so> 1.0 <file.pdf>
```

## Deliberately outside the workspace

It needs a prebuilt `libpdfium.so`, which is **not** a dependency of the
engine and must never become one — nothing in `crates/` links PDFium. The
library is `dlopen`ed at runtime purely to grade us, so this tool builds and
runs independently of the engine's build graph.
