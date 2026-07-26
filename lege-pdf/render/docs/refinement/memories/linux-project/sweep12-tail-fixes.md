---
name: sweep12-tail-fixes
description: "The 2026-07-23 triage of sweep 12's tail — 62 of the 66 pages fixed/closed across four passes; open: JBIG2 refinement, /DefaultCMYK CLUT, nested-soft-mask compositing."
metadata: 
  node_type: memory
  type: project
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-23T09:59:50.010Z
---

Follow-on to [[sweep12-analysis]]: triaged all six blank-page classes in the
66-page tail. **46 pages fixed, 8 distinct root causes, 0 regressions** (verified
on the 20 affected files: 168 rows improved, 0 worse, no tail page left; plus a
297-file / 1,547-page regression sample: 0 changed).

Fixes, by value:

1. **JBIG2 MMR, 16 pages** — `jbig2enc-rust/src/decode/mmr.rs`. T.88 §6.2.6
   makes the EOFB optional and real encoders (the §6.5.9 Huffman *collective
   bitmaps*, not generic regions) just stop after the last line; the `fax`
   crate needs a terminating EOL to close that line, so it failed on the final
   row every time — the tell was "decoded height-1 rows". Retry once with
   `[0x00,0x10,0x01]` (EOFB) appended.
2. **JBIG2 symbol pixel cap, 9 pages** — see [[sweep12-analysis]].
3. **JPX COC, 6 pages** — `jp2lam/src/j2k/decode_markers.rs`. A COC differing
   from the COD *only* in `predictable_termination` was rejected. PTERM is a
   decoder error-*detection* hint (T.800 D.4.5) that jp2lam never reads, so it
   is now exempt from the override comparison.
4. **CFF Standard encoding, 5 pages — the highest-value find.** A CFF with no
   `Encoding` operator uses the predefined Standard encoding by definition, but
   `cff.rs` materialized nothing ("the name-based path covers those"). That is
   false for a **symbolic** simple font: PDF `/Encoding` gives it
   `BaseEncoding::Symbolic` (no base names) and `/Differences` typically covers
   only high codes, so every ASCII code hit `.notdef` and **whole documents of
   subset Type1C text rendered blank** with no diagnostic at all. Now
   reconstructed as code → Standard glyph name → GID against the charset.
   Suspect this class is larger than 5 pages corpus-wide.
5. **CCITT "zero dimension", 4 pages** — `pdf-content/src/interpret.rs`. The
   image dict carried `/Height 5 0 R`; `dict_int` never followed indirect
   references, yielding 0. Added `dict_int_indirect` for Width/Height/BPC on all
   three image paths (base, SMask, stencil).
6. **JBIG2 page > 268 Mpx, 3 pages** — `pdf-image/src/codec.rs`. New
   `DecodeLimits::max_pixels_at_bpp(bpp)`: `max_pixels` is calibrated for
   byte-per-sample output, so a 1 bpp codec was rejected at an eighth of the
   memory it actually costs. Never returns less than `max_pixels`, so no codec
   loses headroom. Applied to JBIG2 and CCITT.
7. **JPX Gray+alpha, 2 pages** — `GrayA8` end-to-end (jp2lam
   `DecodeOutputFormat::GrayA8` + `colour_component_count()`, renderer
   `DecodedFormat::GrayA8` + the split in `prepared.rs`). This is the case the
   old `cheeky-launching-charm` plan deferred; its sRGB+alpha half was already
   landed. Note `colour_component_count` is *not* `channels.min(3)` — Gray+alpha
   has 2 output channels but 1 colour channel.
8. **JPX CRG, 1 page** — accepted and ignored, as OpenJPEG's `opj_j2k_read_crg`
   does. Needed arms in *two* dispatch sites; missing the second panicked an
   `unreachable!`.

**Fixed in the follow-on pass (4 more pages, 20 -> 16 open):**
- **Scaled DCT decode** (`pdf-image/src/jpeg/mod.rs`). `pick_dct_size` picks
  8/4/2/1 samples per block edge from the draw's `target_size` (already plumbed
  for JPX) and from whether a full decode fits at all. Reduction is an area
  average of the full 8x8 IDCT; at 1/8 it is the DC term, which *is* the block
  mean, so no IDCT. Plus: a `SOF` over-declaring its height (issue10989 codes
  6304 rows, writes 60000) is clamped to the dictionary, one-directionally.
- **2-colorant `/DeviceN` over a codec image** — `ImageColorSpace::TintLut2`
  (256x256x3 baked table) + `DecodedFormat::Multi2`. Direct samples were already
  converted per texel; only codec-encoded ones lacked a path.

**Fixed in the third pass (7 more pages, 16 -> 9 open):**
- **Non-sRGB ICC profiles — 4 cover pages.** `/ICCBased` was treated as its
  `/Alternate` (pass-through). Four covers carry the Apple/ColorSync **gamma
  1.8** monitor profile and rendered ~10/255 too dark per channel. New
  `pdf-color/src/icc.rs` handles the matrix/TRC class; `IccRgb::from_profile`
  declines a profile that is already sRGB (measured on the conversion, not on
  tags) so the common case stays byte-identical. Direct samples convert at
  compile time; codec images carry `ImageColorSpace::IccRgb` into the IR. Note a
  pure gamma-2.2 profile is deliberately *not* "sRGB" — sRGB has a linear toe.
- **`pdfjs/issue7229`** — xref subsection header `1 7` written with the object-0
  free-list head as its first entry, shifting every object number by one so the
  page's only image was recorded free.
- **`pdfjs/issue6621`** — `/Mask` pointing at a plain 1-bit `/DeviceGray` image,
  not an `/ImageMask`. §8.9.6.4's "1 masks out" is a *stencil* rule; all three
  controls paint where those samples are 1. Flip the declared polarity.
- **`pdfbox/3874`** — `/lenIV -1` means "charstrings are plaintext"; we clamped
  to 0 and ran the 4330 cipher anyway. Every outline scrambled into stray
  segments *in the right places*, which read as a rasterization bug rather than
  a decryption one.

**Fixed in the fourth pass (Opus subagent, 2026-07-23 — 5 fixed + 1 partial,
9 -> ~4 open).** pdf-renderer commits c893f3e/436ad3d/f23a183/f6b20d7/7a0d976,
jp2lam b8f7451 (branch sweep12-codec-fixes):
- `issue9462` — NOT optional content (my "confirmed" OC-spanning theory was
  wrong; MC state already spans Contents segments). The purple lattice was an
  embedded TrueType with corrupt `head.unitsPerEm = 14` scaling glyphs ~71x;
  now rejected (FreeType's 16..=16384 range) forcing substitution. 0.325→0.004.
- `The-Uyghurs` — jp2lam's `inspect_jp2` ran full validation and rejected the
  2-channel-palette JP2 before the renderer could ask for index passthrough;
  inspect no longer validates. 0.46→0.037 (residual sunburst arc = nested
  transparency, not JPX).
- `issue6364` — 2-component JPEG was decoded as gray dropping channel 1; now
  emits Multi2 → TintLut2. gross 0.727→0.047 (residual = CMYK tint saturation,
  same class as Huser).
- `zusammenleben` — stroke text render modes Tr 1/2/5/6 implemented
  (GlyphStroke: outline → user-space PathData → ordinary stroker). 0.125→0.0006.
- `2561.pdf` p3 — `sc`/`scn` had no Indexed arm; palette index read as colour
  components. Now looked up through the base. gross→6e-5.
- `resvg_masking_mask_recursive_on_self` — partial: self-recursion guard
  committed (recursive mask instance renders empty), but the file is actually
  ACYCLIC; real residual is nested-soft-mask compositing (a mask group whose
  own luminosity must be reduced by another mask). Still 0.13.

**Still open:**
- `bitmap-refine-customat`(+`-tpgron`) — JBIG2 refinement: three faults located
  (page-info default pixel ignored, polarity inverted, region placed top vs
  bottom); needs a T.88 refinement decode fix in jbig2enc-rust against
  pdf.js's jbig2.js.
- `Herbert C. Huser` p0 — `/DefaultCMYK` (§8.6.5.6) is an `mft2` lut16 CMYK
  A2B ICC profile; we have no Default-colourspace plumbing and icc.rs has no
  CLUT evaluator. Deferred: needs a 4D-CLUT mft2 path + corpus validation.
- `Godfrey Dale` p0 — CLOSED as no-op: the "we point-sample" premise was
  false; image.rs already area-averages footprint>1 draws and this image is
  8-bit gray at footprint 1.16. The inkΔ is threshold sensitivity on a noisy
  near-1:1 scan, not a bug.
- Residuals: Uyghurs sunburst arc + resvg mask = nested-transparency
  compositing; issue6364/Huser tint saturation = the CMYK ICC class.

**Process lessons:**
- `pdfium-diff` statically links `pdf-image`; rebuild it too or a codec fix
  looks inert (cost one wrong "still broken" conclusion).
- Removing debug scaffolding with Python string-index arithmetic silently ate an
  adjacent block (the standard-14 metrics branch in `interpret.rs`) and broke a
  text-advance test. The workspace suite caught it; `git checkout` on a clean
  tree is what proved it was mine and not pre-existing. Use anchored
  replacements, not index slicing.
- A wall-clock test (`cheap_parses_are_not_retained`, 400 us) caught a real
  slowdown: the CFF Standard-encoding reconstruction scanned the charset per
  code, O(256 x glyphs) on a full Latin face. Hoisted to a process-wide static
  index. Verify such a failure against the pre-change tree before calling it a
  flake — it fails 5/5 pre-change vs 4/5 after, which is what settled it.
- Two codec tests encoded the *old* limits and needed updating with the fix
  (`jbig2::tests::pixel_limit_is_enforced`, the CRG rejection matrix) — both
  still assert rejection, just at the corrected budget.
