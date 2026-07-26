#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Annotation static-appearance rendering (ISO 32000-1 §12.5.5; PDFium
//! `FPDF_ANNOT` display-pass parity): `/AP /N` selection via `/AS`,
//! §12.5.5 BBox→Rect fitting, flag skipping, `/Popup` structural skipping,
//! and the non-widget-then-widget two-pass order.

use std::sync::Arc;

use pdf_content::semantic::{SemColor, SemanticOp};
use pdf_content::{PageCompiler, SemanticPage};
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

fn compile_with_annots(snapshot: &DocumentSnapshot) -> SemanticPage {
    let mut ctx = ParseContext::new();
    PageCompiler::new()
        .with_annotations(true)
        .compile_semantic(snapshot, PageIndex(0), &mut ctx)
        .expect("compile failed")
}

/// A one-page document with `annots_entries` (raw object refs) in `/Annots`,
/// empty page content, and whatever extra objects the caller added from id 10.
fn base_doc(b: &mut PdfBuilder, annots: &str) {
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 300 300]>>",
    );
    b.add_object(
        3,
        &format!("<</Type/Page/Parent 2 0 R/Contents 4 0 R/Annots[{annots}]>>"),
    );
    b.add_stream(4, "", b"");
}

fn fill_colors(page: &SemanticPage) -> Vec<&SemColor> {
    let mut current: Option<&SemColor> = None;
    let mut out = Vec::new();
    for op in page.ops.iter() {
        match op {
            SemanticOp::SetFillColor(c) => current = Some(c),
            SemanticOp::Fill { .. } => out.extend(current),
            _ => {}
        }
    }
    out
}

#[test]
fn appearance_stream_renders_with_rect_fit_matrix() {
    let mut b = PdfBuilder::new();
    base_doc(&mut b, "10 0 R");
    b.add_object(
        10,
        "<</Type/Annot/Subtype/Square/Rect[100 100 200 150]/AP<</N 11 0 R>>>>",
    );
    b.add_stream(
        11,
        "/Type/XObject/Subtype/Form/BBox[0 0 10 10]",
        b"1 0 0 rg 0 0 10 10 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());

    // Annotations off (the default): nothing painted.
    let mut ctx = ParseContext::new();
    let plain = PageCompiler::new()
        .compile_semantic(&snap, PageIndex(0), &mut ctx)
        .expect("compile failed");
    assert!(
        !plain
            .ops
            .iter()
            .any(|o| matches!(o, SemanticOp::Fill { .. }))
    );

    // Annotations on: the AP form runs under the §12.5.5 fit matrix
    // A = BBox[0,0,10,10] → Rect[100,100,200,150]: scale (10, 5),
    // translate (100, 100).
    let page = compile_with_annots(&snap);
    assert!(
        page.ops
            .iter()
            .any(|o| matches!(o, SemanticOp::Fill { .. }))
    );
    assert!(page.ops.iter().any(|o| matches!(
        o,
        SemanticOp::BeginPaintOrigin(pdf_page_ir::PaintOrigin::AnnotationAppearance)
    )));
    let fit = page
        .ops
        .iter()
        .find_map(|o| match o {
            SemanticOp::Concat(m) => Some(m),
            _ => None,
        })
        .expect("fit matrix emitted");
    assert_eq!((fit.a, fit.d), (10.0, 5.0));
    assert_eq!((fit.e, fit.f), (100.0, 100.0));
    // The form's /BBox clip is applied inside the fit.
    assert!(
        page.ops
            .iter()
            .any(|o| matches!(o, SemanticOp::Clip { .. }))
    );
}

#[test]
fn appearance_state_selects_among_n_substates() {
    let mut b = PdfBuilder::new();
    base_doc(&mut b, "10 0 R");
    b.add_object(
        10,
        "<</Type/Annot/Subtype/Widget/Rect[0 0 50 50]/AS/On\
         /AP<</N<</On 11 0 R/Off 12 0 R>>>>>>",
    );
    b.add_stream(
        11,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"0 1 0 rg 0 0 50 50 re f",
    );
    b.add_stream(
        12,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"0 0 1 rg 0 0 50 50 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile_with_annots(&open(b.into_bytes()));

    // Only the /On state painted: green, not blue.
    let colors = fill_colors(&page);
    assert_eq!(colors.len(), 1);
    assert!(matches!(colors[0], SemColor::DeviceRgb(r, g, bl)
        if *r == 0.0 && *g == 1.0 && *bl == 0.0));
}

#[test]
fn missing_as_falls_back_to_single_entry_dict_else_skips() {
    // Single-entry /N dict without /AS → that entry is used (tolerance).
    let mut b = PdfBuilder::new();
    base_doc(&mut b, "10 0 R");
    b.add_object(
        10,
        "<</Type/Annot/Subtype/Widget/Rect[0 0 50 50]/AP<</N<</Only 11 0 R>>>>>>",
    );
    b.add_stream(
        11,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"1 0 0 rg 0 0 50 50 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile_with_annots(&open(b.into_bytes()));
    assert_eq!(fill_colors(&page).len(), 1);

    // Two-entry /N dict without /AS → ambiguous, skipped.
    let mut b = PdfBuilder::new();
    base_doc(&mut b, "10 0 R");
    b.add_object(
        10,
        "<</Type/Annot/Subtype/Widget/Rect[0 0 50 50]\
         /AP<</N<</On 11 0 R/Off 12 0 R>>>>>>",
    );
    b.add_stream(
        11,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"1 0 0 rg 0 0 50 50 re f",
    );
    b.add_stream(
        12,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"0 0 1 rg 0 0 50 50 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile_with_annots(&open(b.into_bytes()));
    assert!(fill_colors(&page).is_empty());
}

#[test]
fn hidden_and_noview_annotations_are_skipped() {
    // /F 2 = Hidden, /F 32 = NoView: both skipped for screen rendering.
    for flags in [2, 32] {
        let mut b = PdfBuilder::new();
        base_doc(&mut b, "10 0 R");
        b.add_object(
            10,
            &format!("<</Type/Annot/Subtype/Square/Rect[0 0 50 50]/F {flags}/AP<</N 11 0 R>>>>"),
        );
        b.add_stream(
            11,
            "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
            b"1 0 0 rg 0 0 50 50 re f",
        );
        b.finish_classic_xref("/Root 1 0 R");
        let page = compile_with_annots(&open(b.into_bytes()));
        assert!(
            fill_colors(&page).is_empty(),
            "flags {flags} must suppress the appearance"
        );
    }
}

#[test]
fn popup_annotations_are_dropped_at_parse_time() {
    let mut b = PdfBuilder::new();
    base_doc(&mut b, "10 0 R 11 0 R");
    b.add_object(10, "<</Type/Annot/Subtype/Popup/Rect[0 0 50 50]>>");
    b.add_object(11, "<</Type/Annot/Subtype/Square/Rect[0 0 50 50]>>");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = snap.page(PageIndex(0)).expect("page");
    // The popup never even reaches the annotation list (PDFium parity).
    assert_eq!(page.annotations.len(), 1);
    let subtype = page.annotations[0].subtype.expect("subtype");
    assert_eq!(snap.names().resolve(subtype).as_ref(), b"Square");
}

#[test]
fn widgets_draw_after_non_widgets_regardless_of_annots_order() {
    // /Annots lists the widget FIRST; PDFium's display pass still paints
    // non-widgets first, then widgets.
    let mut b = PdfBuilder::new();
    base_doc(&mut b, "10 0 R 12 0 R");
    b.add_object(
        10,
        "<</Type/Annot/Subtype/Widget/Rect[0 0 50 50]/AP<</N 11 0 R>>>>",
    );
    b.add_stream(
        11,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"0 1 0 rg 0 0 50 50 re f", // widget: green
    );
    b.add_object(
        12,
        "<</Type/Annot/Subtype/Square/Rect[0 0 50 50]/AP<</N 13 0 R>>>>",
    );
    b.add_stream(
        13,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"1 0 0 rg 0 0 50 50 re f", // square: red
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile_with_annots(&open(b.into_bytes()));

    let colors = fill_colors(&page);
    assert_eq!(colors.len(), 2);
    // Red (non-widget) first, green (widget) second.
    assert!(matches!(colors[0], SemColor::DeviceRgb(r, ..) if *r == 1.0));
    assert!(matches!(colors[1], SemColor::DeviceRgb(_, g, _) if *g == 1.0));
}

#[test]
fn degenerate_rect_or_bbox_is_skipped_not_fatal() {
    let mut b = PdfBuilder::new();
    base_doc(&mut b, "10 0 R 12 0 R");
    // Zero-area /Rect.
    b.add_object(
        10,
        "<</Type/Annot/Subtype/Square/Rect[10 10 10 60]/AP<</N 11 0 R>>>>",
    );
    b.add_stream(
        11,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"1 0 0 rg 0 0 50 50 re f",
    );
    // Missing /BBox.
    b.add_object(
        12,
        "<</Type/Annot/Subtype/Square/Rect[0 0 50 50]/AP<</N 13 0 R>>>>",
    );
    b.add_stream(13, "/Type/XObject/Subtype/Form", b"1 0 0 rg 0 0 50 50 re f");
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile_with_annots(&open(b.into_bytes()));
    assert!(fill_colors(&page).is_empty());
}

#[test]
fn appearance_ignores_page_content_ending_state() {
    // Page content leaves an unbalanced q, a translate CTM, and a fill color;
    // the annotation must render from a fresh state (its fit matrix is the
    // only transform in effect).
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 300 300]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Annots[10 0 R]>>",
    );
    b.add_stream(4, "", b"q 1 0 0 1 999 999 cm 0 0 1 rg");
    b.add_object(
        10,
        "<</Type/Annot/Subtype/Square/Rect[20 30 70 80]/AP<</N 11 0 R>>>>",
    );
    b.add_stream(
        11,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]",
        b"1 0 0 rg 0 0 50 50 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile_with_annots(&open(b.into_bytes()));

    // Saves and Restores balance (lowering state safety), and the last
    // Concat is the annotation's pure-translate fit matrix, unaffected by
    // the page's dangling 999-translate.
    let saves = page
        .ops
        .iter()
        .filter(|o| matches!(o, SemanticOp::Save))
        .count();
    let restores = page
        .ops
        .iter()
        .filter(|o| matches!(o, SemanticOp::Restore))
        .count();
    assert_eq!(saves, restores);
    let fit = page
        .ops
        .iter()
        .filter_map(|o| match o {
            SemanticOp::Concat(m) => Some(m),
            _ => None,
        })
        .next_back()
        .expect("fit matrix");
    assert_eq!(
        (fit.a, fit.b, fit.c, fit.d, fit.e, fit.f),
        (1.0, 0.0, 0.0, 1.0, 20.0, 30.0)
    );
}
