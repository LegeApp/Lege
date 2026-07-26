#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Doctor (`pdf_read::examine`) integration tests over corpus-style
//! fixtures: clean, recovered, encrypted, and unopenable documents.

use std::sync::Arc;

use pdf_document::DocumentLimits;
use pdf_read::{CompileStatus, OpenOutcome, examine};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::{self, PdfBuilder};

fn examine_bytes(bytes: Vec<u8>) -> pdf_read::DocumentReport {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    examine(source, DocumentLimits::default())
}

#[test]
fn clean_document_reports_ok() {
    let report = examine_bytes(builder::multipage_classic(2));
    assert!(
        matches!(report.open, OpenOutcome::Ok),
        "open: {:?}",
        report.open
    );
    assert!(!report.xref.recovery_used);
    assert!(!report.xref.rebuilt);
    assert_eq!(report.xref.revision_count, 1);
    assert!(report.encryption.is_none());
    assert_eq!(report.pages.len(), 2);
    for page in &report.pages {
        assert!(
            page.media_box_present,
            "page {} inherits /MediaBox",
            page.index
        );
        assert!(
            matches!(page.compile, CompileStatus::Ok { .. }),
            "page {}: {:?}",
            page.index,
            page.compile
        );
    }
    assert!(!report.features.uses_object_streams);
    assert_eq!(report.annotations.total(), 0);
    // Summary renders and mentions the essentials.
    let summary = report.summary();
    assert!(summary.contains("open: ok"), "{summary}");
    assert!(summary.contains("pages: 2"), "{summary}");
}

#[test]
fn object_streams_are_detected() {
    let report = examine_bytes(builder::xref_stream_fixture(1));
    assert!(
        matches!(report.open, OpenOutcome::Ok),
        "open: {:?}",
        report.open
    );
    assert!(report.features.uses_object_streams);
}

#[test]
fn recovered_document_reports_recovery_used() {
    let report = examine_bytes(builder::without_startxref(builder::multipage_classic(2)));
    match &report.open {
        OpenOutcome::Recovered { how } => {
            assert!(!how.is_empty(), "recovery must be described");
        }
        other => panic!("expected Recovered, got {other:?}"),
    }
    assert!(report.xref.recovery_used);
    assert_eq!(report.pages.len(), 2);
    let summary = report.summary();
    assert!(summary.contains("recovered"), "{summary}");
}

#[test]
fn rebuilt_document_reports_rebuild() {
    let report = examine_bytes(builder::without_xref(builder::multipage_classic(1)));
    assert!(report.xref.recovery_used);
    assert!(report.xref.rebuilt, "full scan rebuild must be flagged");
    assert!(
        matches!(report.open, OpenOutcome::Recovered { .. }),
        "open: {:?}",
        report.open
    );
}

#[test]
fn rc4_encrypted_document_reports_scheme() {
    let report = examine_bytes(builder::encrypted_fixture());
    assert!(
        matches!(report.open, OpenOutcome::Ok),
        "open: {:?}",
        report.open
    );
    let enc = report.encryption.expect("encryption info present");
    assert_eq!(enc.version, 2);
    assert_eq!(enc.revision, 3);
    assert_eq!(enc.method, "RC4-128");
    assert!(
        enc.user_password_empty,
        "opened via the empty user password"
    );
}

#[test]
fn aes256_fixture_fails_open_but_still_reports_encryption() {
    // The bare-U aes256 fixture is declined by the handler; open fails, yet
    // the doctor's structural fallback still names the declared scheme.
    let report = examine_bytes(builder::aes256_encrypted_fixture());
    assert!(
        matches!(report.open, OpenOutcome::Failed { .. }),
        "open: {:?}",
        report.open
    );
    let enc = report.encryption.expect("declared scheme still reported");
    assert_eq!(enc.version, 5);
    assert_eq!(enc.revision, 6);
    assert_eq!(enc.method, "AES-256");
    assert!(!enc.user_password_empty);
    // Structural layer itself is healthy.
    assert_eq!(report.xref.revision_count, 1);
    assert!(!report.xref.recovery_used);
}

#[test]
fn garbage_reports_failed_and_nothing_else() {
    let report = examine_bytes(b"this is not a pdf at all".to_vec());
    assert!(matches!(report.open, OpenOutcome::Failed { .. }));
    assert!(report.pages.is_empty());
    assert!(report.encryption.is_none());
    let summary = report.summary();
    assert!(summary.contains("FAILED"), "{summary}");
}

/// Fixture with annotations, an AcroForm (+XFA), outlines, optional
/// content, and document JavaScript.
fn featureful_fixture() -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(
        1,
        "<</Type/Catalog/Pages 2 0 R/AcroForm 7 0 R/Outlines 8 0 R\
         /OCProperties<</OCGs[]/D<<>>>>/Names<</JavaScript 9 0 R>>>>",
    );
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Annots[5 0 R 6 0 R 10 0 R]>>",
    );
    b.add_stream(4, "", b"0 0 10 10 re f");
    b.add_object(5, "<</Type/Annot/Subtype/Link/Rect[0 0 10 10]>>");
    b.add_object(6, "<</Type/Annot/Subtype/Widget/Rect[0 0 10 10]>>");
    b.add_object(10, "<</Type/Annot/Subtype/Link/Rect[20 20 30 30]>>");
    b.add_object(7, "<</Fields[]/XFA(stub)>>");
    b.add_object(8, "<</Type/Outlines/Count 0>>");
    b.add_object(9, "<</Names[(doc-open)11 0 R]>>");
    b.add_object(11, "<</S/JavaScript/JS(app.alert\\(1\\))>>");
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

#[test]
fn features_and_annotations_are_inventoried() {
    let report = examine_bytes(featureful_fixture());
    assert!(
        matches!(report.open, OpenOutcome::Ok),
        "open: {:?}",
        report.open
    );
    let f = &report.features;
    assert!(f.has_acroform);
    assert!(f.has_xfa);
    assert!(f.has_javascript);
    assert!(f.has_outlines);
    assert!(f.has_optional_content);
    assert!(!f.uses_object_streams);

    assert_eq!(report.annotations.total(), 3);
    assert_eq!(report.annotations.count_per_subtype.get("Link"), Some(&2));
    assert_eq!(report.annotations.count_per_subtype.get("Widget"), Some(&1));

    // media box present directly on the pages node (inherited).
    assert!(report.pages[0].media_box_present);
    match &report.pages[0].compile {
        CompileStatus::Ok { op_count } => assert!(*op_count > 0),
        other => panic!("expected Ok compile, got {other:?}"),
    }

    let summary = report.summary();
    assert!(summary.contains("Link: 2"), "{summary}");
    assert!(summary.contains("javascript=true"), "{summary}");
}

#[test]
fn wrong_stream_length_surfaces_as_degraded_or_recovered() {
    // The lazy /Length repair happens during content gathering; the doctor
    // must surface it (either as an open-time recovery or per-page
    // degradation) — never silently.
    let report = examine_bytes(builder::wrong_length_fixture());
    let page_degraded = matches!(&report.pages[0].compile, CompileStatus::Degraded { .. });
    let recovered = report.xref.recovery_used;
    assert!(
        page_degraded || recovered,
        "repair must be visible: page={:?} open={:?}",
        report.pages[0].compile,
        report.open
    );
}
