# Production Integration Notes

## Release Readiness

Current status:

- `sym_unify` diagnostics are opt-in through `JBIG2_DIAGNOSTICS`
- page-0 matching / encoding logs are opt-in through `JBIG2_DEBUG`
- trace logging remains feature-gated through `trace_encoder` / `trace_arith`
- the remaining unconditional library-side `eprintln!` in clustering was removed

Recommended final local verification before merging:

```bash
cargo test --lib
cargo build --release
cargo test --features symboldict --test multi_page_benchmark --no-run
```

If you want one last spot check in release mode:

```bash
BENCH_SOURCE=sahib BENCH_PAGES=10,50 BENCH_MODES=sym_unify BENCH_WRITE=0 \
  cargo test --release --features symboldict --test multi_page_benchmark -- --nocapture
```

## Staging / Push

Recommended files to stage for this line of work:

- `src/jbig2cost.rs`
- `src/jbig2enc.rs`
- `src/jbig2unify.rs`
- `src/jbig2structs.rs`
- `src/jbig2classify.rs`
- `src/jbig2context.rs`
- `src/jbig2comparator.rs`
- `src/jbig2.rs`
- `src/lib.rs`
- `tests/multi_page_benchmark.rs`
- `SYM_UNIFY_HANDOFF.md`
- `RECENT_OPTIMIZATION_SUMMARY.md`
- `CEE_RERANK_RESCUE_SUMMARY.md`
- this file

Recommended staging command:

```bash
git add \
  src/jbig2cost.rs \
  src/jbig2enc.rs \
  src/jbig2unify.rs \
  src/jbig2structs.rs \
  src/jbig2classify.rs \
  src/jbig2context.rs \
  src/jbig2comparator.rs \
  src/jbig2.rs \
  src/lib.rs \
  tests/multi_page_benchmark.rs \
  SYM_UNIFY_HANDOFF.md \
  RECENT_OPTIMIZATION_SUMMARY.md \
  CEE_RERANK_RESCUE_SUMMARY.md \
  PRODUCTION_INTEGRATION_NOTES.md
```

Suggested commit message:

```text
Optimize and harden sym_unify; add diagnostic roadmap and exact cost accounting
```

Remote:

- `origin`: `https://github.com/LegeApp/jbig2enc-rust`

Push after commit:

```bash
git push origin <branch-name>
```

## Calling The Original C Encoder As A Library

The original C/C++ project at:

- `/home/dk/Desktop/testers-linux/jbig2enc`

already builds a library target:

- CMake: `add_library(libjbig2enc ...)`
- Automake/libtool: `libjbig2enc.la`

The public API is in:

- `/home/dk/Desktop/testers-linux/jbig2enc/src/jbig2enc.h`

Important caveat:

- the implementation is compiled as C++
- the header does **not** wrap the exported functions in `extern "C"`
- so the exported symbols are C++-mangled

That means Rust should **not** bind the current library directly with plain `extern "C"` declarations.

### Clean integration options

1. Preferred: add a tiny C-compatible shim library

- write a small wrapper `.cc` file inside the C project
- export `extern "C"` wrapper functions around:
  - `jbig2_init`
  - `jbig2_destroy`
  - `jbig2_add_page`
  - `jbig2_pages_complete`
  - `jbig2_produce_page`
- keep the current core library untouched
- bind the shim from Rust with `bindgen` or handwritten FFI

2. Alternative: use a Rust C++ bridge

- `cxx` crate
- or `autocxx`

This works, but is more moving pieces than a flat C shim for a benchmarking harness.

### Build commands for the C library

Example shared build with CMake:

```bash
cmake -S /home/dk/Desktop/testers-linux/jbig2enc \
      -B /home/dk/Desktop/testers-linux/jbig2enc/build \
      -DBUILD_SHARED_LIBS=ON
cmake --build /home/dk/Desktop/testers-linux/jbig2enc/build -j
```

This should produce a form of `libjbig2enc` linked against Leptonica.

### Rust-side FFI shape

The multipage API already matches the benchmark workflow well:

1. `jbig2_init(...)`
2. for each page: `jbig2_add_page(ctx, pix)`
3. `jbig2_pages_complete(ctx, &len, verbose)`
4. for each page in order: `jbig2_produce_page(ctx, page_no, xres, yres, &len)`
5. `jbig2_destroy(ctx)`

Return-value ownership:

- `jbig2_pages_complete` returns a malloc'ed buffer
- `jbig2_produce_page` returns a malloc'ed buffer
- Rust must free those with the same allocator family used by the shim/library
- simplest is to expose a shim `extern "C" void jbig2_free_buffer(void*)`

### The other dependency you need

`jbig2_add_page()` takes a Leptonica `PIX*`, not a raw bitmap.

So a realistic Rust harness needs one of:

1. `leptonica-sys` bindings and direct `PIX` construction from 1bpp page data
2. a shim helper that takes packed 1bpp bytes plus width/height/xres/yres and creates `PIX*` internally

For benchmarking against the Rust encoder, option 2 is usually the least painful:

- Rust passes page bitmap bytes
- C shim creates `PIX`
- shim calls `jbig2_add_page`
- shim destroys the temporary `PIX`

### Recommended shape for a fair in-process benchmark

Add a dedicated shim API like:

```c
extern "C" jbig2ctx* jbig2_rust_bridge_init(...);
extern "C" int jbig2_rust_bridge_add_page_1bpp(
    jbig2ctx* ctx,
    const uint8_t* packed_bits,
    int width,
    int height,
    int stride_bytes,
    int xres,
    int yres);
extern "C" uint8_t* jbig2_rust_bridge_pages_complete(...);
extern "C" uint8_t* jbig2_rust_bridge_produce_page(...);
extern "C" void jbig2_rust_bridge_free_buffer(void*);
extern "C" void jbig2_rust_bridge_destroy(jbig2ctx*);
```

That would let the Rust benchmark compare:

- Rust encoder in-process
- original encoder in-process

without subprocess overhead, file I/O noise, or CLI argument differences.

## Recommendation

For now:

1. merge the Rust encoder work as the production candidate
2. keep the current diagnostic tooling available but opt-in only
3. if you want the fairest possible benchmark against the original encoder, build a tiny `extern "C"` shim over `libjbig2enc` rather than spawning the CLI
