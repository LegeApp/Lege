---
name: sweep12-harness-and-jpx-psot
description: "Sweep-12 harness hardening plus three render fixes (JPX Psot, §11.6.6 group soft mask, /Indexed over a palettized JPX)"
metadata: 
  node_type: memory
  type: project
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-23T02:01:22.279Z
---

Work of 2026-07-22/23, after sweep 11 was stopped early.

**Sweep-12 harness fixes** (pdf-renderer `5dcffb3`, `tools/pdfium-diff/`):
1. Error rows write empty metrics, not `ink=1, gross=1` — the old form made a
   crashed control look like total disagreement and sorted it to the top of
   every "worst pages" list.
2. `render_pages_resilient` retries a failed batch page-by-page.
3. `dimension-mismatch` now needs >1px. hayro floors what we round; sweep 11
   mislabelled 47,352 clean pages on ±1 alone.
4. Shard runner dropped `xargs` (it stops dead on a child SIGKILL — an OOM
   silently abandoned 1,430 files). Batches now run from bash with a
   per-invocation `timeout`, plus an EXCLUDE list. Sweep 11 also lost a shard to
   a *hang* (`bug1019475_1.pdf`, 97% CPU for 8.5 h) that the tool's per-page
   timeout does not catch.

**Three render fixes, each found by being the sole outlier vs pdfium+mupdf+hayro:**
- `0031730` (jp2lam `e4f3289`): *not* truncation — the last tile-part declares
  `Psot=12`, under the 14-byte minimum. Recovered by scanning for the next
  SOT/EOC, which is exact because Annex B bit stuffing forbids `0xFF90`/`0xFFD9`
  inside packet data. 18/18 comparisons 0.75-0.84 → 0.0000.
- `0041790` p50 (pdf-renderer `765be07`): §11.6.6 has two halves and we had one.
  The soft mask modulates a transparency group's *composite*, and inside the
  group it resets to None. Both must land together or a group whose content
  doesn't clear the mask gets it twice. 0.886 → 0.015.
- `Tirpitz` p0 (jp2lam `a0679f9` + pdf-renderer `88174fd`): the JP2 carries its
  own `pclr`/`cmap`; §7.4.9 makes the PDF `/Indexed` space authoritative, so the
  single component is an index into the *PDF* palette. Expanding the container
  palette produced 3 channels that looked like RGB but were DeviceN inks, and
  the image was dropped. New `DecodeRequest::ignore_container_palette`. 0.68 →
  0.003.

**Method note that paid off twice:** a large ink delta with matching geometry is
a colour bug. On `0041790` the ours/PDFium ratio was a uniform 0.26 on every
channel, which matched Multiply-at-α-0.75 arithmetic exactly and identified the
op before any code was read.

**254-file regression sweep** (`regress-runs/` on the data drive — /tmp gets
wiped mid-session): 144 pairs improved, 0 regressed.

See [[sweep6-analysis]], [[adjudicating-pdfium-disagreements]].
