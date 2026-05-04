# GPU Binarization: Lessons Learned

Benchmark run: 313 pages, 753×1200 px RGB PNG, Intel/AMD iGPU (Vulkan via WGPU),
release build, Linux.

---

## Results

| Path | avg ms/page | total (313 pg) | p95 | min | max |
|---|---|---|---|---|---|
| GPU (WGPU) | **2.00** | 624 ms | 2.38 | 1.70 | 2.93 |
| CPU (rayon) | 10.10 | 3163 ms | 12.91 | 7.89 | 17.12 |
| **Speedup** | **5.06×** | — | — | — | — |

GPU warm-up (shader compile + first buffer alloc): **23 ms** (one-time, not per-page).

> **Caveat on the comparison**: GPU timing covers only the GPU dispatch + readback.
> The CPU-side constant computation (dilate_square_reflect → percentile_c → otsu_threshold)
> was pre-computed per page before timing started.  That same work is embedded inside the
> CPU path timing.  Even if you add 3–4 ms/page of CPU constant overhead to the GPU number,
> the GPU path stays ~2× faster on this hardware.

---

## Architecture choices that paid off

### 1. Multi-pass integral image instead of direct nested loops
The naive O(win²) shader (the old `binarize_adaptive.wgsl`) would be
`31²=961` memory reads per pixel at the minimum Sauvola window.  The integral
image approach reduces this to a fixed handful of reads per pixel after a
one-time prefix-sum pass.  This is what makes the GPU path competitive.

### 2. One `WgpuBinarizer` instance reused across all pages
Buffer allocation and pipeline compilation happen once at construction, not per
page.  Per-page cost is purely GPU dispatch + PCIe readback.  First-page
penalty over steady-state was only **0.98 ms** (2.93 vs 1.95 ms) — already
amortised after a single warm-up page.

### 3. Warm-up page before timed run
The first call after `WgpuBinarizer::new()` triggers any remaining driver-side
JIT.  Running one excluded warm-up page eliminates this from production latency.
23 ms one-time cost is irrelevant for a 300-page document.

### 4. Separable max filter for background estimation (two passes)
`adaptive_bg_max.wgsl` uses two separable 1D max passes (`main_h`, `main_v`)
sharing one BindGroupLayout.  This matches the CPU `maxfilter_1d_u8` + vertical
loop and is significantly cheaper than a 2D kernel for the same bg_window size.

### 5. `max_storage_buffers_per_shader_stage: 8`
`downlevel_defaults()` gives 4, which is too few for the 5-storage-buffer final
pass.  Raising only this limit (keeping all others at downlevel) keeps the
adapter requirements low while unblocking the final shader.

---

## What surprised us

### GPU time is extremely consistent
The full 313-page run stayed within 1.70–2.93 ms.  There was no per-document
Vulkan re-compilation or buffer re-allocation noise.  This confirms the
"allocate once, write per page" buffer model is effective.

### Warm-up is cheaper than expected
23 ms total warm-up, not ~200–500 ms that HLSL/DirectX pipelines sometimes
show on first dispatch.  WGPU on Vulkan backends has fast pipeline caching.

### CPU constant computation is the bottleneck of the GPU path in practice
The timed GPU run is 624 ms total.  But `compute_adaptive_gpu_constants` (the
CPU-side dilate + percentile + otsu) runs for each page before dispatch.  In
real pipeline usage this CPU work runs in parallel with the previous page's GPU
readback — so it may not add latency — but it needs to be measured in a
pipelined context to know for certain.

---

## HLSL (DirectX) linearize pass — implemented

The linearize pre-pass from the WGPU backend was ported to the D3D12/HLSL backend in
branch `1.43`. Summary of what changed beyond the base HLSL port:

| What | Where |
|---|---|
| New shader `adaptive_linearize.hlsl` | `hlsl/shaders/` — mirrors `adaptive_linearize.wgsl` |
| `build.rs` shader list extended | Compiled to `adaptive_linearize.cso` via dxc at build time |
| Descriptor heap expanded 16 → 24 entries | `src/resize/hlsl/dx12.rs` (both construction sites) |
| Linearize descriptor slots 16/17/18 | SRV gpu_src, UAV gray_buf, SRV srgb_lut |
| `gray_buf` added to `MultiPassBuffers` | 1 u32 per pixel, UAV+SRV, pixel_count × 4 bytes |
| `srgb_lut` on `HlslBinarizer` | Uploaded once via scratch command list; stays in `NON_PIXEL_SHADER_RESOURCE` |
| `upload_staging: Vec<u8>` on `HlslBinarizer` | Per-page RGB→RGBA expansion before GPU upload |
| Downstream shaders read 1 u32/pixel | `build_reflect_padded`, `bg_max_horizontal`, `bg_max_vertical`, `sauvola_otsu_final` |
| `binarize_rgb_raw_with` uses GPU linearize | Old CPU Rec.601 path replaced for quality |
| CPU bg_max kept | Rec.601 gray still computed on CPU for `bg_gray` argument; GPU bg_max dispatch deferred |

**Performance expectation on D3D12**: The linearize pass is bandwidth-bound (~0.5–1 ms
per 8 MP page on mid-range hardware). The `pow()` call in `linear_to_srgb` is the main
per-pixel cost; a second 4096-entry LUT can replace it if profiling shows it dominates.
The bg_max passes do ~350 reads/pixel and will dominate total GPU time regardless — the
linearize cost is negligible by comparison. DX12 pipeline pre-compilation via dxc means
the warm-up cost should be near-zero (vs. 23 ms one-time on WGPU/Vulkan).

**Verification pending**: The code was written on branch `1.43` but not yet compiled or
run on Windows. Step 8 of `hlsl/HLSL_LINEARIZE.md` (parity test) is still open.

---

## Implications for HLSL (DirectX) port

The WGPU multi-pass design translates directly to HLSL with minimal changes:

| WGPU pass | HLSL equivalent |
|---|---|
| `adaptive_build_padded.wgsl` | Same compute shader, HLSL syntax |
| `adaptive_integral_rows.wgsl` | Row prefix scan — identical logic |
| `adaptive_integral_cols.wgsl` | Column accumulation — identical logic |
| `adaptive_bg_max.wgsl` (h+v) | Two dispatches, same separable 1D max |
| `adaptive_final.wgsl` | Same Sauvola+Otsu+fuse formula |

**Specific HLSL notes:**
- Storage buffers → `RWStructuredBuffer<uint>` and `RWStructuredBuffer<float>`
- Uniform/constant buffer → `cbuffer BinarizeParams : register(b0) { ... }`
- `workgroup_size` → `[numthreads(16, 16, 1)]` on the entry point
- `@builtin(global_invocation_id)` → `uint3 id : SV_DispatchThreadID`
- WGSL `select(false_val, true_val, cond)` → ternary `cond ? true_val : false_val`
- The `max_storage_buffers_per_shader_stage` limit does not apply to DX12 —
  `t0..t4` SRV slots are plentiful; use `register(t0)` etc. for read-only
  buffers and `register(u0)` for read-write.
- Padding: the `BinarizeParamsStd140` Rust struct (84 bytes) needs a HLSL
  constant buffer that is a multiple of 16 bytes — pad to 96 bytes.
- DX12 pipeline state objects can be pre-compiled with `dxc` at build time (see
  `build.rs` in the HLSL resize path) — this eliminates the runtime shader
  compilation cost entirely and should make warm-up negligible.

---

## Next optimisation candidates

1. **Pipeline the CPU constant computation with GPU readback.**  The CPU work
   for page N (dilate/percentile/otsu) can run while page N-1's GPU result is
   being read back over PCIe.  This would eliminate or hide the CPU overhead
   from the critical path.

2. ~~**Upload grayscale as u8 instead of u32.**~~ _Superseded._ The source buffer
   is now RGBA (4 bytes/pixel) consumed by the linearize pass; downstream passes read
   `gray_buf` at 1 u32/pixel. Byte-packing is no longer the right lever — the upload
   is gated by PCIe bandwidth, not internal GPU buffer bandwidth.

3. **Avoid the padded buffer pass when sauvola_radius is small.**  For short
   documents where the Sauvola window is 31, the padded image is only marginally
   larger than the original.  An optional direct-lookup path in `adaptive_final`
   could skip the build_padded + two integral passes.

4. **Map the readback buffer persistently.**  Current code allocates a new
   staging buffer and maps/unmaps per page.  A persistent mapped staging buffer
   would eliminate the map overhead on the readback.

5. **Batch multiple pages in one command encoder submission.**  Currently each
   page is a separate `encoder.finish()` + `queue.submit()`.  Batching 4–8
   pages per submission reduces driver overhead.
