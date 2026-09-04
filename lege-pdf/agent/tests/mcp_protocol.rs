//! MCP stdio protocol and end-to-end tool smoke tests.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../render/crates/pdf-chaos-tests/tests/fixtures/hello_world.pdf")
}

fn run_mcp(messages: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lege-pdf"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lege-pdf mcp");

    {
        let mut stdin = child.stdin.take().expect("MCP stdin");
        for message in messages {
            serde_json::to_writer(&mut stdin, message).expect("serialize request");
            stdin.write_all(b"\n").expect("write request delimiter");
        }
    }

    let output = child.wait_with_output().expect("wait for MCP server");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON-RPC response"))
        .collect()
}

#[test]
fn initialize_and_list_tools() {
    let responses = run_mcp(&[
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1" }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    ]);

    assert_eq!(
        responses.len(),
        2,
        "notification must not receive a response"
    );
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "lege-pdf");
    assert_eq!(
        responses[0]["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
    assert!(responses[0]["result"]["instructions"].as_str().is_some());

    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert_eq!(tools.len(), 6);
    assert_eq!(tools[0]["name"], "pdf_inspect");
    assert_eq!(tools[5]["name"], "pdf_search");
    assert_eq!(tools[0]["inputSchema"]["required"][0], "path");
    assert_eq!(
        tools[1]["inputSchema"]["properties"]["ocr"]["enum"],
        json!(["never", "auto", "always"])
    );
    assert!(
        responses[0]["result"]["instructions"]
            .as_str()
            .is_some_and(|instructions| instructions.contains("akr papercut"))
    );
}

#[test]
fn call_inspect_returns_structured_content() {
    let responses = run_mcp(&[json!({
        "jsonrpc": "2.0",
        "id": "inspect-1",
        "method": "tools/call",
        "params": {
            "name": "pdf_inspect",
            "arguments": {
                "path": fixture(),
                "pages": 1
            }
        }
    })]);

    let result = &responses[0]["result"];
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["page_count"], 1);
    assert_eq!(result["content"][0]["type"], "text");
    let text = result["content"][0]["text"].as_str().expect("text block");
    let text_value: Value = serde_json::from_str(text).expect("text mirrors JSON result");
    assert_eq!(text_value, result["structuredContent"]);
}

#[test]
fn tool_failures_are_mcp_tool_results() {
    let responses = run_mcp(&[json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "pdf_inspect",
            "arguments": { "path": "/definitely/not/a/document.pdf" }
        }
    })]);

    assert!(responses[0].get("error").is_none());
    assert_eq!(responses[0]["result"]["isError"], true);
    assert!(
        responses[0]["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|message| message.contains("document.pdf"))
    );
}

#[test]
fn invalid_tool_arguments_are_protocol_errors() {
    let responses = run_mcp(&[json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "pdf_render",
            "arguments": { "path": fixture() }
        }
    })]);

    assert_eq!(responses[0]["error"]["code"], -32602);
    assert!(
        responses[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("output"))
    );
}

#[test]
fn call_text_with_ocr_reports_provenance() {
    let responses = run_mcp(&[json!({
        "jsonrpc": "2.0",
        "id": "ocr-1",
        "method": "tools/call",
        "params": {
            "name": "pdf_text",
            "arguments": {
                "path": fixture(),
                "pages": 1,
                "ocr": "always",
                "ocr_language": "eng"
            }
        }
    })]);

    let result = &responses[0]["result"];
    assert_eq!(result["isError"], false);
    let data = &result["structuredContent"]["pages"][0]["data"];
    assert_eq!(data["provenance"]["source"], "ocr");
    // Which engine backs OCR is a platform decision — WinRT on Windows,
    // Tesseract elsewhere — so assert the reported engine is the one this
    // build actually resolves rather than hard-coding either name.
    assert_eq!(
        data["provenance"]["ocr_engine"],
        lege_ocr::engine::default_engine().name()
    );
    assert!(
        data["text"]
            .as_str()
            .is_some_and(|text| text.contains("world"))
    );
}
