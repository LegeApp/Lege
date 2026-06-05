# Winograd F(2,3) — integration handoff

Resume doc for wiring the **validated** Winograd GPU pipeline into the executor.
All math + GPU kernels are done, tested, and committed; what remains is graph
plumbing. Companion context: `lege-gpu/model-findings.md` → "OPTIMIZATION 3" and
the "YOLO inference perf" section (why YOLO is compute-bound, the harness, the
per-kernel baseline).

## Where we are

Branch: **`yolo-dilated-conv-2x2-tile`**. Relevant commits (newest last):

- `a280133e` dilated conv 2×2 tile (d2..d5) — **shipped, −10.3ms**, GPU busy 147.4→137.1ms
- `82e859f2` Winograd transform foundation (CPU helpers + input/output GPU kernels)
- `ab79c4f4` Winograd batched 16-GEMM + **full 3-pass GPU pipeline validated**
- `89ea40fa` docs

Current measured baseline after the dilated work: **GPU busy ≈ 137ms / page** (pages 1-3,
DX12 / RTX 4060). The 38 hot 3×3 s1d1 convs (`Conv3x3Co8Sp2x2`) are **39.4ms** of that.

## The target conv

38 convs, all `k3x3 s1x1 d1x1 group1`, currently routed to `CONV3X3_CO8_SP2X2_S1D1_WGSL` in
`compile.rs` (the branch `kh==3 && kw==3 && group==1 && strides==[1,1] && dilations==[1,1]`).
Channel mix (cout, cin): 48/48, 64/64, 96/96 (×12), 192/192 (×25), plus a few 96/48 etc.
**Pad is [1,1,1,1]** (same-size). The F(2,3) input-transform kernel hardcodes pad=1, so gate
Winograd on `pads == [1,1,1,1]` and fall back to the existing direct kernel otherwise.

## Why 3-pass, not a fused kernel

A fused Winograd kernel needs 16 accumulators *per output channel*. Matching the direct
kernel's 8-channels/workgroup reuse = 128 registers (spills); fewer channels → re-load the
input patch → memory-bound. Passes are nearly free here (profiler shows **0% inter-dispatch
idle** — the DX12 driver overlaps passes), so the clean 3-pass form wins. Don't revisit the
fused approach.

## What's already built and tested (in `ops/winograd.rs`)

**CPU transform helpers** (the numeric source of truth — reuse `weight_transform_f23` at
prep, do NOT re-derive the B/G/A matrices):
- `weight_transform_f23(&[f32;9]) -> [f32;16]`   // U = G·g·Gᵀ, row-major 4×4
- `input_transform_f23(&[f32;16]) -> [f32;16]`   // V = Bᵀ·d·B
- `output_transform_f23(&[f32;16]) -> [f32;4]`   // Y = Aᵀ·m·A

**GPU kernels + wrappers** (all validated vs the CPU oracle and direct conv):
- `WINOGRAD_INPUT_TRANSFORM_WGSL` / `run_input_transform(ctx, x) -> (V: Vec<f32>, p, ntw, nth)`
  - params: `[cin, h, w, ntw, nth]`; dispatch `ceil(cin*P/256)`; bindings `[x, V(out), params]`
- `WINOGRAD_BATCHED_GEMM_WGSL` / `run_batched_gemm(ctx, u, v, cout, cin, p) -> M: Vec<f32>`
  - params: `[cout, cin, p]`; dispatch `(ceil(cout/64), ceil(p/64), 16)`; bindings `[U, V, M(out), params]`
- `WINOGRAD_OUTPUT_TRANSFORM_WGSL` / `run_output_transform(ctx, m, bias, cout, h, w, use_bias) -> Tensor`
  - params: `[cout, h, w, ntw, nth, use_bias]`; dispatch `ceil(cout*P/256)`; bindings `[M, bias, y(out), params]`

**Buffer layouts (must match exactly when wiring):**
- `P = ceil(H/2) * ceil(W/2)` tiles; `ntw = ceil(W/2)`, `nth = ceil(H/2)`
- `V[e*Cin*P + ci*P + tile]`  (ξ-major; each ξ slab is a contiguous Cin×P matrix)
- `U[e*Cout*Cin + co*Cin + ci]`  (ξ-major; each ξ slab is Cout×Cin = GEMM "A")
- `M[e*Cout*P + co*P + tile]`  (ξ-major; each ξ slab is Cout×P)
- `tile = ty*ntw + tx`, output 2×2 at `(2*ty, 2*tx)`

**Tests (all pass; run `cargo test -p lege-gpu winograd --release`):**
- `winograd_f23_matches_direct_conv` (CPU)
- `winograd_gpu_input_transform_matches_cpu`, `winograd_gpu_output_transform_matches_cpu`
- `winograd_gpu_end_to_end_matches_direct_conv` (full 3-pass GPU, 96→128ch, <2e-3)

## Remaining work — graph plumbing only

The memory planner (`memory_plan.rs::ResidentMemoryPlan::from_graph`) discovers buffers from
op inputs/outputs + `graph.constants`. So the clean integration is to **rewrite each eligible
Conv into 3 ops at prep**, letting V/M get slots automatically. Do the rewrite while building
`PreparedGraph` so `graph.planned_ops` AND `graph.constants` both reflect it before
`CompiledGraph::build` / `ResidentMemoryPlan::from_graph`.

### Step 1 — `onnx/types.rs`: 3 new `PlannedOpKind` variants
```
WinogradInputTransform,                 // x[1,Cin,H,W] -> V[16,Cin,P]
WinogradBatchedGemm,                    // (U[16,Cout,Cin], V) -> M[16,Cout,P]
WinogradOutputTransform { use_bias: bool }, // (M, bias) -> y[1,Cout,H,W]
```
Add `label()` arms. (Carry H/W/Cin/Cout via `input_shapes`/`output_shapes` like other ops; the
op wrappers already derive ntw/nth/P from shapes.)

### Step 2 — prep rewrite pass (new `onnx/winograd_rewrite.rs`, called from
`graph.rs::from_model_with_input_dims` after `lower_all_ops`, near line 237; model it on
`fold.rs`)
For each `Conv2d` op that is 3×3 s1 d1 group1 **and** `pads==[1,1,1,1]` **and** its weight name
is in `tensor_consts`/`constants`:
1. Read weight `g[Cout,Cin,3,3]`; build `U[16*Cout*Cin]` with `weight_transform_f23` in the
   `U[e*Cout*Cin + co*Cin + ci]` layout. Insert as a new constant (name e.g. `{weight}__wino_U`,
   shape `[16, Cout, Cin]`).
2. Replace the Conv op with three `PlannedOp`s (new intermediate names `{out}__wino_V`,
   `{out}__wino_M`; set `input_shapes`/`output_shapes`: V=[16,Cin,P], M=[16,Cout,P], y=conv out):
   - InputTransform: inputs `[x]` → `[{out}__wino_V]`
   - BatchedGemm: inputs `[U_const, {out}__wino_V]` → `[{out}__wino_M]`
   - OutputTransform{use_bias}: inputs `[{out}__wino_M, bias_or_dummy]` → `[conv.outputs[0]]`
   (Keep the conv's original output name on the OutputTransform so the downstream SiLU still
   attaches unchanged.)

### Step 3 — `ops/compile.rs::op_steps`: emit StepSpecs for the 3 kinds
Use the same params/dispatch/bindings as the validated wrappers (see layouts above). Bias for
OutputTransform: reuse the `DUMMY_BIAS` mechanism when the conv had no bias.

### Step 4 — `vision/reference.rs::run_op`: CPU impls for the 3 kinds
Needed so `run_cpu` / gpu-diff stay consistent. Trivial via the helpers (the test
`winograd_conv` in `winograd.rs` is essentially the reference for all three combined). Mirror
the same buffer layouts.

### Step 5 — profiler labels + route + measure
- `compiled.rs::step_profile_kind`: add labels for the 3 kinds.
- Build release, run `LEGE_INFERENCE_PROFILE=1 LEGE_CONV_SHAPES=1 ./lege.exe <pdf> 1-3`.
- Confirm detections unchanged vs baseline and `Conv3x3Co8Sp2x2` (39.4ms) is replaced by
  `WinogradInput/Gemm/Output` totalling **less** (expected ~25–30ms).

## Gotchas / watch-list
- **Pad gate.** Only 3×3 s1 d1 group1 with `pads==[1,1,1,1]`. Everything else keeps the direct
  kernel.
- **VRAM.** `V` is `16·Cin·P` floats ≈ 4× the conv input; `M` is `16·Cout·P`. Both transient,
  but at 192ch on a large feature map they're sizable — check peak against the resident-slot
  budget; the planner should reuse slots across the 38 convs since they're sequential.
- **Liveness is the one real risk** (unit tests can't catch it): V must live from InputTransform
  to BatchedGemm, M from BatchedGemm to OutputTransform. Because they're modeled as real op
  outputs/inputs, the planner handles this — but verify end-to-end on the test PDF, not just
  unit tests.
- **Numerics.** F(2,3) is the numerically-benign Winograd variant (small integer/half
  transforms); end-to-end was <2e-3 at 96→128ch. If a deeper 192ch layer drifts, that's
  expected Winograd behavior — confirm detections (boxes) are unchanged, which is the real bar.
- **Don't** re-touch the dilated kernels or attempt conv+SiLU fusion (abandoned — see
  model-findings.md; SiLU is only 3.5ms total and passes are free).

## Test PDF / harness
`D:\to-sort\test\wheelsofcommerce00brau_1.pdf` (large — use a page subset). Models in
`target/release/models/`, `pdfium.dll` next to `lege.exe`. Kernel correctness:
`cargo test -p lege-gpu winograd --release`.
