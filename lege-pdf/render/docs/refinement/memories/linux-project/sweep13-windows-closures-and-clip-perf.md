---
name: sweep13-windows-closures-and-clip-perf
description: "2026-07-24 (Windows): closed the two still-open sweep-12-tail items (JBIG2 custom-AT refinement, Huser /DefaultCMYK CLUT); found a big clip-mask perf class. Only nested-soft-mask compositing remains from the tail."
metadata:
  node_type: memory
  type: project
  originSessionId: 807e0f39-4652-4c65-b926-8f8c4454231e
  modified: 2026-07-24T00:00:00.000Z
---

Follow-on to [[sweep12-tail-fixes]], run on the **Windows** box (Linux has no
full fan control — thermal). Closes 2 of its 3 remaining open items; only
nested-soft-mask compositing is left.

**1. JBIG2 custom-AT refinement — DONE, committed `jbig2enc-rust` 027a485
(branch sweep12-codec-fixes).** The tail note's "three faults" triage was
superseded once the code was read: the real fault is the **GRAT2 reference
adaptive pixel** (T.88 Fig. 12, context bit 8 = `GRREFERENCE(w+GRAT2, h+GRAT2)`,
per pdfium `DecodeTemplate0UnoptCalculateContext`) plus the GRTEMPLATE-1 TPGRON
SLTP context (centre-reference-only value = bit 7 = 0x080, not 0x040). A prior
agent fixed the *standalone* refinement region; the **symbol-dictionary (SDRAT)
and text-region (SBRAT)** paths parsed the second AT pair and *discarded* it,
hardcoding `(-1,-1)` — so custom-AT symbols desynced ("more symbols coded than
SDNUMNEWSYMS"). Threaded `grat2`/`sdrat2` through `decode_refagg_symbol`, the
Huffman refagg path, and both text-region cores (`TextArithParams`/
`HuffTextParams` gained a `grat2` field). All 96 pdf.js `bitmap-*` fixtures now
match pdfium (Δ≈0.0024), 0 silent-blanks; un-blanked `symbolrefineone-customat`
and `symhuffrefine-textrefine`. Regression test
`context_gr0_reads_the_custom_grat2_reference_pixel`.

**2. Huser `/DefaultCMYK` — DONE, committed `pdf-renderer` 188210c (fmt split
into a264b40).** Reframed on inspection: the cover's coloured fills are **PANTONE
`/Separation` spaces whose *alternate* is the ICCBased CMYK press profile**, not
a plain `k`-operator/DefaultCMYK case. New `pdf_color::icc::IccCmyk` evaluates an
`A2B0` `mft2`/`mft1` CMYK→Lab lut (input curves → 4-D n-linear CLUT → output
curves → **v2** Lab(D50) → Bradford D50→D65 → sRGB), matching PDFium's LittleCMS
path (`INTENT_PERCEPTUAL`, no BPC). Validated vs Pillow/lcms2 on the real Huser
profile: mean 1.2 units/channel, 96.75% within ±4. Wired three routes in
`interpret.rs`: a `/DefaultCMYK` redirect for `k`/`K` and `/DeviceCMYK cs`; a
direct `[/ICCBased]` CMYK `scn`; and the actual cover fix — a
`TintSpace.alt_icc_cmyk` field so a Separation/DeviceN whose alternate is an
ICCBased CMYK profile runs its tint output through the profile. Spot-colour
*images* inherit it via the tint LUTs (so `issue6364`'s "same class as Huser"
residual should improve too). Huser cover p0 **0.158→0.0039**; whole book ≤0.005;
no regressions (469 pdf.js pages mean 0.0041, 180 CMYK-corpus pages). A follow-up
extends `convert_special_image_samples` to **non-codec** ICCBased-CMYK images
(uncommitted, pending post-sweep validation); **codec (CMYK JPEG) images** still
use the frozen table and need the profile carried into the IR — sweep-gated.

**Still open from the tail:** nested-soft-mask compositing (Uyghurs sunburst arc
+ `resvg_masking_mask_recursive_on_self`) — a mask group whose own luminosity
must be reduced by another mask. Transparency-backend work, untouched.

**NEW perf class — fractional-edge rect clips build full-page masks.** In
`pdf-render-cpu/src/prepared.rs`, `is_rect` requires `is_integer_rect` (every
corner within 1e-4 of an integer device pixel). Axis-aligned rectangle clips with
**fractional** device edges (the norm at scale 2.0 with fractional PDF coords)
fail it → classified `ClipKind::Path` → `has_mask=true` → a full-page `Alpha8`
mask **per clip**. "Vocal pitch exercises .pdf" (vector music) has ~1981 `re W n`
clips/page → `render.path` = **2.6 s/page** (compile only 11 ms; ~50× pdfium),
**output correct** (sweep-12 ink 0.003). It timed out sweep-13 on Windows
(120 s + contention; fit under the Linux sweep-12 budget). Fix for the
optimization pass: handle axis-aligned fractional rect clips **analytically** — a
separable H×V AA-coverage envelope, O(w+h) not O(w·h) — taking these pages to
~ms. This is a **class** (clip-heavy vector pages: music, diagrams, OCR overlays),
likely behind other slow/timed-out sweep files.

**SWEEP 13 RESULTS (2026-07-24, pdfium-only, 18,956 files / 96,298 pages).**
93.48% clean (≤0.01), 5.63% noise, 0.25% bad (0.05–0.30), 0.64% >0.30. The
>0.30 bucket is mostly **pdfium's own** open/render failures (539 "pdfium open
failed" etc. — ours renders, pdfium doesn't → inkΔ 1.0), not our bugs. **All
prior closures validated corpus-wide**: Huser 0.005, bitmap-customat 0.0024,
issue9462 0.0032, Uyghurs 0.0073, issue6364 0.0027 (CMYK-ICC image class cleared
via the eval_tint fix, as predicted). Genuine our-side tail: 9 timeouts, 9
compile-failed, 1 OOM crash, 15 codec-degraded, 240 bad pages (199 ours-lighter
= scanned-book under-ink, one raster-quality class).

**Fixes landed this pass (all committed on pdf-renderer sweep12-tail-fixes):**
- **`8743526` dash OOM.** `dash_polyline` emitted one Vec per dash period
  unbounded; a 14 KB file (467170761.pdf, `[…~0.0001] 0 d`) drove a 24 GiB
  alloc → 0xc0000409 abort (the all-zero `total<=0` guard misses a tiny
  *nonzero* period). Now renders solid past 100k periods (invisible dash) +
  phase reduced into one cycle. **Cleared the OOM AND 3–4 hayro-fuzz timeouts**
  (373764900/42271177/42271107 now <1.3 s).
- **`c6c3760` fractional rect-clip perf.** `is_rect` required `is_integer_rect`;
  fractional-edge axis-aligned rect clips (the norm at scale≠1) fell to a
  full-region `Alpha8` mask *per clip* — the "Vocal pitch" 1981-clips/page trap.
  New `is_axis_aligned_rect` takes any axis-aligned rect as a bounds clip (hard
  edge ≤1 px vs AA mask — **inkΔ identical**: Vocal p0 0.0032 either way).
  Vocal 3410→170 ms (20×); mbta transit map timeout→1.3 s. The opaque-rect
  *fill* fast path still requires integer alignment (fills need real AA).
- **`95aa7ff` non-codec ICCBased-CMYK images** through IccCmyk (sibling of the
  RGB arm); codec CMYK JPEGs still frozen-table (IR-carry is the follow-up).

**Post-sweep-13 fixes (Fable-subagent advised, all committed):**
- **`7d135e3` direct-object page leaves.** `PageRef.object` → `Option<ObjectId>`;
  a *direct* inline page dict in `/Kids` (not an indirect ref) was dropped
  ("direct-object page leaf skipped") → whole doc = 0 pages. Now rendered
  (rendering reads contents/resources, never the id; the two pd-read doctor
  sites skip when it's None; the (0,0) placeholder sentinel retired). Fixes
  issue9540 (0→1 pp, inkΔ 0.016), 42271379, 42271520.
- **`dcbec96` zero-page rebuild escalation.** A file loads a *clean* xref chain
  yet walks to 0 pages because a `/Kids` ref resolves to a null the xref points
  at a wrong offset — invisible to load_structure's root-level rebuild gates.
  `build_page_tree` now returns `WalkStats{real_pages,lost_subtrees}`; open
  escalates to `load_structure_rebuilt` (new, forces PDFium-style RebuildCrossRef)
  when real_pages==0 && lost_subtrees>0, adopting only if strictly more pages
  (genuinely-empty docs never escalate). Fixes pdfbox/3977 (0→1 pp, inkΔ 0.0037).
  0380221 remains 0 pp — its pages live in object streams the rebuild can't
  index a valid `/Kids` for; needs **orphan `/Type/Page` recovery** (scan all
  indexed objects, PDFium's page-find fallback) — a separate larger fix, deferred.

**Item 3 (scanned-book under-ink) — DIAGNOSED as a metric artifact, NOT ink
loss.** sexcharacter00wein p65 (ours 0.237 / pdfium 0.308 thresholded ink):
profile shows glyph_blits=0, render.path=0 → **pure image** (2 RGB draws, one
minified, one masked), ruling out stroke/glyph/AA. Triptych analysis is
decisive: **continuous ink matches — ours 0.2406 vs pdfium 0.2417 (Δ0.001)** —
we lay down the same ink *energy*. The divergence is only in the diff tool's
**thresholded** metric (`ink` = frac pixels with luma<200, compare.rs:22): our
minification gives a **bimodal/high-contrast** histogram (11.8% dark, ~0%
mid-gray, 88% light) while pdfium **spreads ~10% into the mid-grays [64-191]**
from its softer image stretch / mask anti-aliasing — those gray edge pixels fall
under 200 and count as ink. Not controlled by `FPDF_RENDER_NO_SMOOTHIMAGE`
(toggling it left pdfium's output unchanged → it's the CStretchEngine / mask AA,
always on). **Conclusion:** the 199-page class is energy-correct rendering the
thresholded metric penalizes for crisper edges. Not worth blurring our output to
chase pdfium's particular smoothing (ink energy is right; MuPDF, not pdfium, is
the primary oracle). Better response: treat it as a metric artifact / de-
prioritise, or refine the diff metric toward continuous ink. Left unchanged.

**Item 1 (AET coverage kernel) — DONE, committed `db3a9cd`.** The sparse/
oversized-window scanline fallback (`fill_scanline_ref`, raster.rs) rescanned
every edge against every band row — O(edges×rows); basinmap (1.66M edges/8932
paths) spent ~140s there. Added `fill_scanline_aet`: CSR counting-sort edges
into activation-row buckets (scattered in edge-index order → buckets
index-sorted for free), per-row linear merge maintaining an index-sorted active
list with order-preserving retirement, accumulate only active edges. **Bit-exact**
because contributing edges (passing `yb<=ya`) accumulate in identical
edge-index order — the naive `fill_scanline_ref` is kept solely as the
byte-identity oracle (`aet_matches_reference_bit_exact` + edge-case test). The
MAX_WINDOW/sparse route now points at the AET. **basinmap p0 140.6s→62.0s
(2.3×)**, clearing the 120s timeout; residual is the inherent 1.66M-edge cost.
Fable-subagent's two key catches: don't convert `fill_scanline_ref` in place
(it's the test oracle — add a 3rd fn), and use CSR+merge not insertion-sort
(quadratic at 1.66M edges).

**basinmap ROOT CAUSE FOUND + FIXED (`856c389`) — it was clipped TEXT, not strokes.**
Two Fable-subagent passes plus proper attribution cracked it. The stroke/AET/
hairline theories were ALL the wrong subsystem. Per-command timing split of
`render.path`: draws/strokes = **409ms**, `raster.fill`+blend = **58ms**, but
**text glyph runs = 7204ms (95%)** — ~9887 clipped single-glyph labels at 728µs
EACH. The cost was `raw_mask.is_some_and(|cm| cm.data.iter().all(|&v| v==255))`
— the "is this clip mask all-opaque so I can drop it" test — run **per draw** as
an **O(mask area) scan** (exec.rs glyph site + image site). On clipped-text
pages that's O(draws × mask area). Fix: compute `all_opaque` ONCE in
`build_clip_mask` (carried on `ClipMask`, mask.rs), read O(1) at the draw sites.
Byte-identical output (basinmap ink 0.00433 before/after; 56 render-cpu tests
green). scale-0.5 render.path 7700→2177ms; larger win at the scale-2 timeout
(scan cost ∝ mask area) — not re-measured at scale 2 (thermal). **Lesson: the
prior AET commit `db3a9cd` optimized a 58ms path; always time where the wall-
clock lives BEFORE optimizing (the subagent's "attribution first" call was
right). Likely also speeds rica (2018 clips + 2017 clipped text) and any
clipped-text page.**

**basinmap RESIDUAL RESOLVED (`f056ec2`, 2026-07-24) — the subagent's shared-
ancestor model was WRONG for this file; attribution corrected it again.** The
Fable-subagent's two follow-ups (pure-rect chains maskless; memoize shared
ancestor) assumed the labels shared one big parent path clip. Instrumenting
`build_clip_mask` (chain-kind + count + size buckets) showed the opposite: the
clip chains are `[Path-leaf, Rect-parent]` — **each label's clip IS its own
distinct large basin polygon**, not a rect child of a shared path. 890 builds
(97% >100k px, 559M mask px total), 1.87ms each = **1710ms**, with only 372ms of
actual glyph blitting. No shared work to collapse — it's 890 genuinely-independent
large path rasterizations. Fixes landed (all byte-identical, basinmap ink 0.00433
unchanged; render-cpu tests green):
- **Parallel mask pre-build.** New `prebuild_masks` (exec.rs) builds every
  *consumed* clip mask across the rayon pool before the serial draw loop, via a
  new `Sync` `ClipGeom` view of the page (full page is `!Sync` through its
  diagnostic `RefCell`/`Cell`). Each mask is a pure fn of the page, order-
  independent → safe. `map_init(RasterKernel::default,…)` = one reused scratch
  kernel/thread. Gated past 16 masks so ordinary pages keep the lazy path. Peak
  memory unchanged (every consumed mask was cached by page-end anyway — this only
  front-loads + parallelizes). **This is the win: serial `render.path` 2082→57ms
  (36×), wall ~2260→~460ms (~5×).**
- **single-path fast path in `build_clip_mask`** — fill the first (usually only)
  path clip straight into `acc` (init 0) instead of into zeroed `tmp` multiplied
  in; skips a per-pixel pass + a large alloc for the common one-path chain.
- **`PreparedClip.mask_source`** (nearest path-clip ancestor) — rect descendants
  of a shared path clip share one cached mask. No-op for basinmap (leaf IS the
  path) but a correct general win for the shared-ancestor pattern in other PDFs.

**THERMAL NOTE CORRECTED: the "heat" was Mercury (a Firefox multithread fork)
eating CPU threads, NOT thermal throttling.** The 62→88s basinmap drift across
identical runs earlier was CPU contention from Mercury, not fans. With it closed
the box is fully usable for heavy renders; the "run on Windows because Linux
can't control fans" constraint stands, but per-run timing noise was Mercury.

**TRIAGE CORRECTION (basinmap is NOT AET-bound at typical scales).** Instrumented
the coverage kernel: at scale 0.5, **99% of basinmap's ~8000 fills use the FAST
path (2D deposit), only ~1% hit the AET fallback**. The 2.3× the AET gave at
scale 2 is scale-dependent — bigger bboxes route more fills to the fallback
there; at screen scale it's fast-path-bound. Root cause of the MuPDF gap
(SumatraPDF renders it in ~2s vs our 62s): the file is **~8224 sub-pixel hairline
strokes** (line width **0.0576** user-units, 1272×; miter joins / square caps
dominate, so NOT round-tessellation) that we expand to fill-quads → **1.85M+
fill-edges across 8000 separate fills**, each with per-fill setup. MuPDF strokes
directly / has a hairline fast path and doesn't fill-expand. **So the real
basinmap fix is a stroke-rasterization optimization (hairline fast path for
sub-pixel strokes + edge reduction + lower per-fill overhead) — a separate,
larger project; the AET is a valid improvement for genuinely edge-dense
FILLS but does not close this gap.** rica (29s): 2018 strokes + 2017 clipped
text + 2018 clips — stroke+text mix. Codec-degraded 15: all "undecodable codec"
image drops (JPX/JBIG2/etc.), varying counts (1-30 draws/file) = distinct
edge-case causes, not one shared bug; documented-deferred, low priority.

**Fable-subagent advice on the OTHER remaining issues (not yet done; recommended
order 3→4→2):**
- **Item 3 (metric):** REFINE, don't leave it. In compare.rs:64-86 add continuous
  ink `mean((255−luma)/255)` for ours/ref; change `is_suspect` (compare.rs:49) to
  require corroboration `(ink_delta>0.01 && cont_delta>~0.003) || gross_frac>0.05`,
  keep `severity()` on the hard delta. Blanks/box-fills still flag (they move
  continuous mass); only the crisp-vs-soft-AA artifact (Δcont 0.001) goes quiet.
  Tooling-only, no renderer. Do FIRST — de-noises every future sweep triage.
- **Item 4 (triage, nearly free):** don't assume AET subsumes all timeouts —
  classify rica(29s)/bug1019475 via `pdf-cli dump` display list without rendering
  (few-paths×huge-edges=AET-class; many-small-draws=per-op; deep groups=
  compositing; awkward images=sampling). Codec-degraded 15: surface the
  `note_degraded` reason strings (prepared.rs:1024-33), bucket by (codec,reason),
  cross-check `/Producer` — identical strings across files = one shared cause.
- **Item 2 (orphan /Type/Page, do LAST — biggest/adversarial surface):** extend
  the escalation (lib.rs:411-435) so when the rebuilt walk ALSO yields real_pages==0,
  scan all live xref entries (incl. ObjStm members) for `/Type/Page` dicts w/o
  `/Kids`, synthesize PageRefs in object-number order with depth-capped `/Parent`
  inheritance; adopt only if ≥1 found. Needs a small `XrefMap::iter_live`/`live_id`
  accessor (generations private). Gate on real_pages==0 from BOTH walks; cap at
  max_pages by TRUNCATE+RecoveryEvent not hard Err. **Also fix a gating hole while
  there:** "/Kids is not an array" (pages.rs:152-157) doesn't increment
  lost_subtrees, so a garbage-root-Kids doc gets real=0/lost=0 and never escalates.
  MuPDF has NO orphan synthesizer (checked pdf-repair.c) so object-number order is
  the heuristic. This is what 0380221 needs.

**Remaining tail, by class (ranked):**
1. **Coverage-kernel perf** — the remaining timeouts (designated_basinmap 140 s:
   1.66M edges / 8932 stroked paths, **pixel-bound** — scale 0.5 → 10 s; also
   bug1019475_1, rica 29 s). This is the documented analytic-coverage seam
   (AET/SIMD `KernelSet`, advice §3/§7) — a real optimization-pass item, not a
   quick fix. Lead perf target now.
2. **Page-tree recovery** — `issue9540` (a *direct-object* page leaf dropped —
   needs `PageRef.object` → `Option` across pdf-document+pdf-read) and
   `0380221.pdf` (a `/Kids` null node → 0 pages; needs XrefRebuilt escalation
   when the walk yields far fewer than declared). pdfium renders both. Each
   un-blanks a whole document but is a moderate multi-crate change for few files.
3. **Scanned-book under-ink** — 199 of the 240 bad pages are ours-lighter (thin
   strokes/light scan reproduction). The biggest *count*, a raster-quality class,
   lower per-page value.
4. Adversarial declines (operator_list_cycle, recursion-depth-16, lopdf fuzz) —
   correct, leave. 15 codec-degraded = scattered JPX/JBIG2 edges.

**SWEEP 14 (2026-07-24, pdfium-only, partial 86% = 16,832/19,449 files /
92,361 pages; stopped early by an unknown external kill, NOT the user, NOT a
crash — 0 failures/0 timeouts at the cut).** Distribution vs sweep-13: 93.71%
clean (was 93.48%), 5.59% noise, 0.21% bad, 0.49% >0.30 — **slightly better in
every bucket → NO regression from the parallel clip-mask work** (`f056ec2`),
matching the byte-identical proof. (Couldn't per-file diff vs sweep-12: those are
Linux `/mnt` paths vs Windows `D:\`, zero key overlap — distribution parity is
the regression check.) Actionable tail = 648 rows: **432 files = pdfium's own
open/render failures** (`note: pdfium open failed`, ours=0/ref=1/p-1 — same
not-our-bug class sweep-13 saw; list saved to scratchpad `pdfium_fail_files.txt`,
all 432 present on disk) → **user's next task: re-run these 432 vs MUPDF** (need
a Windows `mutool.exe`; repo only has the Linux `.renderer-bin/mupdf/mutool`);
70 mixed moderate scanned-book residuals; 23 under-ink (metric artifact).

**THE 432 "pdfium open failed" FILES — INVESTIGATED (2026-07-24), mostly NOT
bugs.** Set up a Windows mupdf oracle to re-check: downloaded official MuPDF
**1.28.0** Windows release (Linux side was 1.29.0, no prebuilt Win zip),
`mutool.exe` (static, 46MB) → `pdf-renderer/tools/.renderer-bin/mupdf/mutool.exe`;
diff tool builds with `--features renderer-mupdf`; oracle cmd
`pdfium-diff sweep <files> --reference mupdf --scale 2 --out <dir>` (set
`PDF_RENDERER_MUPDF_BIN` to the .exe). Breakdown:
- **414/432 (96%) = LONG PATHS >260 chars** (up to 354). pdfium's C API AND
  `mutool.exe` CLI both fail (`cannot open`) purely on **Windows MAX_PATH** — the
  files are fine. **Our renderer opens them (verified 40/40 sample); SumatraPDF
  opens them too** (user confirmed via `\\?\` prefix). Rust std auto-prepends
  `\\?\` for long absolute paths → we win for free. `mutool.exe` does NOT accept a
  `\\?\` arg (mangles it), and 8.3 short-names are DISABLED on D: — so feeding
  these to the CLI needs staging (copy to a short temp path). **Not bugs; a
  diff-tool false-positive class + our robustness win.**
- **18/432 = normal paths, real failures.** Ran mupdf(direct)+ours on each:
  - **5 = OUR RECOVERY WINS** (we open, both pdfium & mupdf fail): xref-format /
    startxref-recover / `EBX_HANDLER` Adobe-DRM / zlib "distance too far back".
    e.g. `3798.pdf`, `Jews-and-the-Military`, `hellstorm…`, a Goebbels scan.
  - **10 = GENUINELY BROKEN** (ours AND mupdf both fail; mupdf agrees): "cannot
    find version marker/startxref", "expected object number", password-protected
    (`mhv_KOEPKE_…`). Not our fault.
  - **3 = OUR-SIDE `max_page_bytes` (only real our defect here):** `Evenlyn
    Farkas- Fractured States`, `[Martin_van_Creveld]_The_Culture_of_War`,
    `Admiral-Saumarez-…-Baltic`. We OPEN them but abort a page on the 2 GiB
    `max_page_bytes` cap (RenderLimits::default, pdf-render-api lib.rs:97). Cause:
    a **giant MediaBox** — Saumarez p1 is `[0 0 9600 14400]` (133×200"), ~20000×
    30000 px ×4 = 2.4 GB at pdfr's default 150dpi (>2 GB even at scale 2). mupdf
    renders it. **Real fix = oversized-page handling (clamp effective render
    resolution when surface would blow the byte budget), not just raising the
    cap.** A distinct class, likely more such files corpus-wide.
- **DIFF-TOOL follow-up (the "update scripts" item):** stage long paths for the
  oracles — copy to a short temp path for `mutool.exe`; try `\\?\` for the pdfium
  API — so future sweeps stop false-flagging 414 long-path files as "pdfium open
  failed." Until then, the single-oracle pdfium sweep will keep recording them as
  terminal `ours=0` sentinels (it skips OUR render when the reference fails —
  main.rs page_count/render Err arms; that's why sweep-14 showed ours=0 despite
  us rendering fine).

**5 BLANK/NEAR-BLANK ISSUES — ALL RESOLVED (2026-07-24).**
- **3 whole-page "blank" = xref-recovery bugs, FIXED (`6f1761d`).** `0554304.pdf`
  (leading `<body>` HTML before `%PDF` + catalog in ObjStm), `3948.pdf`
  (truncated trailer + catalog in ObjStm), `issue51.pdf` (dangling `object 1024
  0 R`). Two fixes: (a) `open_with_password` now escalates to
  `load_structure_rebuilt` when **either** `load_structure` OR the page-tree
  build fails structurally (previously only on the 0-pages-after-clean-load
  gate, so these erroring earlier never reached it); (b) the rebuild now parses
  every `/Type/XRef` stream's **dict as a revision trailer** — PDF ≥1.5 files
  have no `trailer` keyword and keep the catalog in an ObjStm, which
  `find_catalog` (offset-objects only) can't see, so /Root was never found. All
  three now render, ink matches mupdf (Δ ≤0.0035, dims identical).
- **2 severe near-blank = NOT our bugs — pdfium anomalies.** `2018_Book_
  Beatmung.pdf` p86 and `Paula Holmes-Eber…` p0: ours matches **mupdf** almost
  exactly (0.0321 vs 0.0327; 0.0515 vs 0.0513). The sweep-14 "ref 0.98" was
  **pdfium** rendering these scanned pages near-black — the pdfium outlier, not
  us. **KEY LESSON: the pdfium-only sweep-14 conflates our bugs with pdfium
  quirks; residual buckets (esp. the ~70 "mixed" + 23 under-ink) should be
  re-checked vs MUPDF before treating any as our-side.**

**3 `max_page_bytes` files FIXED (`1f87d28`).** Oversized-page clamp: `render_with`
now scales an oversized render down uniformly to fit the byte budget (folding the
ratio into the transform) instead of failing the page. Admiral-Saumarez/van
Creveld/Farkas (giant MediaBoxes, e.g. 9600×14400 pt) now render clamped to
~18900×28400 at the 2 GiB default. OOM guard preserved (bounded by construction).

**MUPDF-RERUN of residuals+remainder (2026-07-24) — KEY LESSON: neither oracle is
authoritative; trust only where ours disagrees with BOTH pdfium AND mupdf.**
Re-ran the 97 content-residuals + 2937 unswept files vs mupdf (fresh dir
`regress-runs/mupdf-rerun`). Added a wall-clock watchdog to the multi-sweep's
in-process ours-render (`9a63338`, `PDFIUM_DIFF_OURS_TIMEOUT` default 90s) — but
it only stops *cancellable* hangs; `bug1019475_1/_2` hang inside a single
non-cancellable coverage-kernel op (real never-hang gap, TODO: cancellation check
inside the coverage hot loop). Resumed the remainder with a bash per-batch
`timeout --kill-after` backstop (batch 8, 300s); `timeout` cleanly kills the win
exe. Final coverage 2905/3032 files. **The mupdf-only triage massively
over-flagged** (mupdf has its own artifacts: renders JBIG2 test bitmaps + some
scans all-black, and hallucinated content e.g. `0155999` p30 mupdf=0.800 vs
ours=pdfium=0). The mupdf-rerun `ours_ink` was ALSO unreliable for some rows
(recorded `issue19176` ours=0 though it truly renders 0.643). A 3-way check
(ours vs pdfium-via-`--reference-worker` vs mupdf-via-mutool, PIL/PPM ink) on the
37 OURS_BLANK/ALLBLACK/darker candidates gave the real list.
**GENUINE our-side bugs (ours ≠ BOTH refs), for part (b) fixing:**
- `bug1308536.pdf` p0 — **blank**, drops content (ours 0 / pdfium 0.094 / mupdf 0.086).
- **Over-ink class — visually triaged (rendered ours/pdfium/mupdf to
  `regress-runs/overink-compare/index.html`); NOT one class:**
  - `issue13561(_reduced).pdf` — **FIXED (`2f652a6`)**. An /ImageMask stencil painted
    with a tiling **pattern** (`/Pattern cs /P0 scn` then `/Im0 Do`) rendered solid
    black: `lower_image` (prepared.rs) matched only `Paint::Solid` for the stencil
    colour, fell to `[0,0,0]`. Now routes stencil+`Paint::Pattern` through the tiling
    machinery — `PreparedTiling.stencil: Option<Box<PreparedImage>>`, render_tiling
    folds the stencil coverage into the fill mask (gated → ordinary tilings
    byte-identical); lower_image returns the built image. ink 0.061→0.0068.
  - **3 scanned books** (`Micropolitics`, `Ad Meskens` p220, `Els Stronks` p180) —
    grayscale/gamma, ours darker. **User: ACCEPTABLE, not fixing.**
  - `bug1844576.pdf` — form-annotation widgets ours renders and the harness's pdfium
    doesn't. **NOT our bug.**
- `resvg_masking_mask_recursive_on_self` + `_transform_on_shape` — **FIXED (`8cc30b9`),
  and the long-standing "nested soft-mask compositing" theory was WRONG.** The masking
  was fine; the real bug was **pattern-space anchoring**: §8.7.3.1 says a pattern's
  /Matrix maps to the default space of the *stream referencing it* (form/mask-group
  space when invoked under a CTM), but we anchored every pattern to the page base —
  so a shading pattern inside a mask group invoked under a skew landed unskewed
  (wrong gradient direction + area chopped at the unskewed pattern domain; the
  parallelogram cut at unskewed x=160 was the tell). Fix: interpreter tracks
  `pattern_base` (CTM at stream entry), saved/set around form / soft-mask / Type3
  CharProc invocation (same boundaries as the per-stream pattern-cache swap),
  composed into the pattern matrix at resolution. 0.311→0.024 and 0.082→0.024
  (refs 0.020). Suites green; 35 pattern-heavy spot-checks vs pdfium unchanged.
  NOTE: this changes pattern placement in ALL forms invoked under non-identity
  CTMs (toward spec) — worth watching in the next sweep.
- `bug1308536.pdf` — **FIXED (`9dfbab9`).** The embedded Type1C's **Private DICT held
  garbage hint data** (broken real-number nibbles + naked non-token bytes): FreeType
  engines shrug it off; strict Skrifa/read-fonts rejected the whole dict → font parsed,
  name→GID fine, but `outline()` = None for all 70 glyphs → blank text (fallback never
  engaged since the font "loaded"). Fix: `wrap_bare_cff` runs a **length-preserving
  Private-DICT repair** (cff.rs `sanitize_private_dicts`): clean dicts byte-identical;
  malformed ones rescanned FreeType-style (1-byte resync), surviving entries verbatim,
  Subrs/widths prioritized over hints, freed bytes → nibble-padded BlueFuzz filler so no
  offset moves. Covers CID FDArray privates too. ink 0.000→0.081 (refs 0.086/0.094).
  Diagnosis trick worth remembering: probe test isolated it in minutes (parse ok →
  gid_for_name ok → outline None ⇒ dict-level rejection), then a Python-patched clean
  same-size Private DICT confirmed before writing the Rust fix.
**With this, ALL genuine our-side bugs from the sweep-14 + mupdf-rerun 3-way triage are
closed** (stencil-pattern `2f652a6`, pattern-anchor `8cc30b9`, CFF-private `9dfbab9`,
clamp `1f87d28`, xref recovery `6f1761d`; scanned-book gamma accepted; bug1844576 not
our bug). Next sweep validates corpus-wide.
**CLEARED as mupdf/measurement artifacts (NOT our bugs, ours == pdfium):**
`smask_alpha_oob_transfer`, `pattern_shading_background`, `image_lab_2`,
`path_rendering_14`, `0155999` p30, `issue19176`, `0001763` (all pp), `42271499`,
`issue18042`, `issue20062`.

**Context/process:** sweep 13 is **pdfium-only** by the user's current choice
("multi-oracle no longer needed") even though [[mupdf-is-the-oracle]] remains the
established primary control. `pdfium-diff` statically links the renderer + jbig2
+ jp2lam — rebuilt it against the fixes before sweeping. The 188210c commit
**bundles the user's pre-existing text-extraction WIP** (PLAN-TEXT-EXTRACTION
§5.2) because it shares `interpret.rs` and couldn't be cleanly split; a workspace
`cargo fmt --all` was isolated as its own commit (a264b40) so future code diffs
stay formatting-clean.
