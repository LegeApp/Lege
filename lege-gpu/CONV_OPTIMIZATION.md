# Conv kernel optimization notes

Status of the wgpu conv kernels in `src/vision/ops/conv.rs` and the
optimization avenues that were identified but deliberately deferred
(July 2026 review). Profiling has established that YOLO layout inference is
**conv-compute-bound**: pass/barrier structure and elementwise ops are not the
bottleneck, so any further gains must come from the conv arithmetic itself or
from algorithmic substitution (Winograd).

## Current state

- The workhorse is `CONV3X3_CO8_SP2X2_S1D1_WGSL`: 16×16 workgroup, cooperative
  34×34 smem input patch per input channel, 8 output channels × 2×2 spatial
  register tile per thread (32 accumulators as named scalars — naga 29 lowers
  even constant-index private arrays to Function-space access chains, which is
  ~12× slower, so they must stay named scalars).
- Per input-channel iteration each thread performs 288 MACs against
  36 smem reads for input values + 72 smem reads for weights.
- 1×1 convs route to a tiled GEMM (`CONV1X1_GEMM_S1_*`, vec4 variant when the
  contraction and plane are 4-aligned); 5×5 and dilated 3×3 have dedicated
  `*_SP2X2_DIL` variants; everything else falls back to `CONV2D_WGSL` (naive)
  or `CONV3X3_TILED_WGSL`.
- Winograd F(2,3) (2.25× multiply reduction) exists as a graph rewrite
  (`onnx/winograd_rewrite.rs`) splitting eligible 3×3 convs into
  input-transform / batched-16-GEMM / output-transform steps.
- GPU-vs-CPU-reference test harnesses exist for the s1d1, 1×1 GEMM (both
  paths), dilated 3×3/5×5, and all three Winograd stages
  (`cargo test -p lege-gpu`).

## Deferred optimization: register-cache the 4×4 input window

The inner loop of `CONV3X3_CO8_SP2X2_S1D1_WGSL` re-reads the 2×2 output
block's overlapping input window from shared memory on every tap:

```
for ky in 0..3, kx in 0..3:          // 9 taps
    v00, v01, v10, v11 = smem[...]   // 4 smem reads per tap = 36 per ci
    w0..w7 = wmem[...]               // 8 smem reads per tap = 72 per ci
    32 FMAs
```

A 2×2 output block under a 3×3 kernel only touches a **4×4 input window**
(16 values). Loading those 16 values once per input channel into named
scalars (`v00..v33`, same naga constraint as the accumulators) and indexing
them statically from the unrolled tap loop cuts input smem traffic from 36 to
16 reads per channel iteration — total smem reads per 288 MACs drop from 108
to 88 (~18% less shared-memory pressure).

Why deferred:

- The kernel is already near the arithmetic floor for direct convolution;
  the expected win is bounded by how much of the stall time is LDS-bound vs
  FMA-bound on the target GPUs (DX12 desktop + Vulkan Linux).
- It costs 16 more live registers per thread on top of the 32 accumulators,
  which may reduce occupancy and cancel the gain — this needs measurement,
  not reasoning.
- The same restructuring must be replicated in the `_DIL` and `VEC4LOAD`
  variants to keep them mirror-consistent.

How to evaluate when picked up:

1. `LEGE_CONV_SHAPES=1` on a representative run dumps every conv's shape,
   chosen shader and dispatch — identify which layers dominate.
2. `CompiledGraph::profile()` (timestamp queries) gives per-step GPU
   milliseconds; compare before/after per shader, not just end-to-end.
3. The correctness harness is already in place: extend
   `vision::ops::conv::tests` if a new variant is added.

## Other known headroom (smaller / situational)

- **Weight smem reads dominate** (72 of the 108 reads): reordering the tap
  loop so each cout's 9 weights load once into registers doesn't change the
  total, but a `vec4`-packed `wmem` layout would halve the transaction count
  on hardware that doesn't coalesce consecutive scalar LDS reads.
- **Winograd coverage**: only stride-1/dilation-1/group-1 3×3 convs are
  rewritten today. Each additional layer moved to Winograd saves 2.25× on
  multiplies but adds two transforms — profitable mainly for large-plane
  layers.
- **Small-spatial utilization**: late YOLO layers (e.g. 8×8 plane) fill only
  1/16 of the 32×32 spatial tile, leaving most of the 256-thread workgroup
  idle. A co-major variant (more output channels per workgroup, smaller
  spatial tile) would fix utilization, but these layers are a small share of
  total FLOPs — measure before bothering.
- **Concat/Split copies**: each Concat input is a full copy dispatch. Slot
  planning could alias producers directly into offset views of the concat
  output, but wgpu's 256-byte dynamic-offset alignment makes this a larger
  memory-planner change.
