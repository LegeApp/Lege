# Skeleton Blueprint: Workspace, Contracts, and Day-One Decisions

This document is the repo-concrete companion to:

- `pdfium-concurrency-rust-port.md` — *why* PDFium is serial and what a concurrent
  architecture must guarantee (immutable snapshot, worker-local execution state,
  once-publication semantics).
- `expanded-rust-pdfrender-plan.md` — *what* to build in which order (CPU-first
  phases 0–6, WGPU phases 7–11, `CompiledPage` as the rendering contract).

Those documents define the architecture. This one fixes every decision needed to
lay down a compiling skeleton that will not need restructuring later. The guiding
question throughout is **"How will this scale?"** — to 5000-page documents, to
all CPU cores, to a GPU backend, to years of malformed-PDF compatibility fixes.

---

## 1. Workspace identity

| Decision | Value | Rationale |
|---|---|---|
| Workspace root | `pdf-renderer/` (inside this repo, next to the plans) | Matches roadmap §3; plans and reference source stay adjacent for porting work |
| Crate prefix | `pdf-` (lib name `pdf_*`) | Matches roadmap; private workspace, crates.io collision irrelevant |
| Edition | 2024 | Current stable; better `unsafe` hygiene, RPIT lifetimes |
| MSRV / `rust-version` | 1.97 (toolchain pinned 1.97.1; was 1.93 when this blueprint was written) | Installed toolchain; we control deployment |
| Resolver | 3 | Edition-2024 default, per-target feature resolution |
| Reference source | `../pdfium-reference-source/` | Read-only oracle for semantics; **never** a build input |

## 2. Crate graph (18 crates, roadmap §3 verbatim)

Layers only depend downward. **No layer above the line may import `wgpu`, raster
types, or OS graphics APIs** (roadmap §2.1).

```text
pdf-source           positional-read byte access (mmap / owned / file)
pdf-syntax           lexer + primitive object parser (no I/O policy, no xref)
pdf-object           object model, ObjectId, name interning, Dictionary/Stream
pdf-structure        xref tables+streams, trailers, revisions, object streams
pdf-security         encryption: RC4/AES, /Encrypt handling, permissions
pdf-document         DocumentSnapshot, page tree index, ObjectRepository
pdf-color            color spaces, ICC hooks, conversion policy
pdf-font             encodings, CMaps, font program abstraction
pdf-image            image resource semantics, decode parameter model
pdf-content          content-stream interpreter, graphics state machine → SemanticPage
pdf-page-ir          CompiledPage, DisplayOp, resource tables, PageFeatures   ← THE CONTRACT
──────────────────────────────────────────────────────────────────────────────
pdf-render-api       RenderBackend trait, RenderRequest, HostPage, errors
pdf-render-cpu       reference rasterizer (tiled), CpuWorkerContext
pdf-render-wgpu      stub now; Phase 7+; feature-gated, NO wgpu dep until then
pdf-render-scheduler compile pool → render pool, memory permits, reorder buffer
pdf-postprocess      backend-neutral postprocess op graph (resize/gray/binarize/pack)
pdf-test-support     corpus loading, image diff metrics, determinism harness
pdf-cli              render/dump/diff driver binary
```

Sibling directories: `corpus/` (versioned test PDFs), `fuzz/` (cargo-fuzz
targets, added when parsers exist), `shaders/` (Phase 7+), `tools/`
(reference-render comparison against PDFium).

### Dependency edges the skeleton enforces

```text
source ← syntax ← object ← structure ← document
document ← content (+ font, image, color) ← page-ir
page-ir ← render-api ← {render-cpu, render-wgpu, render-scheduler}
render-api ← postprocess
everything ← test-support (dev-deps only), cli
```

`pdf-page-ir` depends on **nothing** from the render layer. This is checked by
the build graph itself — the strongest lint there is.

## 3. Dependency policy

Ruthlessly minimal until a phase needs more. Workspace-level versions:

| Crate | Dep | Why |
|---|---|---|
| workspace-wide | `thiserror` 2 | typed error enums; **no `anyhow` in libraries** (only `pdf-cli`) |
| pdf-source | `memmap2` | `MmapSource` |
| pdf-page-ir | `bitflags` 2 | `PageFeatures` |
| pdf-render-cpu, scheduler | `rayon` | worker pools |
| pdf-render-scheduler | `crossbeam-channel` | bounded MPMC stage queues |
| pdf-cli | `anyhow` | binary-level error context |

Explicitly deferred: `wgpu` (Phase 7), compression/codec crates — decide
per-codec at Phase 1/2 whether to use `flate2`/`zune-*`/hand-port from the
reference source (JBIG2 and CCITT will almost certainly be ports; there is no
production-quality Rust JBIG2 decoder).

**No global state, anywhere.** No `lazy_static`, no `static mut`, no
process-wide caches, no global font context. Everything threads through
`DocumentSnapshot`, a worker context, or an explicit cache object. This is the
single rule that keeps concurrency honest as the codebase grows.

## 4. Core type decisions (locked in the skeleton)

These are the types other code will be written against; changing them later is
expensive, so they are decided now.

### 4.1 Identity and interning

- `ObjectId { number: u32, generation: u16 }` — `Copy`, `Eq`, `Hash`, `Ord`.
- **Names are interned per document**: `NameId(u32)` into an append-only,
  concurrent-read name table built during the document-open phase and extended
  under a small lock afterwards (rare — most names appear during open).
  Dictionary keys as `NameId` makes dictionary lookup an integer compare and
  eliminates per-key heap traffic on the hot resolution path. Scaling: a
  5000-page document touches millions of dictionary keys; interning is the
  difference between cache-resident lookups and allocator churn.
- All IR resource references are **typed `u32` newtype handles** (`PathId`,
  `PaintId`, `GlyphRunId`, `ImageId`, …) indexing per-page resource tables.
  Handles, not `Arc<T>`, inside hot arrays: 4 bytes, no refcount traffic,
  trivially serializable for the IR debug dump and schema versioning.

### 4.2 Object model

`PdfObject` per concurrency doc §3, with `Arc`-shared containers so a resolved
object can be published once and shared across workers without cloning bodies.
Strings are `PdfString(Arc<[u8]>)` — PDF strings are byte strings, not UTF-8.
Numbers: `Integer(i64)`, `Real(f64)`. **`f64` everywhere in semantic layers**;
narrowing to `f32` happens only inside a backend lowering step (roadmap §5.4).

### 4.3 The snapshot contract

`DocumentSnapshot` is immutable after `open()` returns. Enforced three ways:

1. No `&mut self` methods after construction; interior mutability only inside
   `ObjectRepository` slots (`OnceLock` publication) and explicitly-synchronized
   caches.
2. `DocumentSnapshot: Send + Sync` asserted by a compile-time test in every
   crate that touches it.
3. Object resolution takes `(&self, ObjectId, &mut ParseContext)` — all scratch,
   recursion tracking, and budget accounting lives in the worker-owned
   `ParseContext`, never in the snapshot.

Resolution strategy is **abstracted behind `ObjectRepository`** so Phase 1 can
ship worker-local caches (Design A) and Phase 3 can switch to shared
`OnceLock` slots (Design B) without touching any caller.

### 4.4 Limits are first-class from day one

`DocumentLimits` (open-time) and `RenderLimits` (render-time) structs exist in
the skeleton and are threaded through every entry point, even while
unenforced. Retrofitting limit plumbing through a mature parser is exactly the
kind of change that breaks everything; threading unused parameters is free.
Limits cover: recursion depth, decompressed bytes, object count, pixel count,
nesting depth, operation budget (concurrency doc "hardest roadblocks").

### 4.5 Error taxonomy

Every layer has its own `thiserror` enum; upper layers wrap lower ones. Two
properties locked now:

- Errors are `Clone`-able via `Arc` where they enter shared caches (a failed
  parse is *published* like a success — `OnceLock<Result<Arc<PdfObject>,
  Arc<ObjectError>>>` — so one worker's failure doesn't force another to
  re-parse and possibly diverge).
- A `Recovery` channel separate from errors: structural repair during open
  produces `RecoveryEvent`s recorded on the snapshot, because "we fixed it"
  must be observable for corpus/differential work without being a failure.

### 4.6 The image-codec boundary

The four image codecs (DCT/JPEG, JPX/JPEG 2000, JBIG2, CCITT) are external
projects and must never be build-time requirements of the engine. The
boundary, locked in the skeleton (`pdf_image::codec`):

- `ImageCodec` trait — one filter per codec, `&self` decode (worker-safe),
  output is a `DecodedImage` in a codec-native format (`Mono1`/`Gray8`/…);
  `/Decode` arrays, color-space mapping, and masks are applied *after* the
  codec, uniformly.
- `CodecRegistry` — injected at renderer construction, never global. An
  empty registry is a valid deployment (full text/vector rendering).
- `DecodeLimits` — pixel/byte caps + cancellation probe threaded into every
  decode; decoders fail, never over-allocate.
- Per-codec `PageFeatures` flags (`NEEDS_DCT/JPX/JBIG2/CCITT`), set by the
  page compiler from filter chains via `codec_feature()`; backends add
  `registry_features()` to their advertised features. Preflight then routes
  or degrades codec-missing pages **before work starts** — a missing decoder
  is an observable routing decision, never a silent blank.

Interim decoders from crates.io (e.g. a JPEG crate) may be registered behind
the same trait as stopgaps/differential oracles; swapping in the in-house
decoder later is a registry-construction change only.

### 4.7 The rendering contract

`pdf-page-ir::CompiledPage` and `pdf-render-api::RenderBackend` are copied
structurally from roadmap §5–6: job-based `submit() -> RenderTicket`,
`supports()` preflight with `SupportLevel`, `OutputResidency::{HostRequired,
BackendPreferred}`, `HostPage` as the stable result. `IR_SCHEMA_VERSION: u32`
constant lives in `pdf-page-ir` from the first commit.

## 5. Concurrency skeleton commitments

- Pipeline shape (roadmap §7 Phase 5): compile pool → bounded queue → render
  pool → reorder buffer. The scheduler crate defines these stage types now with
  trivial single-threaded implementations, so the *shape* is stable while the
  internals grow.
- `MemoryBudget` permit type exists from day one; jobs must acquire estimated
  bytes before render (even while the estimate is a stub returning 0).
  Worker-count alone does not bound memory — the plans are explicit on this.
- Determinism is a test axis: `pdf-test-support` defines the "same output for
  worker counts 1..N" harness signature immediately.

## 6. What the skeleton contains vs. deliberately omits

**Contains:** all 18 crates compiling; core types above; trait definitions;
`ParseContext`; error enums; limits; a `NullBackend` and stub CPU backend that
renders a solid background (proves the API shape end-to-end); CLI with `info`/
`render` subcommands wired to stubs; determinism/diff harness signatures;
`corpus/README` defining corpus layout.

**Omits (first agent task, per phases 0–1):** the lexer, xref parsing, object
resolution, page tree walk — i.e. the real core. Also omits: any codec, any
font work, any rasterization beyond the stub, all WGPU.

## 7. First agent task boundary (Phase 1 core)

Implement, in order, with tests at every step:

1. `pdf-source`: `MmapSource`, `OwnedBytesSource`, `FileReadAtSource`.
2. `pdf-syntax`: lexer (tokens, whitespace/comment/EOL rules per ISO 32000-1
   §7.2–7.3), primitive object parser with `ParseContext` budgets.
3. `pdf-object`: dictionary/array/stream construction, name interner.
4. `pdf-structure`: classic xref tables, xref streams, trailer chains,
   incremental-update precedence, object streams, `startxref` recovery scan.
5. `pdf-document`: `open()` producing an immutable `DocumentSnapshot`; page
   tree indexing with inheritance; worker-local object cache (Design A).

Exit gate (roadmap Phase 1): six threads resolve six different pages'
structure from one snapshot, deterministically, with zero document-wide locks.
The reference for edge-case behavior is `pdfium-reference-source/core/fpdfapi/parser/`
— port *intent* (what inputs it tolerates and what it produces), not classes.
