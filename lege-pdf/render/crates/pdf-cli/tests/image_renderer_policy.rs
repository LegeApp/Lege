#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::process::Command;

use pdf_test_support::builder::PdfBuilder;

fn image_only_pdf() -> Vec<u8> {
    let mut pdf = PdfBuilder::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    pdf.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    pdf.add_stream(4, "", b"q 100 0 0 100 0 0 cm /Im Do Q");
    pdf.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8/ColorSpace/DeviceRGB",
        &[255, 0, 0, 0, 0, 255],
    );
    pdf.finish_classic_xref("/Root 1 0 R");
    pdf.into_bytes()
}

#[test]
fn production_render_honors_cpu_policy_and_reports_route() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("image.pdf");
    let output = directory.path().join("image.ppm");
    std::fs::write(&input, image_only_pdf()).expect("write PDF");

    let result = Command::new(env!("CARGO_BIN_EXE_pdfr"))
        .env("LEGE_PDF_IMAGE_RENDERER", "cpu")
        .args([
            "render",
            input.to_str().expect("input path"),
            "0",
            output.to_str().expect("output path"),
        ])
        .output()
        .expect("run pdfr");
    assert!(
        result.status.success(),
        "pdfr failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(output.is_file());
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("renderer: cpu"),
        "route was not reported: {}",
        String::from_utf8_lossy(&result.stdout)
    );
}

#[test]
fn production_render_rejects_an_invalid_policy() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let input = directory.path().join("image.pdf");
    let output = directory.path().join("image.ppm");
    std::fs::write(&input, image_only_pdf()).expect("write PDF");

    let result = Command::new(env!("CARGO_BIN_EXE_pdfr"))
        .env("LEGE_PDF_IMAGE_RENDERER", "definitely-not-a-renderer")
        .args([
            "render",
            input.to_str().expect("input path"),
            "0",
            output.to_str().expect("output path"),
        ])
        .output()
        .expect("run pdfr");
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("expected cpu, gpu, or auto"),
        "unexpected error: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
