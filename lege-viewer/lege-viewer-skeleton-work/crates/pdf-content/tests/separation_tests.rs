#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Separation / DeviceN tint transforms (ISO 32000-1 §8.6.6.4).
//!
//! These spaces are *subtractive*: tint 1.0 means full colorant. Resolving
//! them by component arity instead (one tint looks like DeviceGray) inverts
//! them — 1.0 becomes white — which silently erases anything painted in
//! `[/Separation /Black ...]`, a very common way to spell black in
//! print-oriented PDFs.

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

/// One page that sets `/Cs` and fills a rect with `content`.
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

fn first_solid(page: &CompiledPage) -> Color {
    page.paints
        .iter()
        .find_map(|p| match p {
            Paint::Solid(c) => Some(*c),
            _ => None,
        })
        .expect("a solid paint")
}

/// One page whose `/Cs` colorspace (object 5) references a function *stream*
/// (object 6) — needed for Type 0 (sampled) and Type 4 (PostScript) tint
/// transforms, which are streams rather than inline dictionaries.
fn page_with_fn_stream(
    colorspace: &str,
    fn_dict_extra: &str,
    fn_data: &[u8],
    content: &[u8],
) -> CompiledPage {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ColorSpace<</Cs 5 0 R>>>>>>",
    );
    b.add_stream(4, "", content);
    b.add_object(5, colorspace);
    b.add_stream(6, fn_dict_extra, fn_data);
    b.finish_classic_xref("/Root 1 0 R");
    compile(b.into_bytes())
}

/// `[/Separation /Black /DeviceGray {tint -> 1-tint}]`: an exponential
/// transform from white (tint 0) to black (tint 1).
const SEP_BLACK: &str = "[/Separation/Black/DeviceGray\
     <</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>]";

#[test]
fn separation_full_tint_is_ink_not_white() {
    // The bug this guards: `1 scn` in a Separation space must paint BLACK.
    // Treating the single tint as DeviceGray makes it white — invisible.
    let page = page_with(SEP_BLACK, b"/Cs cs 1 scn 0 0 100 100 re f");
    let c = first_solid(&page);
    assert!(c.r < 0.02 && c.g < 0.02 && c.b < 0.02, "tint 1.0 must be full ink, got {c:?}");
}

#[test]
fn separation_zero_tint_is_blank() {
    let page = page_with(SEP_BLACK, b"/Cs cs 0 scn 0 0 100 100 re f");
    let c = first_solid(&page);
    assert!(c.r > 0.98 && c.g > 0.98 && c.b > 0.98, "tint 0 leaves no colorant, got {c:?}");
}

#[test]
fn separation_tint_runs_through_the_transform() {
    // Half tint through `1 - t` is mid grey — proof the function is really
    // evaluated rather than the tint being passed through.
    let page = page_with(SEP_BLACK, b"/Cs cs 0.25 scn 0 0 100 100 re f");
    let c = first_solid(&page);
    assert!((c.r - 0.75).abs() < 0.02, "0.25 tint -> 0.75 grey, got {c:?}");
}

#[test]
fn separation_initial_color_is_full_colorant() {
    // Setting the space without a colour must default to 1.0 in every
    // component (ISO 32000-1 8.6.8) — i.e. ink, not white.
    let page = page_with(SEP_BLACK, b"/Cs cs 0 0 100 100 re f");
    let c = first_solid(&page);
    assert!(c.r < 0.02, "initial colour of a Separation is full tint, got {c:?}");
}

#[test]
fn separation_into_cmyk_alternate_works() {
    // The alternate space need not be grey: a 1-in, 4-out transform lands in
    // DeviceCMYK.
    let cs = "[/Separation/Spot/DeviceCMYK\
        <</FunctionType 2/Domain[0 1]/C0[0 0 0 0]/C1[0 1 1 0]/N 1>>]";
    let page = page_with(cs, b"/Cs cs 1 scn 0 0 100 100 re f");
    let c = first_solid(&page);
    // CMYK(0,1,1,0) is red — a measured ink red, not the arithmetic
    // (1, 0, 0): magenta and yellow pigment still reflect a little green and
    // blue. The point is that the tint reached DeviceCMYK at all.
    assert!(c.r > 0.85, "red-dominant: {c:?}");
    assert!(c.g < 0.25 && c.b < 0.25, "green/blue suppressed: {c:?}");
}

#[test]
fn devicen_without_an_evaluable_transform_keeps_its_polarity() {
    // A 2-colorant DeviceN whose transform is a *single-input* Type 2 function
    // (technically under-specified for DeviceN, but seen in the wild). It now
    // evaluates through `eval_n`, which reads the first colorant and lands the
    // white->black ramp in real DeviceRGB. The polarity must stay subtractive:
    // more tint = darker. Getting this backwards is what erases text.
    let cs = "[/DeviceN[/A /B]/DeviceRGB\
        <</FunctionType 2/Domain[0 1]/C0[1 1 1]/C1[0 0 0]/N 1>>]";
    let dark = first_solid(&page_with(cs, b"/Cs cs 1 1 scn 0 0 100 100 re f"));
    let light = first_solid(&page_with(cs, b"/Cs cs 0 0 scn 0 0 100 100 re f"));
    assert!(dark.r < 0.1, "full tint is dark, got {dark:?}");
    assert!(light.r > 0.9, "no tint is light, got {light:?}");
}

#[test]
fn devicen_type4_transform_lands_in_rgb() {
    // A 2-colorant DeviceN with a Type 4 (PostScript) tint transform. The
    // program `{ 0 }` leaves the two tints (a, b) on the stack and pushes a
    // blue channel of 0, so the DeviceRGB output is (a, b, 0). With `1 0 scn`
    // that is pure red — proof the transform actually ran (multi-input!),
    // rather than the old `1 - max(tint)` grey fallback (which would give
    // grey(0) = black).
    let cs = "[/DeviceN[/A /B]/DeviceRGB 6 0 R]";
    let page = page_with_fn_stream(
        cs,
        "/FunctionType 4/Domain[0 1 0 1]/Range[0 1 0 1 0 1]",
        b"{ 0 }",
        b"/Cs cs 1 0 scn 0 0 100 100 re f",
    );
    let c = first_solid(&page);
    assert!(c.r > 0.9, "red channel = first tint (1.0): {c:?}");
    assert!(c.g < 0.1 && c.b < 0.1, "green/blue suppressed: {c:?}");
}

#[test]
fn devicen_multi_input_sampled_transform_lands_in_rgb() {
    // A 2-colorant DeviceN with a multi-input Type 0 (sampled) tint transform.
    // 2x2 grid, 3 outputs, input 0 varying fastest. Grid points:
    //   (0,0)=black  (1,0)=red  (0,1)=green  (1,1)=white
    // `1 0 scn` selects corner (1,0) → red. Anything but the real multilinear
    // evaluation (e.g. the grey fallback) would not produce a saturated red.
    let cs = "[/DeviceN[/A /B]/DeviceRGB 6 0 R]";
    let samples: &[u8] = &[
        0, 0, 0, // (0,0) black
        255, 0, 0, // (1,0) red
        0, 255, 0, // (0,1) green
        255, 255, 255, // (1,1) white
    ];
    let page = page_with_fn_stream(
        cs,
        "/FunctionType 0/Domain[0 1 0 1]/Size[2 2]/BitsPerSample 8\
         /Range[0 1 0 1 0 1]/Encode[0 1 0 1]",
        samples,
        b"/Cs cs 1 0 scn 0 0 100 100 re f",
    );
    let c = first_solid(&page);
    assert!(c.r > 0.9, "corner (1,0) is red: {c:?}");
    assert!(c.g < 0.1 && c.b < 0.1, "green/blue suppressed: {c:?}");

    // And the opposite corner (0,1) is green.
    let page2 = page_with_fn_stream(
        cs,
        "/FunctionType 0/Domain[0 1 0 1]/Size[2 2]/BitsPerSample 8\
         /Range[0 1 0 1 0 1]/Encode[0 1 0 1]",
        samples,
        b"/Cs cs 0 1 scn 0 0 100 100 re f",
    );
    let g = first_solid(&page2);
    assert!(g.g > 0.9, "corner (0,1) is green: {g:?}");
    assert!(g.r < 0.1 && g.b < 0.1, "red/blue suppressed: {g:?}");
}

#[test]
fn type4_parse_failure_falls_back_tolerantly() {
    // A Type 4 function whose program is garbage: `parse_postscript` rejects
    // it, `build_function` yields Identity of the right arity, and the page
    // still compiles (no panic, a solid paint is produced).
    let cs = "[/Separation/Spot/DeviceRGB 6 0 R]";
    let page = page_with_fn_stream(
        cs,
        "/FunctionType 4/Domain[0 1]/Range[0 1 0 1 0 1]",
        b"{ 1 2 frobnicate }",
        b"/Cs cs 0.5 scn 0 0 100 100 re f",
    );
    // Identity{n_out:3} fed tint 0.5 → RGB(0.5, 0.5, 0.5); the point is it did
    // not panic and landed somewhere sane.
    let c = first_solid(&page);
    assert!(c.r.is_finite() && c.g.is_finite() && c.b.is_finite(), "sane color: {c:?}");
}

#[test]
fn non_separation_named_spaces_are_untouched() {
    // ICCBased keeps the existing arity approximation: 3 components = RGB.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</ColorSpace<</Cs 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Cs cs 1 0 0 scn 0 0 100 100 re f");
    b.add_object(5, "[/ICCBased 6 0 R]");
    b.add_stream(6, "/N 3", &[]);
    b.finish_classic_xref("/Root 1 0 R");
    let c = first_solid(&compile(b.into_bytes()));
    assert!(c.r > 0.9 && c.g < 0.1, "ICCBased 3-comp stays RGB, got {c:?}");
}

/// One page that draws a 2×2 8-bit image in the given (direct) `/ColorSpace`.
fn page_with_image_cs(colorspace: &str) -> CompiledPage {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 100 0 0 100 0 0 cm /Im Do Q");
    b.add_stream(
        5,
        &format!(
            "/Type/XObject/Subtype/Image/Width 2/Height 2/BitsPerComponent 8/ColorSpace{colorspace}"
        ),
        &[0u8, 85, 170, 255],
    );
    b.finish_classic_xref("/Root 1 0 R");
    compile(b.into_bytes())
}

fn tint_lut(page: &CompiledPage) -> &[u8] {
    match &page.images[0].color_space {
        pdf_page_ir::ImageColorSpace::TintLut { rgb } => rgb,
        other => panic!("expected TintLut, got {other:?}"),
    }
}

#[test]
fn degenerate_separation_image_ramp_is_repaired_and_flagged() {
    // R1 regression (PLAN-POST-SWEEP3): a tint transform that "works" but
    // bakes to a constant near-white ramp (C0 == C1 == white) would blanket
    // the image — the Separation blank-cover class. The LUT must be repaired
    // to the polarity-correct subtractive ramp and the draw flagged degraded.
    let page = page_with_image_cs(
        "[/Separation/Spot/DeviceGray<</FunctionType 2/Domain[0 1]/C0[1]/C1[1]/N 1>>]",
    );
    assert_eq!(page.images.len(), 1);
    assert!(page.images[0].lowering_degraded, "repaired ramp must be flagged");
    let lut = tint_lut(&page);
    // Subtractive polarity: tint 0 → white, tint 1 → black.
    assert_eq!(&lut[0..3], &[255, 255, 255]);
    assert_eq!(&lut[255 * 3..255 * 3 + 3], &[0, 0, 0]);
}

#[test]
fn healthy_separation_image_ramp_is_kept_and_unflagged() {
    // The same shape with a real white→black transform: kept verbatim,
    // not degraded.
    let page = page_with_image_cs(
        "[/Separation/Black/DeviceGray<</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>]",
    );
    assert_eq!(page.images.len(), 1);
    assert!(!page.images[0].lowering_degraded);
    let lut = tint_lut(&page);
    assert_eq!(&lut[0..3], &[255, 255, 255]);
    assert_eq!(&lut[255 * 3..255 * 3 + 3], &[0, 0, 0]);
}

#[test]
fn separation_with_lab_alternate_converts_through_lab() {
    // The DS82 regression: a PANTONE spot whose alternate space is Lab. The
    // tint transform outputs L*a*b* components (L* 0..100, a*/b* signed) —
    // clamping them into [0,1] as DeviceRGB rendered the pale lavender
    // PANTONE 2706 U as pure yellow. They must route through lab_to_rgb
    // (PDFium: CPDF_SeparationCS::GetRGB -> base_cs_->GetRGB).
    let cs = "[/Separation/PANTONE#202706#20U[/Lab<</WhitePoint[0.9505 1 1.089]\
        /Range[-128 127 -128 127]>>]\
        <</FunctionType 2/Domain[0 1]/C0[100 0 0]/C1[85.25 3.36 -20.71]/N 1\
        /Range[0 100 -128 127 -128 127]>>]";
    let c = first_solid(&page_with(cs, b"/Cs cs 1 scn 0 0 100 100 re f"));
    assert!(
        c.b > 0.9 && c.r > 0.6 && c.r < 0.9 && c.g > c.r && c.b > c.g,
        "PANTONE 2706 U is pale lavender (b > g > r, all light), got {c:?}"
    );
    let white = first_solid(&page_with(cs, b"/Cs cs 0 scn 0 0 100 100 re f"));
    assert!(
        white.r > 0.95 && white.g > 0.95 && white.b > 0.95,
        "tint 0 -> L*=100 -> white, got {white:?}"
    );
}

#[test]
fn all_colorant_runs_its_tint_transform_like_any_other() {
    // `/All` (§8.6.6.4, registration marks) is *not* special-cased when
    // painting. PDFium's `CPDF_SeparationCS::GetRGB` branches only on `/None`
    // and runs the tint transform for every other colorant name, and hayro and
    // MuPDF agree. This test previously asserted the opposite — that `/All`
    // bypassed the transform and painted neutral ink at the tint — which
    // rendered custom/color_separation_3 as flat grey 128 where all three
    // controls produce the transform's brown.
    //
    // Here the transform maps every tint to white, so every tint paints white.
    let cs = "[/Separation/All/DeviceGray\
        <</FunctionType 2/Domain[0 1]/C0[1]/C1[1]/N 1>>]";
    for tint in ["1", "0.5", "0"] {
        let content = format!("/Cs cs {tint} scn 0 0 100 100 re f");
        let c = first_solid(&page_with(cs, content.as_bytes()));
        assert!(
            c.r > 0.98 && c.g > 0.98 && c.b > 0.98,
            "tint {tint} must follow the transform to white, got {c:?}"
        );
    }

    // And with a transform that actually varies, `/All` tracks it.
    let ramp = "[/Separation/All/DeviceGray\
        <</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>]";
    let dark = first_solid(&page_with(ramp, b"/Cs cs 1 scn 0 0 100 100 re f"));
    assert!(dark.r < 0.02, "tint 1 through a 1->0 ramp is black, got {dark:?}");
    let light = first_solid(&page_with(ramp, b"/Cs cs 0 scn 0 0 100 100 re f"));
    assert!(light.r > 0.98, "tint 0 through a 1->0 ramp is white, got {light:?}");
}

#[test]
fn all_colorant_image_lut_matches_a_named_colorant() {
    // The image path must make the same decision as the fill path: `/All` is
    // an ordinary colorant, so with one transform it bakes one LUT regardless
    // of the colorant's name. A varying ramp is used deliberately — a constant
    // transform trips the separate degenerate-ramp repair below.
    let func = "<</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>";
    let all = page_with_image_cs(&format!("[/Separation/All/DeviceGray{func}]"));
    let named = page_with_image_cs(&format!("[/Separation/Spot/DeviceGray{func}]"));
    assert!(!all.images[0].lowering_degraded);
    assert!(!named.images[0].lowering_degraded);
    let (a, n) = (tint_lut(&all), tint_lut(&named));
    assert_eq!(a, n, "/All must not be special-cased against a named colorant");
    // And it follows the ramp: tint 0 -> white, tint 1 -> black.
    assert_eq!(&a[0..3], &[255, 255, 255]);
    assert_eq!(&a[255 * 3..255 * 3 + 3], &[0, 0, 0]);
}

#[test]
fn none_colorant_fill_paints_white_like_pdfium() {
    // `/None` marks nothing. PDFium's CPDF_SeparationCS::GetRGB returns
    // nullopt for the None type and the colour state falls back to white
    // (cpdf_colorstate.cpp `value_or(0xFFFFFFFF)`), so white is the
    // oracle-verified composite too.
    let cs = "[/Separation/None/DeviceGray\
        <</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>]";
    let c = first_solid(&page_with(cs, b"/Cs cs 1 scn 0 0 100 100 re f"));
    assert!(c.r > 0.98 && c.g > 0.98 && c.b > 0.98, "/None paints white, got {c:?}");
}

#[test]
fn none_colorant_image_ramp_stays_white_unflagged() {
    // `/None` marks nothing: the all-white LUT is policy, not degradation.
    let page = page_with_image_cs(
        "[/Separation/None/DeviceGray<</FunctionType 2/Domain[0 1]/C0[1]/C1[0]/N 1>>]",
    );
    assert_eq!(page.images.len(), 1);
    assert!(!page.images[0].lowering_degraded);
    let lut = tint_lut(&page);
    assert_eq!(&lut[0..3], &[255, 255, 255]);
    assert_eq!(&lut[255 * 3..255 * 3 + 3], &[255, 255, 255]);
}

/// A **two**-colorant `/DeviceN` image carries two tints per pixel, not a gray
/// value and a spare channel. Directly-decoded samples are already converted to
/// RGB at compile time, but a *codec-encoded* one cannot be — the pixels do not
/// exist yet — so it fell back to the arity approximation, read one tint as
/// DeviceGray and dropped the other, losing a spot+black duotone's spot plate.
/// It must lower to a baked `256 x 256 x 3` table the backend samples instead.
#[test]
fn two_colorant_devicen_image_lowers_to_a_two_input_lut() {
    // A 2-in/3-out sampled transform over a 2x2 grid, input 0 varying fastest:
    //   (0,0)=black  (1,0)=red  (0,1)=green  (1,1)=white
    // so both inputs demonstrably reach the table and in distinguishable ways.
    let samples: &[u8] = &[
        0, 0, 0, // (0,0) black
        255, 0, 0, // (1,0) red
        0, 255, 0, // (0,1) green
        255, 255, 255, // (1,1) white
    ];
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</Im 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"q 100 0 0 100 0 0 cm /Im Do Q");
    // Codec-encoded (the payload is never decoded at compile time), so the
    // colour space has to survive into the IR rather than being baked away.
    b.add_stream(
        5,
        "/Type/XObject/Subtype/Image/Width 2/Height 1/BitsPerComponent 8\
         /Filter/DCTDecode/ColorSpace[/DeviceN[/SpotA/Black]/DeviceRGB 6 0 R]",
        &[0xFFu8, 0xD8, 0xFF, 0xD9],
    );
    b.add_stream(
        6,
        "/FunctionType 0/Domain[0 1 0 1]/Size[2 2]/BitsPerSample 8\
         /Range[0 1 0 1 0 1]/Encode[0 1 0 1]",
        samples,
    );
    b.finish_classic_xref("/Root 1 0 R");
    let page = compile(b.into_bytes());

    assert_eq!(page.images.len(), 1);
    let rgb = match &page.images[0].color_space {
        pdf_page_ir::ImageColorSpace::TintLut2 { rgb } => rgb.clone(),
        other => panic!("2-colorant DeviceN must lower to TintLut2, got {other:?}"),
    };
    assert_eq!(rgb.len(), 256 * 256 * 3, "table is 256 x 256 x 3");

    let at = |a: usize, bb: usize| {
        let o = (a * 256 + bb) * 3;
        [rgb[o], rgb[o + 1], rgb[o + 2]]
    };
    // The four grid corners must survive the bake.
    let black = at(0, 0);
    let red = at(255, 0);
    let green = at(0, 255);
    let white = at(255, 255);
    assert!(black.iter().all(|&c| c < 60), "corner (0,0) black: {black:?}");
    assert!(red[0] > 200 && red[1] < 60, "corner (1,0) red: {red:?}");
    assert!(green[1] > 200 && green[0] < 60, "corner (0,1) green: {green:?}");
    assert!(white.iter().all(|&c| c > 200), "corner (1,1) white: {white:?}");
    // Crucially, the *second* input changes the result — the bug was that it
    // never reached the transform at all.
    assert_ne!(black, green, "the second colorant must affect the baked colour");
    assert_ne!(black, red, "the first colorant must affect the baked colour");
}
