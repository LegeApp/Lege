# tools/

Out-of-workspace tooling (roadmap Phase 0):

- `reference-render/` (planned): drives prebuilt PDFium (via `pdfium-render`
  or the C API directly) to produce reference page rasters + metadata
  (dimensions, flags, PDFium version, timing, failure status) for corpus
  files. Kept out of the main workspace so the engine never links PDFium.

## Differential controls

Third-party renderers kept here as independent controls for the differential
harness. None is authoritative — see `HAYRO-INTEGRATION.md` §6 for why counting
agreement between them does not establish correctness.

- `hayro/` — Rust renderer, dual Apache-2.0/MIT. **Build and usage:
  `HAYRO-INTEGRATION.md`.** Note this corpus is hayro's own test suite, so it is
  tuned on the pages we grade; that is a bias, not extra authority.
- `mupdf/`, `poppler-26.07.0/`, `ghostscript-10.07.1/`, `pdf.js/` — added
  alongside. Okular and qpdfview are both poppler front-ends: one engine, one
  vote.
- `pdfium-diff/` — the existing sweep oracle driver.

**`ATTRIBUTION-HANDOFF.md`** — per-pixel attribution of difference pixels to PDF
object categories (Image XObject / Form XObject / annotation / text /
path-shading / unpainted). The vocabulary and plumbing landed in `cfb637f` and
are inert; that document specifies the remaining work and how to attach it to
the per-renderer difference pipeline.

These are checkouts with their own VCS history, never vendored. `tools/*/target/`
is gitignored; the checkout directories themselves should be too.
