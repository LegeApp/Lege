# TrueTyping and JBIG2 halftone review — 2026-09-05

Reviewed implementation at `4a7349f94b1fb6f6f90b871be5dbb9b5736062de`, with the existing dirty worktree preserved. This is a review, not an implementation of the suggested fixes. No application or codec source was changed. P1 means high priority; P2 means normal-priority correctness; P3 means minor API/documentation trouble.

## Findings

### 1. P1 — Halftone crops without dark tones emit the wrong number of bitplanes

`lege-codecs/jbig2enc-rust/src/encode/halftone.rs:854`: `gray_coded_bitplanes` derives the plane count from the maximum index used by the image. Decoders derive it from the pattern dictionary size (`src/decode/halftone_region.rs:177`). With the default 16 patterns, four planes are required even when the crop only uses indices 0 and 1. The encoder instead emits one plane, which the decoder treats as the most significant plane and then attempts to read three more.

Reproduced through the public grayscale encoder and native decoder; independently confirmed the pale case with system `jbig2dec`:

| Input black coverage, uniform 64×64 crop | Decoded black pixels |
| --- | --- |
| 17/255 = 6.67% | 2816/4096 = **68.75%** |
| 119/255 = 46.67% | 4096/4096 = **100%** |
| 136/255 = 53.33% | 2304/4096 = 56.25% |
| 255/255 = 100% | 4096/4096 = 100% |

The MMR gray-plane option rejects the low-range crops as missing a terminating EOFB. The default arithmetic path silently produces the wrong image instead. Existing `decode_halftone` tests intentionally use gradients spanning the full range and miss this case.

Fix: pass the dictionary size into bitplane decomposition and always emit `ceil(log2(pattern_count))` planes, including leading zero planes. Add constant-tone and restricted-range round trips against an independent decoder before tuning appearance.

### 2. P1 — TrueTyping discards oversized connected content

`lege-process/encoding/glyphfont.rs:1044`: any component wider or taller than `MAX_GLYPH_PIXELS` (2047 pixels) is counted and skipped. No residual raster carries it into the PDF. The pipeline only logs the loss at finalization (`pipeline/pdf_tokio_pipeline.rs:4101`). This is particularly relevant to page rules, table borders and connected line art when layout does not isolate them as images.

Reproduction: a 2200×80 page containing a 2160×3 black rule returned **zero glyphs**, with session statistics `(1, 0, 0, 1)` — one page, zero glyphs/occurrences, one dropped component.

Fix: preserve unrepresentable components in a lossless residual bitmap, or fall back to a raster text layer for the page. Font coordinate limits must not erase page content.

### 3. P1 — Complex but small components create malformed TrueType glyphs

`lege-process/core/truetype_writer.rs:234` casts the number of contours to `i16`; line 241 casts contour endpoint indices to `u16`. Neither limit is checked. The dimension guard above does not bound contour or point counts.

Reproduction: a single connected 380×380 grid, comfortably inside the 2047-pixel limit, was accepted as one glyph. Its generated `glyf.numberOfContours` was **-29814**, although the payload was encoded as a simple glyph. A negative contour count identifies a composite glyph, so the emitted record has the wrong structure. Point indices also overflow on this input. Font readers may reject the glyph or font.

Fix: validate contour and point counts before serialization; split complex shapes into valid pieces or preserve them in the residual bitmap. Return a checked error rather than silently narrowing counts. Validate emitted fonts with an independent parser/sanitizer.

### 4. P2 — Shape sharing overwrites correctly recognized character identities

`lege-process/encoding/glyphfont.rs:1419`: a document-wide vote chooses one Unicode string for each shared shape. `core/pdf_artifact.rs:70` then suppresses the independent OCR layer on any page with visible glyphs. Identical-looking characters can have different identities even when OCR recognizes both correctly.

Reproduction: identical ring bitmaps recognized as `O` and `0` both became CID 2. The resulting CMap contains `<0002> <0030>`, so both occurrences extract as digit zero. Similar collisions apply to Latin/Cyrillic lookalikes and punctuation; this is a representation limitation, not necessarily an OCR error.

Fix: separate outline identity from text identity. Use multiple CIDs mapping to the same outline, or occurrence/word-level `ActualText` retaining the recognized string. Keep shape deduplication without forcing one semantic value onto every occurrence.

### 5. P2 — An incomplete glyph mapping suppresses the entire OCR fallback

`lege-process/core/pdf_artifact.rs:70`: the only condition for dropping `build_text_layer` is that `glyph_lines` is nonempty. It does not check whether glyphs have Unicode mappings or whether every source/OCR word is represented. `glyphfont.rs:1316` skips words with no matching component; `glyphfont.rs:1444` skips unmapped shapes. Thus a word absent from the traced layer — for example, text retained inside a raster figure or a component lost during binarization — has no fallback even if hOCR contains it.

This finding is from the code path, not a new corpus OCR experiment. Fix: account for word coverage and retain unmatched OCR words, while preventing duplicate text for covered words. The occurrence-level approach in finding 4 can provide the same accounting.

### 6. P2 — Reflow plus halftone produces a rejected GUI job

`lege-process/GUI/Freya/src/models.rs:184` forces raster reflow to `ccitt4`, but `can_select_jbig2_halftone_images` at line 223 only checks the stored compression choice and output format. `worker_process.rs:436` consequently emits `--halftone`. The CLI rejects `--text-format ccitt4 --halftone` (`core/main.rs:2211`, also line 4913).

Reproduction through the actual models: select halftone, enable reflow, normalize settings. Result: `reflow=true text_format=ccitt4 layout=true use_halftone=true`.

Fix: gate availability and argument emission on the effective encoder/reflow state. Normalize the image choice when entering reflow, and test that sequence in both selection orders.

### 7. P2 — Partial halftone cells lose ink at right and bottom edges

`lege-codecs/jbig2enc-rust/src/encode/halftone.rs:608` sums only valid pixels in the last cell, but quantization at line 638 always divides by `M*M`. Missing samples are effectively treated as white. Clipping a center-filled pattern further changes the visible density.

Reproduction with default settings: a solid-black **65×65** input decoded to **4110/4225** black pixels; only **14 of 65** pixels in the rightmost column remained black. A solid-black 1×1 input decoded white.

Fix: extend the source to whole cells using edge replication before filtering/decimation, encode the padded extent and clip placement to the original crop; alternatively use an edge-aware quantizer/pattern selection that accounts for the visible cell area. Test every width/height residue modulo M.

### 8. P2 — The quantizer and dictionary disagree about black coverage

`lege-codecs/jbig2enc-rust/src/encode/halftone.rs:638` treats index `i` as density `i/(N-1)`. Dictionary construction at line 701 uses `ceil(i*M*M/(N-1))` black pixels. With M=4 and N=16, interior levels are biased toward extra ink, and error diffusion never sees that error because it subtracts the nominal index.

Reproduction, using a level high enough to avoid finding 1: input 136/255 = 53.33% black maps exactly to index 8, whose pattern contains 9/16 = **56.25%** black. Default patterns have populations `0,2,3,...,16`: the one-black-pixel pattern is missing entirely.

Fix: quantize and diffuse error against actual pattern coverage. A 17-pattern dictionary with populations 0 through 16 is a useful candidate, but requires five planes; compare its rate/quality with 16 carefully selected populations after fixing finding 1.

### 9. P2 — Paper whitening creates an abrupt highlight cutoff inside pictures

`lege-process/pipeline/pdf_tokio_pipeline.rs:1182` and line 1962 clamp every sample at or above `PAPER_WHITE_FLOOR` (245) to zero ink, including samples inside detected photographs. Immediately below the cutoff, linearization gives approximately 9.4% black coverage; at 245 it abruptly becomes zero. Bright gradients lose detail and acquire a discontinuity before halftoning even begins.

This is intentional paper cleanup applied too broadly. Fix: make whitening depend on a paper/background mask, or use a smooth, configurable highlight rolloff. Preserve the existing linear-light coverage conversion for photographic tones.

### 10. P3 — The advertised sharpening setting has no effect

`lege-codecs/jbig2enc-rust/src/encode/structs.rs:115` exposes `sharpening_l`; the quantizer accepts it as `_l` (`halftone.rs:633`) and never uses it. Changing the parameter cannot improve sharpness. The `lossless` setting also only changes the segment type after the same lossy resampling pipeline, despite its documentation promising bit-perfect reconstruction.

Fix: implement and test the promised behavior, or change the API/docs to describe the actual behavior. Do not present these as working quality controls.

## Image-quality improvements after the correctness fixes

1. **Reduce unnecessary blur.** The grayscale path always applies a 3×3 box filter (`halftone.rs:365`) and then a 4×4 cell sum. The additional blur suppresses fine features even on clean continuous-tone inputs. Compare no extra prefilter, a gentler filter, and edge-adaptive filtering. Reserve descreening for sources that already contain a printed/stochastic screen.
2. **Expose a real detail/size tradeoff.** `encode_halftone_region_grayscale` always constructs default settings (`lege-process/encoding/streamline.rs:376`). Compare M=2, 3 and 4, with coverage-correct dictionaries, at the same final display size and byte budget. Smaller cells retain more spatial detail but change tone resolution and compression. A fixed 4-pixel cell has different physical size when TrueTyping raises the working raster resolution.
3. **Compare screen patterns.** Try a properly constructed 45-degree clustered screen and dispersed-dot alternatives against the present axis-aligned radial fill. A rotated screen needs corresponding geometry and masks; changing only the grid vector is insufficient. Judge on the actual e-ink device as well as a monitor.
4. **Preserve line art separately.** Charts, thin rules and figure labels should use lossless generic regions when cell averaging would erase them. Mixed photo/line-art regions can use a grayscale/halftone base and a lossless detail overlay.
5. **Use a focused quality corpus.** Include constant patches, restricted tonal ranges, ramps crossing 244/245, odd crop sizes, one-pixel lines, text-in-figures, photographs, and already-screened scans. Require decoder agreement first. Then compare low-pass reconstructed luminance, edge/detail retention, byte size, and final-size human inspection; raw bilevel pixel error alone rewards the wrong properties.

These are experiments to evaluate, not claimed measured improvements. The encoder's cited [Valliappan et al. paper](https://users.ece.utexas.edu/~bevans/papers/1999/jbig2/jbig99paper.pdf) distinguishes stochastic-halftone descreening from the continuous-tone source case, examines grid size/pattern orientation, and evaluates perceptually weighted distortion. Its prefilter is not a general instruction to blur every photographic crop.

## Validation and limitations

- `cargo test --manifest-path lege-codecs/jbig2enc-rust/Cargo.toml --test decode_halftone --quiet`: **7 passed**.
- `cargo test -p lege --lib --no-default-features encoding::glyphfont --quiet`: **26 passed**.
- `cargo build -p lege --lib --no-default-features --quiet`: passed.
- Diagnostic programs call the current encoder, dictionary/font builder, and GUI model code. Source and generated small fixtures are retained under `.agent/scratch/truetyping-review-20260905/`.
- Independent `jbig2dec` decoding confirmed the pale 64×64 crop contains 2816 black pixels.
- No full OCR/GPU/end-to-end corpus run, GUI launch, e-ink inspection, or font-sanitizer run was performed. The malformed contour field was inspected directly in the generated font.
- Existing unrelated JP2LAM, renderer/ledger and library-order edits were preserved. Passing existing tests does not cover the reproduced edge cases above.
- AKR observation recorded as `@lege-ecosystem.observation.truetyping-halftone-review-20260905/1`; `akr build` refreshed the lock and views. Strict validation still flags the unrelated existing `AKR-G012` ancestry warning on `papercut.akr-mcp-failed-to-connect-and-codegraph-mcp/1`. New records pass parsing, typing, linking and sealing. Scratch cleanup found nothing prunable; generated diagnostic executables were removed, and source/fixtures retained.
