//! Model Context Protocol adapter over the persistent PDF service.

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::bounds::Bounds;
use crate::cache::SnapshotCache;

use super::serve;

const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";
const SERVER_INSTRUCTIONS: &str = r#"FIRST-DRAFT SERVER. Use Lege PDF tools as the primary path for local PDF work. Paths are resolved on this host. Page numbers are one-based; page_index is zero-based. Prefer pdf_inspect before expensive work and pdf_images inventory before extraction. Use explicit absolute output paths for pdf_render and image extraction. OCR is explicit: use ocr=auto for scanned/mixed documents, ocr=always only when native text must be disregarded, and inspect each page's provenance.

Do not run MuPDF merely to double-check a successful Lege result. A narrowly equivalent MuPDF command is permitted only after a concrete Lege deficiency: an MCP tool error, degraded render, unsupported requested representation, missing content that is visibly present, or materially unusable reading order. Before that fallback, if the current repository has a .akr directory, run `akr papercut -m codex "lege-pdf first-draft deficiency: tool=<tool>; file=<basename>; page=<n>; observed=<concise result>; MuPDF fallback=<exact operation>"`. Do not include document contents, passwords, or absolute private paths in the papercut. If no .akr ledger is present, report the deficiency to the user instead. Scope the fallback to the failed file/page/operation: `mutool draw -F txt` for native text ordering, `mutool draw -F png` for a failed/degraded render, `mutool extract` for failed embedded image extraction, or `mutool show`/`mutool draw -F trace` for unsupported structural inspection. MuPDF OCR is not available in the installed build; do not route OCR to it. Never overwrite the source PDF."#;

#[derive(Debug)]
pub struct McpArgs {
    pub max_open: usize,
    pub idle_timeout: u64,
    pub bounds: Bounds,
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Run the newline-delimited MCP stdio transport until the client closes stdin.
pub fn run(args: McpArgs) -> Result<i32> {
    let mut cache = SnapshotCache::new(args.max_open, args.idle_timeout);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line.context("reading MCP stdin")?;
        if line.trim().is_empty() {
            continue;
        }

        let request = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(request) => request,
            Err(err) => {
                write_message(
                    &mut stdout,
                    rpc_error(Value::Null, -32700, format!("parse error: {err}")),
                )?;
                continue;
            }
        };

        if request.jsonrpc.as_deref() != Some("2.0") {
            if let Some(id) = request.id {
                write_message(
                    &mut stdout,
                    rpc_error(id, -32600, "jsonrpc must be \"2.0\""),
                )?;
            }
            continue;
        }

        // JSON-RPC notifications never receive responses. MCP initialization and
        // cancellation notifications do not require state in this synchronous server.
        let Some(id) = request.id else {
            continue;
        };

        let response = match request.method.as_str() {
            "initialize" => rpc_result(id, initialize_result(&request.params)),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({ "tools": tools() })),
            "tools/call" => match call_tool(&mut cache, &request.params, args.bounds) {
                Ok(value) => rpc_result(id, value),
                Err((code, message)) => rpc_error(id, code, message),
            },
            _ => rpc_error(id, -32601, format!("method not found: {}", request.method)),
        };
        write_message(&mut stdout, response)?;
    }

    Ok(0)
}

fn initialize_result(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let protocol_version = requested
        .filter(|version| {
            matches!(
                *version,
                "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
            )
        })
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "lege-pdf",
            "title": "Lege PDF",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": SERVER_INSTRUCTIONS
    })
}

fn call_tool(
    cache: &mut SnapshotCache,
    params: &Value,
    bounds: Bounds,
) -> std::result::Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, "params.name must be a string".to_owned()))?;
    if !is_tool(name) {
        return Err((-32602, format!("unknown tool {name:?}")));
    }

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    validate_arguments(name, &arguments)?;

    match serve::call_method(cache, name, &arguments, bounds) {
        Ok(structured) => {
            let text = serde_json::to_string_pretty(&structured)
                .map_err(|err| (-32603, format!("serializing tool result: {err}")))?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false
            }))
        }
        Err(err) => Ok(json!({
            "content": [{ "type": "text", "text": err.to_string() }],
            "isError": true
        })),
    }
}

fn validate_arguments(name: &str, arguments: &Value) -> std::result::Result<(), (i32, String)> {
    let object = arguments
        .as_object()
        .ok_or_else(|| (-32602, "params.arguments must be an object".to_owned()))?;
    if object.get("path").and_then(Value::as_str).is_none() {
        return Err((-32602, "arguments.path must be a string".to_owned()));
    }
    if name == "pdf_search" && object.get("query").and_then(Value::as_str).is_none() {
        return Err((-32602, "arguments.query must be a string".to_owned()));
    }
    if name == "pdf_render" && object.get("output").and_then(Value::as_str).is_none() {
        return Err((-32602, "arguments.output must be a string".to_owned()));
    }
    Ok(())
}

fn is_tool(name: &str) -> bool {
    matches!(
        name,
        "pdf_inspect" | "pdf_text" | "pdf_images" | "pdf_content" | "pdf_render" | "pdf_search"
    )
}

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "pdf_inspect",
            "title": "Inspect PDF",
            "description": "Inspect document health, metadata, features, page boxes, and page compile status. Results are capped by the server's max-pages bound.",
            "inputSchema": object_schema(
                json!({
                    "path": path_schema(),
                    "pages": pages_schema(),
                    "password": password_schema()
                }),
                &["path"]
            ),
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "pdf_text",
            "title": "Extract PDF text",
            "description": "Extract bounded native or OCR text from one or more one-based pages, with explicit provenance and optional block, word, or character geometry. OCR character geometry is not yet available.",
            "inputSchema": object_schema(
                json!({
                    "path": path_schema(),
                    "pages": pages_schema(),
                    "layout": { "type": "string", "enum": ["plain", "blocks", "words", "chars"], "default": "plain" },
                    "bbox": { "type": "string", "description": "Optional PDF-space rectangle X0,Y0,X1,Y1." },
                    "ocr": ocr_schema(),
                    "ocr_language": ocr_language_schema(),
                    "password": password_schema()
                }),
                &["path"]
            ),
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "pdf_images",
            "title": "Inspect or extract PDF images",
            "description": "Inventory images on one page, or extract source, decoded, or rendered image data. Extraction modes write beneath the explicit extract directory.",
            "inputSchema": object_schema(
                json!({
                    "path": path_schema(),
                    "page": page_schema(),
                    "mode": { "type": "string", "enum": ["inventory", "source", "decoded", "rendered"], "default": "inventory" },
                    "extract": { "type": "string", "description": "Output directory required by extraction modes; prefer an absolute path." },
                    "password": password_schema()
                }),
                &["path"]
            ),
            "annotations": { "readOnlyHint": false, "openWorldHint": false }
        }),
        json!({
            "name": "pdf_content",
            "title": "Inspect PDF page content",
            "description": "Return bounded semantic operations and optional resource or object views for one page.",
            "inputSchema": object_schema(
                json!({
                    "path": path_schema(),
                    "page": page_schema(),
                    "ops": { "type": "boolean", "default": true },
                    "resources": { "type": "boolean", "default": false },
                    "objects": { "type": "boolean", "default": false },
                    "password": password_schema()
                }),
                &["path"]
            ),
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "pdf_render",
            "title": "Render PDF page",
            "description": "Render one PDF page to a PNG or PPM file. This writes the explicit output path and returns a manifest, not inline image bytes.",
            "inputSchema": object_schema(
                json!({
                    "path": path_schema(),
                    "page": page_schema(),
                    "output": { "type": "string", "description": "Output path or template; prefer an absolute path. Supports {page}, {page_index}, and {stem}." },
                    "dpi": { "type": "number", "exclusiveMinimum": 0 },
                    "scale": { "type": "number", "exclusiveMinimum": 0 },
                    "format": { "type": "string", "enum": ["png", "ppm"], "default": "png" },
                    "crop": { "type": "string", "description": "Optional PDF-space rectangle X0,Y0,X1,Y1." },
                    "thumbnail": { "type": "boolean", "default": false },
                    "password": password_schema()
                }),
                &["path", "output"]
            ),
            "annotations": { "readOnlyHint": false, "openWorldHint": false }
        }),
        json!({
            "name": "pdf_search",
            "title": "Search PDF text",
            "description": "Search bounded native or OCR page text and return per-page matches with context, geometry, and text provenance.",
            "inputSchema": object_schema(
                json!({
                    "path": path_schema(),
                    "query": { "type": "string", "minLength": 1 },
                    "pages": pages_schema(),
                    "context": { "type": "integer", "minimum": 0, "default": 32 },
                    "case_insensitive": { "type": "boolean", "default": true },
                    "ocr": ocr_schema(),
                    "ocr_language": ocr_language_schema(),
                    "password": password_schema()
                }),
                &["path", "query"]
            ),
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        }),
    ]
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn path_schema() -> Value {
    json!({
        "type": "string",
        "description": "PDF path visible to the local server process; prefer an absolute path."
    })
}

fn pages_schema() -> Value {
    json!({
        "description": "One-based page selection: a single integer, or a string such as 1, 1-3, 1,4-6, or all.",
        "oneOf": [
            { "type": "integer", "minimum": 1 },
            { "type": "string" }
        ]
    })
}

fn page_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "default": 1,
        "description": "One-based page number."
    })
}

fn ocr_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["never", "auto", "always"],
        "default": "never",
        "description": "OCR policy. auto OCRs only pages whose native text fails conservative trust checks; always disregards native text for output."
    })
}

fn ocr_language_schema() -> Value {
    json!({
        "type": "string",
        "default": "eng",
        "description": "Language code for the compiled Lege OCR backend, for example eng."
    })
}

fn password_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional document password. It is passed to the parser and never returned."
    })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn write_message(out: &mut impl Write, value: Value) -> Result<()> {
    serde_json::to_writer(&mut *out, &value)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
