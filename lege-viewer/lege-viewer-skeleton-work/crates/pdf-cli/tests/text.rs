#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use pdf_test_support::builder::PdfBuilder;

fn fixture() -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    builder.add_stream(4, "", b"BT /F1 12 Tf 10 20 Td (hello world) Tj ET");
    builder.add_object(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
    );
    builder.finish_classic_xref("/Root 1 0 R");
    builder.into_bytes()
}

#[test]
fn text_command_prints_plain_text_and_word_geometry() {
    let path = std::env::temp_dir().join(format!(
        "pdfr-text-{}-{}.pdf",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, fixture()).expect("write fixture");

    let plain = std::process::Command::new(env!("CARGO_BIN_EXE_pdfr"))
        .args(["text", path.to_str().unwrap(), "0"])
        .output()
        .expect("run text");
    assert!(plain.status.success(), "{plain:?}");
    assert_eq!(String::from_utf8_lossy(&plain.stdout), "hello world");

    let words = std::process::Command::new(env!("CARGO_BIN_EXE_pdfr"))
        .args(["text", path.to_str().unwrap(), "0", "--words"])
        .output()
        .expect("run words");
    assert!(words.status.success(), "{words:?}");
    let output = String::from_utf8_lossy(&words.stdout);
    assert!(output.contains("text=\"hello\""), "{output}");
    assert!(output.contains("text=\"world\""), "{output}");

    let _ = std::fs::remove_file(path);
}
