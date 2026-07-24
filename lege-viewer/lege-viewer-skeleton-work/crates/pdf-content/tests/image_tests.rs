#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6C: image XObjects decode their samples and resolve their color
//! space through the interpreter; codec-filtered images carry no samples but
//! still flag the codec requirement.

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_page_ir::{CompiledPage, ImageColorSpace, ImageMask, PageFeatures};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

fn compile(snapshot: &DocumentSnapshot, page: u32) -> CompiledPage {
    let mut ctx = ParseContext::new();
    PageCompiler::new().compile(snapshot, PageIndex(page), &mut ctx).expect("compile failed")
}

#[test]
fn rgb_image_xobject_decodes_samples() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 8 0 0 8 0 0 cm /Im Do Q");
    // 2x1 RGB raw image (no filter): left red, right blue.
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8/ColorSpace/DeviceRGB",
        &[255, 0, 0, 0, 0, 255],
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    assert!(page.features.contains(PageFeatures::IMAGES));
    assert_eq!(page.images.len(), 1);
    let img = &page.images[0];
    assert_eq!((img.width, img.height, img.bits_per_component), (2, 1, 8));
    assert!(matches!(img.color_space, ImageColorSpace::Rgb));
    assert_eq!(img.samples.as_deref().unwrap(), &[255, 0, 0, 0, 0, 255]);
}

#[test]
fn indexed_lab_palette_converts_to_rgb() {
    // Regression (Pearson "The Indian Ocean" cover): an /Indexed image whose
    // base is /Lab. The palette bytes are Lab (signed a*/b*); reading them as
    // raw RGB collapses to near-black, so the whole cover rendered solid black.
    // The base must be pre-converted to sRGB.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 8 0 0 8 0 0 cm /Im Do Q");
    // Base /Lab, hival 1, 2-entry palette (hex string): entry0 = Lab(100,0,0)
    // = white (bytes ff 80 80 under Range[-128 127]); entry1 = Lab(0,0,0) =
    // black (00 80 80). Image samples [0, 1] index those entries.
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace[/Indexed[/Lab<</WhitePoint[0.9642 1.0 0.8249]/Range[-128 127 -128 127]>>]1<ff8080008080>]",
        &[0, 1],
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    let img = &page.images[0];
    match &img.color_space {
        ImageColorSpace::Indexed { base, lookup, .. } => {
            assert!(matches!(**base, ImageColorSpace::Rgb), "Lab base pre-converted to Rgb");
            assert!(
                lookup[0] > 240 && lookup[1] > 240 && lookup[2] > 240,
                "entry 0 (Lab L*=100) is near-white, not the black a raw-RGB read gives: {:?}",
                &lookup[0..3]
            );
            assert!(lookup[3] < 30 && lookup[4] < 30 && lookup[5] < 30, "entry 1 is black: {:?}", &lookup[3..6]);
        }
        other => panic!("expected Indexed color space, got {other:?}"),
    }
}

#[test]
fn image_mask_has_no_color_space() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 8 0 0 8 0 0 cm /Im Do Q");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/ImageMask true",
        &[0x40],
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    assert!(page.features.contains(PageFeatures::STENCIL_MASKS));
    let img = &page.images[0];
    assert!(img.is_stencil);
    assert_eq!(img.bits_per_component, 1);
    assert!(img.samples.is_some());
}

/// One page drawing an image XObject (`5 0 R`); `mask_obj` (`6 0 R`) and
/// `smask_obj` (`7 0 R`) are appended when non-empty. `img_dict` is the image
/// stream's dictionary body (without `/Length`).
fn page_with_masked_image(
    img_dict: &str,
    img_data: &[u8],
    mask_obj: Option<(&str, &[u8])>,
    smask_obj: Option<(&str, &[u8])>,
) -> CompiledPage {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 8 0 0 8 0 0 cm /Im Do Q");
    b.add_stream(5, img_dict, img_data);
    if let Some((d, data)) = mask_obj {
        b.add_stream(6, d, data);
    }
    if let Some((d, data)) = smask_obj {
        b.add_stream(7, d, data);
    }
    b.finish_classic_xref("/Root 1 0 R");
    compile(&open(b.into_bytes()), 0)
}

#[test]
fn color_key_mask_parsed_into_ir() {
    // `/Mask [min max …]` per component, in raw-sample space.
    let page = page_with_masked_image(
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace/DeviceRGB/Mask[0 10 20 30 40 50]",
        &[255, 0, 0, 0, 0, 255],
        None,
        None,
    );
    let img = &page.images[0];
    match &img.mask {
        Some(ImageMask::ColorKey(ranges)) => {
            assert_eq!(ranges.as_ref(), &[[0, 10], [20, 30], [40, 50]]);
        }
        other => panic!("expected color-key mask, got {other:?}"),
    }
    assert!(img.smask.is_none());
}

#[test]
fn malformed_color_key_mask_is_ignored() {
    // Only two pairs for a 3-component image → rejected (parse as no mask).
    let page = page_with_masked_image(
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace/DeviceRGB/Mask[0 10 20 30]",
        &[255, 0, 0, 0, 0, 255],
        None,
        None,
    );
    assert!(page.images[0].mask.is_none(), "wrong-length color-key ignored");
}

#[test]
fn color_key_out_of_range_bound_is_ignored() {
    // A bound above 2^bpc-1 (255 for 8bpc) is malformed → ignored.
    let page = page_with_masked_image(
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace/DeviceRGB/Mask[0 10 0 10 0 300]",
        &[255, 0, 0, 0, 0, 255],
        None,
        None,
    );
    assert!(page.images[0].mask.is_none(), "out-of-range bound ignored");
}

#[test]
fn stencil_mask_stream_parsed_into_ir() {
    // `/Mask` is a 1-bit image-mask XObject with its own geometry.
    let page = page_with_masked_image(
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace/DeviceRGB/Mask 6 0 R",
        &[255, 0, 0, 0, 0, 255],
        Some(("/Type/XObject/Subtype/Image/Width 2/Height 1/ImageMask true", &[0x80])),
        None,
    );
    let img = &page.images[0];
    match &img.mask {
        Some(ImageMask::Stencil(sm)) => {
            assert_eq!((sm.width, sm.height, sm.bits_per_component), (2, 1, 1));
            assert!(!sm.samples.is_empty(), "stencil samples decoded");
        }
        other => panic!("expected stencil mask, got {other:?}"),
    }
}

#[test]
fn stencil_mask_decode_polarity_preserved() {
    // `/Decode [1 0]` on the mask must survive to the IR so the sampler can
    // invert it.
    let page = page_with_masked_image(
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace/DeviceRGB/Mask 6 0 R",
        &[255, 0, 0, 0, 0, 255],
        Some(("/Type/XObject/Subtype/Image/Width 2/Height 1/ImageMask true/Decode[1 0]", &[0x80])),
        None,
    );
    match &page.images[0].mask {
        Some(ImageMask::Stencil(sm)) => {
            let d = sm.decode.as_ref().expect("decode carried");
            assert_eq!(d.first().copied(), Some([1.0, 0.0]));
        }
        other => panic!("expected stencil mask, got {other:?}"),
    }
}

#[test]
fn smask_overrides_mask() {
    // Both /SMask and /Mask present: /SMask wins, /Mask is dropped (§8.9.6.4).
    let page = page_with_masked_image(
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /ColorSpace/DeviceRGB/SMask 7 0 R/Mask 6 0 R",
        &[255, 0, 0, 0, 0, 255],
        Some(("/Type/XObject/Subtype/Image/Width 2/Height 1/ImageMask true", &[0x80])),
        Some((
            "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8/ColorSpace/DeviceGray",
            &[128, 200],
        )),
    );
    let img = &page.images[0];
    assert!(img.smask.is_some(), "soft mask present");
    assert!(img.mask.is_none(), "/SMask overrides /Mask");
}

#[test]
fn dct_image_carries_codec_flag_and_no_samples() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 8 0 0 8 0 0 cm /Im Do Q");
    // Not real JPEG bytes; the point is the DCTDecode filter routes it away.
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 2/BitsPerComponent 8/ColorSpace/DeviceRGB/Filter/DCTDecode",
        &[0xFF, 0xD8, 0xFF, 0xD9],
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(&open(b.into_bytes()), 0);

    assert!(page.features.contains(PageFeatures::NEEDS_DCT), "DCT codec flagged");
    let img = &page.images[0];
    assert!(img.samples.is_none(), "codec image carries no samples");
    assert_eq!(img.codec, Some(pdf_page_ir::ImageCodecKind::Dct));
    assert_eq!(
        img.codec_data.as_deref(),
        Some(&[0xFF, 0xD8, 0xFF, 0xD9][..]),
        "encoded payload carried for the backend's codec registry"
    );
}

/// Build one page that draws image `5 0 R`; the image's `/ColorSpace` is the
/// indirect object `6 0 R` whose body is `cs_body` (e.g. a `[/Separation …]`
/// array). `data` is the raw 1-component sample payload.
fn page_with_image_colorspace(cs_body: &str, data: &[u8]) -> CompiledPage {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 8 0 0 8 0 0 cm /Im Do Q");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8/ColorSpace 6 0 R",
        data,
    );
    b.add_object(6, cs_body);
    b.finish_classic_xref("/Root 1 0 R");
    compile(&open(b.into_bytes()), 0)
}

#[test]
fn separation_image_bakes_a_tint_lut() {
    // A `/Separation` *image* sample is a tint, not a gray value. It must lower
    // to a `TintLut` — a 256-entry sample→sRGB table baked through the tint
    // transform — so the backend paints the spot colour instead of misreading
    // the tint as DeviceGray (which inverts a near-white scan to near-black).
    // Transform: tint 0 → white RGB(1,1,1), tint 1 → red RGB(1,0,0).
    let cs = "[/Separation/Spot/DeviceRGB\
              <</FunctionType 2/Domain[0 1]/C0[1 1 1]/C1[1 0 0]/N 1>>]";
    let page = page_with_image_colorspace(cs, &[0, 255]);
    let img = &page.images[0];
    let ImageColorSpace::TintLut { rgb } = &img.color_space else {
        panic!("Separation image must lower to TintLut, got {:?}", img.color_space);
    };
    assert_eq!(rgb.len(), 256 * 3, "LUT is 256 sample→sRGB entries");
    assert_eq!(&rgb[0..3], &[255, 255, 255], "tint 0 → white");
    assert_eq!(&rgb[255 * 3..255 * 3 + 3], &[255, 0, 0], "tint 255 → red");
}

#[test]
fn separation_image_unresolvable_transform_falls_back_to_lut_not_blank() {
    // If the tint transform will not build (here: a dangling indirect
    // reference), the image must NOT be dropped or blanked. `build_tint_space`
    // still yields a space whose evaluator approximates subtractively — the hue
    // is lost but the polarity is right (more tint → darker) — so a `TintLut`
    // is still baked and the draw still paints. A wrong-ish colour beats a
    // blank page (the project's tolerance philosophy).
    let cs = "[/Separation/Spot/DeviceRGB 999 0 R]"; // 999 0 R does not exist
    let page = page_with_image_colorspace(cs, &[0, 255]);
    let img = &page.images[0];
    let ImageColorSpace::TintLut { rgb } = &img.color_space else {
        panic!("unresolvable transform must still fall back to a TintLut, got {:?}", img.color_space);
    };
    assert_eq!(rgb.len(), 256 * 3);
    // Subtractive approximation: tint 0 → white, tint 255 → black.
    assert_eq!(&rgb[0..3], &[255, 255, 255], "tint 0 approximates to white");
    assert_eq!(&rgb[255 * 3..255 * 3 + 3], &[0, 0, 0], "tint 255 approximates to black");
}

#[test]
fn multi_colorant_devicen_image_evaluates_the_tint_transform_per_sample() {
    // A multi-colorant `/DeviceN` image has no single-input tint LUT (the LUT
    // is 1-D). Its samples are instead converted to RGB8 at compile time by
    // running each texel's tints through the tint transform. Transform:
    // (0,0) → C0 black, (1,1) → C1 white.
    let cs = "[/DeviceN[/A /B]/DeviceRGB\
              <</FunctionType 2/Domain[0 1]/C0[0 0 0]/C1[1 1 1]/N 1>>]";
    // Two pixels × two colorants: (0,0) and (255,255).
    let page = page_with_image_colorspace(cs, &[0, 0, 255, 255]);
    let img = &page.images[0];
    assert!(
        matches!(img.color_space, ImageColorSpace::Rgb),
        "a 2-colorant DeviceN image converts to RGB8 per sample, got {:?}",
        img.color_space
    );
    assert_eq!(img.bits_per_component, 8);
    let samples = img.samples.as_ref().expect("converted samples");
    assert_eq!(&samples[0..3], &[0, 0, 0], "tints (0,0) through C0 -> black");
    assert_eq!(&samples[3..6], &[255, 255, 255], "tints (1,1) through C1 -> white");
}

#[test]
fn multi_colorant_devicen_image_short_data_keeps_the_tint_transform() {
    // A buffer too short for n-component texels cannot be converted per sample.
    // It no longer falls back to the arity approximation (2 colorants → Gray,
    // which reads one tint as a gray value and drops the other): a 2-colorant
    // space bakes to a `TintLut2` the sampler can evaluate directly, so the
    // transform survives even when the pixel data does not.
    let cs = "[/DeviceN[/A /B]/DeviceRGB\
              <</FunctionType 2/Domain[0 1]/C0[0 0 0]/C1[1 1 1]/N 1>>]";
    let page = page_with_image_colorspace(cs, &[0, 255]);
    let img = &page.images[0];
    assert!(
        matches!(img.color_space, ImageColorSpace::TintLut2 { .. }),
        "short DeviceN data keeps the 2-input tint table, got {:?}",
        img.color_space
    );
}

#[test]
fn lab_image_converts_per_sample_not_by_arity() {
    // A Lab image's three components are L*a*b*, not RGB. Each texel must run
    // through the same lab_to_rgb the fill path uses: L=100,a=b≈0 is white,
    // L=0 is black. The old arity remap read the bytes as RGB directly.
    let cs = "[/Lab <</WhitePoint[0.9505 1 1.089]/Range[-100 100 -100 100]>>]";
    // Pixel 0: L=100 (raw 255), a=b≈0 (raw 128). Pixel 1: L=0, a=b≈0.
    let page = page_with_image_colorspace(cs, &[255, 128, 128, 0, 128, 128]);
    let img = &page.images[0];
    assert!(
        matches!(img.color_space, ImageColorSpace::Rgb),
        "Lab image converts to RGB8 per sample, got {:?}",
        img.color_space
    );
    let samples = img.samples.as_ref().expect("converted samples");
    assert!(
        samples[0] > 240 && samples[1] > 240 && samples[2] > 240,
        "L*=100 must be near-white, got {:?}",
        &samples[0..3]
    );
    assert!(
        samples[3] < 20 && samples[4] < 20 && samples[5] < 20,
        "L*=0 must be near-black, got {:?}",
        &samples[3..6]
    );
    assert!(img.decode.is_none(), "decode is consumed by the conversion");
}

#[test]
fn inline_image_colorspace_may_be_a_full_array() {
    // custom/image_inline_5: `/CS` written out as `[/Indexed /DeviceRGB 255
    // <palette>]` rather than an abbreviation or a resource name. Falling back
    // to DeviceGray reads palette *indices* as grey levels, so a light-blue
    // entry renders near black.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 10 10]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>");
    // 2x2 image, palette of two entries: 0 = black, 1 = white.
    b.add_stream(
        4,
        "",
        b"q 10 0 0 10 0 0 cm BI /W 2 /H 2 /BPC 8 /CS [/Indexed /DeviceRGB 1 <000000ffffff>] ID \x00\x01\x01\x00 EI Q",
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    let img = page.images.first().expect("inline image");
    match &img.color_space {
        ImageColorSpace::Indexed { hival, lookup, .. } => {
            assert_eq!(*hival, 1);
            assert_eq!(lookup.len(), 6, "two RGB palette entries");
        }
        other => panic!("expected an Indexed colour space, got {other:?}"),
    }
}

#[test]
fn inline_image_with_an_array_colorspace_is_framed_by_length() {
    // pdfbox/2385_1: a 4-bit indexed inline image whose `/CS` is written out.
    // Without a component count the tokenizer cannot derive the exact data
    // length and falls back to scanning for a whitespace-bounded `EI` — which
    // binary sample data hits long before the real end. Here the payload
    // contains " EI " outright, so a scan truncates it to 4 bytes.
    let (w, h) = (16usize, 4usize);
    let row = (w + 1) / 2; // 4 bpc, one component
    let mut samples = vec![0x33u8; row * h];
    samples[4..8].copy_from_slice(b" EI ");
    let mut content = b"q 16 0 0 4 0 0 cm BI /W 16 /H 4 /BPC 4 /CS [/I /RGB 3 <000000ff0000
00ff000000ff>] ID ".to_vec();
    content.retain(|b| *b != b'\n');
    content.extend_from_slice(&samples);
    content.extend_from_slice(b" EI Q");

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 16 4]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>");
    b.add_stream(4, "", &content);
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    let img = page.images.first().expect("inline image");
    assert_eq!((img.width, img.height), (16, 4));
    let decoded = img.samples.as_ref().expect("inline samples");
    assert_eq!(decoded.len(), row * h, "the embedded \" EI \" must not end the image");
    assert!(matches!(img.color_space, ImageColorSpace::Indexed { hival: 3, .. }));
}

#[test]
fn ascii85_inline_image_ends_at_its_eod_marker() {
    // bug1077808: an inline image filtered with ASCII85 carries a *text*
    // payload, so a whitespace-bounded `EI` can occur inside it by chance —
    // and does. Framing must use the ASCII85 end-of-data marker `~>`, not the
    // first `EI`-looking bytes, or the rest of the content stream is tokenized
    // as operators.
    let raw: Vec<u8> = (0u8..64).collect();
    let mut a85 = ascii85_encode(&raw);
    // Splice a decoy ` EI ` into the middle of the armour.
    let mid = a85.len() / 2;
    a85.splice(mid..mid, b" EI ".iter().copied());
    a85.extend_from_slice(b"~>");

    let mut content = b"q 8 0 0 8 0 0 cm BI /W 8 /H 8 /BPC 8 /CS /G /F /A85 ID ".to_vec();
    content.extend_from_slice(&a85);
    content.extend_from_slice(b" EI Q");

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 8 8]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>");
    b.add_stream(4, "", &content);
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    let img = page.images.first().expect("inline image");
    // The decoy sits half way through the armour, so framing that stopped
    // there would yield roughly half the bytes. Only the *length* is asserted:
    // `E` and `I` are themselves valid base-85 digits, so the decoy shifts the
    // decoded bytes — which is fine, the point here is where the frame ends.
    let decoded = img.samples.as_ref().expect("inline samples");
    assert!(
        decoded.len() >= 64,
        "framing stopped at the decoy EI: {} bytes",
        decoded.len()
    );
}

/// Minimal ASCII85 encoder (no `<~` prefix, no line breaks, no `z` shorthand),
/// enough for the framing fixture above.
fn ascii85_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes(word);
        let mut digits = [0u8; 5];
        let mut v = n;
        for slot in digits.iter_mut().rev() {
            *slot = b'!' + (v % 85) as u8;
            v /= 85;
        }
        out.extend_from_slice(&digits[..chunk.len() + 1]);
    }
    out
}

#[test]
fn ascii85_eod_from_a_later_image_is_not_borrowed() {
    // issue11385: two `[/A85 /Fl]` masks, and the first one's armour has no
    // `~>` at all. Searching forward finds the *second* image's marker, and the
    // frame then swallows every operator in between — that page lost all of its
    // text. An intervening `BI` bounds the search.
    let first = ascii85_encode(&(0u8..40).collect::<Vec<_>>()); // no `~>`
    let second = {
        let mut v = ascii85_encode(&(0u8..40).collect::<Vec<_>>());
        v.extend_from_slice(b"~>");
        v
    };
    let mut content = b"q BI /W 8 /H 5 /BPC 8 /CS /G /F /A85 ID ".to_vec();
    content.extend_from_slice(&first);
    content.extend_from_slice(b" EI Q 1 0 0 RG 5 w 0 0 m 8 8 l S q BI /W 8 /H 5 /BPC 8 /CS /G /F /A85 ID ");
    content.extend_from_slice(&second);
    content.extend_from_slice(b" EI Q");

    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 8 8]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>");
    b.add_stream(4, "", &content);
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    // Both images must be seen, and the stroke between them must survive.
    assert_eq!(page.images.len(), 2, "the first frame swallowed the second image");
    assert!(
        page.paths.iter().any(|p| !p.verbs.is_empty()),
        "the operators between the two images were swallowed"
    );
}

#[test]
fn lab_image_colorspace_survives_as_lab() {
    // custom/image_lab: a *codec* image under `[/Lab …]`. Raw-sample Lab
    // images are pre-converted to RGB8 at compile time, but a DCT stream has
    // no samples until the backend decodes it, so the declared space has to
    // survive into the IR — otherwise the codec's own 3-channel Rgb wins and
    // the L*a*b* bytes are painted as RGB. That renders yellow-green where
    // every control renders blue, because a near-zero b* byte is a strong blue
    // but as a blue channel it is black. `/Range` must come too: it supplies
    // the image's default `/Decode` (ISO 32000-1 Table 89).
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 10 10]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 10 0 0 10 0 0 cm /Im Do Q");
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8/Filter/DCTDecode\
         /ColorSpace[/Lab<</WhitePoint[0.9505 1 1.089]/Range[-120 120 -110 110]>>]",
        &[0xFF, 0xD8, 0xFF, 0xD9],
    );
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let page = compile(&snap, 0);

    match &page.images[0].color_space {
        ImageColorSpace::Lab { white_point, range } => {
            assert!((white_point[0] - 0.9505).abs() < 1e-4, "white point {white_point:?}");
            assert_eq!(*range, [-120.0, 120.0, -110.0, 110.0]);
        }
        other => panic!("expected a Lab image space, got {other:?}"),
    }
}

/// `/Mask` pointing at a stream that is *not* an `/ImageMask` is not a stencil:
/// §8.9.6.4's "sample 1 masks out" polarity is defined for image masks, and a
/// producer that points `/Mask` at an ordinary 1-bit `/DeviceGray` image means
/// the other thing (pdfjs/issue6621). PDFium, MuPDF and hayro all paint where
/// those samples are 1; taking the stencil polarity literally masked out
/// everything they paint and left the image invisible.
#[test]
fn a_mask_stream_that_is_not_an_image_mask_has_the_opposite_polarity() {
    fn mask_decode(image_mask: bool, extra: &str) -> Option<Vec<[f32; 2]>> {
        let mut b = PdfBuilder::new();
        b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
        b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 10 10]>>");
        b.add_object(
            3,
            "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
        );
        b.add_stream(4, "", b"q 10 0 0 10 0 0 cm /Im Do Q");
        b.add_stream(
            5,
            "/Type/XObject/Subtype/Image/Width 2/Height 2/BitsPerComponent 8\
             /ColorSpace/DeviceGray/Mask 6 0 R",
            &[0u8, 64, 128, 255],
        );
        let flag = if image_mask { "/ImageMask true" } else { "/ColorSpace/DeviceGray" };
        b.add_stream(
            6,
            &format!("/Type/XObject/Subtype/Image/Width 2/Height 2/BitsPerComponent 1{flag}{extra}"),
            &[0b0100_0000u8, 0b1000_0000],
        );
        b.finish_classic_xref("/Root 1 0 R");
        let page = compile(&open(b.into_bytes()), 0);
        match page.images[0].mask.as_ref().expect("mask") {
            pdf_page_ir::ImageMask::Stencil(sm) => sm.decode.as_ref().map(|d| d.to_vec()),
            other => panic!("expected a stencil mask, got {other:?}"),
        }
    }

    // A real /ImageMask keeps the spec polarity: no synthesized /Decode.
    assert_eq!(mask_decode(true, ""), None, "an /ImageMask is left alone");
    // A plain image gets the polarity flipped.
    assert_eq!(
        mask_decode(false, ""),
        Some(vec![[1.0, 0.0]]),
        "a non-/ImageMask mask paints where the sample is 1"
    );
    // An explicit /Decode is flipped rather than replaced, so a producer that
    // already inverted does not get inverted twice.
    assert_eq!(
        mask_decode(false, "/Decode[1 0]"),
        Some(vec![[0.0, 1.0]]),
        "an explicit /Decode composes with the flip"
    );
}
