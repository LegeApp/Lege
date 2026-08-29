# AGENTS.md

This file provides guidance to Codex agents working in the `jp2lam` crate.

## Project Goal

`jp2lam` is intended to become an original, idiomatic Rust JPEG 2000 encoder. It should use ISO/IEC 15444-1 as the source of truth and use OpenJPEG only as an interoperability oracle or secondary reference. Do not mechanically transpose OpenJPEG's C data structures into this crate.

The current `openjp2` dependency is transitional. The long-term target is a self-contained safe Rust implementation for document-oriented JPEG 2000 encoding.

## Context Before Algorithm Work

Before changing codec behavior, packet writing, Tier-1 coding, transforms,
quantization, or grayscale decoder-compatibility tests, consult the current AKR
work record and its linked evidence. That ledger is the durable handoff; do not
assume an untracked or scratch agent trace exists.

Some development checkouts also contain ignored local research aids:

- `llm-docs/iso-15444-1-crosswalk.md`
- `llm-docs/rust-idiomatic-guidelines.md`

Use them when present, but they are not required checkout files. For normative
codec questions, use ISO/IEC 15444-1 as the primary technical reference. A local
converted copy may be available at:

- `research/Information technology - JPEG 2000 image coding system - -- ISO_IEC JTC 1_SC 29 Coding of audio.txt`

The corresponding ignored local PDF can be used for diagrams, tables, and cases
where text extraction flattened notation:

- `research/Information technology - JPEG 2000 image coding system - -- ISO_IEC JTC 1_SC 29 Coding of audio, picture, multimedia and -- ISO_IEC 15444, 1, 1, 2000 -- 76723da8899c5d0b104dfbe4d16eef05 -- Anna’s Archive.pdf`

## Native Rust Architecture

The encoder should be a pipeline of typed transformations:

1. Domain model and geometry.
2. Sample preparation and component transform.
3. DWT.
4. Quantization.
5. Code-block coding.
6. Layer formation and rate allocation.
7. Packet planning and header building.
8. Codestream marker writing.
9. Optional JP2 wrapping.

Prefer explicit stage inputs and outputs over a giant mutable encoder context. Each stage should expose typed data that the next stage consumes. Do not let later stages reach into earlier-stage scratch state.

Architectural guardrails:

- Keep geometry and Annex B partitioning test-heavy and separate from entropy coding.
- Keep packetization from reaching inside MQ/Tier-1 internals; it should consume encoded code-block pass metadata and bytes.
- Keep marker writing as serialization of validated structures, not semantic validation mixed with byte writing.
- Keep reversible and irreversible paths explicit in types or enums where practical.
- Build correctness first, then memory optimization, then speed optimization, then rate-distortion optimization.
- Add a test or diagnostic dump whenever a stage boundary is introduced so its output is inspectable.

## Standard-First Workflow

This workflow is mandatory for all agents working on JPEG 2000 behavior in this crate. The crate should be organized by ISO/IEC 15444-1 clauses instead of OpenJPEG internals.

Follow this order when implementing or debugging JPEG 2000 behavior:

1. Read the relevant ISO/IEC 15444-1 clause from an available licensed copy.
2. If the local research aids are present, check
   `llm-docs/iso-15444-1-crosswalk.md` for the module map and known clause
   anchors.
3. Express the standard rule locally in Rust code or a small clause-keyed test.
4. Validate the resulting codestream with standard-conforming decoders.
5. Use OpenJPEG comparisons only as diagnostic evidence after the standard-based behavior exists locally.

Keep "what the standard says" separate from "what OpenJPEG does" in comments, tests, and notes.

Do not start by reading OpenJPEG source when the behavior is defined by the standard. OpenJPEG may confirm interoperability after the local standard-based rule exists; it must not be the primary design source.

## OpenJPEG Usage

OpenJPEG is useful for:

- decoder acceptance tests
- temporary byte-level diagnostics while bringing up narrow native paths
- examples of behavior where the standard leaves implementation latitude

OpenJPEG should not be used as:

- the architectural template for `jp2lam`
- a source of C-shaped structs or ownership models
- a substitute for reading the standard
- the first reference for syntax rules that are explicitly defined in ISO/IEC 15444-1
- the definition of acceptable encoded output
- a byte-parity target, especially for lossy encoding

## Rust Modernization Guidance

The crate is pinned to Rust 1.95. Keep refactors within that toolchain's stable
feature set.

Prefer Rust-native structure and ownership:

- Use safe slices, iterators, typed structs, and explicit domain models.
- Use newer Rust features when they improve clarity without churn, especially `array_windows`, `Vec::push_mut`, `get_disjoint_mut`, const-capable table generation, and clean compile-time configuration.
- Consider safe `#[target_feature]` dispatch only for isolated hot paths such as DWT, color transform, quantization, or entropy-coding helpers.
- Avoid adding `unsafe` unless there is a measured hot-path reason and the safety argument is local and clear.

Do not refactor solely to use a new language feature. Apply newer APIs where they simplify codec code or remove borrow-checker workarounds.

## Testing Expectations

For grayscale native encoder work:

- Keep `cargo test --lib` green when running from `lege-codecs/jp2lam`.
  From the repository root, use
  `cargo test --manifest-path lege-codecs/jp2lam/Cargo.toml --lib`; jp2lam is a
  path-patched nested crate, not a root-workspace member.
- Prefer decoder acceptance and exact roundtrip tests over OpenJPEG byte-parity tests.
- Add clause-keyed tests when fixing standard-defined behavior, especially around Annex B packet headers, Annex D Tier-1 coding, Annex E quantization, and Annex F DWT.
- Keep byte-parity tests ignored unless they guard a very narrow bring-up invariant that cannot be expressed more directly.
- Use diagnostic ignored tests for verbose dumps and OpenJPEG byte comparisons; promote stable decoder-compatibility checks into the normal suite.

For lossy encoding:

- Do not pursue byte parity with OpenJPEG.
- Validate that outputs are accepted by independent decoders and satisfy standard syntax constraints.
- Use quality metrics, size behavior, visual checks, and document-workload acceptance criteria instead of OpenJPEG byte equality.

For packet-header work, prioritize:

- Annex B.10.5 zero bit-plane information
- Annex B.10.6 number of coding passes
- Annex B.10.7 code-block contribution length signaling
- Annex B.10.8 packet-header field ordering

## Current Direction

The native grayscale lossless lane is the immediate proving ground. Preserve the public API while replacing the backend in vertical, testable slices:

1. Internal plan and marker writing.
2. Tier-2 packet writing.
3. Tier-1 code-block encoding.
4. Reversible grayscale end-to-end.
5. RGB lossless with RCT.
6. Irreversible 9/7 and rate control after reversible paths are solid.

The implementation should read like a Rust implementation of the JPEG 2000 standard, not a cleaned-up C port.
