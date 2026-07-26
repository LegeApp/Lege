# Fuzzing (cargo-fuzz / libFuzzer)

This directory is its **own cargo workspace**, deliberately excluded from the
main one (the root manifest's `members = ["crates/*"]` glob cannot reach
`fuzz/`). `libfuzzer-sys` is a sanctioned dev-only external dependency,
confined here; the stable, always-on mutation gate lives in
`crates/pdf-chaos-tests` and uses no external crates.

## Requirements

libFuzzer needs **nightly Rust** and a libFuzzer-capable target — in practice
**Linux** (or macOS). On Windows the targets still type-check
(`cargo +nightly check` from this directory), but linking/running the fuzzer
is not supported; use WSL or a Linux box.

```sh
cargo install cargo-fuzz
cd fuzz              # or run from the repo root with `cargo fuzz ... --fuzz-dir fuzz`
cargo +nightly fuzz list
cargo +nightly fuzz run <target>                 # e.g. cargo +nightly fuzz run open_render
cargo +nightly fuzz run <target> -- -max_total_time=600
cargo +nightly fuzz cmin <target>                # minimize the corpus
cargo +nightly fuzz tmin <target> <crash-file>   # minimize a crash input
```

## Targets

| Target | Surface |
| --- | --- |
| `syntax_lexer` | `pdf_syntax::Lexer` token stream over raw bytes, driven to `Eof`/error |
| `structure_decode` | `pdf_structure::decode_stream` — first byte selects Flate/LZW/AHx/A85/RL/none, rest is the stream body |
| `structure_load` | `pdf_structure::load_structure` (header scan, xref chains, recovery) over an in-memory source |
| `content_compile` | `DocumentSnapshot::open` + `PageCompiler::compile` of page 0, small `DocumentLimits` |
| `text_extract` | open + semantic compile + `TextPage` UTF-16, word, and rectangle queries |
| `image_jpeg` | in-house JPEG (`/DCTDecode`) decoder, small `DecodeLimits` |
| `image_jbig2` | JBIG2 decoder; first byte optionally splits input into globals + page stream |
| `image_ccitt` | CCITT G3/G4 fax decoder; selector byte varies `/K`, `/BlackIs1`, `/EncodedByteAlign` |
| `open_render` | full pipeline: open → compile page 0 → `CpuBackend` raster at 64×64 |

A `/JPXDecode` target is intentionally absent for now: the JPX codec is the
external `jp2lam` crate, which carries its own fuzzing; add a thin target here
if integration-boundary coverage is wanted.

## Invariant

Every target asserts the project-wide **never-panic** contract: arbitrary
bytes must produce a value or a typed error under small DoS budgets — no
panic, abort, OOM, or hang. Any finding is a bug in the corresponding crate,
not in the harness. Reproduce with
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>`, then add
the minimized input to `corpus/<target>/` as a regression seed.

## Corpus

`corpus/<target>/` is committed and seeded from the smallest fixtures in the
repo (`hello_world.pdf` from the PDFium reference resources, the JPEG codec
test fixtures, hand-built filter bodies). libFuzzer writes new interesting
inputs there while running; `artifacts/` (crashes) and `coverage/` are
generated output and are gitignored.
