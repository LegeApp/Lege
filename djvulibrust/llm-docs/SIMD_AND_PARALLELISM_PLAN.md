# djvulibrust SIMD / parallelism plan

Modeled on the primitive-dispatch architecture actually implemented in
`jp2lam` (`../Rust-projects/jp2lam/src/simd/`), adapted to djvulibrust's real
code. This document was written after reading both codebases directly — not
from the pasted advice alone, which got several djvulibrust facts wrong (see
below). Treat this as the corrected, straightforward version of that advice.

## Principle

Same rule jp2lam already validated in production: numeric-map code (color
conversion, wavelet transform, coefficient packing) is the SIMD target. The
ZP arithmetic coder is serial per-bit state and is an ASM/scalar-fast-path
target, never a SIMD target. Rayon goes around independent streams (pages,
components, blocks), never inside one adaptive coder stream. jp2lam's own
measurements back this up: entropy coding + context modeling was ~50% of
CPU and un-vectorizable, while DWT dropped from 11%→4% of CPU from an
unchecked-fast-path change, not from vectorizing it. Vertical-lift
parallelism was tried and measured as a 2–12% *regression* and was disabled
by threshold rather than removed — a caution worth repeating here rather
than assuming every parallel opportunity is a win.

**Sequencing note (per user, 2026-07-01):** the ZP ASM coder is not just
unpolished — it is currently **bit-incorrect** and needs substantial rework,
not the light cleanup Phase order originally implied. Since `asm_zp` is
off by default and IW44/BZZ SIMD work does not depend on it, ZP/ASM (and
JB2, which would ride on the same coder cleanup) are pushed to the end of
the roadmap. IW44 SIMD is the immediate priority and starts right after the
benchmark scaffold.

## Corrections to the pasted advice

The advice document was written without reading either repo. Verified against
current source:

| Claim | Reality |
|---|---|
| `wide` crate already used for portable SIMD | **False.** `wide` is not a dependency anywhere in [Cargo.toml](Cargo.toml). No SIMD of any kind exists in the codebase today. |
| `portable_simd` feature is an active nightly SIMD path | **False.** It only gates `#![feature(portable_simd)]` in [src/lib.rs:2](src/lib.rs). Nothing in the tree uses `std::simd`. [transform.rs](src/encode/iw44/transform.rs) has a comment stating SIMD deps were already removed for stable-Rust compatibility. Dead flag. |
| ASM ZP path used by IW44, JB2, and BZZ | **Partly false.** Only IW44 is feature-gated onto `asm_zp` ([encoder.rs:369-374](src/encode/iw44/encoder.rs)). JB2 has no `asm_zp` gate anywhere. BZZ is **hard-coded** to the Rust ZEncoder with an explicit comment: "Always use the Rust ZEncoder for BZZ to avoid FFI writer constraints" ([bs_byte_stream.rs:7-8](src/iff/bs_byte_stream.rs)). The ASM ZEncoder's current interface cannot serve BZZ without a redesign. |
| No `build.rs`, checked-in `.o` file | **True**, confirmed. `zpcodec_fixed.o` is checked in, no `build.rs` anywhere. `nasm` and `cc` are both on PATH on this machine, so a real build script is buildable. |
| ASM debug hooks in hot path | **True**, confirmed. `zpcodec_fixed.asm` calls `zp_debug_hook` from the zemit path, and [asm.rs:190-209](src/encode/zc/asm.rs) `ZEncoder::finish()` does `eprintln!` on every stream over 20 bytes. This runs today even in release builds with `asm_zp` on. |
| ASM ZP coder just needs cleanup | **False, per user.** It is currently **bit-incorrect**, not merely noisy — it needs substantial rework to reach bit correctness, beyond stripping debug hooks. Treated in this plan as a ground-up redo, scheduled last. |
| `dev_asm_cmp` feature exists as a correctness harness | **Half true.** The feature is declared in [Cargo.toml:13](Cargo.toml) with a comment promising "assembly vs Rust ZP comparison tests," but `grep` finds zero `.rs` files that reference it — the gate is unused. It's a stub worth actually building, not a harness that already exists. |
| BZZ's BWT is naive rotation-sort | **True**, confirmed. [bs_byte_stream.rs:59-104](src/iff/bs_byte_stream.rs) sorts rotation indices with an O(n) comparator per pair — O(n² log n) overall. Blocks are clamped 10KB–4MB, so this is genuinely the worst algorithmic offender in the codebase. |
| No benchmark harness exists | **True.** No criterion, no `benches/`, no timing example. The `[package]` comment in Cargo.toml claims a "50-page IW44 s100 benchmark" was run, but no such code is checked in — it's a historical claim, not reproducible infrastructure. |
| IW44 already has some rayon parallelism | **True.** [encoder.rs:163-190](src/encode/iw44/encoder.rs) uses `rayon::join` to build Y/Cb/Cr codecs concurrently. This is internal only — no public API for page-level parallelism exists ([page_encoder.rs](src/doc/page_encoder.rs) is sequential). |
| JB2 CC analysis is naive per-pixel scanning | **Overstated.** [cc_image.rs](src/encode/jb2/cc_image.rs) is already run-based with union-find, and `Comparator::distance` ([symbol_dict.rs:191-262](src/encode/jb2/symbol_dict.rs)) already does word-packed XOR + `count_ones()` popcount. JB2 preprocessing is in better shape than the advice assumed — lower priority than it claimed. |

Toolchain check on this machine: `nasm`, `cc`, `cargo` all present. Full
djvulibre suite (`ddjvu`, `djvudump`, `djvused`, `csepdjvu`, etc.) and
`djview`/`djview4` are on PATH — usable for round-trip decode and visual
verification without installing anything.

## Target architecture

Mirror jp2lam's actual `src/simd/` layout, not the pasted advice's larger
`src/primitives/x86/neon/...` tree. jp2lam's real version is a 4-file module
with a `LazyLock` singleton and one env var — copy that shape.

```
src/simd/
  mod.rs      # Primitives struct, LazyLock<Primitives>, DJVU_PRIMITIVES dispatch
  scalar.rs   # reference kernels — thin wrappers over existing functions
  wide/
    mod.rs    # setup_all (force everything) / setup_auto (proven winners only)
    color.rs  # rgb_to_ycbcr wide kernel + equality tests (regressed, opt-in only)
    iw44.rs   # filter_fv wide kernel + equality tests (proven win, in auto)
  x86/
    mod.rs    # runtime is_x86_feature_detected! gate, empty AVX2 stub until profiled
```

```rust
// src/simd/mod.rs
pub(crate) struct Primitives {
    pub color: ColorPrimitives,   // rgb_to_ycbcr, rgb_to_gray
    pub iw44: Iw44Primitives,     // filter_fh/fv predict-update, block copy+zigzag
    pub backend: &'static str,    // "scalar" | "wide" | "avx2"
}

pub(crate) static PRIMITIVES: LazyLock<Primitives> = LazyLock::new(select_primitives);
```

Env var `DJVU_PRIMITIVES=scalar|auto|wide|avx2`, same semantics as jp2lam's
`JP2LAM_PRIMITIVES`: start from `scalar::primitives()`, overwrite with
`wide::setup()` when the `simd` feature is on and mode allows it, overwrite
again with `x86::setup()` for AVX2 only after `wide` is measured and an AVX2
win is actually demonstrated.

The ZP ASM/Rust choice is a **separate axis** and stays a compile-time
`asm_zp` feature (it's tied to a linked object file, not a runtime-dispatchable
kernel) — don't fold it into `DJVU_PRIMITIVES`.

### Target feature flags

```toml
[features]
default = []
simd = ["dep:wide"]     # new — replaces the dead `portable_simd` stub
asm_zp = []              # existing — now built via build.rs, debug-hook-free
dev_asm_cmp = []         # existing, currently unused — Phase 5 wires it up for real
rayon = ["dep:rayon"]
iw44-trace = []
debug-logging = []
# portable_simd removed — it did nothing (see corrections table)

[dependencies]
wide = "=0.7.33"   # pin, matching jp2lam's pinned version
```

## Implementation status (2026-07-01)

Phases 0, 1, 2 (color conversion, vertical transform, coefficient packing;
horizontal transform deliberately deferred — see below), 3 (BZZ), and 4
(page-level parallel API — found already implemented, verified + benchmarked)
are done and merged into this working tree. Ground-truthed, not
aspirational — every claim below was verified by running the actual
test/benchmark commands.

| Item | Status |
|---|---|
| `Cargo.toml`: `simd = ["dep:wide"]` feature, `wide = "=0.7.33"` dep, dead `portable_simd` removed | Done |
| `src/simd/{mod,scalar,x86/mod}.rs` + `src/simd/wide/{mod,color,iw44}.rs` scaffold, `Primitives` struct (`color` + `iw44` groups), `LazyLock` singleton, `DJVU_PRIMITIVES` env var | Done |
| `rgb_to_ycbcr_planes` ([encoder.rs](../src/encode/iw44/encoder.rs)) and `Encode::forward` ([transform.rs](../src/encode/iw44/transform.rs)) routed through `PRIMITIVES.color`/`PRIMITIVES.iw44`, scalar path byte-identical to pre-refactor | Done |
| **`auto` is now per-primitive, not a blanket switch**: `wide::setup_auto` installs only kernels proven faster than scalar; `wide::setup_all` (explicit `DJVU_PRIMITIVES=wide`) forces every kernel including known regressions, for testing | Done |
| `wide` kernel for `rgb_to_ycbcr` ([wide/color.rs](../src/simd/wide/color.rs)), bit-exact vs. scalar, **measured ~2x slower** (per-pixel LUT gather has no `wide` equivalent) | Correctness confirmed; regression — opt-in only, not in `auto` |
| `wide` kernel for `filter_fv` at `scale == 1` ([wide/iw44.rs](../src/simd/wide/iw44.rs)), bit-exact vs. scalar (exhaustive matrix: widths 1-129 straddling both the 8-lane and 32-width dispatch thresholds, 13 heights, 3 rowsize paddings, 9 data patterns including near-i16-extremes) | **Correctness confirmed, measured ~31% faster transform stage, ~8% faster full page encode — installed in `auto`** |
| `filter_fh_split_scalar` (Phase 2C, test-only): proved the two-pass split is correct, but required a scratch array to preserve full i32 predict precision the original's rolling registers carry (a real correctness trap, not just interleaving difficulty — see below). Measured ~85% *slower* than original due to per-row `Vec` allocation | Correctness confirmed; **not wired into any dispatch table** — Phase 2D (horizontal SIMD) stays deferred |
| `examples/benchmark.rs`: wall+CPU (via `/proc/self/task/*/schedstat`) for color conversion, IW44 transform, and full page encode | Done |
| `tests/roundtrip_ddjvu_test.rs`: encode → decode via real `ddjvu` (djvulibre) → PSNR check (38.84 dB on the synthetic gradient fixture, identical under scalar/wide/auto backends, before and after all work below) | Done, passing |
| Coefficient packing fuse: `Block::read_from_transform_block` ([coeff_map.rs](../src/encode/iw44/coeff_map.rs)) fuses `copy_block_data` + zigzag `read_liftblock`, skipping the intermediate `[i16; 1024]` liftblock. Bit-exact vs. the two-step version (`fused_matches_two_step`, varied plane sizes/block positions/data patterns) | **Correctness confirmed, measured ~14% faster** (3.36ms -> 2.89ms/iter on the isolated packing loop) — now the only path, no dispatch flag needed (pure algorithmic refactor, not a SIMD variant) |
| BZZ suffix-array BWT: `circular_suffix_array` ([bs_byte_stream.rs](../src/iff/bs_byte_stream.rs)) replaces the naive O(n^2 log n) rotation sort with O(n log n) prefix-doubling using stable counting-sort passes (not comparison sort — see below). Bit-exact vs. the naive reference across boundary sizes, repeated-byte-run worst cases, and the full 0-255 byte-value space (`bwt_tests` module) | **Correctness confirmed** — byte-for-byte identical to real `bzz`/`ddjvu` (djvulibre) output, same as before the change (`bzz_compare_test`, `bzz_dirm_test`) |
| Full test matrix: `cargo test`, `cargo test --release`, `cargo test --features simd`, `DJVU_PRIMITIVES={scalar,wide} cargo test --features simd` | All green (48 lib tests + all integration tests + doctests, debug and release) |
| Page-parallel API, ZP rework, JB2 | Not started, per the deferred ordering below |

**Why the `wide` color-conversion kernel regressed** (unchanged from the
earlier finding): `rgb_to_ycbcr`'s cost is dominated by three 256-entry LUT
lookups per pixel (`wide` has no gather instruction), so batching only the
add/shift/clamp/cast afterward added staging-array overhead without removing
the actual bottleneck. A gather-free direct fixed-point multiply was checked
and rejected (disagrees with the LUT on ~250/256 values per coefficient —
see [wide/color.rs](../src/simd/wide/color.rs)).

**Why `filter_fv` succeeded where the color kernel didn't:** the first
version of the `filter_fv` kernel made the *same* mistake as color — a
per-lane scalar gather into staging arrays — and measured ~0% improvement.
The fix was using `wide`'s actual bulk load/widen (`i16x8::new` +
`i32x8::from_i16x8`) and narrow/store (`i16x8::from_i32x8_truncate`)
instead of manual per-element loops; vertical rows are contiguous at
`scale == 1` (no interleaving), so a real bulk vector load applies cleanly.
`from_i32x8_truncate` specifically was chosen over a saturating pack — the
advice's warning about `packssdw`-style saturation silently changing output
on overflow was a real, checked concern (see
[wide/iw44.rs](../src/simd/wide/iw44.rs) doc comments), not a hypothetical.

**Why horizontal (`filter_fh`) is harder than the interleaving problem alone
suggests:** building `filter_fh_split_scalar` to prove the split was correct
surfaced a second, non-obvious trap — the original's `a0..a3`/`b0..b3`
registers carry full **i32** precision across loop iterations, and only the
*final* buffer write truncates to i16. A split that hands off between passes
by re-reading the (already-truncated) i16 buffer silently loses precision
whenever a predict value overflows i16 range, which real `extremes`-pattern
test data hits. The fix (a scratch array of untruncated i32 values) works
but allocates per row, and measured ~85% slower than the original — so
Phase 2D's eventual SIMD design needs to solve deinterleaving *and*
precision-preserving scratch reuse before it can even be benchmarked fairly.
Two structural bugs were caught by the exhaustive equality tests during
development, not by reasoning alone — a concrete argument for writing the
test matrix before trusting a "should be correct" mechanical refactor.

**BZZ suffix-array rewrite — the highest-confirmed-payoff item, and worse
than advertised.** The naive rotation sort wasn't just slow, it was
computationally infeasible at real block sizes: extrapolating its own
measured scaling from small inputs, a single 4MB block (the top of BZZ's
10KB-4MB range) would take on the order of a minute. The initial suffix-array
rewrite (comparison-sort-based prefix doubling, `sort_unstable_by` per
round) fixed the asymptotic complexity but was still *slower than naive* at
small-to-medium sizes (1000-50000 bytes) due to per-round sort constant
factor, and took 2.25s at 4MB — technically usable, not actually fast.
Replacing the per-round comparison sort with two stable counting-sort
passes (LSD radix sort by the `(rank[i], rank[i+k])` pair, each O(n +
bucket_count) instead of O(n log n)) fixed both problems: 2.4-3.2x faster
than naive even at 1000-50000 bytes, and 4MB dropped to 921ms. Verified
against the real `bzz` CLI (djvulibre) byte-for-byte before and after, not
just self-consistency — `bzz_compare_test`/`bzz_dirm_test` diff the actual
compressed output against the reference tool. The lesson repeats: getting
the asymptotic complexity right (Phase 2A/2B's approach) is necessary but
not sufficient — the constant factor needed its own separate measurement
and fix, exactly as Phase 2's "measure, don't assume" rule already argued
for SIMD kernels specifically.

**The standing lesson for every remaining Phase 2 target** (coefficient
packing, and any future horizontal SIMD attempt): measure with
`examples/benchmark.rs` before assuming a win, and write the exhaustive
equality test before trusting a refactor's correctness — both color and
`filter_fh` looked reasonable and compiled cleanly while being wrong or slow
in ways that only showed up under measurement/exhaustive testing.

## Phased roadmap

### Phase 0 — benchmark + isolated test harness

No benchmark code exists today; build it before touching any hot path, in
this isolated-dev copy so nothing risks the production path.

- `examples/benchmark.rs`: wall time (`Instant`) + summed process CPU time
  (read `/proc/self/task/*/schedstat`, same technique jp2lam uses), reporting
  per-stage timings: `color_convert_ms`, `iw44_transform_ms`, `iw44_zp_ms`,
  `jb2_cc_ms`, `jb2_zp_ms`, `bzz_bwt_ms`, `bzz_zp_ms`, `wall_ms`, `cpu_ms`,
  `output_bytes`. Env vars `DJVU_BENCH_ITERS`, `DJVU_BENCH_PAGES`.
- Test fixture set: 1-bit text page, grayscale scan, color scan, compound
  page, small multi-page doc — synthetic where needed (mirrors jp2lam's
  synthetic fallback in `examples/benchmark.rs`).
- Round-trip tester script: encode with djvulibrust → decode with `ddjvu`
  (from djvulibre, confirmed on PATH) → compare pixels/PSNR → optionally open
  in `djview4` for visual spot checks. This becomes the standing regression
  gate for every later phase.
- No behavior change to library code in this phase.

**Exit gate:** benchmark runs and produces a baseline numbers table; round-trip
tester passes on current `main`-equivalent code.

### Phase 1 — primitives scaffold

- Add `src/simd/{mod,scalar,x86/mod}.rs` per the architecture above, with
  `wide.rs` added but initially exporting zero kernels (structure only).
- Route today's scalar `rgb_to_ycbcr_planes`, `filter_fh`, `filter_fv` calls
  through `PRIMITIVES.color.*` / `PRIMITIVES.iw44.*` so the dispatch table is
  live, but behavior is identical to today (`scalar` is still the only real
  backend).

**Exit gate:** no behavior change; `DJVU_PRIMITIVES=scalar` (the only
implemented mode) round-trips identically to pre-Phase-1 output.

### Phase 2 — IW44 SIMD (immediate priority)

Targets, in order:

1. `rgb_to_ycbcr_planes` / `rgb_to_ycbcr_buffers` ([encoder.rs:94-129](src/encode/iw44/encoder.rs)) — currently a scalar `chunks_exact(3)` loop with table lookups. Vectorize with `wide` (8-lane `i32x8`/`f32x8` fixed-point multiply-add). If rounding differs from the scalar table-lookup path, change both to the same fixed-point formula rather than accepting divergent output — same rule jp2lam applied to `finalize_f32`.
2. `filter_fh` / `filter_fv` ([transform.rs:188,290](src/encode/iw44/transform.rs)) — split the streaming lifting loop into a vectorizable interior (predict/update over `i16` lanes) with scalar boundary prologue/epilogue, same shape as jp2lam's DWT row-lift split. Verify vertical-pass parallelism before enabling it by default — jp2lam measured column/vertical parallelism as a *regression* on real images; don't assume the win transfers without measuring on djvulibrust's own data.
3. `copy_block_data` + zigzag pack ([coeff_map.rs:144](src/encode/iw44/coeff_map.rs)) — fuse into one primitive, avoid the temporary `[i16; 1024]` buffer where possible.

Every kernel gets a `scalar == wide` bit-exact test (mirrors jp2lam's
`src/simd/wide.rs` test suite: fixed set of widths/heights straddling the
lane-width boundary, exact `assert_eq!`, not tolerance-based, except where a
transform is inherently lossy).

**Exit gate:** `DJVU_PRIMITIVES=scalar` and `DJVU_PRIMITIVES=wide` produce
byte-identical IW44 output on the Phase 0 fixture set; benchmark shows
measured wall/CPU improvement (report it, don't assume it — jp2lam's own
q=100 lossless path showed ~0% SIMD gain despite q=50 showing 6–14%).

### Phase 3 — BZZ algorithmic rewrite (highest confirmed payoff)

This is the one item in the pasted advice that's unambiguously correct and
high-value: [bs_byte_stream.rs `bwt()`](src/iff/bs_byte_stream.rs) is O(n²
log n) naive rotation sort against blocks up to 4MB.

- Replace with a suffix-array-based BWT (SA-IS or a license-compatible
  crate). Keep the Rust ZP path for the final ZP-encode stage — no ASM
  interface change needed here.
- Verify: decoded output byte-identical via `ddjvu`/round-trip tester from
  Phase 0 before and after.
- This is independent of SIMD/Phase 2 and can be done in parallel with it if
  useful, but do it before spending effort on JB2 (Phase 6), since JB2's
  preprocessing is already in decent shape per the corrections table.

**Exit gate:** BZZ-heavy fixture (text/annotation doc) round-trips identically
through `ddjvu`; wall time for BWT stage drops measurably on the largest
block-size fixture.

### Phase 4 — page-level parallel API — DONE, already existed, now verified

**Ground truth (2026-07-01): the primitive already existed before this
phase started, undocumented-by-testing.** `DjvuDocument` in
[builder.rs](../src/doc/builder.rs) already has a "bring your own
concurrency" design: `PageCollection` ([page_collection.rs](../src/doc/page_collection.rs))
is a per-slot-`RwLock`'d thread-safe structure, and `encode_page()` is
explicitly documented as "touches no shared mutable state, so it is safe to
call from a worker thread or rayon iterator" — paired with the cheap,
thread-safe `add_encoded_page()` insert. No `#[cfg(feature = "rayon")]`
gate on this path at all: it works with plain `std::thread::scope` too,
zero extra dependencies. What was genuinely missing: nothing in the repo
actually *exercised* this concurrently before now, so the claim was
untested.

[tests/page_parallel_test.rs](../tests/page_parallel_test.rs) fixed that:
- `std_thread_scope_parallel_encode_produces_correct_document`: real OS
  threads via `std::thread::scope`, no extra dependency, decode-verified
  via `ddjvu`.
- `rayon_par_iter_parallel_encode_produces_correct_document`
  (`--features rayon`): the pattern the doc comments recommend.
- `parallel_and_sequential_encode_produce_identical_bytes`: concurrent,
  deliberately-out-of-order encoding produces **byte-identical** output to
  sequential — confirms `PageCollection`'s per-slot locking has no ordering
  leakage into the final document.

`examples/benchmark.rs` Stage 3 (`multipage_seq_ms`/`multipage_par_ms`,
`--features rayon`) measured actual scaling on a 20-core machine:

| Pages | Page size | Sequential wall/doc | Parallel wall/doc | Speedup |
|---:|---|---:|---:|---:|
| 2  | 800x600  | 17.5ms  | 11.9ms | 1.47x |
| 4  | 1600x1200 | 192.0ms | 93.3ms | 2.06x |
| 8  | 800x600  | 74.2ms  | 33.0ms | 2.25x |
| 16 | 800x600  | 145.1ms | 61.7ms | 2.35x |
| 20 | 800x600  | 189.4ms | 85.0ms | 2.23x |

A real, consistent 2-2.35x wall-time win — but **not** linear scaling with
core count (20 pages on 20 cores still only hits 2.23x), and CPU time goes
*up* under parallelism (e.g. 20-page case: ~279ms/doc sequential CPU vs
~724ms/doc parallel CPU — more total work for less latency, the expected
tradeoff, but worth knowing it's not free). Two plausible causes, not yet
isolated: (1) page-level `rayon::par_iter` stacks on top of IW44's existing
per-page `rayon::join` for Y/Cb/Cr ([encoder.rs](../src/encode/iw44/encoder.rs)) —
both share the same global rayon thread pool, so nesting doesn't add real
parallelism headroom, just finer-grained tasks competing for it; (2)
`finalize()`'s document assembly is a serial step whose fixed cost caps
achievable speedup (Amdahl's law) more at small page counts. Not
investigated further this round — the measured win is real and worth
having regardless, and root-causing the plateau is separable follow-up
work if page-level throughput becomes a priority.

**Deliberately not done:** a convenience `encode_pages_parallel` wrapper.
The existing design lets callers pick `std::thread`, `rayon`, or anything
else, and per-page latency (not just page count) varies with real content,
which a single crate-provided wrapper can't tune for as well as the
caller's own code can. `tests/page_parallel_test.rs` and
`examples/benchmark.rs` Stage 3 together are the reference implementation
of the pattern; adding a thin wrapper on top is a small, low-risk follow-up
if a user actually asks for the ergonomics, not a gap in capability.

**Exit gate:** met — multi-page fixture wall time improves with `rayon` on
(2-2.35x across tested page counts/sizes); single-page latency is
unaffected since page-level parallelism is opt-in per caller, not global.

### Phase 5 — ZP coder rework (moved last: currently bit-incorrect)

ZP underlies IW44, JB2, and BZZ, but per user direction this is deferred to
the end since IW44/BZZ SIMD work does not require it and `asm_zp` is off by
default. Confirmed broken, not just noisy — treat as a ground-up redo of the
ASM backend, not a cleanup pass:

- Establish a correctness harness *first*: implement what `dev_asm_cmp`
  already promises but never got — a test that feeds identical input through
  `zc::zcodec::ZEncoder` (Rust, presumed correct/reference) and
  `zc::asm::ZEncoder` and asserts byte-identical output streams across a
  range of inputs. This is what will actually reveal where the ASM diverges.
- Remove `zp_debug_hook` call from `zpcodec_fixed.asm`'s zemit path and the
  `eprintln!` in [asm.rs `ZEncoder::finish()`](src/encode/zc/asm.rs) as part
  of the rework, not as a substitute for it.
- Diagnose and fix the bit-correctness bug(s) against the Rust reference and
  the original C++ (`src/asm/ZPCodec.cpp`) using the new comparison harness.
  Do not re-enable `asm_zp` as a usable path until `dev_asm_cmp` passes.
- Add a real `build.rs` (the `cc` crate driving `nasm`, gated to
  `target_arch = "x86_64"` and `feature = "asm_zp"`) so `zpcodec_fixed.o`
  stops being a checked-in artifact.
- **Do not** attempt to unify BZZ onto the ASM path in this phase — the
  existing "avoid FFI writer constraints" comment reflects a real interface
  mismatch (BZZ needs a generic `Write` sink; the current ASM wrapper
  doesn't support it cleanly). Revisit only if a `Vec<u8>`-sink redesign of
  `ZEncoder` makes it trivial; otherwise BZZ stays on the Rust ZP path
  permanently.

**Exit gate:** `cargo test --features asm_zp,dev_asm_cmp` passes with byte-
identical Rust-vs-ASM output across the fixture set, with no debug output on
stderr. Until this gate passes, `asm_zp` remains experimental/off.

### Phase 6 — JB2 (lower priority, after ZP)

Given CC analysis and symbol distance are already run-based / word-packed:

- Profile first with the Phase 0 harness before writing any code here — the
  original advice's "replace per-pixel BitVec scans" item may already be
  solved by the existing `cc_image.rs` run-based design.
- If profiling shows `encode_bitmap_directly` / `get_direct_context`
  ([symbol_dict.rs:376-425](src/encode/jb2/symbol_dict.rs)) as hot, optimize
  the interior context computation into a rolling integer with scalar
  edge handling — not SIMD, since this is per-pixel arithmetic-coder context,
  same non-SIMD-able category as ZP itself.
- Only after Phase 5 lands a correct ASM ZP backend does it make sense to
  consider wiring JB2 onto it; do not do this speculatively.

**Exit gate:** only proceed past profiling if the Phase 0 harness shows JB2
stages as a meaningful fraction of wall time on the 1-bit text fixture.

## Correctness gates (apply from Phase 1 onward)

```
cargo test                                   # DJVU_PRIMITIVES unset / scalar default
DJVU_PRIMITIVES=wide cargo test
cargo test --features asm_zp,dev_asm_cmp
```

For every new kernel: `scalar == wide` bit-exact (or documented lossy
tolerance), `rust_zp == asm_zp` bit-exact. For every phase: round-trip
through `ddjvu` (decode) with pixel/PSNR comparison, spot-check in `djview4`,
and confirm chunk structure via `djvudump`.

## Non-goals

- No SIMD inside the ZP arithmetic coder itself.
- No parallelism inside one adaptive ZP stream (IW44 plane, JB2 bitmap, or
  BZZ block's final encode stage).
- No default-on AVX2 until `wide` vs scalar is measured and shows a real win
  — mirror jp2lam's empty `x86/avx2.rs` stub, don't hand-write kernels
  speculatively.
- No forcing BZZ onto the ASM ZP path without first resolving the documented
  FFI/`Write` interface mismatch — don't silently change BZZ's coder backend.
- No checked-in `.o` as the normal build route once `build.rs` lands.
- Don't accept "faster but different output" without the round-trip +
  `ddjvu`/`djview4` verification from Phase 0.
