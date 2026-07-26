#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6 item A: shading + function resolution end to end — a real PDF with
//! a `/Shading` resource + `sh` operator compiles to an axial `ShadingKind`
//! with a pre-sampled ramp; a PatternType 2 shading pattern resolves to a
//! `Paint::Shading`.

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{CompiledPage, DisplayOp, PageFeatures, Paint, ShadingKind};
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
fn sh_operator_builds_axial_shading_with_ramp() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R\
         /Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    // Axial shading, DeviceRGB, exponential black->white function.
    b.add_object(
        5,
        "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 100 0]/Extend[true true]\
         /Function<</FunctionType 2/Domain[0 1]/C0[0 0 0]/C1[1 1 1]/N 1>>>>",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    assert!(page.features.contains(PageFeatures::SHADINGS));
    assert_eq!(page.shadings.len(), 1);
    match &page.shadings[0].kind {
        ShadingKind::Axial {
            coords,
            extend,
            ramp,
            ..
        } => {
            assert_eq!(*coords, [0.0, 0.0, 100.0, 0.0]);
            assert_eq!(*extend, [true, true]);
            assert_eq!(ramp.len(), 256);
            // Function ramps black -> white.
            assert!(ramp[0].r < 0.02);
            assert!(ramp[255].r > 0.98);
        }
        other => panic!("expected axial, got {other:?}"),
    }
    assert!(
        page.operations
            .iter()
            .any(|op| matches!(op, DisplayOp::DrawShading { .. }))
    );
}

#[test]
fn sh_operator_builds_axial_shading_with_type4_function() {
    // A shading whose colour function is Type 4 (PostScript). Shadings feed a
    // single input `t`; the program `{ dup dup }` fans it into DeviceRGB
    // (t, t, t), a black->white ramp — exercising the pre-sampled ramp path
    // through `Function::eval` -> `eval_n`. Type 4 is a *stream*, so the
    // function is an indirect object (6 0 R).
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R\
         /Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    b.add_object(
        5,
        "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 100 0]/Extend[true true]\
         /Function 6 0 R>>",
    );
    b.add_stream(
        6,
        "/FunctionType 4/Domain[0 1]/Range[0 1 0 1 0 1]",
        b"{ dup dup }",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    assert!(page.features.contains(PageFeatures::SHADINGS));
    assert_eq!(page.shadings.len(), 1);
    match &page.shadings[0].kind {
        ShadingKind::Axial { ramp, .. } => {
            assert_eq!(ramp.len(), 256);
            assert!(ramp[0].r < 0.02, "t=0 is black: {:?}", ramp[0]);
            assert!(ramp[255].r > 0.98, "t=1 is white: {:?}", ramp[255]);
        }
        other => panic!("expected axial, got {other:?}"),
    }
}

#[test]
fn non_unit_domain_ramp_endpoints_are_the_boundary_colors() {
    // /Domain [0.25 0.75]: the pre-sampled ramp spans the *domain*, so its
    // endpoints are f(0.25) and f(0.75) — and those are exactly the boundary
    // colors an /Extend region must paint (ISO 32000-1 §8.7.4.5.2 defines
    // extension as the constant color at t0/t1, never f(0)/f(1)).
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R\
         /Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    // Linear grey ramp f(t) = t over function domain [0 1]; shading domain
    // restricted to [0.25 0.75].
    b.add_object(
        5,
        "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 100 0]/Domain[0.25 0.75]\
         /Extend[true true]\
         /Function<</FunctionType 2/Domain[0 1]/C0[0 0 0]/C1[1 1 1]/N 1>>>>",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);
    match &page.shadings[0].kind {
        ShadingKind::Axial { ramp, extend, .. } => {
            assert_eq!(*extend, [true, true]);
            assert!(
                (ramp[0].r - 0.25).abs() < 0.01,
                "start boundary f(0.25): {:?}",
                ramp[0]
            );
            assert!(
                (ramp[255].r - 0.75).abs() < 0.01,
                "end boundary f(0.75): {:?}",
                ramp[255]
            );
        }
        other => panic!("expected axial, got {other:?}"),
    }
}

#[test]
fn axial_shading_honors_multi_colorant_devicen_colorspace() {
    // Regression (rosiesmenu3): a type 2/3 shading whose /ColorSpace is a
    // 2-colorant /DeviceN. Its /Function emits 2 colorant tints, which must run
    // through the DeviceN tint transform to reach the alternate space. The old
    // ramp path ignored /ColorSpace and fed the raw 2 components to
    // `comps_to_rgba`, whose arity dispatch has no 2-component arm — so the
    // ramp collapsed to pure black, flooding whole-page background shadings.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R\
         /Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    // Axial shading over [/DeviceN [/Cyan /Black] /DeviceRGB <tint>]. The
    // shading /Function fans t into two tints (`{ dup }`); the DeviceN tint
    // transform ignores them and emits a constant red (`{ pop pop 1 0 0 }`) so
    // the assertion is unambiguous: a correctly routed ramp is red, the broken
    // one is black.
    b.add_object(
        5,
        "<</ShadingType 2/ColorSpace 6 0 R/Coords[0 0 100 0]/Extend[true true]\
         /Function 7 0 R>>",
    );
    b.add_object(6, "[/DeviceN[/Cyan/Black]/DeviceRGB 8 0 R]");
    b.add_stream(7, "/FunctionType 4/Domain[0 1]/Range[0 1 0 1]", b"{ dup }");
    b.add_stream(
        8,
        "/FunctionType 4/Domain[0 1 0 1]/Range[0 1 0 1 0 1]",
        b"{ pop pop 1 0 0 }",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    match &page.shadings[0].kind {
        ShadingKind::Axial { ramp, .. } => {
            assert_eq!(ramp.len(), 256);
            // Not black, and specifically red (the tint transform's output).
            for c in [ramp[0], ramp[128], ramp[255]] {
                assert!(c.r > 0.98, "red channel should be full: {c:?}");
                assert!(c.g < 0.02 && c.b < 0.02, "green/blue should be ~0: {c:?}");
            }
        }
        other => panic!("expected axial, got {other:?}"),
    }
}

#[test]
fn shading_pattern_resolves_to_shading_paint() {
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
    // Select Pattern colorspace, set P1, fill a rectangle.
    b.add_stream(4, "", b"/Pattern cs /P1 scn 10 10 80 80 re f");
    b.add_object(
        5,
        "<</Type/Pattern/PatternType 2/Matrix[1 0 0 1 0 0]\
         /Shading<</ShadingType 3/ColorSpace/DeviceRGB/Coords[50 50 0 50 50 40]\
         /Extend[true true]/Function<</FunctionType 2/Domain[0 1]/C0[1 0 0]/C1[0 0 1]/N 1>>>>>>",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    assert!(page.features.contains(PageFeatures::SHADINGS));
    assert_eq!(page.shadings.len(), 1);
    assert!(matches!(page.shadings[0].kind, ShadingKind::Radial { .. }));
    // The fill's paint is a shading pattern.
    let has_shading_paint = page
        .paints
        .iter()
        .any(|p| matches!(p, Paint::Shading { .. }));
    assert!(has_shading_paint, "fill paint should be Paint::Shading");
}
