# Post-sweep-3 attack plan (2026-07-20)

> **CLOSURE (2026-07-21, production-readiness pass).** The R and B
> workstreams are resolved: **R1** via the degenerate tint-LUT repair plus
> the `ImageIr.lowering_degraded` flag (the tracking hole is closed — a
> lowering-time image drop now always ticks the tracker); **R2** via the
> non-Normal blend / transparency-group generalization (knockout, `/BC`,
> `/TR`, backdrop-seeded non-isolated groups). **L1 (CJK)** is done — the
> predefined CMaps were already bundled (the "decision needed" framing was
> stale), and the pass added CID→Unicode tables for the four registries,
> vertical writing (`/W2`/`/DW2`, wmode 1), and the opt-in CJK substitution
> bridge. **L2** is done (knockout, `/BC`, `/TR` all landed). **L4** is done
> (mesh shadings 1/4–7 parse and rasterize; diff fixtures still to be added
> to the corpus). **H** is half done: image-edge partial-coverage AA landed
> (including rotated placements); minification weight quality (area-average
> tails; stencil boldness on DS82/PhRMA/DLIFLC) remains open. **Next
> milestone: the full Linux re-sweep under the annotations-on baseline** —
> `tools/pdfium-diff` now renders PDFium with `FPDF_ANNOT` and ours with
> annotations (stamped `flags=annot` in results.csv), so every older sweep
> CSV, including sweep 3 below, is baseline-incompatible. The rest of this
> document is retained as provenance.

Sweep 3: **14,656 files / 84,473 sampled pages, 0 worker failures, 0
timeouts, 0 terminal skips** (the hang/crash hardening never had to fire for
our code; the only terminal rows are 11 files PDFium itself cannot open and
3 pages PDFium cannot load — not ours to fix).

Headline vs sweep 2 (65,990 overlapping pages):

| class                    | sweep 2 | sweep 3 |
|--------------------------|--------:|--------:|
| failed (ours)            |   1,571 |   **0** |
| bad (inkΔ > 0.05)        |   3,179 |   2,165* |
| blank (ours≈0, ref>0.3)  |     610 |     **8** |
| pages fixed / regressed  |       — | 1,740 / **12** |

\* sweep-3 "bad" spans the full 84k pages including the new `PDF/` root
(18,472 rows, 643 bad); on the overlap the class shrank.

The hard-failure and silent-blank eras are over. What remains is **2,165 bad
pages in 699 docs**, split: **1,808 lighter-than-ref (567 docs)**, **178
heavier (94 docs)**, **8 blank**, plus **80 degraded-flag pages (42 docs)**
the new codec-drop tracking surfaces. Attack in this order:

---

## R — Regressions from the last batch (3 real, do first)

12 regressed pages total; 9 are 0.05-threshold noise. Three are real, all
lighter, all p0 covers:

> **Status (2026-07-20, post-root-cause).** All three bisected and named; the
> table's original "suspicions" are superseded — notably the transparency
> theory was wrong for both covers, and the "solid black" probe reading was
> an artifact of a naive ink metric (page is too LIGHT; use the oracle's
> fraction-below-luma-200 metric for these).

| doc | old→new inkΔ | root cause (bisect-confirmed) | status |
|---|---|---|---|
| Jewish-Israeli-National-Identity… | 0.012→0.988 | external `jp2lam` improvement made a tiny `[/Indexed …]` JPX overlay *succeed* → multi-component RGB painted raw through the palette = white flood, untracked | **FIXED fd83a63** (drop+track mismatched reinterpretation; silent-drop audit). Oracle 0.988→0.012 |
| Medieval Garments Reconstructed… | 0.003→0.756 | **default-inversion theory DISPROVEN by implementation**: the inverting-`/Decode` fix passed the luma ink gate while flipping photo color brown→blue (the metric is color-blind) and regressed p24 (0.005→0.105); it was reverted. 3211149's no-inversion decode matches libjpeg-turbo byte-for-byte and our CMYK table matches PDFium. The real residual is a **~15-luma warm-vs-cool page tone** on a `/Group`×271 + `/SMask` + `/Multiply` page, amplified by the oracle's hard threshold | **merged into the unified compositing workstream** below, with Young-Turks |
| Young-Turks-and-the-Ottoman… | 0.002→0.212 | re-diagnosed by instrumentation: the tint transform is **correct** (t → CMYK(0,0,0,t), a spec-correct pure-K ramp — the neutral LUT is what the PDF says). The page's gold comes from separate elements that ARE compiled (gold `DeviceCMYK(.15,.25,.75,0)` fills, two `paint-shading` ops, `set-blend Multiply`, groups) but are **lost in backend compositing** — the non-Normal-blend/group family DEFERRED already tracks. The 5d55870 bisect delta is the image's polarity going from accidentally-dark (arity-Gray) to correct-light, which shifted the ink metric while the real (older) compositing gap stayed | **queued for the render agent** after its current batch (pdf-render-cpu; fixture = this cover) |

**R1.** Root-cause the Separation/Indexed blank cover. The draw disappears
without a `degraded_draws` tick, so it dies in lowering (TintLut build
returning None → draw dropped?) not in a codec. Acceptance: p0 renders, and
*any* lowering-time image drop increments `degraded_draws` (close the
tracking hole this exposed — a drop the tracker can't see is exactly what it
was built to prevent).

**R2.** Root-cause the two transparency covers (one black-flood, one light).
Suspects: luminosity-mask/`/BC` handling, group compositing with `/Multiply`,
or the Separation fill flood. Bisect across the ~10 commits between sweeps
with the covers as fixtures; extract minimal regression fixtures for both.

## B — Blanks and codec drops (small, well-named)

**B1. 2-component JPEG — DONE (e133ba6).** The failing streams turned out to
be 2-colorant `/DeviceN` images coded as 2-component baseline JPEGs;
`assemble()` now emits Gray8 from component 0, matching the pipeline's
DeviceN-2 → 1-sample reduction and PDFium's libjpeg passthrough. East Asia
p500's hard decode error is gone.

**L2-unified — RESOLVED, with a split (8127d10).** Non-isolated groups now
seed from the parent backdrop (PDFium ProcessTransparency Stage 1);
*Young-Turks p0: 0.212 → 0.0009, visually gold — fixed.* Medieval Garments
was re-split OUT by pixel forensics: its ~15-luma tone offset exists
pre-composite in the DeviceCMYK background image's decode (ours
~[145,137,135] vs PDFium ~[105,115,123]) — a pdf-image DCT/YCCK nuance now
queued with the image-drop family. Stage-2 items (backdrop removal §11.4.8,
knockout) remain documented approximations no current fixture needs.

*(original workstream description follows for provenance)* Both remaining
regression covers converge here with strong evidence packs: *Young-Turks*
(gold `DeviceCMYK(.15,.25,.75,0)` fills + two `paint-shading` ops +
`set-blend Multiply` + groups all COMPILED but the gold is lost in backend
compositing) and *Medieval Garments* (~15-luma warm-vs-cool page tone on a
`/Group`×271 + `/SMask` + `/Multiply` page; decode and CMYK conversion both
proven byte-correct). Suspect family: shading/fill under non-Normal blends
inside (soft-masked) groups — the DEFERRED items `/BC`+`/TR`, knockout,
non-isolated groups, and the "tiling under non-Normal blend uses
source-over" shortcut generalized. These two covers are the fixtures.

**B2. Remaining blank pages — CLOSED (42476a8 + named handoffs).**
Spence/Shock-Doctrine were never JBIG2: their covers' `/Filter` is an
indirect reference to an ARRAY of references, which shallow resolution left
unresolved — fixed in pdf-document; both covers 0.000 → ~0.986 ours_ink and
the fix likely repairs this filter spelling corpus-wide. The other five are
precisely-named **jp2lam handoffs** (sibling repo): data-after-packets ×3
(State-Society t6, Napoleon III t0 161 KB trailing, Stalin-court t124 1 B),
packet-body-past-tile-part-payload (Jeremy Black t13), and an unsupported
JP2 color-specification method (Stock Analysis p310) — beyond
TRIAGE-RESIDUAL's earlier QCC-override/box-past-input families.

**Text-clipping render modes (Tr 4–7) — DONE (ed8447c).** All six owner
covers now render title-over-photo correctly (inkΔ 0.0026–0.0061, viewed).
Deferred within: Type 3 clip-text, mode-5/6 stroke *painting* (clip half
correct; text stroking is a pre-existing gap).

*(original diagnosis follows for provenance)*
The knockout-jacket "mystery" resolved to this: the owner's covers set title
glyphs in mode 7 (clip-only) and then draw ~99%-white images carrying black
bars — PDFium clips those images to the glyph outlines (title text appears);
we treat 4–7 as merely invisible, never accumulate the clip, and the white
floods the cover. Neither knockout nor /OC (zero `/OC` in the file; PDFium
citations: cpdf_streamcontentparser.cpp:1351/918 AppendTexts,
cpdf_renderstatus.cpp:574–593 ProcessClipPath → SetClip_PathFill). Design:
mark mode-4–7 runs clip-contributing, push the union of their glyph outlines
as a clip at text-object end (popped at enclosing Q); rasterize a
glyph-outline clip mask in render-cpu (`lower_glyph_outlines` already
extracts the geometry).

**B3. Remaining degraded-flag docs after B1** (~25 docs): rerun, re-rank,
individually diagnose. Most are expected to be jp2lam deferrals already
listed in TRIAGE-RESIDUAL.

## L — The lighter class (1,808 pages / 567 docs — the bulk)

Feature-clustered over the top-600 pages' 263 docs:

| signal | docs | workstream |
|---|---:|---|
| jpx/jbig2/ccitt scans | 76/75/61 | L3 (AA thinness, sample first) |
| Type0 fonts | 49 | L1 |
| predefined CJK CMaps (`/Ordering`, 90ms/UniJIS/UniGB…) | 19 (pure-CJK docs) | **L1** |
| transparency (lum-mask/`/BC`/`/TR`/knockout/blend) | 38 | **L2** |
| mesh shadings 4–7 | 3 | L4 |

**L1. CJK text — predefined CMaps.** The 19 Chinese-article docs render text
lighter/missing because only Identity-H/V works. This is the standing
DEFERRED decision: **bundle predefined CMap tables PDFium-style (adds data
weight) vs embedded-CMaps-first**. Decision needed from the owner; then it's
a pdf-font workstream (agent-sized, crate-scoped like the CCITT port).

**L2. Transparency completeness.** `/BC` backdrop + `/TR` transfer on soft
masks, knockout groups, non-isolated groups — currently approximated/ignored
per DEFERRED, implicated by 38 lighter docs *and* two of the three
regressions. One workstream: implement `/BC`+`/TR`, knockout, and verify
non-Normal blends against the oracle using the regression covers as the
fixtures.

**L3. Scan lightness — SAMPLED (16 docs, viewed triptychs). Verdict:
~75–85 % of the 1,808-page class is cosmetic AA/resample thinness** (identical
marks, marginally thinner strokes; dominant across the 415 mild-band
scanned-book pages, and the un-sampled ≤0.09 tail is even more AA-weighted) —
**deferred to the optimization phase with confidence** (image-edge
partial-coverage AA + stencil minification weights). The high-inkΔ head
yields four concrete bug workstreams instead:

- **L3-C. Missing embedded-font glyphs — 2 of 3 families FIXED (9fe35fd);
  third re-scoped.** Three distinct mechanisms, not one: (1)
  Leibniz/EarlyModernTexts = the text matrix never advanced by glyph widths
  across consecutive `Tj` runs — fixed, every page ~0.13 → ~0.016; (2)
  Capital One statements = symbolic bare-CFF fonts' built-in `Encoding` was
  discarded by the OTF wrapper — fixed, statements render complete; plus a
  latent SFNT-wrapped CID-keyed CFF fix. (3) CNKI Chinese articles are NOT a
  glyph-extraction bug: the embedded "SimSun" is a Latin-only decoy and
  PDFium renders the hanzi by **substituting a system CJK face** (GB1 CID →
  Unicode → substitute). New workstream: **CJK substitution** — needs the
  skipped CID→Unicode tables plus substitute-on-missing-glyph routing;
  naturally lands behind the existing opt-in `--system-fonts` provider
  (bundling a CJK face would break the deterministic-default policy; the
  deterministic default keeps rendering notdef).
- **L3-B. Dropped full-bleed cover images — diagnosed, split (no single
  mechanism).** (a) The owner's print jackets: an isolated **knockout** group
  where genuinely-opaque white overlay JPEGs (verified white by three
  decoders) sit above the photo — we ignore knockout so white wins; BUT
  textbook knockout would *also* yield white, and PDFium shows the photo —
  an open behavioral mystery to resolve BEFORE any knockout implementation
  (investigation queued, speculative code explicitly rejected). (b) *Creating
  German Communism*: named jp2lam gap — "tile-part COC component
  coding-style override (0xff53)" — **sibling-repo handoff**. (c)
  *JacketBlue*: JPX + 2-component-ICC colorspace — **sibling-repo handoff**.
  (d) *Tirpitz*: path did not resolve; re-locate at next sweep.
- **L3-D. Presentation-slide image elements — CLOSED**: both docs were the
  /Rotate harness gap; after 18b0edb Forex worst-page inkΔ 0.006, cell
  biology 0.011. No engine work existed.
- **Group-interior alpha double-application — FIXED (12673d2)**: the
  §11.6.6 reset never reached the lowering pass; a 0.5-ca group drew content
  at 0.25. Medieval Garments p0 0.756 → **0.0000**; all three sweep-3
  regression covers are now closed.
- **L3-E. Page-rotation mismatch** — content rendered 90° off vs PDFium
  (cell biology, Crassus). Suspect the *diff tool itself*: `render_ours`
  builds its matrix from the crop box only and never applies `/Rotate`,
  while PDFium bakes rotation into page dims. Verify and fix in
  tools/pdfium-diff (and check pdf-cli render) before blaming the engine.
- **L3-G. PDFium-broken false positives — exclude from ranking.** *Daughters
  of Tunis* (rank #1 by inkΔ 0.976!) is PDFium rendering solid black while
  OURS renders text+photo correctly — the first confirmed case of us beating
  the oracle. Re-rankings must filter pages where pdfium ink ≈ 1.0 over our
  structured render; audit the severe head for more CCITT-invert cases.

Evidence triptychs preserved at `/tmp/l3-sample/<id>/*.png`.

**L4. Mesh shadings (types 4–7).** 3 docs. Deferred hook exists
(`/Background`-only). Small, well-bounded, last among L.

## H — The heavier class (178 pages / 94 docs)

Known cause family: Type 3 bitmap glyphs and 1-bit stencils render bolder
than PDFium (no partial-coverage AA at image edges; nearest-neighbour
minification weights). This is queued optimization-phase work (image-edge
AA + `fast_image_resize`-style weights); the sweep-2→3 movement shows it is
cosmetic, not content loss. Revisit after L3's sampling confirms nothing
else hides in it.

## Order & gates

1. **R1+R2** (regression root-cause + fixtures; close the degraded-tracking
   hole) — gate: the 3 covers match oracle, tracker catches every drop class.
2. **B1** (2-comp JPEG) — gate: East Asia cluster degraded=0.
3. **B2/B3** (blank + degraded residue) — gate: blank class = 0 achievable
   rows; every residual has a named jp2lam/jbig2 issue.
4. **L2** (transparency) — gate: regression covers + 38-doc rerun improve.
5. **L1** (CJK) — needs the predefined-CMap bundling decision first.
6. **L3 sampling** → route findings; **L4** mesh; then **H** with the
   optimization phase.

Inner loop for every workstream: `pdfium-diff --rerun-failures` over the
sweep-3 CSV class, not a full sweep. Next full sweep (4) only after R+B+L2
land.
