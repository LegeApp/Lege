# jp2lam vs OpenJPEG same-stream baseline

**Date:** 2026-08-05  
**Host:** Windows (local), 4 decoder threads unless noted  
**Fixture:** `lear.png` 4485×2791 RGB → jp2lam photo encode quality 75 → `lear_q75.jp2` (578 858 bytes, irreversible 9/7, 3 comps, sRGB)  
**OpenJPEG:** 2.5.3 at `D:\tools\openjpeg\openjpeg-master\build\bin\Release\`  
**Harnesses:**
- `cargo run --release --example compare_openjpeg -- lear.png`
- `cargo run --release --example decode_benchmark -- decode-fix-plan/bench-work/lear_q75.jp2`
- `decode-fix-plan/opj_bench.exe <jp2> <threads> <reduce> <runs>` (MSVC `/MD /DOPJ_STATIC`)

## Pixel correctness (same JP2)

| Metric | Value |
|---|---:|
| max abs sample | **1** |
| mean abs | **0.042** |
| pixels with abs > 2 | **0** |

Gate for irreversible 9/7: max abs ≤ 2. **PASS.**

## Wall times (median of 5 runs)

### Full resolution

| Decoder path | Median ms | Notes |
|---|---:|---|
| **OpenJPEG decode-only** (`opj_bench`) | **161.7 → 171.8** | re-measured after Phase 3; host variance ~10 ms |
| OpenJPEG total (decode+pack checksum) | 318–331 | includes deliberate generic pack |
| **jp2lam session packed Rgb8, Budgeted(4) — after Phase 3 fused pack** | **~190** | ICT + shift + pack fused; no 3× u8 staging planes |
| jp2lam session packed (pre–Phase 3) | 248.1 | staged ICT → 3× u8 → interleave |
| jp2lam `decode_jp2` legacy planar serial | 201.4 | compatibility path; planar i32, no pack |
| `opj_decompress` CLI → PNG | ~12 000 | **not comparable** (includes PNG encode I/O) |

Ratio (session packed / OPJ decode-only):

- Pre–Phase 3: **248 / 162 ≈ 1.53×**
- **Post–Phase 3: 190 / 172 ≈ 1.11×**

Phase 3 removed about **58 ms** on this fixture (~23% of the prior session path). Remaining gap vs OpenJPEG is primarily Tier-1 + DWT compute (audit Phase 4).

Audit control on a five-core host had OpenJPEG RGB full @ 4 threads ≈ **263 ms** (different stream encoding). This host’s OPJ decode-only on the jp2lam-encoded fixture is faster (~162–172 ms); use these numbers for same-stream ratios, not the absolute audit control table.

### Reduced resolution (4 threads)

| Reduce | Output dims | jp2lam session packed (ms) | OpenJPEG decode-only (ms) | Ratio |
|---:|---:|---:|---:|---:|
| 1 | 2243×1396 | **88.0** | **51.6** | 1.70× |
| 2 | 1122×698 | **28.7** | **17.1** | 1.68× |

## Stage split (jp2lam stats path — note: stats serializes Tier-1)

On the full fixture, `decode_jp2_with_stats` reported (not production wall time):

| Stage | ms |
|---|---:|
| Tier-1 (serialized under stats) | 139.6 |
| DWT total | 108.8 |
| DWT horizontal | 59.6 |
| DWT vertical | 49.2 |
| Finalize | 26.4 |

Largest DWT level (finest) ≈ 68 ms of the DWT total — still a primary structural cost after the banded 9/7 work; further gains need fused finalization and Tier-1 scheduling (audit Phases 3–4).

## What this session already landed (decoder)

From the 2026-08-05 audit Phase 1–2 structural work:

1. Lattice-aligned nonzero tile origins use optimized DWT  
2. Coarse 9/7 horizontal row scratch (not per-row alloc)  
3. Banded vertical 9/7 (no full-plane interleave temporary)  
4. `Rgbx8` / `Bgra8` packed formats on the codec direct path  

## PDF renderer integration status

`lege-pdf/render/crates/pdf-image/src/jpx.rs` already uses:

- Thread-local persistent `Jp2Decoder`  
- Packed `Gray8` / `Rgb8` / `Rgba8` / `Cmyk8` for 8-bit streams  
- Load-aware `DecodeConcurrency::Budgeted`  
- `DecodeResolution::AtLeast` from device footprint  

**Not yet wired into PDF:** `Rgbx8` / `Bgra8` as `DecodedFormat` (CPU paint path still samples DeviceRGB tightly; GPU upload would be the first real consumer). Codec APIs are ready when the renderer destination is four-byte RGBX/BGRA.

## Reproduce

```powershell
cd lege-codecs/jp2lam

# Rebuild OpenJPEG decode-only harness (once)
# From VS x64 Native Tools or via vcvars64:
#   cl /O2 /MD /DOPJ_STATIC /I...\openjp2 ... opj_bench.c /link openjp2.lib

$env:JP2LAM_COMPARE_THREADS=4
$env:JP2LAM_COMPARE_ITERS=5
cargo run --release --example compare_openjpeg -- lear.png

$env:JP2LAM_DECODE_BENCH_THREADS=4
cargo run --release --example decode_benchmark -- decode-fix-plan/bench-work/lear_q75.jp2
.\decode-fix-plan\opj_bench.exe decode-fix-plan\bench-work\lear_q75.jp2 4 0 5
```

## Targets for later audit phases

| Phase | Goal | Success signal on this fixture |
|---|---|---|
| 3 Fused packed output | ICT + shift + pack without 3× full u8 planes | finalize ≪ 26 ms; session packed ≤ ~200 ms |
| 4 Tier-1 jobs | weighted scheduling + persistent scratch | Tier-1 wall under Budgeted(4) closer to OPJ |
| 5 Windowed ROI | true subband window IDWT | quarter ROI time_fraction ≪ 0.5 |
| 6 ISA/PGO | AVX2 9/7 + PGO | close remaining ~50 ms gap vs OPJ decode-only |

Do not chase MQ micro-opts or GPU until Phases 3–4 shrink DWT/finalize/pack traffic further.
