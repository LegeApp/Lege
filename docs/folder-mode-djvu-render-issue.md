# Folder-mode DjVu rendering issue

Status: **unresolved**. Resize work-around is in place; rendering bug deeper in
the JB2 path remains.

## Symptom

In folder-mode (`<lege> <image-folder> --text-format djvu --no-layout ...`),
the produced `.djvu` file opens in SumatraPDF and WinDjView but every page
renders as solid white. PDF-mode (`<lege> file.pdf --text-format djvu ...`)
produces files that render correctly in the same viewers, with the same
options.

## What we ruled out

- **CLI error swallowing.** Fixed earlier in this thread (`handle_cli_mode`
  now prints the error before `fast_exit(1)`). The folder-mode job runs
  cleanly to completion; nothing is being suppressed at the harness level.
- **Page dimensions.** First-pass folder-mode output had pages at the JP2's
  native resolution (e.g. 3494×4967). SumatraPDF/WinDjView refused these
  outright with "could not render page". Forcing the resize block in
  `process_djvu_cpu_intensive_work` to run regardless of layout-detection
  state (`src/pipeline/djvu_pipeline.rs`) brought pages down to ~844×1200
  (matching what Pdfium produces in PDF-mode). After this change viewers
  *accept* the file but render it white. So the dimension issue is fixed;
  the white rendering is a separate, second bug.
- **Binarization output.** A debug-only counter on the binarized buffer
  shows realistic ink coverage per page (~3-4% black for content pages,
  ~0% for blank/cover pages). The bytes are 0/255 with correct polarity
  (0=black, 255=white), so the input to the JB2 encoder is sane.
- **JP2 decode color order.** `Legencode/src/jp2_encoder.rs::decode_jp2_bytes`
  returns interleaved RGB; not BGR.
- **FGbz palette chunk.** Initially looked like the bug — folder-mode output
  was missing FGbz on every page. We patched the encoder
  (`DJVULibRust/src/doc/page_encoder.rs`) to set `wrote_bg44 = true` in the
  auto-emit-white-BG44 branch so FGbz would be written. **This patch was
  wrong** and has been reverted: the working PDF-mode file's content-rich
  pages (e.g. page 1 with `Sjbz=19723 + TXTz`) have **no FGbz at all** and
  render fine. So the absence of FGbz is the correct, working state. Adding
  our hand-rolled FGbz with manually-built BZZ correspondence indices was
  itself plausibly what tipped the file into the white-rendering state at
  one point in the investigation.
- **Polarity of `bitmap_to_bitimage`.** `pixel.y < 128 → bit=true (foreground)`,
  matches `add_bitmap_runs`'s convention (`get_pixel_unchecked == true` means
  "consume run of black pixels"). Identical to the path PDF-mode takes.
- **Encoder code path.** PDF-mode and folder-mode both go through
  `DjvuOrchestrator::process_page` → `encode_normal_page` → `with_foreground` →
  `add_jb2_foreground` → `analyze_page`. No mode-specific encoder path.

## Side-by-side IFF chunk dump

Working (PDF-mode, `wheelsofcommerce00brau_1`, page 1 — content-rich, renders correctly):

```
FORM:DJVU size=23140
    INFO  size=10     033e04b018002c0116014247...
    BG44  size=888    00550102033e04b08affffeb...   (auto-white)
    Sjbz  size=19723  802c0e602c431e99ed08e9af...
    TXTz  size=2481   ...
```

Broken (folder-mode, `Finnegans-Wake_jp2`, page 0 — content-rich, renders white):

```
FORM:DJVU size=3856
    INFO  size=10     034c04b018002c0116014247...
    BG44  size=925    00550102034c04b08affffec...   (auto-white)
    Sjbz  size=2892   802b2e602c392e7a8625386...
```

The two structures are functionally identical: same chunk types in the same
order, both have an auto-emitted small white BG44, both have a non-empty
Sjbz. The header bytes of `Sjbz` differ (`802b2e60` vs `802c0e60`) but
neither is a fixed magic — those bytes are the first output of the
arithmetic coder and are content-dependent. INFO chunks differ only by
page width.

## Where the actual bug is — best current theory

The Sjbz chunk size — **2892 bytes for ~38 000 black pixels at 844×1200 with
~105 reported blits** — is suspiciously low. A real text page at this
resolution typically has 1500-3000 connected components and a JB2 stream in
the 10-20 KB range (cf. working page 1's 19 723 bytes). The
under-population strongly suggests one of:

1. **Lanczos3 ringing during the unconditional resize** is producing
   gray-fringe pixels that, after fixed-threshold binarization, fragment
   glyphs into many tiny components. `analyze_page`'s `merge_and_split_ccs`
   then collapses/discards them, leaving 105 large, mostly-empty shapes.
   When viewers paint these shapes in foreground colour, the result is
   approximately background-colour-everywhere because the shapes don't
   cover the actual ink positions.
2. **`shapes_to_encoder_format`'s coordinate flip** (`bottom = page_height -
   bbox.ymax`) might be off-by-one for shapes whose `bbox.ymax` is exclusive
   vs inclusive, placing all shapes one row off (or off-page). This would
   not break the JB2 codestream's validity — viewers would just render
   nothing visible.
3. **Reading-order sort in `shapes_to_encoder_format`** could be desynchronising
   the blit stream from what the JB2 encoder writes, producing a valid but
   semantically empty mask.

PDF-mode avoids hitting whichever of these is the root cause because Pdfium
delivers pages already at target resolution — there is no Lanczos3 step,
so the binarized image has clean, sharp glyphs and `analyze_page` finds the
expected number of components.

## Suggested next investigation

When picking this back up, instrument the encoder path with debug-logging
prints of:

- `shapes.len()` returned by `analyze_page(...).extract_shapes()` per page
- `blits` after `shapes_to_encoder_format`: count, and a sample of
  `(left, bottom, shapeno)` for the first ~10 entries (with original
  `bbox` values for comparison)
- For one representative content page, dump the binarized buffer to PBM
  before it reaches `create_bitmap_from_binarized` — open it in an image
  viewer to confirm the binarized text actually looks like text after
  the resize

If the PBM looks correct but `shapes.len()` is in the dozens rather than
the thousands, the bug is in `analyze_page` / `merge_and_split_ccs`. If
`shapes.len()` is sane but the JB2 still renders blank, the bug is in
`shapes_to_encoder_format` coordinate handling or the JB2 encoder itself.

## Files touched (and current state) related to this issue

- `src/pipeline/djvu_pipeline.rs` — resize block now runs unconditionally
  (no longer gated on `enable_layout_detection()`). **Keep this.** It's
  required for viewers to accept folder-mode output structurally.
- `DJVULibRust/src/doc/page_encoder.rs` — auto-white-BG44 branch reverted
  to NOT set `wrote_bg44 = true`. **Keep this**, matches working file's
  structure.
- `src/main.rs::handle_cli_mode` — prints error before `fast_exit(1)`.
- `src/pipeline/source.rs`, `src/pipeline/djvu_pipeline.rs` — extra
  `error_println!` lines (debug-logging-only) at page-drop sites.
