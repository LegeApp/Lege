---
name: adjudicating-pdfium-disagreements
description: "How to decide who is right when our render disagrees with PDFium: cross-check in Firefox/pdf.js, and treat poppler-based viewers as one vote"
metadata: 
  node_type: memory
  type: feedback
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-22T12:06:27.731Z
---

When a corpus page disagrees with PDFium and it is not obvious which side is correct, cross-check
against the other renderers. **Four controls are available, and the user's standing instruction is
that none may be assumed correct and none may be assumed to agree:**

| Control | How to run |
|---|---|
| **pdf.js** | Firefox. Headless screenshotting fights a running instance; asking the user is faster. |
| **hayro** | **now at `pdf-renderer/tools/hayro/`** (moved 2026-07-22; do NOT use `renderer-corpus/hayro-source/`, that is the corpus the sweep reads), `cargo build --release --manifest-path pdf-renderer/tools/hayro/Cargo.toml --example render`, then `target/release/examples/render <file.pdf> <outdir> <scale>` -> `rendered_<n>.png`. Builds in ~22 s. Integration notes for the multi-renderer harness: `tools/HAYRO-INTEGRATION.md`. **This is the corpus's own upstream**, so its regression PDFs are exactly the ones it is tuned for. |
| **PDFium** | the sweep oracle. |
| **poppler** | Okular, qpdfview — **these two are one vote, not two.** Source at `tools/poppler-26.07.0/`. |
| **mupdf / ghostscript** | `tools/mupdf/`, `tools/ghostscript-10.07.1/` — added 2026-07-22 by another agent, who is building a 5-renderer harness. |

**Counting votes does not settle anything.** On `issue6707`, PDFium *and* hayro *and* poppler all
painted a grey band that pdf.js and we leave light beige — three "independent" renderers agreeing.
They were all wrong: the file has a malformed `/Separation` whose alternate-space slot repeats the
colorant name, and all three fall back to using the tint as DeviceGray (grey 77 = 0.3 x 255). The
question was settled by **evaluating the tint transform by hand** and finding our pixels matched its
output exactly. Adding hayro genuinely changed the reading mid-investigation — it is worth running —
but the decisive step was arithmetic, not a majority.

**So: use the controls to find *where* to look, then compute the expected answer from the file.**

**How to apply:**
- Firefox and Mercury are installed on this machine. Headless screenshotting fights an
  already-running Firefox ("Firefox is already running, but is not responding") — a separate
  `--profile` plus `--no-remote` is the workaround, but **just asking the user for a screenshot is
  faster and they have offered**.
- **Okular and qpdfview are both poppler-based, so they are one vote, not two.** On `issue6707`
  they reproduced PDFium's spurious grey block; that agreement is not evidence.
- Look for PDFium *omitting* content as well as adding it. On `issue6707` it dropped the header
  photo and logo entirely while also painting a block that does not exist — two independent errors
  on one page.
- Once adjudicated, record it as a settled non-bug with the evidence so it is never re-triaged.
  Existing settled cases: Daughters of Tunis, Aksan/Ottomans p0, pdfbox/5260, pdfbox/5657 and
  issue16782, and now issue6707 (see [[post-sweep9-fixes]] and [[sweep7-analysis]]).

This matters more as the residual shrinks: the tail is increasingly made of pages where PDFium is
the one at fault, and chasing those actively makes the renderer worse.
