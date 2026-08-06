# Next audit phases (after structural DWT + OpenJPEG baseline)

Prerequisite reading:

- `jp2lam-decoder-performance-audit-2026-08-05.md` (full roadmap)
- `jp2lam-vs-openjpeg-baseline-2026-08-05.md` (measured gap on `lear_q75.jp2`)

Current same-stream full RGB (after Phase 3): **jp2lam session packed ≈ 190 ms** vs **OpenJPEG decode-only ≈ 172 ms** (≈1.11×), max abs 1.

## Phase 3 — Fused packed finalization — DONE (2026-08-05)

**Landed:**

1. Fused inverse ICT/RCT → level-shift → clamp → store into packed RGB/RGBX/BGRA/CMYK for 8-bit 3-colour (+ optional aux)  
2. Parallel coarse pixel chunks for large planes  
3. Unit tests: fused ICT/RCT byte-identical to staged path  

**Measured on `lear_q75.jp2`:** 248 ms → **190 ms** (~1.11× OpenJPEG decode-only).

**Also landed (Phase 3 remainder, 2026-08-05):**

2. `Jp2Decoder::decode_into(DecodeTarget { data, stride, format, ... })` — public API; stride padding preserved  
3. Multi-tile packed path writes each tile straight into the canvas (`PackedWriteTarget`); no per-tile packed `Vec`  
4. Direct sYCC **4:2:0** packed kernel (nearest chroma + OpenJPEG `sycc_to_rgb`); no full-size Cb/Cr expand; `decode_packed_direct` accepts `YCbCr` for RGB-family formats  

**Still optional later:** true zero-copy `decode_into` (avoid intermediate owned raster copy when stride matches); 4:2:2/4:4:4 parse + kernels; multi-tile parallel decode.

## Phase 4 — Tier-1 / Tier-2 refinement

**Goal:** better core utilization once DWT memory traffic is no longer dominant.

1. Compact prevalidated `BlockJob` after Tier-2  
2. Cost estimate: `codeword_bytes + passes * W + area * W`  
3. Largest-first / bucketed Rayon scheduling  
4. Persistent worker scratch pool owned by `Jp2Decoder`  
5. Fold sign application into write/dequant; tight dequant store  
6. `SmallVec` / inline single-segment Tier-2 contributions  

**Exit:** under `Budgeted(4)`, Tier-1 wall time within ~20% of OpenJPEG’s internal Tier-1 share on the same stream (attribute with coarse external timers, not stats mode).

## Phase 5 — True windowed / line-based ROI

**Goal:** single-tile ROI cost proportional to ROI + filter halo, not full tile.

1. Expand requested ROI by 5/3 and 9/7 support at each retained level  
2. Allocate only needed coefficient windows  
3. Row-/column-limited IDWT  
4. Parsed-plan cache so region selection does not re-parse  

**Exit:** quarter-image single-tile ROI time_fraction ≲ 0.35 vs full (today ~0.9 from crop-after-full).

## Phase 6 — ISA / PGO / optional GPU

Only after Phases 3–4:

1. AVX2/FMA banded 9/7 and fused pack  
2. PGO on PDF JPX corpus  
3. Optional nvJPEG2000 or hybrid CPU Tier-1 + GPU IDWT  

## PDF renderer follow-ups (outside pure codec)

1. Keep using session `Jp2Decoder` + packed Rgb8 (already done)  
2. When GPU compositing wants 4-byte pixels: map `DecodeOutputFormat::Bgra8`/`Rgbx8` through a new `DecodedFormat` and skip RGB→BGRA swizzle  
3. Optional decode telemetry line (`JP2LAM_DECODE_TRACE`) for one-shot integration diagnosis  
4. Re-run pdfium-diff on a JPX-heavy page after Phase 3  

## Do not start with

- MQ symbol micro-opts  
- Nested Rayon inside page-parallel renders without a global permit  
- ROI as “decode full, crop”  
- Changing 9/7 rounding for speed without OpenJPEG differential tests  
