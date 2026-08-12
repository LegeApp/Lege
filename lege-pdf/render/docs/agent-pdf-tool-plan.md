# Native Lege PDF agent tool plan

## Recommendation

Build a stable, structured front end over the existing `pdf-*` crates, using
`pdfr` as the prototype but not exposing its current development-only output as
the agent contract. The tool can replace the common `pdftotext` and `mutool`
calls used by coding agents—document inspection, page text, text geometry,
image/object inventory, page rendering, and bounded content dumps—but should
not claim to replace MuPDF's repair, rewriting, conversion, or full PDF syntax
toolbox in its first release.

There is real benefit, but the first benefit is integration and evidence rather
than raw speed:

- agents see the same parser, recovery behavior, page compiler, text model, and
  renderer that Lege itself uses;
- every answer can carry stable page, object, geometry, codec, paint-origin,
  recovery, and degradation data instead of requiring an agent to correlate
  several unrelated command outputs;
- JSON/JSONL can be schema-versioned and bounded, eliminating brittle parsing
  of human-oriented `mutool show` output;
- one cross-platform Rust binary removes Poppler/MuPDF installation and version
  drift from the agent environment;
- failures found by the tool become direct fixtures for Lege's PDF engine.

It is not yet a performance replacement. On `Structures.pdf` page index 189, a
warm debug `pdfr text` run took about 0.21 seconds and 98 MB RSS, while the
installed `pdftotext -f 190 -l 190 -layout` took about 0.04 seconds and 18 MB.
The extracted payloads were comparable in size (2945 versus 2929 bytes), but a
release benchmark and persistent server mode are required before making speed
claims. Opening and compiling once for a batch of agent queries should remove
most of the current per-command penalty.

## Existing foundation

The repository already contains most of the engine:

- `pdf-source` and `pdf-structure`: owned/mapped input, xref handling, object
  streams, recovery events, and bounded parsing;
- `pdf-document`: page tree, encryption/password handling, names, snapshots,
  and resource resolution;
- `pdf-content`: semantic page compilation and deterministic operation dumps;
- `pdf-text`: normalized UTF-16/text, character metadata, exact word boxes,
  rectangles, text-object identity, RTL handling, and region queries;
- `pdf-page-ir`: typed images, codecs, masks/soft masks, paint origins, paths,
  shadings, fonts, and display operations;
- `pdf-read`: read-only health reports covering recovery, encryption, revisions,
  page compilation, annotations, JavaScript, forms, outlines, optional content,
  and object streams;
- `pdf-render-cpu`/`pdf-render-wgpu`: page and crop rendering plus diagnostic
  attribution planes;
- `pdf-cli`'s `pdfr`: working `info`, `doctor`, `dump`, `text`, `render`, and
  `attribute` prototypes;
- `pdfium-text-diff` and `pdfium-diff`: differential controls that can gate
  compatibility without becoming runtime dependencies.

The missing piece is a cohesive agent-facing protocol, not another PDF parser.

## Proposed command

Name the installed binary `lege-pdf`. Keep `pdfr` as a developer compatibility
alias until scripts migrate.

```text
lege-pdf inspect FILE [--pages RANGE] [--json|--jsonl]
lege-pdf text FILE [--pages RANGE] [--layout plain|blocks|words|chars]
                   [--bbox X0,Y0,X1,Y1] [--json|--jsonl]
lege-pdf images FILE [--pages RANGE] [--extract DIR] [--rendered]
                     [--json|--jsonl]
lege-pdf content FILE --page N [--ops] [--resources] [--objects]
                      [--max-items N] [--json]
lege-pdf render FILE --pages RANGE --output TEMPLATE [--dpi N|--scale N]
                     [--format png|ppm] [--crop X0,Y0,X1,Y1]
lege-pdf search FILE QUERY [--pages RANGE] [--context N] [--jsonl]
lege-pdf serve [--stdio] [--max-open N] [--idle-timeout SECONDS]
```

Use one-based page numbers at the public CLI boundary because agents and users
refer to printed PDF pages that way. JSON must also include `page_index`
(zero-based) so engine calls are unambiguous. Reject mixed conventions rather
than guessing.

## Versioned output contract

Human-readable output is useful interactively, but agents should request JSON
or JSONL. Every record should include:

```json
{
  "schema": "lege-pdf.agent/v1",
  "document": "/absolute/or/redacted/path.pdf",
  "page": 190,
  "page_index": 189,
  "status": "ok",
  "warnings": [],
  "data": {}
}
```

Rules:

1. Write records to stdout and diagnostics to stderr.
2. Emit one bounded JSONL record per page for multi-page operations, so an
   agent can consume partial success without holding a whole book in context.
3. Use stable enum strings and explicit units (`pdf_points`, `device_pixels`).
4. Return nonzero only for command/document failure; page-local failures remain
   records with `status: "failed"` unless `--fail-fast` is selected.
5. Include recovery/degraded-render notes instead of silently dropping content.
6. Add `--max-pages`, `--max-items`, `--max-bytes`, and `--timeout` to every
   potentially expansive operation. Defaults must be safe for agent calls.
7. Never print passwords, decrypted stream data, or unbounded object payloads
   unless explicitly requested.

## High-value data models

### `inspect`

Extend `pdf-read::DocumentReport` with serializable views rather than parsing
its current summary string. Include page boxes/rotation, metadata, outline
count, embedded-file count, font inventory, image count, image codecs, masks,
annotation counts, recovery events, encryption, optional content, and compile
status. This replaces most `pdfinfo` plus the common `mutool info/show` probes.

### `text`

Batch pages through one `DocumentSnapshot`. Expose:

- normalized plain text;
- blocks/lines/words with exact PDF-space boxes;
- characters with Unicode source, CID, glyph ID, text-object identity, tight
  and loose boxes;
- inclusion controls already supported by `TextPageOptions` (hidden content,
  annotations, soft masks, RTL, normalization);
- region extraction and stable reading-order diagnostics.

Add a `--layout blocks` representation before promising parity with
`pdftotext -layout`; current `TextPage::all_text` and `words()` are strong
primitives but not a complete physical-layout serialization.

### `images`

This is where a native tool materially exceeds the current shell combination.
For every `DrawImage`, report resource/object identity, intrinsic dimensions,
bits per component, color space, codec, transform and painted bounds, stencil,
color-key mask, soft mask, paint origin, visibility/optional-content state,
decode degradation, and reuse count. Distinguish:

- encoded source extraction (exact stream when safe and meaningful);
- decoded pixel extraction;
- rendered appearance extraction (after masks, transforms, blend, and color).

Those are different questions and must never share one ambiguous `extract`
mode.

### `content`

Replace the current free-form semantic dump with a serializable operation
view. Retain the deterministic text dump for diffs, but give agents typed
operations and resource references. Raw object inspection should be bounded,
cycle-aware, decoded only on request, and labeled with object/revision origin.

### `render`

Reuse `RenderRequest`, PNG output, crop transforms, cancellation, attribution,
and degradation reporting. Support stdout only for one image; batches use an
output template plus JSONL manifests. A `--thumbnail` preset is useful for
visual agent inspection and avoids multi-megapixel defaults.

## Persistent agent mode

`lege-pdf serve --stdio` provides the largest practical improvement over
repeated `pdftotext`/`mutool` processes. Use newline-delimited requests and
responses with request IDs. Cache a bounded number of immutable
`DocumentSnapshot`s by canonical path, size, and modification time. Compile
pages lazily and cap memory with LRU eviction. Cancellation must be per request.

Do not start with MCP-specific code. First define a small Rust service API and
the JSONL stdio protocol. An MCP adapter or Codex tool can then translate tool
calls without owning PDF semantics. This keeps the binary usable by shell
agents, editors, tests, and other orchestrators.

Suggested tool surface for an adapter:

- `pdf_inspect(path, pages?, detail?)`
- `pdf_text(path, pages, mode?, bbox?)`
- `pdf_images(path, pages, mode?)`
- `pdf_content(path, page, filters?, limit?)`
- `pdf_render(path, pages, dpi?, crop?)`
- `pdf_search(path, query, pages?)`

Return file/image resources for large outputs instead of base64 inside JSON.

## Implementation sequence

### Phase 1 — stabilize `pdfr`

- Add `serde` views for `DocumentReport`, text words/chars, page boxes, images,
  and semantic operations.
- Centralize page-range parsing and one-based/zero-based conversion.
- Add JSON/JSONL, bounded output, stable exit codes, and snapshot reuse within a
  command.
- Add `cargo pdf-tool` and package the binary under `lege-pdf`.
- Golden-test schemas and partial-page failure behavior.

Exit gate: `inspect`, multi-page `text`, and `render` can replace agent calls to
`pdfinfo`, `pdftotext`, and `mutool draw` on the regression corpus.

### Phase 2 — image and content intelligence

- Add image inventory with transform, mask, codec, origin, and reuse metadata.
- Add source/decoded/rendered extraction modes.
- Add typed semantic-operation JSON and bounded object/resource inspection.
- Cross-check image inventories against rendered attribution planes so an image
  that exists but paints no pixels is explained.

Exit gate: an agent can diagnose a missing image without invoking `mutool show`
or reverse-engineering a free-form dump.

### Phase 3 — search and persistent service

- Add document-wide text search with geometry and context.
- Add stdio JSONL service, snapshot/page caches, cancellation, timeouts, LRU
  memory limits, and file-change invalidation.
- Add a thin MCP/Codex adapter only after the protocol is stable.

Exit gate: repeated queries over a large book open it once, remain bounded, and
beat repeated external-process workflows in wall time.

### Phase 4 — compatibility and packaging

- Differentially test text against Poppler/PDFium and rendering against the
  existing multi-renderer harness; discrepancies are review inputs, not votes.
- Package on Linux, Windows/MSIX, and macOS with no external PDF executable.
- Publish schema documentation and compatibility aliases.

## Validation and success criteria

- Corpus: clean, repaired-xref, encrypted, object-stream, CJK/RTL, Type 3,
  scanned, MRC/JBIG2-mask, annotations, optional-content, and malformed PDFs.
- Text: Unicode edit distance, word/character boxes, region extraction, reading
  order, hidden/annotation controls, and deterministic repetition.
- Images: object count, draw count, transforms, masks, codecs, extracted hashes,
  and agreement between draw inventory and attribution coverage.
- Rendering: dimensions, degradation flags, cancellation, deterministic hashes
  where fonts/assets are embedded, and visual differential thresholds.
- Agent behavior: bounded output, valid JSON under every failure, no secret
  leakage, no hangs, and partial results for bad pages.
- Performance: release builds; cold one-shot and warm persistent modes measured
  separately. The warm service should be competitive with or faster than
  repeated Poppler/MuPDF invocations for ten or more queries on one document.

## Bottom line

The tool is worthwhile because Lege now has enough native PDF machinery to
offer agents a single, structured, product-faithful diagnostic surface. It
should initially supplant the narrow external commands agents actually use,
not advertise universal MuPDF replacement. Implement JSONL and multi-query
snapshot reuse first; those provide more value than adding another human-only
subcommand or optimizing a debug one-shot benchmark.
