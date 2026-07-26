#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Optional content (ISO 32000-1 §8.11; PDFium cpdf_occontext.cpp
//! default-visibility semantics): the catalog's `/OCProperties /D` config
//! (`/BaseState`, `/ON`, `/OFF`), `BDC /OC … EMC` span suppression, `/OC` on
//! image and form XObjects, and `/OCMD` any-on membership.

use std::sync::Arc;

use pdf_content::semantic::{SemColor, SemanticOp};
use pdf_content::{PageCompiler, SemanticPage};
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn compile(bytes: Vec<u8>) -> SemanticPage {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snap = DocumentSnapshot::open(source, DocumentLimits::default()).expect("open");
    let mut ctx = ParseContext::new();
    PageCompiler::new()
        .compile_semantic(&snap, PageIndex(0), &mut ctx)
        .expect("compile")
}

/// Two OCGs — 10 (`L1`) and 11 (`L2`) — under the given `/D` config, with
/// `content` and default `/Properties` naming both.
fn doc(d_config: &str, content: &[u8]) -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(
        1,
        &format!("<</Type/Catalog/Pages 2 0 R/OCProperties<</OCGs[10 0 R 11 0 R]/D{d_config}>>>>"),
    );
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<\
         /Properties<</L1 10 0 R/L2 11 0 R/M1 12 0 R>>\
         /XObject<</ImA 20 0 R/ImB 21 0 R/FmB 22 0 R>>>>>>",
    );
    b.add_stream(4, "", content);
    b.add_object(10, "<</Type/OCG/Name(LayerOne)>>");
    b.add_object(11, "<</Type/OCG/Name(LayerTwo)>>");
    // Membership dict over both layers: visible if ANY is on.
    b.add_object(12, "<</Type/OCMD/OCGs[10 0 R 11 0 R]>>");
    // Images tied to each layer via /OC.
    b.add_stream(
        20,
        "/Type/XObject/Subtype/Image/Width 1/Height 1/BitsPerComponent 8/ColorSpace/DeviceGray/OC 10 0 R",
        &[0u8],
    );
    b.add_stream(
        21,
        "/Type/XObject/Subtype/Image/Width 1/Height 1/BitsPerComponent 8/ColorSpace/DeviceGray/OC 11 0 R",
        &[0u8],
    );
    // A form tied to layer two.
    b.add_stream(
        22,
        "/Type/XObject/Subtype/Form/BBox[0 0 50 50]/OC 11 0 R",
        b"0 0 1 rg 0 0 50 50 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

fn fills(page: &SemanticPage) -> usize {
    page.ops
        .iter()
        .filter(|o| matches!(o, SemanticOp::Fill { .. }))
        .count()
}

#[test]
fn off_layer_bdc_span_is_suppressed() {
    let page = compile(doc(
        "<</OFF[11 0 R]>>",
        b"/OC /L1 BDC 1 0 0 rg 0 0 50 50 re f EMC \
          /OC /L2 BDC 0 1 0 rg 50 50 60 60 re f EMC",
    ));
    // Only the L1 fill survives, and it paints red. (The L2 span's `rg` is
    // graphics *state* and legitimately persists — only its marks vanish.)
    assert_eq!(fills(&page), 1);
    let mut current: Option<&SemColor> = None;
    let mut painted = Vec::new();
    for op in page.ops.iter() {
        match op {
            SemanticOp::SetFillColor(c) => current = Some(c),
            SemanticOp::Fill { .. } => painted.extend(current),
            _ => {}
        }
    }
    assert!(matches!(painted[..], [SemColor::DeviceRgb(r, ..)] if *r == 1.0));
}

#[test]
fn off_layer_text_is_retained_for_extraction_but_not_painting() {
    let mut b = PdfBuilder::new();
    b.add_object(
        1,
        "<</Type/Catalog/Pages 2 0 R/OCProperties<</OCGs[10 0 R]/D<</OFF[10 0 R]>>>>>>",
    );
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources\
         <</Properties<</L1 10 0 R>>/Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(
        4,
        "",
        b"BT /F1 12 Tf /OC /L1 BDC (hidden) Tj EMC (visible) Tj ET",
    );
    b.add_object(5, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>");
    b.add_object(10, "<</Type/OCG/Name(LayerOne)>>");
    b.finish_classic_xref("/Root 1 0 R");

    let page = compile(b.into_bytes());
    assert_eq!(page.text_runs.len(), 2);
    assert!(!page.text_runs[0].visible);
    assert!(page.text_runs[1].visible);
    assert_eq!(
        page.ops
            .iter()
            .filter(|op| matches!(op, SemanticOp::ShowText(_)))
            .count(),
        2,
        "both text objects remain in the semantic stream"
    );
}

#[test]
fn unconfigured_layers_default_to_visible() {
    let page = compile(doc(
        "<<>>",
        b"/OC /L1 BDC 1 0 0 rg 0 0 50 50 re f EMC \
          /OC /L2 BDC 0 1 0 rg 50 50 60 60 re f EMC",
    ));
    assert_eq!(fills(&page), 2);
}

#[test]
fn base_state_off_hides_all_but_on_list() {
    let page = compile(doc(
        "<</BaseState/OFF/ON[10 0 R]>>",
        b"/OC /L1 BDC 1 0 0 rg 0 0 50 50 re f EMC \
          /OC /L2 BDC 0 1 0 rg 50 50 60 60 re f EMC",
    ));
    assert_eq!(fills(&page), 1);
}

#[test]
fn oc_on_image_xobjects_is_honored() {
    let page = compile(doc(
        "<</OFF[11 0 R]>>",
        b"q 10 0 0 10 0 0 cm /ImA Do /ImB Do Q",
    ));
    // ImA (layer one, on) draws; ImB (layer two, off) is suppressed.
    assert_eq!(page.images.len(), 1);
    assert_eq!(
        page.ops
            .iter()
            .filter(|o| matches!(o, SemanticOp::DrawImage(_)))
            .count(),
        1
    );
}

#[test]
fn oc_on_form_xobjects_is_honored() {
    let off = compile(doc("<</OFF[11 0 R]>>", b"/FmB Do"));
    assert_eq!(fills(&off), 0);
    let on = compile(doc("<<>>", b"/FmB Do"));
    assert_eq!(fills(&on), 1);
}

#[test]
fn ocmd_is_visible_when_any_member_is_on() {
    // Layer two off, layer one on → the OCMD (over both) stays visible.
    let page = compile(doc(
        "<</OFF[11 0 R]>>",
        b"/OC /M1 BDC 1 0 0 rg 0 0 50 50 re f EMC",
    ));
    assert_eq!(fills(&page), 1);
    // Both off → hidden.
    let page = compile(doc(
        "<</OFF[10 0 R 11 0 R]>>",
        b"/OC /M1 BDC 1 0 0 rg 0 0 50 50 re f EMC",
    ));
    assert_eq!(fills(&page), 0);
}

#[test]
fn hidden_span_still_applies_clips_and_state() {
    // The hidden span sets a clip (W n): the clip must survive (state, not
    // marks), and the following visible fill must still be emitted.
    let page = compile(doc(
        "<</OFF[11 0 R]>>",
        b"/OC /L2 BDC 0 0 20 20 re W n EMC 1 0 0 rg 0 0 50 50 re f",
    ));
    assert_eq!(fills(&page), 1);
    assert!(
        page.ops
            .iter()
            .any(|o| matches!(o, SemanticOp::Clip { .. }))
    );
}

#[test]
fn hidden_text_is_dropped_but_still_advances() {
    // No font resource needed to check suppression: with no /Font the run is
    // dropped anyway — so use a path fill after EMC as the canary that
    // interpretation continued cleanly past nested BMC/EMC.
    let page = compile(doc(
        "<</OFF[11 0 R]>>",
        b"/OC /L2 BDC /Junk BMC 0 1 0 rg 0 0 9 9 re f EMC EMC 1 0 0 rg 0 0 50 50 re f",
    ));
    // The nested BMC level inherits the hidden span; only the trailing red
    // fill lands.
    assert_eq!(fills(&page), 1);
}
