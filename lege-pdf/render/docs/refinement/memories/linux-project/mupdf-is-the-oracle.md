---
name: mupdf-is-the-oracle
description: "As of 2026-07-23 MuPDF, not PDFium, is the primary rendering oracle — it has the broadest coverage, and one control cuts sweep heat to a third."
metadata: 
  node_type: memory
  type: feedback
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-23T07:09:46.868Z
---

The user decided on 2026-07-23 that **MuPDF is the primary oracle/control**, not
PDFium, "even if it is slightly slower — it seems to have better rendering
coverage."

**Why:** broader rendering coverage than PDFium in practice, so it is the better
target to measure against. It also serves the thermal constraint: one control
instead of three cuts a sweep's reference-render work to roughly a third, and
this laptop has no full fan control under Linux.

**How to apply:** `run-sweep-sharded.sh` now defaults to `REFERENCES=mupdf`.
Report ink deltas against mupdf first; bring in pdfium and hayro
(`REFERENCES=mupdf,pdfium,hayro`) only when triaging a specific disagreement
that needs adjudicating — see [[adjudicating-pdfium-disagreements]], whose
voting logic still applies but with mupdf as the anchor rather than PDFium.

**Caveat worth remembering:** this repo is a *PDFium port*, and some behaviour
was deliberately chosen for PDFium parity — the sub-8-bit widening decision in
[[sweep7-followup-fixes]] is explicitly one. Those calls are now judged against
a different target and a few may invert. The user was told this and chose to
proceed; past parity decisions were not re-litigated, so revisit them only if
asked.
