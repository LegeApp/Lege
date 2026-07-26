# Renderer-side pass 3 — CCITT row-band compositing, document-scoped font cache, load-aware JPX budget — 2026-07-20

This pass closes out the three renderer-side items left open by
`optimization-ccitt-type1-20260720.md` and
`optimization-jpx-integration-20260720.md`:

1. the CCITT bilevel compositing residual (~65 ms in image execution);
2. Type 1 / bare-CFF first-render latency across pages (document-scoped cache);
3. the fixed `Budgeted(2)` JPX decode budget (load-aware policy).

Raw paired rows are under `results/optimization-renderer3-20260720/`
(`metadata.txt` included). No git state was touched; the tree carries other
agents' uncommitted JPEG/JPX/jp2lam work throughout.

## Measurement method

Paired A/B, `tools/pdfium-diff` release (built with `pdf-render-cpu/profiling`).
Two binaries were built from the working tree:

- **bin-base** — task 1 reverted (`exec.rs`/`image.rs` restored to `HEAD`),
  task 2 neutralised (shared cache passed `None`), task 3 at its baseline
  (`Budgeted(2)`). `prepared.rs` is **not** reverted (it carries a concurrent
  agent's JPX integration), so the base/opt delta isolates only this pass.
- **bin-opt** — all three tasks.

Tasks 2 and 3 are additionally A/B-isolable **on bin-opt itself** via
`PDF_RENDERER_FONTCACHE=off` and `PDF_RENDERER_JPX_CONCURRENCY=budgeted:2`, which
holds the other two tasks identical across the pair. Task-2 and task-3 numbers
below use that same-binary isolation; task 1 uses base-vs-opt (its page has no
fonts or JPX, so the other tasks are inert there).

Host: i7-13700H, 20 logical cores. Corpus root `/mnt/Samsung980_1TB`. Severity
via `PDF_RENDERER_PDFIUM=…/libpdfium.so`.

---

## Task 1 — CCITT row-band bilevel compositing (implemented)

### What changed (`crates/pdf-render-cpu/src/exec.rs`, `image.rs`)

The prepared axis-aligned bilevel path still counted packed source bits with a
per-destination-pixel popcount loop (`binary_box_average`, ~5.8 taps/pixel on
this page) and divided four RGBA channels per pixel. The new path builds a
**summed-area table (integral image)** of set bits over the referenced source
sub-rectangle once per draw (`BilevelIntegral`), so each destination box's
population count is **four table reads** instead of a popcount loop. The
two-entry color mix is **memoized on `(n, ones)`**, which collapses the long
white/black runs that dominate bilevel scans to one division set per run. Clip
coverage and the source-over blend stay exactly per-pixel.

This is pure arithmetic reordering: the `ones` count, box area `n`, mix, and
compositing are identical to the old path, so output is byte-for-byte
unchanged. A 128-MiB-of-`u32` cap on the integral image falls back to the
per-pixel path for pathologically large source regions (never hit here). The
`image.source_sample_taps` counter (11,418,620) is identical between binaries,
confirming the counts match exactly.

### Result — ccitt-bilevel, scale 2.0, compiled, 30 runs/binary (2 alternating batches)

| Metric | bin-base | bin-opt | Change |
|---|---:|---:|---:|
| Median `render.image` | 44.62 ms | 28.16 ms | **1.58× faster** |
| Median `benchmark.total` | 48.74 ms | 30.16 ms | **1.62× faster** |
| Output hash | `533b4e49e12ab195` | `533b4e49e12ab195` | **unchanged** ✓ |
| PDFium severity | 0.032997166 | 0.032997166 | **unchanged** ✓ |
| Median peak RSS | 31.7 MiB | 38.9 MiB | +7.2 MiB (transient SAT) |

HARD GATE satisfied: hash `533b4e49e12ab195` byte-identical and severity
0.032997166 unchanged. The +7 MiB is the transient integral-image buffer
(~15 MiB for this ~4-megapixel scan), freed at end of draw.

(Absolute times run below the prior doc's ~65 ms/71 ms figures because of
machine-load differences between sessions; the paired same-machine delta is the
comparison of record.)

---

## Task 2 — Document-scoped parsed-font cache (implemented, self-financing)

### Cache design

- **Location / sharing** — `SharedFontProgramCache` lives on `CpuBackend`
  (`Arc<…>`), which the scheduler owns for one document's page range, so its
  lifetime scopes the cache to that render session and **all render workers
  share it**. Interior mutability is 8 `Mutex`-guarded shards; parsing happens
  outside the lock, so a shard is held only for a map get/insert. `FontProgram`
  clones are cheap `Arc` bumps.
- **Identity** — a **128-bit one-pass content hash** of the program bytes
  (`content_hash_128`) plus byte length and face index. This was forced by
  measurement: the scheduler compiles each page independently, so the same
  embedded font is a *fresh* `Arc<[u8]>` per page — pointer identity gave
  **0 cross-page hits** (measured). Content hashing gives full cross-page
  sharing; 128 bits is collision-free with wide margin at document scale (a few
  hundred programs), so no stored-byte verification is kept — the entry never
  retains a second copy of the bytes.
- **What is retained** — only programs that (a) `benefits_from_parse_cache`
  (native Type 1 / wrapped bare CFF, as before) **and** (b) whose parse actually
  took ≥ `MIN_PARSE_TO_CACHE` (400 µs). This parse-cost gate is the key to the
  no-regression requirement: native Type 1 parses in milliseconds and is
  retained; small subsetted CFF/TrueType parses in microseconds and is **not**,
  so a document of cheap-to-parse fonts pays no retention cost. Ordinary
  TrueType/OpenType is uncached as before.
- **Bound** — LRU by retained parsed bytes, **48 MiB** total (32–64 MiB band),
  enforced per shard. Embedded programs run tens of KB, so this holds hundreds
  of distinct programs — beyond any single document's font set — while bounding
  worst-case residency.
- **Observability** — atomic `(hits, inserts)` reported on backend teardown via
  `PDF_RENDERER_FONTCACHE_STATS=1`.

### Gate: type1 corpus page hash unchanged

type1-fonts, scale 2.0, compiled, 40 runs/binary:

| Metric | bin-base | bin-opt |
|---|---:|---:|
| Output hash | `4cd8639fb0f5077c` | `4cd8639fb0f5077c` ✓ |
| PDFium severity | 0.031789505 | 0.031789505 ✓ |
| Median total | 9.26 ms | 9.86 ms (noise) |
| Cold first-render | ~112–118 ms | ~112–120 ms |

The type1 corpus PDF is **single-page**, and compiled-mode profiling reuses one
worker, so the existing worker-local cache already serves the repeats — the
document-scoped cache neither helps nor hurts this specific measurement (its
value is *cross-page / cross-worker*). 16 of the 19 embedded programs clear the
400 µs gate and are document-cached.

### Cross-page demonstration (24-page Type 1 document)

The corpus has **no** multi-page document embedding expensive Type 1 fonts
shared across pages (scanned ~2000 PDFs incl. all latex/springer/hayro dirs;
Type 1 shows up only on single-page test PDFs — modern multi-page docs use cheap
subsetted CFF/TrueType). So, per the task's fallback instruction, a multi-page
fixture was generated by duplicating the type1 corpus page 24× with `pdfunite`
(`type1_x24.pdf`; each copy has *distinct* PDF object numbers but *identical*
font bytes — the exact case content-hash identity is built for).

Whole-document render via the parallel scheduler, scale 2.0, 18 runs/binary,
same-binary isolation (`PDF_RENDERER_FONTCACHE=off` vs on):

| Metric | cache OFF | cache ON | Change |
|---|---:|---:|---:|
| Median document total | 732.0 ms | 530.6 ms | **1.38× faster** |
| Median peak RSS | 134.9 MiB | 166.2 MiB | +31.3 MiB |
| Cache (hits / inserts) | — | 340 / 44 | — |

Parsing the ~44 distinct expensive programs **once** instead of ~384 times
across the 24 pages cuts 27.5 % off the whole-document time. The +31 MiB RSS is
the retention that *buys* that win — the intended trade.

### Whole-document Latin control — no regression (the gate)

latin-text document (74 pages), scale 2.0, 24 runs/binary, same-binary
isolation:

| Metric | cache OFF | cache ON |
|---|---:|---:|
| Median document total | 549.2 ms | 539.4 ms |
| Median peak RSS | 629.8 MiB | 614.7 MiB |
| Cache (hits / inserts) | — | **0 / 0** |
| Pages | 74/74 | 74/74 |

This document's fonts all parse in microseconds, so the parse-cost gate retains
**nothing** — the cache is inert here and the on/off difference is run-to-run
noise. Time and peak RSS do **not** regress: gate satisfied. (An earlier
content-hash-only variant without the parse-cost gate retained 25 cheap
programs and regressed peak RSS ~+4 % for no time gain; the gate removed that.)

---

## Task 3 — Load-aware JPX decode budget (implemented)

### What changed (`crates/pdf-image/src/jpx.rs`)

A process-global atomic `JPX_IN_FLIGHT` counts decodes running across all render
workers (RAII `InFlightGuard`). When no fixed override is set, each decode's
Tier-1 thread budget is `Budgeted(clamp(cores / in_flight, 2, 8))`: a lone
decode on this 20-core host runs at **8**, a saturated scheduler settles to the
**2** cap that protects the render pool from oversubscription.
`PDF_RENDERER_JPX_CONCURRENCY` still forces a fixed `serial`/`budgeted:N` for
reproducibility (and A/B); `auto` (or unset) selects load-aware.

### Result — jpx-scan, scale 2.0833333, same-binary isolation (`budgeted:2` vs `auto`)

| Mode | Metric | Budgeted(2) | Load-aware | Change |
|---|---|---:|---:|---:|
| decode-only (full-res, single-threaded → in_flight=1 → budget 8) | median total | 144.4 ms | 130.4 ms | **−9.7 %** |
| compiled (production, reduction on) | jpx decode | 38.6 ms | 35.1 ms | **−9.0 %** |
| compiled | median total | 65.4 ms | 62.0 ms | −5.2 % |
| compiled | peak RSS | 65.0 MiB | 65.6 MiB | flat |
| both | output hash | `e046caf17a3d75bf` | `e046caf17a3d75bf` | unchanged ✓ |

Load-aware recovers latency on the single-page / viewer case (the exact
"remaining opportunity" the JPX integration doc named) without oversubscribing.

### No regression — MRC (2 concurrent JPX) and whole-document control

| Page | Metric | Budgeted(2) | Load-aware |
|---|---|---:|---:|
| mrc-jpx-jbig2 (compiled) | total / jpx decode | 101.3 / 49.4 ms | 102.6 / 49.3 ms |
| mrc | hash | `f2dc38c07f3ed0c4` | `f2dc38c07f3ed0c4` |

MRC is within noise (two overlapping decodes still land at budget 8 but bounded
by their overlap). The 74-page Latin control has **no JPX**, so `JpxCodec` never
runs and the policy is entirely inert there. Kept.

---

## Gates

- `cargo test -p pdf-render-cpu -p pdf-font -p pdf-image` — 228 passed, 0 failed
  (6 new `font_cache_tests`: content identity across independent allocations,
  cross-worker serve, parse-cost gate, LRU eviction, `None`-cache passthrough).
- `cargo check --workspace --all-features` — clean.
- CCITT hash `533b4e49e12ab195` / severity 0.032997166 — byte-identical.
- type1 hash `4cd8639fb0f5077c` / severity 0.031789505 — unchanged.
- JPX output hashes unchanged across the concurrency policy.

## Summary

| Task | Outcome | Headline |
|---|---|---|
| 1 CCITT SAT compositing | implemented | image 44.6→28.2 ms (1.58×), byte-identical |
| 2 Document-scoped font cache | implemented | 24-page Type 1 doc 1.38× faster; Latin control neutral (self-financing) |
| 3 Load-aware JPX budget | implemented | single-page jpx-scan −9.7% decode-only / −9% compiled decode; controls neutral |
