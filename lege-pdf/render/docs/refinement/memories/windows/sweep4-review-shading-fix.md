---
name: sweep4-review-shading-fix
description: Sweep-4 corpus review findings and the type 2/3 shading /ColorSpace fix (2026-07-21)
metadata: 
  node_type: memory
  type: project
  originSessionId: b9ca21b5-e4c0-4e6a-80d2-998653205910
  modified: 2026-07-21T09:33:54.713Z
---

Sweep 4 (2026-07-21) = `pdf-renderer/tools/pdfium-diff/pdfium-diff-out/results.csv`
(19,449 files / 98,135 pages), produced by the 09:05 binary (HEAD ≈ cb6183a).
It was a RESUMED run (CSV header lacks `flags=annot`, inherited from a pre-existing
file → mixed-vintage rows). Fidelity excellent: median inkΔ 0.0028, p99 0.10,
99.3% ≤0.05. Tail is mostly oracle-side failures (567 `pdfium open/render failed`),
not our bugs. Confirmed fixed & reflected: DS82 (p4 0.186→0.005), DLIFLC (p28 0.076→0.008).

**Annotation "asymmetry" — RETRACTED (was wrong).** My first review claimed the diff
tool rendered PDFium with `FPDF_ANNOT` but ours with annotations off (`AnnotationMode::None`
at `main.rs:1434`/`:1696`), inflating the 542 under-ink deltas. FALSE: `RenderRequest.annotations`
is a **vestigial field the CPU backend never reads** — annotations are baked into the
display list at COMPILE time via `PageCompiler::with_annotations(true)`, which every
diff-tool grading path already calls (lines 1184/1219/1526/1678). The sweep was already
symmetric (both sides annot-on). Commit 5cab73f set the field to `StaticAppearances` for
clarity only (no behavior change). So the 542 under-ink deltas are GENUINE (missing
content / codec / real fidelity), not annotation-inflated. Only cleanliness step for a
restart: run from a FRESH workdir so the CSV header stamp is correct and no mixed-vintage
rows (run-sweep-windows.ps1 already does this → sweep-annot-baseline\).

**Fixed this session (2026-07-21):** type 2/3 (axial/radial) shadings ignored their
`/ColorSpace` — `build_ramp`→`comps_to_rgba` dispatched by arity only, so a
2-colorant DeviceN (`[/DeviceN [/Cyan /Black] /DeviceCMYK …]`) hit the `_ => [0,0,0]`
arm → pure black; the background argyle flooded the whole page, and the 0.5 group
turned it uniform 50% gray. This was the DS82 Separation/CIE fix's shading analog,
never applied to shadings. Fix in `crates/pdf-content/src/interpret.rs`: new
`build_shading_ramp` + `resolve_shading_cs` + `ShadingCs` enum route function
outputs through `eval_tint` (Separation/DeviceN) / `eval_cie` (Lab/Cal) /
`cs_to_rgba` (Device/ICC/Indexed). Regression test
`axial_shading_honors_multi_colorant_devicen_colorspace` in `tests/shading_tests.rs`.
Verified on rosiesmenu3: p0–p4 collapsed from ~0.29–0.81 to 0.002–0.065.
**UNCOMMITTED** as of session end (master, no remote). Recorded as item 7 in
`pdf-renderer/POST-SWEEP-VERIFY.md`.

**Second fix (2026-07-21), committed 7fc70c1:** TIFF Predictor 2 (`/Predictor 2`)
bailed to identity for `bits_per_component != 8` in
`crates/pdf-structure/src/predictor.rs::apply_tiff` — a 1-bpc DeviceGray scan kept
its horizontal deltas un-summed → near-solid ink. Generalized to unpack/difference/
repack per component for any bpc (fast path kept for 8). Verified: issue6071 & 2958
inkΔ 0.922→0.0007. Unit tests added (1-bit, 4-bit/2-color).

**Fourth fix (2026-07-21), committed 76f69bc:** defensive geometry clamps in
`crates/pdf-render-cpu/src/prepared.rs` against corrupt/adversarial input (4778.pdf
motivating case — damaged deflate content decodes to garbage `set-line-width 11016766`
+ mangled CTMs that flooded the page). Two generalized guards: (1) stroke-width clamp —
pen device extent `dw × σ_max(m2)` > output ⇒ fall back to hairline (catches absurd
LineWidth AND garbage CTM whose huge column folds into m1 scale); (2) `device_bounds_sane`
drops any fill/stroke with non-finite or >64×-viewport device coords (folded into the
existing min/max pass, no extra traversal). 4778 p0: ours-ink 1.0→0.046 (PDFium 0.058),
floor plan renders on white; a residual page-sized-but-in-range green blob remains (didn't
chase — would need risky threshold tightening). Known-good pages (DS82/rosiesmenu) byte-
unaffected. 5 tests added. NOTE: could not re-grade via pdfium-diff (binary was locked by
the user's running sweep 5) — verified via direct pdfr render + ink computation.

**Fifth fix (2026-07-21), committed 18fd8ab:** shading `/BBox` clip (ISO 32000-1 §8.7.4.3)
was ignored — a shading pattern/`sh` paint covered its whole fill shape instead of the BBox
sub-rectangle. pattern_shading_bbox.pdf (ShadingType 1, /BBox [50 50 150 150] in /Domain
[20 180]): ours-ink 0.75→0.143, PDFium 0.144 (matches). Threaded normalized /BBox through
SemShading→ShadingResource→PreparedShading; mapped once to a device-space AABB via to_device
(exact for axis-aligned/flip CTMs); shade_pixel returns None outside it (uniform across all
shading types, independent of /Extend). Test added. NOTE: verified via pdfr render + pdfium
panel from the old (executable, locked) diff binary — could not re-grade (sweep 5 holds the
binary lock).

**Sixth fix (2026-07-21), committed 0ee40d2:** two shading-paint gaps.
(a) Shading-pattern TEXT fills — DrawGlyphRun skipped non-solid paints, so shading-filled
text rendered nothing. PreparedGlyphRun now carries an optional prepared shading;
shade_glyph_span colors each covered pixel from it (both fast + masked paths; glyph cache
stores coverage not color so it's unaffected). pattern_shading_on_text ink 0.000→0.033
(PDFium 0.033, pixel-perfect per-glyph gradient).
(b) Axial/radial /Background — types 2/3 dropped /Background (only grid/mesh had it), so a
no-/Extend pattern fill left the surround unpainted. Threaded /Background through
SemShadingKind/ShadingKind Axial+Radial; shade_pixel returns it for out-of-axis pixels when
/Extend off (`sh` still ignores it per §8.7.4.3). pattern_shading_type2_no_extend_with_background
ink 0.644→0.396 (PDFium 0.400). Tests added.

Six code fixes committed on master (no remote): 079ead0 (shading colorspace), 7fc70c1
(TIFF predictor), 5cab73f (annot-field clarity), 76f69bc (defensive geometry clamps),
18fd8ab (shading /BBox clip), 0ee40d2 (shading text fills + axial/radial background). The
pattern_shading fixture cluster is now essentially resolved (bbox, on_text, no_extend_bg all
match PDFium). All verified via pdfr render + pdfium panel from the locked-but-executable diff
binary — a fresh full re-grade awaits sweep 5 completing and freeing the binary lock.

Still-open genuine residuals (each a DIFFERENT root cause, triaged this session):
- **4778.pdf p0**: vector over-fill — FIXED 2026-07-21 (commit 76f69bc, see "Fourth fix"
  above; ours-ink 1.0→0.046). Root cause below. Two earlier hypotheses RETRACTED: it is NOT a number-parse bug and NOT a
  linearized object-resolution bug (object 2 is unrelated — the /Contents is `[38..45 0 R]`).
  TRUE cause: several of the 8 content streams (objs 38-45) are **genuinely CORRUPT** — bad
  deflate adler32 + mid-stream bit-flips (obj 38 = clean-but-bad-adler; obj 39 decodes to
  semantic garbage incl. absurd `10036352183`, `set-line-width 11016766`; obj 40 /Length is
  174 bytes too long). Our decode is CORRECT: I verified gather_content output is
  byte-identical to a reference adler-tolerant raw-inflate of objs 38-45, and NO body length
  yields a passing adler (data truly damaged). Our stream-length repair (parser.rs
  read_stream_body endstream-scan) and flate salvage (decode.rs inflate_with returns full
  output on bad-adler Err) both work. PDFium (same zlib) gets the SAME corrupt content but
  renders it DEFENSIVELY (clamps absurd stroke widths / anisotropic-CTM pens); we draw a
  page-covering green stroke and flood. **Real fix = renderer robustness**: clamp the final
  device-space stroke extent so a garbage width/CTM can't cover the whole page. Site:
  crates/pdf-render-cpu/src/prepared.rs lower_stroke (~2615, decompose_pen m1/m2 split) +
  stroke.rs (scalar half-width). Careful: the anisotropic-CTM case blows up an in-m1-space
  clamp via m2, so clamp POST-m2 device geometry, and bound it to not regress valid thin
  strokes. Diff tool: `PDFIUM_DIFF_DUMP=1 pdfium-diff --worker <lib> 2.0 <outdir> <file>`
  writes triptychs; `pdfr dump <file> <page>` = semantic display list.
- **flate_predictor_bpc_1.pdf**: misnamed — actually PNG predictor 15 with a bpc
  mismatch (image bpc 8 vs DecodeParms bpc 1) and 6109×5187 (~31.7 Mpx); renders
  BLANK (ours 0.0). A decode-robustness/size or bpc-mismatch case, not TIFF predictor.
- missing-content/codec (Daughters of Tunis), JBIG2 generic-region drift
  (bitmap-customat etc.), our open/compile failures (~30 files).
See [[pdf-renderer-state-pointer]].
