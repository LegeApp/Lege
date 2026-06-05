# New Model Findings

Analysis of the three new models to be supported. Inspected with `onnx` Python library
and the existing `inspect` subcommand. All three are currently rejected by the v1 target checker.

---

## YOLO layout optimization notes

### Failed 5x5 d2/d3 patch-loader experiment

Do **not** retry the simple `full_patch` bounds-check fast path for
`CONV5X5_CO8_SP1X2_WGSL`. The experiment added an interior-tile branch like the neighboring 3x3
and 5x5-d1 kernels so interior d2/d3 tiles could skip per-element signed bounds checks while loading
the shared-memory patch. On RTX 4060 profiling, this did not improve the kernel; the d2/d3 totals
became slightly worse/noisier. It was reverted. If revisiting this family, look at a real tiling or
register/shared-memory layout change instead of the simple interior-patch branch.

### Depth-2 YOLO resident slots

Depth-2 in-flight YOLO was tested with two private resident graph slots on the same WGPU device/queue
(`bench-folder-depth2`). It is **not a production win for YOLO right now**. On the 5-page test set
with a simulated 30 ms/page host stage, single-slot submit/work/await overlap measured about
`95.7 ms/page`, while depth-2 measured about `119.0 ms/page`. Pure compute depth-2 was also no clear
win. YOLO is already GPU-compute bound; queueing two full YOLO graphs mostly adds memory pressure and
scheduling noise rather than useful overlap. Keep the split `submit()` / `await_result()` path, but
do not build YOLO production around depth-2 slots unless a later profile shows real idle GPU gaps.

---

## paddle-rotate.onnx

**Architecture:** MobileNetV3-Small classifier. Input `[N, 3, 224, 224]` → output `[N, 4]` (4 rotation classes: 0°/90°/180°/270°). 77 nodes, 69 float32 initializers (weights present as proper ONNX initializers).

**Effort estimate: ~2 days. Start here.**

### New ops needed

| Op | Count | Difficulty | What it does |
|---|---|---|---|
| `Relu` | 2 | Trivial | `max(0, x)` |
| `GlobalAveragePool` | 3 | Low | Mean over H×W per channel → `[N, C, 1, 1]` |
| `HardSwish` | 28 | Trivial | `min(max(0, x+3), 6) * x / 6` |
| `HardSigmoid` | 2 | Trivial | `min(max(0, alpha*x + beta), 1)`, alpha=1/6, beta=0.5 baked into model |

### Shape / dynamic dims

Input spatial dims are hardcoded to 224×224. Only the batch dim is dynamic. One `Shape→Slice→Reshape` sequence flattens the pooled features before the final `MatMul` classifier — at batch=1 (normal inference) this collapses to a static reshape and is const-foldable via ORT basic opt, or special-cased in the planner.

**Static inference at batch=1 is fully achievable without dynamic shape support.**

### No blockers

All four new op types are pointwise or simple reductions. The model is otherwise identical in structure to the existing YOLO pipeline (Conv, MatMul, Softmax, Concat, Reshape, Slice).

### ORT prep

Run through ORT basic optimization to constant-fold `Shape→Slice→Reshape`. May also fuse depthwise Conv patterns. The Conv weights are already proper initializers so no special handling needed.

### Production rotation guard

Do **not** let this model make unconditional 160°-180° page flips in production. In the 5-page
pipeline test, `page_0146` was already correctly aligned but skewed, and the orientation classifier
still predicted the 180° class after correct classifier-style preprocessing. Treat 180° as a
high-risk output: either ignore it, require a much stronger independent confirmation signal, or defer
it to a separate OCR/text-direction check. The model is useful for obvious 90°/270° orientation
cases, but it does not appear reliable enough to decide upside-down vs upright text by itself.

---

## paddle-deskew.onnx

**Architecture:** ResNet-based deskew network. Input `[N, 3, H, W]` (fully dynamic) → output `[N, 3, H', W']` (geometrically corrected image, same size as input). 543 nodes, 0 initializers.

**Effort estimate: ~1 week.**

### Critical pre-requisite: weights are embedded as Constant nodes

The model has **zero ONNX initializers**. All 312 weight tensors are inline `Constant` nodes in the graph — the PaddlePaddle→ONNX export format. Conv weights, BatchNorm parameters, everything. This is different from the YOLO model where weights were proper initializers.

**Must run through ORT basic optimization first.** ORT will:
1. Convert all 312 Constant-node outputs into proper ONNX initializers
2. **Fold all 44 BatchNorm layers into the preceding Conv** — inference-mode BN (`training_mode=0`) is a pure affine transform that folds cleanly into Conv weights/biases, eliminating BN from the graph entirely
3. Constant-fold the `Shape`/`Cast`/`Unsqueeze`/`Squeeze` shape-plumbing nodes

After ORT opt the graph becomes: Conv→Relu (backbone), with a few special layers.

### New ops needed (after ORT opt)

| Op | Count | Difficulty | Notes |
|---|---|---|---|
| `Relu` | 41 | Trivial | `max(0, x)` |
| `PRelu` | 1 | Low | `x < 0 ? alpha*x : x`, alpha is a learned per-channel tensor |
| `Pad` (reflect mode) | 2 | Medium | Reflect padding of a tensor, not Conv padding |
| `Resize` (bilinear) | 2 | Medium | Existing runtime only has nearest-neighbor; needs bilinear + `align_corners` |
| `GridSample` | **1** | **Hard** | See below |

### GridSample — the hard op

`GridSample` takes the original input image and a coordinate grid `[N, H, W, 2]` (the flow field computed by the network) and performs bilinear interpolation at each `(x, y)` coordinate. **This is the deskew warp itself** — the actual geometric correction. It cannot be skipped or approximated.

Attributes: `mode=bilinear`, `padding_mode=zeros`, `align_corners=1`.

A WGSL implementation is straightforward in concept (each output pixel independently samples the input at a fractional coordinate, doing 4-sample bilinear interpolation), but it requires:
- Binding the input as a 2D image (or flat buffer with 2D indexing)
- Handling out-of-bounds as zeros (padding_mode=zeros)
- Correct `align_corners` normalization from `[-1, 1]` grid space to pixel space

### Dynamic shapes

Both spatial dims are fully symbolic — the model processes whatever size image it receives. Supporting this model requires either:
- **(a) Fix the inference size** at a known document resolution (e.g., 1654×1169 for 200dpi A4) and treat it as a static model at that size. This is pragmatic and avoids dynamic allocation.
- **(b) Dynamic buffer allocation** — re-allocate GPU buffers when input shape changes. More flexible but requires restructuring the resident executor.

Option (a) is the right first step.

---

## sauvola.onnx

**Architecture:** Sauvola adaptive binarization with learned attention. Converts a grayscale document image to a binary (black/white) mask. Uses integral images (summed area tables) for efficient local window statistics. Input `[N, H, W, 1]` (NHWC, single channel) → output `[N, H', W', 1]` (binary mask). 712 nodes, 115 initializers.

**Effort estimate: ~2–3 weeks. Implement last.**

### Hard constraints — must be resolved before anything else

**1. NHWC layout throughout.**
Input and output are channels-last `[N, H, W, C]`. The entire graph is NHWC. The current runtime is NCHW everywhere. This is a pervasive layout mismatch that affects every Conv, Transpose, Split, and Reshape in the graph. Options:
- Transpose at input/output only (cheapest, but graph's Conv/pool ops still expect NHWC — would need NHWC Conv kernels)
- Run a graph-level Transpose pass during model preparation to convert to NCHW
- Teach the runtime an NHWC mode

None of these is a small change.

**2. Fully dynamic H and W.**
Every spatial dimension is symbolic. The model is designed to run on arbitrary document sizes. The resident executor allocates fixed buffers at build time. Full dynamic shape support (shape inference at runtime, variable-size buffer allocation per inference call) is required.

**3. Float64 initializers.**
Six initializers are float64: scalar constants (`1.0`, `2.0`, `1e-6`, `0.0`) and a learned weight tensor of shape `(1, 8, 1, 1, 1)`. The runtime is entirely fp32. These must be cast to float32 during model preparation (safe — the values are numerically representable in fp32).

### New ops needed

| Op | Count | Difficulty | Notes |
|---|---|---|---|
| `CumSum` | 8 | **Hard** | See below |
| `GlobalAveragePool` | 12 | Low | Same as paddle-rotate |
| `DepthToSpace` | 5 | Medium | Pixel shuffle, blocksize=2 |
| `SpaceToDepth` | 5 | Medium | Inverse pixel shuffle, blocksize=2 |
| `Equal` | 10 | Medium | Boolean tensor compare, outputs bool tensor |
| `Not` | 10 | Medium | Boolean NOT, requires bool tensor support |
| `Neg` | 10 | Trivial | Elementwise negate |
| `Sqrt` | 7 | Trivial | Elementwise sqrt |
| `Max` | 8 | Trivial | Elementwise max of two tensors |
| `Pow` | 2 | Trivial | Elementwise power |
| `ReduceSum` | 1 | Low | Sum reduction along axis |
| `Pad` | 9 | Medium | Constant/zero padding modes |
| `ConstantOfShape` | 2 | Medium | Creates tensor of dynamic shape filled with a constant — requires dynamic allocation |

### CumSum — the hard op

The 8 `CumSum` ops are the heart of the Sauvola algorithm. They build 2D integral images (summed area tables) via two sequential passes: cumsum along rows, then cumsum along columns. At multiple window scales (the model name mentions `w7.15.23.31.39.47.55.63` — eight window sizes).

All uses are `exclusive=false, reverse=false` (standard inclusive prefix sum).

On GPU: a parallel prefix scan (Blelloch / Hillis-Steele) computes this in O(log N) passes instead of serial O(N), but requires multiple kernel launches with barriers between passes. For a 3300×2550 document image that's ~12 kernel dispatches per CumSum call × 8 CumSum ops = ~96 dispatches just for the integral images.

Correctness is achievable. Performance should be adequate if the scan is implemented well (each pass processes a full row/column via a standard workgroup-level scan pattern).

### Equal/Not — bool tensor support needed

The `Equal` ops compare a slice of a tensor against a constant scalar, producing a bool tensor that `Not` inverts. These appear in a repeated 4× pattern: `Equal(Split[1], const), Not, Equal(Split[3], const), Not`. This is likely selecting which CumSum axis to use at runtime — shape-plumbing that would constant-fold if shapes were known statically. With fully dynamic dims it may not fold.

Supporting `Equal`/`Not` means adding a boolean buffer type (u32 0/1 encoding is simplest) and shaders for bool operations.

### opset note

`ai.onnx.ml:2` is declared in the opset list but no ML-domain ops appear in the node histogram — the declaration is vestigial from the TF2ONNX export (`producer: tf2onnx 1.16.1`).

---

## Summary

| | paddle-rotate | paddle-deskew | sauvola |
|---|---|---|---|
| New op count | 4 (all easy) | 5 (GridSample hard) | 15+ |
| Weight storage | Proper initializers | Constant nodes → need ORT prep | Initializers + float64 |
| Input layout | NCHW ✓ | NCHW ✓ | **NHWC ✗** |
| Shape dynamism | Batch only, 224×224 fixed | Fully dynamic (fix size pragmatically) | Fully dynamic |
| Bool tensor support | Not needed | Not needed | Required (Equal/Not) |
| Hard blockers | None | GridSample, dynamic size | NHWC, CumSum, dynamic shapes, bool tensors |
| Effort estimate | ~2 days | ~1 week | ~2–3 weeks |
| Recommended order | **First** | Second | Third |

### Recommended approach

1. **paddle-rotate first.** Add `Relu`, `GlobalAveragePool`, `HardSwish`, `HardSigmoid`. Run ORT basic opt on the model. Wire to a new executor target (224×224, batch=1 static). This is nearly equivalent work to adding 4 shaders.

2. **paddle-deskew second.** Run ORT basic opt (folds BN + extracts Constants as initializers). Add `Relu` (shared with rotate), `PRelu`, bilinear `Resize`, reflect `Pad`. Implement `GridSample`. Fix inference size rather than full dynamic shapes in the first pass.

3. **sauvola last.** The NHWC layout issue and dynamic shapes require architectural changes. Plan those changes informed by whatever dynamic-shape work was needed for deskew.

### Shared wins

`Relu` is needed by both paddle models (41 in deskew, 2 in rotate) and by sauvola (6) — implement once and all three benefit. `GlobalAveragePool` is needed by all three. `Pad` (non-Conv padding) is needed by deskew and sauvola.

Here's the situation:
Root cause: The original sauvola.onnx (712 nodes, dynamic H/W) has ops the bridge's lower_node rejects — Shape, Cast, ConstantOfShape, Equal, Not, Neg, Gather. These are the shape-plumbing subgraph that computes output tensor dimensions from the input shape. ORT basic optimization constant-folds them away at a fixed H/W into the 281-node prepared model. Without ORT, we can't regenerate that static artifact at a different size.
What deskew actually does: The DocumentUnwarper rebuilds per size, but every rebuild loads the SAME prepared model file (static size). The width/height in DeskewConfig are only used as resize targets in rgb_to_nchw_resized(). The compiled graph is always at whatever size the prepared model was stamped with — it just resizes the input to match. The rebuild is redundant; there's no actual dynamic graph generation.
The real fix: Add the shape-plumbing ops to the bridge — Shape, Gather, Cast, ConstantOfShape, Equal, Not, Neg — and evaluate them on the CPU reference runtime during graph preparation, with the actual input (H,W) injected. This is what execute_cpu_prefix already does for YOLO's prefix, just extended to cover the full shape subgraph. Once resolved, the remaining compute ops (Conv, GlobalAvgPool, SpaceToDepth, etc.) have known static buffer sizes and compile cleanly for WGPU.
What needs to change:
1. Add Shape, Gather, Cast, ConstantOfShape, Equal, Not, Neg to PlannedOpKind + lowering in lower.rs:338
2. Implement CPU reference in reference.rs (all trivially small tensors — shape is just reading .len(), Cast is type punning, ConstantOfShape fills a buffer)
3. Extend PreparedGraph::from_model() to run these shape-plumbing ops on the CPU during preparation with the actual input dims, stripping them from the final compute graph
4. DocumentDeskewer and SauvolaProcessor switch from loading the fixed prepared models to loading the original sauvola.onnx/paddle-deskew.onnx with input (H,W) stamped at the lowering call
The end result: both deskew and sauvola accept any input resolution. The per-size cost is a fast CPU shape-pass + CompiledGraph::build(), exactly analogous to your current deskew rebuild pattern, but this time actually doing dynamic resolution. No resize, no quality loss, no ORT.

---

## Phase 1 (deskew) — DONE

Implemented the CPU shape-fold pass and wired deskew to run at native resolution.

- `lege-gpu/src/vision/onnx/fold.rs` — new shape-plumbing constant folder (`Shape`/`Cast`/`Slice`/`Concat`). A node folds only when every data input is already constant (`Shape` always folds via `known_shapes`), so the `Concat`/`Slice`/`Cast` that sit on the *data* path stay as compute ops.
- `shape.rs::infer_all_shapes` now runs fold + shape-inference together, takes `&mut tensor_consts`, and returns the set of folded node indices.
- `graph.rs::from_model_with_input_dims(model, Some([1,3,H,W]))` stamps the page's real dims, runs inference/fold, and drops folded nodes from the executable graph + lowering. `from_model` delegates with `None`.
- `load.rs` only rejects residual shape-plumbing for the static YOLO `images` target; dynamic targets are accepted (the bridge folds them).
- `api.rs::DocumentDeskewer` now stores the `ModelProto` and prepares/compiles per page size (cached for the last size), feeding a native-resolution NCHW tensor — no resize. `src/deskew.rs::DocumentUnwarper` collapsed to a thin wrapper.

Verified end-to-end on `paddle-deskew.prepared.onnx` (the **dynamic** prepared artifact, BN folded / Constants extracted, H/W still symbolic): page_0146 749×1186 in → 749×1186 out, correct deskew, ~38 ms run, size cached on reuse.

**Deployment action required:** the deployed `paddle-deskew.onnx` is the **raw original** (543 nodes, 312 Constant, 44 BatchNorm) which the bridge cannot consume. It must be replaced with the dynamic-prepared artifact. `prep_models.py::prep_deskew` should emit the dynamic `paddle-deskew.prepared.onnx` (ORT basic opt **without** stamping H/W) instead of the static `*.static-1200x736.prepared.onnx`, and that file deployed under the canonical name. The static deskew artifact is then redundant.

## Phase 2 (sauvola) — DONE

Sauvola now runs at native resolution through the same fold mechanism.

Key realization: the "NHWC blocker" from the findings above is moot — the channel
is 1, so `[1,H,W,1]` (NHWC) and `[1,1,H,W]` (NCHW) share memory layout and the
NCHW bridge runs the graph unchanged. The only real blocker was the shape
plumbing that ORT folds at a static size.

- Generated a **dynamic** `sauvola.prepared.onnx`: `_cast_all_doubles_to_float` then ORT basic opt with **no** H/W stamping. Because nothing is statically folded, the giant `[1,H,W,1]` window count maps are never baked — no rank-1 factoring needed. `prep_models.py::prep_sauvola` updated to emit this.
- Extended `fold.rs` to the full sauvola shape-plumbing set: `Shape`, `Cast`, `Slice`, `Concat`, `Split`, `Gather`, `Unsqueeze`, `Squeeze`, `Neg`, `Not`, `Equal` (bools as int64 0/1), `ConstantOfShape`, `Reshape`, and integer/float `Add`/`Sub`/`Mul`/`Div` (the TF `space_to_batch` dilated-conv padding arithmetic). All fold only when every data input is constant, so the same op types stay as compute ops on the data path.
- Folded **float** constants consumed by a kept op (the `ConstantOfShape` ones tensors feeding `CumSum` for the integral-image counts) are promoted to GPU initializers in `graph.rs`.
- `load.rs`: removed `ConstantOfShape` from hard-reject, allowed `Equal`/`Not`/`Neg`; sauvola target batch dim relaxed to symbolic (stamped to 1 at prep).
- `api.rs::SauvolaProcessor` rewritten like `DocumentDeskewer`: stores the `ModelProto`, prepares/compiles per page size (cached), feeds a native-res NHWC grayscale tensor — no resize. `Legencode/src/color/binarization.rs` drops the resize-back and loads `sauvola.prepared.onnx`.

Verified end-to-end: page_0146 749×1186 → 749×1186, correct adaptive binarization (crisp text, halftoned illustration, skew preserved).

### Sauvola runs CPU-only, whole-image (not GPU, not tiled)

Tiling was rejected: the model uses **global instance normalization** (the `inorm`
in its name — `GlobalAveragePool`→`Sqrt`/`Sub` computes whole-image mean/variance
per channel). Per-tile processing normalizes each tile independently, so identical
pixels binarize differently depending on which tile they fall in (measured 4%
disagreement standalone-patch vs whole-page on a *clean* page; worse on the
degraded pages this mode targets). The original onnxruntime path ran whole-image,
and that is the reference behavior.

GPU was also rejected: the model is dominated by integral images (`CumSum`), 165
`Slice`s and the global reductions — sequential, memory-bound work with little
parallel compute. We measured GPU at ~1.1 s with most of it pure readback, and a
whole 749×1186 graph needs ~2.95 GB VRAM (scales with H·W → OOM on big pages).
So heavy sauvola runs **whole-image on the CPU** (`SauvolaCpuProcessor`,
`PreparedGraph::run_cpu`): RAM-bounded, no VRAM ceiling, global stats correct,
matching the legacy ORT-CPU behavior. The GPU `SauvolaProcessor` is kept dormant.

**CPU executor optimization (`reference.rs`).** The reference executor was a naive
scalar oracle that allocated a coords `Vec` per element (per *MAC* in conv) — a
whole-page run took >7 minutes. Lifting standard fast-CPU techniques (the kind
tract uses) without taking a tract dependency:
- `conv2d`: contiguous accumulate (`out[ow] += w * x[ow+kx]`) over a precomputed
  in-bounds range — vectorizable, no per-element bounds checks; rayon over output rows.
- `elementwise`/`transpose`/`slice`: stride odometers, no per-element allocation; rayon over rows.
- `pad`/`concat`/`split`: contiguous block copies. `space/depth_to_space`: rayon over planes.
- `run_op` takes `&[&Tensor]` (zero-copy inputs); `run_cpu` frees intermediates at last use.

Result on page_0146 (749×1186), release: **>7 min → 23 s → 9.4 s → 5.7 s → ~2.5 s/page**,
bit-identical output throughout. In the ballpark of the SIMD-assembly ORT-CPU it
replaces; further headroom exists (conv ≈0.8 s is now the largest op) but 2.5 s is
fine for this opt-in heavy-duty path.

Note: fixed pixel window sizes (w7..w63) mean whole-image native-res binarization
operates at true pixel scale, not the legacy resized-to-1200×736 scale.

**Deployment action required:** deploy the dynamic `sauvola.prepared.onnx` to the models dir (the deployed `sauvola.onnx` is the raw original — the bridge needs the double-cast + Constant extraction prep). Production prefers `sauvola.prepared.onnx`, falling back to `sauvola.onnx`.

---

## YOLO inference perf — measured investigation (2026-06-05, DX12 / RTX 4060)

This section supersedes loose hypotheses about the wgpu↔CUDA perf gap with **measured**
results. Read this before attempting any of the optimizations suggested in
`lege-gpu/New Text Document.txt` (the conv.rs micro-opt memo) — most of them do not pan
out, and two are specified backwards for this codebase.

### The harness (use this to validate any conv change)

Per-kernel GPU-timestamp profiler, already in-tree:
`compiled.rs::profile_report`, gated behind `LEGE_INFERENCE_PROFILE`, plus
`LEGE_CONV_SHAPES` to dump per-conv shape/shader/dispatch. Each op runs in its own pass
exactly like the real `submit()` path, so it reports **busy (kernel) ms vs inter-dispatch
idle (gap) ms vs wall**:

```sh
# next to lege.exe with the 4 models in ./models/ and pdfium.dll present:
LEGE_INFERENCE_PROFILE=1 LEGE_CONV_SHAPES=1 ./lege.exe <file.pdf> 1-3
```

Test PDF used: `D:\to-sort\test\wheelsofcommerce00brau_1.pdf` (large; profile a page subset).
Numeric kernel correctness: `cargo test -p lege-gpu conv5x5_sp2x2_dil_matches_reference --release`
(GPU test in `conv.rs`; compares a kernel against the CPU `reference::run_op` oracle, skips
cleanly if no adapter).

### KEY FINDING: YOLO is GPU-compute-bound, NOT barrier/pass-count bound

Baseline profile (pages 1-3, release, DX12 / RTX 4060):

```
steps=528   GPU busy(kernels)=147.4ms   GPU span=144.9ms   GPU idle gaps=0.0ms (0%)
per-page wall (compiled.run): ~128–144ms (readback line ≈ GPU-compute wait)
```

**Idle gaps = 0.0 ms.** busy (147) actually *exceeds* span (145) → the DX12 driver
**overlaps consecutive compute passes**; the ~528 separate `begin_compute_pass` dispatches
cost essentially nothing. The intuition that "~450 conv+SiLU+concat passes = barrier/
submission bound" is **false on this DX12 path**. All 528 steps go into one command buffer /
one `submit` anyway (`compiled_chunk_size` returns `step_count` on a GPU adapter).

Corollary: the wgpu (100–145ms) vs CUDA (20ms) gap is **structural arithmetic**, not graph
overhead. CUDA wins via **tensor cores (FP16/TF32, 8–16× FP32 ALU) + a fusing graph
compiler (TensorRT) + Winograd** — none of which WebGPU/wgpu FP32 can reach. 20ms is not a
reachable target on this backend. Realistic wgpu ceilings: ~70ms with conv-arithmetic work,
~45–55ms if `shader-f16` packed math is added. The model is **not** "poorly built for wgpu."

### Baseline per-kernel breakdown (the real hot spots)

| Kernel (profiler label) | Total | # | per-conv |
|---|---|---|---|
| `Conv3x3Co8Sp2x2` 3×3 s1d1 | 38.9ms | 38 | ~1.0ms |
| `Conv1x1GemmS1Vec4` 1×1 | 22.8ms | 79 | ~0.29ms |
| `Conv5x5Co8Sp1x2` d3 | 14.3ms | 4 | **3.6ms** |
| `Conv3x3Co8` 3×3 s2 | 14.2ms | 4 | 3.6ms |
| `Conv5x5Co8Sp1x2` d2 | 14.0ms | 4 | **3.5ms** |
| `Conv5x5Co8Sp2x2D1` d1 | 9.4ms | 4 | **2.35ms** |
| `Conv3x3Co8Sp1x2` d5 / d3 | 13.4ms | 8 | ~1.7ms |
| `SiLU` (all of them) | **3.5ms** | 178 | — |

Model op histogram (`yolo-layout.onnx`, 644 nodes): 191 Conv (79× 1×1 g1, 38× 3×3 s1d1 g1,
34× 3×3 depthwise, 12× 5×5 dilated g1, …), 186 SiLU (Sigmoid+Mul, fused to one pass each),
29 Add (mostly `Add(Mul,Mul)` residuals), 25 Concat.

### Verdict on the `New Text Document.txt` recommendations

1. **Double-buffer 3×3/5×5 co-blocked patch — DO NOT DO.** WGSL/naga has no async-copy
   (`cp.async`); "double buffering" still compiles to load→reg→smem + barrier, and doubles
   smem (halves occupancy). Targets `COBLOCK8`, which isn't even the hot 3×3 path
   (`CO8_SP2X2_S1D1` is). The analogous 5×5 interior-tile experiment already regressed and
   was reverted (see top of this file).
2. **Vectorize depthwise — right target, WRONG mechanism.** `DEPTHWISE3X3_WGSL` is the least
   optimized hot kernel (34 instances, naive scalar, no reuse). But the memo's "vec4 over 4
   consecutive channels" is wrong for **NCHW** (consecutive channels are H·W apart → 4 strided
   loads, no coalescing). The real win is input *reuse* (smem/register spatial tiling, vec4
   along contiguous W). Not yet attempted.
3. **Pad smem rows to multiple of 32 — BACKWARDS.** That forces every row to the same bank
   (max column-conflict). Correct fix is an *odd*/+1 stride — which the existing
   `CONV3X3_CO8_SP2X2_S1D1_VEC4LOAD` already does (`PWV=37`). Low ceiling.
4. **Bigger 1×1 GEMM tile (BM=128) — MAYBE, shape-dependent.** 1×1 is only 0.29ms/conv and
   deep YOLO layers have small planes where BN=64 already over-tiles. A/B only; risks
   regressing the many small-plane GEMMs.
5. **Prefetch GEMM weight tile — DO NOT DO.** Same no-async-copy problem as #1.

### Conv+SiLU epilogue fusion — ABANDONED (data-driven)

Originally proposed as the big win (remove ~186 SiLU passes + barriers). The profiler shows
**all 178 SiLU passes = 3.5ms total (2.4%)**, and passes already overlap, so fusion would
save ≤3.5ms for invasive graph surgery. Not worth it. (Design that *would* have been correct,
if ever revisited: rewrite `planned_ops` to drop Sigmoid+Mul and annotate the producing Conv
with an activation flag **before** memory planning — not a post-plan output redirect, which
risks clobbering a slot the planner thinks is free. Each conv kernel applies `silu()` at its
store site behind a trailing `use_silu` param. The Sigmoid+Mul→SiLU detection already exists
at `compiled.rs:158`.)

### OPTIMIZATION 1 (in progress): dilated 5×5 conv → 2×2 register tile

**Observation:** same 96-ch 5×5 FLOPs, but the **d1** variant uses the 2×2 spatial tile (32
accumulators) at **2.35ms**, while **d2/d3** fell back to the 1×2 tile (16 accumulators, half
the arithmetic intensity) at **3.5–3.6ms**. The fallback reason in the old comments ("exceeds
smem budget at d≥2") was about the **self-imposed `array<f32,1296>` constant, not hardware** —
the RTX 4060 allows ~32KB shared (8192 f32).

**Change (committed to working tree):**
- New kernel `CONV5X5_CO8_SP2X2_DIL_WGSL` in `conv.rs`: generalizes
  `CONV5X5_CO8_SP2X2_D1_WGSL` to runtime dilation (patch span `32 + 4·dil` per side, smem
  `array<f32,1936>` = 44² for d3, tap reads strided by `dil`). d1 keeps its hardcoded PW5=36
  fast path; only d2/d3 route to the new kernel (`compile.rs`).
- Profiler label `Conv5x5Co8Sp2x2Dil` added (`compiled.rs`). `Conv2dPlan` gained `Clone`.
- Correctness: GPU test `conv5x5_sp2x2_dil_matches_reference` (d=1/2/3 vs CPU reference,
  max abs diff < 1e-3) — **PASSES**.

**Expected:** d2/d3 ~3.5ms → ~2.4ms each, ≈ -9ms across the 8 dilated 5×5 convs.

**Measured post-result (CONFIRMED, no backslide):**
- d3: 14.4ms → **10.4ms** (3.6 → 2.6ms/conv)
- d2: 14.0ms → **10.1ms** (3.5 → 2.5ms/conv)
- ≈ **-7.9ms** across the 8 dilated 5×5 convs; total GPU busy 147.4 → 140.8ms. Per-conv
  (2.5–2.6ms) now sits just above the d1 tile's 2.35ms — the small gap is the runtime-dilation
  multiply + larger smem, as expected. Detections unchanged. Kept.

### OPTIMIZATION 2 (DONE): same 2×2-tile generalization for dilated 3×3 (d3, d5)

New kernel `CONV3X3_CO8_SP2X2_DIL_WGSL` (patch span `32 + 2·dil`, smem `array<f32,1764>`
= 42² for d5), routed for 3×3 s1 d2..d5 in `compile.rs`, profiler label
`Conv3x3Co8Sp2x2Dil`, wrapper `run_conv3x3_co8_sp2x2_dil`, test
`conv3x3_sp2x2_dil_matches_reference` (d=1/2/3/5 vs CPU reference) — **PASSES**.

**Measured post-result (CONFIRMED, no backslide):**
- d5: 6.9ms → **5.0ms** (1.7 → 1.25ms/conv)
- d3: 6.5ms → **4.7ms** (1.6 → 1.18ms/conv)
- ≈ **-3.7ms** across the 8 dilated 3×3 convs.

### Combined dilated-tile result

Optimizations 1+2 together: **GPU busy 147.4ms → 137.1ms (-10.3ms, ~7%)** on pages 1-3,
detections unchanged, both kernels validated against the CPU reference. The old 1×2-tile
kernels (`CONV5X5_CO8_SP1X2_WGSL`, `CONV3X3_CO8_SP1X2_WGSL`) are now unused by this model but
left in place (other shapes / future models may route to them).

### OPTIMIZATION 3 (planned, user-requested): Winograd F(2,3) for 3×3 s1d1

The single biggest line item (`Conv3x3Co8Sp2x2`, 38.9ms ×38). Winograd F(2×2,3×3) ≈ 2.25×
fewer MACs. Large effort (input/weight/output transforms + tiling). `shader-f16` deferred.
