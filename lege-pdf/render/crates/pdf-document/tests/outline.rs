use std::sync::Arc;

use pdf_document::{DestinationFit, DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open fixture")
}

#[test]
fn outline_resolves_direct_named_and_action_destinations() {
    let mut builder = PdfBuilder::new();
    builder.add_object(
        1,
        "<</Type/Catalog/Pages 2 0 R/Outlines 5 0 R\
         /Dests<</Legacy [3 0 R/FitH 640]>>\
         /Names<</Dests 9 0 R>>>>",
    );
    builder.add_object(2, "<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>");
    builder.add_object(3, "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>");
    builder.add_object(4, "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>");
    builder.add_object(5, "<</Type/Outlines/First 6 0 R/Last 8 0 R/Count 3>>");
    builder.add_object(
        6,
        "<</Title(Direct)/Parent 5 0 R/Dest[3 0 R/XYZ 12 700 1.5]\
         /First 7 0 R/Last 7 0 R/Next 8 0 R/Count 1>>",
    );
    builder.add_object(
        7,
        "<</Title<FEFF004E0061006D00650064>/Parent 6 0 R/Dest/TreeTarget>>",
    );
    builder.add_object(8, "<</Title(Action)/Parent 5 0 R/A<</S/GoTo/D(Legacy)>>>>");
    builder.add_object(9, "<</Names[(TreeTarget) [4 0 R/Fit]]>>");
    builder.finish_classic_xref("/Root 1 0 R");

    let snapshot = open(builder.into_bytes());
    let outline = snapshot.outline(&mut ParseContext::new());

    assert!(outline.issues.is_empty(), "{:?}", outline.issues);
    assert_eq!(outline.items.len(), 3);
    assert_eq!(&*outline.items[0].title, "Direct");
    assert_eq!(outline.items[0].depth, 0);
    assert!(outline.items[0].initially_open);
    assert_eq!(
        outline.items[0].destination,
        Some(pdf_document::DocumentDestination {
            page: PageIndex(0),
            fit: DestinationFit::Xyz,
            left: Some(12.0),
            top: Some(700.0),
            right: None,
            bottom: None,
            zoom: Some(1.5),
        })
    );

    assert_eq!(&*outline.items[1].title, "Named");
    assert_eq!(outline.items[1].depth, 1);
    assert_eq!(
        outline.items[1].destination.expect("named target").page,
        PageIndex(1)
    );
    assert_eq!(
        outline.items[1].destination.expect("named target").fit,
        DestinationFit::Fit
    );

    assert_eq!(&*outline.items[2].title, "Action");
    assert_eq!(outline.items[2].depth, 0);
    assert_eq!(
        outline.items[2].destination.expect("action target").fit,
        DestinationFit::FitHorizontal
    );
    assert_eq!(
        outline.items[2].destination.expect("action target").top,
        Some(640.0)
    );
}
