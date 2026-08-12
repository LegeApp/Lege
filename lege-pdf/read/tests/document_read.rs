use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lege_pdf_read::{
    CancellationToken, CompileStatus, PageContentKind, RasterPlane, RasterProduct, ReadError,
    RenderSession, examine_document, extract_outline, has_text_layer, page_text,
    page_text_evidence, positioned_words,
};
use pdf_test_support::builder::{self, PdfBuilder};

fn shared(bytes: Vec<u8>) -> Arc<[u8]> {
    Arc::from(bytes)
}

#[test]
fn intake_reports_clean_recovered_and_malformed_documents() {
    let clean = shared(builder::multipage_classic(2));
    let intake = examine_document(&clean);
    assert_eq!(intake.page_count, 2);
    assert!(!intake.encrypted);
    assert!(!intake.needs_password);
    assert!(!intake.recovery_used);
    assert_eq!(intake.per_page_compile.len(), 2);
    assert!(
        intake
            .per_page_compile
            .iter()
            .all(|status| matches!(status, CompileStatus::Ok { .. }))
    );

    let recovered = shared(builder::without_xref(builder::multipage_classic(1)));
    let intake = examine_document(&recovered);
    assert_eq!(intake.page_count, 1);
    assert!(intake.recovery_used);

    let malformed = shared(b"not a PDF".to_vec());
    let intake = examine_document(&malformed);
    assert_eq!(intake.page_count, 0);
    assert!(intake.per_page_compile.is_empty());
}

#[test]
fn intake_distinguishes_empty_password_encryption_from_password_required() {
    let empty_password = shared(builder::encrypted_fixture());
    let intake = examine_document(&empty_password);
    assert!(intake.encrypted);
    assert!(!intake.needs_password);
    assert_eq!(intake.page_count, 1);

    for fixture in [
        "encrypted_hello_world_r2.pdf",
        "encrypted_hello_world_r3.pdf",
        "encrypted_hello_world_r6.pdf",
    ] {
        let protected = shared(read_renderer_fixture(fixture));
        let intake = examine_document(&protected);
        assert!(intake.encrypted, "{fixture}");
        assert!(intake.needs_password, "{fixture}");
        assert_eq!(intake.page_count, 0, "{fixture}");

        assert!(
            RenderSession::open(Arc::clone(&protected), None).is_err(),
            "{fixture}"
        );
        assert!(
            RenderSession::open(Arc::clone(&protected), Some("wrong")).is_err(),
            "{fixture}"
        );
        let session = RenderSession::open(protected, Some("hôtel")).expect("password should open");
        assert_eq!(session.page_count(), 1, "{fixture}");
        let page = session.compile(0).expect("encrypted page should compile");
        assert!(page.operation_count() > 0, "{fixture}");
    }
}

#[test]
fn session_exposes_geometry_and_opaque_compilation() {
    let session = RenderSession::open(shared(builder::multipage_classic(2)), None)
        .expect("fixture should open");
    assert_eq!(session.page_count(), 2);

    let first = session.page_geometry(0).expect("page zero geometry");
    assert_eq!(first.crop_box, [0.0, 0.0, 612.0, 792.0]);
    assert_eq!(first.rotate, 0);
    assert_eq!(first.width(), 612.0);
    assert_eq!(first.height(), 792.0);

    let second = session.page_geometry(1).expect("page one geometry");
    assert_eq!(second.crop_box, [10.0, 10.0, 400.0, 500.0]);
    assert_eq!(second.rotate, 90);
    assert_eq!(second.display_width(), 490.0);
    assert_eq!(second.display_height(), 390.0);

    let compiled = session.compile(0).expect("page should compile");
    assert_eq!(compiled.page_index(), 0);
    assert!(compiled.operation_count() > 0);

    assert!(matches!(
        session.page_geometry(2),
        Err(ReadError::PageOutOfRange {
            page: 2,
            page_count: 2
        })
    ));
}

#[test]
fn cancelled_render_returns_before_allocating_a_surface() {
    let session =
        RenderSession::open(shared(builder::multipage_classic(1)), None).expect("fixture opens");
    let page = session.compile(0).expect("page compiles");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let started = Instant::now();
    assert!(
        session
            .render_cancellable(&page, &RasterProduct::rgb8(2400, 3200), Some(&cancellation),)
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[test]
fn session_renders_compiled_pages_to_lege_owned_rgb_and_gray_planes() {
    let session = RenderSession::open(shared(builder::multipage_classic(1)), None)
        .expect("fixture should open");
    let page = session.compile(0).expect("page should compile");

    let rgb = session
        .render(&page, &RasterProduct::rgb8(64, 48))
        .expect("RGB render");
    let RasterPlane::Rgb8(rgb) = rgb else {
        panic!("expected RGB plane");
    };
    assert_eq!((rgb.width, rgb.height, rgb.stride), (64, 48, 64 * 3));
    assert_eq!(rgb.pixels.len(), rgb.stride * rgb.height as usize);

    let gray = session
        .render(&page, &RasterProduct::gray8(64, 48))
        .expect("gray render");
    let RasterPlane::Gray8(gray) = gray else {
        panic!("expected gray plane");
    };
    assert_eq!((gray.width, gray.height), (64, 48));
    assert!(gray.stride >= gray.width as usize);
    assert!(gray.pixels.len() >= gray.stride * gray.height as usize);

    assert!(matches!(
        session.render(&page, &RasterProduct::gray8(0, 48)),
        Err(ReadError::InvalidRasterProduct(_))
    ));
}

#[test]
fn session_renders_annotation_appearances_by_default() {
    let session =
        RenderSession::open(shared(annotation_fixture()), None).expect("fixture should open");
    let page = session.compile(0).expect("page should compile");
    let plane = session
        .render(&page, &RasterProduct::rgb8(100, 100))
        .expect("annotation render");
    let RasterPlane::Rgb8(rgb) = plane else {
        panic!("expected RGB plane");
    };
    let red_pixels = rgb
        .pixels
        .chunks_exact(3)
        .filter(|pixel| pixel[0] > 200 && pixel[1] < 80 && pixel[2] < 80)
        .count();
    assert!(
        red_pixels > 100,
        "expected annotation appearance ink, found {red_pixels} red pixels"
    );
}

#[test]
fn text_wrapper_returns_exact_words_in_render_pixel_space() {
    let session =
        RenderSession::open(shared(text_fixture()), None).expect("text fixture should open");

    assert!(has_text_layer(&session, 0).expect("text presence"));
    assert_eq!(page_text(&session, 0).expect("page text"), "Illinois will");
    let words = positioned_words(&session, 0, 600, 400).expect("positioned words");
    assert_eq!(
        words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>(),
        vec!["Illinois", "will"]
    );
    assert!(words[0].bbox[2] < words[1].bbox[0]);
    assert!(words.iter().all(|word| {
        word.bbox[0] >= 0.0 && word.bbox[1] >= 0.0 && word.bbox[2] <= 600.0 && word.bbox[3] <= 400.0
    }));

    let evidence = page_text_evidence(&session, 0, 600, 400).expect("page evidence");
    assert_eq!(evidence.kind, PageContentKind::NativeText);
    assert!(evidence.trustworthy);
    assert_eq!(evidence.character_count, "Illinoiswill".len());
    assert_eq!(evidence.image_count, 0);
}

#[test]
fn simple_full_page_jpeg_is_exposed_for_direct_scan_intake() {
    let session = RenderSession::open(shared(scan_fixture()), None).expect("scan fixture opens");
    let evidence = page_text_evidence(&session, 0, 100, 100).expect("scan evidence");
    assert_eq!(evidence.kind, PageContentKind::ScannedImage);
    let scan = evidence.direct_scan.expect("direct JPEG route");
    assert_eq!((scan.width, scan.height), (16, 16));
    assert!(scan.jpeg.starts_with(&[0xff, 0xd8]));
}

#[test]
fn outline_resolves_all_destination_forms_and_promotes_children() {
    let session =
        RenderSession::open(shared(outline_fixture()), None).expect("outline fixture should open");
    let outline = extract_outline(&session);

    let summary = outline
        .iter()
        .map(|node| (node.title.as_str(), node.source_page, node.top))
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        vec![
            ("Direct", 0, Some(700.0)),
            ("Promoted", 1, None),
            ("Named", 1, Some(500.0)),
            ("Über", 0, Some(250.0)),
            ("Legacy", 0, Some(650.0)),
        ]
    );
    assert!(outline.iter().all(|node| node.children.is_empty()));
}

fn outline_fixture() -> Vec<u8> {
    let mut pdf = PdfBuilder::new();
    pdf.add_object(
        1,
        "<</Type/Catalog/Pages 2 0 R/Outlines 5 0 R\
          /Names<</Dests 9 0 R>>\
          /Dests<</legacy[10 0 R/FitH 650]>>>>",
    );
    pdf.add_object(
        2,
        "<</Type/Pages/Kids[10 0 R 12 0 R]/Count 2/MediaBox[0 0 200 300]>>",
    );
    pdf.add_object(10, "<</Type/Page/Parent 2 0 R/Rotate 0>>");
    pdf.add_object(
        12,
        "<</Type/Page/Parent 2 0 R/CropBox[10 20 180 280]/Rotate 90>>",
    );
    pdf.add_object(5, "<</Type/Outlines/First 6 0 R/Last 13 0 R>>");
    pdf.add_object(
        6,
        "<</Title(Direct)/Parent 5 0 R/Dest[10 0 R/XYZ null 700 null]/Next 7 0 R>>",
    );
    // This parent has no destination. Its child must be promoted.
    pdf.add_object(
        7,
        "<</Title(Unresolved)/Parent 5 0 R/First 20 0 R/Next 8 0 R>>",
    );
    pdf.add_object(20, "<</Title(Promoted)/Parent 7 0 R/Dest[12 0 R/Fit]>>");
    pdf.add_object(
        8,
        "<</Title(Named)/Parent 5 0 R/Dest(chapter-two)/Next 11 0 R>>",
    );
    pdf.add_object(9, "<</Kids[14 0 R]>>");
    pdf.add_object(14, "<</Names[(chapter-two)[12 0 R/FitH 500]]>>");
    pdf.add_object(
        11,
        "<</Title<FEFF00DC006200650072>/Parent 5 0 R\
          /A<</S/GoTo/D[10 0 R/FitR 0 0 200 250]>>/Next 13 0 R>>",
    );
    // The back-edge proves the sibling cycle guard without duplicating nodes.
    pdf.add_object(13, "<</Title(Legacy)/Parent 5 0 R/Dest/legacy/Next 6 0 R>>");
    pdf.finish_classic_xref("/Root 1 0 R");
    pdf.into_bytes()
}

fn text_fixture() -> Vec<u8> {
    let mut pdf = PdfBuilder::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 300 200]>>",
    );
    pdf.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    pdf.add_stream(4, "", b"BT /F1 20 Tf 20 100 Td (Illinois will) Tj ET");
    pdf.add_object(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
    );
    pdf.finish_classic_xref("/Root 1 0 R");
    pdf.into_bytes()
}

fn annotation_fixture() -> Vec<u8> {
    let mut pdf = PdfBuilder::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    pdf.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Annots[10 0 R]>>",
    );
    pdf.add_stream(4, "", b"");
    pdf.add_object(
        10,
        "<</Type/Annot/Subtype/Square/Rect[20 20 80 80]/AP<</N 11 0 R>>>>",
    );
    pdf.add_stream(
        11,
        "/Type/XObject/Subtype/Form/BBox[0 0 10 10]",
        b"1 0 0 rg 0 0 10 10 re f",
    );
    pdf.finish_classic_xref("/Root 1 0 R");
    pdf.into_bytes()
}

fn scan_fixture() -> Vec<u8> {
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, 90)
        .encode(&vec![230; 16 * 16], 16, 16, jpeg_encoder::ColorType::Luma)
        .expect("encode fixture JPEG");
    let mut pdf = PdfBuilder::new();
    pdf.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    pdf.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    pdf.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im0 5 0 R>>>>>>",
    );
    pdf.add_stream(4, "", b"q 100 0 0 100 0 0 cm /Im0 Do Q");
    pdf.add_stream(5, "/Type/XObject/Subtype/Image/Width 16/Height 16/ColorSpace/DeviceGray/BitsPerComponent 8/Filter/DCTDecode", &jpeg);
    pdf.finish_classic_xref("/Root 1 0 R");
    pdf.into_bytes()
}

fn read_renderer_fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../render/crates/pdf-document/tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}
