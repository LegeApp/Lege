# Agent Notes

## Local Lege Test Paths

- Large test PDF used for repeated page-processing runs:
  `/mnt/Samsung980_1TB/to-sort/test/wheelsofcommerce00brau_1.pdf`
- Linux runtime folder containing `pdfium.so` and related runtime files:
  `/home/dk/Desktop/lege-run/installer/linux64/`

## Current Performance Context

- Current Linux work targets Vulkan-relative improvement for YOLO/Winograd timing.
- Prior Windows testing used DirectX 12, which measured roughly 40-50 ms/page faster
  than Vulkan on the same workload, so do not compare Linux/Vulkan results against
  the old absolute DX12 page timings.
- For local CLI iteration, prefer `cargo build --profile debug-fast --bin lege`
  and `target/debug-fast/lege`. The `debug-fast` profile is optimized with LTO
  disabled, avoiding the long release LTO link step.
- Winograd is enabled by default in the YOLO graph rewrite. Use
  `LEGE_DISABLE_WINOGRAD=1` only for direct-conv baseline timing comparisons.
