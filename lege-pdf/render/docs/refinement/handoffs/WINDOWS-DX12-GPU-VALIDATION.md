# Windows DX12 GPU renderer validation

Status: ready for a focused Windows session
Prepared: 2026-07-26
Refreshed: 2026-07-27 after the Sweep-15 residual and malformed-page-tree
closures

## Scope

The Linux implementation work is complete for this pass:

- `Auto` prepares/classifies before WGPU startup, so CPU-routed pages do not
  enumerate adapters.
- wgpu's device-loss callback marks the shared context unhealthy and preserves
  the driver-provided reason.
- the process-wide shared context is replaceable after loss;
- `ExperimentalImageRenderer` drops the affected backend, pipelines, and upload
  cache, falls back for the failed request, then retries initialization on the
  next eligible request;
- telemetry exposes `gpu_initializations` and `gpu_recoveries`;
- deterministic injected-loss tests cover fallback and reconstruction;
- real Vulkan runs pass on an RTX 4060 and Intel Iris Xe.

What remains cannot be established by another normal Linux render: validate the
DX12 path and induce an actual Windows driver/device-loss notification.

## Handoff decision

The renderer is ready for the Windows/DX12 pass. Another Linux refinement pass
is not a prerequisite:

- CPU rendering is the complete fallback path for GPU declines and failures.
- `Auto` commits only a complete validated GPU page; any typed GPU error or
  unexpected panic discards that attempt and restarts the immutable request
  from its first operation on CPU.
- GPU page execution permits multiple pages in flight and does not introduce a
  one-page-at-a-time software queue.
- The major actionable Sweep-15 classes are closed: MRC mask minification,
  scan-tone/JPEG investigations, patterned text and stencils, malformed Type 3
  state, markup appearances, CCITT parameters, JP2 palettes, font fallbacks,
  and malformed page trees.

The following work remains useful but is **nonblocking** for DX12 validation.
Keep each as a separate focused session rather than expanding the Windows
driver pass:

- remaining JPX/JBIG2 spec-edge compatibility cases;
- general ICC profile/CMM coverage beyond the supported RGB matrix/TRC and
  CMYK Lab-LUT shapes;
- anisotropic and rotated stroke pens;
- GPU-native mixed-content performance sufficient for `Auto` promotion
  (forced GPU is implemented, while `Auto` intentionally retains CPU routing);
- a real image-only active page-level soft-mask corpus oracle;
- the resident GPU renderer-to-postprocess/presenter handoff;
- a post-fix corpus sweep to discover new unknowns after the focused Sweep-15
  closures.

The full `pdf-render-wgpu --all-targets` run now also covers the newer image
blend modes, patterned-stencil bridge, page-level soft-mask preparation,
atomic fallback/quarantine, and forced-GPU mixed-content/text-clip paths.
Those synthetic gates belong in the Windows test run even though the focused
real-PDF commands below remain centered on the highest-value image workloads.

## Prepare

From a Developer PowerShell in the repository root:

```powershell
$env:WGPU_BACKEND = "dx12"
$env:WGPU_REQUIRE_REAL_GPU = "1"
$env:LEGE_PDF_IMAGE_RENDERER = "auto"

cargo test -p pdf-render-wgpu --all-targets -- --nocapture
cargo build --release -p lege-viewer --example pdf_tile_profile
cargo build --release -p lege-viewer --example pdf_parallel_profile
```

Copy or mount the established fixtures and set their local paths:

```powershell
$BilevelPdf = "D:\pdf-fixtures\Dmitri Obolensky- Byzantium and the Slavs.pdf"
$JpegPdf = "D:\pdf-fixtures\buddhasahibsmenw0000alle_1.pdf"
$MrcPdf = "D:\pdf-fixtures\Appian Roman History.pdf"
$StencilPdf = "D:\pdf-fixtures\Attitudes-of-Zionist-intellectuals.pdf"
$HardMaskPdf = "D:\pdf-fixtures\Argentine Democracy.pdf"
$AnalyticClipPdf = "D:\pdf-fixtures\Byzantium, Latin Romania and the Mediterranean.pdf"
```

## A. Prove prepare-first routing on DX12

Run the minified bilevel case:

```powershell
target\release\examples\pdf_tile_profile.exe $BilevelPdf 0 2 3 12
```

Required result:

- no `wgpu: found` or `wgpu: selected adapter` line;
- `gpu_initializations: 0`;
- all 48 requests route to CPU with zero fallback.

Then run the eligible high-resolution case:

```powershell
target\release\examples\pdf_tile_profile.exe $BilevelPdf 0 4 3 12
```

Required result:

- the selected hardware adapter reports `Dx12`;
- `gpu_initializations: 1`, `gpu_recoveries: 0`;
- all 48 requests route to GPU with zero fallback;
- cold and warm checksums match each other.

Finally exercise the high-value JPEG workload:

```powershell
target\release\examples\pdf_tile_profile.exe $JpegPdf 180 0 5 12
```

Record adapter metadata, cold/warm timing, routing telemetry, and checksum.
CPU/GPU pixel parity is not a gate; visual correctness and stable warm
checksums are.

Confirm that DX12 also keeps multiple pages in flight:

```powershell
$env:LEGE_PDF_IMAGE_RENDERER = "cpu"
target\release\examples\pdf_parallel_profile.exe $JpegPdf 180 8 0 0 1 5
target\release\examples\pdf_parallel_profile.exe $JpegPdf 180 8 0 0 8 5

$env:LEGE_PDF_IMAGE_RENDERER = "auto"
target\release\examples\pdf_parallel_profile.exe $JpegPdf 180 8 0 0 1 5
target\release\examples\pdf_parallel_profile.exe $JpegPdf 180 8 0 0 8 5
```

The Linux Vulkan reference for the four warm medians is CPU1 **2551.587 ms**,
CPU8 **461.894 ms**, GPU1 **354.022 ms**, and GPU8 **76.023 ms**. Exact DX12
timings need not match, but GPU8 must improve materially over GPU1 and must
beat CPU8. Stable checksums and eight-worker GPU telemetry with no fallback
are required. A GPU8 result close to GPU1 would indicate serialization and
should become the sole focus of the Windows session.

Exercise the later opacity-plane expansion as well:

```powershell
target\release\examples\pdf_tile_profile.exe $MrcPdf 196 2 5 12
target\release\examples\pdf_tile_profile.exe $StencilPdf 145 4 5 12
target\release\examples\pdf_tile_profile.exe $HardMaskPdf 108 2 5 12
target\release\examples\pdf_tile_profile.exe $AnalyticClipPdf 290 0 5 12
```

All must route every request to GPU without fallback and produce a stable warm
checksum. These cover a codec-backed JPX/JBIG2 image `/SMask`, a solid-colour
CCITT stencil brush, an explicit hard image `/Mask`, and a nested analytic
path clip around a JBIG2 stencil, respectively. The Linux analytic-clip
reference is **93.839 ms CPU vs 24.359 ms GPU warm** for twelve tiles.

## B. Induce and observe real device loss

Use a long eligible JPEG run so there is time to reset the graphics driver:

```powershell
target\release\examples\pdf_tile_profile.exe $JpegPdf 180 0 5000 12 2>&1 |
    Tee-Object -FilePath gpu-dx12-device-loss.log
```

While it is actively rendering, press `Win+Ctrl+Shift+B` once. Windows should
briefly blank or flicker the display. Do not disable the adapter in Device
Manager as part of this test.

A conclusive recovery has all of these:

1. `wgpu: shared device lost: ...` appears with a driver reason;
2. the in-flight request falls back to CPU rather than terminating the process;
3. adapter discovery runs again;
4. final telemetry has `gpu_fallbacks >= 1`, `gpu_initializations >= 2`, and
   `gpu_recoveries >= 1`;
5. GPU page counts continue increasing after the recovery, and the process
   exits normally.

The Windows reset shortcut does not guarantee that every driver exposes a
WebGPU device loss. If the run completes with no loss callback, record it as
“loss not induced,” not as a recovery failure. Keep the log and adapter/driver
version for a later focused fault-injection session.

## Report template

```text
Windows version:
GPU / driver:
Adapter line:
DX12 tests:
Bilevel scale 2 telemetry:
Bilevel scale 4 telemetry:
JPEG page 180 cold/warm:
Parallel whole-page CPU1/CPU8:
Parallel whole-page GPU1/GPU8:
Reset produced device-loss callback: yes/no
Recovery telemetry:
Process survived and resumed GPU work: yes/no/not induced
Log path:
```

If DX12 initialization or recovery fails, preserve the complete log. The next
session should remain focused on that single failure; do not expand masks,
clips, blends, or mixed-content coverage in the same pass.

## Windows validation result — 2026-07-27

The focused Windows pass completed successfully for normal DX12 rendering and
parallel execution.

```text
Windows version: build 26200.8875, 25H2
GPU / driver: NVIDIA GeForce RTX 4060 Laptop GPU / 596.36
Adapter line: NVIDIA GeForce RTX 4060 Laptop GPU (Dx12, DiscreteGpu)
DX12 tests: pdf-render-wgpu --all-targets passed (34 passed, 0 failed)
Bilevel scale 2 telemetry: GPU 0, CPU 48, fallback 0, initializations 0
Bilevel scale 4 telemetry: GPU 48, CPU 0, fallback 0, initializations 1
JPEG page 180 cold/warm: 1441.692 ms / 175.244 ms
Parallel whole-page CPU1/CPU8: 2412.185 ms / 414.019 ms
Parallel whole-page GPU1/GPU8: 446.148 ms / 94.929 ms
Reset produced device-loss callback: no
Recovery telemetry: unavailable; no callback was exposed before manual stop
Process survived and resumed GPU work: not induced
Log path: gpu-dx12-device-loss.stdout.log / gpu-dx12-device-loss.stderr.log
```

The scale-2 bilevel run did not enumerate an adapter. The scale-4 bilevel and
JPEG runs selected DX12, remained entirely GPU-routed, had zero fallback, and
produced stable cold/warm checksums. GPU8 was 4.70× faster than GPU1 and 4.36×
faster than CPU8, so the Windows path does not show the feared one-page-at-a-
time serialization.

The MRC soft-mask, solid stencil, explicit hard-mask, and analytic-clip
fixtures also routed all 72 requests per case to DX12 with zero fallback and
stable checksums. Their warm medians were 160.577 ms, 140.783 ms, 154.819 ms,
and 104.778 ms respectively.

`Win+Ctrl+Shift+B` caused the expected display flicker during a long JPEG run,
but wgpu emitted no device-loss callback, adapter rediscovery, fallback, or
recovery message during approximately 13 minutes before the run was stopped.
Per the criterion above, record this as **loss not induced**, not a recovery
failure. Deterministic injected-loss reconstruction remains covered by the
passing automated test.
