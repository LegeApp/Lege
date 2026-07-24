#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Vertical writing mode (wmode 1 — ISO 32000-1 §9.7.4.3): `/W2`//`/DW2`
//! vertical metrics, the −y pen advance, and the `v`-vector glyph-origin
//! displacement (default `vx = w0/2`, `vy = /DW2[0]`).

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::CompiledPage;
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn compile(bytes: Vec<u8>) -> CompiledPage {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snap = DocumentSnapshot::open(source, DocumentLimits::default()).expect("open");
    let mut ctx = ParseContext::new();
    PageCompiler::new().compile(&snap, PageIndex(0), &mut ctx).expect("compile")
}

/// A one-page document with a Type 0 font `/F1` using `encoding` and the
/// given descendant CIDFont extras (`/W`, `/DW2`, `/W2` …), showing `content`.
fn doc(encoding: &str, cid_extras: &str, content: &[u8]) -> CompiledPage {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 500 800]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", content);
    b.add_object(
        5,
        &format!(
            "<</Type/Font/Subtype/Type0/BaseFont/Test/Encoding{encoding}\
             /DescendantFonts[6 0 R]>>"
        ),
    );
    b.add_object(
        6,
        &format!(
            "<</Type/Font/Subtype/CIDFontType2/BaseFont/Test\
             /CIDSystemInfo<</Registry(Adobe)/Ordering(Identity)/Supplement 0>>\
             /DW 1000{cid_extras}>>"
        ),
    );
    b.finish_classic_xref("/Root 1 0 R");
    compile(b.into_bytes())
}

#[test]
fn identity_v_lays_out_down_with_w2_and_v_vector() {
    // /W: CID 3 has w0 = 600 (feeds the default vx = w0/2).
    // /W2: CID 4 overrides w1y = −900, v = (300, 800).
    // Text: Tf 10, Td (100,700), show CIDs 3 then 4.
    let page = doc(
        "/Identity-V",
        "/W[3[600]]/DW2[880 -1000]/W2[4[-900 300 800]]",
        b"BT /F1 10 Tf 100 700 Td <00030004> Tj ET",
    );
    assert_eq!(page.glyph_runs.len(), 1);
    let run = &page.glyph_runs[0];
    assert_eq!(run.glyphs.len(), 2);

    // CID 3 (defaults, w0 = 600): drawn at pen − v·fs/1000 =
    // (−600/2/1000·10, 0 − 880/1000·10) = (−3, −8.8).
    assert!((run.glyphs[0].x - -3.0).abs() < 1e-9);
    assert!((run.glyphs[0].y - -8.8).abs() < 1e-9);
    // Pen then advances by DW2[1] = −1000 → cursor −10.
    // CID 4 (/W2 override): (−300/1000·10, −10 − 800/1000·10) = (−3, −18).
    assert!((run.glyphs[1].x - -3.0).abs() < 1e-9);
    assert!((run.glyphs[1].y - -18.0).abs() < 1e-9);

    // The run starts at the Td position.
    assert_eq!((run.transform.e, run.transform.f), (100.0, 700.0));
}

#[test]
fn consecutive_vertical_shows_stack_downward() {
    // Two Tj in a row: the second must start where the first ended —
    // 2 glyphs × (−1000/1000·10) = −20 in y, x unchanged.
    let page = doc(
        "/Identity-V",
        "/DW2[880 -1000]",
        b"BT /F1 10 Tf 100 700 Td <00030004> Tj <0005> Tj ET",
    );
    assert_eq!(page.glyph_runs.len(), 2);
    assert_eq!(
        (page.glyph_runs[0].transform.e, page.glyph_runs[0].transform.f),
        (100.0, 700.0)
    );
    assert_eq!(
        (page.glyph_runs[1].transform.e, page.glyph_runs[1].transform.f),
        (100.0, 680.0)
    );
}

#[test]
fn tj_adjustments_move_along_y_unscaled_by_th() {
    // TJ with a −500 adjustment at Tz 50 (Th = 0.5): vertical adjustments are
    // NOT scaled by Th → +5 in y after a −10 glyph advance (advance also
    // unscaled by Th).
    let page = doc(
        "/Identity-V",
        "/DW2[880 -1000]",
        b"BT /F1 10 Tf 50 Tz 100 700 Td [<0003> -500 <0004>] TJ ET",
    );
    let run = &page.glyph_runs[0];
    assert_eq!(run.glyphs.len(), 2);
    // Glyph 2 pen y: −10 (advance) + 5 (adjust −(−500)/1000·10) = −5;
    // drawn at −5 − 8.8 = −13.8.
    assert!((run.glyphs[1].y - -13.8).abs() < 1e-9);
    // vx displacement IS horizontal → scaled by Th: −(1000/2)/1000·10·0.5 = −2.5.
    assert!((run.glyphs[1].x - -2.5).abs() < 1e-9);
}

#[test]
fn predefined_v_cmap_selects_vertical_layout() {
    // UniGB-UCS2-V: a predefined vertical CMap (wmode 1). Defaults only —
    // glyphs must stack along −y (pen 0, −10) with the default v vector.
    let page = doc(
        "/UniGB-UCS2-V",
        "/DW2[880 -1000]",
        b"BT /F1 10 Tf 100 700 Td <30003001> Tj ET",
    );
    assert_eq!(page.glyph_runs.len(), 1);
    let run = &page.glyph_runs[0];
    assert_eq!(run.glyphs.len(), 2);
    assert!((run.glyphs[0].y - -8.8).abs() < 1e-9);
    assert!((run.glyphs[1].y - -18.8).abs() < 1e-9);
}

#[test]
fn identity_h_layout_is_unchanged() {
    let page = doc(
        "/Identity-H",
        "/W[3[600]]",
        b"BT /F1 10 Tf 100 700 Td <00030004> Tj ET",
    );
    let run = &page.glyph_runs[0];
    assert_eq!(run.glyphs.len(), 2);
    assert_eq!((run.glyphs[0].x, run.glyphs[0].y), (0.0, 0.0));
    // CID 3 advanced 600/1000·10 = 6 horizontally.
    assert_eq!((run.glyphs[1].x, run.glyphs[1].y), (6.0, 0.0));
}
