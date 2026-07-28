---
name: pdfrenderer-native-first
description: "pdf-renderer prefers native in-house implementations over external crates (fonts, JPEG); user supplies specific external components deliberately"
metadata: 
  node_type: memory
  type: feedback
  originSessionId: f965981c-6368-4733-8894-47d9d033901f
---

For the pdf-renderer project (pdfium-port-plan), the user prefers **native,
in-house, single-file implementations** over pulling external crates, so the
code stays easy to optimize iteratively.

**Why:** Performance through later iterative optimization is a core project
goal; small owned implementations give full control (SIMD seams, fixed-point
rewrites) that dependency crates don't.

**How to apply:** Default to writing the component natively (as done for the
font semantics layer and the JPEG decoder at `crates/pdf-image/src/jpeg/`).
Exceptions are deliberate, user-supplied
choices only: Skrifa for font outline parsing, and a JPEG 2000 decoder the
user already has for JPX. Don't suggest crates.io codecs/parsers as
the first option.

As of 2026-07-18 the native-first goal is fully realized — **no third-party
codecs remain at runtime**: JBIG2 via `jbig2enc-rust` (decode half, Phase 5
complete, `Compatible` strictness; `crates/pdf-image/src/jbig2.rs`), and
CCITTFax via a native T.4/T.6 decoder ported line-cited from PDFium's
`faxmodule.cpp` (`crates/pdf-image/src/ccitt.rs`), verified byte-identical to
`hayro-ccitt` across a 1260-case matrix — hayro-ccitt survives only as a
dev-dependency differential oracle. The remaining deliberate externals are
Skrifa (fonts) and jp2lam (JPX). Multi-agent porting pattern that worked:
agent implements in one crate with a full-context brief + PDFium reference
citations; orchestrator verifies, commits, wires call sites. See
[[jbig2-decoder-effort]] and [[pdfrenderer-corpus-gaps]].
