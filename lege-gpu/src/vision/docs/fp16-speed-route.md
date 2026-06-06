# lege-vision fp16 — the speed route (runtime fp16 compute)

This is the design guide for the **large** fp16 job: running the YOLO-layout
convolutions in fp16 *math* on the GPU to roughly halve inference time. It is the
counterpart to the small job that already shipped.

> **What already shipped (the size route, NOT this):**
> `onnx/attrs.rs::tensor_const` decodes FLOAT16 (dtype 10) weight initializers and
> up-converts them to f32 at load (`tensor_f16_as_f32`). Combined with
> `scripts/quantize_yolo_weights_fp16.py` (weight-only cast, no graph rewrite),
> that halves the on-disk model with **zero** kernel changes and **identical**
> detections (validated bit-for-bit). It does nothing for speed — every kernel
> still computes in f32. This document is about making the kernels themselves fp16.

---

## 0. Set expectations first (read before committing)

Per the prior roofline work (`docs/handoff/1-yolo-fp16-and-optimization.md` in the
standalone wgpu-inference project):

- The model does ~283 GFLOP/page. On a 4060 the fp32 **compute floor is ~18 ms**,
  but measured GPU time was **~91–98 ms** — i.e. kernels run at **~5–7% of fp32
  peak**. The bottleneck is *structural* (memory traffic, occupancy, tile shapes),
  **not precision**.
- fp16 doubles the *ceiling*. If you are at 6% of the fp32 ceiling you are at ~3%
  of the fp16 ceiling. **fp16 alone will not deliver 2×** unless the kernels are
  already close to compute-bound. Expect the "~30 ms saving" only *after* the
  structural fp32 work (vec4 global loads on the 3×3 SP2×2 kernel, 5×5 d2/d3
  register tiling) lands. Do that first; treat fp16 as a multiplier on top.
- fp16 **changes results**. It cannot be validated with bit-exact cosine. Validate
  on **task output** (detection box agreement), not raw tensors — see §6.

If the goal is purely "smaller model," you already have it; stop here.

---

## 1. The two fp16 axes

1. **B1 — runtime fp16 compute** (this doc): kernels read f16, accumulate f32,
   write f16; storage buffers are f16; device feature `SHADER_F16`. This is where
   the speed is.
2. **B2 — model-side fp16 weights** (mostly done): weight initializers stored as
   fp16. Today they are up-converted to f32 on load. For B1 you instead keep them
   f16 and upload them directly (no up-convert), so the on-GPU weight footprint is
   also halved (bandwidth win, not just disk).

B1 is the work. B2 becomes a one-line change once B1 exists (don't up-convert
dtype-10 weights; pack them into f16 buffers).

---

## 2. Architecture: a precision-parameterized path, not a fork

Do **not** hand-write 49 fp16 kernels (`grep -rhoE "[A-Z0-9_]+_WGSL" ops/` → 49
constants today). Introduce a precision dimension that flows from device
capability → graph compile → kernel source → buffer sizing. Keep the f32 path as
the always-correct fallback (GL/older backends lack `SHADER_F16`).

```
GpuContext (device.rs)         -> probes SHADER_F16, exposes Precision::{F32,F16}
  └─ CompiledModel (compile.rs)-> per-StepSpec precision (conv ops opt-in to F16)
       └─ kernel source        -> one WGSL template emits f32 or f16 variant
            └─ memory_plan      -> buffer bytes = elements * dtype_size(precision)
                 └─ I/O boundary-> upload f32->f16, readback f16->f32
```

### Precision enum

Add `Precision { F32, F16 }` (in `runtime/mod.rs` or a new `runtime/precision.rs`).
A graph-level mode plus a per-op override is enough; route it the same way the
existing vec4 kernel selection is routed in `ops/compile.rs`.

---

## 3. Step-by-step

### 3.1 Device feature (`runtime/device.rs`)

`GpuContext::new` already inspects `adapter.features()` for `TIMESTAMP_QUERY`
(line ~87). Do the same for `SHADER_F16`:

```rust
let supports_f16 = adapter_features.contains(wgpu::Features::SHADER_F16);
let mut required = wgpu::Features::empty();
if supports_timestamps { required |= wgpu::Features::TIMESTAMP_QUERY; }
if supports_f16        { required |= wgpu::Features::SHADER_F16; }
// store `precision = if supports_f16 && requested { F16 } else { F32 }` on GpuContext
```

Gate the whole fp16 path on `supports_f16`; fall back to F32 when absent. Expose
the chosen `Precision` on `GpuContext` so compile/dispatch can read it. Add a
`--precision fp16|auto|f32` CLI flag (and a GUI toggle later) that requests F16;
`auto` = F16 when the feature is present.

### 3.2 WGSL: one source, two precisions

WGSL fp16 needs `enable f16;` at the top of the module and the `f16` / `vec4<f16>`
types. The mixed-precision recipe for conv: **load f16, accumulate in f32, store
f16.** Do not accumulate in f16 — it wrecks conv numerics.

Two viable mechanisms (pick one, apply to the conv kernels only at first):

- **Type-alias prelude.** Prepend a generated prelude and alias the scalar/vector
  types the kernel uses:
  ```wgsl
  enable f16;
  alias Scalar = f16;        // or f32 in the f32 build
  alias Vec4 = vec4<f16>;
  ```
  Keep accumulators explicitly `f32`/`vec4<f32>` in the kernel body (don't alias
  those). Requires auditing each conv kernel so storage reads/writes use `Scalar`
  and only the accumulator stays f32.
- **String-replace preprocessor.** A tiny step that emits both precisions from one
  `*_WGSL` source by substituting a `SCALAR`/`VEC4` token. Lower-effort to wire,
  but the accumulator-stays-f32 rule must be encoded in the source by writing the
  accumulator as literal `f32`, not the token.

Start with the profile leaders only — `CONV3X3_CO8_SP2X2_S1D1_*` (~25% of GPU
time), `CONV1X1_GEMM_S1_VEC4` (~15%), `CONV5X5_CO8_*` (~17%). The pointwise /
structural ops (`activations`, `elemwise`, `concat`, `slice`, `transpose`,
`resize`, `softmax`, …) can **stay f32**; convert around them (see §3.4). Only the
convs move the needle.

### 3.3 Storage buffers (`runtime/memory_plan.rs`, `reference.rs`)

- `memory_plan.rs:131` sizes every buffer as `elements * size_of::<f32>()`. Make
  the element size precision-aware: `elements * dtype_size(precision)` where
  fp16 = 2 bytes. Buffers consumed/produced by f16 kernels get the 2-byte size;
  buffers on the f32 boundary keep 4.
- WGSL `array<f16>` storage requires `SHADER_F16`; alternatively pack two f16 per
  `u32` and unpack in-shader (`unpack2x16float`). `array<f16>` is cleaner if the
  feature is present — prefer it and keep the packed path only if you need a
  fallback.
- `reference::Tensor.data` is `Vec<f32>` (reference.rs:13). The CPU reference path
  can stay f32; it is the correctness oracle. Only the **GPU** storage goes f16.
  Keep the f32 reference for the validation harness.

### 3.4 Precision routing & conversion points (`ops/compile.rs`)

- Tag each `StepSpec` with its I/O precision. A conv step declares f16 inputs/
  outputs; a step that stays f32 declares f32. Where a f16 producer feeds a f32
  consumer (or vice-versa), insert an explicit **convert** step (a trivial WGSL
  cast kernel, or fold the cast into the consumer's load).
- The two unavoidable conversions are at the **graph boundary**: upload the input
  image f32→f16 once, read the detection head back f16→f32 once. Everything
  between can stay f16 if you converted the whole conv stack; initially you will
  have several internal f32 islands (the non-conv ops) and that's fine — the
  convs are still doing their math in f16.

### 3.5 Weights direct-to-f16 (the B2 tie-in)

Once buffers are f16: stop up-converting dtype-10 weight initializers in
`tensor_const`. Instead carry them as f16 bytes straight into the f16 weight
buffer (no f32 round-trip). Keep `tensor_f16_as_f32` for the f32 fallback path and
for any initializer a f32 kernel still consumes. This halves resident weight
bandwidth, which is a real chunk of the conv cost.

---

## 4. Files to touch (lege-vision, actual paths)

| File | Change |
|---|---|
| `runtime/device.rs` | Probe + request `SHADER_F16`; expose `Precision` on `GpuContext`; fp32 fallback. |
| `runtime/precision.rs` *(new)* | `Precision` enum + `dtype_size()` + WGSL prelude/token helper. |
| `ops/compile.rs` | Per-`StepSpec` precision; insert convert steps; route conv kernels to f16 variant. |
| `ops/conv.rs` (+ the `*_WGSL` conv sources) | f16 variants via the §3.2 mechanism; accumulate f32. |
| `runtime/memory_plan.rs` | Precision-aware buffer byte sizing (line ~131). |
| `runtime/compiled.rs` | Dispatch the selected precision; manage f16 buffers. |
| `onnx/attrs.rs` | Keep `tensor_f16_as_f32` (fallback); add a no-up-convert path for f16 weight buffers. |
| `preprocess.rs` | Input image f32→f16 upload; head f16→f32 readback. |
| CLI/GUI | `--precision` flag / toggle (default `auto`). |

The CPU reference (`reference.rs`) and the non-conv ops stay f32 for the first
milestone.

---

## 5. Suggested sequence

1. **(prereq) Structural fp32 wins** — vec4 global loads on `CONV3X3_CO8_SP2X2`,
   5×5 d2/d3 register tiling. Validate cosine = 1.0. This is what actually unlocks
   the fp16 ceiling; without it fp16 gives little.
2. Agree the acceptable detection delta (§6) with the host-program owner.
3. `SHADER_F16` probe + `Precision` plumbing + `--precision` flag (no kernels yet;
   F16 routes to F32 so it's a no-op — proves the plumbing).
4. f16 variant of the **single** hottest conv (`CONV3X3_CO8_SP2X2_S1D1`), with
   f32→f16 convert steps around it. Validate box agreement on a page set.
5. Extend to the remaining conv families; collapse adjacent converts so the conv
   stack runs f16 end-to-end.
6. B2 tie-in: weights direct-to-f16 (drop the up-convert for f16 buffers).
7. Re-profile. Confirm the saving materialized. If not, you are still bandwidth/
   occupancy-bound → back to step 1.

---

## 6. Validation (fp16 changes numbers — validate the task, not tensors)

- Gate = **detection box agreement** vs the f32 path on a representative page set:
  IoU ≥ threshold + matching class + equal count. Decide the tolerance up front.
- Quick harness: run the same pages through f32 and f16 with `LEGE_BBOX_TRACE=1`
  and diff the `PAGE n img_det[i] bbox=(...) conf=...` lines. (For the *weight-only*
  size route these were bit-identical; for runtime f16 expect small conf/coord
  drift — that's the tolerance you're signing off.)
- Keep the f32 reference (`reference.rs`) as the oracle; never delete the f32
  kernel path — it is both the fallback and the validation baseline.

---

## 7. Risks / gotchas

- **`SHADER_F16` absence** on some GL/old DX12/Vulkan drivers → must fall back to
  f32 cleanly. Test the fallback on an adapter without the feature (or force it).
- **f16 accumulation** anywhere in conv = silent quality loss. Accumulators are
  f32, always.
- **Overflow/denormals**: fp16 max ≈ 65504. Trained conv activations rarely
  approach it, but a BN-fused weight could; the f32 accumulate protects the sum,
  and the §6 box gate catches any regression.
- **Don't re-tile from scratch**: the handoff notes dead ends — Naga already
  promotes the 3×3 SP2×2 accumulator array; the BN-padding "bank conflict" was the
  `thread_col*4` read stride, already fixed. Don't repeat those.
