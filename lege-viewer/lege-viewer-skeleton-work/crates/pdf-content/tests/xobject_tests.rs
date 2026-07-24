#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 2c tests: XObject invocation (form recursion + images), inline
//! images, and ExtGState application.

use std::sync::Arc;

use pdf_content::semantic::{SemanticOp, SemColor};
use pdf_content::{PageCompiler, SemanticPage};
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
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

#[test]
fn form_xobject_executes_inline_with_matrix_and_bbox() {
    // Page content invokes a form; the form fills a rectangle. The form's ops
    // must appear inline, wrapped in Save / Concat(Matrix) / Clip(BBox) /
    // Restore.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Fm 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q /Fm Do Q");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]/Matrix[1 0 0 1 10 20]",
        b"1 0 0 rg 0 0 50 50 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    let kinds: Vec<&SemanticOp> = page.ops.iter().collect();
    // Outer q, then the form's Save/Concat/Clip, its color+fill, Restore, outer Q.
    assert!(matches!(kinds[0], SemanticOp::Save)); // outer q
    assert!(matches!(kinds[1], SemanticOp::Save)); // form save
    assert!(matches!(kinds[2], SemanticOp::Concat(_)));
    assert!(matches!(kinds[3], SemanticOp::Clip { .. })); // BBox clip
    assert!(matches!(
        kinds[4],
        SemanticOp::BeginPaintOrigin(pdf_page_ir::PaintOrigin::FormXObject)
    ));
    assert!(matches!(kinds[5], SemanticOp::SetFillColor(SemColor::DeviceRgb(..))));
    assert!(matches!(kinds[6], SemanticOp::Fill { .. }));
    assert!(matches!(kinds[7], SemanticOp::EndPaintOrigin));
    assert!(matches!(kinds[8], SemanticOp::Restore)); // form restore
    assert!(matches!(kinds[9], SemanticOp::Restore)); // outer Q
    assert_eq!(kinds.len(), 10);

    // Two interned paths: the form fill and the BBox clip.
    assert_eq!(page.paths.len(), 2);
}

#[test]
fn self_referential_form_is_broken_not_infinite() {
    // A form that invokes itself must terminate (cycle detection), not loop
    // or overflow.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Fm 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Fm Do");
    // The form references itself through its own resources.
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Form/BBox[0 0 10 10]/Resources<</XObject<</Fm 5 0 R>>>>",
        b"0 0 10 10 re f /Fm Do",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);
    // The single legitimate fill compiled; the recursive Do was skipped.
    let fills = page.ops.iter().filter(|o| matches!(o, SemanticOp::Fill { .. })).count();
    assert_eq!(fills, 1);
}

#[test]
fn image_xobject_emits_draw_image() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 100 0 0 100 0 0 cm /Im Do Q");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 4/Height 4/BitsPerComponent 8/ColorSpace/DeviceRGB",
        &[0u8; 48],
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    assert_eq!(page.images.len(), 1);
    assert_eq!(page.images[0].width, 4);
    assert_eq!(page.images[0].height, 4);
    assert_eq!(page.images[0].bits_per_component, 8);
    assert!(!page.images[0].is_mask);
    assert!(page.images[0].object.is_some());
    assert!(page.ops.iter().any(|o| matches!(o, SemanticOp::DrawImage(_))));
}

#[test]
fn inline_image_is_captured() {
    let mut src = b"q 10 0 0 10 0 0 cm BI /W 2 /H 2 /BPC 8 /CS /RGB /F /AHx ID ".to_vec();
    src.extend_from_slice(b"00112233445566778899aabb\n");
    src.extend_from_slice(b"EI Q");

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>");
    b.add_stream(4, "", &src);
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    assert_eq!(page.images.len(), 1);
    let img = &page.images[0];
    assert_eq!(img.width, 2);
    assert_eq!(img.height, 2);
    assert_eq!(img.bits_per_component, 8);
    assert!(img.object.is_none()); // inline
    assert_eq!(img.filters, vec![b"ASCIIHexDecode".to_vec()]);
    assert!(page.ops.iter().any(|o| matches!(o, SemanticOp::DrawImage(_))));
}

#[test]
fn ext_gstate_sets_alpha_and_blend() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ExtGState<</GS0 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/GS0 gs 0 0 10 10 re f");
    b.add_object(5, "<</Type/ExtGState/ca 0.5/CA 0.25/BM/Multiply>>");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    assert!(page
        .ops
        .iter()
        .any(|o| matches!(o, SemanticOp::SetFillAlpha(a) if (*a - 0.5).abs() < 1e-6)));
    assert!(page
        .ops
        .iter()
        .any(|o| matches!(o, SemanticOp::SetStrokeAlpha(a) if (*a - 0.25).abs() < 1e-6)));
    assert!(page.ops.iter().any(|o| matches!(
        o,
        SemanticOp::SetBlendMode(pdf_page_ir::BlendMode::Multiply)
    )));
}

#[test]
fn nested_forms_respect_depth_limit() {
    // A chain of forms each invoking the next; with a low depth limit the
    // compile must fail cleanly with RecursionDepth rather than blow the
    // stack. Forms are objects 11..=20; form k invokes form k-1, and form 11
    // invokes object 10 (undefined) so a correctly-terminating run would stop.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Fm 20 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Fm Do");
    for k in (11..=20u32).rev() {
        let child = k - 1;
        let dict = format!(
            "/Type/XObject/Subtype/Form/BBox[0 0 10 10]/Resources<</XObject<</Fm {child} 0 R>>>>"
        );
        b.add_stream(k, &dict, b"/Fm Do");
    }
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());

    let mut ctx = ParseContext::new();
    let limits = pdf_content::ContentLimits { max_invoke_depth: 4, ..Default::default() };
    let err = PageCompiler::with_limits(limits)
        .compile_semantic(&snap, PageIndex(0), &mut ctx)
        .expect_err("deep form chain must be refused");
    assert!(matches!(err, pdf_content::ContentError::RecursionDepth(_)), "{err:?}");
}

#[test]
fn pending_path_does_not_leak_into_or_out_of_a_form() {
    // pdfbox/5302: the page builds a page-sized rectangle, invokes a form
    // while that path is still pending, and only then issues `W` (with no
    // painting operator at all). Path construction is per content stream —
    // PDFium gives each form its own parser — so the form's `re f` must fill
    // its own 10x10 rectangle and nothing else. Sharing one builder made the
    // form fill the page-sized rectangle too, flooding the page.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Fm 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"0 0 200 200 re /Fm Do W n");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Form/BBox[0 0 100 100]",
        b"0 0 10 10 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    let fills: Vec<_> = page
        .ops
        .iter()
        .filter_map(|op| match op {
            SemanticOp::Fill { path, .. } => Some(*path),
            _ => None,
        })
        .collect();
    assert_eq!(fills.len(), 1, "form should paint exactly one fill");
    // One `re` is 5 verbs / 4 points; the page's leaked rectangle would double it.
    let path = &page.paths[fills[0].0 as usize];
    assert_eq!(path.verbs.len(), 5, "form fill must not include the caller's path");
}

#[test]
fn a_resource_name_is_not_cached_across_resource_dictionaries() {
    // pdfjs/issue8565: `/P1` means one pattern in the page's resources and a
    // different one in a form's. Resolution is cached by name, so the form's
    // entry must not answer the page's lookup (or vice versa).
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources\
         <</XObject<</Fm 5 0 R>>/Pattern<</P1 8 0 R>>>>>>",
    );
    // The form runs first and resolves /P1 to the axial shading; the page then
    // resolves its own /P1, which is the radial one.
    b.add_stream(4, "", b"/Fm Do /Pattern cs /P1 scn 0 0 200 200 re f");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Form/BBox[0 0 200 200]/Resources<</Pattern<</P1 7 0 R>>>>",
        b"/Pattern cs /P1 scn 0 0 10 10 re f",
    );
    b.add_object(6, "<</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>");
    b.add_object(
        7,
        "<</Type/Pattern/PatternType 2/Shading\
         <</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 200 0]/Function 6 0 R>>>>",
    );
    b.add_object(
        8,
        "<</Type/Pattern/PatternType 2/Shading\
         <</ShadingType 3/ColorSpace/DeviceRGB/Coords[100 100 0 100 100 100]/Function 6 0 R>>>>",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    let shadings: Vec<_> = page
        .ops
        .iter()
        .filter_map(|op| match op {
            SemanticOp::SetFillColor(SemColor::ShadingPattern { shading, .. }) => Some(*shading),
            _ => None,
        })
        .collect();
    assert_eq!(shadings.len(), 2, "form fill and page fill");
    assert_ne!(
        shadings[0], shadings[1],
        "the form's /P1 must not be reused for the page's /P1"
    );
    // The page's /P1 is the radial pattern (object 8), the form's the axial one.
    assert_eq!(page.shadings[shadings[0].0 as usize].object.map(|o| o.number), Some(7));
    assert_eq!(page.shadings[shadings[1].0 as usize].object.map(|o| o.number), Some(8));
}
