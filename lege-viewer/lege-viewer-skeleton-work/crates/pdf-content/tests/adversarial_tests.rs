#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: panics are the assertion mechanism

//! Phase 6E: malformed-resource behavior & recursion limits. Every fixture
//! here is hostile (self-reference, cycles, absurd counts); the contract is
//! that the page compiler terminates without panicking — degrading to skipped
//! ops or a typed per-page error, never a stack overflow or OOM.

use std::sync::Arc;

use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{OwnedBytesSource, PdfSource};
use pdf_test_support::builder::PdfBuilder;

fn open(bytes: Vec<u8>) -> DocumentSnapshot {
    let source: Arc<dyn PdfSource> = Arc::new(OwnedBytesSource::new(bytes));
    DocumentSnapshot::open(source, DocumentLimits::default()).expect("open failed")
}

/// Compile page 0; returns whether it terminated (Ok or a typed error), never
/// panicking.
fn try_compile(bytes: Vec<u8>) -> bool {
    let snap = open(bytes);
    let mut ctx = ParseContext::new();
    PageCompiler::new().compile(&snap, PageIndex(0), &mut ctx).is_ok()
}

/// Compile page 0, returning whether it succeeded and how many recovery events
/// were recorded — content malformation must be recovered (`Ok`), observably.
fn compile_with_recovery(bytes: Vec<u8>) -> (bool, usize) {
    let snap = open(bytes);
    let mut ctx = ParseContext::new();
    let ok = PageCompiler::new().compile(&snap, PageIndex(0), &mut ctx).is_ok();
    (ok, ctx.recovery.len())
}

/// A minimal one-page document whose single content stream is `body`.
fn one_page_doc(body: &[u8]) -> Vec<u8> {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>");
    b.add_stream(4, "", body);
    b.finish_classic_xref("/Root 1 0 R");
    b.into_bytes()
}

/// A3: a stray `)` — a delimiter the lexer refuses to consume — is dropped and
/// interpretation continues (the *Trotsky Revolution Betrayed* p27 class). The
/// page still compiles and the recovery is observable.
#[test]
fn stray_close_paren_token_recovers() {
    // `re f` paints a rectangle; the trailing `)` is the malformation.
    let (ok, recoveries) = compile_with_recovery(one_page_doc(b"0 0 50 50 re f\n)\n"));
    assert!(ok, "a stray ) must not be page-fatal");
    assert!(recoveries >= 1, "the dropped token must be noted as a recovery");
}

/// A3: an operator/keyword where an operand is expected (here `l` inside a `TJ`
/// array) is dropped, not page-fatal (the *Cambourne New Settlement* p25 class).
#[test]
fn keyword_where_operand_expected_recovers() {
    let (ok, recoveries) = compile_with_recovery(one_page_doc(b"[(a) l (b)] TJ\n0 0 5 5 re f\n"));
    assert!(ok, "a bare keyword inside operands must not be page-fatal");
    assert!(recoveries >= 1, "the malformed array must be noted as a recovery");
}

/// A3: after a token is skipped, the interpreter keeps executing — a paint that
/// follows the malformation still lands on the page.
#[test]
fn recovery_preserves_following_content() {
    // Bad token first, then a real fill; the page must not be empty.
    let snap = open(one_page_doc(b") 0 0 40 40 re f\n"));
    let mut ctx = ParseContext::new();
    let page = PageCompiler::new()
        .compile_semantic(&snap, PageIndex(0), &mut ctx)
        .expect("page must compile despite the leading bad token");
    assert!(!page.paths.is_empty(), "the fill after the bad token must survive");
}

/// A2: a `/FlateDecode` content stream that decodes to nothing (corrupt, zero
/// output) yields a blank page plus a recovery note — never an error. This is
/// the zero-output sibling of pdf-structure's
/// `truncated_flate_salvages_its_inflated_prefix`.
#[test]
fn zero_output_flate_content_yields_blank_page_with_note() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<<>>>>");
    // Bytes that inflate to zero output — the decoder reports Corrupt.
    b.add_stream(4, "/Filter/FlateDecode", b"\xff\xff\xff\xff");
    b.finish_classic_xref("/Root 1 0 R");

    let snap = open(b.into_bytes());
    let mut ctx = ParseContext::new();
    let page = PageCompiler::new()
        .compile_semantic(&snap, PageIndex(0), &mut ctx)
        .expect("a zero-output content stream must not drop the page");
    assert!(page.paths.is_empty() && page.text_runs.is_empty(), "page must be blank");
    assert!(!ctx.recovery.is_empty(), "the skipped stream must be noted as a recovery");
}

/// A1: a malformed inline image whose `EI` cannot be located drops just the
/// image (recoverable framing error) and the page still compiles.
#[test]
fn inline_image_without_ei_recovers_at_page_level() {
    let mut body = b"0 0 5 5 re f\nBI /W 2 /H 2 /CS /RGB /F /DCTDecode ID ".to_vec();
    body.extend_from_slice(&[0x00, 0x01, 0x02, 0x03]); // no EI terminator
    let (ok, recoveries) = compile_with_recovery(one_page_doc(&body));
    assert!(ok, "an unterminated inline image must not be page-fatal");
    assert!(recoveries >= 1, "dropping the inline image must be noted");
}

#[test]
fn self_referential_tiling_pattern_terminates() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Pattern<</P1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Pattern cs /P1 scn 0 0 100 100 re f");
    // The pattern's own cell paints with the same pattern — a cycle.
    b.add_stream(
        5,
        "/Type/Pattern/PatternType 1/PaintType 1/TilingType 1\
         /BBox[0 0 10 10]/XStep 10/YStep 10\
         /Resources<</Pattern<</P1 5 0 R>>>>",
        b"/Pattern cs /P1 scn 0 0 5 5 re f",
    );
    b.finish_classic_xref("/Root 1 0 R");
    // Must terminate (bounded by the invoke-depth guard), not overflow.
    assert!(try_compile(b.into_bytes()));
}

#[test]
fn cyclic_type3_function_degrades_without_overflow() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    b.add_object(
        5,
        "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 100 0]/Function 6 0 R>>",
    );
    // A Type 3 function whose /Functions references itself.
    b.add_object(6, "<</FunctionType 3/Domain[0 1]/Functions[6 0 R]/Bounds[]/Encode[0 1]>>");
    b.finish_classic_xref("/Root 1 0 R");
    // The depth guard bails; the shading compiles with a degraded ramp.
    assert!(try_compile(b.into_bytes()));
}

#[test]
fn absurd_sampled_function_size_is_rejected() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Shading<</Sh1 5 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Sh1 sh");
    b.add_object(
        5,
        "<</ShadingType 2/ColorSpace/DeviceRGB/Coords[0 0 100 0]/Function 6 0 R>>",
    );
    // A Type 0 function claiming a billion samples: the allocation guard bails.
    b.add_stream(
        6,
        "/FunctionType 0/Domain[0 1]/Size[1000000000]/BitsPerSample 8/Range[0 1 0 1 0 1]",
        &[0u8; 8],
    );
    b.finish_classic_xref("/Root 1 0 R");
    assert!(try_compile(b.into_bytes()));
}

#[test]
fn pattern_referencing_missing_resource_falls_back() {
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    b.add_object(2, "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>");
    // /Pattern resource exists but P1 is missing from it.
    b.add_object(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Pattern<</P2 9 0 R>>>>>>",
    );
    b.add_stream(4, "", b"/Pattern cs /P1 scn 0 0 100 100 re f");
    b.finish_classic_xref("/Root 1 0 R");
    // Unresolved pattern → symbolic fallback, page still compiles.
    assert!(try_compile(b.into_bytes()));
}

#[test]
fn deeply_nested_distinct_forms_hit_the_depth_limit() {
    // 40 mutually distinct forms F0..F39, each invoking the next: no cycle, so
    // cycle detection does not fire — the depth guard must (max_invoke_depth
    // default 16). A typed RecursionDepth error is acceptable; a crash is not.
    let mut b = PdfBuilder::new();
    b.add_object(1, "<</Type/Catalog/Pages 2 0 R>>");
    let mut xobjects = String::new();
    for i in 0..40 {
        xobjects.push_str(&format!("/F{i} {} 0 R", 100 + i));
    }
    b.add_object(
        2,
        "<</Type/Pages/Kids[3 0 R]/Count 1/MediaBox[0 0 100 100]>>",
    );
    b.add_object(
        3,
        &format!("<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<<{xobjects}>>>>>>"),
    );
    b.add_stream(4, "", b"/F0 Do");
    for i in 0..40 {
        let body: Vec<u8> = if i < 39 {
            format!("/F{} Do", i + 1).into_bytes()
        } else {
            b"0 0 1 1 re f".to_vec()
        };
        let dict = format!(
            "/Type/XObject/Subtype/Form/BBox[0 0 100 100]\
             /Resources<</XObject<<{xobjects}>>>>"
        );
        b.add_stream(100 + i, &dict, &body);
    }
    b.finish_classic_xref("/Root 1 0 R");
    let snap = open(b.into_bytes());
    let mut ctx = ParseContext::new();
    // Terminates: either a typed recursion error or a truncated-but-Ok page.
    let _ = PageCompiler::new().compile(&snap, PageIndex(0), &mut ctx);
}
