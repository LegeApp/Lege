---
name: sweep6-analysis
description: "Sweep 6 (2026-07-21, Linux, all 3 roots, 100k pages) results vs sweep 5, and the prioritized next-target list"
metadata: 
  node_type: memory
  type: project
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-22T03:19:53.466Z
---

Sweep 6 = `/mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/sweep6/pdfium-diff-out/results.csv`
(100,205 pages, all 3 corpus roots — first sweep to cover hayro-tests + to-sort). Binary: HEAD=8d8ff53
with system fonts ON. Prior context: [[sweep5-analysis]] and SWEEP6-HANDOFF.md (both on the Windows
C: mount, `/media/dk/<vol-id>/Users/dk/.claude/projects/D--Rust-projects-pdfium-port-plan/memory/`;
vol-id was 4668959A68958977 this boot). Windows D: drive == `/mnt/Samsung980_1TB` (same disk).

**Sweep-5 fixes all confirmed in real sweep data, zero regressions.** Overall: mean inkΔ
0.00991→0.00719, p99 0.0526→0.0451, >0.20 448→319, >0.5 425→262. On the 79,420 common pages:
>0.20 went 55→49; worst "regression" is +0.022 (system-font baseline shift, not a bug).
Confirmed fixed in-sweep: Pearson/Gladstone Indexed-Lab (0.82→0.0000), Bannockburn (0.43→0.008),
Hongzhou (0.39→0.001), Rotberg (0.18→0.003), CJK cluster (~0.14→~0.012; Linux Noto CJK worked).

**Residual tail (>0.10, 432 pages) decomposes almost entirely into codecs + parser robustness:**
- `pdfium open failed` 126 pgs — oracle can't open; not our bug, exclude from grading.
- **JPX (jp2lam)** — dominant codec residual: ~60% of silent-blank(codec) (144 pgs/76 files) and
  ~85% of degraded(codec) (147 pgs/95 files) contain JPXDecode. Real books affected (Hobsbawm ×2,
  Abulafia, Northrup, Edgerton). Small fixtures: issue4648, issue11004, 0031730, 42271676, issue12752.
- **JBIG2 (jbig2enc-rust)** — second codec residual, TWO failure modes: (a) blank (in silent-blank
  class), (b) NOISE emitted where pdfium decodes cleanly — the upstream-pdfjs `bitmap-*` fixture
  family (template3-customat, customat, tpgdon, refine-customat, trailing-7fff-stripped; ours~0.4
  vs ref 0.07) pinpoints generic-region decoding with custom AT pixels + TPGDON; also
  jbig2_file_header (renders 0.41 where ref=0 → PDF-embedded stream w/ file header should be
  rejected/handled), image_jbig2_3/4, jbig2_huffman_2 (huffman tables blank).
- **Our `open failed`** 23 files/56 pgs (full-page 1.0 misses): xref/startxref recovery needed —
  issue8614 + issue1536 have startxref beyond EOF (need xref reconstruction); issue7303 is
  Standard-encrypted (check crypt support); also issue51, issue12402, issue2098, issue1877,
  issue15577, pdfbox/3948, bug1130815.
- **`compile failed`** 14 files/19 pgs: 0380221 (whole doc), close-path-bug, lopdf_issue_449_1/2.
- **Structural render diffs (no note, ink>0.10 & gross>0.05): only 74 pgs/60 files.** Top singles:
  issue11230 (CCITT, blank 0.98), flate_predictor_bpc_1 (blank 0.73 — predictor+bpc1 STILL broken
  despite 7fc70c1), pdfbox/5302 (OVER 0.85 vs 0.15), issue8565 (UNDER 0.28 vs 0.82), image_inline_5
  (OVER), bug1077808 (UNDER), issue18466 (OVER), issue968, pdfbox/1416, issue6707, EN-05-10137
  (6 pgs blank regions), What-Was-Socialism pp51-255 (5 pgs blank, DeviceGray Flate — sweep5 §3
  suspect), image_lab/image_lab_2 (ours 1.0 vs ref 0.87 — Lab-adjacent, check vs 8d8ff53).
- **Tone/threshold-metric class** (ink>0.05 but gross<0.05): 66 pgs/49 files — the known ~3–6%
  grayscale tone gap inflated by the THRESHOLD ink metric (Breen, PhRMA, Maloba, trading books).
  Workstream-H minification/resampling lead still open.
- 5 files excised by the controller (perf bugs: 180s timeout or SIGKILL/OOM): designated_basinmap,
  pdfium/467170761, pdfium/373764900, bug1019475_1, "to-sort/Getting started.pdf".

**DONE 2026-07-21 — target #1 JBIG2 generic-region customat/TPGDON (in jbig2enc-rust, uncommitted,
on main w/ unrelated pending changes).** Two root causes, both reproduced through the crate directly
and verified vs jbig2dec + end-to-end vs PDFium (6 fixtures 0.20–0.34 inkΔ → 0.0024):
(a) `SLTP_CTX_T0` in `src/decode/generic.rs` was 0xB325 (a self-consistent-but-wrong value); the
decoder's template-0 bit layout IS the pdf.js/spec sorted order, so the correct SLTP is the spec
0x9B25 (the encoder already had TPGD_CTX=0x9B25). The old oracle test passed only because its benign
images never mapped a real pixel to that slot. (b) `at_pixel` only sampled dy∈{0,-1,-2}, returning 0
for AT pixels reaching deeper (bitmap-customat AT dy=-5..-11) and template 3 passed a zero row for
y-2 — rewrote it to read the bitmap at any row. Files: generic.rs (SLTP + at_pixel + all callers),
tests/common/writer.rs (matching SLTP + generalized causal sampler), tests/decode_generic_templates.rs
(new oracle-checked `custom_at_deep_rows`). Fixes real scanned books (Hobsbawm ×2, Abulafia, Northrup,
Edgerton) whose pages were in silent-blank/noise. **STILL OPEN**: refinement custom-AT — GRTEMPLATE-0's
second AT pixel (GRAT2, reference-adaptive) is discarded ("only GRAT1 used" in text_region.rs:139 etc);
context_gr0 hardcodes it to (rx-1,ry-1). Only bitmap-refine-customat (1 synthetic pg, 0.19) hit; deeper
multi-site change needing its own oracle, deferred.

**DONE 2026-07-21 — target #2 xref recovery (in pdf-renderer/crates/pdf-structure/src/loader.rs,
uncommitted).** Root cause: `load_structure` only escalated to a full rebuild when the trailer was
missing/`/Root`-less or a *recovered* chain hit an empty page tree; a trailer that named a `/Root`
the xref couldn't resolve (stale/missing offset) looked complete and fell through, failing later at
document-open with "object N not found". Added `root_resolves(trailer)` (root present in xref as a
parsable offset dict, or in an object stream = trust) and escalate to rebuild when it fails — with a
snapshot/rollback (entries+revisions+recovery) so a rebuild that doesn't improve `/Root` resolution
is discarded (keeps the two ported PDFium xref-`/Index` fixtures green). rebuild() is objstm-aware so
compressed catalogs aren't lost; healthy files (incl. incremental updates) never trigger it. RESULT:
12 of 20 non-encrypted open-failed files now open+render within ~0.001–0.03 inkΔ of PDFium (issue12402
8pg, issue1536 54pg, issue2098 158pg, issue1877, issue8614, issue7303 [RC4 empty-pw decrypts],
bug1130815, 4279, 5260, 5992_1, issue67, issue18986). All 61 pdf-structure+pdf-document tests pass.
STILL-FAILING (heterogeneous deeper cases, left alone): 3948+0554304 (no xref at all, revisions=0,
/Root exists in-file but rebuild not firing — trailer isn't None; catalog lacks /Type/Catalog);
issue9418 (clean chain, catalog genuinely has no /Pages); issue51+bug1978317 (non-root object missing);
issue351+PDFBOX-4352-0 (bad object offset mid-parse); issue15577 (null catalog after rebuild); 2
AES-256 R6 password-required (auth-event-ef-open, encrypted-attachment — not our bug). Two recovered
files render wrong downstream (5260 blank, issue8614 over-inks to 0.95) — pre-existing renderer bugs on
newly-reachable content, both engines agree on page count so recovery is correct.

**DONE 2026-07-21 — target #3 first increment: JPX scalar-derived quantization (in
Lege-ecosystem/lege-codecs/jp2lam/src/j2k/decode_markers.rs, uncommitted).** `qntsty=1` (SIQNT,
Annex E.1.1) was rejected for irreversible 9/7 ("scalar-derived QCD is not implemented"). Fix: added
`expand_derived_steps` that materializes the single signaled NLLL step into the full per-subband list
(`εb=max(0, ε0-(b-1)/3)`, `μb=μ0`; matches OpenJPEG j2k.c:11387), applied to QCD + each QCC right after
COD is known, so downstream per-subband dequant is style-agnostic. Removed the reject; converted the
rejection test to a positive expansion test. VERIFIED: the repro decodes vs OpenJPEG at max abs diff 4 /
mean 0.13 (a tiled photo; baseline expounded 9/7 is max 1 — the delta is sub-perceptual float rounding,
NOT a quant scale error which would be large+localized), and END-TO-END the source PDF (Cosimo de'
Medici article) renders at inkΔ ~0.0015 vs PDFium on all 6 pages (was blank). Census: decoded 338→343,
scalar-derived signature cleared, no regressions; 269 jp2lam tests pass. OpenJPEG tools now installed
(opj_decompress/opj_dump on PATH — the mandated JPX oracle). Env note: `apt install libopenjp2-tools`
was needed; openjpeg source clone at /mnt/Samsung980_1TB/Rust-projects/clones/openjpeg.
**JPX residual (100 streams still fail, mostly out of scope this session):** biggest addressable class
is COC coding-style override (0xff53, 13 + 4 tile-part) but it needs threading per-component coding
params through the whole t1/t2/reconstruct/dwt pipeline (decoder currently reads `header.cod.X`
globally; only quant is per-component) — a big refactor. Others: sample precision <8-bit (16, bitonal),
JP2 color-spec method (12), palette channel mismatch (8), COD-decomp-levels/tile-part-tail/truncation
(the deliberate §2/§3/§4 traps in jp2lam/HANDOFF-remaining-jpx-decode.md — do NOT loosen). Census tool:
`JPXDUMPDIR=<dir> cargo run --release -p pdf-cli --example jpxcensus -- <list.txt>` from pdf-renderer;
list = jpxfiles.txt in this session's scratchpad (sweep-6 codec-flagged files containing JPXDecode).

**DONE 2026-07-21 — COC per-component transform (jp2lam, committed f5fb3aa; commits so far:
pdf-renderer 5293b87 xref; Lege-ecosystem 88c5548 jbig2, f3c0c8b jp2lam-scalar-derived, f5fb3aa
jp2lam-COC).** Main-header COC (0xff53) was rejected. Implemented per-component wavelet transform
(the CMYK "lossless K" pattern: colour comps 9/7, K comp 5/3): parse+reconcile COC keeping only the
transform (reject structural COC — levels/cblk/precincts — and reject a transform override on an MCT
colour comp, both cleanly); `transform_for(component)`; Tier-1 types each comp's coeff plane by its
own transform; reconstruct splits the MCT colour trio (uniform, shared RCT/ICT) from auxiliary comps
(K) across Image + packed-u8 paths. VERIFIED no-regression: sRGB byte-identical to OpenJPEG (max=1),
grayscale unchanged (max=4), census steady at 348, 296 tests pass, +3 COC tests.
**VERIFICATION BOUNDARY (important):** the corpus's transform-diff COC streams (e.g. 0411272 obj18,
4-comp CMYK, comp3→5/3) ALSO carry genuine per-tile QCC overrides (each tile has different quant
step sizes — confirmed by walking tile-part headers), which jp2lam does NOT support (single global
header, no per-tile resolution). So COC alone cannot decode them end-to-end; the transform-diff path
is correct-by-construction (routes each comp to the already-verified 9/7 or 5/3 primitive) but not
exercised end-to-end by the corpus. **NEXT natural JPX target = per-tile QCC/COC** (tile-part coding/
quant overrides), which needs a per-tile header stack — a larger change that would unblock the CMYK
per-tile-optimized streams (0411272 has 5 such streams; also the census "tile-part QCC" ×6 class).
Other remaining JPX classes unchanged from the census list below.

**DONE 2026-07-21 — per-tile QCC/QCD/COD/COC overrides (jp2lam, UNCOMMITTED on Lege-ecosystem main;
sits atop the earlier committed 88c5548/f3c0c8b/f5fb3aa).** Tile-part header override markers
(QCD 0xff5c, QCC 0xff5d, COC 0xff53, COD 0xff52) were a hard reject ("per-tile coding-style/quant
overrides are not supported"). KEY INSIGHT: the decoder is ALREADY per-tile — `decode_tile_components`
builds each tile's header as a geometry-only clone of the global `CodestreamHeader`, and Tier-1
dequant/reconstruct read quant per-component via `quant_for`/`transform_for`. So NO t1/t2/reconstruct
changes: just parse each tile's markers and merge onto the clone. Added `CodestreamHeader::with_tile_overrides`
(decode_markers.rs) reusing parse_qcd/qcc/coc/cod + expand_derived_steps + validate_quant; reconciles
tile COD (accept identical restatement, reject structural), tile COC (transform-only, MCT-colour guard),
merges QCD/QCC last-wins, re-validates. `parse_jp2_core` base is now main-header-only (+COM-count fold)
so tile 0 is handled uniformly; `decode_tile_components` calls with_tile_overrides before tile_local_header;
`tile_part_indices_by_tile` gate relaxed to allow the 4 markers. ALSO fixed a latent bug it exposed:
`validate_quant` checked each QCC against the GLOBAL COD transform (9/7) — wrong for a COC-overridden 5/3
K channel with NoQuant QCC; now validates against the component's EFFECTIVE transform (new
`effective_transform` helper; applied in both validate_decoder_scope main path AND with_tile_overrides).
+8 unit tests + 1 integration test (`tile_part_qcd_override_is_applied_end_to_end`). 281 lib tests pass,
workspace green. VERIFIED bit-exact vs OpenJPEG: (a) real corpus Helena Szepe obj10 (3-comp sRGB, 3
per-tile QCCs — turned out a benign RESTATEMENT of the default, now accepted vs previously rejected;
decode max=3 = inherent 9/7-ICT rounding on an aggressive thumbnail, control baseline=1); (b) DECISIVE
clean oracle: opj-encoded 2-tile RGB with a genuinely-different QCD injected into tile1 — jp2lam vs
OpenJPEG max abs diff=1 while tile1-vs-original=85 (override substantially changes decode) and tile0=0
(correctly scoped). Census over the 122-file sweep-6 JPX list: 443 streams, **the "tile-part QCC override
(0xff5d)" failure class is entirely GONE** (355 decoded / 88 failed; residuals are all OTHER classes).
**NEW BOUNDARY — 0411272 CMYK streams (obj18/21/24, ×8 "three sRGB components, found 4") are NOT unblocked
by per-tile QCC alone**: obj18 has THREE independent issues — (1) per-tile QCC on comps 0-2 [FIXED],
(2) main-header COC→5/3 + QCC-NoQuant on K comp3 [FIXED by the validate_quant transform fix], and (3) an
ORTHOGONAL container color-spec gate: its `colr` box declares EnumCS=16 (sRGB, 3ch) but ihdr/SIZ have 4
components + a `cdef` box (3 colour + 1 auxiliary channel). jp2lam's `validate_jp2_decode_scope`
(decode/mod.rs:~1446) rejects sRGB-colr + 4-comp. This is the census "JPN color specification method"
(×12) / "three sRGB components" (×8) class — a SEPARATE feature (cdef/channel-definition + colorspace
reconciliation), the real next JPX target if we want 0411272 to render. My earlier COC-boundary memory
note assumed per-tile QCC would unblock 0411272 — that was incomplete; the color-spec gate also blocks it.

**DONE 2026-07-21 — JPX sRGB + in-data alpha / `/SMaskInData` soft masks (COMMITTED: jp2lam
Lege-ecosystem `b00f608`; pdf-renderer pdfium-port-plan `148aff6`).** The "0411272 CMYK" streams
turned out NOT to be CMYK — `cdef` boxes prove they're sRGB(3)+opacity(1), i.e. RGBA. So the
"color-spec" follow-up was really an alpha/soft-mask feature spanning both crates (the COC→5/3 on
comp3 from the earlier COC work is the ALPHA plane, not a K channel). ALL 30 `/SMaskInData 1` alphas
in 0411272 are uniformly opaque (255) — an archival-scanner artifact — but the user chose the full,
correct-in-general feature over the minimal drop-alpha fix. jp2lam: parse `cdef`→`Jp2Header.alpha`
(+`DecodeMetadata.in_data_alpha`/`InDataAlpha`), accept sRGB+4 when cdef marks comp3 opacity, add
`DecodeOutputFormat::Rgba8` reconstructed via the CMYK aux-split (3 colour ICT + straight alpha),
channel count from the output format so Rgb8 drops alpha / Rgba8 keeps it; native `Image` path
reconstructs RGBA too. Renderer: `DecodedFormat::Rgba8`; `JpxCodec` requests it when metadata reports
alpha; `/SMaskInData` read in interpret→`SemImage`/`ImageIr.smask_in_data`; `prepared.rs` splits the
RGBA base into DeviceRGB samples + a grayscale `ImageSMask` (the existing per-pixel `sample_smask`
compositor path — NO compositor change). `/SMaskInData 2` (premultiplied) cleanly drops the draw
(un-premult unverified, deferred); `0` ignores alpha. VERIFIED bit-exact vs OpenJPEG: obj18 RGBA
RGB max abs diff 1 / alpha exact 0; synthetic gradient (non-opaque) alpha exact 0..255. END-TO-END
vs PDFium on 0411272: pages with these scans render at inkΔ <0.005 (were silent-blank ~0.2). 283
jp2lam lib tests + full pdf-renderer workspace green. **STILL BLANK in 0411272: pages 30/40 (obj24
class) — the §4 `TPsot==TNsot` tile-tail trap (deliberate strict-failure, orthogonal, see jp2lam
HANDOFF-remaining-jpx-decode.md §4).** Gated RGBA fixture `jp2lam/rgba_gradient_alpha_sample.jp2`
(gitignored) drives the local `srgb_alpha_decodes_to_rgba_with_gradient_mask` test.

**DONE 2026-07-21 — sYCC (EnumCS 18) colour space (COMMITTED: jp2lam `1a0567e`, renderer `cbd2b2b`).**
Added `reconstruct_ycbcr_image` + `sycc_to_rgb_in_place` (ITU-R BT.601 full range, chroma centred at
2^(prec-1)) = OpenJPEG's `sycc_to_rgb`; accept EnumCS 18 as `ColorSpace::YCbCr` (3 comps, reject MCT),
output DeviceRGB. VERIFIED: full-res RGB→sYCC→jp2lam round-trip max abs diff 1 + unit test. **BUT the
corpus sYCC scans (0210666 obj24 etc.) are 4:2:0 CHROMA-SUBSAMPLED (dx=dy=2), which jp2lam rejects —
so sYCC alone doesn't clear them; they need SUBSAMPLED-COMPONENT support (per-component geometry +
chroma upsampling through tile_rect/DWT/reconstruct — a large pipeline change, the real next step for
the sYCC/precision classes).**

**INVESTIGATED 2026-07-21 — tile-tail trap (§4) is NOT a quick win.** 0411272 obj24 (blanks pages 30/40)
is CONFORMANT (TPsot 0-5, TNsot=6) with a 1-byte tail, but tolerating the tail unmasks a real Tier-1
error ("code-block pass count 22 exceeds maximum 10 for 4 bit-planes") — jp2lam mis-reads tile 10's
packets where OpenJPEG succeeds. So obj24 is a genuine packet-parsing discrepancy, not benign padding;
a speculative small-tail tolerance was tried and REVERTED (no verified beneficiary, unmasks a worse
error). The §4 Prussian (TPsot==TNsot) is a separate assembly bug. Both need real packet/assembly
debugging vs OpenJPEG, not a loosened check.

**DONE 2026-07-22 — three more JPX gaps closed (jp2lam, COMMITTED on Lege-ecosystem main: 2b33e10
subsampled-sYCC, 5db4b21 inalign, a8c8b19 colr/palette; 292 lib tests, workspace green). Three
sub-agent tasks, run sequentially (all touch decode/mod.rs), each verified bit-exact vs OpenJPEG.
[Also committed 35866ed = a checkpoint of unrelated working-tree churn (ecosystem restructure +
musicsheet), binaries excluded; pdf-renderer bb77874 = SWEEP6-HANDOFF.md. The gitignored
jp2lam/HANDOFF-remaining-jpx-decode.md §4 diagnosis was corrected on disk (inalign, not assembly) but
is untracked so not committed.]**
- **Subsampled components (4:2:0 sYCC)** — relaxed the `decode_markers.rs:753` reject, NARROWLY gated
  to verified 4:2:0 sYCC only (3 comps, comp0 1:1, comps1/2 dx=dy=2, no MCT, single tile); everything
  else (4:2:2/4:4:0/mixed/MCT-coupled/multi-tile) still rejects cleanly. Per-component plane geometry
  via `CodestreamHeader::tile_component_bounds/dims` (Annex B.2 div_ceil) threaded through t1/t2/
  reconstruct; `upsample_chroma_nearest` (2×2 top-left replication = OpenJPEG `sycc420_to_rgb`);
  rewrote `sycc_to_rgb_in_place` to match OpenJPEG's integer `sycc_to_rgb` bit-for-bit. VERIFIED:
  0210666 obj24 (1920×1248, 9/7) max abs 3 / mean 0.032 vs opj (the 3 = pre-existing 9/7 float floor
  amplified by chroma gain, NOT geometry — G-channel cancellation confirms); END-TO-END went
  0.13350 degraded(codec)/blank → **inkΔ 0.00155, degraded=0**. THIS UNBLOCKS THE EARLIER-BUILT sYCC
  PATH on real corpus scans.
- **§4 TPsot==TNsot "tile-tail trap" — ROOT CAUSE WAS NOT ASSEMBLY (handoff §4 diagnosis was WRONG).**
  Proven: Prussian obj15 has 35 identically-structured tiles (6-part/TNsot=5) but ONLY tile 16 failed —
  assembly-misplacement can't explain that. Real cause: jp2lam's `PacketBioReader` was missing the
  terminating byte-realignment (Annex B.10.1 = OpenJPEG `opj_bio_inalign`): when a packet header's last
  consumed byte is 0xFF, the following bit-stuff byte belongs to the header and must be consumed before
  the body. Tile 16's R3C0 header ends 0xFF 00; jp2lam sliced the body one byte early and desynced.
  FIX (`t2.rs` only): `PacketBioReader::inalign()` (consume stuff byte when reg==0xFF, then byte-align)
  called after every packet-header parse. VERIFIED: naive tail-tolerance reproduced the documented
  256-row corruption (rows 768-1023 max 255, 75134 px >30); the fix gives that region max 3, 0 px >30;
  overall max 4 (MCT float floor), end-to-end all pages degraded=0, page16 inkΔ 0.00333.
- **§3 truncation-salvage — RESOLVED BY THE §4 FIX (no salvage code needed).** Both named repros
  (Profit-Over-People obj4, Imre Nagy) were NOT truncated — same inalign desync. Post-fix: obj4 (5/3)
  decodes bit-exact max 0. Genuine truncation KEPT STRICT (a raw-truncated stream makes OpenJPEG error
  too — so strict is correct per the discipline). Broad scan 400 PDFs/80 streams → 0 failures.
- **colr METH 3/4 + palette channel mismatch** (`jp2_parse.rs` only). METH 3 (any-ICC): read ICC
  data-space (bytes16..20) GRAY/RGB else infer from component count. METH 4 (vendor): infer from
  component count (1→Gray/3→Srgb/4→CMYK). 2-component METH 3/4 = genuinely ambiguous → REJECT CLEANLY
  (Graebar obj7604: 2-comp METH=4 no cdef; OpenJPEG itself ignores the colr box and emits
  uninterpretable channels, so nothing to match — clean B3 blank is correct). Palette mismatch: when a
  resolved pclr/cmap yields more channels than the (inconsistent) colr space declares, align the
  container space to the palette channel count (= OpenJPEG `opj_jp2_apply_pclr`). VERIFIED: Urinalysis
  obj6/19/26/30/65 (5/3) bit-exact max 0 vs opj 4-channel output; end-to-end inkΔ ≤0.006, degraded=0.

**Remaining JPX gaps (smaller now):** 16-bit sample precision; non-4:2:0 subsampling layouts
(4:2:2/4:4:0/mixed, multi-tile subsampled); genuine stream truncation (OpenJPEG errors too — strict is
correct); §2 degenerate 1-sample DWT (per HANDOFF 2026-07-19 update this was implemented — verify
before re-triaging). The tile-part packet-parsing class and colr-method/palette classes are now
CLEARED. See jp2lam/HANDOFF-remaining-jpx-decode.md (its §4 assembly-bug diagnosis is now known WRONG —
the real fix was inalign).

**Remaining priority order:** 1) (was JBIG2 generic — DONE above); 2) xref-recovery/open-failed robustness (23 files,
full-page wins); 3) JPX blank/degraded classes in jp2lam (largest page count, separate crate, see its
HANDOFF-remaining-jpx-decode.md); 4) singleton render bugs above (flate_predictor_bpc_1 first — it
contradicts a claimed fix); 5) tone/minification pass (workstream H) last — subtle, metric-inflated.
Settled non-bugs (do NOT re-triage): Daughters of Tunis, Aksan/Ottomans p0 (pdfium wrong, we're right).
