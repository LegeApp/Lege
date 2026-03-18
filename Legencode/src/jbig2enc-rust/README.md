# jbig2enc-rust

`jbig2enc-rust` is a production-oriented JBIG2 encoder in Rust for scanned black-and-white documents, PDF embedding, and long multi-page text compression.

The practical question for a project like this is simple: is it actually better than the original `jbig2enc`, or just newer?

On a fair in-process `350`-page heads-up against the original encoder, using the same preloaded page set and release builds on both sides, the answer on text-heavy material is yes. The Rust encoder reaches generic-mode size parity with the original implementation, beats the original in plain symbol mode on both speed and file size, and its current `sym_unify` mode produces the best overall text compression result in the set.

Long-run snapshot on `sahib2/350p`:

- `c generic`: `5228.2 KB` in `1.15s`
- `rust generic`: `5232.2 KB` in `2.47s`
- `c symbol`: `2566.5 KB` in `24.58s`
- `rust symbol`: `2227.1 KB` in `6.05s`
- `rust sym_unify`: `2025.9 KB` in `16.93s`

That means:

- Rust `generic` is effectively size-parity with the original encoder.
- Rust `symbol` is `13.2%` smaller than original `jbig2enc` symbol mode and about `4.1x` faster on this corpus.
- Rust `sym_unify` is `21.1%` smaller than original `jbig2enc` symbol mode and still faster than the original encoder.

Within the Rust encoder itself on the same `350`-page run:

- `symbol` is `57.4%` smaller than Rust `generic`.
- `sym_unify` is `61.3%` smaller than Rust `generic`.
- `sym_unify` is `9.0%` smaller than Rust `symbol`.

No substitution bugs have been observed on the benchmark corpora used during development. The remaining weakness is not catastrophic character confusion, but that the symbol modes still leave more material in generic residual regions than an ideal long-book encoder would. That is a compression-efficiency problem, not a known correctness failure.

The benchmark harness was built specifically to avoid the old subprocess-skewed comparison. Both encoders run in-process, both start from the same preloaded PBM pages, and page preparation is kept outside the timed region. See [HEADSUP_BENCHMARK_RESULTS_350P.md](/home/dk/Desktop/testers-linux/jbig2enc-rust/HEADSUP_BENCHMARK_RESULTS_350P.md) for the benchmark notes and rerun command.

## What The Modes Do

This encoder currently exposes three practical operating points:

### `generic`

This is the fastest and most conservative mode. It encodes pages as generic JBIG2 regions without building a lossy symbol dictionary. It is the right choice when speed, simplicity, or low-risk integration matters more than maximum compression.

- Best for: general bilevel pages, debugging, and lowest-complexity integration
- Tradeoff: largest files of the three modes
- `350p` result: `5232.2 KB`

### `symbol`

This is the plain symbol-dictionary mode for text-heavy scans. The encoder extracts connected components, groups repeated glyphs into dictionary entries, and codes page instances by reference to those entries.

- Best for: fast, practical compression of ordinary scanned text
- Tradeoff: lossy symbol substitution, but far smaller output than `generic`
- `350p` result: `2227.1 KB`
- Savings: `57.4%` smaller than Rust `generic`

### `sym_unify`

This is the current advanced text mode. It starts from the symbol-dictionary pipeline, then does an additional family-unification pass to merge symbol variants more aggressively but still conservatively enough to avoid the substitution problems that make lossy JBIG2 dangerous when done badly.

In practical terms, `sym_unify` tries to recognize that many “different” symbols are really members of the same glyph family once you account for scanning noise, border variation, and page-to-page drift. It builds candidate classes, selects representatives, estimates whether a merge will actually save bytes, and then lets the planner remap page-local one-offs onto already-useful anchors when they are safe enough to attach.

It is not a direct copy of a single paper. It grew out of the project’s own symbol-dictionary work, the same broad classifier-and-representative tradition seen in `djvulibre` and Leptonica’s JBIG2 notes, and then was sharpened with ideas from the JBIG2 dictionary-design literature. The papers listed in the bibliography were used to pressure-test the design, tune the cost model, and refine the roadmap, even where their methods were not transplanted literally.

- Best for: the smallest files on long, fairly uniform text corpora
- Tradeoff: slower than plain `symbol`, but still faster than original `jbig2enc` symbol mode in the current long-run comparison
- `350p` result: `2025.9 KB`
- Savings: `61.3%` smaller than Rust `generic`, `9.0%` smaller than Rust `symbol`

## Why This Repo Is Worth Using

The original `jbig2enc` still deserves respect. It has age, field use, and many years of community familiarity behind it. This project is worth using anyway because it is no longer just a port. It is a faster encoder with a better current text-compression path.

The main technical reasons are:

- A full Rust encoder core with practical JBIG2 segment generation for standalone files and PDF-style split output.
- Connected-component extraction and text-symbol handling integrated directly into the encoder pipeline.
- A modern symbol-dictionary planner that can choose between plain symbol mode and the stronger `sym_unify` path.
- Exact dictionary-entry byte accounting for `sym_unify`, so class formation and export decisions are based on realistic dictionary cost rather than rough bitmap heuristics.
- Planner-side local/global anchor remapping, which helps recover page-local one-offs into the symbol system instead of leaving them in generic output.
- A fair in-process benchmark bridge to the original C encoder, so performance claims can be tested on the same basis.
- Spec-oriented halftone support, including PDF split output for pattern dictionaries and page-local halftone regions.

## Current Limits

This is not presented as “finished forever.”

The current symbol modes are already strong enough to beat the original encoder on the measured corpus, but there is still room to improve:

- Too much text-like material still falls through to generic residual encoding in `symbol` and especially `sym_unify`.
- The long-book behavior is good but not ideal yet; dictionary growth flattens in the right direction, but residual generic bytes are still higher than they should be.
- Refinement-assisted coding remains an open area for improvement rather than a solved one.

Those are compression-efficiency limits. They are exactly the kind of limits you want at this stage: the encoder is already useful, and the remaining work is about getting closer to the best possible file size without giving up safety.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
jbig2enc-rust = "0.5.0"
```

For text-symbol work, enable the symbol-dictionary feature:

```toml
[dependencies.jbig2enc-rust]
version = "0.5.0"
features = ["symboldict"]
```

## Usage

### Basic Encoding

```rust
use jbig2enc_rust::{encode_single_image, Jbig2Config};

let input = vec![0, 1, 0, 1, 1, 0, 1, 0];
let width = 2;
let height = 4;

let _config = Jbig2Config::default();
let result = encode_single_image(&input, width, height, false)?;
```

### PDF Mode Encoding

For PDF embedding, use split global/page output:

```rust
use jbig2enc_rust::{encode_single_image, Jbig2Context};

let input = vec![0, 1, 0, 1, 1, 0, 1, 0];
let width = 2;
let height = 4;

let _ctx = Jbig2Context::with_pdf_mode(true);
let result = encode_single_image(&input, width, height, true)?;
// result.global_data contains global dictionary data, if any
// result.page_data contains page-local data
```

### Mode Presets

```rust
use jbig2enc_rust::Jbig2Config;

let generic = Jbig2Config::lossless();
let symbol = Jbig2Config::text();
let sym_unify = Jbig2Config::text_symbol_unify();
```

## Features

Available Cargo features:

- `symboldict`: enables symbol-dictionary encoding support
- `cc-analysis`: enables connected-component analysis internals used by symbol workflows
- `tracing`: enables tracing-based debug logging
- `trace_encoder`: enables additional encoder tracing
- `line_verify`: enables line verification helpers
- `parallel`: enables Rayon-based parallel helpers where available
- `profiling`: enables profiling support
- `halftone_bin`: builds the standalone halftone test tool
- `c-encoder-bench`: builds the benchmark-only bridge used for fair heads-up tests against the original encoder

## Architecture

The main modules are:

- `jbig2enc`: encoder pipeline, planning, dictionary export, and page assembly
- `jbig2cc`: connected-component extraction for symbol workflows
- `jbig2comparator`: symbol comparison and matching
- `jbig2unify`: the `sym_unify` class-building and representative-selection pass
- `jbig2cost`: dictionary cost accounting used by the planner
- `jbig2halftone`: spec-oriented halftone encoding pipeline
- `jbig2arith`: arithmetic coding support
- `jbig2structs`: public configuration and core JBIG2 data structures

## Testing

Run the library tests with:

```bash
cargo test --lib
```

For the fair long-run heads-up benchmark:

```bash
HEADSUP_SOURCE=sahib2 HEADSUP_PAGES=350 HEADSUP_WRITE=0 \
  cargo test --release --features "symboldict,c-encoder-bench" \
  --test headsup_c_vs_rust -- --nocapture
```

Some validation tools also expect `jbig2dec` to be available in `PATH`.

## License Notice

This crate is published under `MIT OR Apache-2.0`.

When built with symbol-dictionary support, the symbol matching path includes code adapted from `djvulibre`. That means binaries built with the relevant symbol-dictionary functionality may carry GPL implications. Review your build configuration and distribution requirements carefully before shipping.

## Acknowledgments

- Original `jbig2enc`
- `djvulibre`
- Leptonica’s JBIG2 classifier notes
- ISO/IEC 14492 (JBIG2)

## Bibliography

### Directly used implementation references

- ISO/IEC 14492:2001. JBIG2 image coding standard.
- Original `jbig2enc`: https://github.com/agl/jbig2enc
- `djvulibre`: https://github.com/djvuzone/djvulibre
- Leptonica, “Jbig2 Classifier.” Informative background on conservative class formation and substitution-avoidance behavior. 
- M. Valliappan, B. L. Evans, D. A. D. Tompkins, and F. Kossentini, “Lossy Compression of Stochastic Halftones with JBIG2.” This work directly informed the halftone path implemented in this repository.

### Sources used to hone, evaluate, and pressure-test the symbol roadmap

These papers were not copied into the code as one-to-one implementations, but they were useful for understanding the design space, sharpening tradeoffs, and evaluating where the Rust encoder should go next.

- Yan Ye, Dirck Schilling, Pamela Cosman, and Hyung Hwa Koy, “Symbol Dictionary Design for the JBIG2 Standard.” 
- Yan Ye and Pamela Cosman, “Fast and Memory Efficient JBIG2 Encoder.” 
- Maribel Figuera, Jonghyon Yi, and Charles A. Bouman, “A New Approach to JBIG2 Binary Image Compression.” Source text in [/home/dk/Desktop/testers-linux/Jbig2-technical-docs/figuera.txt](/home/dk/Desktop/testers-linux/Jbig2-technical-docs/figuera.txt).
- Yandong Guo, Dejan Depalov, Peter Bauer, Brent Bradburn, Jan P. Allebach, and Charles A. Bouman, “Binary Image Compression Using Conditional Entropy-Based Dictionary Design and Indexing.”
