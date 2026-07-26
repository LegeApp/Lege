---
name: sweep12-analysis
description: "Sweep 12 (2026-07-23) validates the post-sweep-11 fixes corpus-wide with zero genuine regressions, and reduces the >0.10 tail to 66 pages / 38 files dominated by image-codec blanks."
metadata: 
  node_type: memory
  type: project
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-23T05:17:28.294Z
---

Sweep 12 ran 2026-07-23, 2h12m, 6 shards, 0 failed. 19,295 files / 100,027 pages
× 3 controls (pdfium, hayro, mupdf) = 300,081 rows at SCALE=2. Output:
`regress-runs/sweep-12/results.csv`.

**Data quality was the point** — sweep 11's numbers were unusable (error rows
scored as ink=1, 47,352 pages mislabelled dimension-mismatch, 1,430 files
silently dropped). Sweep 12: 164 error rows, 84 dimension-mismatches, perfectly
balanced references. Compare tails across sweeps only via this run, not 11's.
See [[sweep12-harness-and-jpx-psot]] for the harness fixes that bought this.

**Result: the post-sweep-11 fixes are validated corpus-wide.** Against sweep 11's
241,392 comparable rows: 220 materially better (Δ<-0.02), **0 genuine
regressions**. The 4 apparently-worse rows are both non-bugs:
- `pdfium/413597066.pdf` p0 — we converged *onto* PDFium (inkΔ 0.179→0.0019,
  gross 0.564→0.0019). Hayro and mupdf are the outliers here.
- `War of Words…pdf` p0 — `gross` improved ~8× against all three; only the
  coarse ink-coverage proxy rose. Lesson: when inkΔ and gross disagree, gross
  is the finer signal.

**Residual tail: 66 pages / 38 files worse than all three controls by >0.10**
(0.07% of pages; 298 pages >0.05, 1,424 >0.02). 53 of the 66 are pages where we
render *blank* — the tail is overwhelmingly image-codec coverage, not raster
fidelity.

Blank-page causes after the fix below (44 pages remaining):
- **JBIG2 MMR (Group 4) decode failure — 16 pages / 5 files.** Biggest single
  remaining win; one decoder bug in `jbig2enc-rust`. Files: both Hobsbawm
  volumes, `jbig2_huffman_2.pdf`, `image_jbig2_4.pdf`, `4211.pdf`.
- **JPX unsupported features — 11 pages / 4 files**: COC overriding more than
  the wavelet transform (`issue11004`, 6pp), CRG marker (`0518325`), ambiguous
  2-component JP2 colour (`Stock Analysis (2015)`), component layout
  (`issue19326`).
- **Silent blanks, no diagnostic at all — 8 pages / 4 files**: `issue7229` p0,
  `What-Was-Socialism…` p51/102/153/204/255, `issue6621` p0, `3874` p0. Needs
  fresh triage; not a codec message.
- **CCITT "zero dimension" — 4 pages / 2 files** (`issue5592`, Edgerton).
- **JBIG2 page 496 Mpx > our 268 Mpx cap — 3 pages** (`pdfbox/4598.pdf`).
- **DCT dimension cap — 2 pages** (`42270651` 21770×15399; `issue10989`
  10308×60000). PDFium renders both.

Non-blank tail is 13 pages, 10 over-inked / 3 under-inked, and all but one are
**page 0**. Six are scanned-book covers over-darkened by +0.11..+0.27 ink
(Breen/Dijon, Eugenia Lean, Tummers, Huser, Bornstein, Godfrey Dale) — treat as
one class, not six bugs.

**Fix made during this analysis (uncommitted):**
`crates/pdf-image/src/jbig2.rs` pushed our 268 Mpx budget down as
`max_page_pixels`/`max_region_pixels` but left `max_symbol_pixels` at
`jbig2enc-rust`'s 16 Mpx default ("a single symbol larger than a 16 Mpx tile is
almost certainly malformed" — `src/shared/limits.rs:57`). Real 600 dpi scans
encode a page-sized generic region as one ~31 Mpx symbol, so the decode died on
a cap 16× tighter than intended. Now also passes `max_symbol_pixels` and
`max_total_dictionary_pixels`. **Verified**: 9 tail pages went from ink 0.000 to
inkΔ 0.0004–0.0009 vs PDFium (Abulafia ×5, `3156158` ×2 copies, Stalin's Secret
Pogrom p196, Class-Struggle p156).

**Gotcha that cost a wrong conclusion mid-analysis:** `pdfium-diff` statically
links `pdf-image`, so `cargo build -p pdf-cli` alone leaves the sweep tool on
old code and the fix looks like it did nothing. Rebuild
`tools/pdfium-diff --features all-renderers` too before verifying.

Thermal note: the user's laptop runs hot on 3-control sweeps and Linux lacks
full fan control there. Future sweeps should default to `REFERENCES=mupdf`
(cheapest control) and reserve pdfium/hayro for triage — the three-control data
is only needed for adjudication, see [[adjudicating-pdfium-disagreements]].
