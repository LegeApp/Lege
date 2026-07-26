# GPU postprocess executor

Status: **experimental implementation available, CPU-default, parity work paused**  
Started: 2026-07-26

This is the focused implementation plan and promotion record for the sole
remaining postprocess item in `handoffs/DEFERRED.md`. It deliberately does not
claim that `pdf-render-wgpu` can paint PDF pages: that rendering backend remains
a truthful stub. This executor starts from the frozen `HostPage` contract and
accelerates the backend-neutral `pdf-postprocess` graph.

## Invariants

1. `CpuPostprocess` remains the normative reference and permanent fallback.
2. All Lege compute clients share one adapter/device/queue selected by
   `lege-gpu`; the renderer must never create a competing wgpu device.
3. One graph is one transaction. `Auto` either completes the whole graph on
   GPU or restarts it from the original `HostPage` on CPU.
4. GPU intermediates stay resident. There is one source upload and one final
   readback; no operation boundary reads back to the host.
5. Forced GPU mode reports initialization/execution errors. It never silently
   falls back.
6. Experimental builds remain CPU-default until every promotion gate below is
   met.

## Public surface

`lege-gpu::compute::SharedGpuContext` is the safe shared compute seam. It
exposes the existing process-wide device/queue and stable adapter metadata,
while retaining the existing `WGPU_BACKEND`, `WGPU_ADAPTER_NAME`, and
`WGPU_REQUIRE_REAL_GPU` selection behavior used by resize, binarization, and
vision inference.

`pdf-postprocess`, behind its optional `gpu` feature, exposes:

- `WgpuPostprocess` — forced experimental GPU executor.
- `AdaptivePostprocess` — policy wrapper.
- `PostprocessPreference::{Cpu,Gpu,Auto}`.
- `LEGE_POSTPROCESS_BACKEND=cpu|gpu|auto`; absent means `cpu` during the
  experiment.
- `PostprocessReport` / `ExecutionStats` — selected backend, adapter, fallback
  reason, elapsed time, upload bytes, and readback bytes.
- `LEGE_GPU_POSTPROCESS_SESSIONS=1..4` — scratch-session pool size, default 2.

`Auto` accepts only discrete and integrated adapters. CPU/software, virtual,
and other adapters are not silently treated as acceleration. An unavailable
GPU, unsupported graph, or execution failure records a typed fallback reason
and reruns the complete graph on CPU.

## Implemented resident vocabulary

The initial WGPU raster engine uses reusable per-session ping-pong storage
buffers, cached pipelines, one u32 per resident pixel, and a separate f32
resize intermediate. It implements the complete graph vocabulary:

- exact integer crop;
- exact premultiplied-RGBA/Gray8 tone LUT;
- exact Rec.709/flat grayscale conversion over the white paper backdrop;
- Nearest, Box, Bilinear, Catmull–Rom, and Lanczos3 resize;
- deterministic Otsu;
- Sauvola and global/local threshold fusion;
- hard threshold, Bayer 4×4, and exact raster-order Floyd–Steinberg;
- MSB-first monochrome packing with 1 = ink.

Floyd–Steinberg intentionally begins as a single ordered GPU invocation. It is
a correctness implementation, not yet an automatic performance candidate.
Sauvola/fusion use resident two-pass 64-bit summed-area tables (paired u32
words); Otsu builds its histogram in parallel. Shared fixed-point CPU/WGSL
threshold math remains the next focused parity step and must not silently
change established CPU output without the corpus regression gate.

## Current validation

Linux Vulkan, NVIDIA RTX 4060 Laptop GPU:

- exact CPU/GPU discrete graph: crop → tone → gray → Bayer → pack;
- exact fixture output for Otsu, Sauvola, fusion, and Floyd–Steinberg;
- all smooth resize filters within one LSB on a varied Gray8 fixture;
- automatic-mode injected GPU failure restarts and completes the graph on CPU;
- the no-GPU-feature build truthfully rejects forced GPU mode.

Run the focused suite:

```bash
cargo test -p pdf-postprocess --features gpu
```

Profile a synthetic scan graph:

```bash
cargo run -p pdf-postprocess --release --features gpu \
  --example postprocess-profile -- standard 10 1200 1600
```

The first argument may be `standard`, `adaptive`, or `floyd`. The tool prints
CPU/GPU milliseconds per page, speedup, output hashes/differing bytes, adapter,
and transfer sizes.

For a hardware-only measurement, prefix the command with
`WGPU_REQUIRE_REAL_GPU=1`. Initial RTX 4060/Vulkan measurements at 1200×1600,
including an RGBA upload, Lanczos resize to 75%, and packed-mono readback:

- standard tone/gray/resize/Otsu/pack: 49.594 ms CPU vs 2.701 ms GPU
  (**18.36×**), one differing packed byte;
- adaptive tone/gray/resize/fusion/pack: 63.283 ms CPU vs 4.117 ms GPU
  (**15.37×**), two differing packed bytes.

Those tiny binary differences are why the fixed-point/corpus parity gate
remains open. They are measurements, not permission to make `Auto` the
default.

## Priority decision

Further promotion work is paused after this experimental foundation. Recent
renderer profiles identify painting/compositing decoded JPEG image data as a
larger upstream hotspot, and `pdf-render-wgpu` is still a stub. The next
focused GPU session should therefore establish GPU image rendering first.
Postprocess remains available for explicit experiments, retains the measured
speedups above, and stays CPU-default until work returns to its parity gates.

## Promotion gates

GPU remains experimental until all of these are recorded here:

1. Exact bytes for the integer-only operations where practical. Threshold and
   resize graphs may differ when corpus review shows equal or better scan
   quality; CPU output is a reference implementation, not a quality oracle.
2. Smooth resize max absolute error ≤ 1; flat fields and premultiplied-alpha
   invariants exact.
3. Corpus quality adjudication for Sauvola/fusion differences, including ink
   balance, weak-text retention, background suppression, and scan-tone
   continuity. Shared fixed-point math is only required if nondeterminism or a
   real quality regression calls for it.
4. Full postprocess corpus run with no hang, device-loss leak, validation
   error, or nondeterminism.
5. Linux Vulkan and Windows DX12 pass. Other platforms stay on CPU until
   independently validated.
6. At least 15% end-to-end speedup for every graph class selected by `Auto`,
   including upload and final readback. Floyd stays CPU-routed until it clears
   this gate.
7. Repeated forced failures/device loss and concurrent session-pool stress
   prove that `Auto` returns the exact CPU rerun and the next GPU job remains
   usable.

After those gates, the default changes from `Cpu` to `Auto`. The current Lege
pipeline is not wired during this experimental phase: its renderer migration
must first reach the postprocess seam. At that point a hidden Lege setting can
exercise this executor before automatic policy is enabled. Lege's tuned
binarization algorithms remain in place unless they are represented by
equivalent graph operations and pass their own quality gates.

## Follow-up: future GPU renderer

The current executor consumes `HostPage`. Once `pdf-render-wgpu` can return a
resident page, add a backend-private handoff that imports its opaque surface
without upload. This changes neither `PostprocessGraph` nor the stable
host-output compatibility API.
