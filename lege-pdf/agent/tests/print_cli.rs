//! `lege-pdf print` CLI shape, against the in-tree hello_world fixture.
//!
//! Everything here is `--dry-run` or `--to-file`: no test may touch a real
//! print queue.

use std::path::PathBuf;
use std::process::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../render/crates/pdf-chaos-tests/tests/fixtures/hello_world.pdf")
}

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lege-pdf"));
    cmd.env_remove("RUST_LOG");
    cmd
}

fn json(args: &[&str]) -> serde_json::Value {
    let output = bin()
        .arg("print")
        .arg(fixture())
        .args(args)
        .arg("--json")
        .output()
        .expect("run lege-pdf print");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json")
}

/// Whether the platform spools PDF natively — the same rule `--dry-run` uses
/// for its assumed device.
const NATIVE_PDF_SPOOL: bool = cfg!(any(target_os = "linux", target_os = "macos"));

#[test]
fn dry_run_reports_the_route_without_spooling() {
    let value = json(&["--dry-run"]);
    assert_eq!(value["schema"], "lege-pdf.agent/v1");
    assert_eq!(value["status"], "ok");
    assert!(value["document"].as_str().unwrap().ends_with(".pdf"));

    let data = &value["data"];
    assert_eq!(data["unit"], "points");
    assert_eq!(data["dry_run"], true);
    assert_eq!(data["page_count"], 1);
    assert_eq!(data["selected_pages"], serde_json::json!([1]));
    assert_eq!(data["device"]["source"], "assumed");
    assert_eq!(data["device"]["accepts_pdf"], NATIVE_PDF_SPOOL);
    assert_eq!(data["options"]["paper"]["name"], "a4");
    assert_eq!(data["options"]["scaling"], "shrink");
    assert_eq!(data["options"]["n_up"], "1");
    assert!(data["compose"]["dpi"].as_f64().unwrap() > 0.0);

    if NATIVE_PDF_SPOOL {
        // A plain 1-up job on a queue that takes PDF needs no composition, so
        // there is no imposition plan to report.
        assert_eq!(data["route"], "pass_through");
        assert_eq!(data["sheets"], serde_json::json!([]));
        assert!(data["sheet_count"].is_null());
    } else {
        assert_eq!(data["route"], "composed");
    }
}

#[test]
fn dry_run_n_up_reports_every_placement() {
    let data = json(&["--dry-run", "--n-up", "2", "--margin-mm", "10"])["data"].clone();
    assert_eq!(data["route"], "composed");
    assert_eq!(data["options"]["n_up"], "2");
    assert_eq!(data["sheet_count"], 1);
    assert_eq!(data["paper_sheets"], 1);
    assert_eq!(data["total_sides"], 1);
    assert_eq!(data["copies_applied_by"], "spooler");

    let sheets = data["sheets"].as_array().expect("sheets");
    assert_eq!(sheets.len(), 1);
    let sheet = &sheets[0];
    assert_eq!(sheet["index"], 0);
    assert_eq!(sheet["side"], "front");
    assert_eq!(sheet["landscape"], false);
    assert!(sheet["raster"]["width"].as_u64().unwrap() > 0);
    assert_eq!(sheet["raster"]["channels"], 3);
    for key in ["bounds", "imageable"] {
        for edge in ["x0", "y0", "x1", "y1"] {
            assert!(sheet[key][edge].is_number(), "{key}.{edge}");
        }
    }
    // The imageable area is inset by the greater of the user and hardware
    // margins, so it is strictly inside the sheet.
    assert!(sheet["imageable"]["x0"].as_f64().unwrap() > sheet["bounds"]["x0"].as_f64().unwrap());

    let placements = sheet["placements"].as_array().expect("placements");
    assert_eq!(placements.len(), 1, "one source page fills one of two cells");
    let placement = &placements[0];
    assert_eq!(placement["source_page"], 1);
    assert_eq!(placement["source_page_index"], 0);
    assert!(placement["scale_x"].as_f64().unwrap() > 0.0);
    assert!(placement["scale_y"].as_f64().unwrap() > 0.0);
    assert_eq!(placement["translate"].as_array().unwrap().len(), 2);
    for key in ["a", "b", "c", "d", "e", "f"] {
        assert!(placement["transform"][key].is_number(), "transform.{key}");
    }
    for key in ["cell", "content", "painted"] {
        for edge in ["x0", "y0", "x1", "y1"] {
            assert!(placement[key][edge].is_number(), "{key}.{edge}");
        }
    }
    // A shrink-to-fit page never overflows its cell, so nothing is clipped.
    assert_eq!(placement["content"], placement["painted"]);
}

#[test]
fn duplex_counts_sides_and_sheets_of_paper_separately() {
    let data = json(&[
        "--dry-run",
        "--n-up",
        "2",
        "--duplex",
        "long",
        "--copies",
        "3",
    ])["data"]
        .clone();
    // One source page, two-up, duplex: one side carries it, the back is
    // blank, and three copies triple the sides the printer will produce.
    assert_eq!(data["sheet_count"], 2);
    assert_eq!(data["paper_sheets"], 1);
    assert_eq!(data["total_sides"], 6);
    assert_eq!(data["copies_applied_by"], "spooler");
    assert_eq!(data["sheets"].as_array().unwrap().len(), 2);
    assert_eq!(data["sheets"][1]["side"], "back");
}

#[test]
fn dry_run_echoes_the_options_it_parsed() {
    let data = json(&[
        "--dry-run",
        "--paper",
        "letter",
        "--orientation",
        "landscape",
        "--scaling",
        "50%",
        "--duplex",
        "long",
        "--copies",
        "3",
        "--no-collate",
        "--reverse",
        "--source-box",
        "trim",
        "--gray",
        "--dpi",
        "600",
    ])["data"]
        .clone();
    let options = &data["options"];
    assert_eq!(options["paper"]["name"], "letter");
    assert_eq!(options["paper"]["width_pt"], 612.0);
    assert_eq!(options["orientation"], "landscape");
    assert_eq!(options["scaling"], "50%");
    assert_eq!(options["duplex"], "long");
    assert_eq!(options["copies"], 3);
    assert_eq!(options["collate"], false);
    assert_eq!(options["reverse"], true);
    assert_eq!(options["source_box"], "trim");
    assert_eq!(options["grayscale"], true);
    assert_eq!(data["compose"]["dpi"], 600.0);
    assert_eq!(data["compose"]["grayscale"], true);
    assert_eq!(data["sheets"][0]["raster"]["channels"], 1);
    // A non-crop source box and a scale factor both change page geometry.
    assert_eq!(data["route"], "composed");
}

#[test]
fn to_file_spools_through_the_file_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = bin()
        .arg("print")
        .arg(fixture())
        .args(["--to-file", &dir.path().display().to_string(), "--json"])
        .output()
        .expect("run lege-pdf print --to-file");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json");
    let data = &value["data"];
    assert_eq!(data["dry_run"], false);
    assert_eq!(data["backend"], "file");
    assert_eq!(data["route"], "pass_through");
    assert_eq!(data["printer"], "file");
    assert!(data["job_id"].as_str().unwrap().starts_with("file-"));
    assert_eq!(data["spooled_to"], dir.path().display().to_string());

    let written: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read spool dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(written, vec!["document-0001.pdf".to_owned()]);
}

#[test]
fn list_printers_enumerates_the_file_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = bin()
        .args(["print", "--list-printers", "--to-file"])
        .arg(dir.path())
        .arg("--json")
        .output()
        .expect("run lege-pdf print --list-printers");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json");
    let data = &value["data"];
    assert_eq!(data["backend"], "file");
    assert_eq!(data["default"], "file");
    let printers = data["printers"].as_array().expect("printers");
    assert_eq!(printers.len(), 1);
    assert_eq!(printers[0]["id"], "file");
    assert_eq!(printers[0]["is_default"], true);
}

#[test]
fn bad_options_fail_the_command() {
    for args in [
        vec!["--scaling", "bigger"],
        vec!["--scaling", "50"],
        vec!["--n-up", "3"],
        vec!["--paper", "nonsense"],
        vec!["--copies", "0"],
        vec!["--pages", "9-12"],
    ] {
        let output = bin()
            .arg("print")
            .arg(fixture())
            .args(&args)
            .args(["--dry-run", "--json"])
            .output()
            .expect("run lege-pdf print");
        assert!(!output.status.success(), "{args:?} should have failed");
    }
}
