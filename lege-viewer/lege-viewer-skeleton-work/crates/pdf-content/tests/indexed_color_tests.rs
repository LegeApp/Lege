#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! `sc`/`scn` in an `/Indexed` fill colour space (ISO 32000-1 §8.6.6.3): the
//! operand is a palette *index*, not colour components. Reading it as
//! components mis-paints every fill black — pdfbox/2561 page 3 collapsed its
//! red/green/blue bars to a single black bar (the unrenderable colour also
//! dropped two of the three fills).

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{Color, CompiledPage, Paint};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn compile(bytes: Vec<u8>) -> CompiledPage {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snap = DocumentSnapshot::open(source, DocumentLimits::default()).expect("open");
    let mut ctx = ParseContext::new();
    PageCompiler::new().compile(&snap, PageIndex(0), &mut ctx).expect("compile")
}

fn page_with(colorspace: &str, content: &[u8]) -> CompiledPage {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ColorSpace<</Cs 5 0 R>>>>>>",
    );
    b.add_stream(4, "", content);
    b.add_object(5, colorspace);
    b.finish_classic_xref("/Root 1 0 R");
    compile(b.into_bytes())
}

fn solids(page: &CompiledPage) -> Vec<Color> {
    page.paints
        .iter()
        .filter_map(|p| match p {
            Paint::Solid(c) => Some(*c),
            _ => None,
        })
        .collect()
}

fn near(c: Color, r: f32, g: f32, b: f32) -> bool {
    (c.r - r).abs() < 0.02 && (c.g - g).abs() < 0.02 && (c.b - b).abs() < 0.02
}

/// `[/Indexed /DeviceRGB 2 <red green blue>]`: index 0 = red, 1 = green,
/// 2 = blue.
const INDEXED_RGB: &str = "[/Indexed/DeviceRGB 2<FF000000FF000000FF>]";

#[test]
fn indexed_sc_looks_up_the_palette_entry() {
    // Three fills, one per palette index, must paint red/green/blue — not a
    // single black bar.
    let page = page_with(
        INDEXED_RGB,
        b"/Cs cs 0 sc 0 0 10 10 re f 1 sc 0 20 10 10 re f 2 sc 0 40 10 10 re f",
    );
    let colors = solids(&page);
    assert!(
        colors.iter().any(|c| near(*c, 1.0, 0.0, 0.0)),
        "index 0 -> red, got {colors:?}"
    );
    assert!(
        colors.iter().any(|c| near(*c, 0.0, 1.0, 0.0)),
        "index 1 -> green, got {colors:?}"
    );
    assert!(
        colors.iter().any(|c| near(*c, 0.0, 0.0, 1.0)),
        "index 2 -> blue, got {colors:?}"
    );
    // And none of them collapsed to black.
    assert!(
        !colors.iter().any(|c| near(*c, 0.0, 0.0, 0.0)),
        "no fill should be black, got {colors:?}"
    );
}
