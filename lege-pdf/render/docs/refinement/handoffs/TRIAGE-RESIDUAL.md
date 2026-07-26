# Residual triage — sweep-3 drop classes after A/C/JPX (2026-07-19)

Living document for DAYPLAN Session 1 task 4: rank what remains after the three
landed workstreams (failure-tolerant parsing, 4-component DCT polarity, JPX).
Built from targeted `pdfium-diff --rerun-failures` runs on Windows (project
`pdfium.dll`, scale 2.0), not the full Linux sweep. Each row's cause is named so
sessions 2–4 have an exact scope.

## Method

`pdfium-diff --rerun-failures <prior results.csv> <class> <pdfium.dll> <scale>`
(new mode, `tools/pdfium-diff/src/main.rs`) re-grades only the documents a prior
sweep recorded in a drop class. Classes (PLAN-SWEEP3 §1):
- **failed** — hard `compile failed`/open/panic rows.
- **blank** — `ours_ink < 0.1 && ref_ink > 0.1` (silent-blank gate).
- **destroyed** — `ink_delta > 0.3` on a non-blank page.

Prior CSV: `oracle-sweep-2026-07-18/pdfium-diff-out/results.csv` (65,990 keys).
Output: `tools/pdfium-diff/pdfium-diff-rerun-out/results.csv` (separate dir; the
baseline is preserved). Note the current `pdfium.dll` decodes some CMYK images
the sweep build blanked, so a few baseline `ref_ink` values shifted — the oracle
is now stricter, not looser.

## Class: hard failures — RESOLVED ✅

`--rerun-failures … failed` → **17 documents, 0 unopenable, 0 compile-failed,
0 silent-blank/degraded.** All 21 former `compile failed` pages render; 20 match
PDFium at inkΔ < 0.01, the lone outlier Cambourne p25 at inkΔ 0.022 (noise band,
not a drop). Workstream A gate met. Detail in DEFERRED.md item 3.

## Class: destroyed (over-ink / CMYK) — DONE, verdict is NUANCED

`--rerun-failures … destroyed` → **1248 pages, 220 files, 0 silent-blanks,
10 degraded, 420 suspect.** Cross-referenced against the prior CSV
(`destroyed-rerun-backup/results.csv` is the archived output):

**Darker (over-ink, ours > ref) — 198 rows → 135 cleared (inkΔ ≤ 0.05), 60 still
> 0.05, 3 unsampled.** Workstream C fixed the **Adobe YCCK CMYK JPEG** subset
(Nomad Flute etc.). The **60 residual are a DISTINCT class, not the YCCK fix:**

> **NEW FINDING — Separation/DeviceN-colorspace images render inverted.**
> Representative: *One Zambia, Many Histories* (Afrika-Studiecentrum Gewald) p0,
> ours 0.98 vs ref 0.013. Its images are **1-component `/DCTDecode` in a
> `[/Separation …]` colorspace** (no `/Decode`). The sample is a *tint*: a
> near-black scan (Pillow mean 9) is tint ≈ 0.035 → almost no ink → white, which
> PDFium renders. We treat the 1-sample image as **DeviceGray** (9 → near-black)
> → 98% ink. DEFERRED notes Separation is done for *fills/strokes* but **images
> still map by arity** — this is that gap, and it is the dominant residual
> over-ink cause (~60 pages, many documents). Also seen: Michael Pearson *Indian
> Ocean*, Gladstone (Partridge), *rosiesmenu3*, Germany's Genocide.
> **FIXED 2026-07-19.** `build_tint_image_lut` bakes the tint transform into a
> 256-entry `ImageColorSpace::TintLut` (sample→sRGB); the CPU sampler indexes it
> by the `/Decode`-normalized sample. Also required: `lower_image` no longer lets
> a codec's Gray format override a reinterpreting PDF colorspace (a Separation
> image is usually a 1-component JPEG). Verified end-to-end: *One Zambia* p0
> 0.97 → **0.0067** ink (PDFium 0.013); Pat Caplan p47, Finance Capital p0,
> Kalevi Keskinnen p0 all >0.5 → ~0.01–0.03. Test:
> `tint_lut_separation_image_routes_samples_through_lut`. See DEFERRED.md Phase 3
> Separation/DeviceN. *Deferred*: multi-input DeviceN images (no 1-D LUT).

**Lighter (missing-ink, ours < ref) — 721 rows → 85 cleared, 39 still > 0.05,
597 unsampled** (the 6-page sampler didn't hit those exact pages). The residual
mixes JPX blanks now flagged `degraded(codec)` (B3 working — e.g. Weitz
*Creating German Communism*) with silent missing-ink.

> **Indexed `/Decode` fix (spec-correct, unit-tested) + an unresolved Paula
> tail.** A real latent bug was found and fixed: the CPU Indexed sampler read
> the raw sample as the palette index, **ignoring `/Decode`**. Per §8.9.5.2 the
> Decode array remaps sample→index (e.g. `[0 255]` on a 1-bit image sends
> sample 1 to index **255**, not 1). Fixed in `image.rs` (apply `comp(0)` when
> Decode is present); test `indexed_decode_array_remaps_sample_to_index`. BUT
> the representative — Paula Holmes-Eber *Daughters of Tunis* p0
> (`[/Indexed]`, 1-bit, `/Decode [0 255]`, 3508×2474; ours 0.045 vs sweep-ref
> 0.99) — **did not change** (still 0.045). So Paula's over-dark ref is likely
> **stale** (the current `pdfium.dll` renders it differently than the sweep
> build — a known shift), or a deeper sample-read issue. **Needs a fresh
> single-file oracle check with the current `pdfium.dll`** before treating it as
> a real drop; the Decode fix stands on its own merit regardless. Several other
> "lighter" residual rows may be the same stale-ref artifact — re-verify.

> **CAVEAT on these rerun counts:** the destroyed/blank reruns used a
> `pdfium-diff` binary built **before** the Separation-image fix landed, so the
> 60 Separation residuals above are already fixed (4 verified end-to-end) but
> still appear in this run's numbers. A fresh `--rerun-failures destroyed` (queue
> after the blank run) will show them cleared.

**10 degraded rows** = JPX residual codec drops correctly surfaced by B3 (these
are the jp2lam handoff's precinct/DWT/truncation tail, now visible not silent).

### FRESH destroyed rerun (2026-07-19, Separation-fix binary + new jp2lam)

Re-ran `--rerun-failures destroyed` after the Separation fix: **1248 pages, 220
files, 0 silent-blanks, 9 degraded, suspect 380** (was 420). The over-ink
(darker) class: **176/198 cleared** (was 135/198 pre-fix) — the Separation fix
took the class from 60 residual to **19**. Those 19 (`ours` ≈ 1.0, whole page black) were then **almost entirely resolved
by two more fixes** (a definitive rerun with all fixes is running to confirm):

- **Indexed cover images → FIXED by the Indexed-`/Decode` change.** Michael
  Pearson *The Indian Ocean* p0 (obj 1177, 8-bit `[/Indexed]` full-page cover)
  and both Gladstone editions p0 were solid black; with the Decode fix they
  render **0.17 / 0.19 ink — matching PDFium 0.175 / 0.192**. (The `[/ICCBased
  n=2]` note earlier was from a *different* Pearson file; the actual broken image
  was Indexed.)
- **parisut port → FIXED by the indirect-`/DecodeParms` change (the big one).**
  Its full-page DeviceRGB cover used `/Filter/FlateDecode /DecodeParms 51 0 R`
  (an **indirect** PNG-Predictor-15 dict). We never resolved the indirect ref,
  silently skipped the predictor, and returned the raw filtered deltas — a
  near-black smear (ours 0.997, PDFium 0.225). Resolving indirect `/Filter`/
  `/DecodeParms` in `pdf-document` fixes it: **mean 214, exactly matching
  PDFium's render.** This affects *every* Flate/LZW image with a shared indirect
  predictor dict — a common pattern, likely helping far more than these 19.
- **rosiesmenu3 / Aksan — a DIFFERENT tail (transparency compositing), not
  image colorspace.** Unchanged by the image fixes. rosiesmenu3 p4's display
  list is transparency-heavy: **non-isolated groups (`isolated=false`), opacity
  0.5, soft masks, blend modes**, `DeviceGray(0)` fills with alpha. That is the
  DEFERRED "non-isolated transparency groups rendered as isolated" + soft-mask
  approximation class — a rendering-quality item, separate from the image-
  colorspace gap (which is now fully resolved). Left to the transparency pass.

**Bottom line:** the *image-colorspace* over-ink class is resolved by four
fixes (YCCK-CMYK, Separation-image, Indexed-`/Decode`, indirect-`/DecodeParms`).
Definitive `destroyed` rerun with all four: **182/198 over-ink rows cleared**
(progression 135 → 176 → 182); the remaining 13 are the transparency-
compositing tail (non-isolated groups + soft masks — e.g. rosiesmenu3, Aksan),
a separate deferred class. Note this measures only the narrow `inkΔ>0.3`
darker class; the indirect-`/DecodeParms` fix (any Flate/LZW image with a shared
predictor dict) also helps the broader `bad`/noise bands — that shows up only in
a full sweep, now wired to include the renderer-corpus.

### Residual over-ink, ranked (pre-Separation-fix snapshot, superseded above)

| document (repr.) | page | inkΔ | ours | ref | cause |
|---|---:|---:|---:|---:|---|
| One Zambia (Gewald, Afrika-Studiecentrum) | 0 | 0.967 | 0.98 | 0.013 | **separation-image** |
| Germany's Genocide (Sarkin-Hughes) | — | 0.837 | 0.86 | 0.028 | separation-image (verify) |
| Indian Ocean (Pearson) | — | 0.825 | 1.00 | 0.175 | separation-image (verify) |
| Gladstone (Partridge) ×2 | — | 0.808 | 1.00 | 0.192 | separation-image (verify) |
| rosiesmenu3 | — | 0.806 | 1.00 | 0.194 | separation-image (verify) |
| Daughters of Tunis (Holmes-Eber) | — | 0.90+ | 0.02–0.20 | 0.998 | lighter — colorspace inversion (verify) |

## Class: blank (JPX) — census DONE (98.6% decode), page rerun in flight

**Fresh per-stream census with the new jp2lam (precincts/SOP/EPH, all
progression orders, degenerate DWT, multi-segment Tier-1):** over all 451
blank-corpus documents, **1207 / 1224 JPX streams decode (98.6%)** — up from
791/822 (96.2%) before this jp2lam round. The 17 residuals are the jp2lam tail,
now all B3-visible (never silent):
- **9 zero-byte code-block contribution** (Stalin) + **4 data-after-packets**
  (Imre Nagy, Prussian) — the deliberate truncation strict-failures documented
  in `jp2lam/HANDOFF-remaining-jpx-decode.md` (§3/§4; salvage produced visible
  corruption, so strict failure is kept).
- **2 tile-part QCC override** (Szepe *Painters and Patrons*) — main-header QCC
  is handled; the tile-part variant is not. Likely small for the jp2lam owner
  (route the tile QCC like the main one). *Relayed to jp2lam.*
- **1 `box extends past input`** (Frederick Thomas Jane) — a container that is
  NOT a bare SOC codestream (the raw-J2K path doesn't catch it); probably a
  genuinely truncated/odd box. *Relayed to jp2lam.*
- **1 `packet header ended before requested bit`** (*Profit Over People*) — a
  truncation variant. *Relayed to jp2lam.*

Net: the JPX blank class is essentially resolved at the decode level; the tail
is jp2lam's (2 deferred-by-design + 4 small new items for that session).

## Class: blank (JPX) page-level — PENDING ⏳

`--rerun-failures … blank` (451 blank-JPX documents) not yet run (single oracle
process; runs after `destroyed`). Expected: the silent-blank class collapses to
the census residual (~4% of JPX streams — precincts/degenerate-DWT/truncation),
each of which must now carry a `silent-blank(codec)` note (B3), never a clean
score. The jp2lam-side fixes for these are the separate handoff
(`jp2lam/HANDOFF-remaining-jpx-decode.md`); this run just measures the
page-level residual.

- [ ] Run `--rerun-failures … blank` after `destroyed`.
- [ ] Count remaining silent-blanks; every one must have a `degraded > 0` /
      `silent-blank(codec)` note. Any *clean-scored* blank is a B3 gap → bug.
- [ ] Cross-reference remaining blanks to the census error classes (precinct /
      degenerate-DWT / truncation) so Session 2 knows the corpus weight of each.

## Session-scope decisions this sheet feeds

- **Session 2 (jp2lam):** weight precincts vs DWT vs truncation by the blank
  rerun's per-class corpus counts.
- **Session 3 (color):** start only if the destroyed rerun leaves a real
  `icc`/`lab` tail — named rows above are the fixtures.
- **Session 5 (sweep 3):** these reruns are the pre-sweep confidence that
  `hard_drops` and the CMYK `destroyed` rows are already 0 before paying 2 h.

## Not re-verified by fixture (covered by oracle instead)

The plan's second fixture — false-`EI` inline-image regions from a 2021 + 2024
statement — is **superseded** by the `failed`-class oracle rerun: all seven
statement pages render at inkΔ 0.0002–0.003 vs PDFium, a stronger check than a
synthetic unit fixture (which already exists as
`inline_image_length_frames_past_false_ei` et al. in `pdf-content`). Not
extracting the content-stream region; noted here so it isn't mistaken for a gap.
