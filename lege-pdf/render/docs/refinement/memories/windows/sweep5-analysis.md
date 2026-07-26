---
name: sweep5-analysis
description: Sweep 5 (2026-07-21) results analysis and the real-world-informed fix queue
metadata: 
  node_type: memory
  type: project
  originSessionId: b9ca21b5-e4c0-4e6a-80d2-998653205910
  modified: 2026-07-21T10:40:41.180Z
---

Sweep 5 = `D:\Rust-projects\pdfium-port-plan\sweep5\pdfium-diff-out\results.csv` (78,279 pages,
clean `flags=annot` header). Binary vintage: includes shading-colorspace (079ead0) + TIFF-
predictor (7fc70c1) + annot-clarity (5cab73f); does NOT include the later 76f69bc/18fd8ab/0ee40d2.

**COVERAGE GAP:** sweep 5 only swept 2 of run-sweep.ps1's 3 roots — `D:\Pol was right again`
(62,283 pages) and `renderer-corpus\PDF` (16,137). It did NOT reach `D:\to-sort` or
`renderer-corpus\hayro-source`, so rosiesmenu3, issue6071, 4778, and all pattern_shading
fixtures are ABSENT — sweep 5 cannot validate the synthetic-fixture fixes (those were verified
via pdfr instead). A future full sweep (all roots + latest binary) is still needed.

**Fixes confirmed working** (tail roughly halved vs sweep 4): mean inkΔ 0.01205→0.01002,
p99 0.103→0.054, pages>0.05 1493→842, pages>0.20 785→448. DS82 still low (~0.005). rosiesmenu/
issue6071 absent (not swept) but pdfr-verified.

**Real-world-informed fix queue (NEW top priorities from the book corpus):**
1. **CJK non-embedded text — FIXED 2026-07-21 (commits 95b16a8 + c41201b).** ROOT CAUSE:
   `default_font_paths()` in pdf-font/src/system.rs listed ONLY Linux dirs — no `%WINDIR%\Fonts`
   — so on Windows the provider scanned nothing and `--system-fonts` found no CJK face (that's why
   my first `--system-fonts` test didn't help). AND the diff tool never called `.with_system_fonts`
   at all, so the sweep graded CJK with no substitute (unfair vs PDFium which uses system CJK).
   FIX: (a) 95b16a8 — cross-platform font paths (Windows/macOS/Linux) + Windows/macOS CJK families
   in the GB/JP/B5/Hangul preference lists; the doc's own family (e.g. SimSun) now resolves once
   `%WINDIR%\Fonts` is indexed, then the CID→Unicode bridge (already present, needs s.system CJK face)
   maps glyphs. (b) c41201b — diff tool enables system fonts on all 4 grading paths (shared OnceLock
   provider per worker). RESULT: Chinese pages inkΔ ~0.13→~0.013, ours-ink 0.004→0.12 (ref 0.14),
   10× improvement across the cluster; verified on 隋唐佛学思潮泛论 + 唐王朝中央行政体制.
   BASELINE SHIFT (like annotations): the diff tool now substitutes ALL non-embedded fonts against
   installed faces. **Sweep 6 host MUST have CJK fonts installed** (Windows has SimSun/MS Gothic/…;
   a Linux box needs Noto Sans CJK or the CJK cluster stays blank there). DS82 (embedded) moved
   0.005→0.009 — a non-embedded label font now uses a real face; still excellent.
2. **Scanned-book over-ink → FULL BLACK — FIXED 2026-07-21 (commit 8d8ff53).** ROOT CAUSE:
   Michael Pearson "The Indian Ocean (Seas in History)" + Partridge "Gladstone" (x2) use an
   /Indexed image whose base is /Lab. `resolve_image_colorspace` mapped /Lab→Cs::Rgb, so the
   sampler read the palette's Lab bytes (signed a*/b*) as raw RGB → Lab(100,0,0) became a dark
   RGB triple → whole cover solid black (ours 1.0 vs ref ~0.19). FIX: pre-convert the palette at
   colorspace resolution — treat it as a (hival+1)×1 image in the base space and run
   convert_special_image_samples (the same Lab→sRGB path direct Lab images use), then store
   Indexed{base:Rgb, lookup:<converted>}. Multi-input DeviceN bases ride the same path;
   Device/ICC unaffected. Pearson/Gladstone p0 now inkΔ ~0.00005 (was ~0.81). Test added.
   NOTE: "Michael Pearson - Indian Ocean World (Trade/Circulation)" and "Andaman Islanders"
   were RED HERRINGS from name-matching — the Andaman p0 is legitimately black (both engines
   agree, inkΔ 0.0000). Ottomans (Aksan) not yet checked; may be a separate cause.
3. **Scanned-book under-ink → near-blank (~30 pages) — MIXED; Daughters of Tunis is a PDFium
   FALSE POSITIVE (investigated 2026-07-21, NO fix — we're correct).** Daughters of Tunis (6pp,
   ours 0.05 vs ref 0.998): the base image is 1-bit /Indexed /DeviceGray, palette [0,255]
   (black,white), hival=1, image 95.4% ones, with **/Decode[0 255]**. That maps sample 1 → index
   255, out of range for the 2-entry palette (UNDEFINED per §8.6.6.3). WE clamp to hival → palette[1]
   =white → correct readable "Daughters of Tunis" title page (black text on white). PDFium reads
   out-of-range → black → renders the page INVERTED (white text on black), mean L 0.4. Verified via
   triptych: ours is visually correct, PDFium is wrong. So this residual is the ORACLE's bug, not
   ours — no code change. (Implication: some of this cluster is pdfium mis-rendering; the sweep
   penalizes correct output. If strict pdfium-matching is ever desired, replicate pdfium's
   OOB-index→0 behavior, but that degrades correctness.) The OTHER under-ink files differ and may be
   genuine: What-Was-Socialism (/Indexed /CalRGB), Rana Mitter (DCTDecode DeviceRGB), Honorable-Exiles
   (DeviceGray Flate bpc8) — not yet root-caused; likely real codec/decode gaps.
4. Assorted mid-deltas (0.14–0.4) on individual academic-book pages.

**TRIAGE of the over-ink/under-ink residuals (2026-07-21, proposal #1 deep-dive).** Re-grading the
top ~10 with the CURRENT binary (all session fixes) shows the cluster is mostly NOT current bugs:
- **PDFium bugs, we're CORRECT** (user-confirmed via SumatraPDF/Acrobat): Daughters of Tunis
  (Decode[0 255] OOB index → pdfium inverts; we render the correct white title page). Aksan
  "Ottomans and Europeans" p0 (JBIG2 image IS a bad-scan static — SumatraPDF shows the same static;
  pdfium cleans/rejects it). No fix — the oracle is wrong.
- **Already FIXED by this session's commits** (weren't in sweep5's binary): Rana Mitter (0.098→0.006),
  Bannockburn (0.432→0.008), Rotberg Battling Terrorism (0.181→0.009).
- **Minor grayscale/CMYK tone gap (~3–6% darker), amplified by the diff tool's THRESHOLD-based ink
  metric** (a gray at 198 vs pdfium 208 flips a whole region from "white" to "inked", so o=0.67 vs
  r=0.30 despite luminance means of only 0.28 vs 0.26): Micropolitics (DCTDecode CMYK YCCK/APP14),
  Eugenia Lean + From_Central_Asia (plain DeviceGray FlateDecode). Our cmyk_to_rgb already uses
  adobe_cmyk_to_srgb; the DeviceGray cases have NO colorspace conversion, so the darkness is likely
  image MINIFICATION or a tone/gamma resampling difference vs pdfium — a systematic fidelity residual
  affecting many image pages, but subtle (not a catastrophic bug). Candidate for a future
  minification/resampling-fidelity pass (cf. workstream H "minification weight quality" residual).
- **The one clean fixable bug in the class was Indexed-Lab** (Pearson/Gladstone) — FIXED (8d8ff53).
Michael P. Breen (CS 2227) and PhRMA (CS 4206) have indirect colorspaces but are NOT Indexed-Lab
(current binary still shows them ~0.23–0.27, so a different cause — likely the same tone/CMYK gap).

Recommendation: CJK (#1) is highest impact but a deeper font-infra dive; image-decode-to-black
(#2) may be a more contained win and is adjacent to the predictor/image work. See [[sweep4-review-shading-fix]].
