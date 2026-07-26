#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 3a tests: SemanticPage → CompiledPage lowering — explicit state,
//! interned resources, and the explicit clip stack.

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{BlendMode, CompiledPage, DisplayOp, FillRule, Paint};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

fn single_page(content: &[u8]) -> DocumentSnapshot {
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
    b.add_stream(4, "", content);
    b.add_object(5, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>");
    b.finish_classic_xref("/Root 1 0 R");
    open(b.into_bytes())
}

fn compile(snapshot: &DocumentSnapshot) -> CompiledPage {
    let mut ctx = ParseContext::new();
    PageCompiler::new()
        .compile(snapshot, PageIndex(0), &mut ctx)
        .expect("compile failed")
}

fn solid(paint: &Paint) -> (f32, f32, f32, f32) {
    match paint {
        Paint::Solid(c) => (c.r, c.g, c.b, c.a),
        other => panic!("expected solid, got {other:?}"),
    }
}

#[test]
fn fill_lowers_to_explicit_paint_op() {
    let page = compile(&single_page(b"0.2 0.3 0.4 rg 10 20 30 40 re f"));
    assert_eq!(page.paths.len(), 1);
    assert_eq!(page.paints.len(), 1);
    assert_eq!(solid(&page.paints[0]), (0.2, 0.3, 0.4, 1.0));
    assert_eq!(page.operations.len(), 1);
    match &page.operations[0] {
        DisplayOp::FillPath {
            path,
            paint,
            rule,
            alpha,
            blend,
        } => {
            assert_eq!(path.0, 0);
            assert_eq!(paint.0, 0);
            assert_eq!(*rule, FillRule::NonZero);
            assert_eq!(*alpha, 1.0);
            assert_eq!(*blend, BlendMode::Normal);
        }
        other => panic!("expected FillPath, got {other:?}"),
    }
}

#[test]
fn compiled_content_bounds_follow_visible_paint_not_page_box() {
    let page = compile(&single_page(b"10 20 30 40 re f"));
    let bounds = page
        .content_bounds
        .expect("painted rectangle has an extent");
    assert_eq!(
        bounds,
        pdf_page_ir::Rect {
            x0: 10.0,
            y0: 20.0,
            x1: 40.0,
            y1: 60.0,
        }
    );

    let empty = compile(&single_page(b"10 20 30 40 re W n"));
    assert_eq!(
        empty.content_bounds, None,
        "clip-only geometry does not paint"
    );
}

#[test]
fn identical_paints_and_paths_are_interned_once() {
    // Two fills of the same rectangle in the same color: one path, one paint.
    let page = compile(&single_page(
        b"1 0 0 rg 0 0 10 10 re f 1 0 0 rg 0 0 10 10 re f",
    ));
    assert_eq!(page.paths.len(), 1, "identical paths dedup");
    assert_eq!(page.paints.len(), 1, "identical paints dedup");
    assert_eq!(page.operations.len(), 2);
}

#[test]
fn distinct_colors_make_distinct_paints() {
    let page = compile(&single_page(
        b"1 0 0 rg 0 0 10 10 re f 0 1 0 rg 0 0 10 10 re f",
    ));
    assert_eq!(page.paints.len(), 2);
    assert_eq!(page.paths.len(), 1); // same geometry, deduped
}

#[test]
fn cmyk_converts_to_rgb() {
    // `k` lowers to a device RGB paint through the frozen CMYK policy —
    // Adobe's measured table, matching PDFium. Cyan ink is emphatically not
    // the arithmetic (0, 1, 1): it is a real pigment, darker and duller.
    let page = compile(&single_page(b"1 0 0 0 k 0 0 10 10 re f"));
    let (r, g, b, a) = solid(&page.paints[0]);
    assert_eq!(a, 1.0);
    assert!(r < 0.25, "cyan absorbs red: {r}");
    assert!(g > 0.55 && g < 0.85, "not the arithmetic 1.0: {g}");
    assert!(b > 0.75, "cyan is blue-dominant: {b}");
}

#[test]
fn clip_becomes_push_pop_bracketed_to_scope() {
    // q  <rect> W n  <fill>  Q  <fill>
    let page = compile(&single_page(
        b"q 0 0 100 100 re W n 0 0 5 5 re f Q 0 0 5 5 re f",
    ));
    let kinds: Vec<&DisplayOp> = page.operations.iter().collect();
    assert!(matches!(kinds[0], DisplayOp::Save));
    assert!(matches!(kinds[1], DisplayOp::PushClip { .. }));
    assert!(matches!(kinds[2], DisplayOp::FillPath { .. }));
    assert!(matches!(kinds[3], DisplayOp::PopClip)); // unwound before Restore
    assert!(matches!(kinds[4], DisplayOp::Restore));
    assert!(matches!(kinds[5], DisplayOp::FillPath { .. }));
    assert_eq!(kinds.len(), 6);
}

#[test]
fn graphics_state_restores_across_q_q() {
    // Red inside q…Q must not colour the fill after Q (which is default black).
    let page = compile(&single_page(b"q 1 0 0 rg 0 0 5 5 re f Q 0 0 5 5 re f"));
    // Collect the paint of each FillPath in order.
    let fills: Vec<(f32, f32, f32, f32)> = page
        .operations
        .iter()
        .filter_map(|op| match op {
            DisplayOp::FillPath { paint, .. } => Some(solid(&page.paints[paint.index()])),
            _ => None,
        })
        .collect();
    assert_eq!(fills.len(), 2);
    assert_eq!(fills[0], (1.0, 0.0, 0.0, 1.0), "inside q…Q is red");
    assert_eq!(fills[1], (0.0, 0.0, 0.0, 1.0), "after Q reverts to black");
}

#[test]
fn fill_stroke_emits_two_ops_sharing_the_path() {
    let page = compile(&single_page(b"0 0 10 10 re B"));
    assert_eq!(page.operations.len(), 2);
    let (fill_path, stroke_path) = match (&page.operations[0], &page.operations[1]) {
        (
            DisplayOp::FillPath { path: p1, .. },
            DisplayOp::StrokePath {
                path: p2, style, ..
            },
        ) => {
            assert_eq!(style.0, 0);
            (p1.0, p2.0)
        }
        other => panic!("expected Fill then Stroke, got {other:?}"),
    };
    assert_eq!(fill_path, stroke_path, "B reuses one interned path");
    assert_eq!(page.paths.len(), 1);
    assert_eq!(page.stroke_styles.len(), 1);
}

#[test]
fn stroke_style_captures_width_and_dash() {
    let page = compile(&single_page(b"3 w [2 1] 0 d 0 0 m 10 10 l S"));
    assert_eq!(page.stroke_styles.len(), 1);
    let s = &page.stroke_styles[0];
    assert_eq!(s.width, 3.0);
    assert_eq!(&*s.dash_pattern, &[2.0, 1.0]);
}

#[test]
fn ext_gstate_alpha_lands_on_paint_ops() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ExtGState<</GS0 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/GS0 gs 0 0 1 rg /GS0 gs 0 0 10 10 re f");
    b.add_object(5, "<</Type/ExtGState/ca 0.5/BM/Multiply>>");
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    let fill = page
        .operations
        .iter()
        .find_map(|op| match op {
            DisplayOp::FillPath { alpha, blend, .. } => Some((*alpha, *blend)),
            _ => None,
        })
        .expect("a fill op");
    assert_eq!(fill.0, 0.5);
    assert_eq!(fill.1, BlendMode::Multiply);
}

// --- Phase 3b: text, images, transparency groups, features, complexity ------

use pdf_page_ir::PageFeatures;

#[test]
fn text_lowers_to_glyph_run_and_font() {
    let page = compile(&single_page(b"BT /F1 12 Tf 72 700 Td (Hi) Tj ET"));
    assert_eq!(page.glyph_runs.len(), 1);
    assert_eq!(page.fonts.len(), 1);
    let run = &page.glyph_runs[0];
    assert_eq!(run.font.0, 0);
    assert_eq!(run.font_size, 12.0);
    assert_eq!(run.glyphs.len(), 2); // 'H', 'i'
    assert_eq!(run.transform.e, 72.0);
    assert_eq!(run.transform.f, 700.0);
    assert!(
        page.operations
            .iter()
            .any(|o| matches!(o, DisplayOp::DrawGlyphRun { .. }))
    );
    assert!(page.features.contains(PageFeatures::TEXT));
    assert_eq!(page.complexity.glyph_count, 2);
}

#[test]
fn transparency_group_lowers_to_scoped_ops() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Fm 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Fm Do");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Form/BBox[0 0 20 20]/Group<</S/Transparency/I true>>",
        b"0 0 1 rg 0 0 20 20 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    assert_eq!(page.groups.len(), 1);
    assert!(page.groups[0].isolated);
    assert!(page.features.contains(PageFeatures::TRANSPARENCY));
    assert!(
        page.operations
            .iter()
            .any(|o| matches!(o, DisplayOp::BeginTransparencyGroup { .. }))
    );
    assert!(
        page.operations
            .iter()
            .any(|o| matches!(o, DisplayOp::EndTransparencyGroup))
    );
    assert_eq!(page.complexity.transparency_group_count, 1);
    assert!(page.complexity.estimated_peak_bytes > 0);
}

#[test]
fn group_interior_alpha_resets_to_one() {
    // ISO 32000-1 §11.6.6: the outer constant alpha becomes the GROUP's
    // composite opacity; ops inside the group start from alpha 1.0. Before the
    // reset was mirrored into the lowering state, a 0.5-ca group drew its
    // content at 0.25 (the Medieval Garments cover regression: inkΔ 0.756).
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R\
         /Resources<</XObject<</Fm 5 0 R>>/ExtGState<</G0 6 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/G0 gs /Fm Do");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Form/BBox[0 0 20 20]/Group<</S/Transparency>>",
        b"0 0 1 rg 0 0 20 20 re f",
    );
    b.add_object(6, "<</Type/ExtGState/ca 0.5/CA 0.5>>");
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    // The outer ca lands on the group as its composite opacity…
    assert_eq!(page.groups.len(), 1);
    assert!(
        (page.groups[0].opacity - 0.5).abs() < 1e-6,
        "outer ca is the group opacity"
    );
    // …and the fill INSIDE the group is lowered at alpha 1.0, not 0.5/0.25.
    let mut in_group = false;
    let mut checked = false;
    for op in page.operations.iter() {
        match op {
            DisplayOp::BeginTransparencyGroup { .. } => in_group = true,
            DisplayOp::EndTransparencyGroup => in_group = false,
            DisplayOp::FillPath { alpha, .. } if in_group => {
                assert_eq!(
                    *alpha, 1.0,
                    "group-interior alpha must reset (was applied twice)"
                );
                checked = true;
            }
            _ => {}
        }
    }
    assert!(checked, "expected a fill inside the group");
}

#[test]
fn image_lowers_and_sets_feature_flags() {
    // Inline DCT-encoded image: IMAGES + NEEDS_DCT must be flagged.
    let mut src = b"q 10 0 0 10 0 0 cm BI /W 2 /H 2 /BPC 8 /CS /RGB /F /DCT ID ".to_vec();
    src.extend_from_slice(b"deadbeef\nEI Q");
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>",
    );
    b.add_stream(4, "", &src);
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    assert_eq!(page.images.len(), 1);
    assert_eq!(page.images[0].width, 2);
    assert!(page.features.contains(PageFeatures::IMAGES));
    assert!(page.features.contains(PageFeatures::NEEDS_DCT));
    assert!(
        page.operations
            .iter()
            .any(|o| matches!(o, DisplayOp::DrawImage { .. }))
    );
    assert_eq!(page.complexity.image_pixels, 4);
}

#[test]
fn inline_image_dp_carries_ccitt_parameters() {
    // Table 93: an inline image spells /DecodeParms as /DP; the keys inside
    // the parms dict are the standard names. The compiled ImageIr must carry
    // them, or inline CCITT decodes with the spec defaults and shreds.
    let mut src = b"q 24 0 0 2 0 0 cm BI /W 24 /H 2 /BPC 1 /CS /G /F /CCF \
/DP <</K -1 /Columns 24 /BlackIs1 true /EncodedByteAlign true /Rows 2>> ID "
        .to_vec();
    src.extend_from_slice(&[0x13, 0x37]);
    src.extend_from_slice(b"\nEI Q");
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>",
    );
    b.add_stream(4, "", &src);
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    assert_eq!(page.images.len(), 1);
    let parms = page.images[0]
        .codec_parms
        .as_ref()
        .expect("inline /DP must be read");
    assert_eq!(parms.k, -1);
    assert_eq!(parms.columns, 24);
    assert_eq!(parms.rows, 2);
    assert!(parms.black_is_1);
    assert!(parms.byte_align);
    // Spelled-out /DecodeParms works identically.
    let mut src2 = b"BI /W 24 /H 2 /BPC 1 /CS /G /F /CCF \
/DecodeParms <</Columns 24>> ID "
        .to_vec();
    src2.extend_from_slice(&[0x13, 0x37]);
    src2.extend_from_slice(b"\nEI");
    let mut b2 = PdfBuilder::new();
    b2.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b2.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b2.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>",
    );
    b2.add_stream(4, "", &src2);
    b2.finish_classic_xref("/Root 1 0 R");
    let page2 = compile(&open(b2.into_bytes()));
    let parms2 = page2.images[0]
        .codec_parms
        .as_ref()
        .expect("/DecodeParms read");
    assert_eq!(parms2.columns, 24);
}

#[test]
fn icc_and_overprint_feature_flags_are_computed() {
    // /Cs is [/ICCBased stream] used via cs; /G1 sets /op true. Both facts
    // must surface as page feature flags (flags only — rendering keeps the
    // ICC arity approximation and overprint does not change compositing).
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources\
         <</ColorSpace<</Cs 5 0 R>>/ExtGState<</G1<</Type/ExtGState/op true>>>>>>>>",
    );
    b.add_stream(4, "", b"/G1 gs /Cs cs 0.5 0.5 0.5 scn 0 0 50 50 re f");
    b.add_object(5, "[/ICCBased 6 0 R]");
    b.add_stream(6, "/N 3", &[0u8; 4]);
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    assert!(
        page.features.contains(PageFeatures::ICC_COLOR),
        "{:?}",
        page.features
    );
    assert!(
        page.features.contains(PageFeatures::OVERPRINT),
        "{:?}",
        page.features
    );

    // A plain device-color page sets neither.
    let mut b2 = PdfBuilder::new();
    b2.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b2.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b2.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>",
    );
    b2.add_stream(4, "", b"0 0 0 rg 0 0 50 50 re f");
    b2.finish_classic_xref("/Root 1 0 R");
    let plain = compile(&open(b2.into_bytes()));
    assert!(!plain.features.contains(PageFeatures::ICC_COLOR));
    assert!(!plain.features.contains(PageFeatures::OVERPRINT));
}

#[test]
fn symbolic_substitute_carries_synthetic_style() {
    // A non-embedded /Symbol,Bold font substitutes the (only) Symbol face,
    // which has no bold cut — the FontResource must request emboldening
    // (PDFium's weight-700 level, 70/1000 em). An italic request slants.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources         <</Font<</F1 5 0 R/F2 6 0 R/F3 7 0 R>>>>>>",
    );
    b.add_stream(
        4,
        "",
        b"BT /F1 12 Tf 10 10 Td (a) Tj /F2 12 Tf (a) Tj /F3 12 Tf (a) Tj ET",
    );
    b.add_object(5, "<</Type/Font/Subtype/Type1/BaseFont/Symbol,Bold>>");
    b.add_object(6, "<</Type/Font/Subtype/Type1/BaseFont/Symbol,Italic>>");
    b.add_object(7, "<</Type/Font/Subtype/Type1/BaseFont/Symbol>>");
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));
    assert_eq!(page.fonts.len(), 3);
    let bold = &page.fonts[0];
    assert!(
        bold.synthetic_embolden_em > 0.0,
        "bold Symbol wants embolden"
    );
    assert_eq!(bold.synthetic_shear, 0.0);
    let italic = &page.fonts[1];
    assert!(
        italic.synthetic_shear > 0.2,
        "italic Symbol wants the 12 degree shear"
    );
    assert_eq!(italic.synthetic_embolden_em, 0.0);
    let plain = &page.fonts[2];
    assert_eq!(plain.synthetic_shear, 0.0);
    assert_eq!(plain.synthetic_embolden_em, 0.0);
}

#[test]
fn debug_dump_is_stable_and_schema_keyed() {
    let snap = single_page(b"0.2 0.3 0.4 rg 10 20 30 40 re f BT /F1 12 Tf 72 700 Td (Hi) Tj ET");
    let a = compile(&snap).debug_dump();
    let b = compile(&snap).debug_dump();
    assert_eq!(a, b, "debug dump must be deterministic");
    assert!(
        a.starts_with(&format!(
            "CompiledPage schema={}\n",
            pdf_page_ir::IR_SCHEMA_VERSION
        )),
        "{a}"
    );
    assert!(a.contains("fill path#0 paint#0 NonZero"), "{a}");
    assert!(a.contains("draw-glyph-run run#0"), "{a}");
    assert!(a.contains("features: BASIC_PATHS|TEXT"), "{a}");
}

// --- Font Phase 1a: PDF advance widths --------------------------------------

#[test]
fn text_advances_use_pdf_widths() {
    // A font with explicit /Widths: A(65)=1000, B(66)=500, C(67)=250 (per em).
    // At 10pt, advances are 10, 5, 2.5 → glyphs at x = 0, 10, 15.
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
    b.add_stream(4, "", b"BT /F1 10 Tf 0 0 Td (ABC) Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/Custom/FirstChar 65/Widths[1000 500 250]>>",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    assert_eq!(page.glyph_runs.len(), 1);
    let g = &page.glyph_runs[0].glyphs;
    assert_eq!(g.len(), 3);
    // The PDF's /Widths drive the advances, not the substituted face's.
    assert_eq!(g[0].x, 0.0);
    assert!((g[1].x - 10.0).abs() < 1e-6, "B at {}", g[1].x);
    assert!((g[2].x - 15.0).abs() < 1e-6, "C at {}", g[2].x);
    // /BaseFont /Custom is not embedded, so Font Phase 3 substitutes a
    // bundled face: the codes now resolve to real glyph ids, not to the
    // raw character codes they used to fall back to.
    assert!(
        g.iter().all(|gl| gl.glyph != 0),
        "every code resolved to a glyph: {g:?}"
    );
    assert_ne!(g[0].glyph, 65, "resolved through the face, not code-as-gid");
}

#[test]
fn missing_width_falls_back() {
    // Code 90 (Z) is outside [65,67] → MissingWidth 400 → advance 4 at 10pt.
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
    b.add_stream(4, "", b"BT /F1 10 Tf 0 0 Td (AZ) Tj ET");
    b.add_object(5, "<</Type/Font/Subtype/Type1/BaseFont/Custom/FirstChar 65/Widths[1000]/FontDescriptor 6 0 R>>");
    b.add_object(6, "<</Type/FontDescriptor/MissingWidth 400>>");
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    let g = &page.glyph_runs[0].glyphs;
    assert_eq!(g[0].x, 0.0); // A
    assert!(
        (g[1].x - 10.0).abs() < 1e-6,
        "Z after A(width 1000): {}",
        g[1].x
    );
}

#[test]
fn cid_identity_two_byte_advances() {
    // Type0/Identity-H font: /DW 1000, /W [1 [500 250]] → CID1=500, CID2=250,
    // CID3=default 1000. Show 2-byte codes 0001 0002 0003 at 10pt → advances
    // 5, 2.5, 10 → glyphs at x = 0, 5, 7.5.
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
    b.add_stream(4, "", b"BT /F1 10 Tf 0 0 Td <000100020003> Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type0/BaseFont/C/Encoding/Identity-H/DescendantFonts[6 0 R]>>",
    );
    b.add_object(
        6,
        "<</Type/Font/Subtype/CIDFontType2/BaseFont/C/DW 1000/W[1[500 250]]>>",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    let g = &page.glyph_runs[0].glyphs;
    assert_eq!(g.len(), 3, "three 2-byte codes");
    assert_eq!((g[0].glyph, g[0].x), (1, 0.0));
    assert!((g[1].x - 5.0).abs() < 1e-6, "CID2 at {}", g[1].x);
    assert!((g[2].x - 7.5).abs() < 1e-6, "CID3 at {}", g[2].x);
}

#[test]
fn type0_embedded_cmap_decodes_variable_width_codes() {
    // A Type0 font whose /Encoding is an *embedded* CMap stream with a mixed
    // 1-/2-byte codespace and a distinguishable mapping: 1-byte 0x41 → CID 10,
    // 2-byte 0x8140/0x8141 → CID 20/21. The show string 41 81 40 81 41 must
    // decode to THREE codes [10, 20, 21] — not the two-byte Identity split
    // (0x4181, 0x4081, …) — proving the CMap drives byte splitting and code→CID.
    // /W: CID 10 → 500, CID 20 → 250, CID 21 → default 1000. At 10pt the
    // advances are 5.0, 2.5, 10.0 → glyphs at x = 0, 5, 7.5.
    let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
        /CMapName /Custom def /WMode 0 def\n\
        2 begincodespacerange <00> <80> <8100> <FFFF> endcodespacerange\n\
        1 begincidchar <41> 10 endcidchar\n\
        1 begincidrange <8140> <8141> 20 endcidrange\n\
        endcmap end end";
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
    b.add_stream(4, "", b"BT /F1 10 Tf 0 0 Td <418140 8141> Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type0/BaseFont/C/Encoding 7 0 R/DescendantFonts[6 0 R]>>",
    );
    b.add_object(6, "<</Type/Font/Subtype/CIDFontType2/BaseFont/C/CIDToGIDMap/Identity/DW 1000/W[10[500]20[250]]>>");
    b.add_stream(7, "/Type/CMap/CMapName/Custom", cmap);
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    let g = &page.glyph_runs[0].glyphs;
    assert_eq!(
        g.len(),
        3,
        "three variable-width codes, not the 2-byte Identity split"
    );
    assert_eq!(g[0].glyph, 10, "1-byte 0x41 → CID 10");
    assert_eq!(g[1].glyph, 20, "2-byte 0x8140 → CID 20");
    assert_eq!(g[2].glyph, 21, "2-byte 0x8141 → CID 20+1");
    assert_eq!(g[0].x, 0.0);
    assert!(
        (g[1].x - 5.0).abs() < 1e-6,
        "CID10 width 500 @10pt → 5.0, got {}",
        g[1].x
    );
    assert!(
        (g[2].x - 7.5).abs() < 1e-6,
        "CID20 width 250 @10pt → 2.5 more, got {}",
        g[2].x
    );
}

#[test]
fn type0_predefined_cmap_name_maps_to_cids() {
    // A Type0 font whose /Encoding names a predefined CMap (GBK-EUC-H). The
    // 2-byte code 0x8140 must map to CID 0x2758 (kGBK_EUC_H_2 row {0x8140,…}),
    // and the 1-byte code 0x20 to CID 0x1E24 — proving the predefined table is
    // wired, not treated as Identity.
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
    b.add_stream(4, "", b"BT /F1 10 Tf <208140> Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type0/BaseFont/C/Encoding/GBK-EUC-H/DescendantFonts[6 0 R]>>",
    );
    b.add_object(
        6,
        "<</Type/Font/Subtype/CIDFontType2/BaseFont/C/CIDToGIDMap/Identity/DW 1000>>",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    let g = &page.glyph_runs[0].glyphs;
    assert_eq!(g.len(), 2, "1-byte 0x20 then 2-byte 0x8140");
    assert_eq!(g[0].glyph, 0x1E24, "0x20 → CID 0x1E24");
    assert_eq!(g[1].glyph, 0x2758, "0x8140 → CID 0x2758");
}

#[test]
fn ext_gstate_smask_lowers_to_soft_mask_ops() {
    // ExtGState /SMask with a luminosity group → the compiled page brackets
    // the mask content with Begin/EndSoftMask, then the masked fill.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ExtGState<</GS0 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/GS0 gs 0 0 50 50 re f");
    b.add_object(5, "<</Type/ExtGState/SMask<</S/Luminosity/G 6 0 R>>>>");
    b.add_stream(
        6,
        "/Type/XObject/Subtype/Form/BBox[0 0 100 100]/Group<</S/Transparency>>",
        b"1 g 0 0 100 100 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    let has_begin = page
        .operations
        .iter()
        .any(|o| matches!(o, DisplayOp::BeginSoftMask { .. }));
    let has_end = page
        .operations
        .iter()
        .any(|o| matches!(o, DisplayOp::EndSoftMask));
    let has_origin = page.operations.iter().any(|o| {
        matches!(
            o,
            DisplayOp::BeginPaintOrigin(pdf_page_ir::PaintOrigin::SoftMaskContent)
        )
    });
    assert!(has_begin, "compiled page has BeginSoftMask");
    assert!(has_end, "compiled page has EndSoftMask");
    assert!(
        has_origin,
        "soft-mask content carries its attribution marker"
    );
    // The masked fill (50×50 rect) is still present after the mask content.
    assert!(
        page.operations
            .iter()
            .any(|o| matches!(o, DisplayOp::FillPath { .. }))
    );
}

#[test]
fn self_recursive_soft_mask_terminates_with_an_empty_inner_mask() {
    // A luminosity soft mask whose group's own content re-selects the same
    // /SMask (a self-cycle). It must not recurse to the invoke-depth limit:
    // the recursive instance renders empty (fully masking), so the page still
    // compiles and emits a bounded number of Begin/EndSoftMask brackets — one
    // for the outer mask, one empty for the self-reference. Guards against the
    // resvg self-referential-mask class.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ExtGState<</GS0 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/GS0 gs 0 0 50 50 re f");
    b.add_object(5, "<</Type/ExtGState/SMask<</S/Luminosity/G 6 0 R>>>>");
    // The mask group re-selects GS0 (whose SMask is this same group).
    b.add_stream(
        6,
        "/Type/XObject/Subtype/Form/BBox[0 0 100 100]/Group<</S/Transparency>>\
         /Resources<</ExtGState<</GS0 5 0 R>>>>",
        b"/GS0 gs 1 g 0 0 100 100 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    // Must not error (RecursionDepth) or hang.
    let page = compile(&open(b.into_bytes()));

    let begins = page
        .operations
        .iter()
        .filter(|o| matches!(o, DisplayOp::BeginSoftMask { .. }))
        .count();
    let ends = page
        .operations
        .iter()
        .filter(|o| matches!(o, DisplayOp::EndSoftMask))
        .count();
    assert_eq!(begins, ends, "balanced soft-mask brackets");
    // Outer mask + one empty recursive instance — bounded, not depth-limited.
    assert_eq!(begins, 2, "exactly the outer and one empty inner mask");
    assert!(
        page.operations
            .iter()
            .any(|o| matches!(o, DisplayOp::FillPath { .. }))
    );
}

#[test]
fn embedded_cidfont_program_and_gids_flow_to_the_ir() {
    // A Type0/Identity-H font with an embedded CIDFontType2 TrueType program
    // and Identity CIDToGIDMap. The compiled page must carry the program bytes
    // and map CID 1 → GID 1.
    let ttf = pdf_test_support::fonts::minimal_ttf();
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
    b.add_stream(4, "", b"BT /F1 10 Tf 0 0 Td <0001> Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type0/BaseFont/C/Encoding/Identity-H/DescendantFonts[6 0 R]>>",
    );
    b.add_object(6, "<</Type/Font/Subtype/CIDFontType2/BaseFont/C/CIDToGIDMap/Identity/DW 1000/FontDescriptor 7 0 R>>");
    b.add_object(7, "<</Type/FontDescriptor/FontName/C/FontFile2 8 0 R>>");
    b.add_stream(8, "", &ttf);
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    assert_eq!(page.fonts.len(), 1);
    assert!(
        !page.fonts[0].program.is_empty(),
        "embedded program carried to IR"
    );
    assert_eq!(
        page.glyph_runs[0].glyphs[0].glyph, 1,
        "CID 1 → GID 1 (Identity)"
    );
}

#[test]
fn simple_embedded_font_resolves_code_to_gid() {
    // A simple TrueType font with WinAnsiEncoding and an embedded program whose
    // cmap maps 'A' (0x41) → GID 1. Showing "A" must resolve to GID 1.
    let ttf = pdf_test_support::fonts::minimal_ttf();
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
    b.add_stream(4, "", b"BT /F1 10 Tf 0 0 Td (A) Tj ET");
    b.add_object(5, "<</Type/Font/Subtype/TrueType/BaseFont/C/Encoding/WinAnsiEncoding/FirstChar 65/Widths[500]/FontDescriptor 6 0 R>>");
    b.add_object(
        6,
        "<</Type/FontDescriptor/FontName/C/Flags 32/FontFile2 7 0 R>>",
    );
    b.add_stream(7, "", &ttf);
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()));

    assert!(
        !page.fonts[0].program.is_empty(),
        "simple embedded program carried"
    );
    assert_eq!(
        page.glyph_runs[0].glyphs[0].glyph, 1,
        "'A' → GID 1 via encoding+cmap"
    );
}
