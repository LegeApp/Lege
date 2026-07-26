---
name: sweep9-analysis
description: "Sweep 9 (2026-07-22, 100,204 pages) validates the 11 post-sweep-7 fixes corpus-wide: 390 pages improved, 31 ink-regressed but only 2 structurally; plus the sweep-8 contamination lesson"
metadata: 
  node_type: memory
  type: project
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-22T10:12:55.817Z
---

Sweep 9 = `/mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/sweep9/pdfium-diff-out/results.csv`
(100,204 rows, 19,449 files, all 3 roots, scale 2.0). Validates every commit listed in
[[sweep7-followup-fixes]]. Baseline is [[sweep7-analysis]].

**METHOD LESSON — cost two sweeps.** The diff tool execs a **fresh worker binary per chunk**, so
`cargo build` while a sweep runs silently mixes binaries across the corpus. Sweep 8 was discarded
for this; sweep 9's first attempt was killed too (it predated the last two commits). The rule:
**start a sweep only from a clean, committed tree, and do not touch `target/` until it finishes.**
To keep working during a sweep, build and verify with
`CARGO_TARGET_DIR=<scratch>/target2` — a separate binary that cannot disturb the run.

**RESULT — all 11 fixes confirmed, net clearly positive.** On the 100,203 common pages:

| metric | sweep 7 | sweep 9 |
|---|---|---|
| mean inkΔ | 0.00671 | **0.00647** |
| p99 | 0.0431 | **0.0392** |
| >0.10 | 367 | **341** |
| >0.20 | 265 | **245** |
| >0.5 | 217 | **204** |
| mean gross | 0.03371 | **0.03330** |
| gross >0.05 | 21,333 | **21,049** |
| silent-blank(codec) | 123 | **112** |
| degraded(codec) | 118 | **115** |
| compile failed | 18 | **15** |

**390 pages improved >0.002; 31 ink-regressed >0.002 — but only 2 of those 31 also got
structurally worse (`gross`).** The other 29 improved or held on `gross` while the *threshold* ink
metric moved the wrong way, which is the known metric artifact: a page that was over-inking and now
under-inks scores worse on ink even though pixel agreement improved. Clearest cases:
- `2385_1` p1 ink 0.0722→0.1599 but **gross 0.4059→0.1993**; p0 ink 0.0996→0.1334, gross 0.3644→0.1535
- `Brittany Gilmer` p34 ink 0.0046→0.0089 but **gross 0.1635→0.0202** (8x better)
- `e000282` p8 ink 0.0015→0.0099 but **gross 0.1764→0.0416**
- `5250_2` p0 ink 0.0275→0.0725 but gross 0.2025→0.1132

**The only two genuine (structural) regressions were `issue5939` p0/p1 — RESOLVED, and the
scoping was not at fault.** Bisecting showed the resource-cache scoping (`f5063f7`) moved them, and
narrowing cache-by-cache pointed at `font_cache`. But the render diff was **not** a font difference
at all: glyphs and layout matched PDFium exactly and only the *colour* differed — PDFium painted the
"DISTRIBUTED" watermark, byline and drop caps pale mint, we painted them hot magenta (a threshold
ink metric reads pale as un-inked and saturated as inked, hence the large ink delta from a pure hue
change). Root cause: the form redefines `/CS0` as `/Separation /PANTONE#203278#20U` whose alternate
is `[/ICCBased 245 0 R]` — a 3-component profile whose header declares **`data-space = 'Lab '`**.
Its tint emits L*=56.5 a*=-43 b*=2 (mint); read as RGB that is magenta, because a negative a* is
green and a positive one magenta. **So the cache scoping was correct — the form really does
redefine `/CS0` — and it exposed a real colour bug.** Fixed in `ad61d6a` by reading the ICC header's
declared data space (bytes 16..20, ICC.1 §7.2.6) instead of inferring the family from `/N`; only Lab
is intercepted, and ICC Lab's wider -128..127 a*/b* range is used so saturated spot colours are not
clipped. Every page of issue5939 is now better than before *either* change (p0 0.0585→0.0034 against
a pre-scoping 0.0136). The same bug explains the `5464.pdf` movements that had been sitting in the
regression column (p34 0.0664→0.0037, p68 0.0656→0.0046, p51/p85/p17 likewise) and improves
EN-05-10137 ×6 further (~0.025→~0.006, gross ~0.16→~0.04).

**Lesson worth keeping: a large ink delta with matching geometry is a colour bug, not a font or
layout bug.** Two hypotheses were wrong before that (duplicate `SemFont`s; missing page-resource
inheritance) — both were implemented, measured as no-ops, and one reverted.

**Top improvements:** issue17554 1.0000→0.0000, close-path-bug 1.0000→0.0001, 4326 and 3246 ×2
0.9971→0.0000, issue11230 0.9804→0.0005, issue8614 ×2 0.88→0.0004, flate_predictor_bpc_1
0.7254→0.0001, 5302 0.6933→0.0062, issue8565 0.5370→0.0015, image_inline_5 0.4644→0.0196,
issue4648 0.2545→0.0008, Howard Turner 0.2424→0.0014.

**NEW BUG SURFACED AND FIXED — `2385_1.pdf` inline framing (`dfdfab7`).** I first recorded this as
"BPC-4 indexed inline sampling is broken" — **that was wrong**. The samples and palette were always
fine; it was a *framing* bug in the tokenizer. `inline_ncomp` only understood a `/CS` **name**, so
the written-out array form `[/I /RGB <hival> <palette>]` yielded no exact length and the tokenizer
fell back to scanning for a whitespace-bounded `EI` — which 262,260 bytes of palette indices hit
long before the real end, cutting a 1128x465 map to a few rows (the "grey band over white" render).
Now the array head decides the sample component count; `/ICCBased`, `/Separation` and `/DeviceN`
still take the scan since they cannot appear inline (no indirect refs). Results: 2385_1 p0 ink
0.0996→0.0026 / gross 0.3644→0.0334, p1 ink 0.0722→0.0026 / gross 0.4059→0.0434, 2385_2 p0
0.1088→0.0080. Checked over 495 inline-image-containing corpus files (2,316 pages): 1 changed,
0 regressed.

**Note on that misdiagnosis:** the earlier greps that said "2385_1 has no inline images" were run
against *compressed* content streams. **Decompress before grepping content operators.**

**Also landed after the sweep:** `dfdfab7` inline-image framing (above), `ad61d6a` ICC Lab data
space, `c178463` page-`/Resources` category inheritance (a faithful PDFium port —
`CPDF_StreamContentParser::FindResourceHolder` — that measured as **0 changed pages of 3,105**;
kept because partial form `/Resources` are common, not because the corpus asked for it).

**Unchanged classes** (not touched this round): `pdfium open failed` 126 (oracle's problem), our
`open failed` 21, the tone/threshold class, the perf SIGKILL/timeout files.
