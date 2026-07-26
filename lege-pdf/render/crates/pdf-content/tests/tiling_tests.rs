#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6B: tiling patterns (PatternType 1) compile their cell content into
//! a nested CompiledPage and resolve to `Paint::Pattern`.

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{CompiledPage, DisplayOp, PageFeatures, Paint, PaintOrigin};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

fn compile(snapshot: &DocumentSnapshot, page: u32) -> CompiledPage {
    let mut ctx = ParseContext::new();
    PageCompiler::new()
        .compile(snapshot, PageIndex(page), &mut ctx)
        .expect("compile failed")
}

#[test]
fn tiling_pattern_compiles_cell_and_resolves_paint() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R\
         /Resources<</Pattern<</P1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Pattern cs /P1 scn 0 0 100 100 re f");
    // Colored tiling pattern; the cell fills a red square.
    b.add_stream(
        5,
        "/Type/Pattern/PatternType 1/PaintType 1/TilingType 1\
         /BBox[0 0 10 10]/XStep 10/YStep 10/Resources<<>>",
        b"1 0 0 rg 0 0 5 5 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    assert!(page.features.contains(PageFeatures::PATTERNS));
    assert_eq!(page.tilings.len(), 1);
    let tiling = &page.tilings[0];
    assert!(!tiling.uncolored);
    assert_eq!(tiling.x_step, 10.0);
    // The cell content compiled to at least one fill op.
    assert!(!tiling.cell.operations.is_empty(), "cell content compiled");
    assert!(tiling.cell.operations.iter().any(|op| matches!(
        op,
        DisplayOp::BeginPaintOrigin(PaintOrigin::TilingPatternCell)
    )));
    assert!(
        page.paints
            .iter()
            .any(|p| matches!(p, Paint::Pattern { .. }))
    );
}

#[test]
fn uncolored_tiling_records_paint_type_2() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R\
         /Resources<</Pattern<</P1 5 0 R>>/ColorSpace<</CsP[/Pattern/DeviceRGB]>>>>>>",
    );
    // Uncolored pattern selected with an underlying green color.
    b.add_stream(4, "", b"/CsP cs 0 1 0 /P1 scn 0 0 100 100 re f");
    b.add_stream(
        5,
        "/Type/Pattern/PatternType 1/PaintType 2/TilingType 1\
         /BBox[0 0 10 10]/XStep 10/YStep 10/Resources<<>>",
        b"0 0 5 5 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    assert_eq!(page.tilings.len(), 1);
    let tiling = &page.tilings[0];
    assert!(tiling.uncolored, "PaintType 2 → uncolored");
    // Under-color is green.
    assert!(tiling.under_color.g > 0.9 && tiling.under_color.r < 0.1);
}
