# Current renderer state

Snapshot reconciled on 2026-07-26 from the Windows and Linux Claude memories,
the renderer handoffs, and the source tree moved into this directory.

## Canonical locations

- Renderer source: `Lege-ecosystem/lege-pdf/render/crates/`
- Renderer workspace: the `Lege-ecosystem` root workspace
- Viewer source: `Lege-ecosystem/lege-viewer/`
- External corpus, sweep outputs, and PDFium reference material:
  `pdfium-port-plan/`
- Preserved pre-move renderer Git history:
  `pdfium-port-plan/pdf-renderer-history-through-2026-07-25.bundle`

There is no longer a renderer source copy under `lege-viewer` or
`pdfium-port-plan`.

## Viewer integration carried forward

The newer renderer now includes the APIs required by the viewer skeleton:

1. `pdf_content::PageCompiler::compile_artifacts()` returns the retained
   `SemanticPage` and lowered `CompiledPage` from one interpretation pass.
2. `pdf_render_api::CancellationToken::{from_shared, shared_flag}` lets the
   viewer conductor and raster backend observe one atomic cancellation flag.
3. `pdf_document::ParseContext::begin_job()` resets page-scoped budgets and
   diagnostics while retaining a persistent worker's parsed-object and object-
   stream caches.

These are deliberate anti-black-box seams. The viewer retains semantic/text
state and reusable render IR instead of treating the renderer as a bitmap
server. The viewer's Stage 2 implementation now exercises these seams through
its asynchronous compile-and-tile conductor, then composites the resulting
CPU tiles through a bounded WGPU atlas with a software reference fallback.
This presentation work does not replace or black-box the renderer.

## Sweep status

The latest complete account is
`memories/linux-project/sweep13-windows-closures-and-clip-perf.md`, whose later
sections include sweep 14 and its MuPDF rerun.

- Sweep 14 was interrupted at 86%, then its residuals and the unswept
  remainder were rerun against MuPDF.
- All genuine renderer defects found by that three-way triage were closed:
  oversized-page clamping, xref recovery, pattern-filled image masks,
  pattern-space anchoring in nested streams, and malformed CFF Private DICT
  recovery.
- The earlier “nested soft-mask compositing” diagnosis was superseded; the
  observed failures were pattern-space anchoring and are fixed.
- The large scanned-book under-ink class was initially judged a thresholded-
  metric / smoothing difference. Sweep-15 follow-up found a concrete subset:
  minified MRC foreground images area-filtered their color samples but point-
  sampled their high-resolution bilevel soft masks. That subset is now fixed
  with fractional mask-footprint filtering; do not add a page-wide blur.

Sweep 15 is the first complete post-move, two-oracle production-path baseline:

- 19,443 PDFs, six shards, scale 2, PDFium + MuPDF, 100,109 sampled pages per
  oracle (200,218 result rows), zero failed shards, completed in 2h08m. Results:
  `../oracle-sweep-15-2026-07-26/results.csv`.
- PDFium classified 77,926 rows `ok` and 22,105 `suspect`; MuPDF classified
  84,687 `ok` and 15,244 `suspect`. The broad suspect counts remain deliberately
  sensitive and are not a defect count.
- 11,114 pages were suspect against both controls. Requiring both controls to
  agree on direction and at least 0.05 ink delta narrowed that to 188 pages in
  86 documents. Representative triptychs separated three different classes:
  subtle scan-tone/image differences, malformed files whose recovered page
  trees select different pages, and a genuine Type 3 graphics-state leak.
- The genuine class is fixed: a malformed Type 3 CharProc with an unmatched
  `q`/`Q` could consume its wrapper save or leave its glyph transform active,
  repeatedly shrinking all later text. Nested streams now have a graphics-state
  stack floor and leaked inner saves are balanced at the CharProc boundary.
  On `Tang-Dynasty-Tales-A-Guided-Reader-.pdf`, all five formerly suspect
  sampled body pages are now `ok` against both controls; page 114 moved from
  ink deltas 0.08455/0.07835 to 0.00623/0.00003 (PDFium/MuPDF).
- The malformed page-tree class is also fixed. Missing-generation indirect
  references such as `335 R` recover as generation zero with a typed event;
  any partial subtree loss gets one bounded xref-rebuild retry; and exact
  count-backed placeholder spans are inserted in document order when real
  objects remain unrecoverable. The 152-page Cambourne report now recovers its
  final ten-page branch. Page 100 moved from ink deltas 0.30413/0.29196 to
  0.00377/0.00840 and `ok` against PDFium/MuPDF.
- Sweep CSV output now actually matches its declared 18-column schema.
  Successful rows include the empty flags field; renderer failures use schema
  3 with empty measurement fields, aligned `status`/`note`, and CSV-escaped
  diagnostics. Sweep 15's 230 old schema-2 failure rows remain readable as
  historical output but should not be used as measurements.
- JPEG fancy chroma upsampling is now complete. The native decoder matches
  libjpeg's integer h2v1, h1v2, and h2v2 triangle filters and edge/rounding
  behavior in both progressive and baseline-streaming paths. The streaming
  path retains its low-memory design by delaying one MCU band and keeping only
  two reusable bands plus one context row. All 67 `pdf-image` tests pass, and
  the libjpeg-derived 4:2:0/4:2:2 fixture tolerance tightened from a mean 8.0
  to 0.75.
- The hypothesized connection between that JPEG gap and the recurring
  Sweep-15 scan-tone class was disproven: the sampled
  `volkstumlichege03grae.pdf` and `sexcharacter00wein.pdf` rows were numerically
  unchanged by fancy chroma upsampling because these are MRC pages: two JPX
  color layers plus a high-resolution 1-bit JBIG2 `/SMask`.
- The focused follow-up closed that class. Soft masks now use the same
  fractional source-footprint box filtering as their minified base image,
  with a packed-bit weighted-popcount path for 1-bit masks and `/Decode`
  applied after coverage averaging. On the two documents above, all 24 sampled
  PDFium/MuPDF rows are now `ok`. Representative PDFium results moved from
  ink/gross deltas 0.09091/0.05401 to 0.00607/0.00000
  (`volkstumlichege03grae.pdf` p605), and 0.07097/0.05664 to
  0.00552/0.00000 (`sexcharacter00wein.pdf` p65).
- The next same-direction structural residual was `pdfbox/2906.pdf` p2.
  Tiling patterns already painted paths and image masks, while text lowering
  explicitly skipped a `Paint::Pattern` glyph run. Glyph outlines and fallback
  boxes now feed the ordinary tiling fill mask, sharing its bounds, lattice,
  clipping, alpha, blend, and color-policy handling. The formerly absent
  patterned text is present; page 2 moved from suspect to `ok` against both
  controls (PDFium ink/gross 0.07148/0.07929 → 0.03539/0.04187; MuPDF
  0.07471/0.08346 → 0.03862/0.03925).
- A 2026-07-27 focused residual pass then closed five Sweep-15 workstreams
  before the separate malformed page-tree closure: indirect scalar CCITT
  `/DecodeParms`; signed/variable-precision JP2 palettes and literal sub-8-bit
  PDF `/Indexed` JPX indices; Microsoft
  format-0/6 byte cmap fallback; `/NonSymbolic` family classification; duplicate
  Type 1 `/Subrs`; shading-painted image-mask stencils; and synthesized static
  appearances for Highlight/Underline/Squiggly/StrikeOut annotations that omit
  `/AP /N`. Targeted real renders are present on `issue5701`, `issue8697`,
  `issue18548_reduced`, `issue12213`, `issue13372`, `bug1538111`, and the
  Byzantine Legacies CCITT page. `issue12213` is `ok` against both controls
  (ink deltas 0.00146 PDFium and 0.00129 MuPDF). Focused regression suites
  pass; see the dated closure block at the top of `handoffs/DEFERRED.md`.

## Next refinement pass

The immediate next renderer work remains focused triage from Sweep 15, not another
corpus-wide sweep. Treat disagreements as actionable only after checking both
PDFium and MuPDF; either oracle can be the outlier. The recurring MRC
scanned-book class, malformed page-tree class, and the dated 2026-07-27
residual classes described above are closed; select another evidence-backed
cluster rather than continuing speculative JPEG, page-wide tone, malformed
tree, or already-fixed markup/font work.

Known longer-term or deliberately deferred areas remain documented in
`handoffs/DEFERRED.md`. Reconfirm each against the latest sweep memory before
starting it. Notable candidates include:

- remaining JPX/JBIG2 spec-edge compatibility cases;
- general ICC profile/CMM coverage beyond the supported RGB matrix/TRC and
  CMYK Lab-LUT image shapes;
- the renderer's incomplete WGPU execution backend (distinct from the
  completed viewer compositor) and broader viewer-driven rendering features.

Post-move refinement on 2026-07-25 closed two candidates from that list:

- coverage-kernel and clip-mask rasterization now poll the request token
  within a single long fill, discard partial masks, and reset reusable raster
  scratch before returning `RenderError::Cancelled`;
- a rebuilt document whose page tree still yields zero real pages now scans
  live xref entries (including object-stream members) for explicit orphan
  `/Type /Page` leaves, applies depth-bounded `/Parent` inheritance, and
  recovers them in deterministic object-number order. Non-array `/Kids` now
  correctly counts as a lost subtree and reaches this escalation path;
- the CPU raster benchmark's request fixture now carries the production
  `RenderColorPolicy`, restoring `cargo check --all-targets` coverage after
  the request contract changed.

Post-move refinement on 2026-07-26 closed four more known gaps:

- unknown-length inline RunLength images now frame on the format's structural
  byte-128 EOD marker. The packet walker skips literal and repeat payloads, so
  a whitespace-bounded `EI` or literal `0x80` cannot terminate the image early;
- `pdfium-diff` now records continuous page darkness and requires it to
  corroborate thresholded-ink disagreements, suppressing equal-energy
  crisp-versus-soft scan noise without weakening gross-pixel or missing-ink
  detection. Both legacy and multi-renderer CSV schemas carry the new metric;
- JPX `/SMaskInData 2` now safely un-premultiplies decoded GrayA/RGBA samples
  before using the opacity plane as a soft mask, including canonical alpha-zero
  handling, instead of dropping the draw;
- parsed ICCBased CMYK Lab-LUT transforms are cached, carried through
  backend-neutral image IR, and applied to codec-backed CMYK JPEG/JPX samples.
  Unsupported ICC profile shapes still fall back to the documented device
  policy.

## Move-time verification

After workspace integration, all 24 renderer packages passed `cargo check` and
their complete Cargo test/doc-test run from the Lege root workspace. The
viewer is now a direct root-workspace member. Its Stage 1 PDF/CPU/softbuffer
path remains the reference implementation; the Stage 2
PDF/CPU-render/WGPU-compose path also builds cleanly. Synthetic and real-PDF
architecture tests pass, and Windows window smoke tests exercise both strict
presenters against the canonical renderer's `hello_world.pdf` fixture.
