# Winograd F(2,3) Integration Status

This document is the current handoff for the YOLO Winograd work. The original
Winograd math and GPU kernels were already validated in `ops/winograd.rs`; the
graph plumbing has now been implemented.

## Current State

- `PreparedGraph::from_model_with_input_dims` runs `onnx/winograd_rewrite.rs`
  immediately after normal ONNX lowering.
- Eligible convs are rewritten only when they are `3x3 s1 d1 group1` with
  `pads == [1,1,1,1]` and their weights are available as float constants.
- The rewrite transforms each weight tensor once at graph prep into
  `{weight}__wino_U` with shape `[16, Cout, Cin]` and layout
  `U[e*Cout*Cin + co*Cin + ci]`.
- Each eligible conv is replaced with:
  - `WinogradInputTransform`: `x[1,Cin,H,W] -> V[16,Cin,P]`
  - `WinogradBatchedGemm`: `(U[16,Cout,Cin], V[16,Cin,P]) -> M[16,Cout,P]`
  - `WinogradOutputTransform`: `M[16,Cout,P] (+ bias) -> y[1,Cout,H,W]`
- The original conv output name is preserved on `WinogradOutputTransform`, so
  downstream consumers and SiLU fusion stay attached.
- `ops/compile.rs`, `vision/reference.rs`, `ops/mod.rs`, and
  `runtime/compiled.rs` are wired for the three new planned op kinds.
- Profiler labels are now `WinogradInput`, `WinogradGemm`, and
  `WinogradOutput`.

## Validation Done

- `cargo check -p lege-gpu` passes.
- `cargo test -p lege-gpu winograd --release -- --test-threads=1 --nocapture`
  passes all Winograd correctness tests.
- `cargo test -p lege-gpu rewrites_runtime_yolo_model_when_available --release -- --nocapture`
  loads `/home/dk/Desktop/lege-run/installer/linux64/yolo-layout.onnx`, confirms
  exactly 38 convs are rewritten, and compiles the rewritten graph into WGPU
  pipelines.
- On the current sandbox run, rewritten YOLO compiled as:
  `604 steps`, `22 pipelines`, `480.2 MB` resident slots, `120.2 MB` constants.
- `cargo build --profile debug-fast --bin lege` passes on Linux after the OCR
  fixes. Use `target/debug-fast/lege` for iteration; this profile is optimized
  with LTO disabled.
- A 3-page Linux/Vulkan CLI run on
  `/mnt/Samsung980_1TB/to-sort/test/wheelsofcommerce00brau_1.pdf` selected
  `NVIDIA GeForce RTX 4060 Laptop GPU (Vulkan, DiscreteGpu)` and completed.
- `LEGE_DISABLE_WINOGRAD=1` is now available as a same-binary control switch for
  direct-conv baseline timing. Default behavior still enables Winograd.

## Current Pivot Before Timing

1. The full `lege` CLI build exposed unrelated Linux OCR compile errors in
   `lege-ocr/src/engine.rs`. Those have now been fixed enough for
   `cargo build --release --bin lege` to complete. Remaining OCR warnings are
   not blocking the timing harness.
2. The managed sandbox initially enumerated only `llvmpipe` for WGPU. Timing on
   that software Vulkan adapter is useless. A device-node probe outside the
   sandbox sees `/dev/dri`, `/dev/nvidia0`, `/dev/nvidiactl`, and
   `/dev/nvidia-uvm`, so timing runs need real GPU device visibility rather than
   more Winograd code changes.
3. The code supports adapter selection with `WGPU_ADAPTER_NAME`; use it with
   `WGPU_BACKEND=vulkan` and, on this NVIDIA Optimus laptop, try the standard
   NVIDIA Vulkan selection variables listed below until wgpu enumerates the
   GeForce RTX 4060 instead of llvmpipe.

## Timing Harness

Persistent local paths are now recorded in root `AGENTS.md`:

- Test PDF:
  `/mnt/Samsung980_1TB/to-sort/test/wheelsofcommerce00brau_1.pdf`
- Linux runtime/model/PDFium folder:
  `/home/dk/Desktop/lege-run/installer/linux64/`

Use `debug-fast` for local timing iteration unless a final release/LTO run is
explicitly needed. Once the real Vulkan adapter is selected, run a page subset
with profiling enabled:

```bash
LD_LIBRARY_PATH=/home/dk/Desktop/lege-run/installer/linux64 \
LEGE_DATA_DIR=/home/dk/Desktop/lege-run/installer/linux64 \
LEGE_INFERENCE_PROFILE=1 \
LEGE_CONV_SHAPES=1 \
WGPU_REQUIRE_REAL_GPU=1 \
WGPU_BACKEND=vulkan \
WGPU_ADAPTER_NAME=NVIDIA \
__NV_PRIME_RENDER_OFFLOAD=1 \
__GLX_VENDOR_LIBRARY_NAME=nvidia \
VK_LAYER_NV_optimus=NVIDIA_only \
./target/debug-fast/lege /mnt/Samsung980_1TB/to-sort/test/wheelsofcommerce00brau_1.pdf 1-3
```

If multiple adapters enumerate, add:

```bash
WGPU_ADAPTER_NAME=<substring of real GPU adapter name>
```

On the current Linux machine, useful adapter-selection variables to try are:

```bash
WGPU_ADAPTER_NAME=NVIDIA
__NV_PRIME_RENDER_OFFLOAD=1
__GLX_VENDOR_LIBRARY_NAME=nvidia
VK_LAYER_NV_optimus=NVIDIA_only
DRI_PRIME=1
```

`WGPU_REQUIRE_REAL_GPU=1` should stay enabled for benchmark runs so a hidden
device-node problem fails fast instead of producing llvmpipe timings.

Compare against the last Linux/Vulkan baseline, not the old Windows/DX12
absolute time. DirectX 12 was previously about 40-50 ms/page faster than Vulkan
on this workload, so the relevant question is Vulkan-relative improvement.

## Current Linux/Vulkan Timing

Latest successful command used `target/debug-fast/lege`, real NVIDIA Vulkan
selection, `LEGE_INFERENCE_PROFILE=1`, and `LEGE_CONV_SHAPES=1` on pages `1-3`.

- Adapter list included RTX 4060, Intel Iris Xe, and llvmpipe.
- Selected adapter: `NVIDIA GeForce RTX 4060 Laptop GPU (Vulkan, DiscreteGpu)`.
- Build-time graph stats: `604 steps`, `22 pipelines`, `413 ms` build total.
- One-shot inference profile:
  - `GPU busy(kernels)=131.3 ms`
  - `GPU span=130.7 ms`
  - `profile wall=150.2 ms`
  - `compiled.run` first page: `encode+submit=5.0 ms`, `readback=116.4 ms`,
    `total=121.5 ms`
- Top Winograd entries:
  - `WinogradGemm`: `24.1 ms` across `38` dispatches.
  - `WinogradInput`: `4.7 ms` across `38` dispatches.
  - `WinogradOutput` was below the top-10 cutoff in that run.

Same-machine direct-conv control run used the same command plus
`LEGE_DISABLE_WINOGRAD=1`:

- Build-time graph stats: `528 steps`, `20 pipelines`, `215 ms` build total.
- One-shot inference profile:
  - `GPU busy(kernels)=140.9 ms`
  - `GPU span=123.4 ms`
  - `profile wall=157.1 ms`
  - `compiled.run` first page: `encode+submit=4.1 ms`, `readback=118.4 ms`,
    `total=122.4 ms`
- Direct 3x3 baseline entry:
  - `Conv3x3Co8Sp2x2 g1 k3x3 s1x1 d1x1`: `42.2 ms` across `38` dispatches.

Current interpretation: Winograd does make a kernel-time dent on Linux/Vulkan.
The direct 38-conv block was `42.2 ms`; the visible Winograd replacement cost
was `24.1 ms + 4.7 ms`, with output transform below the top-10 cutoff. Whole
graph GPU busy improved from `140.9 ms` to `131.3 ms`. End-to-end page timing
did not materially move in this short run because the reported `readback`/wait
portion stayed around `116-118 ms`.

## Expected Profile Check

The old direct-conv profile had 38 `Conv3x3Co8Sp2x2` dispatches totaling about
39 ms of GPU busy time on the Windows/DX12 run. In the rewritten graph, those
should disappear and be replaced by `WinogradInput`, `WinogradGemm`, and
`WinogradOutput`. The integration is only a win if their combined Vulkan GPU
busy time is meaningfully lower than the prior Vulkan direct-conv total, with
detections unchanged.

For same-binary Vulkan A/B runs, use `LEGE_DISABLE_WINOGRAD=1` for the direct
conv baseline and omit it for the Winograd run.

## Watch List

- Liveness: `V` must live from input transform to GEMM, and `M` from GEMM to
  output transform. They are modeled as real op outputs/inputs, so the resident
  planner should handle this, but the page harness is the real test.
- VRAM: `V` and `M` are transient but large, especially at 192 channels. Watch
  resident slot size and peak memory.
- Numerics: F(2,3) can drift slightly in deeper layers. Check final detections,
  not only tensor max-diff.
- Do not revisit fused Winograd or conv+SiLU fusion unless profiling shows a new
  reason; previous notes explain why those paths were rejected.
