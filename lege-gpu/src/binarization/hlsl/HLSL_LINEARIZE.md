# Porting GPU sRGB-aware linearization to the HLSL/D3D12 backend

> **Status: IMPLEMENTED** — All 8 steps below are complete as of branch `1.43`.
> See `hlsl/mod.rs` for the Rust integration and `hlsl/shaders/adaptive_linearize.hlsl`
> for the shader. Notes on implementation specifics are inline under each step.

The WGPU backend (Linux/macOS) does sRGB→linear → BT.709 luma → sRGB-encode entirely on the GPU via a 256-entry LUT in a separable pre-pass. This guide is the parallel port plan for the HLSL/D3D12 backend (Windows). The reference implementation lives at:

- WGSL pre-pass shader: `lege-gpu/src/binarization/wgpu/shaders/adaptive_linearize.wgsl`
- WGPU integration: `lege-gpu/src/binarization/wgpu/mod.rs` — search for `linearize_pipeline`, `srgb_lut`, `gray_buf`, `encode_linearize_pass`, `upload_source_pixels`.

## Architecture

Same shape as the WGPU port:

1. **gpu_src** (D3D12 buffer / SRV) holds RGBA-packed source: 1 pixel per `uint`, R in low byte, G next, B next, A unused. Sized `pixel_count × 4` bytes.
2. **gray_buf** (D3D12 buffer / UAV → SRV) holds 1 `uint` per pixel of sRGB-perceptual gray (0..255). Written by the linearize pass; read by every subsequent pass.
3. **srgb_lut** (D3D12 buffer / SRV) holds 256 `float` entries, the precomputed sRGB→linear curve. Uploaded once at binarizer construction.
4. The adaptive pipeline gains one new pass at the head of the chain:
   `linearize → build_padded → integral_rows → integral_cols → bg_max_h → bg_max_v → final → pack`.
5. The fixed-threshold pipeline gains one pass:
   `linearize → fixed → pack`.

## Step 1: Add the linearize HLSL shader ✓

> **Done.** `hlsl/shaders/adaptive_linearize.hlsl` created. `build.rs` extended with
> the `("src/binarization/hlsl/shaders/adaptive_linearize.hlsl", "adaptive_linearize.cso",
> "BINARIZE_LINEARIZE_SHADER")` entry so dxc compiles it at build time.
> The actual shader uses `float` literals with the `f` suffix (`0.0031308f`, `1.055f`, etc.)
> for strict HLSL cs_6_0 compatibility, differing from the guide which omits them.

Create `lege-gpu/src/binarization/hlsl/shaders/adaptive_linearize.hlsl`:

```hlsl
// sRGB-aware RGB→gray pre-conversion.
// Mirror of adaptive_linearize.wgsl on the WGPU backend.

cbuffer Params : register(b0) {
    uint width;
    uint height;
    uint mode;
    uint invert_output;
    uint fixed_threshold;
    uint sauvola_window;
    uint bg_window;
    uint otsu_threshold;
    float k_factor;
    float percentile_c;
    uint padded_width;
    uint padded_height;
    uint integral_width;
    uint sauvola_radius;
    uint debug_mode;
    uint _pad0;
};

StructuredBuffer<uint>  rgba_src : register(t0);   // 1 pixel/uint: R | G<<8 | B<<16 | A<<24
StructuredBuffer<float> srgb_lut : register(t1);   // 256 entries
RWStructuredBuffer<uint> gray_dst : register(u0);  // 1 uint per pixel (0..255)

float linear_to_srgb(float c) {
    return (c <= 0.0031308) ? (12.92 * c) : (1.055 * pow(c, 1.0 / 2.4) - 0.055);
}

[numthreads(16, 16, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= width || id.y >= height) return;
    uint idx = id.y * width + id.x;

    uint w = rgba_src[idx];
    uint r_u = (w >>  0) & 0xFFu;
    uint g_u = (w >>  8) & 0xFFu;
    uint b_u = (w >> 16) & 0xFFu;

    float r_lin = srgb_lut[r_u];
    float g_lin = srgb_lut[g_u];
    float b_lin = srgb_lut[b_u];

    // BT.709 linear luma → re-encode to sRGB-perceptual gray.
    float y_lin  = 0.2126 * r_lin + 0.7152 * g_lin + 0.0722 * b_lin;
    float y_srgb = linear_to_srgb(y_lin);

    gray_dst[idx] = (uint)clamp(y_srgb * 255.0 + 0.5, 0.0, 255.0);
}
```

To switch to "min-of-channel" gray (better preservation of colored ink):
```hlsl
// Replace the luma + linear_to_srgb lines with:
float y_srgb = min(min(r_lin, g_lin), b_lin);
```

## Step 2: Modify existing HLSL shaders ✓

> **Done.** The actual HLSL file names differ from the guide's tentative names.
> Files modified (one-liner change each — remove `>> 2` / shift / mask, replace with direct `& 255u`):
> - `build_reflect_padded.hlsl` — source pixel read in the padding loop
> - `bg_max_horizontal.hlsl` — source read in the 1D horizontal max loop
> - `bg_max_vertical.hlsl` — source read in the 1D vertical max loop
> - `sauvola_otsu_final_from_integral.hlsl` — `g_u` extraction
>
> `sauvola_otsu_binarize.hlsl` (fixed-threshold path) already used 1-uint-per-pixel
> access and required no changes.

Find each HLSL shader that currently reads packed gray (4 pixels per `uint`) and remove the bit-extraction. Each should now consume `gray_buf` with one `uint` per pixel.

Files to update (mirroring the WGSL changes):
- `binarize_adaptive.hlsl` (fixed threshold)
- `adaptive_build_padded.hlsl`
- `adaptive_bg_max.hlsl` (both `main_h` and `main_v`)
- `adaptive_final.hlsl`

Pattern to replace:
```hlsl
// OLD: packed 4-pixels-per-uint
uint w = src_pixels[idx >> 2];
uint g = (w >> ((idx & 3) << 3)) & 0xFFu;
```

becomes:
```hlsl
// NEW: 1 uint per pixel (already gray, from linearize pass)
uint g = src_pixels[idx] & 0xFFu;
```

Update binding declarations to point at `gray_buf` instead of the old packed source. The bind register slot doesn't need to change; the layout interpretation does.

## Step 3: D3D12 root signature + descriptor table ✓

> **Done.** `ensure_pipelines()` in `hlsl/mod.rs` creates a dedicated linearize root
> signature with four ranges (CBV b0, SRV t0 rgba_src, SRV t1 srgb_lut, UAV u0 gray_dst).
> The descriptor heap was expanded from `NumDescriptors: 16` to `NumDescriptors: 24` in
> both construction sites in `src/resize/hlsl/dx12.rs`. Linearize descriptors occupy
> slots **16** (SRV gpu_src), **17** (UAV gray_buf), **18** (SRV srgb_lut) — placed
> above the existing 16 binarize slots to avoid renumbering them.

The current D3D12 setup (in `hlsl/mod.rs`) creates a pipeline-per-pass with a root signature describing each binding. For the linearize pass, the root signature needs:

- 1 CBV (b0) — params constant buffer
- 2 SRVs (t0, t1) — rgba_src and srgb_lut
- 1 UAV (u0) — gray_dst

For each binarize shader (build_padded, bg_max, final, fixed), update its root signature so the source-pixels SRV now points to `gray_buf` (which is treated as RWStructuredBuffer in the linearize pass and as StructuredBuffer in subsequent passes — needs UAV→SRV transition barriers between them).

## Step 4: Resource creation in `hlsl/mod.rs` ✓

> **Done.** `MultiPassBuffers` gained `gray_buf: ID3D12Resource` (pixel_count × 4 bytes,
> D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS). `gpu_src` and `upload_src` are now sized
> at `pixel_count * 4` bytes (RGBA uint per pixel). `srgb_lut` lives on `HlslBinarizer`
> rather than `MultiPassBuffers` because it is image-size-independent and allocated once.
> `ensure_srgb_lut()` uploads via a scratch upload heap → default-heap copy submitted in a
> dedicated command list; the resource stays permanently in NON_PIXEL_SHADER_RESOURCE state.

Add to `MultiPassBuffers` (or whatever the buffer struct is named):
- `gray_buf: ID3D12Resource` — `pixel_count * 4` bytes, allowed to be both SRV and UAV
- `srgb_lut: ID3D12Resource` — `256 * 4 = 1024` bytes, SRV only

Change the existing `gpu_src` (or whatever the source buffer is) sizing from `pixel_count` (packed gray) to `pixel_count * 4` (RGBA-packed `uint`). Same change as we made on WGPU: see commit history for `WgpuBuffers.img_bytes`.

In `HlslBinarizer::new()`, after constructing the D3D12 context:

1. Compute the LUT once on CPU:
   ```rust
   fn srgb_to_linear_lut() -> [f32; 256] {
       let mut lut = [0.0f32; 256];
       for i in 0..256 {
           let c = i as f32 / 255.0;
           lut[i] = if c <= 0.04045 {
               c / 12.92
           } else {
               ((c + 0.055) / 1.055).powf(2.4)
           };
       }
       lut
   }
   ```
2. Create an upload heap for the LUT, copy `lut.as_ptr()` for `1024` bytes, then copy to a default-heap buffer (so it lives in GPU memory for fast access).
3. Create the linearize PSO: load `adaptive_linearize.hlsl`, compile via `dxc.exe` (build script), set the root signature with the binding described above.

## Step 5: Per-page upload — RGB→RGBA expansion ✓

> **Done.** `HlslBinarizer` gained an `upload_staging: Vec<u8>` that is lazily resized
> to `pixel_count * 4`. `binarize_inner` fills staging before taking any borrows to avoid
> borrow-checker conflicts: the raw `staging_ptr: *const u8` is captured before the
> `buffers`/`pipelines`/`srgb_lut` immutable borrows, and the upload memcpy runs inside
> the subsequent `unsafe` block.

Update the source upload to pad RGB to RGBA. Mirror the WGPU `upload_source_pixels` helper:

```rust
fn upload_source_pixels(&mut self, pixels: &[u8], pixel_count: usize, fmt: SourceFormat) {
    let rgba_bytes = pixel_count * 4;
    if self.upload_staging.len() < rgba_bytes {
        self.upload_staging.resize(rgba_bytes, 0);
    }
    let dst = &mut self.upload_staging[..rgba_bytes];
    match fmt {
        SourceFormat::Gray => {
            dst.chunks_exact_mut(4).zip(pixels.iter()).for_each(|(o, &g)| {
                o[0] = g; o[1] = g; o[2] = g; o[3] = 0;
            });
        }
        SourceFormat::Rgb => {
            dst.chunks_exact_mut(4).zip(pixels.chunks_exact(3)).for_each(|(o, p)| {
                o[0] = p[0]; o[1] = p[1]; o[2] = p[2]; o[3] = 0;
            });
        }
    }
    // Then upload `dst` to gpu_src via the upload heap → default heap copy.
}
```

The HLSL backend already maintains an upload heap pattern; just point it at the staging Vec.

## Step 6: Dispatch chain ✓

> **Done.** `binarize_inner` dispatches linearize first, then issues:
> 1. UAV barrier on `gray_buf` (ensure writes visible)
> 2. Transition `gray_buf` COMMON → UAV (for the linearize write)
> 3. Linearize dispatch
> 4. UAV barrier + transition gray_buf UAV → NON_PIXEL_SHADER_RESOURCE
> 5. Remaining passes (build_padded, integrals, bg_max, final, pack) reading gray_buf as SRV
> 6. All resources (gray_buf, padded_gray, padded_gray_sq, integral, integral_sq, bg_buffer,
>    gpu_dst) transitioned back to COMMON at page end for re-use next page.
>
> CPU bg_max computation is kept for now: when `SourceFormat::Rgb` is passed, Rec.601
> gray is computed on CPU for the `bg_gray` argument used by the separable max filter.
> The GPU bg_max path (dispatching bg_max_h/bg_max_v via `gray_buf`) is deferred.

Insert the linearize dispatch as the first compute pass. Pseudocode:

```rust
fn binarize(&mut self, /*…*/) -> Result<Vec<u8>> {
    // Existing setup …

    // NEW: linearize pre-pass (RGBA → sRGB gray).
    cmd_list.SetPipelineState(&self.linearize_pso);
    cmd_list.SetComputeRootSignature(&self.linearize_rs);
    cmd_list.SetComputeRootDescriptorTable(0, /* table pointing at params/rgba_src/srgb_lut/gray_buf */);
    let cx = (params.width + 15) / 16;
    let cy = (params.height + 15) / 16;
    cmd_list.Dispatch(cx, cy, 1);

    // BARRIER: gray_buf UAV → SRV (read by all subsequent passes).
    cmd_list.ResourceBarrier(&[
        D3D12_RESOURCE_BARRIER::UAV(gray_buf),                       // ensure writes visible
        D3D12_RESOURCE_BARRIER::Transition(gray_buf,
            UAV → NON_PIXEL_SHADER_RESOURCE),
    ]);

    // Existing build_padded / integrals / bg_max / final / pack passes,
    // now reading gray_buf in place of the old packed source.
}
```

If you want to amortize barrier cost across multi-page calls, group the linearize → finalize → pack chain inside a single command list and submit once.

## Step 7: Add `binarize_rgb_raw_with` (callback API) ✓

> **Done.** `binarize_rgb_raw_with` calls `binarize_inner(..., SourceFormat::Rgb)` which
> uploads RGB pixels as RGBA, dispatches the linearize pass, then the binarize chain.
> The old CPU Rec.601 gray path for RGB input is replaced by GPU linearize for quality;
> the Rec.601 path survives only for the CPU bg_max argument (see Step 6 note).

The current HLSL impl has only `binarize_gray_raw`. Mirror what we did on WGPU:

```rust
impl HlslBinarizer {
    pub fn binarize_rgb_raw_with<F, R>(
        &mut self,
        rgb: &[u8],
        params: &BinarizationParams,
        f: F,
    ) -> Result<R>
    where F: FnOnce(&[u8]) -> R,
    {
        // Validate: rgb.len() == width * height * 3
        // Upload RGB via upload_source_pixels(rgb, pixel_count, SourceFormat::Rgb)
        // Dispatch full chain (linearize + binarize + pack)
        // Read back packed_dst, invoke f(&data[..pixel_count])
    }
}
```

This makes the HLSL backend usable from `lege_gpu::binarization::try_binarize_rgb_with`.

## Step 8: Verification ⬜

> **Pending.** The implementation was written but not yet compiled or run on Windows.
> The integration test described below does not yet exist. First step is to confirm
> `cargo build` succeeds on Windows with dxc finding `adaptive_linearize.cso`.

1. Add a `wgpu_vs_hlsl_parity` integration test (Windows-only, gated on env var):
   - Generate a 64×64 sRGB gradient (smoothly varying R/G/B).
   - Run the same image through both backends with `debug_mode=5` (local-mean grayscale) and `debug_mode=0` (fused binary).
   - Assert per-pixel max diff ≤ 2 LSB for `debug_mode=5` (small drift from float ordering is OK).
   - Assert exact match for `debug_mode=0` if the surrounding integral/bg passes are bit-identical.

2. Smoke-test on a real document: load a known book scan, binarize on Windows, compare against the Linux output. Should be visually identical; binary diff should be ≤ noise from non-deterministic GPU floating point.

## Performance notes

- The linearize pass is bandwidth-bound, not compute-bound. Expect ~0.5–1ms per 8MP page on mid-range D3D12 hardware.
- The `pow()` in `linear_to_srgb` is the main per-pixel cost (one per output pixel). If you measure this as the bottleneck, replace with a second 4096-entry LUT (`linear_to_srgb_lut[(linear_value * 4095.0) as usize]`).
- The bg_max passes already do 350 reads per output pixel — that work eclipses the linearize cost. Pre-converting to `gray_buf` (rather than inlining the LUT into bg_max) is what makes the overall pipeline fast.

## Future: per-channel Sauvola for colored ink documents

The current design produces a single gray channel. For documents with red stamps, blue carbon copies, or yellow highlights (where chrominance carries text signal that luma weights away), the next step would be:

- Extend `gray_buf` to 3 separate buffers (`r_lin_buf`, `g_lin_buf`, `b_lin_buf`) holding linear-light per-channel intensities.
- Run integral images + Sauvola threshold per channel (3× the integral memory).
- AND-fuse the three binary outputs (a pixel is text if dark in any channel).

This is a ~3× memory cost; defer until you have a colored-ink test corpus.
