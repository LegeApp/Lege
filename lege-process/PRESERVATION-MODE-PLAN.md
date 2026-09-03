# Plan: preservation mode beside e-ink preparation

Status: proposed, not implemented. Written 2026-09-03, after truetyping was
promoted to the default PDF text format.

Not to be confused with `lege-document-ocr/PRESERVE-MODE-PLAN.md`, which is
about the business OCR CLI keeping a source PDF's own page images byte for
byte. This plan is about `lege-process` producing a *new* MRC document from a
book, at a requested resolution, without binarizing or recolouring it.

## 1. The two modes

Everything Lege does today is one mode, even though it has never been named:

**Preparation.** Make the book readable and small on an e-ink screen. Render
the page, detect the layout, binarize the text, dither or drop the pictures,
throw the colour away, and re-encode at the device's pixel height. The
output is a reading copy: a fifth to a twentieth of the input, and no longer
the book.

**Preservation** (proposed). Keep the book, but compressed and searchable.
The page is not binarized and its colour is not touched. The ink is
*separated* from the paper so each can be compressed on its own terms —
a heavily compressed continuous-tone background under a crisp ink layer,
which is what MRC (mixed raster content) is for — and the output is a PDF
with a JPEG 2000 background and a JBIG2 ink layer, or the DjVu equivalent
(IW44 background, JB2 mask). hOCR still runs, so the result is searchable.

The two modes share the renderer, the layout model, the OCR stack and the
writers. They differ in what happens between the render and the encode.

## 2. What preservation mode produces

For each page, at the requested resolution:

- a **background**: the page as scanned, in its own colour space (RGB or
  grey), with the ink filled in toward the paper so the wavelet coder has
  nothing sharp to spend bits on; subsampled 2-4x, JPEG 2000 (PDF) or IW44
  (DjVu).
- an **ink layer**: the text and line art at full resolution, as a JBIG2
  stencil (PDF) or a JB2 mask (DjVu). Optionally — never by default — as
  truetyping instead, since that replaces the ink with vector glyphs and is
  therefore a transformation, not a preservation.
- **detected image regions** left as continuous tone, in colour, at the
  requested resolution; no dithering, no halftoning, no palette.
- an **invisible hOCR text layer**, exactly as today.

What it must not do: convert to luma, stretch the white and black points,
apply gamma, floor near-white to paper white, quantize levels, dither, or
threshold the page as a whole. Those are `clean_gray_page`'s job in
preparation mode and they are all one-way.

## 3. What already exists

| Piece | Where | State |
|---|---|---|
| MRC page encode (grey) | `pipeline/pdf_tokio_pipeline.rs:1711` `encode_mrc_base_layer` | Works for any page; JBIG2 Generic ink over a grey JP2 background. Colour is gone before it is called. |
| MRC page encode (source-preserving) | `pipeline/pdf_tokio_pipeline.rs:1849` `encode_preserved_mrc_base_layer` | Re-encodes only the background and passes the source's own JBIG2 mask through untouched. Colour-capable in its `ImageRegionDitherMode::None` branch. |
| The gate on that path | `pipeline/pdf_tokio_pipeline.rs:2869` `preserved_jbig2_smask` | Fires only when the *input* PDF is already MRC: one full-page image whose `/SMask` is itself JBIG2 (`lege-pdf/read/src/session.rs:736` `qualifying_jbig2_smask`). Nothing synthesizes that shape for an ordinary scan. |
| Ink-aware background fill | `core/clean_gray.rs:630` `mrc_background_with_coverage` | Takes a continuous 0-255 coverage plane and whitens in proportion. Single channel only. |
| Exact area resampling | `lege-pdf/read/src/session.rs` `coverage_spans` / `resize_smask_coverage` | Turns a full-resolution mask into an exact-area coverage plane at any smaller size. Written for the decoded source mask; reusable for a derived one. |
| Binarization | `color/binarization.rs`, `lege-gpu/src/binarization/` | Sauvola on GPU and CPU. Emits bilevel, not coverage. |
| Layout | `pipeline/page_analysis.rs` `classify_page`, `core/types.rs` `ContentCategory` | Already separates text, image, table and abandon regions. |
| PDF layering | `lege-pdf/write/src/images.rs` | JPX (RGB or grey) plus JBIG2 `/ImageMask` stencils, stacked as two `Do` operators. No `/SMask`, no `/Mask` on an image. |
| DjVu layering | `lege-codecs/djvulibrust/src/doc/builder.rs:58` `LayerData` | `Background(Pixmap)` (RGB or grey, IW44), `Foreground(Bitmap)`, `Mask(Bitmap)`. |
| DjVu composition | `core/djvu.rs:234` `compose_no_binarization_page`, `:255` `compose_normal_page`, `:449` `compose_color_canvas` | A full-page colour background exists (covers, `--no-binarization`), and a JB2 ink layer exists, but never together: the colour canvas is white everywhere except the detected image regions. |
| hOCR | `pipeline/pdf_tokio_pipeline.rs:1545` `perform_ocr`, writer actor in `pipeline/helper_functions.rs:1148` | On the default PP-OCR backend the recognizer already reads the RGB raster, not the bilevel mask, and the text travels to the writer on its own channel. Nothing to change. |
| Resolution | `pipeline/config.rs` `target_height`/`target_width`, `mrc_bg_subsample`, `mrc_bg_quality` | Already expresses "mask at the requested height, background subsampled beneath it". |
| Page orientation | `encoding/straighten.rs`, `core/pdf_artifact.rs` `display_rotation` | Sideways pages are already turned upright with `/Rotate`. Carries over unchanged. |

## 4. What is missing

**(a) A colour-preserving segmentation for an arbitrary page.** This is the
whole job, and everything else is plumbing. Two outputs are needed from one
render, without altering the page:

1. an ink **coverage** plane, 0-255, at the background's resolution;
2. the page itself, untouched, as the background before filling.

The coverage plane is the piece with no existing producer. Two candidates,
in order of preference:

- *Supersample the mask.* Binarize at 2x the output height (Sauvola already
  runs on the GPU) and area-average down with the existing `coverage_spans`
  machinery. This is exactly what the preserved path does with a decoded
  source mask, so the consumer is already written and already tuned (the
  coverage gain of 2 from pass 7 exists because a soft mask left grey
  halos).
- *Keep the Sauvola margin.* Instead of thresholding the local response to
  0/255, keep the signed distance to the threshold, scaled. Cheaper — no
  second binarization — but it needs its own calibration and it has no
  consumer today.

`mrc_background_with_coverage` then has to grow from one channel to three.
Its inner loop is per-pixel, so this is mechanical; the box-downsample and
the ink fill both become per-channel.

**(b) An RGB background encode.** `jp2lam` encodes RGB and the writer takes
`ColorModel::Rgb`, so this is a matter of not converting to luma on the way.
Everything on the path from `clean_page_for_mrc` down assumes one channel.

**(c) A `PageMode::Preserved`.** `PageMode` is `Binarized | Grayscale`
(`pipeline/config.rs:409`). Preservation is a third value, and it changes the
region policies: image regions stay original, text regions are masked rather
than thresholded, and `clean_gray_page` is not called at all.

**(d) DjVu composition.** `compose_color_canvas` has to be able to paint the
whole page rather than only the detected image boxes, and `compose_normal_page`
has to accept that background under its JB2 ink layer. The encoder already
models it.

**(e) Ink colour (a later stage).** MRC has a foreground *colour* plane; Lege
paints its stencil with one fill colour. Red rubrics, coloured headings and
stamps come out black. PDF would need either per-region stencils with their
own fill colour, or `/Mask` on a low-resolution colour image (the writer has
neither). DjVu would need the FG44 colour layer, which `LayerData` does not
expose. Everything else in this plan works with black ink; this is what
makes it *correct* rather than merely faithful in shape.

**(f) Shared symbol dictionaries on the DjVu side.** `with_shared_dict`
(`builder.rs:314`) exists and Lege never calls it. On a preservation run the
ink layer is the larger half of the file, so a document-wide JB2 dictionary
is worth more here than in preparation mode.

## 5. Order of work

The mode cannot be built before the encoder it depends on, which is why this
is a plan and not a branch.

1. **Coverage from a fresh render.** Supersampled Sauvola into
   `coverage_spans`; verify against a source-MRC page by comparing the
   derived coverage with the decoded one on a book Lege already handles
   (the Gobineau scan is both, which makes it a self-checking corpus).
2. **Colour through the MRC encoder.** Three-channel
   `mrc_background_with_coverage`, RGB JP2 background, RGB IW44 background.
   Still under the existing grayscale-mode switch: at this point
   `--grayscale` on a colour book stops throwing the colour away.
3. **`PageMode::Preserved` and its region policies.** Config, validation,
   `--preserve` on the CLI, the plan-page path, no `clean_gray_page`.
4. **DjVu parity.** Full-page colour background beneath the JB2 layer;
   shared dictionary while we are there.
5. **The GUI toggle.** Preparation / Preservation as the first control in the
   left column, above everything else, each with one sentence of tooltip:
   - *Preparation* — "Rebuilds the book for an e-ink screen: text is
     binarized and re-encoded, pictures are reduced, colour is dropped.
     Small files, made to be read on the device."
   - *Preservation* — "Keeps the book as it is — colour, tone and all — and
     compresses it as layers instead: a JPEG 2000 background under a crisp
     text layer, searchable, at the resolution you ask for. Larger files,
     made to be kept."
   Choosing Preservation hides the binarization method, dithering, halftone
   and compatibility-mode controls, and reveals background quality and
   background subsampling. Truetyping becomes an explicit opt-in there
   rather than the default, because it replaces the scanned ink.
6. **Ink colour.** Per-region fill colour first (cheap, covers rubrics);
   a proper foreground plane only if a corpus demands it.

## 6. Open questions

- **Where does "requested resolution" bind?** The mask and the background
  can differ. Proposal: the requested height is the *mask's*, and
  `mrc_bg_subsample` (already resolution-aware) sets the background beneath
  it. A preservation run at 300 dpi then means a 300-dpi ink layer over a
  100-150 dpi background, which is what every MRC encoder does.
- **Is layout detection wanted at all?** It is not needed to find the ink,
  only to keep photographs out of the mask. Cheaper alternative: variance
  over the coverage plane. Worth measuring before committing the model to a
  mode whose whole point is fidelity.
- **How big is the result?** An MRC PDF of a colour book at 300 dpi is
  perhaps a third to a half of the source scan, not a twentieth. That is the
  honest expectation to set in the tooltip, and it should be measured on a
  corpus before the mode ships.
- **Does the ink layer belong in JBIG2 symbol mode?** Symbol substitution is
  lossy by construction (one bitmap for near-identical glyphs). In a mode
  called preservation it should default to generic, as
  `encode_mrc_base_layer` already forces for its own reasons.

## 7. Verification

- A colour scan through preservation mode, rendered back with `lege-pdf` and
  compared to the input render: no channel may be flat, no pixel outside the
  ink may move by more than the background coder's own error.
- The Gobineau scan, which arrives already MRC: the derived coverage plane
  must agree with the decoded one within a small margin.
- Size and time on a corpus, against the input and against preparation mode.
- The DjVu output opened in a viewer that is not ours.
