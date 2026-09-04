# lege-pdf (agent tool)

Structured, agent-facing CLI and local MCP server over Lege's native PDF engine (`pdf-*` crates).
Binary name: **`lege-pdf`**. Package: **`lege-pdf-agent`**.

The MCP integration is a **first draft**: its bounded tool contract is usable,
but complex layout reconstruction, some malformed/rare PDF features, and OCR
character-level geometry are not yet production-complete.

This is not a MuPDF/Poppler replacement for repair, rewrite, or conversion.
It covers the commands agents actually use: inspect, text, images, content,
render, search, print, and a persistent stdio service.

## Build

```sh
cargo build --manifest-path lege-pdf/agent/Cargo.toml --bin lege-pdf
# aliases
cargo pdf-tool
cargo lege-pdf
```

Install the current workspace build as `/home/dk/bin/lege-pdf`:

```sh
cargo install --path lege-pdf/agent --root /home/dk --locked --force
```

## Page numbering

- CLI page numbers are **one-based** (printed PDF pages).
- JSON always includes both `page` (1-based) and `page_index` (0-based).
- Mixed conventions are rejected (e.g. page `0` is an error).

## Output contract

Records go to **stdout**; diagnostics to **stderr**.

```json
{
  "schema": "lege-pdf.agent/v1",
  "document": "/abs/path.pdf",
  "page": 1,
  "page_index": 0,
  "status": "ok",
  "warnings": [],
  "data": {}
}
```

| Flag | Behavior |
|---|---|
| (default) | Human-readable |
| `--json` | One pretty JSON envelope (multi-page text/render may still stream JSONL when multiple pages) |
| `--jsonl` | One JSON object per line (per page) |

Safe defaults: `--max-pages 50`, `--max-items 10000`, `--max-bytes 8388608`,
`--timeout 60`.

Exit codes: `0` success (page-local failures are records unless `--fail-fast`),
`1` document/command failure, `2` bad CLI.

## Commands

```text
lege-pdf inspect FILE [--pages RANGE] [--json|--jsonl]
lege-pdf text FILE [--pages RANGE] [--layout plain|blocks|words|chars]
                   [--ocr never|auto|always] [--ocr-language eng]
                   [--bbox X0,Y0,X1,Y1] [--json|--jsonl]
lege-pdf images FILE [--pages RANGE] [--mode inventory|source|decoded|rendered]
                     [--extract DIR] [--json|--jsonl]
lege-pdf content FILE --page N [--ops] [--resources] [--objects] [--json]
lege-pdf render FILE --pages RANGE --output TEMPLATE
                     [--dpi N|--scale N] [--format png|ppm] [--thumbnail]
lege-pdf search FILE QUERY [--pages RANGE] [--context N]
                          [--ocr never|auto|always] [--ocr-language eng] [--jsonl]
lege-pdf print FILE [--printer NAME] [--list-printers] [--pages RANGE]
                    [--paper a4|letter|210x297mm|...] [--orientation portrait|landscape|auto]
                    [--margin PT|--margin-mm MM|--margin-in IN]
                    [--scaling actual|fit|shrink|fill|NN%]
                    [--n-up 1|2|4|6|9|16|booklet] [--n-up-border]
                    [--n-up-order right-down|left-down|down-right|down-left]
                    [--duplex none|long|short] [--copies N] [--no-collate] [--reverse]
                    [--source-box crop|media|trim|bleed|art] [--gray] [--dpi N]
                    [--to-file DIR] [--dry-run [--query-device]]
lege-pdf serve --stdio [--max-open N] [--idle-timeout SECONDS]
lege-pdf mcp [--max-open N] [--idle-timeout SECONDS]
```

Output templates for render support `{page}`, `{page_index}`, and `{stem}`.

`--layout blocks` is a **heuristic** (words → lines → blocks), not claimed
`pdftotext -layout` parity. Its `text` field follows the reconstructed block
order. Prefer `words` / `chars` for native geometry.

OCR defaults to `never`. `auto` retains trustworthy native text and OCRs only
scanned or low-trust pages; `always` renders and OCRs every selected page.
Every text/search page reports `provenance.source`, `page_content_kind`, native
trust evidence, and the OCR engine/language when applicable. OCR currently
supports plain, word, and block output; character-level OCR geometry fails
explicitly instead of pretending to be precise.

Image extract modes are exclusive: inventory metadata vs encoded source vs
decoded samples vs a limited rendered appearance (RGB samples only).

## Printing

`lege-pdf print` drives `lege-pdf-print`: office printing — paper, margins,
scaling, N-up, booklet, duplex, copies, page ranges. Prepress (CMYK,
separations, PDF/X) is deliberately out of scope; see `lege-pdf/print/PLAN.md`
§7.

A job takes one of two routes, and the record says which:

| `route` | when | what is spooled |
|---|---|---|
| `pass_through` | nothing changes page geometry **and** the queue accepts `application/pdf` | the original bytes, untouched |
| `composed` | any N-up, booklet, margin, non-crop source box, or geometry-changing scale — or a queue that only takes bitmaps (Windows always) | sheets we impose and rasterize at `--dpi` |

Modes, in order of how safe they are to run unattended:

- `--dry-run` reports the whole plan as JSON — route, sheet count, and every
  placement's source page, scale, rotation, translation, transform and cell —
  and **contacts no spooler at all**. It plans against an assumed device
  (quarter-inch hardware margins; PDF pass-through exactly where the platform
  has it), and says so via `device.source = "assumed"`. Pass `--query-device`
  to ask the real queue instead.
- `--to-file DIR` spools through the `file` backend, writing
  `document-NNNN.pdf` (pass-through) or `sheet-NNNN.png` (composed) into `DIR`.
  No real printer is touched.
- Neither flag spools for real, to `--printer` or the system default.

`--list-printers` enumerates queues (`backend`, `printers`, `default`) and
exits; it accepts `--to-file DIR` to enumerate the file backend instead of the
platform one, and needs no `FILE` argument.

Counting: `sheet_count` is printed **sides** for one copy (the length of
`sheets`), `paper_sheets` is physical sheets for one copy (duplex puts two
sides on one), and `total_sides` multiplies by `--copies`. Imposition emits a
single copy — `copies_applied_by` is `spooler`, because CUPS and winspool both
take a native copy count and multiplying in both places would print copies²
pages.

`--scaling` takes an explicit factor only with the `%`: the underlying value is
a multiplier where `1.0` is 1:1, so a bare `50` would be ambiguous between half
size and fifty times, and is rejected.

All print lengths in the JSON are PostScript points (`unit: "points"`).
`--pages` here also accepts `odd` and `even`, and an open-ended span such as
`8-`, on top of the `1,3-5` / `all` forms the other commands take.

**`print` is deliberately not exposed as an MCP tool or a `serve` method.**
Spooling actuates hardware, which is a side effect no bounded read contract
should hand to a model; and a planning-only tool would be a second, weaker
copy of `--dry-run`. Agents should shell out to `lege-pdf print … --dry-run`
(or `--to-file`) and leave the real submission to a person. If that changes,
the shape is a single read-only `pdf_print_plan` method in
`src/commands/serve.rs` returning the same `PrintPlanData`, with no route to
`submit`.

## MCP server

`lege-pdf mcp` is a newline-delimited JSON-RPC MCP server on stdio. It exposes
`pdf_inspect`, `pdf_text`, `pdf_images`, `pdf_content`, `pdf_render`, and
`pdf_search`. Read-oriented tools are annotated as read-only; image extraction
and rendering are not. Tool results contain both a text block and
`structuredContent`.

After installing the binary, register it with Codex:

```sh
codex mcp add lege-pdf -- /home/dk/bin/lege-pdf mcp
codex mcp list
```

Equivalent `~/.codex/config.toml` configuration:

```toml
[mcp_servers.lege-pdf]
command = "/home/dk/bin/lege-pdf"
args = ["mcp"]
startup_timeout_sec = 20
tool_timeout_sec = 60
default_tools_approval_mode = "writes"
```

The server accepts the established MCP initialization versions through
`2025-11-25`, while `tools/list` and `tools/call` can also be used directly by
clients that do not require a handshake. Paths refer to the machine running
the server. Use absolute output paths for `pdf_render` and extracting
`pdf_images` modes.

The initialization instructions identify this as a first draft and narrowly
permit MuPDF only after a concrete Lege failure or deficient result. Such a
fallback is limited to the failed file/page/operation and, in a workspace with
an AKR ledger, must first log an `akr papercut` without document content,
passwords, or private absolute paths. A successful Lege result is not routinely
double-checked with MuPDF. The installed MuPDF lacks OCR support, so OCR is
never routed to it.

## Native serve protocol

`serve --stdio` is the lower-level Lege JSONL service used by the MCP adapter.
It is retained for shell integrations that do not need MCP.

Newline-delimited JSON on stdio:

```json
{"id":1,"method":"ping","params":{}}
{"id":2,"method":"pdf_inspect","params":{"path":"/doc.pdf","pages":"1-3"}}
{"id":3,"method":"pdf_text","params":{"path":"/doc.pdf","pages":"1","layout":"words"}}
{"id":4,"method":"close","params":{}}
```

Methods: `ping`, `pdf_inspect`, `pdf_text`, `pdf_images`, `pdf_content`,
`pdf_render`, `pdf_search`, `close`/`shutdown`.

Snapshots are cached by canonical path + size + mtime with LRU eviction
(`--max-open`, default 4).

## Examples

```sh
lege-pdf inspect book.pdf --json
lege-pdf text book.pdf --pages 1-5 --layout words --jsonl
lege-pdf text scans.pdf --pages 1-5 --ocr auto --layout blocks --jsonl
lege-pdf render book.pdf --pages 1 --output out-{page}.png --thumbnail --json
lege-pdf search scans.pdf "clause" --pages all --ocr auto --jsonl
lege-pdf print book.pdf --dry-run --n-up 2 --margin-mm 10 --json
lege-pdf print book.pdf --to-file /tmp/spool --pages odd --json
lege-pdf print book.pdf --printer HP_LaserJet --duplex long --copies 2
```

## Relation to `pdfr`

`pdfr` (`pdf-cli`) remains the developer driver with free-form dumps.
`lege-pdf` owns the versioned agent contract.
