#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 2 interpreter tests: operator semantics → `SemanticPage`, verified
//! through the deterministic dump plus a few structural assertions.

use std::sync::Arc;

use pdf_content::dump::dump_semantic;
use pdf_content::semantic::{SemColor, SemanticOp, TextElement};
use pdf_content::{PageCompiler, SemanticPage};
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::{self, PdfBuilder};

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

/// A one-page document with the given content stream and an `/F1` Helvetica.
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

fn compile(snapshot: &DocumentSnapshot, page: u32) -> SemanticPage {
    let mut ctx = ParseContext::new();
    PageCompiler::new()
        .compile_semantic(snapshot, PageIndex(page), &mut ctx)
        .expect("compile failed")
}

fn dump(snapshot: &DocumentSnapshot, page: &SemanticPage) -> String {
    dump_semantic(page, snapshot.names())
}

#[test]
fn pre_cancelled_compilation_stops_before_page_interpretation() {
    let snapshot = single_page(b"0 0 200 200 re f");
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut context = ParseContext::new();
    context.set_cancellation_flag(Some(cancelled));

    let error = PageCompiler::new()
        .compile_artifacts(&snapshot, PageIndex(0), &mut context)
        .expect_err("pre-cancelled compilation must stop");
    assert!(matches!(error, pdf_content::ContentError::Cancelled));
}

#[test]
fn truncated_content_stream_compiles_to_blank_not_error() {
    // A /FlateDecode content stream whose body is not valid deflate: decoding
    // fails, but the page must still compile (blank) rather than dropping the
    // whole page. Real books carry deliberately-blank/truncated pages and
    // viewers (Poppler/PDFium) render them; we must too.
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
    b.add_stream(4, "/Filter/FlateDecode", b"\xff\xff\xff\xff not deflate");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());

    let page = compile(&snap, 0);
    assert!(
        page.ops.is_empty(),
        "a truncated content stream should yield a blank page"
    );
}

#[test]
fn one_bad_content_stream_keeps_the_good_ones() {
    // /Contents as an array: a valid stream then a corrupt one. The valid
    // stream's marks survive; only the corrupt stream is skipped.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents[4 0 R 5 0 R]/Resources<<>>>>",
    );
    b.add_stream(4, "", b"0.2 0.3 0.4 rg 10 20 30 40 re f");
    b.add_stream(5, "/Filter/FlateDecode", b"\xff\xff\xff\xff");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());

    let page = compile(&snap, 0);
    assert_eq!(
        page.ops.len(),
        2,
        "the valid stream's ops must survive the corrupt one"
    );
}

/// A one-page document whose `/ColorSpace` resource `/CS0` is `space_body`,
/// with the given content stream.
fn page_with_colorspace(space_body: &str, content: &[u8]) -> DocumentSnapshot {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ColorSpace<</CS0 5 0 R>>>>>>",
    );
    b.add_stream(4, "", content);
    b.add_object(5, space_body);
    b.finish_classic_xref("/Root 1 0 R");
    open(b.into_bytes())
}

#[test]
fn lab_fill_converts_through_cie_math() {
    // L*=100, a*=b*=0 is diffuse white; L*=0 is black (ISO 32000-1 §8.6.5.4).
    // Before the CIE wiring these landed as Components and were treated as
    // raw RGB — i.e. L*=100 clamped to full red-ish garbage.
    let snap = page_with_colorspace(
        "[/Lab <</WhitePoint[0.9505 1 1.089]>>]",
        b"/CS0 cs 100 0 0 sc 0 0 10 10 re f",
    );
    // `cs` emits the space's initial colour first; the `sc` colour is the
    // LAST SetFillColor before the fill.
    let last_fill = |page: &SemanticPage| {
        page.ops
            .iter()
            .filter_map(|op| match op {
                SemanticOp::SetFillColor(c) => Some(c.clone()),
                _ => None,
            })
            .last()
    };
    let page = compile(&snap, 0);
    let Some(SemColor::DeviceRgb(r, g, b)) = last_fill(&page) else {
        panic!("expected an RGB fill from Lab, got {:?}", last_fill(&page));
    };
    assert!(
        r > 0.98 && g > 0.98 && b > 0.98,
        "Lab white -> RGB white, got {r} {g} {b}"
    );

    let snap = page_with_colorspace(
        "[/Lab <</WhitePoint[0.9505 1 1.089]>>]",
        b"/CS0 cs 0 0 0 sc 0 0 10 10 re f",
    );
    let page = compile(&snap, 0);
    let Some(SemColor::DeviceRgb(r, g, b)) = last_fill(&page) else {
        panic!("expected an RGB fill from Lab");
    };
    assert!(
        r < 0.02 && g < 0.02 && b < 0.02,
        "Lab L*=0 -> black, got {r} {g} {b}"
    );
}

#[test]
fn calgray_fill_is_pass_through_gray() {
    let snap = page_with_colorspace(
        "[/CalGray <</WhitePoint[0.9505 1 1.089]/Gamma 2.2>>]",
        b"/CS0 cs 0.5 sc 0 0 10 10 re f",
    );
    let page = compile(&snap, 0);
    let gray = page
        .ops
        .iter()
        .filter_map(|op| match op {
            SemanticOp::SetFillColor(SemColor::DeviceGray(g)) => Some(*g),
            _ => None,
        })
        .last();
    let Some(g) = gray else {
        panic!("expected a gray fill from CalGray");
    };
    assert!(
        (g - 0.5).abs() < 1e-6,
        "CalGray is pass-through per PDFium, got {g}"
    );
}

#[test]
fn fill_rectangle_with_rgb() {
    let snap = single_page(b"0.2 0.3 0.4 rg 10 20 30 40 re f");
    let page = compile(&snap, 0);

    assert_eq!(page.ops.len(), 2);
    assert!(matches!(
        page.ops[0],
        SemanticOp::SetFillColor(SemColor::DeviceRgb(..))
    ));
    assert!(matches!(page.ops[1], SemanticOp::Fill { .. }));
    assert_eq!(page.paths.len(), 1);
    // re → MoveTo, 3×LineTo, Close = 5 verbs, 4 points.
    assert_eq!(page.paths[0].verbs.len(), 5);
    assert_eq!(page.paths[0].points.len(), 4);

    let text = dump(&snap, &page);
    assert!(text.contains("set-fill DeviceRGB(0.2, 0.3, 0.4)"), "{text}");
    assert!(text.contains("fill path#0 nonzero"), "{text}");
    assert!(text.starts_with("page [0 0 200 200] rotate 0\n"), "{text}");
}

#[test]
fn save_concat_stroke_restore() {
    let snap = single_page(b"q 1 0 0 1 20 40 cm 100 100 m 150 150 l S Q");
    let page = compile(&snap, 0);
    let text = dump(&snap, &page);
    let expected = "\
page [0 0 200 200] rotate 0
  save
  concat [1 0 0 1 20 40]
  stroke path#0
  restore
paths:
  path#0 verbs=2 points=2
";
    assert_eq!(text, expected);
}

#[test]
fn clip_with_w_n_idiom() {
    let snap = single_page(b"0 0 100 100 re W n");
    let page = compile(&snap, 0);
    assert_eq!(page.ops.len(), 1);
    assert!(matches!(page.ops[0], SemanticOp::Clip { .. }));
    let text = dump(&snap, &page);
    assert!(text.contains("clip path#0 nonzero"), "{text}");
}

#[test]
fn text_clip_mode_emits_cliptext_at_et_before_following_fill() {
    // A clip-only text object (Tr 7) followed by a full-page fill: the glyph
    // outlines must become a clip (emitted at ET) that precedes — and so
    // constrains — the fill (ISO 32000-1 §9.4.3). Cover 2.pdf's white overlays
    // are painted exactly this way; without the clip they flood the page.
    let snap = single_page(b"BT /F1 12 Tf 72 720 Td 7 Tr (Hi) Tj ET 0 0 200 200 re f");
    let page = compile(&snap, 0);

    // ShowText (the run, invisible in mode 7) → ClipText (at ET) → Fill.
    let cliptext = page
        .ops
        .iter()
        .position(|o| matches!(o, SemanticOp::ClipText { .. }));
    let fill = page
        .ops
        .iter()
        .position(|o| matches!(o, SemanticOp::Fill { .. }));
    let cliptext = cliptext.expect("ET of a Tr-7 text object must emit ClipText");
    let fill = fill.expect("the trailing fill must survive");
    assert!(
        cliptext < fill,
        "the text clip must precede the fill it constrains"
    );
    match &page.ops[cliptext] {
        SemanticOp::ClipText { runs } => assert_eq!(runs.len(), 1, "one clip-mode run"),
        _ => unreachable!(),
    }
    let text = dump(&snap, &page);
    assert!(text.contains("clip-text [run#0]"), "{text}");
}

#[test]
fn non_clip_text_mode_emits_no_cliptext() {
    // Plain fill text (Tr 0, the default) must NOT accumulate a text clip.
    let snap = single_page(b"BT /F1 12 Tf 72 720 Td (Hi) Tj ET 0 0 200 200 re f");
    let page = compile(&snap, 0);
    assert!(
        !page
            .ops
            .iter()
            .any(|o| matches!(o, SemanticOp::ClipText { .. })),
        "non-clip render modes must not emit a text clip"
    );
}

#[test]
fn even_odd_fill_rule() {
    let snap = single_page(b"0 0 10 10 re f*");
    let page = compile(&snap, 0);
    let text = dump(&snap, &page);
    assert!(text.contains("fill path#0 evenodd"), "{text}");
}

#[test]
fn text_show_produces_run_and_font() {
    let snap = single_page(b"BT /F1 12 Tf 72 720 Td (Hello) Tj ET");
    let page = compile(&snap, 0);

    assert_eq!(page.ops.len(), 1);
    assert!(matches!(page.ops[0], SemanticOp::ShowText(_)));
    assert_eq!(page.fonts.len(), 1);
    assert_eq!(page.fonts[0].base_font, b"Helvetica");
    assert_eq!(page.fonts[0].subtype, b"Type1");
    assert_eq!(page.text_runs.len(), 1);
    assert_eq!(page.text_runs[0].font_size, 12.0);
    assert_eq!(page.text_runs[0].text_matrix.e, 72.0);
    assert_eq!(page.text_runs[0].text_matrix.f, 720.0);
    assert_eq!(
        page.text_runs[0].elements.as_ref(),
        &[TextElement::Show(b"Hello".to_vec())][..]
    );

    let text = dump(&snap, &page);
    assert!(text.contains("show-text run#0"), "{text}");
    assert!(
        text.contains("font#0 /F1 Type1 /Helvetica obj 5 0"),
        "{text}"
    );
    assert!(text.contains("show \"Hello\""), "{text}");
}

#[test]
fn tj_array_interleaves_shows_and_adjustments() {
    let snap = single_page(b"BT /F1 10 Tf 0 0 Td [(A) -120 (B)] TJ ET");
    let page = compile(&snap, 0);
    assert_eq!(page.text_runs.len(), 1);
    assert_eq!(
        page.text_runs[0].elements.as_ref(),
        &[
            TextElement::Show(b"A".to_vec()),
            TextElement::Adjust(-120.0),
            TextElement::Show(b"B".to_vec()),
        ][..]
    );
}

#[test]
fn consecutive_show_operators_advance_the_text_matrix() {
    // ISO 32000-1 §9.4.4: a show operator advances the text position by the
    // glyphs' widths, so a *following* show that does not reposition starts
    // where the previous one ended. Producers (Leibniz/EarlyModernTexts,
    // Capital One, CNKI) emit one `Tj` per word/glyph and rely on this; without
    // it every run piles onto the same origin and the line collapses into an
    // unreadable column.
    let snap = single_page(b"BT /F1 10 Tf 0 0 Td (A) Tj (A) Tj (A) Tj ET");
    let page = compile(&snap, 0);
    assert_eq!(page.text_runs.len(), 3);

    let e: Vec<f64> = page.text_runs.iter().map(|r| r.text_matrix.e).collect();
    assert_eq!(e[0], 0.0, "first run starts at the line origin");
    assert!(
        e[1] > 0.0,
        "the second run is advanced past the first (was 0 before the fix)"
    );
    // Equal glyph, equal advance: the three origins are evenly spaced.
    let step1 = e[1] - e[0];
    let step2 = e[2] - e[1];
    assert!((step1 - step2).abs() < 1e-6, "advances are uniform: {e:?}");
    // 'A' in Helvetica is ~667/1000 em ⇒ ~6.67 units at 10 pt.
    assert!(
        (step1 - 6.67).abs() < 0.5,
        "advance ≈ glyph width·Tfs/1000: {step1}"
    );
}

#[test]
fn char_and_word_spacing_widen_the_run_advance() {
    // Tc (char spacing) applies to every glyph; Tw (word spacing) to code 32.
    // Both feed the text-position advance, matching within-run glyph placement.
    let base = single_page(b"BT /F1 10 Tf 0 0 Td (A B) Tj (X) Tj ET");
    let spaced = single_page(b"BT /F1 10 Tf 5 Tc 3 Tw 0 0 Td (A B) Tj (X) Tj ET");
    let e_base = compile(&base, 0).text_runs[1].text_matrix.e;
    let e_spaced = compile(&spaced, 0).text_runs[1].text_matrix.e;
    // Three glyphs ⇒ +3·Tc; one space ⇒ +1·Tw ⇒ 15 + 3 = 18 extra units.
    assert!(
        (e_spaced - e_base - 18.0).abs() < 1e-6,
        "base {e_base} spaced {e_spaced}"
    );
}

#[test]
fn q_restores_graphics_state() {
    // Fill color set inside q…Q must not leak past the restore.
    let snap = single_page(b"q 1 0 0 rg 0 0 10 10 re f Q 0 0 5 5 re f");
    let page = compile(&snap, 0);
    // Two fills; the second uses the default (black DeviceGray) color, since
    // the red set inside q…Q was popped.
    let fills: Vec<_> = page
        .ops
        .iter()
        .filter(|o| matches!(o, SemanticOp::SetFillColor(_)))
        .collect();
    // Only one explicit SetFillColor op was emitted (the red inside q…Q).
    assert_eq!(fills.len(), 1);
    // But state restored: nothing re-emits black; the renderer replays the
    // Restore. Confirm the op stream ends with a plain fill and a Restore
    // occurred before it.
    let text = dump(&snap, &page);
    assert!(text.contains("restore"), "{text}");
}

#[test]
fn unbalanced_q_is_tolerated() {
    let snap = single_page(b"Q Q 0 0 10 10 re f");
    let page = compile(&snap, 0);
    // The stray Q operators are ignored; the fill still compiles.
    assert!(
        page.ops
            .iter()
            .any(|o| matches!(o, SemanticOp::Fill { .. }))
    );
}

#[test]
fn content_stream_array_is_concatenated() {
    // /Contents as an array of two streams: color in the first, path+paint in
    // the second. A token must not span the boundary — the interpreter joins
    // with a newline.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 200 200]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents[4 0 R 5 0 R]/Resources<<>>>>",
    );
    b.add_stream(4, "", b"1 0 0 rg");
    b.add_stream(5, "", b"0 0 10 10 re f");
    b.add_object(6, "<</unused 0>>");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);
    assert!(matches!(
        page.ops[0],
        SemanticOp::SetFillColor(SemColor::DeviceRgb(..))
    ));
    assert!(
        page.ops
            .iter()
            .any(|o| matches!(o, SemanticOp::Fill { .. }))
    );
}

#[test]
fn compiles_phase1_fixture_pages() {
    // The Phase 1 fixtures carry real text content; every page must compile
    // to a single text run naming Helvetica.
    let snap = open(builder::phase1_exit_fixture(6));
    for i in 0..6 {
        let page = compile(&snap, i);
        assert_eq!(page.fonts.len(), 1, "page {i}");
        assert_eq!(page.fonts[0].base_font, b"Helvetica", "page {i}");
        assert_eq!(page.text_runs.len(), 1, "page {i}");
        // Page 0 was rewritten by the incremental update.
        let shown = &page.text_runs[0].elements;
        let expect: &[u8] = if i == 0 { b"Updated page 0" } else { b"Page 0" };
        let got = match &shown[0] {
            TextElement::Show(s) => s.clone(),
            _ => panic!("expected a show element"),
        };
        if i == 0 {
            assert_eq!(got, expect);
        } else {
            assert_eq!(&got[..5], b"Page ");
        }
    }
}

#[test]
fn to_unicode_is_owned_by_the_semantic_font() {
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
    b.add_stream(4, "", b"BT /F1 12 Tf (A) Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/ToUnicode 6 0 R>>",
    );
    b.add_stream(
        6,
        "",
        b"1 begincodespacerange <00><FF> endcodespacerange \
          1 beginbfchar <41><03A9> endbfchar",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    let mapping = page.fonts[0].unicode_map.get(0x41).expect("mapping");
    assert_eq!(&*mapping.utf16, &[0x03a9]);
    assert_eq!(mapping.source, pdf_font::UnicodeSource::ToUnicode);
}

#[test]
fn actual_text_scope_identity_is_retained_across_show_operations() {
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
    b.add_stream(
        4,
        "",
        b"BT /F1 12 Tf \
          /Span <</ActualText <FEFF004F006E0065>>> BDC (x) Tj (y) Tj EMC \
          /Span <</ActualText <FEFF00540077006F>>> BDC (z) Tj EMC ET",
    );
    b.add_object(5, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>");
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    assert_eq!(page.text_runs.len(), 3);
    let first = page.text_runs[0].actual_text.as_ref().expect("first span");
    let second = page.text_runs[1].actual_text.as_ref().expect("same span");
    let third = page.text_runs[2].actual_text.as_ref().expect("next span");
    assert_eq!(first.id, second.id);
    assert_ne!(first.id, third.id);
    assert_eq!(&*first.utf16, &[b'O' as u16, b'n' as u16, b'e' as u16]);
    assert_eq!(&*third.utf16, &[b'T' as u16, b'w' as u16, b'o' as u16]);
}
