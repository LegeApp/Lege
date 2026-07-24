#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Annotations render by default through the CLI: `pdfr render` with no
//! special flags must paint a page annotation's `/AP /N` appearance stream
//! (AnnotationMode::StaticAppearances is the default).

use pdf_test_support::builder::PdfBuilder;

/// One-page document whose only ink is a red square annotation appearance.
fn annotated_pdf() -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Annots[10 0 R]>>");
    b.add_stream(4, "", b"");
    b.add_object(10, "<</Type/Annot/Subtype/Square/Rect[20 20 80 80]/AP<</N 11 0 R>>>>");
    b.add_stream(11, "/Type/XObject/Subtype/Form/BBox[0 0 10 10]", b"1 0 0 rg 0 0 10 10 re f");
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

#[test]
fn cli_render_paints_annotations_without_flags() {
    let dir = std::env::temp_dir().join("pdfr-annot-default-test");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let pdf = dir.join("annot.pdf");
    let ppm = dir.join("annot.ppm");
    std::fs::write(&pdf, annotated_pdf()).expect("write pdf");

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_pdfr"))
        .args(["render", pdf.to_str().expect("path"), "0", ppm.to_str().expect("path")])
        .status()
        .expect("run pdfr");
    assert!(status.success(), "pdfr render failed: {status:?}");

    let out = std::fs::read(&ppm).expect("read ppm");
    // P6 header: skip three whitespace-delimited fields after magic.
    let body_start = {
        let mut fields = 0;
        let mut i = 2; // skip "P6"
        while fields < 3 {
            while out[i].is_ascii_whitespace() {
                i += 1;
            }
            while !out[i].is_ascii_whitespace() {
                i += 1;
            }
            fields += 1;
        }
        i + 1
    };
    // The annotation appearance is the only content; a default render must
    // show red pixels (R high, G/B low) somewhere.
    let red = out[body_start..]
        .chunks_exact(3)
        .filter(|px| px[0] > 200 && px[1] < 80 && px[2] < 80)
        .count();
    assert!(red > 100, "expected red annotation ink in default render, found {red} red pixels");
}
