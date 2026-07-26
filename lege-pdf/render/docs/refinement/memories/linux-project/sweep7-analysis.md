---
name: sweep7-analysis
description: "Sweep 7 (2026-07-22, Linux, all 3 roots, 100,204 pages) — validates the 14 post-sweep-6 fixes corpus-wide with zero regressions; fresh residual decomposition + priority list"
metadata: 
  node_type: memory
  type: project
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-22T07:38:38.973Z
---

Sweep 7 = `/mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/sweep7/pdfium-diff-out/results.csv`
(100,204 rows, 19,449 files, all 3 roots, scale 2.0). Command (from
`pdf-renderer/tools/pdfium-diff`): `./run-sweep.sh '' 2.0 <fresh-workdir>`. It is **resumable** by
`file|page` key — sweep 7 was interrupted at 16,832/19,449 and finished by re-running the identical
command against the same workdir. Runtime on Linux ≈ 25 min with 10 workers, not the "hours" the
sweep-6 handoff predicted. Binary = renderer `bb77874` + jp2lam path-dep `a8c8b19`.
Predecessor: [[sweep6-analysis]].

**VERDICT: all ~14 post-sweep-6 fixes confirmed corpus-wide, ZERO regressions.** On the 100,202
common pages: mean inkΔ 0.00718→0.00670, p99 0.0451→0.0431, >0.10 431→366, >0.20 318→264,
>0.5 261→216. Note classes: silent-blank(codec) 144→123, degraded(codec) 147→118, our
`open failed` 56→21. Nothing moved the wrong way — importantly **`inalign` (5db4b21), which runs on
every packet header of every JPX stream, and the xref rebuild escalation (5293b87), which runs at
every document open, are both clean at corpus scale.**

**Only 2 pages crossed 0.10 upward, and both are FIXES, not regressions** — `pdfbox/5657.pdf` p0 and
its duplicate `issue16782.pdf` p0, 0.0000(silent-blank) → 0.2068. **PDFium renders these pages
completely blank** (`ref_ink=0.00000`); we now render them correctly. Verified: 1258×711 4-comp JP2,
`cdef` opacity + `/SMaskInData 1`, and unlike every alpha in 0411272 this one is **genuinely
non-opaque** (326,557 of 894,438 px at alpha=0, full 0–255 gradient) — the first real in-data alpha
in the corpus, so the soft-mask feature is now exercised end-to-end on live data. jp2lam RGB is
**bit-exact vs OpenJPEG (max 0 all channels)**; our rendered page vs the OpenJPEG RGBA composited
over white is mean 0.173/255 with 4 px of 894k off by >8 (that residual is downscale resampling).
**SETTLED NON-BUG — do not re-triage; PDFium is wrong here** (same class as Daughters of Tunis).

**Sweep-7 residual tail (367 pgs >0.10 / 294 files):**
- `pdfium open failed` 126 pgs — oracle can't open; exclude from grading.
- codec-noted 241 pgs/142 files. By filter: JPX-containing 95 files, JBIG2-containing 48, CCITT 20.
- our `open failed` 21 pgs (was 56) — 11 files: 0554304, 3948, issue51, issue15577, issue9418 (×2
  entries), issue351, PDFBOX-4352-0, bug1978317, + auth-event-ef-open & encrypted-attachment
  (AES-256 R6 password-required, NOT our bug). Same heterogeneous set sweep 6 left open.
- `compile failed` 18 pgs/12 files: 0380221 (6 pgs), close-path-bug (2), 3977, 42271520, 42270389,
  42270730, lopdf_issue_449_1/2, issue9540, 42271379, issue17554, operator_list_cycle.
- **structural render diffs (no note, ink>0.10 & gross>0.05): 68 pgs/53 files** — essentially
  unchanged from sweep 6's 74, i.e. NOTHING in this class was addressed since sweep 6. Top live
  singles: issue11230 (CCITT, blank 0.98), flate_predictor_bpc_1 (blank 0.725 — STILL broken,
  contradicts the 7fc70c1 claim), 5302 (OVER 0.85 vs 0.15), issue8565 (UNDER 0.28 vs 0.82),
  image_inline_5 (OVER 0.53 vs 0.06), bug1077808 (UNDER), jbig2_file_header (0.41 vs ref 0),
  issue18466, issue968, 1416, issue6707, issue9462, resvg_masking_mask_recursive_on_self, 5250_1,
  6024, 3000_9, issue50. Two xref-recovery downstream bugs persist but improved: 5260 1.0000→0.9253
  (still near-blank), issue8614 1.0000→0.88 (still over-inks, ours 0.95 vs ref 0.07).
  Daughters of Tunis (5 pgs, 0.75–0.95) and From_Central_Asia are settled non-bugs.
- tone/threshold-metric class (ink>0.05, gross<0.05): 68 pgs/51 files — unchanged, workstream H.
- infra: 3 SIGKILL, 2 timeouts, 13 terminally skipped, 2 worker failures.

**FRESH JPX CENSUS (95 sweep-7 codec-flagged JPX files): 354 streams, 293 decoded, 61 failed.**
Classes, by stream count — this is the current jp2lam target list:
1. **16 — non-8-bit sample precision.** Representative dump is **1-bit bitonal** (numcomps=1,
   prec=1, sgnd=0), not 16-bit; the class also contains 16-bit. Biggest single win available.
2. **12 — ambiguous 2-component colr METH 3/4** (Graebar obj7604). **CORRECTLY REJECTED, not a
   gap** — OpenJPEG itself ignores such colr boxes and emits uninterpretable channels, so there is
   nothing to match. Do NOT "fix" by guessing a colour space.
3. **7 — `COD decomposition levels N exceed DWT limit`** — this is HANDOFF §2 degenerate 1-sample
   DWT. **It is STILL OPEN**: the handoff's 2026-07-19 "Sections 1 and 2 are implemented" note is
   wrong about §2. Repro Howard Turner *Science in Medieval Islam* obj7. Fix the 1-sample lifting
   in `src/dwt/` FIRST, then relax the bound — relaxing alone gives ~16/255 drift (§5 trap 1).
4. **5 — `tile-part payload extended past codestream`** (0031730 obj7) — genuine truncation;
   OpenJPEG errors too, strict is correct.
5. **5 — COC overriding more than the wavelet transform** (issue11004 obj1) — structural COC
   (levels/cblk/precincts per component); needs per-component coding params through t1/t2/dwt.
6. **4 — unsupported component layout / N components** (issue19326 obj8) — e.g. gray+alpha; the
   deferred `GrayA8` case from the SMaskInData plan.
7. 3 — non-8-bit or signed palette entries (4328 obj6).
8. 2 — truncated JP2 box header (0357865 obj3); 2 — more than one cdef opacity channel
   (issue11306 obj147).
9. Singles: marker past end of codestream (42271618 obj25), palette-vs-Gray channel mismatch
   (Uyghurs obj1548), trailing bytes after EOC (bug1037816 obj6), CRG component registration
   (0518325 obj7), CMYK with wrong component count (0041790 obj1220).
**The 4:2:0 subsampling, tile-part packet-parsing/inalign, colr METH 3/4-resolvable, per-tile
QCC and cdef-alpha classes are all GONE from the census.**

**Recommended priority order after sweep 7:**
1. `flate_predictor_bpc_1` — self-contained, contradicts a claimed fix, no sweep data needed.
2. JPX non-8-bit precision (16 streams) — largest addressable codec class.
3. JPX §2 degenerate 1-sample DWT (7 streams) — known-scoped, do the DWT before the bound.
4. The 68-page structural render class (nothing there has been touched since sweep 5) —
   issue11230 CCITT blank and 5302/image_inline_5 over-ink are the fattest singles.
5. JBIG2 residual (48 files) incl. jbig2_file_header and the still-open refinement custom-AT.
6. tone/minification (workstream H) last.
