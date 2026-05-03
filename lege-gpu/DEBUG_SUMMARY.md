# lege-gpu Binarization Debug Summary

## Current Status (May 3, 2026)

### Working
- **Fixed-threshold GPU path**: EXACT MATCH with CPU (`gpu_06_fixed.png` == `cpu_05_fixed.png`)
- **Readback path**: Confirmed correct via modes 98 (checkerboard) and 99 (all-white)
- **Raw gray passthrough**: GPU gray buffer matches CPU at all tested positions
- **Descriptor heap infrastructure**: D3D12 context, root signatures, pipeline creation

## FIXED (May 3, 2026)
- **GPU adaptive binarization**: 0.55% difference (was 26-47%) - FIXED by using srv_slot parameter
- Root parameter functions (`set_roots_integral`, `set_roots_pad`, `set_roots_final`) now use the slot parameters correctly

### Broken
- **GPU adaptive binarization**: ~26-47% difference rate vs CPU (depending on bg_max approach)
- **GPU bg_max passes**: Produce constant zeros for all pixels (both horizontal and vertical)
- **Sauvola-only output**: Severe vertical column/block corruption + horizontal striping
- **Otsu-only output**: Columnar/transposed appearance — stride or width misinterpretation

## Architecture Overview

### Pipeline Stages (multi-pass adaptive)
1. **Pad** — `build_reflect_padded.hlsl` — Reflect-pads input gray → `paddedGray` + `paddedGraySq`
2. **Integral H** — `integral_horizontal_prefix.hlsl` — Row-wise prefix sums of padded gray → `rowIntegral`
3. **Integral H (sq)** — `integral_horizontal_prefix_f32.hlsl` — Row-wise prefix sums of padded gray² → `rowIntegralSq` (f32)
4. **Integral V** — `integral_vertical_prefix.hlsl` — Column-wise accumulation of row prefix → `integral` (u32)
5. **Integral V (sq)** — `integral_vertical_prefix_f32.hlsl` — Same for squared → `integralSq` (f32)
6. **BG Max H** — `bg_max_horizontal.hlsl` — Row-wise max-filter → `bgTmp` (u32) **[BROKEN]**
7. **BG Max V** — `bg_max_vertical.hlsl` — Col-wise max-filter on bgTmp → `bgBuffer` (u32) **[BROKEN]**
8. **Final Fusion** — `sauvola_otsu_final_from_integral.hlsl` — Reads gray + integral + integralSq + bgBuffer → outputs fused binary

### Descriptor Heap (16 slots)
All passes share a single `ID3D12DescriptorHeap` with 16 `CBV_SRV_UAV` entries.
Each pass writes its SRV/UAV descriptors into slots, then sets root signature + root parameters.

## Root Cause Analysis

### Issue 1: Descriptor Heap Aliasing
**Status**: Confirmed, workaround in place, but not fully resolved.

D3D12 command lists are **recorded first, executed later**. When recording:
- Pass A writes descriptors to slots 0, 1
- Pass B overwrites slots 0, 1 with different views
- Pass C overwrites slots 0, 1 again
- `ExecuteCommandList()` is called → GPU sees only pass C's descriptors for all passes

**Evidence**: bg_max passes produce constant zeros. When the bg_max descriptor setup was in place but the final pass later overwrote those same slots, the GPU reads from the wrong buffer during bg_max execution.

**Workaround**: Background estimate computed on CPU and uploaded to `bg_buffer`. This brought difference from ~47% → ~26%.

**Failed attempt**: Assigning unique non-overlapping slots per pass (pad:0-2, integral_h:3-4, integral_h_f32:5-6, integral_v:7-8, integral_v_f32:9-10, final:11-15) actually **made things worse** (26% → 40%). This suggests the previous aliasing pattern was accidentally "working" for some intermediate passes, or the unique slot root parameter mapping is incorrect.

### Issue 2: Root Parameter Ordering Mismatch
Each pipeline's root signature declares parameters in a specific order:
- **integral passes**: `[SRV_table, UAV_table, CBV]` → root indices 0, 1, 2
- **bg passes**: `[SRV_table, UAV_table, CBV]` → root indices 0, 1, 2
- **pad pass**: `[SRV_table, UAV_table_0, UAV_table_1, CBV]` → root indices 0, 1, 2, 3
- **final pass**: `[SRV_0, SRV_1, SRV_2, SRV_3, UAV, CBV]` → root indices 0, 1, 2, 3, 4, 5

But `set_roots_integral` always sets root index 0 for SRV and 1 for UAV:
```rust
fn set_roots_integral(&self, buffers, _srv_slot, uav_slot) {
    SetComputeRootDescriptorTable(0, gpu_srv);  // SRV at slot _srv_slot
    SetComputeRootDescriptorTable(1, gpu_uav);  // UAV at slot uav_slot
    SetComputeRootConstantBufferView(2, ...);    // CBV
}
```

The `_srv_slot` parameter is **unused** (prefixed with underscore). The SRV GPU descriptor is always computed from `gpu_heap_start` (slot 0), not from `_srv_slot`. This means even with unique slots, integral_h always reads from slot 0's descriptor — which may have been overwritten by a later pass.

### Issue 3: Root Parameter Index vs Slot Index Confusion
The `_srv_slot` and `uav_slot` parameters to `set_roots_*` are intended as **descriptor heap slot indices** (used to compute the GPU descriptor handle offset). They are NOT the root parameter index. The root parameter index is hardcoded (0 for SRV, 1 for UAV).

But `set_roots_integral` ignores `_srv_slot` entirely:
```rust
let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
let gpu_srv = gpu_heap_start;  // Always slot 0, not _srv_slot!
```

So even though we pass different slot numbers (e.g., 3 for integral_h SRV), the actual GPU descriptor used always points to slot 0.

## What's Needed

### FIXED NOW
- `set_roots_integral`, `set_roots_pad`, `set_roots_final` now correctly use `srv_slot` parameter:
```rust
fn set_roots_integral(&self, buffers, srv_slot, uav_slot) {
    let mut gpu_srv = self.ctx.get_gpu_descriptor_handle(srv_slot);
    let mut gpu_uav = self.ctx.get_gpu_descriptor_handle(uav_slot);
    SetComputeRootDescriptorTable(0, gpu_srv);
    SetComputeRootDescriptorTable(1, gpu_uav);
    SetComputeRootConstantBufferView(2, ...);
}
```

### Remaining 0.55% Difference
The GPU adaptive binarization now matches closely (53,410 / 9,724,293 pixels differ = 0.55%).
The split is roughly even (27,571 CPU-only BG vs 25,839 GPU-only BG), suggesting this may be
floating-point precision or minor algorithm differences, not a descriptor issue.

### Longer-term Options
1. **Investigate remaining 0.55%** — Compare integral values, check for precision issues
2. **Use multiple descriptor heaps** — one per pass, avoids all aliasing complexity
3. **Record and execute each pass separately** — submit → wait → record next (slow but clean)

## File Locations
- `lege-gpu/src/binarization/hlsl/mod.rs` — dispatch logic, descriptor setup
- `lege-gpu/src/binarization/hlsl/shaders/*.hlsl` — shader sources
- `lege-gpu/src/resize/hlsl/dx12.rs` — D3D12Context, descriptor heap, get_cpu/gpu_descriptor_handle
- `lege-gpu/src/bin/compare_binarize.rs` — validation binary

## Key Metrics
- Test image: 2439×3987 = 9,724,293 pixels
- Fixed threshold: 0% difference (exact match)
- Adaptive with CPU bg (before fix): 26.4% difference (2,570,205 pixels differ)
- Adaptive after fix: 0.55% difference (53,410 pixels differ)
