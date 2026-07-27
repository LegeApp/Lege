# Deferred work

Central index of intentionally-unfinished portions of implemented phases.
Each item is also documented at its site in code/commits; this file is the
one-stop map. Status legend: **hook** = the IR/API slot exists but is not
populated; **approx** = implemented with a documented approximation;
**todo** = not started.

Fully complete phases (nothing deferred): none list below means done.

Phase 6E hardened resource recursion: forms (invoke-depth + cycle detection),
tiling patterns (shared invoke-depth guard), soft masks (invoke-depth), Type 3
functions (nesting depth cap), and Type 0 sampled-function `/Size` (allocation
cap). Adversarial fixtures (self-referential pattern, cyclic function, absurd
sample count, missing pattern, deep form nesting) terminate without panic —
`crates/pdf-content/tests/adversarial_tests.rs`.

> **2026-07-27 malformed page-tree closure.** The Sweep-15 wrong-page class is
> closed with bounded recovery rather than a global `/Parent` scan:
>
> - An indirect reference written as `N R` is parsed as `N 0 R` and emits the
>   typed `ReferenceGenerationRepaired` event, including the containing object
>   and byte offset.
> - Any provably lost page-tree subtree triggers one full xref rebuild, even
>   when sibling pages survived. The rebuilt walk is adopted only when it
>   recovers more real pages, or the same number with fewer losses.
> - If rebuild cannot recover a lost child but its span is exactly inferable
>   from the parent `/Count` and readable siblings, blank placeholders are
>   inserted at that child's position. Only genuinely ambiguous multi-hole
>   distributions retain count-backed tail padding.
>
> The real Cambourne report now opens as 152/152 pages and reports repair of
> object 923's `335 R`. Its sampled page 100 changed from the wrong-page
> Sweep-15 ink deltas 0.30413/0.29196 to 0.00377/0.00840 and `ok` against
> PDFium/MuPDF. Regressions cover malformed references, partial-xref rebuild
> adoption, exact middle insertion, ambiguous fallback, and truncation.
>
> The two previously red tests were not production defects. The font-cache
> test was scheduling-sensitive because it asserted a wall-clock parse
> threshold; it now tests the retention policy as a pure decision. The JP2
> test expected method-4 two-component data to be rejected after production
> had intentionally adopted Gray-plus-auxiliary-plane recovery. The corrected
> suites pass: `pdf-render-cpu` 67/67 and JP2 309 passed/5 intentional ignores.

> **2026-07-27 Sweep-15 residual pass — five focused workstreams.** This
> five-workstream pass deliberately excluded malformed page-tree recovery; the
> follow-on closure above handles it. The evidence-backed non-structural
> residuals are now closed:
>
> - CCITT scalar values inside `/DecodeParms` resolve when indirect (`/K`,
>   `/Columns`, `/Rows`, `/BlackIs1`, `/EncodedByteAlign`, `/EndOfLine`,
>   `/EndOfBlock`). The Byzantine Legacies p102 two-page scan now decodes
>   cleanly instead of as vertical noise.
> - JP2 `pclr` entries retain signedness and 1–32-bit precision through palette
>   expansion; widths above 32 bits are a typed decline. When a PDF `/Indexed`
>   color space overrides the container palette, sub-8-bit JPX samples also
>   remain literal palette indices instead of being widened as color
>   components. `issue12213.pdf` is now `ok` against PDFium and MuPDF (ink
>   deltas 0.00146 and 0.00129).
> - Native TrueType cmap fallback now reads byte-oriented format 0/6 tables
>   under Microsoft `(3,1)` as well as Macintosh `(1,0)` (`issue5701.pdf`);
>   `/NonSymbolic` prevents family names such as `SegoeUISymbol` from being
>   misclassified as the Symbol face (`issue8697.pdf`); and the Type 1 parser
>   keeps the first `/Subrs` section rather than overwriting it with a later
>   conditional definition (`issue18548_reduced.pdf`).
> - CPU lowering now paints `/ImageMask` stencils through `Paint::Shading`,
>   using the prepared stencil as coverage inside the ordinary
>   clip/soft-mask/alpha/blend compositor. The experimental GPU vector seam
>   declines the command because it already declines shading commands.
>   `issue13372.pdf` now carries its gradient through the authored mask.
> - Highlight, Underline, Squiggly, and StrikeOut annotations without a usable
>   `/AP /N` receive synthesized geometry from `/QuadPoints`, `/C`, and `/CA`.
>   Authored appearances remain authoritative; highlight uses Multiply and the
>   conventional yellow default. `bug1538111.pdf` now renders all four markup
>   classes.
>
> Focused regressions cover each parser/render path. The full `pdf-font` suite,
> content annotation/image integrations, shading integrations, and corrected
> CPU/JP2 library suites pass.

> **2026-07-21 production-readiness pass — batch closure.** The three-stream
> hardening pass (merged to master 2026-07-20/21) closed many items below;
> each affected section carries a dated note, this is the map. **Closed:**
> mesh shadings types 1/4–7 (parse + rasterize); knockout groups; soft-mask
> `/BC` + `/TR`; non-Normal blends generalized to images/shadings/tilings;
> annotations (`/Annots` + `/AP /N`, rendered by default) and optional
> content (`/OCProperties` default visibility, `/OC`, `/OCMD`) — neither had
> an entry here because they were unstarted features, both are now in;
> CID→Unicode tables + vertical writing + the opt-in CJK substitution
> bridge; full user/owner password support (R2–R6) with real `/U`
> validation; the per-image decode cache (`SharedImageCache`); image-edge
> partial-coverage AA (first half of workstream H); and the never-panic
> program — `catch_unwind` page/scheduler boundaries (`RenderError::Panic`),
> workspace `unwrap_used`/`expect_used`/`panic` clippy lints at deny, the
> Type 1 `unreachable!` removed, clip-mask expects downgraded to degraded
> skips, a `lock_unpoisoned` policy, the `fuzz/` workspace (8 targets), the
> stable `pdf-chaos-tests` mutation gate, and the `pdf-read` doctor crate
> (`pdfr doctor`). The degenerate tint-LUT repair added
> `ImageIr.lowering_degraded`, closing the last silent-blank tracking hole.
> **Still open (honest residuals):** minification weight quality
> (area-average tails; stencil boldness residue on DS82/PhRMA/DLIFLC
> pages) — **closed 2026-07-21** (fractional-tap box-filter weights, see
> Phase 4 4G; the residual DS82/DLIFLC deltas proved to be *other* causes:
> form-widget highlight color and a notdef-fallback font, recorded under
> "Image minification" below); the full corpus re-sweep on the Linux box under the new
> annotations-on baseline (`tools/pdfium-diff` now renders both sides with
> annotations — all older sweep CSVs are baseline-incompatible) and the
> fuzz soak (nightly + Linux); ICCBased arity approximation; the JPX
> residual classes; JBIG2 spec-edge rejections; embedded-file crypt filters
> and `/Perms`; MacExpert / full-AGL tails; Type 1 hinting discarded;
> synthetic bold/italic not applied; WGPU (phases 7–11); the postprocess
> graph (**closed 2026-07-21** — the CPU executor landed, see "Later
> phases"); adaptive band/tile planning; and mesh-shading pdfium-diff fixtures
> are not yet in the diff corpus (**closed 2026-07-21** —
> `corpus/shadings/mesh-type{4,5,6,7}-*.pdf`, oracle grading queued in
> POST-SWEEP-VERIFY.md).

> **2026-07-21 closeout pass — the honest residual set.** A dedicated pass
> implemented every remaining tractable item (each carries a dated note at
> its section: /All + /None verification, inline `/DP`, `gid_for_name`
> index, ICC_COLOR/OVERPRINT flags, Lab + multi-input-DeviceN images,
> `/Extend` re-baseline, the postprocess CPU executor, fractional-tap
> minification, the DS82 Separation/CIE-alternate fix, the DLIFLC
> Macintosh-cmap fallback, synthetic bold/italic application, mid-render
> cancellation, the V5 `/Perms` cross-check, truncated-tree page
> placeholders, MacExpert + full AGL, mesh diff-corpus fixtures).
> **What remains open, all architectural, each justified at its entry:**
> 1. **WGPU backend (Phases 7–11)** — a whole project phase, by plan.
> 2. **ICC CMM / profile parsing** — explicit future policy choice; arity
>    approximation documented (flags now computed).
> 3. **JPX residual classes** — live in the sibling `jp2lam` repo
>    (`HANDOFF-remaining-jpx-decode.md`), not this workspace.
> 4. **JBIG2 defensive spec-edge rejections** — deliberate strictness, not
>    missing features.
> 5. **Embedded-file crypt filters + revision > 6 decline** — no corpus
>    evidence; typed declines in place.
> 6. **Type 1 hinting execution** — hints parsed and discarded per
>    fonts.md's own phase definition.
> 7. **Adaptive band/tile planning (4J)** — deferred by performance advice
>    §2 until direct-page is beaten.
> 8. **Adaptive device-error flattener** — a measured-performance item;
>    fixed subdivision is correct today.
> 9. **Multiple Master substitution** — assessed above (Type 1 MM blending,
>    days).
> 10. **Scheduler permit classes / reorder-buffer accounting +
>     sequence-priority granting** — assessed above (architecture).
> Cosmetic residual granularities noted at their entries (rotated-footprint
> bbox, 256-entry ramp quantization, big-image cancellation latency,
> mid-document placeholder ordering) are documented approximations, not
> open work items.

---

## Phase 1 — document structure & immutable snapshot
- **Encryption (decryption)** — *done*. The standard security handler is wired
  into `pdf-document`: `build_security` parses `/Encrypt` into `EncryptDict`
  (V1/V2/V3 RC4, V4 by `/CF /StmF /CFM` → RC4 or AES-128), derives the file key
  from the empty user password at open, and stores a real `SecurityContext` on
  the snapshot. `resolve.rs::decrypt_uncompressed` decrypts strings and stream
  bodies (converting `InSource` → decrypted `Owned`) with each object's key at
  resolve time. The exemptions are enforced: objects inside an object stream
  are skipped (only the `InObjectStream` arm bypasses decryption — the
  container stream is decrypted whole); cross-reference streams (`/Type /XRef`)
  and the `/Encrypt` dict object are skipped; `/ID` lives in the trailer and is
  never routed through decryption. **AES-256 (V5, revisions 5 and 6) — DONE
  (2026-07-19).** `pdf-security` gained `sha2` (SHA-256/384/512, FIPS 180-4
  vectors) and AES-256-CBC + AES-128-CBC-encrypt (`aes.rs`, FIPS-197 / NIST
  SP 800-38A vectors); `standard.rs` implements ISO 32000-2 Algorithm 2.A
  (empty user-password validation against `/U`, recover the file key by
  AES-256-decrypting `/UE`) and Algorithm 2.B (the R6 SHA-2/AES-128 hash loop).
  `Cipher::Aes256` decrypts every string/stream with the 32-byte file key
  directly (no per-object key). The caller parses `/UE`. Verified against real
  fixtures: pdf.js `empty_protected.pdf` (R6, empty password) opens and renders
  **inkΔ 0.0 vs PDFium**; `issue7665.pdf` (AES-256 with content) matches PDFium
  at **inkΔ 0.0008**; password-protected V5 files (issue21579 etc.) and
  embedded-file-only files are declined cleanly (they need a password PDFium
  also lacks). Fixture test `aes256.rs` + `crates/.../fixtures/`. *Deferred*:
  non-empty / owner-password entry (**done 2026-07-21, production-readiness
  pass**: full user *and* owner passwords, R2–R6, `PasswordRole`,
  `DocumentSnapshot::open_with_password` + `pdf-cli --password`, and real
  `/U` validation on empty-password opens —
  `SecurityError::PasswordRequired`/`IncorrectPassword`); still deferred:
  `/Perms` cross-check, embedded-file crypt
  filters. **`/Perms` cross-check — done 2026-07-21 (closeout pass),
  report-only:** after V5 authentication the 16-byte `/Perms` block is
  AES-256-decrypted with the file key and checked per ISO 32000-2
  §7.6.4.4.12 ('adb' marker, `/P` agreement, `/EncryptMetadata` byte with
  PDFium's `buf[8]=='F' || IsMetadataEncrypted()` leniency). A mismatch
  becomes a `note_recovery` line, never a refusal — PDFium's
  `AES256_CheckPerms` hard-fails there, but the content key is already
  validated against `/U`/`/O`, and declining a tampered-permissions file
  would blank content we can demonstrably decrypt. Round-trip unit tests
  in `pdf-security` (`perms_tests`). A future revision > 6 still declines with a typed `SecurityError`.
  Root-caused and fixed a **latent infinite loop in `pdf-security`'s MD5**
  (`update()` overwrote `buf_len` with the sub-block remainder length,
  clobbering a partial buffer, so `finish()`'s padding loop spun forever) —
  this is why the crypto core's own tests "couldn't run" and key derivation
  never returned. A stale test constant for the 56-byte MD5 vector was also
  corrected.
  Verified: `pdf-security` 15/15 + `pdf-document/tests/encryption.rs` 3/3
  (RC4 string+stream round trip, `/Encrypt`-dict exemption, objstm-member
  exemption) + phase1 gate 17/17; and against the PDFium oracle on real corpus
  files — RC4-128 (111 pp) mean ink_delta 0.0000, AES-128 (670 pp) mean 0.0045,
  zero pages over the 0.05 "wrong" threshold. ~819 corpus pages that refused at
  open now render; re-run the full sweep to reclassify them.

## Corpus oracle ranking

> **Superseded by sweep 3 (2026-07-20, 14,656 files / 84,473 pages):** ours
> failed on **zero** pages PDFium renders; blanks collapsed 610 → 8; 1,740
> pages fixed vs 12 regressed since sweep 2. The current ranked attack plan
> — 3 cover regressions, 2-component JPEG, the 1,808-page lighter class
> (CJK CMaps / transparency completeness / scan-AA sampling) — lives in
> **PLAN-POST-SWEEP3.md**. The history below is retained for provenance.

The (sweep-1 era) differential sweep against PDFium ranks the remaining
open/compile failures. By distinct-document cause, after encryption (#1,
138 docs):

- **`/LZWDecode` filter — done.** Native variable-width (9→12-bit, MSB-first)
  LZW in `pdf-structure/src/decode.rs` (`lzw_decode`), wired into the filter
  chain for both `LZWDecode` and the inline `LZW` abbreviation, with
  `/EarlyChange` (default 1) honored and the TIFF/PNG predictor applied via the
  existing `apply_parms`. Truncated streams salvage their decoded prefix.
  Verified: 5 unit tests (round trip across every width step + KwKwK + budget +
  truncation) and the PDFium oracle on 3 real Adobe LZW PDFs (mean ink_delta
  0.006–0.014, zero pages over the 0.05 threshold).
- **Truncated / corrupt flate streams — done.** `inflate_with`
  (`pdf-structure/src/decode.rs`) already returns the successfully-inflated
  prefix on a truncated or corrupt-tail stream (only a stream that yields *zero*
  bytes — truncated before any output — errors, which PDFium cannot render
  either). Now pinned by `truncated_flate_salvages_its_inflated_prefix`. The
  earlier "we return corrupt stream data and drop the page" characterization
  was inaccurate: salvage has been in place since Phase 1.
- **Indirect `/Filter` and `/DecodeParms` — done (2026-07-19, high-impact).**
  The `pdf-structure` decoder has no object repository, so an indirect
  `/DecodeParms N 0 R` (a *shared PNG-predictor dict* — a very common way to
  write it) or an indirect `/Filter` was silently unread: the predictor was
  skipped and the image decoded to its raw filtered deltas — a **near-black
  smear** across the whole image. `Resolver::resolve_filter_params`
  (`pdf-document/src/resolve.rs`) now resolves an indirect `/Filter`/
  `/DecodeParms` (and their array elements) before decoding, at both
  `decode_stream_data` and `…_to_codec`, no-op when both are already direct.
  Found via *parisut port* p0 (full-page DeviceRGB cover, `/DecodeParms 51 0 R`,
  Predictor 15): ours 0.997 → mean 214, **exactly matching PDFium**. Affects any
  Flate/LZW image with a shared predictor dict, so it likely clears far more than
  the one corpus page. Tests: `indirect_decode_parms_predictor_is_applied`,
  `indirect_filter_reference_is_resolved`
  (`pdf-document/tests/decode_indirect_parms.rs`).

The `compile failed` corpus class is **heterogeneous**, not mostly flate: a
resample of it found files that now open cleanly (encryption + LZW + salvage
already cover them) alongside a distinct **structural** failure, now fixed:

- **Zero-page tree after recovery — done.** A linearized/incremental file whose
  first revision is a stub `<</Type/Pages/Kids[]/Count 0>>` that a later
  revision overrides. When its trailing `startxref` is unfindable (the file
  tail is a binary stream), recovery landed on the stub xref and the document
  read as *zero pages*. `load_structure` now detects a degenerate page tree
  (`page_tree_looks_empty`) **on the recovery path only** and escalates to a
  full `XrefRebuilt`, whose last-occurrence-wins recovers the real page tree.
  Gated on `used_recovery` so a cleanly-loaded genuinely-empty PDF is never
  rebuilt. Verified: `recovered_stub_page_tree_triggers_rebuild` +
  the real file (Hoyos, *Hannibal's Dynasty*) went 0 → 739 pages, rendering
  legibly and matching PDFium.
  **Orphan `/Type/Page` recovery — done 2026-07-25 (post-move refinement).**
  If both the ordinary and rebuilt page-tree walks recover zero real pages,
  `pdf-document` scans the rebuilt xref's live entries (including object-stream
  members) for explicit `/Type /Page` dictionaries without `/Kids`, follows
  `/Parent` only for depth-bounded inheritable attributes, and adopts the
  recovered leaves in deterministic object-number order. The scan is gated
  behind the double-zero recovery path, capped by `max_pages`, and recorded as
  a recovery event. The previously documented gating hole is also closed:
  non-array `/Kids` increments `lost_subtrees`, so it triggers rebuild rather
  than masquerading as a genuinely empty tree. Tests:
  `orphan_page_is_recovered_after_rebuilt_tree_remains_empty` and
  `live_entries_skip_free_objects_and_preserve_generations`.

- **Partial malformed page trees — done 2026-07-27.** A readable catalog and
  some readable leaves no longer prevent escalation when another subtree is
  provably lost. `pdf-document` retries once with a full xref rebuild and
  adopts it only when the real-page/loss score improves. The object parser also
  repairs the observed malformed-reference form `N R` to generation zero and
  surfaces a typed recovery event. This restores all 152 real pages in the
  Cambourne report, whose last ten-page branch was previously rejected because
  object 923 ended `/Kids` with `335 R`. Tests:
  `partial_page_tree_loss_retries_with_rebuilt_xref` and
  `missing_reference_generation_in_page_kids_is_repaired`.

- **Rebuild indexes object-stream members** — *done*. `loader.rebuild()` now
  records each `/Type /ObjStm` container during the scan, decompresses it, and
  indexes its members as `InObjectStream` (revision order preserved: a member
  is overridden only by a *later* uncompressed definition). Without this a
  corrupted modern PDF — most objects live in object streams — lost nearly
  everything on rebuild. Verified synthetically and on a real modern PDF forced
  through `XrefRebuilt` (renders pixel-identically to the clean original).
- **Rebuild page-tree "739 vs 845" — resolved as file truncation, not a
  recovery gap.** Direct byte-level analysis of Hoyos *Hannibal's Dynasty*:
  one real catalog, one `/Pages` root declaring `/Count 845` over 5 kids, but
  the file's tail is physically truncated — the 4 kid references to objects
  5513/5729/5839/5945 point past everything present in the file (5405 distinct
  object headers, max well below), and all 4 gaps fall at leaf position 739,
  i.e. the **end** of the book. Our rebuild recovers every recoverable page in
  correct order; PDFium's "845" is merely the declared `/Count` (it cannot
  load those pages either — they'd be null/blank at the end). No fix needed.
  *Optional PDFium-parity decision for later*: report the declared `/Count`
  and synthesize blank placeholder pages for unresolvable subtrees, so the
  reported count matches PDFium byte-for-byte in diff tooling. Cosmetic when
  truncation is at the tail (the normal case for cut-off downloads).
  **Done 2026-07-21 (closeout pass):** when the walk provably loses
  subtrees (unreadable node, unreadable/non-array `/Kids`, or a kid that
  resolves to Null — the truncated-tail shape) *and* the root's declared
  `/Count` exceeds the recovered pages, blank letter placeholders are
  appended up to the declaration (bounded by `max_pages`), with a recovery
  note. A merely lying `/Count` with an intact tree synthesizes nothing.
  **Refined 2026-07-27:** when one lost child has an exact span derivable from
  its parent `/Count` minus readable sibling spans, placeholders are inserted
  at the lost position, preserving later page indices. Multiple unknown
  children whose spans cannot be assigned unambiguously still receive tail
  padding rather than invented ordering. Tests
  `truncated_page_tree_synthesizes_blank_placeholders_to_declared_count`,
  `exact_mid_tree_loss_inserts_placeholders_in_document_order`, and
  `ambiguous_multiple_tree_holes_keep_tail_padding_fallback`.

- **Content-stream decode failure no longer drops the page** — *done*.
  `append_content` skips a truncated/corrupt content stream (with an observable
  `note_recovery`) and renders what remains — blank if it was the only stream —
  matching viewers. Real books carry interspersed blank/truncated pages that
  used to error the whole page away.

### Full corpus result — 2026-07-18

The re-rank is complete. `../oracle-sweep-2026-07-18/pdfium-diff-out/results.csv`
is the durable artifact: it contains 65,990 sampled rows from all 11,206
inputs (65,969 completed comparisons plus 21 rows where our compiler failed;
PDFium could not open 8 inputs). The prior comparable sweep used the same
65,990 page keys, so the transition counts below are exact rather than an
estimate.

| class | 2026-07-17 baseline | 2026-07-18 current |
|---|---:|---:|
| failed (open/compile) | 1,571 | **21** |
| bad (`inkΔ > 0.05`) | 3,179 | 3,229 |
| raster-noise band (`0.01 < inkΔ <= 0.05`) | 16,259 | 27,949 |
| clean (`inkΔ <= 0.01`) | 44,981 | 34,791 |

This is a substantial compatibility gain, not a regression hidden by the
`suspect` counter. Of the 65,990 matched keys, 1,831 former failed/bad pages
became clean/noise; 331 former clean/noise pages crossed into failed/bad. All
21 current compiler failures were already failures in the baseline: there are
**no new hard failures**. The large move from clean to the 0.01–0.05 band is
expected after starting to paint Type 3, `/Mask`, and CIE content that was
previously omitted. `pdfium-diff`'s 0.01 `suspect` cutoff is an investigation
trigger; the compatibility bar remains 0.05.

The remaining work is deliberately ordered by corpus impact, with fixtures
named below so that each fix is gated by both a focused test and the oracle.

1. **JPX blank pages — root-caused and fixed in `jp2lam` (oracle rerun
   pending).** 610 pages in 451 documents had `ours_ink < 0.001` and
   `ref_ink > 0.3` — successful renders of an empty surface, because the JPX
   decode error was swallowed below the compiler. The plan's hypothesis
   (multiple tile-parts) was only one of **four** distinct `jp2lam` gaps found
   by extracting real corpus streams and diffing against OpenJPEG
   (`opj_decompress`); all four are now fixed and each was verified bit-exact
   (max abs diff ≤ 1, the 9/7 float-rounding tolerance) against OpenJPEG:
   - **`ftyp` brand over-strict** (dominant cause). Real archive.org/Kakadu
     scans set the *major* brand to `jpx ` with `jp2 ` in the compatibility
     list; `jp2_parse.rs` required the major brand to equal `jp2 `. Fixed per
     ISO/IEC 15444-1 §I.5.2 (conformance is decided by the compatibility list).
   - **QCC per-component quantization** (marker 0xff5d) was rejected as
     unsupported. Implemented main-header QCC parsing + `CodestreamHeader::quant_for`,
     threaded through Tier-1 and reconstruction (`decode_markers.rs`, `t1.rs`,
     `reconstruct.rs`).
   - **Non-LRCP progression** (RLCP/RPCL/PCRL/CPRL) was rejected. Implemented
     as a permuted packet odometer (`packet_axis_order` in `t2.rs`); valid
     because precincts are rejected, so P=1 collapses each order to a
     permutation of (layer, resolution, component).
   - **TNsot over-strict / multi-tile-part.** Real files emit one more
     tile-part than `TNsot` declares (e.g. 6 parts, TNsot=5) and use tiled
     images (e.g. 2×2) with interleaved tile-parts. `tile_part_indices_by_tile`
     now treats TNsot as advisory while keeping the load-bearing TPsot ordering
     invariant; the interleaved multi-tile assembly + stitch path was already
     present and is confirmed correct.

   A deeper census over the full 424-file blank corpus (extract every JPX
   stream, decode with `jp2lam`, tally normalized error signatures) surfaced
   and fixed **four more** gaps, each verified against OpenJPEG:
   - **Raw J2K codestreams** (no JP2 container) — `/JPXDecode` may carry a bare
     codestream starting with SOC (`0xFF4F`); `jp2_parse` read the SOC/SIZ
     markers as a box length and failed "box extends past input". `parse_jp2_core`
     now detects SOC and decodes the codestream directly, synthesizing the
     container header from SIZ (color space by component count). Bit-exact
     (max diff **0**) vs OpenJPEG.
   - **CMYK JPX** (EnumCS 12, 4-component). `ColorSpace::Cmyk` added; the
     4-component layout is accepted; `reconstruct_cmyk_image` reconstructs four
     planes and applies the inverse color transform to the first three when MCT
     is signalled (K passes through), matching OpenJPEG's `opj_mct_decode`.
     pdf-image already mapped 4→`Cmyk8`. Verified vs OpenJPEG (mean 0.4/255).
   - **Decomposition-level bound too strict** — `max_dwt_decompositions` used
     `floor(log2(min_dim))`, rejecting valid non-power-of-two streams (e.g. an
     18-wide tile with 5 levels). Relaxed to `ceil(log2(min_dim))`; verified
     vs OpenJPEG.
   - **PLT/PLM/TLM length markers** were rejected; they are inert random-access
     hints that sequential decode ignores (as OpenJPEG does). Now accepted and
     skipped in both main and tile-part headers.
   - **`jpx `-only compatibility brand.** Some JPX-branded scans list only
     `jpx ` (no `jp2 `) in `ftyp` yet carry a standard `jp2h`+`jp2c` structure
     that OpenJPEG decodes; the brand check now accepts a `jpx ` compat brand
     too (verified max diff 2 vs OpenJPEG). Genuinely non-conformant JPX
     constructs still fail loudly downstream.
   Across the full 424-file census these fixes take the blank corpus to
   **791/822 JPX streams decoding (96.2%)**; the box-extends (raw-J2K) and
   EnumCS (CMYK) classes are now entirely gone. **Update 2026-07-19:** a
   subsequent jp2lam round (precincts/SOP/EPH, all five progression orders,
   degenerate-DWT one-sample fix, multi-segment Tier-1) lands the precinct and
   degenerate-DWT classes; a fresh census over all 451 blank docs now decodes
   **1207/1224 JPX streams (98.6%)**. The 17 residuals are the deliberate
   truncation strict-failures (Stalin/Prussian) plus four small new items
   (tile-part QCC ×2, an odd truncated box ×1, a packet-header truncation ×1) —
   all B3-visible, all relayed in `jp2lam/HANDOFF-remaining-jpx-decode.md` §7.
   Verification: across a 25-file blank-corpus sample, 118 JPX streams now
   decode with 1 residual error (`non-empty code-block contribution has zero
   byte length`, a rare T2 edge — one stream); a full 68-stream file batches
   bit-exact to OpenJPEG; the previously-blank *Zen-essence* p0 (was inkΔ 1.0,
   ours 0.0) now renders a real dark image (ink 0.90). New `jp2lam` tests:
   `file_type_accepts_jpx_major_brand_with_jp2_compat`,
   `file_type_rejects_when_jp2_compat_brand_absent`,
   `packet_axis_order_matches_annex_b12_under_single_precinct`,
   `tnsot_is_advisory_extra_tile_part_is_accepted`; full suite green incl. the
   441-stream OpenJPEG baseline. *Still deferred* — the residual 31/822 (3.8%),
   each a small count in the full 424-file census, ranked by frequency:
   **precincts/SOP/EPH** (~10 files; the largest remaining class; needs a
   precinct-aware T2 packet iterator); **degenerate 1-sample DWT split** (~8; a
   power-of-two axis signalling one extra level, e.g. 176×16 with 5 levels —
   `jp2lam`'s inverse DWT drifts ~16/255 vs OpenJPEG on the 1-sample axis, so it
   is *rejected* rather than mis-decoded and shows as a B3-flagged failure, not a
   silent blank; needs a DWT fix); **`TPsot==TNsot` non-conformant tile-parts**
   (~2; OpenJPEG decodes with a warning, but the tail is a genuine tile-part
   chunk our assembly misplaces — dropping it corrupts a 256-row code-block band
   [measured], so the strict `data after packet sequence` failure is kept
   deliberately; the fix is correct non-conformant tile-part boundary handling,
   not tail tolerance); **zero-length code-block contribution** (~6;
   truncated-stream salvage); and `pclr`-without-recent-support/`bpcc`/
   subsampling/COC/POC. See the JPX list under "Image codecs" below.
   **B3 no-silent-blank invariant — done.** The former swallow sites
   (`pdf-render-cpu/src/prepared.rs`, `.ok()?` on `codec.decode` and the
   codec-not-registered `?`) now record a degradation on the prepared page via
   an interior-mutable `RenderDiagnostics` sink (the decode path is ctx-less and
   reads the registry immutably, so interior mutability avoids a reborrow).
   `RenderStats` gains `degraded_draws: u32` + a bounded `recovery_notes`, folded
   in for both the top-level page and every tiling-pattern cell, plus
   `is_silent_blank()` (`degraded_draws > 0 && covered_pixels == 0`). `pdfr
   render` prints a `SILENT BLANK`/`degraded` warning; `tools/pdfium-diff` gains
   a `degraded` CSV column, a `silent-blank(codec)`/`degraded(codec)` note, and
   `silent-blanks`/`degraded` summary counters — so a page we blanked from an
   undecodable image can never again be scored clean. Tests:
   `undecodable_codec_image_is_a_recorded_silent_blank`,
   `decodable_content_is_not_flagged_degraded`
   (`pdf-render-cpu/tests/render_image.rs`).
2. **Four-component DCT image polarity and color — implemented, unit-tested
   (oracle rerun pending).** Root cause: a **double inversion**. Adobe
   CMYK/YCCK JPEGs carry `/Decode [1 0 1 0 1 0 1 0]`, and PDFium's pipeline is
   *raw libjpeg CMYK → `/Decode` remap → frozen DeviceCMYK table* (verified in
   `pdfium-reference-source`: `cpdf_dib.cpp:1186-1234`, `cpdf_devicecs.cpp:131-139`,
   `jpegmodule.cpp` does no CMYK un-inversion; the standalone
   `progressive_decoder.cpp:1073-1074` inverts explicitly *because* it has no
   `/Decode`). Our JPEG decoder was *also* un-inverting Adobe samples, so with
   `/Decode` present the pixel inverted twice (Nomad Flute white → near-black,
   99.9% ink). Fix in `pdf-image/src/jpeg/mod.rs`: the 4-component `assemble()`
   branch now emits **raw libjpeg output** (`cmyk_libjpeg_pixel`) with no Adobe
   un-inversion — YCCK → `[255-R,255-G,255-B,K]`, direct CMYK passthrough — and
   YCCK selection follows libjpeg (`four_comp_is_ycck`: Adobe marker with
   non-zero transform). The frozen `pdf-color` CMYK table is untouched (the
   mismatch was proven to be *before* it). Tests: four synthetic-plane cases
   (YCCK/direct × Adobe/no-Adobe) asserting raw codec bytes and post-`/Decode`
   DeviceCMYK, plus updated `jpeg_fixtures.rs`. **Regression fixture landed
   (2026-07-19):** the real Nomad Flute DeviceCMYK YCCK JPEG (object 7) is
   pinned at `pdf-image/tests/fixtures/nomad_flute_p0.jpg` with a Pillow/libjpeg
   `.truth.bin`; `nomad_flute_devicecmyk_ycck` asserts `255 - our_raw` matches
   the un-inverted true CMYK (mean 0.39) — the pre-fix double inversion diverged
   by ~250. (Extracted with the new `pdf-cli` example `codecdump`.) End-to-end
   oracle already confirmed on Windows: Nomad Flute p0 went 99.9% → **2.07% ink,
   matching PDFium 2.04%** (inkΔ 0.0003), all 6 sampled pages clean.
   **`destroyed`-class rerun verdict (2026-07-19):** the YCCK-CMYK fix cleared
   its subset; a first rerun showed 135/198 over-ink rows cleared with **60
   residual = a distinct Separation-image class** (now also fixed, see Phase 3
   below). A **fresh rerun after the Separation fix: 176/198 cleared, only 19
   remain** (`ours`≈1.0, whole-page black) — a heterogeneous tail of two
   sub-causes (**2-component `[/ICCBased n=2]` DCT** and **dark `DeviceGray` raw
   images used as masks/inverted**) left for a later session; see
   `TRIAGE-RESIDUAL.md`. This fix (YCCK-CMYK) is **DONE and oracle-verified for
   its class**.
3. **Finish failure-tolerant content parsing — DONE, oracle-verified
   2026-07-19.** A `pdfium-diff --rerun-failures <prior.csv> failed` run on
   Windows (project `pdfium.dll`) re-graded all 17 formerly-failing documents:
   **0 unopenable, 0 compile-failed, 0 silent-blank/degraded**. All 21 former
   `compile failed` pages now render, 20 matching PDFium cleanly (inkΔ < 0.01 —
   statements 0.0002–0.003, Magic Lotus Lantern 0.006–0.008, Trotsky p27 0.003,
   document2 p4 0.0002, Byzantine ≤0.0001); the lone outlier is Cambourne p25 at
   inkΔ 0.022 (in the noise band, well under the 0.05 bar — the `l`-with-no-
   operand recovery paints slightly more than PDFium, not a drop). Gate
   `compile failed = 0` met. The 21 `compile failed` rows were three
   causes: 10 malformed inline images missing `EI` (seven 2021 statements, two
   2024 statements, `document2.pdf`); 9 zero-output/corrupt Flate content
   streams (five documents, incl. five *Magic Lotus Lantern* pages); two
   malformed operator/token cases (*Trotsky* p27 stray `)`, *Cambourne* p25 `l`
   with no operand). Fix in `pdf-content`:
   - **`run()` is now recovery-driven** (`interpret.rs`): a `Malformed`/`Syntax`
     error from `next_lexeme()` or `dispatch()` is caught → `note_recovery` +
     resync (skip one byte if the lexer refused to advance, else continue),
     operand stack cleared. Only DoS guards abort — `OperatorBudget`,
     `RecursionDepth`, and the newly-split `OperandStackOverflow`/`NestingDepth`
     variants (moved off `Malformed` so classification survives; `is_fatal()`
     in `lib.rs`). `compile_semantic` now yields a partial page, never a page
     drop, for content malformation. Covers the 2 operator cases and (via
     garbage-tokenizing-into-recovery) the 9 Flate cases; `append_content` was
     already `Ok`-only, confirmed by audit.
   - **Length/filter-aware inline framing** (`tokenizer.rs` `read_inline_image`):
     trust `/L`; else unfiltered → exact `ceil(W·BPC·ncomp/8)·H`; else
     DCT-aware scan (anchor on JPEG `FF D9`). On give-up, advance the cursor and
     return the recoverable `ContentError::InlineImage`, which `run()` maps to a
     note + skip (the tokenizer has no `ctx`, so recovery is surfaced via the
     variant, not threaded). Covers the 10 false-`EI` DCT inline images.
   Tests added: `stray_close_paren_token_recovers`,
   `keyword_where_operand_expected_recovers`, `recovery_preserves_following_content`,
   `zero_output_flate_content_yields_blank_page_with_note`,
   `inline_image_without_ei_recovers_at_page_level` (adversarial_tests.rs);
   `inline_image_length_frames_past_false_ei`,
   `inline_image_filtered_dct_frames_on_eoi`,
   `inline_image_missing_ei_is_recoverable` (tokenizer.rs). This graduates the
   Phase 2 "Inline-image `EI` framing — approx" item below.
4. **Re-rank the residual missing-ink pages after JPX.** Excluding the blank
   JPX class, only 111 pages in 85 documents remain above 0.3 while lighter
   than PDFium. The top examples are still JPX/MRC pages, with a smaller
   ICCBased-image tail. Re-run the single-document oracle fixtures after item
   1 before treating that tail as a separate image or ICC implementation
   project.
5. **Only then investigate lower-severity drift.** 737 non-blank pages lie
   just above 0.05 and should not drive feature work yet. The known Type 3
   stencil/image-edge AA heaviness belongs in the raster-quality pass unless
   it remains above 0.05 after the two image blockers are fixed. The remaining
   2,619 non-blank bad rows are the triage pool; use on-demand triptychs for
   the worst distinct documents, not the 44,425 rows above the deliberately
   sensitive 0.01 `suspect` cutoff.

## Performance snapshot (pdfium-diff `bench`, this box: 20 workers)

First head-to-head timings vs PDFium (add `bench` subcommand to the oracle):
- **Per-page (single-threaded): PDFium is ~4–6× faster.** MRC scan (Etruscan,
  image-codec-bound) 142 ms vs 34 ms; text (Hannibal @2×) 39 ms vs 6.6 ms. The
  per-page gap — dominated by image-codec decode and the scalar coverage/raster
  kernels — is the standing optimization target (advice §3/§7, the codec SIMD
  seams).
- **Whole-document throughput: ours is 1.65–2.34× faster.** The parallel
  `RenderScheduler` (7 compile + 13 render workers) beats PDFium's
  single-threaded sequential rendering despite the per-page deficit — exactly
  the throughput-over-latency trade the concurrency design was for. Closing the
  per-page gap compounds directly with this.

## Phase 2 — content interpreter & semantic page
- **Inline-image `EI` framing** — *done, unit-tested*. Now
  length/filter-aware: trusts `/L`, else exact length from
  `W`/`H`/`BPC`/`CS` for unfiltered data, else a DCT-aware (`FF D9`-anchored)
  scan; malformed framing recovers (drops the image, keeps the page) instead of
  aborting. ASCII85/ASCIIHex use their intrinsic EOD markers, and **RunLength
  joined them 2026-07-26**: the framer walks packet headers to the structural
  byte-128 EOD, so literal `EI` and `0x80` sample bytes cannot truncate the
  image. *Deferred tail*: a Flate, CCITT, or JBIG2 inline image without `/L`
  whose encoded payload contains a false `EI` can still mis-frame the *image*
  (never page-fatal). Safely closing those requires consumed-input reporting
  from the corresponding decoder rather than another byte-pattern heuristic.
- **Shadings (`sh` operator)** — *done* (Phase 6A). Axial (type 2) and radial
  (type 3) shadings resolve and rasterize; the `sh` operator and PatternType 2
  shading patterns are both wired. *Deferred*: shading types 1 and 4–7 (carried
  as a `/Background`-only hook) — **done 2026-07-21 (production-readiness
  pass)**: all mesh types 1/4–7 now parse to the IR (`pdf-content/src/mesh.rs`)
  *and* rasterize in the CPU backend; the `/Background`-only hook wording is
  obsolete (mesh pdfium-diff fixtures are not yet in the diff corpus). Still
  deferred: `/Extend` on function domains other than
  `[0 1]` beyond the sampled ramp resolution (256 entries).
  **Re-baselined 2026-07-21 — verified correct, entry retired.** The ramp
  is sampled across the shading's `/Domain` (not `[0 1]`), so its endpoints
  *are* the `t0`/`t1` boundary colors §8.7.4.5.2 requires an extended
  region to paint; `apply_extend`/`radial_param` clamp to those endpoints.
  Pinned by `non_unit_domain_ramp_endpoints_are_the_boundary_colors`
  (shading_tests.rs) plus the existing render-level extend tests. The only
  residual is the generic 256-entry interior quantization, which is a
  resolution choice, not an `/Extend` gap.
- **PDF functions** — *core complete* (`pdf-function`). Types 0 (sampled,
  now including **multi-input** via `SampledN` with multilinear interpolation),
  2 (exponential), 3 (stitching), and **4 (PostScript calculator)** — full
  Table 42 operator set, bounded execution (10k ops / stack 100 / nesting
  100), range-clamped-zeros on any type error, parse failure → Identity.
  `eval_n(&[f32])` dispatches every variant; the 1-input shading path is
  byte-for-byte untouched. **Wired**: `build_function` covers Types 0 (both
  arities), 2, 3, 4; `eval_tint` feeds all colorants through `eval_n`, so
  multi-input DeviceN lands in its real alternate space (the grey fallback
  survives only as the arity-mismatch tolerance); shadings pick up Type 4
  through the pre-sampled ramp automatically.
- **Marked content / compatibility (`BDC`/`BMC`/`EMC`/`BX`/`EX`)** — ignored
  by design (no geometry).

## Phase 3 — stable compiled-page IR
- **Non-device color** — *Lab/CalRGB/CalGray done; ICC still approx*.
  `pdf-color` carries exact conversions ported from PDFium's
  `cpdf_colorspace.cpp` (Lab→XYZ→sRGB with PDFium's piecewise constants and
  verbatim gamma tables; CalRGB's full gamma → /Matrix → whitepoint-adapted
  XYZ→sRGB — the subagent caught that the reference source contradicts the
  pass-through folklore; CalGray pass-through, which IS what PDFium does),
  and `set_color` routes `sc/scn` operands through them unclamped via a
  cached CIE-space resolve. *Still deferred*: ICCBased stays an arity
  approximation (profile parsing is an explicit future policy choice), and
  **Lab-colored images** (as opposed to fills/strokes) still map by arity —
  converting image pixels needs per-sample `lab_to_rgb` at decode time.
  **Done 2026-07-21:** `convert_special_image_samples` converts 8-bit Lab
  image samples to RGB8 at compile time through the same
  `pdf_color::lab_to_rgb` the fill path uses (`/Decode` defaulting to
  `[0 100]`/`/Range` per §8.9.5.2). Non-8-bpc Lab images and Lab bases
  inside `/Indexed` palettes keep the arity map. Test
  `lab_image_converts_per_sample_not_by_arity`.
  **ICCBased image update 2026-07-26:** RGB matrix/TRC profiles and CMYK
  4-input Lab-PCS `mft1`/`mft2` profiles now survive codec-backed image
  lowering. CMYK lookup tables are cached per profile, copied once into
  backend-neutral `IccCmykTransform` IR, and evaluated by the CPU sampler after
  JPEG/JPX decode. Direct 8-bit samples retain their compile-time conversion.
  Other ICC profile shapes and non-image paints remain under the documented
  arity/device fallback; a general CMM is still deferred. Tests:
  `codec_cmyk_image_carries_icc_lookup_tables_into_ir` and
  `codec_cmyk_uses_the_declared_icc_transform`.
- **Separation / DeviceN** — *done for the single-colorant case*. The tint
  transform is evaluated through `pdf-function`, so `[/Separation /Black ...]`
  and spot colours land in their alternate space correctly, and the initial
  colour is full colorant per §8.6.8. This was a *correctness* bug, not an
  approximation: arity resolution inverted these spaces (tint 1.0 → white),
  making entire pages of real documents render blank. *Deferred*: multi-input
  DeviceN (needs multi-input Type 0 or Type 4 functions) falls back to a
  subtractive `1 − max(tint)` grey — hue is lost but polarity is right; the
  `/None` colorant paints white rather than suppressing marks; `/All` is not
  special-cased. **Update 2026-07-21:** `/All` is now special-cased per
  §8.6.6.4 — it bypasses the tint transform and paints neutral ink
  (`DeviceGray(1 − tint)`, tint 1.0 = black) for both fills and image LUTs;
  note PDFium has *no* `/All` case (it runs the transform), so this follows
  the spec's registration-mark intent deliberately. **2026-07-21 (DS82 root
  cause):** a Separation/DeviceN whose *alternate* space is CIE-based
  (Lab/CalRGB/CalGray) now routes the tint transform's outputs through that
  space's conversion (`TintSpace.alt_cie` → `eval_cie`) instead of clamping
  them into [0,1] as device components — a PANTONE spot with a Lab
  alternate (L* 0..100, a*/b* signed) was rendering pale lavender as pure
  yellow (the DS82/PhRMA H-class "boldness" pages were actually this).
  Matches PDFium `CPDF_SeparationCS::GetRGB` → `base_cs_->GetRGB(results)`.
  Test `separation_with_lab_alternate_converts_through_lab`; oracle
  re-check queued in POST-SWEEP-VERIFY.md §1. `/None` painting white
  is now **verified against PDFium** — `CPDF_SeparationCS::GetRGB` returns
  nullopt and the color state falls back to white
  (`cpdf_colorstate.cpp:130`), so white *is* oracle behavior. Tests:
  `all_colorant_paints_neutral_ink_bypassing_the_transform`,
  `all_colorant_image_ramp_is_the_neutral_ramp`,
  `none_colorant_fill_paints_white_like_pdfium` (separation_tests.rs). **Single-input Separation/DeviceN _images_ — DONE
  (2026-07-19).** A 1-component image in a `[/Separation …]` (or 1-colorant
  `[/DeviceN …]`) space was sampled as DeviceGray, so a low-tint (near-white)
  scan rendered near-black — the dominant residual over-ink class in the
  `destroyed` rerun (~60 pages). Fix: `build_tint_image_lut` bakes the tint
  transform into a 256-entry sample→sRGB `ImageColorSpace::TintLut` at compile
  time; the CPU sampler indexes it by the `/Decode`-normalized sample. Because a
  `/Separation` image is usually a 1-component JPEG, `lower_image` also had to
  stop letting the codec's Gray format override a reinterpreting PDF colorspace
  (Indexed/TintLut now win when the codec decodes to 1 channel). Verified: *One
  Zambia* p0 0.97 → **0.0067** ink (PDFium 0.013); Pat Caplan p47, Finance
  Capital p0, Kalevi Keskinnen p0 all >0.5 → ~0.01–0.03. Test:
  `tint_lut_separation_image_routes_samples_through_lut` (render-cpu). *Still
  deferred*: multi-input DeviceN images (no 1-D LUT) keep the arity
  approximation. **Done 2026-07-21:** multi-input DeviceN *images* (8-bit,
  n ≥ 2 colorants, buildable transform) now convert to RGB8 at compile
  time — each texel's tints run through `eval_tint`/`eval_n`, memoized on
  the raw sample tuple. Short/oversized buffers and non-8-bpc data keep
  the arity fallback; images under a color-key `/Mask` are excluded (the
  key ranges address raw samples). Tests
  `multi_colorant_devicen_image_evaluates_the_tint_transform_per_sample`,
  `…_short_data_keeps_the_arity_fallback`. The device
  **CMYK→RGB policy is frozen** and now matches PDFium bit-for-bit: Adobe's
  measured 9×9×9×9 table, interpolated in fixed point (`pdf_color::cmyk`,
  ported verbatim from PDFium's `cfx_cmyk_to_srgb.cpp`; contract §10). The
  previous naive formula was *wrong*, not approximate — the differential
  oracle caught it rendering a cover's blue as `rgb(0,143,255)`. Verified
  225/225 CMYK swatches identical to PDFium. Indexed is exact for images
  (Phase 6C); **as of 2026-07-19 the Indexed sampler also honors a non-default
  `/Decode` array** (it remaps sample→palette-index per §8.9.5.2 — e.g. `[0 255]`
  on a 1-bit image sends sample 1 to index 255, not 1 — previously the raw
  sample was used directly; test `indexed_decode_array_remaps_sample_to_index`).
  An ICC/device-link path is a
  future, explicitly-selected policy — never a silent replacement.
- **`ICC_COLOR` / `OVERPRINT` feature flags** — *todo*. Not computed.
  **Done 2026-07-21:** both are computed as flags. The interpreter records
  `/ICCBased` sightings (cs/CS named resources via `note_named_space_family`
  and image color spaces via `colorspace_from_array`) and ExtGState
  `/OP`/`/op` true; `compute_features` surfaces them as
  `PageFeatures::ICC_COLOR` / `OVERPRINT`. Flags only, per the plan —
  rendering keeps the ICC arity approximation and overprint does not
  change compositing (no CMM). Test
  `icc_and_overprint_feature_flags_are_computed` (lower_tests.rs).
- **Patterns** — *partial*. Shading patterns (PatternType 2) resolve to
  `Paint::Shading` and render (Phase 6A). Tiling patterns (PatternType 1) are
  compiled into `CompiledPage::tilings` (Phase 6A IR) and rendered by the CPU
  backend in Phase 6B.
- **Shadings in the IR** — *done* (Phase 6A). `ShadingResource` carries an
  axial/radial `ShadingKind` with a pre-sampled color ramp. Soft-mask
  resources still use inline ops (Phase 4I); the `MaskResource` table stays a
  *hook*.

## Phase 4 — CPU raster backend
- **4G Images** — *done* (Phase 6C) for the non-codec path. `ImageIr` carries
  filter-decoded packed samples + color space + `/Decode`; the CPU backend
  samples per device pixel (nearest + bilinear), converts Device Gray/RGB/CMYK
  and Indexed, paints `/ImageMask` stencils with the fill color, and applies a
  grayscale `/SMask`. Flate/RunLength/raw (+ predictor) decode via the existing
  stream-filter layer; DCT decodes natively via the codec registry (see
  "Image codecs"). **`/Mask` is now done** (ISO 32000-1 §8.9.6.3–4): color-key
  (raw-sample ranges pre-`/Decode`, all-components rule, malformed → dropped)
  and stencil streams (own geometry, sample-1-hides polarity — the reverse of
  SMask luminosity — `/Decode [1 0]` honored), `/SMask` precedence, and
  codec-encoded stencils through the registry via the `ImageSMask` reuse.
  *Deferred*: no CPU resource cache (samples re-sampled per draw, not
  memoized); image-edge AA is nearest (no partial-coverage antialiasing at
  the image boundary — also the cause of Type 3 bitmap glyphs rendering
  heavier than PDFium). **Update 2026-07-21 (production-readiness pass):
  both closed** — the production `SharedImageCache` (96 MiB, 8-shard LRU,
  content-hash keyed, `PDF_RENDERER_IMAGECACHE` env override) memoizes
  decoded images, and image-edge partial-coverage AA landed, including
  rotated placements. Still open (workstream H second half): minification
  weight quality (area-average tails) — the stencil-boldness residue on the
  DS82/PhRMA/DLIFLC pages remains until it lands. **Done 2026-07-21
  (workstream H second half):** fractional-tap area weighting landed — see
  the "Image minification" entry below for details and oracle numbers.
- **Image minification** — *done*. A device pixel covering more than one
  source texel area-averages its footprint, matching PDFium's `CStretchEngine`
  (whose downscale path weights source pixels by overlap area). Point sampling
  a 300dpi scan to screen kept one texel in nine and broke strokes: measured
  ink 0.0796 against PDFium's 0.1338 on the same page, now 0.1392 vs 0.1338
  (inkΔ 0.0542 → 0.0054). Applies regardless of `/Interpolate`, which selects
  the *magnification* filter, not whether to discard detail. The loop is
  scalar — `fast_image_resize` (SIMD, axis-aligned only) or Lege's WGSL kernels
  are the optimization path if profiling calls for it, ideally behind
  PDFium's stretch-then-transform split.
  **Done 2026-07-21 (workstream H second half — fractional-tap weights):**
  the box filter now weights every tap by its overlap with the footprint
  (`axis_box_taps`, fixed-point 1/4096 weights) across all minification
  paths: the generic/rotated `area_average`, the RGB8/CMYK opaque fast
  paths, and the bilevel summed-area path (whose `weighted_ones` decomposes
  the fractional box into SAT strip/corner reads, still O(1) per pixel —
  pinned equal to the direct weighted popcount by
  `sat_weighted_ones_matches_direct_binary_average`). Oracle (pdfium.dll,
  scale 2.0): the pure 1-bit minification fixture `ccitt4-cib-test.pdf`
  went inkΔ 0.00255 → **0.00014** (ours-ink 0.0302 → 0.0275 vs PDFium's
  0.0276 — the boldness is gone); Etruscan JPX gross improved on all six
  sampled pages; JPEG-scan/MRC/latin-text spot checks unchanged (worst
  movement +0.0013 on one cover, deep in the noise band). The named H
  fixtures moved only marginally because their deltas turned out to be
  dominated by *other* causes, now on the ledger: DS82 pp4–5 render form
  widgets with a different highlight color than PDFium (ours yellow vs
  PDFium's lavender — an annotation/appearance color question, not
  raster quality), and DLIFLC body text renders as notdef boxes (a font
  substitution/embedding gap), which is what its excess ink actually is.
  **Done 2026-07-26 (MRC soft-mask follow-up):** the earlier area-filtering
  pass covered base images and image masks but left grayscale `/SMask`
  sampling at one nearest source texel. High-resolution MRC scans therefore
  filtered their JPX foreground color while reducing the attached 1-bit JBIG2
  text mask to an arbitrary binary decision. Soft masks now derive their own
  texel footprint from the draw inverse and dimensions, use the same
  fractional box taps, and average packed 1-bit coverage with weighted
  popcounts before applying `/Decode`. On
  `volkstumlichege03grae.pdf` p605, PDFium ink/gross deltas moved
  0.09091/0.05401 → **0.00607/0.00000**; on
  `sexcharacter00wein.pdf` p65 they moved 0.07097/0.05664 →
  **0.00552/0.00000**. All 24 rows from the two six-page, two-oracle focused
  sweeps now classify `ok`. The old note that MRC spot checks were unchanged
  referred to base-image fractional taps, not this previously unfiltered
  soft-mask path.
  **Done 2026-07-26 (DCT magnification follow-up):** the remaining Goebbels
  scan family was not a JPEG decode or colour-conversion defect. Page 97 is a
  503×801 full-page DCT image mapped exactly to a 503×801-point page and
  rendered at 2×. Our native decoded samples agreed with ImageMagick/libjpeg
  to 0.022 RGB levels MAE, but `/Interpolate` absent/false made the CPU backend
  replicate every sample into a hard 2×2 block. PDFium and MuPDF instead
  produced byte-identical bilinear magnification. Decoded DCT images now use
  that continuous-tone magnification policy while raw/stencil images retain
  their explicit nearest behavior; the bilinear channel conversion also
  truncates like the references' fixed-point stretchers instead of adding a
  one-level round-to-nearest bright bias. On page 97, PDFium RGB MAE/gross/
  inkΔ/continuous-inkΔ moved
  **8.89149/0.02407/0.08838/0.00254 →
  0.22878/0.00000/0.00982/0.00085**. Five sampled pages
  (97/194/291/388/485) now have zero gross pixels against both PDFium and
  MuPDF, RGB MAE 0.222–0.234, and continuous-ink delta below 0.0009; their
  remaining threshold-only ink delta is 0.0051–0.0103.
  **Done 2026-07-26 (rotated/sheared footprint):** affine minification now maps
  each device-pixel square through the inverse image matrix and clips the
  resulting source-space parallelogram against candidate texel squares. The
  exact overlap areas become fixed-point sample/coverage weights for RGB,
  low-bit-depth/LUT, and stencil images. Axis-aligned draws and 90° quarter
  turns retain the separable fractional box path, so the common scan path and
  its SIMD/SAT fast paths remain byte-identical. A synthetic slanted-strip
  guard distinguishes the exact `.5/1/.5` coverage from the old bounding
  box's `1/1/1`, for both RGB colour and stencil alpha.

  Oracle control: PDF.js
  `image-rotated-black-white-ratio.pdf` at scale 0.5 (a 379×378 bilevel image
  under a ~30° affine matrix) moves only 0.084% of page pixels. PDFium
  gross/inkΔ improve **0.000635/0.000264 → 0.000586/0.000206** with continuous
  ink unchanged; MuPDF's already microscopic MAE moves 0.0350 → 0.0491,
  showing that the two references use slightly different
  stretch-then-transform approximations rather than exact footprint
  integration. The exact geometry is therefore the correctness invariant, not
  a claim of universal byte-parity improvement. A full-page axis-aligned
  Goebbels scan remains byte-identical (matching SHA-256 before/after).
  **Performance follow-up:** each affine device-pixel footprint now prepares
  its bounds and inward half-plane equations once, rejects disjoint texels,
  and accepts fully covered texels without polygon clipping. Only boundary
  texels take the full four-plane clipping path. On the PDF.js control above,
  the median prepared-page `render.image` time across 20 release runs fell
  **10.528 ms → 8.357 ms (20.6%)**, with the output hash unchanged at
  `601b6a8537a3e8ba`. A direct geometry guard compares all fast
  classifications against full polygon clipping for clockwise,
  counter-clockwise, contained, boundary-crossing, and disjoint footprints.
  Still deferred: the SIMD/stretch-split optimization seams above.
- **4J Adaptive band/tile planning** — *todo*. Only DirectPage execution is
  implemented (advice §2: defer bands/tiles until direct-page is competitive).
- **Patterns & shadings (feature order 10)** — *done* (Phase 6A/6B). Axial +
  radial shadings and both pattern types render. Tiling replicates the compiled
  cell across the fill shape via a bounded offscreen. *Deferred*: per-tile
  lowering is re-done per instance (a lower-once-then-translate tiler is the
  noted optimization); tile count is capped at 2^14 (adversarial tiny-step
  fills are left unpainted beyond the cap); tiling under a non-Normal blend or
  soft mask uses source-over compositing. **Update 2026-07-21
  (production-readiness pass):** render-side per-tile hoists landed; the full
  lower-once-then-translate tiler was evaluated and **rejected** (output is
  not byte-identical — documented at the loop). Non-Normal blends are now
  generalized to images, shadings, and tilings via `composite_px_blended`
  (Normal keeps its fast paths), retiring the source-over shortcut.
  **Tiling-pattern text fills done 2026-07-26:** glyph outlines (or fallback
  boxes when no program exists) now become the existing tiler's fill mask
  instead of being skipped. `pdfbox/2906.pdf` p2 restored all patterned text
  and moved from suspect to `ok` against PDFium and MuPDF; the focused
  `tiling_pattern_paints_text_fill` test pins the path. Tiling-pattern
  *strokes* remain unsupported, as does the 2^14 tile-cap behavior.
- **Complex color (feature order 11)** — *approx* (see Phase 3 non-device color).
- **Non-isolated transparency groups** — *approx*. All groups render as
  isolated (transparent backdrop); backdrop import/removal not implemented.
  **Correction 2026-07-20:** backdrop *import* landed post-sweep-3
  (non-isolated groups seed from the parent backdrop, PDFium
  ProcessTransparency Stage 1); backdrop *removal* (§11.4.8) remains the
  documented approximation.
- **Knockout groups** — *todo*. The `knockout` flag is carried but ignored.
  **Done 2026-07-21 (production-readiness pass):** knockout groups are
  implemented in the CPU backend.
- **Soft-mask `/BC` backdrop and `/TR` transfer function** — *todo*. Ignored.
  **Done 2026-07-21 (production-readiness pass):** `/BC` luminosity backdrops
  render via `MaskKind::LuminosityBc`; `/TR` is applied as a `TransferLut`
  carried on `BeginSoftMask`.
- **Stroking pen** — *approx*. Device-space scalar half-width (exact for
  uniform scale; anisotropic/rotated pens approximated). A user-space stroker
  is future work.
- **Curve/outline flattening** — *approx*. Fixed subdivision counts
  (`CURVE_SEGMENTS`, `OUTLINE_SEGMENTS`); an adaptive device-error flattener is
  a measurable optimization.
- **Coverage kernel** — the analytic-coverage baseline is correct; a fixed-point
  active-edge-table variant and SIMD `KernelSet` are documented optimization
  seams (advice §3/§7).
- **§17 performance gates** — *partial*. A dep-free `cargo bench` harness exists;
  p50/p90/p99 corpus reporting and the separate per-page vs whole-document
  scoreboards are not built.

## Fonts (fonts.md phases)
- **Base-14 / non-embedded fonts (Font Phase 3)** — *done*. All 14 standard
  faces are bundled (PDFium's Foxit CFF, converted once to OTF; provenance and
  licence in `crates/pdf-font/fonts/README.md`), with PDFium-compatible
  aliases, descriptor-driven family/style inference, a deterministic fallback
  for unknown fonts, standard-14 widths when `/Widths` is absent, and the
  Symbol/ZapfDingbats built-in encodings (Annex D, name-based). *Deferred
  within Phase 3*: **synthetic bold/italic is computed but not applied** —
  `pdf_font::synthesis` reports the needed oblique shear / embolden, and only
  the symbolic faces (no bold or italic cut) ever request it; applying them
  needs an outline shear plus stroke-based emboldening in the CPU lowering
  path. **Size evaluated 2026-07-21, deliberately not taken in the
  small-items pass:** application touches the `FontResource` IR schema,
  `pdf-content` font resolution, the render glyph-cache key (synthesis
  must join the key or caches alias), and needs an outline emboldener —
  a multi-crate change for a case that only fires on bold/italic-requested
  Symbol/ZapfDingbats substitutions. Still deferred. **Done 2026-07-21
  (closeout pass):** applied end to end. `FontResource` gained the
  append-only `synthetic_shear` / `synthetic_embolden_em` fields;
  `substitute_with_style` reports the *requested* style so
  `pdf_font::synthesis` fires for the cut-less symbolic faces; the CPU
  backend applies the 12° oblique (`Outline::oblique`) and a native
  `FT_Outline_Embolden` analog (`Outline::embolden` — bisector-normal
  point shift, dominant-winding-oriented so holes shrink; strength =
  PDFium's weight-700 level, 70/1000 em, `cfx_face.cpp` `kWeightPow`) to
  paint, clip, and fallback routes alike. Synthesis bypasses the shared
  glyph bitmap cache and the hinter (no key aliasing possible; PDFium
  loads NO_HINTING when synthesizing). Tests: Outline unit tests
  (shear, grow-outer/shrink-hole), render-level coverage-growth and
  apex-shear pins, and the FontResource plumb test
  `symbolic_substitute_carries_synthetic_style`. Oracle re-check queued
  (POST-SWEEP-VERIFY §3). PDFium's **Multiple Master** substitution for unknown fonts is not
  ported (the MM faces are Type 1 — Font Phase 5); the nearest standard face
  is used instead. **Assessed 2026-07-21 (closeout pass), stays deferred:**
  the FoxitSansMM/FoxitSerifMM faces are deliberately not bundled (Skrifa
  cannot parse Type 1 MM), and honest support needs MM charstring
  *blending* (`WeightVector`/Blend) in the native Type 1 interpreter plus
  PDFium's `AdjustMMParams` weight/width fitting — days, architectural,
  for unknown-font aesthetics only. Residual set. Non-symbolic **`/Differences` names outside the compact AGL
  subset** now resolve via the face's `post` names, but a full AGL would still
  help embedded fonts lacking `post`.
- **Type 1 `/FontFile` (Font Phase 5)** — *done*, native (`pdf-font/src/type1.rs`).
  PFA/PFB unwrapping, eexec + charstring decryption, the Type 1 charstring
  interpreter (including `seac` accent composition, `div`, flex and hint
  replacement via OtherSubrs 0–3), the font's built-in `/Encoding`, and
  `hsbw`/`sbw` metrics. `FontProgram` picks the engine, so callers are
  unchanged. Verified on 136 real embedded Type 1 fonts from a production
  PDF: 136/136 parse, 4887/5159 glyphs outline (the rest are legitimately
  blank). *Deferred*: unhinted (Type 1 stem hints are parsed and discarded —
  what fonts.md asks for at this phase); Multiple Master interpolation, so
  PDFium's MM-based substitution for unknown fonts is still replaced by the
  nearest standard face; FreeType as a differential oracle is not wired up
  (no FreeType/PDFium build here) — correctness is checked against synthetic
  charstrings with known geometry plus the real-font corpus.
- **Hinting (Font Phase 4)** — *done, opt-in*. `pdf_font::HintingPolicy`
  (`None`/`Embedded`/`Auto`) drives Skrifa's hinter; `Auto` is
  resolution-dependent, hinting only axis-aligned runs at or below
  `AUTO_HINT_MAX_PPEM` (50). Wired through `CpuBackendOptions::hinting`,
  defaulting to `None` so the frozen surface contract's reference output is
  unchanged. *Deferred*: the differential comparison fonts.md asks for
  (against PDFium/FreeType) — no PDFium build is wired up here, so hinting is
  verified against the unhinted path and for grid-alignment/determinism
  instead. Also: hinted glyph origins snap in **y only** (x carries the PDF's
  `/Widths`); subpixel-quantized x positioning and a hinted-glyph mask cache
  (fonts.md §4) are future work; anisotropic scales fall back to unhinted
  because Skrifa's hinting size is a single scalar ppem.
- **Type 3 fonts (Font Phase 6)** — *done*. `/Type3` glyphs execute their
  `/CharProcs` content streams inline (Save / Concat(FontMatrix × text matrix)
  / ops / Restore), Form-XObject-style, so backends needed zero changes.
  Code → glyph name → CharProc via `/Encoding` `/Differences` (never through
  Unicode); glyph-space `/Widths` through FontMatrix drive the advance with
  d0/d1 `wx` as fallback; `d1` glyphs are shape-only (interior color ops
  dropped, painting with the show-time fill); shared invoke-depth guard;
  malformed glyphs skip-but-advance with recovery notes. Oracle-validated on
  three real corpus PDFs (two clean; the 1999 dvips-era `jbig99paper.pdf`
  renders its Type 3 bitmap captions legibly at correct positions but ~heavier
  than PDFium — that residual is the stencil-minification/AA quality gap
  already tracked under 4G images, not Type 3 logic). *Deferred within
  Type 3*: fill/stroke-alpha and images inside `d1` shape-only glyphs;
  resource-name cache aliasing (same limitation as `font_cache`/`tint_cache`).
  **Sweep 15 closure 2026-07-26:** malformed CharProcs are now hard-isolated
  from the caller's graphics-state stack. A stream-leading unmatched `Q` cannot
  consume the wrapper save, and trailing unmatched `q` scopes are explicitly
  unwound with balanced semantic restores. This closes the
  `Tang-Dynasty-Tales-A-Guided-Reader-.pdf` repeated-transform collapse: all
  five formerly suspect sampled body pages now pass both PDFium and MuPDF.
  Test `malformed_charproc_graphics_state_is_isolated`.
- **System font providers (Font Phase 7)** — *done, opt-in*.
  `pdf_font::SystemFontProvider` + `FolderFontProvider` (PDFium's
  `CFX_FolderFontInfo`/`CFX_LinuxFontInfo` analogue) scan PDFium's font
  directories, index families from their `name` tables, and match by family
  name → CJK charset preference list. Injected via
  `PageCompiler::with_system_fonts` and **off by default**: system fonts make
  output depend on the host, so the deterministic bundled-face path stays the
  default (`pdfr render … --system-fonts` opts in). Both simple and Type 0/CID
  fonts substitute; font *collections* (`.ttc`) are handled via a face index
  carried through `SemFont`/`FontResource` into `FontProgram::parse_indexed`.
  *Deferred*: PDFium's generic weight/pitch `FindFont` fallback (the bundled
  14 answer that deterministically); fontconfig is not consulted, only
  directory scanning; no per-glyph fallback chain — one substitute per PDF
  font, as PDFium does, so a face lacking a glyph still yields notdef.
- **Macintosh-cmap-only TrueType subsets — done 2026-07-21 (DLIFLC root
  cause).** The DLIFLC notdef-box pages were *embedded* Office TrueType
  subsets whose only cmap is a Macintosh (1,0) format-6 subtable — Skrifa's
  `Charmap` ignores Macintosh subtables, so every code resolved to notdef.
  `FontProgram` now carries a native (1,0) cmap reader (formats 0/6):
  `gid_for_code` falls back to the raw byte through it and `gid_for_char`
  maps Unicode → Mac OS Roman → (1,0), matching PDFium's
  `cpdf_truetypefont.cpp` kMacRoman branches. The `BaseEncoding::MacRoman`
  high range also gained the real Mac OS Roman table (was a latin-1
  approximation). Tests `mac_only_cmap_resolves_through_the_native_fallback`
  (+ `minimal_ttf_mac_cmap_only` builder); oracle re-check queued in
  POST-SWEEP-VERIFY.md §2.
- **Encoding tables** — *done*. Standard, WinAnsi, MacRoman, Symbol and
  ZapfDingbats all resolve **by glyph name** (ported from PDFium's Annex D
  tables), which is how PDF defines simple-font encoding; the old
  code→Unicode→cmap route was lossy and fails outright for a wrapped bare CFF
  (no cmap). The name→Unicode map (a compact AGL subset + `uniXXXX`) is now
  only a fallback for faces without `post` names. *Deferred*: MacExpert;
  a full AGL. **Both done 2026-07-21 (closeout pass):**
  `BaseEncoding::MacExpert` resolves by name through the 224-entry Annex
  D.4 table ported verbatim from PDFium's `kMacExpertEncodingNames`
  (name-only — expert glyphs have no usable Unicode identities, matching
  PDFium); and the **full AGL 2.0** (4,281 names, generated into
  `agl_table.rs` from Adobe's `glyphlist.txt`, binary-searched) now backs
  `glyph_name_to_char`, including the AGL `name.suffix` → `name` rule.
  The compact subset stays as the fast pre-check. Tests
  `macexpert_resolves_by_name_only`,
  `full_agl_resolves_tail_names_and_dot_variants`.
- **Bare CFF `/FontFile3` (`Type1C`, `CIDFontType0C`)** — *done*.
  `pdf_font::wrap_bare_cff` wraps the raw CFF a PDF embeds in a minimal OTF
  (synthesizing `head`/`maxp`/`hhea`/`hmtx`/`post` from the CFF's own
  `FontMatrix`, CharStrings INDEX and charset; the `CFF ` table is the
  original bytes), so Skrifa can read it — FreeType takes bare CFF directly,
  which is why PDFium needs no equivalent. `cid_to_gid_from_cff` then supplies
  CID→GID for `CIDFontType0`. *Deferred*: the synthesized `hmtx` advances are
  **zero** (real widths live in the charstrings; a PDF positions with its own
  `/Widths`, and `advance()` is only consulted for substituted bundled faces),
  so an embedded bare CFF with no `/Widths` would mis-space; `gid_for_name` is
  a linear scan over `post` (fine at 2–3k glyphs, wants an index for large
  CJK faces). **Update 2026-07-21:** `gid_for_name` now builds a lazy
  name→gid `HashMap` once per `FontProgram` (shared across clones via
  `Arc<OnceLock<…>>`, lowest-gid-wins preserving the scan's first-match
  semantics); every lookup after the first is a hash probe. Pure
  reordering — same results, pinned by the existing Symbol/ZapfDingbats
  name-resolution tests.
- **CMaps** — *mostly done* (section re-baselined 2026-07-20; the earlier
  "not parsed" wording was stale). Identity-H/V works; the predefined CJK
  CMaps **are bundled** (`pdf-font/src/cmap_data/{gb1,cns1,japan1,korea1}.bin`,
  served by `cmap_tables.rs`) and embedded CMap stream parsing exists
  (`pdf-font/src/cmap.rs`). Genuinely remaining: CID→Unicode maps (text
  extraction; in flight, Stream B) and vertical writing (below). **Done
  2026-07-21 (production-readiness pass):** CID→Unicode tables for
  Adobe-GB1/CNS1/Japan1/Korea1 are bundled (~166 KB, `cid_to_unicode` API).
- **Vertical writing (`/DW2`, `/W2`, vertical GSUB)** — *in flight*. **Done
  2026-07-21 (production-readiness pass):** `/W2`/`/DW2` metrics, wmode 1,
  and v-vector placement land vertical text. A **CJK substitution bridge**
  (CID→Unicode → substitute face on missing glyphs) also landed behind the
  opt-in `FolderFontProvider` — the deterministic bundled default still
  renders notdef, per the system-fonts policy above.
- **Glyph/outline cache & worker face** — *todo*. `FontProgram::outlines`
  re-parses a Skrifa `FontRef` per glyph run; a worker-local reusable face and
  outline/mask caches (advice §11) are a perf optimization.

## Image codecs (pdf-image)
- **DCT (JPEG)** — *done* (native, `pdf-image/src/jpeg/`): baseline +
  progressive, restarts, gray/YCbCr/RGB/CMYK/YCCK with Adobe conventions.
  The default `CpuBackendOptions` registry bundles it; `ImageIr` carries
  `codec`/`codec_data` so backends decode via their injected registry.
  *Deferred within DCT — re-baselined 2026-07-20 by a structure-walked
  700-doc corpus census (SOI→SOF marker walk, noise-floor-proof):*
  **arithmetic coding, lossless/hierarchical, and 12-bit precision are
  CLOSED as evidence-based won't-do** — zero occurrences among 524
  DCT-bearing docs (real distribution: SOF0 469, SOF2 progressive 94,
  SOF1 extended 10 — both already supported — one SOF5 outlier), and PDFium
  cannot decode them either (its Chromium libjpeg-turbo fork compiles
  arithmetic out and predates lossless support; Chromium builds 8-bit
  samples only), so implementing them serves neither corpus nor oracle
  parity; the typed errors stay, revisitable if a future corpus disagrees.
  **Fancy chroma upsampling is done (2026-07-26).** The decoder now matches
  libjpeg's integer triangle filters and asymmetric rounding for h2v1 (4:2:2),
  h1v2 (4:4:0), and h2v2 (4:2:0), including image-edge replication and
  libjpeg's narrow-component fallback to box sampling. Both the progressive
  whole-plane path and baseline streaming path use the same sampler. Streaming
  remains O(one MCU row): it delays one band, keeps two reusable bands plus one
  preceding edge row, and therefore has real upper/lower context across MCU
  boundaries without restoring whole-image coefficients or planes. Exact
  formula tests, baseline-vs-coefficient byte equality (including a 4:2:0
  restart stream), and all 67 `pdf-image` tests pass. The libjpeg-derived
  4:2:0/4:2:2 fixture mean-difference guard tightened from 8.0 to 0.75.
  A confirmed 2758x3561 baseline 4:2:0 page (`globalhistoryofm0000unse_1.pdf`
  p0) now has ImageMagick RGB MAE 2.81192 in Q16
  (normalized 0.0000429072) against both PDFium and MuPDF.

  The Sweep-15 scan-tone hypothesis was also tested rather than assumed:
  all sampled rows from `volkstumlichege03grae.pdf` and
  `sexcharacter00wein.pdf` are numerically unchanged after fancy upsampling.
  Those documents do not exercise the changed color-chroma path: their body
  pages are MRC compositions of JPX color layers and a JBIG2 bilevel soft
  mask. Their residual was **not** evidence for more JPEG chroma work. The
  subsequent focused investigation found and fixed nearest-only minification
  of that soft mask; see "Image minification" above. Do not use these pages as
  a gate for further JPEG changes.

  Baseline MCU-row streaming, AVX2 IDCT, AVX2 YCbCr conversion, and the
  DC-only fast path are already done (the older "MCU-row streaming deferred"
  line here was stale). Remaining JPEG work is performance/rarity scoped:
  non-interleaved sequential streaming and aarch64 NEON kernels, not a known
  correctness blocker. (**DCT/JPX-coded `/SMask`
  streams are no longer a gap** — `build_image_smask` routes the mask through
  `decode_stream_data_to_codec` and carries `codec`/`codec_data`, and the CPU
  backend's `resolve_smask` decodes it via the same registry as base images;
  the earlier "smask build path skips codec filters" note was stale.)
### 2026-07-22 — post-sweep-7 fixes (see the `sweep7-followup-fixes` memory)

- **xref (`pdf-structure/src/loader.rs`)** — a chain reached by *recovering* the
  `startxref` offset now has its entries validated (each must point at the
  object header it claims) and escalates to a full rebuild when they do not.
  PDFium never uses such a table at all. Fixed flate_predictor_bpc_1 (never a
  predictor bug), issue11230 (never a CCITT bug), close-path-bug, issue17554.
- **FlateDecode is 1.65x faster** — flate2's `zlib-rs` backend plus
  `decompress_vec` into the output vector's spare capacity (was memcpying every
  byte twice). 446 -> 739 MB/s over 22k real corpus streams. Codec throughput,
  not a page-render win.
- **JPX sub-8-bit precision** — jp2lam decodes 1/2/4-bit components; the
  renderer widens them with PDFium's `src << (8 - prec)`, **deliberately not**
  OpenJPEG's full-range scaling. See the comment in `pdf-image/src/jpx.rs`;
  the choice is measured (exactly 0.00000 vs 0.81-0.99) and one-line reversible.
- **JPEG DNL height** — an `SOF` height of `0xFFFF` truncated before its DNL
  marker is repaired from the image dictionary, as PDFium's
  `PatchUpKnownBadHeaderWithInvalidHeight` does. issue8614 0.88 -> 0.0004.
- **Form path scoping** — path construction is now scoped to the content stream
  across `Do`, matching PDFium's per-form parser. pdfbox/5302 0.6933 -> 0.0052.

- **JPX (JPEG 2000)** — *done* via **jp2lam** (`../../jp2lam`, a relative path
  to the sibling repo under `D:\Rust-projects` = `/mnt/Samsung980_1TB/Rust-projects`),
  a deliberate external dependency shared with Lege (hence the workspace's
  Rust 1.95 pin — jp2lam is edition 2024). **Correction 2026-07-21:** the
  codec paths moved to `../../Lege-ecosystem/lege-codecs/` (jp2lam and
  jbig2enc-rust; temporary until the renderer itself moves into
  `Lege-ecosystem/lege-pdf/render/`), and the workspace toolchain is now
  pinned at **1.97.1** — the 1.95-pin note above is obsolete. Verified bit-exact against
  openjpeg on 441 real archive.org streams. **Now also handles the real
  archive.org blank-page corpus** (corpus item 1 above); a census over the 424
  blank-page documents drove these fixes, each verified bit-exact (or
  near-exact) against `opj_decompress`:
  - `jpx `-major-brand containers (conformance by the compat list, §I.5.2);
  - per-component **QCC** quantization (marker 0xff5d);
  - all five **progression orders** (P=1 permuted odometer);
  - **advisory TNsot / multi-tile-part + tiled** images;
  - **decomposition levels** up to `ceil(log2(min_dim))` (the `floor` bound
    wrongly rejected valid non-power-of-two sizes, e.g. 18-wide/5-level);
  - **CMYK** images (EnumCS 12): 4 components, MCT applied to the first three
    (K passes through), reconstructed as independent planes → `Cmyk8`;
  - **`pclr` palette** (+ `cmap`, whether inside jp2h or a top-level box),
    expanding the single index component to the container's channels, with
    OpenJPEG's fallback when the `cmap` is malformed. **Update 2026-07-27:**
    palette entries are no longer restricted to unsigned 8-bit values:
    signed/unsigned 1–32-bit columns preserve their precision and sign in the
    expanded `Component`; >32-bit entries decline explicitly. When the PDF
    supplies `/Indexed` and therefore supersedes `pclr`, 1/2/4-bit decoded
    samples stay literal palette indices rather than undergoing component
    range expansion (`issue12213.pdf`).
  *Still deferred within JPX* (each now a clean, B3-flagged failure, never a
  silent blank — measured counts from the 424-doc census):
  - **degenerate decomposition level** on power-of-two dimensions (a 5th
    "1-sample" level on a 16-tall image): our inverse DWT drifts ~16/255 from
    OpenJPEG on the trivial split, so it is rejected rather than mis-decoded —
    a tracked DWT follow-up (~2 streams);
  - **truncated-codestream salvage**: OpenJPEG decodes a truncated stream's
    recovered packets ("Stream reached its end"); our strict bit-reader hits a
    `zero-length code-block contribution` and declines. Faithful salvage needs
    end-of-data awareness in `PacketBioReader` (~3 streams);
  - `bpcc`; sYCC (EnumCS 18); POC; subsampled components; arithmetic-bypass
    code-block styles. (Done since: raw J2K codestreams, precincts/SOP/EPH,
    main-header COC transform override, per-tile QCD/QCC/COC/COD overrides, and
    `cdef` JPX in-data alpha / `/SMaskInData` soft masks. **Premultiplied
    `/SMaskInData 2` done 2026-07-26:** backend preparation un-premultiplies
    GrayA/RGBA base samples with clamping and alpha-zero canonicalization, then
    uses the opacity plane as the ordinary soft mask. Test
    `jpx_premultiplied_in_data_alpha_is_unassociated_before_compositing`.)
- **JBIG2** — *done, native*: the decode half of our own `jbig2enc-rust`
  crate (`default-features = false, features = ["decode"]`), replacing the
  interim `hayro-jbig2`. Confined to `pdf-image/src/jbig2.rs` + one registry
  entry + one Cargo line. The decoder's packed `MonoBitmap` MSB-first byte
  view maps straight onto the engine's `Mono1` layout; the codec inverts to
  PDF polarity (JBIG2 black=1 → sample 0). `/JBIG2Globals` is plumbed through
  `DecodeParameters`. The decoder is now complete **through Phase 5** (its
  5a–5f "wild-PDF" work order): Huffman symbol dictionaries/text regions,
  generic templates 0–3 + TPGDON, refinement/aggregate coding, striped pages,
  intermediate regions, custom tables, and a `Compatible` strictness mode with
  documented recoveries — which this codec now selects, since PDF input is
  wild by definition. The only remaining `Unsupported` returns are defensive
  spec-edge rejections (misplaced segment types in a globals stream, IAID
  width > 24 bits, unknown-length on non-generic segments), not features.
  *Deferred*: adopt `decode_embedded_into` (zero-alloc destination API) when
  the per-image decode cache / perf pass lands.
- **CCITTFax** — *done, native*. From-scratch T.4/T.6 decoder in
  `pdf-image/src/ccitt.rs`, ported function-by-function from PDFium's
  `faxmodule.cpp` with line-cited provenance (run tables transcribed
  verbatim; G4 pass/horizontal/vertical±3 modes, MH terminal+makeup+extended,
  K>0 per-row tag bits, `/EncodedByteAlign`, `/Rows`, EOL tolerance,
  row-salvage damage handling; only a zero-row stream errors). Public codec
  surface (`CcittParams`/`CcittCodec`/Mono1 contract) unchanged. Verified by
  a 1260-case round-trip matrix AND a byte-identical differential against
  `hayro-ccitt`, which is retained only as a dev-dependency oracle — **no
  third-party codecs remain at runtime**. *Deferred*: `/EndOfBlock`
  unconsulted (matches PDFium's FaxDecoder); the 8-byte FindBit skip (perf
  pass); inline images spell `/DecodeParms` as `/DP` with abbreviated keys,
  so inline CCITT/JBIG2 gets the spec defaults (vanishingly rare).
  **Update 2026-07-21:** inline `/DP` (and spelled-out `/DecodeParms`) is
  now read — `inline_codec_parms` mirrors the XObject `read_codec_parms`
  (K/Columns/Rows/BlackIs1/EncodedByteAlign/EndOfLine/EndOfBlock; the keys
  *inside* the parms dict were never abbreviated per Table 93, only the
  top-level key was; `/JBIG2Globals` needs an indirect stream an inline
  image cannot reference). Test `inline_image_dp_carries_ccitt_parameters`.
  **Update 2026-07-27:** XObject `/DecodeParms` scalar entries themselves may
  be indirect and are now resolved before type conversion. Regression:
  `ccitt_decode_parm_scalars_may_be_indirect`; real verification:
  Byzantine Legacies p102.
  `/EndOfBlock` consultation is **won't-do by oracle-parity policy**
  (re-affirmed 2026-07-21): PDFium's FaxDecoder ignores it, so consulting
  it could only diverge from the oracle with no corpus evidence of need.
- **Per-image decode cache** — *todo*. Codec images are decoded per draw op
  at lowering time; the CPU resource cache (advice §12.2) will memoize by
  `ResourceKey`. **Done 2026-07-21 (production-readiness pass):** the
  production `SharedImageCache` — 96 MiB budget, 8-shard LRU, keyed by
  **content hash** rather than `ResourceKey` (so identical streams share
  across pages), tunable via `PDF_RENDERER_IMAGECACHE`.

## Scheduler (Phase 5)
- **Single physical pool (advice §12)** — the pipeline uses an explicitly
  partitioned compile/render worker split summing to ≈ core count (an accepted
  §12 arrangement); a single work-stealing pool with render-priority scheduling
  is a documented alternative.
- **Fine-grained memory permit classes** — the budget is a single byte pool;
  per-class permits (decoded images, font resources, tile surfaces, output
  pages, downstream) are future refinement. **Assessed 2026-07-21 (closeout
  pass), stays deferred:** class-tagged permits require accounting hooks in
  every allocation site across `pdf-render-cpu` (image cache, font cache,
  tile offscreens, output buffers) plus the sequence-priority granting
  design that also fixes the reorder-buffer note below — a scheduler
  architecture change, not an hours-scale item. Residual set.
- **Reorder-buffer output memory** — *approx*. A job's memory permit is
  released when its render returns, so finished pages parked in the reorder
  buffer behind one slow page are **not** counted against the budget. Holding
  permits until emission would deadlock (buffered outputs starve the earlier
  page they wait on); the fix is sequence-priority permit granting, part of
  the permit-classes refinement above.
- **Mid-render cancellation granularity** — *approx*. The pipeline injects its
  token into each request's `limits.cancellation`, but the CPU backend checks
  it only at render entry; periodic checks between executor ops are future
  work. **Done 2026-07-21 (closeout pass):** `run_ops` checks the token at
  op boundaries every 16 commands (and between tiling-cell instances);
  a fired token stops execution, sets `RenderStats::cancelled`, and the
  backend returns `RenderError::Cancelled`. Codec decodes already honored
  `DecodeLimits::should_cancel`. Residual granularity: one very large
  single image draw still completes before the next check (bounded by the
  output size on the DirectPage path). Tests
  `mid_render_cancellation_stops_between_ops`,
  `uncancelled_render_executes_every_op`. No-cancellation renders are
  byte-identical (checks only).
  **Residual closed 2026-07-25 (post-move refinement):** production path fills
  and clip-mask construction now use `RasterKernel::fill_cancellable`, polling
  while building/depositing edges, sweeping active edges, and emitting rows.
  Cancellation discards partial clip masks and clears reusable signed-area
  scratch before the worker returns, so the next request cannot inherit stale
  coverage. `run_ops` stops immediately after an interrupted command. Tests:
  `cancellation_stops_inside_a_single_coverage_command` and
  `cancellable_fill_stops_inside_one_path_and_resets_worker_scratch`.

## Later phases
- **WGPU backend (Phases 7–11)** — **Started 2026-07-26:** the first
  production-shaped slice renders opaque decoded RGB8 image-only pages on one
  resident GPU page surface with nearest/bilinear/fractional box sampling and
  one final readback. It shares `lege-gpu`'s device/detection, has conservative
  preflight, typed failure boundaries, and an experimental CPU-fallback
  executor.

  **Upload cache + real DCT continuation 2026-07-26:** decoded source buffers
  now live in a thread-safe, byte-bounded 128 MiB LRU keyed by immutable sample
  allocation identity and dimensions. Retaining the decoded Arc prevents
  pointer reuse; a changed decode allocation misses naturally. Telemetry
  exposes per-render upload/reuse and lifetime hit/miss/insert/eviction/
  residency counts. Hit, changed-content miss, eviction, and four-way warm
  concurrent reuse tests pass.

  Searchable scans with only `Tr 3` or zero-alpha OCR text are now eligible;
  those operations are proven non-painting and the CPU preparation seam drops
  only the corresponding zero-alpha prepared glyph runs. Visible or clipping
  text still declines to CPU.

  RTX 4060/Vulkan results: synthetic 2400×3200→1200×1600 is 9.277 ms cold and
  1.986 ms warm-cached vs 30.386 ms CPU (15.30× warm). Sweep-15
  `Goebbels Diaries.pdf` p97 is 20.504 ms cold GPU vs 109.921 ms cold CPU
  (5.36×), and 1.462 vs 100.489 ms warm (68.73×), max RGB difference one.
  The larger `globalhistoryofm0000unse_1.pdf` p134 (25.72 MiB upload,
  2578×3487 output) is 177.728 vs 674.480 ms cold (3.79×), and 16.973 vs
  589.873 ms warm (34.75×), byte-exact.

  **Eligibility + robustness continuation 2026-07-26:** production preflight
  and the new `eligibility-census` example share one structured reason
  classifier. A deterministic 240-page sweep-15 sample contained 136
  image-bearing pages; 23/136 (16.9%) were statically and decode-confirmed
  eligible, with no hidden preparation declines. Overlapping blockers rank
  visible text (167), paths (111), clips (93), non-initial color spaces (90),
  non-8-bit images (70), and soft-mask state (34). The original pure-scan
  `buddhasahibsmenw0000alle_1.pdf` is a stronger positive fixture: all
  **356/356** pages are eligible at scale 2 despite 135,094 invisible OCR
  glyph draws. Page 180 on RTX 4060/Vulkan measured 47.765 ms cold GPU vs
  344.469 ms CPU (7.21×), and 4.222 ms warm vs 293.167 ms CPU (69.43×),
  byte-exact.

  Cancellation is now checked through already-submitted GPU readback polling
  at 1 ms granularity as well as before allocation, between draws, and before
  submission. An after-submit cancellation test verifies `Cancelled` and
  successful reuse of the same device. A one-shot injected
  device-loss-class failure verifies CPU fallback telemetry and that GPU
  routing resumes on the next page, without destructively invalidating Lege's
  shared device.

  **Production full-page wiring 2026-07-26:** `pdfr render` now constructs the
  experimental policy executor, so
  `LEGE_PDF_IMAGE_RENDERER=cpu|gpu|auto` controls an actual production-facing
  caller and reports which route executed plus upload/readback counters. CPU
  remains the unset default. On the original book's page 180 at the command's
  fixed 150 DPI (3486×5696), the complete CPU command took 1.53 s and cold GPU
  took 0.53 s (2.9×), with normalized RGB MAE 0.001363 and PSNR 51.56 dB.
  CLI policy, invalid-policy, annotation fallback, and core pipeline tests
  pass.

  **Viewer tile wiring 2026-07-26:** the viewer's production raster worker now
  uses the same opt-in policy while preserving its caller-owned
  `CpuWorkerContext` for default, ineligible, and fallback requests. The new
  `pdf_tile_profile` example exercises the real worker and tile transforms.
  On the same page, 12 visible 256×256 tiles at scale 1 measured
  **92.734/50.725 ms CPU cold/warm vs 71.636/21.261 ms GPU
  (1.29×/2.39×)**. All 96 GPU requests routed natively with zero fallback and
  an identical aggregate pixel checksum. At scale 2 the cold/warm gains were
  1.27×/2.49×; resampling differences were accepted. The remaining promotion
  issue exposed here was synchronous adapter discovery: forced-GPU document
  open added roughly 159–226 ms.

  **Lazy viewer initialization 2026-07-26:** that startup issue is closed.
  `PdfEngine::open` now validates policy without touching WGPU; one shared
  `OnceLock` initializes the renderer on the first final job inside the
  conductor's background raster pool. Text-first tiles bypass the cell. On the
  same fixture, open fell to **6.359 ms**; the first 12-tile set, now including
  one-time discovery, took 220.661 ms, and warm rendering remained 20.459 ms.
  A future idle prewarm is optional pending interactive measurements, not a
  correctness requirement.

  **Prepared color-space expansion 2026-07-26:** the CPU preparation seam now
  resolves Gray, Indexed, CMYK, `/Decode`, low-bit-depth packed samples, and
  the other already-supported CPU image spaces through the normative
  source-pixel conversion before WGPU upload. Converted RGB allocations use a
  document-scoped, thread-safe 64 MiB LRU with stable `Arc` identity, so
  repeated viewer tiles hit both the conversion cache and the GPU upload
  cache. Direct RGB8 stays zero-copy. Conversion polls cancellation and
  oversized expansions remain on the CPU destination-driven sampler; the
  static classifier reports `image-rgb-conversion-over-64m`, keeping corpus
  census and production routing exact.

  On the same deterministic 240-page sweep-15 sample, static and
  decode-confirmed eligibility both increased from **23 to 47 pages**, or
  **16.9% to 34.6%** of the 136 image-bearing pages. There are no hidden
  preparation declines. Real-GPU tests cover inverted Gray `/Decode`, packed
  1-bit Indexed, and calibrated CMYK output, including a warm upload-cache hit.
  A non-RGB DCT scan (`Stari srpski zapisi i natpisi.pdf` p48) measured
  **82.025 ms CPU vs 17.841 ms GPU warm** for 12 viewer tiles (4.60×).
  Bilevel routing is workload-dependent: an optimized CCITT page was slower
  on GPU at scale 1 (19.341 vs 14.454 ms warm) but faster at scale 4
  (17.310 vs 31.810 ms). **Automatic routing heuristic closed 2026-07-26:**
  scale 2 remained CPU-favorable (16.697 vs 20.162 ms warm), matching the
  boundary of the CPU packed-bilevel minification fast path. `Auto` now keeps
  1-bit images on CPU when either source-footprint axis exceeds one texel per
  destination pixel, and declines before RGB expansion/upload. Near or above
  source resolution it routes to GPU. Forced `Gpu` is intentionally
  unaffected, preserving the complete experimental surface.

  **Prepare-first startup + recoverable shared device 2026-07-26:** `Auto`
  now classifies and prepares before touching WGPU, then gives an eligible
  prepared page directly to the lazily created backend. On the CCITT fixture
  at scale 2, this eliminated adapter discovery entirely
  (`gpu_initializations=0`) and reduced the cold 12-tile set from
  **156.868 ms to 29.501 ms**. Scale 4 initialized once and remained fully
  GPU-routed. The shared GPU context now records wgpu's real device-loss
  callback and occupies a replaceable slot; the renderer invalidates its
  pipelines/cache and lazily reconstructs them after a loss. Initialization
  and recovery counts are exposed in routing telemetry. The eligible scale-4
  case passed without fallback on both the RTX 4060 and Intel Iris Xe Vulkan
  adapters, with identical aggregate checksums.

  **Image soft masks + solid stencil brushes 2026-07-26:** image-attached
  `/SMask` (including codec-backed JPX/JBIG2 MRC and `/SMaskInData`) and
  solid-colour `/ImageMask` draws now enter the GPU renderer. The CPU
  preparation seam normalizes packed mask samples and `/Decode` into a
  document-cached alpha8 plane; WGPU uploads/caches that plane independently,
  samples its own dimensions and minification footprint, and composites it
  source-over. Opaque image draws keep the prior overwrite shader path.
  Synthetic GPU tests cover soft-mask transparency, stencil polarity/brush
  colour, warm RGB+opacity upload reuse, and CPU preparation cache identity.

  The deterministic sweep-15 sample moved **47 → 50** decode-confirmed
  eligible pages (**36.8%** of 136 image-bearing), with no static/preparation
  drift. The new pages are one solid CCITT stencil and two JPX+JBIG2 MRC
  soft-mask pages. On RTX 4060/Vulkan, `Appian Roman History.pdf` p196 at
  scale 2 measured **129.424 ms CPU vs 16.446 ms GPU warm (7.87×)** with all
  48 requests GPU-routed and no fallback. The real solid-stencil fixture at
  scale 4 measured **65.613 ms CPU vs 20.128 ms GPU warm (3.26×)**. This also
  narrowed `Auto`'s bilevel rule: stencils now stay GPU-eligible under
  minification because the CPU's packed-bilevel popcount fast path explicitly
  excludes them.

  **Explicit hard image masks 2026-07-26:** colour-key `/Mask` arrays and
  separate stencil-mask streams now lower into the same bounded, independently
  cached alpha8 upload path. Colour keys are evaluated against every raw source
  component before `/Decode`, and stencil streams retain hard-mask polarity
  after codec decoding. Unlike soft masks and `/ImageMask` coverage, explicit
  hard masks remain nearest/binary under minification. Synthetic CPU/GPU tests
  cover both forms, polarity, cache reuse, upload accounting, and compositing.

  The deterministic sweep-15 sample moved **50 → 51** decode-confirmed
  eligible pages (**37.5%** of 136 image-bearing), again with exact
  static/preparation counts. The newly admitted `Argentine Democracy.pdf`
  p108 page routed all 60 viewer tile requests to the RTX 4060 without fallback
  at scale 2 and measured **220.742 ms CPU vs 22.511 ms GPU warm (9.81×)**.
  The other sampled hard-mask page correctly remains CPU-routed because it also
  contains visible text and clipping.

  **Parallel page execution + rectangular image clips 2026-07-26:** the
  production viewer concurrency path was audited and measured before extending
  GPU coverage. Each persistent raster worker has its own CPU worker context
  and submits an independent page buffer, encoder, and readback through the
  shared WGPU device/queue. There is no page-wide mutex or one-page-at-a-time
  software queue; only short upload-cache lookups are locked. The hardware
  queue orders submissions, but multiple pages are submitted and await
  readback concurrently.

  `lege-viewer/examples/pdf_parallel_profile.rs` now measures that exact
  production path with persistent workers. On eight whole image pages from
  `buddhasahibsmenw0000alle_1.pdf` (pages 180–187, scale 1, RTX 4060/Vulkan),
  warm time was **354.022 ms with one GPU worker vs 76.023 ms with eight
  (4.66× concurrency gain)**. Eight CPU workers took **461.894 ms**, so the
  eight-worker GPU route was **6.08× faster**. Checksums were stable and equal
  across CPU/GPU runs, and all 40 measured GPU requests routed without
  fallback. Small 256×256 tiles are a different boundary: eight GPU workers
  and eight CPU workers were effectively tied (132.161 vs 130.303 ms for 32
  tiles). Page-level GPU rendering is therefore viable without sacrificing
  page parallelism, while `Auto` should remain conservative for small,
  already-parallel tile sets until batching/resident presentation removes
  their fixed transfer cost.

  Axis-aligned rectangular `PushClip` paths are now accepted after tracking
  the full Save/Restore/Concat CTM stack. They reuse the CPU preparer's exact
  bounds-only lowering, so analytic clips still decline rather than being
  approximated. The sweep-15 sample moved **51 → 60** decode-confirmed
  eligible pages (**44.1%** of 136 image-bearing pages); clip-blocked pages
  fell **93 → 19**, with exact static/preparation agreement. Nine real pages
  became fully eligible. `The Image of Edessa.pdf` p86 automatically routed
  all seven whole-page requests to GPU without fallback and measured
  **63.777 ms CPU vs 1.746 ms GPU warm** after initialization. Synthetic
  coverage verifies both the painted rectangular interior and untouched
  exterior; a triangular clip was retained as the next capability gate.

  **Analytic image clip masks 2026-07-27:** arbitrary path clips around image
  draws now reuse the normative CPU rasterizer's exact anti-aliased coverage
  instead of reimplementing PDF curves/fill rules in WGSL. Preparation exports
  one bounded device-space alpha8 plane per distinct consumed path-clip chain;
  nested clips are already multiplied by the CPU mask builder, rectangular
  descendants remain bounds-only, and multiple images under one mask share the
  same `Arc` and device upload. WGPU samples the clip by absolute device
  coordinate and multiplies it with any independent image `/SMask` opacity.
  Cancellation remains cooperative during mask rasterization. Text clips still
  decline because their glyph-outline preparation is a separate vocabulary.

  The deterministic sweep-15 sample moved **60 → 63** decode-confirmed
  eligible pages, or **46.3%** of 136 image-bearing pages, with static and
  prepared counts again identical. Standalone `clip` blockers fell **19 → 0**;
  sixteen of those pages still have independent visible-text/path/group
  blockers. One newly eligible page uses a true analytic clip plane; the other
  two use rectangle forms recognized by the CPU lowerer's final classifier.
  On RTX 4060/Vulkan, `Byzantium, Latin Romania and the Mediterranean.pdf`
  p290 measured **93.463 ms CPU vs 5.349 ms GPU warm whole-page (17.47×)**,
  and **93.839 ms vs 24.359 ms** for twelve viewer tiles (3.85×). All requests
  routed automatically without fallback. A full 1241×1755 render had normalized
  CPU/GPU RMSE **0.000131** (about 77.6 dB PSNR), with visual differences
  limited to ordinary scan resampling.

  **Page-level soft-mask state 2026-07-27:** image-page preparation now walks
  the CPU prepared stream's balanced `PushSoftMask` / `PushSoftMaskNone` /
  `PopSoftMask` scopes. Real mask-group content is still rendered once by the
  normative CPU executor, so arbitrary mask paths/images/text/groups, nested
  masks, Alpha vs Luminosity, `/BC`, and `/TR` retain CPU semantics; only the
  derived bounded device-space alpha8 plane crosses the GPU seam. WGPU binds
  that plane independently from image opacity and analytic clipping and
  multiplies all three coverages. A bounded 64 MiB document-session cache keeps
  derived-mask `Arc` identity stable, allowing the existing GPU upload cache to
  hit on page revisits.

  This work also fixed an independent CPU bug: an empty real Alpha/plain
  Luminosity mask was represented as `None` and therefore disabled masking,
  instead of retaining zero coverage everywhere. `/SMask /None` remains the
  distinct `PushSoftMaskNone` state. CPU regressions cover the distinction;
  RTX 4060/Vulkan tests cover an active path-derived luminosity mask, warm mask
  upload reuse, and nonzero `/BC` coverage outside the mask bounds.

  The deterministic sweep-15 sample moved **63 → 64** decode-confirmed
  eligible pages, or **47.1%** of 136 image-bearing pages, with static and
  prepared counts still exact. The census now separately reports active
  page-mask draws: this sample has **zero**, confirming that the newly admitted
  `The Ashgate Research Companion to Imperial Germany.pdf` p0 only carries
  `/SMask /None` state. It nevertheless measured **71.641 ms CPU vs 0.993 ms
  GPU warm (72.15×)** at scale 2; the active-mask shader path is validated by
  the focused GPU fixture rather than mislabeling that page as a real mask
  oracle. A broader future sweep should locate a real image-only active-mask
  page for external visual validation.

  **Braudel image-`/SMask` oracle + constant image alpha 2026-07-27:** the
  suggested `The Structures of Everyday Life` volume is a strong MRC
  image-resource mask stress test, but it does not use active page-level
  graphics-state masks. All **632/632** pages are statically and
  decode-confirmed eligible at scale 2, with zero active page masks. Page 0
  combines two 2222×3191 JPX images and attaches `/SMask` to the foreground;
  page 300 combines a 699×1017 JPX background with a masked 2099×3055 JPX
  foreground.

  On RTX 4060/Vulkan, page 0 measured **181.673 ms CPU warm vs 1.524 ms GPU
  warm (119.20×)**; page 300 measured **153.356 ms vs 1.268 ms (120.97×)**.
  RGB mean absolute differences were 0.017 and 0.034 respectively. Both warm
  runs reused all three cached planes (two images plus the mask), confirming
  the image `/SMask` preparation, sampling, and upload-cache path on a real
  all-MRC book.

  Constant Normal-blend image opacity now also remains eligible. CPU
  preparation carries it as alpha8 and WGSL multiplies it independently with
  image opacity, analytic clip coverage, and active page-soft-mask coverage
  before source-over composition. CPU-only and real RTX tests cover 50%
  constant alpha combined with image `/SMask`. The deterministic 240-page
  sweep-15 sample remains **64/136 (47.1%)**, since it contains no page blocked
  solely by image alpha; this is a semantic closure rather than a measured
  coverage increase.

  **Image blend modes + atomic Auto fallback 2026-07-27:** all 16 PDF image
  blend modes now cross the prepared image seam. WGSL implements the CPU
  compositor's separable and non-separable formulas, with premultiplied
  source-over and the existing image opacity/resource mask/analytic clip/page
  soft-mask coverages. A two-image overlap regression on RTX 4060/Vulkan
  passed every mode with every output byte within one level of CPU. The
  deterministic sweep-15 sample remains **64/136 (47.1%)** because no sampled
  page was blocked solely by image blend mode. Braudel p0 retained the Normal
  hot path at **182.396 ms CPU vs 1.561 ms GPU warm (116.83×)**.

  The policy seam now treats a GPU attempt as a transaction over the immutable
  request. A complete validated/read-back `HostPage` is the only GPU result
  that can escape; typed errors or unexpected panics discard the attempt and
  repaint the original request from its first operation on CPU. Telemetry now
  distinguishes GPU panics and CPU failures. A real-hardware injected-panic
  test proves byte-identical fallback, then verifies that `Auto` quarantines
  the panicking backend and sends subsequent pages directly to CPU. An
  eight-way parallel fault test also verifies that every in-flight page
  completes across that quarantine boundary. Device loss remains separately
  invalidated and lazily recreated. Cancellation remains cancellation and
  intentionally does not start duplicate CPU work.

  **Patterned stencil brushes 2026-07-27:** patterned `/ImageMask` paint now
  crosses a bounded hybrid seam rather than forcing an otherwise image-only
  page to CPU. The normative CPU tiling executor renders the arbitrary pattern
  cell through the stencil into straight RGB plus alpha; WGPU retains final
  painter ordering, constant alpha, all blend modes, and active page masks.
  A 64 MiB request-shape LRU keeps the two planes stable so the existing upload
  cache hits warm. Degraded nested pattern-cell draws decline to full CPU,
  preserving recovery diagnostics. `Auto` also declines pattern-only pages
  because CPU already performed their only paint, but admits the bridge when
  a native image draw can amortize the GPU transfer/readback.

  On RTX 4060/Vulkan, a native RGB backdrop plus colored tiling stencil under
  Multiply at 75% alpha matched the CPU renderer within one byte and reused
  all three uploads on the warm pass. The deterministic sweep-15 sample
  remains **64/136 (47.1%)** because it has no page blocked solely by a
  patterned stencil; this closes the known semantic gate without fabricating
  a corpus gain.

  **Forced-GPU mixed content + text clipping prototype 2026-07-27:** the
  external-raster seam is now a painter-ordered command stream rather than an
  image list. Decoded images, solid fill/stroke paths, and solid visible text
  can be interleaved without a readback boundary. Visible glyphs are lowered
  as small outline batches and rasterized natively in WGSL; Draft uses 4×4
  samples and Normal uses 8×8. The shader shares each Y crossing calculation
  across a complete sample row, and the preparation seam simplifies the CPU
  flattener's conservative curves to 0.1 device-pixel tolerance before packing
  16-row edge bands. Alpha, all blend modes, rectangular/path clips, active
  page soft masks, and image-resource masks retain painter semantics. Text
  clips work through the existing exact CPU-derived device alpha plane; moving
  clip-outline coverage itself to GPU remains a separate optimization, not a
  correctness gap.

  Forced `Gpu` accepts path-only pages as well as mixed image pages. `Auto`
  deliberately retains both gates: at least one native image draw, and no
  vector/text path commands. The transaction boundary is unchanged, so any
  preparation, validation, submission, mapping, device-loss, or panic failure
  discards the attempt and restarts the immutable request on CPU. Synthetic
  RTX tests cover ordered image→path paint, path-only forced routing, a real
  text clip constraining a native GPU fill, warm path-upload reuse, and the
  existing parallel quarantine/fallback cases. The full `pdf-render-wgpu`
  suite passes 30/30 on RTX 4060/Vulkan.

  **GPU-native mixed span/coverage batching 2026-07-27:** Consecutive path
  commands now lower into bounded batches over deterministic 8×8 active-tile
  worklists; images remain painter-order barriers. One workgroup owns a tile,
  shares each crossing across its horizontal subpixel row, accumulates
  coverage cooperatively, and composites all paths while retaining the page
  pixel locally. Batches split at 64 paths per tile, 4,096 paths, or 64 MiB
  component limits. A stable-identity device LRU caches the complete uploaded
  descriptor/geometry/tile/mask bundle, with clip and soft-mask planes
  deduplicated into one alpha atlas.

  On `The-Flower-of-Chinese-Buddhism.pdf` p99 at 354×592, the 411 logical path
  draws and 45,814 edges now execute as **one path dispatch**, 2,143 active
  tiles, 4,331 tile-path references, and maximum tile depth seven. RTX
  4060/Vulkan warm paint/readback fell from **101.357 ms to 5.914 ms** (about
  17.1×); warm preparation is 0.002 ms and the complete batch upload is reused.
  Output stayed at RGB MAE 0.990, maximum 41, and 14.33% changed channels. CPU
  measured 4.784 ms, leaving GPU at 0.81× CPU: a major implementation win but
  still below the 1.2× Auto gate. Auto therefore continues to decline every
  mixed path page. The next mixed-content session should use a representative
  sweep-15 subset to determine whether the remaining gap is coverage work,
  image/readback overhead, or page-shape crossover; do not relax routing from
  this single-page result.

  A second sweep-15 probe,
  `How-Zen-Became-Zen-…pdf` p50 at 432×648, exercised a path-only page with
  456 logical draws, 76,090 edges, 2,823 tiles, and one dispatch. GPU measured
  5.012 ms versus 0.941 ms CPU, confirming that dispatch collapse alone does
  not make small CPU-native vector pages suitable for automatic routing. A
  bilevel scan probe was correctly rejected by the existing minification gate.

  **Shared GPU completion scheduling 2026-07-27:** PDF rendering,
  postprocessing, resize, binarization, and layout inference already use
  `lege-gpu`'s one process-wide device and queue. Their per-page/session
  scratch remains independent, so CPU command encoding and page preparation
  can proceed in parallel while WGPU preserves submission order. The shared
  poller formerly waited for the most recent process-wide submission,
  however, which allowed work queued later by an unrelated client to become
  an accidental readback dependency. Every production readback path now
  carries its own `SubmissionIndex` and waits for that exact fence. Batched
  binarization waits only for its final ordered submission and no longer
  performs a redundant whole-device wait before mapping.

  Do not add a global one-job mutex: it would undo parallel page rendering.
  A larger centralized scheduler is deferred until cross-workload telemetry
  demonstrates queue monopolization or aggregate VRAM pressure. If needed,
  it should provide bounded, fair admission across workload classes while
  retaining multiple in-flight jobs; the queue itself already supplies
  correctness and ordering. Current local bounds are renderer/page workers,
  two binarizer sessions, and the inference session plus VRAM semaphores.
  Resize sessions are created on demand (only idle retention is capped), so
  their effective bound is the calling pipeline's page-worker limit. None of
  those budgets is coordinated globally yet.

  Still deferred: an actually induced driver-loss/recreation run, Windows
  DX12, a real image-only active page-soft-mask corpus oracle, GPU-native
  clip-outline coverage, performant mixed-content batching sufficient for
  `Auto`, and the resident renderer-to-postprocess/presenter handoff. The
  Windows continuation is isolated in
  [`WINDOWS-DX12-GPU-VALIDATION.md`](WINDOWS-DX12-GPU-VALIDATION.md); see also
  [`PLAN-GPU-IMAGE-RENDERING.md`](../plans/PLAN-GPU-IMAGE-RENDERING.md).
- **Postprocess graph (`pdf-postprocess`)** — skeleton only. **Done
  2026-07-21 (CPU executor):** `CpuPostprocess` implements the full Stage C
  vocabulary on the frozen `HostPage` contract — dpi-preserving `Crop`,
  `Resize` (Nearest / Box area-average with exact fractional-overlap taps /
  Bilinear / CatmullRom / Lanczos3, kernel support scaled by the shrink
  ratio on downscale), `ConvertToGray` (Rec.709 or flat, premultiplied RGBA
  composited over the white paper backdrop), `ApplyToneCurve` (256-entry
  LUT; `ToneCurve::invert` / `::brightness_contrast` constructors; RGBA is
  un-premultiplied → mapped → re-premultiplied, alpha untouched), `Otsu`,
  `Sauvola` (integral-image mean/stddev), `FuseThresholds` (global/local
  blend), `Dither` (hard-128 / Floyd–Steinberg / Bayer 4×4), and
  `PackMonochrome` (MSB-first, 1 = ink, must be final → `PackedMono`).
  Typed errors throughout (no panics); per-op unit fixtures + a chained
  scan-cleaning pipeline test (26 tests, `crates/pdf-postprocess/src/cpu.rs`).
  **Done 2026-07-26 (capability contract):** `BackendCapabilities` now carries
  `PostprocessCapabilities` with an operation bitset and a distinct
  resident-execution flag. The CPU renderer and `CpuPostprocess` truthfully
  advertise the complete Stage C vocabulary after host readback; the WGPU stub
  advertises none. `PostprocessGraph::required_operations` provides the
  backend-neutral negotiation bridge and is pinned against every graph
  operation.

  **Done 2026-07-26 (Floyd–Steinberg performance):** error diffusion now keeps
  only padded current/next scanlines instead of a full-page `i32` error image,
  without changing raster order or integer arithmetic. A byte-equivalence
  guard compares it with the former full-frame algorithm on a varied,
  odd-width image. On a synthetic 2480×3508 gray page (nine release runs),
  median time fell **49.096 ms → 39.970 ms (1.23×)** and error scratch fell
  **34,799,360 bytes → 19,856 bytes**.

  **Experimental implementation 2026-07-26:** the focused WGPU session is now
  underway and recorded in
  [`PLAN-GPU-POSTPROCESS-EXECUTOR.md`](../plans/PLAN-GPU-POSTPROCESS-EXECUTOR.md).
  `pdf-postprocess` has an opt-in `gpu` feature with a full-vocabulary,
  one-upload/one-readback resident executor, a two-session scratch pool, and
  `Cpu`/forced-`Gpu`/hardware-only-`Auto` policy with whole-graph CPU fallback
  and execution telemetry. It reuses `lege-gpu`'s single process-wide
  adapter/device/queue and therefore follows the same detection policy as
  Lege's existing GPU work. Linux Vulkan parity fixtures pass on an RTX 4060:
  discrete operations and threshold/dither/pack are exact; smooth resize is
  within one LSB.

  Otsu now uses a parallel resident histogram, and Sauvola/fusion use paired-u32
  64-bit summed-area tables rather than direct per-pixel window scans. At
  1200×1600 on an RTX 4060/Vulkan, synthetic standard and adaptive scan graphs
  measured 18.36× and 15.37× faster than CPU, respectively, but differed by
  one and two packed bytes after Lanczos/thresholding.

  *Still deferred within this focused item:* shared deterministic fixed-point
  CPU/WGSL threshold math, corpus-scale parity measurement,
  device-loss/concurrency stress, Windows DX12 validation, and the later
  resident handoff from the still-stub GPU PDF renderer. CPU remains the
  default; promotion to automatic selection is gated by the linked plan.
  **Priority decision 2026-07-26:** pause those promotion tasks and focus the
  next GPU session on the larger decoded-JPEG image painting/compositing
  hotspot in `pdf-render-wgpu`; that first experimental slice is now
  implemented and the focused continuation plan is
  [`PLAN-GPU-IMAGE-RENDERING.md`](../plans/PLAN-GPU-IMAGE-RENDERING.md).
  Small postprocess parity differences are acceptable when corpus review shows
  equal or better binarization quality—the CPU result is not itself a perfect
  quality oracle. The experimental executor stays available without changing
  production behavior.
- **Drop-in integration stages (§10), runtime backend policy (§11),
  resource caching (§12)** — designed in the plan; not implemented.

## Affine transforms (affine.md)

§5 (scale-independent singularity) and §18 (non-finite guards) are done.
The rest of the doc was audited against the code; what is left, and why:

- **§13 — anisotropic and rotated stroke pens.** `lower_stroke` takes the
  device half-width from `sqrt(|det|)`, the geometric mean of the CTM scale.
  The doc names this exact shortcut as incomplete, and it is: under
  `scale(10, 1)` the mean is 3.16, so a pen that should be 10 wide
  horizontally and 1 vertically is drawn 3.16 wide both ways. Correct
  handling means expanding the stroke against the full transform (an
  elliptical pen), not one scalar. Real, visible on non-uniform CTMs, and
  the largest remaining affine gap. `sqrt(|det|)` is exact for uniform
  scale, which is the overwhelming majority of real content, so this is
  ranked below whatever the corpus run turns up.

- **§11 — incremental image stepping.** Implemented and *measured*: no
  gain (271 vs 279ms, 882 vs 862ms — noise both ways) and it moved 6479
  pixels on an MRC page, because a 1-ulp difference in the stepped position
  flips a `floor()` at a texel boundary. Reverted. The transform was never
  the cost; the sampler's per-texel work was, and fast-pathing byte-aligned
  reads took the same page 862ms -> 659ms. Transform-per-pixel therefore
  stays in the hot loop **by choice**. Revisit only if a profile says the
  affine is material, which it currently is not.

- **§7 classification, §9 `map_rect_to_aabb`, §12 `with_inner_translation`.**
  All performance work, none of it profiled as hot. §11 is the standing
  reminder of what happens when this doc's performance advice is taken on
  faith: deferred until a measurement asks for them.

- **§4 private fields.** `Matrix`'s coefficients are `pub` and read directly
  across crates. Encapsulating them is a wide mechanical refactor with no
  behaviour change; not worth the churn while the surface is still moving.
