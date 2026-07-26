#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! CJK substitution for non-embedded CID fonts (fonts.md Font Phase 7 + B3
//! tables): with the opt-in system-font provider, a known-charset CID font
//! with no embedded program resolves to an installed face and its CIDs reach
//! glyphs via CID → Unicode (Adobe-*-UCS2) → the face's charmap. With the
//! provider off, behavior is unchanged (deterministic identity mapping).

use std::path::PathBuf;
use std::sync::Arc;

use pdf_content::{PageCompiler, SemanticPage};
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_font::{CidToGid, FolderFontProvider, GlyphMap};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

/// A one-page document with a non-embedded Type 0 font: `/Encoding
/// /UniGB-UCS2-H`, descendant `/CIDSystemInfo` Ordering GB1, `/BaseFont`
/// `family` (PDF-name-escaped).
fn doc(family: &str) -> Vec<u8> {
    let name = family.replace(' ', "#20");
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 500 500]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    // Code 0x4E00 (一) → GB1 CID 4162 through UniGB-UCS2-H.
    b.add_stream(4, "", b"BT /F1 12 Tf 100 400 Td <4E00> Tj ET");
    b.add_object(
        5,
        &format!(
            "<</Type/Font/Subtype/Type0/BaseFont/{name}/Encoding/UniGB-UCS2-H\
             /DescendantFonts[6 0 R]>>"
        ),
    );
    b.add_object(
        6,
        &format!(
            "<</Type/Font/Subtype/CIDFontType2/BaseFont/{name}\
             /CIDSystemInfo<</Registry(Adobe)/Ordering(GB1)/Supplement 5>>/DW 1000>>"
        ),
    );
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

fn compile(bytes: Vec<u8>, provider: Option<Arc<FolderFontProvider>>) -> SemanticPage {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snap = DocumentSnapshot::open(source, DocumentLimits::default()).expect("open");
    let mut ctx = ParseContext::new();
    let mut compiler = PageCompiler::new();
    if let Some(p) = provider {
        compiler = compiler.with_system_fonts(p);
    }
    compiler
        .compile_semantic(&snap, PageIndex(0), &mut ctx)
        .expect("compile")
}

#[test]
fn provider_off_keeps_identity_cid_mapping() {
    // The deterministic default: no provider → bundled face, CID→GID stays
    // exactly what the document said (Identity here), no Unicode bridge.
    let page = compile(doc("SimSun"), None);
    assert_eq!(page.fonts.len(), 1);
    assert!(
        matches!(&*page.fonts[0].glyph_map, GlyphMap::Cid(CidToGid::Identity)),
        "provider-off mapping must stay identity, got {:?}",
        page.fonts[0].glyph_map
    );
}

#[test]
fn provider_on_bridges_cid_through_unicode_to_installed_face() {
    // Point the provider at the OS font folder and use whichever CJK-capable
    // family is installed; skip gracefully on a machine with none.
    let provider = Arc::new(FolderFontProvider::with_paths(&[PathBuf::from(
        "C:\\Windows\\Fonts",
    )]));
    let family = ["SimSun", "Microsoft YaHei", "NSimSun", "SimHei"]
        .into_iter()
        .find(|f| provider.has_family(f));
    let Some(family) = family else {
        eprintln!("skipping: no simplified-Chinese family installed");
        return;
    };

    let page = compile(doc(family), Some(provider));
    assert_eq!(page.fonts.len(), 1);
    let font = &page.fonts[0];
    assert!(font.program.is_some(), "system face resolved");
    // The bridge produced a real CID→GID map, and GB1 CID 4162 (一, U+4E00 —
    // Adobe-GB1-UCS2_5.inc index 4162 = 0x4E00) reaches a non-.notdef glyph.
    match &*font.glyph_map {
        GlyphMap::Cid(map @ CidToGid::Map(_)) => {
            assert_ne!(map.gid(4162), 0, "CID 4162 (U+4E00) must map to a glyph");
        }
        other => panic!("expected a bridged CID→GID map, got {other:?}"),
    }
    // And the metrics still decode the code through UniGB-UCS2-H.
    let decoded = font.metrics.decode(&[0x4E, 0x00]);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].cid, 4162);
}
