#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;
use pdf_text::{CharType, TextPage, TextPageOptions};

fn extract(bytes: Vec<u8>) -> TextPage {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let document = DocumentSnapshot::open(source, DocumentLimits::default()).expect("open");
    let mut context = ParseContext::new();
    let page = PageCompiler::new()
        .compile_semantic(&document, PageIndex(0), &mut context)
        .expect("compile");
    TextPage::build(&page, &TextPageOptions::default())
}

fn simple_page(content: &[u8]) -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 300 200]>>",
    );
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    builder.add_stream(4, "", content);
    builder.add_object(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
    );
    builder.finish_classic_xref("/Root 1 0 R");
    builder.into_bytes()
}

#[test]
fn extracts_utf16_characters_rectangles_and_exact_words() {
    let page = extract(simple_page(b"BT /F1 20 Tf 20 100 Td (Illinois will) Tj ET"));
    assert_eq!(page.all_text(), "Illinois will");
    assert_eq!(page.char_count(), 13);
    assert!(page.has_text());
    assert!(
        page.chars()
            .iter()
            .all(|info| info.char_box.x1 >= info.char_box.x0)
    );

    let words = page.words();
    assert_eq!(words.len(), 2);
    assert_eq!(words[0].text, "Illinois");
    assert_eq!(words[1].text, "will");
    assert!(words[0].bbox.x1 < words[1].bbox.x0);
    assert_eq!(page.rects(0, page.char_count()).len(), 1);
}

#[test]
fn a_to_unicode_character_can_expand_to_multiple_utf16_units() {
    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    builder.add_stream(4, "", b"BT /F1 12 Tf 10 20 Td (A) Tj ET");
    builder.add_object(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/ToUnicode 6 0 R>>",
    );
    builder.add_stream(6, "", b"1 beginbfchar <41><00660069> endbfchar");
    builder.finish_classic_xref("/Root 1 0 R");

    let page = extract(builder.into_bytes());
    assert_eq!(page.all_text(), "fi");
    assert_eq!(page.char_count(), 2);
    assert_eq!(page.chars()[0].char_box, page.chars()[1].char_box);
}

#[test]
fn actual_text_replaces_a_scope_once_and_uses_piece_characters() {
    let page = extract(simple_page(
        b"BT /F1 12 Tf 10 20 Td \
          /Span <</ActualText <FEFF00660069>>> BDC (x) Tj (y) Tj EMC \
          (z) Tj ET",
    ));
    assert_eq!(page.all_text(), "fiz");
    assert_eq!(page.char_count(), 3);
    assert_eq!(page.chars()[0].char_type, CharType::Piece);
    assert_eq!(page.chars()[1].char_type, CharType::Piece);
    assert_eq!(page.chars()[2].char_type, CharType::Normal);
}

#[test]
fn form_concat_is_reflected_in_character_geometry() {
    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 300 200]>>",
    );
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources\
         <</XObject<</Fm 5 0 R>>/Font<</F1 6 0 R>>>>>>",
    );
    builder.add_stream(4, "", b"/Fm Do");
    builder.add_stream(
        5,
        "/Type/XObject/Subtype/Form/BBox[0 0 100 100]/Matrix[1 0 0 1 120 30]",
        b"BT /F1 10 Tf 5 7 Td (A) Tj ET",
    );
    builder.add_object(
        6,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
    );
    builder.finish_classic_xref("/Root 1 0 R");

    let page = extract(builder.into_bytes());
    assert_eq!(page.all_text(), "A");
    assert!((page.chars()[0].origin.x - 125.0).abs() < 0.01);
    assert!((page.chars()[0].origin.y - 37.0).abs() < 0.01);
}

#[test]
fn separate_text_objects_are_sorted_and_receive_generated_separators() {
    let page = extract(simple_page(
        b"BT /F1 12 Tf \
          1 0 0 1 120 100 Tm (B) Tj \
          1 0 0 1 10 100 Tm (A) Tj \
          1 0 0 1 10 70 Tm (C) Tj ET",
    ));
    assert_eq!(page.all_text(), "A B\r\nC");
    assert_eq!(
        page.chars()
            .iter()
            .filter(|info| info.char_type == CharType::Generated)
            .map(|info| info.unicode)
            .collect::<Vec<_>>(),
        vec![0x20, 0x0d, 0x0a]
    );
}

#[test]
fn pdf_doc_encoding_actual_text_is_decoded_and_normalized() {
    let page = extract(simple_page(
        b"BT /F1 12 Tf 10 20 Td /Span <</ActualText <93>>> BDC (x) Tj EMC ET",
    ));
    assert_eq!(page.all_text(), "fi");
    assert!(
        page.chars()
            .iter()
            .all(|info| info.char_type == CharType::Piece)
    );
}

#[test]
fn line_end_hyphen_joins_text_and_marks_the_continued_word() {
    let page = extract(simple_page(
        b"BT /F1 12 Tf 1 0 0 1 10 100 Tm (A-) Tj 1 0 0 1 10 70 Tm (B) Tj ET",
    ));
    assert_eq!(page.all_text(), "AB");
    assert!(
        page.chars()
            .iter()
            .any(|info| info.char_type == CharType::Hyphen)
    );
    let words = page.words();
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].text, "A-B");
    assert!(words[0].continued);
}

#[test]
fn type3_text_is_extracted_with_font_matrix_widths() {
    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 300 200]>>",
    );
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    builder.add_stream(4, "", b"BT /F1 100 Tf 10 20 Td (AB) Tj ET");
    builder.add_object(
        5,
        "<</Type/Font/Subtype/Type3/FontBBox[0 0 1000 1000]\
         /FontMatrix[0.001 0 0 0.001 0 0]/CharProcs 6 0 R/Encoding 7 0 R\
         /FirstChar 65/LastChar 66/Widths[1000 500]/Resources<<>>>>",
    );
    builder.add_object(6, "<</A 8 0 R/B 9 0 R>>");
    builder.add_object(7, "<</Type/Encoding/Differences[65/A/B]>>");
    builder.add_stream(8, "", b"1000 0 d0 0 0 100 100 re f");
    builder.add_stream(9, "", b"500 0 d0 0 0 50 50 re f");
    builder.finish_classic_xref("/Root 1 0 R");

    let page = extract(builder.into_bytes());
    assert_eq!(page.all_text(), "AB");
    assert_eq!(page.char_count(), 2);
    assert!((page.chars()[0].origin.x - 10.0).abs() < 0.01);
    assert!((page.chars()[1].origin.x - 110.0).abs() < 0.01);
    assert!(page.chars()[0].char_box.height() >= 99.0);
}

#[test]
fn rtl_segments_reverse_and_mirror_with_pdfium_rules() {
    let mut builder = PdfBuilder::new();
    builder.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    builder.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    builder.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    builder.add_stream(4, "", b"BT /F1 12 Tf 10 20 Td <0102> Tj ET");
    builder.add_object(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/ToUnicode 6 0 R>>",
    );
    builder.add_stream(6, "", b"2 beginbfchar <01><05D0> <02><0028> endbfchar");
    builder.finish_classic_xref("/Root 1 0 R");

    let page = extract(builder.into_bytes());
    assert_eq!(page.all_text(), ")\u{05d0}");
}
