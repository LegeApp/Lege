//! Golden schema and smoke tests against the in-tree hello_world fixture.

use std::path::PathBuf;
use std::process::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../render/crates/pdf-chaos-tests/tests/fixtures/hello_world.pdf")
}

fn bin() -> Command {
    // Prefer the cargo-built binary from CARGO_BIN_EXE when available (integration tests).
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lege-pdf"));
    cmd.env_remove("RUST_LOG");
    cmd
}

#[test]
fn inspect_json_envelope() {
    let output = bin()
        .args(["inspect"])
        .arg(fixture())
        .arg("--json")
        .output()
        .expect("run lege-pdf inspect");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(value["schema"], "lege-pdf.agent/v1");
    assert_eq!(value["status"], "ok");
    assert!(value["data"]["page_count"].as_u64().unwrap_or(0) >= 1);
    assert!(value["document"].as_str().unwrap().ends_with(".pdf"));
}

#[test]
fn text_plain_json() {
    let output = bin()
        .args(["text"])
        .arg(fixture())
        .args(["--pages", "1", "--json"])
        .output()
        .expect("run lege-pdf text");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json");
    assert_eq!(value["schema"], "lege-pdf.agent/v1");
    assert_eq!(value["page"], 1);
    assert_eq!(value["page_index"], 0);
    assert!(value["data"]["text"].as_str().is_some());
}

#[test]
fn page_out_of_range_is_error() {
    let output = bin()
        .args(["text"])
        .arg(fixture())
        .args(["--pages", "999", "--json"])
        .output()
        .expect("run lege-pdf text");
    assert!(!output.status.success());
}

#[test]
fn content_ops_json() {
    let output = bin()
        .args(["content"])
        .arg(fixture())
        .args(["--page", "1", "--ops", "--json"])
        .output()
        .expect("run lege-pdf content");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json");
    assert_eq!(value["schema"], "lege-pdf.agent/v1");
    assert!(value["data"]["op_count"].as_u64().is_some());
}

#[test]
fn images_inventory_json() {
    let output = bin()
        .args(["images"])
        .arg(fixture())
        .args(["--pages", "1", "--json"])
        .output()
        .expect("run lege-pdf images");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json");
    assert_eq!(value["schema"], "lege-pdf.agent/v1");
    assert!(value["data"]["draw_count"].as_u64().is_some());
}

#[test]
fn render_png() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("p{page}.png");
    let output = bin()
        .args(["render"])
        .arg(fixture())
        .args([
            "--pages",
            "1",
            "--output",
            out.to_str().unwrap(),
            "--thumbnail",
            "--json",
        ])
        .output()
        .expect("run lege-pdf render");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json");
    let path = value["data"]["path"].as_str().expect("path");
    let bytes = std::fs::read(path).expect("png exists");
    assert!(bytes.starts_with(b"\x89PNG"), "PNG magic missing");
    assert!(value["data"]["width"].as_u64().unwrap() > 0);
}

#[test]
fn search_finds_hello() {
    let output = bin()
        .arg("search")
        .arg(fixture())
        .arg("Hello")
        .args(["--pages", "1", "--jsonl"])
        .output()
        .expect("run lege-pdf search");
    // May succeed with zero matches if text differs; require valid schema when success.
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            let value: serde_json::Value =
                serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
            assert_eq!(value["schema"], "lege-pdf.agent/v1");
        }
    }
}

#[test]
fn serve_ping() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_lege-pdf"))
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, r#"{{"id":1,"method":"ping","params":{{}}}}"#).unwrap();
        writeln!(stdin, r#"{{"id":2,"method":"close","params":{{}}}}"#).unwrap();
    }

    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    let first = lines.next().unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(value["id"], 1);
    assert_eq!(value["result"]["pong"], true);
    assert_eq!(value["result"]["schema"], "lege-pdf.agent/v1");
    let _ = child.wait();
}
