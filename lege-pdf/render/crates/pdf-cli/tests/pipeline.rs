#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6 end-to-end: a real PDF flows open → compile → CPU render, and the
//! new features (shadings, patterns, images) produce the expected pixels
//! through the *whole* pipeline (not just hand-built IR).

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{DeviceSize, Matrix};
use pdf_render_api::{
    AnnotationMode, Background, HostPage, OutputFormat, OutputResidency, PageTransform,
    RenderLimits, RenderQuality, RenderRequest,
};
use pdf_render_cpu::CpuBackend;
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

/// Render page 0 of `bytes` at `dim`×`dim` device pixels, mapping the 100×100
/// user-space box to the device with a y-flip (PDF y-up → device y-down).
fn render(bytes: Vec<u8>, dim: u32) -> HostPage {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    let snap = DocumentSnapshot::open(source, DocumentLimits::default()).unwrap();
    let mut ctx = ParseContext::new();
    let page = PageCompiler::new()
        .compile(&snap, PageIndex(0), &mut ctx)
        .unwrap();

    let scale = dim as f64 / 100.0;
    // Flip y: device_y = (100 - user_y) * scale.
    let matrix = Matrix {
        a: scale,
        b: 0.0,
        c: 0.0,
        d: -scale,
        e: 0.0,
        f: dim as f64,
    };
    let req = RenderRequest {
        page: Arc::new(page),
        transform: PageTransform { matrix },
        crop: None,
        output_size: DeviceSize {
            width: dim,
            height: dim,
        },
        output_format: OutputFormat::Rgba8PremultipliedSrgb,
        background: Background::White,
        annotations: AnnotationMode::None,
        color_policy: pdf_render_api::RenderColorPolicy::Original,
        quality: RenderQuality::Normal,
        limits: RenderLimits::default(),
        residency: OutputResidency::HostRequired,
    };
    CpuBackend::default().render_to_host(&req).unwrap().0
}

fn px(host: &HostPage, x: usize, y: usize) -> [u8; 4] {
    let i = y * host.stride + x * 4;
    [
        host.pixels[i],
        host.pixels[i + 1],
        host.pixels[i + 2],
        host.pixels[i + 3],
    ]
}

#[test]
fn axial_shading_pdf_renders_a_gradient() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    b.add_object(
        5,
        "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 100 0]/Extend[true true]\
         /Function<</FunctionType 2/Domain[0 1]/C0[0 0 0]/C1[1 1 1]/N 1>>>>",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let host = render(b.into_bytes(), 50);

    // Horizontal black→white gradient (y-flip does not affect a horizontal axis).
    let left = px(&host, 2, 25)[0];
    let right = px(&host, 47, 25)[0];
    assert!(
        left < 40 && right > 210 && left < right,
        "gradient L={left} R={right}"
    );
}

/// One-page PDF painting a full-page red fill under a luminosity soft mask
/// whose group is a white rect over the left half; `tr` optionally adds a
/// `/TR` entry to the `/SMask` dictionary.
fn softmask_pdf(tr: &str) -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ExtGState<</GS1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/GS1 gs 1 0 0 rg 0 0 100 100 re f");
    b.add_object(
        5,
        &format!("<</Type/ExtGState/SMask<</S/Luminosity/G 6 0 R{tr}>>>>"),
    );
    b.add_stream(
        6,
        "/Type/XObject/Subtype/Form/BBox[0 0 100 100]/Group<</S/Transparency>>",
        b"1 1 1 rg 0 0 50 100 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

#[test]
fn soft_mask_tr_transfer_function_inverts_the_mask() {
    // Without /TR: the white-left mask lets red through on the left only.
    let plain = render(softmask_pdf(""), 50);
    let l = px(&plain, 10, 25);
    let r = px(&plain, 40, 25);
    assert!(l[0] > 200 && l[1] < 80, "no /TR: left red: {l:?}");
    assert!(r == [255, 255, 255, 255], "no /TR: right white: {r:?}");

    // With /TR = 1−t (inverting): the same page paints the opposite side.
    let inv = render(
        softmask_pdf("/TR<</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>"),
        50,
    );
    let l = px(&inv, 10, 25);
    let r = px(&inv, 40, 25);
    assert!(l == [255, 255, 255, 255], "/TR: left white: {l:?}");
    assert!(r[0] > 200 && r[1] < 80, "/TR: right red: {r:?}");
}

#[test]
fn type4_mesh_shading_pdf_renders_a_gouraud_triangle() {
    // A free-form Gouraud triangle spanning the page: red/green/blue corners.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    #[rustfmt::skip]
    let mesh: &[u8] = &[
        0, 0, 0, 255, 0, 0,     // (0,0) red
        0, 255, 0, 0, 255, 0,   // (100,0) green
        0, 128, 255, 0, 0, 255, // (50,100) blue
    ];
    b.add_stream(
        5,
        "/ShadingType 4/ColorSpace/DeviceRGB/BitsPerCoordinate 8/BitsPerComponent 8\
         /BitsPerFlag 8/Decode[0 100 0 100 0 1 0 1 0 1]",
        mesh,
    );
    b.finish_classic_xref("/Root 1 0 R");
    let host = render(b.into_bytes(), 50);

    // Page y is flipped on the device: user (0,0) → device bottom-left.
    let p = px(&host, 3, 47); // near the red corner
    assert!(p[0] > 170 && p[1] < 90 && p[2] < 90, "red corner: {p:?}");
    let p = px(&host, 46, 47); // near the green corner
    assert!(p[1] > 170 && p[0] < 90, "green corner: {p:?}");
    let p = px(&host, 25, 4); // near the blue apex
    assert!(p[2] > 170 && p[0] < 90, "blue apex: {p:?}");
    assert_eq!(
        px(&host, 1, 1),
        [255, 255, 255, 255],
        "outside the triangle stays white"
    );
}

#[test]
fn type6_coons_patch_pdf_renders_corner_colors() {
    // One Coons patch covering the page square, corner colors R/G/B/Y.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    let mut mesh = vec![0u8]; // flag 0
    #[rustfmt::skip]
    let boundary: [[u8; 2]; 12] = [
        [0, 0], [0, 85], [0, 170], [0, 255],
        [85, 255], [170, 255], [255, 255],
        [255, 170], [255, 85], [255, 0],
        [170, 0], [85, 0],
    ];
    for p in boundary {
        mesh.extend_from_slice(&p);
    }
    for c in [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]] {
        mesh.extend_from_slice(&c);
    }
    b.add_stream(
        5,
        "/ShadingType 6/ColorSpace/DeviceRGB/BitsPerCoordinate 8/BitsPerComponent 8\
         /BitsPerFlag 8/Decode[0 100 0 100 0 1 0 1 0 1]",
        &mesh,
    );
    b.finish_classic_xref("/Root 1 0 R");
    let host = render(b.into_bytes(), 50);

    // User-space corners under the y-flip: C(0,0)=(0,0) → device bottom-left.
    let p = px(&host, 3, 46);
    assert!(p[0] > 150 && p[2] < 110, "C(0,0) red: {p:?}");
    let p = px(&host, 3, 3); // (0,100) green
    assert!(p[1] > 150 && p[0] < 110, "C(0,1) green: {p:?}");
    let p = px(&host, 46, 3); // (100,100) blue
    assert!(p[2] > 150 && p[1] < 110, "C(1,1) blue: {p:?}");
    let p = px(&host, 46, 46); // (100,0) yellow
    assert!(
        p[0] > 150 && p[1] > 150 && p[2] < 110,
        "C(1,0) yellow: {p:?}"
    );
}

#[test]
fn rgb_image_pdf_renders_pixels() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    // Place the image over the whole page (100x100 user units).
    b.add_stream(4, "", b"q 100 0 0 100 0 0 cm /Im Do Q");
    // 2x1 RGB raw: left red, right blue.
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8/ColorSpace/DeviceRGB",
        &[255, 0, 0, 0, 0, 255],
    );
    b.finish_classic_xref("/Root 1 0 R");
    let host = render(b.into_bytes(), 50);

    assert_eq!(px(&host, 10, 25), [255, 0, 0, 255], "left half red");
    assert_eq!(px(&host, 40, 25), [0, 0, 255, 255], "right half blue");
}

#[test]
fn dct_jpeg_image_pdf_renders_through_the_codec_registry() {
    // A real JPEG (from the bundled encoder), embedded as a DCTDecode image
    // XObject: 2x1, left red, right blue, scaled across the page. The whole
    // path runs: interpreter carries the encoded payload -> NEEDS_DCT flags
    // the page -> the default CpuBackend's registry (JpegCodec) decodes at
    // lowering time -> pixels.
    let jpeg =
        pdf_image::jpeg::encoder::write_jpeg(&[255, 0, 0, 0, 0, 255], 2, 1, true, 100, false)
            .expect("encode");
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 100 0 0 100 0 0 cm /Im Do Q");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace/DeviceRGB/Filter/DCTDecode",
        &jpeg,
    );
    b.finish_classic_xref("/Root 1 0 R");
    let host = render(b.into_bytes(), 50);

    // JPEG chroma bleeds at the center seam; sample well inside each half.
    let left = px(&host, 8, 25);
    let right = px(&host, 41, 25);
    assert!(left[0] > 200 && left[2] < 80, "left is red: {left:?}");
    assert!(right[2] > 200 && right[0] < 80, "right is blue: {right:?}");
}

#[test]
fn jpx_image_pdf_renders_through_the_codec_registry() {
    // A real JPEG 2000 codestream (via jp2lam's encoder) embedded as a
    // /JPXDecode image XObject: 2x1, left red, right blue, scaled across the
    // page. Exercises interpreter -> NEEDS_JPX preflight -> the default
    // CpuBackend registry's jp2lam-backed codec -> pixels.
    let src = [255u8, 0, 0, 0, 0, 255];
    let image = jp2lam::Image::from_rgb_bytes(2, 1, &src).expect("build image");
    let jp2 = jp2lam::encode(
        &image,
        &jp2lam::EncodeOptions {
            quality: 100,
            ..Default::default()
        },
    )
    .expect("encode");

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 100 0 0 100 0 0 cm /Im Do Q");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace/DeviceRGB/Filter/JPXDecode",
        &jp2,
    );
    b.finish_classic_xref("/Root 1 0 R");
    let host = render(b.into_bytes(), 50);

    let left = px(&host, 8, 25);
    let right = px(&host, 41, 25);
    assert!(left[0] > 200 && left[2] < 60, "left is red: {left:?}");
    assert!(right[2] > 200 && right[0] < 60, "right is blue: {right:?}");
}

/// Render a one-page PDF with a single non-embedded font resource `/F1`.
fn render_text(font_dict: &str, content: &[u8], dim: u32) -> HostPage {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", content);
    b.add_object(5, font_dict);
    b.finish_classic_xref("/Root 1 0 R");
    render(b.into_bytes(), dim)
}

/// Fraction of pixels darker than the white background.
fn ink(host: &HostPage) -> f64 {
    let mut inked = 0usize;
    for y in 0..host.height as usize {
        for x in 0..host.width as usize {
            if host.pixels[y * host.stride + x * 4] < 200 {
                inked += 1;
            }
        }
    }
    inked as f64 / (host.width * host.height) as f64
}

/// A placement box covers its whole em; real glyphs are mostly whitespace.
/// This is what separates "drew something" from "drew letters".
fn assert_glyph_like(host: &HostPage, what: &str) {
    let i = ink(host);
    assert!(i > 0.004, "{what}: nothing drawn (ink {i:.4})");
    assert!(i < 0.30, "{what}: solid boxes, not glyphs (ink {i:.4})");
}

#[test]
fn non_embedded_helvetica_renders_glyphs_not_boxes() {
    let host = render_text(
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
        b"BT /F1 24 Tf 5 40 Td (Hello) Tj ET",
        200,
    );
    assert_glyph_like(&host, "Helvetica");
}

#[test]
fn every_standard_family_renders() {
    for base in [
        "Helvetica",
        "Times-Roman",
        "Courier",
        "Times-BoldItalic",
        "Courier-Bold",
    ] {
        let dict = format!("<</Type/Font/Subtype/Type1/BaseFont/{base}>>");
        let host = render_text(&dict, b"BT /F1 24 Tf 5 40 Td (Hamburg) Tj ET", 200);
        assert_glyph_like(&host, base);
    }
}

#[test]
fn unknown_font_falls_back_and_still_renders() {
    // Neither standard-named nor embedded: substitution must still produce
    // glyphs (fonts.md: "deterministic generic fallback").
    let host = render_text(
        "<</Type/Font/Subtype/TrueType/BaseFont/TotallyMadeUpFace>>",
        b"BT /F1 24 Tf 5 40 Td (Hello) Tj ET",
        200,
    );
    assert_glyph_like(&host, "unknown font");
}

#[test]
fn bold_alias_reaches_a_heavier_face() {
    let regular = render_text(
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
        b"BT /F1 24 Tf 5 40 Td (Hamburg) Tj ET",
        200,
    );
    let bold = render_text(
        "<</Type/Font/Subtype/Type1/BaseFont/Arial,Bold>>",
        b"BT /F1 24 Tf 5 40 Td (Hamburg) Tj ET",
        200,
    );
    assert!(
        ink(&bold) > ink(&regular) * 1.05,
        "bold {:.4} should out-ink regular {:.4}",
        ink(&bold),
        ink(&regular)
    );
}

#[test]
fn differences_names_resolve_through_the_substituted_face() {
    // /Differences names resolve against the bundled face's `post` names,
    // which reaches glyphs the compact AGL subset does not know.
    let host = render_text(
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica\
         /Encoding<</Differences[65/bullet 66/germandbls]>>>>",
        b"BT /F1 36 Tf 10 40 Td (AB) Tj ET",
        200,
    );
    assert_glyph_like(&host, "/Differences");
}

#[test]
fn symbolic_standard_faces_render_through_their_builtin_encodings() {
    // Symbol and ZapfDingbats have no useful Unicode cmap and their built-in
    // encodings are name-based (Annex D). Going through Unicode silently
    // drops glyphs (AGL's `mu` is U+00B5, not Symbol's U+03BC; Dingbats'
    // `a1`..`a191` have no Unicode names at all), so this guards the
    // name-first path.
    for base in ["Symbol", "ZapfDingbats"] {
        let dict = format!("<</Type/Font/Subtype/Type1/BaseFont/{base}>>");
        let host = render_text(&dict, b"BT /F1 28 Tf 8 40 Td (abgdez) Tj ET", 200);
        assert_glyph_like(&host, base);
    }
}

#[test]
fn symbol_resolves_every_greek_letter_in_its_alphabet() {
    // `mu` is the trap: it renders only via the glyph name.
    let dict = "<</Type/Font/Subtype/Type1/BaseFont/Symbol>>";
    let solo = |byte: u8| {
        let content = format!("BT /F1 40 Tf 30 30 Td ({}) Tj ET", byte as char);
        ink(&render_text(dict, content.as_bytes(), 120))
    };
    for (code, name) in [
        (b'a', "alpha"),
        (b'm', "mu"),
        (b'b', "beta"),
        (b'w', "omega"),
    ] {
        assert!(
            solo(code) > 0.004,
            "Symbol '{}' ({name}) drew nothing",
            code as char
        );
    }
}

#[test]
fn embedded_type1_fontfile_renders_through_the_native_engine() {
    // A bare Type 1 /FontFile (not /FontFile2 or /FontFile3): Skrifa cannot
    // read it, so this exercises the native interpreter end to end —
    // interpreter -> eexec/charstring decryption -> outline -> pixels.
    let t1 = pdf_test_support::fonts::minimal_type1();
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"BT /F1 60 Tf 10 20 Td (AAA) Tj ET");
    b.add_object(
        5,
        "<</Type/Font/Subtype/Type1/BaseFont/TestFont/FirstChar 65/LastChar 65\
         /Widths[600]/FontDescriptor 6 0 R>>",
    );
    b.add_object(
        6,
        "<</Type/FontDescriptor/FontName/TestFont/Flags 4/ItalicAngle 0\
         /FontFile 7 0 R>>",
    );
    b.add_stream(
        7,
        &format!("/Length1 {}/Length2 0/Length3 0", t1.len()),
        &t1,
    );
    b.finish_classic_xref("/Root 1 0 R");
    let host = render(b.into_bytes(), 200);

    // Three filled triangles: real ink, and nothing like a solid box.
    assert_glyph_like(&host, "embedded Type 1");
}
