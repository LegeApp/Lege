---
name: post-sweep9-fixes
description: "2026-07-22 post-sweep-9 fixes: inline-image framing (array /CS and ASCII-armour EOD), ICC Lab data space, page-/Resources inheritance, shading-pattern strokes; plus the diagnostic lesson that ended three wrong hypotheses"
metadata: 
  node_type: memory
  type: project
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-22T11:12:26.428Z
---

Continues [[sweep9-analysis]]. Five commits on pdf-renderer `master`, each verified against PDFium
and regression-checked over a 632-file / 3,105-page set chosen for colour-space and inline-image
content plus the standing regression list. Sweep 10 was started on `6f310c2` to confirm at corpus
scale.

- `dfdfab7` **inline image framed by a written-out `/CS` array.** `inline_ncomp` in the *tokenizer*
  only understood a `/CS` name, so `[/I /RGB <hival> <palette>]` gave no exact length and framing
  fell back to scanning for a whitespace-bounded `EI` — which 262 KB of palette indices hits long
  before the real end. pdfbox/2385_1 p0 ink 0.0996→0.0026 / gross 0.3644→0.0334, p1 0.0722→0.0026,
  2385_2 p0 0.1088→0.0080. **I first mis-recorded this as a BPC-4 sampling bug; it never was.**
- `ad61d6a` **ICC profile's declared data space is read, so Lab profiles are Lab.** An `/ICCBased`
  space was approximated by `/N`, which is wrong when the profile's data space is Lab: the numbers
  from a tint transform are L*a*b*, and a negative a* is green while a positive one is magenta.
  pdfjs/issue5939's `/Separation /PANTONE#203278#20U` has a 3-component Lab alternate emitting
  L 56.5 / a -43 / b 2 (mint) which we painted hot magenta. Header bytes 16..20 (ICC.1 §7.2.6) now
  decide; only Lab is intercepted; ICC Lab's -128..127 a*/b* range is used so spot colours are not
  clipped. issue5939 p0 0.0585→0.0034, p1 0.0410→0.0034; **also cleared the 5464.pdf pages that had
  been sitting in the sweep-9 regression column** (p34 0.0664→0.0037, p68 0.0656→0.0046, p51/p85/p17
  likewise) and improved EN-05-10137 ×6 (~0.025→~0.006, gross ~0.16→~0.04).
- `c178463` **page-`/Resources` inheritance for a category a form omits** (PDFium
  `CPDF_StreamContentParser::FindResourceHolder`; per *category*, not per name). **Measured 0 changed
  pages of 3,105** — kept as a faithful port because partial form `/Resources` are common, not
  because the corpus asked. Delete it without hesitation if it ever gets in the way.
- `b5ba9f9` **ASCII-armoured inline image framed at its end-of-data marker.** A payload filtered
  `[/A85 /Fl]` is *text*, so a whitespace-bounded `EI` occurs in it by chance — bug1077808's first
  image contains `\r\nEI(` about 3.5 KB in, and the scan stopped there, leaving ~10 MB of content
  stream tokenized as operators. Now `~>` for ASCII85 and `>` for ASCII-hex, then the `EI` after it,
  mirroring the DCT case that already scans past `FF D9`. bug1077808 p0 ink 0.4307→0.0019 /
  gross 0.4421→0.0175; issue10614 p1 0.0869→0.0064.
- `6f310c2` **stroke with a shading pattern.** `DisplayOp::StrokePath` matched only `Paint::Solid`
  and `continue`d otherwise, so a pattern stroke painted nothing. Fills and glyph runs already
  attach a prepared shading to the draw command; a stroke outline lowers as an ordinary path and
  needed the same. pdfjs/issue968 draws 8 pinwheels — the centres are `scn`+`f` (fine), all 96
  petals are `/Pattern CS /p1 SCN 10 w … S` (dropped), so the page was eight bare dots.
  issue968 p0 0.3706→0.0272 / gross 0.3896→0.0468; 5250_1 p0 0.3069→0.2396. **Tiling strokes stay
  skipped** — `lower_tiled` takes a fill shape and no corpus page needs it.

**DIAGNOSTIC LESSON — three wrong hypotheses in a row, all implemented and measured before being
discarded.** (1) duplicate `SemFont`s from `font_cache` scoping → an object-keyed dedupe measured
byte-identical, reverted; (2) missing page-resource inheritance → measured 0 pages; (3) BPC-4
indexed sampling → it was framing. What actually worked every time was **rendering the page and
looking at it**: issue968's missing petals and issue5939's magenta-vs-mint were both obvious in one
glance and neither was findable by bisecting. **Render first, bisect second.** Corollary already
recorded: a large ink delta with matching geometry is a *colour* bug.

**Also: decompress content streams before grepping for operators.** Greps against compressed bytes
produced two false "this file has no inline images / no forms" conclusions.

**Next structural singles:**
- **`issue18466` — ROOT-CAUSED (not yet fixed): non-embedded CIDFontType2 substitution.** We paint a
  solid black block over the top half of the page (ink 0.412 vs PDFium's 0.027). The IR is normal —
  ordinary dark-grey text runs — so the block is the **synthetic glyph-box fallback**
  (`lower_glyph_boxes`, "fonts.md Font Phase 1": one filled rectangle per glyph, sized from its
  advance) firing because no font program resolves. The page uses two Type0/CIDFontType2 fonts:
  `AAAAAA+SimSun` (has `/FontFile2`, fine) and **`FZSSK--GBK1-0` with no font file at all**.
  PDFium substitutes a system CJK face; we do not, for a *CID* font. Substituting it needs the
  `/CIDSystemInfo` ordering (Adobe-GB1 here) to pick a Simplified Chinese face plus a CID→GID path.
  Noto CJK is installed, so the faces are available. **This is the fattest single left, and it is a
  font-phase feature, not a bug fix.** Note the failure mode is loud — a black block, not missing
  text — so any other page with a non-embedded CID font is over-inking the same way.
- **`issue6707` — SETTLED NON-BUG, proven by arithmetic** (inkΔ 0.342; do not chase). The page fills
  a large band with `/R22 cs 0.300049 scn`. `/R22` is
  `[/Separation /PANTONE#20467#20U#201 /PANTONE#20467#20U#201 14 0 R]` — **the alternate-space slot
  holds the colorant name again instead of a colour space, so the file is malformed.** Its tint
  transform (obj 14, FunctionType 2, `Range [0 1 0 1 0 1]`, `C0 [1 1 1]`, `C1 [0.90699 0.825366
  0.677137]`, N 1) at t=0.300049 gives RGB (0.9721, 0.9476, 0.9031) = **(247.9, 241.9, 230.3)**, a
  very light beige. Our render measures **(247.9, 241.9, 229.9)** — an exact match. We evaluate the
  tint; we are right.
  **PDFium, hayro and poppler all paint it grey 77** — which is just `0.300049 x 255`, i.e. they
  fall back to treating the tint as DeviceGray when the alternate space will not resolve. Three
  renderers agreeing is *not* evidence here: it is one shared shortcut on one malformed construct.
  pdf.js agrees with us.
  Separately, **PDFium alone omits the header photo and VIRENA logo**; we, hayro and pdf.js all draw
  them. So this page has two independent differences and PDFium is wrong on both.
- **`1416` — ISOLATED TO COMPOSITING (next target).** ours 0.123 vs ref 0.487. PDFium **and hayro**
  both render a large background photograph across the right of page 0; we render pure white there
  (`min 255` — literally nothing painted). Everything upstream of the rasteriser was verified
  correct, so do not re-check it:
  - the image (obj 66) is a **CMYK JPEG, APP14 Adobe `transform=2` (YCCK), with `/SMask 65 0 R`**;
  - our JPEG codec decodes it correctly — channel means (60.2, 62.5, 57.9, 31.9), exactly `255 -`
    PIL's stored values, so YCCK and the Adobe inversion are both handled;
  - the soft mask (Flate + PNG predictor 15, DeviceGray) decodes correctly — mean 131, max 255;
  - `lower_image` **keeps** the draw: `bounds = (0,116) 1241x1631`, clip identical, `degraded = 0`,
    samples `len 4,060,972` (= 851x1193x4) `mean 53 max 228`.
  So correct pixels, correct alpha, correct geometry, no drop — and a white result. The loss is in
  **compositing**. Prime suspect: the ops that follow are `begin-soft-mask Luminosity` and
  `begin-group isolated=true ... bounds [-10.09 -640.04 602.19 858.21]` — page-sized — so an
  isolated group whose backdrop is initialised opaque-white instead of transparent would wipe the
  photo. Verify that before anything else.
- `jbig2_file_header` (0.41 vs ref 0 — an embedded JBIG2
  stream carrying a file header should be rejected).
