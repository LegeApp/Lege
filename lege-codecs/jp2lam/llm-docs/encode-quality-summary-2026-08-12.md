# jp2lam perceptual PCRD qualification (2026-08-12, continued 2026-08-13)

Baseline commit: `70147b50c31faf7aad4e0eaf056745ccd7cd8e49`.
Corpus supplied by user: `/mnt/Samsung980_1TB/Rust-projects/jpegXL-rs/test-set`.
Metrics: decoded RGB Butteraugli (lower better), SSIMULACRA2 (higher better), and RGB PSNR.

## Implementation

- Keeps the established quality-to-byte operating point: first evaluates the old measured-MSE curves, then selects the perceptually weighted curves at the same Tier-1 byte budget.
- Wires the existing band bias, block class, and contrast visibility into measured Annex-J delta-MSE curves.
- Maps code-block coordinates through their subband origin and tile origin before sampling the luma contrast map.
- Uses smooth, calibrated perceptual-strength shoulders around q50, q75, and q90 rather than unrelated point modes.
- Gates q50..60 below 0.10 bpp and q61..84 below 0.80 bpp, where sparse allocations showed inconsistent metric response; q96..99 stays on the old MSE allocator.
- Leaves document trim and explicit target byte/bpp/ratio behavior unchanged.

## Size-matched results

All mean size changes below are versus the baseline and are effectively zero.

| Curve point / corpus | N | Mean size change | Mean PSNR change | Mean Butteraugli change | Mean SSIMULACRA2 change | BA wins | SSIM2 wins |
|---|---:|---:|---:|---:|---:|---:|---:|
| q25, seven 0.8 MP photos | 7 | +0.0603% | +0.0898 dB | -0.6149 | +0.1996 | 4/7 | 6/7 |
| q50, seven 0.8 MP photos | 7 | +0.0000% | +0.2770 dB | -0.3178 | +1.8200 | 2/7 | 6/7 |
| q50, 0.8/4.3/12 MP sample | 6 | +0.0016% | +0.0970 dB | -0.2433 | +1.3275 | 2/6 | 3/6 |
| q75, seven 0.8 MP photos | 7 | +0.0016% | +0.1521 dB | -0.3495 | +1.1071 | 3/7 | 3/7 |
| q75, 0.8/4.3/12 MP sample | 6 | +0.0010% | +0.0219 dB | -0.2065 | +0.2155 | 1/6 | 1/6 |
| q90, seven 0.8 MP photos | 7 | +0.0006% | -0.0652 dB | +0.0094 | -0.1156 | 2/7 | 4/7 |
| q90, 0.8/4.3/12 MP sample | 6 | +0.0004% | +0.0456 dB | -0.1488 | +0.3340 | 1/6 | 4/6 |
| q98, 0.8/4.3/12 MP sample | 6 | 0.0000% | 0.0000 dB | 0.0000 | 0.0000 | unchanged | unchanged |

The rate gates turn sparse q50/q75 cases back into the exact baseline allocation and retain the stronger same-size gains where enough truncation points exist. q90 remains deliberately weak: it improves the mixed-resolution sample but is neutral-to-slightly-negative on the seven-small-photo mean. q98 is byte-identical by policy. This is a first perceptual allocation step, not completion of the AKR work item (butteraugli-anchored quality mapping and resolution-stable quality remain open).

## Validation

- `cargo check --all-targets` passed.
- `cargo test --all-targets` passed: library 343 passed / 5 ignored, integration suites all passed.
- `cargo fmt -- --check` passed in `lege-codecs/jp2lam`.
- `git diff --check -- Cargo.toml src/encode/backend/native/backend.rs src/encode/backend/native/rate.rs examples/perceptual_curve.rs` passed.

## Cross-resolution confirmation (q90, one image per tier)

Verified against the detached baseline on the same corpus, including the 50 MP tier the AKR root-cause flags as the quality-collapse case:

| Tier | MP | Size change | dPSNR | dButteraugli | dSSIMULACRA2 |
|---|---:|---:|---:|---:|---:|
| 0.8 MP | 0.8 | +0.0014% | +0.104 dB | 0.000 | +0.121 |
| 4.3 MP | 4.3 | -0.0003% | +0.030 dB | +0.013 | +0.210 |
| 12 MP  | 12.0 | -0.0004% | -0.018 dB | 0.000 | -0.105 |
| 50 MP  | 49.9 | -0.0004% | +0.011 dB | 0.000 | +0.026 |

Output size is preserved at every tier, and the 50 MP tier shows no regression (slight PSNR/SSIMULACRA2 gains).
