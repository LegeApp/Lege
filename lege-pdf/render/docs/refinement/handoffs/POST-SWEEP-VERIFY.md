# Post-sweep oracle verification queue

Output-affecting changes landed while the full corpus sweep (release
`tools/pdfium-diff`) was running — the tool and release artifacts could not
be rebuilt or invoked. Each item below is unit-tested and spec/PDFium-source
cited; run the listed oracle check once the sweep finishes (and before the
next sweep is taken as a baseline).

| # | Change (commit) | Fixture | Expected direction |
|---|---|---|---|
| 1 | Separation/DeviceN with a **CIE (Lab) alternate** routes tint-transform outputs through the alternate's conversion (DS82 lavender-vs-yellow root cause) | `D:/PDF/Misc for further sort/DS82_Complete.pdf` pp. 4–5 (also `DS82_Complete1.pdf`) | ink_delta collapses (baseline p4 0.18590, p5 0.22704 — the header band was rendered pure yellow, PDFium lavender; band now matches). No other page should move except via the same PANTONE-alt-Lab space. |
| 2 | Native Macintosh (1,0) cmap fallback + real Mac OS Roman high table (DLIFLC notdef root cause) | `D:/PDF/Misc slides and documents/DLIFLC-Catalog-2015-2016.pdf` pp. 7/14/21/28/35 | ink_delta drops sharply (baseline p28 0.07647, ours-ink 0.215 vs ref 0.139 — body text was all notdef boxes; now renders real glyphs). Any MacRoman-encoded non-embedded font page may move slightly (high-range mapping fixed from latin-1 to true Mac OS Roman). |
| 3 | Synthetic bold/italic now applied for substituted Symbol/ZapfDingbats lacking the requested cut (commit: this one) | any corpus page using a non-embedded bold/italic Symbol or ZapfDingbats font (grep sweep CSV rows whose docs name `Symbol,Bold`/`ZapfDingbats` variants); no dedicated fixture in-repo | affected pages get slightly *heavier*/slanted dingbats-and-symbol glyphs (closer to Acrobat); all other text is byte-identical (synthesis is inert for the 4-cut text families and embedded fonts). |
| 4 | MacExpert encoding + full AGL name resolution (encoding-tables completion) | any `/Differences`-heavy or MacExpertEncoding page in the sweep; no dedicated fixture | strictly additive: names that previously resolved to notdef now render glyphs (ink can only move toward PDFium); no page should lose content. |
| 5 | Truncated-page-tree blank placeholder synthesis (declared /Count parity) | `Hoyos - Hannibal's Dynasty` (739 → 845 pages) and any `pdfium reported zero pages`/count-mismatch rows | page_count now matches PDFium's declared count on truncated files; new page keys appear at the tail (blank vs blank → inkΔ ≈ 0). Existing page keys unchanged. |
| 6 | Mesh-shading fixtures added to the diff corpus (`corpus/shadings/mesh-type{4,5,6,7}-*.pdf`) | the four new files themselves | first-ever oracle grading of mesh rasterization: expect inkΔ within the noise band on all four; any large delta is a real mesh bug to file. |
| 7 | Axial/radial (type 2/3) shadings now honor their `/ColorSpace` — Separation/DeviceN route through the tint transform, CIE through the CIE math (was: raw function outputs fed to arity-only `comps_to_rgba`, so a 2-colorant DeviceN collapsed to black) | `D:/to-sort/rosiesmenu3.pdf` pp. 0–5 | verified post-fix on rosiesmenu3: p0 0.286→0.0019, p1 0.759→0.037, p2 0.800→0.065, p3 0.786→0.032, p4 0.806→0.025, p5 0.652→0.093 (p5 residual is a separate `degraded(codec)` photo). Any page with a Separation/DeviceN/CIE-space `sh` or shading-pattern fill may move toward PDFium; no Device-space shading changes. The over-ink files issue6071/2958/4778 are a *different* (image-decode) root cause and did **not** move. |

**Binary provenance note:** the restarted sweep runs the 09:05 release build,
which contains the batched-concurrent orchestration plus items 1–2 (the DS82
and DLIFLC fixes) — their rows in this sweep are already post-fix. Items 3–6
landed after that build: re-grade those classes (or take the next sweep)
with a rebuilt tool once this run finishes.
