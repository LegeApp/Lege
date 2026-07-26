#![allow(clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use pdf_document::{
    DestinationFit, DocumentLimits, DocumentLinkTarget, DocumentSnapshot, PageIndex, ParseContext,
};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open link fixture")
}

#[test]
fn links_extract_direct_named_and_uri_targets_once_per_document() {
    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R/Names<</Dests 20 0 R>>>>");
    builder.add_object(2, "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>");
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]\
         /Annots[10 0 R 11 0 R 12 0 R 13 0 R 14 0 R]>>",
    );
    builder.add_object(4, "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>");
    builder.add_object(
        10,
        "<</Type/Annot/Subtype/Link/Rect[10 20 110 40]/Dest[4 0 R/XYZ 15 700 1]>>",
    );
    builder.add_object(
        11,
        "<</Type/Annot/Subtype/Link/Rect[10 50 110 70]/A<</S/GoTo/D(ChapterTwo)>>>>",
    );
    builder.add_object(
        12,
        "<</Type/Annot/Subtype/Link/Rect[10 80 110 100]\
         /A<</S/URI/URI(https://example.com/read)>>>>",
    );
    builder.add_object(
        13,
        "<</Type/Annot/Subtype/Link/Rect[10 110 110 130]/F 32\
         /A<</S/URI/URI(https://hidden.invalid/)>>>>",
    );
    builder.add_object(
        14,
        "<</Type/Annot/Subtype/Link/Rect[0 0 0 0]\
         /A<</S/URI/URI(https://degenerate.invalid/)>>>>",
    );
    builder.add_object(20, "<</Names[(ChapterTwo) [4 0 R/FitH 640]]>>");
    builder.finish_classic_xref("/Root 1 0 R");

    let snapshot = open(builder.into_bytes());
    let links = snapshot.links(&mut ParseContext::new());

    assert_eq!(links.pages.len(), 2);
    assert_eq!(links.pages[0].len(), 3);
    assert!(links.pages[1].is_empty());
    assert_eq!(links.pages[0][0].rect, [10.0, 20.0, 110.0, 40.0]);
    match links.pages[0][0].target {
        DocumentLinkTarget::Internal(destination) => {
            assert_eq!(destination.page, PageIndex(1));
            assert_eq!(destination.fit, DestinationFit::Xyz);
            assert_eq!(destination.left, Some(15.0));
            assert_eq!(destination.top, Some(700.0));
        }
        DocumentLinkTarget::Uri(_) => panic!("expected direct internal target"),
    }
    match links.pages[0][1].target {
        DocumentLinkTarget::Internal(destination) => {
            assert_eq!(destination.page, PageIndex(1));
            assert_eq!(destination.fit, DestinationFit::FitHorizontal);
            assert_eq!(destination.top, Some(640.0));
        }
        DocumentLinkTarget::Uri(_) => panic!("expected named internal target"),
    }
    assert_eq!(
        links.pages[0][2].target,
        DocumentLinkTarget::Uri(Arc::from("https://example.com/read"))
    );
}

#[test]
fn unsupported_actions_are_ignored_without_losing_usable_siblings() {
    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>");
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Annots[10 0 R 11 0 R]>>",
    );
    builder.add_object(
        10,
        "<</Type/Annot/Subtype/Link/Rect[10 10 20 20]/A<</S/Launch/F(file.exe)>>>>",
    );
    builder.add_object(
        11,
        "<</Type/Annot/Subtype/Link/Rect[30 10 50 20]/A<</S/URI/URI(mailto:test@example.com)>>>>",
    );
    builder.finish_classic_xref("/Root 1 0 R");

    let links = open(builder.into_bytes()).links(&mut ParseContext::new());
    assert_eq!(links.pages[0].len(), 1);
    assert_eq!(
        links.pages[0][0].target,
        DocumentLinkTarget::Uri(Arc::from("mailto:test@example.com"))
    );
}
