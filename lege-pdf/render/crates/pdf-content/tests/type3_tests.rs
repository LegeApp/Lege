#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Type 3 font tests (ISO 32000-1 §9.6.5): showing text in a Type 3 font
//! executes each glyph's `/CharProcs` content stream inline (like a form)
//! instead of emitting a glyph run. These verify the emitted geometry, the
//! FontMatrix/`/Widths` placement, `d1` shape-only colour suppression, and the
//! tolerant recovery paths (missing CharProc, self-recursion).

use std::sync::Arc;

use pdf_content::semantic::{SemColor, SemanticOp};
use pdf_content::{PageCompiler, SemanticPage};
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::Matrix;
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

fn compile(snapshot: &DocumentSnapshot, page: u32) -> SemanticPage {
    let mut ctx = ParseContext::new();
    PageCompiler::new()
        .compile_semantic(snapshot, PageIndex(page), &mut ctx)
        .expect("compile failed")
}

fn concats(page: &SemanticPage) -> Vec<Matrix> {
    page.ops
        .iter()
        .filter_map(|o| match o {
            SemanticOp::Concat(m) => Some(*m),
            _ => None,
        })
        .collect()
}

fn count_fills(page: &SemanticPage) -> usize {
    page.ops
        .iter()
        .filter(|o| matches!(o, SemanticOp::Fill { .. }))
        .count()
}

/// Two CharProcs drawing distinct rects: the compiled ops must contain both
/// fills, each bracketed by Save/Concat/Restore, at positions reflecting the
/// FontMatrix and the `/Widths` advance.
#[test]
fn two_charprocs_draw_bracketed_fills_at_widths_positions() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"BT /F1 100 Tf 10 20 Td (AB) Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type3/FontBBox[0 0 1000 1000]\
         /FontMatrix[0.001 0 0 0.001 0 0]/CharProcs 6 0 R/Encoding 7 0 R\
         /FirstChar 65/LastChar 66/Widths[1000 500]/Resources<<>>>>",
    );
    b.add_object(6, "<</square 8 0 R/disc 9 0 R>>");
    b.add_object(7, "<</Type/Encoding/Differences[65/square 66/disc]>>");
    b.add_stream(8, "", b"1000 0 d0 0 0 100 100 re f");
    b.add_stream(9, "", b"500 0 d0 0 0 50 50 re f");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    // The extraction-only run is retained, but painting still comes entirely
    // from inline CharProc geometry.
    assert_eq!(
        page.ops
            .iter()
            .filter(|o| matches!(o, SemanticOp::ShowText(_)))
            .count(),
        1
    );
    assert!(!page.text_runs[0].visible);
    assert_eq!(count_fills(&page), 2, "two glyphs → two fills");
    assert_eq!(
        page.ops
            .iter()
            .filter(|op| matches!(
                op,
                SemanticOp::BeginPaintOrigin(pdf_page_ir::PaintOrigin::Type3Glyph)
            ))
            .count(),
        2
    );

    // Save/Concat/Fill/Restore for A, then again for B.
    let kinds: Vec<&SemanticOp> = page.ops.iter().collect();
    let saves = kinds
        .iter()
        .filter(|o| matches!(o, SemanticOp::Save))
        .count();
    let restores = kinds
        .iter()
        .filter(|o| matches!(o, SemanticOp::Restore))
        .count();
    assert_eq!(saves, 2);
    assert_eq!(restores, 2);

    // Concat matrices: FontMatrix(0.001) × param(Tfs=100) × Tm(10,20).
    // Scale collapses to 0.1; the first glyph sits at the Td origin (10,20),
    // the second is advanced by A's width (1000 glyph units → 0.001·1000·100 =
    // 100 text units): e = 110.
    let cs = concats(&page);
    assert_eq!(cs.len(), 2);
    assert!((cs[0].a - 0.1).abs() < 1e-9, "{:?}", cs[0]);
    assert!((cs[0].d - 0.1).abs() < 1e-9, "{:?}", cs[0]);
    assert!((cs[0].e - 10.0).abs() < 1e-9, "{:?}", cs[0]);
    assert!((cs[0].f - 20.0).abs() < 1e-9, "{:?}", cs[0]);
    assert!(
        (cs[1].e - 110.0).abs() < 1e-9,
        "second glyph advanced by A width: {:?}",
        cs[1]
    );
    assert!((cs[1].f - 20.0).abs() < 1e-9, "{:?}", cs[1]);
}

/// A `d1` (shape-only) CharProc: a colour operator inside it is suppressed, so
/// the glyph fills with the text fill colour set before `Tj`.
#[test]
fn d1_charproc_suppresses_inner_color() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"BT /F1 100 Tf 0.2 0.4 0.6 rg 10 20 Td (A) Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type3/FontBBox[0 0 1000 1000]\
         /FontMatrix[0.001 0 0 0.001 0 0]/CharProcs 6 0 R/Encoding 7 0 R\
         /FirstChar 65/LastChar 65/Widths[1000]/Resources<<>>>>",
    );
    b.add_object(6, "<</square 8 0 R>>");
    b.add_object(7, "<</Type/Encoding/Differences[65/square]>>");
    // d1 marks the glyph shape-only; the inner `1 0 0 rg` must be dropped.
    b.add_stream(8, "", b"1000 0 0 0 100 100 d1 1 0 0 rg 0 0 100 100 re f");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    let fill_colors: Vec<&SemColor> = page
        .ops
        .iter()
        .filter_map(|o| match o {
            SemanticOp::SetFillColor(c) => Some(c),
            _ => None,
        })
        .collect();
    // Only the outer colour survives; the glyph's own red is suppressed.
    assert_eq!(
        fill_colors.len(),
        1,
        "inner d1 colour must be dropped: {fill_colors:?}"
    );
    assert!(
        matches!(fill_colors[0], SemColor::DeviceRgb(r, g, bl)
            if (*r - 0.2).abs() < 1e-9 && (*g - 0.4).abs() < 1e-9 && (*bl - 0.6).abs() < 1e-9),
        "{fill_colors:?}"
    );
    assert_eq!(count_fills(&page), 1, "the glyph still paints its shape");
}

/// A code with no CharProc is skipped (page still compiles), but the glyph
/// still advances by its `/Widths` entry — a following glyph lands past it.
#[test]
fn missing_charproc_skips_glyph_but_still_advances() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    // Show A (square), C (no CharProc, width 800), A again.
    b.add_stream(4, "", b"BT /F1 100 Tf 10 20 Td (ACA) Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type3/FontBBox[0 0 1000 1000]\
         /FontMatrix[0.001 0 0 0.001 0 0]/CharProcs 6 0 R/Encoding 7 0 R\
         /FirstChar 65/LastChar 67/Widths[1000 500 800]/Resources<<>>>>",
    );
    // Only /square is mapped; code 67 ('C') names /gap which has no CharProc.
    b.add_object(6, "<</square 8 0 R>>");
    b.add_object(
        7,
        "<</Type/Encoding/Differences[65/square 66/disc 67/gap]>>",
    );
    b.add_stream(8, "", b"1000 0 d0 0 0 100 100 re f");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    // A drew, C skipped (no CharProc), A drew: two fills.
    assert_eq!(count_fills(&page), 2, "missing glyph draws nothing");
    // Second square sits past A (width 1000 → 100 text units) AND the missing
    // C (width 800 → 80 text units): e = 10 + 100 + 80 = 190.
    let cs = concats(&page);
    assert_eq!(cs.len(), 2);
    assert!((cs[0].e - 10.0).abs() < 1e-9, "{:?}", cs[0]);
    assert!(
        (cs[1].e - 190.0).abs() < 1e-9,
        "missing glyph must still advance by its width: {:?}",
        cs[1]
    );
}

/// A malformed CharProc may not consume its caller's graphics-state save or
/// leak an unmatched inner `q` into the following glyph/page content.
#[test]
fn malformed_charproc_graphics_state_is_isolated() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"BT /F1 100 Tf 10 20 Td (AA) Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type3/FontBBox[0 0 1000 1000]\
         /FontMatrix[0.001 0 0 0.001 0 0]/CharProcs 6 0 R/Encoding 7 0 R\
         /FirstChar 65/LastChar 65/Widths[1000]/Resources<<>>>>",
    );
    b.add_object(6, "<</square 8 0 R>>");
    b.add_object(7, "<</Type/Encoding/Differences[65/square]>>");
    // The leading Q must not pop the CharProc wrapper; the trailing q must be
    // closed at the stream boundary. Real-world producer damage uses both.
    b.add_stream(8, "", b"Q 1000 0 d0 0 0 100 100 re f q 10 0 0 10 0 0 cm");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    assert_eq!(count_fills(&page), 2, "both glyphs must still paint");
    let saves = page
        .ops
        .iter()
        .filter(|op| matches!(op, SemanticOp::Save))
        .count();
    let restores = page
        .ops
        .iter()
        .filter(|op| matches!(op, SemanticOp::Restore))
        .count();
    assert_eq!(saves, restores, "nested graphics state must be balanced");
    assert_eq!(saves, 4, "wrapper + malformed inner q for each glyph");

    let cs = concats(&page);
    assert_eq!(cs.len(), 4);
    assert!(
        (cs[0].a - 0.1).abs() < 1e-9 && (cs[2].a - 0.1).abs() < 1e-9,
        "the second glyph must not inherit the first glyph's transform: {cs:?}"
    );
    assert!((cs[2].e - 110.0).abs() < 1e-9, "{:?}", cs[2]);
}

/// A CharProc that shows its own font recurses; the shared invoke-depth guard
/// must terminate it and the page must still compile.
#[test]
fn self_recursive_charproc_terminates() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"BT /F1 100 Tf 10 20 Td (A) Tj ET");
    // The font's own /Resources reference itself, so the CharProc can re-show
    // /F1 and recurse.
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type3/FontBBox[0 0 1000 1000]\
         /FontMatrix[0.001 0 0 0.001 0 0]/CharProcs 6 0 R/Encoding 7 0 R\
         /FirstChar 65/LastChar 65/Widths[1000]\
         /Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_object(6, "<</square 8 0 R>>");
    b.add_object(7, "<</Type/Encoding/Differences[65/square]>>");
    // Draw a rect, then recurse by showing the same glyph again.
    b.add_stream(8, "", b"1000 0 d0 0 0 100 100 re f BT /F1 100 Tf (A) Tj ET");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());

    // The key assertion is that this terminates and compiles at all.
    let page = compile(&snap, 0);
    let fills = count_fills(&page);
    assert!(fills >= 1, "at least the outermost glyph draws");
    // Bounded by the invoke-depth limit (default 16), not unbounded.
    assert!(fills <= 64, "recursion must be depth-bounded, got {fills}");
}
