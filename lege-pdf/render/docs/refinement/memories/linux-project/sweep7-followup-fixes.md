---
name: sweep7-followup-fixes
description: "2026-07-22 post-sweep-7 work: 6 commits (xref stale offsets, flate 1.65x, JPX sub-8-bit, degenerate 9/7 DWT, JPEG DNL height, form path scoping) + the PDFium-parity decision on sub-8-bit widening"
metadata: 
  node_type: memory
  type: project
  originSessionId: a74f11d0-7a3b-4e31-b083-7160b310b037
  modified: 2026-07-22T08:43:57.265Z
---

Worked the [[sweep7-analysis]] priority list in order. Six commits; every one verified against
PDFium (and, for codec work, bit-exactness against OpenJPEG first). Broad regression check before
committing the risky ones: 378 corpus files / 1,696 pages → **27 pages improved, 0 regressed**.
Sweep 8 launched afterwards to confirm at corpus scale.

**pdf-renderer (master)**
- `d75c6d3` **xref: rebuild when a recovered chain's offsets are stale.** A wrong `startxref` means
  shifted byte offsets, so the table found by scanning for the last `xref` keyword is usually stale
  too. `/Root` often still resolves (low object numbers survive the shift) so neither the trailer
  check nor the degenerate-page-tree check fired — the document opened with page/content objects
  pointing into their neighbours and rendered blank with no diagnostic. PDFium never uses such a
  table at all (a `startxref` that misses goes straight to `RebuildCrossRef`); we do scan, so
  validate what the scan finds — every offset entry must point at the object header it claims —
  and escalate. **Scoped to recovered chains only**; a reported `startxref` is kept (PDFium's
  `VerifyCrossRefTable` checks only the first entry, so it keeps those too). Fixed 4 files:
  flate_predictor_bpc_1 0.7254→0.0001 (misnamed — the TIFF predictor was fine all along),
  issue11230 0.9804→0.0005 (+5 pages; also NOT a CCITT bug), close-path-bug and issue17554
  1.0000→~0 (both were "compile failed").
- `d975d01` **flate 1.65x.** Measured over 22,122 real corpus streams (121.6 MB in / 408 MB out):
  miniz_oxide+scratch 913 ms → zlib-rs+direct 552 ms (446 → 739 MB/s). Two changes: flate2's
  `zlib-rs` feature (zlib-ng port, still pure Rust, no C toolchain) and `decompress_vec` into the
  output vector's spare capacity instead of a 64 KiB scratch + `extend_from_slice` (was copying
  every byte twice), with a PDFium-shaped size guess up front. **Not a page-render win** — a
  98-page text workload measured 1.56s before / 1.55s after; content-stream inflate is a small
  share of compile. It pays on large Flate images, object streams, and big-document xref streams.
- `1d61f3c` **JPX sub-8-bit widening matches PDFium, deliberately not OpenJPEG.** PDFium's
  `CJpx_Decoder::Decode` widens with `src << (8 - prec)`, so a 1-bit set sample becomes **128, not
  255** — a bitonal scan renders black-on-mid-grey. Full-range scaling (what OpenJPEG's writers do,
  and what the spec's colour mapping implies) leaves pdfbox/3246 at 0.81/0.99 and 4326 at 0.98;
  the shift puts all three at **exactly 0.00000**. Codec correctness is unaffected either way —
  jp2lam is bit-exact with OpenJPEG — this is only the renderer's widening, and it now uses one
  convention in both directions (the >8-bit branch already shifted). **Deliberate, documented,
  one-line reversible** if parity is ever traded for spec-literal colour.
- `fffc9dc` **JPEG: DNL height placeholder recovered from the image dict.** `SOF` height `0xFFFF`
  means "real height arrives in a DNL marker"; issue8614 is truncated before the DNL, so a
  2480x3473 scan decoded as **2480x65535** — content in the top 5%, 62k rows of flat mid-grey drawn
  over the page. PDFium repairs this in `PatchUpKnownBadHeaderWithInvalidHeight`; same guards (real
  SOF, height exactly 0xFFFF, widths agree). issue8614 p0 0.8815→0.0004, p1 0.8847→0.0003.
- `560038f` **content: path construction scoped to the content stream across `Do`.** `self.path` /
  `self.pending_clip` are one shared builder, so a path pending when a form is invoked stayed
  pending *inside* the form and its first painting op painted both. PDFium gives each form its own
  `CPDF_StreamContentParser`. pdfbox/5302 emits `0 0 1155 1563 re`, invokes the form mid-path, then
  `W` with no painting operator at all — the form's first `B` filled the page rect black, so a
  FedEx label rendered as a black page with the barcode punched out white. 0.6933→0.0052.
  **Highest blast radius of the session** (every form invocation) — hence the 1,696-page check.

**jp2lam (Lege-ecosystem main)**
- `769c202` **sub-8-bit sample precision (1/2/4-bit).** The SIZ gate accepted only 8..=16; bitonal
  scans are 1-bit and were the largest census class (16 of 61 failing streams). Nothing in the
  pipeline needed a special case — the gate plus the JP2 `ihdr` gate. Verified **bit-exact vs
  OpenJPEG (max abs diff 0 over 8,699,840 px)** on pdfbox/3246 obj3. Also fixed the CLI's PNG
  writers, which clamped raw samples to 0..255 — that wrote 1-bit pages as 0/1 and clipped 16-bit
  components instead of scaling. **That was a hazard in the instrument used to verify everything
  else.**
- `c393bc5` **a one-row 9/7 resolution is an identity, not a K scaling.** THIS RESOLVES HANDOFF §2.
  `forward_97_vertical_in_place`/`inverse_97_vertical_in_place` scaled a single-row resolution by
  INV_K/K. The pair round-trips through our own encoder (so every test passed) but does not match
  conformant streams: OpenJPEG returns from both `opj_dwt_encode_and_deinterleave_v_real` and
  `opj_v8dwt_decode` on height 1 without touching the data, and **our own horizontal 1-D path
  already treated it as an identity — the two disagreed.** This is the drift the handoff recorded
  when it tried lifting the decomposition-level bound: an 884x1344 image with 17 resolutions passes
  through five 1x1 levels, so its DC came out multiplied by **K^5 = 2.8158** while the AC was
  pixel-perfect (a fit against OpenJPEG gave `ours = 1.002*opj + 194` — a pure DC offset; that
  signature is how it was localized). With the identity restored the dimension-derived bound has
  nothing left to reject and is replaced by the spec's own `NL <= 32`. Verified on Howard R. Turner
  *Science in Medieval Islam* obj7: max abs diff 5, mean 0.147, 3062/1,188,096 px over 2 — the 9/7
  float floor at 11 real levels rather than the usual 5, diffuse and edge-concentrated (mean
  |gradient| 34.3 where err>2 vs 3.88 overall). End-to-end 0.2424 silent-blank → 0.0014.

**New settled non-bugs (do NOT re-triage — we are right, PDFium is wrong):**
- `pdfbox/5260.pdf` p0 (inkΔ 0.9253): damaged file, xref rebuilt. We render the real content (a red
  header bar + Chinese hospital-form text); **PDFium floods the page black**.
- `pdfbox/5657.pdf` / `issue16782.pdf` p0 — already recorded in [[sweep7-analysis]].

- `f5063f7` **name-keyed resolution caches scoped to their resource dictionary.** THIS FIXED
  issue8565 (below) and five other files. `font_cache`, `type3_cache`, `pattern_cache`,
  `tint_cache`, `cie_cache` are keyed by resource *name*, but a name means whatever the active
  `/Resources` says. Now parked/restored with the `self.resources` swap at all three sites (form,
  soft mask, Type 3 CharProc). **All five move together on purpose** — `show_type3` reaches
  `type3_cache` via `fonts[id].resource_name`, so scoping one of that pair without the other drops
  glyphs. Wins: issue8565 0.5370→0.0015, PhRMA ChartPack p0 0.2290→0.0024 (p140 0.0514→0.0007),
  EN-05-10137 ×6 (0.16/0.15/0.14→~0.025), 1772 p15 0.1091→0.0143, processstudies ×5 (~0.060→~0.007).
  **Accepted cost (isolated by building each variant separately):** clearing `font_cache`
  re-resolves an identical font object into a duplicate `SemFont`, shifting substitution slightly
  on one book (An Investors Guide to Trading Options p22 +0.0026, p33 +0.0030, p44 +0.0045, p55
  -0.0040) — kept because the same scoping is what fixes processstudies. Also +0.003..+0.009 on
  four `degraded(codec)` pages of 5464.pdf, where a codec failure already leaves the page partial.
  Net over 378 files / 1,696 pages: 24 distinct pages changed, 15 better, 9 worse, none by >0.0088.
  **TRIED AND REJECTED — keying the font cache by resolved object.** The obvious explanation for
  the residual churn was duplicate `SemFont`s: clearing `font_cache` re-resolves the same font
  object under a nested scope and pushes a second entry, and two `SemFont`s for one font substitute
  independently. I implemented a global `font_by_object: HashMap<ObjectId, FontId>` consulted before
  re-resolving (excluding Type 3, whose glyph data is name-keyed and would be unreachable through a
  reused `SemFont`). Measured over the 33 affected files: **byte-identical to not having it** — every
  page matched the scoped-cache numbers exactly. So the mechanism is NOT duplicate `SemFont`s and
  the dedupe was reverted rather than shipped as dead state. **The residual class is real but its
  cause is still unknown**; the likeliest remaining explanation is that these files genuinely map the
  same font name to different objects per scope, in which case our new behaviour is correct and the
  divergence is from PDFium rather than from truth. Worth a fresh look if it ever matters — the
  whole class is under 0.02.

**METHOD NOTE (cost me a sweep):** the diff tool execs a fresh worker binary *per chunk*, so
**rebuilding while a sweep runs silently mixes binaries across the run.** Sweep 8 was discarded for
this reason and re-run as **sweep 9**. Do not `cargo build` during a sweep.

**Was the next structural target — `issue8565`, now FIXED by `f5063f7`** (ours 0.283 vs ref 0.820, we
under-paint). Page is 4 ops: one `fill` with a **tiling pattern**. The tiling pattern's content is
`/Gs1 gs /Pattern cs /Sh1 scn  0 0 400 400 re f` — so it applies an ExtGState (obj 13, a
luminosity soft mask over form 12) and then fills with a **shading pattern** (PatternType 2,
ShadingType 3 radial, `/Extend [true true]`, DeviceGray 0->1 ramp). Form 12 is itself a DeviceGray
transparency group painting another radial shading pattern. So the failing interaction is
**luminosity soft mask -> transparency group -> shading pattern, nested inside a tiling pattern**.
Under-inking points at the mask being applied too strongly or the radial `/Extend` not filling.
Not a quick win — a feature-interaction investigation, not a one-line fix.

**Still open in the structural class** (other top singles, not investigated): image_inline_5 (OVER 0.53 vs 0.06), bug1077808 (UNDER), jbig2_file_header
(0.41 vs ref 0), issue18466, issue968, 1416, issue6707, issue9462,
resvg_masking_mask_recursive_on_self, 5250_1, 6024, 3000_9, issue50.
