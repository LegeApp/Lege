//! A page leaf that is a *direct* inline dict inside `/Kids` (rather than the
//! spec-required indirect reference) must still be counted and rendered — a
//! malformed shape PDFium tolerates (pdfjs/issue9540). Regression for the
//! `PageRef.object: Option<ObjectId>` change.

use std::sync::Arc;

use pdf_document::{DocumentLimits, DocumentSnapshot};
use pdf_source::{OwnedBytesSource, PdfSource};

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open")
}

/// `/Pages` whose sole kid is an inline `<< /Type /Page … >>` dict, not `N 0 R`.
/// A deliberately-wrong `startxref` forces the loader's rebuild path, which
/// indexes the real objects; the walk then reaches the direct-dict leaf.
#[test]
fn direct_object_page_leaf_is_counted_and_rendered() {
    let pdf = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Count 1 /Kids [ << /Type /Page /MediaBox [0 0 200 200] /Parent 2 0 R >> ] >>\nendobj\n\
trailer\n<< /Root 1 0 R /Size 3 >>\n\
startxref\n999999\n%%EOF\n"
        .to_vec();

    let snap = open(pdf);
    assert_eq!(
        snap.page_count(),
        1,
        "the direct-dict page leaf must be counted"
    );

    let page = snap
        .page(pdf_document::PageIndex(0))
        .expect("page 0 exists");
    // A direct leaf has no backing object id, but its geometry is populated.
    assert_eq!(page.object, None, "an inline page dict has no object id");
    assert_eq!(page.media_box, [0.0, 0.0, 200.0, 200.0]);
}

/// Safety gate for the zero-page rebuild escalation: a *legitimately* empty
/// page tree (clean chain, `/Count 0`, empty `/Kids` — no lost subtrees) must
/// NOT trigger a full rebuild, and must stay empty. The escalation only fires
/// when the walk dropped subtrees, so this document is left exactly as-is.
#[test]
fn genuinely_empty_page_tree_is_not_escalated() {
    use pdf_test_support::builder::PdfBuilder;
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids [] /Count 0>>");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    assert_eq!(
        snap.page_count(),
        0,
        "an empty tree must stay empty, not be rebuilt"
    );
}

/// A malformed tree can lose its only `/Kids` array while the actual page
/// dictionaries remain live in the xref. The rebuilt walk still finds zero
/// pages, so the last-resort orphan scan must recover explicit `/Type /Page`
/// leaves and inherit geometry through their `/Parent` chain.
#[test]
fn orphan_page_is_recovered_after_rebuilt_tree_remains_empty() {
    use pdf_test_support::builder::PdfBuilder;
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids null/Count 1/MediaBox[0 0 321 654]/Rotate 90>>",
    );
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R>>");
    b.add_stream(4, "", b"");
    b.finish_classic_xref("/Root 1 0 R");

    let snap = open(b.into_bytes());
    assert_eq!(snap.page_count(), 1, "the orphan page must be recovered");
    let page = snap.page(pdf_document::PageIndex(0)).expect("orphan page");
    assert_eq!(page.object, Some(pdf_object::ObjectId::new(3, 0)));
    assert_eq!(page.media_box, [0.0, 0.0, 321.0, 654.0]);
    assert_eq!(page.rotate, 90);
    assert!(
        snap.recovery_events().iter().any(|event| matches!(
            event,
            pdf_structure::RecoveryEvent::Other(note)
                if note.contains("orphan /Type/Page")
        )),
        "the heuristic recovery must be observable"
    );
}
