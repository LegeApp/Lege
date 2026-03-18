# 350-Page Heads-Up Benchmark

This is the long-run benchmark snapshot for `jbig2enc-rust` against the original `jbig2enc`.

## Fairness Basis

This run was designed to avoid the earlier subprocess-skewed comparison:

- both encoders were run **in-process**
- the original encoder was called through a benchmark-only bridge over `libjbig2enc`
- both sides started from the **same preloaded PBM page set**
- page preparation happened **outside** the timed region
- timings measure **encoding only**
- all results below are from **release builds**

Command:

```bash
HEADSUP_SOURCE=sahib2 HEADSUP_PAGES=350 HEADSUP_WRITE=0 \
  cargo test --release --features "symboldict,c-encoder-bench" \
  --test headsup_c_vs_rust -- --nocapture
```

Corpus:

- `sahib2/`
- `356` PBM pages available
- run used `350` pages

## Results

| Pages | Impl | Mode | Raw KB | Globals KB | vs peer | Enc s | ms/pg | MPix/s |
| ---: | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| 350 | c | generic | 5228.2 | 0.0 | +0.1% | 1.15 | 3.3 | 262.3 |
| 350 | rust | generic | 5232.2 | 0.0 | -0.1% | 2.47 | 7.1 | 121.8 |
| 350 | c | symbol | 2566.5 | 125.8 | -15.2% | 24.58 | 70.2 | 12.2 |
| 350 | rust | symbol | 2227.1 | 141.4 | +13.2% | 6.05 | 17.3 | 49.7 |
| 350 | rust | sym_unify | 2025.9 | 96.4 | +21.1% | 16.93 | 48.4 | 17.8 |

## What Holds Up On The Longer Run

- `generic` mode is essentially size-parity with the original encoder.
- Plain Rust `symbol` mode is now clearly ahead of the original encoder on this corpus:
  - `13.2%` smaller
  - about `4.1x` faster
- Rust `sym_unify` is the best compression result in the set:
  - `21.1%` smaller than original `jbig2enc` symbol mode
  - still faster than original `jbig2enc` symbol mode

## Practical Reading

If the question is “is this port actually worth using?”, this run gives a clean answer:

- for generic-region work, it is already competitive on output size
- for text-symbol work, the Rust encoder is not just viable; it is materially better on this corpus
- the current `sym_unify` path is the strongest size result
- the plain Rust `symbol` path is the strongest speed/size balance against the original encoder

## Notes

- `sym_unify` is a Rust-only mode and is included here because it is the current best text result.
- The original encoder was benchmarked through a direct library bridge, not via its CLI.
- The benchmark harness lives in `tests/headsup_c_vs_rust.rs`.
