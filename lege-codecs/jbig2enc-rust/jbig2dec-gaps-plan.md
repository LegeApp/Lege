# Supplementary Plan: Gaps Around `jbig2decplan.md`

`jbig2decplan.md` is the architectural authority for the decoder itself
(module design, MQ decoder, limits, phases 0–5, testing strategy). This
document fills in what that plan does **not** cover: the crate-level
restructure into encode/decode halves, repairing the pre-existing broken
baseline, the concrete interoperability harness, performance verification,
and the acceptance checklist used to verify each implementation phase.

Read `jbig2decplan.md` first. Where the two documents disagree on decoder
internals, `jbig2decplan.md` wins. Where they disagree on crate layout,
this document wins (it supersedes the layout sketch in its §4).

---

## Gap A: Crate restructure into `encode` / `decode` / `shared`

The crate becomes two equal halves plus a shared protocol core:

```text
src/
├── lib.rs              # thin: feature gates, re-exports, top-level docs
├── shared/             # protocol definitions used by BOTH halves
│   ├── mod.rs
│   ├── segment.rs      # SegmentType, header field enums (Phase 0 extracts these)
│   ├── mq_table.rs     # 94-entry MQ probability state table (Phase 0)
│   ├── int_proc.rs     # IntProc identifiers, integer ranges (Phase 0)
│   ├── bitmap.rs       # MonoBitmap packed u32 bitmap + CombinationOperator (Phase 0)
│   ├── reader.rs       # checked slice Reader (Phase 0, decode-leaning but shared-testable)
│   └── limits.rs       # DecodeLimits (Phase 0)
├── encode/             # the existing encoder, renamed modules
│   ├── mod.rs          # was src/jbig2enc/mod.rs
│   ├── arith.rs        # was jbig2arith.rs (encoder registers stay here;
│   │                   #   MQ table moves to shared/mq_table.rs)
│   ├── cc.rs           # was jbig2cc.rs
│   ├── classify.rs     # was jbig2classify.rs
│   ├── comparator.rs   # was jbig2comparator.rs
│   ├── context.rs      # was jbig2context.rs
│   ├── cost.rs         # was jbig2cost.rs
│   ├── halftone.rs     # was jbig2halftone.rs
│   ├── shared.rs       # was jbig2shared.rs (encoder-internal "shared"; rename
│   │                   #   later if contents move to src/shared/)
│   ├── simd.rs         # was jbig2simd.rs
│   ├── structs.rs      # was jbig2structs.rs (segment writers; SegmentType enum
│   │                   #   re-exported from shared/segment.rs after Phase 0)
│   ├── sym.rs          # was jbig2sym.rs
│   ├── unify.rs        # was jbig2unify.rs
│   ├── api.rs          # was jbig2.rs (top-level encode API)
│   └── symbol/         # was src/jbig2enc/symbol/ (unchanged internally)
├── decode/             # new, per jbig2decplan.md §4 (its decoder/ + arithmetic
│   ├── mod.rs          #   decoder side lives here)
│   ├── arith.rs        # ArithmeticDecoder + MqContext ops
│   ├── integer.rs      # IntegerContexts decode side
│   ├── iaid.rs
│   ├── segment.rs      # full T.88 segment-header parser
│   ├── file.rs         # file organization, embedded streams, ParsedDocument
│   ├── context.rs      # DecoderContext
│   ├── page.rs
│   ├── store.rs
│   ├── generic.rs
│   ├── refinement.rs
│   ├── symbol_dictionary.rs
│   ├── text_region.rs
│   ├── pattern_dictionary.rs
│   ├── halftone_region.rs
│   └── mmr.rs
└── bin/                # unchanged
```

Rules:

1. The restructure commit is **moves and path fixes only** — `git mv`,
   module renames in `lib.rs`, `use` path updates. No behavior change.
   `cargo build` must succeed and the encoder integration tests that
   passed before must still pass. This keeps `git log --follow` working.
2. Backward-compatible re-exports stay in `lib.rs` for one release
   (`pub use encode::api as jbig2;` style aliases) because Lege depends
   on this crate; remove them when Lege migrates.
3. Phase 0 (from `jbig2decplan.md` §23) happens **after** the move:
   extracting the MQ table, `SegmentType`, `IntProc` into `src/shared/`
   is a semantic refactor and must be its own commit(s) with the
   byte-identical-output check.

### Feature gating

```toml
[features]
default = ["encode", "decode", "symboldict"]
encode = []
decode = []
```

The PDF renderer will depend on `default-features = false, features =
["decode"]`. Every module in `src/encode/` is gated on `encode`, every
module in `src/decode/` on `decode`, `src/shared/` is unconditional.
Gating may be introduced with the Phase 1 commit (when `decode` first
has content) rather than during the pure-move commit.

### Naming

The crate keeps the name `jbig2enc-rust` for now to avoid breaking the
Lege dependency; renaming (e.g. to `jbig2-rs`) is a user decision to
revisit before any crates.io publish. Bump version to 0.6.0 when the
decoder lands.

---

## Gap B: Baseline repair (pre-existing breakage)

Discovered while extracting the crate; all predate the extraction:

1. `tests/encoder.rs`, `tests/encoder_pbm.rs`, `tests/mode_comparison.rs`
   fail to compile: they import a crate alias `jbig2` that only exists
   inside `tests/common/mod.rs`, and `mode_comparison.rs` references
   removed fields `global_segments` / `page_streams` on
   `Jbig2EncodeResult` (now `global_data` / `page_data`).
2. `src/jbig2cc.rs` lib test `test_tiny_cc_removal` fails
   (asserts 1 CC, gets 2).
3. `tests/headsup_c_vs_rust.rs` uses undeclared feature
   `c-encoder-bench` (add it to `[features]` or gate differently).
4. `tests/roundtrip_after_fix.rs::standalone_roundtrip_default_config`
   fails: jbig2dec 0.20 rejects the encoder's standalone-organization
   output with "page has no image, cannot be completed" (verified
   failing before the extraction too). Since the native decoder's
   Phase 1 exit criterion includes standalone streams, root-cause this
   during Phase 0/1 — it is either an encoder bug in standalone
   file-organization output or a test-harness invocation problem.

These must be fixed **first** (part of Phase 0's entry criteria): the
decoder's differential tests build on the encoder test harness, so the
harness has to compile and pass before decoder work starts. Fixing the
`test_tiny_cc_removal` failure means understanding whether the encoder
regressed or the test's expectation is stale — check
`git log -p -- '*jbig2cc*'` before "fixing" the assertion.

---

## Gap C: Interoperability matrix and harness

`jbig2decplan.md` §21 lists what to test; this section pins down how.

### Required directions

| # | Producer | Consumer | Status |
|---|----------|----------|--------|
| 1 | jbig2enc-rust (all modes) | system `jbig2dec` (0.20 installed) | partially exists in tests/ |
| 2 | jbig2enc-rust (all modes) | **native decoder** (self-roundtrip) | the core deliverable |
| 3 | C `jbig2enc` corpus | native decoder | needs corpus (see below) |
| 4 | native decoder output vs `jbig2dec` output on same stream | pixel-exact compare | differential oracle |

Direction 3: the C `jbig2enc` binary is **not installed**. Options, in
order of preference: (a) build it once from source
(github.com/agl/jbig2enc, needs leptonica) into `tests/tools/` and check
in only the generated `.jb2`/globals fixtures, not the binary; (b) check
in a small fixture corpus generated elsewhere. Either way the corpus is
committed so CI never needs the C encoder. Keep fixtures small
(< 200 KB total): 2–3 scanned-text pages through symbol mode,
1 generic, 1 refinement, 1 halftone.

### Harness pieces to build (Phase 1, alongside first decoder code)

1. `tests/common/pbm.rs` — PBM read/write + first-differing-pixel
   report `(x, y, expected, got)` (per §21.5).
2. `tests/common/oracle.rs` — run `jbig2dec` via `std::process::Command`
   on a temp file, parse its PBM output. Skip (not fail) when the
   binary is absent, but CI must have it.
3. `tests/decode_roundtrip.rs` — the encoder-mode matrix of §21.3:
   every mode × {standalone, embedded, embedded+globals, multipage},
   asserting `native_decode(encode(img)) == img` pixel-exact AND
   `native_decode(stream) == jbig2dec(stream)`.
4. Grow the matrix with each phase (generic-only in Phase 1, symbols in
   Phase 2, …) so every phase's exit criterion is executable as
   `cargo test --test decode_roundtrip`.

### Fuzzing (Phase 2+, per §21.6)

`cargo fuzz` targets under `fuzz/` seeded from the roundtrip corpus.
Not a phase gate until Phase 5, but the `embedded_document` target
should exist from Phase 2 and run for 10 CPU-minutes before each phase
is accepted.

---

## Gap D: Performance verification (PDF-renderer requirement)

The decoder plan gives architecture rules but no measurable targets.

### Benchmarks

Add `benches/decode.rs` (criterion, dev-dependency, added in Phase 1):

1. `decode_generic_a4_300dpi` — one full-page generic region
   (2480×3508). This is the dominant real-world case.
2. `decode_symbol_page` — a text-heavy page through symbol mode.
3. `decode_page_reuse` — same page 10× through one `DecoderContext`,
   asserting no allocation growth after the first iteration
   (measure with a counting allocator in a test, not criterion).
4. `decode_multipage_parallel` — 8 pages sharing globals across 4
   threads (`parallel` feature).

### Targets (gate for "ready for the PDF renderer", not per-phase)

* Generic A4 300 dpi page: **≤ 25 ms** on this machine in release
  (jbig2dec 0.20 CLI does the comparable file in roughly that range —
  measure it in Phase 1 to set the real baseline, then require native
  ≤ 1.25× jbig2dec, stretch ≤ 1.0×).
* Symbol-mode page: ≤ 1.25× jbig2dec on the same stream.
* Zero heap allocations in steady-state page decode with a reused
  `DecoderContext` (after the first page of a given size class).
* Output is packed `MonoBitmap` handed to the renderer without a
  repack (the `decode_embedded_into` API from `jbig2decplan.md` §5).

### Method

* Measure only after correctness (never optimize before a phase's
  roundtrip tests pass).
* `cargo bench` before/after each phase ≥ 2; a phase may not regress
  earlier benchmarks by > 5 %.
* The existing `profiling` feature (pprof flamegraphs) is available for
  investigation; hot-loop rules of `jbig2decplan.md` §22 apply.

---

## Gap E: Phase acceptance checklist (run after every phase)

The checking agent/reviewer runs, in order:

```bash
cargo build --all-targets                 # everything compiles
cargo test                                # unit + integration
cargo test --no-default-features --features decode   # decode half stands alone (Phase 1+)
cargo test --no-default-features --features encode   # encode half stands alone
cargo clippy --all-targets -- -D warnings            # decoder code is warning-clean
cargo bench                               # Phase 2+: compare against saved baseline
```

Plus per-phase manual checks:

* **Phase 0**: encoder output byte-identical on the full test corpus —
  hash every stream the tests emit before and after the refactor and
  diff the hashes. Baseline breakage from Gap B fixed.
* **Phase 1**: generic + lossless roundtrip exact; jbig2dec agreement;
  fuzz target `parse_segment_header` exists and survives 10 min.
* **Phase 2**: symbol + sym_unify + globals + multipage exact;
  `DecodedGlobals` is `Send + Sync` (compile-time assertion test);
  deterministic across thread counts.
* **Phase 3**: refinement PDF tests pass natively.
* **Phase 4**: all halftone modes exact vs jbig2dec.
* **Every phase**: no `unwrap`/`expect`/`assert!` reachable from input
  in `src/decode/` — enforce with
  `#![deny(clippy::unwrap_used, clippy::expect_used)]` at the top of
  `decode/mod.rs` rather than by grep.

Each phase ends in its own commit(s) on `main`; the checker reviews the
diff, runs the checklist, and only then does the next phase start.

---

## Phase 3 findings (refinement completion + hardening)

* **Encoder S-advance bug — CONFIRMED and FIXED.** T.88 §6.4.5 step 3c v)/xi):
  `CURS` advances by `WI - 1` where `WI` is the width of the bitmap *actually
  placed* (`IBI`), i.e. the refined width `GRW = WOI + RDWI` for `RI=1`
  instances (§6.4.11, Table 12). The decoder (and jbig2dec 0.20) advance by the
  refined placed width. The encoder's `encode_text_region_with_refinement`
  advanced by the *prototype* width, so whenever `RDW != 0` the encoded `IADS`
  deltas desynced S for every later instance in the strip. Fixed by advancing
  `CURS` by the refined trimmed width. Evidence (test_image1, native == jbig2dec
  throughout): native-decode-vs-source pixel diffs dropped symbol 2348→175,
  sym_unify 2984→1215, refine 19897→175; a synthetic RDW=+1 page went 3912→0
  (pixel-exact vs source). The residual (e.g. 175) is lossy `RI=0` substitution,
  a separate matching-policy question, not S-advance. The encoder byte change is
  isolated in a dedicated `encode_hashes.txt` regeneration commit (only
  test_image1 page streams changed; globals unchanged — the fix is text-region
  S-coordinate only). Note: `RDW != 0` only arises when `config.text_refine` is
  set (dim tolerance 2); default `text()` matching requires exact dims
  (`evaluate_symbol_match` `dim_limit = 0`), so `RDW` is always 0 there.

* **Standalone immediate generic refinement regions (types 42/43) — decoded.**
  T.88 §7.4.7: an immediate refinement region with no referred region segment
  refines the page buffer in place; the reference is the page-buffer window under
  the region box (§7.4.7.4), `GRREFERENCEDX/DY = 0`, external combop REPLACE.
  Contexts reset per region segment (§7.4.7.5 step 2). Intermediate (type 40) and
  any refinement region that *refers to another region segment* (retained
  auxiliary buffer reuse) stay typed-`Unsupported` (Phase 5). GRTEMPLATE-1 and
  TPGRON also stay typed-`Unsupported` (the encoder emits neither).

* **jbig2dec 0.20 quirk (documented, not a native bug).** For a standalone
  refinement region whose reference is the page buffer, jbig2dec extracts the
  reference window from the page origin regardless of the region's `(x, y)`, so
  it desyncs for non-zero region offsets. The native decoder follows §7.4.7.4
  (window offset by `(x, y)`) and is validated vs the intended target;
  `standalone_refinement_region_vs_jbig2dec` compares vs jbig2dec only at offset
  `(0, 0)` where both agree.

* **Robustness fix.** `decode_text_region` computed
  `GRREFERENCEDX = floor(RDW/2) + RDX` with an unchecked `i32` add; `RDX`/`RDY`
  are unbounded decoded integers, so a malformed stream overflowed. Now
  saturating (an out-of-range offset just reads the reference window as zero).
  Found by extending the chaos gate to a refine-mode stream.

## Phase 4 findings (MMR + halftone)

* **Phase 4 exit criterion MET.** All current halftone modes and MMR generic
  regions decode exactly. `tests/decode_halftone.rs` asserts, for the §21.3
  rows, `native == jbig2dec` pixel-exact across every mode combination:
  pattern dict {MMR, arithmetic} × gray planes {MMR, arithmetic} × {lossy,
  lossless}, standalone and embedded-with-globals; generic MMR is additionally
  pixel-exact vs source (lossless). Malformed inputs (GRAYMAX overflow,
  truncated collective bitmap, missing referred pattern dictionary) are typed
  errors, and the halftone stream is in the `decode_chaos` mutation/truncation
  gate. New fuzz targets `mmr_generic` and `halftone_region` compile under
  nightly.

* **MMR gray-plane block boundaries.** The encoder emits each halftone gray
  Gray-code plane as an independent, byte-aligned, EOFB-terminated T.6 block,
  concatenated back to back (Annex C, GSMMR=1). The `fax` crate's `decode_g4`
  reports no byte offset and stops at the *first* EOL of the two-EOL EOFB, so
  it undershoots the true byte-aligned boundary. `decode::mmr` recovers each
  boundary with a monotone binary-search "decode point" plus a forward scan to
  the next valid block start. This is the one place the thin `fax` wrapper
  needed non-trivial glue; a future in-house T.6 decoder that tracks bit
  position would remove it.

* **Encoder arithmetic pattern dictionary uses nominal AT, not spec AT
  (documented, not fixed).** `encode_generic_region_inner` ignores the AT
  argument and always forms the nominal template-0 context, whereas the spec
  (and jbig2dec) use AT1 = (-HDPW, 0) for the pattern-dictionary collective
  bitmap (§6.7.5). The native decoder follows the spec, so `native == jbig2dec`
  holds regardless (both decode the same bytes the same way); halftone is lossy
  so there is no source comparison to fail. Only the default MMR pattern dict is
  exercised in production; the arithmetic variant is a config option. Fixing the
  encoder's AT handling is a separate encoder task (would change halftone bytes).

* **Both encoder fixes from the ledger below are now DONE** (referred-segment
  width boundary; refinement_layout test) — see the entries.

## Phase 5a findings (generic templates 1–3 + TPGDON)

* **All four arithmetic generic templates and TPGDON decode pixel-exact vs
  jbig2dec 0.20.** `src/decode/generic.rs` now dispatches on `(template, at,
  tpgdon)`: the verified fast rolling path for nominal template-0 AT (with
  TPGDON row-duplication added), a general per-pixel template-0 path, and new
  per-pixel decoders for templates 1–3 in T.88/jbig2dec context numbering.
  `decode_generic_bitmap` gained a `template` parameter, wired through the
  symbol dictionary (SDTEMPLATE), pattern dictionary (HDTEMPLATE), and halftone
  gray planes (all previously rejected non-zero templates). The
  `GenericTemplate` and `TypicalPrediction` `UnsupportedFeature` variants are
  deleted — nothing returns them.

* **Context numbering is a relabelling; only the SLTP magic needs care.** The
  MQ stream depends only on the pixel *partition*, not the context index
  labels, so the pre-existing template-0 path (custom numbering) and the new
  templates 1–3 (spec numbering) both produce/consume jbig2dec-identical bytes.
  The one place numbering matters is the TPGDON SLTP pseudo-pixel, a reserved
  context slot: templates 1–3 use the literal spec constants (0x0795/0x00E5/
  0x0195); template 0 uses 0xB325, the spec's 0x9B25 pattern re-expressed in
  this crate's template-0 bit layout. The oracle round-trip tests confirm all
  four values against jbig2dec.

* **Pre-existing AT-count bug fixed.** `parse_generic_region` read 0 AT pixels
  for templates 2 and 3 (`_ => 0`); T.88 §7.4.6.3 gives templates 1–3 one AT
  pixel each. Never exercised before because templates 1–3 were unsupported;
  found immediately by the template-2/3 round-trip.

* **Test-only stream writer** (`tests/common/writer.rs`): drives the encoder's
  MQ coder to emit generic-region page streams for forms the encoder never
  produces. `tests/decode_generic_templates.rs` checks every template × TPGDON
  both as a native round-trip and against jbig2dec (odd-width matrix, banded
  and all-duplicate-row images for the LTP=1 path). Reused by later sub-phases.

## Phase 5c findings (Huffman decoding)

* **Huffman symbol dictionaries and text regions decode pixel-exact vs
  jbig2dec 0.20.** `src/decode/huffman.rs` holds the machinery: an MSB-first
  `BitReader`, canonical table representation with B.3 code assignment, all 15
  standard tables B.1–B.15 as static line lists, segment-type-53 custom-table
  parsing (B.2), and a `from_code_lengths` constructor for the RUNCODE and
  SBSYMCODES symbol-ID tables. The `HuffmanCoding` `UnsupportedFeature` variant
  is deleted.

* **Symbol dictionary SDHUFF=1 (§6.5, Figure 24):** height-class delta heights
  (SDHUFFDH) and delta widths (SDHUFFDW/OOB) drive a per-class collective
  bitmap read via SDHUFFBMSIZE — BMSIZE=0 uncompressed (byte-padded rows) or
  MMR of exactly BMSIZE bytes — then split into symbols; export runs via Table
  B.1. Table selection resolves standard tables or referred custom tables
  (type 53) consumed in field order (§7.4.3.1.6).

* **Text region SBHUFF=1 (§6.4):** the symbol-ID Huffman table (§7.4.3.1.7 —
  35 RUNCODE lengths, run-length-coded symbol-ID code lengths, B.3 assignment)
  is the fiddliest part and is exercised directly. FS/DS/DT table selection,
  the SBSTRIPS strip/T bookkeeping, and all four reference corners are
  implemented. A single `BitReader` spans the symbol-ID table and the strip
  data (the table ends byte-aligned).

* **Store/plumbing:** `DecodedSegment::HuffmanTable` + `gather_huffman_tables`;
  type-53 Tables segments are parsed into the store in both `page.rs` and
  `globals.rs`.

* **Oracle:** `tests/common/writer.rs` gained a Huffman writer (a `BitWriter`
  plus `encode_value`/`encode_oob` test helpers on `HuffmanTable`).
  `tests/decode_huffman.rs` builds pages of a Huffman symbol dictionary + a
  Huffman text region and checks native == expected == jbig2dec.

* **Deferred (documented):** (1) The MMR collective-bitmap branch (BMSIZE>0)
  reuses the Phase-4-validated `decode_mmr_bitmap` but is not yet directly
  oracle-tested — the writer emits BMSIZE=0. (2) Custom (type-53) tables are
  unit-tested against the B.5 byte-encoding example but not end-to-end via the
  oracle. (3) Huffman + refinement (SBREFINE=1) and transposed Huffman text
  regions surface as typed `Unsupported`, folded into Phase 5e. (4) A Huffman
  symbol-ID table with a single symbol of code length 0 (zero-bit ID) is a spec
  edge case the current `decode()` bit-matching does not special-case; tests
  use ≥2 symbols.

## Phase 5d findings (striped pages + unknown segment lengths)

* **Unknown segment data length (§7.2.7)** for immediate generic regions:
  `file.rs` scans the data part for the terminator — `FF AC` (arithmetic) or
  `00 00` (MMR), selected by the eighteenth byte's MMR bit — from the
  eighteenth byte onward, then the four-byte row count. `page.rs` overrides the
  region height with that row count and trims it from the coded data. Other
  segment types with unknown length remain a typed `Unsupported`.

* **Striped pages of unknown height (§7.4.8.5)** (page height 0xFFFFFFFF): the
  page starts empty and `MonoBitmap::grow_to_height` extends it as regions and
  end-of-stripe segments arrive, bounded by `max_page_pixels`. End-of-stripe
  (type 50) end rows set the final page height (§7.4.9). Known-height striped
  pages already worked. The `StripedPage` `UnsupportedFeature` variant is
  deleted.

* **jbig2dec quirk (documented).** For an unknown-height page jbig2dec 0.20 pads
  the output to the maximum stripe size rather than trimming to the last
  end-of-stripe row; the native decoder trims per §7.4.9. The oracle test sets
  the max stripe size equal to the true total height so both agree.

* **Writer fix.** The test writer's `generic_arith_data` had called
  `flush(true)` then `into_vec()` (which flushes a second time), leaving bytes
  after the FF AC marker — harmless for known-length regions but fatal to the
  §7.2.7 terminator scan. It now reads the buffer after a single flush.

* Oracle: `tests/decode_striped.rs` checks both forms native == source ==
  jbig2dec.

## Phase 5e/5f findings (refinement, transposed, random access)

* **GRTEMPLATE-1 refinement + TPGRON** (§6.3): both refinement templates and
  typical prediction now decode pixel-exact vs jbig2dec. The template figures
  (T.88 2000 ed., Figures 12–16, supplied as PDF) resolved the exact partitions;
  the SLTP pseudo-pixel contexts were pinned by a 20-image jbig2dec sweep —
  GR0 = 0x010 (centre-reference bit, matching Figure 14 in this crate's
  numbering), GR1 = 0x040 (this crate's template-1 numbering is a
  TPGRON-invisible transposition of jbig2dec's, so the naive 0x080 desynced).

* **Transposed text regions** (TRANSPOSED=1, §6.4.5): `place_symbol` now handles
  both orientations × all four reference corners; the arithmetic and Huffman
  text paths share it. Verified vs jbig2dec via the Huffman writer's transposed
  mode.

* **Random-access file organisation** (§D.2): `file.rs` parses all segment
  headers up to the end-of-file terminator, then the data blocks in order.
  Verified vs jbig2dec on a hand-built random-access generic page.

* **SDREFAGG=1 refinement-coded symbol dictionaries** (REFAGGNINST=1, §6.5.8.2):
  new symbols coded as a generic refinement of an earlier (imported or new)
  symbol now decode; verified vs jbig2dec via a Huffman base dict → SDREFAGG
  dict → text-region page. REFAGGNINST>1 (true aggregates) stays Unsupported.

* **Halftone HENABLESKIP** (§6.6.5.1): the HSKIP bitmap is computed and the
  arithmetic gray planes decode with a skip-aware generic path; verified vs
  jbig2dec. HENABLESKIP with MMR gray planes stays Unsupported.

* Variants deleted: `TransposedTextRegion`, `RandomAccessOrganisation`.

## Phase 5e completeness (aggregate / Huffman-refine / retained contexts)

* **REFAGGNINST>1 true aggregate dictionary symbols** (§6.5.8.2 step 2): a new
  symbol coded as an internal text region over the imported + already-decoded
  symbols. The arithmetic text-region loop was extracted
  (`decode_text_region_arith`) to serve both segments and aggregates; a latent
  bug (early return mid-strip skipping the terminating IADS OOB) was fixed —
  harmless for a segment, fatal to an aggregate's shared stream. Verified vs
  jbig2dec.

* **Huffman + refinement text regions** (SBHUFF=1 ∧ SBREFINE=1, §6.4.11.5):
  Huffman-coded RI/RDW/RDH/RDX/RDY + SBHUFFRSIZE size, then a byte-aligned
  arithmetic refinement block (fresh GR stats per block). **Native round-trip
  only** — jbig2dec 0.20's Huffman refine path does not consume the SBRAT field
  and mis-reads the symbol-ID runcode table (its arithmetic refine path is
  fine); this decoder follows the documented Figure-35 field order.

* **Retained arithmetic contexts** (§6.5.5 steps 3/7, the "bitmap coding context
  used/retained" flags): generic + refinement statistics are saved by a
  retaining dictionary and imported by a later one instead of resetting.
  **Native round-trip only** — jbig2dec 0.20 reports this NYI and aborts. The
  native check is strong: dict B's bitmaps are coded from dict A's warmed
  contexts, so they only decode when the import happens.

## Phase 5f findings (organisation, recovery, renderer polish)

* **Intermediate regions + auxiliary buffers** (types 4/20/36/40, §7.4.7.4,
  §8.2): intermediate regions are retained (`DecodedSegment::Region`) instead of
  composited; a refinement region referring to one uses its bitmap as
  GRREFERENCE. Native-verified — jbig2dec 0.20 reports intermediate regions NYI.

* **`decode_embedded_into`** zero-alloc renderer API (§5): decodes into a
  caller-provided `MonoBitmap`, reusing its allocation. A counting-allocator
  test asserts the per-decode allocation count is *stable* across same-size
  pages (measured 7; reducing to zero is a documented follow-up).

* **`DecodeStrictness::Compatible` + `RecoveryEvent`** (§20): trailing garbage
  after the last well-formed segment is tolerated and recorded; Strict still
  errors. Further recoveries await real malformed streams (the plan ties each
  to a fixture).

* **Fuzz targets** (§21.6): `embedded_document` (full pipeline, both strictness
  modes) and `huffman_table` (custom-table parser) — both survive a smoke run
  with no panic/hang/RSS growth.

## jbig2dec 0.20 coverage limits

Three features are verified by **native round-trip only** because jbig2dec 0.20
cannot decode them (not this decoder's limitation):

* Huffman + refinement text regions — jbig2dec skips the SBRAT field (a
  jbig2dec bug; its arithmetic refine path is fine).
* Retained arithmetic contexts — jbig2dec prints "(NYI)" and aborts.
* Intermediate regions — jbig2dec prints "(NYI)" and aborts.

Each native check is content-dependent (it fails if the feature is a no-op), so
it is a genuine end-to-end verification against a spec-compliant writer.

## Full decode-spec coverage (final coding gaps closed)

* **Huffman refinement/aggregate dictionary** (SDHUFF=1 ∧ SDREFAGG=1, Figure 25,
  §6.5.8.2): the Huffman text-region strip loop was extracted
  (`decode_huffman_text_core`) to serve both a segment and this dictionary.
  New symbols decode as a refinement (REFAGGNINST=1: Huffman RDX/RDY = B.15,
  BMSIZE = B.1, byte-aligned arithmetic block) or an internal Huffman text
  region (REFAGGNINST>1, equal-length SBSYMCODES per §6.5.8.2.3). Native
  round-trip verified (jbig2dec 0.20 cannot decode the Huffman refine path).

* **HENABLESKIP + MMR gray planes**: no longer rejected. Per Annex C.5 GSUSESKIP
  applies only to arithmetic gray planes; MMR planes are coded whole and the
  outside cells are dropped by render-time clipping, so an MMR halftone with
  HENABLESKIP decodes identically to one without (covered by the MMR halftone
  tests).

**Every JBIG2 coding form the spec defines now decodes.** The `SymbolRefinement`
and `HalftoneCoding` `UnsupportedFeature` variants are deleted. The only
remaining `Unsupported` returns are for non-coding cases: an unknown/unassigned
segment type code, an unknown data length on a non-generic segment (§7.2.7
allows it only for immediate generic regions), a region/page segment misplaced
in a globals stream, and a symbol-ID width above 24 bits (a resource limit).

## Non-coding follow-ups (optimisation / tooling)

* Reducing steady-state decode to zero heap allocations (pool the parse /
  segment-store / region scratch) — an optimisation, not a coding gap.
* A wild-PDF corpus from commercial encoders, if any can be identified.

## Gap F: Out of scope for now (explicitly deferred)

* Product B / Phase 5 (Huffman, striped pages, random access, templates
  1–3, TPGDON) — planned in `jbig2decplan.md`, not scheduled here.
* Hayro differential oracle — `jbig2decplan.md` recommends it; jbig2dec
  is sufficient as the single oracle for Phases 1–4. Add Hayro as a
  dev-dependency comparison only if jbig2dec disagreements need a
  tiebreaker.
* Crate rename and crates.io publish.
* Migrating the encoder's `BitImage` internals onto `MonoBitmap`
  (post-decoder cleanup, `jbig2decplan.md` §7).
