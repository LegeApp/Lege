The Opus agent finished, and I've independently verified its work — re-rendered the fixed pages against MuPDF, confirmed the commits, and re-ran the full workspace suite myself (101 suites, 0 failures). Net result: 5 of the 9 fixed, 1 partial, 1 closed as a non-bug, 2 deferred with the work scoped.

Notably, the agent overturned two of my triage conclusions with better evidence:

- issue9462 wasn't optional content after all — my BDC-spans-segments theory was wrong (marked-content state already survives Contents-array boundaries). The purple lattice was an embedded TrueType font with a corrupt head.unitsPerEm = 14 against 1000-unit outlines, scaling every glyph ~71× into giant purple blocks. Fix: reject out-of-range unitsPerEm (FreeType's 16–16384 rule), forcing substitution. inkΔ 0.325 → 0.004; I confirmed the render now matches.
- Godfrey Dale is closed as a non-bug: my "we point-sample bilevel scans" premise was false — the sampler already area-averages, and the image is 8-bit gray at a 1.16 footprint. The inkΔ is pure threshold sensitivity on a noisy near-1:1 scan.

The other verified fixes, each with a unit test and its own commit:
- The-Uyghurs cover — jp2lam's inspect_jp2 was running full validation and rejecting the 2-channel-palette JP2 before the renderer could request index passthrough; inspection no longer validates. 0.46 → 0.037 (the small residual is a nested-transparency arc, not JPX).
- issue6364 — the 2-component JPEG was decoded as gray with channel 1 dropped; now emits Multi2 into the TintLut2 path. gross 0.727 → 0.047. The photo is no longer inverted; the remaining tint-saturation gap is the same CMYK-ICC class as Huser.
- zusammenleben — stroke text render modes (Tr 1/2/5/6) implemented end to end. 0.125 → 0.0006.
- 2561.pdf p3 — sc/scn had no Indexed-colorspace arm, so the palette index was read as a raw color. gross → 6e-5; render is now pixel-identical.
- resvg recursive mask (partial) — the requested recursion guard is committed and tested, but the file turned out to be acyclic; the real residual is nested-soft-mask compositing (a mask whose own luminosity must be reduced by another mask), a transparency-backend change it didn't attempt. Still 0.13.

Deferred with findings written up: bitmap-refine-customat (three JBIG2 refinement faults located — page default pixel, polarity, region placement — needs a T.88 decode fix in jbig2enc-rust against pdf.js's reference) and Huser (/DefaultCMYK is an mft2 lut16 CMYK ICC profile; needs Default-colorspace plumbing plus a 4D-CLUT evaluator in icc.rs, which the agent judged too risky to land without corpus validation — currently thermally off the table).

Commits: five on pdf-renderer/sweep12-tail-fixes, one on jp2lam/sweep12-codec-fixes. One caveat it flagged: your working tree had uncommitted text-extraction WIP (gid_to_char etc., tagged PLAN-TEXT-EXTRACTION §5.2) — it was preserved and kept out of all commits, still sitting uncommitted. The one jp2lam test failure (colr_meth4_vendor_infers_space_from_component_count) is pre-existing — it fails at the parent commit too.

The sweep-12 tail is now 62 of 66 pages resolved. A mupdf-only sweep 13 would validate all of this corpus-wide whenever you want to spend the heat; the remaining real work is JBIG2 refinement, the CMYK ICC CLUT path, and nested soft-mask compositing.
