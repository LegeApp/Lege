# lege-pdf (agent tool)

Structured, agent-facing CLI and local MCP server over Lege's native PDF engine (`pdf-*` crates).
Binary name: **`lege-pdf`**. Package: **`lege-pdf-agent`**.

The MCP integration is a **first draft**: its bounded tool contract is usable,
but complex layout reconstruction, some malformed/rare PDF features, and OCR
character-level geometry are not yet production-complete.

This is not a MuPDF/Poppler replacement for repair, rewrite, or conversion.
It covers the commands agents actually use: inspect, text, images, content,
render, search, and a persistent stdio service.

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
```

## Relation to `pdfr`

`pdfr` (`pdf-cli`) remains the developer driver with free-form dumps.
`lege-pdf` owns the versioned agent contract.
