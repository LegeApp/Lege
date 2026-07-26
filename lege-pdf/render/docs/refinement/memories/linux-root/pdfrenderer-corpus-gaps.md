---
name: pdfrenderer-corpus-gaps
description: "pdf-renderer's ranked rendering gaps from the 11k-PDF differential oracle sweep"
metadata: 
  node_type: memory
  type: project
  originSessionId: f965981c-6368-4733-8894-47d9d033901f
---

The pdf-renderer differential oracle (`tools/pdfium-diff/`, dlopens
`libpdfium.so`) was run over the full 11,206-PDF corpus (`/mnt/Samsung980_1TB/to-sort/`
and `/mnt/Samsung980_1TB/Pol was right again/`) on the current build. Ranked
remaining gaps, by distinct-document cause:

The top-3 ranked gaps are all **RESOLVED** as of 2026-07-18 (validated vs the
PDFium oracle):
1. **Encryption** (138 docs / 819 pages) — **DONE**. Standard handler wired
   into pdf-document (`build_security` parses `/Encrypt`; `resolve.rs`
   decrypts strings + stream bodies with the correct exemptions). RC4-128 +
   AES-128 corpus files render pixel-matching PDFium (mean ink_delta ≤0.005).
   **Root-caused a latent infinite loop in `pdf-security` MD5 `update()`** (it
   clobbered `buf_len` with the sub-block remainder, so `finish()`'s pad loop
   spun forever) — THIS, not just D-state, is why the crypto tests "couldn't
   run." Fixed + corrected a wrong 56-byte MD5 test constant.
2. **`/LZWDecode`** — **DONE**. Native 9→12-bit MSB-first decoder in
   `pdf-structure/decode.rs`, `/EarlyChange` honored, predictor via
   `apply_parms`. Matches PDFium on 3 real Adobe LZW PDFs.
3. **Truncated flate** — **was already done** since Phase 1 (`inflate_with`
   salvages the inflated prefix); the earlier "we error and drop" note was
   inaccurate. Now regression-tested.

4. **Zero-page tree after recovery** — **DONE** (2026-07-18). Linearized/
   incremental file with a stub first revision (`Pages Count 0/Kids[]`) and an
   unfindable trailing startxref: recovery landed on the stub xref → 0 pages.
   `load_structure` now detects a degenerate page tree on the *recovery path
   only* (`page_tree_looks_empty`, gated on `used_recovery`) and escalates to a
   full `XrefRebuilt`. Hoyos *Hannibal's Dynasty* went 0 → 739 pages, matching
   PDFium (max inkΔ 0.022). The `compile failed` corpus class is heterogeneous,
   mostly NOT flate — several sampled files already open cleanly now.

Gotcha learned: `tools/pdfium-diff` links the pdf-renderer crates statically,
so after any engine fix you MUST rebuild the diff binary before trusting its
numbers — a stale binary reported all these pages blank/"compile failed".

**Sweep 3 (2026-07-20, 14,656 files incl. new /mnt/Samsung980_1TB/PDF root,
84,473 pages):** ZERO our-side failures (all 14 terminal rows are PDFium's
own open/load errors); blanks 610→8; 1,740 fixed vs 12 regressed. The sweep
tool now hardens unattended runs (worker-per-file + PDFIUM_DIFF_TIMEOUT kill,
`--rerun-failures` inner loop, degraded/silent-blank CSV columns). Remaining
attack plan is `pdf-renderer/PLAN-POST-SWEEP3.md`: (R) 3 cover regressions —
Separation/Indexed cover blanked by the tint-LUT path WITHOUT a degraded
tick (tracking hole), 2 transparency covers (one solid-black flood);
(B1) DCT `unsupported JPEG component count: 2` — East Asia textbook cluster;
(L) 1,808-page lighter class: CJK predefined CMaps (user decision pending on
bundling tables), transparency /BC+/TR+knockout, scan-AA thinness (sample
triptychs before building); (H) 178-page heavier class = known stencil-AA
boldness, optimization phase.

**Re-run the full oracle sweep** to re-rank; encryption (~819 pages) + LZW +
flate salvage + this recovery fix have all moved pages. See
[[pdfrenderer-native-first]] and DEFERRED.md.

JBIG2 is no longer a gap: the interim `hayro-jbig2` was replaced by the
in-house decoder (2026-07-18, see [[pdfrenderer-native-first]]). Residual
JBIG2 risk is narrow — Huffman symbol dicts (`SDHUFF=1`), refinement/aggregate
symbol coding (`SDREFAGG=1`), one Huffman halftone variant, and random-access
organisation still surface as typed `Unsupported` (page dropped, no panic, not
a blank-render regression). Re-run the oracle over JBIG2 docs to quantify.

The sweep's CSV (`tools/pdfium-diff/pdfium-diff-out/results.csv`) is the
artifact; the `note` column already carries the cause ("open failed",
"compile failed"). Triptych PPM dumps are opt-in (`PDFIUM_DIFF_DUMP=1`) — off
by default because 33k suspect pages at ~47MB would fill the disk and nobody
eyeballs them. Re-run after encryption wiring lands (~819 pages will move).

Environment gotcha: this machine runs up to 4 concurrent Claude Code sessions;
when several `cargo` invocations overlap, test *binaries* stall in
uninterruptible I/O (D-state) and even SIGKILL won't reap them. `cargo build`
still completes; only test *execution* is affected. Verify logic in isolated
`/tmp` crates (tmpfs) when the workspace test runner stalls.
