---
name: sweep12-tail-jbig2-cmyk
description: "2026-07-24 closed the last 2 sweep-12 residuals — JBIG2 custom-AT refinement (committed) and the Huser CMYK-ICC class (validated, uncommitted)"
metadata: 
  node_type: memory
  type: project
  originSessionId: 807e0f39-4652-4c65-b926-8f8c4454231e
  modified: 2026-07-24T01:05:59.368Z
---

2026-07-24 session: closed the last two known sweep-12 residuals before a possible sweep 13.

**1. JBIG2 custom-AT refinement — DONE + COMMITTED.** In `Lege-ecosystem/lege-codecs/jbig2enc-rust`
(commit `027a485` on branch `sweep12-codec-fixes`). A prior agent had fixed the standalone
refinement region (GRAT2 reference-adaptive pixel → context bit 0; GRTEMPLATE-1 SLTP context
0x040→0x080) but left the symbol-dictionary (SDRAT) and text-region (SBRAT) paths parsing the
second AT pair and discarding it, hardcoding `(-1,-1)`. I threaded `grat2`/`sdrat2` through
`decode_refagg_symbol`, the Huffman refagg path, and the arith/Huffman text-region cores
(`TextArithParams`/`HuffTextParams` gained a `grat2` field). Un-blanked
`bitmap-symbol-symbolrefineone-customat` and `bitmap-symbol-symhuffrefine-textrefine`; all 96
pdf.js `bitmap-*` fixtures now match pdfium (Δ≈0.0024), 0 silent-blanks. Regression test
`context_gr0_reads_the_custom_grat2_reference_pixel` (runs under plain `decode` feature).
Verified against `pdfium.dll`. The 6 `roundtrip_after_fix.rs` failures are a missing `jbig2dec`
binary, pre-existing.

**2. Huser / CMYK-ICC class — DONE + VALIDATED, but UNCOMMITTED.** New `pdf_color::icc::IccCmyk`
evaluates an `A2B0` `mft2`/`mft1` CMYK→Lab lut (input curves → 4-D n-linear CLUT → output curves
→ v2 Lab(D50) → Bradford D50→D65 → sRGB), matching PDFium's LittleCMS path (INTENT_PERCEPTUAL,
no BPC). Validated vs Pillow/lcms2 on the real Huser profile: mean 1.2 units/channel, 96.75%
within ±4. Wired in `pdf-content/interpret.rs`: `/DefaultCMYK` redirect for the `k`/`K` operators
and `/DeviceCMYK cs`; direct `[/ICCBased]` CMYK `scn`; and — the actual cover fix — a
`TintSpace.alt_icc_cmyk` field so a Separation/DeviceN whose **alternate** is an ICCBased CMYK
profile (PANTONE spots over the press profile) routes its tint output through it. Spot-color
IMAGES benefit for free (the tint image LUTs call `eval_tint`). Herbert Huser cover p0:
**inkΔ 0.15837 → 0.00390**; whole book ≤0.005. No regressions across 469 pdf.js pages
(mean inkΔ 0.0041) or 180 CMYK-corpus pages.

**Why uncommitted:** `crates/pdf-content/src/interpret.rs` holds the user's pre-existing
text-extraction WIP (tagged PLAN-TEXT-EXTRACTION §5.2, `gid_to_char` etc.) that the handoff said
to keep out of commits. My colorspace wiring is entangled in the same file, so I left the whole
change (`icc.rs` + `interpret.rs`) in the working tree for the user to review/commit, rather than
risk the WIP with hunk surgery. `pdf-color`+`pdf-content` = 194 tests green. Remaining
architectural residuals unchanged (JBIG2 spec-edge rejections, JPX residual classes, WGPU,
ICC CMM for RGB/gray). See [[sweep5-analysis]] and [[pdf-renderer-state-pointer]].
