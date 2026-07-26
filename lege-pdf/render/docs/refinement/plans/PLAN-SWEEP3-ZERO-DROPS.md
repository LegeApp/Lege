# Sweep-3 zero-drop plan

> **Implementation status (2026-07-18).** Workstreams **A** (all 21 hard
> failures) and **C** (198 destroyed CMYK/YCCK pages) are **implemented and
> green**: the whole workspace now `cargo check`s and `cargo test -p pdf-content
> -p pdf-image` **passes on Windows** (the Linux-only dep paths for
> `jp2lam`/`jbig2enc-rust` were switched to relative `../../`, making the tree
> build cross-platform). What remains for A/C is only the **oracle differential
> rerun** against the real corpus (Linux — the 11k-file corpus lives there) plus
> extracting two corpus-derived regression fixtures. Workstream **C** corrected
> the plan's hypothesis: the real fix is emitting *raw libjpeg CMYK* and letting
> the PDF `/Decode [1 0 …]` array do the polarity flip (PDFium does no
> decoder-side un-inversion) — see DEFERRED.md item 2. **B1/B2 (JPX blanks,
> ~610 pages) are implemented and verified** in the sibling `jp2lam` crate.
> Extracting real corpus streams and diffing against OpenJPEG (`opj_decompress`)
> showed the blocker was not multiple tile-parts but **four** gaps — an
> over-strict `ftyp` brand check, missing **QCC** quantization, LRCP-only
> progression, and over-strict **TNsot** — all fixed and each verified
> bit-exact to OpenJPEG, with the full jp2lam suite (incl. the 441-stream
> baseline) and pdf-image/pdf-content tests green; a previously-blank
> *Zen-essence* p0 now renders (ink 0.90 vs 0.0). **B3**'s silent-blank sites
> are pinpointed (`pdf-render-cpu/src/prepared.rs:592`/`:401`) but the
> diagnostic-channel feature is unbuilt. **D**/tooling untouched. Note: the
> corpus and `jp2lam` are on the shared `D:` (Samsung 1TB) drive — same
> dual-boot machine — so extraction/decode verification runs on Windows; the
> full PDFium differential sweep still wants the Linux `libpdfium.so`. Next
> actions are in §8.


Goal: the third full corpus sweep (`pdfium-diff` over all 11,206 inputs)
reports **zero dropped pages**. This plan defines "dropped", enumerates every
gap in `DEFERRED.md` that can produce a drop — including deferred capability
that PDFium has and we don't, where the triggering file plausibly exists in
the wild — and orders the fixes so each is gated by a focused test plus the
oracle before the sweep is re-run.

## 1. Definition of "dropped page" (the sweep-3 exit gate)

A page is *dropped* if any of:

1. **Hard drop** — the row is `compile failed` (or the document refuses to
   open at all, which drops every sampled page of that file).
2. **Silent blank** — we "successfully" render an empty or near-empty surface
   where PDFium paints real content: `ours_ink < 0.001 && ref_ink > 0.1`.
   (The sweep-2 count at the stricter `ref_ink > 0.3` cut is 610 pages in 451
   documents; the gate uses 0.1 so partial-blank MRC pages can't hide.)
3. **Destroyed content** — polarity/decode errors that replace the page with
   noise rather than blanking it: `ink_delta > 0.3` on a non-blank page.
   These are drops in every sense that matters to Lege (the page is unusable)
   even though ink is present. Sweep-2 count: 198 pages darker than PDFium
   (the CMYK/YCCK class) + 111 pages lighter (residual missing-ink tail).

Exit gate: sweep 3 reports **0** rows in class 1, **0** rows in class 2, and
class 3 is empty *or* every remaining row is triaged to a named non-drop cause
(e.g. AA-weight drift) with a triptych on file. Add these three classes as
first-class counters to `pdfium-diff`'s report (see §6) so "0 dropped" is a
number the tool prints, not a query someone has to remember.

Out of scope: the 8 inputs PDFium itself cannot open (no oracle exists), and
the 737-page just-above-0.05 drift band + raster-noise band — those are the
raster-quality pass, not drops.

## 2. Workstream A — eliminate the 21 hard compile failures

All 21 rows share three causes (verified against `results.csv`). Strategy for
each: *first* make the failure recoverable (skip the malformed construct,
`note_recovery`, render the rest of the page), *then* fix the underlying
parser so the construct actually renders. Recovery alone empties the failed
class; the parser fix is what makes the pages correct.

### A1. Malformed inline images missing `EI` — 10 rows
Files: seven `2021xxxx-statements-8645-.pdf` (p1/p2), `Statement 01-19-2024.pdf`
p1, `Statement 02-18-2024.pdf` p1, `document2.pdf` p4.

- Replace the whitespace-delimited `EI` scan with **length- and filter-aware
  framing**: compute the expected data length from `W`/`H`/`BPC`/`CS` (and
  `/L` when present) for uncompressed data; for filtered data, decode
  incrementally until the filter's natural end (Flate stream end, DCT EOI,
  CCITT RTC/known rows, ASCII85 `~>`), then resynchronize on `EI`. This is
  the Phase 2 "approx" item in DEFERRED — it graduates here because it is now
  the single largest hard-failure cause.
- Recovery path: if framing still fails, scan forward to the next plausible
  operator boundary, drop only the inline image, `note_recovery`, continue.
- Fixtures: extract the inline-image region from one 2021 statement and one
  2024 statement (they are likely two generator variants) into
  `pdf-content` tests; oracle-gate all 10 real pages.

### A2. Zero-output / corrupt Flate content streams — 9 rows
Files: five sampled *Magic Lotus Lantern* pages (p43/86/129/172/215) + four
more across five documents.

- `inflate_with` already salvages a non-empty prefix; the remaining error is
  the **zero-output** case (truncated before any byte inflates). Change
  `append_content` policy: a zero-output stream is skipped exactly like a
  truncated one — `note_recovery`, render remaining streams, blank page if it
  was the only one (matching PDFium/viewers). DEFERRED item 3 already names
  this; the test to add is the zero-output sibling of
  `truncated_flate_salvages_its_inflated_prefix`.
- Also try raw-deflate/zlib-header resync heuristics before giving up (some
  broken generators emit raw deflate without the zlib header) — cheap, and
  may recover real content instead of a blank.
- Verify no *resource* decode failure can still abort a paintable page (the
  DEFERRED item 3 tail): audit every `?`/error return between `append_content`
  and executor completion for page-fatal propagation.

### A3. Malformed operator/token — 2 rows
*Trotsky Revolution Betrayed* p27 (unexpected `)`), *Cambourne New Settlement*
p25 (`l` with missing operand).

- Content-parser error policy: an unparseable token or an operator with an
  operand-stack underflow **drops that operator** (with `note_recovery`),
  resets the operand stack, and continues at the next token — PDFium's
  interpreter behavior. Never page-fatal.
- Fixtures: two tiny synthetic streams reproducing each malformation, plus
  oracle gates on both real pages.

Acceptance for Workstream A: `compile failed` = **0** on a re-run over just
these 12 documents, with every recovered page visually sane against PDFium
(inkΔ ≤ 0.05 or triaged).

## 3. Workstream B — eliminate the 610 silent blanks (JPX) and make blanks impossible to hide

> **STATUS 2026-07-18: B1 largely DONE, B3 DONE.** The plan's hypothesis
> (multiple tile-parts) was wrong — that path already worked. The real blank
> causes were **eight distinct `jp2lam` gaps**, found by censusing the full
> 424-file blank corpus and diffing every extracted stream against OpenJPEG.
> Fixed and verified bit-exact vs OpenJPEG: (1) `ftyp` major-brand `jpx `
> over-strict [dominant, ~1640 streams], (2) QCC per-component quantization,
> (3) non-LRCP progression, (4) TNsot/multi-tile-part advisory, (5) **raw J2K
> codestreams** (SOC detection, max diff 0), (6) **CMYK JPX** (EnumCS 12,
> 4-component + MCT-on-first-3), (7) **decomposition-level bound** floor→ceil,
> (8) **PLT/PLM/TLM** length markers ignored. End-to-end oracle: the CMYK
> *Early-Buddhist* file and the raw-J2K/other blank files now render matching
> PDFium with **0 silent-blanks, 0 degraded**. Remaining JPX deferrals (each a
> small count): precincts/SOP/EPH (~10 files), degenerate 1-sample DWT split
> (rejected not mis-decoded — B3-flagged), truncated-stream salvage
> (zero-length-contribution / data-after-packets), `pclr` palette, and the
> no-`jp2`-compat-brand handful. See DEFERRED.md corpus item 1.

### B1. jp2lam: multiple tile-parts per tile — the P0 blocker (SUPERSEDED — see status above)
608 of the 610 blanks decode to nothing because `jp2lam` rejects
`unsupported tiling: multiple tile-parts are not implemented`
(representative: *The African Stakes of the Congo War* p0).

- Implement tile-part assembly in jp2lam: collect all SOT segments per tile
  index (`Isot`), order by `TPsot`, honor `TNsot`/psot=0 (last tile-part runs
  to EOC), concatenate bitstreams per tile, then feed the existing single-tile
  decode path. This is stream-assembly work, not new entropy coding.
- Fixtures: the extracted JP2 from the Congo War page (or a minimal
  derivative) into jp2lam's test corpus and `pdf-image` tests; re-verify the
  441-stream openjpeg bit-exactness suite is unaffected.

### B2. jp2lam: the rest of the real-world JPX surface (drop-capable deferrals)
These are deferred features PDFium (via OpenJPEG) supports and real files use.
Each one currently produces a decode error → silent blank, so each is a
class-2 drop waiting for the file that triggers it. Ordered by real-world
likelihood:

1. **Raw J2K codestreams outside a JP2 container** — explicitly legal as
   `/JPXDecode` data per ISO 32000 and produced by several generators. Detect
   SOC (0xFF4F) at offset 0 and enter the codestream decoder directly.
2. **`cdef` / JPX alpha (`/SMaskInData`)** — scanned-with-alpha files exist;
   currently the alpha channel (or the whole image) is lost.
3. **`pclr` palette + `cmap`** — palettized JP2 is common in DjVu-style
   re-encodes.
4. **Subsampled components** (non-1x1 `XRsiz/YRsiz`) — 4:2:0 JP2 exists in
   camera/scan pipelines.
5. **Non-LRCP progression (RLCP/RPCL/PCRL/CPRL) + POC** — RPCL is emitted by
   Kakadu in common configurations.
6. **COC/QCC** (per-component coding/quant overrides) — cheap once the
   marker plumbing exists; Kakadu emits these too.
7. **Precincts + SOP/EPH** — required for the RPCL class of files.
8. Lower priority (rare in archival corpora): CMYK (EnumCS 12), sYCC (18),
   `bpcc`, arithmetic-bypass and other non-default code-block styles —
   implement or, at minimum, ensure they fail *loudly* (see B3) rather than
   blank.

Each gets a synthetic fixture (Kakadu/openjpeg-generated) in jp2lam plus at
least one corpus-extracted stream where one exists.

### B3. No-silent-blank invariant (the structural fix)
The 610 pages were invisible until the sweep because the codec error is
swallowed below the compiler. Independent of any codec work:

- Every codec decode failure at lowering/draw time must emit a
  `note_recovery` diagnostic naming the codec and reason, and increment a
  per-page `degraded_draws` counter surfaced in render output.
- `pdfium-diff` records that counter per row (new CSV column), so a "clean"
  row with degraded draws can never masquerade as a true pass again.
- Add a debug assertion path (sweep mode) that flags any page with
  `ours_ink < 0.001` and ≥1 degraded draw as class-2 automatically.

### B4. Residual missing-ink re-rank
DEFERRED item 4: after B1 lands, re-run the single-document oracle fixtures
for the 111 lighter-than-PDFium non-blank pages. Expectation: most are
JPX/MRC and clear with B1/B2. Whatever remains (the ICCBased-image tail)
gets triaged under Workstream D3 — do not start an ICC project before this
re-rank.

## 4. Workstream C — 4-component DCT polarity/color (198 destroyed pages)

DEFERRED item 2. Representative: *Eighteen Songs of a Nomad Flute* p0 —
DeviceCMYK `/DCTDecode`, Adobe APP14 transform 2 (YCCK), ours 99.9% ink vs
PDFium 2.1%: classic **missing CMYK inversion** symptom (Adobe writes CMYK
JPEGs with inverted samples; APP14-present files need `255−x` post
YCCK→CMYK).

- Audit, against PDFium's `JpegmoduleDecode`/color path, in this order:
  (1) APP14 transform handling (0 = no transform vs 2 = YCCK), (2) the
  Adobe-CMYK sample inversion, (3) PDF `/Decode` application point, (4) the
  component hand-off from `pdf-image` to `pdf-color`'s frozen CMYK table.
  Per DEFERRED's constraint: prove the mismatch is *before* the frozen
  DeviceCMYK table before touching anything downstream of it.
- Fixtures: the extracted Nomad Flute JPEG + synthetic plain-CMYK, YCCK,
  ICCBased-4, and Separation-image JPEGs with known swatch values.
- Oracle-gate the worst 10 documents of the 137, then confirm the 198-page
  class collapses in a targeted re-run.

## 5. Workstream D — deferred-but-real-world capabilities that cause drops

These are DEFERRED items where PDFium renders and we refuse, blank, or
mangle — i.e., latent class-1/2/3 drops for files not (yet) in the corpus
but expected in the wild. Ordered by likelihood × severity.

### D1. Document-level (whole file drops — worst severity)
- **AES-256 / R6 encryption** (Phase 1 deferral). Currently a clean typed
  refusal — every page of such a file is a hard drop. AES-256/R6 is the
  *only* encryption new PDFs are written with (PDF 2.0); these files are
  common (bank statements, government docs). Implement R6 key derivation
  (SHA-256/384/512 hash loop of Algorithm 2.B), `/Perms` validation, and
  AES-256-CBC via the existing AES core in `pdf-security`. Empty-user-password
  flow first (matching the current RC4/AES-128 posture). Fixture: a
  qpdf-generated AES-256 file + round-trip tests.

### D2. Page/content-level blanks (class-2 latents)
- **DCT-coded `/SMask` streams** (the smask build path skips codec filters) —
  a DCT SMask today silently loses the mask; depending on polarity that
  blanks or un-masks the image. Route SMask streams through the same codec
  registry as the base image. Common in optimized/exported PDFs.
- **DCT arithmetic coding** — rare but real (some scanner firmware); today
  a typed error → blank under B3. Implement or accept as a *loud* failure;
  decision point after checking corpus frequency (grep the corpus for
  SOF9/SOF10 markers before committing to the work).
- **DCT 12-bit precision / lossless-hierarchical** — same treatment:
  measure frequency first; loud failure is acceptable if absent from both
  corpus and Lege's intake profile, but it must be class-2-visible, never
  silent.
- **Embedded/predefined CJK CMaps** (Fonts: CMaps *approx*). A Type 0 font
  with `/Encoding /GBK-EUC-H` etc. currently maps wrong or not at all — CJK
  text pages render blank/garbled. Any Chinese/Japanese/Korean book in
  Lege's intake hits this. Implement the predefined CMap set (ship PDFium's
  compact CMap tables) + embedded CMap stream parsing; Identity fallback
  with a recovery note in the interim.
- **Shading types 1, 4–7** (currently `/Background`-only hook). Type 4/5
  (free-form/lattice triangle meshes) and 7 (tensor patches) appear in
  CAD-exported and illustration-heavy PDFs; today the `sh` paints nothing →
  partial blank. Implement 4/5 first (triangle rasterization reuses the
  existing coverage kernel), 6/7 by tessellating patches to triangles,
  type 1 (function-based) via the existing `pdf-function` sampler.

### D3. Page-level mangling (class-3 latents)
- **Lab-colored images** (Phase 3): per-sample `lab_to_rgb` at decode time —
  arity mapping currently produces wrong colors on the whole image.
- **ICCBased images/fills beyond arity** — only if B4's re-rank leaves an
  ICC tail; then parse profile headers minimally (N-component + rendering
  intent → nearest device space), not a full CMM.
- **`/None` colorant paints white instead of suppressing marks; `/All` not
  special-cased** — small, spec'd, cheap; prepress files hit both.
- **Vertical writing (`/DW2`, `/W2`)** — CJK vertical books mis-lay-out
  every glyph; pair with the CMap work in D2 since the same corpus triggers
  both.
- **Anisotropic/rotated stroke pens (affine §13)** — visible mangling on
  non-uniform CTMs; keep at current rank (below sweep-driven work) unless
  class-3 triage in sweep 3 surfaces it.

Explicitly *not* in this plan (no drop mechanism): knockout groups,
non-isolated groups, soft-mask `/BC`//`TR`, synthetic bold/italic, MacExpert,
hinting differentials, all performance items (caches, SIMD, band/tile,
scheduler refinements). They stay in DEFERRED under the raster-quality and
perf passes.

## 6. Sweep-3 tooling changes (do these first — they gate everything)

1. `pdfium-diff`: add the three drop classes of §1 as printed counters
   (`hard_drops`, `silent_blanks`, `destroyed`) and the `degraded_draws`
   CSV column from B3. Cost: small; makes every later fix measurable.
2. Add a `--rerun-failures <prior results.csv>` mode that re-runs only rows
   that were dropped in a prior sweep — the cheap inner loop for Workstreams
   A–C without paying the full 66k-page sweep each time.
3. Keep the same 65,990 page keys so sweep-3 transition counts stay exact.

## 7. Execution order and gates

| # | Work | Drops removed | Gate |
|---|------|--------------:|------|
| 0 | §6 tooling (counters, rerun mode) | — | counters visible on sweep-2 CSV replay |
| 1 | A2 + A3 recovery policy (small, unblocks) | 11 hard | 12-doc rerun: 0 compile failed |
| 2 | A1 inline-image framing | 10 hard | statement fixtures + 10-page oracle |
| 3 | **B3 no-silent-blank invariant — DONE** | 0 (visibility) | ✅ `RenderStats.degraded_draws` + `is_silent_blank()`; `pdfr render` warns; `pdfium-diff` has a `degraded` CSV column + `silent-blank(codec)` note + summary counters; render-cpu tests green |
| 4 | **B1/B2 jp2lam JPX — DONE** (brand + QCC + progression + TNsot + decomp-ceil + CMYK + pclr) | ~609 blanks + latents | ✅ 424-doc census drove 7 fixes, each bit-exact vs openjpeg; jp2lam suite green incl. 441-stream baseline; Zen p0 0.0→0.90 ink. Remaining: degenerate-DWT (~2) + truncation-salvage (~3) as clean B3-flagged failures. Oracle rerun over 451 docs pending |
| 5 | **C 4-component DCT — verified** | 198 destroyed | ✅ single-file oracle on *Eighteen Songs of a Nomad Flute*: p0 0.999→0.021 ink matching PDFium's 0.020 (inkΔ 0.0003), all 6 sampled pages suspect-free with the current `pdfium.dll`. 137-doc rerun still pending |
| 6 | B4 residual re-rank | ~111 | triage sheet: every row named |
| 7 | B2 JPX residual surface (raw J2K, cdef, pclr, precincts, …) | latents | jp2lam fixtures, openjpeg differential |
| 8 | D1 AES-256/R6 | latent doc drops | qpdf fixture round-trip |
| 9 | D2 items (DCT SMask → CMaps → shadings 4–7 → measured DCT variants) | latents | per-item fixtures + any corpus hits |
| 10 | D3 items as sweep-3 triage demands | latents | class-3 triage empty or named |
| — | **Full sweep 3** after step 6 (steps 7–10 continue after) | | **hard_drops = 0, silent_blanks = 0, destroyed triaged to 0-or-named** |

Rationale for running sweep 3 after step 6 rather than after step 10: steps
1–6 remove every drop the corpus *actually contains*; 7–10 remove drops the
corpus could contain. If sweep 3 then surfaces a file exercising a D-item,
that item's rank jumps and its fixture comes free from the corpus.

Every fix follows the established DEFERRED discipline: focused unit fixture
(synthetic or corpus-extracted) **and** an oracle gate on the real page(s),
recovery paths observable via `note_recovery`, and no silent behavior change
to frozen surfaces (the DeviceCMYK table, the reference-output contract).

## 8. Next actions (on the Linux box, in order)

1. ~~`cargo build` + `cargo test -p pdf-content -p pdf-image`.~~ **Done on
   Windows 2026-07-18** — both crates check clean and all tests pass, including
   the new recovery tests (`stray_close_paren_token_recovers`,
   `zero_output_flate_content_yields_blank_page_with_note`, the three inline-image
   framing tests) and the CMYK-polarity tests (`ycck_adobe_white`, etc.). Path
   deps were made relative so the tree builds cross-platform. Run the full
   `cargo test` (all crates) on Linux as a final check before the oracle rerun.
2. Extract the two named regression fixtures the agents could not reach:
   `pdf-image/tests/fixtures/nomad_flute_p0.jpg` (DeviceCMYK YCCK, expect
   post-`/Decode` mean ink ≈ PDFium 2.1%) and the false-`EI` inline-image
   region from one 2021 + one 2024 statement into `pdf-content` tests.
3. Targeted oracle reruns (add the `--rerun-failures` mode from §6 first): the
   12 hard-failure documents (expect `compile failed` → 0) and the 137
   four-component-DCT documents (expect the 198 over-ink pages to clear).
4. ~~Implement B1 in `jp2lam`.~~ **Done 2026-07-18** — four gaps fixed (brand,
   QCC, progression, TNsot), verified bit-exact vs `opj_decompress`, jp2lam +
   pdf-image/pdf-content suites green. Remaining for B1: the full-corpus oracle
   rerun over the 451 blank-JPX docs (expect the ~609 blanks to clear), and
   triage the 1-in-119 residual `zero-length code-block contribution` stream.
5. ~~Build the B3 no-silent-blank channel.~~ **Done 2026-07-18** —
   `RenderStats.degraded_draws`/`recovery_notes`/`is_silent_blank()`, `pdfr
   render` warning, and a `pdfium-diff` `degraded` CSV column + `silent-blank`
   note + `silent-blanks`/`degraded` summary counters. Any JPX residual (or new
   codec gap) that still blanks a page is now flagged, not scored clean.
6. Then the rest of §3/§5 (B2 residual JPX surface, C oracle rerun) in plan order.
7. Only after A/B1/C/B3 land: full sweep 3 with the §1 counters (now including
   the printed `silent-blanks`/`degraded` totals). Gate: `hard_drops = 0`,
   `silent_blanks = 0`, `destroyed` triaged to 0-or-named.
