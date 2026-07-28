# pdf-renderer

A read-only, memory-bounded, **concurrent** PDF rendering engine in Rust — a
semantic port of PDFium's behavior with a fundamentally different core: an
immutable document snapshot, worker-local execution state, and a
backend-neutral compiled-page IR that both a CPU rasterizer and (later) a WGPU
backend consume.

Design documents (in the parent directory):

- `../pdfium-concurrency-rust-port.md` — why PDFium is serial; the concurrency
  architecture (immutable snapshot, once-publication, worker contexts).
- `../expanded-rust-pdfrender-plan.md` — the phased roadmap (CPU-first phases
  0–6, WGPU phases 7–11) and the `CompiledPage` rendering contract.
- `../skeleton-blueprint.md` — repo-concrete decisions this workspace encodes.

## The one rule

> Rendering a page must operate on an immutable document snapshot, with all
> execution state and mutable scratch data owned by the worker or page render
> session. No global state, anywhere.

## Layout

```text
crates/
  pdf-geom              dependency-free geometry leaf (Matrix etc., extracted
                        from pdf-page-ir in the 2026-07 hardening pass)
  pdf-source            positional-read byte access (mmap / owned / file)
  pdf-syntax            lexer + primitive object parser
  pdf-object            object model, ObjectId, name interning
  pdf-structure         xref, trailers, revisions, object streams
  pdf-security          encryption
  pdf-document          DocumentSnapshot, page index, outlines/destinations,
                        ObjectRepository
  pdf-color             color spaces and conversion policy
  pdf-font              encodings, CMaps, font program abstraction
  pdf-image             image resource semantics
  pdf-content           content-stream interpreter → SemanticPage → CompiledPage
  pdf-page-ir           CompiledPage IR — THE rendering contract (dependency-free)
  pdf-render-api        RenderBackend trait, RenderRequest, HostPage
  pdf-render-cpu        reference CPU rasterizer (normative implementation)
  pdf-render-wgpu       experimental decoded-RGB image renderer + CPU fallback
  pdf-render-scheduler  compile pool → render pool, memory permits, reordering
  pdf-postprocess       postprocess graph + CPU executor (crop/resize/gray/
                        tone/threshold/dither/1-bit pack)
  pdf-test-support      diff metrics, determinism harness, corpus access
  pdf-chaos-tests       stable-toolchain never-panic mutation gate
  pdf-read              document doctor (`pdfr doctor` structural triage)
  pdf-text              UTF-16 text, character geometry, rectangles, words
  pdf-cli               driver binary (info / render / dump / text / doctor)
corpus/                 versioned test PDFs (see corpus/README.md)
fuzz/                   cargo-fuzz workspace, 9 targets (nightly/Linux to run)
tools/                  PDFium render and text differential tooling
shaders/                WGSL (Phase 7+)
```

Dependency direction is enforced by the build graph: nothing below
`pdf-page-ir` imports render types. WGPU is owned by `lege-gpu`; the
experimental `pdf-render-wgpu` backend reuses that process-wide context rather
than creating another device.

## Status

Phases 1–6 (CPU path) and Font Phases 1–2 are implemented and tested.

> **Production-readiness pass — LANDED (2026-07-20/21):** three parallel
> work streams hardened the engine, all merged to master.
> **A — rendering fidelity:** JPEG Huffman pass 4 (u64 bit buffer + batched
> AC decode, −7..16 % decode time), the production `SharedImageCache`
> (96 MiB, 8-shard LRU, content-hash keyed, `PDF_RENDERER_IMAGECACHE`),
> non-Normal blend modes generalized to images/shadings/tilings, knockout
> groups, soft-mask `/BC` backdrops, image-edge partial-coverage AA
> (including rotated placements), and mesh shading **rasterization**
> (types 1, 4–7).
> **B — document-layer completeness:** annotation appearance streams
> (`/Annots` + `/AP /N`, pdfium two-pass order) rendered by default,
> optional-content groups (`/OCProperties` default visibility, `/OC`,
> `/OCMD`), CID→Unicode tables for the four predefined CJK registries,
> vertical writing (`/W2`/`/DW2`, wmode 1), an opt-in CJK substitution
> bridge, and the degenerate tint-LUT repair that closed the last
> silent-blank tracking hole (`ImageIr.lowering_degraded`).
> **C — robustness & safety:** a `catch_unwind` page boundary
> (`RenderError::Panic`, also at the scheduler), workspace-wide
> `unwrap_used`/`expect_used`/`panic` clippy lints at **deny**, the
> `cargo-fuzz` workspace (`fuzz/`, 8 targets), the stable `pdf-chaos-tests`
> never-panic mutation gate, the `pdf-read` document doctor
> (`pdfr doctor`), and **full password support** — non-empty user *and*
> owner passwords, R2–R6, `DocumentSnapshot::open_with_password` /
> `pdf-cli --password`, with real `/U` validation on empty-password opens.
> The toolchain moved to **1.97.1** and `pdf-geom` was extracted as the
> workspace's geometry leaf crate. Still open after the pass: minification
> weight quality (the second half of the image-AA workstream; **landed
> 2026-07-21** — fractional-tap area weights, see
> `docs/refinement/handoffs/DEFERRED.md` "Image
> minification") and the full
> Linux corpus re-sweep under the new annotations-on baseline (older sweep
> CSVs are baseline-incompatible — `tools/pdfium-diff` now renders both
> sides with annotations).

- **Phase 1 — document structure & immutable snapshot.** Random-access
  sources, lexer/parser, xref tables + streams, incremental updates, object
  streams, trailer/catalog resolution, page-tree indexing, recovery, limits.
  Exit gate: six threads structurally resolve six pages from one snapshot
  deterministically, no document-wide mutex.
- **Phase 2 — content interpreter & semantic page.** The PDF graphics-state
  machine (`pdf-content`) compiles a page's operators + resolved resources
  into a backend-neutral `SemanticPage` — paths, painting, clipping, color,
  text, Form/image XObjects (recursion-guarded), inline images, ExtGState —
  with no raster backend involved. Exit gate: pages compile concurrently into
  byte-identical semantic dumps across worker counts and repeated runs.
- **Phase 3 — stable compiled-page IR.** `SemanticPage` lowers to the
  backend-neutral `pdf_page_ir::CompiledPage`: implicit graphics state made
  explicit per op, resources interned into typed tables, an explicit clip
  stack, transparency groups as scoped ops, device color resolution, and
  page feature + complexity summaries. `CompiledPage::debug_dump()` is a
  schema-keyed serialization. Exit gate: the same `CompiledPage` drives two
  independent backends — an SVG-like emitter and a scanline CPU rasterizer.

- **Phase 4 — CPU raster backend.** Analytic exact-area coverage, span
  `KernelSet` dispatch, clipping (rect + path masks), strokes, separable and
  non-separable blend modes, bounded transparency groups, and soft masks.
- **Phase 5 — parallel page scheduling.** Compile pool → render pool with
  memory permits and deterministic reordering.
- **Phase 6 — CPU feature completeness & stabilization.** Axial + radial
  shadings and PDF functions (`pdf-function`), tiling + shading patterns,
  image rasterization (Device Gray/RGB/CMYK + Indexed, `/ImageMask`, `/SMask`,
  nearest/bilinear, Flate/RunLength/raw), the **frozen surface contract**
  (`pdf_render_api::contract`), hardened resource recursion, and the frozen
  DeviceCMYK conversion policy.
- **JPEG (`/DCTDecode`) codec.** A native single-file decoder in
  `pdf-image/src/jpeg/`: baseline + progressive, restart markers,
  gray/YCbCr/RGB/CMYK/YCCK
  with the Adobe APP14 conventions, verified against libjpeg ground truth.
  Injected via `CpuBackendOptions::codecs` (never global).
- **Font Phase 3 — non-embedded text renders as glyphs.** All 14 standard
  faces are bundled (PDFium's Foxit CFF, converted once to OTF — see
  `crates/pdf-font/fonts/README.md` for provenance and licence), reached
  through PDFium-compatible aliases (`Arial,Bold` → Helvetica-Bold) with
  descriptor-driven inference and a deterministic fallback, so an unknown or
  unembedded font never renders as boxes. Symbol/ZapfDingbats resolve through
  their Annex D built-in encodings.
- **Font Phase 4 — hinting.** `pdf_font::HintingPolicy` (`None`/`Embedded`/
  `Auto`) over Skrifa's hinter, resolution-dependent per the roadmap: `Auto`
  grid-fits only axis-aligned text at or below 50px/em, so screen text is
  crisp while print-resolution and rotated text keep exact outlines. Opt-in
  via `CpuBackendOptions::hinting` — the frozen surface contract describes
  the unhinted default.
- **Bare CFF (`/FontFile3` Type1C / CIDFontType0C).** PDF embeds CFF raw;
  Skrifa reads SFNT only, so `wrap_bare_cff` describes the CFF as an OTF
  (its bytes unchanged) and `cid_to_gid_from_cff` recovers CID→GID for
  CID-keyed fonts. Without it such fonts silently substituted (dropping
  glyphs the stand-in lacked) or fell to placement boxes.
- **Font Phase 7 — system fonts (opt-in).** `FolderFontProvider` indexes the
  machine's installed families and resolves non-embedded fonts by name, with
  PDFium's per-charset CJK preference lists; `.ttc` collections resolve to the
  right face. Injected via `PageCompiler::with_system_fonts` and off by
  default, because system fonts make output host-dependent — the deterministic
  bundled path remains the default. `pdfr render … --system-fonts` opts in.
- **Separation / DeviceN tint transforms.** Spot colours evaluate their tint
  transform through `pdf-function` into the alternate space. These spaces are
  subtractive (tint 1.0 = full ink), so resolving them by arity inverted them
  and blanked whole documents; `[/Separation /Black ...]` is a very common way
  to spell black.
- **Font Phase 5 — native Type 1.** Bare `/FontFile` (PFA/PFB) is decoded by
  an in-house engine in `pdf-font/src/type1.rs`: eexec + charstring
  decryption, the charstring interpreter (`seac`, flex, hint replacement),
  and the font's built-in encoding. `FontProgram` routes Type 1 there and
  everything else to Skrifa, so callers see one API. Verified on 136 real
  embedded Type 1 faces (136/136 parse).
- **JPEG 2000 (`/JPXDecode`) + JBIG2 (`/JBIG2Decode`) — MRC renders.**
  JPX decodes via **jp2lam**, shared in-tree from `lege-codecs/jp2lam`; the
  workspace toolchain is pinned at **1.97.1**. Verified bit-exact against openjpeg on
  441 real archive.org streams. JBIG2 and CCITT G3/G4 now decode natively in
  `pdf-image`; together they render Internet Archive MRC scans (JPX background
  + JPX foreground masked by a JBIG2 `/SMask`). The July 2026 full-corpus
  later sweep history and remaining codec edges are indexed in
  `docs/refinement/README.md`.

`cargo run -p pdf-cli -- dump <file.pdf> <page>` prints the semantic display
list; `render` compiles to the IR and runs it through the CPU backend.
