# Phase 5 Sub-Plan: General PDF Compatibility (Product B)

Status: **not started — planned for later.** Phases 0–4 are complete: the
native decoder handles every stream this crate's encoder emits, pixel-exact
against jbig2dec 0.20. Phase 5 turns that self-decoder into a general
decoder for JBIG2 found in arbitrary PDFs, so the PDF renderer no longer
needs Hayro (`jbig2decplan.md` §23 Phase 5 exit condition).

Authority order: `jbig2decplan.md` (architecture) → `jbig2dec-gaps-plan.md`
(process/acceptance) → this document (Phase 5 work order). The rules that
governed Phases 0–4 all still apply: typed errors, limits before allocation,
no input-reachable panics, monomorphic hot loops, encoder byte-identical
guard, clippy `-D warnings` on new code, granular commits, per-sub-phase
verification against the Gap E checklist.

## What is actually missing

Every gap is already a typed `UnsupportedFeature` variant
(`src/shared/error.rs:13`), so coverage is mechanically enumerable:

| Variant | Work item | Sub-phase |
|---|---|---|
| `GenericTemplate(1..=3)` | generic templates 1–3 | 5a |
| `TypicalPrediction` | TPGDON | 5a |
| `HuffmanCoding` | full Huffman machinery | 5c |
| `StripedPage` | striped pages + EOS handling | 5d |
| `UnknownSegmentLength` | §7.2.7 unknown-length generic regions | 5d |
| `SymbolRefinement` | SDREFAGG aggregate/refined dictionary symbols | 5e |
| `TransposedTextRegion` | TRANSPOSED=1 text regions | 5e |
| `RandomAccessOrganisation` | random-access file organisation | 5f |
| `SegmentType(_)` (intermediate 36/40; tables 53) | intermediate regions, custom tables | 5c/5f |

Plus items not gated by a variant: TPGRON and GRTEMPLATE-1 refinement
(currently typed-`Unsupported` inline), `HENABLESKIP` halftone skip,
`DecodeStrictness::Compatible` recovery mode (defined, unused), and the
`decode_embedded_into` zero-alloc API from `jbig2decplan.md` §5.

Ordering below is by frequency in real PDF corpora: scanner-produced JBIG2
overwhelmingly uses generic regions with TPGDON and templates 0–2, or
Huffman-coded symbol dictionaries from hardware encoders; striped pages and
random access are rarer; aggregate dictionaries are rarest.

---

## Sub-phase 5a: Generic templates 1–3 and TPGDON

The highest-value, lowest-risk chunk — most wild generic regions use
TPGDON, and templates 1–3 are small variations of the template-0 pixel loop
already in `src/decode/generic.rs`.

1. Implement templates 1–3 per T.88 §6.2.5.3/Figure 8: separate rolling
   context builders (13/10/10-bit contexts, 1 AT pixel for templates 1–3
   instead of 4). Follow the existing pattern: fast path for nominal AT,
   general path for arbitrary AT; a test asserting both agree.
2. TPGDON per §6.2.5.7: the SLTP pseudo-pixel decoded with the per-template
   magic context before each row; duplicated rows copied from the previous
   row. Applies to all four templates.
3. Extend generic decoding used *inside* symbol dictionaries and refinement
   to accept the same templates only where the spec allows (SDTEMPLATE
   selects 0–3 — wire it through `symbol_dictionary.rs`).
4. Oracle problem: this crate's encoder never emits these forms, so
   encoder-roundtrip cannot cover them. Tests must come from item 5b's
   corpus OR hand-built streams: implement a minimal test-only writer
   (test helper, not product code) that reuses the encoder's MQ coder to
   emit template-1/2/3 + TPGDON streams, then assert native == jbig2dec
   pixel-exact on those. The §21.2 odd-width property matrix reruns per
   template.

Exit: all four templates × TPGDON on/off decode pixel-exact vs jbig2dec on
generated + corpus streams.

## Sub-phase 5b: Wild-corpus infrastructure (parallel with 5a)

Phase 5 correctness claims are only as good as the corpus. Build it early;
every later sub-phase adds its rows to the same harness.

1. Fixture corpus under `tests/fixtures/wild/` (committed, small): extract
   raw JBIG2 streams + globals from real PDFs (PDF.js and Apache PDFBox
   test suites are the richest public sources of odd JBIG2 — check their
   licenses per stream; the T.88 conformance examples if retrievable;
   plus any PDFs already on this machine — the Lege test-outputs folder
   has scanner PDFs). Store as `<name>.page.jb2` + optional
   `<name>.globals.jb2` + `<name>.expected.pbm` (generated once by
   jbig2dec and reviewed).
2. A small extraction tool (`src/bin/` or `tests/tools/`) that pulls
   JBIG2Decode streams + JBIG2Globals out of a PDF (lopdf was removed as a
   dependency — either a dev-dependency, or a 100-line raw scanner since
   streams just need locating; prefer dev-dep simplicity).
3. C `jbig2enc` corpus (gaps-plan Gap C direction 3): build agl/jbig2enc
   once locally, generate symbol/text/refinement/generic streams from the
   two PBM fixtures, commit the outputs (<200 KB), never the binary.
4. `tests/decode_wild.rs`: for every corpus entry, native decode must
   equal the stored expected PBM (which equals jbig2dec). Entries the
   decoder cannot yet handle are listed in an explicit
   `EXPECTED_UNSUPPORTED` table with the variant they should return —
   the table shrinks as sub-phases land, and a stream decoding
   *incorrectly* (rather than `Unsupported`) is always a failure.
5. Optional tiebreaker: keep Hayro out unless a jbig2dec disagreement
   needs arbitration (Gap F stance unchanged).

Exit: harness runs in CI-style `cargo test`, corpus ≥ 15 wild streams
covering Huffman, TPGDON, templates 1–2, striped, and MMR variants.

## Sub-phase 5c: Huffman decoding

The largest single work item; T.88 Annex B + §6.4/§6.5 Huffman paths, plus
segment type 53 (custom tables).

1. `src/decode/huffman.rs`: canonical Huffman table representation built
   from (prefix-length, range-length, range-low) line lists per Annex B.1
   — assignment of prefix codes (B.3), OOB and lower/upper range lines,
   32-bit range escapes. Decode via a small table-driven reader on a new
   bit-reader over `shared::reader::Reader` (MSB-first; distinct from the
   MQ decoder). `max_huffman_table_entries` limit enforced at build time.
2. The 15 standard tables B.1–B.15 as static data, verified by a test
   that rebuilds each from its Annex B line list.
3. Segment type 53 custom-table parsing (§7.4.13) into the same
   representation, stored in `SegmentStore`/`DecodedGlobals` (the
   `HuffmanTable` arm of `DecodedSegment` already exists in the plan §19;
   add it now).
4. Huffman symbol dictionaries (§6.5 with SDHUFF=1): DH/DW/BMSIZE/AGG
   table selection, collective-bitmap path (BMSIZE=0 uncompressed rows
   and MMR variants — the collective-bitmap split logic from
   `pattern_dictionary.rs` generalizes), export runs via Table B.1.
5. Huffman text regions (§6.4 with SBHUFF=1): runcode-based symbol-ID
   code assignment (§6.4.5 step 1 / Table 15 runcodes — the fiddliest
   part; test it in isolation), FS/DS/DT/RDW/RDH/RDX/RDY/RSIZE table
   selection incl. custom-table referencing rules (§7.4.3.1.6).
6. Tests: corpus streams from 5b (hardware-scanner PDFs are mostly
   Huffman) + hand-built minimal streams for each standard-table
   selection path; error tests for over-long prefixes, missing custom
   tables, OOB where not permitted.

Exit: every Huffman stream in the corpus decodes pixel-exact vs jbig2dec;
`HuffmanCoding` variant deleted (compile error guarantees nothing still
returns it).

## Sub-phase 5d: Striped pages and unknown lengths

1. Striped pages (§7.4.8.5, page-info striping field): page bitmap grown
   stripe-by-stripe, END_OF_STRIPE row bookkeeping, unknown page height
   (0xFFFFFFFF) with growth bounded by `DecodeLimits.max_page_pixels`.
2. Unknown segment data length (§7.2.7): only legal for immediate generic
   regions; scan for the terminator sequence per spec (for MMR: the
   two-byte FF AC; for arithmetic: FF AC preceded by the required
   context) with a bounded search window.
3. Auto-detection interplay: embedded streams with striping must keep
   `decode_embedded` single-call ergonomics.

Exit: striped/unknown-length corpus entries decode; malformed terminator
fuzz cases return typed errors within limits.

## Sub-phase 5e: Dictionary and text-region completeness

1. SDREFAGG=1 symbol dictionaries (§6.5.8.2): refinement-coded new symbols
   (REFAGGNINST=1 fast path first — it reuses `refinement.rs` directly),
   then true aggregates via an internal text-region invocation
   (REFAGGNINST>1; rare — corpus-driven priority).
2. Transposed text regions (TRANSPOSED=1, §6.4.5 step 3c placement
   transposition) and the remaining REFCORNER interactions.
3. GRTEMPLATE-1 refinement (§6.3.5.3 second template) and TPGRON
   (§6.3.5.6) — both small once 5a's TPGDON machinery exists.
4. Halftone HENABLESKIP (§6.6.5.1 skip bitmap).
5. Retained arithmetic contexts across segments (§7.4.4.3 SDUSEDCTX /
   context-retention flags) — implement storage in `DecodedSegment`, and
   the reset-vs-retain decision per flags; this is also where
   `RetainedArithmeticContexts` from plan §16 becomes real.

Exit: corresponding corpus entries decode; the variants
`SymbolRefinement`, `TransposedTextRegion` deleted.

## Sub-phase 5f: Organisation, recovery, and renderer polish

1. Random-access file organisation (Annex D.1): parse all headers first,
   then execute in order — the `ParsedDocument` split from plan §9 was
   designed for this; mostly plumbing.
2. Intermediate regions (types 36/40 intermediate) + auxiliary-buffer
   reuse rules; `Region`/`DecodedSegment` retention with
   `max_retained_bytes` enforcement and post-page trimming (plan §13).
3. `DecodeStrictness::Compatible` + `RecoveryEvent` (plan §20): implement
   only recoveries the corpus actually demands (each tied to a named
   fixture), e.g. trailing garbage after last segment, ignored reserved
   bits. Strict remains the test default.
4. `decode_embedded_into(&mut MonoBitmap, …)` zero-alloc API (§5) +
   a counting-allocator test for steady-state zero allocation (Gap D
   item that was deferred).
5. Fuzz expansion: `embedded_document` end-to-end target seeded with the
   full wild corpus; run each target ≥ 10 CPU-minutes as the phase gate.
6. Performance pass: rerun all benches; wild-corpus decode must stay
   within the Gap D targets (≤1.25× jbig2dec per stream); profile with
   the `profiling` feature if any stream regresses.

Exit = **Phase 5 / Product B exit** (`jbig2decplan.md` §23): the PDF
renderer no longer needs Hayro for the supported document corpus; every
corpus stream decodes pixel-exact vs jbig2dec or returns a documented
`RecoveryEvent`; no `UnsupportedFeature` variant remains reachable from
the corpus.

---

## Execution protocol (same as Phases 0–4)

One Opus agent per sub-phase (5a and 5b can run as one agent or two in
sequence — 5b first if only one), each briefed with: the relevant plan
sections, the mirror encoder/reference material, the acceptance checklist
(Gap E: build --all-targets, full test incl. `--features refine`,
`--no-default-features --features decode` and `encode,symboldict`, clippy
`-D warnings` on new code, bench --no-run, byte-identical guard), and the
requirement to report deferred items with justification. The supervisor
verifies each report against the checklist and spot-checks headline claims
before the next sub-phase starts. Encoder changes remain forbidden unless
a conformance bug is proven, with the dedicated-manifest-commit ritual.

Suggested sequencing: 5b → 5a → 5c → 5d → 5e → 5f, with 5c the long pole.
Nothing in 5d–5f depends on 5c except corpus overlap, so 5c can also be
split across two agent runs (tables+dictionaries, then text regions) if a
single run proves too large — Phase 3/4 showed session limits and server
errors interrupt long runs, so prefer committing verified chunks early.
