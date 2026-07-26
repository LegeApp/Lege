# Memory index

- [MuPDF is the oracle](mupdf-is-the-oracle.md) — since 2026-07-23 mupdf is the primary control, not pdfium; sweeps default to REFERENCES=mupdf.
- [Adjudicating PDFium disagreements](adjudicating-pdfium-disagreements.md) — cross-check in Firefox/pdf.js; Okular+qpdfview are one poppler vote, not two.
- [Post-sweep-9 fixes](post-sweep9-fixes.md) — inline framing (array /CS, ASCII-armour EOD), ICC Lab data space, pattern strokes; and the "render first, bisect second" lesson after three wrong hypotheses.
- [Sweep 9 analysis](sweep9-analysis.md) — validates the 11 post-sweep-7 fixes corpus-wide (390 pages better, only 2 structural regressions); the sweep-contamination rule; the new BPC-4 indexed inline-image bug.
- [Sweep 7 follow-up fixes](sweep7-followup-fixes.md) — 2026-07-22: xref stale offsets, flate 1.65x, JPX sub-8-bit, the K^5 degenerate-DWT bug (resolves handoff §2), JPEG DNL height, form path scoping; plus the deliberate PDFium-parity choice on sub-8-bit widening.
- [Sweep 7 analysis](sweep7-analysis.md) — all post-sweep-6 fixes confirmed corpus-wide, zero regressions; fresh JPX census (61 failing streams) + priority list; 5657/issue16782 are PDFium-blank non-bugs.
- [Sweep 13 Windows closures + clip perf](sweep13-windows-closures-and-clip-perf.md) — 2026-07-24: closed the last 2 tail items (JBIG2 custom-AT refinement 027a485, Huser /DefaultCMYK via new IccCmyk evaluator 188210c); only nested-soft-mask remains. Found a clip-mask perf class (fractional-edge rect clips → full-page masks, ~50× slow on vector music pages).
- [Sweep 12 tail fixes](sweep12-tail-fixes.md) — 62 of 66 tail pages fixed/closed across four passes; JBIG2 refinement + /DefaultCMYK now CLOSED (see sweep13 note); still open: nested soft-mask compositing.
- [Sweep 12 analysis](sweep12-analysis.md) — clean run validates post-sweep-11 fixes (220 better, 0 real regressions); tail down to 66 pages, mostly JBIG2/JPX blanks; JBIG2 symbol-pixel cap fixed.
- [Sweep 12 harness + JPX Psot](sweep12-harness-and-jpx-psot.md) — sweep-11 post-mortem fixes (empty error metrics, per-page retry, ±1 dimension tolerance, OOM/hang-resilient shards) and 0031730's bad-Psot blank pages.
- [Sweep 6 analysis](sweep6-analysis.md) — sweep-5 fixes confirmed, zero regressions; residual tail = JPX + JBIG2 codecs, xref-recovery open failures, ~74 structural render pages; prioritized target list. Windows-side memories (sweep4/5 context) live on the C: mount.
