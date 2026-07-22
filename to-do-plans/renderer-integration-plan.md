# Renderer Integration Plan: pdf-renderer into Lege, pdfium removed

Status: planning (renderer not yet moved; it lives at
`pdfium-port-plan/pdf-renderer`, production-readiness pass complete
2026-07-21). Companion to — and partial successor of —
`compute-scheduler-plan.md`. That plan's audit (§1) remains the factual
baseline for Lege's current Tokio/Rayon/actor topology and is not repeated
here; where the two disagree, this document wins, because three of its
premises change the moment the renderer lands:

1. **The pdfium actor (its §2.3) is never built.** pdfium is removed
   entirely, with no fallback. Page rendering stops being a serial resource —
   the largest structural fact in the old plan's queueing analysis.
2. **Priorities and viewport generations come back off the rejected list
   (its §5).** A viewer will be built alongside lege-pdf and lege-process.
   This plan does not design the viewer, but the scheduler seam is shaped so
   the viewer plugs in without a second migration: the priority and
   generation fields exist from day one, batch mode simply submits everything
   at one priority.
3. **Worker-local contexts stop being a rejected idea.** The renderer brings
   genuinely stateful per-worker machinery (raster scratch, font/glyph
   caches, pooled JBIG2 decoder contexts) that pays for thread affinity.
   The substrate stays the global Rayon pool (the old plan's reasoning about
   jbig2enc-rust/jp2lam internal rayon still holds); worker-local state rides
   `thread_local!` on that pool rather than a bespoke pool.

Everything else in compute-scheduler-plan.md — byte-accounted admission, the
`LegeCompute` facade, actor consolidation for the GPU and writer, queue
tightening, Tokio diet, SIGTERM hygiene — is kept and extended below.

---

## 1. The move itself (mechanical, first)

- Move the `pdf-renderer` workspace (crates/, corpus/, tools/, fuzz/, docs)
  from `pdfium-port-plan/` to `Lege-ecosystem/lege-pdf/render/`, replacing
  the README placeholder. Git: preserve history (the repo moves whole; it has
  no remote by choice — it simply changes directory).
- Codec path deps flip from the temporary
  `../../Lege-ecosystem/lege-codecs/{jp2lam,jbig2enc-rust}` to
  `../../lege-codecs/{jp2lam,jbig2enc-rust}` — the two-line manifest change
  the temporary paths were designed for.
- `lege-pdf-write` adopts `pdf-geom`: delete its mirrored `Affine`/`PdfRect`
  in `write/src/types.rs`, depend on `render/crates/pdf-geom`, mechanical
  rename (`Affine` → `Matrix`, `PdfRect` → `Rect`). This is the swap the
  mirror types were built for; do it in one commit with no behavior change.
- Toolchain: the whole ecosystem pins 1.97.1 (renderer and jp2lam already
  do). Restore/align `rust-toolchain.toml` at the Lege and Lege-ecosystem
  roots once the current repo refactor settles.
- `tools/pdfium-diff` stays: it is the renderer's differential oracle and
  loads pdfium via dlopen purely to grade output. Removing pdfium *from
  Lege* does not mean losing the oracle. It remains out-of-workspace,
  dev-only, never a dependency of anything shipped.

## 2. pdfium removal — complete, no fallback

Inventory (from the compute-scheduler-plan audit): all call sites live in
`src/pagerender.rs` (:331 lock, :430, :484, :553, :608, :694), guarded by
`PDFIUM_GLOBAL_LOCK`, invoked through `spawn_blocking`. Bundled libraries
ship with the GUI/installers (incl. the planned macOS `.dmg`'s
`libpdfium.dylib`).

- Delete `PDFIUM_GLOBAL_LOCK`, every pdfium call path in `pagerender.rs`,
  the pdfium FFI/bindings, and library bundling from every installer target.
  No compatibility shim, no runtime fallback, no feature flag.
- Replace with a thin `lege-pdf-render` consumer layer (inside lege-pdf,
  next to `render/`):
  - `DocumentSnapshot::open` / `open_with_password` once per document
    (immutable, `Send + Sync` — sharable across every worker without a lock;
    this alone deletes the reason the render stage was serial).
  - Per page: `PageCompiler::compile` → `CpuBackend` render to `HostPage`,
    executed inside the page's compute job (§4) — not via the renderer's own
    `pdf-render-scheduler`, which is for standalone use; in Lege the
    LegeCompute pool is the only pool (§3).
  - Format mapping at the boundary: `HostPage` → the gray / RGB buffers the
    MRC, binarization, OCR, and layout stages consume today, at the
    resolutions they request (render-at-target-scale replaces
    render-then-resize wherever the consumer allows it — one fewer full-page
    copy).
  - Intake prechecks: `pdf-read::examine` replaces any pdfium-derived page
    counting/metadata probing, and gives the intake path encryption info,
    xref health, and per-page compile status pdfium never exposed.
- Behavioral parity notes (each verified during Phase R1 below):
  rotation (/Rotate) is handled by the renderer's page transform; passwords
  via `open_with_password` (user or owner, R2–R6); annotations render by
  default (harmless for scans, matches viewer expectations later); rendering
  is deterministic across workers (bundled fonts, no system-font default).
- The DjVu input path is untouched (djvulibrust); only PDF rasterization
  changes producers.

## 3. The Tokio ⇄ compute seam (`LegeCompute`, evolved)

Keep the facade and byte semaphore from compute-scheduler-plan §2.2
unchanged in spirit; extend the job metadata so the same seam serves batch
today and the viewer later:

```rust
pub struct ComputeJob {
    pub document: DocumentId,
    pub page: Option<u32>,
    pub class: JobClass,        // Compile, Render, ImageDecode, Jbig2Decode,
                                // Binarize, Encode, Ocr, Layout, Compose,
                                // WriterPrep, Thumbnail (viewer, later)
    pub priority: JobPriority,  // Interactive | Visible | Adjacent |
                                // Prefetch | Background | Maintenance
                                // batch mode: everything Background
    pub generation: u64,        // viewport/config generation; batch: 0
    pub estimated_bytes: u64,   // byte-semaphore admission (unchanged)
    pub cancel: CancelToken,    // Arc<AtomicBool> + generation check
}
```

- **Tokio keeps**: the current_thread control runtime, bounded mpsc between
  control-plane stages, oneshot results, `select!`, broadcast shutdown, the
  byte semaphore, subprocess supervision, progress throttling, GUI timers.
  Exactly the diet the old plan's Phase 4 prescribes (current_thread for
  pnginference, `max_blocking_threads(4)` last, SIGTERM handler, etc. — all
  retained).
- **LegeCompute executes**: everything CPU-bound, as `rayon::spawn` onto the
  single global pool, result via oneshot. The renderer's compile and render
  are jobs like any other. The renderer's own scheduler crate is not wired
  in; its two good ideas (memory permits, arrival-order writer) already
  exist Lege-side as the byte semaphore and the slot-table writer.
- **Priority handling**: a small priority wrapper in front of `rayon::spawn`
  — LegeCompute holds one queue per priority class and feeds rayon a bounded
  number of in-flight jobs (≈ pool width + small overshoot), always draining
  the highest class first. This costs one mutex-protected pop per *job*
  (page-granularity, §4 — thousands of instructions apart, no contention) and
  gives the viewer preemption-at-admission without touching rayon internals.
  Batch conversion sees a single class and behaves exactly like plain spawn.
- **Generations**: `JobCtx::checkpoint()` fails fast when
  `job.generation != current_generation` — free staleness cancellation for
  the viewer, inert (generation 0) for batch.
- **Worker-local state** (new): `thread_local!` on the rayon pool, lazily
  initialized, owning: renderer `CpuWorkerContext` (raster scratch, glyph
  cache shard handle), pooled `Jbig2DecoderContext`, image-decode scratch,
  OCR context where applicable. Rayon's fixed threads make these live for
  the process; nothing is rebuilt per page. (This is the old plan's §5
  rejection reversed — justified now because the renderer's caches are the
  hottest state in the process.)

## 4. Page-owned pipeline: the least-contention execution model

Today's shape — stage pipeline with bounded channels between render →
binarize → inference → OCR → encode, four pages wide, three stages
serialized behind locks — exists because pdfium and the GPU were serial.
With rendering parallel and inference made concurrent (§5), the channels
between CPU stages stop paying for themselves: every handoff is a queue, a
wake-up, and a cache handover.

Target model: **one compute job = one page, end to end.** A worker takes a
page and runs compile → render → gray/binarize → layout (await) → OCR →
MRC/encode → hand off to writer, all in-thread, using its thread-local
contexts. The only cross-thread interactions per page, with their contention
character:

| shared resource | mechanism | contention |
|---|---|---|
| document snapshot | `Arc<DocumentSnapshot>`, immutable | none (reads only) |
| byte budget | async semaphore, acquired once pre-render | one atomic path per page |
| priority queue pop | short mutex, once per page | negligible at page granularity |
| GPU inference | submit + await completion (§5) | queue.submit's internal lock, µs-scale |
| writer | bounded mpsc `blocking_send`, once per page | one channel op per page |
| progress | atomics | none |
| glyph/font caches | sharded LRU (renderer-owned, 8-way) | already built for this |

Nothing else is shared. No stage channels, no per-stage in-flight gauges, no
intermediate buffers parked in queues — the ~12-high-res-buffers-resident
worst case from the old plan's §1.3 collapses to `min(pool width, byte
budget)` pages, each fully owned by one worker. Cancellation checkpoints sit
between the stages inside the job (the old plan's Phase 4 checkpoint list,
unchanged) plus the band/region-level checks inside the long kernels.

Codec-internal `par_iter` (clean_gray, Sauvola, jbig2, jp2) still fans out
into the same pool via work stealing when workers are idle — narrow pages
don't strand cores — and naturally stops stealing when all workers hold
pages of their own. That is rayon behaving exactly as designed, and it is
the reason the substrate stays rayon.

The stage-pipeline code path is deleted, not kept as an alternative: two
execution models means two backpressure designs and double the cancellation
surface. The writer keeps its arrival-order slot table (already
out-of-order-safe).

## 5. Inference: from serial actor to concurrent sessions

Current state (old plan §1.2): one `inference-actor` std::thread owns the
wgpu layout graph; every page blocks on `pollster::block_on`; the "batch"
path clones images to loop single-page detects; GPU binarization serializes
on a separate process-wide mutex. The old plan consolidated all GPU work
onto that one actor thread — right call when render was the bottleneck,
wrong call once it isn't: layout-mode throughput would cap at one page's
inference latency regardless of core count.

Design for concurrent inference with minimal contention, in order of what
shares and what doesn't:

- **One `wgpu::Device` + `Queue`, process-wide.** wgpu's types are
  internally synchronized; command encoding is fully parallel per thread and
  `queue.submit` holds an internal lock for microseconds. One device keeps
  one copy of everything resident.
- **Weights uploaded once, shared immutably.** Model weight buffers are
  read-only `wgpu::Buffer`s referenced by every session's bind groups. N
  concurrent sessions cost N× *activations*, 1× *weights*.
- **A pool of K `InferenceSession`s** (K ≈ 2–4, sized by measured VRAM per
  activation set against a VRAM budget — a second, GPU-side byte semaphore in
  LegeCompute, `JobClass::Layout` cap). Each session owns: activation and
  staging buffers, bind groups, its own encoder per run. Sessions are
  checked out per inference, returned after readback — page workers never
  hold a session while doing CPU work.
- **One poller replaces per-call `pollster::block_on`.** A single `gpu-poll`
  thread runs `device.poll(Wait)`; completion flows via `map_async`
  callbacks that fire oneshots back to the awaiting page job. Every blocking
  poll site disappears; submission threads never spin. (If wgpu's
  submission-index wait API suffices by then, the poll thread reduces to a
  trivial loop; either way there is exactly one.)
- **The page worker's view**: `layout::infer(&session_pool, &gray_page).await`
  — encode preprocessing on the worker (CPU resize/normalize into the
  session's staging buffer), submit, yield until the oneshot fires, resume
  with boxes. During the await the worker's thread is *not* parked on GPU
  progress — the job model in §4 runs the await on the control plane side of
  the oneshot… but note the page job is synchronous rayon code. Resolution:
  inference is the one stage structured as *submit from the worker, block on
  a plain `std::sync::mpsc`/condvar completion* — the worker does block, but
  on a signal, not on `device.poll`, and with K sessions in flight the pool
  loses at most (workers − K) threads to these short waits when the GPU is
  the bottleneck — which is precisely the backpressure you want, applied at
  the true bottleneck, with zero lock contention. If profiling shows those
  blocked workers matter, the escape hatch is splitting the page job at the
  inference boundary (pre-layout job + post-layout continuation submitted on
  completion) — the facade signature already permits it; don't build it
  until measured.
- **GPU binarization and Paddle OCR** join the same session-pool pattern
  (delete `GPU_BINARIZER`'s mutex; each gets its own small session pool or
  shares the layout pool's device and poller). The old plan's single "GPU
  actor" thread is thereby replaced entirely by: shared device + session
  pools + one poller. Delete `InferenceActor`, its batch/clone path, and
  `InferencePool` as before.
- **CPU fallback** (no GPU): sessions become per-worker thread-local CPU
  graph states sharing `Arc` weights — same pool/checkout API, contention-free
  by construction.

## 6. Migration phases (each shippable; baselines per compute-scheduler-plan Phase 0)

Phase R0 — instrument (unchanged from old plan Phase 0; record the same
baselines plus per-stage share of wall time in layout-mode, to prove where
the bottleneck moves).

Phase R1 — move + replace rendering:
§1 move; §2 pdfium removal; page rendering as `LegeCompute` jobs (facade per
old-plan Phase 1, which lands together with this); stage pipeline still
intact otherwise. Verify: output parity on the three baseline docs vs
pdfium-era goldens (visual + downstream-output equivalence, not
pixel-identity — the engines legitimately differ in AA), wall time on
render-bound docs drops roughly with core count, peak threads ≈ cores + 4.

Phase R2 — actor consolidation minus pdfium (old plan Phase 2, amended):
writer thread and GPU work as described there, except the GPU end state is
§5's session pools + poller rather than one actor thread. Delete
InferenceActor/InferencePool/batch path/GPU_BINARIZER.

Phase R3 — page-owned pipeline (§4): collapse the stage channels for the
PDF path; DjVu path follows if the win repeats. Verify: RSS ≈ byte budget,
no regression in wall time on OCR-heavy baselines, cancel latency
< 100 ms.

Phase R4 — inference concurrency (§5): session pool K=2, then measure K=3,4.
Verify in layout-mode: pages/s scales until GPU saturation; no session
starvation deadlock (K permits acquired only after render permit — strict
ordering, no cyclic wait); VRAM within budget.

Phase R5 — Tokio diet + cancellation hygiene (old plan Phase 4 verbatim) and
allocation reuse (old plan Phase 5, re-profiled — the page-owned model
already removes most queue-resident buffers).

Viewer (separate effort, later): plugs into the same facade — Interactive/
Visible priorities, generations, `Thumbnail` class, per-document snapshot
reuse. Nothing in R0–R5 needs revisiting for it; that is the point of the
seam.

## 7. Risks

- **Renderer output vs pdfium downstream expectations**: MRC/OCR/layout were
  tuned on pdfium's rasterization (AA character, stencil weights). The
  renderer is oracle-matched to pdfium within inkΔ tolerances, but Phase R1
  must A/B the *end products* (MRC output size/quality, OCR accuracy on the
  baselines), not just the rasters. Divergences feed back as renderer
  fixture cases — the diff oracle stays available for exactly this.
- **Blocked workers during inference waits** (§5): bounded by design at
  (workers − K); measured escape hatch documented. Do not preemptively build
  the continuation split.
- **wgpu multi-thread submission** is well-supported but the in-house graph
  was written single-threaded: audit for hidden globals (staging belt,
  encoder reuse, readback buffers) during session-pool extraction — that
  audit IS the work of §5's second bullet.
- **Byte estimates for render**: known pre-render from page dims × target
  scale — more predictable than the pdfium era; keep the floor-of-2-pages
  rule and estimated-vs-actual logging.
- **One pool starving short jobs behind long encodes** — unchanged from old
  plan §6; the per-class caps answer it; page-granularity jobs make it rarer.
- **Priority queue in front of rayon adds a scheduling layer** — kept
  trivially small (one pop per page); if the viewer later needs preemption
  *within* a running page, that is viewer-plan territory (band-granularity
  render jobs), not this plan's.
