# Compute Scheduler Plan: Shrink Tokio to a Control Plane

Goal: **lower memory and CPU usage** by making Tokio do only orchestration
(channels, select, cancellation, subprocess supervision) and moving all
CPU-bound work onto one explicitly-sized compute substrate with byte-accounted
admission. Throughput gains are a bonus, not the target.

This plan is grounded in a three-way audit of the current code (Tokio usage,
thread pools/compute stages, dataflow/backpressure). It deliberately drops the
viewer-oriented parts of the original advice (viewport generations, prefetch
priorities, thumbnails) — Lege is a batch converter; every page has the same
priority and the only "interactivity" is progress reporting and cancel.

---

## 1. What the code actually does today (audit summary)

### 1.1 Tokio is already *almost* a control plane

- The CLI conversion pipelines run on a **single-threaded** `current_thread`
  runtime hosted on one dedicated OS thread (`ProcessingQueue`,
  `src/progress.rs:1345-1353`). All stage tasks (`tokio::spawn` +
  `FuturesUnordered`) are cooperative coroutines on that one thread.
- Every heavy unit is pushed to **`spawn_blocking`** (~30 call sites), i.e.
  Tokio's elastic blocking pool with the default **512-thread cap** — never
  configured anywhere.
- Exceptions/inconsistencies:
  - The image-folder/ZIP path builds a full **multi-thread** runtime
    (`src/pnginference.rs:198`) while the PDF path is current-thread.
  - The **PDF writer actor runs deflate + PDF serialization inline on the
    runtime thread** (`src/pipeline/helper_functions.rs:1189-1220`), blocking
    the sole Tokio worker during compression.
  - One stray `tokio::fs::read` (`src/main.rs:4617`). No `tokio::process`
    anywhere (subprocesses are `std::process` + drain threads). GUIs use
    Tokio only for UI tasks/timers; bulk data rides flume.

### 1.2 The real problem is the compute substrate, not Tokio

- One **global Rayon pool** (`cores−1`, `src/lib.rs:234-263`) is shared by:
  - up to `page_workers` (default 4) concurrent `process_page_cpu_work`
    blocking closures, each fanning out over dozens of `par_iter` sites
    (`src/clean_gray.rs`, `src/color/binarization.rs`, the CPU Sauvola
    reference executor `lege-gpu/src/vision/reference.rs`), **plus**
  - **jbig2enc-rust** and **jp2lam**, which spin Rayon internally
    (`jbig2enc-rust .../serialize.rs:56`, `jp2lam .../t1.rs`).

  Blocking pool × nested Rayon = runnable threads well above core count.
  `AdaptiveConcurrency` acknowledges this (`src/pipeline/runtime_limits.rs:54-113`)
  but is only used by the **DjVu** path; the PDF path uses flat
  `page_workers=4` with no clamp.
- Three stages are secretly **serialized behind global locks**, so the 4-wide
  config buys buffers, not throughput:
  - pdfium render: `PDFIUM_GLOBAL_LOCK` (`src/pagerender.rs:331`) → 1 at a time.
  - Layout inference: single `inference-actor` std::thread
    (`src/pipeline/inference.rs:187`), blocking `pollster::block_on` GPU polls.
    Its "batching" clones every image and loops single-page detects
    (`src/engine_impl.rs:129-132`) — pure overhead.
  - GPU binarization: process-wide `GPU_BINARIZER` mutex
    (`lege-gpu/src/binarization/mod.rs:125`).

### 1.3 Memory behavior

- Per-page high-res RGB is ~3 MB normally but up to **~75 MB** at the
  slow-OCR render cap (`target_height × 2.5`, capped 6000 px); adaptive-
  concurrency comments assume 50–250 MB per in-flight page for large scans.
- Channels are all bounded, but `channel_capacity` defaults to
  `max(page_workers, channel_buffer_size)` where `channel_buffer_size`
  defaults to `max(heavy_sauvola_concurrency*2, 8)` (`src/pipeline/config.rs:774`)
  → ~4 pages buffered per stage × 3 stages ≈ **~12 high-res buffers resident**
  worst case, most of them queued behind the three serial resources above.
- Nothing counts **bytes**; concurrency counts tasks. The RSS soft brake
  `wait_for_memory_relief` is a **no-op on machines with ≥16 GB RAM**
  (`src/pipeline/helper_functions.rs:764-776`).
- Buffer reuse is essentially nil — every stage allocates fresh per page;
  heavy-Sauvola allocates several full-page f32 scratch tensors per page.
- The **reflow pipeline holds every source page (gray + a deep RGB clone) in a
  Vec simultaneously** (`src/pipeline/reflow_pipeline.rs:231-288`) — unbounded
  by page count.
- Good news the plan preserves: the PDF writer streams pages to disk in
  arrival order and restores logical order via a slot table (no raster
  reorder buffer); the DjVu writer stages only path metadata.

### 1.4 Cancellation

- In-process: per-job `broadcast` + `task.abort()` at await points; a running
  `spawn_blocking` page always runs to completion.
- GUI: spawns the CLI with `stdin=null` and cancels by **SIGTERM/SIGKILL**
  (`GUI/*/src/worker_process.rs:536-570`); the CLI has no SIGTERM handler, so
  DjVu work dirs leak and a running `djvu-encoder` grandchild can be orphaned.

---

## 2. Target architecture

### 2.1 Thread topology (example: 16-core machine)

```
main thread              — CLI arg handling, progress-bus consumer (as today)
lege-control thread      — Tokio current_thread runtime (as today)
                            channels, select, broadcast cancel, stage loops,
                            subprocess supervision, progress throttling
compute pool             — cores−1 workers = THE global Rayon pool
                            page jobs submitted as rayon::spawn'd tasks;
                            nested par_iter (clean_gray, jbig2, jp2, Sauvola)
                            steals within the SAME pool — no oversubscription
pdfium actor             — 1 dedicated thread (formalizes PDFIUM_GLOBAL_LOCK)
gpu actor                — 1 dedicated thread owning ALL wgpu work
                            (layout graph, GPU binarizer, GPU resize, Paddle OCR)
writer thread            — 1 dedicated thread: PDF serialize+deflate / DjVu manifest
ocr lane                 — tesseract only: OCR jobs run as compute-pool jobs,
                            gated by the existing byte/permit admission (no
                            separate pool); WinRT/Paddle go via OS/gpu actor
tokio blocking pool      — capped small (~4); only true blocking I/O remains
```

Peak threads ≈ `cores + 4`, versus today's `cores−1 (rayon) + up to dozens of
blocking threads + 1 inference thread + drains`.

**Key design choice: the compute pool IS the existing global Rayon pool.**
The original advice suggests a bespoke worker pool with worker-local contexts.
For Lege that is the wrong first move, because the third-party encoders
(jbig2enc-rust, jp2lam) and half the in-tree kernels already target the global
Rayon pool and cannot be redirected without forking them. Rayon is already a
fixed-size work-stealing pool; a job spawned into it whose inner `par_iter`
splits further is handled by work stealing with zero extra threads. What Rayon
lacks — admission control, byte accounting, cancellation checkpoints, an async
result path — is exactly the thin layer we build. The `LegeCompute` facade
keeps the substrate swappable later if profiling ever justifies a bespoke pool.

### 2.2 The `LegeCompute` facade (new module `src/compute/`)

```rust
pub struct ComputeJob {
    pub page: Option<usize>,
    pub class: JobClass,          // Render prep, Binarize, Encode, Ocr, Compose, WriterPrep
    pub estimated_bytes: u64,     // drives the memory permit
    pub cancel: CancelToken,      // cheap Arc<AtomicBool> wrapper
}

impl LegeCompute {
    /// Acquire a byte permit (async, backpressure point), then run `f`
    /// on the rayon pool. Result returns via oneshot to the control plane.
    pub async fn run<T: Send>(
        &self, job: ComputeJob, f: impl FnOnce(&JobCtx) -> Result<T> + Send,
    ) -> Result<T>;
}
```

- **Admission = a byte-denominated async semaphore** (`tokio::sync::Semaphore`
  with 1 permit = 1 MiB). Total permits sized once at startup:
  `clamp(total_ram × 0.5, 1 GiB, 8 GiB)` with a CLI/env override
  (`--memory-budget-mb` / `LEGE_MEMORY_BUDGET_MB`). A job acquires
  `estimated_bytes` before it is allowed to *allocate* (i.e. before render,
  not after), and the permit is released when the page's large buffers drop.
  This replaces the RSS-polling `wait_for_memory_relief` (which is dead on
  ≥16 GB machines) with a mechanism that works everywhere and is exact.
- **Execution**: `rayon::spawn` of the closure; completion sent through a
  `tokio::sync::oneshot`. The control plane awaits the oneshot — Tokio never
  runs the CPU work, and the blocking pool is not involved.
- **Cancellation**: `JobCtx::checkpoint()?` — checks the job token + the
  pipeline shutdown flag. Called at natural boundaries (between regions,
  between encode passes, per band in Sauvola/clean_gray). Target cancel
  latency: one band/region, not one page.
- `JobClass` exists for accounting/telemetry and per-class concurrency caps
  (e.g. keep "at most N pages in Encode simultaneously"), **not** priorities —
  batch conversion has no foreground/background distinction worth scheduling.

### 2.3 Actor threads (formalize what is already serial)

- **Pdfium actor**: replace `PDFIUM_GLOBAL_LOCK` + per-call `spawn_blocking`
  (`src/pagerender.rs:331,430,484,553,608,694`) with one owned thread and an
  mpsc of render requests, mirroring the existing `InferenceActor` pattern.
  Same throughput (it was serial anyway), but: no blocking-pool threads
  parked on a mutex, pdfium state stays on one thread (its actual threading
  contract), and the request queue depth becomes an explicit, visible bound.
- **GPU actor**: extend the existing `inference-actor` thread to own *all*
  wgpu submission — layout graph, `WgpuBinarizer` (deleting the
  `GPU_BINARIZER` mutex), GPU resize, and Paddle OCR det/rec. All of these
  already serialize on blocking `device.poll`; putting them on one thread
  removes three independent lock/poll sites and makes GPU queue depth
  observable. Delete the nominal batch path (`InferenceActor` batch collect +
  per-image clones, `src/pipeline/inference.rs:91-148`,
  `src/engine_impl.rs:123-133`) — it buys nothing and clones every image.
  Delete the unused `InferencePool` (`inference.rs:275-298`).
- **Writer thread**: move `page_to_artifact` + `writer.add_page` (deflate)
  off the Tokio thread onto a dedicated writer thread fed by the existing
  bounded channel (`helper_functions.rs:1148`). The Tokio-side actor becomes a
  pure forwarder. DjVu writer stays a Tokio task (it only stages metadata) but
  its `Finalize → run_encoder` subprocess moves off `spawn_blocking` to a
  plain supervised thread.

### 2.4 What Tokio keeps

Bounded mpsc between stages, oneshot results, `select!`, `broadcast`
shutdown, the byte semaphore, GUI timers. That is all — and it all runs
happily on `current_thread`. Concretely:

- `pnginference.rs:198` switches to `current_thread` (same as the PDF path).
- `main.rs:4617` `tokio::fs::read` → `std::fs::read` (it's a one-shot probe).
- After all compute is off the blocking pool, set
  `max_blocking_threads(4)` on the control runtime and audit that only true
  blocking I/O remains (per Tokio's own warning, do this *last*).

---

## 3. Memory-reduction measures (the point of the exercise)

Ordered by expected impact:

1. **Byte-accounted admission** (§2.2). Render is the allocation point, so the
   source stage acquires the page's estimated bytes (render size is known from
   page dims × target height before rendering) and the permit is dropped when
   the encoded page leaves for the writer. Worst-case resident raster memory
   becomes a configured number instead of "channel_capacity × stages × 75 MB".
2. **Shrink queue depths to match reality.** Render, inference, and GPU
   binarize are serial; queuing 4–8 pages in front of each is pure RSS. Set
   `channel_capacity = page_workers` and `render_buffer = inference_buffer = 2`
   (`src/pipeline/runtime_limits.rs:14-26`; kill the
   `max(heavy_sauvola_concurrency*2, 8)` default in `config.rs:774`). With
   byte admission in place these are latency buffers, not memory governors.
3. **Extend `AdaptiveConcurrency` to the PDF path** (today DjVu-only):
   `page_workers = min(configured, adaptive.cpu_workers)` in `from_config`.
4. **Bound the reflow pipeline.** Convert `render_detect_and_reflow`'s
   hold-everything Vec (`reflow_pipeline.rs:231-288`) to a two-pass or
   windowed design: pass 1 renders at analysis resolution only to collect
   detections + doc-wide scale; pass 2 streams full-res pages one at a time
   into the output writer. Drop the unconditional deep `rgb` clone at `:284`.
5. **Per-worker scratch reuse for heavy Sauvola.** The CPU reference executor
   allocates full-page f32 integral/instance-norm tensors per page
   (`lege-gpu/src/vision/reference.rs`); give the executor an optional arena
   (thread-local per rayon worker) so ~2.5 s/page jobs stop churning hundreds
   of MB of allocations. Same pattern later for `clean_gray` if profiling
   agrees.
6. **Kill the batch-path image clones** in `InferenceActor` (§2.3).
7. **Cap EPUB-sidecar hOCR accumulation** (`pdf_tokio_pipeline.rs:3022`,
   `djvu_pipeline.rs:523`): spill page hOCR strings to the work dir and
   assemble the EPUB from disk at finalize (they are per-page independent).

CPU-reduction measures come mostly for free from the same changes: no
oversubscription (one pool), no blocking-pool thread churn (keep-alive
spawning/parking), no batch clones, no RSS polling loop, fewer wake-ups from
the writer no longer stalling the control thread.

---

## 4. Migration plan (each phase shippable, verified independently)

### Phase 0 — Instrument (no behavior change)
- Add a `--debug-runtime-stats` dump: peak thread count
  (`/proc/self/status` Threads), peak RSS, per-stage in-flight gauges, rayon
  pool size, blocking-pool usage (count `spawn_blocking` entries/exits).
- Record baselines on 2–3 representative documents (small clean PDF, large
  scanned book with slow OCR, DjVu output) — thread count over time, peak
  RSS, wall time, total CPU time (`/usr/bin/time -v`). These are the numbers
  every later phase must not regress (RSS/CPU must go *down*).

### Phase 1 — `LegeCompute` facade over the existing substrate
- New `src/compute/` module: `ComputeJob`, byte semaphore, `run()` backed by
  `rayon::spawn` + oneshot. Wire the budget from RAM detection with the
  override flag.
- Port the **processing stage** first: `process_single_page`'s two
  `spawn_blocking`s (`pdf_tokio_pipeline.rs:365` + encode sites `:2109,2259,
  2394,2506`) become `compute.run(...)` calls. `ENCODE_SEMAPHORE` becomes a
  per-class cap inside `LegeCompute`; delete the standalone
  (`helper_functions.rs:24-39`).
- Port the DjVu binarize/compose `spawn_blocking`s (`djvu_pipeline.rs:585,832`)
  and the OCR call sites (`src/ocr/fast.rs:92,138`, `src/ocr/slow.rs:126,291`;
  `OCR_SEMAPHORE` becomes a `JobClass::Ocr` cap).
- Verify: peak thread count drops to ≈ cores + blocking-I/O residue; RSS on
  the scanned-book baseline drops (admission now binds); output byte-identical
  on all baselines.

### Phase 2 — Actor consolidation
- Pdfium actor thread replacing lock + `spawn_blocking` (`src/pagerender.rs`).
- GPU actor: fold `WgpuBinarizer` and GPU resize submission onto the
  inference-actor thread; delete `GPU_BINARIZER` mutex and the batch/clone
  path and `InferencePool`.
- Writer thread for PDF serialize+deflate; DjVu `run_encoder` onto a
  supervised plain thread.
- Verify: control-runtime never blocks >1 ms (add a debug watchdog task);
  same outputs; GPU stage timings unchanged or better.

### Phase 3 — Queue/limit tightening
- `runtime_limits.rs`: capacities per §3.2, adaptive clamp on the PDF path,
  delete `wait_for_memory_relief` + `MemoryMonitor` RSS polling
  (`helper_functions.rs:653-870`) in favor of the byte semaphore.
- Verify: RSS on the large-scan baseline ≈ budget; wall time within ~5% of
  baseline (serial resources dominate, so tighter queues shouldn't hurt).

### Phase 4 — Tokio diet + cancellation hygiene
- `pnginference.rs` → current_thread; `tokio::fs` removal;
  `max_blocking_threads(4)` after auditing remaining users.
- Cooperative checkpoints in the long kernels (region loops in
  `process_page_cpu_work`, band loops in Sauvola/clean_gray, between OCR
  regions); wire the existing broadcast shutdown into `CancelToken`.
- CLI installs a **SIGTERM handler** that triggers the same broadcast cancel,
  waits briefly for checkpoint-based unwinding, kills the `djvu-encoder`
  child, and cleans the work dir — fixing the GUI-kill orphan/leak
  (`GUI/*/worker_process.rs:556-570` can then keep SIGTERM as its mechanism).
- Track the detached bookmark task (`pdf_tokio_pipeline.rs:3168-3177`) in the
  stage-join set; add a timeout around `djvu-encoder` `child.wait()`.
- Verify: GUI cancel of a mid-DjVu job leaves no work dir and no orphan
  encoder; cancel latency < ~100 ms on the scanned-book baseline.

### Phase 5 — Allocation reuse (profile-driven, optional)
- Sauvola executor arena; reflow streaming rewrite; hOCR spill-to-disk.
  Each gated on the Phase 0 numbers showing it still matters.

---

## 5. Explicitly rejected from the original advice

- **Custom async runtime / replacing Tokio's I/O layer** — Lege has almost no
  async I/O; the control plane is one thread of channels. Nothing to win.
- **Priority classes (Interactive/Visible/Prefetch…) and viewport
  generations** — batch converter; pages are homogeneous. `JobClass` caps are
  kept, priorities are not.
- **Bespoke worker pool with worker-local codec contexts as step one** — the
  third-party encoders pin us to the global Rayon pool; fighting that first
  is high-churn, zero-memory-win. The facade keeps the option open.
- **Per-worker font/raster context affinity** — pdfium state lives on its
  actor thread; there is no other stateful raster context worth pinning.

## 6. Risks

- **Byte estimates wrong** → budget too tight stalls the pipeline (mitigate:
  floor of 2 concurrent pages regardless of estimate; log
  estimated-vs-actual in debug-runtime-stats), too loose reproduces today
  (still bounded, unlike today on ≥16 GB machines).
- **Rayon jobs that block** (tesseract calls, any residual lock waits) occupy
  a pool worker. Audit in Phase 1: anything that *waits* rather than
  *computes* goes to an actor/lane thread, not the pool.
- **`rayon::spawn` FIFO-ish injection vs today's task ordering** could change
  page completion order — harmless (writers already handle out-of-order) but
  progress "current page" may look jumpier; counters are already atomic.
- **jbig2/jp2 internal rayon depth** on the shared pool can starve short jobs
  behind a long encode split. Acceptable for batch; if it bites, cap
  `JobClass::Encode` concurrency (already supported by the facade).
